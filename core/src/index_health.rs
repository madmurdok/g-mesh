//! What a project's index covers and still owes, read from `index.db` and,
//! in [`Mode::Full`], against a walk of the project: the data half of
//! `g-mesh status`, which renders it, shared with the daemon's MCP
//! instructions and tests.
//!
//! Every read goes through `graph` and `storage`; no SQL lives here.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::fsutil::mtime_millis;
use crate::graph::files;
use crate::plugin_manifest::{self, DiscoveredPlugins};
use crate::project_walk;
use crate::storage::connection::open_existing;
use crate::storage::file_rows;
use crate::storage::language_outcome::LanguageOutcome;
use crate::storage::schema::{self, SemanticLeftover, NOT_RUN_REASON};

/// What the index covers, and what it still owes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStatus {
    /// Whether a full project walk has ever finished (`meta.bulkIndexedAt`).
    pub bulk_indexed: bool,
    /// Whether the whole-project semantic pass after a walk has ever finished
    /// (`meta.semanticPassAt`). Independent of `bulk_indexed`: every walk
    /// records `bulkIndexedAt` before asking for this pass, so an interrupted
    /// pass leaves `bulk_indexed` true and this false, the combination
    /// `cli::status::render` calls out (see `daemon::semantic`'s module doc).
    pub semantic_pass_completed: bool,
    /// Semantic-pass-capable languages present in the index whose pass has
    /// not completed (`language_state.semanticPassAt` unset), sorted.
    pub semantic_pass_owed: Vec<String>,
    /// `(language, reason)` for every language whose last whole-project
    /// semantic pass failed (`language_state.semanticPassError`), sorted.
    pub semantic_pass_failures: Vec<(String, String)>,
    /// `(language, trigger)` for every workspace reindex that started and
    /// never swapped in (`pending_reindex`), sorted: the previous graph of
    /// the language still serves until it runs again.
    pub pending_reindex: Vec<(String, String)>,
    /// `(language, since, file count)` for every semantic-pass-capable
    /// language whose pass is owed after a workspace reindex swap and not
    /// done (`semantic_pending`, ADR 0009), sorted.
    pub semantic_pending: Vec<(String, String, usize)>,
    /// `(language, file count)` for every semantic-pass-capable language not
    /// done whose files a workspace reindex changed and whose pass then
    /// failed, finished residual or did not run: no pass has re-checked those
    /// files' semantic edges (`semantic_pending_files` without a
    /// `semantic_pending` row, ADR 0009), sorted.
    pub semantic_stale: Vec<(String, usize)>,
    /// Per semantic-pass-capable language: the files its last whole-project
    /// pass left for the next start (residual) and the files no pass
    /// ever answered, sorted by language.
    pub semantic_leftovers: Vec<SemanticLeftover>,
    /// What the project walk found against the index; `None` in
    /// [`Mode::Light`], which does not walk the project.
    pub coverage: Option<Coverage>,
    /// Project-relative paths of files the plugin flagged as only partially
    /// parseable, sorted.
    pub syntax_error_files: Vec<String>,
}

/// Which `status` view to build: [`Mode::Light`] reads only state files and
/// the index; [`Mode::Full`] also walks the project for [`Coverage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Light,
    Full,
}

/// The index against the project on disk, from one gitignore-aware walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Source files found on disk now - the denominator of coverage.
    pub discovered: usize,
    /// How many of those the index has a `File` node for.
    pub indexed: usize,
    /// Files the index still owes work for: never indexed, or indexed against
    /// a recorded baseline that no longer matches the file on disk.
    ///
    /// A bulk-indexed file with no `indexed_files` row is *not* counted: the
    /// walk leaves no baseline when it cannot prove it saw the current bytes
    /// (`ingest::staleness::record_walk_baselines`), and older indexes have
    /// none at all, so a missing baseline means "not checked since", not
    /// "stale".
    pub dirty: usize,
}

impl Coverage {
    /// Indexed files over discovered files. A project with no source files at
    /// all is fully covered rather than a division by zero.
    pub fn ratio(&self) -> f64 {
        if self.discovered == 0 {
            return 1.0;
        }
        self.indexed as f64 / self.discovered as f64
    }
}

/// What the index records per language, as far as this index can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageSection {
    /// No `index.db` yet.
    NoIndex,
    /// The directory is a front (D11): there is no index to read.
    Front,
    /// The index has no `language_outcome` table: it was built by a schema
    /// before per-language outcomes, and the next daemon start rebuilds it.
    /// `schema_version` is `meta.schema_version` when readable.
    PredatesOutcomes { schema_version: Option<String> },
    /// No outcome recorded, and a live daemon is walking (or about to walk)
    /// the project: the walk records them when it finishes.
    WalkInProgress,
    /// No outcome recorded and no walk under way.
    NoneRecorded,
    /// The last walk's outcomes, sorted by language
    /// (`storage::schema::language_outcomes`).
    Recorded(Vec<(String, LanguageOutcome)>),
}

/// What the index at `db_path` records per language. `walk_live` is whether
/// a live daemon's phase is `walking` or `unindexed`, which tells "not
/// recorded yet" from "none recorded" on an index with no outcome rows.
/// Probes for the table first: an index from before per-language outcomes
/// has none, and says so rather than looking like an empty list.
pub fn language_section(db_path: &Path, walk_live: bool) -> Result<LanguageSection> {
    if !db_path.exists() {
        return Ok(LanguageSection::NoIndex);
    }
    let conn = open_existing(db_path)?;
    if !schema::has_outcome_table(&conn)? {
        let schema_version = schema::stored_schema_version(&conn);
        return Ok(LanguageSection::PredatesOutcomes { schema_version });
    }
    let outcomes = schema::language_outcomes(&conn)?;
    Ok(if !outcomes.is_empty() {
        LanguageSection::Recorded(outcomes)
    } else if walk_live {
        LanguageSection::WalkInProgress
    } else {
        LanguageSection::NoneRecorded
    })
}

/// Each language's automatic retries (`language_retry`), empty when the index
/// has no such table yet or cannot be read: the count only adds to a failed
/// line, so a failure to read it costs that clause, not the report.
pub fn language_retries(db_path: &Path) -> BTreeMap<String, u32> {
    let read = || -> Result<BTreeMap<String, u32>> {
        let conn = open_existing(db_path)?;
        schema::language_retries_if_present(&conn)
    };
    read().unwrap_or_default()
}

/// What the index knows, and in [`Mode::Full`] how it compares with what is
/// on disk. Takes the database path explicitly so it can be tested against a
/// hand-built index.
pub fn index_status(
    project_root: &Path,
    db_path: &Path,
    plugins: &DiscoveredPlugins,
    mode: Mode,
) -> Result<IndexStatus> {
    let discovered = match mode {
        Mode::Full => Some(discover_source_files(project_root, plugins)?),
        Mode::Light => None,
    };

    // No index yet (never bootstrapped, or deleted by hand): everything on
    // disk is owed work, and none of it is covered.
    if !db_path.exists() {
        return Ok(IndexStatus {
            bulk_indexed: false,
            semantic_pass_completed: false,
            semantic_pass_owed: Vec::new(),
            semantic_pass_failures: Vec::new(),
            pending_reindex: Vec::new(),
            semantic_pending: Vec::new(),
            semantic_stale: Vec::new(),
            semantic_leftovers: Vec::new(),
            coverage: discovered.map(|files| Coverage {
                discovered: files.len(),
                indexed: 0,
                dirty: files.len(),
            }),
            syntax_error_files: Vec::new(),
        });
    }

    let conn = open_existing(db_path)?;
    let coverage = match discovered {
        Some(files) => Some(coverage(&conn, &files)?),
        None => None,
    };

    let capable: HashSet<String> =
        plugin_manifest::semantic_pass_capable_languages(&plugins.manifests).into_iter().collect();
    let (semantic_pass_owed, semantic_pass_failures) = semantic_pass_state(&conn, &capable)?;
    Ok(IndexStatus {
        bulk_indexed: schema::bulk_index_completed(&conn)
            .context("failed to read whether the project has been fully walked")?,
        semantic_pass_completed: schema::semantic_pass_completed(&conn)
            .context("failed to read whether the project's semantic pass has completed")?,
        semantic_pass_owed,
        semantic_pass_failures,
        pending_reindex: schema::pending_reindexes(&conn)
            .context("failed to read the interrupted workspace reindexes")?,
        semantic_pending: semantic_pending(&conn, &capable)?,
        semantic_stale: semantic_stale(&conn, &capable)?,
        semantic_leftovers: schema::semantic_leftovers(&conn)
            .context("failed to read the semantic residual and never-answered files")?
            .into_iter()
            .filter(|leftover| capable.contains(&leftover.language))
            .collect(),
        coverage,
        syntax_error_files: files::files_with_syntax_errors(&conn)?,
    })
}

/// Cross-references the files a project walk found against the index's
/// `File` nodes and `indexed_files` baselines.
fn coverage(conn: &Connection, discovered: &[SourceFile]) -> Result<Coverage> {
    let indexed_files = files::indexed_file_paths(conn)?;
    let baselines = file_rows::recorded_mtimes(conn)?;

    let mut indexed = 0;
    let mut dirty = 0;
    for file in discovered {
        if !indexed_files.contains(&file.relative) {
            // Never indexed - the one case that is both uncovered and dirty.
            dirty += 1;
            continue;
        }
        indexed += 1;
        if let Some(&recorded_mtime) = baselines.get(&file.relative) {
            if recorded_mtime != file.mtime_millis {
                dirty += 1;
            }
        }
    }
    Ok(Coverage { discovered: discovered.len(), indexed, dirty })
}

/// The semantic-pending languages a reader should believe: capable, and whose
/// pass is not recorded done.
fn semantic_pending(conn: &Connection, capable: &HashSet<String>) -> Result<Vec<(String, String, usize)>> {
    let pending = schema::semantic_pending(conn).context("failed to read the semantic-pending languages")?;
    let mut believed = Vec::new();
    for (language, since, files) in pending {
        if capable.contains(&language)
            && !schema::language_semantic_pass_done(conn, &language)
                .context("failed to read whether a pending semantic pass completed")?
        {
            believed.push((language, since, files));
        }
    }
    Ok(believed)
}

/// The stale-file counts a reader should believe: capable languages whose
/// pass is not recorded done, as [`semantic_pending`] filters.
fn semantic_stale(conn: &Connection, capable: &HashSet<String>) -> Result<Vec<(String, usize)>> {
    let stale = schema::semantic_stale_files(conn).context("failed to read the stale semantic files")?;
    let mut believed = Vec::new();
    for (language, files) in stale {
        if capable.contains(&language)
            && !schema::language_semantic_pass_done(conn, &language)
                .context("failed to read whether a semantic pass completed")?
        {
            believed.push((language, files));
        }
    }
    Ok(believed)
}

/// `(language, reason)` per language whose last semantic pass failed.
type SemanticPassFailures = Vec<(String, String)>;

/// The languages still owed a semantic pass, and the recorded failures -
/// what [`semantic_pass_lines`] reports.
pub(crate) fn semantic_pass_state(
    conn: &Connection,
    capable: &HashSet<String>,
) -> Result<(Vec<String>, SemanticPassFailures)> {
    let owed = schema::owed_semantic_pass_languages(conn, capable)
        .context("failed to read which languages still owe a semantic pass")?;
    let failures =
        schema::semantic_pass_failures(conn).context("failed to read the recorded semantic-pass failures")?;
    Ok((owed, failures))
}

/// The report's semantic-pass lines: one per language whose last pass
/// failed, with its reason, and the "never completed" advice only while some
/// owed language has no recorded failure or residual to explain it and no
/// live daemon is working on the index. `in_progress` is what to say instead
/// while one is. Then, per `leftovers` entry, the files an owed residual
/// language's next start asks and the files no pass ever answered.
pub(crate) fn semantic_pass_lines(
    completed: bool,
    owed: &[String],
    failures: &[(String, String)],
    leftovers: &[SemanticLeftover],
    in_progress: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    let residual = |language: &String| {
        leftovers.iter().any(|leftover| &leftover.language == language && leftover.residual_files.is_some())
    };
    if completed {
        lines.push("  semantic pass:   complete".to_string());
    } else if let Some(in_progress) = in_progress {
        lines.push(format!("  semantic pass:   {in_progress}"));
    } else {
        let unexplained = owed
            .iter()
            .any(|language| !failures.iter().any(|(failed, _)| failed == language) && !residual(language));
        if unexplained || (failures.is_empty() && !owed.iter().any(residual)) {
            lines.push("  semantic pass:   never completed - run `g-mesh reindex` to repair it".to_string());
        }
    }
    for leftover in leftovers {
        let language = &leftover.language;
        if let Some(files) = leftover.residual_files.filter(|_| owed.contains(language)) {
            let asks =
                if in_progress.is_some() { "the running daemon asks" } else { "the next daemon start asks" };
            lines.push(format!(
                "  semantic pass:   {language} incomplete - {files} file(s) left, {asks} only those"
            ));
        }
        if leftover.never_answered > 0 {
            lines.push(format!(
                "  semantic pass:   {language}: {} file(s) never answered - asked again when they change",
                leftover.never_answered
            ));
        }
    }
    for (language, reason) in failures {
        // A pass deferred because its plugin was asleep has not failed: it is
        // still owed and will be asked again.
        // While a live daemon is working, it is the one that asks again, so
        // the line carries no repair advice.
        if reason == NOT_RUN_REASON {
            let asks_again = if in_progress.is_some() {
                "the running daemon asks again"
            } else {
                "the next daemon start or `g-mesh reindex` asks again"
            };
            lines.push(format!(
                "  semantic pass:   {language} pending - its plugin was asleep or memory-suspended; {asks_again}"
            ));
            continue;
        }
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(format!("  semantic pass:   {language} failed - {reason}"));
    }
    lines
}

pub(crate) struct SourceFile {
    /// Project-relative, forward-slash separated - the same spelling the
    /// `filePath` columns and the wire protocol use.
    pub(crate) relative: String,
    pub(crate) mtime_millis: i64,
}

/// Walks `project_root` for files some discovered plugin would index, through
/// the walker [`project_walk::project_files`] shares with the bulk walk's
/// absent-plugin count: `.gitignore`, the baseline exclusions and each
/// language's `[plugin.workspace] exclude_dirs`; the per-file decision is
/// [`DiscoveredPlugins::indexing_language`], the watcher's filter. Each
/// plugin walks in its own process with the same walk (`g_mesh_walk`), so
/// a file reached only through a followed link is discovered under the
/// link's spelling, as the plugin indexes it, and its mtime is read through
/// the link, as the plugin's baseline records it. A file under a link whose
/// target one language excludes is not that language's (both spellings are
/// checked). Unreadable metadata is skipped, which cannot make a broken
/// index look healthy.
pub(crate) fn discover_source_files(
    project_root: &Path,
    plugins: &DiscoveredPlugins,
) -> Result<Vec<SourceFile>> {
    // Pruned outright: the baseline (always, by the walker), plus any
    // directory *every* discovered language excludes. A directory only some
    // languages exclude is still walked (`dist/app.py` is Python's even though
    // TypeScript skips `dist`), and its files are filtered one by one below.
    let pruned = plugins.excluded_by_every_language();

    let mut files = Vec::new();
    for walked in project_walk::project_files(project_root, &pruned) {
        if plugins.indexing_language(&walked.relative, walked.real_relative.as_deref()).is_none() {
            continue;
        }
        let Ok(metadata) = fs::metadata(&walked.path) else {
            continue;
        };
        let Ok(mtime_millis) = mtime_millis(&metadata) else {
            continue;
        };
        files.push(SourceFile { relative: walked.relative, mtime_millis });
    }
    Ok(files)
}

/// The three per-language reads behind the MCP instructions' coverage, each
/// kept apart so one failing leaves the others usable.
pub struct LanguageOverview {
    /// Every language with a `File` node, and whether its semantic pass is
    /// done.
    pub present: Result<Vec<(String, bool)>>,
    /// The last walk's outcome per language, sorted by language.
    pub outcomes: Result<Vec<(String, LanguageOutcome)>>,
    /// Each language's automatic retries since the last full walk.
    pub retries: Result<BTreeMap<String, u32>>,
}

/// Reads [`LanguageOverview`] from `conn`.
pub fn language_overview(conn: &Connection) -> LanguageOverview {
    LanguageOverview {
        present: schema::present_languages_with_semantic_state(conn),
        outcomes: schema::language_outcomes(conn),
        retries: schema::language_retries(conn),
    }
}
