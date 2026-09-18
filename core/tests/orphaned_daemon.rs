//! GM-320: a daemon whose project root - or whose own executable - has been
//! deleted stops itself, instead of holding its memory until the core's
//! 24-hour idle timeout collects it.
//!
//! Driven through real daemons, because the claim is entirely about a process:
//! whether it is still there, whether the plugin it spawned is still there,
//! and whether the files describing it were released on the way out. Nothing
//! here calls `daemon::lifecycle::orphan_check` - that is unit-tested in its
//! own module; what these tests are for is that the check is actually *reached*
//! on the supervisor's tick, against a daemon doing everything else a daemon
//! does.
//!
//! # Why every path is captured before anything is deleted
//!
//! `daemon::pid_path`/`socket_path`/`endpoint` all resolve through
//! `project_hash`, which canonicalizes the root - and canonicalizing a
//! directory that has just been deleted fails. So a test that deleted the root
//! first and then asked where the pid file was would fail on its own
//! bookkeeping rather than on the daemon's behaviour. Each fixture therefore
//! records its state directory up front and uses the `*_in(state_dir)`
//! variants afterwards.
//!
//! # The control arm
//!
//! [`a_daemon_whose_project_root_still_exists_is_left_alone`] is the same
//! daemon, the same timeouts and the same wait with the deletion left out. It
//! is what makes the two tests above it evidence rather than a coincidence: an
//! exit that happened for any other reason - the core's idle timeout, a crash,
//! a supervisor that exits on every tick - would show up there too.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use g_mesh::daemon;
use g_mesh::daemon::lifecycle::{CORE_IDLE_ENV, PLUGIN_IDLE_ENV};
use g_mesh::storage::connection::project_dir;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Short enough that `IdleTimeouts::tick` - a quarter of the shorter timeout -
/// is 500ms, so an orphan is noticed within a second rather than within the
/// production 30. Nothing here asserts on the plugin's sleep; this is only the
/// knob that sets the supervisor's polling rate.
const PLUGIN_IDLE: Duration = Duration::from_millis(2_000);

/// Stands in for production's 24 hours: far beyond any of these runs, so a
/// core that goes away has gone away for the reason under test and not because
/// it ran out of idle budget. The control arm below depends on this entirely.
const CORE_IDLE_EFFECTIVELY_NEVER: Duration = Duration::from_secs(600);

/// How long an orphaned daemon is allowed to take to notice. Generous next to
/// the 500ms tick the fixtures configure - this is a hang guard, not the
/// timing assertion. The timing claim ("at most one tick") is made by the
/// control arm instead, which waits *longer* than this and still finds a
/// healthy daemon running.
fn orphan_budget() -> Duration {
    common::startup_timeout()
}

/// How long the control arm watches a healthy daemon decline to stop itself.
/// Twenty of the 500ms ticks these fixtures configure - far more than the one
/// tick an orphan is allowed, and small enough that the control costs the
/// suite ten seconds rather than a minute. A fixed duration rather than
/// [`orphan_budget`] because this one *is* spent in full on every run.
const CONTROL_WATCH: Duration = Duration::from_secs(10);

struct Project {
    dir: Option<tempfile::TempDir>,
    /// Captured at construction, because every helper that could derive it
    /// later needs a root that still exists.
    state_dir: PathBuf,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        let root = dir.path().to_path_buf();
        let state_dir = project_dir(&root).expect("failed to resolve the state directory");
        let project = Self { dir: Some(dir), state_dir, root };
        project.write("src/alpha.ts", "export function alpha(): number {\n  return 1;\n}\n");
        project
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative_path: &str, contents: &str) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create a fixture directory");
        }
        std::fs::write(&path, contents).expect("failed to write a fixture file");
    }

    /// Deletes the project root the way a `rm -rf`, a dropped worktree or a
    /// swept `/tmp` build would - and gives up the `TempDir` so its own `Drop`
    /// does not try again and panic.
    fn delete(&mut self) {
        let dir = self.dir.take().expect("a project root is only deleted once");
        dir.close().expect("failed to delete the project root");
        assert!(!self.root.exists(), "the project root must actually be gone");
    }

    fn core_pid_file(&self) -> PathBuf {
        daemon::pid_path_in(&self.state_dir)
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path_in(&self.state_dir)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Every plugin that is still recorded, before the state directory that
        // records it goes: a leaked node process would outlive the whole suite.
        for (_, path) in daemon::registry::discovered_pid_files(&self.state_dir) {
            common::kill_pid_file(&path);
        }
        common::kill_pid_file(&self.core_pid_file());
        let _ = std::fs::remove_dir_all(&self.state_dir);
    }
}

/// A daemon child, spawned from `bin` so the executable-deleted test can point
/// it at a copy it owns.
struct Daemon {
    child: Child,
}

impl Daemon {
    fn spawn(bin: &Path, root: &Path) -> Self {
        let child = Command::new(bin)
            .arg("daemon")
            .arg("--project-root")
            .arg(root)
            .env(PLUGIN_IDLE_ENV, PLUGIN_IDLE.as_millis().to_string())
            .env(CORE_IDLE_ENV, CORE_IDLE_EFFECTIVELY_NEVER.as_millis().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the daemon");
        Self { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
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

/// Brings a daemon all the way up: indexed, answering, and with its plugin
/// running - so that what the orphan check later interrupts is a daemon in its
/// ordinary steady state rather than one still starting.
fn wait_until_fully_up(project: &Project) -> u32 {
    wait_until_indexed(project.root());
    common::wait_for("the plugin to start", common::startup_timeout(), || project.plugin_pid_file().exists());
    recorded_pid(&project.plugin_pid_file())
}

/// Asserts the teardown was the deliberate one: a clean exit, the plugin
/// reaped rather than orphaned in its turn, and every file that described a
/// running daemon removed.
fn assert_released_everything(project: &Project, daemon_process: &mut Daemon, plugin_pid: u32) {
    let status = daemon_process.child.wait().expect("failed to reap the daemon");
    assert!(status.success(), "an orphan shutdown is a clean exit, not a failure: {status}");

    common::wait_for("the plugin to go with its core", common::startup_timeout(), || {
        !daemon::is_process_alive(plugin_pid)
    });

    // `mut` is only used by the `cfg(unix)` push below; on Windows a pipe name
    // goes with the process and there is no socket file to check.
    #[allow(unused_mut)]
    let mut leftovers = vec![
        project.core_pid_file(),
        project.plugin_pid_file(),
        daemon::build_stamp_path_in(&project.state_dir),
    ];
    // Spelled out rather than taken from `daemon::socket_path`, which resolves
    // through the root this test has just deleted; the constant behind it is
    // private to `daemon`, so this is the one place the name is repeated.
    #[cfg(unix)]
    leftovers.push(project.state_dir.join("daemon.sock"));
    for leftover in leftovers {
        assert!(
            !leftover.exists(),
            "an orphaned daemon that stopped itself must release {} - anything left behind \
             describes a daemon that no longer exists",
            leftover.display()
        );
    }
}

/// The observed case, end to end: four daemons were found running from
/// directories that no longer existed. This is one of them, reproduced.
#[test]
fn a_daemon_whose_project_root_is_deleted_stops_itself() {
    let mut project = Project::new();
    let mut daemon_process = Daemon::spawn(Path::new(BIN), project.root());
    let plugin_pid = wait_until_fully_up(&project);
    assert!(daemon_process.is_running(), "the daemon must be up before its root is taken away");

    project.delete();

    common::wait_for("the orphaned daemon to stop itself", orphan_budget(), || !daemon_process.is_running());
    assert_released_everything(&project, &mut daemon_process, plugin_pid);
}

/// The other arm of the check: a `cargo clean`, a deleted worktree, a swept
/// `/tmp` build. The daemon runs from a copy of the test binary precisely so
/// the copy can be deleted without touching the one cargo built - and its
/// project root is left entirely alone, so the only thing that can end it is
/// the executable.
///
/// The copy still finds the repository's plugins: `manifest::bundled_roots`
/// resolves them through `env!("CARGO_MANIFEST_DIR")`, which is compiled into
/// the binary rather than derived from where it happens to sit.
#[test]
fn a_daemon_whose_own_executable_is_deleted_stops_itself() {
    let project = Project::new();
    let bin_dir = tempfile::tempdir().expect("failed to create a directory for the daemon's binary");
    let copied_bin = bin_dir.path().join(Path::new(BIN).file_name().expect("the test binary has a name"));
    std::fs::copy(BIN, &copied_bin).expect("failed to copy the g-mesh binary");

    let mut daemon_process = Daemon::spawn(&copied_bin, project.root());
    let plugin_pid = wait_until_fully_up(&project);
    assert!(daemon_process.is_running(), "the daemon must be up before its binary is taken away");

    std::fs::remove_file(&copied_bin).expect("failed to delete the daemon's own executable");
    assert!(project.root().exists(), "the project root is deliberately untouched by this test");

    common::wait_for("the orphaned daemon to stop itself", orphan_budget(), || !daemon_process.is_running());
    assert_released_everything(&project, &mut daemon_process, plugin_pid);
}

/// The control. Same fixture, same timeouts, same wait - and nothing deleted.
/// A daemon that stopped here would mean the two tests above prove nothing:
/// the exits they observe would be something this supervisor does on a tick
/// regardless, not a verdict about being orphaned.
///
/// Watches for [`CONTROL_WATCH`] - twenty ticks - so "it had not got round to
/// it yet" is not an available explanation for it still running. Nothing is
/// asserted about the *plugin* here: this fixture's 2s plugin timeout means it
/// has correctly gone to sleep long before the watch is over, which is the
/// other test file's subject entirely.
#[test]
fn a_daemon_whose_project_root_still_exists_is_left_alone() {
    let project = Project::new();
    let mut daemon_process = Daemon::spawn(Path::new(BIN), project.root());
    let _plugin_pid = wait_until_fully_up(&project);

    std::thread::sleep(CONTROL_WATCH);

    assert!(
        daemon_process.is_running(),
        "a daemon whose project root and executable both exist must not stop itself"
    );
    assert!(
        daemon::is_listening(project.root()).expect("the endpoint must be resolvable"),
        "and must still be serving its endpoint"
    );
    assert_eq!(
        recorded_pid(&project.core_pid_file()),
        daemon_process.pid(),
        "still the same process, with its pid file intact"
    );
}
