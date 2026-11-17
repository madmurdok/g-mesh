use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::in_savepoint;

/// Marks `language`'s workspace reindex as started, replacing an older mark.
pub fn mark_pending_reindex(conn: &Connection, language: &str, trigger: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO pending_reindex (language, trigger, startedAt)
         VALUES (?1, ?2, CURRENT_TIMESTAMP)",
        params![language, trigger],
    )
    .with_context(|| format!("failed to mark {language}'s workspace reindex as pending"))?;
    Ok(())
}

/// Every `(language, trigger)` whose workspace reindex was started and never
/// swapped in, sorted by language. Empty for an index that predates the
/// table.
pub fn pending_reindexes(conn: &Connection) -> Result<Vec<(String, String)>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'pending_reindex')",
            [],
            |row| row.get(0),
        )
        .context("failed to look for the pending_reindex table")?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare("SELECT language, trigger FROM pending_reindex ORDER BY language")
        .context("failed to prepare the pending-reindex read")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("failed to read the pending reindexes")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the pending reindexes")
}

/// Records `file_paths` of `language` as owed a re-extract after `trigger`
/// changed (`reextract_owed_files`), keeping any row already there.
pub fn owe_reextracts(conn: &Connection, language: &str, trigger: &str, file_paths: &[String]) -> Result<()> {
    in_savepoint(conn, || {
        for path in file_paths {
            conn.execute(
                "INSERT OR IGNORE INTO reextract_owed_files (language, filePath, trigger) VALUES (?1, ?2, ?3)",
                params![language, path, trigger],
            )
            .with_context(|| format!("failed to record {path} as owed a re-extract"))?;
        }
        Ok(())
    })
}

/// Every language's files owed a re-extract, as `(language, trigger, files)`
/// sorted by language and path; `trigger` is an edited file one of the
/// language's rows names. Empty for an index that predates the table.
pub fn owed_reextracts(conn: &Connection) -> Result<Vec<(String, String, Vec<String>)>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'reextract_owed_files')",
            [],
            |row| row.get(0),
        )
        .context("failed to look for the reextract_owed_files table")?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare("SELECT language, filePath, trigger FROM reextract_owed_files ORDER BY language, filePath")
        .context("failed to prepare the owed re-extract read")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .context("failed to read the owed re-extracts")?;
    let mut owed: Vec<(String, String, Vec<String>)> = Vec::new();
    for row in rows {
        let (language, path, trigger) = row.context("failed to read an owed re-extract")?;
        match owed.last_mut() {
            Some((last, last_trigger, files)) if *last == language => {
                *last_trigger = trigger;
                files.push(path);
            }
            _ => owed.push((language, trigger, vec![path])),
        }
    }
    Ok(owed)
}

/// Deletes the owed re-extracts of `file_paths` in `language`, re-extracted.
pub fn settle_owed_reextracts(conn: &Connection, language: &str, file_paths: &[String]) -> Result<()> {
    in_savepoint(conn, || {
        for path in file_paths {
            conn.execute(
                "DELETE FROM reextract_owed_files WHERE language = ?1 AND filePath = ?2",
                params![language, path],
            )
            .with_context(|| format!("failed to settle the owed re-extract of {path}"))?;
        }
        Ok(())
    })
}

/// Deletes `language`'s owed re-extracts.
pub fn clear_owed_reextracts(conn: &Connection, language: &str) -> Result<()> {
    conn.execute("DELETE FROM reextract_owed_files WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s owed re-extracts"))?;
    Ok(())
}
