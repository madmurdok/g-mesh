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

use super::tool_result::{internal_error, success};
use super::{anchor, SymbolQueryParams};

/// Every edge kind that means "this node uses the anchor somewhere in its own
/// source". The extractor files each usage under exactly one of these and
/// never duplicates it: a resolved call becomes a `CALLS` edge and `addUsage`
/// then explicitly skips the `REFERENCES` one for that pair, and a
/// supertype clause becomes only a `SUPERTYPE_OF` edge (see
/// plugins/typescript/src/extract.ts). So filtering on `REFERENCES` alone hides
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
///
/// No `name` field: it never carries information `qualifiedName` doesn't
/// already have (at worst a shorter, less unique view of the same symbol).
/// `qualifiedName`/`startLine`/`startCol` are `None` - omitted from the wire
/// JSON entirely via `skip_serializing_if`, never emitted as `null` or `0` -
/// exactly when `kind` is [`pagination::FILE_KIND`]; see that constant's doc
/// comment for why both are pure redundancy on a `File`-kind row.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceSite {
    referencing_symbol_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    kind: String,
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_col: Option<i64>,
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
    /// See `anchor::AnchorInfo` - what `symbol_id`/`symbol_name` resolved to,
    /// so a caller asking about usages "elsewhere" doesn't need a separate
    /// `find_definition` call just to learn the anchor's own file/line.
    anchor: anchor::AnchorInfo,
    results: Vec<ReferenceSite>,
    /// Every file holding a reference to the anchor, with a per-file count,
    /// computed over the whole edge set rather than this page - see
    /// `pagination::tally_edge_files`. Present only when it says something
    /// `results` doesn't already say (`pagination::tally_is_worth_sending`):
    /// the page is incomplete, or its rows repeat files. Omitted entirely -
    /// not sent as an empty array - otherwise, so an exact narrow lookup's
    /// response is byte-for-byte what it was before this field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<Vec<pagination::FileTally>>,
    has_more: bool,
    next_cursor: Option<String>,
    /// See `Page::all_unresolved` - true when every reference in `results`
    /// came from an edge the linker couldn't confirm.
    all_unresolved: bool,
    /// See `anchor::file_anchor_hint` - present only when the anchor resolved
    /// to a `File` node, absent (not `null`) on every ordinary symbol anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

/// Paginates the incoming `USAGE_EDGE_KINDS` edges for `anchor_id` and
/// resolves each one to its referencing node. Split out from `handle` so
/// tests can drive it with a small `page_size` without needing a page-size
/// field on the public tool parameters.
fn list_references(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    file_paths: &[&str],
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<ReferenceSite>> {
    let page = pagination::paginate_edges(
        conn,
        anchor_id,
        Direction::Incoming,
        USAGE_EDGE_KINDS,
        file_paths,
        anchor_file_path,
        page_size,
        cursor,
    )
    .context("failed to paginate reference edges")?;

    let mut rows = Vec::with_capacity(page.results.len());
    for pagination::ScoredEdge { edge, locality } in page.results {
        let referencing = queries::get_node(conn, &edge.from_id)
            .context("failed to resolve referencing node")?
            .with_context(|| format!("edge {} references missing node {}", edge.id, edge.from_id))?;
        let is_file = referencing.kind == pagination::FILE_KIND;
        rows.push(pagination::EdgeRow {
            resolved: edge.resolved,
            locality,
            edge_id: edge.id.clone(),
            item: ReferenceSite {
                referencing_symbol_id: referencing.id,
                qualified_name: (!is_file).then_some(referencing.qualified_name),
                kind: referencing.kind,
                file_path: referencing.file_path,
                start_line: (!is_file).then_some(referencing.start_line),
                start_col: (!is_file).then_some(referencing.start_col),
                reference_kind: edge.kind,
                resolved: edge.resolved,
            },
        });
    }

    Ok(pagination::bound_page_reserving_tally(rows, page.has_more, page.next_cursor))
}

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match anchor::resolve(&conn, &params)? {
        Ok(node) => node,
        Err(finished) => return Ok(finished),
    };
    let hint = anchor::file_anchor_hint(&anchor);
    let anchor_info = anchor::AnchorInfo::from(&anchor);

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let page = list_references(&conn, &anchor.id, &anchor.file_path, &file_paths, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find references", e))?;

    let tally = pagination::tally_edge_files(&conn, &anchor.id, Direction::Incoming, USAGE_EDGE_KINDS, &file_paths)
        .map_err(|e| internal_error("failed to tally referencing files", e))?;
    let files = pagination::tally_is_worth_sending(page.results.len(), &tally, page.has_more).then_some(tally);

    success(&ReferencePage {
        anchor: anchor_info,
        results: page.results,
        files,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        all_unresolved: page.all_unresolved,
        hint,
    })
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
            let page = list_references(&conn, "target", "target.rs", &[], 1, cursor.as_deref()).unwrap();
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
            SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() },
        )
        .unwrap());
        let mut caller_ids: Vec<&str> =
            callers["results"].as_array().unwrap().iter().map(|r| r["callerSymbolId"].as_str().unwrap()).collect();
        caller_ids.sort();
        assert_eq!(caller_ids, vec!["caller_a", "caller_b"], "precondition: find_callers sees both in-file callers");

        let references =
            json_body(&handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap());
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

    /// The `files` tally is the file-level answer an impact question wants,
    /// and it has to be complete even when `results` isn't: five references
    /// spread over three files, read one row at a time, must still report all
    /// three files on the very first page.
    #[test]
    fn files_tally_covers_every_file_even_when_the_page_is_truncated() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        for (i, file) in ["a.ts", "a.ts", "a.ts", "b.ts", "c.ts"].iter().enumerate() {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, &id, *file, "typescript")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let body = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(1), ..Default::default() }).unwrap(),
        );
        assert_eq!(body["results"].as_array().unwrap().len(), 1, "precondition: limit 1 shows a single row");
        assert_eq!(body["hasMore"], true, "precondition: four more references are behind the cursor");

        let files = body["files"].as_array().expect("a truncated page must carry the whole-set files tally");
        let listed: Vec<(&str, i64)> =
            files.iter().map(|f| (f["path"].as_str().unwrap(), f["refs"].as_i64().unwrap())).collect();
        assert_eq!(
            listed,
            vec![("a.ts", 3), ("b.ts", 1), ("c.ts", 1)],
            "every referencing file, counted over the whole edge set and ordered by count descending"
        );
    }

    /// The gate that keeps the tally free on exact, narrow lookups - the
    /// ambiguous-name category's whole shape. One reference per file on a
    /// complete page means the tally would restate the `filePath` column
    /// verbatim, so it must not be sent at all (absent, not an empty array).
    #[test]
    fn files_tally_is_omitted_when_the_rows_already_are_the_file_answer() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        for (i, file) in ["a.ts", "b.ts"].iter().enumerate() {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, &id, *file, "typescript")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let body = json_body(&handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap());
        assert_eq!(body["hasMore"], false, "precondition: the page is complete");
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert!(body.get("files").is_none(), "a one-row-per-file complete page must not repeat itself as a tally: {body}");
    }

    /// Same gate from the other side: a complete page whose rows repeat files
    /// still gets a tally, because deduplicating those rows by hand is
    /// exactly the work the tally exists to remove.
    #[test]
    fn files_tally_is_sent_when_a_complete_page_repeats_files() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        for (i, file) in ["a.ts", "a.ts", "b.ts"].iter().enumerate() {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, &id, *file, "typescript")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let body = json_body(&handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap());
        assert_eq!(body["hasMore"], false);
        let files = body["files"].as_array().expect("three rows over two files leave real deduplication to do");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "a.ts");
        assert_eq!(files[0]["refs"], 2);
    }

    /// `file_paths` scopes the rows, so it has to scope the tally the same
    /// way - a tally wider than the page it accompanies would report files
    /// the caller explicitly excluded.
    #[test]
    fn files_tally_respects_the_file_paths_scope() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        for (i, file) in ["a.ts", "a.ts", "b.ts"].iter().enumerate() {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, &id, *file, "typescript")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let body = json_body(
            &handle(
                &conn,
                SymbolQueryParams {
                    symbol_id: Some("target".to_string()),
                    file_paths: Some(vec!["a.ts".to_string()]),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let files = body["files"].as_array().expect("two in-scope rows in one file still leave deduplication to do");
        assert_eq!(files.len(), 1, "b.ts is out of scope and must not appear in the tally: {body}");
        assert_eq!(files[0]["path"], "a.ts");
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

        let page = list_references(&conn, "base", "base.ts", &[], 10, None).unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].referencing_symbol_id, "sub");
        assert_eq!(page.results[0].reference_kind, "SUPERTYPE_OF");
    }

    /// A top-level reference with no enclosing function attributes to the
    /// `File` node itself (`extract.ts`'s "degrades to a usage edge from the
    /// File node" case) - the shape that motivated trimming `name` and
    /// omitting `qualifiedName`/`startLine`/`startCol` from `File`-kind rows:
    /// they duplicate `filePath` (`qualifiedName`) or are meaningless
    /// (`startLine`/`startCol`, always the file's own root position). Both
    /// must be entirely absent from the JSON, not present as duplicated text
    /// or as `0`/`0`. Symbol-kind rows are untouched by this and keep both.
    #[test]
    fn a_file_kind_referencing_row_omits_qualified_name_and_position_but_keeps_them_for_symbol_kind_rows() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "index.ts", "src/index.ts", "src/index.ts", "typescript")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller", "Function", "caller", "pkg::caller", "caller.ts", "typescript"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_file", "file", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_caller", "caller", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let body =
            json_body(&handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap());
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);

        for row in results {
            assert!(row.get("name").is_none(), "the name field must never be present on any row: {row}");
        }

        let file_row = results.iter().find(|r| r["kind"] == "File").expect("the File-kind row must be present");
        assert!(file_row.get("qualifiedName").is_none(), "qualifiedName duplicates filePath for a File-kind row: {file_row}");
        assert!(file_row.get("startLine").is_none(), "startLine is meaningless for a File-kind row: {file_row}");
        assert!(file_row.get("startCol").is_none(), "startCol is meaningless for a File-kind row: {file_row}");
        assert_eq!(file_row["filePath"], "src/index.ts");

        let symbol_row = results.iter().find(|r| r["kind"] == "Function").expect("the Function-kind row must be present");
        assert_eq!(symbol_row["qualifiedName"], "pkg::caller", "a symbol-kind row must still carry its qualifiedName");
        assert_eq!(symbol_row["startLine"], 0, "a symbol-kind row must still carry its startLine");
        assert_eq!(symbol_row["startCol"], 0, "a symbol-kind row must still carry its startCol");
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

        let page = list_references(&conn, "target", "tasks.ts", &[], 10, None).unwrap();
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
            let page = list_references(&conn, "target", "target.ts", &[], 1, cursor.as_deref()).unwrap();
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

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(25), ..Default::default() };
        let body = json_body(&handle(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    /// Reproduces the benchmark's own membership-test shape (g-mesh-bench's
    /// v0.4.0 outlier findings, section 3b): "of these N known files, which
    /// ones reference symbol S?" Five known candidate files stand in for the
    /// five Trail-implementation files from that corpus - three of them
    /// actually reference the target, one is a known file that happens not
    /// to, and a sixth file outside the known set also references it but
    /// must not leak into a scoped answer. One call with `file_paths` set to
    /// the five known files must return exactly the three that reference it,
    /// where previously this required either one unscoped call plus manual
    /// filtering, or one grep per candidate file.
    #[test]
    fn scoping_to_five_known_files_returns_exactly_the_ones_that_reference_it() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "pointFrom", "pkg::pointFrom", "target.ts", "typescript")).unwrap();

        let known_files = ["trail1.ts", "trail2.ts", "trail3.ts", "trail4.ts", "trail5.ts"];
        for (i, file) in known_files.iter().enumerate() {
            let id = format!("known_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), *file, "typescript")).unwrap();
            // Only the first three known files actually reference the target;
            // trail4/trail5 are known candidates that turn out not to call it.
            if i < 3 {
                upsert_edge(&mut conn, EdgeRecord::new(format!("e_known_{i}"), &id, "target", "REFERENCES", "tree-sitter", true))
                    .unwrap();
            }
        }
        // A reference from outside the known set - must never appear in a
        // scoped answer, however many other unrelated files reference the
        // symbol too.
        upsert_node(&mut conn, NodeRecord::new("outsider", "Function", "outsider", "pkg::outsider", "unrelated.ts", "typescript"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_outsider", "outsider", "target", "REFERENCES", "tree-sitter", true)).unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            file_paths: Some(known_files.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let body = json_body(&handle(&conn, params).unwrap());
        let mut file_paths: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        file_paths.sort();
        assert_eq!(
            file_paths,
            vec!["trail1.ts", "trail2.ts", "trail3.ts"],
            "must return exactly the known files that reference the target, in one call"
        );
    }

    /// A file outside the scope that also references the target must not
    /// appear, and the row count must reflect the narrowed set, not the
    /// project-wide one.
    #[test]
    fn scoping_find_references_excludes_a_referencing_file_outside_the_scope() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("in_scope", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("out_of_scope", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "in_scope", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "out_of_scope", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            file_paths: Some(vec!["a.rs".to_string()]),
            ..Default::default()
        };
        let body = json_body(&handle(&conn, params).unwrap());
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "the out-of-scope file's reference must not come back");
        assert_eq!(results[0]["referencingSymbolId"], "in_scope");
    }

    /// Omitting `file_paths` entirely must be indistinguishable from a call
    /// made before this parameter existed - the whole point of "empty/absent
    /// means unfiltered" is that existing callers see no behavior change.
    #[test]
    fn omitting_file_paths_behaves_identically_to_before_this_change() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let omitted = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap(),
        );
        let explicit_empty = json_body(
            &handle(
                &conn,
                SymbolQueryParams { symbol_id: Some("target".to_string()), file_paths: Some(Vec::new()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(omitted, explicit_empty, "an explicit empty file_paths array must answer exactly as omitting it does");
        assert_eq!(omitted["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn zero_references_is_an_empty_page_not_an_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["allUnresolved"], false, "an empty page has nothing to be suspicious of");
    }

    /// The gap this field closes: a caller list-shaped like a complete
    /// answer (`hasMore: false`, a plausible non-empty `results`) but built
    /// entirely from name-matched edges the linker couldn't confirm - the
    /// benchmark shape (g-mesh #104) where this was easy to mistake for a
    /// verified, complete result.
    #[test]
    fn a_page_where_every_reference_is_unresolved_is_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "REFERENCES", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(body["allUnresolved"], true, "every row unresolved must set the response-level marker");
    }

    /// A single confirmed row is enough to clear the marker, even though most
    /// of the page is still unresolved - the field means "the whole answer
    /// is unconfirmed", not "some rows are".
    #[test]
    fn a_page_with_at_least_one_resolved_reference_is_not_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "REFERENCES", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "REFERENCES", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 3);
        assert_eq!(body["allUnresolved"], false, "one resolved row among several unresolved ones must clear the marker");
    }

    /// The point of task #190: the response itself says what it anchored on,
    /// so a caller asking about usages "elsewhere" (excluding the definition
    /// file) can read the anchor's own `filePath` straight off this response
    /// instead of issuing a separate `find_definition` call first.
    #[test]
    fn the_response_echoes_the_resolved_anchor() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["anchor"]["id"], "target");
        assert_eq!(body["anchor"]["qualifiedName"], "pkg::run");
        assert_eq!(body["anchor"]["kind"], "Function");
        assert_eq!(body["anchor"]["filePath"], "target.rs");
        assert_eq!(body["anchor"]["startLine"], 0);
    }

    /// The whole point of `symbol_name`: an unambiguous name lands on the
    /// same anchor a `find_definition`-then-`find_references` pair would
    /// have, in one call instead of two.
    #[test]
    fn an_unambiguous_symbol_name_anchors_the_walk_without_a_symbol_id() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "REFERENCES", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let by_id = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() }).unwrap(),
        );
        let by_name = json_body(
            &handle(&conn, SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() }).unwrap(),
        );
        assert_eq!(by_name, by_id, "a name that resolves to one node must answer exactly as its id does");
        assert_eq!(by_name["results"][0]["referencingSymbolId"], "caller_a");
    }

    #[test]
    fn unknown_symbol_name_is_a_tool_level_error() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_name: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    /// The footgun this hint closes: `symbol_name` resolves through
    /// `queries::find_by_name`/`find_by_qualified_name`, neither of which
    /// filter by kind, so a name that happens to match a file's basename
    /// (e.g. `"connection.ts"`) anchors on that `File` node exactly as if it
    /// were a declared symbol. `find_references` never walks an edge kind
    /// incident on a File node in this direction, so the answer is a
    /// perfectly honest but useless empty page - unless the response tells
    /// the caller to retry with `get_dependencies` instead.
    #[test]
    fn a_file_anchor_carries_a_hint_pointing_at_get_dependencies() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "connection.ts", "src/connection.ts", "src/connection.ts", "typescript"))
            .unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: Some("file".to_string()), ..Default::default() };
        let body = json_body(&handle(&conn, params).unwrap());

        let hint = body["hint"].as_str().expect("a File-anchored call must carry a hint");
        assert!(hint.contains("get_dependencies"), "the hint must point at get_dependencies: {hint}");
        assert_eq!(body["results"].as_array().unwrap().len(), 0, "a File anchor still answers as an empty page, not an error");
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["allUnresolved"], false);
    }

    /// Purely additive: an ordinary symbol anchor must never carry the
    /// `hint` field at all, not even as `null` - mirrors how the File-kind
    /// row tests assert `qualifiedName`/`startLine` are absent, not null.
    #[test]
    fn a_normal_symbol_anchor_never_carries_a_hint_field() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert!(body.get("hint").is_none(), "hint must be entirely absent, not null, on a normal symbol anchor: {body}");
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
            let params = SymbolQueryParams { symbol_id: Some("target".to_string()), cursor: cursor.clone(), ..Default::default() };
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
