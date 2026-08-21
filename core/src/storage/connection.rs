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
pub fn open(root: &Path) -> Result<Connection> {
    vectors::register_extension();

    let dir = ensure_project_dir(root)?;

    let db_path = dir.join("index.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open SQLite database at {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL").context("failed to enable WAL mode")?;
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
