//! Acceptance tests for GM-395's slice 3 (`docs/architecture/lazy-indexing.md`,
//! D6, D7 and D2's "independent of the caller"): what a tool call does while
//! it waits for the index.
//!
//! - With a `progressToken` it gets a heartbeat of progress notifications
//!   (`progress` = seconds waited, strictly increasing; `message` = the real
//!   counters, prefixed with the project root), then the full answer.
//! - Without one it gets no notifications at all, and the same full answer.
//! - A wait that reaches the cap is a "still being built, no answer was
//!   computed" tool error, and the next call after the walk answers.
//! - A cancelled wait returns at once, and the walk it triggered carries on.
//!
//! The walk is held open with `bulk_index::WALK_HOLD_FILE_ENV`, which parks
//! the finished-and-linked walk (still `Phase::Walking`) until the file is
//! removed, so every wait below is as long as the test says rather than as
//! long as Node happens to take. The heartbeat and the cap are shrunk from
//! 5 s / 25 min to 200 ms / 500 ms through their env overrides.
//!
//! Everything is real: the real binary, a real shim bootstrapping a real
//! detached daemon, a real `rmcp` client (tests 1, 3, 4) and a raw NDJSON
//! client straight to the daemon's socket (test 2 - `rmcp`'s client attaches
//! a `progressToken` to every request it sends, so it cannot send one
//! without).

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::bulk_index::WALK_HOLD_FILE_ENV;
use g_mesh::ipc;
use g_mesh::mcp::{INDEX_WAIT_CAP_ENV, PROGRESS_INTERVAL_ENV, TRACE_CALLS_ENV};
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest, ContentBlock,
    ProgressNotificationParam,
};
use rmcp::service::{NotificationContext, PeerRequestOptions, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One TS file with three top-level declarations, so "the full outline" is a
/// list a partial or empty index cannot fake.
const FILES: [(&str, &str); 1] = [(
    "src/index.ts",
    "export function connect(): number {\n  return 1;\n}\n\nexport class Pool {}\n\nexport const size = 3;\n",
)];

const FULL_OUTLINE: [&str; 3] = ["connect", "Pool", "size"];

/// What `bulk_index::hold_the_walk_open_for_tests` logs once the walk is
/// parked on the hold file.
const HOLDING_LINE: &str = "holding the finished bulk walk";

/// How long each test keeps a call waiting on the held walk. At a 200 ms
/// heartbeat that is room for about five notifications; test 1 asks for 3.
const HELD_FOR: Duration = Duration::from_millis(1_100);

struct Project {
    dir: tempfile::TempDir,
    /// Outside the project root, so writing to it cannot feed the watcher.
    side: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            side: tempfile::tempdir().expect("failed to create a temp side directory"),
        };
        for (rel, contents) in FILES {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        std::fs::write(project.hold(), b"").expect("failed to create the walk hold file");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn log(&self) -> PathBuf {
        self.side.path().join("daemon.log")
    }

    fn hold(&self) -> PathBuf {
        self.side.path().join("walk.hold")
    }

    fn release(&self) {
        std::fs::remove_file(self.hold()).expect("failed to remove the walk hold file");
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    /// Blocks until the daemon log contains `needle`. Blocking, so every
    /// test here runs on a multi-threaded runtime: on the default
    /// current-thread one this would starve the rmcp client's own transport
    /// task, and a request "sent" before it would never leave the process.
    fn wait_for_log(&self, needle: &str) {
        common::wait_for(&format!("the daemon log to contain {needle:?}"), common::startup_timeout(), || {
            self.log_text().contains(needle)
        });
    }

    /// Blocks until the structural index is built, without triggering
    /// anything: the activation this is waiting on is already running.
    fn wait_until_structural(&self) {
        let path =
            daemon::phase_path_in(&project_dir(self.root()).expect("failed to resolve the state directory"));
        common::wait_for("the walk to finish", common::startup_timeout(), || {
            std::fs::read_to_string(&path)
                .map(|phase| matches!(phase.trim(), "structural" | "embedding" | "ready"))
                .unwrap_or(false)
        });
    }

    /// Both spellings of the root: the daemon reports the canonical one.
    fn root_spellings(&self) -> [String; 2] {
        [self.root().display().to_string(), self.root().canonicalize().unwrap().display().to_string()]
    }

    /// A client over a real shim, which bootstraps the project's daemon with
    /// the walk hold, call tracing and a 200 ms heartbeat, plus `env`.
    async fn connect<H: ClientHandler>(
        &self,
        handler: H,
        env: &[(&str, &str)],
    ) -> RunningService<RoleClient, H> {
        let root = self.root().to_path_buf();
        let (log, hold) = (self.log(), self.hold());
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(WALK_HOLD_FILE_ENV, &hold)
                .env(TRACE_CALLS_ENV, "1")
                .env(PROGRESS_INTERVAL_ENV, "200")
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
            for (key, value) in &env {
                cmd.env(key, value);
            }
        }))
        .expect("failed to spawn the shim");
        handler.serve(transport).await.expect("the shim must reach the daemon")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.hold());
        for path in
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())].into_iter().flatten()
        {
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A client that records every progress notification it receives.
#[derive(Clone, Default)]
struct ProgressRecorder(Arc<Mutex<Vec<ProgressNotificationParam>>>);

impl ProgressRecorder {
    fn seen(&self) -> Vec<ProgressNotificationParam> {
        self.0.lock().unwrap().clone()
    }
}

impl ClientHandler for ProgressRecorder {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.lock().unwrap().push(params);
    }
}

fn outline_request() -> CallToolRequestParams {
    CallToolRequestParams::new("get_file_outline")
        .with_arguments(json!({ "file_path": "src/index.ts" }).as_object().cloned().unwrap())
}

async fn call_outline<H: ClientHandler>(client: &RunningService<RoleClient, H>) -> CallToolResult {
    client
        .call_tool(outline_request())
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn assert_full_outline(result: &CallToolResult) {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {}", text(result));
    let outline: Value = serde_json::from_str(&text(result)).expect("tool result is not JSON");
    let names: Vec<&str> = outline["results"]
        .as_array()
        .unwrap_or_else(|| panic!("an outline has a results array: {outline}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("every outline symbol has a name"))
        .collect();
    assert_eq!(names, FULL_OUTLINE, "the call must answer off the complete index: {outline}");
}

/// D6, test 1: a call carrying a `progressToken` that waits on the held walk
/// gets at least three notifications at a 200 ms heartbeat, with strictly
/// increasing `progress` and the project root in every `message`, and then
/// the full answer. The daemon's trace records the token as present and the
/// count it sent.
///
/// Control: set `G_MESH_PROGRESS_INTERVAL_MS` to `"0"` in `connect` (or
/// delete the ticker branch of `wait_for_index`'s `select!`): 0 notifications
/// arrive and the `>= 3` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_call_with_a_token_gets_a_heartbeat_then_the_full_answer() {
    let project = Project::new();
    let recorder = ProgressRecorder::default();
    let client = project.connect(recorder.clone(), &[]).await;

    let call = tokio::spawn({
        let peer = client.peer().clone();
        async move { peer.call_tool(outline_request()).await }
    });
    project.wait_for_log(HOLDING_LINE);
    tokio::time::sleep(HELD_FOR).await;
    project.release();
    let result = call.await.unwrap().expect("tools/call must return a result, not a protocol failure");
    assert_full_outline(&result);

    let seen = recorder.seen();
    assert!(
        seen.len() >= 3,
        "expected at least 3 progress notifications while held, got {}: {seen:?}",
        seen.len()
    );
    for pair in seen.windows(2) {
        assert!(pair[1].progress > pair[0].progress, "progress must strictly increase: {seen:?}");
    }
    let spellings = project.root_spellings();
    for notification in &seen {
        let message = notification.message.as_deref().unwrap_or_default();
        assert!(
            spellings.iter().any(|root| message.contains(root.as_str())),
            "every progress message must name the project root {spellings:?}: {message:?}"
        );
        assert_eq!(notification.total, None, "D6 sends no total: {notification:?}");
    }

    let log = project.log_text();
    assert!(
        log.contains("prepare: entered tool=get_file_outline") && log.contains("progressToken=present"),
        "the trace must record the request's progressToken as present:\n{log}"
    );
    assert!(
        log.contains("outcome=satisfied") && log.contains(&format!("progress_sent={}", seen.len())),
        "the trace must record the wait's outcome and how many notifications it sent ({}):\n{log}",
        seen.len()
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// D6, test 2: the same wait on a request with no `progressToken` sends no
/// notification at all, and still answers in full. The trace records the
/// token as absent.
///
/// Sent over a raw NDJSON connection to the daemon, because `rmcp`'s client
/// attaches a token to every request. The shim's session is only there to
/// bootstrap the daemon.
///
/// Control: in `wait_for_index`, make the ticker send regardless of the
/// token (e.g. `let token = Some(ctx.meta.get_progress_token().unwrap_or(
/// rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(0))))`):
/// notifications arrive and the `== 0` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_call_without_a_token_gets_no_notifications_and_the_full_answer() {
    let project = Project::new();
    let bootstrap = project.connect((), &[]).await;

    let endpoint = daemon::endpoint(project.root()).expect("the daemon must have an endpoint");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(raw_outline_call_without_token(&endpoint));
    });

    project.wait_for_log(HOLDING_LINE);
    tokio::time::sleep(HELD_FOR).await;
    project.release();
    let (progress_frames, result) = done_rx
        .recv_timeout(common::startup_timeout())
        .expect("the raw call must be answered once the walk is released");
    let result: CallToolResult =
        serde_json::from_value(result).expect("the response must be a CallToolResult");

    assert_eq!(progress_frames, 0, "a request without a progressToken must get no progress notifications");
    assert_full_outline(&result);
    let log = project.log_text();
    assert!(log.contains("progressToken=absent"), "the trace must record the missing progressToken:\n{log}");

    bootstrap.cancel().await.expect("failed to shut the client down");
}

/// Handshakes on a raw connection, sends one `get_file_outline` call with no
/// `_meta`, and reads frames until its response: returns how many
/// `notifications/progress` frames arrived first, and the response's
/// `result`.
fn raw_outline_call_without_token(endpoint: &ipc::Endpoint) -> (usize, Value) {
    let stream = ipc::Stream::connect(endpoint).expect("failed to connect to the daemon");
    let mut writer = stream.try_clone().expect("failed to clone the connection");
    let mut reader = BufReader::new(stream);
    let mut send = |message: Value| {
        write_ndjson_frame(&mut writer, &serde_json::to_vec(&message).unwrap())
            .expect("failed to send a frame")
    };
    let mut read = || -> Value {
        let frame = read_ndjson_frame(&mut reader)
            .expect("failed to read a frame")
            .expect("the daemon closed the connection before answering");
        serde_json::from_slice(&frame).expect("a frame is JSON")
    };

    send(json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-test-no-token", "version": "0" },
        },
    }));
    let initialized = read();
    assert_eq!(initialized["id"], 0, "expected the initialize response first: {initialized}");
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": "src/index.ts" } },
    }));

    let mut progress_frames = 0;
    loop {
        let frame = read();
        if frame["method"] == "notifications/progress" {
            progress_frames += 1;
        } else if frame["id"] == 1 {
            let result = frame.get("result").cloned().unwrap_or_else(|| panic!("tools/call failed: {frame}"));
            return (progress_frames, result);
        }
    }
}

/// D7, test 3: with the cap at 500 ms and the walk held, the call returns a
/// "still being built, no answer was computed" tool error well within 2 s,
/// carrying no rows. Once the walk is released and finished, the same call
/// answers in full.
///
/// Control: set `G_MESH_INDEX_WAIT_CAP_MS` to `"0"` (no cap) below, or pass
/// `None` instead of `deadline` to `wait_for` in `wait_for_index`: the first
/// call does not return within the 3 s timeout and the test fails there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_that_reaches_the_cap_is_a_retryable_tool_error() {
    let project = Project::new();
    let client = project.connect((), &[(INDEX_WAIT_CAP_ENV, "500")]).await;

    let started = Instant::now();
    let capped = tokio::time::timeout(Duration::from_secs(3), call_outline(&client))
        .await
        .expect("a call held past the cap must return, not keep waiting");
    let took = started.elapsed();
    assert!(took < Duration::from_secs(2), "the capped call must return within 2 s, took {took:?}");
    assert_eq!(capped.is_error, Some(true), "a capped wait is a tool error: {}", text(&capped));
    let message = text(&capped);
    assert!(
        message.contains("still being built") && message.contains("No answer was computed"),
        "the error must say the index is still being built and nothing was answered: {message}"
    );
    assert!(
        serde_json::from_str::<Value>(&message).is_err(),
        "the error must carry no result body a caller could read as rows: {message}"
    );
    assert!(
        project.log_text().contains("outcome=timed_out"),
        "the trace must record the cap:\n{}",
        project.log_text()
    );

    project.release();
    project.wait_until_structural();
    assert_full_outline(&call_outline(&client).await);

    client.cancel().await.expect("failed to shut the client down");
}

/// D2 / D6, test 4: cancelling a waiting call (`notifications/cancelled`)
/// makes its handler return at once - the trace logs `prepare: cancelled` -
/// while the walk it triggered keeps going: after release the walk is
/// recorded and a later call answers in full.
///
/// Control: delete the `ctx.ct.cancelled()` branch of `wait_for_index`'s
/// `select!`: no "prepare: cancelled" line appears and the 2 s wait for it
/// fails (and, were that wait skipped, the cancelled call's wait would end
/// `outcome=satisfied` after release, failing the count of those lines).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_wait_returns_at_once_and_the_walk_continues() {
    let project = Project::new();
    let client = project.connect((), &[]).await;

    let handle = client
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(outline_request())),
            PeerRequestOptions::no_options(),
        )
        .await
        .expect("failed to send the call");
    project.wait_for_log(HOLDING_LINE);
    handle.cancel(Some("test".to_string())).await.expect("failed to send notifications/cancelled");

    // Within 2 s, not the startup budget: "returns at once" is the claim.
    common::wait_for("the handler to log its cancellation", Duration::from_secs(2), || {
        project.log_text().contains("prepare: cancelled tool=get_file_outline")
    });
    assert!(
        !project.log_text().contains("initial index built"),
        "sanity: the walk must still be held when the call is cancelled"
    );

    project.release();
    project.wait_for_log("initial index built");
    let result = call_outline(&client).await;
    assert_full_outline(&result);
    // Only the later call's wait ended satisfied: the cancelled one did not
    // go on waiting in the background and finish once the walk was released.
    let log = project.log_text();
    assert_eq!(
        log.matches("outcome=satisfied").count(),
        1,
        "only the later call may have finished its wait:\n{log}"
    );

    client.cancel().await.expect("failed to shut the client down");
}
