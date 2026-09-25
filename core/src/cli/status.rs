//! `g-mesh status`: what the current project's daemon, plugin and index are
//! actually doing.
//!
//! Everything here is answered from outside the daemon - the recorded pids,
//! the socket, the project's own `index.db`, and the phase word a running
//! daemon publishes to `index.phase` (D13 in
//! `docs/architecture/lazy-indexing.md`) - and never by asking the daemon a
//! question. That is deliberate: the single most useful moment to
//! run `status` is when the daemon is *not* well, and a report that needs a
//! healthy daemon to be produced would go quiet exactly then.
//!
//! # How each field is established
//!
//! - **Daemon core**: the pid in `daemon.pid` plus whether anything is
//!   accepting connections on the socket. Both, because either alone lies in
//!   a way the other catches - a recycled pid looks alive, and a socket file
//!   outlives the process that bound it. Note that "running" no longer
//!   implies "ready to answer at once": since task 105 the socket is bound
//!   before the cold-start walk, so a daemon can be running and listening
//!   while its first tool call is still waiting on that walk to finish
//!   (GM-394 - it waits rather than erroring, so "answering" is no longer the
//!   binary this note used to describe, just "answering slowly") - which is
//!   what the `index:` line below reports on.
//! - **Daemon build**: the stamp a live daemon publishes about the executable
//!   it started from (`daemon::build_stamp`), compared with this command's
//!   own. A daemon that outlived an upgrade answers every query correctly
//!   *for the build it is*, so nothing else in this report can show it up -
//!   which is precisely why it gets a line of its own.
//! - **Plugins**: every `plugin-<language>.pid` file present in the state
//!   directory (`daemon::registry::discovered_pid_files`), checked the same
//!   way as the core's own pid and then read against the core's state - one
//!   line per language, since `daemon::registry::PluginRegistry` gives each
//!   language its own pid file rather than the single one a pre-registry
//!   daemon wrote. A language with no pid file at all is not reported by
//!   name: under the registry's lazy-spawn model that is the ordinary state
//!   for a language nothing has touched yet *and* for one that was spawned
//!   and has since gone idle (`daemon::lifecycle`'s two-tier sleep model
//!   removes the pid file on every sleep, same as before) - the two are
//!   indistinguishable from outside a running daemon, so this reports what it
//!   can honestly tell apart: "active" for a live pid, "orphaned" for one
//!   whose core is gone, and a single summary line when no language has a pid
//!   file at all.
//! - **Index phase**: the word a running daemon last published to
//!   `index.phase` (`daemon::read_phase_in`, D13) - `unindexed`, `walking`,
//!   `structural`, `embedding`, `ready` or `failed`. GM-395's lazy activation
//!   is what makes this its own field rather than something `bulk_indexed`
//!   and `core` could keep implying together: a project can now sit
//!   `unindexed` under a live, idle daemon for as long as nothing has asked,
//!   which the old "`daemon_alive` implies a walk is under way" reasoning
//!   could not tell apart from an actual walk in progress.
//! - **Index progress**: the counters the same daemon publishes to
//!   `index.progress` (`daemon::read_progress_in`), shown per stage only
//!   while the pid they carry is the live daemon's ([`live_progress`]).
//! - **Dirty files / index coverage**: a gitignore-aware walk of the project,
//!   cross-referenced against the `File` nodes and `indexed_files` baselines
//!   in the index. See [`IndexStatus`] for exactly what each number counts.
//! - **Files with syntax errors**: the `hasSyntaxErrors` flag the plugin sets
//!   on a file it could only partially parse.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rusqlite::{Connection, OpenFlags};

use crate::daemon;
use crate::daemon::build_stamp::{self, Vintage};
use crate::daemon::indexing_status::{group_thousands, ProgressSnapshot};
use crate::daemon::manifest::{self, DiscoveredPlugins};
use crate::gc::last_used::{self, LastUsed};
use crate::gc::warning;
use crate::storage::connection::project_dir;
use crate::watcher::staleness::mtime_millis;
use crate::watcher::BASELINE_EXCLUDED_DIRS;

/// Whether a daemon core is serving this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreState {
    /// Its pid is alive and its socket is bound. Says nothing about whether
    /// the index behind it is complete - a daemon in its cold-start walk
    /// binds first and answers its handshake immediately, but a tool call
    /// waits for the walk to finish before it answers (task 105, later
    /// GM-394). `render` cross-references this with `IndexStatus::
    /// bulk_indexed` (task 108) so a walk in progress reads as exactly that,
    /// not as a stuck daemon next to an unrelated-looking "cold start still
    /// owed" line.
    Running { pid: u32 },
    /// Its pid is alive but nothing answers on the socket. Since the bind
    /// moved ahead of the cold-start walk this no longer covers a daemon that
    /// is merely busy indexing - it means one that died without clearing its
    /// pid file, or a wedged one.
    NotAccepting { pid: u32 },
    /// Nothing in `daemon.pid`, but a live process is still holding this
    /// project's singleton lock after having served it - so no other daemon
    /// can take the project over, and nothing can reach this one either
    /// (task 184). Kept apart from `NotAccepting`, which describes a daemon
    /// that still has a pid file: what makes this one worth its own variant is
    /// precisely that every check keyed off `daemon.pid` reports it as nothing
    /// running at all.
    Wedged { pid: u32 },
    /// No live daemon, whatever `daemon.pid` may still say.
    NotRunning,
}

/// How the build a running daemon started from compares with the build
/// answering this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildState {
    /// Nothing is running, so there is no build to compare against.
    NotRunning,
    /// The daemon started from this executable, or from one built later.
    Current,
    /// The daemon started from an executable built before this one: it
    /// predates whatever has been installed since, and the index invalidation
    /// that a start on the new build would have performed.
    Outdated,
    /// The daemon started from this very executable, but is holding a JS/TS
    /// plugin that has been rebuilt since - so the graph it is serving was
    /// computed by extraction logic that is no longer on disk. Reported apart
    /// from `Outdated` because "your core binary is old" would be false here,
    /// and would send someone looking in the wrong place.
    PluginChanged,
    /// Running, but nothing usable is on record about its build - the shape
    /// every daemon that predates this check has. Reported rather than
    /// silently called current, because "we did not compare" and "we compared
    /// and it matched" are different answers.
    Unknown,
}

/// Whether one language's plugin process is up - reported per language, in
/// [`PluginReport`], rather than once for "the" plugin: since
/// `daemon::registry::PluginRegistry` replaced the daemon's single
/// `Arc<PluginSupervisor>`, there can be any number of these at once.
///
/// No `Asleep`/`NotRunning` variant here, unlike the pre-registry version of
/// this type: those meant "a live core with no plugin pid on record has
/// exactly one explanation" - true when the daemon spawned its one plugin
/// unconditionally at startup, but no longer true under lazy per-language
/// spawning, where "no pid file for this language" now covers two states
/// this command cannot tell apart from outside a running daemon: a language
/// nothing has touched yet, and one that was spawned and has since gone
/// idle. [`plugin_reports`] only ever constructs a [`PluginReport`] for a
/// language that *does* have a pid file, and [`render`] prints a single
/// summary line, not a per-language guess, when none do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Running under a live core.
    Active { pid: u32 },
    /// Alive with no core serving this project: a leaked child of a daemon
    /// that died without closing its stdin. `g-mesh stop` clears it.
    Orphaned { pid: u32 },
}

/// One language's plugin, as `status` reports it - `daemon::registry
/// ::PluginRegistry` gives each language its own pid file, so there is one of
/// these per `plugin-<language>.pid` file found, not one per daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReport {
    pub language: String,
    pub state: PluginState,
}

/// One language whose semantic passes are suspended right now (task GM-274's
/// own acceptance criterion: "`g-mesh status` shows suspension per
/// language") - written by a running daemon's `daemon::lifecycle
/// ::PluginSupervisor::check_memory_limit` the instant `[plugin]
/// memoryLimitMb` catches that language's process tree over the limit (see
/// the architecture doc's "Plugin memory limit" section), and read here the
/// same way every other runtime fact this command reports is: off disk,
/// never by asking a live daemon a question (see this module's own doc
/// comment).
///
/// Deliberately its own listing rather than a field on [`PluginReport`]
/// above: a plugin `check_memory_limit` just suspended has *no* pid file by
/// the time anyone runs `status` - the same `sleep_now` path idle-sleep
/// already uses removes it (`PluginSupervisor::put_to_sleep`) - so it is
/// invisible to [`plugin_reports`] regardless, and a suspension marker is the
/// only thing that survives to describe it. See
/// `daemon::registry::discovered_suspended_markers`, this field's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedLanguage {
    pub language: String,
    /// The human-readable reason `PluginSupervisor::check_memory_limit`
    /// recorded when it suspended this language (naming the configured limit
    /// and the measured figure) - persisted verbatim, because nothing else
    /// about *why* survives outside the daemon process that decided it.
    pub reason: String,
}

/// What the index covers, and what it still owes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    /// Whether a full project walk has ever finished (`meta.bulkIndexedAt`).
    pub bulk_indexed: bool,
    /// Whether the whole-project semantic pass that follows a walk has ever
    /// finished (`meta.semanticPassAt`) - independent of `bulk_indexed`,
    /// deliberately: every walk records `bulkIndexedAt` before asking for
    /// this pass (see `daemon::semantic`'s module doc), so a pass
    /// interrupted afterwards (a killed plugin, a crash, a timeout) leaves
    /// `bulk_indexed` true and this `false`. That combination is the one
    /// [`render`] calls out by name - see task 62cc2d0f.
    pub semantic_pass_completed: bool,
    /// Semantic-pass-capable languages present in the index whose pass has
    /// not completed (`language_state.semanticPassAt` unset), sorted.
    pub semantic_pass_owed: Vec<String>,
    /// `(language, reason)` for every language whose last whole-project
    /// semantic pass failed (`language_state.semanticPassError`), sorted.
    pub semantic_pass_failures: Vec<(String, String)>,
    /// Source files found on disk now - the denominator of coverage.
    pub discovered: usize,
    /// How many of those the index has a `File` node for.
    pub indexed: usize,
    /// Files the index still owes work for: never indexed at all, or indexed
    /// against a recorded baseline that no longer matches the file on disk.
    ///
    /// A bulk-indexed file with no `indexed_files` row is *not* counted:
    /// `daemon::bulk_index` leaves a walked file without a baseline when it
    /// cannot prove the walk saw its current bytes (GM-401,
    /// `watcher::staleness::record_walk_baselines`), and an index built
    /// before GM-401 has none at all - so a missing baseline means "walked,
    /// not yet checked since", not "stale".
    pub dirty: usize,
    /// Project-relative paths of files the plugin flagged as only partially
    /// parseable, sorted.
    pub syntax_error_files: Vec<String>,
}

impl IndexStatus {
    /// Indexed files over discovered files. A project with no source files at
    /// all is fully covered rather than a division by zero.
    pub fn coverage(&self) -> f64 {
        if self.discovered == 0 {
            return 1.0;
        }
        self.indexed as f64 / self.discovered as f64
    }
}

/// Everything `g-mesh status` prints, as data.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub project_root: PathBuf,
    pub project_id: String,
    pub state_dir: PathBuf,
    pub core: CoreState,
    pub build: BuildState,
    /// Every language with a pid file on disk right now, sorted by language -
    /// see [`PluginState`]'s doc comment for what "no pid file" now means
    /// under lazy per-language spawning.
    pub plugins: Vec<PluginReport>,
    /// Every language currently suspended by `[plugin] memoryLimitMb`, sorted
    /// by language - see [`SuspendedLanguage`]'s own doc comment for why this
    /// is independent of `plugins` above.
    pub suspended_languages: Vec<SuspendedLanguage>,
    pub last_used: Option<LastUsed>,
    pub index: IndexStatus,
    /// The word a running daemon last published to `index.phase` (D13 in
    /// `docs/architecture/lazy-indexing.md`) - `None` when no daemon is
    /// serving this project right now, since `daemon::lifecycle::
    /// release_state_files` removes the file on the way out. This is what
    /// [`render`] uses to tell "idle, never indexed" (`unindexed`) apart from
    /// "building right now" (`walking`) - a distinction `IndexStatus::
    /// bulk_indexed` alone cannot make under GM-395's lazy activation, since
    /// a project can sit unwalked for as long as nothing has asked.
    pub phase: Option<String>,
    /// Set when `phase` reads `front` (GM-399, D11): the folder's projects as
    /// a fresh bounded walk counts them. `index` is then left empty, since
    /// its whole-folder file walk is exactly the cost a front exists to
    /// avoid.
    pub front: Option<FrontSummary>,
    /// The progress counters last published to `index.progress`, whichever
    /// daemon wrote them - [`render`] shows them only while their `pid` is
    /// the live daemon's (see [`live_progress`]).
    pub progress: Option<ProgressSnapshot>,
}

/// What `g-mesh status` says about a front: how many projects, and whether
/// the walk stopped at a limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrontSummary {
    pub projects: usize,
    pub truncated: bool,
}

/// Reports on the project the current directory belongs to.
///
/// Also prints the GC idle-project warning (`gc::warning`) after the report,
/// if `cleanup.enabled` and any project is past `cleanup.idleThresholdDays` -
/// `status` is a command a human runs and reads at a terminal, exactly the
/// audience that warning is for. It is never printed here for `mcp-shim` or
/// `daemon`, whose stdout is protocol traffic read by an MCP client, not a
/// person.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    print!("{}", render(&collect(&cwd)?));
    warning::maybe_print_stale_projects_warning()?;
    Ok(())
}

/// Gathers every field of the report for `project_root`.
pub fn collect(project_root: &Path) -> Result<Report> {
    let state_dir = project_dir(project_root).context("failed to resolve the project's state directory")?;
    let project_id =
        state_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    let core = core_state(project_root)?;
    let phase = daemon::read_phase_in(&state_dir);
    let front = (phase.as_deref() == Some(daemon::front::FRONT_PHASE)).then(|| {
        let walk = daemon::candidates::walk(project_root, daemon::candidates::Limits::default());
        FrontSummary { projects: walk.candidates.len(), truncated: walk.truncated }
    });
    let index = if front.is_some() {
        IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: 0,
            indexed: 0,
            dirty: 0,
            syntax_error_files: Vec::new(),
        }
    } else {
        // The same discovery the daemon runs at startup, so the files counted
        // here are the files the discovered plugins index, in every language.
        let plugins =
            manifest::discover(&manifest::default_roots()).context("failed to discover language plugins")?;
        index_status(project_root, &state_dir.join("index.db"), &plugins)?
    };
    Ok(Report {
        project_id,
        core,
        build: build_state(core, &state_dir),
        plugins: plugin_reports(&state_dir, core),
        suspended_languages: suspended_language_reports(&state_dir),
        last_used: last_used::read_from_project_dir(&state_dir)
            .context("failed to read the project's lastUsed")?,
        index,
        phase,
        front,
        progress: daemon::read_progress_in(&state_dir),
        project_root: project_root.to_path_buf(),
        state_dir,
    })
}

fn core_state(project_root: &Path) -> Result<CoreState> {
    let recorded =
        daemon::read_pid_file(&daemon::pid_path(project_root)?).filter(|&pid| daemon::is_process_alive(pid));
    let Some(pid) = recorded else {
        // No pid file, or one left behind by a daemon that crashed or was
        // killed - which used to end the enquiry. It no longer can: a daemon
        // that removed its own pid file on the way out and then failed to
        // actually exit is still holding the project's singleton lock, and
        // reporting that as "not running" is what let it wedge the project
        // unnoticed. The lock is asked because it is the same fact
        // `daemon::acquire_singleton_lock` acts on, so this report and the
        // next bootstrap cannot disagree.
        return Ok(match daemon::inspect_daemon_lock(project_root)? {
            daemon::DaemonLock::Wedged { pid } => CoreState::Wedged { pid },
            _ => CoreState::NotRunning,
        });
    };
    Ok(if daemon::is_listening(project_root)? {
        CoreState::Running { pid }
    } else {
        CoreState::NotAccepting { pid }
    })
}

/// Compares the running daemon's published build with this command's own.
///
/// `NotAccepting` is compared like `Running` rather than skipped: the stamp is
/// published before the socket is bound (see `daemon::run`), so even a daemon
/// that has not got as far as binding has already said which build it is - and
/// a daemon that is up but not answering is exactly when someone is most
/// likely to be asking why an upgrade has not taken effect.
///
/// Infallible, unlike its neighbours: every failure along the way - no
/// executable to stat, no stamp on disk, an unreadable one - is the same
/// answer, `Unknown`, and none of them is a reason for `status` to refuse to
/// print the rest of the report.
fn build_state(core: CoreState, state_dir: &Path) -> BuildState {
    if matches!(core, CoreState::NotRunning) {
        return BuildState::NotRunning;
    }
    let Ok(ours) = build_stamp::of_running_process() else {
        return BuildState::Unknown;
    };
    let published = build_stamp::read(&daemon::build_stamp_path_in(state_dir));
    match build_stamp::vintage(published.as_ref(), &ours) {
        Vintage::Current => BuildState::Current,
        Vintage::Outdated => BuildState::Outdated,
        Vintage::PluginChanged => BuildState::PluginChanged,
        Vintage::Unknown => BuildState::Unknown,
    }
}

/// Every language with a live pid file in `state_dir` right now, sorted by
/// language (`daemon::registry::discovered_pid_files` already sorts, so this
/// just carries that order through). A pid file naming a dead process is
/// dropped rather than reported - same "unreadable/stale means nothing
/// recorded" convention every pid-file read in this daemon uses.
fn plugin_reports(state_dir: &Path, core: CoreState) -> Vec<PluginReport> {
    crate::daemon::registry::discovered_pid_files(state_dir)
        .into_iter()
        .filter_map(|(language, path)| {
            let pid = daemon::read_pid_file(&path).filter(|&pid| daemon::is_process_alive(pid))?;
            Some(PluginReport { language, state: classify_plugin(pid, core) })
        })
        .collect()
}

/// Every `plugin-<language>.suspended` marker in `state_dir`, as
/// [`SuspendedLanguage`] rows - a thin wrapper over
/// `daemon::registry::discovered_suspended_markers`, unconditional on `core`
/// unlike [`plugin_reports`]: a suspension is a fact about this project's
/// state directory, not about whether a daemon happens to be running to read
/// it back right now (the marker is what makes that possible in the first
/// place - see [`SuspendedLanguage`]'s own doc comment).
fn suspended_language_reports(state_dir: &Path) -> Vec<SuspendedLanguage> {
    crate::daemon::registry::discovered_suspended_markers(state_dir)
        .into_iter()
        .map(|(language, reason)| SuspendedLanguage { language, reason })
        .collect()
}

/// The judgement itself, split from the lookup that feeds it so it can be
/// exercised over its whole table without conjuring a live process (and a
/// dead core to go with it) for every row.
fn classify_plugin(pid: u32, core: CoreState) -> PluginState {
    match core {
        // A wedged core counts as gone for the plugin's purposes: it is not
        // answering anything, so a plugin still alive under it is as orphaned
        // as one whose core has actually exited, and `g-mesh stop` clears
        // both together.
        CoreState::NotRunning | CoreState::Wedged { .. } => PluginState::Orphaned { pid },
        CoreState::Running { .. } | CoreState::NotAccepting { .. } => PluginState::Active { pid },
    }
}

/// Cross-references what is on disk against what the index knows about it.
///
/// Split out from [`collect`] - and given the database path explicitly -
/// because this is the part with real logic in it, and it should be testable
/// against a hand-built index rather than only through a live daemon.
pub fn index_status(project_root: &Path, db_path: &Path, plugins: &DiscoveredPlugins) -> Result<IndexStatus> {
    let discovered = discover_source_files(project_root, plugins)?;

    // No index yet (never bootstrapped, or deleted by hand): everything on
    // disk is owed work, and none of it is covered.
    if !db_path.exists() {
        return Ok(IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            discovered: discovered.len(),
            indexed: 0,
            dirty: discovered.len(),
            syntax_error_files: Vec::new(),
        });
    }

    // Read-write without CREATE, for the same reason `gc::last_used` uses it:
    // recovering a WAL an abandoned daemon left behind needs write access,
    // and a missing database must never be conjured into existence by a
    // command that only reports. Nothing here writes.
    let conn =
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI)
            .with_context(|| format!("failed to open the project index at {}", db_path.display()))?;

    let indexed_files = indexed_file_paths(&conn)?;
    let baselines = recorded_baselines(&conn)?;

    let mut indexed = 0;
    let mut dirty = 0;
    for file in &discovered {
        if !indexed_files.contains(&file.relative) {
            // Never indexed - the one case that is both uncovered and dirty.
            dirty += 1;
            continue;
        }
        indexed += 1;
        if let Some(&recorded_mtime) = baselines.get(&file.relative) {
            if recorded_mtime != file.mtime_millis {
                dirty += 1;
            }
        }
    }

    let capable: HashSet<String> =
        manifest::semantic_pass_capable_languages(&plugins.manifests).into_iter().collect();
    let (semantic_pass_owed, semantic_pass_failures) = semantic_pass_state(&conn, &capable)?;
    Ok(IndexStatus {
        bulk_indexed: crate::storage::schema::bulk_index_completed(&conn)
            .context("failed to read whether the project has been fully walked")?,
        semantic_pass_completed: crate::storage::schema::semantic_pass_completed(&conn)
            .context("failed to read whether the project's semantic pass has completed")?,
        semantic_pass_owed,
        semantic_pass_failures,
        discovered: discovered.len(),
        indexed,
        dirty,
        syntax_error_files: syntax_error_files(&conn)?,
    })
}

/// `(language, reason)` per language whose last semantic pass failed.
type SemanticPassFailures = Vec<(String, String)>;

/// The languages still owed a semantic pass, and the recorded failures -
/// what [`semantic_pass_lines`] reports.
pub(crate) fn semantic_pass_state(
    conn: &Connection,
    capable: &HashSet<String>,
) -> Result<(Vec<String>, SemanticPassFailures)> {
    let owed = crate::storage::schema::owed_semantic_pass_languages(conn, capable)
        .context("failed to read which languages still owe a semantic pass")?;
    let failures = crate::storage::schema::semantic_pass_failures(conn)
        .context("failed to read the recorded semantic-pass failures")?;
    Ok((owed, failures))
}

/// The report's semantic-pass lines: one per language whose last pass
/// failed, with its reason, and the "never completed" advice only while some
/// owed language has no recorded failure to explain it and no live daemon is
/// working on the index. `in_progress` is what to say instead while one is.
pub(crate) fn semantic_pass_lines(
    completed: bool,
    owed: &[String],
    failures: &[(String, String)],
    in_progress: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if completed {
        lines.push("  semantic pass:   complete".to_string());
    } else if let Some(in_progress) = in_progress {
        lines.push(format!("  semantic pass:   {in_progress}"));
    } else {
        let unexplained = owed.iter().any(|language| !failures.iter().any(|(failed, _)| failed == language));
        if unexplained || failures.is_empty() {
            lines.push("  semantic pass:   never completed - run `g-mesh reindex` to repair it".to_string());
        }
    }
    for (language, reason) in failures {
        // A pass deferred because its plugin was asleep has not failed: it is
        // still owed and will be asked again.
        if reason == daemon::semantic::NOT_RUN_REASON {
            lines.push(format!(
                "  semantic pass:   {language} pending - its plugin was asleep or memory-suspended; \
                 the next daemon start or `g-mesh reindex` asks again"
            ));
            continue;
        }
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(format!("  semantic pass:   {language} failed - {reason}"));
    }
    lines
}

struct SourceFile {
    /// Project-relative, forward-slash separated - the same spelling the
    /// `filePath` columns and the wire protocol use.
    relative: String,
    mtime_millis: i64,
}

/// Walks `project_root` for files some discovered plugin would index,
/// honoring `.gitignore`, [`BASELINE_EXCLUDED_DIRS`], and each language's own
/// `[plugin.workspace] exclude_dirs` - the file-level decision is
/// [`DiscoveredPlugins::indexing_language`], the same filter the watcher
/// routes changes through.
///
/// Core cannot literally reuse the bulk walk: each plugin walks the project
/// itself, in its own process (the TS plugin's `ignorePolicy.ts`, the Go
/// plugin's `walk.go`, the SDK's `walk_project` for Rust and Python). What they
/// share, and what this mirrors, is the manifest: extensions claimed, the
/// directories excluded per language, and the baseline pair.
///
/// Deliberate divergences from those walks, all in the direction of doing
/// less: symlinks are not followed (the TS plugin follows them under a cycle
/// guard), and a file whose metadata cannot be read is skipped rather than
/// failing the report. Neither can make a broken index look healthy.
fn discover_source_files(project_root: &Path, plugins: &DiscoveredPlugins) -> Result<Vec<SourceFile>> {
    // Pruned outright: the baseline, plus any directory *every* discovered
    // language excludes. A directory only some languages exclude is still
    // walked (`dist/app.py` is Python's even though TypeScript skips `dist`),
    // and its files are filtered one by one below.
    let pruned: Vec<String> = BASELINE_EXCLUDED_DIRS
        .iter()
        .map(|dir| (*dir).to_string())
        .chain(excluded_by_every_language(plugins))
        .collect();

    let mut files = Vec::new();
    let walk = WalkBuilder::new(project_root)
        // Matching the plugins' walks, which read each directory's own
        // .gitignore and nothing else: no dotfile skipping, no rules from
        // above the project root, no global/`info/exclude` rules, and rules
        // honored even outside a git repository.
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            !is_dir || !entry.file_name().to_str().is_some_and(|name| pruned.iter().any(|dir| dir == name))
        })
        .build();

    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            // An unreadable directory costs its subtree, not the report.
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(relative) = relative_wire_path(project_root, entry.path()) else {
            continue;
        };
        if plugins.indexing_language(&relative).is_none() {
            continue;
        }
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        let Ok(mtime_millis) = mtime_millis(&metadata) else {
            continue;
        };
        files.push(SourceFile { relative, mtime_millis });
    }
    Ok(files)
}

/// The directory names in every discovered manifest's `exclude_dirs` - safe to
/// prune from the walk, since no language would index anything under them.
/// Empty when nothing was discovered.
fn excluded_by_every_language(plugins: &DiscoveredPlugins) -> Vec<String> {
    let mut manifests = plugins.manifests.values();
    let Some(first) = manifests.next() else {
        return Vec::new();
    };
    let mut common: Vec<String> = first.workspace.exclude_dirs.clone();
    for manifest in manifests {
        common.retain(|dir| manifest.workspace.exclude_dirs.contains(dir));
    }
    common
}

fn relative_wire_path(root: &Path, absolute: &Path) -> Option<String> {
    let relative = absolute.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        parts.push(component.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// Every file the index has a `File` node for. The plugin emits exactly one
/// per file it parsed, which makes this the honest answer to "which files
/// does the graph actually cover?" - unlike `indexed_files`, which only the
/// incremental path writes, and unlike `DISTINCT filePath`, which a file with
/// no symbols in it would be missing from.
fn indexed_file_paths(conn: &Connection) -> Result<HashSet<String>> {
    let mut statement = conn
        .prepare("SELECT filePath FROM nodes WHERE kind = 'File'")
        .context("failed to query indexed files")?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to read indexed files")?
        .collect::<rusqlite::Result<HashSet<String>>>()
        .context("failed to read indexed files")?;
    Ok(paths)
}

fn recorded_baselines(conn: &Connection) -> Result<std::collections::HashMap<String, i64>> {
    let mut statement = conn
        .prepare("SELECT filePath, mtimeMillis FROM indexed_files")
        .context("failed to query indexed_files baselines")?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
        .context("failed to read indexed_files baselines")?
        .collect::<rusqlite::Result<std::collections::HashMap<String, i64>>>()
        .context("failed to read indexed_files baselines")?;
    Ok(rows)
}

fn syntax_error_files(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT DISTINCT filePath FROM nodes WHERE hasSyntaxErrors = 1 ORDER BY filePath")
        .context("failed to query files with syntax errors")?;
    let files = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to read files with syntax errors")?
        .collect::<rusqlite::Result<Vec<String>>>()
        .context("failed to read files with syntax errors")?;
    Ok(files)
}

/// Renders a report as the text the command prints.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "g-mesh status for {}", report.project_root.display());
    let _ = writeln!(out, "  project id:      {}", report.project_id);
    let _ = writeln!(out, "  state directory: {}", report.state_dir.display());
    let _ = writeln!(out, "  daemon core:     {}", describe_core(report.core));
    if let Some(build) = describe_build(report.build) {
        let _ = writeln!(out, "  daemon build:    {build}");
    }
    if report.plugins.is_empty() {
        let _ = writeln!(
            out,
            "  plugins:         none active - languages are spawned lazily on first use and \
             sleep when idle, so this is normal on a quiet project"
        );
    } else {
        for plugin in &report.plugins {
            let _ = writeln!(out, "  plugin ({}):     {}", plugin.language, describe_plugin(plugin.state));
        }
    }
    if !report.suspended_languages.is_empty() {
        for suspended in &report.suspended_languages {
            let _ = writeln!(out, "  semantic ({}): suspended - {}", suspended.language, suspended.reason);
        }
    }
    let _ = writeln!(out, "  last used:       {}", describe_last_used(report.last_used.as_ref()));

    let index = &report.index;
    // A phase word or progress file whose daemon is not running was left
    // behind by one killed before it could remove them, so neither is read
    // as what is happening now.
    let daemon_alive = !matches!(report.core, CoreState::NotRunning);
    let phase = if daemon_alive { report.phase.as_deref() } else { None };
    let progress = live_progress(report);
    if let Some(front) = report.front {
        // A front has no index: no coverage, no dirty files, nothing to
        // repair. A session picks one of the projects, which is then served
        // by its own daemon with its own `g-mesh status`.
        let _ = writeln!(
            out,
            "  index:           folder of {}{} projects - no index; a session selects one",
            front.projects,
            if front.truncated { "+" } else { "" }
        );
        return out;
    }
    let _ = writeln!(
        out,
        "  index:           {}",
        describe_index(phase, progress, index.bulk_indexed, daemon_alive)
    );
    if let Some(estimate) = phase.and_then(|phase| overall_estimate(phase, progress?)) {
        let _ = writeln!(
            out,
            "  overall:         ~{estimate:.0}% (estimate: walk 40%, semantic pass 20%, embeddings 40% of the work)"
        );
    }
    let _ = writeln!(
        out,
        "  index coverage:  {:.1}% ({}/{} source files)",
        index.coverage() * 100.0,
        index.indexed,
        index.discovered
    );
    if !index.bulk_indexed && phase == Some("walking") {
        let _ = writeln!(out, "  dirty files:     {} awaiting the walk already in progress", index.dirty);
    } else {
        let _ = writeln!(out, "  dirty files:     {} awaiting reindex", index.dirty);
    }

    // Only meaningful once a walk has actually landed - before that,
    // `index:` above already says a cold start (or the walk in progress) is
    // what is owed, and a semantic pass has nowhere to run yet regardless.
    // Once `bulk_indexed` is true, `semantic_pass_completed` is the fact
    // `bulk_indexed` alone cannot tell apart from a genuinely finished index:
    // a pass interrupted after the walk was already recorded (a killed
    // plugin, a crash, a timeout) leaves exactly this combination, and
    // nothing else in this report would ever call it out - see task
    // 62cc2d0f / `daemon::semantic`'s module doc.
    if index.bulk_indexed {
        let in_progress = match phase {
            Some("unindexed" | "walking" | "structural" | "embedding") => {
                Some(semantic_in_progress(progress))
            }
            _ => None,
        };
        for line in semantic_pass_lines(
            index.semantic_pass_completed,
            &index.semantic_pass_owed,
            &index.semantic_pass_failures,
            in_progress.as_deref(),
        ) {
            let _ = writeln!(out, "{line}");
        }
    }

    if index.syntax_error_files.is_empty() {
        let _ = writeln!(out, "  syntax errors:   none");
    } else {
        let _ = writeln!(out, "  syntax errors:   {} file(s)", index.syntax_error_files.len());
        for file in &index.syntax_error_files {
            let _ = writeln!(out, "    {file}");
        }
    }
    out
}

/// `report.progress`, only when the daemon serving the project right now is
/// the one that wrote it. A running daemon replaces the file with its own pid
/// as soon as it starts, so a snapshot with any other pid - or with no daemon
/// running at all - was left by a daemon that died without removing it.
/// The pid is corroborated by the socket (`CoreState::Running`), so a
/// recycled pid alone does not make a leftover snapshot look live.
pub(crate) fn live_progress(report: &Report) -> Option<&ProgressSnapshot> {
    match report.core {
        CoreState::Running { pid } => report.progress.as_ref().filter(|progress| progress.pid == pid),
        _ => None,
    }
}

/// The `index:` line's text. `phase` is `None` when no daemon is running or
/// the running one published no phase.
fn describe_index(
    phase: Option<&str>,
    progress: Option<&ProgressSnapshot>,
    bulk_indexed: bool,
    daemon_alive: bool,
) -> String {
    match phase {
        Some("unindexed") => "not indexed yet - builds on the first tool call".to_string(),
        Some("walking") => match progress {
            Some(progress) => format!("building now - {}", describe_walk(progress)),
            None => "building now".to_string(),
        },
        Some("structural") => match progress.and_then(describe_semantic_running) {
            Some(running) => format!("structural index ready; semantic pass running - {running}"),
            None => "structural index ready; embedding pass not started yet".to_string(),
        },
        Some("embedding") => match progress.map(|progress| &progress.embeddings) {
            Some(embeddings) if embeddings.total == 0 => {
                "structural index ready; embeddings being computed - counting what needs embedding"
                    .to_string()
            }
            Some(embeddings) => format!(
                "structural index ready; embeddings being computed - {}/{} ({:.1}%)",
                group_thousands(embeddings.done),
                group_thousands(embeddings.total),
                percent(embeddings.done, embeddings.total)
            ),
            None => "structural index ready; embeddings being computed".to_string(),
        },
        Some("ready") => "ready - every tool answers".to_string(),
        Some("failed") => "last build failed - see daemon log; retried on the next tool call".to_string(),
        _ if !bulk_indexed && daemon_alive => {
            "building now - first walk in progress, nothing to restart".to_string()
        }
        _ if !bulk_indexed => "never fully walked - a cold start is still owed".to_string(),
        _ if daemon_alive => "built".to_string(),
        _ => "built - no daemon is serving it right now".to_string(),
    }
}

fn describe_walk(progress: &ProgressSnapshot) -> String {
    let walk = &progress.walk;
    let items = group_thousands(walk.items);
    if walk.languages_total > 0 && walk.languages_done >= walk.languages_total {
        format!("linking imports and symbols ({items} nodes and edges walked)")
    } else if walk.languages_total > 0 {
        let current = walk.current_language.as_deref().unwrap_or("the first language");
        format!(
            "walking {current} ({}/{} languages done), {items} nodes and edges so far",
            walk.languages_done, walk.languages_total
        )
    } else {
        format!("walking, {items} nodes and edges so far")
    }
}

/// `"typescript (0/2 languages done)"` while a language's pass is running.
fn describe_semantic_running(progress: &ProgressSnapshot) -> Option<String> {
    let semantic = &progress.semantic;
    let current = semantic.current_language.as_deref()?;
    Some(format!("{current} ({}/{} languages done)", semantic.languages_done, semantic.languages_total))
}

/// What the semantic-pass line says instead of repair advice while a live
/// daemon is still working through the index.
fn semantic_in_progress(progress: Option<&ProgressSnapshot>) -> String {
    match progress.and_then(describe_semantic_running) {
        Some(running) => format!("running - {running}"),
        None => "not completed yet - the running daemon is still indexing; nothing to repair".to_string(),
    }
}

/// A rough whole-index percentage while a stage is actively running, by
/// fixed stage weights (walk 40%, semantic pass 20%, embeddings 40%) - only
/// the numbers within a stage are measured. `None` when nothing is running.
fn overall_estimate(phase: &str, progress: &ProgressSnapshot) -> Option<f64> {
    const WALK: f64 = 40.0;
    const SEMANTIC: f64 = 20.0;
    const EMBEDDINGS: f64 = 40.0;
    let fraction =
        |done: u64, total: u64| if total == 0 { 0.0 } else { (done.min(total) as f64) / (total as f64) };
    match phase {
        "walking" => {
            let walk = &progress.walk;
            Some(WALK * fraction(u64::from(walk.languages_done), u64::from(walk.languages_total)))
        }
        "structural" => {
            progress.semantic.current_language.as_ref()?;
            let semantic = &progress.semantic;
            Some(
                WALK + SEMANTIC
                    * fraction(u64::from(semantic.languages_done), u64::from(semantic.languages_total)),
            )
        }
        "embedding" => {
            Some(WALK + SEMANTIC + EMBEDDINGS * fraction(progress.embeddings.done, progress.embeddings.total))
        }
        _ => None,
    }
}

fn percent(done: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    done as f64 * 100.0 / total as f64
}

fn describe_core(core: CoreState) -> String {
    match core {
        CoreState::Running { pid } => format!("running (pid {pid})"),
        CoreState::NotAccepting { pid } => {
            format!("running but not accepting connections yet (pid {pid})")
        }
        CoreState::Wedged { pid } => format!(
            "wedged (pid {pid}) - holds this project's daemon lock but serves nothing; \
             run `g-mesh stop` to clear it"
        ),
        CoreState::NotRunning => "not running".to_string(),
    }
}

/// `None` for a project with nothing running, whose report has no room for a
/// line about a build that does not exist.
///
/// The two unhealthy states both name `g-mesh stop` even though a shim now
/// replaces an outdated daemon on its own: someone running `status` is asking
/// *now*, and telling them the next MCP call would have sorted it out is not
/// an answer to that. Running it is harmless if the shim got there first.
fn describe_build(build: BuildState) -> Option<&'static str> {
    match build {
        BuildState::NotRunning => None,
        BuildState::Current => Some("this build"),
        BuildState::Outdated => Some(
            "older than this g-mesh - it predates any index invalidation this build \
             would do; run `g-mesh stop`, or let the next MCP call replace it",
        ),
        BuildState::PluginChanged => Some(
            "this build, but holding a JS/TS plugin that has been rebuilt since - \
             its graph came from extraction logic no longer on disk; run \
             `g-mesh stop`, or let the next MCP call replace it",
        ),
        BuildState::Unknown => Some(
            "cannot be compared with this g-mesh - it published no build stamp, so it \
             predates this check; run `g-mesh stop`, or let the next MCP call replace it",
        ),
    }
}

fn describe_plugin(plugin: PluginState) -> String {
    match plugin {
        PluginState::Active { pid } => format!("active (pid {pid})"),
        PluginState::Orphaned { pid } => {
            format!("orphaned (pid {pid}) - no core is serving this project; run `g-mesh stop`")
        }
    }
}

fn describe_last_used(last_used: Option<&LastUsed>) -> String {
    match last_used {
        Some(record) => format!("{} UTC ({})", record.timestamp, humanize(record.idle)),
        None => "never recorded".to_string(),
    }
}

fn humanize(idle: Duration) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let seconds = idle.as_secs();
    match seconds {
        0..MINUTE => "just now".to_string(),
        MINUTE..HOUR => plural(seconds / MINUTE, "minute"),
        HOUR..DAY => plural(seconds / HOUR, "hour"),
        _ => plural(seconds / DAY, "day"),
    }
}

fn plural(count: u64, unit: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{count} {unit}{suffix} ago")
}

#[cfg(test)]
mod tests;
