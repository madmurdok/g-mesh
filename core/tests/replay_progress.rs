//! Acceptance test for GM-403: progress while `prepare` replays a language's
//! queued dirty-file changes (`mcp::GMeshMcpServer::replay_queued_changes`,
//! `daemon::registry::PluginRegistry::replay_pending`).
//!
//! GM-401 heartbeated a query-time reindex (`ensure_file_fresh`) because a
//! cold language server can take tens of seconds and, before that fix,
//! nothing was sent to the client while it ran. The replay this test covers
//! is the same silent gap one call earlier in `prepare`: a language whose
//! plugin was asleep queues every file changed in the meantime, and the next
//! tool call - any tool call, not necessarily one about a queued file -
//! replays the whole queue synchronously before it answers. On a cold
//! language server that is the same tens-of-seconds cost, and until this
//! task nothing ticked while it ran.
//!
//! The setup: shorten the plugin's idle timeout so it sleeps on its own
//! (`daemon::lifecycle::PLUGIN_IDLE_ENV`, the same knob `idle_lifecycle.rs`
//! uses), edit a file while it is asleep so the change queues rather than
//! being applied, then hold the replay's round trip open with
//! `watcher::apply::HOLD_COMPUTE_FILE_ENV` - the same hook
//! `first_query_after_walk.rs`'s test 3 uses to hold `ensure_file_fresh`'s
//! own reindex - so the test controls exactly how long the replay takes
//! rather than racing a real tsserver-class cold start.
//!
//! Everything is real: the real binary, a real shim bootstrapping a real
//! detached daemon, the real bundled JS/TS plugin, and a real `rmcp` client.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::daemon::lifecycle::PLUGIN_IDLE_ENV;
use g_mesh::mcp::{PROGRESS_INTERVAL_ENV, TRACE_CALLS_ENV};
use g_mesh::storage::connection::project_dir;
use g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ProgressNotificationParam};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Two independent TS files: `alpha.ts` is the one edited while the plugin
/// sleeps (and therefore the one whose replay is held open), `beta.ts` is the
/// one the held tool call actually asks about - untouched since the walk, so
/// its own `ensure_file_fresh` check is a free `AlreadyFresh` and every
/// progress notification the test sees can only have come from the replay,
/// not from a second heartbeat of `ensure_file_fresh`'s own (GM-401).
const ALPHA_OLD: &str = "export function alpha(): number {\n  return 1;\n}\n";
const ALPHA_NEW: &str = "export function alpha(): number {\n  return 1;\n}\n\nexport function alphaAgain(): number {\n  return 2;\n}\n";
const BETA: &str = "export function beta(): number {\n  return 3;\n}\n";

/// Short enough that the plugin sleeps on its own within a couple of seconds
/// once activation settles, long enough that the test can still catch it
/// awake to make its own assertions - same reasoning as `idle_lifecycle.rs`'s
/// identically-named constant, and the same value.
const PLUGIN_IDLE: Duration = Duration::from_millis(1_500);

/// What `watcher::apply`'s test-only hold logs once a round trip is parked -
/// unconditionally, whether or not the hold file actually exists (see
/// `hold_compute_open_for_tests`'s own doc comment), so the whole-project
/// semantic pass that runs during cold start prints this line too. The test
/// counts occurrences rather than merely checking presence for exactly that
/// reason - see `Project::wait_for_new_hold`.
const HOLDING_LINE: &str = "holding a reparse's lock-free embedding window open";

/// How long the test keeps the replay held open. At a 200 ms heartbeat that
/// is room for about five notifications; the test asks for 3.
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
        project.write("src/alpha.ts", ALPHA_OLD);
        project.write("src/beta.ts", BETA);
        project
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(&path, contents).expect("failed to write a fixture file");
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn log(&self) -> PathBuf {
        self.side.path().join("daemon.log")
    }

    /// Named for the daemon at spawn, created only when the test wants the
    /// hold active - see `HOLD_COMPUTE_FILE_ENV`'s own doc comment.
    fn hold(&self) -> PathBuf {
        self.side.path().join("compute.hold")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    fn wait_for(&self, what: &str, ready: impl FnMut() -> bool) {
        common::wait_for(what, common::startup_timeout(), ready);
    }

    /// Blocks until the plugin has gone to sleep on its own shortened idle
    /// timeout - the pid file is removed only after the process is actually
    /// reaped (`daemon::lifecycle`), so this cannot observe a half-finished
    /// sleep.
    fn wait_until_plugin_sleeps(&self) {
        let pid_file = self.plugin_pid_file();
        self.wait_for("the plugin to sleep on its shortened idle timeout", || !pid_file.exists());
    }

    /// Blocks until a *new* occurrence of [`HOLDING_LINE`] appears beyond
    /// `baseline` - not merely until the line is present, since the
    /// whole-project semantic pass that runs during cold start (before this
    /// project's `connect` even returns) already printed it once.
    fn wait_for_new_hold(&self, baseline: usize) {
        self.wait_for("the replay's round trip to reach its hold", || {
            self.log_text().matches(HOLDING_LINE).count() > baseline
        });
    }

    fn hold_count(&self) -> usize {
        self.log_text().matches(HOLDING_LINE).count()
    }

    /// A client over a real shim, which bootstraps the project's daemon with
    /// call tracing, a 200 ms heartbeat, the compute hold's file name and a
    /// shortened plugin idle timeout.
    async fn connect<H: ClientHandler>(&self, handler: H) -> RunningService<RoleClient, H> {
        let root = self.root().to_path_buf();
        let (log, hold) = (self.log(), self.hold());
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(HOLD_COMPUTE_FILE_ENV, &hold)
                .env(TRACE_CALLS_ENV, "1")
                .env(PROGRESS_INTERVAL_ENV, "200")
                .env(PLUGIN_IDLE_ENV, PLUGIN_IDLE.as_millis().to_string())
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        let client = handler.serve(transport).await.expect("the shim must reach the daemon");
        // The walk, its whole-project semantic pass and the backfill, all
        // done: by the time `Phase::Ready` is reached the semantic pass has
        // already run and committed (`daemon::activation::Activation::walk`
        // runs it inline, before the watcher consumer even starts), so
        // nothing of the cold start is left to hold a round trip open or
        // contend with the calls below.
        let root = self.root().to_path_buf();
        tokio::task::spawn_blocking(move || common::wait_until_phase(&root, "ready")).await.unwrap();
        client
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

fn outline_request(file: &str) -> CallToolRequestParams {
    CallToolRequestParams::new("get_file_outline")
        .with_arguments(json!({ "file_path": file }).as_object().cloned().unwrap())
}

fn names(result: &CallToolResult) -> Vec<String> {
    let text = match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert_ne!(result.is_error, Some(true), "expected a successful call: {text}");
    let outline: Value = serde_json::from_str(&text).expect("tool result is not JSON");
    outline["results"]
        .as_array()
        .unwrap_or_else(|| panic!("an outline has a results array: {outline}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("every outline symbol has a name").to_string())
        .collect()
}

/// The acceptance criterion: a request carrying a `progressToken` that lands
/// while a language's queued replay is held open gets at least three
/// heartbeats at 200 ms, `progress` strictly increasing, `message` naming the
/// language and the queue size, and then the full answer for the file it
/// actually asked about (which was never queued - see `BETA`'s doc comment
/// above).
///
/// Control: in `mcp::GMeshMcpServer::replay_queued_changes`, delete the
/// ticker branch of the `select!` (or drop the whole `tokio::select!` loop
/// and simply `.await` the spawned task directly, as the pre-GM-403 version
/// did): the held replay produces zero notifications and the `>= 3`
/// assertion fails; the call still answers correctly once the hold is
/// released, since the fix changes nothing about what gets replayed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_replay_with_a_token_gets_a_heartbeat_then_the_full_answer() {
    let project = Project::new();
    let recorder = ProgressRecorder::default();
    let client = project.connect(recorder.clone()).await;
    assert!(recorder.seen().is_empty(), "precondition: nothing sent before the call");

    // --- the plugin falls asleep on its own -----------------------------
    project.wait_until_plugin_sleeps();

    // --- a change made while it sleeps is queued, not processed ----------
    let baseline_holds = project.hold_count();
    project.write("src/alpha.ts", ALPHA_NEW);
    // Long enough for the watcher's debounce to settle and queue the change
    // (see `idle_lifecycle.rs`'s identical wait and its own comment on why).
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!project.plugin_pid_file().exists(), "sanity: a queued change alone must not wake the plugin");

    // --- hold the replay's round trip open, then ask about the *other* file
    std::fs::write(project.hold(), b"").expect("failed to create the compute hold file");
    let call = tokio::spawn({
        let peer = client.peer().clone();
        async move { peer.call_tool(outline_request("src/beta.ts")).await }
    });
    project.wait_for_new_hold(baseline_holds);
    tokio::time::sleep(HELD_FOR).await;
    std::fs::remove_file(project.hold()).expect("failed to remove the compute hold file");

    let result = call.await.unwrap().expect("tools/call must return a result, not a protocol failure");
    assert_eq!(names(&result), ["beta"], "the call must still answer the question it was actually asked");

    let seen = recorder.seen();
    assert!(
        seen.len() >= 3,
        "expected at least 3 progress notifications while the replay was held, got {}: {seen:?}",
        seen.len()
    );
    for pair in seen.windows(2) {
        assert!(pair[1].progress > pair[0].progress, "progress must strictly increase: {seen:?}");
    }
    for notification in &seen {
        let message = notification.message.as_deref().unwrap_or_default();
        assert!(message.contains("typescript"), "every message must name the language: {message:?}");
        assert!(
            message.contains("1 file"),
            "every message must name the queue size (one queued file): {message:?}"
        );
        assert_eq!(notification.total, None, "no total is sent, matching the other progress tickers");
    }

    project.wait_for("the replay's trace line to appear", || {
        project.log_text().contains("replay: tool=get_file_outline")
    });
    let log = project.log_text();
    let replay_lines: Vec<&str> =
        log.lines().filter(|line| line.contains("replay: tool=get_file_outline")).collect();
    assert_eq!(replay_lines.len(), 1, "one tool call, one replay:\n{log}");
    assert!(
        replay_lines[0].contains("replayed=1") && replay_lines[0].contains("summary=typescript (1 file)"),
        "the trace must record what was replayed and for which language: {}",
        replay_lines[0]
    );
    assert!(
        replay_lines[0].contains(&format!("progress_sent={}", seen.len())),
        "the trace must record how many notifications the replay itself sent: {}",
        replay_lines[0]
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// The same held replay, on a request with no `progressToken`, sends no
/// notification at all and still answers in full - `rmcp`'s client attaches a
/// token to every request it sends, so this goes over a raw NDJSON connection
/// like `index_wait_progress.rs`'s equivalent test does.
///
/// Control: in `replay_queued_changes`, make the ticker send regardless of
/// the token (e.g. drop the `.filter(|_| !interval.is_zero())`/token check
/// and always clone a token to send with): notifications arrive over the raw
/// connection and the `== 0` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_replay_without_a_token_gets_no_notifications_and_the_full_answer() {
    let project = Project::new();
    let bootstrap = project.connect(()).await;

    project.wait_until_plugin_sleeps();

    let baseline_holds = project.hold_count();
    project.write("src/alpha.ts", ALPHA_NEW);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(!project.plugin_pid_file().exists(), "sanity: a queued change alone must not wake the plugin");

    std::fs::write(project.hold(), b"").expect("failed to create the compute hold file");
    let endpoint = daemon::endpoint(project.root()).expect("the daemon must have an endpoint");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(raw_outline_call_without_token(&endpoint));
    });

    project.wait_for_new_hold(baseline_holds);
    tokio::time::sleep(HELD_FOR).await;
    std::fs::remove_file(project.hold()).expect("failed to remove the compute hold file");

    let (progress_frames, result) = done_rx
        .recv_timeout(common::startup_timeout())
        .expect("the raw call must be answered once the replay is released");
    let result: CallToolResult =
        serde_json::from_value(result).expect("the response must be a CallToolResult");

    assert_eq!(progress_frames, 0, "a request without a progressToken must get no progress notifications");
    assert_eq!(names(&result), ["beta"]);

    bootstrap.cancel().await.expect("failed to shut the client down");
}

/// Handshakes on a raw connection, sends one `get_file_outline` call with no
/// `_meta`, and reads frames until its response: returns how many
/// `notifications/progress` frames arrived first, and the response's
/// `result`.
fn raw_outline_call_without_token(endpoint: &g_mesh::ipc::Endpoint) -> (usize, Value) {
    use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
    use std::io::BufReader;

    let stream = g_mesh::ipc::Stream::connect(endpoint).expect("failed to connect to the daemon");
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
        "params": { "name": "get_file_outline", "arguments": { "file_path": "src/beta.ts" } },
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
