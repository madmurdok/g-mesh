//! `search_code`'s cross-encoder rerank through the real handler, with a stub
//! scorer standing in for the model (`embedding::rerank`).

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::query_shapes::QueryShapes;
use super::search_code::handle;
use super::session_hints::SessionHints;
use super::SearchCodeParams;
use crate::embedding::pipeline::test_support::{fake_model_dir, fake_pipeline, fake_vector, Counters};
use crate::embedding::rerank::{RerankSettings, Reranker, Scorer};
use crate::embedding::EmbeddingPipeline;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};

/// The handler's output with the rerank switched off, captured from the
/// handler as it was before the rerank existed.
const GOLDEN: &str = "src/mcp/testdata/search_code_golden.json";

/// Rewrites [`GOLDEN`] instead of comparing against it.
const WRITE_GOLDEN_ENV: &str = "G_MESH_WRITE_SEARCH_GOLDEN";

const LANGUAGES: [&str; 4] = ["rust", "typescript", "python", "go"];

fn unit(v: Vec<f64>) -> Vec<f64> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    v.into_iter().map(|x| x / norm).collect()
}

/// A stored vector whose cosine to `query`'s fake query vector is `cosine`.
fn vector_at(query: &str, cosine: f64) -> Vec<f32> {
    let u = unit(fake_vector(query).into_iter().map(f64::from).collect());
    let e: Vec<f64> = fake_vector(&format!("{query}#orthogonal")).into_iter().map(f64::from).collect();
    let along: f64 = e.iter().zip(&u).map(|(a, b)| a * b).sum();
    let w = unit(e.iter().zip(&u).map(|(a, b)| a - along * b).collect());
    let sine = (1.0 - cosine * cosine).sqrt();
    u.iter().zip(&w).map(|(a, b)| (cosine * a + sine * b) as f32).collect()
}

/// One node of a fixture: id, language and the cosine its vector has to the
/// query.
pub(super) struct Row {
    pub(super) id: String,
    pub(super) language: &'static str,
    pub(super) cosine: f64,
}

/// A store holding `rows` for `query`. Each node's signature is `fn <id>()`,
/// so a stub scorer can tell the rows apart by text.
pub(super) fn store_for(query: &str, rows: &[Row]) -> Arc<IndexStore> {
    register_extension();
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for row in rows {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, \
             language, docComment, signature)
             VALUES (?1, 'Function', ?1, ?2, 'a.src', 1, 0, 3, 1, ?3, ?4, ?5)",
            rusqlite::params![
                row.id,
                format!("pkg::{}", row.id),
                row.language,
                format!("Does {}.", row.id),
                format!("fn {}()", row.id)
            ],
        )
        .unwrap();
        insert(&conn, &row.id, &vector_at(query, row.cosine), "v1").unwrap();
    }
    Arc::new(IndexStore::new(conn))
}

/// `count` rows from `top` down in steps of `step`, languages in turn; rows
/// 10 and 11 tie.
fn descending(count: usize, top: f64, step: f64) -> Vec<Row> {
    (0..count)
        .map(|i| {
            let rank = if i == 11 { 10 } else { i };
            Row { id: format!("n{i:02}"), language: LANGUAGES[i % 4], cosine: top - step * rank as f64 }
        })
        .collect()
}

pub(super) fn call(
    store: &Arc<IndexStore>,
    embedding: &EmbeddingPipeline,
    query: &str,
    limit: Option<u32>,
    cursor: Option<String>,
) -> CallToolResult {
    let params = SearchCodeParams { query: query.to_string(), cursor, limit };
    handle(store, embedding, QueryShapes::shipped(), &SessionHints::default(), params, None).unwrap()
}

/// The JSON body of a one-block result.
pub(super) fn body(result: &CallToolResult) -> serde_json::Value {
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

/// Every call the golden file records, as (label, result), through
/// `embedding`.
fn golden_calls(embedding: &EmbeddingPipeline) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for query in ["parseConfig", "parses the config file"] {
        let high = store_for(query, &descending(64, 0.9, 0.01));
        for limit in [5, 20, 30, 50] {
            let result = call(&high, embedding, query, Some(limit), None);
            out.push((format!("{query} high limit {limit}"), serde_json::to_value(&result).unwrap()));
        }
        let mut cursor = None;
        for page in 1..=3 {
            let result = call(&high, embedding, query, None, cursor);
            cursor = body(&result)["nextCursor"].as_str().map(str::to_string);
            out.push((
                format!("{query} high default limit page {page}"),
                serde_json::to_value(&result).unwrap(),
            ));
        }
        let low = store_for(query, &descending(40, 0.5, 0.005));
        for limit in [20, 30] {
            let result = call(&low, embedding, query, Some(limit), None);
            out.push((format!("{query} low limit {limit}"), serde_json::to_value(&result).unwrap()));
        }
    }
    out
}

fn golden_json(calls: &[(String, serde_json::Value)]) -> String {
    let map: serde_json::Map<String, serde_json::Value> = calls.iter().cloned().collect();
    serde_json::to_string_pretty(&serde_json::Value::Object(map)).unwrap() + "\n"
}

/// The pipeline the golden calls go through: the fake query model, and the
/// rerank switched off.
fn fake(dir: &Path) -> EmbeddingPipeline {
    fake_pipeline(&fake_model_dir(dir, "weights"), None, &Counters::default())
}

/// How far a `score`, or a cursor's score, may drift from [`GOLDEN`]'s. The
/// golden was captured on x86_64; the cosine differs around the 7th digit
/// on aarch64 (GM-478), which no ranking in the fixture depends on.
const SCORE_TOLERANCE: f64 = 1e-6;

/// Compares `embedding`'s golden calls with [`GOLDEN`] structurally:
/// everything exactly except scores, which may differ by
/// [`SCORE_TOLERANCE`], including the ones cursors carry.
pub(super) fn assert_matches_golden(embedding: &EmbeddingPipeline) {
    let calls = golden_calls(embedding);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os(WRITE_GOLDEN_ENV).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, golden_json(&calls)).unwrap();
        return;
    }
    let expected: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let labels: Vec<&str> = calls.iter().map(|(label, _)| label.as_str()).collect();
    let mut golden_labels: Vec<&str> = expected.as_object().unwrap().keys().map(String::as_str).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    golden_labels.sort_unstable();
    assert_eq!(sorted, golden_labels, "search_code's golden calls differ from {GOLDEN}'s");
    for (label, actual) in &calls {
        if let Err(mismatch) = golden_diff(label, actual, &expected[label]) {
            panic!("search_code output differs from {GOLDEN}: {mismatch}");
        }
    }
}

/// The first place `actual` differs from `expected`, as a readable line.
/// A `text` string holding JSON is compared as JSON, a `score` within
/// [`SCORE_TOLERANCE`], a `nextCursor` by its decoded parts.
fn golden_diff(path: &str, actual: &serde_json::Value, expected: &serde_json::Value) -> Result<(), String> {
    use serde_json::Value;
    let differ = || format!("at {path}:\n  actual:   {actual}\n  expected: {expected}");
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            let mut keys: Vec<&String> = a.keys().collect();
            let mut golden_keys: Vec<&String> = e.keys().collect();
            keys.sort_unstable();
            golden_keys.sort_unstable();
            if keys != golden_keys {
                return Err(format!("{} (keys {keys:?} vs {golden_keys:?})", differ()));
            }
            for (key, value) in a {
                let at = format!("{path}.{key}");
                match (key.as_str(), value, &e[key]) {
                    ("score", Value::Number(x), Value::Number(y)) => {
                        let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
                        if (x - y).abs() > SCORE_TOLERANCE {
                            return Err(format!(
                                "at {at}: score {x} vs golden {y}, beyond {SCORE_TOLERANCE}"
                            ));
                        }
                    }
                    ("nextCursor", Value::String(x), Value::String(y)) => cursor_diff(&at, x, y)?,
                    ("text", Value::String(x), Value::String(y)) => {
                        match (serde_json::from_str::<Value>(x), serde_json::from_str::<Value>(y)) {
                            (Ok(x), Ok(y)) => golden_diff(&at, &x, &y)?,
                            _ if x == y => {}
                            _ => return Err(format!("at {at}:\n  actual:   {x:?}\n  expected: {y:?}")),
                        }
                    }
                    (_, value, golden) => golden_diff(&at, value, golden)?,
                }
            }
            Ok(())
        }
        (Value::Array(a), Value::Array(e)) => {
            for (i, (value, golden)) in a.iter().zip(e).enumerate() {
                golden_diff(&format!("{path}[{i}]"), value, golden)?;
            }
            if a.len() == e.len() {
                Ok(())
            } else {
                Err(format!("at {path}: {} items vs golden {}", a.len(), e.len()))
            }
        }
        _ if actual == expected => Ok(()),
        _ => Err(differ()),
    }
}

/// Compares two cursors by what they decode to: ids exactly, scores within
/// [`SCORE_TOLERANCE`].
fn cursor_diff(path: &str, actual: &str, expected: &str) -> Result<(), String> {
    use super::search_code::{cursor_parts, CursorParts};
    fn same(a: &CursorParts, e: &CursorParts) -> bool {
        match (a, e) {
            (CursorParts::Score { score: x, id: a }, CursorParts::Score { score: y, id: e }) => {
                a == e && (x - y).abs() <= SCORE_TOLERANCE
            }
            (CursorParts::Rerank { rest: a, after: x }, CursorParts::Rerank { rest: e, after: y }) => {
                a == e
                    && match (x, y) {
                        (Some(x), Some(y)) => same(x, y),
                        (x, y) => x.is_none() && y.is_none(),
                    }
            }
            (CursorParts::Partial { stored: a, inner: x }, CursorParts::Partial { stored: e, inner: y }) => {
                a == e && same(x, y)
            }
            _ => false,
        }
    }
    match (cursor_parts(actual), cursor_parts(expected)) {
        (Ok(a), Ok(e)) if same(&a, &e) => Ok(()),
        (a, e) => Err(format!(
            "at {path}: cursor {actual:?} vs golden {expected:?}\n  actual:   {a:?}\n  expected: {e:?}"
        )),
    }
}

/// Scores a row by its id alone: the stub's logit for `nXX` is `logit(XX)`.
/// Node texts end in `fn nXX()` (see [`store_for`]).
struct Stub(Arc<dyn Fn(usize) -> f32 + Send + Sync>);

impl Scorer for Stub {
    fn score(&self, _query: &str, texts: &[String]) -> anyhow::Result<Vec<f32>> {
        Ok(texts.iter().map(|text| (self.0)(row_number(text))).collect())
    }
}

fn row_number(text: &str) -> usize {
    let name = text.rsplit("fn n").next().unwrap();
    name.trim_end_matches("()").parse().unwrap_or_else(|_| panic!("not a fixture row: {text:?}"))
}

/// Every line the rerank logged.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    fn sink(&self) -> impl Fn(&str) + Send + Sync + 'static {
        let lines = Arc::clone(&self.0);
        move |line: &str| lines.lock().unwrap_or_else(PoisonError::into_inner).push(line.to_string())
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

fn stub_reranker(
    enabled: bool,
    logit: impl Fn(usize) -> f32 + Send + Sync + 'static,
    lines: &Lines,
) -> Reranker {
    let logit: Arc<dyn Fn(usize) -> f32 + Send + Sync> = Arc::new(logit);
    Reranker::with_parts(
        move || RerankSettings { enabled, model_dir: Ok("/stub".into()) },
        move |_| Ok(Box::new(Stub(Arc::clone(&logit))) as Box<dyn Scorer>),
        lines.sink(),
    )
}

/// A logit that reverses the window: later rows score far higher.
fn reversing(row: usize) -> f32 {
    row as f32 * 100.0
}

/// T3. With the switch off, a stub that would reorder every window changes
/// nothing: the output is the handler's from before the rerank existed.
///
/// *Control:* ignore the switch (`Reranker::scorer` loading regardless of
/// `enabled`), and the stub reorders the first pages.
#[test]
fn with_the_switch_off_the_output_equals_todays() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    assert_matches_golden(&fake(dir.path()).with_reranker(stub_reranker(false, reversing, &lines)));
    assert_eq!(lines.count(), 0);
}

/// The control's own arm: the same stub switched on does change the output,
/// so the golden comparison above can tell the arms apart.
#[test]
fn with_the_switch_on_the_stub_changes_the_first_page() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let embedding = fake(dir.path()).with_reranker(stub_reranker(true, reversing, &lines));
    let store = store_for("parseConfig", &descending(64, 0.9, 0.01));
    let page = body(&call(&store, &embedding, "parseConfig", Some(5), None));
    assert_eq!(page["results"][0]["symbolId"], "n29", "{page}");
}

/// T4. A rerank switched on whose model directory holds nothing: every
/// call answers exactly as with the switch off, and the failure is logged
/// once across all of them, not per call and never as a tool error.
///
/// *Control:* return the load error as a tool error from `handle`, or log
/// it on every call (resolve outside the `OnceLock`).
#[test]
fn a_missing_model_keeps_todays_output_and_logs_once() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let missing = dir.path().join("no-rerank-model-here");
    assert_matches_golden(&fake(dir.path()).with_reranker(Reranker::real_at(&missing, lines.sink())));
    let logged = lines.0.lock().unwrap().clone();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert!(logged[0].contains("rerank model ms-marco-MiniLM-L6-v2 is not available"), "{logged:?}");
    assert!(logged[0].contains("g-mesh model fetch"), "{logged:?}");
}

/// A scorer that fails on one call falls back to the embedding order for
/// that call, with one log line for it.
#[test]
fn a_failing_call_keeps_the_embedding_order_for_that_call() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let failing = stub_reranker(true, |_| f32::NAN, &lines);
    let embedding = fake(dir.path()).with_reranker(failing);
    let store = store_for("parseConfig", &descending(64, 0.9, 0.01));
    let plain = fake(dir.path());
    for _ in 0..2 {
        assert_eq!(
            serde_json::to_value(call(&store, &embedding, "parseConfig", Some(5), None)).unwrap(),
            serde_json::to_value(call(&store, &plain, "parseConfig", Some(5), None)).unwrap()
        );
    }
    assert_eq!(lines.count(), 2, "one line per failing call");
}

/// The verdict fields of a page.
fn verdict_of(page: &serde_json::Value) -> (serde_json::Value, serde_json::Value) {
    (page.get("noMatch").cloned().unwrap_or_default(), page.get("lowSimilarity").cloned().unwrap_or_default())
}

fn row(i: usize, language: &'static str, cosine: f64) -> Row {
    Row { id: format!("n{i:02}"), language, cosine }
}

/// T2. The verdict is the same with the rerank on and off, in both
/// directions a reranked page could flip it, for a name query (`noMatch`)
/// and a prose one (`lowSimilarity`). The stub pulls embedding row 25 to the
/// top of page 1 and pushes row 0 past it.
///
/// - Only row 0 clears its floor (typescript 0.56 >= 0.53; every other row
///   is python under 0.59): the embedding page is not judged below the
///   floor, the reranked page would be.
/// - No row of the embedding page clears its floor, and row 25 does
///   (typescript 0.536): the embedding page is judged below the floor, the
///   reranked page would not be.
///
/// *Control:* judge the verdict on the reranked page (`judged: None` in
/// `first_page_reranked`).
#[test]
fn the_verdict_is_the_same_with_the_rerank_on_and_off() {
    let only_row_0_clears: Vec<Row> = (0..40)
        .map(|i| if i == 0 { row(0, "typescript", 0.56) } else { row(i, "python", 0.555 - 0.002 * i as f64) })
        .collect();
    let only_row_25_clears: Vec<Row> = (0..40)
        .map(|i| match i {
            0..=24 => row(i, "python", 0.585 - 0.002 * i as f64),
            25 => row(25, "typescript", 0.536),
            _ => row(i, "python", 0.535 - 0.002 * (i - 25) as f64),
        })
        .collect();
    let pull_25_push_0 = |row: usize| match row {
        25 => 1000.0,
        0 => -1000.0,
        _ => 0.0,
    };

    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let off = fake(dir.path());
    let on = fake(dir.path()).with_reranker(stub_reranker(true, pull_25_push_0, &lines));
    for (rows, judged_below) in [(&only_row_0_clears, false), (&only_row_25_clears, true)] {
        for query in ["parseConfig", "parses the config file"] {
            let store = store_for(query, rows);
            let plain = body(&call(&store, &off, query, Some(20), None));
            let reranked = body(&call(&store, &on, query, Some(20), None));
            let ids: Vec<&str> = reranked["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["symbolId"].as_str().unwrap())
                .collect();
            assert_eq!(ids[0], "n25", "the fixture must pull row 25 onto page 1: {ids:?}");
            assert!(!ids.contains(&"n00"), "the fixture must push row 0 off page 1: {ids:?}");

            assert_eq!(verdict_of(&reranked), verdict_of(&plain), "{query}: {reranked}");
            let (no_match, low_similarity) = verdict_of(&plain);
            assert_eq!(!no_match.is_null() || !low_similarity.is_null(), judged_below, "{query}: {plain}");
        }
    }
    assert_eq!(lines.count(), 0);
}

/// The stub for pagination: a permutation of the window that no cosine can
/// overturn (logit steps of 100 against cosine spreads under 0.3 * 80).
fn permuting(row: usize) -> f32 {
    ((row * 7) % 30) as f32 * 100.0
}

/// The order every walk must produce over [`descending`]'s 64 rows under
/// [`permuting`]: the window by the stub's logit, then rows 30 to 63.
fn expected_walk() -> Vec<String> {
    let mut window: Vec<usize> = (0..30).collect();
    window.sort_by(|&a, &b| permuting(b).partial_cmp(&permuting(a)).unwrap());
    window.into_iter().chain(30..64).map(|i| format!("n{i:02}")).collect()
}

/// Every row of a search, page by page from a first page of `limit`.
/// `between` runs after the first page.
fn walk(
    store: &Arc<IndexStore>,
    embedding: &EmbeddingPipeline,
    limit: u32,
    between: impl FnOnce(&Arc<IndexStore>),
) -> Vec<String> {
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut between = Some(between);
    for _ in 0..200 {
        let page = body(&call(store, embedding, "parseConfig", Some(limit), cursor));
        let rows = page["results"].as_array().unwrap();
        assert!(rows.len() <= limit as usize, "{page}");
        seen.extend(rows.iter().map(|r| r["symbolId"].as_str().unwrap().to_string()));
        if !page["hasMore"].as_bool().unwrap() {
            assert!(page.get("nextCursor").is_none_or(|c| c.is_null()), "{page}");
            return seen;
        }
        cursor = Some(page["nextCursor"].as_str().unwrap().to_string());
        if let Some(between) = between.take() {
            between(store);
        }
    }
    panic!("the walk did not end");
}

/// T5. At every page size, the pages of a reranked search concatenate to
/// the reranked window followed by the embedding order after it: no row
/// twice, none skipped.
///
/// *Control:* hand out `search`'s own cosine cursor on a reranked first page
/// when `limit < 30`, and rows repeat or go missing.
#[test]
fn every_page_size_walks_the_reranked_window_then_the_rest_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let embedding = fake(dir.path()).with_reranker(stub_reranker(true, permuting, &lines));
    let store = store_for("parseConfig", &descending(64, 0.9, 0.01));
    for limit in [1, 5, 20, 29, 30, 50] {
        assert_eq!(walk(&store, &embedding, limit, |_| {}), expected_walk(), "limit {limit}");
    }
}

/// A row the cursor still holds but that was deleted between the pages is
/// skipped, and nothing else changes.
#[test]
fn a_row_deleted_between_pages_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let embedding = fake(dir.path()).with_reranker(stub_reranker(true, permuting, &lines));
    let store = store_for("parseConfig", &descending(64, 0.9, 0.01));
    let expected = expected_walk();
    let deleted = expected[7].clone();

    let seen = walk(&store, &embedding, 5, |store| {
        let writer = store.lock().unwrap();
        writer.execute("DELETE FROM vectors WHERE nodeId = ?1", [&deleted]).unwrap();
        writer.execute("DELETE FROM nodes WHERE id = ?1", [&deleted]).unwrap();
    });

    let remaining: Vec<String> = expected.into_iter().filter(|id| *id != deleted).collect();
    assert_eq!(seen, remaining);
}

/// D6. A partial page (the embedding pass still running) is ranked exactly
/// as with the rerank off: the stub would reorder it, and does not.
///
/// *Control:* rerank whenever `cursor` is `None`, whatever `coverage` says.
#[test]
fn a_partial_page_is_not_reranked() {
    use crate::daemon::indexing_status::Phase;

    let dir = tempfile::tempdir().unwrap();
    let lines = Lines::default();
    let on = fake(dir.path()).with_reranker(stub_reranker(true, reversing, &lines));
    let off = fake(dir.path());
    let store = store_for("parseConfig", &descending(64, 0.9, 0.01));
    let coverage = super::search_code::Coverage::partial(
        &Phase::Embedding,
        40,
        64,
        std::time::Duration::ZERO,
        dir.path(),
    );
    let partial = |embedding: &EmbeddingPipeline| {
        let params = SearchCodeParams { query: "parseConfig".to_string(), cursor: None, limit: Some(5) };
        serde_json::to_value(
            handle(
                &store,
                embedding,
                QueryShapes::shipped(),
                &SessionHints::default(),
                params,
                coverage.as_ref(),
            )
            .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(partial(&on), partial(&off));
    let complete = body(&call(&store, &on, "parseConfig", Some(5), None));
    assert_eq!(
        complete["results"][0]["symbolId"], "n29",
        "the same stub reorders a complete page: {complete}"
    );
}

/// One case of `embedding/testdata/rerank_parity.json`: the Python
/// reference's query and window, each row with the text it scored.
#[derive(serde::Deserialize)]
struct ParityCase {
    #[serde(rename = "queryId")]
    query_id: String,
    query: String,
    rows: Vec<ParityRow>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParityRow {
    id: String,
    doc_comment: Option<String>,
    signature: Option<String>,
    text: String,
    cosine: f64,
}

#[derive(serde::Deserialize)]
struct Parity {
    cases: Vec<ParityCase>,
}

/// Every `(query, texts)` the handler handed the scorer; scores all zero,
/// so the blend keeps the embedding order.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<ScoringCall>>>);

/// One scoring call: the query and the window's texts, in window order.
type ScoringCall = (String, Vec<String>);

impl Scorer for Capture {
    fn score(&self, query: &str, texts: &[String]) -> anyhow::Result<Vec<f32>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push((query.to_string(), texts.to_vec()));
        Ok(vec![0.0; texts.len()])
    }
}

/// A store holding one parity case's window: each row's own doc comment
/// and signature, its vector at the reference's cosine.
fn store_for_parity(case: &ParityCase) -> Arc<IndexStore> {
    register_extension();
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for row in &case.rows {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, \
             language, docComment, signature)
             VALUES (?1, 'Function', ?1, ?1, 'a.src', 1, 0, 3, 1, 'typescript', ?2, ?3)",
            rusqlite::params![row.id, row.doc_comment, row.signature],
        )
        .unwrap();
        insert(&conn, &row.id, &vector_at(&case.query, row.cosine), "v1").unwrap();
    }
    Arc::new(IndexStore::new(conn))
}

/// The product path feeds the cross-encoder exactly the (query, text)
/// pairs the Python reference scored: for every parity case, the handler's
/// own window texts, matched to rows through the page it returns, equal
/// the fixture's stored `text`. This ties `window_texts` to the fixture
/// that `the_real_model_reproduces_the_reference_logits_and_order` checks
/// the model against (that test builds its texts itself).
///
/// *Control:* have `window_texts` call `full_text` instead of
/// `text_to_embed` (49 of the fixture's 540 rows then differ).
#[test]
fn the_handler_scores_the_texts_the_reference_scored() {
    let parity: Parity =
        serde_json::from_str(include_str!("../embedding/testdata/rerank_parity.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for case in &parity.cases {
        let capture = Capture::default();
        let scorer = capture.clone();
        let lines = Lines::default();
        let reranker = Reranker::with_parts(
            || RerankSettings { enabled: true, model_dir: Ok("/stub".into()) },
            move |_| Ok(Box::new(scorer.clone()) as Box<dyn Scorer>),
            lines.sink(),
        );
        let embedding = fake(dir.path()).with_reranker(reranker);
        let store = store_for_parity(case);
        let window = case.rows.len() as u32;
        let page = body(&call(&store, &embedding, &case.query, Some(window), None));

        let calls = capture.0.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{}: one scoring call", case.query_id);
        let (query, texts) = &calls[0];
        assert_eq!(query, &case.query, "{}", case.query_id);
        let ids: Vec<&str> =
            page["results"].as_array().unwrap().iter().map(|r| r["symbolId"].as_str().unwrap()).collect();
        assert_eq!((ids.len(), texts.len()), (case.rows.len(), case.rows.len()), "{}", case.query_id);
        for (id, text) in ids.iter().zip(texts) {
            let row = case.rows.iter().find(|row| row.id == *id).unwrap();
            assert_eq!(
                text, &row.text,
                "{} {id}: the text scored differs from the reference's",
                case.query_id
            );
        }
        assert_eq!(lines.count(), 0, "{}", case.query_id);
    }
}
