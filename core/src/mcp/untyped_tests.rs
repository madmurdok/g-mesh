//! `untypedReceiverCalls`: the probe against a hand-built index,
//! then both handlers. The end-to-end cases through the rust plugin sit
//! beside `unlinkedUsages`' in `unlinked_tests.rs`, which owns that fixture.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::*;
use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::MAX_RESPONSE_BYTES;
use crate::graph::queries;
use crate::mcp::query_shapes::QueryShapes;
use crate::mcp::session_hints::SessionHints;
use crate::mcp::{find_callers_callees, find_references, SymbolQueryParams};
use crate::protocol::types::{PathSegment, QualifiedPath};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::{apply_diff, Diff, EdgeRecord};

/// `unlinked::MAX_UNLINKED_FILE_TALLY`, private to that module; the design
/// doc's cap of 20 (§3).
const FILE_CAP: usize = 20;

/// `find_references`' `USAGE_EDGE_KINDS`, private to that module.
const USAGE_KINDS: &[&str] = &["CALLS", "REFERENCES", "SUPERTYPE_OF"];

fn path(segments: &[&str]) -> QualifiedPath {
    QualifiedPath(
        segments
            .iter()
            .enumerate()
            .map(|(i, name)| PathSegment { sep: (i > 0).then(|| "::".to_string()), name: name.to_string() })
            .collect(),
    )
}

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn write(conn: &mut Connection, diff: Diff) {
    apply_diff(conn, &diff).unwrap();
}

/// A `kind` node at `segments` in `t.rs`, of `native_kind`, read back.
fn anchor(conn: &mut Connection, kind: &str, native_kind: Option<&str>, segments: &[&str]) -> NodeRecord {
    let id = segments.join("::");
    let mut node = NodeRecord::new(&id, kind, *segments.last().unwrap(), &id, "t.rs", "rust");
    node.native_kind = native_kind.map(str::to_string);
    node.qualified_path = Some(path(segments));
    write(conn, Diff { upsert_nodes: vec![node], ..Default::default() });
    queries::get_node(conn, &id).unwrap().unwrap()
}

/// The method `m::P::m` (`nativeKind` `method`).
fn method(conn: &mut Connection) -> NodeRecord {
    anchor(conn, "Function", Some("method"), &["m", "P", "m"])
}

/// A function `id` in `file`, of `language`, calling `names` through
/// untyped receivers.
fn caller_in(conn: &mut Connection, id: &str, file: &str, language: &str, names: &[&str]) {
    let mut node = NodeRecord::new(id, "Function", id, format!("c::{id}"), file, language);
    node.untyped_calls = names.iter().map(|name| name.to_string()).collect();
    write(conn, Diff { upsert_nodes: vec![node], ..Default::default() });
}

fn caller(conn: &mut Connection, id: &str, file: &str, names: &[&str]) {
    caller_in(conn, id, file, "rust", names);
}

/// A `kind` edge `from` -> `to` of tier `source` (`syntactic`/`semantic`).
fn edge(conn: &mut Connection, from: &str, to: &str, kind: &str, source: &str) {
    let id = format!("{from}-{kind}-{source}-{to}");
    write(
        conn,
        Diff { upsert_edges: vec![EdgeRecord::new(id, from, to, kind, source, true)], ..Default::default() },
    );
}

fn count(found: Option<UntypedReceiverCalls>) -> usize {
    found.map_or(0, |found| found.count)
}

fn files(found: &UntypedReceiverCalls) -> Vec<(String, i64)> {
    found.files.iter().map(|tally| (tally.path.clone(), tally.refs)).collect()
}

// --- which anchors count (`is_method`) --------------------------------------

/// Only a `Function` qualifies, whatever its `nativeKind` says. Control: drop
/// the `anchor.kind != "Function"` check (the `Variable` gets the field).
#[test]
fn a_non_function_anchor_gets_no_field() {
    let mut conn = setup();
    let variable = anchor(&mut conn, "Variable", Some("method"), &["m", "P", "m"]);
    caller(&mut conn, "c1", "a.rs", &["m"]);

    assert!(probe(&conn, &variable, &["CALLS"], &[]).is_none());
}

/// The three Rust member kinds qualify on their `nativeKind` alone: a
/// trait-impl method's parent `<S as Tr>` names no node, so the type-member
/// fallback could not find it. Control: drop any one kind from
/// `METHOD_NATIVE_KINDS` (its anchor gets no field).
#[test]
fn every_rust_member_kind_qualifies_on_its_native_kind() {
    for native_kind in ["method", "trait_method", "trait_impl_method"] {
        let mut conn = setup();
        let member = anchor(&mut conn, "Function", Some(native_kind), &["m", "<S as Tr>", "m"]);
        caller(&mut conn, "c1", "a.rs", &["m"]);

        assert_eq!(count(probe(&conn, &member, &["CALLS"], &[])), 1, "{native_kind}");
    }
}

/// Any other `nativeKind` qualifies only as a member of a `Type` node of its
/// own language (`unlinked::is_type_member`). Control: drop that fallback
/// from `is_method` (the member gets no field).
#[test]
fn another_native_kind_qualifies_only_as_a_member_of_a_type() {
    let mut conn = setup();
    let member = anchor(&mut conn, "Function", Some("function"), &["m", "P", "m"]);
    caller(&mut conn, "c1", "a.rs", &["m"]);
    assert!(probe(&conn, &member, &["CALLS"], &[]).is_none(), "no `m::P` Type node yet");

    let mut python_type = NodeRecord::new("py-P", "Type", "P", "m::P", "t.py", "python");
    python_type.qualified_path = Some(path(&["m", "P"]));
    write(&mut conn, Diff { upsert_nodes: vec![python_type], ..Default::default() });
    assert!(probe(&conn, &member, &["CALLS"], &[]).is_none(), "a Type of another language is not its parent");

    anchor(&mut conn, "Type", None, &["m", "P"]);
    assert_eq!(count(probe(&conn, &member, &["CALLS"], &[])), 1);
}

/// A free function never qualifies: `x.f()` cannot reach it. Control: make
/// `is_method` return true for every `Function` (the free fn gets the
/// field).
#[test]
fn a_free_function_never_qualifies() {
    let mut conn = setup();
    anchor(&mut conn, "Module", None, &["k"]);
    let free = anchor(&mut conn, "Function", Some("function"), &["k", "m"]);
    let bare = anchor(&mut conn, "Function", None, &["m"]);
    caller(&mut conn, "c1", "a.rs", &["m"]);

    assert!(probe(&conn, &free, &["CALLS"], &[]).is_none());
    assert!(probe(&conn, &bare, &["CALLS"], &[]).is_none(), "a one-segment path");
}

// --- which rows count (`candidate_sql`) -------------------------------------

/// A row counts only under the anchor's own name and language. Controls:
/// drop `u.name = ?1` (`other` counts); drop `f.language = ?2` (the python
/// caller counts).
#[test]
fn only_rows_of_the_anchors_name_and_language_count() {
    let mut conn = setup();
    let anchor = method(&mut conn);
    caller(&mut conn, "named", "a.rs", &["m"]);
    caller(&mut conn, "other", "b.rs", &["n"]);
    caller_in(&mut conn, "python", "c.py", "python", &["m"]);

    let found = probe(&conn, &anchor, &["CALLS"], &[]).expect("a candidate");
    assert_eq!(found.count, 1);
    assert_eq!(files(&found), [("a.rs".to_string(), 1)]);
}

/// A caller with an edge of the page's kinds to the anchor is already a row,
/// whatever tier wrote the edge, and is not counted again; an edge of a kind
/// the page does not walk does not hide it. Control: drop the first
/// `NOT EXISTS` (all three count on the callers page).
#[test]
fn a_caller_already_on_the_page_is_not_counted() {
    let mut conn = setup();
    let anchor = method(&mut conn);
    for id in ["syntactic", "semantic", "referencing", "open"] {
        caller(&mut conn, id, "a.rs", &["m"]);
    }
    edge(&mut conn, "syntactic", &anchor.id, "CALLS", "syntactic");
    edge(&mut conn, "semantic", &anchor.id, "CALLS", "semantic");
    edge(&mut conn, "referencing", &anchor.id, "REFERENCES", "syntactic");

    assert_eq!(count(probe(&conn, &anchor, &["CALLS"], &[])), 2, "`referencing` and `open`");
    assert_eq!(count(probe(&conn, &anchor, USAGE_KINDS, &[])), 1, "`open` alone on the references page");
}

/// A row drops once the semantic tier answered that call - the caller
/// has a `semantic` edge to some node of the row's name - whether the answer
/// was this anchor (here through `REFERENCES`, a kind the callers page does
/// not walk, so the edge filter alone would keep it) or another method of
/// that name. A syntactic edge to a namesake, or a semantic edge to a node of
/// another name, answers nothing. Controls: drop the second `NOT EXISTS`
/// (`to_anchor` and `elsewhere` count); drop `e.source = 'semantic'`
/// (`guessed` drops too); drop `t.name = u.name` (`unrelated` drops too).
#[test]
fn a_call_the_semantic_tier_answered_drops_wherever_it_landed() {
    let mut conn = setup();
    let anchor = method(&mut conn);
    let namesake = self::anchor(&mut conn, "Function", Some("method"), &["m", "Q", "m"]);
    let unrelated_target = self::anchor(&mut conn, "Function", Some("method"), &["m", "Q", "n"]);
    for (id, file) in
        [("to_anchor", "a.rs"), ("elsewhere", "b.rs"), ("guessed", "c.rs"), ("unrelated", "d.rs")]
    {
        caller(&mut conn, id, file, &["m"]);
    }
    edge(&mut conn, "to_anchor", &anchor.id, "REFERENCES", "semantic");
    edge(&mut conn, "elsewhere", &namesake.id, "CALLS", "semantic");
    edge(&mut conn, "guessed", &namesake.id, "CALLS", "syntactic");
    edge(&mut conn, "unrelated", &unrelated_target.id, "CALLS", "semantic");

    let found = probe(&conn, &anchor, &["CALLS"], &[]).expect("the unanswered calls");
    assert_eq!(files(&found), [("c.rs".to_string(), 1), ("d.rs".to_string(), 1)]);
    assert_eq!(found.count, 2);
}

/// `file_paths` scopes by the caller's file. Control: drop `scope_filter`
/// (`b.rs`'s caller counts in the `a.rs` scope).
#[test]
fn the_file_scope_narrows_by_the_callers_file() {
    let mut conn = setup();
    let anchor = method(&mut conn);
    caller(&mut conn, "in_a", "a.rs", &["m"]);
    caller(&mut conn, "in_b", "b.rs", &["m"]);

    assert_eq!(files(&probe(&conn, &anchor, &["CALLS"], &["a.rs"]).unwrap()), [("a.rs".to_string(), 1)]);
    assert_eq!(count(probe(&conn, &anchor, &["CALLS"], &["a.rs", "b.rs"])), 2);
    assert!(probe(&conn, &anchor, &["CALLS"], &["t.rs"]).is_none());
}

/// `count` is calling functions, not call sites - a function's repeated
/// call is one row - summed over files, each file's `refs` its own
/// functions. Control: set `count` to `by_file.len()` in `probe` (2, the
/// files, instead of 3).
#[test]
fn count_is_calling_functions_summed_over_files() {
    let mut conn = setup();
    let anchor = method(&mut conn);
    caller(&mut conn, "a1", "a.rs", &["m", "m", "n"]);
    caller(&mut conn, "a2", "a.rs", &["m"]);
    caller(&mut conn, "b1", "b.rs", &["m"]);

    let found = probe(&conn, &anchor, &["CALLS"], &[]).expect("candidates");
    assert_eq!(found.count, 3);
    assert_eq!(files(&found), [("a.rs".to_string(), 2), ("b.rs".to_string(), 1)]);
}

/// The lookup seeks the table's primary key on `name` rather than scanning
/// it. Control: reorder the table's primary key to `(nodeId, name)`.
#[test]
fn the_candidate_lookup_seeks_the_name_key() {
    let conn = setup();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {}", candidate_sql(3, 2)))
        .unwrap()
        .query_map(rusqlite::params_from_iter(["x"; 8]), |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(plan[0].contains("u USING PRIMARY KEY (name=?)"), "{plan:?}");
}

// --- the tally (`CandidateTally::from_files`, `wire_len`) -------------------

fn tallied(paths: usize) -> Vec<(String, i64)> {
    (0..paths).map(|i| (format!("f{i:02}.rs"), 1)).collect()
}

/// No candidate is no field, not a zero. Control: drop the `count == 0`
/// return (`Some` with `count: 0`).
#[test]
fn a_zero_count_is_no_field() {
    assert!(CandidateTally::from_files(0, tallied(2), "hint").is_none());
    assert!(CandidateTally::from_files(0, Vec::new(), "hint").is_none());
}

/// Files sort by refs, then path. Control: drop the sort, or its path
/// tie-break (`c.rs` before `a.rs`).
#[test]
fn files_sort_by_refs_then_path() {
    let by_file = vec![("c.rs".to_string(), 1), ("b.rs".to_string(), 3), ("a.rs".to_string(), 1)];
    let found = CandidateTally::from_files(5, by_file, "hint").unwrap();
    assert_eq!(files(&found), [("b.rs".to_string(), 3), ("a.rs".to_string(), 1), ("c.rs".to_string(), 1)]);
}

/// The list caps at 20 with `filesTruncated`, while `count` stays whole;
/// exactly 20 is not truncated and sends no `filesTruncated` key. Controls:
/// drop the truncate (21 files); compare with `>=` (20 reads truncated).
#[test]
fn files_cap_at_twenty_with_files_truncated() {
    let over = CandidateTally::from_files(FILE_CAP + 1, tallied(FILE_CAP + 1), "hint").unwrap();
    assert_eq!((over.count, over.files.len(), over.files_truncated), (FILE_CAP + 1, FILE_CAP, true));
    assert_eq!(serde_json::to_value(&over).unwrap()["filesTruncated"], true);

    let exact = CandidateTally::from_files(FILE_CAP, tallied(FILE_CAP), "hint").unwrap();
    assert!(!exact.files_truncated);
    assert!(serde_json::to_value(&exact).unwrap().get("filesTruncated").is_none());
}

/// The bytes held back cover the field as it is sent, key included, and
/// nothing is held back for an absent field. Control: restore the old fixed
/// `+ 20` (sized for `,"unlinkedUsages":`) in `wire_len` (short by four).
#[test]
fn wire_len_covers_the_field_under_its_own_key() {
    let found = CandidateTally::from_files(3, tallied(3), UNTYPED_RECEIVER_CALLS_HINT);
    let sent = format!(",\"{FIELD}\":{}", serde_json::to_string(found.as_ref().unwrap()).unwrap());
    assert!(UntypedReceiverCalls::wire_len(&found, FIELD) >= sent.len(), "{sent}");
    assert_eq!(UntypedReceiverCalls::wire_len(&None, FIELD), 0);
}

// --- the handlers ---------------------------------------------------------

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

fn by_id(id: &str, limit: Option<u32>) -> SymbolQueryParams {
    SymbolQueryParams { symbol_id: Some(id.to_string()), limit, ..Default::default() }
}

fn callers(store: &Arc<IndexStore>, params: SymbolQueryParams) -> (serde_json::Value, usize) {
    body(
        &find_callers_callees::handle_callers(
            store,
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &rust_with_a_semantic_tier(),
            &SessionHints::default(),
            params,
        )
        .unwrap(),
    )
}

fn references(store: &Arc<IndexStore>, params: SymbolQueryParams) -> (serde_json::Value, usize) {
    body(
        &find_references::handle(
            store,
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &rust_with_a_semantic_tier(),
            &SessionHints::default(),
            params,
        )
        .unwrap(),
    )
}

/// `m::P::m` in `t.rs`, called by 60 functions in `a.rs` whose qualified
/// names are 400 bytes (enough to cut a 200-row page at the byte budget),
/// plus, `with_untyped`, 20 functions each in its own long-named file
/// calling `m` through an untyped receiver.
fn crowded(with_untyped: bool) -> Arc<IndexStore> {
    let mut conn = setup();
    method(&mut conn);
    let mut diff = Diff::default();
    for i in 0..60 {
        let id = format!("c{i:02}");
        let mut node =
            NodeRecord::new(&id, "Function", &id, format!("{id}_{}", "x".repeat(400)), "a.rs", "rust");
        node.signature = Some("fn c()".to_string());
        diff.upsert_nodes.push(node);
        diff.upsert_edges.push(EdgeRecord::new(
            format!("e{i:02}"),
            &id,
            "m::P::m",
            "CALLS",
            "syntactic",
            true,
        ));
    }
    if with_untyped {
        for i in 0..FILE_CAP {
            let mut node = NodeRecord::new(
                format!("u{i:02}"),
                "Function",
                "u",
                format!("u::u{i:02}"),
                format!("src/{}/u{i:02}.rs", "d".repeat(150)),
                "rust",
            );
            node.untyped_calls = vec!["m".to_string()];
            diff.upsert_nodes.push(node);
        }
    }
    write(&mut conn, diff);
    Arc::new(IndexStore::new(conn))
}

/// Both handlers hold the field's bytes back from the row budget: with the
/// field, a page cut by bytes carries fewer rows, and the whole response
/// stays inside `MAX_RESPONSE_BYTES`. Control: drop the
/// `UntypedReceiverCalls::wire_len` term from either handler's reserve (the
/// row counts match).
#[test]
fn both_handlers_reserve_the_fields_bytes() {
    let plain = crowded(false);
    let marked = crowded(true);
    for (page, query) in [
        ("callers", callers as fn(&Arc<IndexStore>, SymbolQueryParams) -> (serde_json::Value, usize)),
        ("references", references),
    ] {
        let (without, _) = query(&plain, by_id("m::P::m", Some(200)));
        let (with, bytes) = query(&marked, by_id("m::P::m", Some(200)));

        assert!(without.get("untypedReceiverCalls").is_none(), "{page}: {without}");
        assert_eq!(with["untypedReceiverCalls"]["count"], FILE_CAP, "{page}");
        assert_eq!(with["hasMore"], true, "{page}: the byte budget cuts the page");
        let rows = |body: &serde_json::Value| body["results"].as_array().unwrap().len();
        assert!(
            rows(&with) < rows(&without),
            "{page}: {} rows with the field, {} without",
            rows(&with),
            rows(&without)
        );
        assert!(bytes <= MAX_RESPONSE_BYTES, "{page}: {bytes} bytes");
    }
}

/// A file the untyped tally alone names is one the response touches, so a
/// pending semantic pass on it is disclosed. Control: drop the
/// `UntypedReceiverCalls::file_paths` chain from either handler's
/// `touched` (no `pendingFiles`).
#[test]
fn a_pending_file_named_only_by_the_untyped_tally_is_disclosed() {
    let mut conn = setup();
    method(&mut conn);
    caller(&mut conn, "u1", "u.rs", &["m"]);
    conn.execute(
        "INSERT INTO semantic_pending (language, since) VALUES ('rust', '2026-09-26T10:14:03Z')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO semantic_pending_files (language, filePath) VALUES ('rust', 'u.rs')", [])
        .unwrap();
    let store = Arc::new(IndexStore::new(conn));

    for (page, (body, _)) in [
        ("callers", callers(&store, by_id("m::P::m", None))),
        ("references", references(&store, by_id("m::P::m", None))),
    ] {
        assert!(body["results"].as_array().unwrap().is_empty(), "{page}: {body}");
        assert_eq!(body["untypedReceiverCalls"]["files"], serde_json::json!([{ "path": "u.rs", "refs": 1 }]));
        assert_eq!(body["provenance"]["semanticTier"], "pending", "{page}: {body}");
        assert_eq!(body["provenance"]["pendingFiles"], serde_json::json!(["u.rs"]), "{page}: {body}");
    }
}

/// With no candidate the key is absent, not `null`. Control: drop
/// `skip_serializing_if` from either page's field (`"untypedReceiverCalls":
/// null`).
#[test]
fn no_candidate_sends_no_key() {
    let store = crowded(false);
    for (page, (body, _)) in [
        ("callers", callers(&store, by_id("m::P::m", None))),
        ("references", references(&store, by_id("m::P::m", None))),
    ] {
        assert!(body.as_object().unwrap().get("untypedReceiverCalls").is_none(), "{page}: {body}");
    }
}
