//! Real logic behind the `get_file_outline` MCP tool. Same shape as
//! `find_references`/`find_implementations` - anchor lookup, then a
//! paginated edge walk - except the anchor is a `File` node found by path
//! rather than a symbol found by id, and the walk follows `DEFINES` edges
//! ordered by source position (`graph::pagination::paginate_defines`)
//! instead of the resolved/locality rule the symbol-anchored tools use.

use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination;
use crate::graph::queries;
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::GetFileOutlineParams;

/// One symbol the file declares. No `file_path` field - every entry in this
/// list is by definition in the file the caller just named, so repeating it
/// per row would only be noise.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlineSymbol {
    symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    start_line: i64,
    start_col: i64,
    end_line: i64,
    end_col: i64,
    signature: Option<String>,
    exported: bool,
}

impl From<NodeRecord> for OutlineSymbol {
    fn from(n: NodeRecord) -> Self {
        Self {
            symbol_id: n.id,
            name: n.name,
            qualified_name: n.qualified_name,
            kind: n.kind,
            start_line: n.start_line,
            start_col: n.start_col,
            end_line: n.end_line,
            end_col: n.end_col,
            signature: n.signature,
            exported: n.exported,
        }
    }
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlinePage {
    results: Vec<OutlineSymbol>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// Paginates the `DEFINES` edges out of `file_node_id`, in source order.
/// Split out from `handle` so tests can drive it with a small `page_size`
/// without needing a page-size field on the public tool parameters.
fn list_outline(
    conn: &Connection,
    file_node_id: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<OutlineSymbol>> {
    let page = pagination::paginate_defines(conn, file_node_id, page_size, cursor)?;
    Ok(pagination::Page {
        results: page.results.into_iter().map(OutlineSymbol::from).collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        all_unresolved: page.all_unresolved,
    })
}

pub(super) fn handle(
    conn: &Arc<Mutex<Connection>>,
    params: GetFileOutlineParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let file_node = queries::find_file_node(&conn, &params.file_path)
        .map_err(|e| internal_error("failed to look up file", e))?;
    let file_node = match file_node {
        Some(node) => node,
        None => return error(format!("g-mesh: no file '{}' found in the index", params.file_path)),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    let page = list_outline(&conn, &file_node.id, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to list file outline", e))?;

    success(&OutlinePage { results: page.results, has_more: page.has_more, next_cursor: page.next_cursor })
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

    fn symbol_at(id: &str, name: &str, start_line: i64) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", name, format!("pkg::{name}"), "a.rs", "rust");
        node.start_line = start_line;
        node
    }

    /// Acceptance criteria: a handful of top-level functions/classes come
    /// back as exactly that outline, in source order - declared here out of
    /// source order to prove the tool sorts rather than echoing insertion or
    /// `DEFINES`-edge order.
    #[test]
    fn top_level_symbols_come_back_in_source_order() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, symbol_at("third", "third", 30)).unwrap();
        upsert_node(&mut conn, symbol_at("first", "first", 5)).unwrap();
        upsert_node(&mut conn, symbol_at("second", "second", 15)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_third", "file", "third", "DEFINES", "tree-sitter", false))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_first", "file", "first", "DEFINES", "tree-sitter", false))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new("e_second", "file", "second", "DEFINES", "tree-sitter", false),
        )
        .unwrap();

        let params = GetFileOutlineParams { file_path: "a.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        let names: Vec<&str> = results.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn zero_symbols_is_an_empty_page_not_an_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();

        let params = GetFileOutlineParams { file_path: "a.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn unknown_file_path_is_a_tool_level_error() {
        let conn = setup();
        let params =
            GetFileOutlineParams { file_path: "does/not/exist.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does/not/exist.rs"));
    }

    /// A symbol node happening to share the queried path in its own
    /// `filePath` column must not be mistaken for the `File` node itself -
    /// only `kind = 'File'` identifies the anchor.
    #[test]
    fn a_symbol_sharing_the_file_path_is_not_mistaken_for_the_file_node() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("not_the_file", "Function", "a.rs", "a.rs", "a.rs", "rust"))
            .unwrap();

        let params = GetFileOutlineParams { file_path: "a.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("a.rs"));
    }

    #[test]
    fn handle_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..5 {
            let id = format!("n{i}");
            upsert_node(&mut conn, symbol_at(&id, &id, i)).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = GetFileOutlineParams {
                file_path: "a.rs".to_string(),
                cursor: cursor.clone(),
                ..Default::default()
            };
            let result = handle(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["name"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        assert_eq!(
            seen,
            vec!["n0", "n1", "n2", "n3", "n4"],
            "every symbol must appear exactly once, in source order"
        );
    }

    /// A file with more top-level symbols than the default page size must
    /// still come back in one call when the caller raises `limit` past the
    /// symbol count - the whole point of adding `limit` here, mirroring
    /// `find_references`' `a_custom_limit_returns_more_than_the_default_page_in_one_call`.
    #[test]
    fn a_custom_limit_returns_a_large_file_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..30 {
            let id = format!("n{i}");
            upsert_node(&mut conn, symbol_at(&id, &id, i)).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }

        let params =
            GetFileOutlineParams { file_path: "a.rs".to_string(), limit: Some(30), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 30, "30 symbols must fit in a single page once limit covers them all");
        assert_eq!(body["hasMore"], false);
    }

    /// Omitting `limit` must keep paging at the same default as before this
    /// field existed - a caller that never touches `limit` must see no
    /// behavior change.
    #[test]
    fn omitting_limit_keeps_the_default_page_size() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..25 {
            let id = format!("n{i}");
            upsert_node(&mut conn, symbol_at(&id, &id, i)).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }

        let params = GetFileOutlineParams { file_path: "a.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            pagination::DEFAULT_PAGE_SIZE,
            "no limit means the first page is exactly the default size"
        );
        assert_eq!(
            body["hasMore"], true,
            "25 symbols against the default page size must still have a next page"
        );
    }

    /// A `limit` above the ceiling must be clamped, not honored verbatim or
    /// rejected - same contract `pagination::resolve_page_size` gives the
    /// symbol-query tools.
    #[test]
    fn an_oversized_limit_is_clamped_to_the_ceiling() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..(pagination::MAX_PAGE_SIZE as i64 + 5) {
            let id = format!("n{i}");
            upsert_node(&mut conn, symbol_at(&id, &id, i)).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }

        let params =
            GetFileOutlineParams { file_path: "a.rs".to_string(), limit: Some(10_000), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(
            results.len(),
            pagination::MAX_PAGE_SIZE,
            "an oversized limit must clamp to the ceiling, not return every row"
        );
        assert_eq!(body["hasMore"], true);
    }
}
