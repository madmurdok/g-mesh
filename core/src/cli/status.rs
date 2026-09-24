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
use crate::gc::last_used::{self, LastUsed};
use crate::gc::warning;
use crate::storage::connection::project_dir;
use crate::watcher::staleness::mtime_millis;

/// Extensions the bundled JS/TS plugin will index, mirroring `grammarFor` in
/// plugins/typescript/src/extract.ts. Anything else on disk is not a file this
/// project's index is ever expected to cover, so counting it would make
/// coverage look permanently broken.
const SOURCE_EXTENSIONS: [&str; 8] = ["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx"];

/// Directory names excluded whatever `.gitignore` says, mirroring
/// `HARD_EXCLUDED_DIRS` in plugins/typescript/src/ignorePolicy.ts - the walk here
/// has to agree with the walk that built the index, or every file the plugin
/// skipped would read as a coverage gap.
const HARD_EXCLUDED_DIRS: [&str; 4] = [".git", "node_modules", "dist", ".claude"];

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
            discovered: 0,
            indexed: 0,
            dirty: 0,
            syntax_error_files: Vec::new(),
        }
    } else {
        index_status(project_root, &state_dir.join("index.db"))?
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
pub fn index_status(project_root: &Path, db_path: &Path) -> Result<IndexStatus> {
    let discovered = discover_source_files(project_root)?;

    // No index yet (never bootstrapped, or deleted by hand): everything on
    // disk is owed work, and none of it is covered.
    if !db_path.exists() {
        return Ok(IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
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

    Ok(IndexStatus {
        bulk_indexed: crate::storage::schema::bulk_index_completed(&conn)
            .context("failed to read whether the project has been fully walked")?,
        semantic_pass_completed: crate::storage::schema::semantic_pass_completed(&conn)
            .context("failed to read whether the project's semantic pass has completed")?,
        discovered: discovered.len(),
        indexed,
        dirty,
        syntax_error_files: syntax_error_files(&conn)?,
    })
}

struct SourceFile {
    /// Project-relative, forward-slash separated - the same spelling the
    /// `filePath` columns and the wire protocol use.
    relative: String,
    mtime_millis: i64,
}

/// Walks `project_root` for files the plugin would index, honoring
/// `.gitignore` and the hard exclusions the plugin's own walk applies.
///
/// Two deliberate divergences from that walk, both in the direction of doing
/// less: symlinks are not followed (the plugin follows them under a cycle
/// guard), and a file whose metadata cannot be read is skipped rather than
/// failing the report. Neither can make a broken index look healthy.
fn discover_source_files(project_root: &Path) -> Result<Vec<SourceFile>> {
    let mut files = Vec::new();
    let walk = WalkBuilder::new(project_root)
        // Matching the plugin's walk, which reads each directory's own
        // .gitignore and nothing else: no dotfile skipping, no rules from
        // above the project root, no global/`info/exclude` rules, and rules
        // honored even outside a git repository.
        .hidden(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            !entry.file_name().to_str().is_some_and(|name| HARD_EXCLUDED_DIRS.contains(&name))
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
        if !is_source_file(entry.path()) {
            continue;
        }
        let Some(relative) = relative_wire_path(project_root, entry.path()) else {
            continue;
        };
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

fn is_source_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
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
    // A daemon in its cold-start walk binds its socket and answers "still
    // indexing" per call (task 105) well before `bulk_indexed` flips - so
    // `!bulk_indexed` alone no longer means "nothing is happening about this".
    // Cross-referencing `report.core` is what tells "a walk is under way right
    // now" apart from "no daemon is doing anything about this at all", which
    // is the only case that still means "still owed".
    let daemon_alive = !matches!(report.core, CoreState::NotRunning);
    // D13 in `docs/architecture/lazy-indexing.md`: `report.phase` is the
    // running daemon's own word for where it stands, read off `index.phase`
    // rather than inferred from `bulk_indexed`/`core` alone - the fact GM-395's
    // lazy activation needs and the pre-existing fields cannot give on their
    // own, since a project can now sit `unindexed` for as long as nothing has
    // asked, indistinguishable from "about to be walked" under the old
    // `daemon_alive && !bulk_indexed` reasoning. `structural` and `ready` get
    // no line here, same as the pre-GM-395 "say nothing once the walk is
    // done" behaviour - `phase` is `None` whenever no daemon is running, which
    // falls through to the same two messages this reported before D13.
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
    match report.phase.as_deref() {
        Some("unindexed") => {
            let _ = writeln!(out, "  index:           not indexed yet - builds on the first tool call");
        }
        Some("walking") => {
            let _ = writeln!(out, "  index:           building now");
        }
        Some("embedding") => {
            let _ = writeln!(out, "  index:           structural index ready; embeddings being computed");
        }
        Some("failed") => {
            let _ = writeln!(
                out,
                "  index:           last build failed - see daemon log; retried on the next tool call"
            );
        }
        _ if !index.bulk_indexed => {
            if daemon_alive {
                let _ = writeln!(
                    out,
                    "  index:           building now - first walk in progress, nothing to restart"
                );
            } else {
                let _ = writeln!(out, "  index:           never fully walked - a cold start is still owed");
            }
        }
        _ => {}
    }
    let _ = writeln!(
        out,
        "  index coverage:  {:.1}% ({}/{} source files)",
        index.coverage() * 100.0,
        index.indexed,
        index.discovered
    );
    if !index.bulk_indexed && report.phase.as_deref() == Some("walking") {
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
        if index.semantic_pass_completed {
            let _ = writeln!(out, "  semantic pass:   complete");
        } else {
            let _ = writeln!(out, "  semantic pass:   never completed - run `g-mesh reindex` to repair it");
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
mod tests {
    use super::*;
    use crate::storage::schema;

    /// A project directory with the given files, and an index database
    /// alongside it that no daemon owns.
    struct Fixture {
        project: tempfile::TempDir,
        state: tempfile::TempDir,
    }

    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            let project = tempfile::tempdir().unwrap();
            for (path, contents) in files {
                let full = project.path().join(path);
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&full, contents).unwrap();
            }
            Self { project, state: tempfile::tempdir().unwrap() }
        }

        fn root(&self) -> &Path {
            self.project.path()
        }

        fn db_path(&self) -> PathBuf {
            self.state.path().join("index.db")
        }

        /// Opens (creating on first call) the index this fixture's status is
        /// computed against.
        fn index(&self) -> Connection {
            let conn = Connection::open(self.db_path()).unwrap();
            schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
            conn
        }

        /// Records the `File` node the plugin would emit for `path`.
        fn index_file(&self, conn: &Connection, path: &str, has_syntax_errors: bool) {
            conn.execute(
                "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol,
                                    endLine, endCol, language, hasSyntaxErrors)
                 VALUES (?1, 'File', ?1, ?1, ?1, 1, 0, 1, 0, 'typescript', ?2)",
                rusqlite::params![path, has_syntax_errors],
            )
            .unwrap();
        }

        /// Records the on-disk baseline the incremental path would write.
        fn record_baseline(&self, conn: &Connection, path: &str, mtime_millis: i64) {
            conn.execute(
                "INSERT INTO indexed_files (filePath, mtimeMillis, contentHash)
                 VALUES (?1, ?2, 'hash')",
                rusqlite::params![path, mtime_millis],
            )
            .unwrap();
        }

        fn current_mtime(&self, path: &str) -> i64 {
            mtime_millis(&fs::metadata(self.root().join(path)).unwrap()).unwrap()
        }

        fn status(&self) -> IndexStatus {
            index_status(self.root(), &self.db_path()).unwrap()
        }
    }

    #[test]
    fn a_project_with_no_index_owes_work_for_every_file_it_has() {
        let fixture = Fixture::new(&[("a.ts", "export const a = 1;"), ("b.ts", "export const b = 2;")]);

        let status = fixture.status();

        assert_eq!(status.discovered, 2);
        assert_eq!(status.indexed, 0);
        assert_eq!(status.dirty, 2);
        assert!(!status.bulk_indexed);
        assert_eq!(status.coverage(), 0.0);
    }

    #[test]
    fn a_fully_indexed_project_reports_complete_coverage_and_nothing_dirty() {
        let fixture = Fixture::new(&[("a.ts", "export const a = 1;"), ("src/b.tsx", "export const b = 2;")]);
        let conn = fixture.index();
        fixture.index_file(&conn, "a.ts", false);
        fixture.index_file(&conn, "src/b.tsx", false);
        // GM-264: the roll-up `record_bulk_index` now checks fires only once
        // every *present* language (here, just "typescript" - `index_file`'s
        // own doc comment) has its own `language_state.bulkIndexedAt` set -
        // see `storage::schema::record_bulk_index`'s doc comment.
        schema::record_language_bulk_indexed(&conn, "typescript", None).unwrap();
        schema::record_bulk_index(&conn).unwrap();

        let status = fixture.status();

        assert_eq!(status.discovered, 2);
        assert_eq!(status.indexed, 2);
        assert_eq!(status.dirty, 0, "a bulk-indexed file with no baseline is not stale");
        assert!(status.bulk_indexed);
        assert_eq!(status.coverage(), 1.0);
    }

    /// The acceptance criterion's "known dirty-queue size": one file the
    /// index has never seen, and one whose recorded baseline no longer
    /// matches what is on disk.
    #[test]
    fn dirty_counts_never_indexed_files_and_files_whose_baseline_went_stale() {
        let fixture = Fixture::new(&[
            ("fresh.ts", "export const fresh = 1;"),
            ("edited.ts", "export const edited = 1;"),
            ("new.ts", "export const brand_new = 1;"),
        ]);
        let conn = fixture.index();
        fixture.index_file(&conn, "fresh.ts", false);
        fixture.index_file(&conn, "edited.ts", false);
        // `new.ts` deliberately has no node at all.
        fixture.record_baseline(&conn, "fresh.ts", fixture.current_mtime("fresh.ts"));
        // A baseline from before the file was last written.
        fixture.record_baseline(&conn, "edited.ts", fixture.current_mtime("edited.ts") - 10_000);

        let status = fixture.status();

        assert_eq!(status.discovered, 3);
        assert_eq!(status.indexed, 2);
        assert_eq!(status.dirty, 2, "the never-indexed file and the stale one");
        assert!((status.coverage() - 2.0 / 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn files_the_plugin_could_only_partly_parse_are_listed() {
        let fixture = Fixture::new(&[
            ("broken.ts", "export function ("),
            ("also-broken.ts", "class {"),
            ("fine.ts", "export const fine = 1;"),
        ]);
        let conn = fixture.index();
        fixture.index_file(&conn, "broken.ts", true);
        fixture.index_file(&conn, "also-broken.ts", true);
        fixture.index_file(&conn, "fine.ts", false);

        let status = fixture.status();

        assert_eq!(status.syntax_error_files, vec!["also-broken.ts", "broken.ts"], "sorted");
    }

    /// The walk has to agree with the one that built the index, or every file
    /// the plugin never looked at would read as a permanent coverage hole.
    #[test]
    fn the_walk_skips_exactly_what_the_plugins_walk_skips() {
        let fixture = Fixture::new(&[
            ("keep.ts", ""),
            ("keep.mjs", ""),
            ("README.md", ""),
            ("styles.css", ""),
            (".gitignore", "ignored/\n"),
            ("ignored/hidden.ts", ""),
            ("node_modules/dep/index.ts", ""),
            ("dist/bundle.js", ""),
            (".git/hooks/pre-commit.js", ""),
            (".claude/worktrees/copy/a.ts", ""),
        ]);

        let mut found: Vec<String> =
            discover_source_files(fixture.root()).unwrap().into_iter().map(|f| f.relative).collect();
        found.sort();

        assert_eq!(found, vec!["keep.mjs", "keep.ts"]);
    }

    #[test]
    fn a_project_with_no_source_files_is_covered_rather_than_dividing_by_zero() {
        let fixture = Fixture::new(&[("README.md", "# nothing to index")]);

        let status = fixture.status();

        assert_eq!(status.discovered, 0);
        assert_eq!(status.coverage(), 1.0);
        assert_eq!(status.dirty, 0);
    }

    #[test]
    fn a_report_renders_every_field_it_was_asked_for() {
        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::Running { pid: 4242 },
            build: BuildState::Current,
            plugins: vec![PluginReport {
                language: "typescript".to_string(),
                state: PluginState::Active { pid: 4243 },
            }],
            suspended_languages: Vec::new(),
            last_used: Some(LastUsed {
                timestamp: "2026-07-31 00:00:00.000".to_string(),
                idle: Duration::from_secs(3 * 60 * 60),
            }),
            index: IndexStatus {
                bulk_indexed: true,
                semantic_pass_completed: true,
                discovered: 4,
                indexed: 3,
                dirty: 1,
                syntax_error_files: vec!["src/broken.ts".to_string()],
            },
            phase: None,
            front: None,
        };

        let rendered = render(&report);

        assert!(rendered.contains("running (pid 4242)"), "{rendered}");
        assert!(rendered.contains("daemon build:    this build"), "{rendered}");
        assert!(rendered.contains("active (pid 4243)"), "{rendered}");
        assert!(rendered.contains("3 hours ago"), "{rendered}");
        assert!(rendered.contains("75.0% (3/4 source files)"), "{rendered}");
        assert!(rendered.contains("1 awaiting reindex"), "{rendered}");
        assert!(rendered.contains("src/broken.ts"), "{rendered}");
        assert!(rendered.contains("semantic pass:   complete"), "{rendered}");
    }

    /// The gap task 62cc2d0f closes: a walked index whose semantic pass never
    /// finished must not read as fully healthy just because `bulk_indexed` is
    /// true - `status` is the explicit-surfacing half of the fix (the other
    /// half is the daemon retrying on its own next start).
    #[test]
    fn a_walked_index_with_no_completed_semantic_pass_is_called_out() {
        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::NotRunning,
            build: BuildState::NotRunning,
            plugins: Vec::new(),
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed: true,
                semantic_pass_completed: false,
                discovered: 4,
                indexed: 4,
                dirty: 0,
                syntax_error_files: Vec::new(),
            },
            phase: None,
            front: None,
        };

        let rendered = render(&report);

        assert!(
            rendered.contains("semantic pass:   never completed - run `g-mesh reindex` to repair it"),
            "an interrupted semantic pass must be surfaced explicitly, not silently folded into \
             a healthy-looking report:\n{rendered}"
        );
    }

    /// Task 108: a daemon mid-cold-start-walk (task 105 already made it
    /// reachable and answering, not merely running) must not be reported next
    /// to a line implying nothing is happening or that a restart is owed - the
    /// walk already in progress is exactly what would do the work a "cold
    /// start still owed" reading would suggest is missing.
    #[test]
    fn a_daemon_mid_cold_start_walk_reports_the_walk_in_progress_not_a_cold_start_owed() {
        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::Running { pid: 4242 },
            build: BuildState::Current,
            plugins: vec![PluginReport {
                language: "typescript".to_string(),
                state: PluginState::Active { pid: 4243 },
            }],
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed: false,
                semantic_pass_completed: false,
                discovered: 4,
                indexed: 1,
                dirty: 3,
                syntax_error_files: Vec::new(),
            },
            phase: Some("walking".to_string()),
            front: None,
        };

        let rendered = render(&report);

        assert!(rendered.contains("running (pid 4242)"), "{rendered}");
        assert!(rendered.contains("building now"), "{rendered}");
        assert!(
            !rendered.contains("cold start is still owed"),
            "a live daemon is already doing the walk this line would suggest is missing:\n{rendered}"
        );
        assert!(
            rendered.contains("3 awaiting the walk already in progress"),
            "the dirty count must not read as work nobody is doing:\n{rendered}"
        );
    }

    /// A minimal fixture for the GM-395 slice 2b tests below, which only vary
    /// `phase` and `bulk_indexed` - everything else about a live, unwalked
    /// project is incidental to what they check.
    fn phase_fixture(bulk_indexed: bool, phase: Option<&str>) -> Report {
        Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::Running { pid: 4242 },
            build: BuildState::Current,
            plugins: Vec::new(),
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed,
                semantic_pass_completed: false,
                discovered: 4,
                indexed: if bulk_indexed { 4 } else { 0 },
                dirty: 4,
                syntax_error_files: Vec::new(),
            },
            phase: phase.map(str::to_string),
            front: None,
        }
    }

    /// D13 in `docs/architecture/lazy-indexing.md`: an idle daemon that owns
    /// its project but has never been asked to walk it (GM-395's lazy
    /// activation) must read as "not indexed yet", not as the pre-GM-395
    /// "cold start still owed" - that line implied nothing was happening
    /// about it, which a live daemon waiting for the first tool call is not.
    #[test]
    fn unindexed_phase_reports_not_indexed_yet_rather_than_cold_start_owed() {
        let rendered = render(&phase_fixture(false, Some("unindexed")));
        assert!(
            rendered.contains("index:           not indexed yet - builds on the first tool call"),
            "{rendered}"
        );
        assert!(!rendered.contains("cold start is still owed"), "{rendered}");
        assert!(
            !rendered.contains("building now"),
            "an unindexed project is idle, not building:\n{rendered}"
        );
    }

    /// D13: `embedding` is the phase covering "the structural walk is done
    /// and answering, the embedding backfill pass is running" - distinct from
    /// both `walking` (no structural answers yet) and silence (nothing left
    /// to say once the whole project, embeddings included, is `ready`).
    #[test]
    fn embedding_phase_reports_structural_ready_with_embeddings_in_progress() {
        let rendered = render(&phase_fixture(true, Some("embedding")));
        assert!(
            rendered.contains("index:           structural index ready; embeddings being computed"),
            "{rendered}"
        );
    }

    /// D13: `failed` names what happened and that the next tool call retries
    /// it (`IndexingStatus::activation_failed`) - the daemon log is where the
    /// actual failure message lives (`Phase::Failed`'s own doc comment), so
    /// this line only has to point there, not repeat it.
    #[test]
    fn failed_phase_reports_the_last_build_failed_and_will_be_retried() {
        let rendered = render(&phase_fixture(false, Some("failed")));
        assert!(
            rendered.contains(
                "index:           last build failed - see daemon log; retried on the next tool call"
            ),
            "{rendered}"
        );
    }

    /// `structural` and `ready` are deliberately silent on this line (D13
    /// names messages only for `unindexed`, `walking`, `embedding` and
    /// `failed`) - once the walk itself is done, `status` has nothing left to
    /// add here that the coverage/dirty lines below do not already say.
    #[test]
    fn structural_and_ready_phases_print_no_index_line() {
        for phase in ["structural", "ready"] {
            let rendered = render(&phase_fixture(true, Some(phase)));
            assert!(
                !rendered.contains("index:"),
                "phase {phase:?} must not print an \"index:\" line (\"index coverage:\" is a different \
                 line and is unaffected):\n{rendered}"
            );
        }
    }

    #[test]
    fn a_dead_project_renders_as_such_without_pretending_to_know_pids() {
        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::NotRunning,
            build: BuildState::NotRunning,
            plugins: vec![PluginReport {
                language: "typescript".to_string(),
                state: PluginState::Orphaned { pid: 99 },
            }],
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed: false,
                semantic_pass_completed: false,
                discovered: 2,
                indexed: 0,
                dirty: 2,
                syntax_error_files: Vec::new(),
            },
            phase: None,
            front: None,
        };

        let rendered = render(&report);

        assert!(rendered.contains("daemon core:     not running"), "{rendered}");
        assert!(
            !rendered.contains("daemon build:"),
            "a project with no daemon has no build to report on:\n{rendered}"
        );
        assert!(rendered.contains("orphaned (pid 99)"), "{rendered}");
        assert!(rendered.contains("never recorded"), "{rendered}");
        assert!(rendered.contains("never fully walked"), "{rendered}");
        assert!(rendered.contains("syntax errors:   none"), "{rendered}");
    }

    /// A pid file naming a live process under a live core reads as active;
    /// under no core at all it reads as orphaned - `classify_plugin` only
    /// ever runs on a pid `plugin_reports` has already confirmed is alive, so
    /// there is no "nothing recorded" case left for it to classify (see
    /// [`PluginState`]'s doc comment for where that case went).
    #[test]
    fn a_live_pid_reads_as_active_under_a_core_and_orphaned_without_one() {
        assert_eq!(classify_plugin(7, CoreState::Running { pid: 1 }), PluginState::Active { pid: 7 });
        assert_eq!(
            classify_plugin(7, CoreState::NotAccepting { pid: 1 }),
            PluginState::Active { pid: 7 },
            "a core that is up but not yet accepting connections still counts as serving it"
        );
        assert_eq!(classify_plugin(7, CoreState::NotRunning), PluginState::Orphaned { pid: 7 });

        let described = describe_plugin(PluginState::Orphaned { pid: 7 });
        assert!(described.contains("orphaned"), "{described}");
        assert!(described.contains("g-mesh stop"), "{described}");
    }

    /// [`plugin_reports`]'s own acceptance criterion: one entry per
    /// `plugin-<language>.pid` file actually present, sorted, a dead pid file
    /// dropped rather than reported, and a project with none at all reads as
    /// an empty list rather than an error - `status` must never panic or fail
    /// just because no language has ever been touched yet.
    #[test]
    fn plugin_reports_lists_one_entry_per_live_pid_file_sorted_by_language() {
        let state = tempfile::tempdir().unwrap();
        let live = std::process::id();
        fs::write(state.path().join("plugin-typescript.pid"), live.to_string()).unwrap();
        fs::write(state.path().join("plugin-python.pid"), live.to_string()).unwrap();
        // A pid nothing alive holds - large, and vanishingly unlikely to
        // collide with a real process on the machine running this test.
        fs::write(state.path().join("plugin-go.pid"), "999999999").unwrap();

        let reports = plugin_reports(state.path(), CoreState::Running { pid: 1 });

        assert_eq!(
            reports,
            vec![
                PluginReport { language: "python".to_string(), state: PluginState::Active { pid: live } },
                PluginReport { language: "typescript".to_string(), state: PluginState::Active { pid: live } },
            ],
            "sorted by language, and the dead go.pid dropped rather than reported: {reports:?}"
        );
    }

    /// A project nothing has ever spawned a plugin for (or one where every
    /// language has since gone idle - the registry removes a language's pid
    /// file on every sleep, same as before) renders one summary line, not a
    /// per-language guess this command cannot actually back up.
    #[test]
    fn a_report_with_no_plugin_pid_files_renders_a_summary_line() {
        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::Running { pid: 4242 },
            build: BuildState::Current,
            plugins: Vec::new(),
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed: true,
                semantic_pass_completed: true,
                discovered: 0,
                indexed: 0,
                dirty: 0,
                syntax_error_files: Vec::new(),
            },
            phase: None,
            front: None,
        };

        let rendered = render(&report);

        assert!(rendered.contains("none active"), "{rendered}");
        assert!(
            !rendered.contains("plugin ("),
            "no per-language line may be printed when nothing has a pid file:\n{rendered}"
        );
    }

    /// The whole point of the line: a daemon left behind by an upgrade
    /// answers everything else in this report perfectly well, so this is the
    /// only place the report can say something is wrong at all.
    #[test]
    fn a_daemon_left_behind_by_an_upgrade_is_called_out_with_what_to_do_about_it() {
        for (state, expected) in [
            (BuildState::Outdated, "older than this g-mesh"),
            (BuildState::PluginChanged, "JS/TS plugin that has been rebuilt"),
            (BuildState::Unknown, "published no build stamp"),
        ] {
            let described = describe_build(state).expect("a running daemon always reports a build");
            assert!(described.contains(expected), "{state:?} rendered as {described}");
            assert!(described.contains("g-mesh stop"), "{state:?} must say what to do: {described}");
        }
    }

    /// A plugin-only rebuild must not be reported as an old core binary: the
    /// binary is the one the person asking has just built, and being told
    /// otherwise sends them to look at the wrong half of the pipeline.
    #[test]
    fn a_daemon_holding_a_rebuilt_plugin_is_named_as_that_and_not_as_an_old_binary() {
        let state = tempfile::tempdir().unwrap();
        let mut incumbent = build_stamp::of_running_process().unwrap();
        incumbent.plugin = format!("{}-before", incumbent.plugin);
        build_stamp::write(&daemon::build_stamp_path_in(state.path()), &incumbent).unwrap();

        assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::PluginChanged);
        let described = describe_build(BuildState::PluginChanged).unwrap();
        assert!(!described.contains("older than this g-mesh"), "{described}");
    }

    #[test]
    fn a_daemon_that_published_this_builds_stamp_reads_as_current() {
        let state = tempfile::tempdir().unwrap();
        let ours = build_stamp::of_running_process().unwrap();
        build_stamp::write(&daemon::build_stamp_path_in(state.path()), &ours).unwrap();

        assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Current);
        // Still comparable for a daemon that is up but not answering on its
        // socket: the stamp is published before the bind, not after.
        assert_eq!(build_state(CoreState::NotAccepting { pid: 1 }, state.path()), BuildState::Current);
    }

    #[test]
    fn a_daemon_that_published_an_older_builds_stamp_reads_as_outdated() {
        let state = tempfile::tempdir().unwrap();
        let mut older = build_stamp::of_running_process().unwrap();
        older.exe_mtime_millis -= 24 * 60 * 60 * 1000;
        build_stamp::write(&daemon::build_stamp_path_in(state.path()), &older).unwrap();

        assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Outdated);
    }

    /// The state every daemon in the wild is in the first time a build
    /// carrying this check meets it.
    #[test]
    fn a_daemon_that_published_no_stamp_at_all_reads_as_uncomparable() {
        let state = tempfile::tempdir().unwrap();

        assert_eq!(build_state(CoreState::Running { pid: 1 }, state.path()), BuildState::Unknown);
        assert_eq!(build_state(CoreState::NotRunning, state.path()), BuildState::NotRunning);
    }

    #[test]
    fn idle_durations_read_the_way_a_human_would_say_them() {
        assert_eq!(humanize(Duration::from_secs(3)), "just now");
        assert_eq!(humanize(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(humanize(Duration::from_secs(45 * 60)), "45 minutes ago");
        assert_eq!(humanize(Duration::from_secs(2 * 60 * 60)), "2 hours ago");
        assert_eq!(humanize(Duration::from_secs(90 * 24 * 60 * 60)), "90 days ago");
    }

    /// Task GM-274's status acceptance criterion, exercised end to end
    /// through the real production write path rather than a hand-written
    /// marker: a real `PluginSupervisor` (over the same memory-hungry fixture
    /// `daemon::lifecycle`'s own acceptance test uses) whose process tree is
    /// found over an artificially low `memoryLimitMb` writes a real
    /// `plugin-<language>.suspended` marker into this project's *actual*
    /// state directory (`storage::connection::project_dir`, not an arbitrary
    /// tempdir - the same one `collect` resolves for any project), and
    /// `status::collect`/`render` - a *separate* process's-worth of code from
    /// the daemon that wrote it, reading only off disk - reports it.
    ///
    /// # GM-390: reached through the injected sampler, not the real one
    ///
    /// This used to call `check_memory_limit()`, the real, OS-backed sampler -
    /// and so, exactly like `daemon::lifecycle`'s own acceptance test
    /// before GM-340 fixed it there, depended on `sysinfo` reporting this
    /// fixture's 200MB buffer as over `memoryLimitMb` on *two* consecutive
    /// whole-system scans. Seen failing on x86_64-apple-darwin: a first scan
    /// read 230MB (over the 100MB limit), the confirming one 30MB - roughly a
    /// bare Node process's idle RSS, i.e. the confirming scan did not see the
    /// held buffer at all. `daemon::memory`'s own doc comment says why a
    /// whole-tree snapshot can move like that between two calls milliseconds
    /// apart with nothing about the fixture having changed: a shared runner
    /// under other concurrent tests' process churn, or swap pressure, is
    /// enough - `process_tree_sample`'s doc comment measured falls of
    /// 170-200MB between two such snapshots taken a few milliseconds apart on
    /// this repository's own `cargo test --workspace`, under real load.
    ///
    /// What this test is judged on is not "can two real samples confirm each
    /// other" - that guard's own decision logic (an unconfirmed reading
    /// leaves the plugin running; a confirmed one suspends it; an under-limit
    /// reading costs one sample) already has deterministic, seam-driven
    /// coverage in `daemon::lifecycle`'s
    /// `an_unconfirmed_over_limit_sample_leaves_the_plugin_running` and
    /// `a_confirmed_over_limit_sample_suspends_the_language`. This test's own
    /// subject is narrower and different: that once a suspension *has*
    /// happened, through the real production write path, a status read in a
    /// separate process finds it on disk. So the two readings are scripted
    /// (both comfortably over the limit, confirming each other every time)
    /// rather than left to a real sampler that has already been shown to
    /// disagree with itself between calls on a busy machine - deterministic
    /// input to the same real write path, not a weaker test.
    #[test]
    fn a_daemon_suspended_language_is_reported_by_a_separate_status_read() {
        use crate::daemon::lifecycle::PluginSupervisor;
        use crate::daemon::manifest::read_manifest;
        use crate::daemon::test_plugin;
        use crate::embedding::EmbeddingPipeline;
        use std::sync::Arc;

        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        let plugin_dir = test_plugin::install_memory_hungry(plugins.path(), "heavy", &[".heavy-src"]);
        let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");

        let state_dir = project_dir(project.path()).expect("failed to resolve the state directory");
        std::fs::create_dir_all(&state_dir).unwrap();
        let pid_file = state_dir.join("plugin-heavy.pid");

        let supervisor = PluginSupervisor::start(
            project.path(),
            manifest,
            pid_file,
            None,
            Some(100),
            Arc::new(EmbeddingPipeline::disabled()),
        )
        .expect("the fixture plugin must start");

        // The write path this task adds - not a hand-written marker file.
        // GM-390: through the injected sampler (see this test's own doc
        // comment on `check_memory_limit_sampled_by` vs `check_memory_limit`)
        // - two scripted readings, both over the 100MB limit and confirming
        // each other, so the real production write path below still runs off
        // a genuine over-limit-then-confirmed decision, just not off two
        // readings a real, shared-machine sampler can disagree with itself
        // about.
        supervisor.check_memory_limit_sampled_by(|_pid| Some(230));
        assert!(supervisor.is_semantic_suspended(), "the fixture must actually have suspended");

        let report = collect(project.path()).expect("status must still collect over a suspended project");
        assert_eq!(report.suspended_languages.len(), 1, "{:?}", report.suspended_languages);
        assert_eq!(report.suspended_languages[0].language, "heavy");
        assert!(
            report.suspended_languages[0].reason.contains("memoryLimitMb"),
            "{:?}",
            report.suspended_languages[0]
        );

        let rendered = render(&report);
        assert!(rendered.contains("semantic (heavy): suspended"), "{rendered}");
        assert!(rendered.contains("memoryLimitMb"), "{rendered}");

        supervisor.sleep_now("test cleanup");
    }

    /// The other half: a project nothing ever suspended reports no suspended
    /// languages at all, and `render` prints no `semantic (...)` line - the
    /// ordinary, overwhelmingly common case must not grow a line nobody asked
    /// for.
    #[test]
    fn a_project_with_no_suspension_marker_reports_none() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(suspended_language_reports(state.path()), Vec::new());

        let report = Report {
            project_root: PathBuf::from("/tmp/project"),
            project_id: "a1b2c3d4e5f6a7b8".to_string(),
            state_dir: PathBuf::from("/home/u/.g-mesh/projects/a1b2c3d4e5f6a7b8"),
            core: CoreState::NotRunning,
            build: BuildState::NotRunning,
            plugins: Vec::new(),
            suspended_languages: Vec::new(),
            last_used: None,
            index: IndexStatus {
                bulk_indexed: false,
                semantic_pass_completed: false,
                discovered: 0,
                indexed: 0,
                dirty: 0,
                syntax_error_files: Vec::new(),
            },
            phase: None,
            front: None,
        };
        let rendered = render(&report);
        assert!(!rendered.contains("semantic ("), "{rendered}");
    }
}
