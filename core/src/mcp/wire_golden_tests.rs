//! The exact bytes of every tool answer shape, rendered from the handlers'
//! `CallToolResult`: `isError` and each content block's text, in order.
//! Expected text lives in `testdata/wire_golden/<shape>.txt`;
//! `G_MESH_UPDATE_WIRE_GOLDEN=1` rewrites the files from the current code.
//!
//! Assertions go through [`assert_wire`] on the rendered text only, so the
//! type that builds the answer can change without touching a test.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use rusqlite::Connection;

use super::context::QueryCtx;
use super::find_callers_callees::{handle_callees, handle_callers};
use super::find_definition;
use super::find_implementations;
use super::find_references;
use super::get_dependencies;
use super::get_file_outline;
use super::not_indexed::test_support::python_absent;
use super::search_code::{self, Coverage};
use super::{
    Answer, FindDefinitionParams, FindImplementationsParams, GetDependenciesParams, GetFileOutlineParams,
    OutlineDetail, SearchCodeParams, SymbolQueryParams,
};
use crate::embedding::model::EMBEDDING_DIM;
use crate::embedding::pipeline::test_support::{fake_pipeline, Counters};
use crate::graph::Direction;
use crate::indexing_status::Phase;
use crate::mcp::query::test_support::Rendered;
use crate::model::{EdgeRecord, NodeRecord};
use crate::protocol::types::Position;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};
use crate::storage::write::{upsert_edge, upsert_node};

const UPDATE_ENV: &str = "G_MESH_UPDATE_WIRE_GOLDEN";

/// `isError`, then every content block's text in order.
fn render(result: &impl Rendered) -> String {
    let result = &result.rendered();
    let mut out = format!("isError: {:?}\n", result.is_error);
    for (i, block) in result.content.iter().enumerate() {
        match block {
            ContentBlock::Text(text) => out.push_str(&format!("--- block {i} ---\n{}\n", text.text)),
            other => panic!("expected text content, got {other:?}"),
        }
    }
    out
}

/// Compares `result`'s rendering with `testdata/wire_golden/<shape>.txt`.
fn assert_wire(shape: &str, result: &impl Rendered) {
    let actual = render(result);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/mcp/testdata/wire_golden")
        .join(format!("{shape}.txt"));
    if std::env::var_os(UPDATE_ENV).is_some() {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(actual, expected, "wire bytes of '{shape}' differ from {}", path.display());
}

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn no_sources() -> PathBuf {
    std::env::temp_dir().join("g-mesh-wire-golden-no-sources")
}

fn ctx(conn: Connection) -> QueryCtx {
    QueryCtx { project_root: no_sources(), ..QueryCtx::for_test(&Arc::new(IndexStore::new(conn))) }
}

fn func(id: &str, name: &str, qualified: &str, file: &str, line: i64) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", name, qualified, file, "rust");
    node.start_line = line;
    node.end_line = line + 2;
    node
}

fn edge(id: &str, from: &str, to: &str, kind: &str) -> EdgeRecord {
    EdgeRecord::new(id, from, to, kind, "tree-sitter", true)
}

fn by_id(id: &str) -> SymbolQueryParams {
    SymbolQueryParams { symbol_id: Some(id.to_string()), ..Default::default() }
}

fn by_symbol_name(name: &str) -> SymbolQueryParams {
    SymbolQueryParams { symbol_name: Some(name.to_string()), ..Default::default() }
}

fn definition(symbol_id: Option<&str>, symbol_name: Option<&str>) -> FindDefinitionParams {
    FindDefinitionParams {
        symbol_id: symbol_id.map(str::to_string),
        symbol_name: symbol_name.map(str::to_string),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    }
}

/// `target` called from `caller_a` and `caller_b` (different files), and
/// itself calling `callee_a` and `callee_b`.
fn call_graph() -> Connection {
    let mut conn = setup();
    for node in [
        func("target", "target", "pkg::target", "t.rs", 10),
        func("caller_a", "caller_a", "pkg::caller_a", "a.rs", 1),
        func("caller_b", "caller_b", "pkg::caller_b", "b.rs", 1),
        func("callee_a", "callee_a", "pkg::callee_a", "ca.rs", 1),
        func("callee_b", "callee_b", "pkg::callee_b", "cb.rs", 1),
    ] {
        upsert_node(&mut conn, node).unwrap();
    }
    for (id, from, to) in [
        ("e1", "caller_a", "target"),
        ("e2", "caller_b", "target"),
        ("e3", "target", "callee_a"),
        ("e4", "target", "callee_b"),
    ] {
        upsert_edge(&mut conn, edge(id, from, to, "CALLS")).unwrap();
    }
    conn
}

/// 150 callers of `hub`, each in its own file with a long name, so one page
/// of `limit: 200` overflows the response ceiling and is cut.
fn hub_with_many_callers() -> Connection {
    let mut conn = setup();
    upsert_node(&mut conn, func("hub", "hub", "pkg::hub", "hub.rs", 1)).unwrap();
    for i in 0..150 {
        let name = format!("a_rather_long_caller_function_name_to_fill_the_page_{i:03}");
        let file = format!("src/some/deeply/nested/module/path/file_{i:03}.rs");
        upsert_node(&mut conn, func(&format!("c{i:03}"), &name, &format!("pkg::{name}"), &file, 5)).unwrap();
        upsert_edge(&mut conn, edge(&format!("e{i:03}"), &format!("c{i:03}"), "hub", "CALLS")).unwrap();
    }
    conn
}

// ---------------------------------------------------------------- find_definition

#[test]
fn definition_node() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "run", "pkg::run", "a/lib.rs", 3)).unwrap();
    let result = ctx(conn).resolve_lazily(find_definition::handle, definition(Some("n1"), None)).unwrap();
    assert_wire("definition_node", &result);
}

#[test]
fn definition_by_position() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "run", "pkg::run", "a/lib.rs", 3)).unwrap();
    let params = FindDefinitionParams {
        file_path: Some("a/lib.rs".to_string()),
        position: Some(Position { line: 4, col: 0 }),
        ..definition(None, None)
    };
    let result = ctx(conn).resolve_lazily(find_definition::handle, params).unwrap();
    assert_wire("definition_by_position", &result);
}

#[test]
fn definition_ambiguous_candidates_page() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "run", "pkg_a::run", "a/lib.rs", 3)).unwrap();
    upsert_node(&mut conn, func("n2", "run", "pkg_b::run", "b/lib.rs", 3)).unwrap();
    let result = ctx(conn).resolve_lazily(find_definition::handle, definition(None, Some("run"))).unwrap();
    assert_wire("definition_ambiguous", &result);
}

#[test]
fn definition_file_name_page() {
    let mut conn = setup();
    for (id, name, file, line) in [("s", "Stat", "fs.go", 10), ("o", "Open", "fs.go", 12)] {
        let mut node = NodeRecord::new(id, "Function", name, name, file, "go");
        node.start_line = line;
        node.end_line = line;
        node.exported = true;
        upsert_node(&mut conn, node).unwrap();
    }
    let result = ctx(conn).resolve_lazily(find_definition::handle, definition(None, Some("fs"))).unwrap();
    assert_wire("definition_file_name", &result);
}

#[test]
fn definition_text_refusal_unknown_id() {
    let result =
        ctx(setup()).resolve_lazily(find_definition::handle, definition(Some("nope"), None)).unwrap();
    assert_wire("definition_refusal_unknown_id", &result);
}

#[test]
fn definition_not_indexed_refusal_on_position_miss() {
    let params = FindDefinitionParams {
        file_path: Some("tools/gen.py".to_string()),
        position: Some(Position { line: 1, col: 0 }),
        ..definition(None, None)
    };
    let ctx = QueryCtx { coverage: Some(python_absent()), ..ctx(setup()) };
    let result = ctx.resolve_lazily(find_definition::handle, params).unwrap();
    assert_wire("definition_not_indexed", &result);
}

// ---------------------------------------------------------------- find_references

#[test]
fn references_rows() {
    let result = ctx(call_graph()).resolve_lazily(find_references::handle, by_id("target")).unwrap();
    assert_wire("references_rows", &result);
}

#[test]
fn references_rows_cut_with_total_and_cursor() {
    let params = SymbolQueryParams { limit: Some(200), ..by_id("hub") };
    let result = ctx(hub_with_many_callers()).resolve_lazily(find_references::handle, params).unwrap();
    assert_wire("references_rows_cut", &result);
}

#[test]
fn references_files() {
    let params = SymbolQueryParams { answer: Some(Answer::Files), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(find_references::handle, params).unwrap();
    assert_wire("references_files", &result);
}

#[test]
fn references_count() {
    let params = SymbolQueryParams { answer: Some(Answer::Count), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(find_references::handle, params).unwrap();
    assert_wire("references_count", &result);
}

#[test]
fn references_anchor_candidates_page() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "run", "pkg_a::run", "a/lib.rs", 3)).unwrap();
    upsert_node(&mut conn, func("n2", "run", "pkg_b::run", "b/lib.rs", 3)).unwrap();
    let result = ctx(conn).resolve_lazily(find_references::handle, by_symbol_name("run")).unwrap();
    assert_wire("references_anchor_candidates", &result);
}

#[test]
fn references_anchor_refusal() {
    let result = ctx(setup()).resolve_lazily(find_references::handle, by_id("nope")).unwrap();
    assert_wire("references_anchor_refusal", &result);
}

// ---------------------------------------------------------------- find_callers / find_callees

#[test]
fn callers_rows() {
    let result = ctx(call_graph()).resolve_lazily(handle_callers, by_id("target")).unwrap();
    assert_wire("callers_rows", &result);
}

#[test]
fn callers_rows_cut_with_total_and_cursor() {
    let params = SymbolQueryParams { limit: Some(200), ..by_id("hub") };
    let result = ctx(hub_with_many_callers()).resolve_lazily(handle_callers, params).unwrap();
    assert_wire("callers_rows_cut", &result);
}

#[test]
fn callers_files() {
    let params = SymbolQueryParams { answer: Some(Answer::Files), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(handle_callers, params).unwrap();
    assert_wire("callers_files", &result);
}

#[test]
fn callers_count() {
    let params = SymbolQueryParams { answer: Some(Answer::Count), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(handle_callers, params).unwrap();
    assert_wire("callers_count", &result);
}

#[test]
fn callers_anchor_candidates_page() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "run", "pkg_a::run", "a/lib.rs", 3)).unwrap();
    upsert_node(&mut conn, func("n2", "run", "pkg_b::run", "b/lib.rs", 3)).unwrap();
    let result = ctx(conn).resolve_lazily(handle_callers, by_symbol_name("run")).unwrap();
    assert_wire("callers_anchor_candidates", &result);
}

#[test]
fn callees_rows() {
    let result = ctx(call_graph()).resolve_lazily(handle_callees, by_id("target")).unwrap();
    assert_wire("callees_rows", &result);
}

#[test]
fn callees_files() {
    let params = SymbolQueryParams { answer: Some(Answer::Files), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(handle_callees, params).unwrap();
    assert_wire("callees_files", &result);
}

#[test]
fn callees_count() {
    let params = SymbolQueryParams { answer: Some(Answer::Count), ..by_id("target") };
    let result = ctx(call_graph()).resolve_lazily(handle_callees, params).unwrap();
    assert_wire("callees_count", &result);
}

#[test]
fn callees_anchor_refusal() {
    let result = ctx(setup()).resolve_lazily(handle_callees, by_id("nope")).unwrap();
    assert_wire("callees_anchor_refusal", &result);
}

// ---------------------------------------------------------------- find_implementations

/// `class_b` extends `class_a`, which implements `interface` (subtype ->
/// supertype edges).
fn implementation_chain() -> Connection {
    let mut conn = setup();
    for (id, name, file) in
        [("interface", "Iface", "iface.rs"), ("class_a", "ClassA", "a.rs"), ("class_b", "ClassB", "b.rs")]
    {
        upsert_node(&mut conn, NodeRecord::new(id, "Type", name, format!("pkg::{name}"), file, "rust"))
            .unwrap();
    }
    upsert_edge(&mut conn, edge("e_a", "class_a", "interface", "SUPERTYPE_OF")).unwrap();
    upsert_edge(&mut conn, edge("e_b", "class_b", "class_a", "SUPERTYPE_OF")).unwrap();
    conn
}

fn implementations(symbol_id: &str, transitive: Option<bool>) -> FindImplementationsParams {
    FindImplementationsParams { symbol_id: Some(symbol_id.to_string()), transitive, ..Default::default() }
}

#[test]
fn implementations_direct() {
    let result = ctx(implementation_chain())
        .resolve_lazily(find_implementations::dispatch, implementations("interface", None))
        .unwrap();
    assert_wire("implementations_direct", &result);
}

#[test]
fn implementations_transitive() {
    let result = ctx(implementation_chain())
        .resolve_lazily(find_implementations::dispatch, implementations("interface", Some(true)))
        .unwrap();
    assert_wire("implementations_transitive", &result);
}

#[test]
fn implementations_refusal_unknown_id() {
    let result =
        ctx(setup()).resolve_lazily(find_implementations::dispatch, implementations("nope", None)).unwrap();
    assert_wire("implementations_refusal", &result);
}

// ---------------------------------------------------------------- get_file_outline

fn outline_params(
    file_path: &str,
    limit: Option<u32>,
    detail: Option<OutlineDetail>,
) -> GetFileOutlineParams {
    GetFileOutlineParams { file_path: file_path.to_string(), cursor: None, limit, detail }
}

/// `file` as a File node that DEFINES `symbols`.
fn define(conn: &mut Connection, file: &str, symbols: &[&str]) {
    upsert_node(conn, NodeRecord::new(file, "File", file, file, file, "rust")).unwrap();
    for symbol in symbols {
        upsert_edge(conn, edge(&format!("d_{symbol}"), file, symbol, "DEFINES")).unwrap();
    }
}

#[test]
fn outline_page() {
    let mut conn = setup();
    upsert_node(&mut conn, func("n1", "first", "pkg::first", "o.rs", 1)).unwrap();
    upsert_node(&mut conn, func("n2", "second", "pkg::second", "o.rs", 10)).unwrap();
    define(&mut conn, "o.rs", &["n1", "n2"]);
    let result = get_file_outline::handle(&ctx(conn), outline_params("o.rs", None, None)).unwrap();
    assert_wire("outline_page", &result);
}

#[test]
fn outline_page_cut_at_the_byte_ceiling() {
    let mut conn = setup();
    let mut ids = Vec::new();
    for i in 0..120 {
        let name = format!("a_rather_long_function_name_in_the_outline_{i:03}");
        let id = format!("n{i:03}");
        upsert_node(&mut conn, func(&id, &name, &format!("pkg::{name}"), "big.rs", 1 + 4 * i)).unwrap();
        ids.push(id);
    }
    define(&mut conn, "big.rs", &ids.iter().map(String::as_str).collect::<Vec<_>>());
    let params = outline_params("big.rs", Some(200), Some(OutlineDetail::Full));
    let result = get_file_outline::handle(&ctx(conn), params).unwrap();
    assert_wire("outline_page_cut", &result);
}

#[test]
fn outline_not_indexed_refusal() {
    let ctx = QueryCtx { coverage: Some(python_absent()), ..ctx(setup()) };
    let result = get_file_outline::handle(&ctx, outline_params("tools/gen.py", None, None)).unwrap();
    assert_wire("outline_not_indexed", &result);
}

#[test]
fn outline_plain_miss() {
    let result = get_file_outline::handle(&ctx(setup()), outline_params("src/nope.rs", None, None)).unwrap();
    assert_wire("outline_plain_miss", &result);
}

// ---------------------------------------------------------------- get_dependencies

fn dependencies(file_path: &str, direction: Direction, max_fanout: Option<u32>) -> GetDependenciesParams {
    GetDependenciesParams {
        file_path: Some(file_path.to_string()),
        module_id: None,
        direction,
        max_depth: None,
        max_fanout,
        resume_token: None,
    }
}

/// a.rs imports b.rs imports c.rs; a.rs also imports d.rs and e.rs.
fn import_graph() -> Connection {
    let mut conn = setup();
    for path in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
        upsert_node(&mut conn, NodeRecord::new(path, "File", path, path, path, "rust")).unwrap();
    }
    for (from, to) in [("a.rs", "b.rs"), ("b.rs", "c.rs"), ("a.rs", "d.rs"), ("a.rs", "e.rs")] {
        upsert_edge(&mut conn, edge(&format!("e_{from}_{to}"), from, to, "IMPORTS")).unwrap();
    }
    conn
}

#[test]
fn dependencies_walk() {
    let mut conn = setup();
    for path in ["a.rs", "b.rs", "c.rs"] {
        upsert_node(&mut conn, NodeRecord::new(path, "File", path, path, path, "rust")).unwrap();
    }
    upsert_edge(&mut conn, edge("e_ab", "a.rs", "b.rs", "IMPORTS")).unwrap();
    upsert_edge(&mut conn, edge("e_bc", "b.rs", "c.rs", "IMPORTS")).unwrap();
    let result =
        get_dependencies::handle(&ctx(conn), dependencies("a.rs", Direction::Outgoing, None)).unwrap();
    assert_wire("dependencies_walk", &result);
}

#[test]
fn dependencies_walk_truncated_with_resume_token() {
    let result =
        get_dependencies::handle(&ctx(import_graph()), dependencies("a.rs", Direction::Outgoing, Some(1)))
            .unwrap();
    assert_wire("dependencies_walk_truncated", &result);
}

#[test]
fn dependencies_walk_cut_by_depth_with_frontier_and_resume_token() {
    let params =
        GetDependenciesParams { max_depth: Some(1), ..dependencies("a.rs", Direction::Outgoing, None) };
    let result = get_dependencies::handle(&ctx(import_graph()), params).unwrap();
    assert_wire("dependencies_walk_depth_cut", &result);
}

#[test]
fn dependencies_text_refusal_unknown_file() {
    let result =
        get_dependencies::handle(&ctx(setup()), dependencies("nope.rs", Direction::Outgoing, None)).unwrap();
    assert_wire("dependencies_refusal_unknown_file", &result);
}

#[test]
fn dependencies_not_indexed_refusal() {
    let ctx = QueryCtx { coverage: Some(python_absent()), ..ctx(setup()) };
    let result =
        get_dependencies::handle(&ctx, dependencies("tools/gen.py", Direction::Outgoing, None)).unwrap();
    assert_wire("dependencies_not_indexed", &result);
}

// ---------------------------------------------------------------- search_code

/// A vector with every component negative: every stored row scores below
/// zero against the fake model's all-positive query vectors.
fn opposed_vector(seed: usize) -> Vec<f32> {
    (0..EMBEDDING_DIM).map(|i| if i == seed { -10.0 } else { -1.0 }).collect()
}

/// `result` with every `"score":<number>` digit run replaced by `#`: the
/// cosine differs around the 7th digit between platforms.
fn mask_scores(mut result: CallToolResult) -> CallToolResult {
    result.content = result
        .content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Text(text) => {
                let mut out = String::new();
                let mut rest = text.text.as_str();
                while let Some(at) = rest.find("\"score\":") {
                    let (head, tail) = rest.split_at(at + "\"score\":".len());
                    out.push_str(head);
                    let end = tail
                        .find(|c: char| !(c.is_ascii_digit() || "-.e+".contains(c)))
                        .unwrap_or(tail.len());
                    out.push('#');
                    rest = &tail[end..];
                }
                out.push_str(rest);
                ContentBlock::text(out)
            }
            other => other,
        })
        .collect();
    result
}

fn search_ctx(vectors: usize) -> (tempfile::TempDir, QueryCtx) {
    let dir = tempfile::tempdir().unwrap();
    register_extension();
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for i in 0..vectors {
        let id = format!("n{i}");
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, 'a.ts', 1, 0, 3, 1, 'typescript')",
            rusqlite::params![id],
        )
        .unwrap();
        insert(&conn, &id, &opposed_vector(i), "v1").unwrap();
    }
    let embedding = Arc::new(fake_pipeline(&dir.path().join("model"), None, &Counters::default()));
    let ctx = QueryCtx { embedding, ..QueryCtx::for_test(&Arc::new(IndexStore::new(conn))) };
    (dir, ctx)
}

#[test]
fn search_partial_note_then_page() {
    let (_dir, ctx) = search_ctx(3);
    let coverage = Coverage::partial(
        &Phase::Embedding,
        3,
        10,
        Some(Duration::from_millis(200)),
        Path::new("/wire-golden/project"),
    );
    let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
    let result = search_code::handle(&ctx, params, coverage.as_ref()).unwrap().rendered();
    assert_wire("search_partial", &mask_scores(result));
}

#[test]
fn search_complete_page() {
    let (_dir, ctx) = search_ctx(3);
    let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
    let result = search_code::handle(&ctx, params, None).unwrap().rendered();
    assert_wire("search_complete", &mask_scores(result));
}

#[test]
fn search_empty_page_for_a_blank_query() {
    let (_dir, ctx) = search_ctx(0);
    let params = SearchCodeParams { query: "   ".to_string(), ..Default::default() };
    let result = search_code::handle(&ctx, params, None).unwrap();
    assert_wire("search_empty_page", &result);
}
