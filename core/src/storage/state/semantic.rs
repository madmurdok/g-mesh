use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::languages::language_semantic_pass_done;
use super::{in_savepoint, present_languages};

/// Whether the whole-project semantic pass has ever finished for this index -
/// independent of [`bulk_index_completed`], and deliberately so. `false` with
/// `bulk_index_completed` also `false` just means the project has not been
/// walked yet (the ordinary "brand new index" case, and the pass has nowhere
/// to run before the walk it upgrades). `false` with `bulk_index_completed`
/// `true` is the gap this column exists to name: a walk that finished, and a
/// pass that started (its own record is what set `bulkIndexedAt`, per
/// `daemon::semantic`'s module doc's ordering) but was interrupted - a killed
/// plugin, a crash, a process timeout - before it could finish and record
/// itself. See `daemon::semantic::run_with_registry` / `run_once` for the two
/// call shapes that write this, and `daemon::mod`'s cold start / `cli::init`
/// for the readers that treat the second case as still owed.
pub fn semantic_pass_completed(conn: &Connection) -> Result<bool> {
    let recorded: Option<Option<String>> = conn
        .query_row("SELECT semanticPassAt FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read semanticPassAt")?;
    Ok(matches!(recorded, Some(Some(_))))
}

/// Upserts `language`'s `language_state.semanticPassAt` to now, with no
/// roll-up check - the per-language half [`record_semantic_pass`] builds on.
/// Exists as its own function (rather than folded straight into
/// `record_semantic_pass`) so a test can write one language's row without
/// tripping the roll-up, the same way [`record_language_bulk_indexed`] is
/// kept apart from [`record_bulk_index`] - see
/// `a_two_language_semantic_pass_roll_up_waits_for_the_slower_language`
/// below for why that separation is what makes the roll-up rule testable at
/// all.
///
/// Also removes `language`'s semantic-pending, owed-file, residual and gap
/// rows, in the same savepoint: a completed pass has refreshed every file it
/// owed, never-answered ones included.
pub fn record_language_semantic_pass(conn: &Connection, language: &str) -> Result<()> {
    in_savepoint(conn, || {
        conn.execute(
            "INSERT INTO language_state (language, semanticPassAt) VALUES (?1, CURRENT_TIMESTAMP)
             ON CONFLICT(language) DO UPDATE SET semanticPassAt = excluded.semanticPassAt,
                 semanticPassError = NULL",
            params![language],
        )
        .with_context(|| format!("failed to record that {language}'s semantic pass completed"))?;
        clear_semantic_pending(conn, language)?;
        clear_semantic_leftovers(conn, language)
    })
}

/// Records `language`'s pass once its residual files are all answered or
/// given up: `semanticPassAt` is set, so the
/// language is no longer owed, and the residual row goes. The gap rows stay:
/// they are what `g-mesh status` counts as never answered.
pub fn record_language_semantic_pass_settled(conn: &Connection, language: &str) -> Result<()> {
    in_savepoint(conn, || {
        conn.execute(
            "INSERT INTO language_state (language, semanticPassAt) VALUES (?1, CURRENT_TIMESTAMP)
             ON CONFLICT(language) DO UPDATE SET semanticPassAt = excluded.semanticPassAt,
                 semanticPassError = NULL",
            params![language],
        )
        .with_context(|| format!("failed to record that {language}'s semantic pass settled"))?;
        clear_semantic_pending(conn, language)?;
        clear_owed_files(conn, language)?;
        conn.execute("DELETE FROM semantic_residual WHERE language = ?1", params![language])
            .with_context(|| format!("failed to clear {language}'s semantic residual"))?;
        Ok(())
    })
}

/// Records an incomplete whole-project pass of `language` that named the
/// files it did not finish (`unfinished`): every one of them is owed, one
/// attempt more than before; every other owed or given-up file was answered
/// by this pass and is settled; and the language is marked residual, so the
/// next start asks only the owed files. `semanticPassAt` stays unset and
/// `semanticPassError` is cleared: the pass did not fail. Like a failure, it
/// clears the language's semantic-pending row and keeps its pending-file rows
/// as stale (ADR 0009). Returns how many owed files are left (a file at its
/// last attempt is dropped as a gap instead).
pub fn record_language_semantic_residual(
    conn: &Connection,
    language: &str,
    unfinished: &[String],
    reason: &str,
) -> Result<usize> {
    in_savepoint(conn, || {
        let settled: Vec<String> =
            owed_files(conn, language)?.into_iter().filter(|path| !unfinished.contains(path)).collect();
        conn.execute("DELETE FROM semantic_gap_files WHERE language = ?1", params![language])
            .with_context(|| format!("failed to clear {language}'s never-answered files"))?;
        settle_owed_files(conn, language, &[], &settled, unfinished)?;
        conn.execute(
            "INSERT INTO semantic_residual (language, since, reason)
             VALUES (?1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), ?2)
             ON CONFLICT(language) DO UPDATE SET since = excluded.since, reason = excluded.reason",
            params![language, reason],
        )
        .with_context(|| format!("failed to record {language}'s semantic residual"))?;
        conn.execute(
            "UPDATE language_state SET semanticPassError = NULL WHERE language = ?1",
            params![language],
        )
        .with_context(|| format!("failed to clear {language}'s semantic-pass error"))?;
        clear_semantic_pending_language(conn, language)?;
        Ok(owed_files(conn, language)?.len())
    })
}

/// `language`'s residual files, when its last whole-project pass left some
/// (`semantic_residual`): `Some` of its owed files - possibly none, when
/// edits have answered them all since - or `None` when the language is not
/// residual and is owed a whole-project pass.
pub fn semantic_residual_files(conn: &Connection, language: &str) -> Result<Option<Vec<String>>> {
    let residual: Option<i64> = conn
        .query_row("SELECT 1 FROM semantic_residual WHERE language = ?1", params![language], |row| row.get(0))
        .optional()
        .with_context(|| format!("failed to read whether {language} has a semantic residual"))?;
    match residual {
        Some(_) => owed_files(conn, language).map(Some),
        None => Ok(None),
    }
}

/// The reason the plugin gave for the incomplete pass that made `language`
/// residual, or `None` when it is not residual - what `g-mesh plugins check`
/// quotes when it fails a session on a residual pass.
pub fn semantic_residual_reason(conn: &Connection, language: &str) -> Result<Option<String>> {
    conn.query_row("SELECT reason FROM semantic_residual WHERE language = ?1", params![language], |row| {
        row.get(0)
    })
    .optional()
    .with_context(|| format!("failed to read {language}'s semantic residual reason"))
}

/// What one language has left over from its semantic passes, for
/// `g-mesh status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticLeftover {
    pub language: String,
    /// `Some(owed file count)` when the language is residual: its last
    /// whole-project pass left these files, and the next start asks only them.
    pub residual_files: Option<usize>,
    /// Files given up after [`MAX_OWED_ATTEMPTS`] passes: never answered.
    pub never_answered: usize,
}

/// Every language with a residual row or given-up files, sorted by
/// language. Empty for an index that predates the tables.
pub fn semantic_leftovers(conn: &Connection) -> Result<Vec<SemanticLeftover>> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) = 3 FROM sqlite_master WHERE type = 'table'
             AND name IN ('semantic_residual', 'semantic_gap_files', 'semantic_owed_files')",
            [],
            |row| row.get(0),
        )
        .context("failed to look for the semantic residual tables")?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(
            "SELECT l.language,
                    EXISTS (SELECT 1 FROM semantic_residual r WHERE r.language = l.language),
                    (SELECT COUNT(*) FROM semantic_owed_files o WHERE o.language = l.language),
                    (SELECT COUNT(*) FROM semantic_gap_files g WHERE g.language = l.language)
             FROM (SELECT language FROM semantic_residual UNION SELECT language FROM semantic_gap_files) l
             ORDER BY l.language",
        )
        .context("failed to prepare the semantic leftovers read")?;
    let rows = statement
        .query_map([], |row| {
            let residual: bool = row.get(1)?;
            Ok(SemanticLeftover {
                language: row.get(0)?,
                residual_files: residual.then_some(row.get::<_, i64>(2)? as usize),
                never_answered: row.get::<_, i64>(3)? as usize,
            })
        })
        .context("failed to read the semantic leftovers")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the semantic leftovers")
}

/// Records why `language`'s whole-project semantic pass failed, replacing any
/// earlier reason. `semanticPassAt` is left as it was: a language still owed
/// its pass stays owed, and the next [`record_language_semantic_pass`] clears
/// the reason.
///
/// Also removes `language`'s semantic-pending row, in the same savepoint: no
/// pass is working on its files any more, so the language reads as `absent`
/// rather than `pending`. Its pending-file rows stay: no pass has re-checked
/// those files' edges, so `absent` lists them as stale (ADR 0009).
pub fn record_language_semantic_pass_failure(conn: &Connection, language: &str, reason: &str) -> Result<()> {
    in_savepoint(conn, || {
        conn.execute(
            "INSERT INTO language_state (language, semanticPassError) VALUES (?1, ?2)
             ON CONFLICT(language) DO UPDATE SET semanticPassError = excluded.semanticPassError",
            params![language, reason],
        )
        .with_context(|| format!("failed to record that {language}'s semantic pass failed"))?;
        clear_semantic_pending_language(conn, language)
    })
}

/// Deletes `language`'s rows from both semantic-pending tables.
fn clear_semantic_pending(conn: &Connection, language: &str) -> Result<()> {
    clear_semantic_pending_language(conn, language)?;
    conn.execute("DELETE FROM semantic_pending_files WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s semantic-pending files"))?;
    Ok(())
}

/// Deletes `language`'s `semantic_pending` row only, so the language is no
/// longer `pending`; its `semantic_pending_files` rows stay as stale files.
fn clear_semantic_pending_language(conn: &Connection, language: &str) -> Result<()> {
    conn.execute("DELETE FROM semantic_pending WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s semantic-pending row"))?;
    Ok(())
}

/// When `language`'s pending whole-project semantic pass became owed (the
/// swap's commit time, RFC 3339 UTC), or `None` when no pass is pending for
/// it. Says nothing about whether the pass has since completed: a caller
/// checks [`language_semantic_pass_done`] first.
pub fn semantic_pending_since(conn: &Connection, language: &str) -> Result<Option<String>> {
    conn.query_row("SELECT since FROM semantic_pending WHERE language = ?1", params![language], |row| {
        row.get(0)
    })
    .optional()
    .with_context(|| format!("failed to read {language}'s semantic-pending row"))
}

/// Which of `file_paths` are pending files of `language`, in no particular
/// order.
pub fn semantic_pending_files_among(
    conn: &Connection,
    language: &str,
    file_paths: &[&str],
) -> Result<Vec<String>> {
    // Chunked well under SQLite's bound-parameter limit.
    const CHUNK: usize = 500;
    let mut pending = Vec::new();
    for chunk in file_paths.chunks(CHUNK) {
        let placeholders: Vec<String> = (0..chunk.len()).map(|i| format!("?{}", i + 2)).collect();
        let sql = format!(
            "SELECT filePath FROM semantic_pending_files WHERE language = ?1 AND filePath IN ({})",
            placeholders.join(", ")
        );
        let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&language];
        sql_params.extend(chunk.iter().map(|path| path as &dyn rusqlite::ToSql));
        let mut statement = conn.prepare(&sql).context("failed to prepare the pending-files read")?;
        let rows = statement
            .query_map(sql_params.as_slice(), |row| row.get::<_, String>(0))
            .context("failed to read the pending files")?;
        for row in rows {
            pending.push(row.context("failed to read a pending file")?);
        }
    }
    Ok(pending)
}

/// Records `file_paths` of `language` as semantic-pending files: reparsed
/// structurally, their semantic edges not refreshed yet. A complete per-file
/// pass over them clears the rows ([`clear_semantic_pending_files`]).
pub fn record_semantic_pending_files(conn: &Connection, language: &str, file_paths: &[String]) -> Result<()> {
    in_savepoint(conn, || {
        for path in file_paths {
            conn.execute(
                "INSERT OR IGNORE INTO semantic_pending_files (language, filePath) VALUES (?1, ?2)",
                params![language, path],
            )
            .with_context(|| format!("failed to record the semantic-pending file {path}"))?;
        }
        Ok(())
    })
}

/// Deletes the semantic-pending file rows of `file_paths`, whatever their
/// language (a file belongs to one), after a complete per-file semantic pass
/// over them. The language rows stay: a per-file pass does not re-resolve
/// unchanged call sites elsewhere.
pub fn clear_semantic_pending_files(conn: &Connection, file_paths: &[String]) -> Result<usize> {
    let mut cleared = 0;
    for path in file_paths {
        cleared += conn
            .execute("DELETE FROM semantic_pending_files WHERE filePath = ?1", params![path])
            .with_context(|| format!("failed to clear the pending file {path}"))?;
    }
    Ok(cleared)
}

/// How many semantic passes (per-file ride-alongs and residual passes at
/// start alike) may try one owed file before it stops being re-asked.
/// Bounded so that a file no server ever answers is not asked on every
/// per-file pass, or on every daemon start, for good.
pub const MAX_OWED_ATTEMPTS: i64 = 3;

/// The owed files of `language` (`semantic_owed_files`), in path order: the
/// files earlier per-file passes did not finish, which core adds to the
/// scope of the next one.
pub fn owed_files(conn: &Connection, language: &str) -> Result<Vec<String>> {
    let mut statement = conn
        .prepare("SELECT filePath FROM semantic_owed_files WHERE language = ?1 ORDER BY filePath")
        .context("failed to prepare the owed-files read")?;
    let rows = statement
        .query_map(params![language], |row| row.get::<_, String>(0))
        .with_context(|| format!("failed to read {language}'s owed files"))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("failed to read {language}'s owed files"))
}

/// Settles `language`'s owed files after a per-file pass that reported its
/// `unfinishedFiles`: the `settled` files are no longer owed, and every file
/// in `unfinished` counts one more attempt - restarting at one for a file the
/// pass was sent for (`requested`), whose sites an edit may have changed. A
/// file that has used up [`MAX_OWED_ATTEMPTS`] is dropped, with a log line,
/// and is asked again only when it changes.
pub fn settle_owed_files(
    conn: &Connection,
    language: &str,
    requested: &[String],
    settled: &[String],
    unfinished: &[String],
) -> Result<()> {
    in_savepoint(conn, || {
        for path in settled {
            conn.execute(
                "DELETE FROM semantic_owed_files WHERE language = ?1 AND filePath = ?2",
                params![language, path],
            )
            .with_context(|| format!("failed to settle the owed file {path}"))?;
            conn.execute(
                "DELETE FROM semantic_gap_files WHERE language = ?1 AND filePath = ?2",
                params![language, path],
            )
            .with_context(|| format!("failed to settle the never-answered file {path}"))?;
        }
        for path in unfinished {
            let attempts = if requested.contains(path) {
                1
            } else {
                let earlier: Option<i64> = conn
                    .query_row(
                        "SELECT attempts FROM semantic_owed_files WHERE language = ?1 AND filePath = ?2",
                        params![language, path],
                        |row| row.get(0),
                    )
                    .optional()
                    .with_context(|| format!("failed to read the owed file {path}"))?;
                earlier.unwrap_or(0).saturating_add(1)
            };
            if attempts >= MAX_OWED_ATTEMPTS {
                conn.execute(
                    "DELETE FROM semantic_owed_files WHERE language = ?1 AND filePath = ?2",
                    params![language, path],
                )
                .with_context(|| format!("failed to drop the owed file {path}"))?;
                conn.execute(
                    "INSERT OR IGNORE INTO semantic_gap_files (language, filePath) VALUES (?1, ?2)",
                    params![language, path],
                )
                .with_context(|| format!("failed to record the never-answered file {path}"))?;
                crate::log_line!(
                    "g-mesh: [{language}] {path}: {attempts} semantic pass(es) did not finish it - no longer \
                     re-asked until it changes"
                );
            } else {
                conn.execute(
                    "INSERT INTO semantic_owed_files (language, filePath, attempts) VALUES (?1, ?2, ?3)
                     ON CONFLICT(language, filePath) DO UPDATE SET attempts = excluded.attempts",
                    params![language, path, attempts],
                )
                .with_context(|| format!("failed to record the owed file {path}"))?;
                conn.execute(
                    "DELETE FROM semantic_gap_files WHERE language = ?1 AND filePath = ?2",
                    params![language, path],
                )
                .with_context(|| format!("failed to re-owe the never-answered file {path}"))?;
            }
        }
        Ok(())
    })
}

/// Deletes `language`'s owed-file rows.
pub fn clear_owed_files(conn: &Connection, language: &str) -> Result<()> {
    conn.execute("DELETE FROM semantic_owed_files WHERE language = ?1", params![language])
        .with_context(|| format!("failed to clear {language}'s owed files"))?;
    Ok(())
}

/// Deletes `file_path`'s owed-file and never-answered rows, whatever their
/// language, for a file that is gone. A per-file pass over a deleted file has
/// an empty scope and reports nothing unfinished, so without this its rows
/// would stay until a complete whole-project pass.
pub fn clear_gone_file_semantic_rows(conn: &Connection, file_path: &str) -> Result<()> {
    conn.execute("DELETE FROM semantic_owed_files WHERE filePath = ?1", params![file_path])
        .and_then(|_| conn.execute("DELETE FROM semantic_gap_files WHERE filePath = ?1", params![file_path]))
        .with_context(|| {
            format!("failed to clear the gone file {file_path}'s owed and never-answered rows")
        })?;
    Ok(())
}

/// Deletes `language`'s owed-file, residual and never-answered rows: what a
/// complete whole-project pass answered, or what a workspace reindex swap
/// made stale (the whole-project pass that follows writes fresh ones).
pub fn clear_semantic_leftovers(conn: &Connection, language: &str) -> Result<()> {
    clear_owed_files(conn, language)?;
    conn.execute("DELETE FROM semantic_residual WHERE language = ?1", params![language])
        .and_then(|_| conn.execute("DELETE FROM semantic_gap_files WHERE language = ?1", params![language]))
        .with_context(|| {
            format!("failed to clear {language}'s semantic residual and never-answered files")
        })?;
    Ok(())
}

/// Deletes the semantic-pending rows no pass will ever clear: every row of a
/// language not in `semantic_pass_languages` (its plugin is gone or declares
/// no semantic tier), and the language row of a language whose pass is
/// recorded done (a clear whose write failed). A done language's file rows
/// stay: they are files a per-file or scoped pass still owes (a daemon
/// killed between a seeded reparse and its scoped pass), which the start's
/// [`owed_semantic_pending_files`] pass asks again. Returns how many
/// languages were cleared.
pub fn clear_stale_semantic_pending(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<usize> {
    let languages: Vec<String> = conn
        .prepare("SELECT language FROM semantic_pending UNION SELECT language FROM semantic_pending_files")
        .and_then(|mut statement| statement.query_map([], |row| row.get(0))?.collect())
        .context("failed to read the semantic-pending languages")?;
    let mut cleared = 0;
    for language in languages {
        if !semantic_pass_languages.contains(&language) {
            clear_semantic_pending(conn, &language)?;
            cleared += 1;
        } else if language_semantic_pass_done(conn, &language)?
            && semantic_pending_since(conn, &language)?.is_some()
        {
            clear_semantic_pending_language(conn, &language)?;
            cleared += 1;
        }
    }
    Ok(cleared)
}

/// The `semantic_pending_files` rows a per-file pass still owes, per
/// language, sorted: the rows of each language whose whole-project pass is
/// recorded done and that has no `semantic_pending` row. A language whose
/// pass is not done is left out: the start's whole-project retry
/// (`daemon::semantic`) re-asks every one of its files and clears them.
pub fn owed_semantic_pending_files(conn: &Connection) -> Result<BTreeMap<String, Vec<String>>> {
    let mut statement = conn
        .prepare(
            "SELECT f.language, f.filePath FROM semantic_pending_files f
             JOIN language_state s ON s.language = f.language AND s.semanticPassAt IS NOT NULL
             WHERE NOT EXISTS (SELECT 1 FROM semantic_pending p WHERE p.language = f.language)
             ORDER BY f.language, f.filePath",
        )
        .context("failed to prepare the owed semantic-pending files read")?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .context("failed to read the owed semantic-pending files")?;
    let mut owed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (language, file_path) = row.context("failed to read the owed semantic-pending files")?;
        owed.entry(language).or_default().push(file_path);
    }
    Ok(owed)
}

/// Every `(language, since, pending file count)` with a semantic-pending row,
/// sorted by language. Empty for an index that predates the table. Includes
/// stale rows; a reader filters them as `mcp::provenance` does.
pub fn semantic_pending(conn: &Connection) -> Result<Vec<(String, String, usize)>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'semantic_pending')",
            [],
            |row| row.get(0),
        )
        .context("failed to look for the semantic_pending table")?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(
            "SELECT p.language, p.since,
                    (SELECT COUNT(*) FROM semantic_pending_files f WHERE f.language = p.language)
             FROM semantic_pending p ORDER BY p.language",
        )
        .context("failed to prepare the semantic-pending read")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? as usize)))
        .context("failed to read the semantic-pending languages")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the semantic-pending languages")
}

/// Every `(language, stale file count)` with `semantic_pending_files` rows and
/// no `semantic_pending` row, sorted by language: files a reindex changed that
/// no semantic pass has re-checked since its pass failed, finished residual
/// or did not run. Empty for an index that predates the tables. Includes rows
/// of languages no longer capable or since done; a reader filters them.
pub fn semantic_stale_files(conn: &Connection) -> Result<Vec<(String, usize)>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'semantic_pending_files')",
            [],
            |row| row.get(0),
        )
        .context("failed to look for the semantic_pending_files table")?;
    if !exists {
        return Ok(Vec::new());
    }
    let mut statement = conn
        .prepare(
            "SELECT f.language, COUNT(*) FROM semantic_pending_files f
             WHERE NOT EXISTS (SELECT 1 FROM semantic_pending p WHERE p.language = f.language)
             GROUP BY f.language ORDER BY f.language",
        )
        .context("failed to prepare the stale semantic files read")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get::<_, i64>(1)? as usize)))
        .context("failed to read the stale semantic files")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the stale semantic files")
}

/// Every language whose last whole-project semantic pass failed, with the
/// recorded reason, sorted by language.
pub fn semantic_pass_failures(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut statement = conn
        .prepare(
            "SELECT language, semanticPassError FROM language_state
             WHERE semanticPassError IS NOT NULL ORDER BY language",
        )
        .context("failed to prepare the semantic-pass failure read")?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .context("failed to read semantic-pass failures")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read semantic-pass failures")
}

/// Whether every *present and semantic-pass-capable* language has recorded
/// `language_state.semanticPassAt` - [`record_semantic_pass`]'s roll-up
/// condition, generalized from [`every_present_language_has`]'s
/// single-column check to skip a present language whose manifest declares
/// `capabilities.semantic_pass = false`.
///
/// That skip is the whole point of this being a separate function rather
/// than `every_present_language_has(conn, "semanticPassAt")` reused as-is: a
/// language nobody will ever ask for a pass (the design doc's `plugin.toml`
/// `semantic_pass = false`, the conservative default for a manifest that
/// says nothing) also never gets a `language_state` row written for that
/// column - nothing calls [`record_language_semantic_pass`] for it - so the
/// old, uncapped check would wait on a fact that can never happen and hold
/// `meta.semanticPassAt` hostage forever. `bulkIndexedAt`'s own roll-up
/// ([`record_bulk_index`], still built on [`every_present_language_has`]) has
/// no equivalent gap: every discovered plugin gets walked regardless of its
/// semantic capability, so every present language does eventually write that
/// column.
///
/// `semantic_pass_languages` is the caller's set of languages whose manifest
/// declares `capabilities.semantic_pass = true` - this module reads no
/// capability itself and has no `plugin_manifest` dependency, matching its
/// existing "pure storage, callers bring the facts" boundary; the caller
/// (`daemon::semantic`, which already holds the discovered manifests or a
/// `PluginRegistry` to ask) is what can answer "capable of what".
///
/// Vacuously `true` when no present language is in `semantic_pass_languages`
/// at all - including the empty set, i.e. no discovered plugin anywhere
/// declares the capability - for the same reason
/// [`every_present_language_has`] is vacuously `true` on an empty project:
/// there is nothing to wait on, so a project with no semantic-capable plugin
/// must not be retried forever by `daemon::mod`'s "pass still owed" check.
fn every_present_semantic_language_has_passed(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<bool> {
    Ok(owed_semantic_pass_languages(conn, semantic_pass_languages)?.is_empty())
}

/// Every language among `semantic_pass_languages` that is currently *owed* a
/// whole-project semantic pass - present in the index (at least one `File`
/// node, the same [`present_languages`] definition the roll-up uses) and
/// whose `language_state.semanticPassAt` is still `NULL`.
///
/// This is the "Owed" definition `daemon::semantic::run_with_registry`/
/// `run_once` schedule against: asking again for a language whose
/// pass already completed would repeat real, possibly expensive work (a cold
/// `rust-analyzer`/`tsserver` load) for no new information, and is exactly
/// what makes "an interrupted pass for one language is retried without
/// re-running the other" true rather than aspirational - a retry call only
/// ever asks the languages this returns, never every capable language again.
///
/// A capable language with **no** files in this project yet is not owed
/// either, the same way it is not part of [`every_present_semantic_language_has_passed`]'s
/// roll-up: there is nothing for its semantic tier to improve on, so asking
/// would only cost a plugin spawn (and, for a heavy engine, real memory) for
/// an empty-diff answer.
pub fn owed_semantic_pass_languages(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut owed = Vec::new();
    for language in present_languages(conn)? {
        if !semantic_pass_languages.contains(&language) {
            continue;
        }
        let recorded: Option<Option<String>> = conn
            .query_row(
                "SELECT semanticPassAt FROM language_state WHERE language = ?1",
                params![language],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read language_state.semanticPassAt for {language}"))?;
        if !matches!(recorded, Some(Some(_))) {
            owed.push(language);
        }
    }
    // Sorted: `present_languages` reads `SELECT DISTINCT language FROM
    // nodes`, which carries no ordering guarantee, but
    // `daemon::semantic::run_with_registry`/`run_once` schedule the languages
    // this returns *sequentially* (see that module's doc comment on
    // "Sequential, not concurrent") - a fixed, sorted order is what makes
    // which language runs first (and therefore which one a shared machine's
    // memory pressure hits first) reproducible from run to run, matching
    // `plugin_manifest::semantic_pass_capable_languages`'s own sort for the
    // same reason.
    owed.sort();
    Ok(owed)
}

/// Re-checks the project-wide semantic-pass roll-up and marks
/// `meta.semanticPassAt` if [`every_present_semantic_language_has_passed`]
/// now says yes - the half of [`record_semantic_pass`] that does not write
/// any one language's own row, split out so it can be called on its own.
///
/// That split is needed because the roll-up can become true without any
/// call in the same batch writing a per-language row: a whole-project pass
/// run (`daemon::semantic::run_with_registry`/`run_once`) asks every
/// semantic-pass-capable language, and when that set is empty (nothing
/// discovered declares the capability) or every capable language's row was
/// already set by an earlier run, the loop over languages never calls
/// [`record_language_semantic_pass`] at all - yet the roll-up still has to
/// fire once per run, or a project with no semantic-capable plugin would be
/// reported as "pass still owed" by `daemon::mod`'s cold-start retry on
/// every single daemon start, forever.
pub fn reconcile_semantic_pass_rollup(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<()> {
    // Clears as well as sets, like `record_bulk_index`: meta never claims a
    // pass some present capable language no longer has.
    let sql = if every_present_semantic_language_has_passed(conn, semantic_pass_languages)? {
        "UPDATE meta SET semanticPassAt = CURRENT_TIMESTAMP WHERE id = 1"
    } else {
        "UPDATE meta SET semanticPassAt = NULL WHERE id = 1 AND semanticPassAt IS NOT NULL"
    };
    conn.execute(sql, []).context("failed to record semanticPassAt")?;
    Ok(())
}

/// Marks `language`'s whole-project semantic pass complete, then re-checks
/// the roll-up via [`reconcile_semantic_pass_rollup`] - the one-call
/// convenience for a caller that has exactly one language's completion to
/// record and wants both effects at once (a test, or a single-language
/// caller). `daemon::semantic::run_with_registry`/`run_once` iterate many
/// languages per call, so they use [`record_language_semantic_pass`] and
/// [`reconcile_semantic_pass_rollup`] separately instead - one row write per
/// completed language, one roll-up check at the end of the whole run, not one
/// per language (see that module for why, and for the case with zero
/// completions this function alone would not reach).
///
/// Written only after a caller's own pass attempt for `language` has
/// actually succeeded - never for an `Err` (the pass was asked for and did
/// not finish), which must leave `language`'s row unset so the next attempt
/// retries it. `semantic_pass_languages` is forwarded to
/// [`reconcile_semantic_pass_rollup`] unchanged - see that function and
/// [`every_present_semantic_language_has_passed`] for what it is for.
pub fn record_semantic_pass(
    conn: &Connection,
    language: &str,
    semantic_pass_languages: &HashSet<String>,
) -> Result<()> {
    record_language_semantic_pass(conn, language)?;
    reconcile_semantic_pass_rollup(conn, semantic_pass_languages)
}
