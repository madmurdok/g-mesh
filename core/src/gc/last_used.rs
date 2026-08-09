//! Keeps `meta.lastUsed` current, and reads it back without a daemon.
//!
//! The column has existed since the first schema, but nothing ever advanced
//! it past the moment the index was created - so every project looked equally
//! idle and the idle duration the GC scan is supposed to key off was not
//! actually recorded anywhere. [`touch`] is what makes it true: the daemon
//! calls it once at startup and once per handled request.
//!
//! # Why it lives in SQLite rather than a stamp file
//!
//! The project's index is already the one file per project that has to be
//! opened to decide anything about it, and writing the timestamp inside the
//! same transaction-protected file rules out the stamp file and the index
//! disagreeing (or a stamp surviving an index that was deleted by hand).
//!
//! # Reading it back needs no running daemon
//!
//! That is the whole point of persisting it: [`read_from_project_dir`] opens
//! `<project dir>/index.db` directly, so a future GC scan over
//! `~/.g-mesh/projects/*` is a cheap sequence of disk reads that never has to
//! bootstrap - or even find - a daemon. Missing, empty and never-initialized
//! directories all read as `None` rather than as errors: a scan over a
//! directory it does not own must not fail on the first oddity it meets.
//!
//! # Time handling
//!
//! Timestamps are written and compared entirely by SQLite - `strftime(...,
//! 'now')` to write, `julianday()` to diff - so no date parsing (and no date
//! dependency) exists on the Rust side. Millisecond precision, not
//! `CURRENT_TIMESTAMP`'s whole seconds, because two requests handled in the
//! same second must still be distinguishable as two touches.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

/// SQLite's `CURRENT_TIMESTAMP` down to the millisecond, in UTC. Compatible
/// with the whole-second stamps `schema::record_version` writes on a fresh
/// index: `julianday()` parses either form.
const NOW_MILLIS: &str = "strftime('%Y-%m-%d %H:%M:%f', 'now')";

/// What a project's index records about the last time it was used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastUsed {
    /// The raw `meta.lastUsed` text, exactly as SQLite recorded it (UTC).
    pub timestamp: String,
    /// How long ago that was. Clamped at zero, so a timestamp that is
    /// somehow in the future (a clock stepped backwards, an index copied
    /// from another machine) reads as "just used" rather than as a negative
    /// duration - the conservative direction for anything deciding what to
    /// delete.
    pub idle: Duration,
}

/// Advances the project's `lastUsed` stamp to now.
///
/// Called on every daemon start and on every handled request, so it has to
/// stay a single cheap `UPDATE` - no read-modify-write, no transaction of its
/// own.
///
/// A project whose `meta` row does not exist yet is left alone (zero rows
/// updated, no error): `schema::ensure_current` creates that row before the
/// daemon does anything else, so in practice this is unreachable, and a
/// caller that got the order wrong should not have its request fail over
/// bookkeeping.
pub fn touch(conn: &Connection) -> Result<()> {
    conn.execute(&format!("UPDATE meta SET lastUsed = {NOW_MILLIS} WHERE id = 1"), [])
        .context("failed to advance meta.lastUsed")?;
    Ok(())
}

/// Reads `lastUsed` (and the idle duration derived from it) out of an open
/// connection. `None` if the index has no `meta` row, or if the recorded
/// timestamp is not one SQLite can parse - an unreadable stamp is reported as
/// "unknown" rather than guessed at in either direction.
pub fn read(conn: &Connection) -> Result<Option<LastUsed>> {
    let row: Option<(String, Option<f64>)> = conn
        .query_row(
            // Diffed by SQLite rather than in Rust so that whatever textual
            // form the stamp is in, exactly one implementation interprets it.
            "SELECT lastUsed, (julianday('now') - julianday(lastUsed)) * 86400.0 FROM meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("failed to read meta.lastUsed")?;

    Ok(row.and_then(|(timestamp, idle_seconds)| {
        // NULL means julianday() could not parse the stamp.
        let seconds = idle_seconds?.max(0.0);
        Some(LastUsed { timestamp, idle: Duration::from_secs_f64(seconds) })
    }))
}

/// Reads `lastUsed` straight out of `<project_dir>/index.db`, with no daemon
/// involved - the read a GC scan over `~/.g-mesh/projects/*` makes for each
/// directory it finds.
///
/// `None` covers every way a directory can have nothing to say: no
/// `index.db`, an `index.db` whose schema was never applied, or a `meta` row
/// that is missing or unparseable. Only a genuinely broken database (one
/// SQLite refuses to open, or that fails mid-query) is an error.
pub fn read_from_project_dir(project_dir: &Path) -> Result<Option<LastUsed>> {
    let db_path = project_dir.join("index.db");
    if !db_path.exists() {
        return Ok(None);
    }

    // Read-write rather than read-only, deliberately: the daemon keeps these
    // databases in WAL mode, and recovering a WAL left behind by a killed
    // daemon needs write access - a read-only open would fail on exactly the
    // abandoned projects a GC scan most wants to look at. Nothing is written.
    // `SQLITE_OPEN_CREATE` is left out so a scan can never conjure an index
    // for a directory that had none.
    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open the project index at {}", db_path.display()))?;

    if !has_meta_table(&conn)? {
        return Ok(None);
    }
    read(&conn)
}

fn has_meta_table(conn: &Connection) -> Result<bool> {
    let found: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("failed to inspect the project index's schema")?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    /// A connection with the `meta` row a daemon would have created by the
    /// time anything touches `lastUsed`.
    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
        conn
    }

    fn stamp(conn: &Connection) -> String {
        read(conn).unwrap().expect("a schema-initialized index always has a lastUsed").timestamp
    }

    /// The acceptance criterion, at unit scale: successive touches - the
    /// daemon's per-request call - each move the recorded stamp forward.
    #[test]
    fn every_touch_advances_the_recorded_stamp() {
        let conn = setup();
        let initial = stamp(&conn);

        // Longer than the millisecond resolution the stamp is written at, so
        // "advanced" is observable rather than a coin flip.
        std::thread::sleep(Duration::from_millis(5));
        touch(&conn).unwrap();
        let after_first = stamp(&conn);
        assert!(after_first > initial, "{after_first} must be later than {initial}");

        std::thread::sleep(Duration::from_millis(5));
        touch(&conn).unwrap();
        let after_second = stamp(&conn);
        assert!(after_second > after_first, "{after_second} must be later than {after_first}");
    }

    #[test]
    fn a_fresh_touch_reads_back_as_no_idle_time() {
        let conn = setup();
        touch(&conn).unwrap();

        let idle = read(&conn).unwrap().unwrap().idle;
        assert!(idle < Duration::from_secs(5), "a just-touched project is not idle: {idle:?}");
    }

    #[test]
    fn idle_duration_is_measured_from_the_recorded_stamp() {
        let conn = setup();
        conn.execute("UPDATE meta SET lastUsed = datetime('now', '-90 days') WHERE id = 1", [])
            .unwrap();

        let idle = read(&conn).unwrap().unwrap().idle;
        let ninety_days = Duration::from_secs(90 * 24 * 60 * 60);
        // A minute of slack for the query's own clock reads, nothing more.
        assert!(
            idle.abs_diff(ninety_days) < Duration::from_secs(60),
            "expected ~90 days idle, got {idle:?}"
        );
    }

    #[test]
    fn a_stamp_in_the_future_reads_as_zero_idle_rather_than_going_negative() {
        let conn = setup();
        conn.execute("UPDATE meta SET lastUsed = datetime('now', '+7 days') WHERE id = 1", [])
            .unwrap();

        assert_eq!(read(&conn).unwrap().unwrap().idle, Duration::ZERO);
    }

    #[test]
    fn an_unparseable_stamp_reads_as_unknown_rather_than_as_expired() {
        let conn = setup();
        conn.execute("UPDATE meta SET lastUsed = 'not a timestamp' WHERE id = 1", []).unwrap();

        assert_eq!(read(&conn).unwrap(), None);
    }

    /// The other half of the acceptance criterion: the value survives the
    /// connection that wrote it and is readable from the directory alone.
    #[test]
    fn the_stamp_is_readable_from_disk_with_nothing_holding_the_database_open() {
        let dir = tempfile::tempdir().unwrap();
        let written = {
            let conn = Connection::open(dir.path().join("index.db")).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
            touch(&conn).unwrap();
            stamp(&conn)
        }; // connection closed here - nothing is holding the index open

        let read_back = read_from_project_dir(dir.path()).unwrap().expect("a stamp was written");
        assert_eq!(read_back.timestamp, written);
        assert!(read_back.idle < Duration::from_secs(5));
    }

    #[test]
    fn a_directory_without_an_index_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_from_project_dir(dir.path()).unwrap(), None);
    }

    /// A daemon killed between creating `index.db` and applying the schema
    /// leaves exactly this behind; a scan has to shrug it off, not fail.
    #[test]
    fn an_index_whose_schema_was_never_applied_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        Connection::open(dir.path().join("index.db")).unwrap();

        assert_eq!(read_from_project_dir(dir.path()).unwrap(), None);
    }

    /// Reading must never create an index for a directory that had none -
    /// otherwise a scan would leave a trail of empty databases behind it.
    #[test]
    fn reading_never_creates_an_index() {
        let dir = tempfile::tempdir().unwrap();
        read_from_project_dir(dir.path()).unwrap();
        assert!(!dir.path().join("index.db").exists());
    }
}
