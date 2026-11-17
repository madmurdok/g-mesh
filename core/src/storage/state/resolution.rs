use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// The resolution facts stored for `language`, `None` when there are none.
pub fn resolution_facts(conn: &Connection, language: &str) -> Result<Option<String>> {
    conn.query_row("SELECT facts FROM resolution_facts WHERE language = ?1", params![language], |row| {
        row.get(0)
    })
    .optional()
    .with_context(|| format!("failed to read {language}'s resolution facts"))
}

/// Replaces `language`'s resolution facts and its watch paths together;
/// `None` deletes both, and `watch_paths` is then ignored. The paths are
/// stored as given: a caller on a case-insensitive volume passes them
/// through `daemon::path_case` first. All or nothing, inside or outside a
/// caller's transaction (a savepoint).
pub fn set_resolution_facts(
    conn: &Connection,
    language: &str,
    facts: Option<&str>,
    watch_paths: &[String],
) -> Result<()> {
    conn.execute_batch("SAVEPOINT set_resolution_facts")
        .with_context(|| format!("failed to start storing {language}'s resolution facts"))?;
    let written = write_resolution_facts(conn, language, facts, watch_paths);
    let ended = if written.is_ok() {
        conn.execute_batch("RELEASE set_resolution_facts")
    } else {
        conn.execute_batch("ROLLBACK TO set_resolution_facts; RELEASE set_resolution_facts")
    };
    written?;
    ended.with_context(|| format!("failed to finish storing {language}'s resolution facts"))
}

fn write_resolution_facts(
    conn: &Connection,
    language: &str,
    facts: Option<&str>,
    watch_paths: &[String],
) -> Result<()> {
    conn.execute("DELETE FROM resolution_watch_paths WHERE language = ?1", params![language])
        .with_context(|| format!("failed to drop {language}'s watch paths"))?;
    let Some(facts) = facts else {
        conn.execute("DELETE FROM resolution_facts WHERE language = ?1", params![language])
            .with_context(|| format!("failed to drop {language}'s resolution facts"))?;
        return Ok(());
    };
    conn.execute(
        "INSERT OR REPLACE INTO resolution_facts (language, facts) VALUES (?1, ?2)",
        params![language, facts],
    )
    .with_context(|| format!("failed to store {language}'s resolution facts"))?;
    let mut insert = conn
        .prepare_cached("INSERT OR IGNORE INTO resolution_watch_paths (path, language) VALUES (?1, ?2)")
        .context("failed to prepare the watch-path insert")?;
    for path in watch_paths {
        insert
            .execute(params![path, language])
            .with_context(|| format!("failed to store {language}'s watch path {path}"))?;
    }
    Ok(())
}

/// The languages whose stored watch paths hold exactly `path` (already
/// case-folded by the caller where the volume needs it), sorted.
pub fn watch_path_languages(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare_cached("SELECT language FROM resolution_watch_paths WHERE path = ?1 ORDER BY language")
        .context("failed to prepare the watch-path lookup")?;
    let languages = stmt
        .query_map(params![path], |row| row.get(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
        .with_context(|| format!("failed to look up the watch path {path}"))?;
    Ok(languages)
}

/// `language`'s stored watch paths, sorted.
pub fn watch_paths(conn: &Connection, language: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare_cached("SELECT path FROM resolution_watch_paths WHERE language = ?1 ORDER BY path")
        .context("failed to prepare the watch-path listing")?;
    let paths = stmt
        .query_map(params![language], |row| row.get(0))
        .and_then(|rows| rows.collect::<rusqlite::Result<Vec<String>>>())
        .with_context(|| format!("failed to read {language}'s watch paths"))?;
    Ok(paths)
}
