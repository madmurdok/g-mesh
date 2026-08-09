//! A `discover()` failure at daemon startup - task 155's acceptance
//! criterion for wiring `daemon::manifest::discover` into `daemon::run`.
//!
//! Two plugins claiming the same file extension is a hard daemon-startup
//! failure per `docs/architecture/plugin-modularity.md`'s Failure Modes
//! section ("hard-fail daemon startup, naming the manifest path and the
//! specific problem"), the same "protocol is code, a mismatch is a hard load
//! failure" philosophy `protocol::handshake::verify` already applies to a
//! live handshake. This exercises the real `g-mesh daemon` binary against a
//! `G_MESH_PLUGIN_ROOTS_OVERRIDE` fixture - the override
//! `daemon::manifest::default_roots` documents specifically so a test never
//! has to touch the real `~/.g-mesh/plugins/` - rather than calling
//! `discover()` directly, because what matters here is that `daemon::run`
//! actually propagates the failure: exits with a clear error rather than
//! panicking or limping into a partially-started state that still binds a
//! socket.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION;
use g_mesh::storage::connection::project_dir;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal, otherwise-well-formed `plugin.toml` claiming `.foo` - the
/// fields `daemon::manifest::read_manifest` requires, nothing else.
fn manifest_toml(language: &str) -> String {
    format!(
        r#"
[plugin]
language = "{language}"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "true"

[plugin.languages]
extensions = [".foo"]
"#,
        version = CURRENT_PROTOCOL_VERSION,
    )
}

/// A discovery root with two plugin directories - `alpha` and `beta` - both
/// declaring `.foo`, which is exactly the conflict `discover()` hard-fails
/// on.
fn conflicting_plugin_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a plugin discovery root");
    for language in ["alpha", "beta"] {
        let dir = root.path().join(language);
        std::fs::create_dir_all(&dir).expect("failed to create a fixture plugin directory");
        std::fs::write(dir.join("plugin.toml"), manifest_toml(language))
            .expect("failed to write a fixture plugin.toml");
    }
    root
}

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

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn two_plugins_claiming_the_same_extension_fails_daemon_startup_with_a_clear_error() {
    let project = Project::new();
    let plugin_root = conflicting_plugin_root();

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
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match daemon.try_wait().expect("failed to poll the daemon") {
                Some(status) => break status,
                None if Instant::now() >= deadline => {
                    let _ = daemon.kill();
                    panic!(
                        "a daemon whose plugin discovery must hard-fail did not exit within \
                         {TIMEOUT:?} - it may have limped into a partially-started state instead"
                    );
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    };

    assert!(
        !status.success(),
        "a discover() conflict must fail daemon startup, not exit cleanly: {status}"
    );

    let mut stderr = String::new();
    use std::io::Read;
    daemon.stderr.take().unwrap().read_to_string(&mut stderr).expect("failed to read the daemon's stderr");
    assert!(stderr.contains("alpha"), "the error must name both languages: {stderr}");
    assert!(stderr.contains("beta"), "the error must name both languages: {stderr}");
    assert!(stderr.contains(".foo"), "the error must name the conflicting extension: {stderr}");
    assert!(
        stderr.contains("plugin.toml"),
        "the error must name the manifest path(s), per the architecture doc's Failure Modes \
         section: {stderr}"
    );

    // Nothing is left accepting connections: `daemon::run` binds its socket
    // deliberately early (ahead of even the plugin spawn, so a shim's
    // bootstrap race is decided by the bind alone - see that function's own
    // doc comment), the same ordering a plugin spawn failure already lived
    // with before this task, so a `discover()` failure this much later can
    // leave a socket *file* behind - what matters is that the exited process
    // is not still serving off it.
    assert!(
        !daemon::is_listening(project.root()).unwrap(),
        "a daemon that failed to start must not be answering connections"
    );
}

/// The healthy counterpart, pinned in the same file so the failure above
/// cannot be satisfied by a discovery root the daemon simply never reads:
/// one language, no conflict, and `G_MESH_PLUGIN_ROOTS_OVERRIDE` is doing
/// something real, since a daemon that ignored it would also "pass" this by
/// finding the real bundled plugin instead.
#[test]
fn a_single_discovered_language_with_no_conflict_starts_the_daemon_normally() {
    let project = Project::new();
    let root = tempfile::tempdir().expect("failed to create a plugin discovery root");
    let dir = root.path().join("alpha");
    std::fs::create_dir_all(&dir).expect("failed to create a fixture plugin directory");
    std::fs::write(dir.join("plugin.toml"), manifest_toml("alpha"))
        .expect("failed to write a fixture plugin.toml");

    let mut daemon = Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon");

    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening despite an override with no conflict", || pid_file.exists());

    assert!(
        daemon.try_wait().expect("failed to poll the daemon").is_none(),
        "the daemon must still be running, not have exited"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}
