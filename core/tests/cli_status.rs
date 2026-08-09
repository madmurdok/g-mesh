//! `g-mesh status` against a real daemon, in a project whose state the test
//! arranged on purpose: one file with a deliberate syntax error, and later a
//! file the index has never seen.
//!
//! The command is driven as a subprocess with the project as its cwd - the
//! same way a person runs it - so what is asserted is the text a human reads,
//! not an internal struct.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::config::{self, CleanupConfig, GlobalConfig};
use g_mesh::daemon;
use g_mesh::daemon::{manifest, registry};
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rusqlite::Connection;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

/// Serializes the two tests below that rewrite the real
/// `~/.g-mesh/config.toml`: with no override hook to point `config::mod` at
/// a scratch directory, two threads flipping `cleanup.enabled` at the same
/// time would race on that one shared file.
static GLOBAL_CONFIG_LOCK: Mutex<()> = Mutex::new(());

/// Temporarily replaces `~/.g-mesh/config.toml` with a known value, and
/// restores whatever (if anything) was there before it on drop - so a test
/// exercising `cleanup.enabled` never leaves the developer's real global
/// config mutated.
struct GlobalConfigGuard {
    path: PathBuf,
    original: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl GlobalConfigGuard {
    fn set(cfg: &GlobalConfig) -> Self {
        let lock = GLOBAL_CONFIG_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let path =
            config::global_config_path().expect("failed to resolve the global config path");
        let original = std::fs::read_to_string(&path).ok();
        config::write_global_config(cfg).expect("failed to write the global config");
        Self { path, original, _lock: lock }
    }
}

impl Drop for GlobalConfigGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(contents) => {
                let _ = std::fs::write(&self.path, contents);
            }
            None => {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// A project with a known, deliberately mixed index state.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("good.ts"), b"export function good() { return 1; }\n")
            .expect("failed to write good.ts");
        // Deliberately unparseable: the plugin still emits a File node for it,
        // flagged, which is exactly the state `status` has to surface.
        std::fs::write(dir.path().join("broken.ts"), b"export function broken( {\n")
            .expect("failed to write broken.ts");
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

    fn recorded_pid(&self, path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }

    fn status(&self) -> String {
        let output = Command::new(BIN)
            .arg("status")
            .current_dir(self.root())
            .output()
            .expect("failed to run `g-mesh status`");
        assert!(
            output.status.success(),
            "`g-mesh status` failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("status output is not valid UTF-8")
    }

    /// Builds this project's index with no daemon involved, and backdates
    /// `lastUsed` by `idle_days` - the on-disk state a project idle past the
    /// GC warning's threshold would have.
    fn backdate_last_used(&self, idle_days: u64) {
        std::fs::create_dir_all(self.state_dir()).expect("failed to create the state directory");
        let conn = Connection::open(self.state_dir().join("index.db"))
            .expect("failed to open index.db");
        // The same generation a daemon would stamp this index with (see
        // `daemon::registry::indexer_version`), computed the same way from the
        // plugins actually installed - so a daemon started against this
        // project afterwards finds it current and leaves the backdated
        // `lastUsed` this fixture is about alone, instead of wiping it.
        let discovered = manifest::discover(&manifest::default_roots())
            .expect("failed to discover language plugins");
        schema::ensure_current(&conn, &registry::indexer_version(&discovered))
            .expect("failed to initialize the schema");
        conn.execute(
            "UPDATE meta SET lastUsed = datetime('now', ?1) WHERE id = 1",
            rusqlite::params![format!("-{idle_days} days")],
        )
        .expect("failed to backdate lastUsed");
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

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
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

fn assert_contains(haystack: &str, needle: &str) {
    assert!(haystack.contains(needle), "expected `{needle}` in status output:\n{haystack}");
}

#[test]
fn status_reports_the_daemon_plugin_coverage_and_syntax_errors_of_a_live_project() {
    let project = Project::new();
    let mut daemon_process = spawn_daemon(project.root());
    // The daemon's pid file now appears at the socket bind, which is ahead of
    // both the plugin spawn and the cold-start walk (task 105) - and this test
    // asserts on the plugin's pid and on complete coverage, so it has to wait
    // for the walk's own completion marker instead.
    wait_until_indexed(project.root());
    // The registry spawns the bundled (typescript) plugin lazily - here, as a
    // side effect of the post-walk semantic pass `daemon::run` runs once the
    // cold-start walk commits - rather than unconditionally at daemon
    // startup, so its pid file can appear a moment *after* the walk's own
    // completion marker does, not atomically with it.
    wait_for("the typescript plugin to start", || project.plugin_pid_file().exists());

    let core_pid = project.recorded_pid(&project.pid_file());
    let plugin_pid = project.recorded_pid(&project.plugin_pid_file());
    assert_eq!(core_pid, daemon_process.id(), "the pid file must name the daemon we started");
    assert_ne!(plugin_pid, core_pid, "the plugin runs in a process of its own");

    let status = project.status();

    assert_contains(&status, &format!("daemon core:     running (pid {core_pid})"));
    assert_contains(&status, &format!("plugin (typescript):     active (pid {plugin_pid})"));
    // Both files were walked, and neither has been edited since.
    assert_contains(&status, "index coverage:  100.0% (2/2 source files)");
    assert_contains(&status, "dirty files:     0 awaiting reindex");
    assert_contains(&status, "syntax errors:   1 file(s)");
    assert_contains(&status, "broken.ts");
    assert!(
        !status.contains("never fully walked"),
        "the project has been fully walked:\n{status}"
    );
    // The stamp the daemon wrote at startup, read back off disk by the
    // command rather than asked of the daemon.
    assert_contains(&status, "just now");

    assert!(daemon_process.try_wait().unwrap().is_none(), "the daemon must still be running");
}

#[test]
fn status_reports_a_dead_daemon_and_the_files_its_index_never_saw() {
    let project = Project::new();
    let mut daemon_process = spawn_daemon(project.root());
    // The index this asserts on has to be the finished one - see the wait in
    // the test above.
    wait_until_indexed(project.root());
    // See the test above: the bundled plugin's pid file can appear a moment
    // after the walk's completion marker does.
    wait_for("the typescript plugin to start", || project.plugin_pid_file().exists());
    let plugin_pid = project.recorded_pid(&project.plugin_pid_file());

    // Killed, not stopped, so the pid files are deliberately left behind:
    // status has to see through them rather than trust them.
    daemon_process.kill().expect("failed to kill the daemon");
    daemon_process.wait().expect("failed to reap the daemon");
    // The plugin exits when the daemon's end of its stdin closes, which is
    // what makes "no plugin pid file reads as live" the correct report a
    // moment later.
    wait_for("the plugin to exit with its core", || !daemon::is_process_alive(plugin_pid));

    // A file added while nothing was watching: the index has never seen it.
    std::fs::write(project.root().join("late.ts"), b"export const late = 1;\n")
        .expect("failed to write late.ts");

    let status = project.status();

    assert_contains(&status, "daemon core:     not running");
    // The stale plugin-typescript.pid left behind by the kill names a dead
    // process, so `plugin_reports` drops it rather than reporting it -
    // "status has to see through them rather than trust them" applies to
    // this line just as much as to the daemon core's own.
    assert_contains(&status, "plugins:         none active");
    assert_contains(&status, "index coverage:  66.7% (2/3 source files)");
    assert_contains(&status, "dirty files:     1 awaiting reindex");
    // Still true of the index, and still reported with no daemon to ask.
    assert_contains(&status, "syntax errors:   1 file(s)");
    assert_contains(&status, "broken.ts");
}

/// Running it somewhere g-mesh has never indexed must report an empty state,
/// not fail - `status` is the command someone reaches for precisely when they
/// are not sure whether anything is set up.
#[test]
fn status_on_a_project_that_was_never_indexed_reports_an_empty_state() {
    let project = Project::new();

    let status = project.status();

    assert_contains(&status, "daemon core:     not running");
    assert_contains(&status, "plugins:         none active");
    assert_contains(&status, "last used:       never recorded");
    assert_contains(&status, "never fully walked");
    assert_contains(&status, "index coverage:  0.0% (0/2 source files)");
    assert_contains(&status, "dirty files:     2 awaiting reindex");
    assert_contains(&status, "syntax errors:   none");
}

/// Task #62's acceptance criterion: a project whose `lastUsed` is older
/// than `cleanup.idleThresholdDays` makes `g-mesh status` print the GC
/// warning naming it, when `cleanup.enabled` is on.
#[test]
fn status_warns_about_a_project_idle_past_the_threshold() {
    let _config = GlobalConfigGuard::set(&GlobalConfig {
        cleanup: CleanupConfig { enabled: true, idle_threshold_days: 90 },
    });
    let project = Project::new();
    project.backdate_last_used(100);
    let project_id =
        project.state_dir().file_name().unwrap().to_string_lossy().into_owned();

    let status = project.status();

    assert_contains(&status, "idle for more than 90 days");
    assert_contains(&status, &project_id);
    assert_contains(&status, "g-mesh clean expired");
}

/// The other half of the same criterion: `cleanup.enabled = false` prints
/// nothing at all, regardless of how idle a project is.
#[test]
fn status_prints_no_warning_when_cleanup_is_disabled() {
    let _config = GlobalConfigGuard::set(&GlobalConfig {
        cleanup: CleanupConfig { enabled: false, idle_threshold_days: 90 },
    });
    let project = Project::new();
    project.backdate_last_used(100);

    let status = project.status();

    assert!(!status.contains("idle for more than"), "{status}");
}
