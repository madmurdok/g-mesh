//! What a dead daemon leaves in its state directory - its socket file, a
//! language's suspension marker, a reindex's staging index, a seed's temp
//! file - is gone by the time the next daemon publishes its pid file. Each is
//! cleared earlier in the same startup thread (`daemon::startup::open_index`
//! for the staging and seed files, `daemon::startup::publish` for the socket
//! and markers, both before the bind the pid file follows), so the assertions
//! read the directory once, right when the pid file names the new daemon.
//! `docs/design/GM-707-watcher-ingest-startup-phases.md`, section 4, rows 7,
//! 17 and 18.
//!
//! Unix only: on Windows the endpoint is a pipe name with no file to leave
//! behind.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use g_mesh::daemon;
use g_mesh::paths::state_dir::StateDir;
use g_mesh::storage::connection::project_dir;

use crate::common;
use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

struct Project {
    dir: tempfile::TempDir,
    daemons: Vec<Child>,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        fs::write(dir.path().join("a.ts"), b"export const a = 1;\n")
            .expect("failed to seed the project with a source file");
        Self { dir, daemons: Vec::new() }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state(&self) -> StateDir {
        StateDir::at(project_dir(self.root()).expect("failed to resolve the project state directory"))
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    /// Starts a daemon directly (this process reaps it) and waits until the
    /// pid file names it.
    fn start_daemon(&mut self) -> u32 {
        let mut child = Command::new(BIN)
            .lifeline()
            .arg("daemon")
            .arg("--project-root")
            .arg(self.root())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the daemon");
        let pid = child.id();
        let pid_file = self.pid_file();
        let mut exited = None;
        common::wait_for("the pid file to name the new daemon", common::startup_timeout(), || {
            exited = child.try_wait().expect("failed to poll the daemon");
            exited.is_some() || fs::read_to_string(&pid_file).is_ok_and(|text| text.trim() == pid.to_string())
        });
        assert!(exited.is_none(), "the daemon exited ({exited:?}) before it published its pid file");
        self.daemons.push(child);
        pid
    }

    /// SIGKILLs the newest daemon and reaps it: nothing of its teardown runs.
    fn kill_daemon(&mut self) {
        let mut child = self.daemons.pop().expect("a daemon to kill");
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for child in &mut self.daemons {
            let _ = child.kill();
            let _ = child.wait();
        }
        let state = self.state();
        let _ = fs::remove_dir_all(state.path());
    }
}

#[test]
fn a_daemon_start_clears_what_a_dead_daemon_left_before_it_publishes_its_pid_file() {
    let mut project = Project::new();
    project.start_daemon();
    project.kill_daemon();

    let state = project.state();
    assert!(project.socket().exists(), "a killed daemon leaves its socket file behind");
    let marker = state.plugin_suspended_marker("typescript");
    let staging = state.staging_db("typescript");
    let seed_tmp = state.seed_tmp_file();
    fs::write(&marker, "over its memory limit\n").unwrap();
    fs::write(&staging, b"left by a reindex that died").unwrap();
    fs::write(&seed_tmp, b"left by a seed that died").unwrap();

    project.start_daemon();

    assert!(daemon::is_listening(project.root()).unwrap(), "the new daemon must serve the endpoint");
    assert!(!marker.exists(), "the dead daemon's suspension marker must be cleared");
    assert!(state.suspended_markers().is_empty(), "no suspension marker may survive a restart");
    assert!(!staging.exists(), "the dead reindex's staging index must be removed");
    assert!(!seed_tmp.exists(), "the dead seed's temp file must be removed");
}
