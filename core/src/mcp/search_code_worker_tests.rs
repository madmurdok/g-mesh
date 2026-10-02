//! `search_code`'s query inference runs off the async workers: while one
//! call's inference is blocked, another session's call on the same daemon is
//! still answered.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::GMeshMcpServer;
use crate::daemon::indexing_status::{IndexingStatus, Phase};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::registry::PluginRegistry;
use crate::embedding::model::EMBEDDING_DIM;
use crate::embedding::pipeline::Embedder;
use crate::embedding::EmbeddingPipeline;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};

/// How long the gate stays shut if the test never opens it: long enough
/// that a served call returns well before it, short enough that a stalled
/// one fails the test instead of hanging it.
pub(super) const WATCHDOG: Duration = Duration::from_secs(5);

/// Holds every inference until opened. `entered` fires once an inference is
/// waiting on it.
#[derive(Default)]
pub(super) struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
    pub(super) entered: Notify,
}

impl Gate {
    pub(super) fn open(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.opened.notify_all();
    }

    pub(super) fn is_open(&self) -> bool {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens the gate after [`WATCHDOG`] unless it was opened first.
    pub(super) fn open_after_watchdog(self: &Arc<Self>) {
        let gate = Arc::clone(self);
        std::thread::spawn(move || {
            let open = gate.open.lock().unwrap_or_else(PoisonError::into_inner);
            let (mut open, _) = gate
                .opened
                .wait_timeout_while(open, WATCHDOG, |open| !*open)
                .unwrap_or_else(PoisonError::into_inner);
            *open = true;
            gate.opened.notify_all();
        });
    }
}

pub(super) struct GatedEmbedder(pub(super) Arc<Gate>);

impl Embedder for GatedEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        self.0.entered.notify_one();
        let open = self.0.open.lock().unwrap_or_else(PoisonError::into_inner);
        drop(self.0.opened.wait_while(open, |open| !*open).unwrap_or_else(PoisonError::into_inner));
        Ok(vec![1.0; EMBEDDING_DIM])
    }
}

pub(super) async fn connect(server: GMeshMcpServer) -> RunningService<RoleClient, ()> {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_io).await {
            let _ = service.waiting().await;
        }
    });
    ().serve(client_io).await.expect("the client must initialize against the server")
}

pub(super) async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(arguments.as_object().cloned().expect("arguments literal is an object")),
        )
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

/// One worker: a call that blocks it in inference leaves nothing to serve
/// any other call, so the second session's `get_file_outline` returns while
/// the gate is still shut only if the inference is off the worker.
///
/// Control: call `search_code::handle` directly in
/// `GMeshMcpServer::search_code` instead of `handle_off_worker` - the
/// outline call stalls behind the inference until the watchdog opens the
/// gate, and the first assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn another_call_is_served_while_a_search_code_inference_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    register_extension();
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
         VALUES ('n0', 'Function', 'n0', 'n0', 'a.ts', 1, 0, 3, 1, 'typescript')",
        [],
    )
    .unwrap();
    insert(&conn, "n0", &vec![1.0; EMBEDDING_DIM], "v1").unwrap();
    let store = Arc::new(IndexStore::new(conn));

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
    let gate = Arc::new(Gate::default());
    let loader_gate = Arc::clone(&gate);
    let embedding = Arc::new(EmbeddingPipeline::with_loader(
        &dir.path().join("model"),
        move |_: &Path| Ok(Box::new(GatedEmbedder(Arc::clone(&loader_gate))) as Box<dyn Embedder>),
        None,
    ));
    let server = GMeshMcpServer::new(store, registry, CoreActivity::new(), indexing, embedding);

    let searching = connect(server.clone()).await;
    let other = connect(server).await;

    let entered = gate.entered.notified();
    let search = tokio::spawn(async move {
        let result = call(&searching, "search_code", json!({ "query": "reads a file" })).await;
        drop(searching);
        result
    });
    entered.await;
    gate.open_after_watchdog();

    let outline = call(&other, "get_file_outline", json!({ "file_path": "a.ts" })).await;
    let served_while_blocked = !gate.is_open();
    gate.open();

    assert!(served_while_blocked, "get_file_outline must be answered while the inference is still blocked");
    assert!(
        format!("{:?}", outline.content).contains("no file 'a.ts' found in the index"),
        "the outline handler itself answered: {:?}",
        outline.content
    );
    let search = search.await.unwrap();
    assert_ne!(search.is_error, Some(true), "{:?}", search.content);
}
