//! `search_code`'s bounded wait for the embedding pass, through the real
//! `GMeshMcpServer` served in-process to a real rmcp client: the structural
//! wait, the embedding wait and the partial answer, as a caller sees them.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use rusqlite::Connection;
use serde_json::{json, Value};

use super::{GMeshMcpServer, SEARCH_EMBEDDING_WAIT_ENV};
use crate::daemon::indexing_status::{IndexingStatus, Phase};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::registry::PluginRegistry;
use crate::embedding::model::EMBEDDING_DIM;
use crate::embedding::pipeline::test_support::{fake_pipeline, Counters};
use crate::embedding::EmbeddingPipeline;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};

/// Every test here sets the same value, so parallel tests never disagree
/// about it.
const WAIT_MS: &str = "200";

/// Bounds every call: the unbounded wait this module guards against would
/// otherwise hang the test instead of failing it.
const CALL_BOUND: Duration = Duration::from_secs(5);

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<IndexStore>,
    indexing: IndexingStatus,
    client: RunningService<RoleClient, ()>,
}

/// A vector with every component negative. The fake model's query vectors
/// have every component positive, so every stored row scores below zero:
/// below any language's floor, whatever the table says.
fn opposed_vector(seed: usize) -> Vec<f32> {
    (0..EMBEDDING_DIM).map(|i| if i == seed { -10.0 } else { -1.0 }).collect()
}

/// Distinct `seed`s give distinct vectors, so rows never tie on score.
fn add_embedded_node(conn: &Connection, id: &str, seed: usize) {
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
         VALUES (?1, 'Function', ?1, ?1, 'a.ts', 1, 0, 3, 1, 'typescript')",
        rusqlite::params![id],
    )
    .unwrap();
    insert(conn, id, &opposed_vector(seed), "v1").unwrap();
}

/// A server over `vectors` stored vectors, in [`Phase::Embedding`] with
/// `embed_progress` at (`embedded`, `total`), and a client connected to it.
async fn fixture(vectors: usize, embedded: u64, total: u64) -> Fixture {
    std::env::set_var(SEARCH_EMBEDDING_WAIT_ENV, WAIT_MS);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    register_extension();
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    for i in 0..vectors {
        add_embedded_node(&conn, &format!("n{i}"), i);
    }
    let store = Arc::new(IndexStore::new(conn));

    let indexing = IndexingStatus::structural();
    indexing.set_phase(Phase::Embedding);
    indexing.set_embed_total(total);
    indexing.add_embed_done(embedded);

    let registry = Arc::new(PluginRegistry::new(
        &root,
        state,
        DiscoveredPlugins::default(),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    ));
    let embedding = Arc::new(fake_pipeline(&dir.path().join("model"), None, &Counters::default()));
    let server =
        GMeshMcpServer::new(Arc::clone(&store), registry, CoreActivity::new(), indexing.clone(), embedding);

    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_io).await {
            let _ = service.waiting().await;
        }
    });
    let client = ().serve(client_io).await.expect("the client must initialize against the server");
    Fixture { _dir: dir, store, indexing, client }
}

async fn search(client: &RunningService<RoleClient, ()>, arguments: Value) -> CallToolResult {
    let call = client.call_tool(
        CallToolRequestParams::new("search_code")
            .with_arguments(arguments.as_object().cloned().expect("arguments literal is an object")),
    );
    tokio::time::timeout(CALL_BOUND, call)
        .await
        .expect("search_code must answer within its bound while the embedding pass runs")
        .expect("tools/call must return a result, not a protocol failure")
}

fn texts(result: &CallToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        })
        .collect()
}

/// The page's JSON: the last content block, after a partial page's note.
fn page(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    serde_json::from_str(texts(result).last().expect("a result carries content")).unwrap()
}

/// The embedding pass is 3 of 10 through and never finishes: the call
/// answers after its bounded wait, from the 3 stored vectors, and says so.
///
/// Controls: pass `Need::Embeddings` to `prepare` in
/// `GMeshMcpServer::search_code` - the call waits for `Phase::Ready` under
/// the 25 min cap and `CALL_BOUND` fires. Use `similarity::verdict` for a
/// partial page in `search_code::handle` - `noMatch` appears (every row is
/// below the floor, as the next test's complete page shows).
#[tokio::test]
async fn a_call_during_the_embedding_pass_answers_partially_within_its_bound() {
    let fixture = fixture(3, 3, 10).await;

    let result = search(&fixture.client, json!({ "query": "reads a file" })).await;

    let texts = texts(&result);
    assert_eq!(texts.len(), 2, "a partial page is a note and the page: {texts:?}");
    assert!(
        texts[0].starts_with("g-mesh: the embedding pass for ")
            && texts[0].contains("is still running (embeddings 3 of 10 computed, this call waited")
            && texts[0].contains("Call again later for complete results."),
        "the note leads the page: {}",
        texts[0]
    );
    let body = page(&result);
    assert_eq!(body["results"].as_array().unwrap().len(), 3, "{body}");
    assert_eq!(body["partial"], json!({ "embedded": 3, "total": 10 }));
    assert!(body.get("noMatch").is_none(), "a partial page carries no floor verdict: {body}");
    assert!(
        body.get("hint").is_none(),
        "a partial page does not spend the once-per-session search hint: {body}"
    );
    assert_eq!(fixture.indexing.phase(), Phase::Embedding, "sanity: the pass never finished");
}

/// The pass finishes 50 ms into the call's 200 ms wait: the answer is
/// complete - no note, no `partial` - and carries the floor verdict the
/// partial page above withheld for the same rows.
///
/// Control: set `G_MESH_SEARCH_EMBEDDING_WAIT_MS` to `0` here (or skip the
/// embedding wait in `GMeshMcpServer::search_code`) - the page comes back
/// partial.
#[tokio::test]
async fn a_pass_that_finishes_within_the_wait_gives_a_complete_answer() {
    let fixture = fixture(3, 3, 10).await;
    let indexing = fixture.indexing.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        indexing.set_phase(Phase::Ready);
    });

    let result = search(&fixture.client, json!({ "query": "reads a file" })).await;

    assert_eq!(texts(&result).len(), 1, "a complete page has no note: {:?}", texts(&result));
    let body = page(&result);
    assert_eq!(body["results"].as_array().unwrap().len(), 3, "{body}");
    assert!(body.get("partial").is_none(), "{body}");
    assert_eq!(body["noMatch"]["reason"], "belowSimilarityFloor", "{body}");
}

/// A cursor from a partial page continues while the stored vectors are the
/// ones it was ranked from, and is refused once one more is stored.
///
/// Control: drop the `issued != stored` arm in `search_code::handle` - the
/// last call pages on and returns a success.
#[tokio::test]
async fn a_partial_pages_cursor_is_refused_once_the_stored_vectors_change() {
    let fixture = fixture(3, 3, 10).await;

    let first = page(&search(&fixture.client, json!({ "query": "reads a file", "limit": 2 })).await);
    assert_eq!(first["hasMore"], true, "{first}");
    let cursor = first["nextCursor"].as_str().expect("a first page with more has a cursor").to_string();

    let second =
        search(&fixture.client, json!({ "query": "reads a file", "limit": 2, "cursor": cursor })).await;
    let second = page(&second);
    assert_eq!(second["results"].as_array().unwrap().len(), 1, "the unchanged set pages on: {second}");
    assert_eq!(second["partial"], json!({ "embedded": 3, "total": 10 }));

    fixture.store.with(|conn| add_embedded_node(conn, "late", 100));
    let refused =
        search(&fixture.client, json!({ "query": "reads a file", "limit": 2, "cursor": cursor })).await;
    assert_eq!(refused.is_error, Some(true), "a stale partial cursor is refused: {:?}", refused.content);
    let message = &texts(&refused)[0];
    assert!(
        message.contains("(3 symbols embedded then, 4 now)")
            && message.contains("Call search_code again without `cursor`."),
        "{message}"
    );
}
