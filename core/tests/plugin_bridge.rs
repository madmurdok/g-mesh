//! End-to-end coverage for the daemon<->plugin wiring: starting the real
//! `g-mesh daemon` binary must spawn the real JS/TS plugin (not a stub),
//! complete its handshake, and - when a fixture file changes under the
//! watched project root - route that change to the plugin and commit its
//! diff response to the project's SQLite index.
//!
//! Requires `plugins/js-ts/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there automatically whenever this crate is built.

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
// Generous relative to daemon_core.rs's 10s: this path also pays for a Node
// process start and a real tree-sitter parse, not just SQLite/socket setup.
const TIMEOUT: Duration = Duration::from_secs(20);

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root") }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn file_change_is_routed_through_the_real_js_ts_plugin_and_applied_to_storage() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());

    // The daemon spawns and handshakes with the plugin *before* it writes
    // its pid file (see daemon::run) - if the plugin failed to start or its
    // handshake didn't check out, the daemon would have already exited with
    // an error and this pid file would never appear, so waiting on it also
    // proves the plugin came up cleanly.
    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon (and its JS/TS plugin) to start", || pid_file.exists());

    let fixture = project.root().join("fixture.ts");
    fs::write(
        &fixture,
        "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
    )
    .expect("failed to write the fixture file");

    let db_path = project_dir(project.root()).unwrap().join("index.db");
    wait_for("the plugin's file-change diff to be committed to the SQLite index", || {
        let Ok(conn) = Connection::open(&db_path) else {
            return false;
        };
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE name = 'add'", [], |row| row.get::<_, i64>(0))
            .map(|count| count > 0)
            .unwrap_or(false)
    });

    let conn = Connection::open(&db_path).unwrap();
    let (kind, file_path): (String, String) = conn
        .query_row("SELECT kind, filePath FROM nodes WHERE name = 'add'", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(kind, "Function", "the real tree-sitter extraction must classify `add` as a Function node");
    assert_eq!(file_path, "fixture.ts", "the node's filePath must be the project-relative path, not absolute");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
