//! Runs a plugin the way the daemon does, and keeps a transcript of
//! everything that crossed the wire so `checks` can judge it afterwards.
//!
//! # What "the way the daemon does" means here, and where it deviates
//!
//! The protocol-level work goes through the daemon's own functions, unchanged:
//! the handshake is verified by `protocol::handshake::verify`, each bulk
//! stream is committed by `daemon::bulk_index::ingest` (the real batching)
//! and linked by `graph::imports::link_all` + `graph::symbol_links::link_all`,
//! and every control-plane round trip is `watcher::apply::apply_file_change`
//! / `apply_semantic_pass` - including the `semantic_pass` capability gate
//! `apply_file_change` applies itself - so the diffs land through the real
//! `storage::write::apply_diff`, container maintenance, and `link_diff`.
//! `cli::plugin_check::expectations` queries exactly the linked state this
//! builds.
//!
//! Four things differ from `daemon::plugin::PluginProcess` /
//! `daemon::bulk_index::walk_one_language`, each on purpose:
//!
//! - **The pipes are tee'd.** [`TeeReader`]/[`TeeWriter`] copy every byte
//!   `apply_file_change` reads and every frame it writes, so the checks see
//!   the plugin's *raw* answers (legacy wire fields, `deleteNodeIds`, whether
//!   a diff was empty) - which `apply_file_change` itself consumes and never
//!   hands back. That is also what lets the writer look for the
//!   semantic-engine marker at the exact moment the first `semanticPass`
//!   frame is about to leave, rather than at some step boundary around it.
//! - **A crash is a finding, not something to recover from.** `PluginProcess`
//!   relaunches a dead plugin and replays its pending files, which is right
//!   for a daemon and wrong for a conformance kit: it would turn "this plugin
//!   crashes on an empty file" into a pass. So this spawns the child itself
//!   and stops the session at the first failure.
//! - **The bulk walk has a timeout.** The daemon's walk has none (it reads
//!   until EOF), but a kit that hangs on a wedged plugin is exactly what
//!   decision 8 of GM-276 rules out. The walk borrows
//!   `RoundTripTimeouts::semantic_pass_project_timeout` - the only
//!   whole-project budget the daemon defines, sized off the same file count -
//!   and the handshake borrows `file_changed`, the smallest one, since
//!   announcing a handshake is less work than any reparse.
//! - **The plugin's stderr is kept, not only forwarded.** The daemon's
//!   `Stdio::inherit` is right for it - plugin logs are diagnostic, and the
//!   daemon has a log of its own to interleave them into. A kit whose whole
//!   product is a report must be able to *quote* them: "the bulk index
//!   exited with exit code: 1" and nothing else is not a finding a plugin
//!   author can act on, and it is what 25 Windows failures got. See
//!   [`StderrCapture`], which still forwards every byte on the way past.
//!
//! # Isolation
//!
//! Every child gets `G_MESH_HOME` pointed into this run's scratch directory,
//! which is the one variable every g-mesh state path resolves through
//! (`paths::g_mesh_home`), so nothing a plugin (or anything it runs) does can
//! reach the user's real `~/.g-mesh/projects`. `HOME` itself is deliberately
//! *not* overridden: a semantic tier legitimately needs the user's toolchain
//! from it - rustup's `~/.rustup` for rust-analyzer, `GOPATH`/`GOMODCACHE`
//! for go/packages - and a kit that breaks the engine it is meant to observe
//! would report "not instrumented" for every such plugin. The fixture itself
//! is copied into the scratch directory before anything runs, because the
//! whitespace-edit, emptied-file and declaration-edit steps write to it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use super::checks::is_placeholder;
use super::expectations::FilesCreatedPair;
use crate::daemon::bulk_index::{self, WalkContext, BULK_INDEX_FLAG};
use crate::daemon::manifest::PluginManifest;
use crate::daemon::plugin::RoundTripTimeouts;
use crate::embedding::EmbeddingPipeline;
use crate::paths;
use crate::protocol::handshake;
use crate::protocol::jsonrpc::{read_frame, read_message_with_timeout, write_message};
use crate::protocol::ndjson::BulkItem;
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, Handshake, NodeKind, RequestId,
    ResolutionChangedResponse, ResolutionChangedResult, WireNode, JSONRPC_VERSION,
};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::watcher::apply::{apply_file_change, apply_semantic_pass, SemanticPassOutcome};

/// The directory the kit hands every plugin process it spawns, via this
/// environment variable, for the plugin-side markers the kit defines.
///
/// The one marker defined today is [`SEMANTIC_ENGINE_MARKER`]. The contract,
/// as the README states it for plugin authors: when this variable is set and
/// non-empty, a plugin writes (or appends to) a file of that name inside the
/// directory at the moment it *starts* its semantic engine - spawns tsserver,
/// launches rust-analyzer, loads go/packages. Nothing else. A plugin that
/// never does so is not failed for it; `checks` reports it "not instrumented"
/// instead, because the absence of a marker is no evidence the engine was
/// lazy.
pub const MARKER_DIR_ENV: &str = "G_MESH_PLUGIN_CHECK_MARKER_DIR";

/// See [`MARKER_DIR_ENV`].
pub const SEMANTIC_ENGINE_MARKER: &str = "semantic-engine-started";

/// How long a plugin gets to exit on its own once its stdin closes - the
/// daemon's own "please exit" signal (`PluginProcess::shutdown`) - before it
/// is killed. Not a check: a plugin that needs the kill is still conformant.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// After killing a timed-out bulk walk, how long to wait for its stdout to
/// reach EOF before abandoning the reader thread. Only exceeded when the
/// plugin handed its stdout to a grandchild that outlives it.
const KILL_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// This run's scratch directory: the fixture copy, the isolated state root
/// and the marker directory. Removed on drop.
pub(crate) struct Scratch {
    root: PathBuf,
}

impl Scratch {
    /// Created under the system temp directory rather than via `tempfile`,
    /// which is a dev-dependency of this crate only.
    pub(crate) fn create() -> Result<Self> {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        for attempt in 0..64 {
            let root = base.join(format!("g-mesh-plugin-check-{}-{nanos}-{attempt}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let scratch = Self { root };
                    for dir in [scratch.workspace(), scratch.home(), scratch.markers()] {
                        fs::create_dir_all(&dir)
                            .with_context(|| format!("failed to create {}", dir.display()))?;
                    }
                    return Ok(scratch);
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("failed to create {}", root.display()));
                }
            }
        }
        bail!("failed to create a scratch directory under {}", base.display())
    }

    pub(crate) fn workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    fn home(&self) -> PathBuf {
        self.root.join("g-mesh-home")
    }

    pub(crate) fn markers(&self) -> PathBuf {
        self.root.join("markers")
    }

    pub(crate) fn semantic_engine_marker(&self) -> PathBuf {
        self.markers().join(SEMANTIC_ENGINE_MARKER)
    }

    /// The environment every child of this run gets - see this module's
    /// "Isolation" section.
    fn isolate(&self, command: &mut Command) {
        command.env(paths::HOME_ENV, self.home()).env(MARKER_DIR_ENV, self.markers());
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Copies `from` into `to` recursively: directories and regular files only.
/// Symlinks are skipped rather than followed, because a fixture is a small
/// hand-made tree and following links would let one reach outside it into
/// whatever the scratch copy was meant to protect.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("failed to create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("failed to list {}", from.display()))? {
        let entry = entry.with_context(|| format!("failed to read an entry of {}", from.display()))?;
        let file_type = entry.file_type()?;
        let destination = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &destination)
                .with_context(|| format!("failed to copy {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// `.ts` for `src/a.TS` - lowercased with its dot, matching how
/// `daemon::registry` routes files to a manifest's `extensions`.
fn extension_of(path: &str) -> Option<String> {
    let extension = Path::new(path).extension()?.to_str()?;
    Some(format!(".{}", extension.to_lowercase()))
}

pub(crate) fn claims_extension(manifest: &PluginManifest, path: &str) -> bool {
    extension_of(path)
        .is_some_and(|ext| manifest.extensions.iter().any(|claimed| claimed.to_lowercase() == ext))
}

/// Files under `dir` this plugin's manifest claims - what sizes the bulk
/// walk's timeout, the same way `daemon::semantic` sizes a whole-project
/// pass off the language's own file count.
pub(crate) fn count_claimed_files(manifest: &PluginManifest, dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => count_claimed_files(manifest, &entry.path()),
            Ok(t) if t.is_file() => {
                usize::from(claims_extension(manifest, &entry.file_name().to_string_lossy()))
            }
            _ => 0,
        })
        .sum()
}

// --- bulk -------------------------------------------------------------------

/// How many lines of a plugin's own stderr a failure quotes - enough for a
/// runtime's uncaught-exception banner and the top of its stack, capped so a
/// plugin that logs steadily cannot bury the finding it is attached to.
const STDERR_LINES_QUOTED: usize = 20;

/// Once a plugin has exited, how long to wait for its stderr drain thread to
/// reach EOF before quoting what it has so far. The same bound, for the same
/// reason, as [`KILL_DRAIN_GRACE`]: only exceeded when the plugin handed its
/// stderr to a grandchild that outlives it.
const STDERR_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A spawned plugin's stderr, drained on a thread and kept for whatever
/// failure message the kit ends up writing about that process.
///
/// This is the whole of what a plugin gets to say about its own death, and
/// until GM-337 the kit threw it away: `stderr` was `Stdio::inherit`, so the
/// plugin's account went to wherever the kit's own stderr went - under `cargo
/// nextest`, into a buffer the failing assertion never printed - and the
/// report said `the bulk index exited with exit code: 1` and nothing else.
/// Twenty-five Windows failures were diagnosable only by reasoning about
/// paths, because the one process that knew the answer had been told to say
/// it somewhere nobody was listening.
struct StderrCapture {
    captured: Arc<Mutex<Vec<u8>>>,
    /// Disconnected when the drain thread ends: its sender is dropped with it.
    drained: Option<std::sync::mpsc::Receiver<()>>,
}

impl StderrCapture {
    /// Takes `child`'s piped stderr and starts draining it. Every byte is
    /// also written straight through to this process's own stderr, which is
    /// what `Stdio::inherit` did and all it did - a plugin's live log still
    /// reaches whoever is watching a run.
    ///
    /// Drained on a thread rather than read at the end because a pipe is
    /// finite: a plugin that fills it while nobody reads would block in its
    /// own `write` and hang the run, which is a deadlock `inherit` could not
    /// have had and this must not introduce.
    fn attach(child: &mut Child) -> Self {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let Some(mut stderr) = child.stderr.take() else {
            return Self { captured, drained: None };
        };
        let sink = Arc::clone(&captured);
        let (drained_tx, drained) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _drained_tx = drained_tx;
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match stderr.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = io::stderr().write_all(&chunk[..n]);
                        sink.lock().unwrap().extend_from_slice(&chunk[..n]);
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        Self { captured, drained: Some(drained) }
    }

    /// Waits, at most [`STDERR_DRAIN_GRACE`], for the drain thread to reach
    /// EOF. Called once the child is reaped: its exit does not mean the
    /// thread has read the last of the pipe, and a quote taken before then
    /// can miss exactly the lines the plugin died writing (GM-547).
    fn wait_drained(&self) {
        if let Some(drained) = &self.drained {
            let _ = drained.recv_timeout(STDERR_DRAIN_GRACE);
        }
    }

    /// `failure` with what the plugin wrote to stderr quoted under it, or
    /// unchanged when there was nothing to quote. Read wherever the failure
    /// is finally assembled, which for a process still running is a
    /// best-effort snapshot - the drain thread may be mid-chunk.
    fn explain(&self, failure: String) -> String {
        quote_stderr(&self.captured.lock().unwrap(), failure, false)
    }

    /// As [`Self::explain`], for a failure that *is* the child's own fate -
    /// a spawn that failed, a walk that was killed, a non-zero exit. There,
    /// having said nothing on the way out is itself part of the answer and
    /// is said out loud, because a report that goes quiet about stderr reads
    /// exactly like one that never looked, which is the report this kit used
    /// to print.
    fn explain_end(&self, failure: String) -> String {
        quote_stderr(&self.captured.lock().unwrap(), failure, true)
    }
}

/// `failure`, with the last [`STDERR_LINES_QUOTED`] non-blank lines of
/// `stderr` quoted under it - and, when `note_silence`, an explicit note
/// when there were none.
///
/// The quoted lines are indented past the column `report` renders a verdict
/// in and prefixed with `|`, so nothing a plugin prints can be read back as a
/// check result by that line format or by the tests that parse it.
///
/// The *last* lines rather than the first: a runtime that dies on an uncaught
/// exception prints its banner last, after whatever the plugin had already
/// logged.
fn quote_stderr(stderr: &[u8], failure: String, note_silence: bool) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return if note_silence { format!("{failure}, having written nothing to stderr") } else { failure };
    }
    let shown = lines.len().min(STDERR_LINES_QUOTED);
    let mut out = failure;
    out.push_str(&format!("\n            its stderr, last {shown} of {} line(s):", lines.len()));
    for line in &lines[lines.len() - shown..] {
        out.push_str(&format!("\n            | {line}"));
    }
    out
}

/// One non-blank line of a bulk stream, with its 1-based line number in the
/// raw output (blank lines still count, so the number matches what a plugin
/// author sees running `--bulk-index` by hand).
pub(crate) struct BulkLine {
    pub line_no: usize,
    pub item: Result<BulkItem, String>,
}

pub(crate) struct BulkRun {
    pub bytes: Vec<u8>,
    pub lines: Vec<BulkLine>,
    /// Why this walk did not complete, if it did not: spawn failure, timeout,
    /// a non-zero exit. A failed walk's lines are kept but never judged by a
    /// check - a stream cut short by a kill can end mid-line or before the
    /// node an earlier edge was promised.
    pub failure: Option<String>,
}

impl BulkRun {
    pub(crate) fn complete(&self) -> bool {
        self.failure.is_none()
    }
}

/// Spawns `manifest`'s plugin in `--bulk-index` mode over the scratch
/// workspace - `bulk_index::walk_one_language`'s exact argv - and captures its
/// whole stdout, within `timeout`.
pub(crate) fn run_bulk(manifest: &PluginManifest, scratch: &Scratch, timeout: Duration) -> BulkRun {
    let mut command = Command::new(&manifest.command);
    command
        .args(&manifest.args)
        .arg(BULK_INDEX_FLAG)
        .arg(scratch.workspace())
        // The manifest being checked, so an SDK plugin reads the same file the
        // kit is judging it against rather than one beside its own binary -
        // see `daemon::manifest::MANIFEST_PATH_ENV`. Checking a plugin against
        // a manifest it cannot see is checking something else.
        .env(crate::daemon::manifest::MANIFEST_PATH_ENV, manifest.path())
        // The daemon's lifeline, exactly as `walk_one_language` sets it up:
        // a stdin pipe kept inside `child` and never written, plus
        // the variable that arms the plugin's watcher on it. Checking a
        // plugin under a different stdin than the daemon gives it would be
        // checking something else.
        .stdin(Stdio::piped())
        .env(crate::daemon::bulk_index::BULK_STDIN_LIFELINE_ENV, "1")
        .stdout(Stdio::piped())
        // Piped rather than inherited, and echoed on by `StderrCapture` - see
        // its own doc comment for what inheriting it cost.
        .stderr(Stdio::piped());
    scratch.isolate(&mut command);

    // The same missing-binary check `daemon::bulk_index::walk_one_language`
    // makes before spawning this same plugin, for the same reason - see
    // `daemon::plugin::missing_workspace_binary_hint`'s doc comment. Without
    // it, a fresh worktree's unbuilt `target/debug/g-mesh-plugin-{python,rust}`
    // fails `Command::spawn` below with a bare `No such file or directory
    // (os error 2)`, which blames the plugin for a build step nothing in
    // this kit's own path runs: `core/build.rs` builds the
    // go plugin as a side effect of `cargo build`, but the
    // cargo-workspace plugins are ordinary workspace members with no
    // such step, so `cargo build --workspace` is the one command that
    // produces them and this kit deliberately does not run it - see this
    // module's doc comment on why a crash/missing binary is a finding, not
    // something to work around.
    if let Some(hint) = crate::daemon::plugin::missing_workspace_binary_hint(&manifest.command) {
        return BulkRun { bytes: Vec::new(), lines: Vec::new(), failure: Some(hint) };
    }

    let mut child = match crate::process::spawn_serialized(&mut command) {
        Ok(child) => child,
        Err(err) => {
            return BulkRun {
                bytes: Vec::new(),
                lines: Vec::new(),
                failure: Some(format!("failed to spawn `{}`: {err}", manifest.command.display())),
            };
        }
    };
    let stderr = StderrCapture::attach(&mut child);
    let mut stdout = child.stdout.take().expect("stdout was piped");

    // Read on a thread into a shared buffer, so a timeout can kill the child
    // and still keep whatever it had written - and, if the pipe never reaches
    // EOF even then, abandon the thread rather than hang on it.
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    {
        let captured = Arc::clone(&captured);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 64 * 1024];
            let outcome = loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break Ok(()),
                    Ok(n) => captured.lock().unwrap().extend_from_slice(&chunk[..n]),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => break Err(err),
                }
            };
            let _ = done_tx.send(outcome);
        });
    }

    let started = Instant::now();
    let mut failure = match done_rx.recv_timeout(timeout) {
        Ok(Ok(())) => None,
        Ok(Err(err)) => Some(format!("failed to read the bulk-index stream: {err}")),
        Err(_) => {
            let _ = child.kill();
            let _ = done_rx.recv_timeout(KILL_DRAIN_GRACE);
            Some(format!("the bulk index did not finish within {timeout:?} and was killed"))
        }
    };

    let remaining = timeout.saturating_sub(started.elapsed()).max(Duration::from_secs(1));
    match wait_with_deadline(&mut child, remaining) {
        Some(status) if !status.success() && failure.is_none() => {
            failure = Some(format!("the bulk index exited with {status}"));
        }
        Some(_) => {}
        None => {
            let _ = child.kill();
            let _ = child.wait();
            failure.get_or_insert_with(|| {
                format!(
                    "the bulk index closed its output but did not exit within {remaining:?} and was killed"
                )
            });
        }
    }

    // Only now, with the child reaped: whatever it wrote to stderr is the
    // only account of a walk that produced nothing and exited non-zero.
    if failure.is_some() {
        stderr.wait_drained();
    }
    let failure = failure.map(|failure| stderr.explain_end(failure));
    let bytes = std::mem::take(&mut *captured.lock().unwrap());
    BulkRun { lines: parse_bulk_lines(&bytes), bytes, failure }
}

/// `child.try_wait()` polled until it exits or `deadline` passes; `None` if
/// it is still running.
fn wait_with_deadline(child: &mut Child, deadline: Duration) -> Option<ExitStatus> {
    let until = Instant::now() + deadline;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            _ => return None,
        }
    }
}

/// Splits a bulk stream into lines and parses each through `BulkItem::parse`,
/// the same per-line parse `protocol::ndjson::NdjsonReader` does - kept apart
/// from that reader only so line numbers stay true to the raw output (the
/// reader skips blank lines without counting them).
pub(crate) fn parse_bulk_lines(bytes: &[u8]) -> Vec<BulkLine> {
    bytes
        .split(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                return None;
            }
            let line_no = index + 1;
            let Ok(text) = std::str::from_utf8(line) else {
                return Some(BulkLine { line_no, item: Err("line is not valid UTF-8".to_string()) });
            };
            Some(BulkLine { line_no, item: BulkItem::parse(text).map_err(|err| format!("{err:#}")) })
        })
        .collect()
}

/// A fresh in-memory index, configured like the daemon's own connection in
/// the one respect that changes what a diff commits: `foreign_keys` is off,
/// as `storage::connection::open` sets it (WAL and the vector extension mean
/// nothing to a schema-only in-memory database). So a dangling edge is
/// stored exactly as the daemon would store it, and it is
/// `checks::stream_order` that reports it rather than a constraint error that
/// would end the session - and a delete plus re-upsert of a symbol whose
/// unchanged edges were not re-sent commits, as it does in the daemon.
///
/// Set explicitly rather than left to the default, which is the mistake
/// GM-292 was: this comment used to say the daemon's connection "sets
/// nothing ... notably not `foreign_keys`", on the belief that the default is
/// off. The bundled SQLite compiles it *on*, so this index enforced foreign
/// keys the daemon was never meant to. GM-276 worked around it rather than
/// finding it: the kit's only edits were a whitespace edit (an empty diff),
/// emptying the file (every edge out of it deleted along with its nodes) and
/// restoring it (from an extraction with nothing left to keep), and its test
/// fake upserts changed rows in place because a delete plus re-upsert was
/// refused. A real declaration edit through the TS
/// plugin, the shape every user edit has, was never sent. The
/// declaration-edit step (`id-stability.declaration-edit-applies`) now is.
///
/// An `Arc<IndexStore>`: the expectations call the MCP tools' own handler
/// functions, which take the same `&Arc<IndexStore>` `mcp::mod` holds, and
/// every other caller here borrows it as `&IndexStore`. Linked under
/// `manifest`'s own rules, as the daemon would link that language.
pub(crate) fn open_index(manifest: &PluginManifest) -> Result<Arc<IndexStore>> {
    let conn = Connection::open_in_memory().context("failed to open an in-memory index")?;
    conn.pragma_update(None, "foreign_keys", "OFF").context("failed to disable foreign-key enforcement")?;
    schema::apply(&conn)?;
    let rules = crate::daemon::manifest::link_rules([manifest]);
    Ok(Arc::new(IndexStore::new(conn).with_link_rules(rules)))
}

/// Commits one bulk stream through the daemon's own batching and links it
/// project-wide, exactly as `bulk_index::run` does after its walk.
pub(crate) fn ingest_and_link(conn: &IndexStore, bytes: &[u8]) -> Result<()> {
    bulk_index::ingest(Cursor::new(bytes.to_vec()), &mut WalkContext::new(conn))?;
    conn.link_all()?;
    Ok(())
}

/// `file`'s node ids as the linked index holds them - read once after the
/// bulk walk is committed and linked, and again after the session restored
/// the file, for `id-stability.incremental-matches-bulk`.
///
/// Read from the index rather than from the plugin's own output on both
/// sides, because linking legitimately removes nodes a plugin emitted:
/// `graph::imports` drops a `resolved_module` placeholder once it has
/// repointed its edge onto the real file. Comparing raw bulk output against
/// the index would report every linked import as a missing id.
pub(crate) fn file_node_ids(store: &IndexStore, file: &str) -> Result<BTreeSet<String>> {
    let conn = store.read();
    let mut statement = conn.prepare("SELECT id FROM nodes WHERE filePath = ?1")?;
    let ids = statement.query_map([file], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// A node's `(startLine, startCol, endLine, endCol)` as the index stores it.
pub(crate) type StoredRange = (i64, i64, i64, i64);

/// `file`'s nodes and where the linked index says each one is - read after
/// the session's declaration edit was applied, and from a fresh index that
/// bulk run 3 of the edited tree was committed and linked into, for
/// `id-stability.declaration-edit-applies`. Index against index for the same
/// reason [`file_node_ids`] is.
pub(crate) fn file_node_ranges(store: &IndexStore, file: &str) -> Result<BTreeMap<String, StoredRange>> {
    let conn = store.read();
    let mut statement =
        conn.prepare("SELECT id, startLine, startCol, endLine, endCol FROM nodes WHERE filePath = ?1")?;
    let rows = statement
        .query_map([file], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

// --- the edited file ----------------------------------------------------------

/// The file the control-plane session edits, the one whitespace edit made to
/// it, and the one declaration edit.
pub(crate) struct EditTarget {
    /// Project-relative, exactly as the plugin's own `File` node spells it -
    /// which is also what `fileChanged` carries.
    pub file_path: String,
    /// 1-based line the space is inserted at the end of.
    pub line: usize,
    pub original: Vec<u8>,
    pub edited: Vec<u8>,
    /// `None` when bulk run 1 gave the file no declaration to edit - see
    /// [`declaration_edit`].
    pub declaration: Option<DeclarationEdit>,
}

/// The session's one *real* edit: a line break inserted into a declaration,
/// so its range - and, for a declaration that spans lines, its length -
/// legitimately changes.
pub(crate) struct DeclarationEdit {
    /// The node whose last line the break is inserted before.
    pub node_id: String,
    pub node_name: String,
    /// 1-based line the break is inserted before (the node's last line).
    pub line: usize,
    pub edited: Vec<u8>,
}

/// Inserts a line break (the file's own: `\r\n` if its first newline is one)
/// at the start of the last line of the file's first declaration - the
/// earliest non-`File`, non-placeholder node bulk run 1 emitted for it, ties
/// broken by id - or `None` if there is no such node or that line is not in
/// the file.
///
/// # Why this edit
///
/// Every other edit the session makes is one no declaration survives in a
/// changed form: a whitespace edit moves nothing, emptying a file deletes
/// everything, restoring it re-adds everything. None of them is what a user's
/// edit is - an existing symbol that is still there, somewhere else or a
/// different size - which is the shape the TS plugin sends as a delete plus
/// an upsert of the same id, and the shape GM-292 silently refused on every
/// warm edit for a whole release without the kit noticing. A break before a
/// multi-line declaration's last line grows it by one; before a one-line
/// declaration's only line, it moves it down one; either way every later
/// range in the file moves too, and the `File` node's end with them.
///
/// What the edit legitimately does to anything else - a doc comment that no
/// longer attaches, a template literal that gains a line - does not matter
/// here, because the check compares the index after the edit against a fresh
/// bulk walk of the *edited* tree, not against a prediction. Both sides read
/// the same bytes; only a plugin whose diff fails to carry what its own bulk
/// path says the file now contains, or a core that fails to commit it, can
/// make them disagree.
pub(crate) fn declaration_edit<'a>(
    bytes: &[u8],
    nodes: impl IntoIterator<Item = &'a WireNode>,
) -> Option<DeclarationEdit> {
    let first = nodes
        .into_iter()
        .filter(|node| node.kind != NodeKind::File && !is_placeholder(node.native_kind.as_deref()))
        .min_by(|a, b| {
            (a.range.start.line, a.range.start.col, &a.id).cmp(&(
                b.range.start.line,
                b.range.start.col,
                &b.id,
            ))
        })?;
    let last_line = first.range.end.line as usize;
    let at = if last_line == 0 {
        0
    } else {
        bytes.iter().enumerate().filter(|(_, byte)| **byte == b'\n').nth(last_line - 1)?.0 + 1
    };
    if at > bytes.len() {
        return None;
    }
    let first_newline = bytes.iter().position(|byte| *byte == b'\n');
    let crlf = first_newline.is_some_and(|newline| newline > 0 && bytes[newline - 1] == b'\r');
    let mut edited = bytes.to_vec();
    edited.splice(at..at, if crlf { b"\r\n".to_vec() } else { b"\n".to_vec() });
    Some(DeclarationEdit {
        node_id: first.id.clone(),
        node_name: first.name.clone(),
        line: last_line + 1,
        edited,
    })
}

/// Inserts one space immediately before the file's last newline (before its
/// `\r` in a CRLF file), or `None` for a file with no newline at all.
///
/// # Why this edit and no other
///
/// "A whitespace-only edit yields an empty diff" is only a fair rule for an
/// edit after which no range a plugin reports can *legitimately* move, and
/// most whitespace edits fail that test:
///
/// - **Appending a newline at EOF** moves any whole-file end measured on the
///   raw text, such as tree-sitter's root end `(lines, 0)`. A plugin's `File`
///   node ends where the content ends instead - trailing whitespace trimmed,
///   then `(newlines, length of the last line)`, the SDK's
///   `CharColumns::file_range` - which this edit does not move either; the
///   check also requires that end line (`checks::whitespace_edit`). The
///   space below lengthens the raw text too, so a plugin that measures it
///   still fails.
/// - **Inserting a blank line** shifts every declaration below it.
/// - **Trailing space on an arbitrary line** can land inside a multi-line
///   doc comment or string, whose text a plugin legitimately reports.
///
/// A space before the *last* newline moves nothing: no content follows it on
/// its line, every later line (at most the final one, when the file does not
/// end in a newline) keeps its row and columns, so the content's end is
/// unchanged, and a declaration ending on that line ends at its last token,
/// before the space. The one residue is a line whose end is inside a construct
/// still open at that point - a template literal or block comment spanning
/// into an unterminated final line - which is a syntax error in every
/// language this is meant for. The report names the file and line, so a
/// failure here can always be checked by hand.
pub(crate) fn whitespace_edit(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    let newline = bytes.iter().rposition(|byte| *byte == b'\n')?;
    let at = if newline > 0 && bytes[newline - 1] == b'\r' { newline - 1 } else { newline };
    let line = bytes[..newline].iter().filter(|byte| **byte == b'\n').count() + 1;
    let mut edited = bytes.to_vec();
    edited.insert(at, b' ');
    Some((edited, line))
}

/// Picks the file the session edits: among the `File` nodes bulk run 1
/// emitted for a path the manifest claims and that has a newline to edit at,
/// the one with the most nodes (so the emptied-file step deletes as many ids
/// as the fixture offers), ties broken by path for determinism.
pub(crate) fn choose_edit_target(
    manifest: &PluginManifest,
    workspace: &Path,
    lines: &[BulkLine],
) -> Option<EditTarget> {
    let mut node_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut files: BTreeSet<&str> = BTreeSet::new();
    for line in lines {
        if let Ok(BulkItem::Node(node)) = &line.item {
            *node_counts.entry(node.file_path.as_str()).or_default() += 1;
            if node.kind == NodeKind::File {
                files.insert(node.file_path.as_str());
            }
        }
    }

    let mut best: Option<(usize, EditTarget)> = None;
    for file_path in files {
        if !claims_extension(manifest, file_path) {
            continue;
        }
        let Ok(original) = fs::read(workspace.join(file_path)) else { continue };
        let Some((edited, line)) = whitespace_edit(&original) else { continue };
        let count = node_counts[file_path];
        // `files` iterates in path order, so a strict `>` keeps the first path
        // among equal counts.
        if best.as_ref().is_none_or(|(best_count, _)| count > *best_count) {
            best = Some((
                count,
                EditTarget { file_path: file_path.to_string(), line, original, edited, declaration: None },
            ));
        }
    }
    // Chosen after the file, not as a criterion for it: the file choice stays
    // what it was before the declaration edit existed, so every earlier
    // check keeps judging the same file.
    best.map(|(_, mut target)| {
        let nodes = lines.iter().filter_map(|line| match &line.item {
            Ok(BulkItem::Node(node)) if node.file_path == target.file_path => Some(node.as_ref()),
            _ => None,
        });
        target.declaration = declaration_edit(&target.original, nodes);
        target
    })
}

// --- filesCreated -------------------------------------------------------------

/// One `IMPORTS` edge leaving a node of the importer, and where it lands in
/// the linked index - `capabilities.files-created-resolves`' evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportRow {
    pub to_file: String,
    pub to_kind: String,
    pub to_name: String,
    pub to_native_kind: Option<String>,
    pub resolved: bool,
    /// When the edge lands on a core-owned container (a Python module, a Go
    /// package), the files of that container's members - empty otherwise.
    pub container_member_files: Vec<String>,
}

/// Every `IMPORTS` edge whose `fromId` is a node of `importer`, with its
/// landing node, as the linked index holds them - and, for a landing node
/// that is a core-owned container, the files its members live in (the
/// container's `DEFINES` edges, which core alone writes).
pub(crate) fn import_rows(store: &IndexStore, importer: &str) -> Result<Vec<ImportRow>> {
    let conn = store.read();
    let mut statement = conn.prepare(
        "SELECT t.filePath, t.kind, t.name, t.nativeKind, e.resolved, \
                (SELECT group_concat(DISTINCT m.filePath) FROM containers c \
                   JOIN edges d ON d.fromId = c.nodeId AND d.kind = 'DEFINES' \
                   JOIN nodes m ON m.id = d.toId \
                 WHERE c.nodeId = t.id) \
         FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
         WHERE e.kind = 'IMPORTS' AND f.filePath = ?1 \
         ORDER BY t.filePath, t.name",
    )?;
    let rows = statement
        .query_map([importer], |row| {
            let members: Option<String> = row.get(5)?;
            Ok(ImportRow {
                to_file: row.get(0)?,
                to_kind: row.get(1)?,
                to_name: row.get(2)?,
                to_native_kind: row.get(3)?,
                resolved: row.get::<_, i64>(4)? != 0,
                container_member_files: members
                    .map(|members| members.split(',').map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Why `pair` cannot be run against `workspace`, one finding per violation -
/// empty when it can. Checked before anything is written: a pair naming an
/// existing fixture file would overwrite it, and one whose extension the
/// manifest does not claim would never reach the plugin.
pub(crate) fn files_created_pair_findings(
    manifest: &PluginManifest,
    workspace: &Path,
    pair: &FilesCreatedPair,
) -> Vec<String> {
    let mut findings = Vec::new();
    for (field, path) in [("target", &pair.target), ("importer", &pair.importer)] {
        let relative = Path::new(path.as_str());
        let escapes = path.is_empty()
            || relative.is_absolute()
            || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_)));
        if escapes {
            findings.push(format!(
                "[files_created] {field} = {path:?} is not a plain workspace-relative path (no `..`, `.`, or root)"
            ));
            continue;
        }
        if workspace.join(relative).exists() {
            findings.push(format!(
                "[files_created] {field} = {path:?} already exists in the fixture - the pair must name new files"
            ));
        }
        if !claims_extension(manifest, path) {
            findings.push(format!(
                "[files_created] {field} = {path:?} has none of the manifest's extensions ({})",
                manifest.extensions.join(", ")
            ));
        }
    }
    if pair.target == pair.importer {
        findings.push(format!("[files_created] target and importer are the same path ({:?})", pair.target));
    }
    findings
}

/// What [`run_files_created_session`] observed. `session.failure` set means
/// the run did not reach its verdict (spawn, handshake, timeout, apply error,
/// a write or index read failing); `import_rows` is then `None`.
pub(crate) struct FilesCreatedRun {
    pub session: Session,
    pub import_rows: Option<Vec<ImportRow>>,
}

/// The pair's files on disk, removed again however the run ends - the
/// expectations evaluated after it must see the fixture without them.
struct CreatedFiles {
    files: Vec<PathBuf>,
    /// Directories this run created, innermost first.
    dirs: Vec<PathBuf>,
}

impl CreatedFiles {
    fn write(&mut self, path: &Path, contents: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            let mut missing = Vec::new();
            let mut dir = parent;
            while !dir.exists() {
                missing.push(dir.to_path_buf());
                match dir.parent() {
                    Some(up) => dir = up,
                    None => break,
                }
            }
            fs::create_dir_all(parent)?;
            self.dirs.extend(missing);
        }
        self.files.push(path.to_path_buf());
        fs::write(path, contents)
    }
}

impl Drop for CreatedFiles {
    fn drop(&mut self) {
        for file in &self.files {
            let _ = fs::remove_file(file);
        }
        for dir in &self.dirs {
            let _ = fs::remove_dir(dir);
        }
    }
}

/// Drives `capabilities.files-created-resolves`' session: a fresh plugin
/// process and a fresh index, structural gate closed throughout.
///
/// 1. Spawn and handshake.
/// 2. `fileChanged` on `warm_file` - its answer proves the plugin processed a
///    request after building its project model, so the pair written next is
///    new to that model (without it an SDK plugin still in `load_project`
///    would see the files on disk, and the check would pass with the
///    notification ignored).
/// 3. Write `pair.target` and `pair.importer`.
/// 4. `filesCreated { filePaths: [importer, target] }`, with no `id`.
/// 5. `fileChanged` on the importer, then on the target - importer first,
///    which is the window the capability exists for.
/// 6. Read the importer's `IMPORTS` rows ([`import_rows`]), then remove both
///    files and finish the process.
pub(crate) fn run_files_created_session(
    manifest: &PluginManifest,
    scratch: &Scratch,
    pair: &FilesCreatedPair,
    warm_file: &str,
    timeouts: RoundTripTimeouts,
) -> FilesCreatedRun {
    let conn = match open_index(manifest) {
        Ok(conn) => conn,
        Err(err) => {
            return FilesCreatedRun {
                session: Session { failure: Some(format!("{err:#}")), ..Session::default() },
                import_rows: None,
            };
        }
    };
    let mut driver = match Driver::spawn(manifest, scratch, &conn, timeouts) {
        Ok(driver) => driver,
        Err(session) => return FilesCreatedRun { session: *session, import_rows: None },
    };
    let structural_only = Operation::FileChanged { semantic_pass_capable: false };
    let workspace = scratch.workspace();
    let mut created = CreatedFiles { files: Vec::new(), dirs: Vec::new() };

    let finish = |driver: Driver, import_rows| FilesCreatedRun { session: driver.finish(), import_rows };

    if !driver.step(
        &format!("files-created warm-up: fileChanged ({warm_file} unmodified)"),
        warm_file,
        &structural_only,
    ) {
        return finish(driver, None);
    }
    for (path, text) in [(&pair.target, &pair.target_text), (&pair.importer, &pair.importer_text)] {
        let on_disk = workspace.join(path);
        if let Err(err) = created.write(&on_disk, text) {
            driver.session.failure =
                Some(format!("files-created: failed to write {}: {err}", on_disk.display()));
            return finish(driver, None);
        }
    }
    let label = format!("files-created: filesCreated ({}, {})", pair.importer, pair.target);
    let message =
        ControlMessage::FilesCreated { file_paths: vec![pair.importer.clone(), pair.target.clone()] };
    if !driver.notify(&label, message) {
        return finish(driver, None);
    }
    for (role, path) in [("importer", &pair.importer), ("target", &pair.target)] {
        if !driver.step(
            &format!("files-created: fileChanged ({path}, the new {role})"),
            path,
            &structural_only,
        ) {
            return finish(driver, None);
        }
    }
    let rows = match import_rows(&conn, &pair.importer) {
        Ok(rows) => rows,
        Err(err) => {
            driver.session.failure = Some(format!(
                "files-created: reading {}'s IMPORTS edges back from the index: {err:#}",
                pair.importer
            ));
            return finish(driver, None);
        }
    };
    drop(created);
    finish(driver, Some(rows))
}

// --- resolution delta ----------------------------------------------------------

/// The `version` a [`VersionBump`] writes when the file declares none; one
/// that does gets this appended to its own.
const VERSION_BUMP_SUFFIX: &str = "-plugin-check";

/// `capabilities.resolution-delta-version-bump`'s edit: a fixture watch file
/// whose only change is a version - an edit no resolution model reads, so a
/// plugin declaring `resolution_delta` must answer `unchanged`.
pub(crate) struct VersionBump {
    /// Workspace-relative, `/`-separated.
    pub file_path: String,
    pub original: Vec<u8>,
    pub edited: Vec<u8>,
    /// The version that changed, for the report: "the top-level `version`",
    /// "`[package].version`", "the `require example.com/m` version".
    pub field: String,
}

/// The fixture watch file the version bump is made to, and the edit.
///
/// The rule, the only language-neutral one a manifest gives enough for: the
/// shallowest (then path-sorted) file under `workspace`, outside the
/// manifest's `exclude_dirs`, whose name matches one of its `watch_files` and
/// that [`version_bump_for`] can bump. Every other byte of the file stays.
/// `None` when no watch file qualifies (a `setup.cfg`, a JSONC config with
/// comments, a virtual Cargo workspace with no member manifest).
pub(crate) fn choose_version_bump(manifest: &PluginManifest, workspace: &Path) -> Option<VersionBump> {
    let matchers: Vec<_> = manifest.workspace.watch_files.iter().map(|glob| glob.compile_matcher()).collect();
    let mut found = Vec::new();
    let mut pending = vec![(workspace.to_path_buf(), String::new())];
    while let Some((dir, relative)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            match entry.file_type() {
                Ok(t) if t.is_dir() => {
                    if !manifest.workspace.exclude_dirs.contains(&name) {
                        pending.push((entry.path(), path));
                    }
                }
                Ok(t) if t.is_file() && matchers.iter().any(|m| m.is_match(&name)) => found.push(path),
                _ => {}
            }
        }
    }
    found.sort_by(|a, b| a.matches('/').count().cmp(&b.matches('/').count()).then_with(|| a.cmp(b)));
    found.into_iter().find_map(|file_path| {
        let original = fs::read(workspace.join(&file_path)).ok()?;
        let name = file_path.rsplit('/').next().unwrap_or(&file_path);
        let (edited, field) = version_bump_for(name, std::str::from_utf8(&original).ok()?)?;
        Some(VersionBump { file_path, original, edited: edited.into_bytes(), field })
    })
}

/// The bumped text of a watch file named `name`, and the version it changed,
/// by the file's format:
///
/// - `Cargo.toml`: `[package].version`, else `[workspace.package].version`
///   ([`toml_version_bump`]);
/// - `pyproject.toml`: `[project].version`, else `[tool.poetry].version`;
/// - `go.mod`: the first `require`d module's version, else the `go`
///   directive ([`go_mod_version_bump`]);
/// - any other name: the JSON rule ([`version_bump`]).
///
/// `None` when the file has no version of that shape to bump.
pub(crate) fn version_bump_for(name: &str, text: &str) -> Option<(String, String)> {
    match name {
        "Cargo.toml" => toml_version_bump(text, &[&["package"], &["workspace", "package"]]),
        "pyproject.toml" => toml_version_bump(text, &[&["project"], &["tool", "poetry"]]),
        "go.mod" => go_mod_version_bump(text),
        _ => version_bump(text).map(|edited| (edited, "the top-level `version`".to_string())),
    }
}

/// `text` (TOML) with the `version` string of the first of `tables` that has
/// one bumped by [`VERSION_BUMP_SUFFIX`], and that version's name. A
/// targeted textual edit: the suffix goes in before the closing quote of the
/// `version = "..."` line under the table's own `[header]`, and the result
/// is re-parsed and must equal the original but for that one string - so a
/// `version` in another table, a dotted `version.workspace = true` or a
/// multi-line string is never the one edited. No `version` is inserted where
/// there is none: `[project]` may list it in `dynamic`, a Cargo package may
/// inherit it, and either would make an insertion a real change.
pub(crate) fn toml_version_bump(text: &str, tables: &[&[&str]]) -> Option<(String, String)> {
    let original: toml::Table = text.parse().ok()?;
    tables.iter().find_map(|table| {
        let mut section = &original;
        for key in *table {
            section = section.get(*key)?.as_table()?;
        }
        let current = section.get("version")?.as_str()?;
        let bumped = format!("{current}{VERSION_BUMP_SUFFIX}");
        let mut expected = original.clone();
        let mut target = &mut expected;
        for key in *table {
            target = target.get_mut(*key)?.as_table_mut()?;
        }
        target.insert("version".to_string(), toml::Value::String(bumped));
        let edited = toml_section_lines(text, table).find_map(|(at, line)| {
            let close = toml_version_value_end(line)?;
            let insert_at = at + close;
            Some(format!("{}{VERSION_BUMP_SUFFIX}{}", &text[..insert_at], &text[insert_at..]))
        })?;
        let verified = edited.parse::<toml::Table>().is_ok_and(|parsed| parsed == expected);
        verified.then(|| (edited, format!("`[{}].version`", table.join("."))))
    })
}

/// The lines (byte offset, text without the newline) of the TOML section
/// headed `[table]`, header excluded, up to the next header.
fn toml_section_lines<'a>(
    text: &'a str,
    table: &'a [&'a str],
) -> impl Iterator<Item = (usize, &'a str)> + 'a {
    let mut inside = false;
    let mut at = 0;
    text.split_inclusive('\n').filter_map(move |raw| {
        let start = at;
        at += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            inside = toml_header_keys(trimmed).is_some_and(|keys| keys == table);
            return None;
        }
        inside.then_some((start, line))
    })
}

/// The dotted keys of a `[a.b]` header line (`trimmed` starts with `[`);
/// `None` for an array-of-tables `[[a]]` header.
fn toml_header_keys(trimmed: &str) -> Option<Vec<&str>> {
    let inner = trimmed.strip_prefix('[')?;
    if inner.starts_with('[') {
        return None;
    }
    let inner = &inner[..inner.find(']')?];
    Some(inner.split('.').map(|key| key.trim().trim_matches(|c| c == '"' || c == '\'')).collect())
}

/// For a `version = "..."` (or `'...'`) line, the byte offset in `line` of
/// the value's closing quote; `None` for any other line, a dotted
/// `version.x` key, or a multi-line string.
fn toml_version_value_end(line: &str) -> Option<usize> {
    let rest = line.trim_start().strip_prefix("version")?;
    let value = rest.trim_start().strip_prefix('=')?.trim_start();
    let value_at = line.len() - value.len();
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let body = &value[1..];
    if body.starts_with(quote) {
        return None; // `""` empty or `"""` multi-line - neither is bumped
    }
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if quote == '"' && !escaped => escaped = true,
            c if c == quote && !escaped => return Some(value_at + 1 + i),
            _ => escaped = false,
        }
    }
    None
}

/// `text` (a `go.mod`) with one version changed, and which. The first
/// `require`d module's version (single-line or block form) gets
/// [`VERSION_BUMP_SUFFIX`] appended - still a valid semver pre-release; a
/// `+incompatible` or other build-metadata version is passed over. With no
/// usable `require`, the `go` directive is rewritten to the same language
/// version: `1.22` becomes `1.22.0` and `1.22.3` becomes `1.22`.
///
/// Both are edits the Go plugin's workspace model never reads: it takes only
/// `module` from a `go.mod` and `use` from a `go.work`
/// (`plugins/go/workspace.go`'s doc), so its resolution facts cannot move.
pub(crate) fn go_mod_version_bump(text: &str) -> Option<(String, String)> {
    let mut in_require_block = false;
    let mut go_directive = None;
    let mut at = 0;
    for raw in text.split_inclusive('\n') {
        let start = at;
        at += raw.len();
        let line = raw.split("//").next().unwrap_or_default();
        let tokens = go_mod_tokens(line);
        let words: Vec<&str> = tokens.iter().map(|(_, word)| *word).collect();
        let required = match words.as_slice() {
            [")"] if in_require_block => {
                in_require_block = false;
                None
            }
            ["require", "("] => {
                in_require_block = true;
                None
            }
            [module, version] if in_require_block => Some((*module, *version, tokens[1].0)),
            ["require", module, version] => Some((*module, *version, tokens[2].0)),
            ["go", version] if !in_require_block => {
                go_directive.get_or_insert((start + tokens[1].0, *version));
                None
            }
            _ => None,
        };
        if let Some((module, version, offset)) = required {
            if version.starts_with('v') && !version.contains('+') {
                let end = start + offset + version.len();
                let edited = format!("{}{VERSION_BUMP_SUFFIX}{}", &text[..end], &text[end..]);
                return Some((edited, format!("the `require {module}` version")));
            }
        }
    }
    let (offset, version) = go_directive?;
    let parts: Vec<&str> = version.split('.').collect();
    if !parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let rewritten = match parts.as_slice() {
        [major, minor] => format!("{major}.{minor}.0"),
        [major, minor, _] => format!("{major}.{minor}"),
        _ => return None,
    };
    let edited = format!("{}{rewritten}{}", &text[..offset], &text[offset + version.len()..]);
    Some((edited, "the `go` directive".to_string()))
}

/// `line`'s whitespace-separated words and their byte offsets.
fn go_mod_tokens(line: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut word_start = None;
    for (i, c) in line.char_indices().chain(std::iter::once((line.len(), ' '))) {
        match (c.is_whitespace(), word_start) {
            (true, Some(from)) => {
                tokens.push((from, &line[from..i]));
                word_start = None;
            }
            (false, None) => word_start = Some(i),
            _ => {}
        }
    }
    tokens
}

/// `text` (JSON) with its top-level `version` string bumped: it gets
/// [`VERSION_BUMP_SUFFIX`] appended, or one is inserted when it has none.
/// `None` when `text` is not a JSON object, its `version` is not a string,
/// or no textual edit verifies:
/// each candidate is re-parsed and must equal the original but for
/// `version`, so a nested `"version"` key is never the one edited.
pub(crate) fn version_bump(text: &str) -> Option<String> {
    let serde_json::Value::Object(original) = serde_json::from_str(text).ok()? else { return None };
    let (bumped, candidates): (String, Vec<String>) = match original.get("version") {
        None => {
            let open = text.find('{')?;
            let bumped = format!("0.0.0{VERSION_BUMP_SUFFIX}");
            let separator = if original.is_empty() { "" } else { "," };
            let edited =
                format!("{}\"version\": \"{bumped}\"{separator}{}", &text[..=open], &text[open + 1..]);
            (bumped, vec![edited])
        }
        Some(serde_json::Value::String(current)) => {
            let bumped = format!("{current}{VERSION_BUMP_SUFFIX}");
            let quoted = serde_json::to_string(current).ok()?;
            let candidates = text
                .match_indices("\"version\"")
                .filter_map(|(at, key)| {
                    let rest = &text[at + key.len()..];
                    let after_colon = rest.trim_start().strip_prefix(':')?;
                    let value_at = text.len() - after_colon.trim_start().len();
                    text[value_at..].starts_with(&quoted).then(|| {
                        let replacement = serde_json::to_string(&bumped).unwrap_or_default();
                        format!("{}{replacement}{}", &text[..value_at], &text[value_at + quoted.len()..])
                    })
                })
                .collect();
            (bumped, candidates)
        }
        Some(_) => return None,
    };
    let mut expected = original;
    expected.insert("version".to_string(), serde_json::Value::String(bumped));
    candidates.into_iter().find(|edited| {
        serde_json::from_str::<serde_json::Value>(edited)
            .is_ok_and(|value| value.as_object().is_some_and(|object| *object == expected))
    })
}

/// What [`run_resolution_delta_session`] observed. `session.failure` set
/// means it did not reach a verdict; `result` is then `None`.
pub(crate) struct ResolutionDeltaRun {
    pub session: Session,
    pub result: Option<ResolutionChangedResult>,
}

/// Puts the watch file back however the run ends - the expectations
/// evaluated after it must see the fixture as it was.
struct Restore<'a> {
    path: PathBuf,
    original: &'a [u8],
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, self.original);
    }
}

/// Drives `capabilities.resolution-delta-version-bump`'s session: a fresh
/// plugin process, the bump written to disk, then one `resolutionChanged`
/// carrying bulk run 1's `resolutionFacts` - what core would send on that
/// save - and the file restored.
pub(crate) fn run_resolution_delta_session(
    manifest: &PluginManifest,
    scratch: &Scratch,
    conn: &IndexStore,
    bump: &VersionBump,
    previous_facts: String,
    timeouts: RoundTripTimeouts,
) -> ResolutionDeltaRun {
    let mut driver = match Driver::spawn(manifest, scratch, conn, timeouts) {
        Ok(driver) => driver,
        Err(session) => return ResolutionDeltaRun { session: *session, result: None },
    };
    let path = scratch.workspace().join(&bump.file_path);
    let label = format!("resolution-delta: resolutionChanged ({} after a version bump)", bump.file_path);
    let _restore = Restore { path: path.clone(), original: &bump.original };
    if let Err(err) = fs::write(&path, &bump.edited) {
        driver.session.failure = Some(format!("{label}: failed to write {}: {err}", path.display()));
        return ResolutionDeltaRun { session: driver.finish(), result: None };
    }
    let result = driver.resolution_changed(&label, &bump.file_path, Some(previous_facts));
    ResolutionDeltaRun { session: driver.finish(), result }
}

// --- control plane ------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    FileChanged,
    SemanticPass,
}

impl Method {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Method::FileChanged => "fileChanged",
            Method::SemanticPass => "semanticPass",
        }
    }
}

pub(crate) struct Response {
    pub diff: Result<FileChangeDiff, String>,
}

/// One id-carrying request the session sent, and the plugin's answer to it.
pub(crate) struct Exchange {
    /// The session step that sent it - one step can send two requests
    /// (`fileChanged` plus the `semanticPass` `apply_file_change` follows it
    /// with), and both carry the step's label.
    pub step: String,
    pub method: Method,
    pub file_paths: Vec<String>,
    pub response: Option<Response>,
}

#[derive(Default)]
pub(crate) struct Session {
    pub exchanges: Vec<Exchange>,
    /// Response frames that were not JSON at all, by step - a `shape` finding.
    pub malformed_frames: Vec<String>,
    pub failure: Option<String>,
    /// Index into `exchanges` of the `fileChanged` sent after the whitespace
    /// edit, once it was sent.
    pub whitespace_exchange: Option<usize>,
    /// Index into `exchanges` of the `fileChanged` sent after emptying the
    /// file - the step that exercises `deleteNodeIds`.
    pub emptied_exchange: Option<usize>,
    /// Index into `exchanges` of the `fileChanged` sent after the declaration
    /// edit, once it was answered and committed.
    pub declaration_exchange: Option<usize>,
    /// The edited file's node ids in the linked index once the file was
    /// restored, before any semantic pass could add to them - compared with
    /// `file_node_ids` right after the bulk walk.
    pub restored_file_ids: Option<BTreeSet<String>>,
    /// The edited file's nodes and ranges in the linked index once the
    /// declaration edit (`EditTarget::declaration`) was applied, before any
    /// semantic pass could add to them - compared with bulk run 3 of the
    /// edited tree. `None` if that step never ran. The file is left edited on
    /// disk afterwards, for bulk run 3 to walk.
    pub declaration_edit_ranges: Option<BTreeMap<String, StoredRange>>,
    /// Whether the semantic-engine marker already existed when the first
    /// `semanticPass` frame was written; `None` if none was ever written.
    pub marker_at_first_semantic_pass: Option<bool>,
    /// Every id-less `filesCreated` notification the session wrote, as
    /// (step label, the paths it listed) - only
    /// [`run_files_created_session`] sends one.
    pub notifications: Vec<(String, Vec<String>)>,
}

/// Copies every byte read through it, so the session can recover the raw
/// response frames `watcher::apply` consumed.
struct TeeReader<R> {
    inner: R,
    log: Vec<u8>,
}

impl<R: BufRead> Read for TeeReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.log.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for TeeReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        // `consume(amt)` only ever follows a `fill_buf` that returned at least
        // `amt` bytes, and a second `fill_buf` over a non-empty buffer returns
        // that same buffer without touching the pipe - so this re-borrow is a
        // lookup, not I/O.
        if let Ok(buffered) = self.inner.fill_buf() {
            let take = amt.min(buffered.len());
            self.log.extend_from_slice(&buffered[..take]);
        }
        self.inner.consume(amt);
    }
}

/// Holds each frame `jsonrpc::write_message` writes until its flush, records
/// it, and - for the first `semanticPass` - records whether the semantic
/// engine marker already exists *before* the frame reaches the plugin.
struct TeeWriter<W> {
    inner: W,
    pending: Vec<u8>,
    sent: Vec<ControlEnvelope>,
    marker: PathBuf,
    marker_at_first_semantic_pass: Option<bool>,
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let bytes = std::mem::take(&mut self.pending);
        let mut frames = Cursor::new(bytes.as_slice());
        while let Ok(Some(body)) = read_frame(&mut frames) {
            let Ok(envelope) = serde_json::from_slice::<ControlEnvelope>(&body) else { continue };
            if matches!(envelope.message, ControlMessage::SemanticPass { .. })
                && self.marker_at_first_semantic_pass.is_none()
            {
                self.marker_at_first_semantic_pass = Some(self.marker.exists());
            }
            self.sent.push(envelope);
        }
        self.inner.write_all(&bytes)?;
        self.inner.flush()
    }
}

enum Operation {
    FileChanged { semantic_pass_capable: bool },
    WholeProjectSemanticPass { sweep_language: Option<String>, timeout: Duration },
}

struct Driver<'a> {
    child: Child,
    reader: TeeReader<BufReader<ChildStdout>>,
    writer: TeeWriter<ChildStdin>,
    /// The plugin's own stderr, quoted into `session.failure` by
    /// [`Driver::finish`] - see [`StderrCapture`].
    stderr: StderrCapture,
    conn: &'a IndexStore,
    /// The project root the plugin was started on.
    workspace: PathBuf,
    timeouts: RoundTripTimeouts,
    /// The manifest's language, which scopes a semantic pass's linked edges.
    language: String,
    next_id: i64,
    session: Session,
}

/// Runs the control-plane session against the scratch workspace. See
/// `checks`' module doc for why these steps, in this order.
///
/// 1. `fileChanged` on the unmodified file.
/// 2. `fileChanged` after the whitespace edit.
/// 3. `fileChanged` after emptying the file.
/// 4. `fileChanged` after restoring it - then the index snapshot of its ids.
/// 5. `fileChanged` after the declaration edit ([`declaration_edit`]), when
///    the file has a declaration - then the index snapshot of its ranges. The
///    file stays edited from here on.
/// 6. whole-project `semanticPass`, when the manifest declares the capability.
/// 7. `fileChanged` on the unchanged file, through the manifest's own gate.
///
/// Steps 1-5 are sent with the semantic gate closed - the state of a plugin
/// woken for structural work only, which is when a plugin with an eagerly
/// started engine does the most harm - so the first `semanticPass` the plugin
/// ever sees is step 6's (or step 7's per-file one). Step labels number the
/// `fileChanged` requests, not the steps.
pub(crate) fn run_session(
    manifest: &PluginManifest,
    scratch: &Scratch,
    conn: &IndexStore,
    target: &EditTarget,
    timeouts: RoundTripTimeouts,
    whole_project_timeout: Duration,
) -> Session {
    let mut driver = match Driver::spawn(manifest, scratch, conn, timeouts) {
        Ok(driver) => driver,
        Err(session) => return *session,
    };

    let file = target.file_path.as_str();
    let workspace_file = scratch.workspace().join(file);
    let structural_only = Operation::FileChanged { semantic_pass_capable: false };

    let steps: [(String, Option<&[u8]>); 4] = [
        (format!("fileChanged #1 ({file} unmodified)"), None),
        (
            format!(
                "fileChanged #2 ({file} after a whitespace-only edit at the end of line {})",
                target.line
            ),
            Some(&target.edited),
        ),
        (format!("fileChanged #3 ({file} emptied)"), Some(&[])),
        (format!("fileChanged #4 ({file} restored)"), Some(&target.original)),
    ];
    for (index, (label, contents)) in steps.into_iter().enumerate() {
        if let Some(contents) = contents {
            if let Err(err) = fs::write(&workspace_file, contents) {
                driver.session.failure =
                    Some(format!("{label}: failed to write {}: {err}", workspace_file.display()));
                return driver.finish();
            }
        }
        let first_exchange = driver.session.exchanges.len();
        if !driver.step(&label, file, &structural_only) {
            return driver.finish();
        }
        match index {
            1 => driver.session.whitespace_exchange = Some(first_exchange),
            2 => driver.session.emptied_exchange = Some(first_exchange),
            _ => {}
        }
    }

    match file_node_ids(driver.conn, file) {
        Ok(ids) => driver.session.restored_file_ids = Some(ids),
        Err(err) => {
            driver.session.failure = Some(format!("reading {file}'s node ids back from the index: {err:#}"));
            return driver.finish();
        }
    }

    // Through the cache step 4 just warmed with the original text, so the
    // plugin answers with a diff against it - for the TS plugin, the delete
    // plus re-upsert of a symbol whose unchanged edges are not re-sent.
    if let Some(edit) = &target.declaration {
        let label = format!(
            "fileChanged #5 ({file} after a line break before line {}, the last line of {:?})",
            edit.line, edit.node_name
        );
        if let Err(err) = fs::write(&workspace_file, &edit.edited) {
            driver.session.failure =
                Some(format!("{label}: failed to write {}: {err}", workspace_file.display()));
            return driver.finish();
        }
        let first_exchange = driver.session.exchanges.len();
        if !driver.step(&label, file, &structural_only) {
            return driver.finish();
        }
        driver.session.declaration_exchange = Some(first_exchange);
        match file_node_ranges(driver.conn, file) {
            Ok(ranges) => driver.session.declaration_edit_ranges = Some(ranges),
            Err(err) => {
                driver.session.failure =
                    Some(format!("reading {file}'s node ranges back from the index: {err:#}"));
                return driver.finish();
            }
        }
    }

    if manifest.capabilities.semantic_pass {
        let operation = Operation::WholeProjectSemanticPass {
            sweep_language: manifest.capabilities.semantic_sweep.then(|| manifest.language.clone()),
            timeout: whole_project_timeout,
        };
        if !driver.step("semanticPass #1 (whole project)", file, &operation) {
            return driver.finish();
        }
        // Marks the whole-project pass completed. `daemon::semantic`
        // records this the moment the same `apply_semantic_pass` call
        // returns `Ok` - it is what makes
        // `language_state.semanticPassAt` mean "this language's semantic
        // tier has run", and the index the expectations are then evaluated
        // against is supposed to be the index a daemon would have left
        // behind. Without it every expectation ran against a linked index
        // that still described itself as owing a semantic pass it had in
        // fact just completed, which nothing noticed while nothing read that
        // column - `mcp::provenance` reads it, so the gap became visible as
        // a response claiming the semantic tier was absent in the one arm
        // that had just driven it successfully.
        if let Err(err) =
            driver.conn.with(|conn| schema::record_language_semantic_pass(conn, &manifest.language))
        {
            driver.session.failure =
                Some(format!("recording {}'s completed semantic pass: {err:#}", manifest.language));
            return driver.finish();
        }
    }

    let gated = Operation::FileChanged { semantic_pass_capable: manifest.capabilities.semantic_pass };
    let label = format!("fileChanged #6 ({file} unchanged, through the manifest's semantic_pass gate)");
    driver.step(&label, file, &gated);
    driver.finish()
}

impl<'a> Driver<'a> {
    /// Spawns the plugin on the scratch workspace and verifies its
    /// handshake - the start every session shares. `Err` carries the
    /// finished [`Session`] (boxed: it is large) whose `failure` says why
    /// nothing more can run.
    fn spawn(
        manifest: &PluginManifest,
        scratch: &Scratch,
        conn: &'a IndexStore,
        timeouts: RoundTripTimeouts,
    ) -> Result<Driver<'a>, Box<Session>> {
        let mut command = Command::new(&manifest.command);
        command
            .args(&manifest.args)
            .arg(scratch.workspace())
            // As in `run_bulk` above - see `daemon::manifest::MANIFEST_PATH_ENV`.
            .env(crate::daemon::manifest::MANIFEST_PATH_ENV, manifest.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // As in `run_bulk` above - see [`StderrCapture`].
            .stderr(Stdio::piped());
        scratch.isolate(&mut command);

        // As in `run_bulk` above - see `daemon::plugin::missing_workspace_binary_hint`'s
        // doc comment. `missing_workspace_binary_hint` alone, not the
        // combined `missing_plugin_binary_hint`, for the same reason given there:
        // the typescript plugin's own spawn failure here is already honest and
        // must stay untouched.
        if let Some(hint) = crate::daemon::plugin::missing_workspace_binary_hint(&manifest.command) {
            return Err(Box::new(Session { failure: Some(hint), ..Session::default() }));
        }

        let mut child = match crate::process::spawn_serialized(&mut command) {
            Ok(child) => child,
            Err(err) => {
                return Err(Box::new(Session {
                    failure: Some(format!("failed to spawn `{}`: {err}", manifest.command.display())),
                    ..Session::default()
                }));
            }
        };
        let stderr = StderrCapture::attach(&mut child);
        let reader = TeeReader {
            inner: BufReader::new(child.stdout.take().expect("stdout was piped")),
            log: Vec::new(),
        };
        let writer = TeeWriter {
            inner: child.stdin.take().expect("stdin was piped"),
            pending: Vec::new(),
            sent: Vec::new(),
            marker: scratch.semantic_engine_marker(),
            marker_at_first_semantic_pass: None,
        };
        let mut driver = Driver {
            child,
            reader,
            writer,
            stderr,
            conn,
            workspace: scratch.workspace(),
            timeouts,
            language: manifest.language.clone(),
            next_id: 1,
            session: Session::default(),
        };

        if let Err(err) = driver.handshake(manifest) {
            driver.session.failure = Some(format!("handshake: {err:#}"));
            return Err(Box::new(driver.finish()));
        }
        Ok(driver)
    }

    fn handshake(&mut self, manifest: &PluginManifest) -> Result<()> {
        let Driver { child, reader, timeouts, .. } = self;
        let mut kill = || {
            let _ = child.kill();
        };
        let handshake: Handshake = read_message_with_timeout(reader, timeouts.file_changed, &mut kill)
            .context("failed to read the plugin's handshake")?
            .context("the plugin closed its stdout before sending a handshake")?;
        self.reader.log.clear();
        handshake::verify(&handshake)?;
        if handshake.language != manifest.language {
            bail!(
                "the manifest declares language {:?} but the handshake reports {:?} - the daemon refuses to load this plugin",
                manifest.language,
                handshake.language
            );
        }
        Ok(())
    }

    /// Runs one operation through `watcher::apply` and records what crossed
    /// the wire. Returns whether the session can continue.
    fn step(&mut self, label: &str, file: &str, operation: &Operation) -> bool {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        let embedding = EmbeddingPipeline::disabled();
        let result = {
            let Driver { child, reader, writer, conn, workspace, timeouts, language, .. } = self;
            let mut kill = || {
                let _ = child.kill();
            };
            // `conn` is the store, not a held guard: both functions below
            // take it only for as long as each of their own steps needs it.
            match operation {
                Operation::FileChanged { semantic_pass_capable } => apply_file_change(
                    reader,
                    writer,
                    conn,
                    workspace,
                    language,
                    file,
                    id,
                    &embedding,
                    timeouts.file_changed,
                    timeouts.semantic_pass_file,
                    *semantic_pass_capable,
                    &mut kill,
                )
                // A kit session runs no whole-language reindex; what the
                // answer's `affected` re-extracted is already committed.
                .map(|_| ()),
                Operation::WholeProjectSemanticPass { sweep_language, timeout } => apply_semantic_pass(
                    reader,
                    writer,
                    conn,
                    language,
                    sweep_language.as_deref(),
                    Vec::new(),
                    id,
                    &embedding,
                    *timeout,
                    &mut kill,
                )
                // A listed incomplete pass is recorded residual in the scratch
                // index like any other, which a daemon then finishes on its
                // next start. A kit session has no next start, and its
                // expectations need the fully linked index a completed session
                // leaves behind, so a pass that left files owed fails the
                // session, quoting the plugin's own reason (GM-550).
                .and_then(|outcome| match outcome {
                    SemanticPassOutcome::Residual { left } if left > 0 => {
                        let reason = schema::semantic_residual_reason(&conn.read(), language)
                            .ok()
                            .flatten()
                            .unwrap_or_else(|| "the plugin gave no reason".to_string());
                        bail!("the whole-project semantic pass left {left} file(s) unfinished: {reason}")
                    }
                    _ => Ok(()),
                }),
            }
        };
        self.record(label, result)
    }

    /// Sends one `resolutionChanged` request - the envelope
    /// `PluginProcess::send_resolution_changed` builds - and reads its answer
    /// under the `fileChanged` timeout. `None` when the session cannot
    /// continue (the failure is recorded).
    fn resolution_changed(
        &mut self,
        label: &str,
        file: &str,
        previous_facts: Option<String>,
    ) -> Option<ResolutionChangedResult> {
        let id = RequestId::Number(self.next_id);
        self.next_id += 1;
        let envelope = ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id.clone()),
            message: ControlMessage::ResolutionChanged { file_path: file.to_string(), previous_facts },
        };
        let answer = {
            let Driver { child, reader, writer, timeouts, .. } = self;
            let mut kill = || {
                let _ = child.kill();
            };
            write_message(writer, &envelope)
                .context("failed to write the resolutionChanged request")
                .and_then(|()| {
                    read_message_with_timeout::<ResolutionChangedResponse, _>(
                        reader,
                        timeouts.file_changed,
                        &mut kill,
                    )
                    .context("failed to read the plugin's resolutionChanged response")?
                    .context("the plugin closed its output before answering resolutionChanged")
                })
                .and_then(|response| {
                    if response.id != id {
                        bail!(
                            "resolutionChanged response id {:?} does not match request id {:?}",
                            response.id,
                            id
                        );
                    }
                    Ok(response.result)
                })
        };
        match answer {
            Ok(result) => self.record(label, Ok(())).then_some(result),
            Err(err) => {
                self.record(label, Err(err));
                None
            }
        }
    }

    /// Writes `message` as an id-less notification - the envelope
    /// `PluginProcess::notify_files_created` builds - and records it. No
    /// answer is awaited: a notification has none. Returns whether the
    /// session can continue.
    fn notify(&mut self, label: &str, message: ControlMessage) -> bool {
        let envelope = ControlEnvelope { jsonrpc: JSONRPC_VERSION.to_string(), id: None, message };
        let result = write_message(&mut self.writer, &envelope).context("failed to send the notification");
        self.record(label, result)
    }

    fn record(&mut self, label: &str, result: Result<()>) -> bool {
        let sent = std::mem::take(&mut self.writer.sent);
        let received = std::mem::take(&mut self.reader.log);

        let mut frames = Vec::new();
        let mut cursor = Cursor::new(received.as_slice());
        loop {
            match read_frame(&mut cursor) {
                Ok(Some(body)) => match serde_json::from_slice::<serde_json::Value>(&body) {
                    Ok(value) => frames.push(value),
                    Err(err) => self
                        .session
                        .malformed_frames
                        .push(format!("{label}: response frame is not JSON: {err}")),
                },
                Ok(None) => break,
                // A frame cut short by a kill - the timeout the failure below
                // already reports, not a shape problem of its own.
                Err(_) => break,
            }
        }

        let mut unanswered = Vec::new();
        for envelope in sent {
            let Some(id) = &envelope.id else {
                // A notification: nothing to pair with an answer, but the
                // checks still need to see what was sent and when.
                if let ControlMessage::FilesCreated { file_paths } = envelope.message {
                    self.session.notifications.push((label.to_string(), file_paths));
                }
                continue;
            };
            let (method, file_paths) = match envelope.message {
                ControlMessage::FileChanged { file_path, .. } => (Method::FileChanged, vec![file_path]),
                ControlMessage::SemanticPass { file_paths, .. } => (Method::SemanticPass, file_paths),
                _ => continue,
            };
            let id = serde_json::to_value(id).unwrap_or_default();
            let response = frames.iter().find(|frame| frame.get("id") == Some(&id)).map(|raw| Response {
                diff: serde_json::from_value::<FileChangeResponse>(raw.clone())
                    .map(|response| response.result)
                    .map_err(|err| err.to_string()),
            });
            if response.is_none() {
                unanswered.push(method.name());
            }
            self.session.exchanges.push(Exchange { step: label.to_string(), method, file_paths, response });
        }

        let failure = match result {
            Err(err) => Some(format!("{label}: {err:#}")),
            // `apply_file_change` reports and drops a failed follow-up
            // semantic pass by design - a daemon keeps the structural answer -
            // so a timed-out one only shows up here, as a request with no
            // answer.
            Ok(()) if !unanswered.is_empty() => Some(format!(
                "{label}: the plugin never answered its {} request (see the plugin's stderr above)",
                unanswered.join(" and ")
            )),
            Ok(()) => match self.child.try_wait() {
                Ok(Some(status)) => Some(format!("{label}: the plugin exited ({status}) after answering")),
                _ => None,
            },
        };
        match failure {
            Some(failure) => {
                self.session.failure = Some(failure);
                false
            }
            None => true,
        }
    }

    /// Closes the plugin's stdin - the daemon's own shutdown signal - waits
    /// [`SHUTDOWN_GRACE`], then kills whatever is left.
    fn finish(self) -> Session {
        let Driver { mut child, reader, writer, stderr, mut session, .. } = self;
        session.marker_at_first_semantic_pass = writer.marker_at_first_semantic_pass;
        drop(writer);
        drop(reader);
        if wait_with_deadline(&mut child, SHUTDOWN_GRACE).is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // After the child is gone, so a plugin that explained itself on the
        // way out is quoted having said all of it.
        if session.failure.is_some() {
            stderr.wait_drained();
        }
        session.failure = session.failure.take().map(|failure| stderr.explain(failure));
        session
    }
}

#[cfg(test)]
mod tests;
