//! `g-mesh stop`: shut down the current project's daemon core and every
//! language plugin it has spawned.
//!
//! The architecture doc's lifecycle model says the core deliberately does not
//! die on a short idle timeout - it lives until it is explicitly stopped, or
//! until a much longer timeout of its own expires (`daemon::lifecycle`) - so
//! this command is what makes that "explicitly" mean something. Until it
//! existed the only documented way to stop a daemon was to read its pid out
//! of `daemon.pid` and kill it by hand, which is also the only way to find
//! out afterwards that a plugin was still running.
//!
//! Since task 155, "every plugin" is not one hardcoded process: `daemon
//! ::registry::PluginRegistry` gives each language its own pid file
//! (`plugin-<language>.pid`), so this command lists whatever is present
//! (`daemon::registry::discovered_pid_files`) rather than assuming exactly
//! one. A language asleep on *its own* idle timeout is not something this
//! command has to handle specially, and that is by construction: its
//! supervisor removes its pid file when it puts that language to sleep, so a
//! sleeping (or never-spawned) language's half of a stop is simply a no-op -
//! the same shape as stopping a project whose plugins never started.
//!
//! # Shutdown order, and why plugins are waited for rather than killed
//!
//! The core is signalled first. Each plugin is its child with the daemon
//! holding the write end of its stdin, and a plugin exits when that pipe
//! closes (index.ts's stdin `end` handler, for the bundled one) - so once the
//! core is gone every plugin normally goes on its own, and the honest thing
//! to report is that it did. Each is only signalled if it fails to, which is
//! what makes "no orphaned processes" a guarantee rather than an expectation.
//!
//! Both stops escalate: on Unix `SIGTERM`, and `SIGKILL` if that is ignored.
//! Neither process installs a handler today, so the escalation is insurance
//! against a future one that hangs, not a routine path. On Windows there is
//! only one way to stop a process that did not ask to be stopped, so the two
//! rungs are the same call and the escalation never escalates - see
//! `crate::process`, which also explains why that is the honest port of what
//! this command does rather than a shortcut past it.
//!
//! # Doing nothing is a success
//!
//! Stopping a project with no daemon running is a no-op that exits zero.
//! `stop` is what someone reaches for when they are not sure whether anything
//! is running, and a command that fails when the answer is "nothing" is a
//! command people learn to ignore the exit code of. Stale pid files left by a
//! crashed daemon are cleared on the way past.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::daemon;
use crate::process;
use crate::storage::connection::project_dir;

/// How long a process that has been asked to stop is given before the next,
/// harder attempt - generous, because overshooting costs a moment on a command
/// a person is watching, while undershooting escalates against a process that
/// was shutting down cleanly all along.
pub(crate) const TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the plugin is given to notice its core is gone and exit by
/// itself, before it is signalled like anything else.
const PLUGIN_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How a process this command was responsible for came to be gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// Exited after `SIGTERM` - the ordinary path.
    Terminated { pid: u32 },
    /// Ignored `SIGTERM` and had to be killed.
    Killed { pid: u32 },
    /// Exited on its own once its core was gone; never signalled. Only ever
    /// reported for the plugin.
    ExitedWithCore { pid: u32 },
}

impl Stopped {
    fn pid(self) -> u32 {
        match self {
            Stopped::Terminated { pid } | Stopped::Killed { pid } | Stopped::ExitedWithCore { pid } => pid,
        }
    }
}

/// What a single `stop` actually had to do. `core` is `None` and `plugins` is
/// empty when nothing was running, which is the no-op case rather than a
/// failure.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    pub core: Option<Stopped>,
    /// One entry per language that had a live pid file, in the order
    /// `daemon::registry::discovered_pid_files` reports them (sorted by
    /// language).
    pub plugins: Vec<(String, Stopped)>,
    /// Whether the core this command stopped was found through the daemon
    /// lock rather than through `daemon.pid` - a daemon that had already
    /// deleted its own pid file and then failed to go away (task 184).
    /// Reported because "nothing was running" was the *wrong* answer this
    /// command used to give for it, and someone who has just been told that
    /// deserves to know why the answer changed.
    pub core_was_wedged: bool,
}

impl Outcome {
    pub fn stopped_anything(&self) -> bool {
        self.core.is_some() || !self.plugins.is_empty()
    }
}

/// Stops the daemon serving the current directory's project.
pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve the current directory")?;
    let outcome = stop(&cwd)?;
    print!("{}", render(&outcome, &cwd));
    Ok(())
}

/// Stops whatever is running for `project_root` and clears the state files
/// the stopped processes owned.
///
/// Returns once every process it stopped is genuinely gone - a caller that
/// gets `Ok` back can bootstrap a fresh daemon immediately without racing the
/// old one for the socket.
pub fn stop(project_root: &Path) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    // The pid file first, because a healthy daemon is what this command
    // stops almost every time it is run. The daemon lock second, and only
    // when the pid file has nothing to say: a daemon whose idle shutdown
    // removed its pid file and then failed to exit is invisible to every
    // check keyed off that file, and holds the project's lock for as long as
    // it lives - so the lock, not the pid file, is the source of truth this
    // has to agree with `daemon::acquire_singleton_lock` on.
    let core_pid = match live_pid(&daemon::pid_path(project_root)?) {
        Some(pid) => Some(pid),
        None => match daemon::inspect_daemon_lock(project_root)? {
            daemon::DaemonLock::Wedged { pid } => {
                outcome.core_was_wedged = true;
                Some(pid)
            }
            _ => None,
        },
    };
    let state_dir = project_dir(project_root).context("failed to resolve the project's state directory")?;
    let plugin_pids: Vec<(String, u32)> = daemon::registry::discovered_pid_files(&state_dir)
        .into_iter()
        .filter_map(|(language, path)| live_pid(&path).map(|pid| (language, pid)))
        .collect();

    if let Some(pid) = core_pid {
        outcome.core = Some(
            terminate(pid, TERMINATION_TIMEOUT)
                .with_context(|| format!("failed to stop the daemon core (pid {pid})"))?,
        );
    }

    for (language, pid) in plugin_pids {
        // Only worth waiting on if a core was just taken away from it; an
        // already-orphaned plugin has nothing left to notice.
        let exited_by_itself = outcome.core.is_some() && wait_until_gone(pid, PLUGIN_EXIT_TIMEOUT);
        let stopped = if exited_by_itself {
            Stopped::ExitedWithCore { pid }
        } else {
            terminate(pid, TERMINATION_TIMEOUT)
                .with_context(|| format!("failed to stop the {language} plugin (pid {pid})"))?
        };
        outcome.plugins.push((language, stopped));
    }

    tidy_state_files(project_root)?;
    Ok(outcome)
}

/// The pid recorded in `path`, if it names a process that is actually
/// running. A pid file left behind by a crashed daemon reads as `None`.
fn live_pid(path: &Path) -> Option<u32> {
    daemon::read_pid_file(path).filter(|&pid| daemon::is_process_alive(pid))
}

/// Ask, then insist, waiting for the process to go after each
/// (`process::request_stop`, then `process::force_stop`).
///
/// `pub(crate)` for `shim::evict_wedged_daemon`, which has to clear exactly
/// this kind of process out of the way before it can bootstrap a replacement,
/// and must escalate the same way rather than grow a second implementation of
/// it - the same argument `shim::retire_outdated_daemon` already makes for
/// reusing [`stop`] itself.
///
/// On Windows both rungs are `TerminateProcess`, which cannot be ignored, so
/// the second one is dead weight there rather than a second chance -
/// [`Stopped::Killed`] is a verdict only Unix can reach. Kept as one shape on
/// both platforms because everything downstream of it (the escalation's
/// waiting, `Outcome`, what `stop` reports) is genuinely the same.
pub(crate) fn terminate(pid: u32, grace: Duration) -> Result<Stopped> {
    process::request_stop(pid)?;
    if wait_until_gone(pid, grace) {
        return Ok(Stopped::Terminated { pid });
    }

    process::force_stop(pid)?;
    if wait_until_gone(pid, grace) {
        return Ok(Stopped::Killed { pid });
    }

    // Neither `SIGKILL` nor `TerminateProcess` can be caught, so reaching this
    // means the process is stuck in the kernel (uninterruptible I/O) or is an
    // unreaped zombie whose parent is still around - neither of which this
    // command can fix, and both of which are worth saying out loud rather than
    // reporting success.
    bail!("pid {pid} is still present after being asked and then made to stop")
}

fn wait_until_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !daemon::is_process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Clears the files whose owning process is confirmed gone.
///
/// Every removal is guarded by a fresh liveness check rather than by "this
/// command just stopped something": `stop` also runs against projects whose
/// daemon crashed, and it must not delete the socket of a daemon that is
/// somehow still serving (e.g. one whose pid file was removed by hand).
fn tidy_state_files(project_root: &Path) -> Result<()> {
    let state_dir = project_dir(project_root).context("failed to resolve the project's state directory")?;
    let mut paths = vec![daemon::pid_path(project_root)?];
    paths.extend(daemon::registry::discovered_pid_files(&state_dir).into_iter().map(|(_, path)| path));
    for path in paths {
        if live_pid(&path).is_none() {
            let _ = fs::remove_file(&path);
        }
    }

    // On Unix a socket file outlives the daemon that bound it, and a leftover
    // one is what `daemon::run` has to clear before it can bind. Removing it
    // here is what "the socket is released" means to the next daemon. On
    // Windows there is nothing to release - the pipe name went with the
    // process - and `clear_stale` is a no-op.
    //
    // The build stamp goes with it, under the same condition and for a
    // related reason: it describes the process that was serving this
    // endpoint, so once nothing is answering there it describes nobody. Keyed
    // off the endpoint rather than off the pid file the loop above may have
    // just removed, which would read as "nothing running" whatever the truth.
    if !daemon::is_listening(project_root)? {
        daemon::endpoint(project_root)?.clear_stale();
        let _ = fs::remove_file(daemon::build_stamp_path(project_root)?);
    }
    Ok(())
}

/// Renders an outcome as the text the command prints.
pub fn render(outcome: &Outcome, project_root: &Path) -> String {
    if !outcome.stopped_anything() {
        return format!("g-mesh: no daemon is running for {}\n", project_root.display());
    }

    let mut out = format!("g-mesh: stopped the daemon for {}\n", project_root.display());
    if let Some(core) = outcome.core {
        let _ = writeln!(out, "  daemon core: pid {} ({})", core.pid(), describe(core));
        if outcome.core_was_wedged {
            let _ = writeln!(
                out,
                "    it had removed its own pid file but was still holding this project's \
                 daemon lock, which is why nothing could connect to it"
            );
        }
    }
    for (language, plugin) in &outcome.plugins {
        let _ = writeln!(out, "  plugin ({language}): pid {} ({})", plugin.pid(), describe(*plugin));
    }
    out
}

fn describe(stopped: Stopped) -> &'static str {
    match stopped {
        Stopped::Terminated { .. } => "terminated",
        Stopped::Killed { .. } => "killed after ignoring SIGTERM",
        Stopped::ExitedWithCore { .. } => "exited with its core",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The half of this module's coverage that needs a real process which can
    /// *ignore* being asked to stop - which is what `SIGTERM` is and what
    /// Windows has no equivalent of (`crate::process`). The escalation these
    /// exercise still exists on both platforms; only the second rung's
    /// meaning is Unix-specific, so only these tests are.
    #[cfg(unix)]
    mod signals {
        use super::*;
        use std::process::{Child, Command, Stdio};

        /// A short grace period, so the escalation tests take milliseconds rather
        /// than the production timeout's seconds.
        const TEST_GRACE: Duration = Duration::from_millis(500);

        /// A real child process to signal. Killed on drop, so a failing
        /// assertion cannot leak one.
        struct Victim(Child);

        impl Victim {
            /// Exits on `SIGTERM`, like every process this command signals today.
            fn cooperative() -> Self {
                Self::spawn("sleep 30")
            }

            /// Ignores `SIGTERM` outright - the case the `SIGKILL` escalation
            /// exists for.
            fn stubborn() -> Self {
                Self::spawn("trap '' TERM; sleep 30")
            }

            fn spawn(script: &str) -> Self {
                let child = Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("failed to spawn a test process");
                Self(child)
            }

            fn pid(&self) -> u32 {
                self.0.id()
            }

            /// Reaps it, so `kill(pid, 0)` stops finding a zombie. Signalling a
            /// child of this process is the one place a test has to do the
            /// daemon's own parent's job - in production nothing here is a child
            /// of the process running `stop`.
            fn reap(&mut self) {
                let _ = self.0.wait();
            }
        }

        impl Drop for Victim {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn a_process_that_honors_sigterm_is_terminated_without_escalating() {
            let mut victim = Victim::cooperative();
            let pid = victim.pid();

            process::request_stop(pid).unwrap();
            victim.reap();

            assert!(wait_until_gone(pid, TEST_GRACE), "SIGTERM must be enough for pid {pid}");
        }

        #[test]
        fn a_process_that_ignores_sigterm_is_killed() {
            let mut victim = Victim::stubborn();
            let pid = victim.pid();

            // Give the shell a moment to install its trap, or the test would be
            // asserting about a process that had not started ignoring anything.
            thread::sleep(Duration::from_millis(100));
            process::request_stop(pid).unwrap();
            assert!(
                !wait_until_gone(pid, Duration::from_millis(200)),
                "the stubborn process must survive SIGTERM, or this test proves nothing"
            );

            process::force_stop(pid).unwrap();
            victim.reap();
            assert!(wait_until_gone(pid, TEST_GRACE), "SIGKILL must be enough for pid {pid}");
        }

        /// The race `stop` runs into whenever a process exits between the
        /// liveness check and the signal: not an error, just the outcome arriving
        /// early.
        #[test]
        fn signalling_an_already_dead_process_is_not_an_error() {
            let mut victim = Victim::cooperative();
            let pid = victim.pid();
            victim.0.kill().unwrap();
            victim.reap();
            assert!(wait_until_gone(pid, TEST_GRACE));

            process::request_stop(pid).expect("a process that is already gone is not a failure");
        }

        #[test]
        fn a_pid_file_naming_a_dead_process_reads_as_nothing_running() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("daemon.pid");

            let mut victim = Victim::cooperative();
            fs::write(&path, victim.pid().to_string()).unwrap();
            assert_eq!(live_pid(&path), Some(victim.pid()));

            victim.0.kill().unwrap();
            victim.reap();
            assert!(wait_until_gone(victim.pid(), TEST_GRACE));

            assert_eq!(live_pid(&path), None, "a stale pid file names nothing running");
        }
    }

    #[test]
    fn a_missing_or_unreadable_pid_file_reads_as_nothing_running() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(live_pid(&dir.path().join("absent.pid")), None);

        let garbage = dir.path().join("garbage.pid");
        fs::write(&garbage, b"not a pid").unwrap();
        assert_eq!(live_pid(&garbage), None);
    }

    #[test]
    fn nothing_running_renders_as_a_no_op_rather_than_a_shutdown() {
        let outcome = Outcome::default();
        assert!(!outcome.stopped_anything());

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));
        assert_eq!(rendered, "g-mesh: no daemon is running for /tmp/project\n");
    }

    #[test]
    fn a_shutdown_renders_both_processes_and_how_each_one_went() {
        let outcome = Outcome {
            core: Some(Stopped::Terminated { pid: 111 }),
            plugins: vec![("typescript".to_string(), Stopped::ExitedWithCore { pid: 222 })],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("stopped the daemon for /tmp/project"), "{rendered}");
        assert!(rendered.contains("daemon core: pid 111 (terminated)"), "{rendered}");
        assert!(rendered.contains("plugin (typescript): pid 222 (exited with its core)"), "{rendered}");
    }

    /// Two languages each get their own line, independently described.
    #[test]
    fn a_shutdown_with_two_languages_renders_one_line_each() {
        let outcome = Outcome {
            core: Some(Stopped::Terminated { pid: 111 }),
            plugins: vec![
                ("go".to_string(), Stopped::ExitedWithCore { pid: 222 }),
                ("typescript".to_string(), Stopped::Killed { pid: 333 }),
            ],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("plugin (go): pid 222 (exited with its core)"), "{rendered}");
        assert!(
            rendered.contains("plugin (typescript): pid 333 (killed after ignoring SIGTERM)"),
            "{rendered}"
        );
    }

    #[test]
    fn an_escalated_shutdown_says_so() {
        let outcome = Outcome {
            core: Some(Stopped::Killed { pid: 111 }),
            plugins: vec![("typescript".to_string(), Stopped::Killed { pid: 222 })],
            ..Outcome::default()
        };

        let rendered = render(&outcome, &PathBuf::from("/tmp/project"));

        assert!(rendered.contains("pid 111 (killed after ignoring SIGTERM)"), "{rendered}");
        assert!(rendered.contains("pid 222 (killed after ignoring SIGTERM)"), "{rendered}");
    }
}
