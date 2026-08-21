//! Acceptance test for task 105: a cold-start bulk walk that outlasts the
//! shim's bootstrap timeout must cost its caller a retry, not its whole tool
//! surface.
//!
//! The failure this rules out was made reachable on a schedule by tasks 96 and
//! 99 together: a bumped `CURRENT_INDEXER_VERSION` wipes the index, a shim
//! retires the daemon an upgrade left behind, and the replacement therefore
//! owes the project a full walk - so the first MCP call after any real upgrade
//! lands on a daemon that is cold-starting. While `daemon::run` bound its
//! socket only after that walk, a project big enough for the walk to exceed
//! `shim::BOOTSTRAP_TIMEOUT` left the shim erroring out on a socket that was
//! never going to appear in time, and the MCP client with no g-mesh tools at
//! all.
//!
//! # Why this does not take ten real seconds
//!
//! The criterion is "the walk outlasts the shim's bootstrap timeout", which is
//! a *relation* between two durations, not a requirement that either be ten
//! seconds. So both are set explicitly, from opposite ends: the walk is held
//! open past its natural length (`bulk_index::WALK_DELAY_ENV`) and the shim's
//! budget is shortened well below it (`shim::BOOTSTRAP_TIMEOUT_ENV`). The
//! relation under test is genuine and the suite pays a few seconds rather than
//! a few tens of them on every commit.
//!
//! Everything else is real: the real binary, a real shim spawning a real
//! detached daemon, a real plugin walk, and a real `rmcp` client asking real
//! questions over the socket.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::Path;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Long enough that a client connecting, initializing and asking one question
/// is comfortably inside the window, and short enough that the suite does not
/// notice. Nothing is asserted about this number itself - only that the walk
/// it belongs to is still running when the first call is made.
const WALK_HELD_OPEN: Duration = Duration::from_millis(3_000);

/// Deliberately a fraction of [`WALK_HELD_OPEN`]: this is the budget the shim
/// gives the daemon to *bind*, and the whole point of the change under test is
/// that binding no longer waits for the walk. A shim that still had to wait
/// for the walk would fail here rather than time out somewhere later, which is
/// what makes this the acceptance criterion and not a detail.
const SHORT_BOOTSTRAP_BUDGET: Duration = Duration::from_millis(1_000);

/// One file importing another - enough of a graph for `find_definition` to
/// have a real, checkable answer, and small enough that the walk's natural
/// duration contributes nothing to this test's wall clock.
const FILES: [(&str, &str); 2] = [
    (
        "src/index.ts",
        r#"import { connect } from "./db/connection.js";

export function start(): number {
  return connect();
}
"#,
    ),
    ("src/db/connection.ts", "export function connect(): number {\n  return 1;\n}\n"),
];

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

    /// Blocks until the walk has committed a node by this name, read straight
    /// out of the index the daemon is writing (WAL, so a second process may
    /// read it mid-transaction).
    ///
    /// This is what turns "the daemon refused" into "the daemon refused
    /// *while holding the answer*": `WALK_DELAY_ENV` keeps the walk running
    /// for seconds after its last commit, so a question asked once this
    /// returns could have been answered correctly - and is deliberately not.
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
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the daemon and its plugin the way a reboot would, leaving the
    /// index on disk exactly as it is - the state the next start has to
    /// judge. Waits for each process to actually die (see
    /// `common::kill_and_wait`) so a restart spawned right after this returns
    /// cannot race a not-yet-dead process for `daemon.lock`.
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
        self.stop();
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A client over a real shim, with the walk held open and the bootstrap budget
/// shortened. Both knobs travel through the shim's own environment into the
/// daemon it spawns, which inherits it.
async fn connect_with_a_slow_walk(project: &Project) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    connect_with(project, Some(WALK_HELD_OPEN)).await
}

async fn connect_with(
    project: &Project,
    hold_the_walk_open: Option<Duration>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env("G_MESH_BOOTSTRAP_TIMEOUT_MS", SHORT_BOOTSTRAP_BUDGET.as_millis().to_string());
        if let Some(delay) = hold_the_walk_open {
            cmd.env("G_MESH_BULK_INDEX_DELAY_MS", delay.as_millis().to_string());
        }
    }))
    .expect("failed to spawn the shim");

    // The acceptance criterion itself: the shim gets a connection within a
    // budget far shorter than the walk. Before this change it could only have
    // failed here - the socket did not exist until the walk was over.
    ().serve(transport)
        .await
        .expect("the shim must reach the daemon while its cold-start walk is still running")
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

/// The headline case: a walk that outlasts the shim's bootstrap budget by
/// three times over, and the client is served throughout - first the honest
/// refusal, then, on the very same session, the real answer.
#[tokio::test]
async fn a_walk_that_outlasts_the_bootstrap_timeout_is_answered_with_still_indexing_rather_than_losing_the_client(
) {
    let project = Project::new();
    let started = Instant::now();
    let client = connect_with_a_slow_walk(&project).await;

    let during_the_walk = find_definition(&client, "connect").await;
    assert!(
        started.elapsed() < WALK_HELD_OPEN,
        "this call has to land while the walk is still running, and it took {:?}",
        started.elapsed()
    );

    // A tool-level error, so no caller can mistake it for a complete answer -
    // and one that names indexing, so no caller has to guess what to do next.
    assert_eq!(during_the_walk.is_error, Some(true), "a half-built graph must not be answered from");
    assert_eq!(
        text(&during_the_walk),
        g_mesh::mcp::STILL_INDEXING,
        "the wording an agent acts on is part of the contract"
    );
    assert!(
        text(&during_the_walk).contains("still being built"),
        "the reason has to be indexing, not a bare failure: {}",
        text(&during_the_walk)
    );

    // The refusal above could still, on its own, have been an accident of an
    // empty table: this early in the walk there genuinely is no `connect` node
    // yet, and with the guard removed the call comes back "no symbol named
    // 'connect' found" - a confident, well-formed, wrong answer, which is the
    // whole failure being prevented. So ask again once the node *is*
    // committed. Same refusal, with the answer sitting in the database: it is
    // the flag doing the work, not the emptiness.
    project.wait_until_the_graph_holds("connect");
    let with_the_answer_committed = find_definition(&client, "connect").await;
    assert_eq!(
        with_the_answer_committed.is_error,
        Some(true),
        "a walk that has committed a node has still not committed the graph around it"
    );
    assert_eq!(text(&with_the_answer_committed), g_mesh::mcp::STILL_INDEXING);

    wait_until_indexed(project.root());

    // Same client, same session, nothing reconnected or re-initialized: the
    // status is consulted per call, so the next one simply works.
    let after_the_walk = find_definition(&client, "connect").await;
    let node = body(&after_the_walk);
    assert_eq!(node["filePath"], "src/db/connection.ts");
    assert_eq!(node["name"], "connect");

    client.cancel().await.expect("failed to shut the client down");
}

/// The control that keeps the test above about indexing rather than about
/// `find_definition`: with no walk held open, the same first call on the same
/// fixture is answered outright.
#[tokio::test]
async fn a_first_call_is_answered_normally_when_nothing_holds_the_walk_open() {
    let project = Project::new();
    let client = connect_with(&project, None).await;

    wait_until_indexed(project.root());
    let node = body(&find_definition(&client, "connect").await);
    assert_eq!(node["filePath"], "src/db/connection.ts");

    client.cancel().await.expect("failed to shut the client down");
}

/// The fast path tasks 96 and 99 left intact, and the one this change must not
/// touch: a restart against an index that was already fully walked has no walk
/// to be in the middle of, so its socket is bound immediately and its very
/// first call is a real answer.
///
/// Proven by the delay knob rather than by timing: the restarted daemon is
/// started with the walk held open for far longer than this test's own
/// patience, so if it walked at all - if the flag were set from anything other
/// than the recorded `bulkIndexedAt` - the call below could only come back as
/// "still indexing".
#[tokio::test]
async fn a_restart_against_an_already_walked_project_never_reports_itself_as_still_indexing() {
    let project = Project::new();

    let first = connect_with(&project, None).await;
    wait_until_indexed(project.root());
    body(&find_definition(&first, "connect").await);
    first.cancel().await.expect("failed to shut the first client down");
    project.stop();

    // Held open for ten times the window the first phase needed, so that a
    // walk this daemon must not do could not possibly go unnoticed.
    let restarted = connect_with(&project, Some(WALK_HELD_OPEN * 10)).await;

    let first_call = find_definition(&restarted, "connect").await;
    assert_ne!(
        first_call.is_error,
        Some(true),
        "a project that owed no walk must answer its first call outright: {}",
        text(&first_call)
    );
    assert_eq!(body(&first_call)["filePath"], "src/db/connection.ts");

    restarted.cancel().await.expect("failed to shut the restarted client down");
}
