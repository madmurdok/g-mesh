//! `g-mesh deps --json`: every file-level import edge of one project, as one
//! JSON document on stdout (format and exit codes: `docs/cli/deps-json.md`).
//!
//! The data comes in two steps (ADR 0041,
//! `docs/adr/0041-deps-cli-reads-the-index-after-a-daemon-readiness-wait.md`):
//! the project's daemon is reached or bootstrapped exactly as `mcp-shim` does
//! and asked, over plain MCP, to make the structural index ready
//! ([`daemon::client`]); then the index database is read directly, read-only,
//! in one transaction, so the export is one WAL snapshot while the daemon
//! keeps writing.
//!
//! stdout carries the JSON document and nothing else; progress and per-language
//! notes go to stderr.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::cli::{
    language_outcome_lines, status, DepsArgs, ExitStatus, INDEX_UNAVAILABLE_EXIT_CODE,
    NOT_A_PROJECT_EXIT_CODE, TIMEOUT_EXIT_CODE,
};
use crate::daemon::candidates::{self, Limits, Mode};
use crate::daemon::indexing_status::group_thousands;
use crate::daemon::{self, client, manifest};
use crate::graph::containers;
use crate::graph::imports::EXTERNAL_MODULE_NATIVE_KIND;
use crate::languages::LanguageOutcome;
use crate::process;
use crate::shim;
use crate::storage::connection::project_dir;
use crate::storage::schema;

/// The JSON format's version. Removing or renaming a key, or changing a
/// field's meaning or type, bumps it; adding a key or a new `kind`/`status`
/// value does not.
pub const DEPS_FORMAT_VERSION: u32 = 1;

/// `--timeout`'s default, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// How often a waiting run prints a progress line.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);

/// Between two attempts while a rebuild holds the project.
const REBUILD_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Between two activating calls whose answer came back before the index was
/// ready (the daemon's own wait cap fired).
const NOT_READY_RETRY_INTERVAL: Duration = Duration::from_millis(200);

/// How many times the daemon may go away mid-wait before the index counts as
/// unavailable.
const MAX_LOST_CONNECTIONS: u32 = 2;

/// How long a read of the index waits on a writer's lock.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// `clientInfo.name` of the activating MCP session.
const CLIENT_NAME: &str = "g-mesh-deps";

const FILE_KIND: &str = "File";
const IMPORTS_KIND: &str = "IMPORTS";

pub fn run(args: &DepsArgs) -> Result<()> {
    // The daemon a cold run spawns must not hold this process's stdout: the
    // caller reads it to EOF.
    process::keep_our_stdio_from_children();

    let root = resolve_root(args.project.as_deref())?;
    reject_folder_of_projects(&root)?;
    // A timeout too large to be an instant is no limit.
    let deadline =
        (args.timeout != 0).then(|| Instant::now().checked_add(Duration::from_secs(args.timeout))).flatten();
    wait_ready(&root, deadline)?;

    let state_dir = project_dir(&root).context("failed to resolve the project's state directory")?;
    let db_path = state_dir.join("index.db");
    if !db_path.exists() {
        return Err(unavailable(
            &root,
            &state_dir,
            "the daemon reported ready but there is no index.db".into(),
        ));
    }
    let conn = open_read_only(&db_path)?;
    let extensions = plugin_extensions();
    let (report, outcomes) = export(&conn, &root, &extensions)?;
    drop(conn);

    for line in language_outcome_lines(&outcomes) {
        eprintln!("{line}");
    }
    let mut out = BufWriter::new(io::stdout().lock());
    serde_json::to_writer(&mut out, &report).context("failed to write the JSON document")?;
    out.write_all(b"\n").context("failed to write the JSON document")?;
    out.flush().context("failed to write the JSON document")?;
    Ok(())
}

/// `--project`, else the current directory; canonical. Never
/// `$CLAUDE_PROJECT_DIR`, which names the calling session's project.
fn resolve_root(project: Option<&Path>) -> Result<PathBuf> {
    let given = match project {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().context("failed to resolve the current directory")?,
    };
    let canonical = given.canonicalize().map_err(|err| {
        exit_status(NOT_A_PROJECT_EXIT_CODE, format!("{} is not a project directory: {err}", given.display()))
    })?;
    if !canonical.is_dir() {
        return Err(exit_status(
            NOT_A_PROJECT_EXIT_CODE,
            format!("{} is not a project directory: not a directory", canonical.display()),
        ));
    }
    Ok(canonical)
}

/// Exit 3 for a folder of projects, before anything is started: its daemon
/// would run as a front with no index of its own.
fn reject_folder_of_projects(root: &Path) -> Result<()> {
    let detection = candidates::detect(root, Limits::default());
    if detection.mode == Mode::Multi {
        return Err(folder_of_projects(root, Some(detection.candidates.len())));
    }
    Ok(())
}

fn folder_of_projects(root: &Path, count: Option<usize>) -> anyhow::Error {
    let what = match count {
        Some(count) => format!("a folder of {count} projects"),
        None => "a folder of projects".to_string(),
    };
    exit_status(NOT_A_PROJECT_EXIT_CODE, format!("{} is {what}; run deps inside one of them", root.display()))
}

fn exit_status(code: i32, message: String) -> anyhow::Error {
    ExitStatus { code, message }.into()
}

fn unavailable(root: &Path, state_dir: &Path, reason: String) -> anyhow::Error {
    exit_status(
        INDEX_UNAVAILABLE_EXIT_CODE,
        format!(
            "the index for {} is unavailable: {reason} (daemon log: {})",
            root.display(),
            state_dir.join(shim::DAEMON_LOG_FILE).display()
        ),
    )
}

/// Blocks until the daemon serving `root` has a structural index, printing a
/// progress line every [`PROGRESS_INTERVAL`]. The wait itself runs on a
/// thread ([`readiness_loop`]) so `deadline` bounds every step, socket reads
/// included; on expiry that thread is abandoned and the daemon keeps indexing.
fn wait_ready(root: &Path, deadline: Option<Instant>) -> Result<()> {
    let state_dir = project_dir(root).context("failed to resolve the project's state directory")?;
    let rebuild_note: Arc<Mutex<Option<String>>> = Arc::default();
    let (tx, rx) = mpsc::channel();
    {
        let root = root.to_path_buf();
        let state_dir = state_dir.clone();
        let rebuild_note = Arc::clone(&rebuild_note);
        thread::Builder::new()
            .name("deps-readiness".to_string())
            .spawn(move || {
                // A send fails only once the receiver gave up waiting.
                let _ = tx.send(readiness_loop(&root, &state_dir, &rebuild_note));
            })
            .context("failed to start the readiness wait")?;
    }

    let started = Instant::now();
    loop {
        let wait = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()).min(PROGRESS_INTERVAL),
            None => PROGRESS_INTERVAL,
        };
        match rx.recv_timeout(wait) {
            Ok(outcome) => return outcome,
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("the readiness wait for {} ended without an outcome", root.display())
            }
            Err(RecvTimeoutError::Timeout) => {
                let note = rebuild_note.lock().map(|note| note.clone()).unwrap_or_default();
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(timed_out(root, &state_dir, note, started.elapsed()));
                }
                let doing = note.unwrap_or_else(|| progress_detail(&state_dir));
                eprintln!("g-mesh deps: waiting for the index of {} ({doing})", root.display());
            }
        }
    }
}

fn timed_out(root: &Path, state_dir: &Path, rebuild_note: Option<String>, waited: Duration) -> anyhow::Error {
    let waited = format!("{} s", waited.as_secs());
    let message = match rebuild_note {
        Some(note) => format!("{note}; waited {waited}. Run again once it finishes."),
        None => format!(
            "the index for {} is still being built ({}); waited {waited}. The daemon keeps indexing; run again.",
            root.display(),
            progress_detail(state_dir)
        ),
    };
    exit_status(TIMEOUT_EXIT_CODE, message)
}

/// The phase word and, while walking, how far the walk got.
fn progress_detail(state_dir: &Path) -> String {
    let phase = daemon::read_phase_in(state_dir).unwrap_or_else(|| "starting".to_string());
    let Some(progress) = daemon::read_progress_in(state_dir) else {
        return phase;
    };
    let walk = &progress.walk;
    if phase != "walking" || walk.languages_total == 0 {
        return phase;
    }
    let current = walk.current_language.as_deref().unwrap_or("the first language");
    format!(
        "walking {current}, {}/{} languages done, {} nodes and edges so far",
        walk.languages_done,
        walk.languages_total,
        group_thousands(walk.items)
    )
}

/// Connect or bootstrap, activate, check the phase; again until the phase
/// allows structural answers. Never returns for a rebuild that does not end:
/// the caller's deadline ends that.
fn readiness_loop(root: &Path, state_dir: &Path, rebuild_note: &Mutex<Option<String>>) -> Result<()> {
    let mut lost = 0u32;
    let mut lose = |reason: String| -> Result<()> {
        lost += 1;
        if lost > MAX_LOST_CONNECTIONS {
            return Err(unavailable(root, state_dir, reason));
        }
        Ok(())
    };
    loop {
        let stream = match shim::connect_or_bootstrap(root, "for g-mesh deps") {
            Ok(stream) => {
                if let Ok(mut note) = rebuild_note.lock() {
                    *note = None;
                }
                stream
            }
            Err(err) => {
                if let Some(rebuild) = err.chain().find_map(|cause| cause.downcast_ref::<shim::Rebuilding>())
                {
                    if let Ok(mut note) = rebuild_note.lock() {
                        *note = Some(format!(
                            "{} is being reindexed (g-mesh {}, pid {})",
                            rebuild.root.display(),
                            rebuild.command,
                            rebuild.pid
                        ));
                    }
                    thread::sleep(REBUILD_RETRY_INTERVAL);
                    continue;
                }
                let reason = format!("{err:#}");
                let reason = reason.strip_prefix("g-mesh: ").unwrap_or(&reason).to_string();
                return Err(unavailable(root, state_dir, reason));
            }
        };
        let answer = match client::activate_and_wait(stream, CLIENT_NAME) {
            Ok(answer) => answer,
            Err(err) => {
                lose(format!("the daemon went away while building the index: {err:#}"))?;
                continue;
            }
        };
        match daemon::read_phase_in(state_dir).as_deref() {
            Some("structural" | "embedding" | "ready") => return Ok(()),
            Some("failed") => {
                let reason = if answer.is_error && !answer.text.is_empty() {
                    answer.text
                } else {
                    "the walk failed".to_string()
                };
                return Err(unavailable(root, state_dir, reason));
            }
            Some(daemon::front::FRONT_PHASE) => return Err(folder_of_projects(root, None)),
            Some(_) => thread::sleep(NOT_READY_RETRY_INTERVAL),
            None => lose("the daemon exited while building the index".to_string())?,
        }
    }
}

/// [`status::open_index`] (never creates a database) plus `query_only`, so
/// nothing this command does can write.
fn open_read_only(db_path: &Path) -> Result<Connection> {
    let conn = status::open_index(db_path)?;
    conn.pragma_update(None, "query_only", true).context("failed to make the index connection read-only")?;
    conn.busy_timeout(BUSY_TIMEOUT).context("failed to set the index busy timeout")?;
    Ok(conn)
}

/// Each discovered plugin's extensions, sorted. Empty when discovery fails:
/// they only annotate `coverage`.
fn plugin_extensions() -> HashMap<String, Vec<String>> {
    let Ok(discovered) = manifest::discover(&manifest::default_roots()) else {
        return HashMap::new();
    };
    discovered
        .manifests
        .into_iter()
        .map(|(language, manifest)| {
            let mut extensions = manifest.extensions;
            extensions.sort();
            extensions.dedup();
            (language, extensions)
        })
        .collect()
}

/// The document `deps --json` prints.
#[derive(Debug, Serialize)]
pub struct DepsReport {
    pub version: u32,
    pub project_root: String,
    /// Languages whose outcome is `indexed`, sorted.
    pub languages: Vec<String>,
    pub coverage: BTreeMap<String, Coverage>,
    /// Indexed languages with at least one unresolved import.
    pub unresolved: BTreeMap<String, u64>,
    /// Sorted by `(from, to, kind)`.
    pub edges: Vec<DepsEdge>,
}

/// One language's line in `coverage`.
#[derive(Debug, Serialize)]
pub struct Coverage {
    /// `indexed` | `plugin_absent` | `failed`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DepsEdge {
    pub from: String,
    pub to: String,
    /// What `to` names: `file` | `dir`.
    pub kind: &'static str,
    /// The importing file's language.
    pub language: String,
    /// The container key the import resolved to; `null` for a file-to-file
    /// import.
    pub module: Option<String>,
}

/// Per-language tallies, by importing file's language.
#[derive(Default)]
struct Tally {
    unresolved: HashSet<(String, String)>,
    external: HashSet<(String, String)>,
}

/// Builds the report from one read transaction over `conn`, and returns the
/// language outcomes it read for the stderr lines.
pub fn export(
    conn: &Connection,
    project_root: &Path,
    extensions: &HashMap<String, Vec<String>>,
) -> Result<(DepsReport, BTreeMap<String, LanguageOutcome>)> {
    let tx = conn.unchecked_transaction().context("failed to begin the index read")?;
    let outcomes: BTreeMap<String, LanguageOutcome> = schema::language_outcomes(&tx)?.into_iter().collect();
    let container_keys = container_keys(&tx)?;
    let defining = containers::defining_files(&tx)?;
    let mut fallback: HashMap<String, Vec<(String, &'static str)>> = HashMap::new();

    let mut tallies: HashMap<String, Tally> = HashMap::new();
    // (from, to) -> (kind, language, module); the smallest module wins, so the
    // pick does not depend on row order.
    let mut edges: BTreeMap<(String, String), (&'static str, String, Option<String>)> = BTreeMap::new();

    let mut stmt = tx
        .prepare(
            "SELECT f.filePath, f.language, t.id, t.kind, t.filePath, t.nativeKind, \
                    COALESCE(e.specifier, t.qualifiedName) \
             FROM edges e \
             JOIN nodes f ON f.id = e.fromId AND f.kind = ?1 \
             JOIN nodes t ON t.id = e.toId \
             WHERE e.kind = ?2",
        )
        .context("failed to prepare the import edge query")?;
    let rows = stmt
        .query_map(params![FILE_KIND, IMPORTS_KIND], |row| {
            Ok(ImportRow {
                from: row.get(0)?,
                language: row.get(1)?,
                target_id: row.get(2)?,
                target_kind: row.get(3)?,
                target_path: row.get(4)?,
                native_kind: row.get(5)?,
                specifier: row.get(6)?,
            })
        })
        .context("failed to query the import edges")?;

    for row in rows {
        let row = row.context("failed to read an import edge")?;
        let tally = tallies.entry(row.language.clone()).or_default();
        let (targets, module) = if row.target_kind == FILE_KIND {
            (vec![(row.target_path.clone(), "file")], None)
        } else if let Some(key) = container_keys.get(&row.target_id) {
            let targets = match defining.get(&row.target_id) {
                Some(files) => targets_of(files),
                None => match fallback.get(&row.target_id) {
                    Some(targets) => targets.clone(),
                    None => {
                        let targets = fallback_targets(&tx, &row.target_id, &row.language, key)?;
                        fallback.insert(row.target_id.clone(), targets.clone());
                        targets
                    }
                },
            };
            (targets, Some(key.clone()))
        } else if row.native_kind.as_deref() == Some(EXTERNAL_MODULE_NATIVE_KIND) {
            if is_relative_specifier(&row.language, &row.specifier) {
                tally.unresolved.insert((row.from, row.specifier));
            } else {
                tally.external.insert((row.from, row.specifier));
            }
            continue;
        } else {
            tally.unresolved.insert((row.from, row.specifier));
            continue;
        };

        if targets.is_empty() {
            tally.unresolved.insert((row.from, row.specifier));
            continue;
        }
        for (to, kind) in targets {
            // An import of the importer itself, or of the package directory
            // it sits in, says nothing about a dependency between files.
            let onto_itself = match kind {
                "dir" => to == parent_dir(&row.from),
                _ => to == row.from,
            };
            if onto_itself {
                continue;
            }
            let value = (kind, row.language.clone(), module.clone());
            edges
                .entry((row.from.clone(), to))
                .and_modify(|existing| {
                    if value.2 < existing.2 {
                        *existing = value.clone();
                    }
                })
                .or_insert(value);
        }
    }
    drop(stmt);
    tx.finish().context("failed to end the index read")?;

    let mut edge_counts: HashMap<&str, u64> = HashMap::new();
    for (_, language, _) in edges.values() {
        *edge_counts.entry(language.as_str()).or_default() += 1;
    }

    let mut languages = Vec::new();
    let mut coverage = BTreeMap::new();
    let mut unresolved = BTreeMap::new();
    for (language, outcome) in &outcomes {
        let entry = match outcome {
            LanguageOutcome::Indexed { files } => {
                languages.push(language.clone());
                let tally = tallies.get(language);
                let unresolved_count = tally.map_or(0, |t| t.unresolved.len() as u64);
                if unresolved_count > 0 {
                    unresolved.insert(language.clone(), unresolved_count);
                }
                Coverage {
                    status: "indexed",
                    files: Some(*files as u64),
                    edges: Some(edge_counts.get(language.as_str()).copied().unwrap_or(0)),
                    unresolved: Some(unresolved_count),
                    external: Some(tally.map_or(0, |t| t.external.len() as u64)),
                    extensions: extensions.get(language).cloned(),
                    error: None,
                }
            }
            LanguageOutcome::PluginAbsent { files } => Coverage {
                status: "plugin_absent",
                files: files.map(|files| files as u64),
                edges: None,
                unresolved: None,
                external: None,
                extensions: None,
                error: None,
            },
            LanguageOutcome::Failed { error } => Coverage {
                status: "failed",
                files: None,
                edges: None,
                unresolved: None,
                external: None,
                extensions: None,
                error: Some(crate::languages::error_on_one_line(error)),
            },
        };
        coverage.insert(language.clone(), entry);
    }

    // The map's (from, to) order is (from, to, kind) order: one kind per pair.
    let edges = edges
        .into_iter()
        .map(|((from, to), (kind, language, module))| DepsEdge { from, to, kind, language, module })
        .collect();

    let report = DepsReport {
        version: DEPS_FORMAT_VERSION,
        project_root: project_root.to_string_lossy().into_owned(),
        languages,
        coverage,
        unresolved,
        edges,
    };
    Ok((report, outcomes))
}

struct ImportRow {
    from: String,
    language: String,
    target_id: String,
    target_kind: String,
    target_path: String,
    native_kind: Option<String>,
    specifier: String,
}

/// Container node id -> container key.
fn container_keys(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn
        .prepare("SELECT nodeId, key FROM containers")
        .context("failed to prepare the container listing")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))).context("failed to list the containers")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the containers")
}

/// What a resolved target's files become: one file -> that file; several in
/// one directory -> that directory; several directories -> each file. `files`
/// is sorted.
fn targets_of(files: &[String]) -> Vec<(String, &'static str)> {
    match files {
        [] => Vec::new(),
        [only] => vec![(only.clone(), "file")],
        [first, rest @ ..] => {
            let dir = parent_dir(first);
            if rest.iter().all(|file| parent_dir(file) == dir) {
                vec![(dir.to_string(), "dir")]
            } else {
                files.iter().map(|file| (file.clone(), "file")).collect()
            }
        }
    }
}

/// Where an import of a container no file defines leads: the file whose own
/// module declaration it is; else the files holding its ordinary members;
/// else, for a container whose only members are sub-modules (a Python package
/// with an empty `__init__.py`, or a namespace package), the sub-modules'
/// common directory's `__init__.py` when indexed, or that directory.
fn fallback_targets(
    conn: &Connection,
    node_id: &str,
    language: &str,
    key: &str,
) -> Result<Vec<(String, &'static str)>> {
    let own = containers::own_module_files(conn, language, key)?;
    if !own.is_empty() {
        return Ok(targets_of(&own));
    }
    let members = containers::member_files(conn, node_id)?;
    if !members.declarations.is_empty() {
        return Ok(targets_of(&members.declarations));
    }
    let Some(dir) = common_dir(&members.submodules) else {
        return Ok(Vec::new());
    };
    let init = if dir == "." { "__init__.py".to_string() } else { format!("{dir}/__init__.py") };
    if containers::has_file(conn, &init)? {
        return Ok(vec![(init, "file")]);
    }
    Ok(vec![(dir, "dir")])
}

/// The deepest directory holding every one of `files`; `None` for no files.
fn common_dir(files: &[String]) -> Option<String> {
    let mut dirs = files.iter().map(|file| parent_dir(file));
    let first: Vec<&str> = match dirs.next()? {
        "." => Vec::new(),
        dir => dir.split('/').collect(),
    };
    let mut common = first.len();
    for dir in dirs {
        let segments: Vec<&str> = if dir == "." { Vec::new() } else { dir.split('/').collect() };
        common = common.min(first.iter().zip(&segments).take_while(|(a, b)| a == b).count());
    }
    Some(if common == 0 { ".".to_string() } else { first[..common].join("/") })
}

/// Whether an import specifier names a path relative to the importing file
/// (`.`, `..`, `./x`, `../x`; for Python also `.x`, `..x`). Such a specifier can never
/// name an external package, so an `external_module` placeholder carrying one
/// is an import that resolved to nothing in the project: the TypeScript plugin
/// marks an unresolvable relative import that way (docs/cli/deps-json.md).
fn is_relative_specifier(language: &str, specifier: &str) -> bool {
    specifier == "."
        || specifier == ".."
        || specifier.starts_with("./")
        || specifier.starts_with("../")
        || (language == "python" && specifier.starts_with('.'))
}

/// A project-relative path's directory; `.` for the root.
fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir,
        _ => ".",
    }
}
