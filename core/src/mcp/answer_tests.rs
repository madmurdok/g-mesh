//! `answer: rows|files|count` and `total` on `find_references`,
//! `find_callers` and `find_callees`, through their real handlers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::query_shapes::QueryShapes;
use super::session_hints::SessionHints;
use super::{find_callers_callees, find_references, Answer, SymbolQueryParams};
use crate::embedding::EmbeddingPipeline;
use crate::graph::queries::{upsert_edge, upsert_node};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::{EdgeRecord, NodeRecord};

#[derive(Clone, Copy)]
enum Tool {
    References,
    Callers,
    Callees,
}

fn body(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

fn call(tool: Tool, store: &Arc<IndexStore>, params: SymbolQueryParams) -> serde_json::Value {
    let embedding = EmbeddingPipeline::disabled();
    let shapes = QueryShapes::shipped();
    let capabilities = HashMap::new();
    let hints = SessionHints::default();
    let result = match tool {
        Tool::References => find_references::handle(store, &embedding, shapes, &capabilities, &hints, params),
        Tool::Callers => {
            find_callers_callees::handle_callers(store, &embedding, shapes, &capabilities, &hints, params)
        }
        Tool::Callees => {
            find_callers_callees::handle_callees(store, &embedding, shapes, &capabilities, params)
        }
    };
    body(&result.unwrap())
}

fn on(anchor: &str, answer: Option<Answer>) -> SymbolQueryParams {
    SymbolQueryParams { symbol_id: Some(anchor.to_string()), answer, ..Default::default() }
}

fn store(nodes: &[(&str, &str, &str)], edges: &[(&str, &str, &str, &str, bool)]) -> Arc<IndexStore> {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    for (id, kind, file) in nodes {
        upsert_node(&mut conn, NodeRecord::new(*id, *kind, *id, format!("pkg::{id}"), *file, "rust"))
            .unwrap();
    }
    for (id, from, to, kind, resolved) in edges {
        upsert_edge(&mut conn, EdgeRecord::new(*id, *from, *to, *kind, "tree-sitter", *resolved)).unwrap();
    }
    Arc::new(IndexStore::new(conn))
}

/// `target` (in `t.rs`) is used five times: called twice from `a.rs`, once
/// from `b.rs` and `c.rs`, and referenced once, unresolved, from `d.rs`.
fn referenced() -> Arc<IndexStore> {
    store(
        &[
            ("target", "Function", "t.rs"),
            ("a1", "Function", "a.rs"),
            ("a2", "Function", "a.rs"),
            ("b1", "Function", "b.rs"),
            ("c1", "Function", "c.rs"),
            ("d1", "Function", "d.rs"),
        ],
        &[
            ("e_a1", "a1", "target", "CALLS", true),
            ("e_a2", "a2", "target", "CALLS", true),
            ("e_b1", "b1", "target", "CALLS", true),
            ("e_c1", "c1", "target", "CALLS", true),
            ("e_d1", "d1", "target", "REFERENCES", false),
        ],
    )
}

/// `target` (in `t.rs`) is called twice from `a.rs` and once, unresolved,
/// from `b.rs`; two non-`CALLS` usages come from `a.rs` (which also calls
/// it) and `x.rs` (which does not).
fn called() -> Arc<IndexStore> {
    store(
        &[
            ("target", "Function", "t.rs"),
            ("a1", "Function", "a.rs"),
            ("a2", "Function", "a.rs"),
            ("b1", "Function", "b.rs"),
            ("x1", "Function", "x.rs"),
        ],
        &[
            ("c_a1", "a1", "target", "CALLS", true),
            ("c_a2", "a2", "target", "CALLS", true),
            ("c_b1", "b1", "target", "CALLS", false),
            ("r_a1", "a1", "target", "REFERENCES", true),
            ("r_x1", "x1", "target", "REFERENCES", true),
        ],
    )
}

/// `source` (in `s.rs`) calls two functions in `p.rs` and one, unresolved,
/// in `q.rs`; it also references one in `p.rs` (which it calls into) and one
/// in `z.rs` (which it does not).
fn calling() -> Arc<IndexStore> {
    store(
        &[
            ("source", "Function", "s.rs"),
            ("f1", "Function", "p.rs"),
            ("f2", "Function", "p.rs"),
            ("f3", "Function", "q.rs"),
            ("v1", "Variable", "p.rs"),
            ("v2", "Variable", "z.rs"),
        ],
        &[
            ("c_f1", "source", "f1", "CALLS", true),
            ("c_f2", "source", "f2", "CALLS", true),
            ("c_f3", "source", "f3", "CALLS", false),
            ("r_v1", "source", "v1", "REFERENCES", true),
            ("r_v2", "source", "v2", "REFERENCES", true),
        ],
    )
}

fn fixture(tool: Tool) -> (Arc<IndexStore>, &'static str) {
    match tool {
        Tool::References => (referenced(), "target"),
        Tool::Callers => (called(), "target"),
        Tool::Callees => (calling(), "source"),
    }
}

fn tally(body: &serde_json::Value) -> Vec<(String, i64)> {
    body["files"]
        .as_array()
        .unwrap_or_else(|| panic!("a `files` tally: {body}"))
        .iter()
        .map(|entry| (entry["path"].as_str().unwrap().to_string(), entry["refs"].as_i64().unwrap()))
        .collect()
}

fn owned(entries: &[(&str, i64)]) -> Vec<(String, i64)> {
    entries.iter().map(|(path, refs)| (path.to_string(), *refs)).collect()
}

const ALL: [Tool; 3] = [Tool::References, Tool::Callers, Tool::Callees];

#[test]
fn omitting_answer_is_the_rows_answer_on_every_tool() {
    for tool in ALL {
        let (store, anchor) = fixture(tool);

        let omitted = call(tool, &store, on(anchor, None));
        let rows = call(tool, &store, on(anchor, Some(Answer::Rows)));

        assert!(omitted["results"].is_array(), "the default answer is rows: {omitted}");
        assert_eq!(omitted, rows);
    }
}

/// `limit: 1` shows the tally is the whole set's, not one page's.
#[test]
fn answer_files_is_the_whole_sets_tally_and_no_rows() {
    let expected = [
        (Tool::References, owned(&[("a.rs", 2), ("b.rs", 1), ("c.rs", 1), ("d.rs", 1)]), 5),
        (Tool::Callers, owned(&[("a.rs", 2), ("b.rs", 1)]), 3),
        (Tool::Callees, owned(&[("p.rs", 2), ("q.rs", 1)]), 3),
    ];
    for (tool, files, total) in expected {
        let (store, anchor) = fixture(tool);

        let answer =
            call(tool, &store, SymbolQueryParams { limit: Some(1), ..on(anchor, Some(Answer::Files)) });

        assert_eq!(tally(&answer), files, "{answer}");
        assert_eq!(answer["total"], total, "{answer}");
        assert_eq!(answer["anchor"]["id"], anchor, "{answer}");
        for absent in ["results", "hasMore", "nextCursor", "unresolved", "filesTruncated"] {
            assert!(answer.get(absent).is_none(), "`{absent}` must be absent: {answer}");
        }
    }
}

#[test]
fn answer_count_is_the_total_and_unresolved_and_nothing_else() {
    for tool in ALL {
        let (store, anchor) = fixture(tool);
        let expected_total = if matches!(tool, Tool::References) { 5 } else { 3 };

        let answer = call(tool, &store, on(anchor, Some(Answer::Count)));

        assert_eq!(answer["total"], expected_total, "{answer}");
        assert_eq!(answer["unresolved"], 1, "{answer}");
        for absent in ["results", "hasMore", "nextCursor", "files", "filesTruncated"] {
            assert!(answer.get(absent).is_none(), "`{absent}` must be absent: {answer}");
        }
    }
}

#[test]
fn a_truncated_row_page_carries_the_whole_sets_total_and_a_complete_one_does_not() {
    for tool in ALL {
        let (store, anchor) = fixture(tool);
        let expected_total = if matches!(tool, Tool::References) { 5 } else { 3 };

        let truncated = call(tool, &store, SymbolQueryParams { limit: Some(1), ..on(anchor, None) });
        let complete = call(tool, &store, on(anchor, None));

        assert_eq!(truncated["hasMore"], true, "precondition: {truncated}");
        assert_eq!(truncated["total"], expected_total, "{truncated}");
        assert_eq!(complete["hasMore"], false, "precondition: {complete}");
        assert!(complete.get("total").is_none(), "a complete page's total is its length: {complete}");
    }
}

#[test]
fn total_counts_only_the_file_paths_scope() {
    let store = referenced();
    let scoped = SymbolQueryParams {
        limit: Some(1),
        file_paths: Some(vec!["a.rs".to_string(), "b.rs".to_string()]),
        ..on("target", None)
    };

    let page = call(Tool::References, &store, scoped.clone());
    let count = call(Tool::References, &store, SymbolQueryParams { answer: Some(Answer::Count), ..scoped });

    assert_eq!(page["total"], 3, "{page}");
    assert_eq!(count["total"], 3, "{count}");
}

/// `limit: 0` was clamped to one row before `answer` existed, and still is:
/// it is not a spelling of `answer: "count"`.
#[test]
fn limit_zero_still_means_a_one_row_page() {
    let store = referenced();

    let page = call(Tool::References, &store, SymbolQueryParams { limit: Some(0), ..on("target", None) });

    assert_eq!(page["results"].as_array().map(Vec::len), Some(1), "{page}");
    assert_eq!(page["hasMore"], true, "{page}");
    assert_eq!(page["total"], 5, "{page}");
}

#[test]
fn a_files_answer_cut_by_the_tally_cap_says_so_and_keeps_the_total_exact() {
    let mut nodes = vec![("target".to_string(), "t.rs".to_string())];
    let mut edges = Vec::new();
    for i in 0..201 {
        nodes.push((format!("u{i}"), format!("f{i:03}.rs")));
        edges.push((format!("e{i}"), format!("u{i}")));
    }
    let node_refs: Vec<(&str, &str, &str)> =
        nodes.iter().map(|(id, file)| (id.as_str(), "Function", file.as_str())).collect();
    let edge_refs: Vec<(&str, &str, &str, &str, bool)> =
        edges.iter().map(|(id, from)| (id.as_str(), from.as_str(), "target", "CALLS", true)).collect();
    let store = store(&node_refs, &edge_refs);

    let answer = call(Tool::References, &store, on("target", Some(Answer::Files)));

    assert_eq!(answer["total"], 201, "{answer}");
    assert_eq!(answer["files"].as_array().map(Vec::len), Some(200), "{answer}");
    assert_eq!(answer["filesTruncated"], true, "{answer}");
}

fn excluded_files(body: &serde_json::Value) -> Vec<String> {
    body["excludedReferences"]["files"]
        .as_array()
        .map(|files| files.iter().map(|entry| entry["path"].as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

/// Every path the response names outside `excludedReferences`.
fn named_elsewhere(body: &serde_json::Value) -> HashSet<String> {
    let rows = body["results"].as_array().into_iter().flatten().map(|row| row["filePath"].as_str().unwrap());
    let files = body["files"].as_array().into_iter().flatten().map(|entry| entry["path"].as_str().unwrap());
    rows.chain(files).map(str::to_string).collect()
}

/// The file that also holds a call (`a.rs` / `p.rs`) is already named by a
/// row or the `files` tally, so only the other one is listed again; the
/// count still covers both.
#[test]
fn the_excluded_tally_lists_only_files_the_response_does_not_already_name() {
    let cases = [
        (Tool::Callers, None),
        (Tool::Callers, Some(Answer::Files)),
        (Tool::Callees, None),
        (Tool::Callees, Some(Answer::Files)),
    ];
    for (tool, answer) in cases {
        let (store, anchor) = fixture(tool);
        let lone = if matches!(tool, Tool::Callers) { "x.rs" } else { "z.rs" };
        let shared = if matches!(tool, Tool::Callers) { "a.rs" } else { "p.rs" };

        let body = call(tool, &store, on(anchor, answer));

        assert_eq!(body["excludedReferences"]["count"], 2, "{body}");
        assert!(named_elsewhere(&body).contains(shared), "precondition: {shared} is named: {body}");
        assert_eq!(excluded_files(&body), vec![lone.to_string()], "{body}");
    }
}

#[test]
fn the_excluded_disclosure_on_a_count_answer_is_a_count_without_files() {
    for tool in [Tool::Callers, Tool::Callees] {
        let (store, anchor) = fixture(tool);

        let body = call(tool, &store, on(anchor, Some(Answer::Count)));

        let excluded = &body["excludedReferences"];
        assert_eq!(excluded["count"], 2, "{body}");
        assert!(excluded.get("files").is_none(), "{body}");
        assert!(excluded.get("filesTruncated").is_none(), "{body}");
        assert!(excluded["hint"].as_str().unwrap().contains("`count` here is how many"), "{body}");
    }
}

/// Paging a ranked walk at a small limit serves every usage exactly once.
#[test]
fn paging_references_at_limit_two_serves_every_row_once() {
    let store = store(
        &[
            ("target", "Function", "src/a/t.rs"),
            ("same", "Function", "src/a/t.rs"),
            ("dir", "Function", "src/a/u.rs"),
            ("sub", "Function", "src/a/sub/v.rs"),
            ("far", "Function", "lib/w.rs"),
            ("imp", "File", "src/a/i.rs"),
            ("odd", "Function", "src/ab/x.rs"),
        ],
        &[
            ("e1", "far", "target", "CALLS", true),
            ("e2", "imp", "target", "REFERENCES", true),
            ("e3", "sub", "target", "REFERENCES", false),
            ("e4", "dir", "target", "CALLS", true),
            ("e5", "same", "target", "CALLS", true),
            ("e6", "odd", "target", "CALLS", true),
        ],
    );
    let whole: Vec<String> = call(Tool::References, &store, on("target", None))["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["referencingSymbolId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(whole.len(), 6, "precondition: one page holds every row");

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = call(
            Tool::References,
            &store,
            SymbolQueryParams { limit: Some(2), cursor, ..on("target", None) },
        );
        seen.extend(
            page["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["referencingSymbolId"].as_str().unwrap().to_string()),
        );
        if page["hasMore"] != true {
            break;
        }
        cursor = page["nextCursor"].as_str().map(str::to_string);
    }

    assert_eq!(seen, whole);
    assert_eq!(whole, vec!["same", "dir", "imp", "far", "odd", "sub"]);
}

#[test]
fn answer_accepts_its_three_values_and_names_them_when_refusing_another() {
    for (value, expected) in [("rows", Answer::Rows), ("files", Answer::Files), ("count", Answer::Count)] {
        let params: SymbolQueryParams =
            serde_json::from_value(serde_json::json!({ "answer": value })).unwrap();
        assert_eq!(params.answer, Some(expected));
    }

    let err = serde_json::from_value::<SymbolQueryParams>(serde_json::json!({ "answer": "bogus" }))
        .expect_err("an unknown answer must be refused")
        .to_string();

    for value in ["rows", "files", "count"] {
        assert!(err.contains(value), "{err}");
    }
}
