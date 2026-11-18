//! `GMeshMcpServer` served over a fake [`McpHost`] alone: the order the
//! trait's lock-order doc states (use recorded before the replay, no store
//! guard held around it), one `Activity::request` per tool call, and the
//! per-call `QueryCtx`'s `uncovered` reaching every find tool.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::ContentBlock;
use rusqlite::Connection;
use serde_json::{json, Value};

use super::search_code_worker_tests::{call, connect};
use super::GMeshMcpServer;
use crate::embedding::EmbeddingPipeline;
use crate::indexing_status::{IndexingStatus, Phase};
use crate::mcp::host::{Activity, McpHost, PathCoverage};
use crate::mcp::host_fake::FakeHost;
use crate::mcp::query::test_support::Rendered;
use crate::model::{Diff, EdgeRecord, NodeRecord};
use crate::plugin_manifest::{Capabilities, PluginManifest};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::apply_diff;

/// `meta.lastUsed` before any tool call: older than any `touch`.
const SEEDED_LAST_USED: &str = "2000-01-01 00:00:00.000";

/// How long `replay_pending` waits for the store before it reports it held.
const STORE_PROBE: Duration = Duration::from_secs(2);

/// A [`FakeHost`] that records, in order, each `Activity::request` and each
/// `replay_pending` - with what the store says at that moment.
struct Recording {
    inner: FakeHost,
    store: Arc<IndexStore>,
    events: Mutex<Vec<String>>,
}

impl Recording {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl Activity for Recording {
    fn request(&self) {
        self.events.lock().unwrap().push("request".to_string());
        self.inner.request();
    }
}

impl McpHost for Recording {
    fn project_root(&self) -> &Path {
        self.inner.project_root()
    }
    fn manifests(&self) -> Vec<&PluginManifest> {
        self.inner.manifests()
    }
    fn receiver_call_capabilities(&self) -> HashMap<String, Capabilities> {
        self.inner.receiver_call_capabilities()
    }
    fn entry_points(&self) -> Vec<String> {
        self.inner.entry_points()
    }
    fn missing_languages(&self) -> Vec<&'static str> {
        self.inner.missing_languages()
    }
    fn path_coverage(&self, file_path: &str) -> Option<PathCoverage> {
        self.inner.path_coverage(file_path)
    }
    fn has_pending(&self) -> bool {
        self.inner.has_pending()
    }
    fn pending_summary(&self) -> String {
        self.inner.pending_summary()
    }

    /// Reads `meta.lastUsed` from another thread: a store guard held by the
    /// caller shows as a timeout instead of a deadlock.
    fn replay_pending(&self, store: &IndexStore) -> usize {
        let (tx, rx) = mpsc::channel();
        let probe = Arc::clone(&self.store);
        std::thread::spawn(move || {
            let last_used: String = probe.with(|conn| {
                conn.query_row("SELECT lastUsed FROM meta WHERE id = 1", [], |r| r.get(0)).unwrap()
            });
            let _ = tx.send(last_used);
        });
        let seen = match rx.recv_timeout(STORE_PROBE) {
            Ok(last_used) => format!("replay lastUsed={last_used}"),
            Err(_) => "replay store-held".to_string(),
        };
        self.events.lock().unwrap().push(seen);
        self.inner.replay_pending(store)
    }

    fn ensure_fresh(&self, store: &IndexStore, file_path: &str) -> anyhow::Result<Option<String>> {
        self.inner.ensure_fresh(store, file_path)
    }
    fn watcher_quiet(&self) -> Option<Duration> {
        self.inner.watcher_quiet()
    }
}

fn node(id: &str, kind: &str, file: &str) -> NodeRecord {
    NodeRecord::new(id, kind, id, format!("pkg::{id}"), file, "rust")
}

/// `c1` (in `c.rs`) calls `target` (in `t.rs`); `c_impl` implements `iface`.
fn store() -> Arc<IndexStore> {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let diff = Diff {
        upsert_nodes: vec![
            NodeRecord::new("file-c.rs", "File", "c.rs", "c.rs", "c.rs", "rust"),
            NodeRecord::new("file-t.rs", "File", "t.rs", "t.rs", "t.rs", "rust"),
            node("target", "Function", "t.rs"),
            node("c1", "Function", "c.rs"),
            node("iface", "Interface", "t.rs"),
            node("c_impl", "Class", "c.rs"),
        ],
        upsert_edges: vec![
            EdgeRecord::new("e1", "c1", "target", "CALLS", "tree-sitter", true),
            EdgeRecord::new("s1", "c_impl", "iface", "SUPERTYPE_OF", "tree-sitter", true),
        ],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();
    conn.execute(
        "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, 'test', 'test', ?1)
         ON CONFLICT(id) DO UPDATE SET lastUsed = excluded.lastUsed",
        [SEEDED_LAST_USED],
    )
    .unwrap();
    Arc::new(IndexStore::new(conn))
}

fn server(store: Arc<IndexStore>, host: Arc<dyn McpHost>) -> GMeshMcpServer {
    let indexing = IndexingStatus::structural();
    indexing.set_phase(Phase::Ready);
    GMeshMcpServer::new(store, host, indexing, Arc::new(EmbeddingPipeline::disabled()))
}

fn project(dir: &Path) -> std::path::PathBuf {
    let root = dir.join("project");
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn json_body(result: &impl Rendered) -> Value {
    let result = &result.rendered();
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

/// With changes queued, a tool call records use (`Activity::request` and the
/// `lastUsed` write) before the replay, and calls the replay with no store
/// guard held; with nothing queued, it records use and replays nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_records_use_before_the_replay_and_holds_no_store_guard_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store();
    let host = Arc::new(Recording {
        inner: FakeHost::new(&project(dir.path())),
        store: Arc::clone(&store),
        events: Mutex::new(Vec::new()),
    });
    host.inner.queue(3);
    let client = connect(server(Arc::clone(&store), Arc::clone(&host) as Arc<dyn McpHost>)).await;

    call(&client, "find_references", json!({ "symbol_id": "target" })).await;
    let events = host.events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], "request", "use is recorded first: {events:?}");
    let last_used = events[1].strip_prefix("replay lastUsed=").unwrap_or_else(|| {
        panic!("the replay ran with the store free: {events:?}");
    });
    assert!(last_used > SEEDED_LAST_USED, "lastUsed was written before the replay: {events:?}");
    assert!(!host.has_pending(), "the queue was drained");

    call(&client, "find_callers", json!({ "symbol_id": "target" })).await;
    assert_eq!(host.events()[2..], ["request".to_string()], "nothing queued, nothing replayed");
}

/// Every tool answers over a `FakeHost` alone, and each tool call records
/// exactly one `Activity::request`; the session's `initialize` and
/// `tools/list` record none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_tool_call_records_one_request_over_a_fake_host() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(FakeHost::new(&project(dir.path())));
    let client = connect(server(store(), Arc::clone(&host) as Arc<dyn McpHost>)).await;

    let listed = client.list_all_tools().await.expect("tools/list must answer");
    assert_eq!(host.requests(), 0, "initialize and tools/list are not tool calls");

    let calls = [
        ("find_definition", json!({ "symbol_name": "target" })),
        ("find_references", json!({ "symbol_id": "target" })),
        ("find_callers", json!({ "symbol_id": "target" })),
        ("find_callees", json!({ "symbol_id": "c1" })),
        ("find_implementations", json!({ "symbol_id": "iface" })),
        ("get_file_outline", json!({ "file_path": "c.rs" })),
        ("get_dependencies", json!({ "file_path": "c.rs", "direction": "Outgoing" })),
        ("search_code", json!({ "query": "anything" })),
    ];
    let mut names: Vec<&str> = listed.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    let mut called: Vec<&str> = calls.iter().map(|(tool, _)| *tool).collect();
    called.sort_unstable();
    assert_eq!(names, called, "every listed tool is called below");
    for (n, (tool, arguments)) in calls.into_iter().enumerate() {
        call(&client, tool, arguments).await;
        assert_eq!(host.requests(), n + 1, "{tool}: one request per tool call");
    }
    let callers = json_body(&call(&client, "find_callers", json!({ "symbol_id": "target" })).await);
    assert_eq!(callers["results"][0]["callerSymbolId"], "c1", "answered from the store: {callers}");
}

/// Each find tool builds its context from the call's own `file_paths`: an
/// entry whose language is not indexed is named in `notIndexed`. A
/// transitive `find_implementations` ignores `file_paths`, so it names none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_find_tool_names_its_unindexed_filter_entries() {
    let dir = tempfile::tempdir().unwrap();
    let host: Arc<dyn McpHost> = Arc::new(FakeHost::new(&project(dir.path())));
    let client = connect(server(store(), host)).await;
    let filter = json!(["c.rs", "tools/gen.py"]);

    for (tool, anchor) in [
        ("find_references", "target"),
        ("find_callers", "target"),
        ("find_callees", "c1"),
        ("find_implementations", "iface"),
    ] {
        let body =
            json_body(&call(&client, tool, json!({ "symbol_id": anchor, "file_paths": filter })).await);
        let named: Vec<&str> = body["notIndexed"]
            .as_array()
            .unwrap_or_else(|| panic!("{tool}: no notIndexed: {body}"))
            .iter()
            .flat_map(|entry| entry["filePaths"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        assert!(named.contains(&"tools/gen.py"), "{tool}: {body}");
    }

    let transitive = json_body(
        &call(
            &client,
            "find_implementations",
            json!({ "symbol_id": "iface", "file_paths": filter, "transitive": true }),
        )
        .await,
    );
    assert!(transitive.get("notIndexed").is_none(), "{transitive}");
}
