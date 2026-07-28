use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Bumped whenever the DDL below changes in a way that isn't backward
/// compatible. No migration framework in v1 - a mismatch means a full
/// wipe-and-reindex, since the index is a reproducible cache, not source
/// of truth.
///
/// Bumped to "2" when `indexed_files` was added (query-time staleness
/// checking - see `watcher::staleness`): it records the on-disk mtime/hash
/// a file's index was last built from, which didn't exist in "1".
pub const CURRENT_SCHEMA_VERSION: &str = "2";

/// DDL per the architecture doc's Data Model erDiagram
/// (docs/architecture/g-mesh-v1.md). `vectors` is deliberately not created
/// here - it's added by the Embeddings epic when sqlite-vec is wired in.
const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    name            TEXT NOT NULL,
    qualifiedName   TEXT NOT NULL,
    filePath        TEXT NOT NULL,
    startLine       INTEGER NOT NULL,
    startCol        INTEGER NOT NULL,
    endLine         INTEGER NOT NULL,
    endCol          INTEGER NOT NULL,
    signature       TEXT,
    exported        INTEGER NOT NULL DEFAULT 0,
    docComment      TEXT,
    language        TEXT NOT NULL,
    nativeKind      TEXT,
    hasSyntaxErrors INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_nodes_filePath ON nodes(filePath);
CREATE INDEX IF NOT EXISTS idx_nodes_qualifiedName ON nodes(qualifiedName);

CREATE TABLE IF NOT EXISTS edges (
    id       TEXT PRIMARY KEY,
    fromId   TEXT NOT NULL REFERENCES nodes(id),
    toId     TEXT NOT NULL REFERENCES nodes(id),
    kind     TEXT NOT NULL,
    source   TEXT NOT NULL CHECK (source IN ('tree-sitter', 'ts-compiler')),
    resolved INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_edges_fromId ON edges(fromId);
CREATE INDEX IF NOT EXISTS idx_edges_toId ON edges(toId);

CREATE TABLE IF NOT EXISTS meta (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version  TEXT NOT NULL,
    embedding_model TEXT,
    lastUsed        TEXT NOT NULL
);

-- Baseline on-disk state (mtime + content hash) a file's index was last
-- built from - see watcher::staleness. mtime is a cheap fast-path check
-- (no file read); contentHash is the authoritative fallback used only when
-- mtime disagrees, so a query touching many files doesn't have to hash all
-- of them every time.
CREATE TABLE IF NOT EXISTS indexed_files (
    filePath    TEXT PRIMARY KEY,
    mtimeMillis INTEGER NOT NULL,
    contentHash TEXT NOT NULL
);
"#;

/// Applies the graph schema DDL to a fresh (or already up-to-date) connection.
pub fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(DDL).context("failed to apply schema DDL")
}

/// Ensures the DB's schema matches [`CURRENT_SCHEMA_VERSION`], wiping and
/// recreating it on mismatch - no migration framework, full reindex is the
/// only upgrade path. Returns `true` if a full reindex is now required
/// (fresh DB or version mismatch), `false` if the existing schema and data
/// were already current and were left untouched.
pub fn ensure_current(conn: &Connection) -> Result<bool> {
    apply(conn)?;

    let existing: Option<String> = conn
        .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read schema_version")?;

    match existing {
        Some(version) if version == CURRENT_SCHEMA_VERSION => Ok(false),
        Some(_) => {
            wipe(conn)?;
            apply(conn)?;
            record_version(conn)?;
            Ok(true)
        }
        None => {
            record_version(conn)?;
            Ok(true)
        }
    }
}

fn wipe(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS edges; DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS meta; DROP TABLE IF EXISTS indexed_files;",
    )
    .context("failed to wipe schema")
}

fn record_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (id, schema_version, lastUsed) VALUES (1, ?1, CURRENT_TIMESTAMP)",
        rusqlite::params![CURRENT_SCHEMA_VERSION],
    )
    .context("failed to record schema_version")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        conn
    }

    #[test]
    fn creates_all_tables_and_indexes() {
        let conn = setup();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.sort();
        assert_eq!(tables, vec!["edges", "indexed_files", "meta", "nodes"]);

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "idx_nodes_filePath",
            "idx_nodes_qualifiedName",
            "idx_edges_fromId",
            "idx_edges_toId",
        ] {
            assert!(indexes.contains(&expected.to_string()), "missing index {expected}");
        }
    }

    #[test]
    fn nodes_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        let (name, kind): (String, String) = conn
            .query_row(
                "SELECT name, kind FROM nodes WHERE id = 'n1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "foo");
        assert_eq!(kind, "Function");
    }

    #[test]
    fn edges_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n2', 'Function', 'bar', 'mod::bar', 'src/lib.rs', 5, 0, 7, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved)
             VALUES ('e1', 'n1', 'n2', 'CALLS', 'tree-sitter', 0)",
            [],
        )
        .unwrap();

        let (kind, resolved): (String, bool) = conn
            .query_row(
                "SELECT kind, resolved FROM edges WHERE id = 'e1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "CALLS");
        assert!(!resolved);
    }

    #[test]
    fn wipes_and_reindexes_on_version_mismatch() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta (id, schema_version, lastUsed) VALUES (1, '0', CURRENT_TIMESTAMP)",
            [],
        )
        .unwrap();

        let reindex_needed = ensure_current(&conn).unwrap();
        assert!(reindex_needed);

        let version: String = conn
            .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 0, "old data must not survive a version mismatch wipe");
    }

    #[test]
    fn leaves_current_version_untouched() {
        let conn = setup();
        // First call on a fresh DB: no meta row yet, so a reindex is (correctly) signaled.
        assert!(ensure_current(&conn).unwrap());

        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        // Second call at the same (current) version must not wipe existing data.
        let reindex_needed = ensure_current(&conn).unwrap();
        assert!(!reindex_needed);

        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 1, "data at the current schema version must survive");
    }

    #[test]
    fn meta_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO meta (id, schema_version, embedding_model, lastUsed)
             VALUES (1, '1', 'jina-embeddings-v2-base-code', '2026-07-27T00:00:00Z')",
            [],
        )
        .unwrap();

        let version: String = conn
            .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, "1");
    }
}
