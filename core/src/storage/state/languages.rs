use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{every_present_language_has, in_savepoint, present_languages};

use crate::storage::language_outcome::LanguageOutcome;

/// Whether a full project walk has ever finished for this index. `false`
/// means the daemon owes the project a cold-start bulk index - on a fresh or
/// wiped DB, but equally after a walk that was killed part way through, which
/// is why this is its own recorded fact rather than "was the schema just
/// created?".
pub fn bulk_index_completed(conn: &Connection) -> Result<bool> {
    let recorded: Option<Option<String>> = conn
        .query_row("SELECT bulkIndexedAt FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read bulkIndexedAt")?;
    Ok(matches!(recorded, Some(Some(_))))
}

/// `meta.bulkIndexedAt` as Unix seconds (it is stored as SQLite's UTC
/// `CURRENT_TIMESTAMP`); `None` before a walk has completed.
pub fn bulk_indexed_at_unix(conn: &Connection) -> Result<Option<i64>> {
    let recorded: Option<Option<i64>> = conn
        .query_row(
            "SELECT CAST(strftime('%s', bulkIndexedAt) AS INTEGER) FROM meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read bulkIndexedAt")?;
    Ok(recorded.flatten())
}

/// Records `meta.bulkIndexedAt` as the `.gitignore` baseline, after a full
/// project walk has recorded it; nothing when it is unset. A per-language
/// swap must not call this.
pub fn record_gitignore_baseline(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO gitignore_baseline (id, walkedAt) \
         SELECT 1, bulkIndexedAt FROM meta WHERE id = 1 AND bulkIndexedAt IS NOT NULL",
        [],
    )
    .context("failed to record the .gitignore baseline")?;
    Ok(())
}

/// The `.gitignore` baseline ([`record_gitignore_baseline`]) as Unix
/// seconds; `None` before a walk has completed. An index with no baseline row
/// adopts `meta.bulkIndexedAt` as its baseline first, so later per-language
/// swaps no longer move it.
pub fn gitignore_baseline_unix(conn: &Connection) -> Result<Option<i64>> {
    conn.execute(
        "INSERT OR IGNORE INTO gitignore_baseline (id, walkedAt) \
         SELECT 1, bulkIndexedAt FROM meta WHERE id = 1 AND bulkIndexedAt IS NOT NULL",
        [],
    )
    .context("failed to adopt bulkIndexedAt as the .gitignore baseline")?;
    let recorded: Option<Option<i64>> = conn
        .query_row(
            "SELECT CAST(strftime('%s', walkedAt) AS INTEGER) FROM gitignore_baseline WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read the .gitignore baseline")?;
    Ok(recorded.flatten())
}

/// [`present_languages`], each paired with whether that language's
/// `language_state.semanticPassAt` is set. `mcp::instructions` reads only
/// the languages: its text is rendered from manifest capabilities, never
/// from pass state (ADR 0022).
///
/// `pub` rather than folded into a private helper: this is read from
/// `mcp::mod::GMeshMcpServer::get_info`, a synchronous trait method with a
/// plain `&Connection` (via the session's `Mutex` guard) and no roll-up to
/// compute - it wants the raw per-language pairs, not `present_languages`'
/// column-agnostic boolean roll-up shape.
pub fn present_languages_with_semantic_state(conn: &Connection) -> Result<Vec<(String, bool)>> {
    let mut result = Vec::new();
    for language in present_languages(conn)? {
        let passed = language_semantic_pass_done(conn, &language)?;
        result.push((language, passed));
    }
    Ok(result)
}

/// Whether `language`'s whole-project semantic pass has completed at least
/// once - the single-language read
/// [`present_languages_with_semantic_state`] does for every present language
/// at session start, and [`mcp::provenance`](crate::mcp) does for one
/// language per tool call.
///
/// Split out rather than left inline because the two callers want different
/// shapes of the same fact and neither wants the other's: the instructions
/// path needs every present language paired up, a tool call needs exactly the
/// one language its anchor is written in and has no reason to pay a
/// `SELECT DISTINCT` over `nodes` to get there.
///
/// **A `false` here says the pass has not completed, and deliberately does
/// not say why.** The three reasons - never scheduled yet, scheduled and
/// still running, attempted and answered `incomplete` because the language's
/// engine could not be started - are indistinguishable in this table by
/// design (`language_state`'s own DDL comment, and
/// `docs/architecture/multi-language-plugins.md`'s "Semantic engine missing"
/// paragraph). Anything reported to a caller from this value must therefore
/// be true of all three, which is what `mcp::provenance` is careful to say
/// and no more.
pub fn language_semantic_pass_done(conn: &Connection, language: &str) -> Result<bool> {
    let passed: Option<Option<String>> = conn
        .query_row(
            "SELECT semanticPassAt FROM language_state WHERE language = ?1",
            params![language],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("failed to read language_state.semanticPassAt for {language}"))?;
    Ok(matches!(passed, Some(Some(_))))
}

/// Upserts `language`'s `language_state.bulkIndexedAt` to now - the per-
/// language half of [`record_bulk_index`]'s roll-up (design doc: Data Model >
/// Per-language index state). Called once per language from
/// `daemon::bulk_index::walk_one_language`, right after that language's own
/// walk finishes, which is the only place that reliably knows *which*
/// language just completed - `record_bulk_index` itself, called once after
/// every language's walk is done, no longer takes a language at all and only
/// re-checks the roll-up condition.
///
/// `plugin_fingerprint` is `plugin_manifest::fingerprint::fingerprint`'s digest of the
/// plugin build that produced this walk, when the caller has it "readily
/// available" (this task's own scope note) - `walk_one_language` does,
/// because it already holds the `PluginManifest`. `None` leaves whatever
/// `pluginFingerprint` this language already had untouched (via `COALESCE`),
/// rather than overwriting a real value with a gap - the column has no other
/// reader yet (see `language_state`'s own DDL comment), so a caller with
/// nothing to report should not erase what an earlier caller did.
pub fn record_language_bulk_indexed(
    conn: &Connection,
    language: &str,
    plugin_fingerprint: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO language_state (language, bulkIndexedAt, pluginFingerprint)
         VALUES (?1, CURRENT_TIMESTAMP, ?2)
         ON CONFLICT(language) DO UPDATE SET
            bulkIndexedAt = excluded.bulkIndexedAt,
            pluginFingerprint = COALESCE(excluded.pluginFingerprint, language_state.pluginFingerprint)",
        params![language, plugin_fingerprint],
    )
    .with_context(|| format!("failed to record that {language} was bulk-indexed"))?;
    Ok(())
}

/// Re-checks the project-wide roll-up and, if every present language's
/// `language_state.bulkIndexedAt` is now set, marks the project as fully
/// walked. Written only after the last batch of a bulk index has been
/// committed and every language's own [`record_language_bulk_indexed`] call
/// has already landed (`daemon::bulk_index::run` walks every discovered
/// language, in order, before returning - see its own doc comment), so a
/// crash mid-walk leaves both the per-language row and this roll-up unset,
/// and the next daemon start redoes the walk (idempotent - every batch is an
/// upsert).
///
/// Takes no language itself, unlike [`record_language_bulk_indexed`]: every
/// caller here (`daemon::mod`'s cold start, `cli::init`, `cli::reindex`)
/// calls this exactly once, after *every* language's own walk has already
/// recorded its row - the fact this function writes is a property of the
/// whole project, not of any one language, which is what "roll-up" means.
///
/// A reconciler, not only a setter: when some present language lacks the
/// fact, `meta.bulkIndexedAt` is cleared, so a language row reset after the
/// roll-up was written can never leave meta claiming a complete walk.
pub fn record_bulk_index(conn: &Connection) -> Result<()> {
    let sql = if every_present_language_has(conn, "bulkIndexedAt")? {
        "UPDATE meta SET bulkIndexedAt = CURRENT_TIMESTAMP WHERE id = 1"
    } else {
        "UPDATE meta SET bulkIndexedAt = NULL WHERE id = 1 AND bulkIndexedAt IS NOT NULL"
    };
    conn.execute(sql, []).context("failed to record bulkIndexedAt")?;
    Ok(())
}

/// Replaces every recorded language outcome with `outcomes`, in one
/// savepoint, and clears every language's retry count: a full walk gives a
/// language that fails again a fresh set of retries. An `Indexed` row stores no count ([`language_outcomes`] reads
/// it live).
pub fn record_language_outcomes(
    conn: &Connection,
    outcomes: &BTreeMap<String, LanguageOutcome>,
) -> Result<()> {
    in_savepoint(conn, || {
        conn.execute("DELETE FROM language_outcome", []).context("failed to clear the language outcomes")?;
        conn.execute("DELETE FROM language_retry", []).context("failed to clear the language retries")?;
        let mut insert = conn
            .prepare_cached(
                "INSERT INTO language_outcome (language, outcome, files, error, recordedAt)
                 VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)",
            )
            .context("failed to prepare the language outcome insert")?;
        for (language, outcome) in outcomes {
            let (kind, files, error) = match outcome {
                LanguageOutcome::Indexed { .. } => ("indexed", None, None),
                LanguageOutcome::PluginAbsent { files } => {
                    ("plugin_absent", files.map(|n| i64::try_from(n).unwrap_or(i64::MAX)), None)
                }
                LanguageOutcome::Failed { error } => ("failed", None, Some(error.as_str())),
            };
            insert
                .execute(params![language, kind, files, error])
                .with_context(|| format!("failed to record {language}'s outcome"))?;
        }
        Ok(())
    })
}

/// Whether the index has a `language_outcome` table: an index built before
/// per-language outcomes has none.
pub fn has_outcome_table(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'language_outcome')",
        [],
        |row| row.get(0),
    )
    .context("failed to look for the language outcome table")
}

/// Every recorded outcome, sorted by language. `Indexed { files }` is the
/// live `File`-node count. Empty before any walk has recorded outcomes.
pub fn language_outcomes(conn: &Connection) -> Result<Vec<(String, LanguageOutcome)>> {
    let mut stmt = conn
        .prepare(
            "SELECT o.language, o.outcome, o.files, o.error,
                    (SELECT COUNT(*) FROM nodes n WHERE n.kind = 'File' AND n.language = o.language)
             FROM language_outcome o ORDER BY o.language",
        )
        .context("failed to prepare the language outcome query")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .context("failed to query the language outcomes")?;
    let mut outcomes = Vec::new();
    for row in rows {
        let (language, kind, files, error, live_files) = row.context("failed to read a language outcome")?;
        let to_usize = |n: i64| usize::try_from(n).unwrap_or(0);
        let outcome = match kind.as_str() {
            "indexed" => LanguageOutcome::Indexed { files: to_usize(live_files) },
            "plugin_absent" => LanguageOutcome::PluginAbsent { files: files.map(to_usize) },
            "failed" => LanguageOutcome::Failed { error: error.unwrap_or_default() },
            other => anyhow::bail!("unknown language outcome {other:?} for {language}"),
        };
        outcomes.push((language, outcome));
    }
    Ok(outcomes)
}
