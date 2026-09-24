//! Acceptance tests for GM-394: `initialize` and `tools/list` must never
//! depend on the cold-start bulk walk's progress - not even while the walk is
//! holding the daemon's one SQLite mutex for a batch commit, which is exactly
//! the lock `mcp::mod::GMeshMcpServer::instructions` used to take
//! unconditionally before this task. That lock is what a batch's embedding
//! inference (an ONNX forward pass that can run for minutes on a big project)
//! held for its whole duration, so a client's MCP handshake - `initialize`
//! calls `get_info`, which called `instructions` - hung for as long as the
//! inference did. See `daemon::indexing_status`'s own "GM-394" doc section
//! for the fuller argument, and [`HOLD_LOCK_FILE_ENV`] for the test-only knob
//! this file uses to hold that lock open on purpose, deterministically,
//! without needing a real ONNX model on this machine (or a repository large
//! enough for a real walk to take minutes).
//!
//! # Why the daemon is spawned directly, not through the shim
//!
//! The bug this task fixes is itself a race - "the very first handshake right
//! after spawn succeeds only because bulk index has not taken the lock yet"
//! (the task's own bug report) - so a test that spawns the daemon *and*
//! measures the handshake in one connect (the way `serving_while_indexing.rs`
//! does for the *flag*-only case) can race the same way in reverse: the
//! client's `initialize` can land before the walk has reached the hold at
//! all, passing for a reason that has nothing to do with the fix. This file
//! splits the two: [`spawn_daemon_holding_the_lock`] starts the daemon
//! directly (the same `g-mesh daemon --project-root` the shim's own
//! `spawn_detached_daemon` uses), `common::trigger_activation` asks it to
//! walk (GM-395 slice 2: nothing is walked until a tool call asks), and
//! [`Project::wait_until_the_graph_holds`]
//! blocks until the lock is *provably* held (a row from the batch that just
//! ran `apply_diff` is visible, and `commit` only reaches the hold after
//! that). Only then does [`attach`] connect a client - to a daemon already
//! known to be inside the hold, which is what makes the timing below mean
//! what it claims.
//!
//! Two tests, and they do not carry equal weight:
//! - [`initialize_and_tools_list_answer_quickly_while_a_batch_commit_holds_the_lock`]
//!   is the control for item 1 and item 3: it fails against the pre-fix code
//!   (see "The control" below) and passes against this one, so it actually
//!   tells the two arms apart.
//! - [`a_tool_call_issued_while_the_lock_is_held_waits_and_then_answers_in_full`]
//!   is **not** a control for item 2, despite an earlier version of this
//!   file's doc comment claiming it was. Run against the pre-fix code
//!   directly, it also passes - not because item 2 was already true there
//!   (`still_indexing`'s pre-fix bounded grace wait absolutely does answer a
//!   slow call with `STILL_INDEXING`, and `serving_while_indexing.rs`'s
//!   `a_walk_that_outlasts_the_bootstrap_timeout_is_waited_out_rather_than_
//!   losing_the_client` demonstrates exactly that failure - see that file for
//!   the real item-2 control), but because [`attach`] in *this* test is
//!   itself exposed to item 1's bug: on pre-fix code `attach`'s own
//!   `initialize` blocks on the held lock until this file's 30-second safety
//!   valve releases it, by which point the walk has already finished, so the
//!   `find_definition` dispatched afterward finds `is_indexing() == false`
//!   already and answers instantly - a pass for a reason that has nothing to
//!   do with item 2. What this test *does* still verify, on either arm, is
//!   narrower but real: `still_indexing`'s wait never itself contends for
//!   `conn`'s mutex, so a tool call can be parked waiting for the walk at the
//!   exact moment something else holds that mutex without deadlocking - a
//!   property `serving_while_indexing.rs`'s tests cannot exercise, because
//!   none of them ever hold the real lock for the artificial delay they use.
//!   Kept as a regression guard for that property, not as evidence for the
//!   wait-vs-error contract change.
//!
//! # The control
//!
//! [`initialize_and_tools_list_answer_quickly_while_a_batch_commit_holds_the_lock`]
//! is meaningless on its own without having been shown to fail against the
//! pre-fix code - a passing test proves nothing about whether it can tell the
//! two arms apart. That control was run separately (a worktree of
//! `release-3.11.1` with only this file and a copy of `daemon::bulk_index`'s
//! [`HOLD_LOCK_FILE_ENV`] scaffolding added, `mcp::mod` left unfixed): it
//! failed its `HANDSHAKE_BUDGET` assertion, `initialize` having taken as long
//! as the lock was held for - the exact hang GM-394 reports, made
//! deterministic instead of a race. See the task's completion notes for the
//! command and its output.

use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::bulk_index::HOLD_LOCK_FILE_ENV;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One file, one symbol - enough for `find_definition` to have a real,
/// checkable answer once the walk finishes.
const FILES: [(&str, &str); 1] = [("src/index.ts", "export function connect(): number {\n  return 1;\n}\n")];

/// Generous next to the cost of a plain socket connect and one JSON-RPC round
/// trip - all `attach` does, since by the time it is called the daemon this
/// file connects to already exists and is already past its own startup - but
/// nowhere near the minutes a bulk-index batch's embedding inference could
/// hold the lock for on a real project, which is exactly the gap this file
/// exists to prove is now zero. Not the literal ~1s the task's acceptance
/// criterion states, to absorb ordinary CI scheduling noise; what matters is
/// the multiple of headroom below this bound against the lock hold, not the
/// literal number.
const HANDSHAKE_BUDGET: Duration = Duration::from_secs(3);

/// How long a test in this file waits for its own assertions before giving
/// up - a deadlock guard, not a timing assertion, comfortably past
/// [`HANDSHAKE_BUDGET`] so a handshake that (incorrectly) waited on the lock
/// fails these tests slowly rather than hanging them forever. The daemon's
/// own `hold_the_lock_open_for_tests` has its own 30s ceiling on top of this
/// as a second, independent backstop.
const LOCK_HOLD_BUDGET: Duration = Duration::from_secs(20);

struct Project {
    dir: tempfile::TempDir,
    /// The directly-spawned daemon `spawn_daemon_holding_the_lock` starts,
    /// once it has been called - `None` until then. Held here, rather than
    /// dropped the way a real detached daemon's `Child` is
    /// (`shim::spawn_detached_daemon`'s own doc comment), purely so `Drop`
    /// below can reap it explicitly: `Project::stop`'s pid-file-based kill
    /// already covers a daemon spawned through the shim, but nothing else in
    /// this file would otherwise wait on *this* `Child`, which is what
    /// `clippy::zombie_processes` rightly flags.
    daemon: Option<std::process::Child>,
}

impl Project {
    fn new() -> Self {
        let project =
            Self { dir: tempfile::tempdir().expect("failed to create a temp project root"), daemon: None };
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

    /// Blocks until the walk has committed a node by this name - which, with
    /// [`HOLD_LOCK_FILE_ENV`] set, is also the instant the lock this file is
    /// about is actually being held: `daemon::bulk_index::commit` calls
    /// `apply_diff` (what makes this row visible) and only then
    /// `hold_the_lock_open_for_tests`, both inside the same `Mutex::lock`
    /// guard - so a row appearing here means that guard is still alive and
    /// spinning on the hold file.
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
                common::kill_and_wait(pid);
            }
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Reaped first, and directly: `stop`'s pid-file-based kill below would
        // also find this same process (`daemon::run` writes its pid file
        // regardless of how it was spawned), but killing it by pid and then
        // separately `wait()`ing the `Child` this struct still holds would
        // race two different ways of ending the same process. Ending it here
        // makes `stop`'s own kill a no-op for this process specifically.
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        self.stop();
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// Starts the project's daemon directly - the same `g-mesh daemon
/// --project-root` invocation `shim::spawn_detached_daemon` uses, bypassing
/// the shim's own bootstrap entirely - with its bulk-index batch commit
/// holding `conn`'s lock until `hold_file` is deleted.
///
/// See this module's own "Why the daemon is spawned directly" doc section for
/// why going through the shim here would race the very thing this file needs
/// to hold still. The `Child` is stored on `project` rather than dropped -
/// unlike a real detached daemon's `Child` (`shim::spawn_detached_daemon`'s
/// own doc comment explains why *that* one is dropped without waiting) - so
/// `Project::drop` can reap it explicitly instead of leaving it for
/// `clippy::zombie_processes` to (rightly) flag.
fn spawn_daemon_holding_the_lock(project: &mut Project, hold_file: &Path) {
    let daemon = StdCommand::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .env(HOLD_LOCK_FILE_ENV, hold_file)
        // This file is about lock contention, not embeddings - pointing at a
        // nonexistent model directory keeps `EmbeddingModel::load` failing
        // its own `Path::exists()` check immediately, so no real (and
        // machine-dependent) model load competes with the timing assertions
        // below.
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon directly");
    project.daemon = Some(daemon);
}

/// Attaches a real shim to whatever daemon is already serving `project`.
///
/// A plain connect, with none of `serving_while_indexing.rs`'s own bootstrap-
/// timeout or walk-delay knobs: by the time every caller of this function
/// calls it, the daemon already exists and is confirmed to be inside the
/// held lock (`Project::wait_until_the_graph_holds`), so all this measures is
/// the shim finding an already-listening socket and completing one MCP
/// handshake over it.
async fn attach(project: &Project) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true).arg("mcp-shim").current_dir(&root).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");

    ().serve(transport).await.expect("the shim must reach the already-serving daemon")
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

async fn find_definition(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": name }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

/// GM-394 items 1 and 3: `initialize` (which carries `get_info`'s
/// instructions) and a follow-up `tools/list` both answer fast, and the
/// instructions say the index is still being built, while a bulk-index batch
/// commit is holding the daemon's single SQLite mutex open.
#[tokio::test]
async fn initialize_and_tools_list_answer_quickly_while_a_batch_commit_holds_the_lock() {
    let mut project = Project::new();
    let hold_file = project.root().join(".g-mesh-hold-the-lock");
    std::fs::write(&hold_file, b"").expect("failed to plant the lock-hold file");

    spawn_daemon_holding_the_lock(&mut project, &hold_file);
    // GM-395 slice 2: the daemon walks nothing until a tool call asks, so the
    // walk (and the hold inside it) has to be triggered first.
    common::trigger_activation(project.root());
    // Confirms the lock is genuinely held right now, not merely that
    // `IndexingStatus` reads as indexing - which never needed this fix at
    // all, see `daemon::indexing_status`'s own "GM-394" doc section for the
    // distinction this file's whole design turns on.
    project.wait_until_the_graph_holds("connect");

    let connect_started = Instant::now();
    let client = attach(&project).await;
    let connect_elapsed = connect_started.elapsed();
    assert!(
        connect_elapsed < HANDSHAKE_BUDGET,
        "initialize must not depend on the lock a bulk-index batch is holding: took {connect_elapsed:?}"
    );

    let info = client.peer_info().expect("server never reported its info");
    let instructions = info.instructions.clone().unwrap_or_default();
    // The transient fact (`instructions::cold_start`, D12 in
    // `docs/architecture/lazy-indexing.md` - `INDEXING_NOTE` before GM-395
    // slice 2b) and the steady-state paragraph every session gets
    // (`instructions::P4_GENERIC` and its siblings) both say something about
    // this - checked separately since GM-394 moved the "index is being
    // built" wording into the first and the "a tool call waits" wording into
    // the second (see `instructions::cold_start`'s own doc comment for why
    // they are not the same sentence).
    assert!(
        instructions.contains("Being built now"),
        "instructions must say indexing is in progress, right now, while it is: {instructions}"
    );
    assert!(
        instructions.contains("a tool call waits for the walk to finish"),
        "instructions must say what a tool call does about it: {instructions}"
    );

    let list_started = Instant::now();
    let listed = client.list_tools(None).await.expect("tools/list failed");
    let list_elapsed = list_started.elapsed();
    assert!(
        list_elapsed < HANDSHAKE_BUDGET,
        "tools/list must not depend on the lock a bulk-index batch is holding: took {list_elapsed:?}"
    );
    assert!(!listed.tools.is_empty(), "the tool surface must still be advertised while indexing");

    std::fs::remove_file(&hold_file).expect("failed to release the lock");
    common::wait_until_indexed(project.root());

    client.cancel().await.expect("failed to shut the client down");
}

/// A regression guard, not a control - see this module's own doc comment
/// ("Two tests, and they do not carry equal weight") for why this passes on
/// the pre-fix code too, and for nothing to do with the fix it might look
/// like it tests. What it actually pins: a tool call parked in
/// `still_indexing`'s wait while a batch commit holds `conn`'s mutex does not
/// deadlock against it, because that wait never touches the mutex at all -
/// and once released, the call answers in full rather than with an error.
/// `serving_while_indexing.rs`'s `a_walk_that_outlasts_the_bootstrap_timeout_
/// is_waited_out_rather_than_losing_the_client` is the real control for the
/// wait-vs-error behavior this test's name describes.
#[tokio::test]
async fn a_tool_call_issued_while_the_lock_is_held_waits_and_then_answers_in_full() {
    let mut project = Project::new();
    let hold_file = project.root().join(".g-mesh-hold-the-lock");
    std::fs::write(&hold_file, b"").expect("failed to plant the lock-hold file");

    spawn_daemon_holding_the_lock(&mut project, &hold_file);
    common::trigger_activation(project.root());
    project.wait_until_the_graph_holds("connect");
    let client = attach(&project).await;

    let call_started = Instant::now();
    let call = find_definition(&client, "connect");
    let release = async {
        // Long enough that the call is demonstrably still waiting when this
        // fires, not a race against how fast the connect above happened to
        // run.
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::remove_file(&hold_file).expect("failed to release the lock");
    };
    let (result, ()) = tokio::join!(call, release);
    let call_elapsed = call_started.elapsed();

    assert_ne!(
        result.is_error,
        Some(true),
        "a tool call issued while the index is being built must wait, never refuse: {}",
        text(&result)
    );
    let node = body(&result);
    assert_eq!(node["name"], "connect");
    assert_eq!(node["filePath"], "src/index.ts");

    assert!(
        call_elapsed >= Duration::from_millis(150),
        "a call served this fast looks like it did not actually wait for the walk to finish: {call_elapsed:?}"
    );
    assert!(
        call_elapsed < LOCK_HOLD_BUDGET,
        "the call should have been released promptly once the walk finished: {call_elapsed:?}"
    );

    client.cancel().await.expect("failed to shut the client down");
}
