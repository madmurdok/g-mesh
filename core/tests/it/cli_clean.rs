//! `g-mesh clean` driven as a real subprocess, against a real project state
//! directory.
//!
//! Each test runs against a `G_MESH_HOME` of its own (`common::IsolatedHome`).
//! The ambient home is shared by every test binary and worktree, and any
//! process there that scans the projects root (`g-mesh status`, `clean
//! orphaned`) opens every project's `index.db`, creating files inside the
//! very state directory a test is deleting. That failed `clean` with
//! "Directory not empty".
//!
//! Only the forms scoped to this test's own project are driven here;
//! `clean expired`, `clean orphaned --force` and `clean all` live in
//! `cli_clean_sweeping.rs`. What is worth driving as a subprocess is the
//! wiring: that the word reaches the right code path and reports rather than
//! deletes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use g_mesh::daemon;
use g_mesh::daemon::identity::project_hash;
use g_mesh::paths::HOME_ENV;

use crate::common;

use crate::common::{IsolatedHome, Lifeline};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

struct Project {
    home: IsolatedHome,
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("a.ts"), b"export const a = 1;\n")
            .expect("failed to seed the project with a source file");
        Self { home: IsolatedHome::new(), dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Derived from the home rather than asked of `storage::connection`,
    /// which would answer for the *ambient* root this process inherited.
    fn state_dir(&self) -> PathBuf {
        self.home.projects_root().join(project_hash(self.root()).expect("failed to hash the project root"))
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path_in(&self.state_dir())
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path_in(&self.state_dir())
    }

    /// Bootstraps a detached daemon through the shim, so the state directory
    /// this test deletes is one a real daemon really built.
    fn bootstrap_daemon(&self) {
        let mut shim = Command::new(BIN)
            .lifeline()
            .arg("mcp-shim")
            .current_dir(self.root())
            .env(HOME_ENV, &self.home.dir)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");
        wait_for("the daemon to start listening", || self.pid_file().exists());
        let _ = shim.kill();
        let _ = shim.wait();
        assert!(
            self.pid_file().starts_with(&self.home.dir) && self.state_dir().starts_with(&self.home.dir),
            "the pid file and state directory must live under this test's own home"
        );
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root())
            .env(HOME_ENV, &self.home.dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `g-mesh {}`: {e}", args.join(" ")))
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            common::kill_pid_file(&path);
        }
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

/// GM-301: see `common::wait_for`'s doc comment for why this delegates
/// instead of polling against a file-local timeout constant.
fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Names what is left in a directory a `clean` failed to remove, so a
/// recurrence identifies the writer.
fn leftovers_of(dir: &Path) -> String {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut names: Vec<String> =
                entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            format!("{names:?}")
        }
        Err(e) => format!("unreadable: {e}"),
    }
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

    assert!(
        cleaned.status.success(),
        "clean failed: {} (left in {}: {})",
        stderr_of(&cleaned),
        state_dir.display(),
        leftovers_of(&state_dir)
    );
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
    assert!(
        cleaned_again.status.success(),
        "clean failed: {} (left in {}: {})",
        stderr_of(&cleaned_again),
        project.state_dir().display(),
        leftovers_of(&project.state_dir())
    );
    assert!(!project.state_dir().exists());
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

/// GM-399 slice 4 (D11): a front's state directory holds a socket, a pid
/// file and `index.phase`, and no `index.db`. `clean orphaned` and `clean`
/// must handle it like any other. A regression guard: no code changed to
/// make this pass.
#[test]
fn clean_and_clean_orphaned_handle_a_front_state_dir() {
    let project = Project::new();
    for repo in ["a/.git", "b/.git"] {
        std::fs::create_dir_all(project.root().join(repo)).expect("failed to create a candidate repo");
    }
    project.bootstrap_daemon();
    let state_dir = project.state_dir();
    wait_for("the fixture's daemon to be a front", || {
        daemon::read_phase_in(&state_dir).as_deref() == Some("front")
    });
    assert!(!state_dir.join("index.db").exists(), "a front has no index.db");

    let reported = project.command(&["clean", "orphaned"]);
    assert!(reported.status.success(), "clean orphaned failed: {}", stderr_of(&reported));
    assert!(state_dir.is_dir(), "`clean orphaned` must leave a live front's state alone");

    let stopped = project.command(&["stop"]);
    assert!(stopped.status.success(), "stop failed: {}", stderr_of(&stopped));
    let cleaned = project.command(&["clean"]);
    assert!(
        cleaned.status.success(),
        "clean failed: {} (left in {}: {})",
        stderr_of(&cleaned),
        state_dir.display(),
        leftovers_of(&state_dir)
    );
    assert!(!state_dir.exists(), "the front's state directory must be gone");
}
