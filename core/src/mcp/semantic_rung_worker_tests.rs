//! The resolution ladder's semantic rung (`find_definition::by_semantic_neighbours`,
//! reached by every name-resolving tool) embeds its query lazily, with no store
//! lock held and off the async workers: while one call's inference is
//! blocked, the daemon still serves other calls and other lock takers, and the
//! answers are the ones the ladder gave before.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::Result;
use rmcp::model::CallToolResult;
use rusqlite::Connection;
use serde_json::{json, Value};

use super::search_code_worker_tests::{call, connect, Gate, GatedEmbedder};
use super::{find_definition, FindDefinitionParams, GMeshMcpServer};
use crate::daemon::indexing_status::{IndexingStatus, Phase};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::registry::PluginRegistry;
use crate::embedding::model::EMBEDDING_DIM;
use crate::embedding::pipeline::Embedder;
use crate::embedding::EmbeddingPipeline;
use crate::graph::queries::upsert_node;
use crate::protocol::types::QualifiedPath;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};
use crate::storage::write::NodeRecord;

/// A name nothing declares, no file carries and nothing imports: only the
/// semantic rung can answer it.
const UNNAMED: &str = "NoSuchSymbolAnywhere";

/// One declaration per structural rung the samples below reach, and one
/// stored vector for the semantic rung to find.
fn fixture() -> Connection {
    register_extension();
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Exact: `run` is its name, `pkg::run` its whole qualifiedName.
    upsert_node(&mut conn, NodeRecord::new("run", "Function", "run", "pkg::run", "a.rs", "rust")).unwrap();
    // Suffix: `Stream::read` is neither its name nor its qualifiedName.
    let mut read = NodeRecord::new("read", "Function", "read", "ipc::Stream::read", "b.rs", "rust");
    read.qualified_path = Some(QualifiedPath::root("ipc").child("::", "Stream").child("::", "read"));
    upsert_node(&mut conn, read).unwrap();
    // Every test embedder answers `[1.0; DIM]`, so this scores 1.0.
    insert(&conn, "run", &vec![1.0; EMBEDDING_DIM], "v1").unwrap();
    conn
}

/// Answers every query with the fixture's vector and counts how often it
/// was asked.
struct CountingEmbedder(Arc<AtomicUsize>);

impl Embedder for CountingEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1.0; EMBEDDING_DIM])
    }
}

fn pipeline(
    dir: &Path,
    make: impl Fn() -> Box<dyn Embedder> + Send + Sync + 'static,
) -> Arc<EmbeddingPipeline> {
    Arc::new(EmbeddingPipeline::with_loader(&dir.join("model"), move |_: &Path| Ok(make()), None))
}

fn server(dir: &Path, store: Arc<IndexStore>, embedding: Arc<EmbeddingPipeline>) -> GMeshMcpServer {
    let root = dir.join("project");
    let state = dir.join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let indexing = IndexingStatus::structural();
    indexing.set_phase(Phase::Ready);
    let registry = Arc::new(PluginRegistry::new(
        &root,
        state,
        DiscoveredPlugins::default(),
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    ));
    GMeshMcpServer::new(store, registry, CoreActivity::new(), indexing, embedding)
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "{:?}", result.content);
    let text = &result.content[0].as_text().expect("text content").text;
    serde_json::from_str(text).expect("a JSON body")
}

/// One worker: a `find_definition` blocked in the semantic rung's inference
/// leaves nothing to serve any other call, so another session's structural
/// call and `tools/list` return while the gate is still shut only if the
/// inference is off the worker.
///
/// Control: in `GMeshMcpServer::find_definition`, call
/// `find_definition::resolve_lazily` (the synchronous driver, which embeds
/// on the calling thread) instead of `resolve_lazily_off_worker` - both calls
/// stall behind the inference until the watchdog opens the gate, and the
/// first assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn another_session_is_served_while_a_semantic_rung_inference_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let loader_gate = Arc::clone(&gate);
    let embedding = pipeline(dir.path(), move || Box::new(GatedEmbedder(Arc::clone(&loader_gate))));
    let server = server(dir.path(), Arc::new(IndexStore::new(fixture())), embedding);

    let resolving = connect(server.clone()).await;
    let other = connect(server).await;

    let entered = gate.entered.notified();
    let definition = tokio::spawn(async move {
        let result = call(&resolving, "find_definition", json!({ "symbol_name": UNNAMED })).await;
        drop(resolving);
        result
    });
    entered.await;
    gate.open_after_watchdog();

    let outline = call(&other, "get_file_outline", json!({ "file_path": "a.rs" })).await;
    let tools = other.list_all_tools().await.expect("tools/list must answer");
    let served_while_blocked = !gate.is_open();
    gate.open();

    assert!(
        served_while_blocked,
        "get_file_outline and tools/list must be answered while the inference is blocked"
    );
    assert!(
        format!("{:?}", outline.content).contains("no file 'a.rs' found in the index"),
        "the outline handler itself answered: {:?}",
        outline.content
    );
    assert!(tools.iter().any(|tool| tool.name == "find_definition"), "a real tool list");
    let definition = body(&definition.await.unwrap());
    assert_eq!(definition["resolvedBy"], "semanticNeighbours", "{definition}");
}

/// While the semantic rung's inference is blocked, another thread takes the
/// store: the inference runs with no store lock held.
///
/// Control: in `by_semantic_neighbours`' `Deferred` arm, embed there and
/// search with the result instead of recording the name (the old
/// order: inference inside the handler's `store.read()`) - the probe cannot
/// take the store until the watchdog opens the gate, and the assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_semantic_rung_embeds_with_no_store_lock_held() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(Gate::default());
    let loader_gate = Arc::clone(&gate);
    let embedding = pipeline(dir.path(), move || Box::new(GatedEmbedder(Arc::clone(&loader_gate))));
    let store = Arc::new(IndexStore::new(fixture()));
    let server = server(dir.path(), Arc::clone(&store), embedding);

    let resolving = connect(server).await;
    let entered = gate.entered.notified();
    let definition = tokio::spawn(async move {
        let result = call(&resolving, "find_callers", json!({ "symbol_name": UNNAMED })).await;
        drop(resolving);
        result
    });
    entered.await;
    gate.open_after_watchdog();

    let (taken, probe) = mpsc::channel();
    let probe_store = Arc::clone(&store);
    std::thread::spawn(move || {
        probe_store.with(|_| ());
        let _ = taken.send(());
    });
    let free_while_embedding = probe.recv_timeout(Duration::from_secs(2)).is_ok() && !gate.is_open();
    gate.open();

    assert!(free_while_embedding, "the store must be free while the semantic rung's query is embedded");
    let callers = body(&definition.await.unwrap());
    assert_eq!(callers["resolvedBy"], "semanticNeighbours", "{callers}");
}

/// The ladder answers exactly as before on a sample of rungs: exact (by
/// qualifiedName and by name), suffix
/// and semantic, through the server (`resolve_lazily_off_worker`)
/// and through the synchronous driver (`resolve_lazily`) alike.
///
/// Control: in `resolve_lazily_off_worker`, return the first pass's answer
/// even when it reached the semantic rung (ignore `first.reached()`) - the
/// semantic sample answers the terse refusal instead of its candidate page,
/// and its assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ladder_answers_as_before_on_exact_suffix_and_semantic_rungs() {
    let dir = tempfile::tempdir().unwrap();
    let embedding = pipeline(dir.path(), || Box::new(CountingEmbedder(Arc::default())));
    let store = Arc::new(IndexStore::new(fixture()));
    let server = server(dir.path(), Arc::clone(&store), Arc::clone(&embedding));
    let client = connect(server).await;

    let samples = [
        ("pkg::run", "qualifiedName", "run"),
        ("run", "name", "run"),
        ("Stream::read", "qualifiedNameSuffix", "read"),
        (UNNAMED, "semanticNeighbours", "run"),
    ];
    for (query, rung, id) in samples {
        let served = body(
            &call(&client, "find_definition", json!({ "symbol_name": query, "include_source": false })).await,
        );
        let params = FindDefinitionParams {
            symbol_name: Some(query.to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: Some(false),
        };
        let direct = body(&find_definition::handle(&store, Path::new("."), &embedding, params).unwrap());

        assert_eq!(served["resolvedBy"], rung, "{query}: {served}");
        let served_id =
            if rung == "semanticNeighbours" { &served["results"][0]["id"] } else { &served["id"] };
        assert_eq!(served_id, id, "{query}: {served}");
        assert_eq!(served, direct, "{query}: both drivers answer alike");
    }
    let semantic = body(&call(&client, "find_definition", json!({ "symbol_name": UNNAMED })).await);
    assert_eq!(
        semantic["explanation"],
        format!(
            "Nothing is named '{UNNAMED}'. These are the closest declarations by meaning, not by \
             name - they may be what you meant, or may merely be nearby. Check one before \
             relying on it, and re-query by its id."
        ),
        "{semantic}"
    );
    assert_eq!(semantic["ambiguous"], false, "{semantic}");
}

/// A resolution an earlier rung settles embeds nothing; only one that falls
/// through to the semantic rung embeds, once. The last call is this test's
/// own check that the counter sees an inference at all.
///
/// Control: embed eagerly - call `embedding.embed_query(name)` at the top of
/// `resolve_symbol_name` (or before the first pass in either driver), as
/// `search_code` does - and the count after the structural calls is no
/// longer zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_embedding_happens_when_an_earlier_rung_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    let embedding = pipeline(dir.path(), move || Box::new(CountingEmbedder(Arc::clone(&counted))));
    let server = server(dir.path(), Arc::new(IndexStore::new(fixture())), embedding);
    let client = connect(server).await;

    for (tool, query) in [
        ("find_definition", "run"),
        ("find_definition", "Stream::read"),
        ("find_callers", "run"),
        ("find_callees", "Stream::read"),
        ("find_references", "pkg::run"),
        ("find_implementations", "run"),
        // A specifier never reaches inference either.
        ("find_definition", "@scope/pkg"),
    ] {
        call(&client, tool, json!({ "symbol_name": query })).await;
    }
    assert_eq!(count.load(Ordering::SeqCst), 0, "no structural resolution may embed");

    call(&client, "find_references", json!({ "symbol_name": UNNAMED })).await;
    assert_eq!(count.load(Ordering::SeqCst), 1, "the semantic rung embeds its query once");
}
