//! The `pending` provenance block (ADR 0009) on the four edge-walking tools,
//! through their real handlers.

use crate::mcp::query_shapes::QueryShapes;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::session_hints::SessionHints;
use super::{find_callers_callees, find_implementations, find_references};
use super::{FindImplementationsParams, SymbolQueryParams};
use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::MAX_RESPONSE_BYTES;
use crate::storage::connection::open_staging;
use crate::storage::index_store::IndexStore;
use crate::storage::language_swap::{self, SwapBookkeeping};
use crate::storage::schema;
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};

fn rust_with_a_semantic_tier() -> HashMap<String, Capabilities> {
    HashMap::from([(
        "rust".to_string(),
        Capabilities {
            semantic_pass: true,
            semantic_sweep: false,
            semantic_prepare: false,
            files_created: false,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
            member_overrides: crate::daemon::manifest::MemberOverrides::None,
        },
    )])
}

fn body(result: &CallToolResult) -> (serde_json::Value, usize) {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => (serde_json::from_str(&text.text).unwrap(), text.text.len()),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

fn node(id: &str, file: &str, signature: &str) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", id, format!("pkg::{id}"), file, "rust");
    node.signature = Some(signature.to_string());
    node
}

fn file_node(file: &str) -> NodeRecord {
    NodeRecord::new(format!("file-{file}"), "File", file, file, file, "rust")
}

/// One walk of the fixture: `a1` (in `p.rs`) and `c1` (in `c.rs`) call
/// `target`, so `c1` pages first by file path; `a_impl`/`c_impl` implement
/// `iface`. `a_signature` is `a1`'s.
fn walk(a_signature: &str) -> Diff {
    let mut upsert_nodes: Vec<NodeRecord> = ["p.rs", "c.rs", "t.rs"].iter().map(|f| file_node(f)).collect();
    upsert_nodes.extend([
        node("target", "t.rs", "fn target()"),
        node("iface", "t.rs", "trait Iface"),
        node("a1", "p.rs", a_signature),
        node("a_impl", "p.rs", "struct AImpl"),
        node("c1", "c.rs", "fn c()"),
        node("c_impl", "c.rs", "struct CImpl"),
    ]);
    Diff {
        upsert_nodes,
        upsert_edges: vec![
            EdgeRecord::new("e1-c", "c1", "target", "CALLS", "tree-sitter", true),
            EdgeRecord::new("e2-a", "a1", "target", "CALLS", "tree-sitter", true),
            EdgeRecord::new("s1-c", "c_impl", "iface", "SUPERTYPE_OF", "tree-sitter", true),
            EdgeRecord::new("s2-a", "a_impl", "iface", "SUPERTYPE_OF", "tree-sitter", true),
        ],
        ..Default::default()
    }
}

/// A live index after a workspace reindex of `rust` swapped in a walk where
/// only `a1`'s signature changed, before the semantic pass: `p.rs` pending.
fn after_a_swap() -> (tempfile::TempDir, Arc<IndexStore>) {
    let dir = tempfile::tempdir().unwrap();
    let live_path = dir.path().join("index.db");
    let staging_path = dir.path().join("staging-rust.db");
    let mut live = open_staging(&live_path).unwrap();
    live.execute(
        "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'x', 'x', 'x')",
        [],
    )
    .unwrap();
    apply_diff(&mut live, &walk("fn a()")).unwrap();
    schema::record_language_semantic_pass(&live, "rust").unwrap();

    let mut staging = open_staging(&staging_path).unwrap();
    apply_diff(&mut staging, &walk("fn a(x: u32)")).unwrap();
    language_swap::plan(&mut staging, live_path.to_str().unwrap(), "rust", "model", true).unwrap();
    drop(staging);
    let capable = HashSet::from(["rust".to_string()]);
    language_swap::swap(
        &mut live,
        &staging_path,
        None,
        &SwapBookkeeping { language: "rust", plugin_fingerprint: "fp", semantic_pass_languages: &capable },
    )
    .unwrap();
    (dir, Arc::new(IndexStore::new(live)))
}

fn by_id(id: &str) -> SymbolQueryParams {
    SymbolQueryParams { symbol_id: Some(id.to_string()), ..Default::default() }
}

fn callers(store: &Arc<IndexStore>, params: SymbolQueryParams) -> serde_json::Value {
    let result = find_callers_callees::handle_callers(
        store,
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        &rust_with_a_semantic_tier(),
        &SessionHints::default(),
        params,
    )
    .unwrap();
    body(&result).0
}

fn callees(store: &Arc<IndexStore>, id: &str) -> serde_json::Value {
    let result = find_callers_callees::handle_callees(
        store,
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        &rust_with_a_semantic_tier(),
        by_id(id),
    )
    .unwrap();
    body(&result).0
}

fn references(store: &Arc<IndexStore>, id: &str) -> serde_json::Value {
    let result = find_references::handle(
        store,
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        &rust_with_a_semantic_tier(),
        &SessionHints::default(),
        by_id(id),
    )
    .unwrap();
    body(&result).0
}

fn implementations(store: &Arc<IndexStore>, params: FindImplementationsParams) -> (serde_json::Value, usize) {
    let result = find_implementations::dispatch(
        store,
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        &rust_with_a_semantic_tier(),
        params,
    )
    .unwrap();
    body(&result)
}

fn implementations_of(store: &Arc<IndexStore>, id: &str) -> serde_json::Value {
    implementations(
        store,
        FindImplementationsParams { symbol_id: Some(id.to_string()), ..Default::default() },
    )
    .0
}

fn pending_files(body: &serde_json::Value) -> serde_json::Value {
    assert_eq!(body["provenance"]["semanticTier"], "pending", "{body}");
    body["provenance"]["pendingFiles"].clone()
}

/// Between the swap and the pass, each of the four tools names the pending
/// file a response touches. Controls: disclose with an empty touched set
/// (`p.rs` is not named where it is only a row's file); don't write the rows
/// in the swap (`semanticTier` reads `absent`).
#[test]
fn during_the_pass_each_tool_names_the_pending_files_it_touches() {
    let (_dir, store) = after_a_swap();

    assert_eq!(pending_files(&callers(&store, by_id("target"))), serde_json::json!(["p.rs"]));
    assert_eq!(pending_files(&references(&store, "target")), serde_json::json!(["p.rs"]));
    assert_eq!(pending_files(&implementations_of(&store, "iface")), serde_json::json!(["p.rs"]));
    assert_eq!(pending_files(&callees(&store, "a1")), serde_json::json!(["p.rs"]), "the anchor's file");
}

/// A response touching no pending file still says the language is pending,
/// with `since`, and no `pendingFiles` key.
#[test]
fn a_response_touching_no_pending_file_carries_the_language_level_fact() {
    let (_dir, store) = after_a_swap();

    let body = callees(&store, "c1");

    let provenance = body["provenance"].as_object().unwrap_or_else(|| panic!("{body}"));
    assert_eq!(provenance["language"], "rust");
    assert_eq!(provenance["semanticTier"], "pending");
    assert!(provenance["since"].as_str().is_some_and(|since| since.ends_with('Z')), "{body}");
    assert!(!provenance.contains_key("pendingFiles"), "{body}");
    assert!(!provenance.contains_key("pendingFilesOmitted"), "{body}");
}

/// `p.rs` only in the `files` tally, not in the one row of the page, is still
/// named. Control: leave the tally out of the touched set.
#[test]
fn a_pending_file_named_only_by_the_tally_is_disclosed() {
    let (_dir, store) = after_a_swap();

    let body = callers(&store, SymbolQueryParams { limit: Some(1), ..by_id("target") });

    assert_eq!(body["results"][0]["filePath"], "c.rs", "{body}");
    assert!(
        body["files"].as_array().is_some_and(|files| files.iter().any(|f| f["path"] == "p.rs")),
        "{body}"
    );
    assert_eq!(pending_files(&body), serde_json::json!(["p.rs"]));
}

/// Once the pass completes, the block is gone entirely.
#[test]
fn after_the_pass_no_provenance_key_remains() {
    let (_dir, store) = after_a_swap();
    store.with(|conn| schema::record_language_semantic_pass(conn, "rust")).unwrap();

    for body in [
        callers(&store, by_id("target")),
        callees(&store, "a1"),
        references(&store, "target"),
        implementations_of(&store, "iface"),
    ] {
        assert!(body.get("provenance").is_none(), "{body}");
    }
}

/// An in-memory index with `rust` pending since a fixed time and `files`
/// pending.
fn pending_index(diff: &Diff, files: &[String]) -> Arc<IndexStore> {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    apply_diff(&mut conn, diff).unwrap();
    conn.execute(
        "INSERT INTO semantic_pending (language, since) VALUES ('rust', '2026-09-26T10:14:03Z')",
        [],
    )
    .unwrap();
    for file in files {
        conn.execute("INSERT INTO semantic_pending_files (language, filePath) VALUES ('rust', ?1)", [file])
            .unwrap();
    }
    Arc::new(IndexStore::new(conn))
}

/// `iface` in `src/anchor.rs` with `count` implementors whose qualified names
/// are `name_bytes` long, implementor `i` in `file(i)`.
fn many_implementors(count: usize, name_bytes: usize, file: impl Fn(usize) -> String) -> Diff {
    let mut diff =
        Diff { upsert_nodes: vec![node("iface", "src/anchor.rs", "trait Iface")], ..Default::default() };
    for i in 0..count {
        let id = format!("impl{i:03}");
        let mut implementor = node(&id, &file(i), "struct Impl");
        implementor.qualified_name = format!("{id}_{}", "x".repeat(name_bytes));
        diff.upsert_nodes.push(implementor);
        diff.upsert_edges.push(EdgeRecord::new(
            format!("s{i:03}"),
            id,
            "iface",
            "SUPERTYPE_OF",
            "tree-sitter",
            true,
        ));
    }
    diff
}

/// Thirty pending files in a page cut at the byte budget: 25 listed, the
/// other 5 counted, and the whole response inside `MAX_RESPONSE_BYTES`. One
/// implementor per file; the pending ones sort first by path, so all thirty
/// are on the page. Controls: measure candidate pages without their
/// `provenance` block (single-hop `find_implementations::handle_in`) -> over
/// budget; drop the entry cap -> 30 listed.
#[test]
fn the_pending_list_is_capped_and_the_page_keeps_its_budget() {
    let file = |i: usize| format!("src/pending/module_{i:02}/implementation.rs");
    let files: Vec<String> = (0..30).map(file).collect();
    let store = pending_index(&many_implementors(60, 430, file), &files);

    let (body, bytes) = implementations(
        &store,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            limit: Some(200),
            ..Default::default()
        },
    );

    assert_eq!(body["hasMore"], true, "rows reach the budget: {body}");
    assert!(body["results"].as_array().unwrap().len() >= 30, "every file is on the page: {body}");
    assert_eq!(body["provenance"]["pendingFiles"].as_array().unwrap().len(), 25, "{body}");
    assert_eq!(body["provenance"]["pendingFilesOmitted"], 5, "{body}");
    assert!(bytes <= MAX_RESPONSE_BYTES, "{bytes} bytes");
}

/// A resumed transitive page resolves no anchor, and still names a pending
/// file among its rows. Control: keep resumed pages silent.
#[test]
fn a_resumed_transitive_page_names_its_pending_files() {
    let store = pending_index(
        &many_implementors(120, 400, |i| if i < 20 { "c.rs".to_string() } else { "a.rs".to_string() }),
        &["a.rs".to_string()],
    );

    let (first, _) = implementations(
        &store,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            transitive: Some(true),
            ..Default::default()
        },
    );
    let token =
        first["resumeToken"].as_str().unwrap_or_else(|| panic!("the walk is cut: {first}")).to_string();
    let (resumed, _) = implementations(
        &store,
        FindImplementationsParams { resume_token: Some(token), ..Default::default() },
    );

    assert!(resumed.get("anchor").is_none(), "{resumed}");
    let rows = resumed["results"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["filePath"] == "a.rs"), "the resumed rows reach a.rs: {resumed}");
    assert_eq!(pending_files(&resumed), serde_json::json!(["a.rs"]));
}

/// A resumed page whose pending files cannot be read stays silent rather
/// than falling back to `absent`. Control: drop `continued`'s tier filter.
#[test]
fn a_resumed_transitive_page_stays_silent_when_pending_files_fail() {
    let store = pending_index(
        &many_implementors(120, 400, |i| if i < 20 { "c.rs".to_string() } else { "a.rs".to_string() }),
        &["a.rs".to_string()],
    );

    let (first, _) = implementations(
        &store,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            transitive: Some(true),
            ..Default::default()
        },
    );
    let token =
        first["resumeToken"].as_str().unwrap_or_else(|| panic!("the walk is cut: {first}")).to_string();
    store.with(|conn| conn.execute("DROP TABLE semantic_pending_files", [])).unwrap();
    let (resumed, _) = implementations(
        &store,
        FindImplementationsParams { resume_token: Some(token), ..Default::default() },
    );

    let rows = resumed["results"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["filePath"] == "a.rs"), "the resumed rows reach a.rs: {resumed}");
    assert!(resumed.get("provenance").is_none(), "{resumed}");
}

/// A resumed page of a language that is `absent`, not pending, stays silent:
/// the fresh page already said so.
#[test]
fn a_resumed_transitive_page_never_says_absent() {
    let store = pending_index(&many_implementors(120, 400, |_| "a.rs".to_string()), &[]);
    store.with(|conn| conn.execute("DELETE FROM semantic_pending", [])).unwrap();

    let (first, _) = implementations(
        &store,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            transitive: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(first["provenance"]["semanticTier"], "absent", "{first}");
    let token = first["resumeToken"].as_str().unwrap().to_string();
    let (resumed, _) = implementations(
        &store,
        FindImplementationsParams { resume_token: Some(token), ..Default::default() },
    );

    assert!(resumed.get("provenance").is_none(), "{resumed}");
}

// ---------------------------------------------------------------------------
// GM-330/S3 (ADR 0022): the statements moved out of the instructions ride on
// the answers as once-per-session hints.
// ---------------------------------------------------------------------------

use super::find_definition;
use super::session_hints::{ALL_UNRESOLVED, PROVENANCE, UNRESOLVED_ROW};

/// Rust nodes with a declared semantic tier that has never run (so every
/// page carries `provenance: absent`), and one cross-file edge per tool the
/// linker could not confirm beside a confirmed one:
/// - callers/references of `target`: `c1` (resolved), `a1` (unresolved);
/// - callees of `a1`: `target` (unresolved), `helper` (resolved);
/// - implementations of `iface`: `c_impl` (resolved), `a_impl` (unresolved);
/// - callers of `lonely`: `a1` only, unresolved (an `allUnresolved` page).
fn hint_index() -> Arc<IndexStore> {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let mut upsert_nodes: Vec<NodeRecord> = ["a.rs", "c.rs", "t.rs"].iter().map(|f| file_node(f)).collect();
    upsert_nodes.extend([
        node("target", "t.rs", "fn target()"),
        node("helper", "t.rs", "fn helper()"),
        node("lonely", "t.rs", "fn lonely()"),
        node("iface", "t.rs", "trait Iface"),
        node("a1", "a.rs", "fn a()"),
        node("a_impl", "a.rs", "struct AImpl"),
        node("c1", "c.rs", "fn c()"),
        node("c_impl", "c.rs", "struct CImpl"),
    ]);
    let diff = Diff {
        upsert_nodes,
        upsert_edges: vec![
            EdgeRecord::new("e1", "c1", "target", "CALLS", "tree-sitter", true),
            EdgeRecord::new("e2", "a1", "target", "CALLS", "tree-sitter", false),
            EdgeRecord::new("e3", "a1", "helper", "CALLS", "tree-sitter", true),
            EdgeRecord::new("e4", "a1", "lonely", "CALLS", "tree-sitter", false),
            EdgeRecord::new("s1", "c_impl", "iface", "SUPERTYPE_OF", "tree-sitter", true),
            EdgeRecord::new("s2", "a_impl", "iface", "SUPERTYPE_OF", "tree-sitter", false),
        ],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();
    Arc::new(IndexStore::new(conn))
}

/// The four tools through the handlers the server calls, all on one
/// session's `hints`.
#[derive(Clone, Copy, Debug)]
enum Tool {
    Callers,
    References,
    Callees,
    Implementations,
}

const TOOLS: [Tool; 4] = [Tool::Callers, Tool::References, Tool::Callees, Tool::Implementations];

fn ask(store: &Arc<IndexStore>, hints: &SessionHints, tool: Tool, id: &str) -> serde_json::Value {
    let capabilities = rust_with_a_semantic_tier();
    let embedding = EmbeddingPipeline::disabled();
    let shapes = QueryShapes::shipped();
    let result = match tool {
        Tool::Callers => {
            find_callers_callees::handle_callers(store, &embedding, shapes, &capabilities, hints, by_id(id))
        }
        Tool::References => {
            find_references::handle(store, &embedding, shapes, &capabilities, hints, by_id(id))
        }
        Tool::Callees => find_definition::resolve_lazily(&embedding, shapes, |semantic| {
            find_callers_callees::handle_callees_in(store, semantic, &capabilities, hints, by_id(id))
        }),
        Tool::Implementations => find_definition::resolve_lazily(&embedding, shapes, |semantic| {
            find_implementations::dispatch_in(
                store,
                semantic,
                &capabilities,
                hints,
                FindImplementationsParams { symbol_id: Some(id.to_string()), ..Default::default() },
            )
        }),
    }
    .unwrap();
    body(&result).0
}

/// The anchor each tool is asked about in [`hint_index`] to get one
/// confirmed and one unconfirmed row.
fn mixed_anchor(tool: Tool) -> &'static str {
    match tool {
        Tool::Callers | Tool::References => "target",
        Tool::Callees => "a1",
        Tool::Implementations => "iface",
    }
}

fn hint_of(body: &serde_json::Value) -> String {
    body.get("hint").and_then(|hint| hint.as_str()).unwrap_or_default().to_string()
}

/// On each of the four tools, a page with one `resolved: false` row (and
/// not `allUnresolved`) and a `provenance` block carries both moved
/// sentences the first time, and neither on the same session's second
/// call. Controls: pass a fresh `SessionHints::default()` inside a handler
/// (the second call repeats); set the `UnresolvedRow` or `SemanticTier`
/// trigger to `false` (the first call lacks it).
#[test]
fn each_tool_sends_the_moved_sentences_once_per_session() {
    let store = hint_index();
    for tool in TOOLS {
        let hints = SessionHints::default();
        let first = ask(&store, &hints, tool, mixed_anchor(tool));
        assert_eq!(first["allUnresolved"], false, "{tool:?}: {first}");
        assert_eq!(first["provenance"]["semanticTier"], "absent", "{tool:?}: {first}");
        let rows = first["results"].as_array().unwrap();
        assert!(rows.iter().any(|row| row["resolved"] == false), "{tool:?}: {first}");
        assert!(rows.iter().any(|row| row["resolved"] == true), "{tool:?}: {first}");
        let hint = hint_of(&first);
        assert!(hint.contains(UNRESOLVED_ROW), "{tool:?}: {first}");
        assert!(hint.contains(PROVENANCE), "{tool:?}: {first}");

        let second = ask(&store, &hints, tool, mixed_anchor(tool));
        assert_eq!(second["provenance"]["semanticTier"], "absent", "{tool:?}: {second}");
        let hint = hint_of(&second);
        assert!(!hint.contains(UNRESOLVED_ROW), "{tool:?}: sent once: {second}");
        assert!(!hint.contains(PROVENANCE), "{tool:?}: sent once: {second}");
    }
}

/// Once per session, not once per tool: after `find_callers` sent both
/// sentences, the other three tools on the same session send neither.
/// Control: give each tool its own `HintKey` (or its own `SessionHints`).
#[test]
fn the_moved_sentences_are_sent_once_across_the_four_tools() {
    let store = hint_index();
    let hints = SessionHints::default();
    let first = ask(&store, &hints, Tool::Callers, "target");
    assert!(hint_of(&first).contains(UNRESOLVED_ROW) && hint_of(&first).contains(PROVENANCE), "{first}");
    for tool in [Tool::References, Tool::Callees, Tool::Implementations] {
        let body = ask(&store, &hints, tool, mixed_anchor(tool));
        let hint = hint_of(&body);
        assert!(!hint.contains(UNRESOLVED_ROW) && !hint.contains(PROVENANCE), "{tool:?}: {body}");
    }
}

/// An `allUnresolved` page carries `ALL_UNRESOLVED`, never the row sentence,
/// and does not spend it: the next mixed page on the session still gets it.
/// Controls: drop `!all_unresolved &&` from the trigger (both sentences on
/// the first page); record the key even when the trigger is false.
#[test]
fn an_all_unresolved_page_carries_its_own_sentence_and_does_not_spend_the_row_hint() {
    let store = hint_index();
    let hints = SessionHints::default();
    let lonely = ask(&store, &hints, Tool::Callers, "lonely");
    assert_eq!(lonely["allUnresolved"], true, "{lonely}");
    let hint = hint_of(&lonely);
    assert!(hint.contains(ALL_UNRESOLVED), "{lonely}");
    assert!(!hint.contains(UNRESOLVED_ROW), "{lonely}");

    let mixed = ask(&store, &hints, Tool::Callers, "target");
    assert!(hint_of(&mixed).contains(UNRESOLVED_ROW), "{mixed}");
}

/// No `provenance` (the pass has run), no provenance sentence, on any of the
/// four tools - and the key is not spent. Control: trigger the
/// `SemanticTier` hint unconditionally.
#[test]
fn no_provenance_means_no_provenance_sentence() {
    let store = hint_index();
    store.with(|conn| schema::record_language_semantic_pass(conn, "rust")).unwrap();
    let hints = SessionHints::default();
    for tool in TOOLS {
        let body = ask(&store, &hints, tool, mixed_anchor(tool));
        assert!(body.get("provenance").is_none(), "{tool:?}: {body}");
        assert!(!hint_of(&body).contains(PROVENANCE), "{tool:?}: {body}");
    }
    store.with(|conn| conn.execute("DELETE FROM language_state", [])).unwrap();
    let body = ask(&store, &hints, Tool::Callers, "target");
    assert!(hint_of(&body).contains(PROVENANCE), "the key was not spent: {body}");
}

fn transitive(
    store: &Arc<IndexStore>,
    hints: &SessionHints,
    params: FindImplementationsParams,
) -> serde_json::Value {
    let capabilities = rust_with_a_semantic_tier();
    let embedding = EmbeddingPipeline::disabled();
    let result = find_definition::resolve_lazily(&embedding, QueryShapes::shipped(), |semantic| {
        find_implementations::dispatch_in(store, semantic, &capabilities, hints, params.clone())
    })
    .unwrap();
    body(&result).0
}

/// A transitive walk (`from_root`) and its resumed page (`continued`) carry
/// the provenance sentence once per session too: the fresh walk sends it,
/// its continuation on the same session does not, and a continuation on a
/// session that has not seen it does. Controls: drop the `append` in
/// `from_root` or in `continued`; use a fresh `SessionHints` there.
#[test]
fn transitive_walks_send_the_provenance_sentence_once_per_session() {
    let store = pending_index(
        &many_implementors(120, 400, |i| if i < 20 { "c.rs".to_string() } else { "a.rs".to_string() }),
        &["a.rs".to_string()],
    );
    let hints = SessionHints::default();
    let first = transitive(
        &store,
        &hints,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            transitive: Some(true),
            ..Default::default()
        },
    );
    assert!(first["provenance"].is_object(), "{first}");
    assert!(hint_of(&first).contains(PROVENANCE), "the fresh walk sends it: {first}");
    let token =
        first["resumeToken"].as_str().unwrap_or_else(|| panic!("the walk is cut: {first}")).to_string();

    let resume = || FindImplementationsParams { resume_token: Some(token.clone()), ..Default::default() };
    let same_session = transitive(&store, &hints, resume());
    assert_eq!(same_session["provenance"]["semanticTier"], "pending", "{same_session}");
    assert!(!hint_of(&same_session).contains(PROVENANCE), "already sent: {same_session}");

    let other_session = transitive(&store, &SessionHints::default(), resume());
    assert!(hint_of(&other_session).contains(PROVENANCE), "a new session gets it: {other_session}");
}

/// Candidate pages are measured with the provenance sentence in their
/// `hint`: the whole response stays within `MAX_RESPONSE_BYTES`. With rows
/// shorter than that field, a cut that ignored it would stop less than one
/// row short of the ceiling, so adding the field would go over. With `provenance: absent` there is no pending
/// list. Control: measure candidate pages with no `hint` (single-hop
/// `find_implementations::handle_in`).
#[test]
fn a_page_at_the_budget_edge_keeps_room_for_the_provenance_sentence() {
    let store = pending_index(&many_implementors(300, 4, |i| format!("m{i:03}.rs")), &[]);
    store.with(|conn| conn.execute("DELETE FROM semantic_pending", [])).unwrap();

    let (body, bytes) = implementations(
        &store,
        FindImplementationsParams {
            symbol_id: Some("iface".to_string()),
            limit: Some(200),
            ..Default::default()
        },
    );

    assert_eq!(body["provenance"]["semanticTier"], "absent", "{body}");
    assert_eq!(body["hasMore"], true, "rows reach the budget: {body}");
    let rows = body["results"].as_array().unwrap();
    assert!(rows.len() < 200, "the byte budget, not the limit, cut the page: {} rows", rows.len());
    let longest_row = rows.iter().map(|row| row.to_string().len() + 1).max().unwrap();
    assert_eq!(hint_of(&body), PROVENANCE, "{body}");
    let hint_field = r#","hint":"#.len() + body["hint"].to_string().len();
    assert!(
        longest_row < hint_field,
        "fixture: rows ({longest_row} bytes) shorter than the hint ({hint_field})"
    );
    assert!(bytes <= MAX_RESPONSE_BYTES, "{bytes} bytes, hint {hint_field}, over {MAX_RESPONSE_BYTES}");
}

// ---------------------------------------------------------------------------
// GM-330/S14: the session's `SessionHints` is the server's own (`self.hints`),
// reached through the same handler an MCP `tools/call` uses.
// ---------------------------------------------------------------------------

/// A server over `store` (no plugins discovered, index ready), so each call
/// below goes through `GMeshMcpServer`'s tool handler over a real session.
fn hint_server(dir: &std::path::Path, store: Arc<IndexStore>) -> super::GMeshMcpServer {
    use crate::daemon::indexing_status::{IndexingStatus, Phase};
    use crate::daemon::lifecycle::CoreActivity;
    use crate::daemon::manifest::DiscoveredPlugins;
    use crate::daemon::registry::PluginRegistry;
    let root = dir.join("project");
    let state = dir.join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let indexing = IndexingStatus::structural();
    indexing.set_phase(Phase::Ready);
    let registry = Arc::new(PluginRegistry::new(
        &root,
        state,
        DiscoveredPlugins::default(),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    ));
    super::GMeshMcpServer::new(
        store,
        registry,
        CoreActivity::new(),
        indexing,
        Arc::new(EmbeddingPipeline::disabled()),
    )
}

/// One MCP call on `client`'s session, as its JSON body.
async fn served(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    tool: &str,
    id: &str,
) -> serde_json::Value {
    let result =
        super::search_code_worker_tests::call(client, tool, serde_json::json!({ "symbol_id": id })).await;
    body(&result).0
}

/// Through the server, `find_callees` and `find_implementations` each send
/// the `resolved: false` sentence on the session's first qualifying page
/// only. Each tool gets its own server (its own session), so the first
/// call is the one that must carry it. Controls: in `GMeshMcpServer::find_callees`
/// (resp. `find_implementations`), pass `SessionHints::default()` instead
/// of `self.hints.clone()` - that tool's second call repeats the sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_sends_the_row_hint_once_per_session_on_callees_and_implementations() {
    for (tool, anchor) in [("find_callees", "a1"), ("find_implementations", "iface")] {
        let dir = tempfile::tempdir().unwrap();
        let client = super::search_code_worker_tests::connect(hint_server(dir.path(), hint_index())).await;

        let first = served(&client, tool, anchor).await;
        let rows = first["results"].as_array().unwrap_or_else(|| panic!("{tool}: {first}"));
        assert!(rows.iter().any(|row| row["resolved"] == false), "{tool}: {first}");
        assert_eq!(first["allUnresolved"], false, "{tool}: {first}");
        assert!(hint_of(&first).contains(UNRESOLVED_ROW), "{tool}: the first page sends it: {first}");

        let second = served(&client, tool, anchor).await;
        assert!(second["results"].as_array().unwrap().iter().any(|row| row["resolved"] == false), "{second}");
        assert!(!hint_of(&second).contains(UNRESOLVED_ROW), "{tool}: sent once per session: {second}");
    }
}

/// Once per session across tools, through the server: after `find_callers`
/// spent the sentence, `find_callees` and `find_implementations` on the
/// same session do not repeat it, while a new session's `find_callees`
/// does. Controls: the `SessionHints::default()` swap above in either
/// handler (that tool sends it again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_hint_spent_by_find_callers_is_not_repeated_by_the_other_tools() {
    let dir = tempfile::tempdir().unwrap();
    let client = super::search_code_worker_tests::connect(hint_server(dir.path(), hint_index())).await;

    let callers = served(&client, "find_callers", "target").await;
    assert!(hint_of(&callers).contains(UNRESOLVED_ROW), "{callers}");
    for (tool, anchor) in [("find_callees", "a1"), ("find_implementations", "iface")] {
        let page = served(&client, tool, anchor).await;
        assert!(
            page["results"].as_array().unwrap().iter().any(|row| row["resolved"] == false),
            "{tool}: {page}"
        );
        assert!(!hint_of(&page).contains(UNRESOLVED_ROW), "{tool}: already sent: {page}");
    }

    let other_dir = tempfile::tempdir().unwrap();
    let other = super::search_code_worker_tests::connect(hint_server(other_dir.path(), hint_index())).await;
    let callees = served(&other, "find_callees", "a1").await;
    assert!(hint_of(&callees).contains(UNRESOLVED_ROW), "a new session gets it: {callees}");
}
