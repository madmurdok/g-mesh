use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::identity::{self, project_hash};
use crate::paths;
use crate::storage::vectors;

/// `~/.g-mesh/projects/` (or `$G_MESH_HOME/projects/`), the one directory
/// every project's state lives under. Public because `cli::clean` enumerates
/// it rather than deriving a single project's path from a root the way
/// everything else here does.
pub fn projects_root() -> Result<PathBuf> {
    Ok(paths::g_mesh_home()?.join("projects"))
}

/// `~/.g-mesh/projects/<hash>/` for the given (canonicalized) project root.
/// Uses the same hash as the daemon's own socket/pid file location
/// (`daemon::identity::project_hash`) so the two can never disagree.
pub fn project_dir(root: &Path) -> Result<PathBuf> {
    Ok(projects_root()?.join(project_hash(root)?))
}

/// The project's state directory, created if absent and recording which
/// project root it belongs to.
///
/// The one place a state directory comes into existence, so that no path can
/// create one without also leaving `project.root` in it - which is what
/// `cli::clean orphaned` reads to tell state whose project was deleted from
/// state whose project is merely idle. `identity::record_project_root` is
/// idempotent, so an existing directory acquires the file the next time
/// anything opens it.
///
/// That claim was false until GM-255. `daemon::run` and
/// `shim::acquire_bootstrap_lock` both created the directory with a bare
/// `fs::create_dir_all` and no root file, and the shim is the *earliest* of
/// the three - so a project whose first contact was a shim got a state
/// directory with no identity. Nothing recovers it later: `project_hash` is
/// one-way, so `clean orphaned` can only class such a directory `Legacy` and
/// leave it alone. Measured on this machine's own test home, 707 of 775
/// directories were unsweepable for that reason.
///
/// So if a fourth call site ever needs a project directory, it goes through
/// here. A bare `create_dir_all` on this path is not a shortcut, it is a
/// directory that can never be cleaned up.
pub fn ensure_project_dir(root: &Path) -> Result<PathBuf> {
    let dir = project_dir(root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;
    identity::record_project_root(&dir, root)?;
    Ok(dir)
}

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
/// is deleted or renamed, the importer's edge is left dangling until that
/// importer is itself reindexed (see `graph::imports::link_diff`). And the TS
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
/// `graph::containers`' empty-container delete, `graph::imports`' placeholder
/// drop and `daemon::workspace_reindex::delete_language_rows` all do this.
/// `ON DELETE CASCADE` in the DDL is still honoured where a connection turns
/// enforcement on (many unit tests do, to catch an edge pointed at a node that
/// was never written, or a delete made in the wrong order); it is not what
/// production relies on.
///
/// Every connection that *writes* a project's index comes through here: the
/// daemon (`daemon::run`), `g-mesh init` and `g-mesh reindex`. The two that
/// open an existing file without `CREATE` - `cli::status::index_status` and
/// `gc::last_used::read_from_project_dir` - deliberately leave the pragma
/// alone: they only read, and foreign-key enforcement only ever affects
/// writes. The one other index core builds, `g-mesh plugins check`'s
/// in-memory one (`cli::plugin_check::session::open_index`), switches it off
/// itself, because it exists to commit diffs exactly as the daemon would.
pub fn open(root: &Path) -> Result<Connection> {
    vectors::register_extension();

    let dir = ensure_project_dir(root)?;

    let db_path = dir.join("index.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open SQLite database at {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL").context("failed to enable WAL mode")?;
    conn.pragma_update(None, "foreign_keys", "OFF").context("failed to disable foreign-key enforcement")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_root_hashes_to_same_directory() {
        let root = std::env::current_dir().unwrap();
        let first = project_dir(&root).unwrap();
        let second = project_dir(&root).unwrap();
        assert_eq!(first, second);
    }

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

    /// Opening an index is the moment a state directory learns which project
    /// it is for - `cli::clean orphaned` can only judge directories that were
    /// created through here.
    #[test]
    fn opening_an_index_records_the_project_root_beside_it() {
        let tmp = tempfile::tempdir().unwrap();

        let _conn = open(tmp.path()).unwrap();

        let state_dir = project_dir(tmp.path()).unwrap();

        let recorded = crate::daemon::identity::read_project_root(&state_dir);
        assert_eq!(recorded, Some(tmp.path().canonicalize().unwrap()));
    }
}
