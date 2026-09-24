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
//! # GM-301: the two durations are measured, not guessed
//!
//! "Well below it" used to be two fixed constants, 1s and 3s, picked once on
//! whoever's laptop was idle at the time. That is a bet that spawning a
//! process and getting the OS to schedule it to bind a socket always costs
//! about the same - which a machine doing other real work at the same time
//! does not honor: three observed failures here were `ConnectionClosed` on
//! the shim's own connect, at a load of 45-60, with a control run on the
//! unmodified base commit failing the same binary *worse*. The daemon was
//! never slow to bind; the process tree just was not getting scheduled inside
//! a 1-second window that had nothing to do with how big the walk was.
//!
//! [`calibrate`] replaces the guess with a real measurement taken on this
//! machine, right before each run that needs it, and [`bootstrap_budget`] and
//! [`walk_held_open`] derive generous multiples of it. The *relation* the
//! acceptance criterion depends on - the bootstrap budget staying a fraction
//! of the walk-held-open window - is preserved by construction rather than by
//! two numbers that happened to still be in the right ratio: whatever this
//! machine's connect-and-ask costs right now, the walk stays held open at
//! least three times that long past it.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

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
async fn connect_with(
    project: &Project,
    hold_the_walk_open: Option<Duration>,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    connect_with_budget(project, hold_the_walk_open, bootstrap_budget().await).await
}

/// [`connect_with`], but with the shim's own bootstrap budget passed in
/// explicitly rather than derived from [`bootstrap_budget`] - the escape
/// hatch [`calibrate`] needs so measuring a real bind does not depend on the
/// number that measurement itself produces.
async fn connect_with_budget(
    project: &Project,
    hold_the_walk_open: Option<Duration>,
    bootstrap_budget: Duration,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let root = project.root().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env("G_MESH_BOOTSTRAP_TIMEOUT_MS", bootstrap_budget.as_millis().to_string());
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

/// GM-301: measures how long a completely ordinary connect-and-ask takes on
/// this machine right now - spawn the shim, let it bootstrap a real daemon
/// with nothing artificially delayed, wait for the (tiny, unforced) walk to
/// finish, and ask one question - so [`bootstrap_budget`] and
/// [`walk_held_open`] can be generous multiples of a real number instead of a
/// guess written on whoever's laptop was idle at the time.
///
/// Measured once per test binary and cached: every test in this file runs in
/// the same process against the same machine, so a second measurement would
/// just pay the cost again for the same answer. If two tests race this before
/// either has cached anything, both measure something real off the same
/// machine; whichever `set` wins is fine, and the loser still returns its own
/// real measurement rather than a stale or fabricated one.
async fn calibrate() -> (Duration, Duration) {
    static CACHED: OnceLock<(Duration, Duration)> = OnceLock::new();
    if let Some(cached) = CACHED.get() {
        return *cached;
    }
    let project = Project::new();
    let bind_started = Instant::now();
    // A large, fixed budget of its own - not `bootstrap_budget()`, which is
    // what this measurement calibrates. Using it here would be circular: a
    // slow-but-real bind has to be measured faithfully rather than cut off by
    // the very number that is about to be derived from it.
    let client = connect_with_budget(&project, None, Duration::from_secs(60)).await;
    let bind_time = bind_started.elapsed();
    wait_until_indexed(project.root());
    let call_started = Instant::now();
    let _ = find_definition(&client, "connect").await;
    let call_time = call_started.elapsed();
    client.cancel().await.expect("failed to shut the calibration client down");
    let measured = (bind_time, call_time);
    let _ = CACHED.set(measured);
    measured
}

/// The budget the shim gives a freshly spawned daemon to bind its socket - a
/// generous multiple of an actually-measured bind (see [`calibrate`]),
/// floored near the old fixed constant so an idle run keeps roughly its old,
/// fast shape, and capped so a genuinely wedged machine still fails within a
/// bounded time rather than hanging the suite.
async fn bootstrap_budget() -> Duration {
    let (bind_time, _) = calibrate().await;
    (bind_time * 8).clamp(Duration::from_millis(1_000), Duration::from_secs(30))
}

/// How long a test holds a walk open past its last commit. At least three
/// times [`bootstrap_budget`] - the relation `a_walk_that_outlasts_the_
/// bootstrap_timeout_is_answered_with_still_indexing_rather_than_losing_the_
/// client` depends on, see this module's header - and comfortably past a full
/// calibrated connect-and-ask, so the timed call in that test has the same
/// margin the calibration run itself needed.
async fn walk_held_open() -> Duration {
    let (bind_time, call_time) = calibrate().await;
    let budget = bootstrap_budget().await;
    ((bind_time + call_time) * 6).max(budget * 3).clamp(Duration::from_millis(3_000), Duration::from_secs(90))
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
/// three times over. The client is never lost (task 105's own criterion:
/// connecting must not itself wait on the walk), and a tool call issued while
/// the walk is still running waits for it rather than being refused - GM-394's
/// owner decision, which replaced task 105/107's "answer `STILL_INDEXING`,
/// with at most a short grace wait" with "never answer a tool call 'not
/// ready' or partially; wait however long the walk actually takes and serve
/// the real thing".
#[tokio::test]
async fn a_walk_that_outlasts_the_bootstrap_timeout_is_waited_out_rather_than_losing_the_client() {
    let project = Project::new();
    // Warms the calibration cache (see `calibrate`) before the clock starts -
    // otherwise the one-off cost of measuring this machine's own speed would
    // count against the very budget it is calibrating, on whichever test
    // happens to run first.
    let hold_open = walk_held_open().await;

    let connect_started = Instant::now();
    let client = connect_with(&project, Some(hold_open)).await;
    let connect_elapsed = connect_started.elapsed();
    // Task 105's own criterion, unchanged by GM-394: reaching the daemon at
    // all must not wait on the walk - the socket is bound long before the
    // walk even starts. Since GM-395 slice 2 the walk does not start at all
    // until a tool call asks, so this can only hold more easily than before.
    assert!(
        connect_elapsed < hold_open,
        "connecting must not itself wait on the walk, and it took {connect_elapsed:?} against a hold \
         of {hold_open:?}"
    );

    // GM-395 slice 2: the daemon walks nothing until a tool call asks, so
    // the walk is started here, by a separate throwaway call, and the timed
    // call below lands on a walk already in progress - the shape this test
    // was written for.
    common::trigger_activation(project.root());

    // Dispatched while the walk is still running (by construction: `hold_open`
    // outlasts the bootstrap budget several times over) - GM-394's own
    // criterion is that this call waits for the walk rather than coming back
    // early or with an error.
    let call_started = Instant::now();
    let result = find_definition(&client, "connect").await;
    let call_elapsed = call_started.elapsed();

    assert_ne!(
        result.is_error,
        Some(true),
        "a tool call issued during indexing must wait and then answer in full, never refuse: {}",
        text(&result)
    );
    let node = body(&result);
    assert_eq!(node["filePath"], "src/db/connection.ts");
    assert_eq!(node["name"], "connect");

    // Timing, not just the response shape, is the point: a call answered this
    // fast could only mean the wait was skipped, since the walk was held open
    // for `hold_open` on purpose. Some slack under `hold_open` itself absorbs
    // ordinary scheduling noise without letting a skipped wait pass unnoticed.
    assert!(
        call_elapsed >= hold_open.mul_f64(0.8),
        "a call served this fast looks like it did not actually wait for the walk: {call_elapsed:?} \
         against a hold of {hold_open:?}"
    );

    // Same client, same session, nothing reconnected or re-initialized: the
    // status is consulted per call, so the next one simply works, same as it
    // always has once a project is indexed.
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
/// than the recorded `bulkIndexedAt` - the call below (which now waits
/// unconditionally rather than failing fast, see GM-394) could only hang
/// until that artificially-extended walk finally finished, which this test's
/// own patience does not survive.
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
    let restarted = connect_with(&project, Some(walk_held_open().await * 10)).await;

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
