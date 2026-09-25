//! `g-mesh status`: what the current project's daemon, plugin and index are
//! actually doing.
//!
//! Everything is answered from outside the daemon (the recorded pids, the
//! socket, `index.db`, and the phase word a running daemon publishes to
//! `index.phase`, D13 in `docs/architecture/lazy-indexing.md`), never by
//! asking the daemon: `status` matters most when the daemon is not well.
//!
//! - **Daemon core**: the pid in `daemon.pid` plus whether the socket accepts
//!   connections; either alone lies (a recycled pid looks alive, a socket file
//!   outlives its process). The socket is bound before the cold-start walk,
//!   so "running" does not mean "ready": the `index:` line reports that.
//! - **Daemon build**: the stamp a live daemon publishes about its executable
//!   (`daemon::build_stamp`), compared with this command's own; nothing else
//!   in the report can show up a daemon that outlived an upgrade.
//! - **Plugins**: one line per `plugin-<language>.pid` file
//!   (`daemon::registry::discovered_pid_files`). A language with no pid file
//!   is not reported by name: "never touched" and "spawned, now asleep" are
//!   indistinguishable from outside the daemon, so only "active", "orphaned"
//!   and one summary line when there are none are reported.
//! - **Index phase**: the word in `index.phase` (`daemon::read_phase_in`):
//!   `unindexed`, `walking`, `structural`, `embedding`, `ready` or `failed`.
//!   A project can sit `unindexed` under a live, idle daemon, so a live
//!   daemon does not imply a walk.
//! - **Index progress**: `index.progress` counters, shown only while their
//!   pid is the live daemon's ([`live_progress`]).
//! - **Dirty files / index coverage**: a gitignore-aware walk cross-referenced
//!   against `File` nodes and `indexed_files` baselines (see [`IndexStatus`]).
//! - **Files with syntax errors**: the plugin's `hasSyntaxErrors` flag.

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
    /// the index is complete: the socket is bound before the cold-start walk,
    /// and a tool call waits for the walk. `render` reads this with the phase
    /// and `IndexStatus::bulk_indexed`, so a walk in progress reads as one.
    Running { pid: u32 },
    /// Its pid is alive but nothing answers on the socket: a daemon that died
    /// without clearing its pid file, or a wedged one (not one busy indexing,
    /// since the bind precedes the walk).
    NotAccepting { pid: u32 },
    /// Nothing in `daemon.pid`, but a live process still holds this project's
    /// singleton lock: no other daemon can take the project over and nothing
    /// can reach this one. Its own variant because every `daemon.pid`-keyed
    /// check reports it as nothing running.
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
    /// Running, but nothing usable is on record about its build. Reported
    /// rather than called current: "not compared" is not "matched".
    Unknown,
}

/// Whether one language's plugin process is up, reported per language in
/// [`PluginReport`]. There is no asleep/not-running variant: "no pid file"
/// covers both "never touched" and "spawned, now idle", which this command
/// cannot tell apart, so [`plugin_reports`] only builds a report for a
/// language with a pid file and [`render`] prints one summary line when none
/// has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Running under a live core.
    Active { pid: u32 },
    /// Alive with no core serving this project: a leaked child of a daemon
    /// that died without closing its stdin. `g-mesh stop` clears it.
    Orphaned { pid: u32 },
}

/// One language's plugin, as `status` reports it: one per
/// `plugin-<language>.pid` file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReport {
    pub language: String,
    pub state: PluginState,
}

/// One language whose semantic passes are suspended right now: written by a
/// running daemon's `PluginSupervisor::check_memory_limit` when `[plugin]
/// memoryLimitMb` catches its process tree over the limit, and read here off
/// disk (`daemon::registry::discovered_suspended_markers`). A listing of its
/// own, not a field on [`PluginReport`]: a suspended plugin has no pid file
/// (suspension removes it, like idle sleep), so the marker is the only trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedLanguage {
    pub language: String,
    /// The reason `check_memory_limit` recorded (the configured limit and the
    /// measured figure), verbatim: nothing else about why survives the daemon.
    pub reason: String,
}

/// What the index covers, and what it still owes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    /// Whether a full project walk has ever finished (`meta.bulkIndexedAt`).
    pub bulk_indexed: bool,
    /// Whether the whole-project semantic pass after a walk has ever finished
    /// (`meta.semanticPassAt`). Independent of `bulk_indexed`: every walk
    /// records `bulkIndexedAt` before asking for this pass, so an interrupted
    /// pass leaves `bulk_indexed` true and this false, the combination
    /// [`render`] calls out (see `daemon::semantic`'s module doc).
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
    /// Files the index still owes work for: never indexed, or indexed against
    /// a recorded baseline that no longer matches the file on disk.
    ///
    /// A bulk-indexed file with no `indexed_files` row is *not* counted: the
    /// walk leaves no baseline when it cannot prove it saw the current bytes
    /// (`watcher::staleness::record_walk_baselines`), and older indexes have
    /// none at all, so a missing baseline means "not checked since", not
    /// "stale".
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
    /// Every language with a pid file on disk right now, sorted (see
    /// [`PluginState`] for what "no pid file" means).
    pub plugins: Vec<PluginReport>,
    /// Every language suspended by `[plugin] memoryLimitMb`, sorted;
    /// independent of `plugins` (see [`SuspendedLanguage`]).
    pub suspended_languages: Vec<SuspendedLanguage>,
    pub last_used: Option<LastUsed>,
    pub index: IndexStatus,
    /// The word a running daemon last published to `index.phase` (D13);
    /// `None` when no daemon serves the project (`release_state_files`
    /// removes it). It tells "idle, never indexed" (`unindexed`) from
    /// "building now" (`walking`), which `bulk_indexed` alone cannot.
    pub phase: Option<String>,
    /// Set when `phase` reads `front` (D11): the folder's projects as a fresh
    /// bounded walk counts them. `index` is then left empty: a whole-folder
    /// file walk is the cost a front exists to avoid.
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

/// Reports on the project the current directory belongs to, then prints the
/// GC idle-project warning (`gc::warning`) when it applies. That warning is
/// printed for this human-facing command only, never for `mcp-shim` or
/// `daemon`, whose stdout is protocol traffic.
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
        // No live pid file does not end the enquiry: a daemon that removed its
        // pid file and failed to exit still holds the singleton lock. The lock
        // is what `daemon::acquire_singleton_lock` acts on, so this report and
        // the next bootstrap cannot disagree.
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
/// `NotAccepting` is compared too: the stamp is published before the socket
/// is bound (`daemon::run`). Infallible: every failure (no executable to
/// stat, a missing or unreadable stamp) is `Unknown`, never a reason to skip
/// the rest of the report.
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

/// Every language with a live pid file in `state_dir`, sorted by language
/// (the order `discovered_pid_files` returns). A pid file naming a dead
/// process is dropped, as in every pid-file read in this daemon.
fn plugin_reports(state_dir: &Path, core: CoreState) -> Vec<PluginReport> {
    crate::daemon::registry::discovered_pid_files(state_dir)
        .into_iter()
        .filter_map(|(language, path)| {
            let pid = daemon::read_pid_file(&path).filter(|&pid| daemon::is_process_alive(pid))?;
            Some(PluginReport { language, state: classify_plugin(pid, core) })
        })
        .collect()
}

/// Every `plugin-<language>.suspended` marker in `state_dir`. Unlike
/// [`plugin_reports`] it does not depend on `core`: a suspension is a fact
/// about the state directory, not about a running daemon.
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
        // A wedged core counts as gone: a plugin alive under it is as orphaned
        // as one whose core exited, and `g-mesh stop` clears both.
        CoreState::NotRunning | CoreState::Wedged { .. } => PluginState::Orphaned { pid },
        CoreState::Running { .. } | CoreState::NotAccepting { .. } => PluginState::Active { pid },
    }
}

/// Cross-references what is on disk against what the index knows about it.
/// Takes the database path explicitly so it can be tested against a
/// hand-built index.
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
        // While a live daemon is working, it is the one that asks again, so
        // the line carries no repair advice.
        if reason == daemon::semantic::NOT_RUN_REASON {
            let asks_again = if in_progress.is_some() {
                "the running daemon asks again"
            } else {
                "the next daemon start or `g-mesh reindex` asks again"
            };
            lines.push(format!(
                "  semantic pass:   {language} pending - its plugin was asleep or memory-suspended; {asks_again}"
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
/// honoring `.gitignore`, [`BASELINE_EXCLUDED_DIRS`] and each language's
/// `[plugin.workspace] exclude_dirs`; the per-file decision is
/// [`DiscoveredPlugins::indexing_language`], the watcher's filter. Each
/// plugin walks in its own process, so this mirrors their shared manifest
/// rules rather than reusing a walk. It diverges only toward doing less (no
/// symlinks followed, unreadable metadata skipped), which cannot make a
/// broken index look healthy.
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

    // Only once a walk has landed: before that `index:` already says what is
    // owed. After it, `semantic_pass_completed` is what tells an interrupted
    // semantic pass (a killed plugin, a crash, a timeout) from a finished
    // index (see `daemon::semantic`'s module doc).
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

/// `report.progress`, only when the live daemon wrote it: a running daemon
/// rewrites the file with its own pid at start, so any other pid is a
/// leftover. The pid is corroborated by the socket (`CoreState::Running`), so
/// a recycled pid alone does not make a leftover look live.
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

/// `None` when nothing is running. Both unhealthy states name `g-mesh stop`
/// even though a shim replaces an outdated daemon on its own: someone running
/// `status` is asking now, and stopping is harmless if the shim got there
/// first.
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
