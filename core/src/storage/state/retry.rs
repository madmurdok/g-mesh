use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::languages::language_outcomes;
use crate::storage::language_outcome::LanguageOutcome;

/// How many daemon starts may retry a language whose walk failed, between
/// two full walks. Bounded so that a plugin that always fails costs a fixed
/// number of walks rather than one per start.
pub const MAX_LANGUAGE_RETRIES: u32 = 2;

/// Counts a retry of `language` and returns the new count. Called before the
/// retry walks, so a retry that hangs or takes the daemon down still uses its
/// attempt.
pub fn begin_language_retry(conn: &Connection, language: &str) -> Result<u32> {
    conn.query_row(
        "INSERT INTO language_retry (language, retries, lastRetryAt)
         VALUES (?1, 1, CURRENT_TIMESTAMP)
         ON CONFLICT(language) DO UPDATE SET
            retries = language_retry.retries + 1,
            lastRetryAt = excluded.lastRetryAt
         RETURNING retries",
        params![language],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    .with_context(|| format!("failed to count a retry of {language}"))
}

/// Records that a retry of `language` failed: its `failed` outcome now
/// carries `error`, the latest cause.
pub fn record_language_retry_failed(conn: &Connection, language: &str, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE language_outcome SET error = ?2, recordedAt = CURRENT_TIMESTAMP
         WHERE language = ?1 AND outcome = 'failed'",
        params![language, error],
    )
    .with_context(|| format!("failed to record {language}'s failed retry"))?;
    Ok(())
}

/// Records a successful retry of `language`: its outcome becomes `indexed`
/// and its retry count is dropped. Runs inside the swap's transaction, so the
/// language's rows and its `indexed` outcome land together.
pub fn record_language_retry_succeeded(conn: &Connection, language: &str) -> Result<()> {
    conn.execute(
        "UPDATE language_outcome
         SET outcome = 'indexed', files = NULL, error = NULL, recordedAt = CURRENT_TIMESTAMP
         WHERE language = ?1",
        params![language],
    )
    .with_context(|| format!("failed to record {language} as indexed"))?;
    conn.execute("DELETE FROM language_retry WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s retries"))?;
    Ok(())
}

/// The languages of `discovered` recorded `failed` that have retries left
/// (fewer than [`MAX_LANGUAGE_RETRIES`]), sorted.
pub fn languages_owed_a_retry(conn: &Connection, discovered: &[&str]) -> Result<Vec<String>> {
    let retries = language_retries(conn)?;
    let mut owed: Vec<String> = language_outcomes(conn)?
        .into_iter()
        .filter(|(language, outcome)| {
            matches!(outcome, LanguageOutcome::Failed { .. })
                && discovered.contains(&language.as_str())
                && retries.get(language).copied().unwrap_or(0) < MAX_LANGUAGE_RETRIES
        })
        .map(|(language, _)| language)
        .collect();
    owed.sort();
    Ok(owed)
}

/// Each language's retry count since the last full walk; a language with no
/// retry yet is absent.
pub fn language_retries(conn: &Connection) -> Result<BTreeMap<String, u32>> {
    let mut stmt = conn
        .prepare("SELECT language, retries FROM language_retry")
        .context("failed to prepare the language retry query")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
        .context("failed to query the language retries")?;
    let mut retries = BTreeMap::new();
    for row in rows {
        let (language, n) = row.context("failed to read a language retry")?;
        retries.insert(language, u32::try_from(n).unwrap_or(0));
    }
    Ok(retries)
}
