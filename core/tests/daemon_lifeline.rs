//! A daemon started with `G_MESH_LIFELINE_PID` shuts itself down once
//! the process that pid names is gone, so a test run that dies without its
//! teardown (SIGKILL, a nextest timeout, ctrl-c) does not leave its daemons -
//! and their plugins - running.
//!
//! The lifeline here is a helper process this test owns, never the test
//! process itself: the test has to be able to take the lifeline away and then
//! watch what the daemon does about it. `orphan_check`'s verdicts are
//! unit-tested in `daemon::lifecycle`; these tests are for the verdict being
//! reached on the supervisor's tick, in a real daemon, and acted on.
//!
//! # Telling a lifeline exit from an idle exit
//!
//! The plugin timeout is 2s only so the tick is 500ms; the core's timeout is
//! ten minutes, far beyond any wait here, so the core going away cannot be
//! its idle timer. The daemon's stderr is also captured, and the exit test
//! asserts the line it logged names the lifeline and is not the idle one.
//!
//! # The control arm
//!
//! [`a_daemon_whose_lifeline_is_running_or_unreadable_is_left_alone`] is the
//! same daemon and the same timeouts with the lifeline left running (and a
//! second daemon with a lifeline that does not parse). An exit for any other
//! reason - a supervisor that exits on every tick, a lifeline read as dead
//! whatever it is - shows up there.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::daemon::lifecycle::{CORE_IDLE_ENV, LIFELINE_PID_ENV, PLUGIN_IDLE_ENV};
use g_mesh::storage::connection::project_dir;

mod common;

use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Sets the supervisor's tick to 500ms (a quarter of the shorter timeout).
const PLUGIN_IDLE: Duration = Duration::from_millis(2_000);

/// Far beyond every wait in this file, so the core never idles out here.
const CORE_IDLE_EFFECTIVELY_NEVER: Duration = Duration::from_secs(600);

/// How long the control arm watches a daemon with a live (or unreadable)
/// lifeline decline to stop: ten 500ms ticks, against the one tick a daemon
/// whose lifeline is gone is allowed.
const CONTROL_WATCH: Duration = Duration::from_secs(5);

/// A process that runs until this test stops it, to hand a daemon as its
/// lifeline. Killed and reaped on drop, if the test has not already.
struct Helper {
    child: Child,
}

impl Helper {
    fn spawn() -> Self {
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sleep");
            command.arg("600");
            command
        };
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("ping");
            command.args(["-n", "600", "127.0.0.1"]);
            command
        };
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the lifeline helper");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Ends the helper and reaps it, so its pid names nothing any more - the
    /// state a test process is in once it has died.
    fn end(&mut self) {
        let _ = self.child.kill();
        self.child.wait().expect("failed to reap the lifeline helper");
        assert!(!daemon::is_process_alive(self.pid()), "the reaped helper must read as gone");
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Project {
    _dir: tempfile::TempDir,
    state_dir: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        let root = dir.path().to_path_buf();
        let state_dir = project_dir(&root).expect("failed to resolve the state directory");
        let source = root.join("src/alpha.ts");
        std::fs::create_dir_all(source.parent().unwrap()).expect("failed to create a fixture directory");
        std::fs::write(&source, "export function alpha(): number {\n  return 1;\n}\n")
            .expect("failed to write a fixture file");
        Self { _dir: dir, state_dir, root }
    }

    fn core_pid_file(&self) -> PathBuf {
        daemon::pid_path_in(&self.state_dir)
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path_in(&self.state_dir)
    }

    /// Everything that describes a running daemon; all of it must be gone
    /// once the daemon has stopped itself.
    fn state_files(&self) -> Vec<PathBuf> {
        #[allow(unused_mut)]
        let mut files =
            vec![self.core_pid_file(), self.plugin_pid_file(), daemon::build_stamp_path_in(&self.state_dir)];
        #[cfg(unix)]
        files.push(self.state_dir.join("daemon.sock"));
        files
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // A failed test must not leave its daemon or plugin running.
        for (_, path) in daemon::registry::discovered_pid_files(&self.state_dir) {
            common::kill_pid_file(&path);
        }
        common::kill_pid_file(&self.core_pid_file());
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

/// A daemon child with `lifeline` as its `G_MESH_LIFELINE_PID` and its stderr
/// (where `log_line!` writes) captured to `log`.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn spawn(root: &Path, lifeline: &str, log: &Path) -> Self {
        let stderr = File::create(log).expect("failed to create the daemon's log file");
        let child = Command::new(BIN)
            .arg("daemon")
            .arg("--project-root")
            .arg(root)
            .env(LIFELINE_PID_ENV, lifeline)
            .env(PLUGIN_IDLE_ENV, PLUGIN_IDLE.as_millis().to_string())
            .env(CORE_IDLE_ENV, CORE_IDLE_EFFECTIVELY_NEVER.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("failed to spawn the daemon");
        Self { child }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("failed to check on the daemon").is_none()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn recorded_pid(path: &Path) -> u32 {
    daemon::read_pid_file(path).unwrap_or_else(|| panic!("no pid recorded in {}", path.display()))
}

/// Indexed, answering, and with its plugin running: the steady state a test's
/// daemon is in when its test process dies.
fn wait_until_fully_up(project: &Project) -> u32 {
    wait_until_indexed(&project.root);
    common::wait_for("the plugin to start", common::startup_timeout(), || project.plugin_pid_file().exists());
    recorded_pid(&project.plugin_pid_file())
}

/// The process a daemon was tied to is gone, and the
/// daemon - with its plugin - goes too, releasing every file that described it.
#[test]
fn a_daemon_whose_lifeline_exits_stops_itself() {
    let project = Project::new();
    let logs = tempfile::tempdir().expect("failed to create a log directory");
    let log = logs.path().join("daemon.log");
    let mut helper = Helper::spawn();
    let lifeline = helper.pid();

    let mut daemon_process = Daemon::spawn(&project.root, &lifeline.to_string(), &log);
    let plugin_pid = wait_until_fully_up(&project);
    assert!(daemon_process.is_running(), "the daemon must be up before its lifeline is taken away");

    helper.end();

    common::wait_for("the daemon to stop after its lifeline exited", common::startup_timeout(), || {
        !daemon_process.is_running()
    });
    let status = daemon_process.child.wait().expect("failed to reap the daemon");
    assert!(status.success(), "a lifeline shutdown is a clean exit, not a failure: {status}");

    common::wait_for("the plugin to go with its core", common::startup_timeout(), || {
        !daemon::is_process_alive(plugin_pid)
    });
    for leftover in project.state_files() {
        assert!(!leftover.exists(), "a daemon that stopped itself must release {}", leftover.display());
    }

    let logged = std::fs::read_to_string(&log).expect("failed to read the daemon's log");
    assert!(
        logged.contains(&format!("lifeline process {lifeline}")),
        "the daemon must say it stopped because its lifeline {lifeline} is gone; it logged:\n{logged}"
    );
    assert!(
        !logged.contains("no MCP requests for"),
        "the exit must not be the core's idle timeout; it logged:\n{logged}"
    );
}

/// The control. Same timeouts, same wait, and the lifeline left running - plus
/// a second daemon whose lifeline does not parse as a pid, which must read as
/// no lifeline at all rather than as a dead one. Both watched for ten ticks.
#[test]
fn a_daemon_whose_lifeline_is_running_or_unreadable_is_left_alone() {
    let logs = tempfile::tempdir().expect("failed to create a log directory");
    let helper = Helper::spawn();

    let live = Project::new();
    let mut live_daemon = Daemon::spawn(&live.root, &helper.pid().to_string(), &logs.path().join("live.log"));
    let garbage = Project::new();
    let mut garbage_daemon = Daemon::spawn(&garbage.root, "not-a-pid", &logs.path().join("garbage.log"));
    wait_until_fully_up(&live);
    wait_until_fully_up(&garbage);

    std::thread::sleep(CONTROL_WATCH);

    for (name, project, daemon_process) in [
        ("a live lifeline", &live, &mut live_daemon),
        ("an unparseable lifeline", &garbage, &mut garbage_daemon),
    ] {
        assert!(daemon_process.is_running(), "a daemon with {name} must not stop itself");
        assert!(
            daemon::is_listening(&project.root).expect("the endpoint must be resolvable"),
            "a daemon with {name} must still be serving its endpoint"
        );
        assert_eq!(recorded_pid(&project.core_pid_file()), daemon_process.child.id(), "{name}: same process");
    }
}

/// A shim passes its environment to the daemon it detaches, so a daemon a
/// test reached through `mcp-shim` is tied to the same lifeline - and outlives
/// the shim itself, which is exactly the daemon nothing else would ever stop.
#[test]
fn a_daemon_detached_by_a_shim_stops_once_the_shims_lifeline_exits() {
    let project = Project::new();
    let mut helper = Helper::spawn();

    let mut shim = Command::new(BIN)
        .arg("mcp-shim")
        .current_dir(&project.root)
        .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
        .env(LIFELINE_PID_ENV, helper.pid().to_string())
        .env(PLUGIN_IDLE_ENV, PLUGIN_IDLE.as_millis().to_string())
        .env(CORE_IDLE_ENV, CORE_IDLE_EFFECTIVELY_NEVER.as_millis().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the shim");

    common::wait_for("the shim to bootstrap a daemon", common::startup_timeout(), || {
        daemon::is_listening(&project.root).unwrap_or(false) && project.core_pid_file().exists()
    });
    let daemon_pid = recorded_pid(&project.core_pid_file());

    // Closing stdin ends the shim; the daemon it detached stays.
    drop(shim.stdin.take());
    let _ = shim.wait();
    assert!(daemon::is_process_alive(daemon_pid), "the detached daemon must outlive its shim");

    helper.end();

    common::wait_for(
        "the detached daemon to stop after its lifeline exited",
        common::startup_timeout(),
        || !daemon::is_process_alive(daemon_pid),
    );
    common::wait_for("the detached daemon to release its pid file", common::startup_timeout(), || {
        !project.core_pid_file().exists()
    });
}
