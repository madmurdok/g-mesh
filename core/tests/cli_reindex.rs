//! `g-mesh reindex` against a project whose index was deliberately made to
//! disagree with disk - the acceptance test for task 57.
//!
//! Two independent ways an index can go stale are staged at once, matching
//! the two halves of "disagree with disk": data written straight into the
//! SQLite index that nothing on disk justifies (a phantom node, and a stripped
//! import edge a fresh walk would restore), and a file added to disk after the
//! index was built, which the index has never even seen. `reindex` has to fix
//! both, because it rebuilds from scratch rather than reconciling row by row.
//!
//! Driven over the real `g-mesh` binary and a real daemon, the same way
//! `cli_stop.rs` and `stale_index_invalidation.rs` are: `reindex` has to
//! signal a *live* daemon safely, and the only way to prove that is to give it
//! one.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command as TokioCommand;

mod common;

use common::wait_until_indexed;
use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One file importing another - enough to prove import edges survive the
/// rebuild relinked, not just present.
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

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    /// Bootstraps a detached daemon through the shim and returns once it has
    /// finished its cold-start walk, so `reindex` has a live daemon - and a
    /// complete index - to contend with.
    fn bootstrap_daemon(&self) {
        let mut shim = Command::new(BIN)
            .lifeline()
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");
        // GM-395 slice 2: the daemon walks - and so spawns its plugin - only once a tool call asks.
        common::trigger_activation(self.root());
        wait_for("the daemon to spawn its plugin", || self.plugin_pid_file().exists());
        wait_until_indexed(self.root());
        // The shim was only the vehicle: the daemon it spawned is detached
        // and outlives it.
        let _ = shim.kill();
        let _ = shim.wait();
    }

    fn daemon_pid(&self) -> u32 {
        std::fs::read_to_string(self.pid_file())
            .expect("the daemon must have a pid file")
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }

    fn reindex(&self) -> Output {
        Command::new(BIN)
            .arg("reindex")
            .current_dir(self.root())
            .output()
            .expect("failed to run `g-mesh reindex`")
    }

    /// Every `nodes.filePath` currently recorded, sorted and de-duplicated -
    /// the index's own account of what it covers.
    fn indexed_file_paths(&self) -> Vec<String> {
        let conn = Connection::open(self.index_path()).expect("failed to open the project index");
        let mut statement = conn.prepare("SELECT DISTINCT filePath FROM nodes ORDER BY filePath").unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn node_id_exists(&self, id: &str) -> bool {
        let conn = Connection::open(self.index_path()).expect("failed to open the project index");
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = ?1", [id], |row| row.get::<_, i64>(0)).unwrap()
            > 0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            common::kill_pid_file(&path);
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// GM-301: see `common::wait_for`'s doc comment for why this delegates
/// instead of polling against a file-local timeout constant.
fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

/// A node id no real walk would ever produce, planted directly in the index
/// so the test can prove `reindex` actually threw the table away rather than
/// merely appending to it.
const PHANTOM_NODE_ID: &str = "phantom-node-from-a-stale-index";

/// Writes data into the index that nothing on disk justifies: a node for a
/// file that was deleted, and a missing import edge a fresh walk would
/// restore. Exactly the shape "an index disagrees with disk" is meant to
/// cover, staged directly against the SQLite file the way a corrupted or
/// hand-edited index would be found.
fn corrupt_the_index(index: &Path) {
    let conn = Connection::open(index).expect("failed to open the project index");
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol,
                            endLine, endCol, language)
         VALUES (?1, 'Function', 'ghost', 'ghost', 'src/deleted-file-that-never-existed.ts', 1, 0, 1, 0, 'typescript')",
        [PHANTOM_NODE_ID],
    )
    .expect("failed to plant a phantom node");
    let removed =
        conn.execute("DELETE FROM edges WHERE kind = 'IMPORTS'", []).expect("failed to strip edges");
    assert!(removed > 0, "the fixture must have had import edges to strip");
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Boots a fresh shim (and, through it, a fresh daemon) and asks who imports
/// `file_path` - the same round trip `stale_index_invalidation.rs` uses to
/// prove a rebuilt index answers correctly, not just that its row count
/// changed.
async fn importers_of(project: &Project, file_path: &str) -> Vec<String> {
    let transport = TokioChildProcess::new(TokioCommand::new(BIN).configure(|cmd| {
        cmd.lifeline();
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
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

/// The acceptance criterion for task 57: a project whose index was
/// deliberately made to disagree with disk ends up, after `reindex`, with an
/// index matching current disk content exactly - verified against a *live*
/// daemon, which `reindex` has to stop safely to get there.
#[tokio::test]
async fn reindex_against_a_deliberately_stale_index_rebuilds_it_to_match_disk() {
    let project = Project::new();
    project.bootstrap_daemon();
    let daemon_pid = project.daemon_pid();
    assert!(daemon::is_process_alive(daemon_pid), "the daemon must be running before reindex");

    // Disk gains a file the existing index has never seen, and the index
    // itself is doctored to disagree with disk on top of that - both halves
    // of "the index is suspected wrong".
    std::fs::write(
        project.root().join("src/util.ts"),
        "export function double(n: number): number {\n  return n * 2;\n}\n",
    )
    .expect("failed to add a file the existing index has never seen");
    corrupt_the_index(&project.index_path());
    assert!(project.node_id_exists(PHANTOM_NODE_ID), "the fixture must have planted its phantom node");

    let output = project.reindex();
    assert!(
        output.status.success(),
        "`g-mesh reindex` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("reindex output is not valid UTF-8");
    assert!(stdout.contains("stopped for the rebuild"), "{stdout}");
    assert!(!daemon::is_process_alive(daemon_pid), "reindex must stop the daemon it found running");

    // The phantom node is gone: the table was thrown away, not patched.
    assert!(
        !project.node_id_exists(PHANTOM_NODE_ID),
        "a wiped index must not still answer for a deleted file"
    );

    // Every file actually on disk is covered, and nothing else is.
    assert_eq!(
        project.indexed_file_paths(),
        vec!["src/db/connection.ts".to_string(), "src/index.ts".to_string(), "src/util.ts".to_string()],
        "the rebuilt index must cover exactly what is on disk now"
    );

    // And the graph itself is right, not just the file coverage: the import
    // edge `corrupt_the_index` stripped is back, freshly relinked.
    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "the rebuilt index must relink the import a stale index had lost"
    );
}

/// The other shape `reindex` has to handle: nothing is running, so there is
/// nothing to stop first - and the rebuild still has to happen.
#[test]
fn reindex_against_an_idle_project_still_rebuilds_from_scratch() {
    let project = Project::new();

    let output = project.reindex();

    assert!(
        output.status.success(),
        "`g-mesh reindex` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("reindex output is not valid UTF-8");
    assert!(!stdout.contains("stopped for the rebuild"), "{stdout}");
    assert_eq!(
        project.indexed_file_paths(),
        vec!["src/db/connection.ts".to_string(), "src/index.ts".to_string()],
    );
}

/// Shortens how long a shim waits for a daemon it bootstrapped, so a shim
/// that wrongly bootstraps one during a rebuild fails sooner. Only for a shim
/// that must not bootstrap at all: a real bootstrap under load can take
/// longer.
const BOOTSTRAP_TIMEOUT_ENV: &str = "G_MESH_BOOTSTRAP_TIMEOUT_MS";

/// Plays a CLI rebuild of a project the way `g-mesh reindex` holds one: its
/// daemon lock held and its rebuild marker naming this process. Dropping it
/// removes the marker, then releases the lock.
struct HeldForRebuild {
    marker: PathBuf,
    _lock: File,
}

impl HeldForRebuild {
    fn take(root: &Path) -> Self {
        let state = g_mesh::storage::connection::ensure_project_dir(root).unwrap();
        let lock = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(daemon::daemon_lock_path_in(&state))
            .unwrap();
        // The kernel releases a killed daemon's lock shortly after it dies.
        wait_for("the daemon lock to be free", || lock.try_lock().is_ok());
        let marker = daemon::rebuild_marker_path_in(&state);
        let started = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(&marker, format!("{}\n{started}\nreindex\n", std::process::id())).unwrap();
        Self { marker, _lock: lock }
    }
}

impl Drop for HeldForRebuild {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// An MCP session over a shim started in `project`, with `env` on top.
async fn shim_session(project: &Project, env: &[(&str, &str)]) -> RunningService<RoleClient, ()> {
    let transport = TokioChildProcess::new(TokioCommand::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
        for (key, value) in env {
            cmd.env(key, value);
        }
    }))
    .expect("failed to spawn the shim");
    ().serve(transport).await.expect("MCP initialization failed")
}

async fn outline_of_index(client: &RunningService<RoleClient, ()>) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new("get_file_outline").with_arguments(
            json!({ "file_path": "src/index.ts" }).as_object().cloned().expect("an object literal"),
        ))
        .await
        .expect("tools/call must return a result, not a protocol failure")
}

fn texts_of(result: &CallToolResult) -> Vec<&str> {
    result.content.iter().filter_map(|block| block.as_text()).map(|text| text.text.as_str()).collect()
}

/// The symbol names of a `get_file_outline` answer, read from its last text
/// item: the shim may put its own lines before it.
fn outline_names(result: &CallToolResult) -> Vec<String> {
    assert_ne!(result.is_error, Some(true), "expected an outline: {:?}", texts_of(result));
    let outline: Value = serde_json::from_str(texts_of(result).last().expect("no text item"))
        .unwrap_or_else(|err| panic!("the outline is not JSON ({err}): {:?}", texts_of(result)));
    outline["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .filter_map(|symbol| symbol["name"].as_str().map(str::to_string))
        .collect()
}

/// The line the shim puts first in the first answer after it reconnected.
const RESTARTED: &str = "restarted since this session's previous answer from it";

fn marks_restart(result: &CallToolResult) -> bool {
    texts_of(result).iter().any(|text| text.contains(RESTARTED))
}

/// An MCP session open over the shim survives `g-mesh reindex` of its
/// project: the call after the rebuild is answered by a new daemon, the
/// first such answer starts with one restart line, and later answers carry
/// none.
///
/// Control: in `upstream_ended`, send `Event::Done` when the current
/// upstream ends: the shim exits with its daemon and the call after the
/// rebuild fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_mcp_session_survives_reindex_and_marks_the_restart_once() {
    let project = Project::new();
    let client = shim_session(&project, &[]).await;
    let first = outline_of_index(&client).await;
    assert!(!marks_restart(&first), "nothing restarted yet: {:?}", texts_of(&first));
    wait_until_indexed(project.root());
    let daemon_pid = project.daemon_pid();

    let output = tokio::task::block_in_place(|| project.reindex());
    assert!(
        output.status.success(),
        "`g-mesh reindex` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("stopped for the rebuild"));
    assert!(!daemon::is_process_alive(daemon_pid), "reindex stops the session's daemon");
    assert_eq!(daemon::rebuild_in_progress(project.root()).unwrap(), None, "reindex released the project");

    let after = outline_of_index(&client).await;
    let texts = texts_of(&after);
    assert!(texts[0].starts_with("g-mesh: the daemon serving ") && texts[0].contains(RESTARTED), "{texts:?}");
    assert!(outline_names(&after).contains(&"start".to_string()), "{texts:?}");
    let new_pid = project.daemon_pid();
    assert_ne!(new_pid, daemon_pid, "the call after the rebuild is served by a new daemon");
    assert!(daemon::is_process_alive(new_pid));

    let later = outline_of_index(&client).await;
    assert!(!marks_restart(&later), "only the first answer is marked: {:?}", texts_of(&later));
    assert!(outline_names(&later).contains(&"start".to_string()));

    client.cancel().await.expect("failed to shut the client down");
}

/// While a rebuild holds the project, a call over an open session is
/// answered at once with an error naming the rebuild, and no daemon is
/// bootstrapped; once the rebuild lets go, the next call reconnects.
///
/// Control: drop both `rebuilding()` checks in `connect_or_bootstrap`: the
/// shim bootstraps a daemon, which cannot take the held lock, and the call
/// gets the bootstrap failure instead (after the default bootstrap timeout).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_while_the_project_is_held_for_a_rebuild_is_answered_at_once() {
    let project = Project::new();
    let client = shim_session(&project, &[]).await;
    outline_of_index(&client).await;
    wait_until_indexed(project.root());
    let daemon_pid = project.daemon_pid();
    tokio::task::block_in_place(|| common::kill_and_wait(daemon_pid));
    let held = tokio::task::block_in_place(|| HeldForRebuild::take(project.root()));

    let during = outline_of_index(&client).await;
    let text = texts_of(&during).concat();
    assert_eq!(during.is_error, Some(true), "{text}");
    let expected = format!("is being reindexed (g-mesh reindex, pid {}, ", std::process::id());
    assert!(text.starts_with("g-mesh: ") && text.contains(&expected), "{text}");
    assert!(!text.contains("could not reach"), "the rebuild is named, not a bootstrap failure: {text}");
    let running = daemon::read_pid_file(&project.pid_file()).filter(|pid| daemon::is_process_alive(*pid));
    assert_eq!(running, None, "no daemon may serve a project held for a rebuild");

    drop(held);
    let after = outline_of_index(&client).await;
    assert!(marks_restart(&after), "{:?}", texts_of(&after));
    assert!(outline_names(&after).contains(&"start".to_string()));

    client.cancel().await.expect("failed to shut the client down");
}

/// A shim started while a rebuild holds its project has no daemon to answer
/// `initialize`: it exits with the rebuild named, without bootstrapping one.
///
/// Control: drop both `rebuilding()` checks in `connect_or_bootstrap`: the
/// shim bootstraps a daemon and fails on the bootstrap timeout instead.
#[test]
fn a_shim_started_while_the_project_is_held_for_a_rebuild_exits_naming_it() {
    let project = Project::new();
    let held = HeldForRebuild::take(project.root());

    let output = Command::new(BIN)
        .lifeline()
        .arg("mcp-shim")
        .current_dir(project.root())
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .env(BOOTSTRAP_TIMEOUT_ENV, "2000")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the shim");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    let expected = format!("is being reindexed (g-mesh reindex, pid {}, ", std::process::id());
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(!project.pid_file().exists(), "no daemon was bootstrapped");
    drop(held);
}
