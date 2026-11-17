//! File-level reads of the graph: which files the index covers, by language,
//! and which ones the plugin flagged.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// The `kind` of the one node a plugin emits per file it parsed.
const FILE_NODE_KIND: &str = "File";

/// Every file the index has a `File` node for. The plugin emits exactly one
/// per file it parsed, which makes this the answer to "which files does the
/// graph cover?" - unlike `indexed_files`, which only the incremental path
/// writes, and unlike `DISTINCT filePath`, which a file with no symbols in it
/// would be missing from.
pub fn indexed_file_paths(conn: &Connection) -> Result<HashSet<String>> {
    let mut statement = conn
        .prepare("SELECT filePath FROM nodes WHERE kind = 'File'")
        .context("failed to query indexed files")?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to read indexed files")?
        .collect::<rusqlite::Result<HashSet<String>>>()
        .context("failed to read indexed files")?;
    Ok(paths)
}

/// Project-relative paths of files the plugin flagged as only partially
/// parseable, sorted.
pub fn files_with_syntax_errors(conn: &Connection) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT DISTINCT filePath FROM nodes WHERE hasSyntaxErrors = 1 ORDER BY filePath")
        .context("failed to query files with syntax errors")?;
    let files = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to read files with syntax errors")?
        .collect::<rusqlite::Result<Vec<String>>>()
        .context("failed to read files with syntax errors")?;
    Ok(files)
}

/// Every indexed `File` path of `language`.
pub(crate) fn indexed_file_paths_of(conn: &Connection, language: &str) -> Result<Vec<String>> {
    conn.prepare_cached("SELECT filePath FROM nodes WHERE kind = 'File' AND language = ?1")
        .and_then(|mut statement| statement.query_map(params![language], |row| row.get(0))?.collect())
        .with_context(|| format!("failed to read {language}'s indexed files"))
}

/// How many `File` nodes `language` has indexed.
pub(crate) fn indexed_file_count(conn: &Connection, language: &str) -> Result<usize> {
    conn.prepare_cached("SELECT COUNT(*) FROM nodes WHERE kind = 'File' AND language = ?1")
        .and_then(|mut statement| statement.query_row(params![language], |row| row.get::<_, i64>(0)))
        .map(|count| count as usize)
        .with_context(|| format!("failed to count {language}'s indexed files"))
}

/// Calls `visit` with `(language, filePath)` of every `File` node.
pub(crate) fn for_each_file_node(conn: &Connection, mut visit: impl FnMut(String, String)) -> Result<()> {
    let mut statement = conn
        .prepare("SELECT language, filePath FROM nodes WHERE kind = 'File'")
        .context("failed to query indexed files")?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .context("failed to read indexed files")?;
    for row in rows {
        let (language, file_path) = row.context("failed to read indexed files")?;
        visit(language, file_path);
    }
    Ok(())
}

/// Every file the index holds anything for: an `indexed_files` baseline or a
/// `File` node.
pub(crate) fn known_file_paths(conn: &Connection) -> Result<HashSet<String>> {
    let mut statement = conn
        .prepare("SELECT filePath FROM indexed_files UNION SELECT filePath FROM nodes WHERE kind = 'File'")
        .context("failed to query indexed files")?;
    let rows =
        statement.query_map([], |row| row.get::<_, String>(0)).context("failed to read indexed files")?;
    rows.collect::<rusqlite::Result<HashSet<String>>>().context("failed to read indexed files")
}

/// Every file path `language`'s nodes carry, sorted. Containers
/// (`filePath = ''`) belong to no file and are left out.
pub(crate) fn file_paths_of_language(conn: &Connection, language: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT filePath FROM nodes WHERE language = ?1 AND filePath <> '' ORDER BY filePath",
        )
        .context("failed to prepare the language's file query")?;
    let rows = stmt
        .query_map([language], |row| row.get(0))
        .context("failed to query the language's files")?
        .collect::<rusqlite::Result<_>>()
        .context("failed to read the language's files")?;
    Ok(rows)
}

/// Each language's `File`-node count.
pub(crate) fn file_counts_by_language(conn: &Connection) -> Result<BTreeMap<String, usize>> {
    let mut stmt = conn
        .prepare("SELECT language, COUNT(*) FROM nodes WHERE kind = ?1 GROUP BY language")
        .context("failed to prepare the per-language file count")?;
    let rows = stmt
        .query_map([FILE_NODE_KIND], |row| {
            Ok((row.get::<_, String>(0)?, usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0)))
        })
        .context("failed to count each language's files")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read each language's file count")
}
