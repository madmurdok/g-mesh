//! That a real daemon stops when it is *asked* to - `g_mesh::process
//! ::request_stop`, which is `SIGTERM` on Unix - and stops without anyone
//! having to insist.
//!
//! # Why this test exists at all (GM-320)
//!
//! Every other stop mechanism in this project is built on this one. `g-mesh
//! stop` asks before it insists (`cli::stop::terminate`), `shim
//! ::retire_outdated_daemon` and `shim::evict_wedged_daemon` both go through
//! that same escalation, and `g_mesh::process`'s own header states as fact
//! that this daemon "installs no signal handler anywhere" and that "`SIGTERM`
//! kills it outright today". Until now nothing checked it. Two things hid
//! that: the escalation to `SIGKILL` means `g-mesh stop` would go on *working*
//! if the polite rung stopped working, only slower and with a different
//! verdict nobody asserted on (now asserted - see `cli_stop.rs`); and every
//! teardown in this suite (`common::kill_pid_file`) uses `kill -9` on purpose,
//! so no test ever exercised the polite path against a real daemon at all.
//!
//! GM-320 went looking for the gap GM-321 had just found one layer down - a
//! termination request treated as a completed termination - and measured this
//! one instead: on macOS the core exited 0.19-0.30s after a plain `SIGTERM`
//! in every configuration tried, including with its project root deleted,
//! with its own executable deleted, and while holding a live `rust-analyzer`
//! whose tree was half a gigabyte. The claim was true. This file is what keeps
//! it true, so a future handler that swallows or blocks the signal shows up as
//! a failing test rather than as a daemon somebody has to find in `ps`.
//!
//! # Why the daemon is bootstrapped through the shim
//!
//! The same reason `cli_stop.rs` does it, and this file learned it the hard
//! way: a daemon spawned as the test's own child lingers as an unreaped zombie
//! after being signalled, and a zombie still answers `kill(pid, 0)`. The first
//! version of this test spawned one directly and failed its five-second budget
//! against a daemon that had in fact exited in milliseconds - it was asserting
//! about a process state that cannot occur in production, where the shim
//! detaches the daemon and init reaps it. So the signal is sent to a genuinely
//! detached process, exactly as `g-mesh stop` sends it.
//!
//! # What is and is not asserted
//!
//! The discriminating assertion is [`TERMINATION_BUDGET`]: the daemon must be
//! gone within it, and nothing stronger than `request_stop` is ever sent. A
//! test that only waited for the process to disappear would also pass against
//! a daemon that ignored the signal and was cleaned up by this file's own
//! teardown.
//!
//! Cross-platform, but the claim is carried by Unix. `process::request_stop`
//! is `TerminateProcess` on Windows, which cannot be caught, so the assertion
//! there is about the teardown that follows rather than about the daemon
//! choosing to honour anything - see `g_mesh::process`'s header on why that is
//! an honest difference in what the platform offers rather than a gap here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::process;
use g_mesh::storage::connection::project_dir;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// How long the daemon gets to honour a `SIGTERM` before this test calls it a
/// failure.
///
/// Unlike most of this suite's waits, this one *is* a timing assertion, and it
/// is the whole point of the test: `cli::stop::TERMINATION_TIMEOUT` gives a
/// real daemon five seconds before escalating to `SIGKILL`, so a daemon that
/// needed longer than this is by definition one `g-mesh stop` has to kill.
/// Measured at 0.19-0.30s on macOS with a plugin running, so this is roughly
/// twenty times the observed cost - loose enough for a loaded machine, nowhere
/// near loose enough to pass a daemon that is ignoring the signal.
const TERMINATION_BUDGET: Duration = Duration::from_secs(5);

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

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn plugin_pid_file(&self) -> PathBuf {
        daemon::plugin_pid_path(self.root()).expect("failed to resolve the plugin pid file path")
    }

    /// Bootstraps a detached daemon through the shim and waits for its plugin
    /// too - valid here because the fixture is a fresh project, whose
    /// cold-start semantic pass is exactly the startup path that spawns one
    /// (see `cli_stop.rs`'s note on the lazy per-language spawn).
    fn bootstrap(&self) -> (u32, u32) {
        let mut shim = Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the shim");

        common::wait_for("the daemon to bind its socket", common::startup_timeout(), || {
            self.pid_file().exists()
        });
        // The shim was only the vehicle; the daemon it spawned is detached and
        // outlives it, which is the process this test has to signal.
        let _ = shim.kill();
        let _ = shim.wait();

        let core = read_pid(&self.pid_file());
        common::wait_for("the daemon to spawn its plugin", common::startup_timeout(), || {
            self.plugin_pid_file().exists()
        });
        let plugin = read_pid(&self.plugin_pid_file());
        assert_ne!(core, plugin, "the plugin runs in a process of its own");
        assert!(daemon::is_process_alive(core), "the daemon must be up before it is asked to stop");
        assert!(daemon::is_process_alive(plugin), "and so must its plugin");
        (core, plugin)
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

/// The acceptance criterion: asked once, politely, a real daemon goes - and
/// takes the plugin it spawned with it.
#[test]
fn a_real_daemon_stops_when_it_is_asked_to_without_having_to_be_killed() {
    let project = Project::new();
    let (core, plugin) = project.bootstrap();

    // The one and only thing sent. Nothing in this test ever escalates.
    let asked_at = Instant::now();
    process::request_stop(core).expect("failed to ask the daemon to stop");

    while daemon::is_process_alive(core) {
        assert!(
            asked_at.elapsed() < TERMINATION_BUDGET,
            "the daemon (pid {core}) was still running {:?} after being asked to stop - a daemon \
             that has to be killed is one nobody can stop politely, and `g-mesh stop` would be \
             escalating to SIGKILL on every single stop",
            asked_at.elapsed()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The plugin is the daemon's child with the daemon holding the write end
    // of its stdin, so a core that really went away closes that pipe and the
    // plugin exits on it (index.ts's `end` handler). A plugin still running
    // here would be half a gigabyte of tsserver left behind by a stop that
    // reported success - which is the shape GM-320 set out to end.
    common::wait_for("the plugin to go with its core", common::startup_timeout(), || {
        !daemon::is_process_alive(plugin)
    });
}
