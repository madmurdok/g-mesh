use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::storage::schema::apply;

/// Records that this index is a seed copied from the index of `source_root`
/// (`daemon::seed`), with the source's `bulkIndexedAt` and no `reconciledAt`.
/// Runs on the copy's own connection, before its rename into place.
pub fn record_seed_origin(conn: &Connection, source_root: &str) -> Result<()> {
    apply(conn)?;
    conn.execute(
        "INSERT OR REPLACE INTO seed_origin (id, sourceRoot, sourceBulkIndexedAt, seededAt, reconciledAt) \
         SELECT 1, ?1, bulkIndexedAt, CURRENT_TIMESTAMP, NULL FROM meta WHERE id = 1 AND bulkIndexedAt IS NOT NULL",
        rusqlite::params![source_root],
    )
    .and_then(|written| {
        if written == 1 {
            Ok(())
        } else {
            Err(rusqlite::Error::QueryReturnedNoRows)
        }
    })
    .context("failed to record the seed's origin")
}

/// Whether this index is a seed whose first-start reconcile has not run yet
/// (a `seed_origin` row with `reconciledAt` NULL). Hook point for the seeded
/// start: a daemon must not serve such an index before that
/// reconcile.
pub fn seed_pending(conn: &Connection) -> Result<bool> {
    let row: Option<Option<String>> = conn
        .query_row("SELECT reconciledAt FROM seed_origin WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read seed_origin")?;
    Ok(matches!(row, Some(None)))
}

/// Stamps the seed's first-start reconcile as done.
pub fn record_seed_reconciled(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE seed_origin SET reconciledAt = CURRENT_TIMESTAMP WHERE id = 1", [])
        .context("failed to record that the seed was reconciled")?;
    Ok(())
}

/// The working tree a seeded index was copied from (`seed_origin.sourceRoot`),
/// or `None` for an index that is not a seed.
pub fn seed_source_root(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT sourceRoot FROM seed_origin WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read seed_origin.sourceRoot")
}

/// When the seed's first-start reconcile finished, in Unix seconds; `None`
/// for an index that is not a seed or whose reconcile has not finished.
pub fn seed_reconciled_at_unix(conn: &Connection) -> Result<Option<i64>> {
    let recorded: Option<Option<i64>> = conn
        .query_row(
            "SELECT CAST(strftime('%s', reconciledAt) AS INTEGER) FROM seed_origin WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read seed_origin.reconciledAt")?;
    Ok(recorded.flatten())
}
