//! Bridges a (debounced) file-change event to a committed diff: sends a
//! `FileChanged` control-plane request to a language plugin, reads back its
//! diff, and applies it through `storage::write::apply_diff`.
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

use crate::protocol::jsonrpc::{read_message, write_message};
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, RequestId, WireEdge, WireNode,
    JSONRPC_VERSION,
};
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};

/// Sends a `FileChanged` request (tagged with `request_id`) for `file_path`
/// over `writer`, reads the plugin's `FileChangeResponse` off `reader`,
/// validates the response id matches the request id, converts the wire
/// diff into a `storage::write::Diff`, and commits it via `apply_diff`.
///
/// `request_id` is supplied by the caller rather than generated internally
/// so this function stays pure and easy to test; a later ticket wiring this
/// into a live plugin session can decide whether id generation belongs here
/// or further up the call stack (e.g. a per-session atomic counter).
pub fn apply_file_change<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    file_path: impl Into<String>,
    request_id: RequestId,
) -> Result<()> {
    let request = ControlEnvelope {
        jsonrpc: JSONRPC_VERSION.to_string(),
        id: Some(request_id.clone()),
        message: ControlMessage::FileChanged { file_path: file_path.into() },
    };
    write_message(writer, &request).context("failed to write FileChanged request to plugin")?;

    let response: FileChangeResponse = read_message(reader)
        .context("failed to read plugin's file-change response")?
        .context("plugin closed its output before responding to FileChanged")?;

    if response.id != request_id {
        bail!(
            "file-change response id {:?} does not match request id {:?} - refusing to apply a diff that answers a different request",
            response.id,
            request_id,
        );
    }

    let diff = to_storage_diff(response.result);
    apply_diff(conn, &diff).context("failed to apply file-change diff")?;
    Ok(())
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
    fn spawn_stub_plugin(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_path: &'static str,
        expected_id: RequestId,
        response: FileChangeResponse,
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
        })
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
}
