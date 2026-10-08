use super::*;
use crate::protocol::jsonrpc::read_message;
use crate::protocol::types::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, QualifiedPath, Range, SourceTier, TargetKey,
    TargetScope, Visibility, WireEdge, WireNode,
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

/// A project root on which every file these tests name exists, so a diff that
/// upserts nothing is not read as the file being gone.
fn project_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    for file in ["src/lib.rs", "src/unchanged.rs"] {
        std::fs::write(root.path().join(file), "").unwrap();
    }
    root
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
        alias_paths: Vec::new(),
        untyped_calls: Vec::new(),
        qualified_path: None,
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
            ControlMessage::SemanticPass { file_paths, .. } => {
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

    let record = to_node_record(node, &mut PathWarnings::default());

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
    assert!(to_node_record(canned_node("n2"), &mut PathWarnings::default()).declarations.is_empty());
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
        key_path: None,
    });

    let record = to_node_record(node, &mut PathWarnings::default());

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
    let record = to_node_record(canned_node("n1"), &mut PathWarnings::default());
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
            complete: false,
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
        project_root().path(),
        "rust",
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
        project_root().path(),
        "rust",
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
        project_root().path(),
        "rust",
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
        project_root().path(),
        "rust",
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
            ControlMessage::SemanticPass { file_paths, .. } => {
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
        "rust",
        None,
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
        project_root().path(),
        "rust",
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
        project_root().path(),
        "rust",
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
        project_root().path(),
        "rust",
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
        "rust",
        None,
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
        "rust",
        None,
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
        "rust",
        None,
        vec!["src/lib.rs".to_string()],
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .expect("a per-file pass reports incompleteness without failing");
    plugin.join().unwrap();
}

/// A live index holding `rust`'s semantic edges `sem-stale` (n1 -> n2) and
/// `sem-kept` (n1 -> n3), its structural `syn` (n2 -> n1), and `go-sem`, a
/// semantic edge of another language.
fn index_with_semantic_edges() -> IndexStore {
    let semantic = |id: &str, from: &str, to: &str| {
        let mut edge = EdgeRecord::new(id, from, to, "CALLS", "semantic", true);
        edge.engine = "rust-analyzer".to_string();
        edge
    };
    let mut raw_conn = setup_conn();
    apply_diff(
        &mut raw_conn,
        &Diff {
            upsert_nodes: vec![
                NodeRecord::new("n1", "Function", "a", "m::a", "src/lib.rs", "rust"),
                NodeRecord::new("n2", "Function", "b", "m::b", "src/lib.rs", "rust"),
                NodeRecord::new("n3", "Function", "c", "m::c", "src/lib.rs", "rust"),
                NodeRecord::new("g1", "Function", "g", "p.g", "main.go", "go"),
                NodeRecord::new("g2", "Function", "h", "p.h", "main.go", "go"),
            ],
            upsert_edges: vec![
                semantic("sem-stale", "n1", "n2"),
                semantic("sem-kept", "n1", "n3"),
                EdgeRecord::new("syn", "n2", "n1", "CALLS", "tree-sitter", true),
                semantic("go-sem", "g1", "g2"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    IndexStore::new(raw_conn)
}

/// Runs one `rust` semantic pass over `file_paths` against `conn`, whose
/// answer re-sends only `sem-kept`, and returns the ids of the edges left.
fn pass_resending_only_sem_kept(
    conn: &IndexStore,
    file_paths: Vec<String>,
    incomplete: bool,
) -> (Result<()>, Vec<String>) {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let mut resent = unresolved_edge("sem-kept", "n1", "n3");
    resent.source = SourceTier::Semantic;
    resent.engine = "rust-analyzer".to_string();
    let plugin = spawn_semantic_stub(
        plugin_reader,
        plugin_writer,
        file_paths.clone(),
        FileChangeDiff { upsert_edges: vec![resent], ..Default::default() },
        incomplete,
        None,
    );
    let mut buf_reader = BufReader::new(core_reader);
    let outcome = apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        conn,
        "rust",
        Some("rust"),
        file_paths,
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    );
    plugin.join().unwrap();
    let ids = conn
        .lock()
        .unwrap()
        .prepare("SELECT id FROM edges ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    (outcome, ids)
}

/// A complete whole-project pass deletes the language's semantic edges it
/// did not re-send, and nothing else: not its structural edges, not another
/// language's semantic ones.
///
/// Control: remove the `sweep_semantic_edges` call from
/// `apply_semantic_pass_in` -> `sem-stale` survives.
#[test]
fn a_complete_whole_project_pass_sweeps_the_semantic_edges_it_did_not_resend() {
    let conn = index_with_semantic_edges();
    let (outcome, ids) = pass_resending_only_sem_kept(&conn, Vec::new(), false);
    outcome.expect("a complete pass succeeds");
    assert_eq!(ids, vec!["go-sem", "sem-kept", "syn"]);
    assert_eq!(count(&conn, "nodes"), 5, "the sweep deletes edges only");
}

/// An incomplete whole-project pass is a partial answer: what it did not
/// re-send may be what it never got to, so nothing is swept.
///
/// Control: sweep before the `outcome.incomplete` check (make the sweep's
/// branch run whenever `whole_project` holds) -> `sem-stale` is gone.
#[test]
fn an_incomplete_whole_project_pass_sweeps_nothing() {
    let conn = index_with_semantic_edges();
    let (outcome, ids) = pass_resending_only_sem_kept(&conn, Vec::new(), true);
    outcome.expect_err("an incomplete whole-project pass is still an error");
    assert_eq!(ids, vec!["go-sem", "sem-kept", "sem-stale", "syn"]);
}

/// A per-file pass answers for one file, not the language, so nothing is
/// swept.
///
/// Control: drop the `whole_project` condition from the sweep's branch ->
/// `sem-stale` is gone.
#[test]
fn a_per_file_pass_sweeps_nothing() {
    let conn = index_with_semantic_edges();
    let (outcome, ids) = pass_resending_only_sem_kept(&conn, vec!["src/lib.rs".to_string()], false);
    outcome.expect("a complete per-file pass succeeds");
    assert_eq!(ids, vec!["go-sem", "sem-kept", "sem-stale", "syn"]);
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

/// An index with `rust` pending and `a.rs`, `b.rs` pending files.
fn index_with_pending_files() -> IndexStore {
    let conn = setup_conn();
    conn.execute_batch(
        "INSERT INTO semantic_pending (language, since) VALUES ('rust', '2026-09-26T10:14:03Z');
         INSERT INTO semantic_pending_files (language, filePath) VALUES ('rust', 'a.rs'), ('rust', 'b.rs');",
    )
    .unwrap();
    IndexStore::new(conn)
}

fn per_file_pass_over_a(conn: &IndexStore, incomplete: bool) {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let plugin = spawn_semantic_stub(
        plugin_reader,
        plugin_writer,
        vec!["a.rs".to_string()],
        FileChangeDiff::default(),
        incomplete,
        None,
    );
    let mut buf_reader = BufReader::new(core_reader);
    apply_semantic_pass(
        &mut buf_reader,
        &mut core_writer,
        conn,
        "rust",
        None,
        vec!["a.rs".to_string()],
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    plugin.join().unwrap();
}

fn pending_state(conn: &IndexStore) -> (usize, Vec<String>) {
    conn.with(|conn| {
        let languages: i64 =
            conn.query_row("SELECT COUNT(*) FROM semantic_pending", [], |row| row.get(0)).unwrap();
        let files = conn
            .prepare("SELECT filePath FROM semantic_pending_files ORDER BY filePath")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        (languages as usize, files)
    })
}

/// A complete per-file pass over `a.rs` refreshes it: `a.rs` is no longer
/// pending, `b.rs` and the language row are. Control: remove the clear from
/// `apply_semantic_pass_in` -> `a.rs` remains.
#[test]
fn a_complete_per_file_pass_clears_only_its_file() {
    let conn = index_with_pending_files();

    per_file_pass_over_a(&conn, false);

    assert_eq!(pending_state(&conn), (1, vec!["b.rs".to_string()]));
}

/// An incomplete per-file pass clears nothing. Control: clear regardless of
/// `incomplete` -> `a.rs` is gone.
#[test]
fn an_incomplete_per_file_pass_clears_nothing() {
    let conn = index_with_pending_files();

    per_file_pass_over_a(&conn, true);

    assert_eq!(pending_state(&conn), (1, vec!["a.rs".to_string(), "b.rs".to_string()]));
}

fn path_of(first: &str, rest: &[(&str, &str)]) -> QualifiedPath {
    rest.iter().fold(QualifiedPath::root(first), |path, (sep, name)| path.child(*sep, *name))
}

/// `canned_node` (`mod::foo`) with its path and the alias `alias::foo`.
fn pathed_node(id: &str, alias: &str) -> WireNode {
    WireNode {
        qualified_path: Some(path_of("mod", &[("::", "foo")])),
        alias_paths: vec![path_of(alias, &[("::", "foo")])],
        ..canned_node(id)
    }
}

fn suffixes(conn: &IndexStore, node_id: &str) -> Vec<String> {
    conn.lock()
        .unwrap()
        .prepare("SELECT suffix FROM qualified_suffixes WHERE nodeId = ?1 ORDER BY suffix")
        .unwrap()
        .query_map([node_id], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn a_valid_path_and_alias_reach_the_record_without_a_warning() {
    let mut warnings = PathWarnings::default();
    let record = to_node_record(pathed_node("n1", "Alias"), &mut warnings);

    assert_eq!(record.qualified_path, Some(path_of("mod", &[("::", "foo")])));
    assert_eq!(record.alias_paths, vec![path_of("Alias", &[("::", "foo")])]);
    assert!(warnings.emitted.is_empty());
}

/// A path that does not join back to `qualifiedName` is dropped with its
/// aliases and one warning per file; the node itself is kept unchanged.
/// Control: return the wire path unchecked from `checked_paths` (the bad path
/// reaches the record, no warning).
#[test]
fn a_path_that_does_not_join_back_is_dropped_with_a_warning_and_the_node_kept() {
    let mut warnings = PathWarnings::default();
    let bad =
        WireNode { qualified_path: Some(path_of("mod", &[(".", "foo")])), ..pathed_node("n1", "Alias") };

    let record = to_node_record(bad, &mut warnings);
    assert_eq!(record.id, "n1");
    assert_eq!(record.qualified_name, "mod::foo");
    assert_eq!(record.qualified_path, None);
    assert!(record.alias_paths.is_empty(), "a bad path takes its aliases with it");
    assert_eq!(warnings.emitted.len(), 1);
    assert!(warnings.emitted[0].contains("src/lib.rs") && warnings.emitted[0].contains("qualifiedPath"));

    let also_bad = WireNode { qualified_path: Some(QualifiedPath::root("x")), ..canned_node("n2") };
    to_node_record(also_bad, &mut warnings);
    assert_eq!(warnings.emitted.len(), 1, "one warning per file");
    let other_file = WireNode {
        qualified_path: Some(QualifiedPath::root("x")),
        file_path: "src/other.rs".to_string(),
        ..canned_node("n3")
    };
    to_node_record(other_file, &mut warnings);
    assert_eq!(warnings.emitted.len(), 2, "another file warns again");
}

/// An alias that breaks a rule is dropped alone. Control: keep every alias
/// in `checked_paths` (the one-segment alias survives).
#[test]
fn a_bad_alias_is_dropped_alone() {
    let mut warnings = PathWarnings::default();
    let node = WireNode {
        alias_paths: vec![QualifiedPath::root("foo"), path_of("Alias", &[("::", "foo")])],
        ..pathed_node("n1", "unused")
    };

    let record = to_node_record(node, &mut warnings);
    assert_eq!(record.qualified_path, Some(path_of("mod", &[("::", "foo")])));
    assert_eq!(record.alias_paths, vec![path_of("Alias", &[("::", "foo")])]);
    assert_eq!(warnings.emitted.len(), 1);
}

/// A `keyPath` that does not join back to its key is dropped; the target is
/// kept. Control: copy `target.key_path` without `check_key_path`.
#[test]
fn a_bad_key_path_is_dropped_and_the_target_kept() {
    let mut warnings = PathWarnings::default();
    let target = |key_path| PlaceholderTarget {
        scope: TargetScope::File("src/b.rs".to_string()),
        key: TargetKey::QualifiedName("a::T.f".to_string()),
        from_container: None,
        key_path: Some(key_path),
    };
    let good =
        WireNode { target: Some(target(path_of("a", &[("::", "T"), (".", "f")]))), ..canned_node("p1") };
    let bad =
        WireNode { target: Some(target(path_of("a", &[("::", "T"), ("::", "f")]))), ..canned_node("p2") };

    let good = to_node_record(good, &mut warnings).target.unwrap();
    let bad = to_node_record(bad, &mut warnings).target.unwrap();
    assert_eq!(good.key_path, Some(path_of("a", &[("::", "T"), (".", "f")])));
    assert_eq!((bad.key.as_str(), bad.key_path), ("a::T.f", None));
    assert_eq!(warnings.emitted.len(), 1);
}

/// A node in the shape every plugin sends today (no path keys) indexes with
/// a NULL path, no suffix rows and no warning, and its row equals a record
/// built the old way.
#[test]
fn an_old_shape_node_indexes_exactly_as_before() {
    let line = r#"{"id":"n1","kind":"Function","name":"foo","qualifiedName":"mod::foo","filePath":"src/lib.rs","range":{"start":{"line":1,"col":0},"end":{"line":3,"col":1}},"visibility":"public","language":"rust"}"#;
    let node: WireNode = serde_json::from_str(line).unwrap();
    let mut warnings = PathWarnings::default();
    let record = to_node_record(node, &mut warnings);
    assert!(warnings.emitted.is_empty());
    assert_eq!((record.qualified_path.as_ref(), record.alias_paths.len()), (None, 0));

    let mut conn = setup_conn();
    apply_diff(&mut conn, &Diff { upsert_nodes: vec![record], ..Default::default() }).unwrap();
    let conn = IndexStore::new(conn);
    assert_eq!(count(&conn, "qualified_suffixes"), 0);
    let stored: Option<String> = conn
        .lock()
        .unwrap()
        .query_row("SELECT qualifiedPath FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stored, None);
    let found =
        crate::graph::queries::find_by_qualified_name(&conn.lock().unwrap(), "mod::foo", None).unwrap();
    assert_eq!(found.len(), 1);
}

/// Sends `diff` as a reparse of `src/lib.rs` through `apply_file_change`.
fn reparse(conn: &IndexStore, id: i64, diff: FileChangeDiff) {
    reparse_in(conn, project_root().path(), id, diff);
}

/// [`reparse`] of `src/lib.rs` against `root`, which may lack the file.
fn reparse_in(conn: &IndexStore, root: &std::path::Path, id: i64, diff: FileChangeDiff) {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let request_id = RequestId::Number(id);
    let response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: diff,
    };
    let plugin = spawn_stub_plugin(
        plugin_reader,
        plugin_writer,
        "src/lib.rs",
        request_id.clone(),
        response,
        Some(FileChangeDiff::default()),
    );
    apply_file_change(
        &mut BufReader::new(core_reader),
        &mut core_writer,
        conn,
        root,
        "rust",
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
}

/// An incremental reparse that re-sends a node with another alias replaces
/// its suffix rows, and one that deletes a node removes them. Control: drop
/// `qualified_path`/`alias_paths` from `to_node_record`'s result (no rows at
/// all after the first reparse).
#[test]
fn an_incremental_reparse_rebuilds_suffix_rows() {
    let conn = IndexStore::new(setup_conn());
    let upsert = |nodes| FileChangeDiff { upsert_nodes: nodes, ..Default::default() };

    reparse(&conn, 1, upsert(vec![pathed_node("n1", "A"), pathed_node("n2", "B")]));
    assert_eq!(suffixes(&conn, "n1"), vec!["A::foo"]);
    assert_eq!(suffixes(&conn, "n2"), vec!["B::foo"]);

    reparse(
        &conn,
        2,
        FileChangeDiff { delete_node_ids: vec!["n2".to_string()], ..upsert(vec![pathed_node("n1", "C")]) },
    );
    assert_eq!(suffixes(&conn, "n1"), vec!["C::foo"]);
    assert_eq!(count(&conn, "qualified_suffixes"), 1, "n2's rows went with it");
}

/// `canned_node` calling `names` through untyped receivers.
fn untyped_node(id: &str, names: &[&str]) -> WireNode {
    WireNode { untyped_calls: names.iter().map(|name| name.to_string()).collect(), ..canned_node(id) }
}

fn untyped_rows(conn: &IndexStore) -> Vec<(String, String)> {
    conn.lock()
        .unwrap()
        .prepare("SELECT nodeId, name FROM untyped_calls ORDER BY nodeId, name")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A reparse that re-sends a node replaces its
/// untyped-call rows, one that re-sends it without the key clears them, and
/// one that deletes a node removes them - no stale row survives. Control:
/// drop `untyped_calls: node.untyped_calls` from `to_node_record` (no rows
/// at all after the first reparse).
#[test]
fn an_incremental_reparse_replaces_untyped_call_rows() {
    let conn = IndexStore::new(setup_conn());
    let row = |id: &str, name: &str| (id.to_string(), name.to_string());

    reparse(
        &conn,
        1,
        FileChangeDiff {
            upsert_nodes: vec![untyped_node("n1", &["m", "n"]), untyped_node("n2", &["m"])],
            ..Default::default()
        },
    );
    assert_eq!(untyped_rows(&conn), vec![row("n1", "m"), row("n1", "n"), row("n2", "m")]);

    reparse(
        &conn,
        2,
        FileChangeDiff {
            upsert_nodes: vec![untyped_node("n1", &["o"]), untyped_node("n2", &[])],
            ..Default::default()
        },
    );
    assert_eq!(untyped_rows(&conn), vec![row("n1", "o")], "replaced, and cleared by an absent key");

    reparse(&conn, 3, FileChangeDiff { delete_node_ids: vec!["n1".to_string()], ..Default::default() });
    assert!(untyped_rows(&conn).is_empty(), "a deleted node's rows go with it");
}

// --- rows core retires for a file: gone, or answered in full -------------

fn node_in(id: &str, file_path: &str) -> WireNode {
    WireNode { file_path: file_path.to_string(), ..canned_node(id) }
}

fn semantic_edge(id: &str, from: &str, to: &str) -> WireEdge {
    WireEdge {
        source: SourceTier::Semantic,
        engine: "types".to_string(),
        resolved: true,
        ..unresolved_edge(id, from, to)
    }
}

fn ids(conn: &IndexStore, sql: &str) -> Vec<String> {
    let guard = conn.lock().unwrap();
    let mut stmt = guard.prepare(sql).unwrap();
    let rows =
        stmt.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap();
    rows
}

/// `src/lib.rs` holds `n1`, `n2` and a placeholder `p`, with a syntactic
/// edge `s12` and semantic edges `m1p` (from `n1`) and `m2p` (from `n2`);
/// `src/other.rs` holds `o`, with `o1` into `n1`. `src/lib.rs` has an
/// `indexed_files` row. Foreign keys are off, as on the daemon's connection:
/// `o1` outlives the node it points into.
fn seeded_store() -> IndexStore {
    let mut raw = setup_conn();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let seed = FileChangeDiff {
        upsert_nodes: vec![
            node_in("n1", "src/lib.rs"),
            node_in("n2", "src/lib.rs"),
            node_in("p", "src/lib.rs"),
            node_in("o", "src/other.rs"),
        ],
        upsert_edges: vec![
            unresolved_edge("s12", "n1", "n2"),
            semantic_edge("m1p", "n1", "p"),
            semantic_edge("m2p", "n2", "p"),
            unresolved_edge("o1", "o", "n1"),
        ],
        ..Default::default()
    };
    apply_diff(&mut raw, &to_storage_diff(seed, &mut PathWarnings::default())).unwrap();
    crate::storage::write::upsert_indexed_file(&raw, "src/lib.rs", 1, "hash").unwrap();
    crate::storage::write::upsert_indexed_file(&raw, "src/other.rs", 1, "hash").unwrap();
    IndexStore::new(raw)
}

#[test]
fn a_file_gone_from_disk_loses_every_row_it_owns_though_the_plugin_named_none() {
    let conn = seeded_store();
    reparse_in(&conn, tempfile::tempdir().unwrap().path(), 1, FileChangeDiff::default());

    assert_eq!(ids(&conn, "SELECT id FROM nodes ORDER BY id"), vec!["o"]);
    // `o1` points into a deleted node from another file: left for that
    // file's next reparse.
    assert_eq!(ids(&conn, "SELECT id FROM edges ORDER BY id"), vec!["o1"]);
    assert_eq!(ids(&conn, "SELECT filePath FROM indexed_files"), vec!["src/other.rs"]);
}

#[test]
fn an_empty_answer_for_a_file_still_on_disk_deletes_nothing() {
    let conn = seeded_store();
    reparse(&conn, 1, FileChangeDiff::default());

    assert_eq!(count(&conn, "nodes"), 4);
    assert_eq!(count(&conn, "edges"), 4);
    assert_eq!(count(&conn, "indexed_files"), 2);
}

#[test]
fn a_complete_answer_retires_what_it_does_not_upsert_but_keeps_semantic_edges_of_surviving_nodes() {
    let conn = seeded_store();
    reparse(
        &conn,
        1,
        FileChangeDiff {
            upsert_nodes: vec![node_in("n1", "src/lib.rs")],
            complete: true,
            ..Default::default()
        },
    );

    assert_eq!(ids(&conn, "SELECT id FROM nodes ORDER BY id"), vec!["n1", "o"]);
    // `s12` is structural and not re-sent; `m2p` leaves a deleted node; `m1p`
    // is the semantic pass's, from a node that stays, and waits for it.
    assert_eq!(ids(&conn, "SELECT id FROM edges ORDER BY id"), vec!["m1p", "o1"]);
    assert_eq!(count(&conn, "indexed_files"), 2);
}

#[test]
fn a_partial_answer_deletes_only_what_it_names() {
    let conn = seeded_store();
    reparse(
        &conn,
        1,
        FileChangeDiff { upsert_nodes: vec![node_in("n1", "src/lib.rs")], ..Default::default() },
    );

    assert_eq!(count(&conn, "nodes"), 4);
    assert_eq!(count(&conn, "edges"), 4);
}

// --- GM-486: a semantic pass re-sending a caller to shorten its list ----------

/// Every row `table` holds for `node_id`, each rendered as text, in order.
fn rows_of(conn: &IndexStore, table: &str, node_id: &str) -> Vec<String> {
    let conn = conn.lock().unwrap();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM {table} WHERE {} = ?1",
            if table == "nodes" { "id" } else { "nodeId" }
        ))
        .unwrap();
    let columns = stmt.column_count();
    let mut rows: Vec<String> = stmt
        .query_map([node_id], |row| {
            Ok((0..columns).map(|i| format!("{:?}", row.get_ref(i).unwrap())).collect::<Vec<_>>().join("|"))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.sort();
    rows
}

fn vector_of(conn: &IndexStore, node_id: &str) -> Option<Vec<u8>> {
    conn.lock()
        .unwrap()
        .query_row("SELECT embedding FROM vectors WHERE nodeId = ?1", [node_id], |row| row.get(0))
        .ok()
}

/// A caller as the structural tier sends it: every column and child row a
/// node can carry, and two untyped receiver calls.
fn rich_caller() -> WireNode {
    use crate::protocol::types::WireDeclaration;
    WireNode {
        signature: Some("pub fn caller(ws: Vec<W>)".to_string()),
        doc_comment: Some("Calls frob on each.".to_string()),
        container: Some("mod".to_string()),
        declarations: Some(vec![
            WireDeclaration {
                ordinal: 0,
                start_line: 1,
                start_col: 0,
                end_line: 2,
                end_col: 1,
                signature: Some("pub fn caller(ws: Vec<W>)".to_string()),
                has_body: false,
            },
            WireDeclaration {
                ordinal: 1,
                start_line: 3,
                start_col: 0,
                end_line: 5,
                end_col: 1,
                signature: None,
                has_body: true,
            },
        ]),
        target: Some(PlaceholderTarget {
            scope: TargetScope::Container("mod".to_string()),
            key: TargetKey::QualifiedName("mod::foo".to_string()),
            from_container: Some("mod".to_string()),
            key_path: None,
        }),
        untyped_calls: vec!["frob".to_string(), "len".to_string()],
        ..pathed_node("caller", "Alias")
    }
}

/// **GM-486.** The bridge re-sends a caller whole with only `untypedCalls`
/// shortened. Through a real semantic round trip, that shortens its
/// `untyped_calls` rows and leaves every other column and child row -
/// declarations, qualified and alias suffixes, placeholder target - as the
/// structural tier stored them. And since the caller already has a vector,
/// the pass does not embed it again, while a genuinely new node in the
/// same answer still is embedded.
///
/// Controls, in `round_trip`: drop the `SemanticPass` `retain` (the caller
/// is re-embedded: its text reaches the model and its vector changes); keep
/// only nodes that *have* a vector (`embedded.contains`) or clear
/// `upsert_nodes` instead (the new node gets no vector); move the `retain`
/// before `apply_diff_linked` (the caller's rows are never shortened). The
/// child-row half guards the bridge's `..node.clone()` in
/// `trim_untyped_calls`, which `a_re_sent_caller_keeps_every_other_column`
/// pins on the SDK side.
#[test]
fn a_semantic_pass_shortens_a_callers_list_keeps_its_other_rows_and_does_not_re_embed_it() {
    use crate::embedding::pipeline::test_support::{fake_model_dir, fake_pipeline, Counters};

    let scratch = tempfile::tempdir().unwrap();
    let model_dir = fake_model_dir(&scratch.path().join("model"), "weights v1");
    let counters = Counters::default();
    let pipeline = fake_pipeline(&model_dir, None, &counters);

    let mut raw_conn = setup_conn();
    apply_diff(
        &mut raw_conn,
        &Diff {
            upsert_nodes: vec![to_node_record(rich_caller(), &mut PathWarnings::default())],
            ..Default::default()
        },
    )
    .unwrap();
    let stored_vector = [0.25_f32, 0.5, 0.75];
    crate::storage::vectors::insert(&raw_conn, "caller", &stored_vector, "earlier-model").unwrap();
    let conn = IndexStore::new(raw_conn);

    let children = ["declarations", "qualified_suffixes", "placeholder_targets"];
    let before: Vec<Vec<String>> = children.iter().map(|table| rows_of(&conn, table, "caller")).collect();
    assert_eq!(
        before.iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 1, 1],
        "the fixture stores every child row"
    );
    let node_before = rows_of(&conn, "nodes", "caller");
    let vector_before = vector_of(&conn, "caller").expect("the caller was embedded by an earlier pass");

    let fresh = WireNode {
        id: "fresh".to_string(),
        signature: Some("pub fn fresh()".to_string()),
        ..canned_node("fresh")
    };
    let answer = FileChangeDiff {
        upsert_nodes: vec![WireNode { untyped_calls: vec!["len".to_string()], ..rich_caller() }, fresh],
        ..Default::default()
    };
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let plugin = spawn_semantic_stub(plugin_reader, plugin_writer, Vec::new(), answer, false, None);
    apply_semantic_pass(
        &mut BufReader::new(core_reader),
        &mut core_writer,
        &conn,
        "rust",
        None,
        Vec::new(),
        RequestId::Number(7),
        &pipeline,
        TEST_TIMEOUT,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    plugin.join().unwrap();

    assert_eq!(untyped_rows(&conn), vec![("caller".to_string(), "len".to_string())], "`frob` was answered");
    let after: Vec<Vec<String>> = children.iter().map(|table| rows_of(&conn, table, "caller")).collect();
    assert_eq!(after, before, "every child row the structural tier stored survives the re-send");
    assert_eq!(rows_of(&conn, "nodes", "caller"), node_before, "and every column");

    assert_eq!(
        counters.received(),
        vec!["pub fn fresh()".to_string()],
        "only the node with no vector yet reaches the model"
    );
    assert_eq!(vector_of(&conn, "caller"), Some(vector_before), "the caller's vector is untouched");
    assert!(vector_of(&conn, "fresh").is_some(), "a genuinely new node is still embedded");
}

// --- GM-489: one caller row for a typed receiver call through an edit ---------

/// `src/lib.rs` holds the caller `f` and its callee `d`. Without
/// `pre_gm489`, the store is what the GM-489 bridge leaves after a pass that
/// confirmed the typed call `f -> d`: the structural edge `x` alone. With it,
/// it is what the bridge used to leave: `x` retracted and a semantic `e-sem`
/// onto the placeholder `p-sem` in its place.
fn store_after_a_confirming_pass(pre_gm489: bool) -> IndexStore {
    let mut raw = setup_conn();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let mut seed = FileChangeDiff {
        upsert_nodes: vec![node_in("f", "src/lib.rs"), node_in("d", "src/lib.rs")],
        ..Default::default()
    };
    if pre_gm489 {
        seed.upsert_nodes.push(node_in("p-sem", "src/lib.rs"));
        seed.upsert_edges.push(semantic_edge("e-sem", "f", "p-sem"));
    } else {
        seed.upsert_edges.push(unresolved_edge("x", "f", "d"));
    }
    apply_diff(&mut raw, &to_storage_diff(seed, &mut PathWarnings::default())).unwrap();
    crate::storage::write::upsert_indexed_file(&raw, "src/lib.rs", 1, "hash").unwrap();
    IndexStore::new(raw)
}

/// An edit of `src/lib.rs` that keeps the call: the reparse re-sends `f`,
/// `d` and the structural `x` under its unchanged id, completely. Then the
/// semantic pass answers `pass`, or - `None` - fails (the stub goes away).
fn edit_keeping_the_call(conn: &IndexStore, id: i64, pass: Option<FileChangeDiff>) {
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let request_id = RequestId::Number(id);
    let response = FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.to_string(),
        incomplete: false,
        incomplete_reason: None,
        id: request_id.clone(),
        result: FileChangeDiff {
            upsert_nodes: vec![node_in("f", "src/lib.rs"), node_in("d", "src/lib.rs")],
            upsert_edges: vec![unresolved_edge("x", "f", "d")],
            complete: true,
            ..Default::default()
        },
    };
    let plugin =
        spawn_stub_plugin(plugin_reader, plugin_writer, "src/lib.rs", request_id.clone(), response, pass);
    let root = project_root();
    apply_file_change(
        &mut BufReader::new(core_reader),
        &mut core_writer,
        conn,
        root.path(),
        "rust",
        "src/lib.rs",
        request_id,
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        true,
        &mut on_timeout_must_not_fire,
    )
    .expect("a lost semantic pass never fails the reparse");
    plugin.join().unwrap();
}

/// The `CALLS` rows from `f`: what `find_callers(d)` counts for this one
/// call.
fn calls_from_f(conn: &IndexStore) -> Vec<String> {
    ids(conn, "SELECT id FROM edges WHERE fromId = 'f' AND kind = 'CALLS' ORDER BY id")
}

/// **GM-489, T8.** With the stub answering in the shapes the GM-489 bridge
/// sends (R1/R2: the structural edge re-sent unchanged and no semantic edge
/// for a confirmed call), an edit followed by a pass that agrees or answers
/// empty (both re-send `x`), one that fails, and one that sends nothing for
/// the file each leaves exactly one `CALLS` row, `x`, still syntactic.
///
/// The second half is the arm the first is told apart from: the shape the
/// bridge sent before GM-489 (`e-sem` in place of a retracted `x`) leaves two
/// rows once an edit re-sends `x` and the pass then fails - the reported
/// duplicate. This stands in for the bridge, so it is not the reproduction;
/// the bridge's own tests (`plugins/sdk/tests/lsp_bridge.rs`, "GM-489") are
/// what show the bridge sends these shapes.
#[test]
fn a_typed_call_keeps_one_caller_row_through_an_edit_whatever_its_pass_does() {
    let conn = store_after_a_confirming_pass(false);
    let re_sent =
        || FileChangeDiff { upsert_edges: vec![unresolved_edge("x", "f", "d")], ..Default::default() };
    for (id, (what, pass)) in [
        ("agrees or answers empty", Some(re_sent())),
        ("fails", None),
        ("sends nothing for the file", Some(FileChangeDiff::default())),
    ]
    .into_iter()
    .enumerate()
    {
        edit_keeping_the_call(&conn, id as i64 + 1, pass);
        assert_eq!(calls_from_f(&conn), vec!["x"], "edit, then a pass that {what}");
        assert_eq!(edge_source_and_resolved(&conn, "x").0, "syntactic", "edit, then a pass that {what}");
    }

    let pre_gm489 = store_after_a_confirming_pass(true);
    edit_keeping_the_call(&pre_gm489, 1, None);
    assert_eq!(
        calls_from_f(&pre_gm489),
        vec!["e-sem", "x"],
        "the pre-GM-489 shape: the reparse's x beside the surviving semantic edge"
    );
}
