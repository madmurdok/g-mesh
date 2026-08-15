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

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command as TokioCommand;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

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
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");
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
        let mut statement =
            conn.prepare("SELECT DISTINCT filePath FROM nodes ORDER BY filePath").unwrap();
        statement.query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    fn node_id_exists(&self, id: &str) -> bool {
        let conn = Connection::open(self.index_path()).expect("failed to open the project index");
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = ?1", [id], |row| row.get::<_, i64>(0))
            .unwrap()
            > 0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            if let Ok(pid) = std::fs::read_to_string(&path) {
                let _ =
                    Command::new("kill").arg("-9").arg(pid.trim()).stderr(Stdio::null()).status();
            }
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
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
    let removed = conn.execute("DELETE FROM edges WHERE kind = 'IMPORTS'", []).expect("failed to strip edges");
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
        cmd.arg("mcp-shim")
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
    assert!(!project.node_id_exists(PHANTOM_NODE_ID), "a wiped index must not still answer for a deleted file");

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
