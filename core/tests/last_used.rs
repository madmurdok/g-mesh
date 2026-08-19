//! End-to-end coverage for `meta.lastUsed`: a real daemon process advances it
//! when it starts and again for every request it handles, and every read here
//! is made straight off the project's `index.db` - never by asking the daemon.
//!
//! That last part is the point of the ticket, so it is also the shape of the
//! test: `gc::last_used::read_from_project_dir` is called from this process,
//! against a directory a *different* process owns, including after that
//! process is gone.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::ipc;
use g_mesh::gc::last_used::{self, LastUsed};
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::{json, Value};

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Comfortably longer than the millisecond resolution `lastUsed` is recorded
/// at, so "the stamp moved" is an observation rather than a race.
const TICK: Duration = Duration::from_millis(20);

/// Temp project root plus teardown of the `~/.g-mesh/projects/<hash>/`
/// directory the daemon creates outside it - same shape as
/// `shim_bootstrap.rs`'s helper.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    /// One real source file, so the daemon's cold-start bulk index produces a
    /// graph an outline query can actually be answered from.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("a.ts"), b"export function hello() { return 1; }\n")
            .expect("failed to seed the project with a source file");
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory")
    }

    fn endpoint(&self) -> ipc::Endpoint {
        daemon::endpoint(self.root()).expect("failed to resolve the daemon endpoint")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    /// The whole point of the ticket: read the project's `lastUsed` from its
    /// state directory alone, with nothing asked of any daemon.
    fn last_used(&self) -> LastUsed {
        last_used::read_from_project_dir(&self.state_dir())
            .expect("failed to read the project's lastUsed")
            .expect("a started daemon must have recorded a lastUsed")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.pid_file()) {
            // Silenced: a test that killed its own daemon leaves a pid file
            // behind, and "no such process" here is the expected case, not a
            // failure worth printing into the test output.
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.trim())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
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

/// One `initialize` + `tools/call` round trip straight over the daemon
/// endpoint. `tools/call` specifically, not `tools/list`: `lastUsed` is
/// advanced by the tool handlers, which is what "a request was handled"
/// means here.
fn call_a_tool(endpoint: &ipc::Endpoint) {
    let stream = ipc::Stream::connect(endpoint)
        .unwrap_or_else(|e| panic!("failed to connect to {endpoint}: {e}"));
    let mut writer = stream.try_clone().expect("failed to clone the daemon connection");
    let mut reader = BufReader::new(stream);

    send(&mut writer, &json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-last-used-tests", "version": "0" },
        },
    }));
    let initialized = receive(&mut reader);
    assert_eq!(
        initialized["result"]["serverInfo"]["name"], "g-mesh",
        "unexpected initialize response: {initialized}"
    );
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    send(&mut writer, &json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": "a.ts" } },
    }));
    let called = receive(&mut reader);
    assert!(
        called["result"].is_object(),
        "the tool call must have been handled, got: {called}"
    );
}

fn send<W: Write>(writer: &mut W, message: &Value) {
    let body = serde_json::to_vec(message).expect("request is always serializable");
    write_ndjson_frame(writer, &body).unwrap_or_else(|e| panic!("cannot send {message}: {e:#}"));
}

fn receive<R: Read>(reader: &mut BufReader<R>) -> Value {
    let frame = read_ndjson_frame(reader)
        .unwrap_or_else(|e| panic!("cannot read a response: {e:#}"))
        .expect("the daemon closed the connection instead of answering");
    serde_json::from_slice(&frame).unwrap_or_else(|e| {
        panic!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame))
    })
}

#[test]
fn a_daemon_start_and_every_handled_request_advance_last_used_on_disk() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    // Waited out in full rather than just to the bind: a call answered with
    // "still indexing" deliberately does not touch `lastUsed` (see
    // `mcp::GMeshMcpServer::still_indexing` on why it must not take the
    // SQLite mutex the walk is holding), and the daemon's own startup touch
    // already covers the walk. This test is about what a *handled* request
    // does, so it has to make one.
    wait_until_indexed(project.root());

    let after_start = project.last_used();
    assert!(
        after_start.idle < Duration::from_secs(30),
        "a daemon that just started must not look idle: {:?}",
        after_start.idle
    );

    thread::sleep(TICK);
    call_a_tool(&project.endpoint());
    let after_first_request = project.last_used();
    assert!(
        after_first_request.timestamp > after_start.timestamp,
        "a handled request must advance lastUsed: {} is not later than {}",
        after_first_request.timestamp,
        after_start.timestamp,
    );

    // Not once per session: a second request on a *new* connection has to
    // move it again.
    thread::sleep(TICK);
    call_a_tool(&project.endpoint());
    let after_second_request = project.last_used();
    assert!(
        after_second_request.timestamp > after_first_request.timestamp,
        "every handled request must advance lastUsed: {} is not later than {}",
        after_second_request.timestamp,
        after_first_request.timestamp,
    );

    assert!(daemon.try_wait().unwrap().is_none(), "the daemon must still be running");
    // Teardown kills by pid via `Project::drop`.
}

#[test]
fn last_used_survives_the_daemon_that_wrote_it() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    wait_until_indexed(project.root());

    thread::sleep(TICK);
    call_a_tool(&project.endpoint());
    let while_running = project.last_used();

    daemon.kill().expect("failed to kill the daemon");
    daemon.wait().expect("failed to reap the daemon");
    // Killed, not asked to stop: whatever is on disk is all a GC scan will
    // ever have to work from.
    let after_death = project.last_used();

    assert_eq!(
        after_death.timestamp, while_running.timestamp,
        "the recorded stamp must survive the process that wrote it"
    );
    assert!(
        after_death.idle < Duration::from_secs(60),
        "idle is measured from the stamp, not from the read: {:?}",
        after_death.idle
    );
}

/// A `~/.g-mesh/projects/` entry no daemon ever finished setting up must read
/// as "nothing recorded", not as an error that would abort a whole scan.
#[test]
fn a_project_directory_with_no_index_reads_as_nothing_recorded() {
    let dir = tempfile::tempdir().expect("failed to create a temp directory");
    assert_eq!(last_used::read_from_project_dir(dir.path()).unwrap(), None);
}
