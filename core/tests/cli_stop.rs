//! `g-mesh stop` against a real, detached daemon.
//!
//! The daemon under test is bootstrapped the way a real one is - by running
//! `g-mesh mcp-shim` and letting it spawn one - rather than spawned directly
//! by the test. That is not incidental: a daemon spawned as this process's
//! own child would linger as an unreaped zombie after being signalled, and a
//! zombie still answers `kill(pid, 0)`, so the test would be asserting about
//! a process state that cannot occur in production. Bootstrapping through the
//! shim reparents the daemon to init, exactly as in real use.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    /// Bootstraps a detached daemon through the shim and returns the core and
    /// plugin pids it recorded.
    fn bootstrap_daemon(&self) -> (u32, u32) {
        let mut shim = Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");

        // The pid file is written once the daemon has bound its socket, so it
        // is the signal that the bootstrap is complete.
        wait_for("the daemon to start listening", || self.pid_file().exists());

        // The shim was only the vehicle: the daemon it spawned is detached
        // and outlives it, which is what the `stop` under test has to reach.
        let _ = shim.kill();
        let _ = shim.wait();

        let core = read_pid(&self.pid_file());
        let plugin = read_pid(&self.plugin_pid_file());
        assert_ne!(core, plugin, "the plugin runs in a process of its own");
        assert!(daemon::is_process_alive(core), "the daemon must be running before it is stopped");
        assert!(daemon::is_process_alive(plugin), "the plugin must be running before it is stopped");
        (core, plugin)
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
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            if let Ok(pid) = std::fs::read_to_string(&path) {
                let _ =
                    Command::new("kill").arg("-9").arg(pid.trim()).stderr(Stdio::null()).status();
            }
        }
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

/// The acceptance criterion: both processes gone, the socket released, and
/// nothing orphaned. Asserted immediately after the command returns, with no
/// polling - `stop` promises to return only once that is all true.
#[test]
fn stop_shuts_down_both_the_core_and_the_plugin() {
    let project = Project::new();
    let (core, plugin) = project.bootstrap_daemon();

    let output = project.stop();

    assert!(output.contains(&format!("daemon core: pid {core}")), "{output}");
    assert!(output.contains(&format!("plugin:      pid {plugin}")), "{output}");
    assert!(
        !daemon::is_process_alive(core),
        "the daemon core (pid {core}) is still running after stop"
    );
    assert!(
        !daemon::is_process_alive(plugin),
        "the plugin (pid {plugin}) was orphaned by stop"
    );
    assert!(!project.socket().exists(), "the daemon socket must be released");
    assert!(!project.pid_file().exists(), "the daemon pid file must be cleared");
    assert!(!project.plugin_pid_file().exists(), "the plugin pid file must be cleared");
}

/// "Socket released" means the next daemon can have it - which is a stronger
/// claim than the file being gone, and the one that actually matters.
#[test]
fn a_stopped_project_can_be_bootstrapped_again_immediately() {
    let project = Project::new();
    let (first_core, _) = project.bootstrap_daemon();
    project.stop();

    let (second_core, second_plugin) = project.bootstrap_daemon();

    assert_ne!(second_core, first_core, "a genuinely new daemon must have taken over");
    assert!(daemon::is_process_alive(second_core));
    assert!(daemon::is_process_alive(second_plugin));
    assert!(project.socket().exists(), "the new daemon must have bound the socket");
}

#[test]
fn stopping_a_project_with_no_daemon_running_is_a_clean_no_op() {
    let project = Project::new();

    let output = project.stop();

    assert!(
        output.contains("no daemon is running"),
        "expected a no-op message, got:\n{output}"
    );
    // The assertion that matters is the exit status, which `Project::stop`
    // already required to be a success.
}

/// Stopping twice in a row is the same no-op: the second call finds the state
/// files the first one cleared.
#[test]
fn stopping_an_already_stopped_project_is_a_clean_no_op() {
    let project = Project::new();
    project.bootstrap_daemon();
    project.stop();

    let output = project.stop();

    assert!(
        output.contains("no daemon is running"),
        "expected a no-op message, got:\n{output}"
    );
}

/// A daemon that was killed rather than stopped leaves its pid files behind.
/// `stop` has to see through them - report nothing running, and clear them -
/// instead of trying to signal a pid that may since have been reused.
#[test]
fn stop_clears_the_state_a_crashed_daemon_left_behind() {
    let project = Project::new();
    let (core, plugin) = project.bootstrap_daemon();

    Command::new("kill")
        .arg("-9")
        .arg(core.to_string())
        .status()
        .expect("failed to kill the daemon");
    wait_for("the daemon to die", || !daemon::is_process_alive(core));
    // The plugin exits by itself when its core's end of its stdin closes.
    wait_for("the plugin to exit with its core", || !daemon::is_process_alive(plugin));
    assert!(project.pid_file().exists(), "a killed daemon leaves its pid file behind");

    let output = project.stop();

    assert!(
        output.contains("no daemon is running"),
        "expected a no-op message, got:\n{output}"
    );
    assert!(!project.pid_file().exists(), "the stale daemon pid file must be cleared");
    assert!(!project.plugin_pid_file().exists(), "the stale plugin pid file must be cleared");
    assert!(!project.socket().exists(), "the stale socket file must be cleared");
}
