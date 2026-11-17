use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::storage::vectors;

pub use crate::paths::state_dir::{ensure_project_dir, project_dir, projects_root};

/// Opens (creating if absent) the project's SQLite index in WAL mode, with
/// the sqlite-vec extension available on the returned connection (see
/// `storage::vectors` for what that unlocks and why registering it here is
/// enough for every connection, not just this one).
///
/// # Foreign keys are switched off here, explicitly
///
/// The schema declares `edges`, `declarations`, `vectors`,
/// `placeholder_targets` and `containers` as foreign keys onto `nodes`, and
/// every writer is built on those declarations *not* being enforced.
/// Cross-file edges are lazy by design: when a symbol another file points at
/// is deleted or renamed, an edge into it that no placeholder carried is left
/// dangling until its file is reindexed, and an import linked to a deleted
/// file until core re-extracts the importer (see `link::imports::link_diff`
/// and `storage::write::apply_diff`). And the TS
/// plugin reports any change to a symbol as a delete plus an upsert of the
/// same id, without re-sending the unchanged edges into it - so under
/// enforcement the delete is refused and the whole diff rolls back.
///
/// This used to be left to the default, on the belief that the default is
/// off. It is off in the `sqlite3` shell and in a stock SQLite build, which is
/// presumably where the belief came from - but `rusqlite`'s `bundled` feature
/// compiles SQLite with `-DSQLITE_DEFAULT_FOREIGN_KEYS=1` (libsqlite3-sys's
/// build script), so the connection this function returned *enforced* them.
/// Every edit after the first in a plugin process's lifetime was refused, and
/// the refusal was swallowed (GM-292; fixed on 2.12.1 by GM-293 and carried
/// into 3.0.0 by GM-294). Setting it here rather than trusting either default
/// is what keeps that from depending on how the dependency happens to be
/// built.
///
/// Because nothing cascades, a delete from `nodes` owes its dependents an
/// explicit delete of their own - `storage::write::apply_diff`,
/// `graph::containers`' empty-container delete, `link::imports`' placeholder
/// drop and `storage::language_swap::swap` all do this.
/// `ON DELETE CASCADE` in the DDL is still honoured where a connection turns
/// enforcement on (many unit tests do, to catch an edge pointed at a node that
/// was never written, or a delete made in the wrong order); it is not what
/// production relies on.
///
/// Every connection that *writes* a project's index comes through here: the
/// daemon (`daemon::run`), `g-mesh init` and `g-mesh reindex`. The readers
/// that open an existing file without `CREATE` ([`open_existing`]) leave the
/// pragma alone: they only read, and foreign-key enforcement only ever affects
/// writes. The one other index core builds, `g-mesh plugins check`'s
/// in-memory one (`cli::plugin_check::session::open_index`), switches it off
/// itself, because it exists to commit diffs exactly as the daemon would.
pub fn open(root: &Path) -> Result<Connection> {
    vectors::register_extension();

    let dir = ensure_project_dir(root)?;

    let db_path = crate::paths::state_dir::StateDir::at(&dir).index_db();
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open SQLite database at {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL").context("failed to enable WAL mode")?;
    conn.pragma_update(None, "foreign_keys", "OFF").context("failed to disable foreign-key enforcement")?;
    // A Writer step runs its units as savepoints inside one transaction; with
    // file temp storage each step's statement journal spills to a temp file.
    conn.pragma_update(None, "temp_store", "MEMORY").context("failed to keep temp storage in memory")?;
    Ok(conn)
}

/// Opens the existing index at `db_path` for reading: read-write without
/// `CREATE`. Recovering a WAL an abandoned daemon left behind needs write
/// access, and a missing database must never be conjured into existence by a
/// reader. Its callers only read.
pub fn open_existing(db_path: &Path) -> Result<Connection> {
    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI)
        .with_context(|| format!("failed to open the project index at {}", db_path.display()))
}

/// Opens (creating) a throwaway index at `path` with the graph schema - the
/// staging file a per-language reindex walks into before its swap
/// (`daemon::workspace_reindex`). Foreign keys off, as in [`open`]; the
/// rollback journal is kept in memory and nothing is synced, because the
/// file is deleted rather than recovered after a crash.
pub fn open_staging(path: &Path) -> Result<Connection> {
    vectors::register_extension();
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open the staging index at {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "MEMORY").context("failed to set the staging journal mode")?;
    conn.pragma_update(None, "synchronous", "OFF").context("failed to set the staging sync mode")?;
    conn.pragma_update(None, "foreign_keys", "OFF").context("failed to disable foreign-key enforcement")?;
    crate::storage::schema::apply(&conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_database_in_wal_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open(tmp.path()).unwrap();

        let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let expected_db = project_dir(tmp.path()).unwrap().join("index.db");
        assert!(expected_db.exists());
    }

    /// GM-293, carried into 3.0.0 by GM-294. The bundled SQLite this crate
    /// links is compiled with `SQLITE_DEFAULT_FOREIGN_KEYS=1`, so leaving the
    /// pragma alone means enforcement is *on* - the opposite of what the
    /// write path is built for. Asked of the connection itself rather than of
    /// the build, so the test keeps meaning something if the dependency or
    /// its defines change.
    #[test]
    fn opens_database_with_foreign_keys_off() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open(tmp.path()).unwrap();

        let enforced: i64 = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0)).unwrap();
        assert_eq!(enforced, 0, "the daemon's index connection must not enforce foreign keys");
    }

    /// `temp_store = MEMORY` is 2. Asked of the connection, so a build define
    /// that forces file temp storage would show here too.
    #[test]
    fn opens_database_with_temp_storage_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = open(tmp.path()).unwrap();

        let temp_store: i64 = conn.pragma_query_value(None, "temp_store", |row| row.get(0)).unwrap();
        assert_eq!(temp_store, 2, "savepoint journals of a Writer step must not spill to temp files");
    }

    /// Opening an index is the moment a state directory learns which project
    /// it is for - `cli::clean orphaned` can only judge directories that were
    /// created through here.
    #[test]
    fn opening_an_index_records_the_project_root_beside_it() {
        let tmp = tempfile::tempdir().unwrap();

        let _conn = open(tmp.path()).unwrap();

        let state_dir = project_dir(tmp.path()).unwrap();

        let recorded = crate::paths::state_dir::read_project_root(&state_dir);
        assert_eq!(recorded, Some(tmp.path().canonicalize().unwrap()));
    }
}
