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

mod common;

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

    /// The Unix socket *file*, for the three assertions below that are about
    /// the file rather than about the daemon. Unix-only on purpose: on
    /// Windows the endpoint is a pipe name with no filesystem presence, so
    /// there is no file whose existence could be asserted - see
    /// `g_mesh::ipc::windows`. The behavioural half of each of those
    /// assertions is checked through [`Project::is_listening`] on both
    /// platforms.
    #[cfg(unix)]
    fn socket(&self) -> PathBuf {
        daemon::socket_path(self.root()).expect("failed to resolve the daemon socket path")
    }

    /// Whether anything is answering on the project's endpoint - the
    /// platform-independent form of "the socket is (not) held".
    fn is_listening(&self) -> bool {
        daemon::is_listening(self.root()).expect("failed to probe the daemon endpoint")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    /// Bootstraps a detached daemon through the shim and returns its core
    /// pid, waiting only for the daemon's own pid file - not for any plugin.
    /// Under `daemon::registry::PluginRegistry`'s lazy per-language spawn, a
    /// plugin only comes up once something actually needs it (a fresh
    /// project's cold-start semantic pass, or a file changing under a
    /// running daemon); a daemon that starts against a project whose index
    /// is already complete legitimately spawns none at all, so waiting on a
    /// plugin pid file here would hang forever in exactly that case.
    fn bootstrap_core(&self) -> u32 {
        let mut shim = Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");

        wait_for("the daemon to bind its socket", || self.pid_file().exists());

        // The shim was only the vehicle: the daemon it spawned is detached
        // and outlives it, which is what the `stop` under test has to reach.
        let _ = shim.kill();
        let _ = shim.wait();

        let core = read_pid(&self.pid_file());
        assert!(daemon::is_process_alive(core), "the daemon must be running before it is stopped");
        core
    }

    /// Like [`bootstrap_core`](Self::bootstrap_core), but also waits for the
    /// bundled plugin to come up and returns its pid too - only valid
    /// against a project that still owes a bulk index (i.e. a fresh one),
    /// since that is the only startup path that spawns a plugin without a
    /// file change first landing on an already-running daemon (see
    /// `daemon::registry::PluginRegistry`'s lazy per-language spawn).
    fn bootstrap_daemon(&self) -> (u32, u32) {
        let core = self.bootstrap_core();

        wait_for("the daemon to spawn its plugin", || self.plugin_pid_file().exists());
        let plugin = read_pid(&self.plugin_pid_file());
        assert_ne!(core, plugin, "the plugin runs in a process of its own");
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
            common::kill_pid_file(&path);
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
    assert!(output.contains(&format!("plugin (typescript): pid {plugin}")), "{output}");
    assert!(!daemon::is_process_alive(core), "the daemon core (pid {core}) is still running after stop");
    assert!(!daemon::is_process_alive(plugin), "the plugin (pid {plugin}) was orphaned by stop");
    assert!(!project.is_listening(), "the daemon endpoint must be released");
    #[cfg(unix)]
    assert!(!project.socket().exists(), "the daemon socket file must be removed");
    assert!(!project.pid_file().exists(), "the daemon pid file must be cleared");
    assert!(!project.plugin_pid_file().exists(), "the plugin pid file must be cleared");
}

/// "Socket released" means the next daemon can have it - which is a stronger
/// claim than the file being gone, and the one that actually matters.
///
/// The second bootstrap is against a project whose index the first daemon
/// already completed, so under `daemon::registry::PluginRegistry`'s lazy
/// per-language spawn no plugin comes up on its own this time - only the
/// core does. That is the behavior this test exists to confirm still works,
/// not a gap in what it checks: asserting a plugin here would mean waiting
/// on something this daemon correctly never does without a file changing
/// under it first.
#[test]
fn a_stopped_project_can_be_bootstrapped_again_immediately() {
    let project = Project::new();
    let (first_core, _) = project.bootstrap_daemon();
    project.stop();

    let second_core = project.bootstrap_core();

    assert_ne!(second_core, first_core, "a genuinely new daemon must have taken over");
    assert!(daemon::is_process_alive(second_core));
    assert!(project.is_listening(), "the new daemon must have bound the endpoint");
}

#[test]
fn stopping_a_project_with_no_daemon_running_is_a_clean_no_op() {
    let project = Project::new();

    let output = project.stop();

    assert!(output.contains("no daemon is running"), "expected a no-op message, got:\n{output}");
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

    assert!(output.contains("no daemon is running"), "expected a no-op message, got:\n{output}");
}

/// A daemon that was killed rather than stopped leaves its pid files behind.
/// `stop` has to see through them - report nothing running, and clear them -
/// instead of trying to signal a pid that may since have been reused.
#[test]
fn stop_clears_the_state_a_crashed_daemon_left_behind() {
    let project = Project::new();
    let (core, plugin) = project.bootstrap_daemon();

    common::kill_and_wait(core);
    wait_for("the daemon to die", || !daemon::is_process_alive(core));
    // The plugin exits by itself when its core's end of its stdin closes.
    wait_for("the plugin to exit with its core", || !daemon::is_process_alive(plugin));
    assert!(project.pid_file().exists(), "a killed daemon leaves its pid file behind");

    let output = project.stop();

    assert!(output.contains("no daemon is running"), "expected a no-op message, got:\n{output}");
    assert!(!project.pid_file().exists(), "the stale daemon pid file must be cleared");
    assert!(!project.plugin_pid_file().exists(), "the stale plugin pid file must be cleared");
    #[cfg(unix)]
    assert!(!project.socket().exists(), "the stale socket file must be cleared");
}
