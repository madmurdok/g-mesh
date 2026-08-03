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

fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    wait_for_within(what, TIMEOUT, ready);
}

fn wait_for_within(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
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

/// The other half of `symbol_links`'s
/// `two_reexport_branches_offering_one_name_leave_the_edge_unresolved`, with
/// nothing faked in between: a real barrel whose two `export *` branches both
/// offer `mutate`, a real plugin, a real `tsserver`, and the upgraded edge
/// read back out of the real index.
///
/// The structural pass cannot answer this one - all a name-matching walk sees
/// is two equally good candidates - so an edge that is `ts-compiler` and
/// resolved here can only have come from the checker, over the wire, through
/// `apply_semantic_pass`.
#[test]
fn an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());

    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon (and its JS/TS plugin) to start", || pid_file.exists());

    let db_path = project_dir(project.root()).unwrap().join("index.db");
    let count = |sql: &str| -> i64 {
        Connection::open(&db_path)
            .and_then(|conn| conn.query_row(sql, [], |row| row.get(0)))
            .unwrap_or(0)
    };

    // The whole fixture is re-written until the index answers, rather than
    // written once. The watcher is registered a moment *after* the cold-start
    // walk and pass, and nothing marks the instant it starts, so a write can
    // land in that gap - where it is not missed-and-retried but missed
    // outright, leaving the test waiting on an event that is never coming (the
    // same race `semantic_pass_trigger.rs` documents). Re-writing costs
    // nothing once the watcher is up: unchanged content reparses to an empty
    // diff, and the first round that gets through ends the loop.
    //
    // The deadline is longer than the shared one: this wait also pays for a
    // `tsserver` child's startup and its first project load, which the
    // structural path never touches.
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut edits = 0;
    loop {
        edits += 1;
        fs::write(
            project.root().join("tsconfig.json"),
            "{ \"compilerOptions\": { \"strict\": true } }\n",
        )
        .unwrap();
        fs::write(project.root().join("a.ts"), "export function mutate(): \"a\" {\n  return \"a\";\n}\n")
            .unwrap();
        fs::write(project.root().join("b.ts"), "export function mutate(): \"b\" {\n  return \"b\";\n}\n")
            .unwrap();
        fs::write(project.root().join("index.ts"), "export * from \"./a\";\nexport * from \"./b\";\n")
            .unwrap();
        // The importer last, and with a changing tail so every round is a real
        // edit: an unchanged `caller.ts` is a reparse the plugin short-circuits.
        fs::write(
            project.root().join("caller.ts"),
            format!(
                "import {{ mutate }} from \"./index\";\n\n\
                 export function run(): void {{\n  mutate();\n}}\n\
                 // edit {edits}\n"
            ),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(250));

        if count(
            "SELECT COUNT(*) FROM edges WHERE kind = 'CALLS' AND source = 'ts-compiler' AND resolved = 1",
        ) > 0
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not resolve the call through the ambiguous barrel after {edits} edit(s)"
        );
    }

    let conn = Connection::open(&db_path).unwrap();
    // Both branches really are in the index, so the edge above was chosen
    // between two candidates rather than defaulted to the only one there was.
    let branches: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes WHERE name = 'mutate' AND kind = 'Function'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(branches, 2, "both `export *` branches must be indexed for this to be ambiguous at all");
    let target: String = conn
        .query_row(
            "SELECT n.filePath FROM edges e JOIN nodes n ON n.id = e.toId \
             WHERE e.kind = 'CALLS' AND e.source = 'ts-compiler'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // `export * from "./a"` is written first, and that is the branch the
    // compiler hands a consumer - the second one is a TS2308 ambiguity against
    // the barrel, not against the import.
    assert_eq!(target, "a.ts", "the call must land on the branch TypeScript itself resolves to");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
