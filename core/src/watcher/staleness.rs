//! Query-time staleness check with synchronous fallback reindex.
//!
//! The filesystem watcher (`watcher::mod`, `watcher::debounce`,
//! `watcher::burst`) is the normal path that keeps the index up to date, but
//! it can miss changes entirely - not just on some filesystems (network
//! drives, WSL's polling fallback), but on *any* filesystem whenever the
//! change happens while this project's daemon is not running at all: nothing
//! re-walks an already-current index on restart (see
//! `storage::schema::ensure_current`), and a watcher that has not started yet
//! cannot have seen an event that occurred before it did. This module is the
//! safety net: before a handler answers a query that touches a given file, it
//! calls [`ensure_fresh`], which compares the file's on-disk state against
//! what's recorded in the `indexed_files` table and, on a mismatch,
//! synchronously reindexes it via `watcher::apply::apply_file_change` before
//! the caller proceeds - so no tool response is ever silently based on
//! stale data.
//!
//! Wired into `mcp::GMeshMcpServer` via
//! `daemon::lifecycle::PluginSupervisor::ensure_fresh` /
//! `daemon::plugin::PluginProcess::ensure_fresh`, for the three tools that
//! anchor a query on one specific file (`find_definition`,
//! `get_file_outline`, `get_dependencies`). See that module's doc comment for
//! why this is a materially different gap from the one
//! `daemon::indexing_status`'s "Why the incremental-edit watcher path does
//! not re-arm this" section reasons about, and for why the other four tools
//! (symbol-anchored, not file-anchored) are deliberately left out.
//!
//! # Two-tier mtime/hash design
//!
//! A file's baseline is recorded as *both* an mtime (milliseconds since the
//! Unix epoch) and a SHA-256 content hash:
//!
//! 1. **Fast path (cheap, no file read):** `stat` the file and compare its
//!    current mtime against the recorded one. If they match exactly, assume
//!    the file is unchanged and skip everything else. A query that touches
//!    many files can't afford to read and hash every one of them on every
//!    call, so this has to be the common case.
//! 2. **Fallback (authoritative, reads the file):** if the mtime differs -
//!    or there's no recorded row at all, i.e. the file was never indexed -
//!    read the file and compute its SHA-256 hash:
//!    - If there's a prior recorded hash and it matches the freshly
//!      computed one, the file's *content* hasn't actually changed (e.g. a
//!      `touch` with no edit, or a save that produced byte-identical
//!      content) - just refresh the recorded mtime (so the next check hits
//!      the fast path again) without paying for a reindex.
//!    - Otherwise (hash differs, or there was no prior record at all) the
//!      file is genuinely stale: synchronously reindex it via
//!      `apply_file_change`, then record the new mtime+hash as the new
//!      baseline.
//!
//! This is what makes the check both cheap in the common case and correct
//! on filesystems where mtimes are coarse or unreliable, which is exactly
//! the "network drives, WSL" motivation in the ticket this module
//! implements.
//!
//! The baseline is only written *after* `apply_file_change` succeeds, never
//! before - if the reindex fails, no false "this file is now fresh" record
//! is left behind.
//!
//! # Baselines from the bulk walk (GM-401)
//!
//! A cold-start walk (`daemon::bulk_index::run`) records a baseline for each
//! file it indexed, via [`record_walk_baselines`], so the first query of an
//! untouched file after a walk takes the fast path above rather than a
//! synchronous reindex. That function's doc comment has the argument for why
//! such a row never vouches for bytes the walk did not see.

use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::embedding::EmbeddingPipeline;
use crate::protocol::types::RequestId;
use crate::storage::write::upsert_indexed_file;
use crate::watcher::apply::apply_file_change;

/// What [`ensure_fresh`] did to bring a file's index up to date. All three
/// non-fresh variants imply `apply_file_change` was actually invoked
/// (a synchronous reindex happened); `AlreadyFresh` implies it was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StalenessOutcome {
    /// Recorded mtime matched current on-disk mtime - no file read, no
    /// reindex. The common case.
    AlreadyFresh,
    /// mtime differed but the freshly computed content hash matched the
    /// recorded one - content is unchanged, so only the mtime baseline was
    /// refreshed. No reindex.
    MtimeMismatchContentUnchanged,
    /// No `indexed_files` row existed for this file at all (never
    /// indexed) - treated as stale, reindexed.
    ReindexedNoPriorRecord,
    /// mtime differed and the freshly computed content hash did not match
    /// the recorded one - genuinely stale, reindexed.
    ReindexedViaHashMismatch,
}

impl StalenessOutcome {
    /// Whether this outcome means a synchronous reindex actually happened.
    pub fn reindexed(self) -> bool {
        !matches!(self, StalenessOutcome::AlreadyFresh | StalenessOutcome::MtimeMismatchContentUnchanged)
    }
}

/// The context [`ensure_fresh`] attaches when the reindex itself - the plugin
/// round trip and its commit - failed, as opposed to anything around it (a
/// file that could not be stat'ed or hashed, a baseline that could not be
/// written).
///
/// A type rather than only a message so that a caller can ask
/// `err.downcast_ref::<ReindexFailed>()` instead of matching text:
/// `daemon::plugin::PluginProcess::ensure_fresh` relaunches its plugin on
/// this failure and on nothing else around it (GM-293, ported by GM-294),
/// because only this one can leave the plugin's cached copy of the file ahead
/// of the index. A timeout wears this context too - it is a failed round
/// trip - so that caller also rules out `protocol::jsonrpc::is_timeout`,
/// which already has its own relaunch path.
#[derive(Debug, Clone, Copy)]
pub struct ReindexFailed;

impl std::fmt::Display for ReindexFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed to synchronously reindex stale file")
    }
}

/// Compares `file_path` (resolved against `project_root`) against the
/// baseline recorded in the `indexed_files` table and, on a mismatch,
/// synchronously reindexes it by calling
/// `watcher::apply::apply_file_change` (which sends a `FileChanged`
/// request over `writer`, reads the plugin's diff back off `reader`, and
/// commits it). See the module docs for the full two-tier mtime/hash
/// decision procedure.
///
/// `file_path` is expected to be project-relative, matching the convention
/// used elsewhere for `filePath` columns and `FileChanged` requests;
/// `project_root` + `file_path` are joined with `Path::join` to locate the
/// real file on disk.
///
/// `file_changed_timeout`/`semantic_pass_timeout`/`on_timeout` are forwarded
/// to `apply_file_change` unchanged - this synchronous reindex is a reparse
/// settling exactly like any other (see that function's own doc comment on
/// why the semantic pass rides along), so it is bound by the very same
/// per-method budgets, not a query-time timeout of its own.
///
/// `semantic_pass_capable` is forwarded to `apply_file_change` unchanged too
/// - see that function's own doc comment. The caller (`daemon::plugin::
/// PluginProcess::ensure_fresh`) is what owns the manifest this comes from.
#[allow(clippy::too_many_arguments)]
pub fn ensure_fresh<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &Mutex<Connection>,
    project_root: &Path,
    file_path: &str,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    file_changed_timeout: Duration,
    semantic_pass_timeout: Duration,
    semantic_pass_capable: bool,
    on_timeout: &mut dyn FnMut(),
) -> Result<StalenessOutcome> {
    let decision = {
        let guard = conn.lock().unwrap();
        decide(&guard, project_root, file_path)?
    };
    match decision {
        Decision::AlreadyFresh => Ok(StalenessOutcome::AlreadyFresh),
        Decision::ContentUnchanged { mtime, hash } => {
            // Content is unchanged (e.g. a touch, or a byte-identical
            // rewrite) - just refresh the mtime baseline so the next check
            // hits the fast path again. No reindex.
            upsert_indexed_file(&conn.lock().unwrap(), file_path, mtime, &hash)?;
            Ok(StalenessOutcome::MtimeMismatchContentUnchanged)
        }
        Decision::NeedsReindex { mtime, hash, had_prior_record } => {
            // Genuinely stale (or never indexed) - synchronously reindex
            // before recording the new baseline. `apply_file_change` locks
            // `conn` itself, only for as long as each of its steps actually
            // needs it (GM-396) - it is never held across this call.
            apply_file_change(
                reader,
                writer,
                conn,
                file_path,
                request_id,
                embedding,
                file_changed_timeout,
                semantic_pass_timeout,
                semantic_pass_capable,
                on_timeout,
            )
            .context(ReindexFailed)?;
            upsert_indexed_file(&conn.lock().unwrap(), file_path, mtime, &hash)?;
            Ok(if had_prior_record {
                StalenessOutcome::ReindexedViaHashMismatch
            } else {
                StalenessOutcome::ReindexedNoPriorRecord
            })
        }
    }
}

/// Whether [`ensure_fresh`] would need to reindex `file_path` right now -
/// the plugin-free half of its decision, for a caller that guards the actual
/// reindex with a lock (e.g. `daemon::plugin::PluginProcess`'s stdin/stdout
/// pair) it does not want to take for the overwhelmingly common case of
/// nothing having changed. Pays only for [`decide`]'s stat-plus-maybe-hash
/// cost, never a reader/writer round trip.
///
/// A `ContentUnchanged` decision is resolved here exactly as `ensure_fresh`
/// resolves it - the mtime baseline is refreshed before returning - so a
/// caller that gets `false` back needs to do nothing further, same guarantee
/// `ensure_fresh` gives for that outcome.
pub fn is_stale(conn: &Connection, project_root: &Path, file_path: &str) -> Result<bool> {
    match decide(conn, project_root, file_path)? {
        Decision::AlreadyFresh => Ok(false),
        Decision::ContentUnchanged { mtime, hash } => {
            upsert_indexed_file(conn, file_path, mtime, &hash)?;
            Ok(false)
        }
        Decision::NeedsReindex { .. } => Ok(true),
    }
}

/// The plugin-free half of [`ensure_fresh`]'s two-tier mtime/hash decision -
/// see the module doc for the full procedure. Shared by `ensure_fresh`
/// (which acts on it, reindexing when it says to) and [`is_stale`] (which
/// only needs to know whether it would).
enum Decision {
    /// Recorded mtime matched current on-disk mtime.
    AlreadyFresh,
    /// mtime differed but content didn't - the caller still owes the
    /// `indexed_files` table a refreshed mtime baseline.
    ContentUnchanged { mtime: i64, hash: String },
    /// Genuinely stale, or never indexed - the caller owes a reindex, then
    /// this mtime/hash as the new baseline.
    NeedsReindex { mtime: i64, hash: String, had_prior_record: bool },
}

fn decide(conn: &Connection, project_root: &Path, file_path: &str) -> Result<Decision> {
    let full_path = project_root.join(file_path);
    let metadata =
        fs::metadata(&full_path).with_context(|| format!("failed to stat {}", full_path.display()))?;
    let current_mtime = mtime_millis(&metadata)
        .with_context(|| format!("failed to read mtime of {}", full_path.display()))?;

    let prior = lookup_indexed_file(conn, file_path)?;

    if let Some(record) = &prior {
        if record.mtime_millis == current_mtime {
            // Fast path: no file read.
            return Ok(Decision::AlreadyFresh);
        }
    }

    // mtime disagreed (or there's no prior record at all) - fall back to
    // reading the file and hashing its content.
    let current_hash =
        hash_file(&full_path).with_context(|| format!("failed to hash {}", full_path.display()))?;

    if let Some(record) = &prior {
        if record.content_hash == current_hash {
            return Ok(Decision::ContentUnchanged { mtime: current_mtime, hash: current_hash });
        }
    }

    Ok(Decision::NeedsReindex { mtime: current_mtime, hash: current_hash, had_prior_record: prior.is_some() })
}

struct IndexedFileRecord {
    mtime_millis: i64,
    content_hash: String,
}

fn lookup_indexed_file(conn: &Connection, file_path: &str) -> Result<Option<IndexedFileRecord>> {
    conn.query_row(
        "SELECT mtimeMillis, contentHash FROM indexed_files WHERE filePath = ?1",
        params![file_path],
        |row| Ok(IndexedFileRecord { mtime_millis: row.get(0)?, content_hash: row.get(1)? }),
    )
    .optional()
    .context("failed to query indexed_files")
}

/// How much older than the walk's start a file's mtime must be before
/// [`record_walk_baselines`] trusts that the walk read its current content.
/// Covers the coarsest mtime granularity in common use (FAT's 2 s; HFS+ and
/// some network filesystems record whole seconds): a write made at or after
/// the walk's start can be stamped up to one granule *earlier* than the
/// instant it happened, so "mtime before the start" alone would not rule it
/// out.
pub(crate) const WALK_BASELINE_MTIME_MARGIN: Duration = Duration::from_secs(2);

/// What [`record_walk_baselines`] did with the files a walk reported.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalkBaselines {
    /// Files that got an `indexed_files` row.
    pub recorded: usize,
    /// Files left without one - modified too close to (or after) the walk's
    /// start, changed while being hashed, or unreadable. Each of these pays
    /// the query-time reindex on its first touch, exactly as every file did
    /// before GM-401.
    pub skipped: usize,
}

/// Records an `indexed_files` baseline for each of `files` (project-relative)
/// that a bulk walk started at `walk_started` indexed - GM-401. Without it the
/// first query of *any* file after a fresh walk finds no row, reads the file
/// as never indexed, and pays a synchronous reindex (a `fileChanged` round
/// trip plus a per-file semantic pass, which on a cold language server ran
/// for over a minute) before it answers.
///
/// # Why a row written here never claims content the index did not see
///
/// A baseline is a promise: "the graph was built from exactly these bytes".
/// The walk's plugin read each file at some unknown instant after
/// `walk_started` (the caller takes it before spawning the first plugin), and
/// this runs after every plugin has exited. The daemon never sees the bytes
/// the plugin read, so the promise is established from mtimes instead:
///
/// 1. `stat` the file. Its mtime must be earlier than `walk_started` by at
///    least [`WALK_BASELINE_MTIME_MARGIN`]. A write at or after the walk's
///    start - so possibly after the plugin's read - stamps an mtime no
///    earlier than `walk_started` minus one granule, so this rules every
///    such write out. The file's bytes have therefore been the same from
///    before the plugin could have read them until this `stat`.
/// 2. Hash the file, then `stat` it again. A different mtime means it was
///    written while being hashed: the hash may not be of the bytes the walk
///    saw, so the file is skipped.
///
/// A skipped file is simply left without a row, which is the state every
/// file was in before this existed - its first query reindexes it. Nothing
/// here can make a stale file look fresh except a write that lies about its
/// own mtime (a backdating `touch -d`, `tar -x`, `rsync -t` landing *during*
/// the walk), the same trust in mtimes `ensure_fresh`'s own fast path
/// already places. In the daemon the watcher is registered before the walk,
/// so even that write is queued and reparsed once the watcher's consumer
/// starts.
///
/// Best-effort per file: a file that cannot be read is skipped, not fatal.
/// Only the insert itself can fail this call. The rows are written in one
/// transaction; `conn` is locked only for that transaction, never while
/// hashing.
pub(crate) fn record_walk_baselines<'a>(
    conn: &Mutex<Connection>,
    project_root: &Path,
    files: impl IntoIterator<Item = &'a str>,
    walk_started: std::time::SystemTime,
) -> Result<WalkBaselines> {
    let started_millis = walk_started
        .checked_sub(WALK_BASELINE_MTIME_MARGIN)
        .and_then(|cutoff| cutoff.duration_since(UNIX_EPOCH).ok())
        .map(|cutoff| cutoff.as_millis() as i64);
    let mut summary = WalkBaselines::default();
    let Some(cutoff_millis) = started_millis else {
        // A clock this close to the epoch cannot order anything.
        summary.skipped = files.into_iter().count();
        return Ok(summary);
    };

    let mut rows: Vec<(&str, i64, String)> = Vec::new();
    for file_path in files {
        match walk_baseline_for(project_root, file_path, cutoff_millis) {
            Some((mtime, hash)) => rows.push((file_path, mtime, hash)),
            None => summary.skipped += 1,
        }
    }

    let mut guard = conn.lock().unwrap();
    let tx = guard.transaction().context("failed to start the walk-baseline transaction")?;
    for (file_path, mtime, hash) in &rows {
        upsert_indexed_file(&tx, file_path, *mtime, hash)?;
    }
    tx.commit().context("failed to commit the walk's indexed_files baselines")?;
    summary.recorded = rows.len();
    Ok(summary)
}

/// One file's half of [`record_walk_baselines`]: its `(mtime, hash)` if the
/// procedure in that function's doc comment proves the walk saw its current
/// bytes, `None` otherwise.
fn walk_baseline_for(project_root: &Path, file_path: &str, cutoff_millis: i64) -> Option<(i64, String)> {
    let full_path = project_root.join(file_path);
    let before = fs::metadata(&full_path).ok().filter(|metadata| metadata.is_file())?;
    let mtime = mtime_millis(&before).ok()?;
    if mtime >= cutoff_millis {
        return None;
    }
    let hash = hash_file(&full_path).ok()?;
    let after = fs::metadata(&full_path).ok()?;
    if mtime_millis(&after).ok()? != mtime || after.len() != before.len() {
        return None;
    }
    Some((mtime, hash))
}

/// Milliseconds since the Unix epoch, per `metadata.modified()`. Kept as a
/// plain integer (rather than e.g. an RFC3339 string) so the fast-path
/// comparison in `ensure_fresh` is a cheap integer equality check.
///
/// Shared with `cli::status`, which compares the same recorded baselines to
/// count how many files the index still owes work for - it has to read an
/// `indexed_files` row exactly the way the code that wrote it does.
pub(crate) fn mtime_millis(metadata: &fs::Metadata) -> Result<i64> {
    let modified = metadata.modified().context("filesystem does not report mtimes")?;
    let duration = modified.duration_since(UNIX_EPOCH).context("mtime is before the Unix epoch")?;
    Ok(duration.as_millis() as i64)
}

/// Hex-encoded SHA-256 of the file's full content. Only called on the
/// (comparatively rare) mtime-mismatch path, never on the fast path.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::jsonrpc::{read_message, write_message};
    use crate::protocol::types::{
        ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, NodeKind, Position, Range,
        Visibility, WireNode, JSONRPC_VERSION,
    };
    use crate::storage::schema;
    use std::io::BufReader;
    use std::sync::mpsc;

    /// See `watcher::apply`'s own test module for why this exists and what it
    /// is deliberately not testing - every stub plugin below answers
    /// immediately, so nothing here should ever wait this long.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn on_timeout_must_not_fire() {
        panic!(
            "on_timeout fired in a test whose stub plugin always answers - the stub or the timeout is broken"
        );
    }

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Mutex<Connection>, table: &str) -> i64 {
        conn.lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    fn wire_node(id: &str, name: &str) -> WireNode {
        WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: name.to_string(),
            qualified_name: format!("mod::{name}"),
            file_path: "src/lib.rs".to_string(),
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 3, col: 1 } },
            signature: None,
            visibility: Visibility::Public,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
        }
    }

    /// Spawns a stub plugin thread that reads one `FileChanged` request and
    /// replies with `response`, exactly like `watcher::apply`'s own tests.
    /// Sends `()` down `invoked_tx` right before replying so a test can
    /// assert on the *number* of times the plugin transport was actually
    /// used (the mechanism for proving the fast/skip paths never invoke it).
    ///
    /// It then answers the semantic pass `apply_file_change` sends once the
    /// reparse has settled - a query-time catch-up is a reparse settling
    /// like any other, so this transport carries both round trips. The
    /// empty diff keeps these tests about staleness: what a real pass would
    /// answer is `watcher::apply`'s subject, not this module's.
    fn spawn_stub_plugin(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_path: &'static str,
        expected_id: RequestId,
        response: FileChangeResponse,
        invoked_tx: mpsc::Sender<()>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(&mut reader);
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            assert_eq!(request.id, Some(expected_id));
            match request.message {
                ControlMessage::FileChanged { file_path } => assert_eq!(file_path, expected_file_path),
                other => panic!("expected FileChanged, got {other:?}"),
            }
            invoked_tx.send(()).unwrap();
            write_message(&mut writer, &response).unwrap();

            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            let id = request.id.clone().expect("the semantic pass carries an id");
            match request.message {
                ControlMessage::SemanticPass { file_paths } => {
                    assert_eq!(file_paths, vec![expected_file_path.to_string()])
                }
                other => panic!("expected SemanticPass after the reparse, got {other:?}"),
            }
            write_message(
                &mut writer,
                &FileChangeResponse {
                    jsonrpc: JSONRPC_VERSION.to_string(),
                    id,
                    result: FileChangeDiff::default(),
                    incomplete: false,
                    incomplete_reason: None,
                },
            )
            .unwrap();
        })
    }

    fn diff_response(request_id: RequestId, node_name: &str) -> FileChangeResponse {
        FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id,
            result: FileChangeDiff {
                upsert_nodes: vec![wire_node("n1", node_name)],
                delete_node_ids: vec![],
                upsert_edges: vec![],
                delete_edge_ids: vec![],
            },
            incomplete: false,
            incomplete_reason: None,
        }
    }

    #[test]
    fn no_prior_record_is_treated_as_stale_and_reindexed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file_path_on_disk = tmp.path().join("src/lib.rs");
        fs::write(&file_path_on_disk, b"fn foo() {}").unwrap();
        let conn = Mutex::new(setup_conn());

        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let (invoked_tx, invoked_rx) = mpsc::channel();

        let request_id = RequestId::Number(1);
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            diff_response(request_id.clone(), "foo"),
            invoked_tx,
        );

        let mut buf_reader = BufReader::new(core_reader);
        let outcome = ensure_fresh(
            &mut buf_reader,
            &mut core_writer,
            &conn,
            tmp.path(),
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(outcome, StalenessOutcome::ReindexedNoPriorRecord);
        assert!(outcome.reindexed());
        assert!(invoked_rx.try_recv().is_ok(), "plugin transport must have been invoked");
        assert_eq!(count(&conn, "nodes"), 1);
        assert_eq!(count(&conn, "indexed_files"), 1);
    }

    #[test]
    fn modifying_file_on_disk_without_the_watcher_triggers_a_synchronous_reindex() {
        // The acceptance-criterion test: seed an initial index (as if a
        // prior `ensure_fresh`/`apply_file_change` call had run), then
        // modify the file directly via `std::fs::write` - simulating a
        // missed watcher event, no debounce/watcher involved at all - then
        // call `ensure_fresh` again and confirm it (a) reports a reindex
        // happened and (b) the DB reflects the *new* content.
        let tmp = tempfile::tempdir().unwrap();
        let file_on_disk = tmp.path().join("lib.rs");
        fs::write(&file_on_disk, b"fn old() {}").unwrap();
        let conn = Mutex::new(setup_conn());

        // --- initial index ---
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let (invoked_tx, invoked_rx) = mpsc::channel();
        let request_id = RequestId::Number(1);
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "lib.rs",
            request_id.clone(),
            diff_response(request_id.clone(), "old"),
            invoked_tx,
        );
        let mut buf_reader = BufReader::new(core_reader);
        let outcome = ensure_fresh(
            &mut buf_reader,
            &mut core_writer,
            &conn,
            tmp.path(),
            "lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();
        assert_eq!(outcome, StalenessOutcome::ReindexedNoPriorRecord);
        invoked_rx.try_recv().unwrap();

        let name: String = conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "old");

        // --- modify the file directly on disk, bypassing the watcher entirely ---
        // Ensure the mtime actually advances (some filesystems have
        // coarse-grained mtime resolution).
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&file_on_disk, b"fn new_and_improved() {}").unwrap();

        // --- query-time staleness check must catch the missed change ---
        let (plugin_reader2, mut core_writer2) = std::io::pipe().unwrap();
        let (core_reader2, plugin_writer2) = std::io::pipe().unwrap();
        let (invoked_tx2, invoked_rx2) = mpsc::channel();
        let request_id2 = RequestId::Number(2);
        let plugin2 = spawn_stub_plugin(
            plugin_reader2,
            plugin_writer2,
            "lib.rs",
            request_id2.clone(),
            diff_response(request_id2.clone(), "new_and_improved"),
            invoked_tx2,
        );
        let mut buf_reader2 = BufReader::new(core_reader2);
        let outcome2 = ensure_fresh(
            &mut buf_reader2,
            &mut core_writer2,
            &conn,
            tmp.path(),
            "lib.rs",
            request_id2,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin2.join().unwrap();

        assert_eq!(outcome2, StalenessOutcome::ReindexedViaHashMismatch);
        assert!(outcome2.reindexed(), "a missed watcher event must trigger a synchronous reindex");
        assert!(invoked_rx2.try_recv().is_ok(), "plugin transport must have been invoked for the stale file");

        let name_after: String = conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name_after, "new_and_improved", "response must reflect the new on-disk content");
    }

    #[test]
    fn unchanged_file_hits_the_fast_path_and_never_invokes_the_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let file_on_disk = tmp.path().join("lib.rs");
        fs::write(&file_on_disk, b"fn foo() {}").unwrap();
        let conn = Mutex::new(setup_conn());

        // First call: no prior record, must reindex.
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let (invoked_tx, invoked_rx) = mpsc::channel();
        let request_id = RequestId::Number(1);
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "lib.rs",
            request_id.clone(),
            diff_response(request_id.clone(), "foo"),
            invoked_tx,
        );
        let mut buf_reader = BufReader::new(core_reader);
        ensure_fresh(
            &mut buf_reader,
            &mut core_writer,
            &conn,
            tmp.path(),
            "lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();
        invoked_rx.try_recv().unwrap();

        // Second call, nothing changed on disk: must be AlreadyFresh and
        // must not touch the plugin transport at all. Wire up a second stub
        // "plugin" thread that just tries to read one message and records
        // whether it ever received one; `ensure_fresh`'s fast path must
        // return without writing anything, so once it returns we drop the
        // writer end (causing the stub's `read_message` to observe a clean
        // EOF - `Ok(None)` per `read_message`'s own contract - rather than
        // blocking forever) and then assert nothing was ever received. This
        // is the structural, non-hanging proof that the transport was never
        // invoked a second time.
        let (plugin_reader2, mut core_writer2) = std::io::pipe().unwrap();
        let (core_reader2, _plugin_writer2) = std::io::pipe().unwrap();
        let invoked2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invoked2_clone = invoked2.clone();
        let plugin2 = std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(plugin_reader2);
            let msg: Option<ControlEnvelope> = read_message(&mut buf_reader).unwrap();
            invoked2_clone.store(msg.is_some(), std::sync::atomic::Ordering::SeqCst);
        });

        let mut buf_reader2 = BufReader::new(core_reader2);
        let outcome = ensure_fresh(
            &mut buf_reader2,
            &mut core_writer2,
            &conn,
            tmp.path(),
            "lib.rs",
            RequestId::Number(2),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();

        drop(core_writer2); // unblocks the stub thread's read via clean EOF
        plugin2.join().unwrap();

        assert_eq!(outcome, StalenessOutcome::AlreadyFresh);
        assert!(!outcome.reindexed());
        assert!(
            !invoked2.load(std::sync::atomic::Ordering::SeqCst),
            "the fast path must not invoke the plugin transport at all"
        );
    }

    #[test]
    fn mtime_change_with_identical_content_updates_baseline_without_reindexing() {
        // Same byte content rewritten after a short sleep - mtime advances,
        // content hash does not. Portable alternative to forcing an exact
        // mtime match/mismatch via platform-specific APIs.
        let tmp = tempfile::tempdir().unwrap();
        let file_on_disk = tmp.path().join("lib.rs");
        let content = b"fn stable() {}";
        fs::write(&file_on_disk, content).unwrap();
        let conn = Mutex::new(setup_conn());

        // Initial index.
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let (invoked_tx, invoked_rx) = mpsc::channel();
        let request_id = RequestId::Number(1);
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "lib.rs",
            request_id.clone(),
            diff_response(request_id.clone(), "stable"),
            invoked_tx,
        );
        let mut buf_reader = BufReader::new(core_reader);
        ensure_fresh(
            &mut buf_reader,
            &mut core_writer,
            &conn,
            tmp.path(),
            "lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();
        invoked_rx.try_recv().unwrap();

        let (mtime_before,): (i64,) = conn
            .lock()
            .unwrap()
            .query_row("SELECT mtimeMillis FROM indexed_files WHERE filePath = 'lib.rs'", [], |row| {
                Ok((row.get(0)?,))
            })
            .unwrap();

        // Rewrite byte-identical content after a short sleep so mtime
        // advances but the hash doesn't change.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&file_on_disk, content).unwrap();

        // Same non-invocation proof as the fast-path test: a stub thread
        // that records whether it ever received a message, unblocked via a
        // clean EOF (not a hang) once `ensure_fresh` has returned.
        let (plugin_reader2, mut core_writer2) = std::io::pipe().unwrap();
        let (core_reader2, _plugin_writer2) = std::io::pipe().unwrap();
        let invoked2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invoked2_clone = invoked2.clone();
        let plugin2 = std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(plugin_reader2);
            let msg: Option<ControlEnvelope> = read_message(&mut buf_reader).unwrap();
            invoked2_clone.store(msg.is_some(), std::sync::atomic::Ordering::SeqCst);
        });

        let mut buf_reader2 = BufReader::new(core_reader2);
        let outcome = ensure_fresh(
            &mut buf_reader2,
            &mut core_writer2,
            &conn,
            tmp.path(),
            "lib.rs",
            RequestId::Number(2),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();

        drop(core_writer2);
        plugin2.join().unwrap();
        assert!(
            !invoked2.load(std::sync::atomic::Ordering::SeqCst),
            "the hash-matches-despite-mtime-mismatch path must not invoke the plugin transport"
        );

        assert_eq!(outcome, StalenessOutcome::MtimeMismatchContentUnchanged);
        assert!(!outcome.reindexed());

        let (mtime_after,): (i64,) = conn
            .lock()
            .unwrap()
            .query_row("SELECT mtimeMillis FROM indexed_files WHERE filePath = 'lib.rs'", [], |row| {
                Ok((row.get(0)?,))
            })
            .unwrap();
        assert!(mtime_after > mtime_before, "mtime baseline must be refreshed even without a reindex");
    }

    /// Sets `path`'s mtime to `when`, so a test can place a file firmly
    /// before (or after) a walk's start without sleeping.
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    fn baseline_row(conn: &Mutex<Connection>, file_path: &str) -> Option<(i64, String)> {
        conn.lock()
            .unwrap()
            .query_row(
                "SELECT mtimeMillis, contentHash FROM indexed_files WHERE filePath = ?1",
                params![file_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .unwrap()
    }

    /// GM-401: a file last written well before the walk started gets a
    /// baseline equal to what `decide` would compute for it - so the very
    /// next `ensure_fresh` takes the fast path - while a file written at or
    /// after the walk's start (the walk may have read it before that write)
    /// gets none, and neither does a path that is gone or not a file.
    ///
    /// Control: drop the `mtime >= cutoff_millis` check in
    /// `walk_baseline_for` - `fresh.rs` gets a row and the `None` assertion
    /// fails.
    #[test]
    fn walk_baselines_vouch_only_for_files_untouched_since_the_walk_started() {
        let tmp = tempfile::tempdir().unwrap();
        let walk_started = std::time::SystemTime::now();

        fs::write(tmp.path().join("old.rs"), b"fn old() {}").unwrap();
        set_mtime(&tmp.path().join("old.rs"), walk_started - Duration::from_secs(3600));
        // Written "during the walk": after its start.
        fs::write(tmp.path().join("fresh.rs"), b"fn fresh() {}").unwrap();
        set_mtime(&tmp.path().join("fresh.rs"), walk_started + Duration::from_millis(1));
        // Written just before the start, within one coarse mtime granule of it.
        fs::write(tmp.path().join("edge.rs"), b"fn edge() {}").unwrap();
        set_mtime(&tmp.path().join("edge.rs"), walk_started - Duration::from_millis(500));
        fs::create_dir(tmp.path().join("dir.rs")).unwrap();

        let conn = Mutex::new(setup_conn());
        let summary = record_walk_baselines(
            &conn,
            tmp.path(),
            ["old.rs", "fresh.rs", "edge.rs", "gone.rs", "dir.rs"],
            walk_started,
        )
        .unwrap();

        assert_eq!(summary, WalkBaselines { recorded: 1, skipped: 4 });
        let (mtime, hash) = baseline_row(&conn, "old.rs").expect("old.rs must be baselined");
        assert_eq!(mtime, mtime_millis(&fs::metadata(tmp.path().join("old.rs")).unwrap()).unwrap());
        assert_eq!(hash, hash_file(&tmp.path().join("old.rs")).unwrap());
        assert_eq!(baseline_row(&conn, "fresh.rs"), None, "a file written after the walk started");
        assert_eq!(baseline_row(&conn, "edge.rs"), None, "a file within the mtime margin of the start");
        assert_eq!(baseline_row(&conn, "gone.rs"), None);
        assert_eq!(baseline_row(&conn, "dir.rs"), None);

        // What the baseline buys: no reindex on the first check.
        assert!(!is_stale(&conn.lock().unwrap(), tmp.path(), "old.rs").unwrap());
        // And what it must not cost: an edit after the walk is still stale.
        fs::write(tmp.path().join("old.rs"), b"fn old_and_edited() {}").unwrap();
        assert!(is_stale(&conn.lock().unwrap(), tmp.path(), "old.rs").unwrap());
    }
}
