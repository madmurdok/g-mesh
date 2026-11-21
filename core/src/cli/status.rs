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
//!   (`StateDir::plugin_pid_files`). A language with no pid file
//!   is not reported by name: "never touched" and "spawned, now asleep" are
//!   indistinguishable from outside the daemon, so only "active", "orphaned"
//!   and one summary line when there are none are reported.
//! - **Index phase**: the word in `index.phase` (`StateDir::read_phase`):
//!   `unindexed`, `walking`, `structural`, `embedding`, `ready` or `failed`.
//!   A project can sit `unindexed` under a live, idle daemon, so a live
//!   daemon does not imply a walk.
//! - **Index progress**: `index.progress` counters, shown only while their
//!   pid is the live daemon's ([`live_progress`]).
//! - **Dirty files / index coverage**: a gitignore-aware walk cross-referenced
//!   against `File` nodes and `indexed_files` baselines (see [`Coverage`]).
//!   The only part that costs one `stat` per project file, so it runs only
//!   under `--full` ([`Mode::Full`]); the default view says it was not checked.
//! - **Languages**: what the last walk recorded per language
//!   (`language_outcome`, ADR 0021) beside the version of each installed
//!   plugin, plus installed plugins the last walk did not see
//!   ([`LanguageSection`]).
//! - **Files with syntax errors**: the plugin's `hasSyntaxErrors` flag.
//!
//! `--json` prints the same report as one object ([`to_json`]).
//!
//! The index's half of the report (coverage, languages, semantic pass) is
//! read by [`crate::index_health`]; this module adds the daemon, plugin and
//! build state around it and renders the whole.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::control::lock::{self, DaemonLock};
use crate::daemon;
use crate::daemon::build_stamp::{self, Vintage};
use crate::embedding::rerank::{OutcomeRecord, RerankModelState, RerankOutcome, RerankVariant};
use crate::gc::last_used::{self, LastUsed};
use crate::gc::warning;
use crate::index_health::{index_status, language_retries, language_section, semantic_pass_lines};
pub use crate::index_health::{Coverage, IndexStatus, LanguageSection, Mode};
use crate::indexing_status::{group_thousands, ProgressSnapshot};
use crate::paths::state_dir::{read_pid_file, StateDir};
use crate::plugin_manifest;
use crate::storage::language_outcome::LanguageOutcome;
use crate::storage::schema::MAX_LANGUAGE_RETRIES;

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
    /// The daemon started from this very executable, but is holding a
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
/// disk (`StateDir::suspended_markers`). A listing of its
/// own, not a field on [`PluginReport`]: a suspended plugin has no pid file
/// (suspension removes it, like idle sleep), so the marker is the only trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendedLanguage {
    pub language: String,
    /// The reason `check_memory_limit` recorded (the configured limit and the
    /// measured figure), verbatim: nothing else about why survives the daemon.
    pub reason: String,
}

/// The languages part of the report: the index's outcomes and the plugins
/// installed now (`plugin_manifest::discover`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguagesReport {
    pub section: LanguageSection,
    /// `language -> plugin_version` of every discovered plugin, or the
    /// discovery error (a malformed `plugin.toml`) on one line.
    pub installed: std::result::Result<BTreeMap<String, String>, String>,
    /// Each failed language's automatic retries since the last full walk
    /// (`storage::schema::language_retries`); a language with none is absent.
    pub retries: BTreeMap<String, u32>,
}

/// One row of the languages block: a recorded outcome, or `None` for a
/// plugin installed now that the last walk did not see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageRow<'a> {
    pub language: &'a str,
    pub outcome: Option<&'a LanguageOutcome>,
    /// The installed plugin's version; `None` when not installed or when
    /// discovery failed.
    pub plugin_version: Option<&'a str>,
    /// The language's automatic retries since the last full walk.
    pub retries: u32,
}

impl LanguagesReport {
    /// Every row, sorted by language: the recorded outcomes, plus each
    /// installed plugin without one. Empty unless outcomes are recorded.
    pub fn rows(&self) -> Vec<LanguageRow<'_>> {
        let LanguageSection::Recorded(outcomes) = &self.section else {
            return Vec::new();
        };
        let installed = self.installed.as_ref().ok();
        let version = |language: &str| installed.and_then(|map| map.get(language)).map(String::as_str);
        let mut rows: Vec<LanguageRow<'_>> = outcomes
            .iter()
            .map(|(language, outcome)| LanguageRow {
                language,
                outcome: Some(outcome),
                plugin_version: version(language),
                retries: self.retries.get(language).copied().unwrap_or(0),
            })
            .collect();
        if let Some(installed) = installed {
            for (language, plugin_version) in installed {
                if !outcomes.iter().any(|(recorded, _)| recorded == language) {
                    rows.push(LanguageRow {
                        language,
                        outcome: None,
                        plugin_version: Some(plugin_version),
                        retries: 0,
                    });
                }
            }
        }
        rows.sort_by(|a, b| a.language.cmp(b.language));
        rows
    }
}

/// Everything `g-mesh status` prints, as data.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub mode: Mode,
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
    pub languages: LanguagesReport,
    pub rerank_model: RerankStatus,
}

/// The rerank as `g-mesh status` reports it.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankStatus {
    /// The live daemon's own outcome, read from its state directory: it holds
    /// under the daemon's environment, which may differ from this shell's.
    Daemon(OutcomeRecord),
    /// No outcome from a live daemon (none running, or no `search_code` call
    /// has resolved it yet): the selected variant's model on disk under this
    /// shell's environment.
    OnDisk { daemon_alive: bool, variant: RerankVariant, state: RerankModelState },
}

/// The live daemon's [`OutcomeRecord`] in `state_dir`, else the on-disk check.
/// A record whose `pid` is not the live daemon's is a leftover and ignored.
fn rerank_status(core: CoreState, state_dir: &Path) -> RerankStatus {
    let live_pid = match core {
        CoreState::Running { pid } | CoreState::NotAccepting { pid } | CoreState::Wedged { pid } => Some(pid),
        CoreState::NotRunning => None,
    };
    match OutcomeRecord::read(&StateDir::at(state_dir).rerank_outcome_file())
        .filter(|record| Some(record.pid) == live_pid)
    {
        Some(record) => RerankStatus::Daemon(record),
        None => {
            let settings = crate::config::embedding::rerank_settings();
            RerankStatus::OnDisk {
                daemon_alive: live_pid.is_some(),
                variant: settings.variant,
                state: RerankModelState::of(settings),
            }
        }
    }
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
/// `daemon`, whose stdout is protocol traffic; under `--json` it goes to
/// stderr so stdout stays one JSON object. Exits 0 whatever the report says.
pub fn run(full: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let mode = if full { Mode::Full } else { Mode::Light };
    let report = collect(&cwd, mode)?;
    if json {
        let text = serde_json::to_string_pretty(&to_json(&report)).context("failed to encode the status")?;
        println!("{text}");
        if let Some(text) = warning::stale_projects_warning()? {
            eprint!("{text}");
        }
    } else {
        print!("{}", render(&report));
        warning::maybe_print_stale_projects_warning()?;
    }
    Ok(())
}

/// Gathers every field of the report for `project_root`. Only
/// [`Mode::Full`] walks the project's files.
pub fn collect(project_root: &Path, mode: Mode) -> Result<Report> {
    let sd = StateDir::of(project_root).context("failed to resolve the project's state directory")?;
    let state_dir = sd.path().to_path_buf();
    let project_id =
        state_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();

    let core = core_state(project_root)?;
    let phase = sd.read_phase();
    let front = (phase.as_deref() == Some(daemon::front::FRONT_PHASE)).then(|| {
        let walk = crate::project_detect::walk(project_root, crate::project_detect::Limits::default());
        FrontSummary { projects: walk.candidates.len(), truncated: walk.truncated }
    });
    // The same discovery the daemon runs at startup, so the files counted
    // here are the files the discovered plugins index, in every language.
    // A failure is reported rather than fatal, except where the walk needs
    // the plugins to know which files count.
    let discovered = plugin_manifest::discover(&plugin_manifest::default_roots());
    if mode == Mode::Full && front.is_none() {
        if let Err(err) = &discovered {
            anyhow::bail!("failed to discover language plugins: {err:#}");
        }
    }
    let installed = match &discovered {
        Ok(plugins) => Ok(plugins
            .manifests
            .iter()
            .map(|(language, manifest)| (language.clone(), manifest.plugin_version.clone()))
            .collect()),
        Err(err) => Err(format!("{err:#}")),
    };
    let plugins = discovered.unwrap_or_default();

    let db_path = sd.index_db();
    let (index, section) = if front.is_some() {
        let index = IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            pending_reindex: Vec::new(),
            semantic_pending: Vec::new(),
            semantic_stale: Vec::new(),
            semantic_leftovers: Vec::new(),
            coverage: None,
            syntax_error_files: Vec::new(),
        };
        (index, LanguageSection::Front)
    } else {
        // A phase word left by a dead daemon says nothing about a walk now.
        let live_phase = if matches!(core, CoreState::NotRunning) { None } else { phase.as_deref() };
        let walk_live = matches!(live_phase, Some("walking" | "unindexed"));
        (index_status(project_root, &db_path, &plugins, mode)?, language_section(&db_path, walk_live)?)
    };
    let retries = if matches!(section, LanguageSection::Recorded(_)) {
        language_retries(&db_path)
    } else {
        BTreeMap::new()
    };
    Ok(Report {
        mode,
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
        progress: crate::indexing_status::read_progress(&sd),
        languages: LanguagesReport { section, installed, retries },
        rerank_model: rerank_status(core, &state_dir),
        project_root: project_root.to_path_buf(),
        state_dir,
    })
}

fn core_state(project_root: &Path) -> Result<CoreState> {
    let state_dir = StateDir::of(project_root)?;
    let recorded = read_pid_file(&state_dir.pid_file()).filter(|&pid| crate::process::is_alive(pid));
    let Some(pid) = recorded else {
        // No live pid file does not end the enquiry: a daemon that removed its
        // pid file and failed to exit still holds the singleton lock. The lock
        // is what `daemon::acquire_singleton_lock` acts on, so this report and
        // the next bootstrap cannot disagree.
        return Ok(match lock::inspect_daemon_lock(project_root)? {
            DaemonLock::Wedged { pid } => CoreState::Wedged { pid },
            _ => CoreState::NotRunning,
        });
    };
    Ok(if lock::is_listening(&state_dir)? {
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
    let published = build_stamp::read(&StateDir::at(state_dir).build_stamp_file());
    match build_stamp::vintage(published.as_ref(), &ours) {
        Vintage::Current => BuildState::Current,
        Vintage::Outdated => BuildState::Outdated,
        Vintage::PluginChanged => BuildState::PluginChanged,
        Vintage::Unknown => BuildState::Unknown,
    }
}

/// Every language with a live pid file in `state_dir`, sorted by language
/// (the order `StateDir::plugin_pid_files` returns). A pid file naming a dead
/// process is dropped, as in every pid-file read in this daemon.
fn plugin_reports(state_dir: &Path, core: CoreState) -> Vec<PluginReport> {
    StateDir::at(state_dir)
        .plugin_pid_files()
        .into_iter()
        .filter_map(|(language, path)| {
            let pid = read_pid_file(&path).filter(|&pid| crate::process::is_alive(pid))?;
            Some(PluginReport { language, state: classify_plugin(pid, core) })
        })
        .collect()
}

/// Every `plugin-<language>.suspended` marker in `state_dir`. Unlike
/// [`plugin_reports`] it does not depend on `core`: a suspension is a fact
/// about the state directory, not about a running daemon.
fn suspended_language_reports(state_dir: &Path) -> Vec<SuspendedLanguage> {
    StateDir::at(state_dir)
        .suspended_markers()
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
    let _ = writeln!(out, "  rerank model:    {}", describe_rerank(&report.rerank_model));

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
    match &index.coverage {
        Some(coverage) => {
            let _ = writeln!(
                out,
                "  index coverage:  {:.1}% ({}/{} source files)",
                coverage.ratio() * 100.0,
                coverage.indexed,
                coverage.discovered
            );
            if !index.bulk_indexed && phase == Some("walking") {
                let _ = writeln!(
                    out,
                    "  dirty files:     {} awaiting the walk already in progress",
                    coverage.dirty
                );
            } else {
                let _ = writeln!(out, "  dirty files:     {} awaiting reindex", coverage.dirty);
            }
        }
        None => {
            let _ = writeln!(
                out,
                "  index coverage:  not checked - `g-mesh status --full` walks the project for coverage and \
                 dirty files"
            );
        }
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
            &index.semantic_leftovers,
            in_progress.as_deref(),
        ) {
            let _ = writeln!(out, "{line}");
        }
    }

    for (language, since, files) in &index.semantic_pending {
        let tail = if daemon_alive {
            "have structural edges until its pass finishes"
        } else {
            "have structural edges until its pass finishes; the next daemon start runs it"
        };
        let _ = writeln!(
            out,
            "  semantic pending: {language} since {since} - {files} file(s) changed by the reindex {tail}"
        );
    }

    for (language, files) in &index.semantic_stale {
        let _ = writeln!(
            out,
            "  semantic stale:  {language}: {files} file(s) stale since the last reindex - their semantic \
             edges may point where the previous configuration resolved them until a pass re-checks them"
        );
    }

    for (language, trigger) in &index.pending_reindex {
        let _ = writeln!(
            out,
            "  pending reindex: {language} (after {trigger} changed) - interrupted; its previous graph \
             serves until the daemon runs it again on its next start"
        );
    }

    for line in language_lines(&report.languages) {
        let _ = writeln!(out, "{line}");
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

/// The `languages:` block: a header line, then one row per language.
pub(crate) fn language_lines(languages: &LanguagesReport) -> Vec<String> {
    let mut lines = Vec::new();
    let header = match &languages.section {
        LanguageSection::Front => return lines,
        LanguageSection::NoIndex => "none recorded - no index yet".to_string(),
        LanguageSection::PredatesOutcomes { schema_version } => format!(
            "not recorded - this index predates per-language outcomes (schema {}); the next daemon \
             start rebuilds it",
            schema_version.as_deref().unwrap_or("unknown")
        ),
        LanguageSection::WalkInProgress => {
            "not recorded yet - the walk in progress records them when it finishes".to_string()
        }
        LanguageSection::NoneRecorded => {
            "none recorded - no walk has finished on this index; run `g-mesh reindex`".to_string()
        }
        LanguageSection::Recorded(outcomes) => format!("{} recorded by the last walk", outcomes.len()),
    };
    lines.push(format!("  languages:       {header}"));
    let discovery_failed = languages.installed.is_err();
    for row in languages.rows() {
        lines.push(format!(
            "    {:<15}{}",
            format!("{}:", row.language),
            describe_language_row(&row, discovery_failed)
        ));
    }
    if let Err(err) = &languages.installed {
        lines.push(format!("  plugins:         discovery failed - {err}"));
    }
    lines
}

/// One language row's text, after its name. `discovery_failed` drops every
/// version clause: nothing is known about what is installed.
fn describe_language_row(row: &LanguageRow<'_>, discovery_failed: bool) -> String {
    let version = || match (discovery_failed, row.plugin_version) {
        (true, _) => String::new(),
        (false, Some(version)) => format!(" (plugin {version})"),
        (false, None) => " (plugin no longer installed)".to_string(),
    };
    match row.outcome {
        None => format!("installed {} - not in the last walk", row.plugin_version.unwrap_or("?")),
        Some(LanguageOutcome::Indexed { files }) => {
            let unit = if *files == 1 { "file" } else { "files" };
            format!("indexed, {files} {unit}{}", version())
        }
        Some(LanguageOutcome::Failed { error }) => {
            // Only an installed plugin is retried: a removed one changes the
            // indexer version, and the next start re-walks everything.
            let retry = match (discovery_failed, row.plugin_version) {
                (false, Some(_)) if row.retries < MAX_LANGUAGE_RETRIES => {
                    format!("; retry {} of {MAX_LANGUAGE_RETRIES} on the next start", row.retries + 1)
                }
                (false, Some(_)) => "; retries used up - run `g-mesh reindex`".to_string(),
                _ => String::new(),
            };
            format!(
                "failed{} - not in the index: {}{retry}",
                version(),
                crate::storage::language_outcome::error_on_one_line(error)
            )
        }
        Some(LanguageOutcome::PluginAbsent { files }) => {
            if let Some(installed) = row.plugin_version {
                return format!(
                    "plugin absent at the last walk; {installed} installed since - the next daemon start re-walks"
                );
            }
            let what = match files {
                Some(files) => format!("{files} file(s) not indexed"),
                None => "its files are not indexed (not counted)".to_string(),
            };
            match crate::languages::entry(row.language) {
                Some(entry) => {
                    format!("plugin absent - {what}; install it with `{}`", entry.install_command())
                }
                None => format!("plugin absent - {what}"),
            }
        }
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

const RERANK_DISABLED: &str = "disabled ([rerank] enabled = false or G_MESH_RERANK=off)";

fn describe_rerank(status: &RerankStatus) -> String {
    match status {
        RerankStatus::Daemon(record) => {
            let said = describe_outcome(record);
            match (record.outcome, record.variant) {
                (RerankOutcome::Loaded, Some(variant)) if record.fallback => format!(
                    "{said}: model {variant}, the fallback for the default {}, which is not fetched - \
                     run `g-mesh model fetch`, then restart the daemon (`g-mesh stop`)",
                    RerankVariant::DEFAULT
                ),
                (RerankOutcome::Loaded, Some(variant)) => format!("{said}: model {variant}"),
                (RerankOutcome::Absent | RerankOutcome::Failed, Some(variant)) => {
                    format!("{said}; model {variant}")
                }
                _ => said,
            }
        }
        RerankStatus::OnDisk { daemon_alive, variant, state } => {
            let lead = if *daemon_alive { "not loaded yet" } else { "no daemon running" };
            let on_disk = match state {
                RerankModelState::Present => {
                    "present (file sizes only; the daemon checks sha256)".to_string()
                }
                RerankModelState::Fallback(fallback) => format!(
                    "missing, {fallback} present - the daemon ranks with {fallback} until `g-mesh \
                     model fetch` adds this model"
                ),
                RerankModelState::Missing => "missing - run `g-mesh model fetch`".to_string(),
                RerankModelState::Disabled => RERANK_DISABLED.to_string(),
            };
            format!("{lead}; on disk: {on_disk}; model {variant}")
        }
    }
}

/// The daemon's outcome in words, without the variant.
fn describe_outcome(record: &OutcomeRecord) -> String {
    match record.outcome {
        RerankOutcome::Loaded => "loaded".to_string(),
        RerankOutcome::Off => RERANK_DISABLED.to_string(),
        RerankOutcome::Absent => {
            "missing - search_code ranks by embedding similarity only; run `g-mesh model \
                                      fetch`, then restart the daemon (`g-mesh stop`)"
                .to_string()
        }
        RerankOutcome::Failed => match &record.dir {
            Some(dir) => format!(
                "failed to load (see the daemon log) - search_code ranks by embedding similarity only; \
                     delete {}, run `g-mesh model fetch`, then restart the daemon (`g-mesh stop`)",
                dir.display()
            ),
            None => "failed to load (see the daemon log) - search_code ranks by embedding similarity only"
                .to_string(),
        },
    }
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
            "this build, but holding a plugin that has been rebuilt since - \
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

mod json;
pub use json::to_json;

#[cfg(test)]
mod tests;
