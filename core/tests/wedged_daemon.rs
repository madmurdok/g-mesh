//! A daemon that is alive, holds its project's singleton lock, and serves
//! nothing - task 184.
//!
//! # Unix only, and it is the staging that is Unix-only, not the bug
//!
//! The state under test is platform-neutral: `daemon::DaemonLock::Wedged` and
//! `shim::evict_wedged_daemon` know nothing about the transport, and a Windows
//! daemon can reach exactly the same shape. What has no Windows equivalent is
//! how these tests *force* it - by unlinking a live daemon's socket file from
//! outside the process. A named pipe has no file to unlink and its name is
//! held by the daemon's own open handle, so nothing outside that process can
//! take its endpoint away from it (see `g_mesh::ipc::windows`). Rather than
//! assert a weaker version of the same thing on Windows, this file is skipped
//! there and the coverage gap is stated plainly.
//!
//! # How the state is forced
//!
//! Not by waiting for a leaked daemon to turn up: the whole difficulty of this
//! bug in the field was that it needed a daemon to reach its idle shutdown and
//! then fail to finish it, which is neither quick nor reliable to provoke. It
//! does not have to be. What made the project unreachable was never *how* the
//! daemon got there, only the shape it was left in - the socket and pid files
//! its own shutdown had already deleted (`daemon::lifecycle
//! ::release_state_files`), with the process still alive and still holding
//! `daemon.lock`. So these tests bootstrap a perfectly ordinary daemon and
//! then delete exactly those files out from under it, which reproduces that
//! shape exactly and in a fraction of a second.
//!
//! That is also why the daemon here dies on `SIGTERM` where the leaked ones in
//! the field reportedly did not: nothing in the daemon installs a signal
//! handler, so a process that ignores `SIGTERM` is not a state this codebase
//! can produce, and none of the recovery below depends on the signal being
//! ignored - `cli::stop::terminate` escalates to `SIGKILL` either way.
//!
//! # What is asserted
//!
//! That such a daemon can no longer keep a project to itself: it is visible to
//! `status` and to `stop` by pid, a shim bootstrapping over it clears it and
//! serves the project rather than timing out, and a daemon started by hand
//! against it fails fast saying which pid is in the way instead of exiting
//! silently. And, on the other side of the same line, that a daemon which *is*
//! serving is never evicted by any of it - the singleton guarantee the lock
//! exists for.

#![cfg(unix)]

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;

use g_mesh::cli::status::{self, CoreState};
use g_mesh::daemon::{self, DaemonLock};
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::{json, Value};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const PROTOCOL_VERSION: &str = "2025-06-18";

struct Project {
    dir: tempfile::TempDir,
    /// Where every daemon this project's shims spawn sends its stderr
    /// (`shim::DAEMON_LOG_ENV`), instead of the `/dev/null` it defaults to.
    ///
    /// GM-355, and it is about what a failure here is allowed to claim. A
    /// daemon can end for reasons that have nothing to do with the shim under
    /// test - a cold-start walk that could not spawn a plugin ends it inside
    /// the first second, which is squarely inside the window these tests look
    /// at. With its stderr discarded, all that reaches the assertion is "the
    /// process is gone", and the message beside it names the only suspect the
    /// test knows about. That is how
    /// `a_serving_daemon_is_reused_by_a_second_shim_and_never_evicted` was
    /// seen blaming a second shim that had, demonstrably, printed nothing and
    /// done nothing. One line of captured stderr is the difference between a
    /// verdict and an accusation.
    ///
    /// Its own directory, not the project root (a file there is something the
    /// daemon would walk and watch) and not the state directory (which the
    /// shim creates, so it does not exist yet when the first shim is spawned).
    logs: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        std::fs::write(dir.path().join("a.ts"), b"export const a = 1;\n")
            .expect("failed to seed the project with a source file");
        let logs = tempfile::tempdir().expect("failed to create a temp daemon log directory");
        Self { dir, logs }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn daemon_log(&self) -> PathBuf {
        self.logs.path().join("daemon.log")
    }

    /// What the daemons behind this project have said, for a failure message
    /// to quote - see [`Project::logs`]. Empty (rather than a panic) when no
    /// daemon has written anything, which is the ordinary case.
    fn daemon_said(&self) -> String {
        match std::fs::read_to_string(self.daemon_log()) {
            Ok(log) if !log.trim().is_empty() => log,
            _ => "(the daemon logged nothing)\n".to_string(),
        }
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

    /// Bootstraps a detached daemon through the shim - the same vehicle
    /// `cli_stop.rs` uses, and for its reason: a daemon spawned as this
    /// process's own child would linger as a zombie that still answers
    /// `kill(pid, 0)` once it is signalled, which is not a state production
    /// can reach.
    fn bootstrap_core(&self) -> u32 {
        let mut shim = self.spawn_shim();
        // Waits for the state every caller is about to *read*, not for a
        // proxy that becomes true earlier. `Serving` is the daemon having
        // published itself as the lock's owner, which is exactly the fact
        // `wedge` below converts into `Wedged` by taking the socket away.
        // Waiting on the pid file instead returned a few lines too soon, and
        // the assertion that followed read `Starting` - a legitimate
        // intermediate state, reported correctly, about a daemon that simply
        // had not got there yet (GM-254).
        //
        // GM-390: `Serving` alone is not that whole fact either, for the same
        // reason the pid file wasn't. `daemon::mod::run` binds the socket
        // (what `Serving` reads), writes the main pid file, and only *then*
        // calls `record_serving_owner` - three separate writes in that order,
        // with nothing serialising a reader against the gaps between them.
        // `wedge` deletes files and immediately asserts `Wedged`, which
        // `inspect_daemon_lock` can only report once `record_serving_owner`
        // has actually run - see its own doc comment: a lock held with no
        // recorded serving owner reads as `Starting`, not `Wedged`, because a
        // holder with nothing recorded yet is "entitled to a moment" rather
        // than wedged. Seen at load average 89 on this machine: `Starting` at
        // `wedge`'s own assertion, immediately after a `bootstrap_core` that
        // had already observed `Serving`. So this waits for both facts to be
        // true together, not just the one `DaemonLock` happens to collapse
        // them into.
        wait_for("the daemon to publish itself as serving", || {
            daemon::inspect_daemon_lock(self.root()).ok() == Some(DaemonLock::Serving)
                && daemon::serving_owner_in(&self.state_dir()).is_some()
        });
        let _ = shim.kill();
        let _ = shim.wait();

        let core = read_pid(&self.pid_file());
        assert!(daemon::is_process_alive(core), "the daemon must be up before it is wedged");
        core
    }

    fn spawn_shim(&self) -> std::process::Child {
        Command::new(BIN)
            .arg("mcp-shim")
            .current_dir(self.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env(g_mesh::shim::DAEMON_LOG_ENV, self.daemon_log())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn the shim")
    }

    /// Leaves this project in the wedged state and returns the pid holding it:
    /// a live daemon, still holding `daemon.lock`, with every file that
    /// describes a running daemon already gone. Exactly what a daemon that
    /// began its idle shutdown and never finished leaves behind - see this
    /// module's header.
    fn wedge(&self) -> u32 {
        let pid = self.bootstrap_core();

        for path in [self.socket(), self.pid_file(), daemon::build_stamp_path(self.root()).unwrap()] {
            let _ = std::fs::remove_file(&path);
        }

        assert!(daemon::is_process_alive(pid), "the wedged daemon must still be alive");
        assert!(
            !daemon::is_listening(self.root()).unwrap(),
            "the wedged daemon must be unreachable - that is what makes it a wedge"
        );
        assert_eq!(
            daemon::inspect_daemon_lock(self.root()).unwrap(),
            DaemonLock::Wedged { pid },
            "the lock must name the process holding this project hostage"
        );
        pid
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
    /// Nothing may outlive one of these tests: a leaked daemon holding a lock
    /// is the very failure under test, and one left behind here would go on to
    /// break unrelated suites. `stop` handles both the healthy and the wedged
    /// shape now, which is itself part of what is being asserted.
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("stop")
            .current_dir(self.root())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

/// GM-390: `daemon::mod::run` binds the listener and only *then* writes the
/// pid file (see the comment on that ordering right above its
/// `write_pid_file` call) - which means `is_listening`/`DaemonLock::Serving`
/// can already be true for a window before this file exists. Every caller
/// here reaches `read_pid` only after waiting for exactly that signal
/// (`bootstrap_core` waits for `Serving`; the shim-bootstrap test waits for
/// `is_listening`), so reading immediately raced that window and lost under
/// load: `failed to read ... No such file or directory` at this line, once
/// locally under load avg 44 and once on CI aarch64-apple-darwin.
///
/// So this waits for the file itself, via the same [`common::wait_for`] /
/// [`common::startup_timeout`] budget [`wait_for`] above already uses for the
/// rest of this family (GM-301). [`daemon::write_pid_file`] renames a
/// complete file into place, so there is no "exists but half-written" state
/// to additionally wait out - once `path` exists its contents are already
/// whatever the writer meant to put there. That is what lets the two failure
/// modes below stay distinct instead of collapsing into one generic message:
/// a file that never shows up within the deadline is the race this comment
/// describes, and a file that shows up holding something unparseable is a
/// different, real bug that waiting longer would not fix.
fn read_pid(path: &Path) -> u32 {
    wait_for(&format!("{} to record a pid", path.display()), || path.exists());

    let contents = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{} existed a moment ago but could not be read now ({e}) - \
             something removed it out from under this read",
            path.display()
        )
    });
    contents.trim().parse().unwrap_or_else(|e| {
        panic!(
            "{} exists but does not hold a parseable pid ({e}): {contents:?} - \
             write_pid_file renames a complete file into place, so this is not the \
             GM-390 race, the file that landed here is simply wrong",
            path.display()
        )
    })
}

/// GM-301: delegates to [`common::wait_for`] with [`common::startup_timeout`]
/// instead of this file's old fixed 20s deadline - part of the same family of
/// flakes as `cli_stop.rs`, seen failing under load in this same session.
fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

/// Drives one real MCP `initialize` through a shim's stdio and returns what
/// it said, so a test can wait for **the shim having arrived and decided**
/// rather than for a guess at how long that takes.
///
/// GM-355. `a_serving_daemon_is_reused_by_a_second_shim_and_never_evicted`
/// used to wait `thread::sleep(500ms)` here, which is the same 500ms
/// `shim::WEDGE_CONFIRMATION` gives a suspected wedge before acting on it -
/// so the test was racing the exact decision it exists to judge, and losing
/// that race looked identical to passing. An answered `initialize` is
/// positive evidence instead: the shim connected to a serving daemon and
/// proxied a request to it. *Which* daemon that was is then the assertions'
/// business, and they are what catch a shim that evicted and replaced one.
///
/// The conversation runs on a helper thread against
/// [`common::startup_timeout`] so a shim that never answers fails the test
/// instead of hanging it - the same shape, and the same budget, as
/// `shim_bootstrap.rs`'s `mcp_tool_names`.
fn initialize_through_shim(shim: &mut Child) -> Value {
    let writer = shim.stdin.take().expect("shim stdin was not piped");
    let reader = shim.stdout.take().expect("shim stdout was not piped");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(initialize(writer, reader));
    });

    let timeout = common::startup_timeout();
    match rx.recv_timeout(timeout) {
        Ok(Ok(answer)) => answer,
        Ok(Err(err)) => panic!("the shim's MCP session failed: {err}"),
        Err(err) => panic!("the shim did not answer initialize within {timeout:?}: {err}"),
    }
}

fn initialize<W: Write, R: Read>(mut writer: W, reader: R) -> Result<Value, String> {
    let mut reader = BufReader::new(reader);
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-wedged-daemon-tests", "version": "0" },
        },
    }))
    .expect("the request is always serializable");
    write_ndjson_frame(&mut writer, &body).map_err(|e| format!("cannot send initialize: {e:#}"))?;

    let frame = read_ndjson_frame(&mut reader)
        .map_err(|e| format!("cannot read the answer: {e:#}"))?
        .ok_or("the shim closed the connection instead of answering")?;
    let answer: Value = serde_json::from_slice(&frame)
        .map_err(|e| format!("the answer is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame)))?;
    if answer["result"]["serverInfo"]["name"] != "g-mesh" {
        return Err(format!("unexpected initialize response: {answer}"));
    }
    Ok(answer)
}

/// Whatever a finished shim wrote to its stderr, for a failure message to
/// quote - see [`Project::logs`] for why a failure here has to say what the
/// other processes involved actually did rather than assert a culprit.
fn shim_said(shim: &mut Child) -> String {
    let Some(mut stderr) = shim.stderr.take() else { return "(stderr already taken)".to_string() };
    let mut said = String::new();
    match stderr.read_to_string(&mut said) {
        Ok(_) if !said.trim().is_empty() => said,
        Ok(_) => "(the shim logged nothing)\n".to_string(),
        Err(err) => format!("(could not read the shim's stderr: {err})\n"),
    }
}

/// `status` used to answer "not running" here, because it reads `daemon.pid`
/// and the wedged daemon deleted its own. The lock is the fact that survives
/// that, and the one the next bootstrap acts on, so this is what `status` has
/// to agree with.
#[test]
fn status_names_the_wedged_daemon_instead_of_reporting_nothing_running() {
    let project = Project::new();
    let pid = project.wedge();

    let report = status::collect(project.root()).expect("status must not need a healthy daemon");

    assert_eq!(report.core, CoreState::Wedged { pid });
    let rendered = status::render(&report);
    assert!(rendered.contains(&format!("wedged (pid {pid})")), "{rendered}");
    assert!(rendered.contains("g-mesh stop"), "the report must say what to do:\n{rendered}");
}

/// `stop` used to report "no daemon is running" while the daemon it could not
/// see held the project. Now it finds it through the lock and stops it - and
/// the project is immediately bootstrappable again, which is the outcome that
/// actually matters.
#[test]
fn stop_clears_a_wedged_daemon_that_no_pid_file_names() {
    let project = Project::new();
    let pid = project.wedge();

    let output = project.stop();

    assert!(output.contains(&format!("daemon core: pid {pid}")), "{output}");
    assert!(
        output.contains("still holding this project's daemon lock"),
        "stop must explain why nothing could reach it:\n{output}"
    );
    assert!(!daemon::is_process_alive(pid), "the wedged daemon (pid {pid}) is still running");
    assert_eq!(daemon::inspect_daemon_lock(project.root()).unwrap(), DaemonLock::Free);

    let replacement = project.bootstrap_core();
    assert_ne!(replacement, pid, "a genuinely new daemon must be serving the project");
}

/// The end-to-end acceptance criterion: an MCP client's shim, arriving at a
/// wedged project, gets a working daemon instead of waiting out its whole
/// bootstrap timeout on a socket that would never appear.
#[test]
fn a_shim_bootstrap_recovers_a_wedged_project_rather_than_timing_out() {
    let project = Project::new();
    let wedged = project.wedge();

    let mut shim = project.spawn_shim();
    wait_for("the shim to bring a working daemon up", || {
        project.pid_file().exists() && daemon::is_listening(project.root()).unwrap_or(false)
    });
    let replacement = read_pid(&project.pid_file());
    let _ = shim.kill();
    let _ = shim.wait();

    assert_ne!(replacement, wedged, "the replacement must be a new process");
    assert!(daemon::is_process_alive(replacement));
    assert!(
        !daemon::is_process_alive(wedged),
        "the wedged daemon (pid {wedged}) must have been cleared, not left holding the lock"
    );
    assert_eq!(daemon::inspect_daemon_lock(project.root()).unwrap(), DaemonLock::Serving);
}

/// The other half of that line, and the one the singleton lock exists for: a
/// daemon that is *answering* is never a candidate for eviction, however many
/// shims arrive. A second shim reuses it; nothing is signalled, nothing is
/// replaced, and there is still exactly one daemon at the end.
///
/// # GM-355: what this waits on, and what it is allowed to conclude
///
/// It used to sleep 500ms and then assert. Both halves of that were wrong in
/// the same direction - they turned a busy machine into a verdict about the
/// daemon.
///
/// The wait is now [`initialize_through_shim`]: the second shim has provably
/// arrived, judged the incumbent and proxied a request to whatever it
/// decided to serve from. 500ms was a guess at that, and a bad one, because
/// `shim::WEDGE_CONFIRMATION` is *also* 500ms - a shim that had wrongly
/// judged this daemon wedged would have been killed by this test mid-decision
/// and the test would have reported success. Slow machine, green test, no
/// coverage. Waiting for the answer removes the race in both directions.
///
/// The conclusion is now allowed to be narrower than the assertion's own
/// wording. "A serving daemon must survive another shim's arrival" names the
/// shim, but the observation underneath it is only "the process is gone", and
/// a daemon ends for reasons that have nothing to do with any shim - a
/// cold-start walk that cannot spawn a plugin ends it inside the first
/// second, which is inside exactly this window. So the failure quotes what
/// the shim and the daemon each actually said (see [`Project::logs`]), and a
/// reader gets to see whether anything evicted anything at all.
#[test]
fn a_serving_daemon_is_reused_by_a_second_shim_and_never_evicted() {
    let project = Project::new();
    let incumbent = project.bootstrap_core();

    let mut second = project.spawn_shim();
    initialize_through_shim(&mut second);
    let _ = second.kill();
    let _ = second.wait();
    let second_said = shim_said(&mut second);

    assert!(
        daemon::is_process_alive(incumbent),
        "a serving daemon must survive another shim's arrival - pid {incumbent} is gone.\n\
         the second shim said:\n{second_said}\nthe daemon(s) said:\n{}",
        project.daemon_said()
    );
    assert_eq!(
        read_pid(&project.pid_file()),
        incumbent,
        "no replacement may have taken over.\nthe second shim said:\n{second_said}"
    );
    assert_eq!(daemon::inspect_daemon_lock(project.root()).unwrap(), DaemonLock::Serving);
}

/// A daemon started by hand against a wedged project must not take over - the
/// lock still excludes it, exactly as it excludes a second daemon from a
/// healthy one - but it must say why it is giving up, and name the pid, rather
/// than exiting silently with "another daemon already serves this project".
#[test]
fn a_daemon_started_over_a_wedged_one_fails_fast_and_names_the_pid() {
    let project = Project::new();
    let wedged = project.wedge();

    let output = Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .output()
        .expect("failed to run the daemon");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(!output.status.success(), "taking over a wedged project must be a failure:\n{stderr}");
    assert!(stderr.contains(&format!("pid {wedged}")), "{stderr}");
    assert!(stderr.contains("g-mesh stop"), "the diagnostic must say what to do:\n{stderr}");
    assert!(
        daemon::is_process_alive(wedged),
        "the singleton lock must still exclude the newcomer - a daemon may not evict its own way in"
    );
}
