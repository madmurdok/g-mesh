//! The one thing every integration test in this directory needs and none of
//! them could express before task 105: "wait until this project's index is
//! actually complete".
//!
//! It used to be free. The daemon bound its socket only after its cold-start
//! bulk walk, so `the pid file exists` - or, through a shim, `initialize
//! answered` - implied a fully built index, and several tests here say so in
//! as many words. Task 105 moved the bind ahead of the walk so that a walk
//! longer than `shim::BOOTSTRAP_TIMEOUT` no longer costs an MCP client its
//! whole tool surface; a daemon is now reachable, and honestly answers "still
//! indexing", well before it can answer anything else.
//!
//! So the fact tests need is no longer the socket but `meta.bulkIndexedAt`,
//! which the daemon writes after the walk's final commit and immediately
//! before it flips the flag those tool answers are gated on. Polling it keeps
//! every existing assertion about *what* gets served, without any of them
//! having to become an assertion about *when*.
//!
//! GM-395 slice 2 made the daemon lazy: it walks nothing until a tool call
//! asks. So polling alone would now wait forever on a daemon nobody has
//! asked, and [`wait_until_indexed`] *triggers* activation first
//! ([`trigger_activation`]) - one throwaway tool call over a raw connection -
//! which keeps every test here on the same lazy path a user runs, rather
//! than behind an eager-mode switch no user has.
//!
//! Living in `tests/common/` rather than being copied into each file for the
//! usual Cargo reason: a subdirectory module is compiled into the test
//! binaries that ask for it, not built as a test binary of its own.

// Each integration test file is its own crate, so cargo warns about anything
// this module offers that *that* crate does not happen to use. Every item
// here is unused by someone, and none of them is dead.
#![allow(dead_code)]

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::ipc;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION;
use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rusqlite::Connection;

/// Generous next to the sub-second walks these fixtures produce; it is a
/// deadlock guard, not a timing assertion - so it is deliberately far above
/// what a healthy walk needs rather than tuned close to it.
///
/// The old 30s was not far enough. The walk competes with whatever else the
/// machine is doing, and the first one after an idle spell also pays for a
/// cold file cache: measured at 7-9s on a quiet machine, but 38.5s during a
/// 2.4.0 release run, which reported an environmental slowdown as a test
/// failure and forced the release gate to be skipped.
const DEFAULT_INDEXED_TIMEOUT_SECS: u64 = 90;

/// [`DEFAULT_INDEXED_TIMEOUT_SECS`], or `G_MESH_TEST_INDEXED_TIMEOUT_SECS` if
/// it is set to a parsable number - an unparsable value falls back to the
/// default rather than failing the run, since a typo in a debugging env var
/// should not look like a product bug.
fn indexed_timeout() -> Duration {
    let secs = std::env::var("G_MESH_TEST_INDEXED_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_INDEXED_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// The other half of this module's timeout story - GM-301. `wait_until_indexed`
/// above is about "did the walk finish"; this is about everything that came
/// before it in a dozen other test files: the pid file existing, the socket
/// answering, the lock reading `Serving`, the plugin pid file showing up. Each
/// of those used to poll against its own hand-picked, file-local constant
/// (10s in most, 20s or 30s in a couple that had already been bitten once) -
/// which is how `cli_stop.rs` and `wedged_daemon.rs` each independently
/// flaked under nothing more exotic than other work sharing the machine:
/// spawning a process and getting it scheduled to bind a socket or write a
/// pid file is not free, and a fixed 10s budget that was never revisited
/// since whoever wrote it had a quiet laptop is not a promise the daemon ever
/// made.
///
/// One number for the whole family fixes that once rather than per file, and
/// overriding it for an unusually loaded box - or an unusually fast one that
/// wants tests to fail faster - takes one env var instead of an edit to
/// thirteen files.
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 60;

/// [`DEFAULT_STARTUP_TIMEOUT_SECS`], or `G_MESH_TEST_STARTUP_TIMEOUT_SECS` if
/// it names a parsable number. Same "unparsable is not fatal" rule as
/// [`indexed_timeout`], for the same reason.
pub fn startup_timeout() -> Duration {
    let secs = std::env::var("G_MESH_TEST_STARTUP_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Polls `ready` every 10ms until it returns `true`, or panics naming `what`
/// once `timeout` has elapsed.
///
/// A hang guard, not a timing assertion: nothing here claims the daemon
/// *should* answer within `timeout`, only that a test that has waited that
/// long is looking at something genuinely stuck rather than something merely
/// slow. Pass [`startup_timeout`] unless a specific test has its own,
/// documented reason to want a different bound - most of this suite's
/// `wait_for` helpers used to duplicate this loop against a file-local
/// constant; this is the one copy.
pub fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what} within {timeout:?}");
}

/// Asks the daemon serving `root` to build its index, the way the first
/// tool call of a real session does (GM-395 slice 2, D14 in
/// `docs/architecture/lazy-indexing.md`), and returns without waiting for
/// the build: activation runs independently of the call that asked for it.
///
/// Retries the connection until [`startup_timeout`], so it is safe to call
/// right after spawning a daemon or a shim that has not bound its endpoint
/// yet.
pub fn trigger_activation(root: &Path) {
    let timeout = startup_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        match try_trigger_activation(root) {
            Ok(()) => return,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "could not trigger activation for {} within {timeout:?}: {err}",
                    root.display()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// One attempt at [`trigger_activation`]: a small synchronous raw-NDJSON MCP
/// client. It completes the handshake (so the session exists before the
/// call is sent), sends one cheap `get_file_outline` - any index-needing
/// tool call triggers - and disconnects without reading the answer. The
/// daemon has already read the call off the connection by the time it sees
/// the disconnect, and rmcp does not abort a request handler whose session
/// ended, so the handler still reaches `prepare`'s `request_activation`.
fn try_trigger_activation(root: &Path) -> Result<(), String> {
    let endpoint = daemon::endpoint(root).map_err(|err| format!("no endpoint: {err:#}"))?;
    let stream = ipc::Stream::connect(&endpoint).map_err(|err| format!("connecting to {endpoint}: {err}"))?;
    let mut writer = stream.try_clone().map_err(|err| format!("cloning the connection: {err}"))?;
    let mut reader = BufReader::new(stream);
    let mut send = |message: serde_json::Value| {
        let body = serde_json::to_vec(&message).expect("a literal message always serializes");
        write_ndjson_frame(&mut writer, &body).map_err(|err| format!("sending {message}: {err:#}"))
    };

    send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-test-trigger", "version": "0" },
        },
    }))?;
    read_ndjson_frame(&mut reader)
        .map_err(|err| format!("reading the initialize response: {err:#}"))?
        .ok_or("the daemon closed the connection instead of answering initialize")?;
    send(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
    send(serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": "\u{0000}" } },
    }))?;
    Ok(())
}

/// Blocks until the daemon serving `root` has recorded a completed
/// cold-start walk, triggering activation first ([`trigger_activation`]) -
/// the daemon walks nothing until something asks.
///
/// Returning means tool calls are already being answered for real, not merely
/// that they are about to be: `daemon::run` flips its in-memory flag *before*
/// it writes this marker, precisely so that an outside observer of the marker
/// can never be told "still indexing" by the daemon that wrote it.
///
/// Safe to call before the daemon exists, or while it is mid-wipe: every way
/// of failing to read the marker (no file, no tables yet, a `DROP TABLE` in
/// flight from `schema::ensure_current`) is treated as "not yet" and retried,
/// which is also what makes it correct to call right after a *restart* that
/// invalidates the index - the wipe clears the marker before the socket is
/// bound, so a caller that waits for the connection first can never read the
/// previous generation's marker and believe the new walk is done.
pub fn wait_until_indexed(root: &Path) {
    wait_until_indexed_within(root, indexed_timeout());
}

/// [`wait_until_indexed`], but with the deadline passed in explicitly instead
/// of always reading [`indexed_timeout`].
///
/// GM-301: exists for callers like `serving_while_indexing.rs` that hold a
/// walk open on purpose (`bulk_index::WALK_DELAY_ENV`) for a duration derived
/// from *this machine's own measured speed*, which under real contention can
/// legitimately be a large fraction of [`indexed_timeout`]'s default 90s - a
/// calibrated hold of, say, 70s left only 20s of that budget for the walk to
/// actually finish once the hold expired, which is not a fair deadline for
/// the same machine that just needed 70s to justify the hold in the first
/// place. Such a caller should pass a deadline that accounts for its own
/// hold, not the bare default.
pub fn wait_until_indexed_within(root: &Path, timeout: Duration) {
    let db = project_dir(root).expect("failed to resolve the state directory").join("index.db");
    let deadline = Instant::now() + timeout;
    // Kept across attempts so the timeout can say *why* the last read failed.
    // Treating every failure as "not yet" is right for the transient cases
    // named above, and it is exactly wrong for a persistent one: a database
    // that can never be opened looks identical to a walk that has not
    // finished, forever. Windows spent two CI rounds on that - the daemon log
    // showed the walk completing while this loop timed out, and the reason it
    // could not be read was thrown away here.
    let mut last_error: Option<String>;
    // Tried once per iteration until it succeeds, rather than up front with
    // `trigger_activation`'s own retry loop: a caller may hand this a root
    // whose index is already complete and whose daemon is not running at
    // all (`g-mesh init` with no session), which must return, not wait out a
    // connection that is never coming.
    let mut triggered: Result<(), String> = Err("not attempted yet".to_string());
    loop {
        last_error = None;
        let indexed = match Connection::open(&db) {
            Ok(conn) => match schema::bulk_index_completed(&conn) {
                Ok(done) => done,
                Err(err) => {
                    last_error = Some(format!("reading the marker: {err}"));
                    false
                }
            },
            Err(err) => {
                last_error = Some(format!("opening {}: {err}", db.display()));
                false
            }
        };
        if indexed {
            return;
        }
        if triggered.is_err() {
            triggered = try_trigger_activation(root);
        }
        // Both ways this can fire are named, because for three releases only
        // the first one was and it was the wrong one every time. A walk that
        // is merely slow finishes eventually and wants a bigger budget; a walk
        // that never started ignores any budget at all, and the way that
        // happens is a shim that went and served a *different* project - see
        // `shim::PROJECT_DIR_ENV` and task 192.
        assert!(
            Instant::now() < deadline,
            "the cold-start bulk walk for {} did not finish within {timeout:?}. Raise \
             G_MESH_TEST_INDEXED_TIMEOUT_SECS if this machine is simply slow - but if a bigger \
             budget changes nothing, no daemon is walking this root at all: check that the shim \
             was not handed an inherited CLAUDE_PROJECT_DIR, and set G_MESH_DAEMON_LOG to see \
             what the daemon that did start was doing.{}{}",
            root.display(),
            match &triggered {
                Ok(()) => String::new(),
                Err(err) => format!("\n\nActivation was never triggered - the last attempt failed: {err}"),
            },
            match &last_error {
                Some(err) => format!(
                    "\n\nThe last attempt failed rather than reporting \
                                      \"not yet\" - which is a third possibility the two above \
                                      do not cover: {err}"
                ),
                None => String::new(),
            }
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Sends `kill -9` to `pid` and does not return until the kernel has actually
/// finished tearing the process down, not merely until the signal was
/// delivered.
///
/// `kill -9` returning only means the signal was *delivered* - its `flock` on
/// `daemon.lock` (`daemon::acquire_singleton_lock`) is released as part of
/// teardown, not the instant the signal lands. A restart spawned immediately
/// after a hand-rolled `stop()` that skipped this wait could race a
/// not-yet-fully-dead process for that lock, lose (`try_lock`'s `WouldBlock`
/// reads as "an incumbent is already serving"), and exit without ever binding
/// a socket - which nothing retries, so a bootstrap budget that follows burns
/// its whole timeout waiting for a socket that was never coming. Polling
/// `daemon::is_process_alive` until it reports dead gives the same
/// confirmation `cli::stop::stop` already gives its own callers (see
/// `cli_stop.rs`'s `wait_for("the daemon to die", ...)`), just applied to a
/// test's hand-rolled kill.
///
pub fn kill_and_wait(pid: u32) {
    force_kill(pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon::is_process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Kills whatever process a pid file names, if it names one that is still
/// running. The three ways a pid file disappoints - absent, empty, or holding
/// something that is not a number - are all the same non-event to a teardown,
/// and they all happen: a test that killed its own daemon leaves the file
/// behind, and a daemon that has created the file but not yet written to it
/// leaves it empty for a window long enough that CI has caught it (GM-242).
///
/// The point of routing every test's teardown through here rather than
/// hand-rolling `kill -9`: on Windows the hand-rolled version killed nothing
/// at all, and a surviving daemon holds an inherited handle to its parent's
/// stdout pipe - so the *test process* could not exit either, long after its
/// own teardown had finished (GM-249).
#[allow(dead_code)]
pub fn kill_pid_file(path: &Path) {
    if let Some(pid) = daemon::read_pid_file(path) {
        kill_and_wait(pid);
    }
}

#[cfg(not(windows))]
fn force_kill(pid: u32) {
    let _ =
        StdCommand::new("kill").arg("-9").arg(pid.to_string()).stderr(std::process::Stdio::null()).status();
}

/// `kill -9` is not merely unavailable on Windows - it is a *different
/// program*. The one Git for Windows ships speaks MSYS pids, not Win32 ones,
/// so handing it a native pid gets `kill: 1840: No such process` and nothing
/// dies. That is not hypothetical: it is in the CI log of every Windows run
/// this suite has ever had, which means teardown there has been a no-op since
/// the port and every integration test leaked its daemon (GM-249).
///
/// `/F` without `/T`, deliberately: this stands in for `kill -9` on one
/// process, and a tree kill would be a different promise. Teardown does not
/// need it - it kills the plugin's pid file separately - and one test asserts
/// that the plugin exits *by itself* when its core dies, which a tree kill
/// would make trivially true.
#[cfg(windows)]
fn force_kill(pid: u32) {
    let _ = StdCommand::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// A plugin discovery root (for `G_MESH_PLUGIN_ROOTS_OVERRIDE`) with one
/// plugin - "python", matching the real bundled plugin's directory name -
/// whose `command` names a `target/debug/` binary that is never created, and
/// that binary's path. A `Cargo.toml` sits beside it so
/// `plugin::missing_plugin_binary_hint`'s "Run `cargo build --workspace` in
/// <root>" branch has a real workspace root to name, exactly like the real
/// repository root does for the genuine bundled plugin (GM-316).
///
/// Shared by `daemon_missing_plugin_binary.rs` and `lazy_activation.rs`; the
/// latter creates the binary afterwards to show that a failed walk is
/// retried.
pub fn missing_workspace_binary_plugin_root() -> (tempfile::TempDir, PathBuf) {
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
