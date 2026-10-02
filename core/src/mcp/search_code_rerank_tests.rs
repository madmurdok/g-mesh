//! `search_code`'s cross-encoder rerank through the real handler, with a stub
//! scorer standing in for the model (`embedding::rerank`).

use std::path::Path;
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::search_code::handle;
use super::session_hints::SessionHints;
use super::SearchCodeParams;
use crate::embedding::pipeline::test_support::{fake_model_dir, fake_pipeline, fake_vector, Counters};
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
    handle(store, embedding, &SessionHints::default(), params, None).unwrap()
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
            out.push((format!("{query} high default limit page {page}"), serde_json::to_value(&result).unwrap()));
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

/// Compares `embedding`'s golden calls with [`GOLDEN`] byte for byte.
pub(super) fn assert_matches_golden(embedding: &EmbeddingPipeline) {
    let actual = golden_json(&golden_calls(embedding));
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
    if std::env::var_os(WRITE_GOLDEN_ENV).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap();
    assert!(actual == expected, "search_code output differs from {GOLDEN}:\n{actual}");
}

#[test]
fn with_the_switch_off_the_output_equals_todays() {
    let dir = tempfile::tempdir().unwrap();
    assert_matches_golden(&fake(dir.path()));
}
