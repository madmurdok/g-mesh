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
//! Living in `tests/common/` rather than being copied into each file for the
//! usual Cargo reason: a subdirectory module is compiled into the test
//! binaries that ask for it, not built as a test binary of its own.

use std::path::Path;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use g_mesh::daemon;
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

/// Blocks until the daemon serving `root` has recorded a completed
/// cold-start walk.
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
    let db = project_dir(root).expect("failed to resolve the state directory").join("index.db");
    let timeout = indexed_timeout();
    let deadline = Instant::now() + timeout;
    // Kept across attempts so the timeout can say *why* the last read failed.
    // Treating every failure as "not yet" is right for the transient cases
    // named above, and it is exactly wrong for a persistent one: a database
    // that can never be opened looks identical to a walk that has not
    // finished, forever. Windows spent two CI rounds on that - the daemon log
    // showed the walk completing while this loop timed out, and the reason it
    // could not be read was thrown away here.
    let mut last_error: Option<String>;
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
             what the daemon that did start was doing.{}",
            root.display(),
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
/// `allow(dead_code)`: each integration test file is its own crate, and only
/// two of this module's many consumers hand-roll a kill; cargo has no way to
/// see it is used from the crate that does not include this one.
#[allow(dead_code)]
pub fn kill_and_wait(pid: u32) {
    let _ =
        StdCommand::new("kill").arg("-9").arg(pid.to_string()).stderr(std::process::Stdio::null()).status();
    let deadline = Instant::now() + Duration::from_secs(5);
    while daemon::is_process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}
