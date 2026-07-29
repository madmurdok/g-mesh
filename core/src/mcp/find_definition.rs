//! Real logic behind the `find_definition` MCP tool. Kept out of `mcp/mod.rs`
//! so that file stays pure tool-router wiring - this is where the actual
//! "name or position -> node(s)" decision lives.

use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::{Connection, Row};
use serde::Serialize;

use crate::graph::pagination;
use crate::graph::queries;
use crate::graph::symbol_links::PENDING_SYMBOL_NATIVE_KIND;
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::FindDefinitionParams;

/// Nothing in the ticket specifies a page size for the ambiguous-candidate
/// list, so 20 is a plain, generous-enough default - there's no existing
/// constant for this shape of list to reuse.
const CANDIDATE_PAGE_SIZE: usize = 20;

/// The full node, returned whenever the lookup is unambiguous: a
/// file+position query, an exact qualifiedName match, or a bare name that
/// happens to match exactly one node.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionNode {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    end_line: i64,
    end_col: i64,
    signature: Option<String>,
    doc_comment: Option<String>,
}

impl From<NodeRecord> for DefinitionNode {
    fn from(n: NodeRecord) -> Self {
        Self {
            id: n.id,
            kind: n.kind,
            name: n.name,
            qualified_name: n.qualified_name,
            file_path: n.file_path,
            start_line: n.start_line,
            start_col: n.start_col,
            end_line: n.end_line,
            end_col: n.end_col,
            signature: n.signature,
            doc_comment: n.doc_comment,
        }
    }
}

/// One entry in a ranked candidate list for an ambiguous bare name - a
/// preview, not the full node, since the caller is expected to re-query once
/// it picks one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionCandidate {
    /// The handle to re-query with, and the only one guaranteed to resolve:
    /// `qualifiedName` is not unique either. Excalidraw has two distinct
    /// `getNonDeletedElements` functions - `packages/element/src/index.ts`
    /// and `packages/element/src/Scene.ts` - whose qualifiedName is bare
    /// `getNonDeletedElements` in both, so picking one and asking again by
    /// name returns this very page a second time. Anchoring on `id` always
    /// terminates.
    id: String,
    qualified_name: String,
    file_path: String,
    kind: String,
    /// Signature over docstring when both exist - it's denser and more
    /// identifying in a ranked list than prose.
    preview: Option<String>,
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePage {
    /// Always `true`, and the reason this field exists at all: the four
    /// symbol-anchored tools return this page *in place of* their own result
    /// shape when a `symbol_name` turns out ambiguous, and both shapes are
    /// `{results, hasMore, nextCursor}`. Without a marker a caller would have
    /// to sniff item fields to tell "here are your callers" from "say which
    /// symbol you meant".
    ambiguous: bool,
    results: Vec<DefinitionCandidate>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// Ranks candidates by inbound `REFERENCES`+`CALLS` edge count, descending -
/// a rough proxy for "how central is this symbol", since nothing else in the
/// graph orders same-named definitions across files.
fn find_candidates_by_name(
    conn: &Connection,
    name: &str,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    // The `nativeKind` filter matches `graph::queries`': a pending-symbol
    // placeholder is named after the symbol it is waiting for, so without it
    // every file importing `foo` would offer itself as a candidate `foo`.
    let base_sql = "SELECT n.id AS id, n.qualifiedName AS qualifiedName, n.filePath AS filePath, \
                    n.kind AS kind, n.signature AS signature, n.docComment AS docComment, \
                    CAST((SELECT COUNT(*) FROM edges e WHERE e.toId = n.id AND e.kind IN ('REFERENCES', 'CALLS')) AS REAL) AS score \
                    FROM nodes n WHERE n.name = ?1 AND n.nativeKind IS NOT ?2";

    fn map_row(row: &Row) -> rusqlite::Result<(DefinitionCandidate, f64, String)> {
        let id: String = row.get("id")?;
        let score: f64 = row.get("score")?;
        let candidate = DefinitionCandidate {
            id: id.clone(),
            qualified_name: row.get("qualifiedName")?,
            file_path: row.get("filePath")?,
            kind: row.get("kind")?,
            preview: row
                .get::<_, Option<String>>("signature")?
                .or(row.get::<_, Option<String>>("docComment")?),
        };
        Ok((candidate, score, id))
    }

    pagination::paginate_by_score(
        conn,
        base_sql,
        &[&name, &PENDING_SYMBOL_NATIVE_KIND],
        CANDIDATE_PAGE_SIZE,
        cursor,
        map_row,
    )
}

/// Resolves `find_definition`'s file+position input - always unambiguous by
/// construction, so the answer is a single node, never a candidate list.
fn by_position(conn: &Connection, file_path: &str, line: u32, col: u32) -> Result<CallToolResult, ErrorData> {
    let found = queries::find_by_position(conn, file_path, line, col)
        .map_err(|e| internal_error("failed to resolve file+position", e))?;

    match found {
        Some(node) => success(&DefinitionNode::from(node)),
        None => error(format!("g-mesh: no symbol found at {file_path}:{line}:{col}")),
    }
}

/// Resolves a symbol name to the single node it names: an exact
/// qualifiedName match is tried first as a fast path (this is how a caller
/// re-queries a candidate it picked off a previous ambiguous page), then
/// falls back to a bare-name lookup.
///
/// `Ok(Ok(node))` is that node. `Ok(Err(result))` is a finished response the
/// caller must return unchanged - the ranked candidate page when the name is
/// ambiguous, or the not-found tool error - which is what lets the four
/// symbol-anchored tools accept a `symbol_name` (see `mcp::anchor`) and mean
/// exactly what `find_definition` means by it, down to the error text.
/// Shaped like `find_callers_callees`' old `resolve_anchor` rather than a
/// bespoke enum so every call site is the same two-line `match`.
pub(super) fn resolve_symbol_name(
    conn: &Connection,
    name: &str,
    cursor: Option<&str>,
) -> Result<Result<NodeRecord, CallToolResult>, ErrorData> {
    let mut exact = queries::find_by_qualified_name(conn, name, None)
        .map_err(|e| internal_error("failed to look up node by qualifiedName", e))?;
    if exact.len() == 1 {
        return Ok(Ok(exact.remove(0)));
    }

    let matches =
        queries::find_by_name(conn, name, None).map_err(|e| internal_error("failed to look up node by name", e))?;

    match matches.len() {
        0 => error(format!("g-mesh: no symbol named '{name}' found")).map(Err),
        1 => Ok(Ok(matches.into_iter().next().expect("len checked above"))),
        _ => {
            let page = find_candidates_by_name(conn, name, cursor)
                .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
            success(&CandidatePage {
                ambiguous: true,
                results: page.results,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
            })
            .map(Err)
        }
    }
}

/// `find_definition`'s own symbol-name input: the shared resolution above,
/// with the resolved case formatted as the full node this tool promises.
fn by_name(conn: &Connection, name: &str, cursor: Option<&str>) -> Result<CallToolResult, ErrorData> {
    match resolve_symbol_name(conn, name, cursor)? {
        Ok(node) => success(&DefinitionNode::from(node)),
        Err(finished) => Ok(finished),
    }
}

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: FindDefinitionParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    match (params.file_path, params.position, params.symbol_name) {
        (Some(file_path), Some(position), _) => by_position(&conn, &file_path, position.line, position.col),
        (None, None, Some(name)) => by_name(&conn, &name, params.cursor.as_deref()),
        (None, None, None) if params.cursor.is_some() => {
            error("g-mesh: `cursor` continues a previous ambiguous symbol_name lookup - give the same symbol_name again")
        }
        (None, None, None) => {
            error("g-mesh: give either `symbol_name`, or both `file_path` and `position`")
        }
        _ => error("g-mesh: `file_path` and `position` must be given together"),
    }
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

    fn node_with_span(id: &str, name: &str, qualified_name: &str, file_path: &str, end: (i64, i64)) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", name, qualified_name, file_path, "rust");
        node.end_line = end.0;
        node.end_col = end.1;
        node
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
    fn ambiguous_bare_name_returns_both_as_ranked_candidates() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();
        // A third, unrelated node calls n2 twice so its inbound CALLS count
        // outranks n1's zero - exercises the ranking, not just presence.
        upsert_node(&mut conn, NodeRecord::new("caller1", "Function", "caller1", "pkg_c::caller1", "c/lib.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "caller1", "n2", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e2", "caller1", "n2", "REFERENCES", "tree-sitter", true)).unwrap();

        let params = FindDefinitionParams {
            symbol_name: Some("run".to_string()),
            file_path: None,
            position: None,
            cursor: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both same-named symbols must come back as candidates");
        assert_eq!(results[0]["qualifiedName"], "pkg_b::run", "the higher inbound-edge count must rank first");
        assert_eq!(results[1]["qualifiedName"], "pkg_a::run");
    }

    #[test]
    fn file_and_position_query_returns_a_single_node_not_a_list() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

        let params = FindDefinitionParams {
            symbol_name: None,
            file_path: Some("a/lib.rs".to_string()),
            position: Some(crate::protocol::types::Position { line: 2, col: 0 }),
            cursor: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["id"], "n1");
        assert_eq!(body["qualifiedName"], "pkg_a::run");
        assert!(body.get("results").is_none(), "an unambiguous file+position query must not be wrapped in a list");
    }

    #[test]
    fn qualified_name_requery_returns_the_exact_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

        let params = FindDefinitionParams {
            symbol_name: Some("pkg_b::run".to_string()),
            file_path: None,
            position: None,
            cursor: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["id"], "n2");
        assert_eq!(body["qualifiedName"], "pkg_b::run");
    }

    #[test]
    fn no_match_is_a_tool_level_error() {
        let conn = setup();
        let params = FindDefinitionParams {
            symbol_name: Some("does_not_exist".to_string()),
            file_path: None,
            position: None,
            cursor: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn neither_name_nor_position_is_a_tool_level_error() {
        let conn = setup();
        let params = FindDefinitionParams { symbol_name: None, file_path: None, position: None, cursor: None };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("symbol_name"));
    }

    #[test]
    fn ambiguous_candidates_paginate_across_cursor_continuation() {
        let mut conn = setup();
        // One more node than CANDIDATE_PAGE_SIZE, so the first page is
        // truncated and a second `handle()` call with its cursor must
        // return exactly the remainder, with nothing repeated or skipped.
        let total = CANDIDATE_PAGE_SIZE + 1;
        for i in 0..total {
            let id = format!("n{i}");
            upsert_node(&mut conn, node_with_span(&id, "run", &format!("pkg{i}::run"), "a/lib.rs", (5, 0))).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let first_params =
            FindDefinitionParams { symbol_name: Some("run".to_string()), file_path: None, position: None, cursor: None };
        let first = handle(&conn, first_params).unwrap();
        let first_body = json_body(&first);
        let first_results = first_body["results"].as_array().unwrap();
        assert_eq!(first_results.len(), CANDIDATE_PAGE_SIZE);
        assert_eq!(first_body["hasMore"], true);
        let cursor = first_body["nextCursor"].as_str().unwrap().to_string();

        let second_params = FindDefinitionParams {
            symbol_name: Some("run".to_string()),
            file_path: None,
            position: None,
            cursor: Some(cursor),
        };
        let second = handle(&conn, second_params).unwrap();
        let second_body = json_body(&second);
        let second_results = second_body["results"].as_array().unwrap();
        assert_eq!(second_results.len(), 1, "the one remaining candidate must land on the second page");
        assert_eq!(second_body["hasMore"], false);

        let mut all_ids: Vec<String> = first_results
            .iter()
            .chain(second_results.iter())
            .map(|c| c["qualifiedName"].as_str().unwrap().to_string())
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), total, "every candidate must appear exactly once across both pages");
    }
}
