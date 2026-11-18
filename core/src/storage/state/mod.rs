pub mod embedding_meta;
pub mod languages;
pub mod last_used;
pub mod reindex;
pub mod resolution;
pub mod retry;
pub mod seed;
pub mod semantic;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// Every language currently "present" in the index, for the roll-up
/// [`record_bulk_index`]/[`record_semantic_pass`] compute below - defined as
/// "has at least one `File` node", not "was discovered" or "has a
/// `language_state` row". A plugin that was *discovered* but never matched a
/// single file in this project (the multi-language test fixtures,
/// say) would otherwise block the roll-up forever waiting on a fact that
/// language has no reason to ever record; a `File`-node count is the same
/// signal `daemon::semantic::indexed_file_count` already uses to size a
/// pass's timeout, for the same underlying reason - it is the cheapest thing
/// already in the table that means "this language actually has content
/// here".
pub(super) fn present_languages(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT language FROM nodes WHERE kind = 'File'")
        .context("failed to prepare the present-language query")?;
    let rows = stmt.query_map([], |row| row.get(0)).context("failed to query the present languages")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the present language set")
}

/// Whether every language [`present_languages`] names has a non-NULL
/// `language_state.<column>` - the shared roll-up condition behind both
/// [`record_bulk_index`] and [`record_semantic_pass`]. `column` is always one
/// of this module's own two literal strings (`"bulkIndexedAt"` /
/// `"semanticPassAt"`), never external input, so interpolating it into the
/// query text is as safe as it would be to hand-write two near-identical
/// functions - which this exists to avoid.
///
/// Vacuously `true` when no language is present at all (an index with no
/// `File` nodes yet, or a project none of whose discovered plugins matched
/// anything): there is nothing to wait on, so the roll-up fires immediately,
/// exactly like the old unconditional `UPDATE meta SET ... = CURRENT_TIMESTAMP`
/// this replaces did for the same case.
pub(super) fn every_present_language_has(conn: &Connection, column: &str) -> Result<bool> {
    for language in present_languages(conn)? {
        let recorded: Option<Option<String>> = conn
            .query_row(
                &format!("SELECT {column} FROM language_state WHERE language = ?1"),
                params![language],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read language_state.{column} for {language}"))?;
        if !matches!(recorded, Some(Some(_))) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Runs `body` inside a savepoint on `conn`, so its statements commit or roll
/// back together whether or not the caller already holds a transaction.
pub(super) fn in_savepoint<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT schema_record").context("failed to open a savepoint")?;
    match body() {
        Ok(value) => {
            conn.execute_batch("RELEASE schema_record").context("failed to release a savepoint")?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback) = conn.execute_batch("ROLLBACK TO schema_record; RELEASE schema_record") {
                crate::log_line!("g-mesh: failed to roll back a savepoint ({rollback:#})");
            }
            Err(err)
        }
    }
}
