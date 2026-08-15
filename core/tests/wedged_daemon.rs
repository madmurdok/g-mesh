//! A daemon that is alive, holds its project's singleton lock, and serves
//! nothing - task 184.
//!
//! # How the state is forced
//!
//! Not by waiting for a leaked daemon to turn up: the whole difficulty of this
//! bug in the field was that it needed a daemon to reach its idle shutdown and
//! then fail to finish it, which is neither quick nor reliable to provoke. It
//! does not have to be. What made the project unreachable was never *how* the
//! daemon got there, only the shape it was left in - the socket and pid files
//! its own shutdown had already deleted (`daemon::lifecycle
//! ::release_state_files`), with the process still alive and still holding
//! `daemon.lock`. So these tests bootstrap a perfectly ordinary daemon and
//! then delete exactly those files out from under it, which reproduces that
//! shape exactly and in a fraction of a second.
//!
//! That is also why the daemon here dies on `SIGTERM` where the leaked ones in
//! the field reportedly did not: nothing in the daemon installs a signal
//! handler, so a process that ignores `SIGTERM` is not a state this codebase
//! can produce, and none of the recovery below depends on the signal being
//! ignored - `cli::stop::terminate` escalates to `SIGKILL` either way.
//!
//! # What is asserted
//!
//! That such a daemon can no longer keep a project to itself: it is visible to
//! `status` and to `stop` by pid, a shim bootstrapping over it clears it and
//! serves the project rather than timing out, and a daemon started by hand
//! against it fails fast saying which pid is in the way instead of exiting
//! silently. And, on the other side of the same line, that a daemon which *is*
//! serving is never evicted by any of it - the singleton guarantee the lock
//! exists for.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::cli::status::{self, CoreState};
use g_mesh::daemon::{self, DaemonLock};
use g_mesh::storage::connection::project_dir;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
/// Deadline for anything these tests wait on - a hang guard, not a timing
/// assertion.
const TIMEOUT: Duration = Duration::from_secs(20);

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

    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    /// Bootstraps a detached daemon through the shim - the same vehicle
    /// `cli_stop.rs` uses, and for its reason: a daemon spawned as this
    /// process's own child would linger as a zombie that still answers
    /// `kill(pid, 0)` once it is signalled, which is not a state production
    /// can reach.
    fn bootstrap_core(&self) -> u32 {
        let mut shim = self.spawn_shim();
        wait_for("the daemon to bind its socket", || self.pid_file().exists());
        let _ = shim.kill();
        let _ = shim.wait();

        let core = read_pid(&self.pid_file());
        assert!(
            daemon::is_process_alive(core),
            "the daemon must be up before it is wedged"
        );
        core
    }

    fn spawn_shim(&self) -> std::process::Child {
        Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the shim")
    }

    /// Leaves this project in the wedged state and returns the pid holding it:
    /// a live daemon, still holding `daemon.lock`, with every file that
    /// describes a running daemon already gone. Exactly what a daemon that
    /// began its idle shutdown and never finished leaves behind - see this
    /// module's header.
    fn wedge(&self) -> u32 {
        let pid = self.bootstrap_core();

        for path in [
            self.socket(),
            self.pid_file(),
            daemon::build_stamp_path(self.root()).unwrap(),
        ] {
            let _ = std::fs::remove_file(&path);
        }

        assert!(
            daemon::is_process_alive(pid),
            "the wedged daemon must still be alive"
        );
        assert!(
            !daemon::is_listening(self.root()).unwrap(),
            "the wedged daemon must be unreachable - that is what makes it a wedge"
        );
        assert_eq!(
            daemon::inspect_daemon_lock(self.root()).unwrap(),
            DaemonLock::Wedged { pid },
            "the lock must name the process holding this project hostage"
        );
        pid
    }

    fn stop(&self) -> String {
        let output = Command::new(BIN)
            .arg("stop")
            .current_dir(self.root())
            .output()
            .expect("failed to run `g-mesh stop`");
        assert!(
            output.status.success(),
            "`g-mesh stop` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("stop output is not valid UTF-8")
    }
}

impl Drop for Project {
    /// Nothing may outlive one of these tests: a leaked daemon holding a lock
    /// is the very failure under test, and one left behind here would go on to
    /// break unrelated suites. `stop` handles both the healthy and the wedged
    /// shape now, which is itself part of what is being asserted.
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("stop")
            .current_dir(self.root())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

fn read_pid(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
        .trim()
        .parse()
        .expect("pid file does not contain a pid")
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

/// `status` used to answer "not running" here, because it reads `daemon.pid`
/// and the wedged daemon deleted its own. The lock is the fact that survives
/// that, and the one the next bootstrap acts on, so this is what `status` has
/// to agree with.
#[test]
fn status_names_the_wedged_daemon_instead_of_reporting_nothing_running() {
    let project = Project::new();
    let pid = project.wedge();

    let report = status::collect(project.root()).expect("status must not need a healthy daemon");

    assert_eq!(report.core, CoreState::Wedged { pid });
    let rendered = status::render(&report);
    assert!(
        rendered.contains(&format!("wedged (pid {pid})")),
        "{rendered}"
    );
    assert!(
        rendered.contains("g-mesh stop"),
        "the report must say what to do:\n{rendered}"
    );
}

/// `stop` used to report "no daemon is running" while the daemon it could not
/// see held the project. Now it finds it through the lock and stops it - and
/// the project is immediately bootstrappable again, which is the outcome that
/// actually matters.
#[test]
fn stop_clears_a_wedged_daemon_that_no_pid_file_names() {
    let project = Project::new();
    let pid = project.wedge();

    let output = project.stop();

    assert!(
        output.contains(&format!("daemon core: pid {pid}")),
        "{output}"
    );
    assert!(
        output.contains("still holding this project's daemon lock"),
        "stop must explain why nothing could reach it:\n{output}"
    );
    assert!(
        !daemon::is_process_alive(pid),
        "the wedged daemon (pid {pid}) is still running"
    );
    assert_eq!(
        daemon::inspect_daemon_lock(project.root()).unwrap(),
        DaemonLock::Free
    );

    let replacement = project.bootstrap_core();
    assert_ne!(
        replacement, pid,
        "a genuinely new daemon must be serving the project"
    );
}

/// The end-to-end acceptance criterion: an MCP client's shim, arriving at a
/// wedged project, gets a working daemon instead of waiting out its whole
/// bootstrap timeout on a socket that would never appear.
#[test]
fn a_shim_bootstrap_recovers_a_wedged_project_rather_than_timing_out() {
    let project = Project::new();
    let wedged = project.wedge();

    let mut shim = project.spawn_shim();
    wait_for("the shim to bring a working daemon up", || {
        project.pid_file().exists() && daemon::is_listening(project.root()).unwrap_or(false)
    });
    let replacement = read_pid(&project.pid_file());
    let _ = shim.kill();
    let _ = shim.wait();

    assert_ne!(replacement, wedged, "the replacement must be a new process");
    assert!(daemon::is_process_alive(replacement));
    assert!(
        !daemon::is_process_alive(wedged),
        "the wedged daemon (pid {wedged}) must have been cleared, not left holding the lock"
    );
    assert_eq!(
        daemon::inspect_daemon_lock(project.root()).unwrap(),
        DaemonLock::Serving
    );
}

/// The other half of that line, and the one the singleton lock exists for: a
/// daemon that is *answering* is never a candidate for eviction, however many
/// shims arrive. A second shim reuses it; nothing is signalled, nothing is
/// replaced, and there is still exactly one daemon at the end.
#[test]
fn a_serving_daemon_is_reused_by_a_second_shim_and_never_evicted() {
    let project = Project::new();
    let incumbent = project.bootstrap_core();

    let mut second = project.spawn_shim();
    thread::sleep(Duration::from_millis(500));
    let _ = second.kill();
    let _ = second.wait();

    assert!(
        daemon::is_process_alive(incumbent),
        "a serving daemon must survive another shim's arrival"
    );
    assert_eq!(
        read_pid(&project.pid_file()),
        incumbent,
        "no replacement may have taken over"
    );
    assert_eq!(
        daemon::inspect_daemon_lock(project.root()).unwrap(),
        DaemonLock::Serving
    );
}

/// A daemon started by hand against a wedged project must not take over - the
/// lock still excludes it, exactly as it excludes a second daemon from a
/// healthy one - but it must say why it is giving up, and name the pid, rather
/// than exiting silently with "another daemon already serves this project".
#[test]
fn a_daemon_started_over_a_wedged_one_fails_fast_and_names_the_pid() {
    let project = Project::new();
    let wedged = project.wedge();

    let output = Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .output()
        .expect("failed to run the daemon");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "taking over a wedged project must be a failure:\n{stderr}"
    );
    assert!(stderr.contains(&format!("pid {wedged}")), "{stderr}");
    assert!(
        stderr.contains("g-mesh stop"),
        "the diagnostic must say what to do:\n{stderr}"
    );
    assert!(
        daemon::is_process_alive(wedged),
        "the singleton lock must still exclude the newcomer - a daemon may not evict its own way in"
    );
}
