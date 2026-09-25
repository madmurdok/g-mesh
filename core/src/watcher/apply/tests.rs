use super::*;
use crate::protocol::jsonrpc::read_message;
use crate::protocol::types::{
    EdgeKind, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::apply_diff;
use rusqlite::Connection;
use std::io::BufReader;

/// A timeout no test below is meant to hit - every stub plugin in this
/// module answers immediately, so this only has to be longer than a
/// slow CI box's scheduling noise. The dedicated timeout tests near the
/// bottom of this module use their own short, explicit durations.
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// `on_timeout` for every call below that is not itself testing the
/// timeout mechanism - asserting it is never invoked would be redundant
/// with `TEST_TIMEOUT` never elapsing, but a stub plugin that hung would
/// otherwise turn into a 5-second wait per test instead of a fast panic.
fn on_timeout_must_not_fire() {
    panic!("on_timeout fired in a test whose stub plugin always answers - the stub or the timeout is broken");
}

fn setup_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn count(conn: &IndexStore, table: &str) -> i64 {
    conn.lock().unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
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
            &FileChangeResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id,
                result,
                incomplete: false,
                incomplete_reason: None,
            },
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
        source: SourceTier::Syntactic,
        engine: "tree-sitter".to_string(),
        resolved: false,
        to_declaration: None,
    }
}

/// `(source, engine, resolved)` - `source` is the tier
/// (`"syntactic"`/`"semantic"`) `edges.source`'s CHECK now enforces,
/// `engine` its own new column (`storage::schema`'s DDL comment on
/// `edges`).
fn edge_source_and_resolved(conn: &IndexStore, id: &str) -> (String, String, bool) {
    conn.lock()
        .unwrap()
        .query_row("SELECT source, engine, resolved FROM edges WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
}

#[test]
fn a_wire_nodes_declaration_list_becomes_storage_records() {
    use crate::protocol::types::WireDeclaration;

    let mut node = canned_node("n1");
    node.declarations = Some(vec![
        WireDeclaration {
            ordinal: 0,
            start_line: 1,
            start_col: 7,
            end_line: 1,
            end_col: 47,
            signature: Some("parse(input: string): string[]".to_string()),
            has_body: false,
        },
        WireDeclaration {
            ordinal: 1,
            start_line: 3,
            start_col: 7,
            end_line: 5,
            end_col: 1,
            signature: None,
            has_body: true,
        },
    ]);

    let record = to_node_record(node);

    assert_eq!(
        record.declarations,
        vec![
            DeclarationRecord {
                ordinal: 0,
                start_line: 1,
                start_col: 7,
                end_line: 1,
                end_col: 47,
                signature: Some("parse(input: string): string[]".to_string()),
                has_body: false,
            },
            DeclarationRecord {
                ordinal: 1,
                start_line: 3,
                start_col: 7,
                end_line: 5,
                end_col: 1,
                signature: None,
                has_body: true,
            },
        ]
    );
    // And an ordinary node still says "no declarations", which `apply_diff`
    // reads as "one" - the same thing an absent field on the wire means.
    assert!(to_node_record(canned_node("n2")).declarations.is_empty());
}

#[test]
fn a_wire_edges_declaration_binding_becomes_a_storage_record() {
    let mut edge = unresolved_edge("e1", "n1", "n2");
    assert_eq!(to_edge_record(edge.clone()).to_declaration, None);

    edge.to_declaration = Some(0);
    assert_eq!(
        to_edge_record(edge).to_declaration,
        Some(0),
        "ordinal 0 is a binding, not the absence of one"
    );
}

/// The GM-264 wire boundary: a container-scoped, qualifiedName-keyed
/// placeholder (the shape only a semantic tier over a containered
/// language sends - see `protocol::types`'s own
/// `wire_node_v2_shape_round_trips_container_and_target` test) becomes
/// the storage-layer `visibility`/`visibility_container`/`container`/
/// `target` fields `apply_diff` writes.
#[test]
fn to_node_record_derives_visibility_container_and_target_from_the_wire_v2_shape() {
    let mut node = canned_node("n1");
    node.visibility = Visibility::Container("github.com/x/app/server".to_string());
    node.container = Some("github.com/x/app/server".to_string());
    node.native_kind = Some("pending_symbol".to_string());
    node.target = Some(PlaceholderTarget {
        scope: TargetScope::Container("github.com/x/app/server".to_string()),
        key: TargetKey::QualifiedName("Server.Close".to_string()),
        from_container: Some("github.com/x/app/client".to_string()),
    });

    let record = to_node_record(node);

    assert!(!record.exported, "container visibility is never Public");
    assert_eq!(record.visibility, "container");
    assert_eq!(record.visibility_container.as_deref(), Some("github.com/x/app/server"));
    assert_eq!(record.container.as_deref(), Some("github.com/x/app/server"));
    let target = record.target.expect("a pending_symbol with a wire target must keep it");
    assert_eq!(target.scope_kind, "container");
    assert_eq!(target.scope, "github.com/x/app/server");
    assert_eq!(target.key_kind, "qualifiedName");
    assert_eq!(target.key, "Server.Close");
    assert_eq!(target.from_container.as_deref(), Some("github.com/x/app/client"));
}

/// The ordinary case: `Visibility::Public` maps to `"public"`/`exported`,
/// and an ordinary (non-placeholder) node carries no target at all.
#[test]
fn to_node_record_maps_public_visibility_and_leaves_target_absent_for_an_ordinary_node() {
    let record = to_node_record(canned_node("n1"));
    assert!(record.exported);
    assert_eq!(record.visibility, "public");
    assert_eq!(record.visibility_container, None);
    assert_eq!(record.target, None);
}

/// `to_edge_record`'s own boundary: `engine` comes from the wire's real
/// `WireEdge.engine`, not from `EdgeRecord::new`'s legacy-string
/// inference off the tier - see `to_edge_record`'s own comment on why the
/// guess `::new` makes has to be overwritten.
#[test]
fn to_edge_record_carries_the_wires_own_engine_rather_than_guessing_one_from_the_tier() {
    let mut edge = unresolved_edge("e1", "n1", "n2");
    edge.source = SourceTier::Semantic;
    edge.engine = "go-types".to_string();

    let record = to_edge_record(edge);

    assert_eq!(record.source, "semantic");
    assert_eq!(record.engine, "go-types", "a real wire engine must never be collapsed to the tier name");
}

#[test]
fn file_change_diff_is_committed_to_sqlite() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(1);
    let canned_response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: FileChangeDiff {
            upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
            delete_node_ids: vec![],
            upsert_edges: vec![WireEdge {
                id: "e1".to_string(),
                from_id: "n1".to_string(),
                to_id: "n2".to_string(),
                kind: EdgeKind::Calls,
                source: SourceTier::Syntactic,
                engine: "tree-sitter".to_string(),
                resolved: false,
                to_declaration: None,
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
    apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
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

    assert_eq!(count(&conn, "nodes"), 2);
    assert_eq!(count(&conn, "edges"), 1);

    let name: String = conn
        .lock()
        .unwrap()
        .query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(name, "foo");
    let start_line: i64 = conn
        .lock()
        .unwrap()
        .query_row("SELECT startLine FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(start_line, 1);
    let edge_kind: String = conn
        .lock()
        .unwrap()
        .query_row("SELECT kind FROM edges WHERE id = 'e1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(edge_kind, "CALLS");
}

#[test]
fn diff_with_deletes_removes_rows() {
    let mut raw_conn = setup_conn();
    // Seed rows the stub plugin's diff will delete.
    apply_diff(
        &mut raw_conn,
        &Diff {
            upsert_nodes: vec![NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")],
            ..Default::default()
        },
    )
    .unwrap();
    let conn = IndexStore::new(raw_conn);

    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();

    let request_id = RequestId::String("req-2".to_string());
    let canned_response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
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
    apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
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

    assert_eq!(count(&conn, "nodes"), 0);
}

#[test]
fn mismatched_response_id_is_rejected() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(10);
    let wrong_id_response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
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
    let result = apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        "src/lib.rs",
        request_id,
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        true,
        &mut on_timeout_must_not_fire,
    );
    plugin.join().unwrap();

    assert!(result.is_err(), "a response for a different request id must not be applied");
    assert_eq!(count(&conn, "nodes"), 0, "diff from a mismatched-id response must not be committed");
}

#[test]
fn empty_diff_response_is_a_safe_no_op() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(3);
    let empty_response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
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
        &conn,
        "src/unchanged.rs",
        request_id,
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        true,
        &mut on_timeout_must_not_fire,
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
    incomplete: bool,
    incomplete_reason: Option<&'static str>,
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
            &FileChangeResponse {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id,
                result,
                incomplete,
                incomplete_reason: incomplete_reason.map(str::to_string),
            },
        )
        .unwrap();
    })
}

/// The acceptance criterion, at the unit level: an edge the structural
/// pass left as a `tree-sitter` guess comes back confirmed, and nothing
/// else in the index moves.
#[test]
fn a_semantic_pass_upgrades_an_edge_in_place_and_leaves_the_others_alone() {
    let mut raw_conn = setup_conn();
    apply_diff(
        &mut raw_conn,
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
    let conn = IndexStore::new(raw_conn);

    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();

    // Only e1 is answered for - e2 is not in the diff at all.
    let mut upgraded = unresolved_edge("e1", "n1", "n2");
    upgraded.source = SourceTier::Semantic;
    upgraded.engine = "ts-compiler".to_string();
    upgraded.resolved = true;
    let plugin = spawn_semantic_stub(
        plugin_reader,
        plugin_writer,
        vec!["src/lib.rs".to_string()],
        FileChangeDiff { upsert_edges: vec![upgraded], ..Default::default() },
        false,
        None,
    );

    let mut buf_reader = BufReader::new(core_reader);
    apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        vec!["src/lib.rs".to_string()],
        RequestId::Number(9),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    plugin.join().unwrap();

    assert_eq!(
        edge_source_and_resolved(&conn, "e1"),
        ("semantic".to_string(), "ts-compiler".to_string(), true),
        "the answered edge must be upgraded in place, not duplicated"
    );
    assert_eq!(
        edge_source_and_resolved(&conn, "e2"),
        ("syntactic".to_string(), "tree-sitter".to_string(), false),
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
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(4);
    let structural = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: FileChangeDiff {
            upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
            upsert_edges: vec![unresolved_edge("e1", "n1", "n2")],
            ..Default::default()
        },
    };

    let mut upgraded = unresolved_edge("e1", "n1", "n2");
    upgraded.source = SourceTier::Semantic;
    upgraded.engine = "ts-compiler".to_string();
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
    apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
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

    assert_eq!(
        edge_source_and_resolved(&conn, "e1"),
        ("semantic".to_string(), "ts-compiler".to_string(), true),
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
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(5);
    let structural = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
    };

    // `None`: the stub answers the reparse and then goes away, which is
    // what a plugin whose semantic layer died looks like from here.
    let plugin =
        spawn_stub_plugin(plugin_reader, plugin_writer, "src/lib.rs", request_id.clone(), structural, None);

    let mut buf_reader = BufReader::new(core_reader);
    let outcome = apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        "src/lib.rs",
        request_id,
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        true,
        &mut on_timeout_must_not_fire,
    );
    plugin.join().unwrap();

    assert!(outcome.is_ok(), "a lost upgrade must not undo a committed reparse: {outcome:?}");
    assert_eq!(count(&conn, "nodes"), 1, "the structural diff still committed");
}

/// GM-270's acceptance criterion at the unit level: `semantic_pass_capable
/// = false` must skip the second round trip entirely, not just ignore its
/// answer - a plugin whose manifest never declared
/// `capabilities.semantic_pass = true` gets no `semanticPass` request at
/// all, per the architecture doc's `plugin.toml additions` ("core never
/// sends it, and no empty-diff answer is required").
///
/// Discriminates: the stub here is built with `spawn_stub_plugin`'s
/// `semantic_diff: None`, which answers only the first (`FileChanged`)
/// request and then returns - it never reads a second frame. If this
/// function sent `semanticPass` anyway (the pre-GM-270, unconditional
/// behaviour), the stub thread would still be blocked reading a request
/// nobody is answering when the main thread reaches `plugin.join()`
/// below, and the test would hang instead of completing - flip
/// `semantic_pass_capable` to `true` here to see it hang.
#[test]
fn a_semantic_pass_incapable_plugin_is_never_sent_a_semantic_pass_request() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let request_id = RequestId::Number(6);
    let structural = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
    };

    // `None`: this stub answers exactly one request and never looks for a
    // second - the same shape `a_failing_semantic_pass_does_not_fail_the_reparse`
    // uses for "the semantic layer died", reused here for "was never
    // asked in the first place".
    let plugin =
        spawn_stub_plugin(plugin_reader, plugin_writer, "src/lib.rs", request_id.clone(), structural, None);

    let mut buf_reader = BufReader::new(core_reader);
    apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        "src/lib.rs",
        request_id,
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        false,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    plugin.join().unwrap();

    assert_eq!(count(&conn, "nodes"), 1, "the structural diff must still commit with no semantic pass sent");
}

/// The post-bulk-index shape: nothing to name, so the list is empty and
/// the plugin reads that as "the whole project".
#[test]
fn a_whole_project_semantic_pass_sends_an_empty_file_list() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let plugin =
        spawn_semantic_stub(plugin_reader, plugin_writer, Vec::new(), FileChangeDiff::default(), false, None);

    let mut buf_reader = BufReader::new(core_reader);
    apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        Vec::new(),
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    plugin.join().unwrap();

    assert_eq!(count(&conn, "edges"), 0);
}

/// A plugin that answers a whole-project pass with `incomplete` (GM-289's
/// `FileChangeResponse::incomplete`) gets both halves of what it asked
/// for: the diff it did manage is committed, and the pass is reported as
/// *not* finished, which is what stops `daemon::semantic` recording
/// `language_state.semanticPassAt` for that language.
#[test]
fn an_incomplete_whole_project_pass_commits_its_diff_and_is_still_an_error() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let plugin = spawn_semantic_stub(
        plugin_reader,
        plugin_writer,
        Vec::new(),
        FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
        true,
        Some("the language server exited during the pass"),
    );

    let mut buf_reader = BufReader::new(core_reader);
    let outcome = apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        Vec::new(),
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    );
    plugin.join().unwrap();

    let err = outcome.expect_err("an incomplete pass must not be reported as a completed one");
    assert!(format!("{err:#}").contains("incomplete"), "{err:#}");
    assert!(
        format!("{err:#}").contains("the language server exited during the pass"),
        "the plugin's own reason is what the error carries: {err:#}"
    );
    assert_eq!(
        count(&conn, "nodes"),
        1,
        "what the pass did resolve is committed - an incomplete pass is partial, not failed"
    );
}

/// The same flag on a *per-file* pass is a log line, not a failure: there
/// is no completion record for one to protect, and failing it would only
/// make `apply_file_change` print the same thing twice.
#[test]
fn an_incomplete_per_file_pass_is_not_an_error() {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let conn = IndexStore::new(setup_conn());

    let plugin = spawn_semantic_stub(
        plugin_reader,
        plugin_writer,
        vec!["src/lib.rs".to_string()],
        FileChangeDiff::default(),
        true,
        None,
    );

    let mut buf_reader = BufReader::new(core_reader);
    apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        vec!["src/lib.rs".to_string()],
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .expect("a per-file pass reports incompleteness without failing");
    plugin.join().unwrap();
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
