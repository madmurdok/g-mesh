use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// Records the project's current active embedding model, so `meta` reflects
/// what `embedding::EmbeddingPipeline` actually tagged its rows with rather
/// than staying `NULL` forever (see the DDL comment on `vectors` for why the
/// two columns are a distinct fact from each other, and `EmbeddingPipeline`'s
/// module doc for why this is written on every `apply`, not once). A plain
/// `UPDATE` rather than a schema column type built for a fixed set of models:
/// a future model switch is a data change to this one row, never a migration.
pub fn set_embedding_model(conn: &Connection, model: &str) -> Result<()> {
    conn.execute("UPDATE meta SET embedding_model = ?1 WHERE id = 1", rusqlite::params![model])
        .context("failed to record the active embedding model")?;
    Ok(())
}

/// The noise-rule generation whose vectors this index last purged
/// (`embedding_noise_purge.ruleVersion`), or `None` if it never ran one.
pub fn noise_purge_rule(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("SELECT ruleVersion FROM embedding_noise_purge WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read embedding_noise_purge")
}

/// Records that this index purged the vectors noise-rule generation `rule`
/// filters.
pub fn record_noise_purge(conn: &Connection, rule: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO embedding_noise_purge (id, ruleVersion, purgedAt) VALUES (1, ?1, CURRENT_TIMESTAMP)",
        params![rule],
    )
    .context("failed to record the embedding noise purge")?;
    Ok(())
}
