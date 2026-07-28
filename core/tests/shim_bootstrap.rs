//! End-to-end coverage for `g-mesh mcp-shim`: it proxies to an already
//! running daemon, and bootstraps a detached one when none is running.
//! Everything here drives the real binary as a subprocess - the point is to
//! exercise actual process spawning, sockets and framing, not a mock.

use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
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

const REQUEST: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"status"}"#;

/// Temp project root plus teardown of the `~/.g-mesh/projects/<hash>/`
/// directory the daemon creates outside it.
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

    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn daemon_pid(&self) -> u32 {
        let path = self.pid_file();
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if self.pid_file().exists() {
            let pid = self.daemon_pid();
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
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

fn spawn_shim(root: &Path) -> Child {
    Command::new(BIN)
        .arg("mcp-shim")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the shim")
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

/// Reads one frame off a stream on a helper thread so a shim that never
/// answers fails the test instead of hanging it forever.
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

/// Sends one request through the shim's stdio and returns what came back,
/// then closes stdin so the shim shuts down and waits for it to exit.
fn round_trip_through_shim(shim: &mut Child) -> Vec<u8> {
    let mut stdin = shim.stdin.take().expect("shim stdin was not piped");
    write_frame(&mut stdin, REQUEST).expect("failed to write a request to the shim");

    let stdout = shim.stdout.take().expect("shim stdout was not piped");
    let response = read_one_frame(stdout);

    drop(stdin);
    let status = wait_with_timeout(shim);
    assert!(status.success(), "shim exited with {status}");
    response
}

fn wait_with_timeout(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("failed to poll the child process") {
            Some(status) => return status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("child process did not exit within {TIMEOUT:?}");
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// One request/response over the daemon socket directly, bypassing the shim.
fn round_trip_over_socket(socket: &Path) -> Vec<u8> {
    let mut stream = UnixStream::connect(socket)
        .unwrap_or_else(|e| panic!("failed to connect to {}: {e}", socket.display()));
    write_frame(&mut stream, REQUEST).expect("failed to write a request to the daemon");
    read_one_frame(stream)
}

/// Asserts the frame is a real `status` response naming the given daemon pid
/// (not a byte-for-byte echo of the request).
fn assert_status_response(frame: &[u8], expected_pid: u32) {
    let response: Value = serde_json::from_slice(frame).expect("response is not valid JSON");
    assert_eq!(response["result"]["status"], "ok");
    assert_eq!(response["result"]["pid"].as_u64(), Some(expected_pid as u64));
}

#[test]
fn shim_proxies_through_an_already_running_daemon() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    let socket = project.socket();
    // The pid file is written once the socket is bound, so it is the signal
    // that the daemon is actually listening.
    let pid_file = project.pid_file();
    wait_for("the daemon to start listening", || pid_file.exists());

    let pid_before = project.daemon_pid();
    assert_eq!(pid_before, daemon.id(), "pid file must name the daemon we started");

    let mut shim = spawn_shim(project.root());
    let response = round_trip_through_shim(&mut shim);

    assert_status_response(&response, pid_before);
    assert!(socket.exists(), "the daemon socket must survive the shim");
    assert_eq!(
        project.daemon_pid(),
        pid_before,
        "the shim must reuse the running daemon, not start another one"
    );
    assert!(daemon.try_wait().unwrap().is_none(), "the daemon must still be running");
    // Teardown kills by pid via `Project::drop`.
}

#[test]
fn shim_bootstraps_a_detached_daemon_when_none_is_running() {
    let project = Project::new();
    let socket = project.socket();
    assert!(!socket.exists(), "no daemon may be running for a fresh project root");

    let mut shim = spawn_shim(project.root());
    let response = round_trip_through_shim(&mut shim);

    assert!(socket.exists(), "the bootstrapped daemon must have bound its socket");
    let pid = project.daemon_pid();
    assert_status_response(&response, pid);

    // The shim has already exited; a second round trip over the same socket
    // proves the daemon it spawned is genuinely detached rather than a child
    // that died with its parent.
    assert_status_response(&round_trip_over_socket(&socket), pid);

    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("failed to signal the daemon");
    assert!(alive.success(), "daemon pid {pid} is no longer alive");
}

#[test]
fn two_concurrent_shim_bootstraps_produce_exactly_one_daemon() {
    let project = Project::new();
    let socket = project.socket();
    assert!(!socket.exists(), "no daemon may be running for a fresh project root");

    // Spawned back-to-back with no synchronization in between: both are
    // independent OS processes that start racing to bootstrap this
    // project's daemon the instant they're spawned, well before either of
    // them appears in this test's own control flow below. That's the real
    // concurrency the bootstrap lock has to serialize.
    let mut shim_a = spawn_shim(project.root());
    let mut shim_b = spawn_shim(project.root());

    let response_a = round_trip_through_shim(&mut shim_a);
    let response_b = round_trip_through_shim(&mut shim_b);

    let pid = project.daemon_pid();
    assert_status_response(&response_a, pid);
    assert_status_response(&response_b, pid);

    // If a second daemon had ever won a later bind race, it would have
    // overwritten the pid file with its own pid; a third round trip straight
    // over the socket still reporting the same pid proves the daemon both
    // shims talked to is still the only one alive for this project.
    assert_status_response(&round_trip_over_socket(&socket), pid);

    let alive = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("failed to signal the daemon");
    assert!(alive.success(), "daemon pid {pid} is no longer alive");
}
