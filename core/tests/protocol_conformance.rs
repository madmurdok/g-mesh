use std::io::{BufReader, Cursor};

use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::protocol::conformance::{check_bulk_output, check_control_plane_output};
use g_mesh::protocol::jsonrpc::read_message;
use g_mesh::protocol::types::{ControlEnvelope, ControlMessage, RequestId};
use g_mesh::storage::schema;
use g_mesh::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};
use g_mesh::watcher::apply::apply_semantic_pass;
use rusqlite::Connection;

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()))
}

#[test]
fn well_formed_ndjson_fixture_is_conformant() {
    let report = check_bulk_output(&fixture("valid.ndjson"));
    assert!(report.is_conformant(), "{:?}", report.violations);
}

#[test]
fn ndjson_fixture_with_invalid_edge_kind_is_rejected() {
    let report = check_bulk_output(&fixture("invalid_kind.ndjson"));
    assert!(!report.is_conformant());
    assert!(
        report.violations.iter().any(|v| v.message.contains("neither a valid node nor edge")),
        "{:?}",
        report.violations
    );
}

#[test]
fn well_formed_control_plane_fixture_is_conformant() {
    let report = check_control_plane_output(&fixture("valid_control.rpc"));
    assert!(report.is_conformant(), "{:?}", report.violations);
}

#[test]
fn broken_framing_fixture_is_rejected_with_a_specific_error() {
    let report = check_control_plane_output(&fixture("broken_framing.rpc"));
    assert!(!report.is_conformant());
    assert!(
        report.violations.iter().any(|v| v.message.to_lowercase().contains("content-length")),
        "{:?}",
        report.violations
    );
}

#[test]
fn semantic_pass_request_fixture_is_conformant() {
    let report = check_control_plane_output(&fixture("semantic_pass_request.rpc"));
    assert!(report.is_conformant(), "{:?}", report.violations);
}

fn seeded_index() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    // The graph as the structural pass leaves it: two call edges it could
    // only guess at, neither confirmed.
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                NodeRecord::new("n1", "Function", "caller", "a::caller", "src/a.ts", "typescript"),
                NodeRecord::new("n2", "Function", "callee", "a::callee", "src/a.ts", "typescript"),
            ],
            upsert_edges: vec![
                EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false),
                EdgeRecord::new("e2", "n2", "n1", "CALLS", "tree-sitter", false),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    conn
}

fn edge(conn: &Connection, id: &str) -> (String, bool) {
    conn.query_row("SELECT source, resolved FROM edges WHERE id = ?1", [id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
    .unwrap()
}

/// The whole point of the `semanticPass` method, exercised against golden
/// fixture bytes rather than a hand-built struct: a plugin's recorded answer,
/// replayed byte for byte, must upgrade exactly the edge it names.
///
/// Both halves of the round trip are checked. The *request* core writes is
/// held to the same conformance bar a plugin's output is - it must parse as
/// a well-formed control frame, and must be the very envelope the request
/// fixture records - and the *response* fixture is fed back through
/// `apply_semantic_pass`, which is the same commit-and-link pipeline an
/// ordinary reparse runs.
#[test]
fn a_semantic_pass_diff_upgrades_only_the_edge_it_answers_for() {
    let mut conn = seeded_index();
    assert_eq!(edge(&conn, "e1"), ("tree-sitter".to_string(), false));
    assert_eq!(edge(&conn, "e2"), ("tree-sitter".to_string(), false));

    let mut plugin_answer = BufReader::new(Cursor::new(fixture("semantic_pass_upgrade.rpc")));
    let mut core_wrote: Vec<u8> = Vec::new();

    apply_semantic_pass(
        &mut plugin_answer,
        &mut core_wrote,
        &mut conn,
        vec!["src/a.ts".to_string()],
        // Matches the id both fixtures carry; a mismatch is refused outright.
        RequestId::Number(7),
        &EmbeddingPipeline::disabled(),
    )
    .unwrap();

    // e1 is the one the fixture answers for: tree-sitter/false -> ts-compiler/true.
    assert_eq!(
        edge(&conn, "e1"),
        ("ts-compiler".to_string(), true),
        "the answered edge must be confirmed in place"
    );
    // e2 is not in the diff at all, so nothing about it may move.
    assert_eq!(
        edge(&conn, "e2"),
        ("tree-sitter".to_string(), false),
        "an edge the pass said nothing about must be left exactly as it was"
    );
    let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
    assert_eq!(edges, 2, "an upgrade updates the existing row - it never inserts a second one");

    // Core's own emitted request is conformant, and is the fixture's request.
    let report = check_control_plane_output(&core_wrote);
    assert!(report.is_conformant(), "{:?}", report.violations);

    let sent: ControlEnvelope = read_message(&mut BufReader::new(Cursor::new(core_wrote))).unwrap().unwrap();
    let recorded: ControlEnvelope =
        read_message(&mut BufReader::new(Cursor::new(fixture("semantic_pass_request.rpc"))))
            .unwrap()
            .unwrap();
    assert_eq!(sent, recorded);
    assert!(matches!(
        sent.message,
        ControlMessage::SemanticPass { ref file_paths } if file_paths == &["src/a.ts".to_string()]
    ));
}
