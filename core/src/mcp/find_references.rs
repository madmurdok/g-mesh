//! Real logic behind the `find_references` MCP tool. The hard part - cursor
//! pagination with the resolved/locality/id ordering rule - already lives in
//! `graph::pagination::paginate_edges`; this module is just the "anchor
//! lookup -> incoming usage edges -> usage-site JSON" wiring around it.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction};
use crate::graph::queries;

use super::tool_result::{error, internal_error, success};
use super::SymbolQueryParams;

/// Every edge kind that means "this node uses the anchor somewhere in its own
/// source". The extractor files each usage under exactly one of these and
/// never duplicates it: a resolved call becomes a `CALLS` edge and `addUsage`
/// then explicitly skips the `REFERENCES` one for that pair, and a
/// supertype clause becomes only a `SUPERTYPE_OF` edge (see
/// plugins/js-ts/src/extract.ts). So filtering on `REFERENCES` alone hides
/// exactly the usages the more specific kinds claimed - which made
/// `find_references` return *fewer* results than `find_callers` on the same
/// symbol. Matches the `('REFERENCES', 'CALLS')` set `find_definition`
/// already ranks candidates by, widened with `SUPERTYPE_OF` so
/// `find_implementations`' answers are covered too.
///
/// `DEFINES`/`EXPORTS`/`IMPORTS` stay out on purpose: they run File -> symbol
/// (or File -> Module) and describe where a symbol is *declared* or which
/// module a file pulls in, not where it is used. `get_file_outline` and
/// `get_dependencies` own those.
const USAGE_EDGE_KINDS: &[&str] = &["CALLS", "REFERENCES", "SUPERTYPE_OF"];

/// One usage site. Edges don't carry a range of their own (only
/// `fromId`/`toId`/`kind`/`source`/`resolved`), so the location is the
/// *referencing* node's own span - the closest thing to "where this
/// reference lives" the current schema can answer without a token-level
/// range table.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceSite {
    referencing_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    /// Which `USAGE_EDGE_KINDS` edge produced this row - the result set is a
    /// union of several kinds, so without it a call is indistinguishable from
    /// a plain value usage.
    reference_kind: String,
    resolved: bool,
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferencePage {
    results: Vec<ReferenceSite>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// Paginates the incoming `USAGE_EDGE_KINDS` edges for `anchor_id` and
/// resolves each one to its referencing node. Split out from `handle` so
/// tests can drive it with a small `page_size` without needing a page-size
/// field on the public tool parameters.
fn list_references(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<ReferenceSite>> {
    let page = pagination::paginate_edges(
        conn,
        anchor_id,
        Direction::Incoming,
        USAGE_EDGE_KINDS,
        anchor_file_path,
        page_size,
        cursor,
    )
    .context("failed to paginate reference edges")?;

    let mut results = Vec::with_capacity(page.results.len());
    for edge in page.results {
        let referencing = queries::get_node(conn, &edge.from_id)
            .context("failed to resolve referencing node")?
            .with_context(|| format!("edge {} references missing node {}", edge.id, edge.from_id))?;
        results.push(ReferenceSite {
            referencing_symbol_id: referencing.id,
            name: referencing.name,
            qualified_name: referencing.qualified_name,
            kind: referencing.kind,
            file_path: referencing.file_path,
            start_line: referencing.start_line,
            start_col: referencing.start_col,
            reference_kind: edge.kind,
            resolved: edge.resolved,
        });
    }

    Ok(pagination::Page { results, has_more: page.has_more, next_cursor: page.next_cursor })
}

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = queries::get_node(&conn, &params.symbol_id)
        .map_err(|e| internal_error("failed to look up anchor node", e))?;
    let anchor = match anchor {
        Some(node) => node,
        None => return error(format!("g-mesh: no symbol with id '{}' found", params.symbol_id)),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    let page = list_references(&conn, &params.symbol_id, &anchor.file_path, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find references", e))?;

    success(&ReferencePage { results: page.results, has_more: page.has_more, next_cursor: page.next_cursor })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::{EdgeRecord, NodeRecord};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn json_body(result: &CallToolResult) -> serde_json::Value {
        assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
            other => panic!("expected text/json content, got {other:?}"),
        }
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected an error result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn referenced_from_three_files_returns_all_three_across_small_pages() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "REFERENCES", "tree-sitter", true)).unwrap();

        // Page size of 1 against 3 references forces 3 pages, well past the
        // acceptance criteria's "small page size... multiple pages" bar.
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_references(&conn, "target", "target.rs", 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|r| r.referencing_symbol_id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["caller_a", "caller_b", "caller_c"], "all three usage sites must come back, once each");
    }

    /// Regression: `find_references` used to filter on `e.kind =
    /// 'REFERENCES'` alone, but the extractor files a resolved call as a
    /// `CALLS` edge *instead of* a `REFERENCES` one. A function whose only
    /// usages are in-file calls therefore had `find_callers` return both
    /// callers while `find_references` returned an empty page on the very
    /// same symbol id - observed live on `requireTask` in
    /// task-tracker-mcp's src/domain/tasks.ts. References must be a superset
    /// of callers, never a stricter subset.
    #[test]
    fn in_file_callers_are_references_too() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "requireTask", "tasks::requireTask", "tasks.ts", "typescript"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "setTaskPriority", "tasks::setTaskPriority", "tasks.ts", "typescript"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "splitTask", "tasks::splitTask", "tasks.ts", "typescript"))
            .unwrap();
        // Exactly what bulk indexing writes for two same-file calls: CALLS
        // edges and no REFERENCES edge at all.
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let callers = json_body(&super::super::find_callers_callees::handle_callers(
            &conn,
            SymbolQueryParams { symbol_id: "target".to_string(), cursor: None, limit: None },
        )
        .unwrap());
        let mut caller_ids: Vec<&str> =
            callers["results"].as_array().unwrap().iter().map(|r| r["callerSymbolId"].as_str().unwrap()).collect();
        caller_ids.sort();
        assert_eq!(caller_ids, vec!["caller_a", "caller_b"], "precondition: find_callers sees both in-file callers");

        let references =
            json_body(&handle(&conn, SymbolQueryParams { symbol_id: "target".to_string(), cursor: None, limit: None }).unwrap());
        let mut reference_ids: Vec<&str> = references["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["referencingSymbolId"].as_str().unwrap())
            .collect();
        reference_ids.sort();
        assert_eq!(reference_ids, caller_ids, "find_references must return at least every caller find_callers returns");
        assert_eq!(references["results"][0]["referenceKind"], "CALLS");
    }

    /// The other kind the union has to cover: `class Sub extends Base` is
    /// recorded only as a `SUPERTYPE_OF` edge, so a type whose only usage is
    /// being extended would otherwise report zero references while
    /// `find_implementations` reports one.
    #[test]
    fn supertype_usages_are_references_too() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("base", "Type", "Base", "pkg::Base", "base.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("sub", "Type", "Sub", "pkg::Sub", "sub.ts", "typescript")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_sub", "sub", "base", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();

        let page = list_references(&conn, "base", "base.ts", 10, None).unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].referencing_symbol_id, "sub");
        assert_eq!(page.results[0].reference_kind, "SUPERTYPE_OF");
    }

    /// Widening the filter must not drag in declaration-side edges: the File
    /// node that defines and exports a symbol is not a usage of it.
    #[test]
    fn defines_and_exports_edges_are_not_references() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "tasks.ts", "tasks.ts", "tasks.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "requireTask", "tasks::requireTask", "tasks.ts", "typescript"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_def", "file", "target", "DEFINES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_exp", "file", "target", "EXPORTS", "tree-sitter", true)).unwrap();

        let page = list_references(&conn, "target", "tasks.ts", 10, None).unwrap();
        assert!(page.results.is_empty(), "a symbol's own declaration site is not a reference to it");
    }

    /// The union must still paginate as one ordered stream rather than one
    /// page per edge kind.
    #[test]
    fn mixed_edge_kinds_paginate_as_a_single_stream() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller", "Function", "caller", "pkg::caller", "a.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("user", "Function", "user", "pkg::user", "b.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("sub", "Type", "Sub", "pkg::Sub", "c.ts", "typescript")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "caller", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_ref", "user", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_sup", "sub", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_references(&conn, "target", "target.ts", 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|r| r.referencing_symbol_id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["caller", "sub", "user"], "every usage kind comes back exactly once across pages");
    }

    /// A caller-supplied `limit` above the default page size must actually
    /// reach `paginate_edges`, not just be accepted and ignored.
    #[test]
    fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        for i in 0..25 {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), "a.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "REFERENCES", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: "target".to_string(), cursor: None, limit: Some(25) };
        let body = json_body(&handle(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn zero_references_is_an_empty_page_not_an_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: "target".to_string(), cursor: None, limit: None };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: "does_not_exist".to_string(), cursor: None, limit: None };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn handle_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams { symbol_id: "target".to_string(), cursor: cursor.clone(), limit: None };
            let result = handle(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["referencingSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(seen, vec!["caller_a", "caller_b", "caller_c"]);
    }
}
