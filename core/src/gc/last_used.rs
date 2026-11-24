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

use std::path::Path;

use anyhow::Result;

use crate::storage::connection::open_existing;
use crate::storage::state::last_used::has_meta_table;
pub use crate::storage::state::last_used::{read, touch, LastUsed};

/// Reads `lastUsed` straight out of `<project_dir>/index.db`, with no daemon
/// involved - the read a GC scan over `~/.g-mesh/projects/*` makes for each
/// directory it finds.
///
/// `None` covers every way a directory can have nothing to say: no
/// `index.db`, an `index.db` whose schema was never applied, or a `meta` row
/// that is missing or unparseable. Only a genuinely broken database (one
/// SQLite refuses to open, or that fails mid-query) is an error.
pub fn read_from_project_dir(project_dir: &Path) -> Result<Option<LastUsed>> {
    let db_path = crate::paths::state_dir::StateDir::at(project_dir).index_db();
    if !db_path.exists() {
        return Ok(None);
    }

    // Read-write without `CREATE`: recovering the WAL a killed daemon left
    // behind needs write access, and a scan must never conjure an index for a
    // directory that had none.
    let conn = open_existing(&db_path)?;

    if !has_meta_table(&conn)? {
        return Ok(None);
    }
    read(&conn)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rusqlite::Connection;

    use super::*;
    use crate::storage::schema;

    fn stamp(conn: &Connection) -> String {
        read(conn).unwrap().expect("a schema-initialized index always has a lastUsed").timestamp
    }

    /// The other half of the acceptance criterion: the value survives the
    /// connection that wrote it and is readable from the directory alone.
    #[test]
    fn the_stamp_is_readable_from_disk_with_nothing_holding_the_database_open() {
        let dir = tempfile::tempdir().unwrap();
        let written = {
            let conn = Connection::open(dir.path().join("index.db")).unwrap();
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
            schema::ensure_current(&conn, &crate::plugin_manifest::fingerprint::fixture_indexer_version())
                .unwrap();
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
