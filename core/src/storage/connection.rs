use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::identity::project_hash;

/// `~/.g-mesh/projects/`, the one directory every project's state lives
/// under. Public because `cli::clean` enumerates it rather than deriving a
/// single project's path from a root the way everything else here does.
pub fn projects_root() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    Ok(home.join(".g-mesh").join("projects"))
}

/// `~/.g-mesh/projects/<hash>/` for the given (canonicalized) project root.
/// Uses the same hash as the daemon's own socket/pid file location
/// (`daemon::identity::project_hash`) so the two can never disagree.
pub fn project_dir(root: &Path) -> Result<PathBuf> {
    Ok(projects_root()?.join(project_hash(root)?))
}

/// Opens (creating if absent) the project's SQLite index in WAL mode.
pub fn open(root: &Path) -> Result<Connection> {
    let dir = project_dir(root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;

    let db_path = dir.join("index.db");
    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open SQLite database at {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("failed to enable WAL mode")?;
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

        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        let expected_db = project_dir(tmp.path()).unwrap().join("index.db");
        assert!(expected_db.exists());
    }
}
