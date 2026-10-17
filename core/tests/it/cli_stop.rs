//! `g-mesh stop` against a real, detached daemon.
//!
//! The daemon under test is bootstrapped the way a real one is - by running
//! `g-mesh mcp-shim` and letting it spawn one - rather than spawned directly
//! by the test. That is not incidental: a daemon spawned as this process's
//! own child would linger as an unreaped zombie after being signalled unless
//! something waits on it, and a zombie still answers `kill(pid, 0)`. The shim
//! reaps its daemons with a `daemon-reaper` thread while it lives (a folder
//! session's shim lives the whole session), and once the shim exits the
//! daemon is reparented to init - both as in real use.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

mod common;

use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

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
            .lifeline()
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

        // GM-395 slice 2: the daemon walks - and so spawns its plugin - only once a tool call asks.
        common::trigger_activation(self.root());
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

/// GM-301: delegates to [`common::wait_for`] with [`common::startup_timeout`]
/// rather than polling against a file-local constant - see that function's
/// doc comment for why. This file's own fixed 10s deadline was exactly what
/// flaked under load: `stop_clears_the_state_a_crashed_daemon_left_behind`
/// timed out waiting for the plugin to spawn with the machine at load
/// 169-298, nothing to do with `stop` itself being broken.
fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

/// The acceptance criterion: both processes gone, the socket released, and
/// nothing orphaned. Asserted immediately after the command returns, with no
/// polling - `stop` promises to return only once that is all true.
#[test]
fn stop_shuts_down_both_the_core_and_the_plugin() {
    let project = Project::new();
    let (core, plugin) = project.bootstrap_daemon();

    let output = project.stop();

    // Which *rung* stopped the core, not merely that it was named. GM-320: the
    // escalation to `SIGKILL` means this command goes on succeeding if the
    // polite rung quietly stops working, so without this assertion a daemon
    // that ignored every `SIGTERM` would pass this whole file - the verdict is
    // the only place that difference is visible. `core/tests/daemon_sigterm.rs`
    // makes the same claim directly against a raw signal; this one makes it
    // about what a user is told.
    assert!(
        output.contains(&format!("daemon core: pid {core} (terminated)")),
        "the core must stop when it is asked, not have to be killed:\n{output}"
    );
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

/// A folder of two Rust projects, `a` and `b`, served by one folder-mode
/// shim.
struct Folder {
    dir: tempfile::TempDir,
}

impl Folder {
    fn new() -> Self {
        let folder = Self { dir: tempfile::tempdir().expect("failed to create a temp folder") };
        for name in ["a", "b"] {
            let sub = folder.sub(name);
            std::fs::create_dir_all(sub.join("src")).expect("failed to create a project directory");
            std::fs::create_dir_all(sub.join(".git")).expect("failed to mark a project root");
            std::fs::write(
                sub.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            )
            .expect("failed to write a Cargo.toml");
            std::fs::write(sub.join("src/lib.rs"), format!("pub fn {name}() {{}}\n"))
                .expect("failed to write a source file");
        }
        folder
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        for dir in [self.root().to_path_buf(), self.sub("a"), self.sub("b")] {
            let Ok(state) = project_dir(&dir) else { continue };
            for path in [daemon::pid_path(&dir), daemon::plugin_pid_path(&dir)].into_iter().flatten() {
                common::kill_pid_file(&path);
            }
            for (_, plugin_pid) in daemon::registry::discovered_pid_files(&state) {
                common::kill_pid_file(&plugin_pid);
            }
            if let Ok(endpoint) = daemon::endpoint(&dir) {
                endpoint.clear_stale();
            }
            let _ = std::fs::remove_dir_all(state);
        }
    }
}

/// A folder-mode shim over `dir`, killed when the client drops it.
fn folder_shim(dir: &Path, plugins: &Path) -> rmcp::transport::TokioChildProcess {
    use rmcp::transport::ConfigureCommandExt;

    let dir = dir.to_path_buf();
    let plugins = plugins.to_path_buf();
    rmcp::transport::TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&dir)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by every daemon the shim bootstraps.
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", &plugins)
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim")
}

async fn select(client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>, project: &str) {
    let arguments = serde_json::json!({ "project": project }).as_object().cloned().unwrap();
    let result = client
        .call_tool(rmcp::model::CallToolRequestParams::new("select_project").with_arguments(arguments))
        .await
        .unwrap_or_else(|err| panic!("select_project {project} must return a result: {err}"));
    assert_ne!(result.is_error, Some(true), "the switch to {project} must succeed: {result:?}");
}

/// The pid `root`'s daemon wrote, waiting for it: the daemon writes its pid
/// file just after binding its endpoint, so a reachable daemon may not have
/// written it yet.
fn daemon_pid(root: &Path) -> u32 {
    let path = daemon::pid_path(root).expect("failed to resolve the pid file path");
    wait_for("the daemon to write its pid file", || daemon::read_pid_file(&path).is_some());
    read_pid(&path)
}

/// `g-mesh stop` in `dir`, asserting it exits 0.
fn stop_in(dir: &Path) {
    let output =
        Command::new(BIN).arg("stop").current_dir(dir).output().expect("failed to run `g-mesh stop`");
    assert!(
        output.status.success(),
        "`g-mesh stop` failed with {}: {}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// GM-534: a folder session's shim lives the whole session and stays the
/// parent of every daemon it bootstraps, so it must reap them. `a`'s daemon
/// is stopped while the shim that started it is still serving `b`: `stop`
/// exits 0 and the pid is gone (not a zombie), the same shim still answers,
/// and reselecting `a` bootstraps a fresh daemon that can be stopped too.
/// No order between processes is asserted beyond the test's own sequential
/// calls and `stop`'s own promise to return only once the pid is gone.
///
/// Control: in `shim::spawn_detached_daemon`, drop the `Child` instead of
/// passing it to `spawn_daemon_reaper` - the stopped daemon stays a zombie
/// child of the shim and `stop` exits 1 after its grace period.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_reaps_a_daemon_whose_folder_shim_is_still_running() {
    use rmcp::ServiceExt;

    let folder = Folder::new();
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    common::add_real_rust_plugin(plugins.path());
    let a = folder.sub("a");

    let client =
        ().serve(folder_shim(folder.root(), plugins.path())).await.expect("the shim must reach the front");

    select(&client, "a").await;
    let first = daemon_pid(&a);
    assert!(daemon::is_process_alive(first), "a's daemon must be running before it is stopped");
    select(&client, "b").await;

    let stopper = a.clone();
    tokio::task::spawn_blocking(move || stop_in(&stopper)).await.expect("the stop task panicked");
    assert!(!daemon::is_process_alive(first), "a's daemon (pid {first}) must be gone, not a zombie");
    assert!(!daemon::is_listening(&a).unwrap_or(true), "a's endpoint must be released");

    // The shim that bootstrapped the stopped daemon is still serving, and
    // bootstraps (and reaps) a fresh one for `a`.
    select(&client, "a").await;
    let second = daemon_pid(&a);
    assert_ne!(second, first, "reselecting a must bootstrap a new daemon");
    assert!(daemon::is_listening(&a).unwrap_or(false), "a's new daemon must be reachable");

    let stopper = a.clone();
    tokio::task::spawn_blocking(move || stop_in(&stopper)).await.expect("the stop task panicked");
    assert!(!daemon::is_process_alive(second), "a's second daemon (pid {second}) must be gone");

    client.cancel().await.expect("failed to shut the client down");
}
