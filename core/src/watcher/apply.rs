//! Bridges a (debounced) file-change event to a committed diff: sends a
//! `FileChanged` control-plane request to a language plugin, reads back its
//! diff, and applies it through `storage::write::apply_diff`.
//!
//! The semantic pass ([`apply_semantic_pass`]) is the same motion with a
//! different question: instead of "what does this file look like now", it
//! asks "what can your type checker now resolve that tree-sitter could
//! only guess at". Its answer comes back in the same diff shape and goes
//! through the same commit-and-link pipeline, because an upgraded edge is
//! just that edge re-sent under its own id with a better `source`.
//!
//! This module is transport-agnostic on purpose - it only knows about
//! `Read`/`Write` streams (the same abstraction `jsonrpc.rs` and
//! `handshake.rs` already use), not about how the peer on the other end of
//! those streams came to exist. A real spawned-plugin-process transport can
//! be plugged in later without touching this function; for now, tests fake
//! the peer with `std::io::pipe()` plus a thread, exactly like
//! `jsonrpc.rs`'s own pipe-based tests do.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use crate::graph::{imports, symbol_links};
use crate::protocol::jsonrpc::{read_message, write_message};
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, RequestId, WireEdge, WireNode,
    JSONRPC_VERSION,
};
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};

/// Sends a `FileChanged` request (tagged with `request_id`) for `file_path`
/// over `writer`, reads the plugin's `FileChangeResponse` off `reader`,
/// validates the response id matches the request id, converts the wire
/// diff into a `storage::write::Diff`, and commits it via `apply_diff` -
/// then asks the same plugin for a semantic pass over that same file.
///
/// The semantic pass rides here, rather than at each of this function's
/// call sites, because "the reparse has settled" is precisely the condition
/// it needs and this is the only place that knows it. Every caller that
/// reparses a file gets it for free: the watcher (via
/// `daemon::lifecycle::PluginSupervisor::file_changed`), the wake-up replay
/// of everything queued while the plugin slept, and the query-time
/// staleness catch-up in `watcher::staleness::ensure_fresh` - which is a
/// reparse settling as much as any other, and whose edges deserve the
/// upgrade just as much.
///
/// A failing semantic pass is reported and dropped, not propagated. It is
/// an *upgrade* over a graph that is already committed, correct and
/// serviceable: failing the whole reparse because the semantic layer was
/// unavailable would turn a better answer that did not arrive into a worse
/// one that did. (A plugin that died is still noticed - the next request's
/// write fails, which is what `daemon::plugin::PluginProcess` relaunches
/// and replays on.)
///
/// `request_id` is supplied by the caller rather than generated internally
/// so this function stays pure and easy to test; the id for the second
/// round trip is derived from it by [`semantic_pass_id`].
pub fn apply_file_change<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    file_path: impl Into<String>,
    request_id: RequestId,
) -> Result<()> {
    let file_path = file_path.into();
    round_trip(
        reader,
        writer,
        conn,
        ControlMessage::FileChanged { file_path: file_path.clone() },
        request_id.clone(),
    )?;

    if let Err(err) = apply_semantic_pass(
        reader,
        writer,
        conn,
        vec![file_path.clone()],
        semantic_pass_id(&request_id),
    ) {
        eprintln!(
            "g-mesh: the semantic pass over {file_path} failed after its reparse ({err:#}) - \
             its edges keep whatever the structural pass resolved"
        );
    }
    Ok(())
}

/// Asks the plugin's semantic layer what it can now resolve, and commits
/// the answer exactly like a file-change diff.
///
/// `file_paths` scopes the pass: one entry after an incremental reparse,
/// **empty** for the whole project once the cold-start bulk walk is done -
/// see `ControlMessage::SemanticPass` on why "empty" means everything
/// rather than nothing.
///
/// There is deliberately no new storage path here. The pass answers with a
/// `FileChangeDiff` whose edges carry the ids the structural pass already
/// gave them, so `apply_diff`'s `ON CONFLICT(id) DO UPDATE` upgrades each
/// one in place - `source` `tree-sitter` -> `ts-compiler`, `resolved`
/// `false` -> `true` - and touches nothing it was not sent.
pub fn apply_semantic_pass<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    file_paths: Vec<String>,
    request_id: RequestId,
) -> Result<()> {
    round_trip(reader, writer, conn, ControlMessage::SemanticPass { file_paths }, request_id)
}

/// The id for the semantic pass that follows a file change, derived from
/// that file change's own id.
///
/// Both round trips happen back to back on one stream that the caller
/// (`daemon::plugin::PluginProcess`) holds locked for their whole duration,
/// so the second needs an id the plugin cannot confuse with the first, not
/// a globally unique one. Deriving beats threading a second id through
/// every caller, and cannot collide with the counter-issued ids either:
/// those are always `Number`, these are always `String`, and `RequestId`
/// compares across variants.
///
/// Which variant it was derived from is part of the derived id, so the two
/// id spaces cannot alias: without it a `Number(3)` and a `String("3")`
/// base - both legal per JSON-RPC 2.0, and both reachable, since
/// `PluginProcess` issues numbers while callers may pass strings - would
/// derive the very same id.
fn semantic_pass_id(base: &RequestId) -> RequestId {
    RequestId::String(match base {
        RequestId::Number(n) => format!("semanticPass:num:{n}"),
        RequestId::String(s) => format!("semanticPass:str:{s}"),
    })
}

/// One control-plane round trip that answers with a diff: write the
/// request, read the response, check that it answers *this* request, then
/// commit and link the diff it carried.
///
/// Shared by [`apply_file_change`] and [`apply_semantic_pass`], which
/// differ only in the message they send. Everything after the response is
/// identical, which is the whole reason a semantic upgrade needed no
/// storage or schema work of its own.
fn round_trip<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    message: ControlMessage,
    request_id: RequestId,
) -> Result<()> {
    let method = method_name(&message);
    let request =
        ControlEnvelope { jsonrpc: JSONRPC_VERSION.to_string(), id: Some(request_id.clone()), message };
    write_message(writer, &request)
        .with_context(|| format!("failed to write {method} request to plugin"))?;

    let response: FileChangeResponse = read_message(reader)
        .with_context(|| format!("failed to read plugin's {method} response"))?
        .with_context(|| format!("plugin closed its output before responding to {method}"))?;

    if response.id != request_id {
        bail!(
            "{method} response id {:?} does not match request id {:?} - refusing to apply a diff that answers a different request",
            response.id,
            request_id,
        );
    }

    let diff = to_storage_diff(response.result);
    apply_diff(conn, &diff).with_context(|| format!("failed to apply the {method} diff"))?;
    // After the commit, never before: linking points edges at `File` nodes,
    // and the ones this diff brought with it have to be in the index first.
    imports::link_diff(conn, &diff).context("failed to link the file's resolved imports")?;
    // Symbols second, and for the same reason: a usage edge can only be
    // repointed at an export that is already committed - including the ones
    // this very diff added, which other files may have been waiting for.
    symbol_links::link_diff(conn, &diff).context("failed to link the file's cross-file symbol usages")?;
    Ok(())
}

/// The wire `method` string for a control message, for error messages that
/// name the request that failed. Matched rather than round-tripped through
/// serde so that adding a variant to `ControlMessage` fails to compile here
/// instead of silently reporting the wrong method name.
fn method_name(message: &ControlMessage) -> &'static str {
    match message {
        ControlMessage::Reindex { .. } => "reindex",
        ControlMessage::FileChanged { .. } => "fileChanged",
        ControlMessage::Status => "status",
        ControlMessage::SemanticPass { .. } => "semanticPass",
    }
}

/// Converts the wire-level diff (nested `range: {start, end}`) into the
/// storage layer's flat `start_line`/`start_col`/`end_line`/`end_col`
/// fields. `delete_node_ids`/`delete_edge_ids` pass through unchanged since
/// both sides already agree on `Vec<String>`.
fn to_storage_diff(wire: FileChangeDiff) -> Diff {
    Diff {
        upsert_nodes: wire.upsert_nodes.into_iter().map(to_node_record).collect(),
        delete_node_ids: wire.delete_node_ids,
        upsert_edges: wire.upsert_edges.into_iter().map(to_edge_record).collect(),
        delete_edge_ids: wire.delete_edge_ids,
    }
}

/// Wire node -> storage record. Shared with the cold-start bulk index
/// (`daemon::bulk_index`), which ingests the very same `WireNode` shape off
/// an NDJSON stream instead of out of a diff response - the two paths must
/// never disagree about how a wire node becomes a row.
pub(crate) fn to_node_record(node: WireNode) -> NodeRecord {
    NodeRecord {
        id: node.id,
        kind: format!("{:?}", node.kind),
        name: node.name,
        qualified_name: node.qualified_name,
        file_path: node.file_path,
        start_line: node.range.start.line as i64,
        start_col: node.range.start.col as i64,
        end_line: node.range.end.line as i64,
        end_col: node.range.end.col as i64,
        signature: node.signature,
        exported: node.exported,
        doc_comment: node.doc_comment,
        language: node.language,
        native_kind: node.native_kind,
        has_syntax_errors: node.has_syntax_errors,
    }
}

/// Wire edge -> storage record; see [`to_node_record`] on why this is shared.
pub(crate) fn to_edge_record(edge: WireEdge) -> EdgeRecord {
    EdgeRecord::new(
        edge.id,
        edge.from_id,
        edge.to_id,
        edge_kind_wire_value(&edge.kind),
        edge_source_wire_value(&edge.source),
        edge.resolved,
    )
}

/// `nodes.kind` in the schema is a plain string matching `NodeKind`'s Rust
/// variant name (see `WireNode`'s doc comment in `protocol::types`); no
/// custom serde attributes are attached to `NodeKind`, so `{:?}` already
/// gives the exact variant name (e.g. "Function").
///
/// `edges.kind`/`edges.source`, by contrast, have custom serde renames
/// (`SCREAMING_SNAKE_CASE` / `kebab-case`) - reuse those exact wire strings
/// by round-tripping through serde_json rather than re-deriving the mapping
/// by hand, so the storage string always matches what the wire format (and
/// therefore what the plugin actually sent) says, and any future rename
/// attribute change on these enums doesn't silently desync this file.
fn edge_kind_wire_value(kind: &crate::protocol::types::EdgeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn edge_source_wire_value(source: &crate::protocol::types::EdgeSource) -> String {
    serde_json::to_value(source)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{source:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{EdgeKind, EdgeSource, NodeKind, Position, Range, WireEdge, WireNode};
    use crate::storage::schema;
    use std::io::BufReader;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    fn canned_node(id: &str) -> WireNode {
        WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: "foo".to_string(),
            qualified_name: "mod::foo".to_string(),
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

    /// Spawns a thread acting as a stub plugin: reads one `ControlEnvelope`
    /// request off `reader`, asserts it's the expected `FileChanged` with
    /// the expected id, then writes `response` back over `writer`. Mirrors
    /// how `jsonrpc.rs`/`handshake.rs` fake a peer over a pipe in their own
    /// tests.
    ///
    /// `semantic_diff` says whether this stub should also expect the
    /// semantic pass that `apply_file_change` sends once the reparse has
    /// settled, and what to answer it with. It has to be explicit rather
    /// than "answer one if it comes": a stub that speculatively read a
    /// second request would block forever against the tests where no second
    /// request is sent (a rejected response never gets that far), and the
    /// test would deadlock on `join` instead of failing.
    fn spawn_stub_plugin(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_path: &'static str,
        expected_id: RequestId,
        response: FileChangeResponse,
        semantic_diff: Option<FileChangeDiff>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(&mut reader);
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            assert_eq!(request.id, Some(expected_id));
            match request.message {
                ControlMessage::FileChanged { file_path } => assert_eq!(file_path, expected_file_path),
                other => panic!("expected FileChanged, got {other:?}"),
            }
            write_message(&mut writer, &response).unwrap();

            let Some(result) = semantic_diff else { return };
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            let id = request.id.clone().expect("the semantic pass must be a request, not a notification");
            match request.message {
                ControlMessage::SemanticPass { file_paths } => {
                    assert_eq!(
                        file_paths,
                        vec![expected_file_path.to_string()],
                        "a reparse-triggered pass names exactly the file that settled"
                    );
                }
                other => panic!("expected SemanticPass, got {other:?}"),
            }
            write_message(
                &mut writer,
                &FileChangeResponse { jsonrpc: JSONRPC_VERSION.to_string(), id, result },
            )
            .unwrap();
        })
    }

    /// An edge as the structural pass leaves it: a guess, unconfirmed.
    fn unresolved_edge(id: &str, from: &str, to: &str) -> WireEdge {
        WireEdge {
            id: id.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: EdgeKind::Calls,
            source: EdgeSource::TreeSitter,
            resolved: false,
        }
    }

    fn edge_source_and_resolved(conn: &Connection, id: &str) -> (String, bool) {
        conn.query_row("SELECT source, resolved FROM edges WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
    }

    #[test]
    fn file_change_diff_is_committed_to_sqlite() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(1);
        let canned_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            result: FileChangeDiff {
                upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
                delete_node_ids: vec![],
                upsert_edges: vec![WireEdge {
                    id: "e1".to_string(),
                    from_id: "n1".to_string(),
                    to_id: "n2".to_string(),
                    kind: EdgeKind::Calls,
                    source: EdgeSource::TreeSitter,
                    resolved: false,
                }],
                delete_edge_ids: vec![],
            },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            canned_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(&mut buf_reader, &mut core_writer, &mut conn, "src/lib.rs", request_id)
            .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 2);
        assert_eq!(count(&conn, "edges"), 1);

        let name: String = conn
            .query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "foo");
        let start_line: i64 = conn
            .query_row("SELECT startLine FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(start_line, 1);
        let edge_kind: String = conn
            .query_row("SELECT kind FROM edges WHERE id = 'e1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(edge_kind, "CALLS");
    }

    #[test]
    fn diff_with_deletes_removes_rows() {
        let mut conn = setup_conn();
        // Seed rows the stub plugin's diff will delete.
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")],
                ..Default::default()
            },
        )
        .unwrap();

        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();

        let request_id = RequestId::String("req-2".to_string());
        let canned_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            result: FileChangeDiff { delete_node_ids: vec!["n1".to_string()], ..Default::default() },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            canned_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(&mut buf_reader, &mut core_writer, &mut conn, "src/lib.rs", request_id)
            .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
    }

    #[test]
    fn mismatched_response_id_is_rejected() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(10);
        let wrong_id_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: RequestId::Number(999), // deliberately does not match the request
            result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            wrong_id_response,
            // A rejected file-change response never gets as far as the
            // semantic pass, so no second request is coming.
            None,
        );

        let mut buf_reader = BufReader::new(core_reader);
        let result =
            apply_file_change(&mut buf_reader, &mut core_writer, &mut conn, "src/lib.rs", request_id);
        plugin.join().unwrap();

        assert!(result.is_err(), "a response for a different request id must not be applied");
        assert_eq!(count(&conn, "nodes"), 0, "diff from a mismatched-id response must not be committed");
    }

    #[test]
    fn empty_diff_response_is_a_safe_no_op() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(3);
        let empty_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            result: FileChangeDiff::default(),
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/unchanged.rs",
            request_id.clone(),
            empty_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/unchanged.rs",
            request_id,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
        assert_eq!(count(&conn, "edges"), 0);
    }

    /// A stub that expects a `SemanticPass` as its *first* request - for
    /// exercising [`apply_semantic_pass`] on its own, the way the
    /// post-bulk-index caller reaches it.
    fn spawn_semantic_stub(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_paths: Vec<String>,
        result: FileChangeDiff,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(&mut reader);
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            let id = request.id.clone().expect("a semantic pass expects an answer, so it carries an id");
            match request.message {
                ControlMessage::SemanticPass { file_paths } => {
                    assert_eq!(file_paths, expected_file_paths)
                }
                other => panic!("expected SemanticPass, got {other:?}"),
            }
            write_message(
                &mut writer,
                &FileChangeResponse { jsonrpc: JSONRPC_VERSION.to_string(), id, result },
            )
            .unwrap();
        })
    }

    /// The acceptance criterion, at the unit level: an edge the structural
    /// pass left as a `tree-sitter` guess comes back confirmed, and nothing
    /// else in the index moves.
    #[test]
    fn a_semantic_pass_upgrades_an_edge_in_place_and_leaves_the_others_alone() {
        let mut conn = setup_conn();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "typescript"),
                    NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "typescript"),
                ],
                upsert_edges: vec![
                    EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false),
                    EdgeRecord::new("e2", "n2", "n1", "CALLS", "tree-sitter", false),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();

        // Only e1 is answered for - e2 is not in the diff at all.
        let mut upgraded = unresolved_edge("e1", "n1", "n2");
        upgraded.source = EdgeSource::TsCompiler;
        upgraded.resolved = true;
        let plugin = spawn_semantic_stub(
            plugin_reader,
            plugin_writer,
            vec!["src/lib.rs".to_string()],
            FileChangeDiff { upsert_edges: vec![upgraded], ..Default::default() },
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            vec!["src/lib.rs".to_string()],
            RequestId::Number(9),
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(
            edge_source_and_resolved(&conn, "e1"),
            ("ts-compiler".to_string(), true),
            "the answered edge must be upgraded in place, not duplicated"
        );
        assert_eq!(
            edge_source_and_resolved(&conn, "e2"),
            ("tree-sitter".to_string(), false),
            "an edge the pass said nothing about must not change"
        );
        assert_eq!(count(&conn, "edges"), 2, "an upgrade is an update, never an insert");
        assert_eq!(count(&conn, "nodes"), 2);
    }

    /// The trigger half of the same criterion: a reparse that settles is
    /// followed, on the same stream, by a pass over exactly that file -
    /// asserted inside `spawn_stub_plugin` - whose diff lands.
    #[test]
    fn a_settled_reparse_is_followed_by_a_semantic_pass_over_that_file() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(4);
        let structural = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            result: FileChangeDiff {
                upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
                upsert_edges: vec![unresolved_edge("e1", "n1", "n2")],
                ..Default::default()
            },
        };

        let mut upgraded = unresolved_edge("e1", "n1", "n2");
        upgraded.source = EdgeSource::TsCompiler;
        upgraded.resolved = true;
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            structural,
            Some(FileChangeDiff { upsert_edges: vec![upgraded], ..Default::default() }),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(&mut buf_reader, &mut core_writer, &mut conn, "src/lib.rs", request_id)
            .unwrap();
        plugin.join().unwrap();

        assert_eq!(
            edge_source_and_resolved(&conn, "e1"),
            ("ts-compiler".to_string(), true),
            "the edge the reparse left unresolved must come back upgraded"
        );
        assert_eq!(count(&conn, "edges"), 1);
    }

    /// The semantic pass is an upgrade over a graph that is already
    /// committed and serviceable, so losing it must not lose the reparse
    /// that earned it.
    #[test]
    fn a_failing_semantic_pass_does_not_fail_the_reparse() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(5);
        let structural = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: request_id.clone(),
            result: FileChangeDiff {
                upsert_nodes: vec![canned_node("n1")],
                ..Default::default()
            },
        };

        // `None`: the stub answers the reparse and then goes away, which is
        // what a plugin whose semantic layer died looks like from here.
        let plugin =
            spawn_stub_plugin(plugin_reader, plugin_writer, "src/lib.rs", request_id.clone(), structural, None);

        let mut buf_reader = BufReader::new(core_reader);
        let outcome =
            apply_file_change(&mut buf_reader, &mut core_writer, &mut conn, "src/lib.rs", request_id);
        plugin.join().unwrap();

        assert!(outcome.is_ok(), "a lost upgrade must not undo a committed reparse: {outcome:?}");
        assert_eq!(count(&conn, "nodes"), 1, "the structural diff still committed");
    }

    /// The post-bulk-index shape: nothing to name, so the list is empty and
    /// the plugin reads that as "the whole project".
    #[test]
    fn a_whole_project_semantic_pass_sends_an_empty_file_list() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let plugin =
            spawn_semantic_stub(plugin_reader, plugin_writer, Vec::new(), FileChangeDiff::default());

        let mut buf_reader = BufReader::new(core_reader);
        apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            Vec::new(),
            RequestId::Number(1),
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "edges"), 0);
    }

    /// The derived id has to be distinguishable from the file change it
    /// follows *and* from every counter-issued id that comes after it.
    #[test]
    fn the_semantic_pass_id_cannot_collide_with_the_request_it_follows() {
        let base = RequestId::Number(3);
        let derived = semantic_pass_id(&base);
        assert_ne!(derived, base);
        assert_ne!(derived, RequestId::Number(4));
        assert_ne!(semantic_pass_id(&RequestId::String("3".to_string())), derived);
    }
}
