//! Real logic behind the `find_implementations` MCP tool. Same shape as
//! `find_references`: anchor lookup, then a single-direction `paginate_edges`
//! walk over one edge kind. The only two things that differ are the edge kind
//! (`SUPERTYPE_OF` instead of `REFERENCES`) and which end of that edge is "the
//! anchor" - `SUPERTYPE_OF` edges point subtype -> supertype (fromId is the
//! implementing/extending type, toId is the interface/base type it points
//! at), so "who implements this interface" is the `Incoming` direction,
//! resolving each edge's `from_id`. Single-hop only by design: a class
//! extending a class that implements the anchor interface must NOT show up
//! here, only the direct implementor/extender does.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction};
use crate::graph::queries;

use super::tool_result::{internal_error, success};
use super::{anchor, SymbolQueryParams};

/// One implementing/extending type on the other end of an inbound
/// `SUPERTYPE_OF` edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImplementationSite {
    implementing_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    resolved: bool,
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImplementationPage {
    results: Vec<ImplementationSite>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// Paginates the incoming `SUPERTYPE_OF` edges for `anchor_id` and resolves
/// each one to the implementing/extending node. Split out from `handle` so
/// tests can drive it with a small `page_size` without needing a page-size
/// field on the public tool parameters.
fn list_implementations(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<ImplementationSite>> {
    let page = pagination::paginate_edges(
        conn,
        anchor_id,
        Direction::Incoming,
        &["SUPERTYPE_OF"],
        anchor_file_path,
        page_size,
        cursor,
    )
    .context("failed to paginate SUPERTYPE_OF edges")?;

    let mut rows = Vec::with_capacity(page.results.len());
    for edge in page.results {
        let implementing = queries::get_node(conn, &edge.from_id)
            .context("failed to resolve implementing node")?
            .with_context(|| format!("edge {} points at missing node {}", edge.id, edge.from_id))?;
        // Same rule `paginate_edges`' SQL applies: locality 0 when the
        // enriched row shares the anchor's file, 1 otherwise.
        let locality = if implementing.file_path == anchor_file_path { 0 } else { 1 };
        rows.push(pagination::EdgeRow {
            resolved: edge.resolved,
            locality,
            edge_id: edge.id.clone(),
            item: ImplementationSite {
                implementing_symbol_id: implementing.id,
                name: implementing.name,
                qualified_name: implementing.qualified_name,
                kind: implementing.kind,
                file_path: implementing.file_path,
                start_line: implementing.start_line,
                start_col: implementing.start_col,
                resolved: edge.resolved,
            },
        });
    }

    Ok(pagination::bound_page(rows, page.has_more, page.next_cursor))
}

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match anchor::resolve(&conn, &params)? {
        Ok(node) => node,
        Err(finished) => return Ok(finished),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    let page = list_implementations(&conn, &anchor.id, &anchor.file_path, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find implementations", e))?;

    success(&ImplementationPage { results: page.results, has_more: page.has_more, next_cursor: page.next_cursor })
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

    /// Sets up the acceptance criteria's chain: ClassA implements Interface,
    /// ClassB extends ClassA. Edge direction per `SUPERTYPE_OF`'s subtype ->
    /// supertype convention: ClassA -> Interface, ClassB -> ClassA.
    fn setup_chain() -> Connection {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("class_a", "Type", "ClassA", "pkg::ClassA", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("class_b", "Type", "ClassB", "pkg::ClassB", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a_iface", "class_a", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b_a", "class_b", "class_a", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        conn
    }

    #[test]
    fn find_implementations_of_interface_returns_exactly_class_a_not_class_b() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "the interface has exactly one direct implementor, not the transitive subclass");
        assert_eq!(results[0]["implementingSymbolId"], "class_a");
    }

    #[test]
    fn zero_implementations_is_an_empty_page_not_an_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    /// Anchoring by name must reach the same node the id does - here the
    /// interface, whose one direct implementor is `class_a`.
    #[test]
    fn an_unambiguous_symbol_name_anchors_the_walk_without_a_symbol_id() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let by_id = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() })
                .unwrap(),
        );
        let by_name = json_body(
            &handle(&conn, SymbolQueryParams { symbol_name: Some("Iface".to_string()), ..Default::default() }).unwrap(),
        );
        assert_eq!(by_name, by_id, "a name that resolves to one node must answer exactly as its id does");
        assert_eq!(by_name["results"][0]["implementingSymbolId"], "class_a");
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn implemented_by_three_types_returns_all_three_across_small_pages() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_implementations(&conn, "target", "target.rs", 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|r| r.implementing_symbol_id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["impl_a", "impl_b", "impl_c"], "all three implementors must come back, once each");
    }

    /// A caller-supplied `limit` above the default page size must actually
    /// reach `paginate_edges`, not just be accepted and ignored.
    #[test]
    fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        for i in 0..25 {
            let id = format!("impl_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "a.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(25), ..Default::default() };
        let body = json_body(&handle(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn handle_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams { symbol_id: Some("target".to_string()), cursor: cursor.clone(), ..Default::default() };
            let result = handle(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["implementingSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(seen, vec!["impl_a", "impl_b", "impl_c"]);
    }
}
