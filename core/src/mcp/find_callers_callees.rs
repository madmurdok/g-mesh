//! Real logic behind the `find_callers`/`find_callees` MCP tools. Same shape
//! as `find_references` - anchor lookup, then `paginate_edges` over one edge
//! kind - except the edge kind is `CALLS` and the direction to walk flips
//! depending on which tool is asking: callers are the `Incoming` `CALLS`
//! edges (who calls the anchor), callees are the `Outgoing` ones (what the
//! anchor calls). Both are single-hop by design; the transitive walk lives in
//! `get_dependencies`, not here.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::SymbolQueryParams;

/// Mirrors `find_references::REFERENCE_PAGE_SIZE` - no page-size convention
/// exists beyond "20" yet, so this reuses it rather than inventing a second
/// arbitrary number.
const CALL_PAGE_SIZE: usize = 20;

/// One "other end of a CALLS edge" record, plus whether that edge is
/// `resolved`. Direction-agnostic on purpose: `list_calls` doesn't know
/// whether it's resolving a caller or a callee, only which node id sits at
/// the non-anchor end of each edge. The two `handle_*` functions attach the
/// role-specific field name (`callerSymbolId` vs `calleeSymbolId`) when they
/// serialize.
struct CallSite {
    node: NodeRecord,
    resolved: bool,
}

/// Paginates the `CALLS` edges incident to `anchor_id` in `direction` and
/// resolves each to the node on the other end. Split out from the `handle_*`
/// functions, like `find_references::list_references`, so tests can drive it
/// with a small `page_size` directly.
fn list_calls(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    direction: Direction,
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<CallSite>> {
    let page = pagination::paginate_edges(conn, anchor_id, direction, Some("CALLS"), anchor_file_path, page_size, cursor)
        .context("failed to paginate CALLS edges")?;

    let mut results = Vec::with_capacity(page.results.len());
    for edge in page.results {
        // Outgoing: anchor is fromId, the callee sits at toId. Incoming: anchor
        // is toId, the caller sits at fromId.
        let other_id = match direction {
            Direction::Outgoing => &edge.to_id,
            Direction::Incoming => &edge.from_id,
        };
        let node = queries::get_node(conn, other_id)
            .context("failed to resolve call-edge endpoint")?
            .with_context(|| format!("edge {} points at missing node {other_id}", edge.id))?;
        results.push(CallSite { node, resolved: edge.resolved });
    }

    Ok(pagination::Page { results, has_more: page.has_more, next_cursor: page.next_cursor })
}

/// Wire shape for `find_callers`: the calling function on the other end of
/// each inbound `CALLS` edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerSite {
    caller_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    resolved: bool,
}

impl From<CallSite> for CallerSite {
    fn from(site: CallSite) -> Self {
        CallerSite {
            caller_symbol_id: site.node.id,
            name: site.node.name,
            qualified_name: site.node.qualified_name,
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: site.node.start_line,
            start_col: site.node.start_col,
            resolved: site.resolved,
        }
    }
}

/// Wire shape for `find_callees`: the called function on the other end of
/// each outbound `CALLS` edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleeSite {
    callee_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    resolved: bool,
}

impl From<CallSite> for CalleeSite {
    fn from(site: CallSite) -> Self {
        CalleeSite {
            callee_symbol_id: site.node.id,
            name: site.node.name,
            qualified_name: site.node.qualified_name,
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: site.node.start_line,
            start_col: site.node.start_col,
            resolved: site.resolved,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerPage {
    results: Vec<CallerSite>,
    has_more: bool,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleePage {
    results: Vec<CalleeSite>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// Shared anchor lookup: both tools fail the same way on an unknown
/// `symbol_id`, matching `find_references::handle`'s convention exactly.
fn resolve_anchor(conn: &Connection, symbol_id: &str) -> Result<Result<NodeRecord, CallToolResult>, ErrorData> {
    let anchor = queries::get_node(conn, symbol_id).map_err(|e| internal_error("failed to look up anchor node", e))?;
    match anchor {
        Some(node) => Ok(Ok(node)),
        None => match error(format!("g-mesh: no symbol with id '{symbol_id}' found")) {
            Ok(result) => Ok(Err(result)),
            Err(e) => Err(e),
        },
    }
}

pub(super) fn handle_callers(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match resolve_anchor(&conn, &params.symbol_id)? {
        Ok(node) => node,
        Err(early_return) => return Ok(early_return),
    };

    let page = list_calls(&conn, &params.symbol_id, &anchor.file_path, Direction::Incoming, CALL_PAGE_SIZE, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find callers", e))?;

    success(&CallerPage {
        results: page.results.into_iter().map(CallerSite::from).collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
    })
}

pub(super) fn handle_callees(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match resolve_anchor(&conn, &params.symbol_id)? {
        Ok(node) => node,
        Err(early_return) => return Ok(early_return),
    };

    let page = list_calls(&conn, &params.symbol_id, &anchor.file_path, Direction::Outgoing, CALL_PAGE_SIZE, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find callees", e))?;

    success(&CalleePage {
        results: page.results.into_iter().map(CalleeSite::from).collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::EdgeRecord;

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

    /// Sets up the acceptance criteria's chain: A calls B, B calls C.
    fn setup_chain() -> Connection {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_ab", "a", "b", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_bc", "b", "c", "CALLS", "tree-sitter", true)).unwrap();
        conn
    }

    #[test]
    fn find_callers_of_b_returns_exactly_a_not_c_not_itself() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: "b".to_string(), cursor: None };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one caller, not the transitive chain");
        assert_eq!(results[0]["callerSymbolId"], "a");
    }

    #[test]
    fn find_callees_of_b_returns_exactly_c_not_a() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: "b".to_string(), cursor: None };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one callee, not the transitive chain");
        assert_eq!(results[0]["calleeSymbolId"], "c");
    }

    #[test]
    fn callers_of_a_root_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: "a".to_string(), cursor: None };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn callees_of_a_leaf_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: "c".to_string(), cursor: None };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callers() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: "does_not_exist".to_string(), cursor: None };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callees() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: "does_not_exist".to_string(), cursor: None };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    /// Mirrors `find_references`'s small-page-size loop test. Proven here for
    /// callers only: the underlying pagination call is identical code for
    /// both directions (only `Direction` differs), so proving it once plus
    /// the trivial A/B/C chain test for callees is enough coverage without
    /// duplicating the whole loop.
    #[test]
    fn called_by_three_functions_returns_all_three_across_small_pages() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "CALLS", "tree-sitter", true)).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_calls(&conn, "target", "target.rs", Direction::Incoming, 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|c| c.node.id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["caller_a", "caller_b", "caller_c"], "all three callers must come back, once each");
    }

    #[test]
    fn handle_callees_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "target", "callee_a", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "target", "callee_b", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "target", "callee_c", "CALLS", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams { symbol_id: "target".to_string(), cursor: cursor.clone() };
            let result = handle_callees(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["calleeSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(seen, vec!["callee_a", "callee_b", "callee_c"], "all three callees must come back, once each");
    }
}
