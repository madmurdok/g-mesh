//! A client must be able to tell that its server has gone.
//!
//! The MCP stdio transport is a pair of pipes between the client and the shim.
//! When the shim exits, the client learns it by reading EOF - and EOF arrives
//! only once *every* handle to the write end is closed. The shim bootstraps a
//! detached daemon that outlives it by design, so the question this file exists
//! to answer is whether that daemon ends up holding a copy of the client's
//! pipe.
//!
//! On Unix it never could: Rust opens its file descriptors `CLOEXEC`, so a
//! child gets what `Command`'s stdio names and nothing else. On Windows
//! `Stdio::null()` does not mean what it appears to - `CreateProcess` is
//! called with `bInheritHandles = TRUE` and hands over every inheritable
//! handle in the parent, whatever STARTUPINFO says - so the daemon held the
//! client's pipe and the client waited forever for a server that was already
//! gone (GM-251).
//!
//! Deliberately not `#![cfg(windows)]`, though only Windows ever failed it.
//! The invariant is platform-neutral, it is cheap to check, and a test that
//! runs on the three platforms where it already passes is what stops the
//! fourth from regressing quietly.

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Generous, because it covers a daemon cold-starting a real JS/TS plugin.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(30);

/// Not generous, and that is the point. Once the shim is gone, EOF is either
/// immediate or never - there is no third case where it merely takes a while,
/// so a budget this size fails fast on the bug rather than idling toward a
/// harness timeout.
const EOF_BUDGET: Duration = Duration::from_secs(10);

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn a_daemon_the_shim_leaves_behind_does_not_hold_the_client_s_pipe() {
    let project = tempfile::tempdir().expect("failed to create a temp project root");
    fs::write(project.path().join("a.ts"), "export const a = 1;\n")
        .expect("failed to seed the project with a source file");

    // Piped stdout is what a real MCP client gives it, and is the handle whose
    // fate this test is about.
    let mut shim = Command::new(BIN)
        .arg("mcp-shim")
        .current_dir(project.path())
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the shim");

    let pid_file: PathBuf = daemon::pid_path(project.path()).expect("failed to resolve the pid file");
    wait_for("the shim to bootstrap a daemon", || pid_file.exists());
    let daemon_pid = daemon::read_pid_file(&pid_file).expect("the daemon must have recorded its pid");

    let mut client_end = shim.stdout.take().expect("the shim's stdout was piped");
    shim.kill().expect("failed to kill the shim");
    shim.wait().expect("failed to reap the shim");

    // The read runs on its own thread so this one can bound it. A blocking
    // `read_to_end` against a write end nobody closed does not return, and the
    // failure worth reporting is "it did not", not a hung test binary.
    let (done, eof) = mpsc::channel();
    thread::spawn(move || {
        let mut drained = Vec::new();
        let _ = client_end.read_to_end(&mut drained);
        let _ = done.send(());
    });

    let reached_eof = eof.recv_timeout(EOF_BUDGET).is_ok();

    // Read before the assertion below can end the test, because it is half of
    // what makes the result meaningful either way.
    let daemon_still_running = daemon::is_process_alive(daemon_pid);
    common::kill_and_wait(daemon_pid);
    if let Ok(state) = project_dir(project.path()) {
        let _ = fs::remove_dir_all(&state);
    }

    assert!(
        reached_eof,
        "the client's read did not end within {EOF_BUDGET:?} after the shim exited - \
         something else still holds the write end of its pipe, and the daemon \
         (pid {daemon_pid}, alive: {daemon_still_running}) is the only candidate"
    );
    assert!(
        daemon_still_running,
        "the daemon must outlive the shim - otherwise EOF proves nothing about \
         handle inheritance, only that everything died"
    );
}
