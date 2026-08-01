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

use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::protocol::types::RequestId;
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
pub fn ensure_fresh<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    project_root: &Path,
    file_path: &str,
    request_id: RequestId,
) -> Result<StalenessOutcome> {
    match decide(conn, project_root, file_path)? {
        Decision::AlreadyFresh => Ok(StalenessOutcome::AlreadyFresh),
        Decision::ContentUnchanged { mtime, hash } => {
            // Content is unchanged (e.g. a touch, or a byte-identical
            // rewrite) - just refresh the mtime baseline so the next check
            // hits the fast path again. No reindex.
            upsert_indexed_file(conn, file_path, mtime, &hash)?;
            Ok(StalenessOutcome::MtimeMismatchContentUnchanged)
        }
        Decision::NeedsReindex { mtime, hash, had_prior_record } => {
            // Genuinely stale (or never indexed) - synchronously reindex
            // before recording the new baseline.
            apply_file_change(reader, writer, conn, file_path, request_id)
                .context("failed to synchronously reindex stale file")?;
            upsert_indexed_file(conn, file_path, mtime, &hash)?;
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
    let metadata = fs::metadata(&full_path)
        .with_context(|| format!("failed to stat {}", full_path.display()))?;
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
    let current_hash = hash_file(&full_path)
        .with_context(|| format!("failed to hash {}", full_path.display()))?;

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
        |row| {
            Ok(IndexedFileRecord {
                mtime_millis: row.get(0)?,
                content_hash: row.get(1)?,
            })
        },
    )
    .optional()
    .context("failed to query indexed_files")
}

fn upsert_indexed_file(
    conn: &Connection,
    file_path: &str,
    mtime_millis: i64,
    content_hash: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO indexed_files (filePath, mtimeMillis, contentHash)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(filePath) DO UPDATE SET
            mtimeMillis = excluded.mtimeMillis,
            contentHash = excluded.contentHash",
        params![file_path, mtime_millis, content_hash],
    )
    .context("failed to upsert indexed_files row")?;
    Ok(())
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
    use crate::protocol::types::{
        ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, NodeKind, Position, Range,
        WireNode, JSONRPC_VERSION,
    };
    use crate::protocol::jsonrpc::{read_message, write_message};
    use crate::storage::schema;
    use std::io::BufReader;
    use std::sync::mpsc;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
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
            exported: true,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
        }
    }

    /// Spawns a stub plugin thread that reads one `FileChanged` request and
    /// replies with `response`, exactly like `watcher::apply`'s own tests.
    /// Sends `()` down `invoked_tx` right before replying so a test can
    /// assert on the *number* of times the plugin transport was actually
    /// used (the mechanism for proving the fast/skip paths never invoke it).
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
        }
    }

    #[test]
    fn no_prior_record_is_treated_as_stale_and_reindexed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file_path_on_disk = tmp.path().join("src/lib.rs");
        fs::write(&file_path_on_disk, b"fn foo() {}").unwrap();
        let mut conn = setup_conn();

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
            &mut conn,
            tmp.path(),
            "src/lib.rs",
            request_id,
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
        let mut conn = setup_conn();

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
        let outcome =
            ensure_fresh(&mut buf_reader, &mut core_writer, &mut conn, tmp.path(), "lib.rs", request_id)
                .unwrap();
        plugin.join().unwrap();
        assert_eq!(outcome, StalenessOutcome::ReindexedNoPriorRecord);
        invoked_rx.try_recv().unwrap();

        let name: String =
            conn.query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0)).unwrap();
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
            &mut conn,
            tmp.path(),
            "lib.rs",
            request_id2,
        )
        .unwrap();
        plugin2.join().unwrap();

        assert_eq!(outcome2, StalenessOutcome::ReindexedViaHashMismatch);
        assert!(outcome2.reindexed(), "a missed watcher event must trigger a synchronous reindex");
        assert!(invoked_rx2.try_recv().is_ok(), "plugin transport must have been invoked for the stale file");

        let name_after: String =
            conn.query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0)).unwrap();
        assert_eq!(name_after, "new_and_improved", "response must reflect the new on-disk content");
    }

    #[test]
    fn unchanged_file_hits_the_fast_path_and_never_invokes_the_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let file_on_disk = tmp.path().join("lib.rs");
        fs::write(&file_on_disk, b"fn foo() {}").unwrap();
        let mut conn = setup_conn();

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
        ensure_fresh(&mut buf_reader, &mut core_writer, &mut conn, tmp.path(), "lib.rs", request_id)
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
            &mut conn,
            tmp.path(),
            "lib.rs",
            RequestId::Number(2),
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
        let mut conn = setup_conn();

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
        ensure_fresh(&mut buf_reader, &mut core_writer, &mut conn, tmp.path(), "lib.rs", request_id)
            .unwrap();
        plugin.join().unwrap();
        invoked_rx.try_recv().unwrap();

        let (mtime_before,): (i64,) = conn
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
            &mut conn,
            tmp.path(),
            "lib.rs",
            RequestId::Number(2),
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
            .query_row("SELECT mtimeMillis FROM indexed_files WHERE filePath = 'lib.rs'", [], |row| {
                Ok((row.get(0)?,))
            })
            .unwrap();
        assert!(mtime_after > mtime_before, "mtime baseline must be refreshed even without a reindex");
    }
}
