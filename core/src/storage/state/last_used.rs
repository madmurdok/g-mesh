//! `meta.lastUsed`: when the project's index was last used, written by the
//! daemon and read back by `gc` without one.
//!
//! Timestamps are written and compared entirely by SQLite - `strftime(...,
//! 'now')` to write, `julianday()` to diff - so no date parsing exists on the
//! Rust side. Millisecond precision, because two requests handled in the same
//! second must still be distinguishable as two touches.

use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

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

/// Whether the index has a `meta` table: an index whose schema was never
/// applied has none.
pub fn has_meta_table(conn: &Connection) -> Result<bool> {
    let found: Option<String> = conn
        .query_row("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |row| {
            row.get(0)
        })
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
        conn.execute("UPDATE meta SET lastUsed = datetime('now', '-90 days') WHERE id = 1", []).unwrap();

        let idle = read(&conn).unwrap().unwrap().idle;
        let ninety_days = Duration::from_secs(90 * 24 * 60 * 60);
        // A minute of slack for the query's own clock reads, nothing more.
        assert!(idle.abs_diff(ninety_days) < Duration::from_secs(60), "expected ~90 days idle, got {idle:?}");
    }

    #[test]
    fn a_stamp_in_the_future_reads_as_zero_idle_rather_than_going_negative() {
        let conn = setup();
        conn.execute("UPDATE meta SET lastUsed = datetime('now', '+7 days') WHERE id = 1", []).unwrap();

        assert_eq!(read(&conn).unwrap().unwrap().idle, Duration::ZERO);
    }

    #[test]
    fn an_unparseable_stamp_reads_as_unknown_rather_than_as_expired() {
        let conn = setup();
        conn.execute("UPDATE meta SET lastUsed = 'not a timestamp' WHERE id = 1", []).unwrap();

        assert_eq!(read(&conn).unwrap(), None);
    }
}
