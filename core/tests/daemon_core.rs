//! Coverage for `g-mesh daemon` itself, independent of the shim: starting it
//! for a project opens the SQLite index, registers the file watcher, and
//! serves a real status response over its socket.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::protocol::jsonrpc::{read_frame, write_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

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
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn read_one_frame<R: Read + Send + 'static>(reader: R) -> Vec<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let _ = tx.send(read_frame(&mut reader).expect("failed to read a frame"));
    });
    rx.recv_timeout(TIMEOUT)
        .expect("timed out waiting for a response frame")
        .expect("stream closed before a response frame arrived")
}

#[test]
fn daemon_opens_sqlite_watches_files_and_answers_a_status_ping() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());

    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let db_path = project_dir(project.root()).unwrap().join("index.db");
    assert!(db_path.exists(), "the daemon must open (and thus create) the project's SQLite file");

    let socket = daemon::socket_path(project.root()).unwrap();
    let mut stream = UnixStream::connect(&socket)
        .unwrap_or_else(|e| panic!("failed to connect to {}: {e}", socket.display()));
    write_frame(&mut stream, br#"{"jsonrpc":"2.0","id":1,"method":"status"}"#).unwrap();

    let response: Value = serde_json::from_slice(&read_one_frame(stream)).unwrap();
    assert_eq!(response["result"]["status"], "ok");
    assert_eq!(response["result"]["pid"].as_u64(), Some(daemon.id() as u64));
    assert_eq!(response["result"]["nodeCount"], 0, "a fresh index has no nodes yet");

    // A file write under the project root must not crash or hang the
    // daemon - proves the watcher is registered and running, even though
    // wiring its events into a reindex is a separate ticket.
    std::fs::write(project.root().join("tracked.txt"), b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let mut stream = UnixStream::connect(&socket).unwrap();
    write_frame(&mut stream, br#"{"jsonrpc":"2.0","id":2,"method":"status"}"#).unwrap();
    let response: Value = serde_json::from_slice(&read_one_frame(stream)).unwrap();
    assert_eq!(response["result"]["status"], "ok", "daemon must still be responsive after a watched file write");

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn unknown_method_gets_a_json_rpc_error_not_a_crash() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let socket = daemon::socket_path(project.root()).unwrap();
    let mut stream = UnixStream::connect(&socket).unwrap();
    write_frame(&mut stream, br#"{"jsonrpc":"2.0","id":1,"method":"not_a_real_method"}"#).unwrap();

    let response: Value = serde_json::from_slice(&read_one_frame(stream)).unwrap();
    assert_eq!(response["error"]["code"], -32601);

    let _ = daemon.kill();
    let _ = daemon.wait();
}
