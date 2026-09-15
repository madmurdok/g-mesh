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
//! GM-277's expectations will query exactly the linked state this builds.
//!
//! Three things differ from `daemon::plugin::PluginProcess` /
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
//! whitespace-edit and emptied-file steps write to it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use crate::daemon::bulk_index::{self, BulkIndexSummary, BULK_INDEX_FLAG};
use crate::daemon::manifest::PluginManifest;
use crate::daemon::plugin::RoundTripTimeouts;
use crate::embedding::EmbeddingPipeline;
use crate::graph::{imports, symbol_links};
use crate::paths;
use crate::protocol::handshake;
use crate::protocol::jsonrpc::{read_frame, read_message_with_timeout};
use crate::protocol::ndjson::BulkItem;
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, Handshake, NodeKind, RequestId,
};
use crate::storage::schema;
use crate::watcher::apply::{apply_file_change, apply_semantic_pass};

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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    scratch.isolate(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return BulkRun {
                bytes: Vec::new(),
                lines: Vec::new(),
                failure: Some(format!("failed to spawn `{}`: {err}", manifest.command.display())),
            };
        }
    };
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

/// A fresh in-memory index, configured like the daemon's own connection
/// (`storage::connection::open` sets nothing a schema-only in-memory database
/// needs - notably not `foreign_keys`, so a dangling edge is stored exactly as
/// the daemon would store it, and it is `checks::stream_order` that reports
/// it rather than a constraint error that would end the session).
pub(crate) fn open_index() -> Result<Mutex<Connection>> {
    let conn = Connection::open_in_memory().context("failed to open an in-memory index")?;
    schema::apply(&conn)?;
    Ok(Mutex::new(conn))
}

/// Commits one bulk stream through the daemon's own batching and links it
/// project-wide, exactly as `bulk_index::run` does after its walk.
pub(crate) fn ingest_and_link(conn: &Mutex<Connection>, bytes: &[u8]) -> Result<()> {
    let mut summary = BulkIndexSummary::default();
    bulk_index::ingest(Cursor::new(bytes.to_vec()), conn, &mut summary, &EmbeddingPipeline::disabled())?;
    let mut conn = conn.lock().unwrap();
    imports::link_all(&mut conn).context("failed to link the walk's resolved imports")?;
    symbol_links::link_all(&mut conn).context("failed to link the walk's cross-file symbol usages")?;
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
pub(crate) fn file_node_ids(conn: &Mutex<Connection>, file: &str) -> Result<BTreeSet<String>> {
    let conn = conn.lock().unwrap();
    let mut statement = conn.prepare("SELECT id FROM nodes WHERE filePath = ?1")?;
    let ids = statement.query_map([file], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

// --- the edited file ----------------------------------------------------------

/// The file the control-plane session edits, and the one whitespace edit made
/// to it.
pub(crate) struct EditTarget {
    /// Project-relative, exactly as the plugin's own `File` node spells it -
    /// which is also what `fileChanged` carries.
    pub file_path: String,
    /// 1-based line the space is inserted at the end of.
    pub line: usize,
    pub original: Vec<u8>,
    pub edited: Vec<u8>,
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
/// - **Appending a newline at EOF** moves the end of the whole-file range: the
///   TS plugin's `File` node ends at tree-sitter's root end, `(lines, 0)`,
///   measured on this repo's own fixture as `(8, 0)` for an 8-line file, so a
///   ninth newline legitimately changes it to `(9, 0)`.
/// - **Inserting a blank line** shifts every declaration below it.
/// - **Trailing space on an arbitrary line** can land inside a multi-line
///   doc comment or string, whose text a plugin legitimately reports.
///
/// A space before the *last* newline moves nothing: no content follows it on
/// its line, every later line (at most the final one, when the file does not
/// end in a newline) keeps its row and columns, so the root's end is
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
            best = Some((count, EditTarget { file_path: file_path.to_string(), line, original, edited }));
        }
    }
    best.map(|(_, target)| target)
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
    /// The edited file's node ids in the linked index once the file was
    /// restored, before any semantic pass could add to them - compared with
    /// `file_node_ids` right after the bulk walk.
    pub restored_file_ids: Option<BTreeSet<String>>,
    /// Whether the semantic-engine marker already existed when the first
    /// `semanticPass` frame was written; `None` if none was ever written.
    pub marker_at_first_semantic_pass: Option<bool>,
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
    WholeProjectSemanticPass { timeout: Duration },
}

struct Driver<'a> {
    child: Child,
    reader: TeeReader<BufReader<ChildStdout>>,
    writer: TeeWriter<ChildStdin>,
    conn: &'a Mutex<Connection>,
    timeouts: RoundTripTimeouts,
    next_id: i64,
    session: Session,
}

/// Runs the control-plane session against the scratch workspace. See
/// `checks`' module doc for why these steps, in this order.
///
/// 1. `fileChanged` on the unmodified file.
/// 2. `fileChanged` after the whitespace edit.
/// 3. `fileChanged` after emptying the file.
/// 4. `fileChanged` after restoring it - then the index snapshot.
/// 5. whole-project `semanticPass`, when the manifest declares the capability.
/// 6. `fileChanged` on the unchanged file, through the manifest's own gate.
///
/// Steps 1-4 are sent with the semantic gate closed - the state of a plugin
/// woken for structural work only, which is when a plugin with an eagerly
/// started engine does the most harm - so the first `semanticPass` the plugin
/// ever sees is step 5's (or step 6's per-file one).
pub(crate) fn run_session(
    manifest: &PluginManifest,
    scratch: &Scratch,
    conn: &Mutex<Connection>,
    target: &EditTarget,
    timeouts: RoundTripTimeouts,
    whole_project_timeout: Duration,
) -> Session {
    let mut command = Command::new(&manifest.command);
    command
        .args(&manifest.args)
        .arg(scratch.workspace())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    scratch.isolate(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return Session {
                failure: Some(format!("failed to spawn `{}`: {err}", manifest.command.display())),
                ..Session::default()
            };
        }
    };
    let reader =
        TeeReader { inner: BufReader::new(child.stdout.take().expect("stdout was piped")), log: Vec::new() };
    let writer = TeeWriter {
        inner: child.stdin.take().expect("stdin was piped"),
        pending: Vec::new(),
        sent: Vec::new(),
        marker: scratch.semantic_engine_marker(),
        marker_at_first_semantic_pass: None,
    };
    let mut driver =
        Driver { child, reader, writer, conn, timeouts, next_id: 1, session: Session::default() };

    if let Err(err) = driver.handshake(manifest) {
        driver.session.failure = Some(format!("handshake: {err:#}"));
        return driver.finish();
    }

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

    if manifest.capabilities.semantic_pass {
        let operation = Operation::WholeProjectSemanticPass { timeout: whole_project_timeout };
        if !driver.step("semanticPass #1 (whole project)", file, &operation) {
            return driver.finish();
        }
    }

    let gated = Operation::FileChanged { semantic_pass_capable: manifest.capabilities.semantic_pass };
    let label = format!("fileChanged #5 ({file} unchanged, through the manifest's semantic_pass gate)");
    driver.step(&label, file, &gated);
    driver.finish()
}

impl Driver<'_> {
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
            let Driver { child, reader, writer, conn, timeouts, .. } = self;
            let mut conn = conn.lock().unwrap();
            let mut kill = || {
                let _ = child.kill();
            };
            match operation {
                Operation::FileChanged { semantic_pass_capable } => apply_file_change(
                    reader,
                    writer,
                    &mut conn,
                    file,
                    id,
                    &embedding,
                    timeouts.file_changed,
                    timeouts.semantic_pass_file,
                    *semantic_pass_capable,
                    &mut kill,
                ),
                Operation::WholeProjectSemanticPass { timeout } => apply_semantic_pass(
                    reader,
                    writer,
                    &mut conn,
                    Vec::new(),
                    id,
                    &embedding,
                    *timeout,
                    &mut kill,
                ),
            }
        };
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
            let Some(id) = &envelope.id else { continue };
            let (method, file_paths) = match envelope.message {
                ControlMessage::FileChanged { file_path } => (Method::FileChanged, vec![file_path]),
                ControlMessage::SemanticPass { file_paths } => (Method::SemanticPass, file_paths),
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
        let Driver { mut child, reader, writer, mut session, .. } = self;
        session.marker_at_first_semantic_pass = writer.marker_at_first_semantic_pass;
        drop(writer);
        drop(reader);
        if wait_with_deadline(&mut child, SHUTDOWN_GRACE).is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        session
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whitespace_edit_goes_before_the_last_newline() {
        let (edited, line) = whitespace_edit(b"a\nb\n").unwrap();
        assert_eq!(edited, b"a\nb \n");
        assert_eq!(line, 2);
    }

    /// A file that does not end in a newline keeps its final line untouched:
    /// the space goes at the end of the line before it.
    #[test]
    fn the_whitespace_edit_leaves_an_unterminated_final_line_alone() {
        let (edited, line) = whitespace_edit(b"a\nb").unwrap();
        assert_eq!(edited, b"a \nb");
        assert_eq!(line, 1);
    }

    #[test]
    fn the_whitespace_edit_goes_before_the_carriage_return_of_a_crlf_file() {
        let (edited, line) = whitespace_edit(b"a\r\nb\r\n").unwrap();
        assert_eq!(edited, b"a\r\nb \r\n");
        assert_eq!(line, 2);
    }

    #[test]
    fn a_file_without_any_newline_offers_no_whitespace_edit() {
        assert!(whitespace_edit(b"export const a = 1;").is_none());
    }

    #[test]
    fn bulk_line_numbers_count_blank_lines() {
        let lines = parse_bulk_lines(b"not json\n\n{\"broken\":\r\n");
        assert_eq!(lines.iter().map(|l| l.line_no).collect::<Vec<_>>(), vec![1, 3]);
        assert!(lines.iter().all(|l| l.item.is_err()));
    }

    /// The tee must hand back exactly the bytes the consumer read, whichever
    /// of `read`/`fill_buf`+`consume` the consumer used - `read_frame` uses
    /// both (`read_until` for headers, `read_exact` for the body).
    #[test]
    fn the_tee_reader_logs_exactly_what_was_consumed() {
        let frame = b"Content-Length: 2\r\n\r\n{}Content-Length: 4\r\n\r\nnull";
        let mut tee =
            TeeReader { inner: BufReader::with_capacity(3, Cursor::new(frame.to_vec())), log: Vec::new() };
        assert_eq!(read_frame(&mut tee).unwrap().unwrap(), b"{}");
        assert_eq!(tee.log, b"Content-Length: 2\r\n\r\n{}");
        assert_eq!(read_frame(&mut tee).unwrap().unwrap(), b"null");
        assert_eq!(tee.log, frame);
    }
}
