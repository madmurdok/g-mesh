//! Acceptance test for task 107: a tool call that lands close enough to the
//! end of the cold-start bulk walk gets the real answer instead of a refusal
//! it would only have had to retry a moment later.
//!
//! Task 105 (`serving_while_indexing.rs`) made every call answered while
//! `IndexingStatus::is_indexing()` reads true come back `STILL_INDEXING`,
//! unconditionally. That was correct for the case it targeted - a walk with
//! real minutes left to run - but it also meant a small project's first call
//! paid for a whole retry purely because it asked a few milliseconds before
//! `mark_ready` rather than a few milliseconds after: pure overhead, since
//! the walk was always going to finish almost immediately anyway.
//! `mcp::GMeshMcpServer::still_indexing` now gives such a call up to
//! `mcp::INDEXING_GRACE_WINDOW` to let the walk finish before it commits to
//! the refusal - and this file proves both halves of that: a walk that
//! finishes inside the window is served for real, and a walk that does not
//! still gets `STILL_INDEXING` back quickly rather than hanging around for
//! its own sake.
//!
//! # How the timing is controlled without a slow real fixture
//!
//! Same tool as task 105's suite: `bulk_index::WALK_DELAY_ENV` holds a
//! finished walk open for an explicit number of milliseconds before
//! `IndexingStatus::mark_ready` is called, which turns "the walk is about to
//! finish" into a knob instead of a race against however fast this machine
//! happens to parse two tiny files.
//!
//! What is new here is *when* the tool call is dispatched relative to that
//! knob. Task 105's suite calls immediately after connecting, which is fine
//! when the only thing being asserted is "still running" for a walk held
//! open for seconds. This file needs finer control than that - a call has to
//! land with a known, bounded amount of time left before `mark_ready`, not
//! merely "sometime before it". So each test here first waits for the walk's
//! last commit to land in the database (`wait_until_the_graph_holds`, lifted
//! from task 105's suite) - the instant `bulk_index::run` starts counting
//! down `WALK_DELAY_ENV` - and only then sleeps a further, explicitly chosen
//! offset before dispatching. Because that offset is measured *after* the
//! commit is observed, it can only be detected late, never early - so the
//! remaining time before `mark_ready` at the moment of the call is bounded
//! above by construction (offset subtracted from the total delay), whatever
//! this machine's scheduling noise happens to be that run.

use std::path::Path;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::bulk_index::WALK_DELAY_ENV;
use g_mesh::mcp::INDEXING_GRACE_WINDOW;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Generous next to the shim's own bootstrap budget: nothing in this file
/// depends on the socket bind timing task 105's suite exercises, so this
/// only needs to be long enough that it is never itself the bottleneck.
const BOOTSTRAP_BUDGET: Duration = Duration::from_millis(2_000);

/// Total time the walk is held open past its last commit, for the case where
/// it must finish *inside* the grace window. Comfortably longer than
/// [`INDEXING_GRACE_WINDOW`] on its own - what keeps the call inside the
/// window is not this number but [`SLEEP_BEFORE_THE_CALL`], which eats most
/// of it before the call is ever dispatched.
const WALK_HELD_OPEN_WITHIN_GRACE: Duration = Duration::from_millis(400);

/// How long this test sleeps after observing the walk's last commit and
/// before dispatching its tool call. Subtracted from
/// [`WALK_HELD_OPEN_WITHIN_GRACE`], what is left over
/// (`WALK_HELD_OPEN_WITHIN_GRACE` - `SLEEP_BEFORE_THE_CALL` = 90ms) is the
/// most time that can possibly remain before `mark_ready` when the call is
/// dispatched - comfortably inside [`INDEXING_GRACE_WINDOW`] (150ms), with
/// 60ms of headroom against the timeout and 90ms of headroom against the
/// "did this even wait" floor the test asserts on the other side.
const SLEEP_BEFORE_THE_CALL: Duration = Duration::from_millis(310);

/// Held open far longer than [`INDEXING_GRACE_WINDOW`] could ever absorb, so
/// the call made immediately after connecting is guaranteed to still be
/// refused - the huge-project case task 105 exists for, which this task must
/// not reintroduce a smaller version of.
const WALK_HELD_OPEN_PAST_GRACE: Duration = Duration::from_secs(2);

/// One file, one symbol - enough for `find_definition` to have a real,
/// checkable answer, and small enough that the walk's own natural duration
/// contributes nothing next to the delays above.
const FILES: [(&str, &str); 1] = [("src/index.ts", "export function connect(): number {\n  return 1;\n}\n")];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (rel, contents) in FILES {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Blocks until the walk has committed a node by this name - the instant
    /// `WALK_DELAY_ENV` starts counting down, and the synchronization point
    /// every "land the call at a known offset" test in this file is built on.
    /// See `serving_while_indexing.rs`'s copy of this same helper for why
    /// reading straight out of the index (rather than any daemon-side signal)
    /// is what makes a "still indexing, with the answer already sitting
    /// there" refusal provably about the flag and not an empty table.
    fn wait_until_the_graph_holds(&self, name: &str) {
        let db = project_dir(self.root()).expect("failed to resolve the state directory").join("index.db");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let committed = Connection::open(&db)
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM nodes WHERE name = ?1 AND kind = 'Function'",
                        [name],
                        |row| row.get::<_, i64>(0),
                    )
                    .ok()
                })
                .unwrap_or(0);
            if committed > 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the walk never committed a node named `{name}`");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stop(&self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            if let Some(pid) = daemon::read_pid_file(&path) {
                let _ = StdCommand::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        if let Ok(socket) = daemon::socket_path(self.root()) {
            let _ = std::fs::remove_file(socket);
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.stop();
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A client over a real shim, with the walk held open for `hold_the_walk_open`
/// past its last commit.
async fn connect_with(
    project: &Project,
    hold_the_walk_open: Duration,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env("G_MESH_BOOTSTRAP_TIMEOUT_MS", BOOTSTRAP_BUDGET.as_millis().to_string())
            .env(WALK_DELAY_ENV, hold_the_walk_open.as_millis().to_string())
            // This suite is about grace-window/bootstrap timing, not
            // embeddings - the fixture's one function has a signature, which
            // is embeddable text, so without this the walk would pay a real
            // (multi-second, machine-dependent) model load on whichever
            // machine happens to have the weights fetched, blowing through
            // every millisecond-scale budget below. Pointing at a directory
            // with no model files makes `EmbeddingModel::load` fail its
            // `Path::exists()` check immediately - see its doc comment - so
            // embedding is skipped exactly like it would be for any
            // contributor who has not run `fetch-embedding-model.sh`.
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim");

    ().serve(transport).await.expect("the shim must reach the daemon while its cold-start walk is still running")
}

async fn find_definition(client: &rmcp::service::RunningService<rmcp::RoleClient, ()>, name: &str) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("find_definition").with_arguments(
                json!({ "symbol_name": name }).as_object().cloned().expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {}", text(result));
    serde_json::from_str(&text(result)).expect("tool result is not JSON")
}

/// The regression task 107 fixes: a call that lands with less than
/// `INDEXING_GRACE_WINDOW` left before the walk finishes is served the real
/// answer instead of a refusal it would only have had to retry a moment
/// later.
///
/// Timing, not just the response shape, is the point - a lucky race would
/// also produce a real answer, so this asserts both ends: the call took
/// meaningfully longer than an already-indexed project's instant response
/// (proving something was actually waited on, not skipped past), and
/// meaningfully less than the grace window itself (proving the wait ended
/// because the walk finished, not because the window ran out and something
/// downstream happened to still succeed).
#[tokio::test]
async fn a_walk_that_finishes_inside_the_grace_window_is_served_the_real_answer() {
    let project = Project::new();
    let client = connect_with(&project, WALK_HELD_OPEN_WITHIN_GRACE).await;

    project.wait_until_the_graph_holds("connect");
    tokio::time::sleep(SLEEP_BEFORE_THE_CALL).await;

    let call_started = Instant::now();
    let result = find_definition(&client, "connect").await;
    let elapsed = call_started.elapsed();

    assert_ne!(
        result.is_error,
        Some(true),
        "a walk finishing inside the grace window must be served for real, not refused: {}",
        text(&result)
    );
    assert_eq!(body(&result)["name"], "connect");

    assert!(
        elapsed >= Duration::from_millis(25),
        "a call served this fast looks like it skipped the wait rather than being closed by it: {elapsed:?}"
    );
    // Back to the tight bound this test was written with, now that task 164
    // has landed. It was dropped for one release because the daemon's own
    // post-walk semantic pass spawns the bundled plugin around the moment
    // this call resumes, and `PluginRegistry::get_or_spawn` used to hold the
    // registry's one supervisors lock for that whole spawn - which
    // `PluginRegistry::has_pending`, this call's own preamble via
    // `replay_queued_changes`, took just to read the map. This entirely
    // structural query therefore waited out a Node process launch plus a
    // handshake (measured 270-494ms on this machine), and no fixed bound was
    // honest about a number that is really "however long spawning a plugin
    // takes here".
    //
    // `get_or_spawn` no longer holds that lock across the spawn (see
    // `daemon::registry`'s doc comment), so what this measures is the
    // grace-wait mechanism again - the same thing it measured before there
    // was a registry at all - and a bound of one grace window is meaningful
    // rather than machine-dependent.
    assert!(
        elapsed < INDEXING_GRACE_WINDOW,
        "a call that waited the full grace window and beyond did not get closed by the walk finishing: {elapsed:?}"
    );

    client.cancel().await.expect("failed to shut the client down");
}

/// Task 105's headline case, still true after task 107: a walk with real time
/// left on it - far more than the grace window could ever absorb - still
/// fails fast rather than making its caller wait around for it.
#[tokio::test]
async fn a_walk_that_outlasts_the_grace_window_still_answers_still_indexing_promptly() {
    let project = Project::new();
    let client = connect_with(&project, WALK_HELD_OPEN_PAST_GRACE).await;

    let call_started = Instant::now();
    let result = find_definition(&client, "connect").await;
    let elapsed = call_started.elapsed();

    assert_eq!(result.is_error, Some(true), "a half-built graph must not be answered from");
    assert_eq!(text(&result), g_mesh::mcp::STILL_INDEXING);

    // A generous multiple of the grace window rather than the window itself:
    // this is a deadline against "hung around for something closer to the
    // full 2-second walk", not a tight timing assertion the way the test
    // above is - the huge-project acceptance criterion is "fast", not
    // "exactly as fast as the window".
    assert!(
        elapsed < INDEXING_GRACE_WINDOW * 3,
        "a walk with seconds left to run must still fail fast, not after some much longer wait: {elapsed:?}"
    );

    client.cancel().await.expect("failed to shut the client down");
}
