//! What a settled edit to a watch file does for a plugin whose manifest
//! declares `capabilities.resolution_delta`: the plugin says what the edit
//! changed for resolution, and only the files that change touches are
//! re-extracted. Design: docs/architecture/gm-509-selective-config-reindex.md.
//!
//! [`run`], inside `PluginSupervisor::with_awake_exclusive_access`:
//!
//! 0. A `pending_reindex` row for the language falls back to the
//!    whole-language reindex: an earlier run was killed with some rows
//!    re-extracted against facts that were never stored, so no answer
//!    against the stored facts says which rows are stale (a config reverted
//!    meanwhile reads as `Unchanged`). The fallback clears the row.
//! 1. Sends `resolutionChanged` with the facts stored for the language
//!    (`schema::resolution_facts`). No stored facts, an error, a timeout or
//!    an [`ResolutionDelta::Unknown`] answer falls back to the whole-language
//!    reindex (`daemon::workspace_reindex`), whose walk stores fresh facts.
//! 2. [`ResolutionDelta::Unchanged`] stores the answer's facts and stops.
//! 3. [`ResolutionDelta::Affected`] is expanded against the stored rows by
//!    [`select_affected`]. A selection above [`FALLBACK_SHARE_PERCENT`] of the
//!    language's indexed files falls back to the whole-language reindex.
//!    Otherwise [`reextract`] marks `pending_reindex`, re-extracts each
//!    selected file, sends one `semanticPass` over them plus the owed files,
//!    and then stores the facts and clears `pending_reindex` in one
//!    transaction.
//!
//! The stored facts are replaced only after the rows they describe: a run
//! killed before that leaves `pending_reindex` and the old facts, and
//! `workspace_reindex::resume_pending` reaches step 0 at the next start.
//!
//! The fallback runs after the exclusive section ends, because
//! `workspace_reindex::run` takes the same lock.
//!
//! A source edit can change resolution too (GM-507: a Rust `mod` line places
//! a module file). Its plugin answers the `fileChanged` with `affected`, and
//! `watcher::apply` expands it with [`select`] (the same selection and
//! threshold) and re-extracts inside that file change's own exclusive
//! section. Its files are recorded in `reextract_owed_files` before the
//! loop, not `pending_reindex`; [`resume_owed_reextracts`] finishes them
//! after a kill.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::daemon::lifecycle::PluginSupervisor;
use crate::daemon::plugin::PluginProcess;
use crate::daemon::registry::PluginRegistry;
use crate::daemon::workspace_reindex;
use crate::embedding::EmbeddingPipeline;
use crate::protocol::types::{ImportMatch, ImportSelector, PathScope, ResolutionDelta, TargetScopeKind};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;

/// A selection of more than this share of the language's indexed files is
/// re-indexed whole instead: the staging swap is atomic and, at that size,
/// no slower than one round trip per file.
pub(crate) const FALLBACK_SHARE_PERCENT: usize = 30;

/// What the exclusive section decided.
#[derive(Debug)]
enum Outcome {
    /// Nothing was re-extracted; the new facts are stored.
    Unchanged,
    /// These files were re-extracted; the new facts are stored.
    Reextracted(usize),
    /// The whole-language reindex is owed, for this reason.
    Fallback(String),
}

/// Handles a settled edit of `changed_file`, one of the watch files of
/// `supervisor`'s language, whose manifest declares `resolution_delta`.
/// `supervisor` must be `registry.get_or_spawn(language)`'s own.
pub(crate) fn run(
    registry: &PluginRegistry,
    supervisor: &PluginSupervisor,
    store: &IndexStore,
    changed_file: &str,
) -> Result<()> {
    let language = supervisor.language().to_string();
    let semantic_pass =
        supervisor.manifest().capabilities.semantic_pass && !supervisor.is_semantic_suspended();
    let embedding = registry.embedding();
    let started = std::time::Instant::now();
    let outcome = supervisor.with_awake_exclusive_access(|process| {
        selective(process, store, embedding, &language, changed_file, semantic_pass)
    });
    let outcome = match outcome {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) | Err(err) => Outcome::Fallback(format!("{err:#}")),
    };
    match outcome {
        Outcome::Unchanged => {
            crate::log_line!(
                "g-mesh daemon: {changed_file} changed nothing {language} resolution reads - \
                 nothing re-extracted ({} ms)",
                started.elapsed().as_millis()
            );
            Ok(())
        }
        Outcome::Reextracted(count) => {
            crate::log_line!(
                "g-mesh daemon: {changed_file} changed {language} resolution - re-extracted {count} \
                 file(s) ({} ms)",
                started.elapsed().as_millis()
            );
            Ok(())
        }
        Outcome::Fallback(why) => {
            crate::log_line!(
                "g-mesh daemon: reindexing all of {language} after {changed_file} changed: {why}"
            );
            workspace_reindex::run(registry, supervisor, store, changed_file)
        }
    }
}

/// The exclusive section of [`run`]. An `Err` is a fallback too.
fn selective(
    process: &PluginProcess,
    store: &IndexStore,
    embedding: &EmbeddingPipeline,
    language: &str,
    changed_file: &str,
    semantic_pass: bool,
) -> Result<Outcome> {
    let pending = store.with(schema::pending_reindexes)?;
    if pending.iter().any(|(pending_language, _)| pending_language == language) {
        return Ok(Outcome::Fallback("an earlier reindex of the language was interrupted".to_string()));
    }
    let Some(previous) = store.with(|conn| schema::resolution_facts(conn, language))? else {
        return Ok(Outcome::Fallback("no resolution facts are stored for the language".to_string()));
    };
    let answer = match process.send_resolution_changed(changed_file, Some(previous)) {
        Ok(answer) => answer,
        Err(err) => return Ok(Outcome::Fallback(format!("resolutionChanged failed ({err:#})"))),
    };
    let facts = answer.facts;
    match answer.delta {
        ResolutionDelta::Unchanged => {
            store.with(|conn| schema::set_resolution_facts(conn, language, facts.as_deref()))?;
            Ok(Outcome::Unchanged)
        }
        ResolutionDelta::Unknown { reason } => {
            Ok(Outcome::Fallback(format!("the plugin cannot tell what changed ({reason})")))
        }
        ResolutionDelta::Affected { files, imports } => {
            let selected = match store.with(|conn| select(conn, language, &files, &imports, None))? {
                Selection::Nothing => {
                    store.with(|conn| schema::set_resolution_facts(conn, language, facts.as_deref()))?;
                    return Ok(Outcome::Unchanged);
                }
                Selection::TooMany(why) => return Ok(Outcome::Fallback(why)),
                Selection::Files(selected) => selected,
            };
            reextract(
                process,
                store,
                embedding,
                language,
                changed_file,
                &selected,
                facts.as_deref(),
                semantic_pass,
            )
        }
    }
}

/// What a resolution delta selects for a re-extract.
#[derive(Debug)]
pub(crate) enum Selection {
    /// No indexed file.
    Nothing,
    /// More than [`FALLBACK_SHARE_PERCENT`] of the language's indexed files:
    /// the whole-language reindex is owed, for this reason.
    TooMany(String),
    /// These files, few enough to re-extract one by one.
    Files(BTreeSet<String>),
}

/// [`select_affected`] with the threshold applied, leaving out `trigger`
/// (a source edit's own file, extracted by the round trip that answered the
/// delta). The share counts the trigger out of the selection but not out of
/// the indexed files.
pub(crate) fn select(
    conn: &Connection,
    language: &str,
    files: &[PathScope],
    imports: &[ImportSelector],
    trigger: Option<&str>,
) -> Result<Selection> {
    let (mut selected, indexed) = select_affected(conn, language, files, imports)?;
    if let Some(trigger) = trigger {
        selected.remove(trigger);
    }
    if selected.is_empty() {
        return Ok(Selection::Nothing);
    }
    if selected.len() * 100 > indexed * FALLBACK_SHARE_PERCENT {
        return Ok(Selection::TooMany(format!(
            "the edit affects {} of {indexed} file(s), more than {FALLBACK_SHARE_PERCENT}%",
            selected.len()
        )));
    }
    Ok(Selection::Files(selected))
}

/// The indexed files of `language` that `files` or `imports` select, and how
/// many files the language has indexed.
///
/// A file scope selects every indexed `File` of the language inside it. An
/// import selector selects the importing file of every `IMPORTS` edge of the
/// language whose importer lies in its `importers` scope and whose specifier
/// (`edges.specifier`) or target matches. The target of a linked edge is the
/// `File` path or the container key it points at; of an unlinked one, its
/// placeholder's `placeholder_targets.scope`.
pub(crate) fn select_affected(
    conn: &Connection,
    language: &str,
    files: &[PathScope],
    imports: &[ImportSelector],
) -> Result<(BTreeSet<String>, usize)> {
    let indexed: Vec<String> = conn
        .prepare("SELECT filePath FROM nodes WHERE kind = 'File' AND language = ?1")
        .and_then(|mut statement| statement.query_map(params![language], |row| row.get(0))?.collect())
        .with_context(|| format!("failed to read {language}'s indexed files"))?;
    let mut selected: BTreeSet<String> =
        indexed.iter().filter(|path| files.iter().any(|scope| scope.contains(path))).cloned().collect();

    if !imports.is_empty() {
        let mut statement = conn
            .prepare(
                "SELECT src.filePath, e.specifier, t.kind, t.filePath, c.key, pt.scopeKind, pt.scope
                 FROM edges e
                 JOIN nodes src ON src.id = e.fromId
                 JOIN nodes t ON t.id = e.toId
                 LEFT JOIN containers c ON c.nodeId = t.id
                 LEFT JOIN placeholder_targets pt ON pt.nodeId = t.id
                 WHERE e.kind = 'IMPORTS' AND src.language = ?1",
            )
            .context("failed to prepare the import scan")?;
        let rows = statement
            .query_map(params![language], |row| {
                Ok(StoredImport {
                    importer: row.get(0)?,
                    specifier: row.get(1)?,
                    target_kind: row.get(2)?,
                    target_file: row.get(3)?,
                    container_key: row.get(4)?,
                    placeholder_scope_kind: row.get(5)?,
                    placeholder_scope: row.get(6)?,
                })
            })
            .with_context(|| format!("failed to scan {language}'s imports"))?;
        for row in rows {
            let import = row.with_context(|| format!("failed to read one of {language}'s imports"))?;
            if selected.contains(&import.importer) {
                continue;
            }
            if imports.iter().any(|selector| import.matches(selector)) {
                selected.insert(import.importer);
            }
        }
    }
    Ok((selected, indexed.len()))
}

/// One `IMPORTS` edge as [`select_affected`] reads it.
struct StoredImport {
    importer: String,
    specifier: Option<String>,
    target_kind: String,
    target_file: String,
    container_key: Option<String>,
    placeholder_scope_kind: Option<String>,
    placeholder_scope: Option<String>,
}

impl StoredImport {
    fn matches(&self, selector: &ImportSelector) -> bool {
        if !selector.importers.contains(&self.importer) {
            return false;
        }
        match &selector.by {
            ImportMatch::Specifier(matcher) => self.specifier.as_deref().is_some_and(|s| matcher.matches(s)),
            ImportMatch::Target { scope_kind, matcher } => {
                self.target(*scope_kind).is_some_and(|target| matcher.matches(target))
            }
        }
    }

    /// The stored target of `kind`: the placeholder's own scope while the
    /// edge is unlinked, otherwise the node it was linked onto.
    fn target(&self, kind: TargetScopeKind) -> Option<&str> {
        let wanted = match kind {
            TargetScopeKind::File => "file",
            TargetScopeKind::Container => "container",
        };
        if let Some(scope_kind) = self.placeholder_scope_kind.as_deref() {
            return (scope_kind == wanted).then_some(self.placeholder_scope.as_deref()).flatten();
        }
        match kind {
            TargetScopeKind::File => (self.target_kind == "File").then_some(self.target_file.as_str()),
            TargetScopeKind::Container => self.container_key.as_deref(),
        }
    }
}

/// Re-extracts `selected` under `pending_reindex`, sends one semantic pass
/// over them (plus the owed files) when `semantic_pass`, then stores `facts`
/// and clears `pending_reindex` together. A failed re-extract stops the loop
/// and falls back, leaving `pending_reindex` for the whole-language reindex
/// to clear.
#[allow(clippy::too_many_arguments)]
fn reextract(
    process: &PluginProcess,
    store: &IndexStore,
    embedding: &EmbeddingPipeline,
    language: &str,
    changed_file: &str,
    selected: &BTreeSet<String>,
    facts: Option<&str>,
    semantic_pass: bool,
) -> Result<Outcome> {
    store
        .with(|conn| schema::mark_pending_reindex(conn, language, changed_file))
        .with_context(|| format!("failed to mark {language}'s reindex as pending"))?;
    for file_path in selected {
        if let Err(err) = process.reextract(store, file_path, embedding) {
            return Ok(Outcome::Fallback(format!("re-extracting {file_path} failed ({err:#})")));
        }
    }
    if semantic_pass {
        let scope: Vec<String> = selected.iter().cloned().collect();
        if let Err(err) = process.scoped_semantic_pass(store, scope, embedding) {
            crate::log_line!(
                "g-mesh daemon: the semantic pass over {language}'s re-extracted files failed ({err:#}) - \
                 their edges keep whatever the structural pass resolved"
            );
        }
    }
    store
        .with(|conn| -> Result<()> {
            let tx = conn.unchecked_transaction().context("failed to start the facts transaction")?;
            schema::set_resolution_facts(&tx, language, facts)?;
            tx.execute("DELETE FROM pending_reindex WHERE language = ?1", params![language])
                .with_context(|| format!("failed to clear {language}'s pending reindex"))?;
            tx.commit().context("failed to commit the facts transaction")
        })
        .with_context(|| format!("failed to store {language}'s resolution facts"))?;
    Ok(Outcome::Reextracted(selected.len()))
}

/// Re-extracts the files a source edit's re-extract loop (GM-507,
/// `watcher::apply`) recorded as owed and did not finish: a daemon killed
/// mid-loop leaves their rows, which hold the module placement of before
/// the edit. Called once at start, after
/// `workspace_reindex::resume_pending` (whose swap clears a reindexed
/// language's rows). Per language: each file is re-extracted, one semantic
/// pass covers them, and the rows are cleared; a failure falls back to the
/// whole-language reindex, whose swap clears them. Rows of a language no
/// discovered plugin serves are dropped.
pub(crate) fn resume_owed_reextracts(registry: &PluginRegistry, store: &IndexStore) {
    let owed = match store.with(schema::owed_reextracts) {
        Ok(owed) => owed,
        Err(err) => {
            crate::log_line!("g-mesh daemon: could not read the interrupted re-extracts ({err:#})");
            return;
        }
    };
    for (language, trigger, files) in owed {
        if !registry.has_manifest(&language) {
            if let Err(err) = store.with(|conn| schema::clear_owed_reextracts(conn, &language)) {
                crate::log_line!("g-mesh daemon: could not drop {language}'s owed re-extracts ({err:#})");
            }
            continue;
        }
        crate::log_line!(
            "g-mesh daemon: re-extracting {} {language} file(s) an interrupted run owed after {trigger} changed",
            files.len()
        );
        let supervisor = match registry.get_or_spawn(&language) {
            Ok(supervisor) => supervisor,
            Err(err) => {
                crate::log_line!(
                    "g-mesh daemon: could not start the {language} plugin to finish its owed re-extracts \
                     ({err:#}) - they are retried at the next start"
                );
                continue;
            }
        };
        let semantic_pass =
            supervisor.manifest().capabilities.semantic_pass && !supervisor.is_semantic_suspended();
        let embedding = registry.embedding();
        let finished = supervisor.with_awake_exclusive_access(|process| -> Result<()> {
            for file_path in &files {
                process
                    .reextract(store, file_path, embedding)
                    .with_context(|| format!("re-extracting {file_path} failed"))?;
            }
            if semantic_pass {
                if let Err(err) = process.scoped_semantic_pass(store, files.clone(), embedding) {
                    crate::log_line!(
                        "g-mesh daemon: the semantic pass over {language}'s owed re-extracts failed ({err:#}) - \
                         their edges keep whatever the structural pass resolved"
                    );
                }
            }
            store.with(|conn| schema::settle_owed_reextracts(conn, &language, &files))
        });
        if let Err(err) = finished.and_then(|finished| finished) {
            crate::log_line!(
                "g-mesh daemon: reindexing all of {language}: its owed re-extracts did not finish ({err:#})"
            );
            if let Err(err) = workspace_reindex::run(registry, &supervisor, store, &trigger) {
                crate::log_line!(
                    "g-mesh daemon: failed to reindex {language} for its owed re-extracts ({err:#}) - \
                     they are retried at the next start"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
