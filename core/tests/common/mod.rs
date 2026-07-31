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
use std::time::{Duration, Instant};

use g_mesh::storage::connection::project_dir;
use g_mesh::storage::schema;
use rusqlite::Connection;

/// Generous next to the sub-second walks these fixtures produce; it is a
/// deadlock guard, not a timing assertion.
const INDEXED_TIMEOUT: Duration = Duration::from_secs(30);

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
    let deadline = Instant::now() + INDEXED_TIMEOUT;
    loop {
        let indexed = Connection::open(&db)
            .ok()
            .and_then(|conn| schema::bulk_index_completed(&conn).ok())
            .unwrap_or(false);
        if indexed {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the cold-start bulk walk for {} did not finish within {INDEXED_TIMEOUT:?}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
