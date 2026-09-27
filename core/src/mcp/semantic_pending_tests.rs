//! The `pending` provenance block (ADR 0009) on the four edge-walking tools,
//! through their real handlers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

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
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
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

/// One walk of the fixture: `a1`/`c1` call `target` (edges `e1-c`, `e2-a`,
/// so `c1` pages first), `a_impl`/`c_impl` implement `iface`. `a_signature`
/// is `a1`'s.
fn walk(a_signature: &str) -> Diff {
    let mut upsert_nodes: Vec<NodeRecord> = ["a.rs", "c.rs", "t.rs"].iter().map(|f| file_node(f)).collect();
    upsert_nodes.extend([
        node("target", "t.rs", "fn target()"),
        node("iface", "t.rs", "trait Iface"),
        node("a1", "a.rs", a_signature),
        node("a_impl", "a.rs", "struct AImpl"),
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
/// only `a1`'s signature changed, before the semantic pass: `a.rs` pending.
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
    language_swap::plan(&mut staging, live_path.to_str().unwrap(), "rust", "model").unwrap();
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
        &rust_with_a_semantic_tier(),
        params,
    )
    .unwrap();
    body(&result).0
}

fn callees(store: &Arc<IndexStore>, id: &str) -> serde_json::Value {
    let result = find_callers_callees::handle_callees(
        store,
        &EmbeddingPipeline::disabled(),
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
        &rust_with_a_semantic_tier(),
        by_id(id),
    )
    .unwrap();
    body(&result).0
}

fn implementations(store: &Arc<IndexStore>, params: FindImplementationsParams) -> (serde_json::Value, usize) {
    let result = find_implementations::dispatch(
        store,
        &EmbeddingPipeline::disabled(),
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
/// (`a.rs` is not named where it is only a row's file); don't write the rows
/// in the swap (`semanticTier` reads `absent`).
#[test]
fn during_the_pass_each_tool_names_the_pending_files_it_touches() {
    let (_dir, store) = after_a_swap();

    assert_eq!(pending_files(&callers(&store, by_id("target"))), serde_json::json!(["a.rs"]));
    assert_eq!(pending_files(&references(&store, "target")), serde_json::json!(["a.rs"]));
    assert_eq!(pending_files(&implementations_of(&store, "iface")), serde_json::json!(["a.rs"]));
    assert_eq!(pending_files(&callees(&store, "a1")), serde_json::json!(["a.rs"]), "the anchor's file");
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

/// `a.rs` only in the `files` tally, not in the one row of the page, is still
/// named. Control: leave the tally out of the touched set.
#[test]
fn a_pending_file_named_only_by_the_tally_is_disclosed() {
    let (_dir, store) = after_a_swap();

    let body = callers(&store, SymbolQueryParams { limit: Some(1), ..by_id("target") });

    assert_eq!(body["results"][0]["filePath"], "c.rs", "{body}");
    assert!(
        body["files"].as_array().is_some_and(|files| files.iter().any(|f| f["path"] == "a.rs")),
        "{body}"
    );
    assert_eq!(pending_files(&body), serde_json::json!(["a.rs"]));
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
/// other 5 counted, and the whole response inside `MAX_RESPONSE_BYTES`.
/// Controls: reserve nothing for a pending block (`page_reserve` returns 0)
/// -> over budget; drop the cap -> 30 listed.
#[test]
fn the_pending_list_is_capped_and_the_page_keeps_its_budget() {
    let file = |i: usize| format!("src/pending/module_{:02}/implementation.rs", i % 30);
    let files: Vec<String> = (0..30).map(file).collect();
    let store = pending_index(&many_implementors(60, 440, file), &files);

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
