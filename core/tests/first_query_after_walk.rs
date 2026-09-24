//! Acceptance tests for GM-401: the first query of a file after a fresh walk.
//!
//! S1 (`docs/results/gm-401-first-answer-gap.md`) traced a 51-77 s silent gap
//! on a cold project's first `get_file_outline` to two things:
//!
//! - The bulk walk wrote no `indexed_files` baselines, so the first query of
//!   *any* file read it as never indexed and paid a synchronous reindex (a
//!   `fileChanged` round trip plus a per-file semantic pass) before it
//!   answered.
//! - Nothing was sent to the client while that reindex ran: the progress
//!   heartbeat stopped when the indexing wait did.
//!
//! The three tests below cover the fix to each, and the guard that the
//! baselines do not hide a real edit:
//!
//! 1. After a walk, every walked file whose bytes predate the walk has a
//!    baseline, and the first outline of one is answered off the fast path:
//!    the daemon's trace records `outcome=AlreadyFresh`.
//! 2. A file edited after the walk is still reindexed by the next query
//!    (`outcome=ReindexedViaHashMismatch`) and answered with the edit.
//! 3. A query-time reindex that takes a while sends heartbeats the whole
//!    time, then the full answer.
//!
//! Everything is real: the real binary, a real shim bootstrapping a real
//! detached daemon, the real bundled JS/TS plugin, and a real `rmcp` client.
//! File mtimes are set explicitly rather than left to when the fixture
//! happened to be written, because the walk's baseline rule is about mtimes
//! relative to the walk's start (`watcher::staleness::record_walk_baselines`)
//! and a slow machine must not move a file from one side of it to the other.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use g_mesh::daemon;
use g_mesh::mcp::{PROGRESS_INTERVAL_ENV, TRACE_CALLS_ENV};
use g_mesh::storage::connection::project_dir;
use g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, ProgressNotificationParam};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Last written an hour before the test: firmly before any walk's start, so
/// the walk baselines it.
const OLD: [(&str, &str); 2] = [
    ("src/index.ts", "export function connect(): number {\n  return 1;\n}\n\nexport class Pool {}\n"),
    ("src/other.ts", "export const size = 3;\n"),
];

/// Stamped an hour in the *future*: never before a walk's start, so the walk
/// must leave it without a baseline, and its first query must reindex it.
const LATE: (&str, &str) = ("src/late.ts", "export function late(): void {}\n");

/// What `watcher::apply`'s test-only hold logs once a round trip is parked.
const HOLDING_LINE: &str = "holding a reparse's lock-free embedding window open";

/// How long test 3 keeps the reindex in flight. At a 200 ms heartbeat that is
/// room for about five notifications; the test asks for 3.
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
        let now = SystemTime::now();
        let hour = Duration::from_secs(3600);
        for (rel, contents) in OLD {
            project.write(rel, contents, now - hour);
        }
        project.write(LATE.0, LATE.1, now + hour);
        project
    }

    fn write(&self, rel: &str, contents: &str, mtime: SystemTime) {
        let path = self.root().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(&path, contents).expect("failed to write a fixture file");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(mtime))
            .expect("failed to set a fixture file's mtime");
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn log(&self) -> PathBuf {
        self.side.path().join("daemon.log")
    }

    /// Named for the daemon at spawn, created only when a test wants the
    /// hold - see `HOLD_COMPUTE_FILE_ENV`'s own doc comment.
    fn hold(&self) -> PathBuf {
        self.side.path().join("compute.hold")
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn wait_for_log(&self, needle: &str) {
        common::wait_for(&format!("the daemon log to contain {needle:?}"), common::startup_timeout(), || {
            self.log_text().contains(needle)
        });
    }

    /// The `ensure_fresh` trace lines the daemon logged for `file`.
    fn ensure_fresh_lines(&self, file: &str) -> Vec<String> {
        let needle = format!("file={file} ");
        self.log_text()
            .lines()
            .filter(|line| line.contains("ensure_fresh: tool=") && line.contains(&needle))
            .map(str::to_string)
            .collect()
    }

    /// `indexed_files`' rows, read through a connection of the test's own.
    fn baselines(&self) -> Vec<String> {
        let db = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        let conn = Connection::open(db).expect("failed to open the index");
        let mut stmt = conn.prepare("SELECT filePath FROM indexed_files ORDER BY filePath").unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0)).unwrap().map(Result::unwrap).collect()
    }

    /// A client over a real shim, which bootstraps the project's daemon with
    /// call tracing, a 200 ms heartbeat and the compute hold's file name.
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
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        let client = handler.serve(transport).await.expect("the shim must reach the daemon");
        // The walk, its semantic pass and the backfill, all done: nothing of
        // the activation's own is left to contend with the calls below.
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

async fn outline<H: ClientHandler>(client: &RunningService<RoleClient, H>, file: &str) -> Vec<String> {
    let result = client
        .call_tool(outline_request(file))
        .await
        .expect("tools/call must return a result, not a protocol failure");
    names(&result)
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

/// Test 1: the walk leaves a baseline for each file whose bytes predate it -
/// including `src/other.ts`, which nothing below ever queries, so its row can
/// only have come from the walk - and none for `src/late.ts`, stamped after
/// the walk's start. The first outline of `src/index.ts` is then answered off
/// the fast path: the daemon traces `outcome=AlreadyFresh`, i.e. no
/// `fileChanged` round trip and no semantic pass.
///
/// Control: in `daemon::bulk_index::run_with_progress`, delete the
/// `staleness::record_walk_baselines` call (or pass it an empty iterator).
/// The baselines assertion fails on an empty table, and the trace reads
/// `outcome=ReindexedNoPriorRecord`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_query_after_a_walk_takes_the_fast_path() {
    let project = Project::new();
    let client = project.connect(()).await;

    assert_eq!(
        project.baselines(),
        ["src/index.ts", "src/other.ts"],
        "the walk must baseline exactly the files whose bytes predate it"
    );

    assert_eq!(outline(&client, "src/index.ts").await, ["connect", "Pool"]);
    let lines = project.ensure_fresh_lines("src/index.ts");
    assert_eq!(lines.len(), 1, "one outline, one staleness check:\n{}", project.log_text());
    assert!(
        lines[0].contains("outcome=AlreadyFresh"),
        "the first query of an untouched walked file must not reindex it: {}",
        lines[0]
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// Test 2, the guard against a baseline that lies: a walked file edited after
/// the walk is reindexed by the next query that touches it, and the answer
/// carries the edit. The watcher may also have reparsed it by then, but it
/// never moves a baseline - only `ensure_fresh` does - so the check itself
/// must still see the walk's old hash and reindex (`ReindexedViaHashMismatch`).
///
/// Control: in `watcher::staleness::decide`, return `Decision::AlreadyFresh`
/// whenever a prior record exists (ignoring its mtime and hash). The trace
/// then reads `outcome=AlreadyFresh` and the outcome assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_edited_after_the_walk_is_still_reindexed() {
    let project = Project::new();
    let client = project.connect(()).await;
    assert!(project.baselines().contains(&"src/other.ts".to_string()), "precondition: other.ts is baselined");

    project.write(
        "src/other.ts",
        "export const size = 3;\n\nexport function grown(): void {}\n",
        SystemTime::now(),
    );
    assert_eq!(outline(&client, "src/other.ts").await, ["size", "grown"]);

    let lines = project.ensure_fresh_lines("src/other.ts");
    assert_eq!(lines.len(), 1, "one outline, one staleness check:\n{}", project.log_text());
    assert!(
        lines[0].contains("outcome=ReindexedViaHashMismatch"),
        "an edit after the walk must be reindexed, not trusted to the walk's baseline: {}",
        lines[0]
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// Test 3: a query-time reindex that is held in flight (`HOLD_COMPUTE_FILE_ENV`
/// parks its round trip after the commit) gets a heartbeat of progress
/// notifications at 200 ms, strictly increasing and naming the file, then the
/// full answer. The index was `ready` before the call, so every notification
/// came from `ensure_file_fresh`, not from the indexing wait; the trace's
/// `progress_sent` on the `ensure_fresh` line says the same.
///
/// Control: set `G_MESH_PROGRESS_INTERVAL_MS` to `"0"` in `connect` (or
/// delete the ticker branch of `ensure_file_fresh`'s `select!`): 0
/// notifications arrive and the `>= 3` assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_query_time_reindex_sends_a_heartbeat_then_the_full_answer() {
    let project = Project::new();
    let recorder = ProgressRecorder::default();
    let client = project.connect(recorder.clone()).await;
    assert!(recorder.seen().is_empty(), "precondition: nothing sent before the call");

    std::fs::write(project.hold(), b"").expect("failed to create the compute hold file");
    let call = tokio::spawn({
        let peer = client.peer().clone();
        async move { peer.call_tool(outline_request(LATE.0)).await }
    });
    tokio::task::spawn_blocking({
        let log = project.log();
        move || {
            common::wait_for("the reindex to reach its hold", common::startup_timeout(), || {
                std::fs::read_to_string(&log).unwrap_or_default().contains(HOLDING_LINE)
            })
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(HELD_FOR).await;
    std::fs::remove_file(project.hold()).expect("failed to remove the compute hold file");

    let result = call.await.unwrap().expect("tools/call must return a result, not a protocol failure");
    assert_eq!(names(&result), ["late"]);

    let seen = recorder.seen();
    assert!(
        seen.len() >= 3,
        "expected at least 3 progress notifications while the reindex was held, got {}: {seen:?}",
        seen.len()
    );
    for pair in seen.windows(2) {
        assert!(pair[1].progress > pair[0].progress, "progress must strictly increase: {seen:?}");
    }
    for notification in &seen {
        let message = notification.message.as_deref().unwrap_or_default();
        assert!(
            message.contains(LATE.0),
            "every message must name the file being brought up to date: {message:?}"
        );
    }

    project.wait_for_log("ensure_fresh: tool=get_file_outline");
    let lines = project.ensure_fresh_lines(LATE.0);
    assert_eq!(lines.len(), 1, "one outline, one staleness check:\n{}", project.log_text());
    assert!(
        lines[0].contains("outcome=ReindexedNoPriorRecord")
            && lines[0].contains(&format!("progress_sent={}", seen.len())),
        "the trace must record the reindex and how many notifications it sent ({}): {}",
        seen.len(),
        lines[0]
    );

    client.cancel().await.expect("failed to shut the client down");
}
