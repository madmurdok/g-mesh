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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::prelude::{Engine, BASE64_STANDARD};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::daemon::indexing_status::Phase;
use crate::embedding::rerank::{self, Scorer};
use crate::embedding::text::text_to_embed;
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination;
use crate::storage::index_store::IndexStore;
use crate::storage::vectors::pack;

use super::query_shapes::QueryShapes;
use super::session_hints::{self, HintKey, SessionHints};
use super::similarity;
use super::tool_result::{error, internal_error, success};
use super::{human_duration, SearchCodeParams};

/// One matched symbol, ranked by relevance to the query.
#[derive(Clone, Serialize)]
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
    /// is also the column `paginate_by_score` orders and pages by. Within a
    /// reranked window (`embedding::rerank`) the rows are ordered by the
    /// cross-encoder's blend, so this score need not fall monotonically
    /// there.
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
    /// [`super::similarity::low_similarity`]'s sentence: the same below-floor
    /// page asked in prose, where the top row may still be right. Never
    /// present together with `noMatch`; absent wherever `noMatch` would be.
    #[serde(skip_serializing_if = "Option::is_none")]
    low_similarity: Option<&'static str>,
    /// `session_hints::SEARCH_HITS`, once per session, on a page with hits
    /// and neither `noMatch` nor `lowSimilarity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
    /// Present only when the embedding pass was still owed: the rows were
    /// ranked from the vectors stored so far.
    #[serde(skip_serializing_if = "Option::is_none")]
    partial: Option<PartialCounts>,
}

/// `IndexingStatus::embed_progress` at the moment the page was ranked.
#[derive(Serialize)]
struct PartialCounts {
    embedded: u64,
    total: u64,
}

/// Where the embedding pass stood when a `search_code` call stopped waiting
/// for it.
enum PassState {
    /// Counting done: `embedded` of `total` computed.
    Running,
    /// Started, with nothing counted yet (`total` is 0).
    Counting,
    /// Not started ([`Phase::Structural`], or a walk under way again).
    NotStarted,
}

/// Why a `search_code` page is partial: the embedding pass had not finished
/// when the call's bounded wait ran out.
pub(super) struct Coverage {
    state: PassState,
    embedded: u64,
    total: u64,
    waited: Duration,
    root: PathBuf,
}

impl Coverage {
    /// `None` once `phase` is [`Phase::Ready`]: every embeddable node has its
    /// vector and the page is complete.
    pub(super) fn partial(
        phase: &Phase,
        embedded: u64,
        total: u64,
        waited: Duration,
        root: &Path,
    ) -> Option<Self> {
        let state = match phase {
            Phase::Ready => return None,
            Phase::Embedding if total > 0 => PassState::Running,
            Phase::Embedding => PassState::Counting,
            _ => PassState::NotStarted,
        };
        Some(Self { state, embedded, total, waited, root: root.to_path_buf() })
    }

    /// The text that leads a partial page, from the pass's own counts and
    /// `stored`, the number of vectors the page was ranked from.
    fn note(&self, stored: u64) -> String {
        let root = self.root.display();
        let waited = human_duration(self.waited);
        let head = match self.state {
            PassState::Running => format!(
                "g-mesh: the embedding pass for {root} is still running (embeddings {} of {} computed, this \
                 call waited {waited}). Only symbols embedded so far were ranked",
                self.embedded, self.total
            ),
            PassState::Counting => format!(
                "g-mesh: the embedding pass for {root} has started but has not yet counted what needs \
                 embedding (this call waited {waited}); {stored} symbols from an earlier pass were ranked"
            ),
            PassState::NotStarted => format!(
                "g-mesh: the embedding pass for {root} has not started yet (this call waited {waited}); \
                 {stored} symbols from an earlier pass were ranked"
            ),
        };
        format!(
            "{head}. A symbol missing from these results may simply not be embedded yet. Call again later \
             for complete results."
        )
    }
}

/// Marks a cursor issued on a partial page: `partial:<stored>:<cursor>`,
/// where `<stored>` is the vector count the page was ranked from. `<stored>`
/// never contains `:`, so the inner cursor is everything after its first
/// `:` after the prefix (it may itself be a `rerank:` cursor).
const PARTIAL_CURSOR_PREFIX: &str = "partial:";

fn partial_cursor(stored: u64, cursor: &str) -> String {
    format!("{PARTIAL_CURSOR_PREFIX}{stored}:{cursor}")
}

/// `(stored, inner cursor)` for a cursor issued on a partial page; `None`
/// for any other cursor.
fn split_partial_cursor(cursor: &str) -> Option<(u64, &str)> {
    let (stored, inner) = cursor.strip_prefix(PARTIAL_CURSOR_PREFIX)?.split_once(':')?;
    Some((stored.parse().ok()?, inner))
}

/// How many vectors [`search`] ranks from.
fn stored_vectors(conn: &Connection) -> anyhow::Result<u64> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM vectors v JOIN nodes n ON n.id = v.nodeId", [], |row| {
            row.get(0)
        })?;
    Ok(u64::try_from(count).unwrap_or(0))
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

/// The first `k` rows [`search`] returns for `query`, as (node id, score):
/// exactly what `search_code` would rank. The embedding eval's harness-parity
/// check (`cli::embed_eval`) compares its own ranking against this.
pub(crate) fn top_k_for_eval(
    conn: &Connection,
    query: &[f32],
    k: usize,
) -> anyhow::Result<Vec<(String, f64)>> {
    let page = search(conn, query, k, None)?;
    Ok(page.results.into_iter().map(|row| (row.symbol_id, row.score)).collect())
}

/// [`handle`] on the blocking pool. Query inference (and the model's load on
/// first use) and the store read block their thread for as long as they
/// take; on an async worker that would stall every other call the daemon is
/// serving. Dropping the returned future (a cancelled call) does not stop
/// the blocking work: it runs to completion and its answer is discarded.
pub(super) async fn handle_off_worker(
    store: Arc<IndexStore>,
    embedding: Arc<EmbeddingPipeline>,
    shapes: Arc<QueryShapes>,
    hints: SessionHints,
    params: SearchCodeParams,
    coverage: Option<Coverage>,
) -> Result<CallToolResult, ErrorData> {
    tokio::task::spawn_blocking(move || {
        handle(&store, &embedding, &shapes, &hints, params, coverage.as_ref())
    })
    .await
    .map_err(|e| internal_error("search_code task failed", e.into()))?
}

/// Answers one `search_code` call. `coverage` is `Some` when the embedding
/// pass is still owed: the page is then ranked from the stored vectors, led
/// by a note, marked `partial`, carries no floor verdict, and its cursor is
/// refused once the stored vector count changes (the continuation would rank
/// a different set).
///
/// Only a first, complete page is reranked (`embedding::rerank`), and its
/// verdict is judged on the embedding order's page, so the verdict never
/// depends on the rerank.
pub(super) fn handle(
    store: &Arc<IndexStore>,
    embedding: &EmbeddingPipeline,
    shapes: &QueryShapes,
    hints: &SessionHints,
    params: SearchCodeParams,
    coverage: Option<&Coverage>,
) -> Result<CallToolResult, ErrorData> {
    let Some(query_vector) = embedding.embed_query(&params.query) else {
        return error(
            "g-mesh: semantic search is not available for this project - no embedding model \
             is loaded. Run `g-mesh model fetch` and restart the daemon, or use the \
             structural tools (find_references, find_definition, ...) instead.",
        );
    };

    let conn = store.read();
    let partial_cursor_in = params.cursor.as_deref().and_then(split_partial_cursor);
    let stored = if coverage.is_some() || partial_cursor_in.is_some() {
        stored_vectors(&conn).map_err(|e| internal_error("failed to count stored vectors", e))?
    } else {
        0
    };
    let cursor = match partial_cursor_in {
        Some((issued, _)) if issued != stored => {
            return error(format!(
                "g-mesh: this cursor came from a page ranked while the embedding pass was still running \
                 ({issued} symbols embedded then, {stored} now), so its continuation would rank a different \
                 set. Call search_code again without `cursor`."
            ));
        }
        Some((_, inner)) => Some(inner),
        None => params.cursor.as_deref(),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    // A first, complete page reranks when the cross-encoder is on and
    // loaded; every other call is ranked exactly as without a rerank.
    let scorer = match (cursor, coverage) {
        (None, None) => embedding.reranker().scorer(),
        _ => None,
    };
    let ranked = match (scorer, cursor.and_then(split_rerank_cursor)) {
        (Some(scorer), _) => {
            first_page_reranked(&conn, embedding, scorer, &params.query, &query_vector, page_size)
        }
        (None, Some(rerank_cursor)) => continue_rerank(&conn, &query_vector, page_size, rerank_cursor),
        (None, None) => search(&conn, &query_vector, page_size, cursor).map(Ranked::plain),
    }
    .map_err(|e| internal_error("failed to search code", e))?;
    let Ranked { page, judged } = ranked;
    let judged = judged.as_deref().unwrap_or(&page.results);

    let no_match = match coverage {
        None => similarity::verdict(shapes, &params.query, cursor, judged),
        Some(_) => similarity::partial_verdict(shapes, &params.query, cursor, judged),
    };
    let low_similarity = match coverage {
        None => similarity::low_similarity(shapes, &params.query, cursor, judged),
        Some(_) => None,
    };
    let next_cursor = match coverage {
        None => page.next_cursor,
        Some(_) => page.next_cursor.map(|next| partial_cursor(stored, &next)),
    };
    let hint = match coverage {
        None => search_hint(&page.results, no_match.is_some() || low_similarity.is_some(), hints),
        Some(_) => None,
    };
    let body = SearchPage {
        results: page.results,
        has_more: page.has_more,
        next_cursor,
        no_match,
        low_similarity,
        hint,
        partial: coverage.map(|c| PartialCounts { embedded: c.embedded, total: c.total }),
    };
    match coverage {
        None => success(&body),
        Some(coverage) => Ok(CallToolResult::success(vec![
            ContentBlock::text(coverage.note(stored)),
            ContentBlock::json(&body)?,
        ])),
    }
}

/// A page ready to answer with, and the rows its verdict is judged on when
/// those are not the page's own: a reranked first page is judged on the
/// embedding order's page, the rows and the page size the verdict has
/// always seen.
struct Ranked {
    page: pagination::Page<SearchResult>,
    judged: Option<Vec<SearchResult>>,
}

impl Ranked {
    fn plain(page: pagination::Page<SearchResult>) -> Self {
        Self { page, judged: None }
    }
}

/// Marks a cursor issued on a reranked first page whose window did not fit:
/// `rerank:<base64 JSON RerankCursor>`.
const RERANK_CURSOR_PREFIX: &str = "rerank:";

/// What a page after a reranked first page continues from: the window rows
/// not shown yet, in reranked order, then the embedding order after the
/// window's last row.
#[derive(Serialize, serde::Deserialize)]
struct RerankCursor {
    rest: Vec<String>,
    /// [`search`]'s cursor at the window's last row; `None` when nothing
    /// ranked after it.
    after: Option<String>,
}

fn rerank_cursor(cursor: &RerankCursor) -> String {
    let json = serde_json::to_vec(cursor).expect("a rerank cursor always serializes");
    format!("{RERANK_CURSOR_PREFIX}{}", BASE64_STANDARD.encode(json))
}

/// The payload of a `rerank:` cursor; `None` for any other cursor.
fn split_rerank_cursor(cursor: &str) -> Option<&str> {
    cursor.strip_prefix(RERANK_CURSOR_PREFIX)
}

fn decode_rerank_cursor(payload: &str) -> anyhow::Result<RerankCursor> {
    let bytes = BASE64_STANDARD.decode(payload).context("invalid rerank cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid rerank cursor payload")
}

/// What a `search_code` cursor carries, decoded through the product's own
/// decoders, for tests that compare cursors with a float tolerance (a score
/// cursor holds the score's exact bits, which differ across platforms).
#[cfg(test)]
#[derive(Debug)]
pub(super) enum CursorParts {
    Score { score: f64, id: String },
    Rerank { rest: Vec<String>, after: Option<Box<CursorParts>> },
    Partial { stored: u64, inner: Box<CursorParts> },
}

#[cfg(test)]
pub(super) fn cursor_parts(cursor: &str) -> anyhow::Result<CursorParts> {
    if let Some((stored, inner)) = split_partial_cursor(cursor) {
        return Ok(CursorParts::Partial { stored, inner: Box::new(cursor_parts(inner)?) });
    }
    if let Some(payload) = split_rerank_cursor(cursor) {
        let RerankCursor { rest, after } = decode_rerank_cursor(payload)?;
        let after = after.as_deref().map(cursor_parts).transpose()?.map(Box::new);
        return Ok(CursorParts::Rerank { rest, after });
    }
    let (score, id) = pagination::score_cursor_parts(cursor)?;
    Ok(CursorParts::Score { score, id })
}

/// A first page with the window reranked (`embedding::rerank`): the
/// embedding ranking's first `max(page_size, WINDOW)` rows, the first
/// `WINDOW` of them reordered. A `page_size` under the window shows the
/// first `page_size` reranked rows and hands out a `rerank:` cursor for the
/// rest. When this call's scoring fails, the page is the plain one.
fn first_page_reranked(
    conn: &Connection,
    embedding: &EmbeddingPipeline,
    scorer: &dyn Scorer,
    query: &str,
    query_vector: &[f32],
    page_size: usize,
) -> anyhow::Result<Ranked> {
    let mut window = search(conn, query_vector, page_size.max(rerank::WINDOW), None)?;
    let reranked = window.results.len().min(rerank::WINDOW);
    if reranked < 2 {
        return search(conn, query_vector, page_size, None).map(Ranked::plain);
    }
    let texts = window_texts(conn, &window.results[..reranked])?;
    let cosines: Vec<f64> = window.results[..reranked].iter().map(|row| row.score).collect();
    let Some(order) = embedding.reranker().order(scorer, query, &texts, &cosines) else {
        return search(conn, query_vector, page_size, None).map(Ranked::plain);
    };

    let judged = window.results[..page_size.min(window.results.len())].to_vec();
    let mut rows: Vec<Option<SearchResult>> = window.results.drain(..).map(Some).collect();
    let tail = rows.split_off(reranked);
    let mut ordered: Vec<SearchResult> =
        order.iter().map(|&i| rows[i].take().expect("the order is a permutation")).collect();
    ordered.extend(tail.into_iter().flatten());

    if ordered.len() <= page_size {
        // The whole window is on this page: `search`'s own cursor, at the
        // page's last embedding-order row, continues after the same set.
        let page = pagination::Page { results: ordered, ..window };
        return Ok(Ranked { page, judged: Some(judged) });
    }
    let rest = ordered.split_off(page_size).into_iter().map(|row| row.symbol_id).collect();
    let next = rerank_cursor(&RerankCursor { rest, after: window.next_cursor.take() });
    let page = pagination::Page { results: ordered, has_more: true, next_cursor: Some(next), ..window };
    Ok(Ranked { page, judged: Some(judged) })
}

/// The text each row was embedded from, which is also what the
/// cross-encoder reads: `text_to_embed` of the node's current doc comment
/// and signature.
fn window_texts(conn: &Connection, rows: &[SearchResult]) -> anyhow::Result<Vec<String>> {
    let mut statement = conn.prepare_cached("SELECT docComment, signature FROM nodes WHERE id = ?1")?;
    rows.iter()
        .map(|row| {
            let (doc_comment, signature): (Option<String>, Option<String>) = statement
                .query_row([&row.symbol_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .unwrap_or_default();
            Ok(text_to_embed(doc_comment.as_deref(), signature.as_deref()).unwrap_or_default())
        })
        .collect()
}

/// A page after a reranked first page: the window rows the cursor still
/// holds, in its order, each re-read with its cosine recomputed and skipped
/// if it no longer has a vector; then the embedding order after the window.
/// No scoring runs here.
fn continue_rerank(
    conn: &Connection,
    query_vector: &[f32],
    page_size: usize,
    payload: &str,
) -> anyhow::Result<Ranked> {
    let RerankCursor { rest, after } = decode_rerank_cursor(payload)?;
    let mut results = rows_by_id(conn, query_vector, &rest)?;
    if results.len() > page_size {
        let remaining = results.split_off(page_size).into_iter().map(|row| row.symbol_id).collect();
        let next = rerank_cursor(&RerankCursor { rest: remaining, after });
        return Ok(Ranked::plain(pagination::Page {
            results,
            has_more: true,
            next_cursor: Some(next),
            all_unresolved: false,
        }));
    }
    let page = match after {
        None => pagination::Page { results, has_more: false, next_cursor: None, all_unresolved: false },
        Some(after) if results.len() == page_size => {
            pagination::Page { results, has_more: true, next_cursor: Some(after), all_unresolved: false }
        }
        Some(after) => {
            let tail = search(conn, query_vector, page_size - results.len(), Some(&after))?;
            results.extend(tail.results);
            pagination::Page { results, ..tail }
        }
    };
    Ok(Ranked::plain(page))
}

/// The rows for `ids` that still have a vector, in `ids`' order, scored as
/// [`search`] scores them.
fn rows_by_id(conn: &Connection, query_vector: &[f32], ids: &[String]) -> anyhow::Result<Vec<SearchResult>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql =
        "SELECT v.nodeId AS id, n.qualifiedName, n.kind, n.filePath, n.startLine, n.startCol, n.language, \
               (1.0 - vec_distance_cosine(v.embedding, ?1)) AS score \
               FROM vectors v JOIN nodes n ON n.id = v.nodeId \
               WHERE v.nodeId IN (SELECT value FROM json_each(?2))";
    let packed = pack(query_vector);
    let ids_json = serde_json::to_string(ids)?;
    let mut statement = conn.prepare(sql)?;
    let mut found: std::collections::HashMap<String, SearchResult> = statement
        .query_map(rusqlite::params![packed, ids_json], |row| {
            let id: String = row.get("id")?;
            Ok((
                id.clone(),
                SearchResult {
                    symbol_id: id,
                    qualified_name: row.get("qualifiedName")?,
                    kind: row.get("kind")?,
                    file_path: row.get("filePath")?,
                    start_line: row.get("startLine")?,
                    start_col: row.get("startCol")?,
                    score: row.get("score")?,
                    language: row.get("language")?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids.iter().filter_map(|id| found.remove(id)).collect())
}

/// `judged` is true when the page carries `noMatch` or `lowSimilarity`: the
/// hint's "once one plausibly matches ... stop" must not sit beside either.
fn search_hint(results: &[SearchResult], judged: bool, hints: &SessionHints) -> Option<&'static str> {
    hints.once(!results.is_empty() && !judged, HintKey::SearchHits, session_hints::SEARCH_HITS)
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
        let conn = Arc::new(IndexStore::new(setup()));
        let embedding = EmbeddingPipeline::disabled();

        let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
        let result =
            handle(&conn, &embedding, QueryShapes::shipped(), &SessionHints::default(), params, None)
                .unwrap();
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
        let conn = Arc::new(IndexStore::new(setup()));
        let embedding = EmbeddingPipeline::load(&crate::config::EmbeddingConfig::default());

        let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
        let result =
            handle(&conn, &embedding, QueryShapes::shipped(), &SessionHints::default(), params, None)
                .unwrap();
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

        assert_eq!(
            super::super::similarity::verdict(QueryShapes::shipped(), "readFile", None, &matched.results),
            None
        );
        let verdict =
            super::super::similarity::verdict(QueryShapes::shipped(), "readFile", None, &missed.results)
                .expect("a page scoring 0.0 cannot be a match");
        let body: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&SearchPage {
                results: missed.results,
                has_more: false,
                next_cursor: None,
                no_match: Some(verdict),
                low_similarity: None,
                hint: None,
                partial: None,
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
                low_similarity: None,
                hint: None,
                partial: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(healthy.get("noMatch").is_none(), "a matching page must carry no key at all: {healthy}");
        assert!(healthy.get("lowSimilarity").is_none(), "{healthy}");
    }

    /// The prose counterpart of the test above: the same missed page, asked
    /// in prose, serializes a `lowSimilarity` string beside its rows and no
    /// `noMatch` key.
    #[test]
    fn a_prose_page_below_the_floor_carries_low_similarity_and_its_rows() {
        let conn = setup();
        insert_node_in(&conn, "ts", "typescript");
        insert(&conn, "ts", &[1.0, 0.0], "v1").unwrap();
        let missed = search(&conn, &[0.0, 1.0], 10, None).unwrap();

        let no_match = similarity::verdict(QueryShapes::shipped(), "reads a file", None, &missed.results);
        let low_similarity =
            similarity::low_similarity(QueryShapes::shipped(), "reads a file", None, &missed.results);
        let body: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&SearchPage {
                results: missed.results,
                has_more: false,
                next_cursor: None,
                no_match,
                low_similarity,
                hint: None,
                partial: None,
            })
            .unwrap(),
        )
        .unwrap();

        assert!(body.get("noMatch").is_none(), "{body}");
        assert!(body["lowSimilarity"].as_str().is_some_and(|s| s.contains("may still be right")), "{body}");
        assert_eq!(body["results"].as_array().unwrap().len(), 1, "the rows stay: {body}");
    }

    #[test]
    fn a_page_with_hits_and_no_verdict_carries_the_search_hint_once_per_session() {
        let hit = || vec![SearchResult::for_test(0.9, "rust")];
        let session = SessionHints::default();

        assert_eq!(search_hint(&[], false, &session), None, "no hits");
        assert_eq!(search_hint(&hit(), true, &session), None, "noMatch or lowSimilarity");
        assert_eq!(search_hint(&hit(), false, &session), Some(session_hints::SEARCH_HITS));
        assert_eq!(search_hint(&hit(), false, &session), None, "once per session");
        assert_eq!(search_hint(&hit(), false, &SessionHints::default()), Some(session_hints::SEARCH_HITS));
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
            super::super::similarity::verdict(QueryShapes::shipped(), "readFile", None, &first.results)
                .is_some(),
            "the control: the first page of this same search is a no"
        );
        assert_eq!(
            super::super::similarity::verdict(
                QueryShapes::shipped(),
                "readFile",
                Some(&cursor),
                &second.results
            ),
            None,
            "a continuation is never judged"
        );
        assert!(
            similarity::low_similarity(QueryShapes::shipped(), "reads a file", None, &first.results)
                .is_some(),
            "prose control"
        );
        assert_eq!(
            similarity::low_similarity(
                QueryShapes::shipped(),
                "reads a file",
                Some(&cursor),
                &second.results
            ),
            None
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

        let conn = Arc::new(IndexStore::new(conn));
        let embedding = EmbeddingPipeline::load(&crate::config::EmbeddingConfig::default());
        let params = SearchCodeParams {
            query: "load the contents of a file from the filesystem".to_string(),
            ..Default::default()
        };
        let body = json_body(
            &handle(&conn, &embedding, QueryShapes::shipped(), &SessionHints::default(), params, None)
                .unwrap(),
        );

        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both embedded symbols must come back");
        assert_eq!(
            results[0]["symbolId"], "reader",
            "the symbol whose docstring matches the query must rank first: {results:?}"
        );
    }
}
