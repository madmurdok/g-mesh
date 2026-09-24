//! Real logic behind the `search_code` MCP tool: embed the caller's free-text
//! query with the project's [`EmbeddingPipeline`], then rank stored node
//! vectors against it by cosine similarity via
//! `graph::pagination::paginate_by_score` - the generic score-ranked cursor
//! this tool was always meant to drive (see that function's own doc comment).
//!
//! Unlike every other tool in this module, there is no anchor node: the query
//! is arbitrary text, not a symbol already in the graph, so there is nothing
//! to resolve before searching and no `still_indexing`-style staleness check
//! beyond the shared one `prepare` already runs.

use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination;
use crate::storage::vectors::pack;

use super::similarity;
use super::tool_result::{error, internal_error, success};
use super::SearchCodeParams;

/// One matched symbol, ranked by similarity to the query.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchResult {
    pub(super) symbol_id: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    pub(super) file_path: String,
    start_line: i64,
    start_col: i64,
    /// Cosine similarity to the query, in `[-1.0, 1.0]` (in practice close to
    /// `[0.0, 1.0]` for related code text - both sides are L2-normalized, so
    /// this is exactly `1 - cosine_distance`). Higher is more similar; this
    /// is also the column `paginate_by_score` orders and pages by.
    pub(super) score: f64,
    /// The declaration's own `nodes.language`, read for
    /// [`super::similarity::floor`] and never serialized: the floor a row is
    /// judged against is a property of the language its text was written in,
    /// and the caller is handed the verdict rather than the inputs to it -
    /// see `super::similarity`'s doc comment on why the constant stays off
    /// the wire. Twenty copies of `"language":"typescript"` on a page whose
    /// rows are nearly always one language would also be the per-row
    /// repetition of a per-call fact that `super::provenance` argues against.
    #[serde(skip)]
    pub(super) language: String,
}

#[cfg(test)]
impl SearchResult {
    /// The two fields `super::similarity`'s tests vary, with the rest filled
    /// in. A constructor rather than `..Default::default()` because the two
    /// that matter are then impossible to omit by accident - a row with a
    /// default score of `0.0` would make every one of those tests pass for
    /// the wrong reason.
    pub(super) fn for_test(score: f64, language: &str) -> Self {
        Self {
            symbol_id: "n".to_string(),
            qualified_name: "pkg::n".to_string(),
            kind: "Function".to_string(),
            file_path: "a.rs".to_string(),
            start_line: 1,
            start_col: 0,
            score,
            language: language.to_string(),
        }
    }
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type. No `all_unresolved` field - unlike an edge
/// walk, a similarity-ranked page has no per-row linker-confidence concept to
/// report (mirrors `get_file_outline`'s `OutlinePage`, ranked by source
/// position for the same reason).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchPage {
    results: Vec<SearchResult>,
    has_more: bool,
    next_cursor: Option<String>,
    /// The verdict [`super::similarity::verdict`] reached about this page, and
    /// the only shape this tool has ever had that means *no* - see that
    /// module's doc comment. Absent on a page that matched, and absent on a
    /// continuation page, where the scores this verdict is computed from are
    /// not in hand.
    #[serde(skip_serializing_if = "Option::is_none")]
    no_match: Option<similarity::NoMatch>,
}

/// Ranks every embedded node against `query` and paginates the result.
/// Split out from `handle` so tests can drive it with a small `page_size`
/// without needing a page-size field on the public tool parameters - and,
/// since the resolution ladder's semantic rung was added, so that rung asks
/// the same question this tool does rather than growing a second copy of the
/// SQL that would drift from it.
pub(super) fn search(
    conn: &Connection,
    query: &[f32],
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<SearchResult>> {
    // `v.nodeId AS id` and the distance-to-similarity flip are the only
    // things this base query owes `paginate_by_score`'s contract (a `score`
    // and an `id` column); every other column just rides along for the
    // caller. The inner join is what keeps unembedded nodes (no doc comment
    // or signature - see `embedding::pipeline::text_to_embed`) out of the
    // results without a separate filter.
    let base_sql = "SELECT v.nodeId AS id, n.qualifiedName, n.kind, n.filePath, n.startLine, n.startCol, \
                     n.language, \
                     (1.0 - vec_distance_cosine(v.embedding, ?1)) AS score \
                     FROM vectors v JOIN nodes n ON n.id = v.nodeId";
    let packed = pack(query);
    let params: Vec<&dyn rusqlite::ToSql> = vec![&packed];

    pagination::paginate_by_score(conn, base_sql, &params, page_size, cursor, |row| {
        let score: f64 = row.get("score")?;
        let id: String = row.get("id")?;
        Ok((
            SearchResult {
                symbol_id: id.clone(),
                qualified_name: row.get("qualifiedName")?,
                kind: row.get("kind")?,
                file_path: row.get("filePath")?,
                start_line: row.get("startLine")?,
                start_col: row.get("startCol")?,
                score,
                language: row.get("language")?,
            },
            score,
            id,
        ))
    })
}

pub(super) fn handle(
    conn: &Arc<Mutex<Connection>>,
    embedding: &EmbeddingPipeline,
    params: SearchCodeParams,
) -> Result<CallToolResult, ErrorData> {
    let Some(query_vector) = embedding.embed_query(&params.query) else {
        return error(
            "g-mesh: semantic search is not available for this project - no embedding model \
             is loaded. Run `g-mesh model fetch` and restart the daemon, or use the \
             structural tools (find_references, find_definition, ...) instead.",
        );
    };

    let conn = conn.lock().unwrap();
    let page_size = pagination::resolve_page_size(params.limit);
    let page = search(&conn, &query_vector, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to search code", e))?;

    let no_match = similarity::verdict(&params.query, params.cursor.as_deref(), &page.results);

    success(&SearchPage {
        results: page.results,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        no_match,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedding::model::MODEL_DIR_ENV;
    use crate::storage::schema;
    use crate::storage::vectors::{insert, register_extension};

    fn setup() -> Connection {
        register_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn insert_node(conn: &Connection, id: &str, qualified_name: &str, file_path: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?2, ?3, 1, 0, 3, 1, 'rust')",
            rusqlite::params![id, qualified_name, file_path],
        )
        .unwrap();
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

    /// The unit-level half of the ranking behavior: given vectors already in
    /// the table (no real model needed here - see `search`'s split-out from
    /// `handle`), the nearest one by direction ranks first and carries the
    /// highest score.
    #[test]
    fn the_nearest_vector_ranks_first_with_the_highest_score() {
        let conn = setup();
        insert_node(&conn, "alpha", "pkg::alpha", "a.rs");
        insert_node(&conn, "beta", "pkg::beta", "b.rs");
        insert_node(&conn, "gamma", "pkg::gamma", "c.rs");
        insert(&conn, "alpha", &[1.0, 0.0, 0.0], "v1").unwrap();
        insert(&conn, "beta", &[0.0, 1.0, 0.0], "v1").unwrap();
        insert(&conn, "gamma", &[0.0, 0.0, 1.0], "v1").unwrap();

        let page = search(&conn, &[0.9, 0.05, 0.05], 10, None).unwrap();
        assert_eq!(page.results.len(), 3);
        assert_eq!(page.results[0].symbol_id, "alpha");
        assert!(
            page.results[0].score > page.results[1].score && page.results[0].score > page.results[2].score,
            "the nearest result must carry the highest score: {:?}",
            page.results.iter().map(|r| (r.symbol_id.as_str(), r.score)).collect::<Vec<_>>()
        );
    }

    /// A node with no vector row at all (nothing to embed - see
    /// `embedding::pipeline::text_to_embed`) must never appear, without a
    /// separate filter beyond the inner join.
    #[test]
    fn a_node_with_no_vector_row_never_appears() {
        let conn = setup();
        insert_node(&conn, "embedded", "pkg::embedded", "a.rs");
        insert_node(&conn, "unembedded", "pkg::unembedded", "b.rs");
        insert(&conn, "embedded", &[1.0, 0.0, 0.0], "v1").unwrap();

        let page = search(&conn, &[1.0, 0.0, 0.0], 10, None).unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].symbol_id, "embedded");
    }

    #[test]
    fn zero_matches_is_an_empty_page_not_an_error() {
        let conn = setup();
        let page = search(&conn, &[1.0, 0.0, 0.0], 10, None).unwrap();
        assert!(page.results.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn search_paginates_across_cursor_continuation() {
        let conn = setup();
        for i in 0..5 {
            let id = format!("n{i}");
            insert_node(&conn, &id, &format!("pkg::{id}"), "a.rs");
            // Distinct, decreasing similarity to [1.0, 0.0, ...] as i grows.
            insert(&conn, &id, &[1.0 - (i as f32) * 0.1, i as f32 * 0.1, 0.0], "v1").unwrap();
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = search(&conn, &[1.0, 0.0, 0.0], 2, cursor.as_deref()).unwrap();
            seen.extend(page.results.into_iter().map(|r| r.symbol_id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        assert_eq!(
            seen,
            vec!["n0", "n1", "n2", "n3", "n4"],
            "every match must appear exactly once, most similar first"
        );
    }

    /// `handle`'s own contract when no model is loaded: a tool-level error
    /// naming the fetch script, not a crash or a silently empty page - an
    /// empty page here would be indistinguishable from "no matches" (the same
    /// reasoning task 104 gives for `allUnresolved`, and GM-394's cold-start
    /// wait gives for never answering a tool call "not ready" instead).
    #[test]
    fn handle_reports_a_tool_error_when_no_embedding_model_is_loaded() {
        let conn = Arc::new(Mutex::new(setup()));
        let embedding = EmbeddingPipeline::disabled();

        let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
        let result = handle(&conn, &embedding, params).unwrap();
        assert!(error_text(&result).contains("g-mesh model fetch"));
    }

    /// A disabled pipeline (`MODEL_DIR_ENV` pointed at nothing) is the same
    /// "not available" path a project that never fetched weights hits -
    /// `EmbeddingPipeline::load` fails its own `Path::exists()` check before
    /// touching the network or the filesystem further, matching
    /// `cold_start_grace_wait.rs`'s use of the same override.
    #[test]
    fn handle_reports_a_tool_error_when_the_configured_model_is_unavailable() {
        std::env::set_var(MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        let conn = Arc::new(Mutex::new(setup()));
        let embedding = EmbeddingPipeline::load(&crate::config::EmbeddingConfig::default());

        let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
        let result = handle(&conn, &embedding, params).unwrap();
        assert!(error_text(&result).contains("g-mesh model fetch"));
        std::env::remove_var(MODEL_DIR_ENV);
    }

    /// `search`'s own page-size handling, exercised directly the same way
    /// `find_references`' equivalent test drives `list_references` - a real
    /// model is only needed to turn a caller's text into `query`, never to
    /// rank or paginate rows already in `vectors`.
    #[test]
    fn a_large_page_size_returns_every_match_in_one_call() {
        let conn = setup();
        for i in 0..25 {
            let id = format!("n{i}");
            insert_node(&conn, &id, &format!("pkg::{id}"), "a.rs");
            insert(&conn, &id, &[1.0, i as f32 * 0.01, 0.0], "v1").unwrap();
        }

        let page = search(&conn, &[1.0, 0.0, 0.0], 25, None).unwrap();
        assert_eq!(page.results.len(), 25, "all 25 must come back in one page");
        assert!(!page.has_more);
    }

    /// Inserts a node whose language is `language`, so a test can drive
    /// `similarity::floor`'s per-language table through the real handler.
    fn insert_node_in(conn: &Connection, id: &str, language: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, 'a.ts', 1, 0, 3, 1, ?2)",
            rusqlite::params![id, language],
        )
        .unwrap();
    }

    /// GM-381's wire contract, and its control: the same index and the same
    /// page shape, one query that clears TypeScript's floor and one that does
    /// not. The healthy page must carry no `noMatch` key at all - not `null`,
    /// not an empty object - or the field is a footnote rather than a signal.
    #[test]
    fn a_page_below_the_floor_says_no_and_a_page_above_it_says_nothing() {
        let conn = setup();
        insert_node_in(&conn, "ts", "typescript");
        // Cosine 1.0 against [1,0] and 0.0 against [0,1]: comfortably either
        // side of typescript's floor, whatever the table says it is.
        insert(&conn, "ts", &[1.0, 0.0], "v1").unwrap();

        let matched = search(&conn, &[1.0, 0.0], 10, None).unwrap();
        let missed = search(&conn, &[0.0, 1.0], 10, None).unwrap();

        assert_eq!(super::super::similarity::verdict("reads a file", None, &matched.results), None);
        let verdict = super::super::similarity::verdict("reads a file", None, &missed.results)
            .expect("a page scoring 0.0 cannot be a match");
        let body: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&SearchPage {
                results: missed.results,
                has_more: false,
                next_cursor: None,
                no_match: Some(verdict),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(body["noMatch"]["reason"], "belowSimilarityFloor");
        let healthy: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&SearchPage {
                results: matched.results,
                has_more: false,
                next_cursor: None,
                no_match: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(healthy.get("noMatch").is_none(), "a matching page must carry no key at all: {healthy}");
    }

    /// The other half of that contract, against a real paginated index: the
    /// second page of a search holds rows that are all below the floor - it is
    /// the tail of a ranking, so of course it does - and must still carry no
    /// verdict. The first page of the same search is the control, and it does
    /// carry one, so the two arms differ only in the cursor.
    #[test]
    fn a_continuation_page_carries_no_verdict_although_its_rows_are_all_below_the_floor() {
        let conn = setup();
        for i in 0..4 {
            let id = format!("n{i}");
            insert_node_in(&conn, &id, "typescript");
            // Nearly orthogonal to the query below, so every row on both
            // pages scores far under typescript's floor.
            insert(&conn, &id, &[0.01 * (i as f32) + 0.01, 1.0, 0.0], "v1").unwrap();
        }
        let first = search(&conn, &[1.0, 0.0, 0.0], 2, None).unwrap();
        assert!(first.has_more, "the fixture must produce a second page");
        let cursor = first.next_cursor.clone().unwrap();
        let second = search(&conn, &[1.0, 0.0, 0.0], 2, Some(&cursor)).unwrap();
        assert!(
            second.results.iter().all(|r| r.score < super::super::similarity::floor("typescript")),
            "the fixture must put the whole continuation under the floor: {:?}",
            second.results.iter().map(|r| r.score).collect::<Vec<_>>()
        );

        assert!(
            super::super::similarity::verdict("reads a file", None, &first.results).is_some(),
            "the control: the first page of this same search is a no"
        );
        assert_eq!(
            super::super::similarity::verdict("reads a file", Some(&cursor), &second.results),
            None,
            "a continuation is never judged"
        );
    }

    /// Task #50's acceptance criterion, end to end: a free-text query
    /// semantically related to a symbol's own doc comment ranks that symbol
    /// above an unrelated one - both sides (the stored node vectors via
    /// `embed_node`, the query vector via `handle`'s call to `embed_query`)
    /// going through the same real model, not synthetic vectors like the
    /// tests above.
    #[test]
    #[ignore = "needs the real model weights; run `g-mesh model fetch` first"]
    fn a_query_related_to_a_docstring_ranks_that_symbol_above_an_unrelated_one() {
        let dir = crate::embedding::default_model_dir(&crate::config::EmbeddingConfig::default().model)
            .expect("failed to resolve the default model directory");
        assert!(
            dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists(),
            "real model weights are required for this test; run `g-mesh model fetch` first"
        );
        let model = crate::embedding::EmbeddingModel::load(&dir).expect("failed to load the real model");

        let conn = setup();
        insert_node(&conn, "reader", "pkg::readFileAsString", "a.rs");
        insert_node(&conn, "math", "pkg::unrelatedMathHelper", "b.rs");
        crate::embedding::pipeline::embed_node(
            &model,
            &conn,
            "reader",
            Some("Reads a file from disk and returns its contents as a string."),
            Some("readFileAsString(path: string): string"),
            "jina-embeddings-v2-base-code",
        )
        .unwrap();
        crate::embedding::pipeline::embed_node(
            &model,
            &conn,
            "math",
            None,
            Some("unrelatedMathHelper(a: number, b: number): number"),
            "jina-embeddings-v2-base-code",
        )
        .unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let embedding = EmbeddingPipeline::load(&crate::config::EmbeddingConfig::default());
        let params = SearchCodeParams {
            query: "load the contents of a file from the filesystem".to_string(),
            ..Default::default()
        };
        let body = json_body(&handle(&conn, &embedding, params).unwrap());

        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both embedded symbols must come back");
        assert_eq!(
            results[0]["symbolId"], "reader",
            "the symbol whose docstring matches the query must rank first: {results:?}"
        );
    }
}
