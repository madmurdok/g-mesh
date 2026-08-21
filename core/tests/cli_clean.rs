//! `g-mesh clean` driven as a real subprocess, against a real project state
//! directory under the projects root the run resolves.
//!
//! Only forms that delete nothing outside this test's own project are driven
//! here. `clean expired`, `clean orphaned --force` and `clean all` are scoped
//! to *every* project under the root, and the root is shared: before task 217
//! it was the developer's real `~/.g-mesh`, and since then it is one
//! `G_MESH_HOME` that every test binary in this directory writes into
//! concurrently. Either way a sweeping delete from here would take out
//! fixtures that belong to someone else.
//!
//! Those forms now live in `cli_clean_sweeping.rs`, which gives each test a
//! `G_MESH_HOME` of its own so a sweep inside it is safe by construction. The
//! unit tests in `cli::clean` still cover the classification with an injected
//! root; what they cannot show, and that file can, is that the word on the
//! command line reaches that code at all.
//!
//! What is still worth driving as a subprocess is the wiring: that the word
//! reaches the right code path and reports rather than deletes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("a.ts"), b"export const a = 1;\n")
            .expect("failed to seed the project with a source file");
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the project state directory")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    /// Bootstraps a detached daemon through the shim, so the state directory
    /// this test deletes is one a real daemon really built.
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
        wait_for("the daemon to start listening", || self.pid_file().exists());
        let _ = shim.kill();
        let _ = shim.wait();
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root())
            .output()
            .unwrap_or_else(|e| panic!("failed to run `g-mesh {}`: {e}", args.join(" ")))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            if let Ok(pid) = std::fs::read_to_string(&path) {
                let _ = Command::new("kill").arg("-9").arg(pid.trim()).stderr(Stdio::null()).status();
            }
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
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

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The acceptance criterion: deletion actually removes the project's
/// `~/.g-mesh/projects/<hash>/` directory.
#[test]
fn clean_deletes_the_cwds_project_state_directory() {
    let project = Project::new();
    project.bootstrap_daemon();
    let state_dir = project.state_dir();
    assert!(state_dir.is_dir(), "the daemon must have built a state directory to delete");

    let stopped = project.command(&["stop"]);
    assert!(stopped.status.success(), "stop failed: {}", stderr_of(&stopped));

    let cleaned = project.command(&["clean"]);

    assert!(cleaned.status.success(), "clean failed: {}", stderr_of(&cleaned));
    assert!(stdout_of(&cleaned).contains("deleted project"), "unexpected output: {}", stdout_of(&cleaned));
    assert!(!state_dir.exists(), "the project's state directory must be gone");
}

/// Deleting the index out from under a live daemon would leave it serving a
/// database that no longer exists.
#[test]
fn clean_refuses_while_a_daemon_is_still_serving_the_project() {
    let project = Project::new();
    project.bootstrap_daemon();

    let cleaned = project.command(&["clean"]);

    assert!(!cleaned.status.success(), "clean must refuse a project that is being served");
    let stderr = stderr_of(&cleaned);
    assert!(stderr.contains("g-mesh stop"), "the error must say what to do: {stderr}");
    assert!(project.state_dir().is_dir(), "nothing may have been deleted");

    // ...and it works once the daemon is out of the way.
    let stopped = project.command(&["stop"]);
    assert!(stopped.status.success(), "stop failed: {}", stderr_of(&stopped));
    let cleaned_again = project.command(&["clean"]);
    assert!(cleaned_again.status.success(), "clean failed: {}", stderr_of(&cleaned_again));
    assert!(!project.state_dir().exists());
}

/// Per the requirements: an unrecognized cwd asks for an explicit id rather
/// than picking something arbitrary to delete.
#[test]
fn clean_in_a_never_indexed_directory_asks_for_an_explicit_project_id() {
    let project = Project::new();

    let cleaned = project.command(&["clean"]);

    assert!(!cleaned.status.success(), "there is nothing here to clean");
    let stderr = stderr_of(&cleaned);
    assert!(stderr.contains("pass an explicit <project-id>"), "the error must ask for an id: {stderr}");
}

/// `orphaned` end to end, in the form that cannot delete anything: the target
/// has to parse, reach `cli::clean`, and report. What it decides about each
/// directory is the unit tests' subject - here the point is that the word is
/// wired up at all, which no unit test can show.
#[test]
fn clean_orphaned_without_force_reports_and_deletes_nothing() {
    let project = Project::new();
    project.bootstrap_daemon();

    let reported = project.command(&["clean", "orphaned"]);

    assert!(reported.status.success(), "clean orphaned failed: {}", stderr_of(&reported));
    let stdout = stdout_of(&reported);
    assert!(
        stdout.contains("deleted") || stdout.contains("no project index"),
        "clean orphaned said nothing about what it found: {stdout}"
    );
    assert!(
        project.state_dir().is_dir(),
        "this project is still on disk - `clean orphaned` must not have touched its state"
    );
}

/// A project id that is really a path must never be joined onto the projects
/// root - checked through the real binary, not just the unit under it.
#[test]
fn clean_refuses_a_path_shaped_project_id() {
    let project = Project::new();

    for id in ["..", "../../etc", "/tmp"] {
        let cleaned = project.command(&["clean", id]);
        assert!(!cleaned.status.success(), "`clean {id}` must be refused");
        assert!(
            stderr_of(&cleaned).contains("is not a project id"),
            "`clean {id}` was refused for the wrong reason: {}",
            stderr_of(&cleaned)
        );
    }
}
