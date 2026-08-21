//! Acceptance test for task 99: a daemon that was already running when a
//! newer g-mesh was installed must not go on answering out of the old build.
//!
//! Task 96 taught the index to invalidate itself, and
//! `stale_index_invalidation.rs` proves it does - but only across a restart
//! it stages by killing the daemon between phases. That is the one thing a
//! real upgrade does *not* do. Every daemon in the wild is long-lived, the
//! shim connects to whatever incumbent is listening, and so the invalidation
//! that task 96 built simply never ran: three separate investigations in one
//! day chased "bugs" that were only an old process serving an old index, one
//! of which became a high-priority ticket before being cancelled.
//!
//! So the difference between this file and that one is exactly one thing:
//! **nothing here kills the daemon between phases**. Each test starts a
//! daemon, doctors what a newer build would disagree with while that daemon
//! is still running and still holding the index open, and then asks who
//! answers the next call.
//!
//! Simulating "a newer build" without producing a second compiled artifact is
//! done the same way task 96's test simulates an older index: by rewriting
//! what the running daemon has recorded about itself. Backdating its
//! published build stamp is indistinguishable, to every consumer, from the
//! real case of the executable having been rebuilt since it started.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use g_mesh::daemon::{self, build_stamp};
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

/// A day, in the milliseconds the build stamp records mtimes in - far more
/// than enough to be unambiguously "an older build" and immune to whatever
/// resolution the filesystem under the test happens to have.
const A_DAY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// One file importing another: the smallest graph whose `Incoming` walk is
/// non-empty, and the exact shape that came back empty off a stale index.
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

    fn index_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }

    fn build_stamp_path(&self) -> PathBuf {
        daemon::build_stamp_path(self.root()).expect("failed to resolve the build stamp path")
    }

    /// The pid of the daemon currently serving this project - the answer to
    /// "which process answered that call?", and the thing every assertion
    /// here ultimately turns on.
    fn daemon_pid(&self) -> u32 {
        let path = daemon::pid_path(self.root()).expect("failed to resolve the pid file path");
        daemon::read_pid_file(&path).unwrap_or_else(|| panic!("no daemon pid recorded at {}", path.display()))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
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
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Runs one `get_dependencies` call through a freshly spawned shim and lets
/// go of it again - deliberately *without* stopping the daemon behind it,
/// which is the whole subject of this file.
async fn importers_of(project: &Project, file_path: &str) -> Vec<String> {
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(project.root()).env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    // Waited for *after* the connection is up, which is what makes it correct
    // across the replacements this file stages: the incoming daemon wipes the
    // index - completion marker and all - before it binds, so a marker still
    // readable here could only be the new walk's own.
    wait_until_indexed(project.root());

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_dependencies").with_arguments(
                json!({ "file_path": file_path, "direction": "Incoming" })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call failed");
    let walk = body(&result);

    client.cancel().await.expect("failed to shut the client down");

    let mut paths: Vec<String> = walk["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .filter_map(|row| row["filePath"].as_str().map(str::to_string))
        .collect();
    paths.sort();
    paths
}

/// Leaves behind what a previous generation of the indexer would have: the
/// files and their symbols are all still there, so every symptom is "this
/// query has no answer" rather than "this file is unknown".
///
/// Written straight into the database the running daemon has open, which
/// SQLite is perfectly happy with and which is what makes the doctored state
/// something the *incumbent* is serving rather than something staged for a
/// restart that has already been arranged.
fn make_the_index_look_like_an_older_generations_work(index: &Path) {
    let conn = Connection::open(index).expect("failed to open the project index");
    let removed =
        conn.execute("DELETE FROM edges WHERE kind = 'IMPORTS'", []).expect("failed to strip edges");
    assert!(removed > 0, "the fixture must have had import edges to strip");
    let restamped = conn
        .execute("UPDATE meta SET indexer_version = '0' WHERE id = 1", [])
        .expect("failed to restamp the index");
    assert_eq!(restamped, 1, "the index must carry exactly one meta row to restamp");
}

/// Rewrites the running daemon's published stamp to name an executable a day
/// older than the one the test binary spawns shims from - which is what "the
/// binary has been rebuilt since this daemon started" looks like from the
/// outside.
fn make_the_daemon_look_like_it_started_from_an_older_build(project: &Project) {
    let path = project.build_stamp_path();
    let mut stamp = build_stamp::read(&path)
        .unwrap_or_else(|| panic!("a running daemon must publish a build stamp at {}", path.display()));
    stamp.exe_mtime_millis -= A_DAY_MILLIS;
    build_stamp::write(&path, &stamp).expect("failed to backdate the build stamp");
}

/// The headline case, end to end: an upgrade lands while a daemon is up, and
/// the very next MCP call is answered by the new build off a rebuilt index -
/// with no restart, no `g-mesh stop`, and nothing else intervening.
#[tokio::test]
async fn a_daemon_started_from_an_older_build_is_replaced_before_it_answers_again() {
    let project = Project::new();

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "the cold-start index must answer this before anything is done to it"
    );
    let outdated_daemon = project.daemon_pid();

    make_the_index_look_like_an_older_generations_work(&project.index_path());
    make_the_daemon_look_like_it_started_from_an_older_build(&project);

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "a daemon left behind by an upgrade must not answer off the index it was holding"
    );
    assert_ne!(
        project.daemon_pid(),
        outdated_daemon,
        "the answer has to come from a daemon this build started, not the old process"
    );
}

/// The population this ships into: every daemon alive right now was started
/// by a build that had never heard of a build stamp, so it publishes none.
/// Nothing on record is treated as evidence of currency - it is treated as
/// what it is, a process from before the check existed.
#[tokio::test]
async fn a_daemon_that_published_no_build_stamp_at_all_is_replaced_too() {
    let project = Project::new();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    let unstamped_daemon = project.daemon_pid();

    make_the_index_look_like_an_older_generations_work(&project.index_path());
    std::fs::remove_file(project.build_stamp_path()).expect("failed to unpublish the build stamp");

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "a daemon that cannot vouch for its build must not be taken at its word"
    );
    assert_ne!(project.daemon_pid(), unstamped_daemon, "the old process must have been retired");
}

/// The control that makes the two tests above about the build stamp and
/// nothing else. Same doctored index, same second call, same everything -
/// except that the incumbent is still this build, so it is left alone and
/// goes on serving the graph it has, wrong answer and all. If this ever
/// starts returning the importer, those tests have stopped proving anything:
/// something other than the staleness check is restarting the daemon.
#[tokio::test]
async fn a_daemon_started_from_this_build_is_left_alone_and_keeps_answering() {
    let project = Project::new();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    let incumbent = project.daemon_pid();

    make_the_index_look_like_an_older_generations_work(&project.index_path());

    assert!(
        importers_of(&project, "src/db/connection.ts").await.is_empty(),
        "a current daemon is not restarted, so the doctored graph is what it still has to serve"
    );
    assert_eq!(project.daemon_pid(), incumbent, "a current daemon must never be retired");
}

/// The failure mode the retirement had to be designed around: two MCP client
/// sessions starting at the same moment both notice the same outdated daemon,
/// and the second one kills the *replacement* the first had just started -
/// leaving the first client proxying to a corpse.
///
/// The bootstrap lock is what rules that out, because retiring happens under
/// it and the verdict is re-taken after it is acquired. That is only worth
/// asserting against real concurrent processes, which is what this drives:
/// four shims spawned with nothing synchronizing them, all racing to replace
/// the same incumbent. Four rather than two because a lost race is a race
/// won by chance rather than by design - with the serialization deliberately
/// removed, two racers catch it about a third of the time and four almost
/// always do, and a regression test that only sometimes notices is not one.
#[tokio::test]
async fn shims_racing_to_replace_one_outdated_daemon_produce_exactly_one_replacement() {
    let project = Project::new();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    let outdated_daemon = project.daemon_pid();

    make_the_index_look_like_an_older_generations_work(&project.index_path());
    make_the_daemon_look_like_it_started_from_an_older_build(&project);

    // Every future spawns its shim before any of them can finish, so the race
    // is between four independent processes rather than between four turns of
    // this test's own control flow.
    let racers = tokio::join!(
        importers_of(&project, "src/db/connection.ts"),
        importers_of(&project, "src/db/connection.ts"),
        importers_of(&project, "src/db/connection.ts"),
        importers_of(&project, "src/db/connection.ts")
    );
    for (position, served) in [racers.0, racers.1, racers.2, racers.3].into_iter().enumerate() {
        assert_eq!(
            served,
            vec!["src/index.ts".to_string()],
            "racer {position} must have been served by whichever daemon won"
        );
    }

    let replacement = project.daemon_pid();
    assert_ne!(replacement, outdated_daemon, "the outdated daemon must be gone");
    assert!(
        daemon::is_process_alive(replacement),
        "the surviving daemon must be alive - a racer that killed the other's replacement \
         would leave the pid file naming a dead process"
    );

    // A third call proves the replacement is not merely alive but still the
    // one being served from, and that nothing is left wanting to replace it.
    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    assert_eq!(project.daemon_pid(), replacement, "a second replacement must never have happened");
}

/// `g-mesh status` is the other half of the acceptance criteria - the
/// discoverable signal for a human who suspects an upgrade has not taken -
/// and it has to work without asking the daemon anything, which is the whole
/// design of that command.
#[tokio::test]
async fn status_reports_a_daemon_that_an_upgrade_has_left_behind() {
    let project = Project::new();

    importers_of(&project, "src/db/connection.ts").await;
    let incumbent = project.daemon_pid();

    let current = status(&project);
    assert!(current.contains("daemon build:    this build"), "{current}");

    make_the_daemon_look_like_it_started_from_an_older_build(&project);

    let outdated = status(&project);
    assert!(outdated.contains("older than this g-mesh"), "{outdated}");
    assert!(outdated.contains("g-mesh stop"), "the report has to say what to do: {outdated}");
    assert_eq!(
        project.daemon_pid(),
        incumbent,
        "reporting on a daemon must never be a way of restarting one"
    );
}

fn status(project: &Project) -> String {
    let output = StdCommand::new(BIN)
        .arg("status")
        .current_dir(project.root())
        .output()
        .expect("failed to run `g-mesh status`");
    assert!(
        output.status.success(),
        "`g-mesh status` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("status output is not valid UTF-8")
}
