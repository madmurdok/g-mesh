//! Real logic behind the `get_file_outline` MCP tool. Same shape as
//! `find_references`/`find_implementations` - anchor lookup, then a
//! paginated edge walk - except the anchor is a `File` node found by path
//! rather than a symbol found by id, and the walk follows `DEFINES` edges
//! ordered by source position (`graph::pagination::paginate_defines`)
//! instead of the resolved/locality rule the symbol-anchored tools use.

use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination;
use crate::graph::queries;
use crate::storage::index_store::IndexStore;
use crate::storage::write::NodeRecord;

use crate::daemon::registry::PathCoverage;

use super::not_indexed;
use super::tool_result::{internal_error, success};
use super::{GetFileOutlineParams, OutlineDetail};

/// One symbol the file declares. No `file_path` field - every entry in this
/// list is by definition in the file the caller just named, so repeating it
/// per row would only be noise.
///
/// A compact row (the default) carries only what locates and anchors a
/// symbol; the `Option` fields are filled by a full render alone
/// (`detail: "full"`) and skipped otherwise, so a full row is byte-identical
/// to the row shape before GM-523, `"signature": null` included.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlineSymbol {
    symbol_id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    kind: String,
    start_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_col: Option<i64>,
    end_line: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_col: Option<i64>,
    /// Outer `None`: a compact row, key absent. `Some(None)`: a full row for
    /// a symbol without a signature, sent as `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<Option<String>>,
    /// Reachable from outside the file this symbol is declared in - **not**
    /// whether the symbol's own line carries a visibility keyword. Those
    /// coincide for a top-level item but diverge for a member whose
    /// reachability is inherited rather than stated: a method of a `pub`
    /// trait, or of any `impl Trait for T` block, is exported even though
    /// Rust forbids (and so never shows) a `pub` on that line - the trait or
    /// the impl already carries the visibility, and the member is reachable
    /// wherever the trait/type both are. Grepping the symbol's own line for
    /// `pub` answers a different, narrower question and will disagree with
    /// this field on exactly those rows - correctly, not as a bug in either
    /// one (GM-369, measured against a real consumer that re-derived a
    /// correct outline as wrong by making that substitution).
    ///
    /// An `impl` block itself can appear as its own row (kind `Type`, native
    /// kind `impl`) when the implemented type has no declaration in this
    /// project to attach its methods to instead - see `declare_impl_block` in
    /// the Rust plugin's `extractor::bodies`. It is unconditionally `true`
    /// there too, for the same reason: a trait impl's reach is the trait's
    /// and the type's, never narrower, and there is no `pub` keyword on an
    /// impl block for this field to instead mean.
    exported: bool,
}

/// `n` as the row `detail` asks for.
fn render(n: NodeRecord, detail: OutlineDetail) -> OutlineSymbol {
    let full = detail == OutlineDetail::Full;
    OutlineSymbol {
        symbol_id: n.id,
        name: n.name,
        qualified_name: full.then_some(n.qualified_name),
        kind: n.kind,
        start_line: n.start_line,
        start_col: full.then_some(n.start_col),
        end_line: n.end_line,
        end_col: full.then_some(n.end_col),
        signature: full.then_some(n.signature),
        exported: n.exported,
    }
}

/// The response body, serialized: `Page<T>` itself isn't `Serialize` since
/// it's shared by every list-shaped tool and none of them agree on an item
/// type. Borrowed, so the byte cut can measure a candidate page without
/// copying its rows.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlinePage<'a> {
    results: &'a [OutlineSymbol],
    /// The file's whole row count, present only when `has_more`: what is
    /// left to page through, as on `find_references`.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    has_more: bool,
    next_cursor: Option<&'a str>,
}

/// One outline page, cut to its byte budget, with its `total`.
struct Outline {
    page: pagination::Page<OutlineSymbol>,
    total: Option<usize>,
}

impl Outline {
    fn body(&self) -> OutlinePage<'_> {
        OutlinePage {
            results: &self.page.results,
            total: self.total,
            has_more: self.page.has_more,
            next_cursor: self.page.next_cursor.as_deref(),
        }
    }
}

/// Paginates the `DEFINES` edges out of `file_node_id`, in source order, at
/// most `page_size` rows and at most `max_bytes` of serialized response
/// (`pagination::bound_defines_page`). Split out from `handle` so tests can
/// drive it with a small `page_size` or another budget without needing those
/// fields on the public tool parameters.
fn list_outline(
    conn: &Connection,
    file_node_id: &str,
    page_size: usize,
    cursor: Option<&str>,
    detail: OutlineDetail,
    max_bytes: usize,
) -> anyhow::Result<Outline> {
    let page = pagination::paginate_defines(conn, file_node_id, page_size, cursor)?;
    let page = pagination::bound_defines_page(
        page,
        |n| render(n, detail),
        |results, has_more, next_cursor| {
            pagination::wire_len(&OutlinePage {
                results,
                total: pagination::widest_total(has_more),
                has_more,
                next_cursor,
            })
        },
        max_bytes,
    );
    let total = if page.has_more { Some(pagination::count_defines(conn, file_node_id)?) } else { None };
    Ok(Outline { page, total })
}

/// [`handle_covered`] for a path whose language is indexed.
#[cfg(test)]
pub(super) fn handle(
    store: &Arc<IndexStore>,
    params: GetFileOutlineParams,
) -> Result<CallToolResult, ErrorData> {
    handle_covered(store, None, params)
}

/// [`handle`], told whether the path's language is indexed at all: a miss on
/// a path in an absent or failed language is refused with
/// [`not_indexed::miss`]'s structured reason instead of the bare message.
pub(super) fn handle_covered(
    store: &Arc<IndexStore>,
    coverage: Option<&PathCoverage>,
    params: GetFileOutlineParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = store.read();

    let file_node = queries::find_file_node(&conn, &params.file_path)
        .map_err(|e| internal_error("failed to look up file", e))?;
    let file_node = match file_node {
        Some(node) => node,
        None => {
            return not_indexed::miss(
                &conn,
                coverage,
                format!("g-mesh: no file '{}' found in the index", params.file_path),
            )
        }
    };

    // Default to the row ceiling: the byte budget, not a row count, is what
    // bounds a default page (GM-523 Q2).
    let page_size =
        params.limit.map_or(pagination::MAX_PAGE_SIZE, |l| pagination::resolve_page_size(Some(l)));
    let outline = list_outline(
        &conn,
        &file_node.id,
        page_size,
        params.cursor.as_deref(),
        params.detail.unwrap_or_default(),
        pagination::OUTLINE_MAX_RESPONSE_BYTES,
    )
    .map_err(|e| internal_error("failed to list file outline", e))?;

    success(&outline.body())
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
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
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
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn unknown_file_path_is_a_tool_level_error() {
        let conn = setup();
        let params =
            GetFileOutlineParams { file_path: "does/not/exist.rs".to_string(), ..Default::default() };
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
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
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
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
        let conn = Arc::new(IndexStore::new(conn));

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
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 30, "30 symbols must fit in a single page once limit covers them all");
        assert_eq!(body["hasMore"], false);
    }

    /// Omitting `limit` fills the page up to the byte budget rather than
    /// stopping at the symbol tools' 20-row default (GM-523 Q2): a small
    /// file comes back whole in one call.
    #[test]
    fn omitting_limit_fills_the_page_to_the_byte_budget() {
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
        let result = handle(&Arc::new(IndexStore::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 25, "no limit means every row that fits the budget");
        assert_eq!(body["hasMore"], false);
    }

    /// A `limit` above the ceiling must be clamped, not honored verbatim or
    /// rejected - same contract `pagination::resolve_page_size` gives the
    /// symbol-query tools. Driven through `list_outline` with no byte budget,
    /// since 205 rows are over the outline's budget and it would cut first.
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

        let page_size = pagination::resolve_page_size(Some(10_000));
        let outline =
            list_outline(&conn, "file", page_size, None, OutlineDetail::Compact, usize::MAX).unwrap();
        assert_eq!(
            outline.page.results.len(),
            pagination::MAX_PAGE_SIZE,
            "an oversized limit must clamp to the ceiling, not return every row"
        );
        assert!(outline.page.has_more);
    }

    // -----------------------------------------------------------------
    // GM-523: the byte budget, compact/full rows, `total`
    // -----------------------------------------------------------------

    const BUDGET: usize = pagination::OUTLINE_MAX_RESPONSE_BYTES;

    /// The id of the `i`th row of a [`big_file`], in source order.
    fn row_id(i: usize) -> String {
        format!("s{i:04}")
    }

    /// A file of `n` symbols on lines `0..n`, each with a distinct
    /// `sig_len`-char signature, so a full row is far wider than a compact one.
    fn big_file(n: usize, sig_len: usize) -> Arc<IndexStore> {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..n {
            let id = row_id(i);
            let mut node = symbol_at(&id, &id, i as i64);
            node.end_line = i as i64;
            node.signature = Some(format!("{id}{}", "x".repeat(sig_len.saturating_sub(id.len()))));
            upsert_node(&mut conn, node).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{id}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }
        Arc::new(IndexStore::new(conn))
    }

    /// One `get_file_outline` call: the response text, as sent, and its JSON.
    fn outline_call(
        store: &Arc<IndexStore>,
        limit: Option<u32>,
        cursor: Option<String>,
        detail: Option<OutlineDetail>,
    ) -> (String, serde_json::Value) {
        let params = GetFileOutlineParams { file_path: "a.rs".to_string(), cursor, limit, detail };
        let result = handle(store, params).unwrap();
        let body = json_body(&result);
        let text = match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        (text, body)
    }

    fn ids_of(body: &serde_json::Value) -> Vec<String> {
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["symbolId"].as_str().unwrap().to_string())
            .collect()
    }

    fn keys_of(row: &serde_json::Value) -> std::collections::BTreeSet<String> {
        row.as_object().unwrap().keys().cloned().collect()
    }

    /// B1: no `limit` or `detail` lifts the 8,000-byte budget, and a file
    /// wider than the budget comes back cut, with `hasMore`. Omitting `limit`
    /// is the same call as `limit: 200` (GM-523 Q2).
    #[test]
    fn every_response_fits_the_byte_budget_whatever_limit_and_detail() {
        let store = big_file(200, 200);
        for detail in [None, Some(OutlineDetail::Compact), Some(OutlineDetail::Full)] {
            for limit in [None, Some(1), Some(50), Some(200), Some(10_000)] {
                let (text, body) = outline_call(&store, limit, None, detail);
                assert!(
                    text.len() <= BUDGET,
                    "limit {limit:?}, detail {detail:?}: {} bytes over the {BUDGET}-byte budget",
                    text.len()
                );
                assert!(!ids_of(&body).is_empty(), "limit {limit:?}, detail {detail:?}: empty page");
                assert_eq!(body["hasMore"], true, "limit {limit:?}, detail {detail:?}: 200 rows never fit");
            }
            let (no_limit, _) = outline_call(&store, None, None, detail);
            let (limit_200, _) = outline_call(&store, Some(200), None, detail);
            assert_eq!(no_limit, limit_200, "detail {detail:?}: no limit must mean limit 200");
        }
    }

    /// B2: following `nextCursor` across byte-cut pages, switching `detail`
    /// between pages, returns every row exactly once, in source order. 250
    /// rows so the SQL page itself also has a next page to resume from.
    #[test]
    fn following_the_cursor_across_byte_cuts_returns_every_row_once() {
        const N: usize = 250;
        let store = big_file(N, 200);
        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= N, "paging never ended after {N} pages");
            let detail = if pages % 2 == 0 { OutlineDetail::Full } else { OutlineDetail::Compact };
            let (text, body) = outline_call(&store, Some(200), cursor.clone(), Some(detail));
            assert!(text.len() <= BUDGET, "page {pages}: {} bytes", text.len());
            let ids = ids_of(&body);
            assert!(!ids.is_empty(), "page {pages} is empty");
            seen.extend(ids);
            if body["hasMore"] == false {
                assert!(body.get("total").is_none(), "the last page carries no total");
                break;
            }
            assert_eq!(body["total"], N, "page {pages}: a cut page carries the file's row count");
            cursor = Some(body["nextCursor"].as_str().expect("hasMore implies a cursor").to_string());
        }
        let expected: Vec<String> = (0..N).map(row_id).collect();
        assert_eq!(seen, expected, "every row exactly once, in source order");
        assert!(pages > 2, "the fixture must need several byte-cut pages, took {pages}");
    }

    /// B3: a page cut by bytes reports `hasMore` and a cursor even when
    /// `limit` covers the whole file (the SQL page says there is no more),
    /// with `total` placed before `hasMore`.
    #[test]
    fn a_byte_cut_sets_has_more_even_when_limit_covers_the_file() {
        let store = big_file(120, 200);
        let (text, body) = outline_call(&store, Some(200), None, Some(OutlineDetail::Full));
        assert!(ids_of(&body).len() < 120, "120 full rows must not fit {BUDGET} bytes");
        assert_eq!(body["hasMore"], true);
        assert!(body["nextCursor"].is_string(), "a cut page must carry a cursor");
        assert_eq!(body["total"], 120);
        assert!(text.contains(r#""total":120,"hasMore":true"#), "total precedes hasMore: {text}");
    }

    /// B4 and "small files unchanged": a file that fits has no `total` key,
    /// and with `detail: "full"` its body is byte-for-byte the pre-GM-523
    /// response, `"signature": null` included.
    #[test]
    fn a_small_file_has_no_total_and_its_full_body_is_unchanged() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..3i64 {
            let id = format!("n{i}");
            let mut node = symbol_at(&id, &id, 10 * i + 3);
            node.start_col = 4;
            node.end_line = 10 * i + 5;
            node.end_col = 6;
            if i == 1 {
                node.signature = Some("fn n1()".to_string());
                node.visibility = "public".to_string();
                node.exported = true;
            }
            upsert_node(&mut conn, node).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }
        let store = Arc::new(IndexStore::new(conn));

        let (_, compact) = outline_call(&store, None, None, None);
        assert!(compact.get("total").is_none(), "a complete page has no total key: {compact}");
        assert_eq!(compact["hasMore"], false);

        let (full, _) = outline_call(&store, None, None, Some(OutlineDetail::Full));
        assert_eq!(
            full,
            concat!(
                r#"{"results":["#,
                r#"{"symbolId":"n0","name":"n0","qualifiedName":"pkg::n0","kind":"Function","startLine":3,"startCol":4,"endLine":5,"endCol":6,"signature":null,"exported":false},"#,
                r#"{"symbolId":"n1","name":"n1","qualifiedName":"pkg::n1","kind":"Function","startLine":13,"startCol":4,"endLine":15,"endCol":6,"signature":"fn n1()","exported":true},"#,
                r#"{"symbolId":"n2","name":"n2","qualifiedName":"pkg::n2","kind":"Function","startLine":23,"startCol":4,"endLine":25,"endCol":6,"signature":null,"exported":false}"#,
                r#"],"hasMore":false,"nextCursor":null}"#,
            )
        );
    }

    /// B5: rows are compact by default (exactly six keys, with `symbolId`)
    /// and full on request (exactly the pre-GM-523 ten keys).
    #[test]
    fn rows_are_compact_by_default_and_full_on_request() {
        let store = big_file(2, 0);
        let compact_keys: std::collections::BTreeSet<String> =
            ["symbolId", "name", "kind", "startLine", "endLine", "exported"].map(String::from).into();
        let mut full_keys = compact_keys.clone();
        full_keys.extend(["qualifiedName", "startCol", "endCol", "signature"].map(String::from));

        for detail in [None, Some(OutlineDetail::Compact)] {
            let (_, body) = outline_call(&store, None, None, detail);
            for row in body["results"].as_array().unwrap() {
                assert_eq!(keys_of(row), compact_keys, "detail {detail:?}");
            }
        }
        let (_, body) = outline_call(&store, None, None, Some(OutlineDetail::Full));
        let rows = body["results"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(keys_of(row), full_keys);
        }
    }

    /// B6: a row that alone is over the budget is still sent, alone, with
    /// `hasMore`, and the next page carries the rest - never an empty page
    /// that pages forever.
    #[test]
    fn a_single_row_over_the_budget_is_sent_alone() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "a.rs", "a.rs", "a.rs", "rust")).unwrap();
        for i in 0..3i64 {
            let id = format!("n{i}");
            let mut node = symbol_at(&id, &id, i);
            if i == 0 {
                node.signature = Some("x".repeat(9_000));
            }
            upsert_node(&mut conn, node).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e{i}"), "file", id, "DEFINES", "tree-sitter", false),
            )
            .unwrap();
        }
        let store = Arc::new(IndexStore::new(conn));

        let (text, first) = outline_call(&store, None, None, Some(OutlineDetail::Full));
        assert_eq!(ids_of(&first), vec!["n0"]);
        assert!(text.len() > BUDGET, "the fixture row must be over the budget on its own");
        assert_eq!(first["hasMore"], true);
        assert_eq!(first["total"], 3);

        let cursor = first["nextCursor"].as_str().map(str::to_string);
        let (_, second) = outline_call(&store, None, cursor, Some(OutlineDetail::Full));
        assert_eq!(ids_of(&second), vec!["n1", "n2"]);
        assert_eq!(second["hasMore"], false);
    }

    // -----------------------------------------------------------------
    // a miss on a path whose language is not indexed at all
    // -----------------------------------------------------------------

    use crate::mcp::not_indexed::test_support::*;

    fn outline_of(conn: Connection, coverage: Option<&PathCoverage>, file_path: &str) -> CallToolResult {
        let params = GetFileOutlineParams { file_path: file_path.to_string(), ..Default::default() };
        handle_covered(&Arc::new(IndexStore::new(conn)), coverage, params).unwrap()
    }

    /// An absent language's file is refused with the
    /// structured reason and the install command.
    ///
    /// Control: in `handle_covered`'s miss arm, call
    /// `tool_result::error(..)` instead of `not_indexed::miss` - the body is
    /// not JSON.
    #[test]
    fn an_absent_languages_file_is_refused_with_the_install_command() {
        let result = outline_of(setup(), Some(&python_absent()), "tools/gen.py");
        assert_python_absent_refusal(
            &refusal_body(&result),
            "g-mesh: no file 'tools/gen.py' found in the index",
        );
    }

    /// A failed language's file names `g-mesh reindex`
    /// and the innermost recorded cause.
    ///
    /// Control: the same as the absent test; or make
    /// `PathCoverage::Failed` read as absent in `from_coverage`.
    #[test]
    fn a_failed_languages_file_is_refused_with_the_reindex_command() {
        let conn = setup();
        record_failed(&conn, "python");
        let result = outline_of(conn, Some(&python_failed()), "tools/gen.py");
        assert_python_failed_refusal(
            &refusal_body(&result),
            "g-mesh: no file 'tools/gen.py' found in the index",
        );
    }

    /// A covered language's miss stays the plain message.
    ///
    /// Control: make `not_indexed::miss`'s `None` arm build a refusal.
    #[test]
    fn a_covered_languages_miss_stays_the_plain_message() {
        let result = outline_of(setup(), None, "src/nope.rs");
        assert_eq!(plain_error(&result), "g-mesh: no file 'src/nope.rs' found in the index");
    }

    /// An indexed file answers normally even when told
    /// its language is uncovered - coverage is read only on a miss.
    ///
    /// Control: consult `coverage` before the file lookup (refuse whenever
    /// it is `Some`) - the hit becomes an error.
    #[test]
    fn a_hit_carries_no_not_indexed_key_whatever_the_coverage() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "gen.py", "gen.py", "tools/gen.py", "python"))
            .unwrap();
        let body = json_body(&outline_of(conn, Some(&python_absent()), "tools/gen.py"));
        assert!(body.get("notIndexed").is_none(), "{body}");
        assert!(body.get("results").is_some(), "{body}");
    }
}
