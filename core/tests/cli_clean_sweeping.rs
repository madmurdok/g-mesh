//! The three forms of `g-mesh clean` that are scoped to *every* project under
//! the state root - `expired`, `orphaned --force`, `all --force` - driven as
//! real subprocesses against real state directories.
//!
//! `cli_clean.rs` cannot host these, and says so in its own header: it runs
//! against the state root the whole test run shares, so a sweeping delete from
//! there would take out fixtures belonging to tests running beside it. That
//! left the destructive paths covered only by unit tests in `cli::clean` with
//! an injected root.
//!
//! A unit test with an injected root is a good test of the *classification* -
//! which candidates count as expired, which as orphaned. What it cannot show
//! is that the word on the command line reaches that code, that `--force` is
//! threaded through, that the idle threshold is read from the global config
//! rather than defaulted, and that what gets deleted is a state directory a
//! real daemon really built. That is precisely the risk profile of a command
//! whose job is `rm -rf`.
//!
//! What makes it drivable here is that every path in the product resolves
//! through `paths::g_mesh_home()`, which reads `G_MESH_HOME`. So each test
//! gets a home of its own, set on the `Command` rather than on this process -
//! no `set_var`, and therefore no race with the test threads cargo runs in
//! parallel, which is the trap this file would otherwise have walked into.
//!
//! Per *test*, not per test binary: the tests in this file sweep the same
//! kinds of directory as each other, so a per-binary root would just move the
//! collision from between files to within one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon::{self, identity::project_hash};
use g_mesh::paths::HOME_ENV;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

/// A `G_MESH_HOME` belonging to one test and nothing else, so a sweep inside
/// it is safe by construction rather than by every test remembering to scope
/// itself.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // On Unix: deliberately short, and deliberately not
        // `tempfile::tempdir()`. A daemon binds
        // `<home>/projects/<hash>/daemon.sock`, and an AF_UNIX address holds
        // 104 bytes of path on macOS; the system temp directory there is
        // `/var/folders/<..>/<..>/T/`, some 50 bytes before anything of ours,
        // which would leave the socket path riding the limit. Since GM-220 an
        // over-long path is at least a clear refusal naming the limit rather
        // than a bind failure - but the point is not to produce one.
        //
        // That budget is Unix's alone, and so is `/tmp`. On Windows the
        // transport is a named pipe, so there is no path length to protect -
        // and `/tmp/...` is not even an absolute path there but a *drive-
        // relative* one, resolved against whatever drive each process happens
        // to be on. This binary runs from the checkout's drive and the daemon
        // it spawns need not, which is how the two came to disagree about
        // where the state root was and every sweep here timed out waiting for
        // a pid file that was being written somewhere else (GM-252). The
        // identical mistake, in `.cargo/config.toml`, is what GM-246 was.
        let base = if cfg!(windows) { std::env::temp_dir() } else { PathBuf::from("/tmp") };
        let dir =
            base.join(format!("gm-sweep-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
        Self { dir }
    }

    fn projects_root(&self) -> PathBuf {
        self.dir.join("projects")
    }

    /// Writes the global config `clean expired` reads its threshold from.
    /// Key spelling is the file's, not Rust's - see `config::CleanupConfig`.
    fn set_idle_threshold_days(&self, days: u64) {
        fs::write(
            self.dir.join("config.toml"),
            format!("[cleanup]\nenabled = true\nidleThresholdDays = {days}\n"),
        )
        .expect("failed to write the global config");
    }

    /// How many project state directories the root currently holds - the
    /// quantity every assertion here is really about.
    fn project_count(&self) -> usize {
        fs::read_dir(self.projects_root()).map(|entries| entries.flatten().count()).unwrap_or(0)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A project whose state lives in one particular [`Home`].
struct Project<'a> {
    home: &'a Home,
    dir: tempfile::TempDir,
}

impl<'a> Project<'a> {
    fn new(home: &'a Home) -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        fs::write(dir.path().join("a.ts"), b"export const a = 1;\n")
            .expect("failed to seed the project with a source file");
        Self { home, dir }
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

    fn command(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .current_dir(self.root())
            .env(HOME_ENV, &self.home.dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to run `g-mesh {}`: {e}", args.join(" ")))
    }

    /// Bootstraps a detached daemon through the shim, so the state directory
    /// a test then deletes is one a real daemon really built - the whole
    /// reason for driving these as subprocesses.
    fn bootstrap_daemon(&self) {
        let mut shim = Command::new(BIN)
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
    }

    /// `clean` refuses to delete state a daemon is still serving, so every
    /// sweep here has to get past this first.
    fn stop_daemon(&self) {
        let stopped = self.command(&["stop"]);
        assert!(stopped.status.success(), "stop failed: {}", stderr_of(&stopped));
    }

    fn ready_to_be_swept(&self) -> PathBuf {
        self.bootstrap_daemon();
        self.stop_daemon();
        let state_dir = self.state_dir();
        assert!(state_dir.is_dir(), "a daemon must have built a state directory to delete");
        state_dir
    }
}

impl Drop for Project<'_> {
    fn drop(&mut self) {
        for path in [self.pid_file(), self.plugin_pid_file()] {
            common::kill_pid_file(&path);
        }
    }
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

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The guarantee the rest of this file rests on, asserted rather than assumed:
/// a subprocess run under one of these homes writes there and not into the
/// root the test run shares. If the override ever stopped crossing the process
/// boundary, every test below would still pass - they would simply be sweeping
/// the shared root, which is the exact accident this file exists to avoid. So
/// it is checked directly, the same way `state_isolation.rs` checks the
/// suite-wide override.
#[test]
fn a_sweep_runs_against_a_root_of_its_own_not_the_shared_one() {
    let home = Home::new();
    let project = Project::new(&home);

    let shared =
        g_mesh::storage::connection::projects_root().expect("failed to resolve the shared projects root");
    assert!(
        !home.projects_root().starts_with(&shared),
        "this test's home ({}) is inside the shared root ({})",
        home.projects_root().display(),
        shared.display()
    );

    // `status` reports the state directory it would use without building one.
    let output = project.command(&["status"]);
    assert!(output.status.success(), "status failed: {}", stderr_of(&output));
    let reported = stdout_of(&output)
        .lines()
        .find_map(|line| line.trim().strip_prefix("state directory:").map(|p| PathBuf::from(p.trim())))
        .expect("status did not report a state directory");

    assert!(
        reported.starts_with(home.projects_root()),
        "a spawned g-mesh wrote outside this test's home: {} is not under {}",
        reported.display(),
        home.projects_root().display()
    );
}

/// `clean expired` end to end, including the part no unit test can reach: the
/// threshold comes from the global `config.toml`, so the same project is spared
/// under one value and deleted under another with nothing else changed.
#[test]
fn clean_expired_honours_the_configured_threshold() {
    let home = Home::new();
    let project = Project::new(&home);
    let state_dir = project.ready_to_be_swept();

    home.set_idle_threshold_days(90);
    let spared = project.command(&["clean", "expired"]);
    assert!(spared.status.success(), "clean expired failed: {}", stderr_of(&spared));
    assert!(state_dir.is_dir(), "a project idle for seconds must survive a 90-day threshold");

    home.set_idle_threshold_days(0);
    let swept = project.command(&["clean", "expired"]);

    assert!(swept.status.success(), "clean expired failed: {}", stderr_of(&swept));
    assert!(!state_dir.exists(), "the expired project's state directory must be gone: {}", stdout_of(&swept));
}

/// `clean orphaned --force` end to end, and the half that matters most about
/// it: it deletes state whose project is gone *and leaves everything else
/// alone*. A sweep that took the live project too would satisfy "the orphan
/// was deleted" just as well.
#[test]
fn clean_orphaned_force_deletes_only_the_state_whose_project_is_gone() {
    let home = Home::new();
    let live = Project::new(&home);
    let live_state = live.ready_to_be_swept();

    let orphan_state = {
        let orphan = Project::new(&home);
        let state = orphan.ready_to_be_swept();
        // Dropping the project deletes its temp directory, which is exactly
        // what makes the state behind it an orphan.
        state
    };
    assert!(
        orphan_state.is_dir(),
        "deleting the project directory must not remove its state - that is what makes it an orphan"
    );
    assert_eq!(home.project_count(), 2, "both projects must have state before the sweep");

    let swept = live.command(&["clean", "orphaned", "--force"]);

    assert!(swept.status.success(), "clean orphaned --force failed: {}", stderr_of(&swept));
    assert!(!orphan_state.exists(), "the orphaned state directory must be gone: {}", stdout_of(&swept));
    assert!(live_state.is_dir(), "the live project's state must survive: {}", stdout_of(&swept));
}

/// `clean all --force` end to end. Its scope is the whole root, so the test
/// that proves it needs more than one project in that root - with a single
/// project it is indistinguishable from `clean`.
#[test]
fn clean_all_force_deletes_every_project_in_the_root() {
    let home = Home::new();
    let first = Project::new(&home);
    let second = Project::new(&home);
    let first_state = first.ready_to_be_swept();
    let second_state = second.ready_to_be_swept();
    assert_ne!(first_state, second_state, "the two fixtures must be distinct projects");
    assert_eq!(home.project_count(), 2);

    let swept = first.command(&["clean", "all", "--force"]);

    assert!(swept.status.success(), "clean all --force failed: {}", stderr_of(&swept));
    assert!(!first_state.exists(), "{}", stdout_of(&swept));
    assert!(!second_state.exists(), "{}", stdout_of(&swept));
    assert_eq!(home.project_count(), 0, "the root must be empty: {}", stdout_of(&swept));
}

/// `--force` is what separates a report from a deletion, so its absence has to
/// be driven through the same path rather than trusted. `cli_clean.rs` covers
/// this for `orphaned`; `all` is only reachable here, because answering it
/// honestly means having projects in the root to count.
#[test]
fn clean_all_without_force_counts_without_deleting() {
    let home = Home::new();
    let first = Project::new(&home);
    let second = Project::new(&home);
    let first_state = first.ready_to_be_swept();
    let second_state = second.ready_to_be_swept();

    let previewed = first.command(&["clean", "all"]);

    assert!(previewed.status.success(), "clean all failed: {}", stderr_of(&previewed));
    assert!(first_state.is_dir(), "clean all without --force must delete nothing");
    assert!(second_state.is_dir(), "clean all without --force must delete nothing");
    assert_eq!(home.project_count(), 2, "clean all without --force must delete nothing");
}
