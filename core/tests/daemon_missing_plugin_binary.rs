//! GM-316: a discovered plugin whose `command` is a cargo-workspace binary
//! that was never built must fail daemon startup with a message naming the
//! binary, the fact it was never built, and `cargo build --workspace` - not
//! the bare `No such file or directory (os error 2)` `Command::spawn`
//! reports on its own.
//!
//! Traced while verifying GM-301: a worktree where `cargo build --workspace`
//! had not been run failed `cargo test -p g-mesh --test cli_stop` with
//! "timed out waiting for the daemon to spawn its plugin within 60s" at load
//! 30, which reads exactly like a regression in the work under review. The
//! real cause was in the daemon log, not the test failure: `daemon::
//! bulk_index::walk_one_language` spawns every *discovered* plugin's one-shot
//! `--bulk-index` binary unconditionally (see that module's own doc comment),
//! `cargo test -p g-mesh` never builds sibling workspace members, and
//! `plugins/python/plugin.toml`'s bundled `command` names a `target/debug/
//! g-mesh-plugin-python` that therefore did not exist. Every test waiting for
//! a plugin pid timed out naming the daemon, because the daemon's cold-start
//! walk had already failed and nothing had spawned.
//!
//! This exercises the real `g-mesh daemon` binary against a fixture plugin
//! whose `command` points at a `target/debug/` binary that is deliberately
//! never created - the same shape as the real bug, without needing an actual
//! cargo build to reproduce it - the same approach
//! `daemon_plugin_discovery_failure.rs` already takes for a different
//! `discover()`-time failure.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION;
use g_mesh::storage::connection::project_dir;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

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
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// A discovery root with one plugin - "python", matching the real bundled
/// plugin's directory name - whose `command` names a `target/debug/`
/// binary that is never created. A `Cargo.toml` sits beside it so the fixed
/// message's "Run `cargo build --workspace` in <root>" branch has a real
/// workspace root to name, exactly like the real repository root does for
/// the genuine bundled plugin.
fn missing_workspace_binary_plugin_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("failed to create a plugin discovery root");
    std::fs::write(root.path().join("Cargo.toml"), "[workspace]\nmembers = []\n")
        .expect("failed to write a fixture Cargo.toml");

    let dir = root.path().join("python");
    std::fs::create_dir_all(&dir).expect("failed to create a fixture plugin directory");

    let binary = root.path().join("target").join("debug").join("g-mesh-plugin-python");
    // Deliberately not created - this is the "never built" case.

    let manifest = format!(
        r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "{command}"

[plugin.languages]
extensions = [".py"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
        // TOML string: escape backslashes for a Windows path.
        command = binary.display().to_string().replace('\\', "\\\\"),
    );
    std::fs::write(dir.join("plugin.toml"), manifest).expect("failed to write a fixture plugin.toml");

    (root, binary)
}

#[test]
fn a_missing_workspace_built_plugin_binary_fails_daemon_startup_naming_the_build_command() {
    let project = Project::new();
    let (plugin_root, binary) = missing_workspace_binary_plugin_root();

    let mut daemon = Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugin_root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the daemon");

    let status = {
        let timeout = common::startup_timeout();
        let deadline = Instant::now() + timeout;
        loop {
            match daemon.try_wait().expect("failed to poll the daemon") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = daemon.kill();
                    panic!(
                        "a daemon whose bulk index must hard-fail on a missing plugin binary did \
                         not exit within {timeout:?} - it may be waiting on the plugin it can never \
                         spawn instead of failing fast"
                    );
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    };

    assert!(
        !status.success(),
        "a missing workspace-built plugin binary must fail daemon startup, not exit cleanly: {status}"
    );

    let mut stderr = String::new();
    use std::io::Read;
    daemon.stderr.take().unwrap().read_to_string(&mut stderr).expect("failed to read the daemon's stderr");

    assert!(
        stderr.contains(&binary.display().to_string()),
        "the error must name the missing binary's path: {stderr}"
    );
    assert!(
        stderr.contains("has not been built yet"),
        "the error must say the binary was never built, not just that spawn failed: {stderr}"
    );
    assert!(stderr.contains("cargo build --workspace"), "the error must name the fix: {stderr}");
    assert!(
        !stderr.contains("os error 2"),
        "the raw OS error must not be the only thing surfaced - the hint should replace it, not \
         merely accompany it: {stderr}"
    );

    assert!(
        !daemon::is_listening(project.root()).unwrap(),
        "a daemon that failed to start must not be answering connections"
    );
}
