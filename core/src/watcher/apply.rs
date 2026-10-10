//! Bridges a (debounced) file-change event to a committed diff: sends a
//! `FileChanged` control-plane request to a language plugin, reads back its
//! diff, and applies it through `storage::write::apply_diff`.
//!
//! The semantic pass ([`apply_semantic_pass`]) is the same motion with a
//! different question: instead of "what does this file look like now", it
//! asks "what can your type checker now resolve that tree-sitter could
//! only guess at". Its answer comes back in the same diff shape and goes
//! through the same commit-and-link pipeline.
//!
//! This module is transport-agnostic on purpose - it only knows about
//! `Read`/`Write` streams (the same abstraction `jsonrpc.rs` and
//! `handshake.rs` already use), not about how the peer on the other end of
//! those streams came to exist. A real spawned-plugin-process transport can
//! be plugged in later without touching this function; for now, tests fake
//! the peer with `std::io::pipe()` plus a thread, exactly like
//! `jsonrpc.rs`'s own pipe-based tests do.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::embedding::{EmbedStats, EmbeddingPipeline};
use crate::protocol::jsonrpc::{read_message_with_timeout, write_message};
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, LinkedEdge, PathError,
    PlaceholderTarget, QualifiedPath, RequestId, SourceTier, TargetKey, TargetScope, Visibility, WireEdge,
    WireNode, JSONRPC_VERSION,
};
use crate::storage::file_rows::FileScope;
use crate::storage::index_store::{IndexStore, Unit, Writer};
use crate::storage::schema;
use crate::storage::write::{DeclarationRecord, Diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

/// Sends a `FileChanged` request (tagged with `request_id`) for `file_path`
/// over `writer`, reads the plugin's `FileChangeResponse` off `reader`,
/// validates the response id matches the request id, converts the wire
/// diff into a `storage::write::Diff`, and commits it via `apply_diff` -
/// then asks the same plugin for a semantic pass over that same file.
///
/// The semantic pass rides here, rather than at each of this function's
/// call sites, because "the reparse has settled" is precisely the condition
/// it needs and this is the only place that knows it. Every caller that
/// reparses a file gets it for free: the watcher (via
/// `daemon::lifecycle::PluginSupervisor::file_changed`), the wake-up replay
/// of everything queued while the plugin slept, and the query-time
/// staleness catch-up in `watcher::staleness::ensure_fresh` - which is a
/// reparse settling as much as any other, and whose edges deserve the
/// upgrade just as much.
///
/// A failing semantic pass is reported and dropped, not propagated. It is
/// an *upgrade* over a graph that is already committed, correct and
/// serviceable: failing the whole reparse because the semantic layer was
/// unavailable would turn a better answer that did not arrive into a worse
/// one that did. (A plugin that died is still noticed - the next request's
/// write fails, which is what `daemon::plugin::PluginProcess` relaunches
/// and replays on.)
///
/// `request_id` is supplied by the caller rather than generated internally
/// so this function stays pure and easy to test; the id for the second
/// round trip is derived from it by [`semantic_pass_id`].
///
/// `file_changed_timeout`/`semantic_pass_timeout` bound each of this
/// function's two round trips independently - see
/// `daemon::plugin::RoundTripTimeouts`'s doc comment for why they differ and
/// how their values are chosen. `on_timeout` is called at most once, from
/// whichever round trip actually times out (the two never overlap: a timed-
/// out `FileChanged` returns early via `?` before the semantic pass is ever
/// sent) - see [`round_trip`] for what it is for and why this function does
/// not know how to implement it itself.
///
/// `semantic_pass_capable` gates the second round trip entirely: a plugin
/// whose manifest declares `capabilities.semantic_pass = false` (the
/// conservative default - see `daemon::manifest::Capabilities::default`)
/// never receives a `semanticPass` request at all, per the architecture
/// doc's `plugin.toml additions` ("`false`: core never sends it, and no
/// empty-diff answer is required"). A plain `bool` rather than the
/// `Capabilities` type itself: this module is transport-agnostic and knows
/// nothing about manifests on purpose (see its own doc comment), so the
/// caller that *does* own the manifest - `daemon::plugin::PluginProcess`,
/// via its own `manifest.capabilities.semantic_pass` - resolves the
/// capability and hands down only the one bit this function needs, rather
/// than this module reaching into `daemon::manifest` for itself.
#[allow(clippy::too_many_arguments)]
pub fn apply_file_change<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &IndexStore,
    project_root: &Path,
    language: &str,
    file_path: impl Into<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    file_changed_timeout: Duration,
    semantic_pass_timeout: Duration,
    semantic_pass_capable: bool,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    store.unit(Unit::WatcherApply, |store| {
        apply_file_change_in(
            reader,
            writer,
            store,
            project_root,
            language,
            file_path,
            request_id,
            embedding,
            file_changed_timeout,
            semantic_pass_timeout,
            semantic_pass_capable,
            false,
            on_timeout,
        )
    })
}

/// One structural `fileChanged` round trip with `reextract` set: the plugin
/// extracts `file_path` even though its text is what it last extracted,
/// because something its resolution reads changed. No semantic pass follows;
/// the caller sends one for every file it re-extracted
/// (`daemon::config_reindex`).
#[allow(clippy::too_many_arguments)]
pub fn reextract_file<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &IndexStore,
    project_root: &Path,
    language: &str,
    file_path: impl Into<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    file_changed_timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    store.unit(Unit::WatcherApply, |store| {
        apply_file_change_in(
            reader,
            writer,
            store,
            project_root,
            language,
            file_path,
            request_id,
            embedding,
            file_changed_timeout,
            file_changed_timeout,
            false,
            true,
            on_timeout,
        )
    })
}

/// A per-file `semanticPass` over `file_paths` plus the language's owed
/// files, settled like the pass after a reparse. `file_paths` must not be
/// empty: an empty list asks for the whole project.
#[allow(clippy::too_many_arguments)]
pub fn apply_scoped_semantic_pass<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &IndexStore,
    language: &str,
    file_paths: Vec<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    anyhow::ensure!(!file_paths.is_empty(), "a scoped semantic pass needs at least one file");
    store.unit(Unit::WatcherApply, |store| {
        let owed = store.step(|conn| schema::owed_files(conn, language)).unwrap_or_else(|err| {
            crate::log_line!("g-mesh: failed to read {language}'s owed semantic files ({err:#})");
            Vec::new()
        });
        apply_semantic_pass_in(
            reader,
            writer,
            store,
            language,
            None,
            Scope::Files { requested: file_paths, owed },
            request_id,
            embedding,
            timeout,
            on_timeout,
        )
        .map(|_| ())
    })
}

/// [`apply_file_change`] for a caller already inside a unit. `reextract` is
/// sent as `fileChanged`'s own flag.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_file_change_in<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &mut Writer<'_>,
    project_root: &Path,
    language: &str,
    file_path: impl Into<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    file_changed_timeout: Duration,
    semantic_pass_timeout: Duration,
    semantic_pass_capable: bool,
    reextract: bool,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    let file_path = file_path.into();
    // A structural reparse has nothing to be incomplete about - the plugin
    // either extracted the file or it did not - so this round trip's report is
    // deliberately dropped here and read only for a `semanticPass`.
    let _structural = round_trip(
        reader,
        writer,
        store,
        ControlMessage::FileChanged { file_path: file_path.clone(), reextract },
        Some(project_root),
        request_id.clone(),
        embedding,
        file_changed_timeout,
        on_timeout,
    )?;

    if !semantic_pass_capable {
        return Ok(());
    }

    // The files earlier per-file passes did not finish ride along:
    // core, not the plugin, keeps them, so the next pass asks them again
    // without waiting for an edit, and knows the whole scope it sent.
    // Best-effort: an unreadable owed set only means they wait one more pass.
    //
    // The same rows are a residual language's leftovers (GM-521), so an edit
    // invalidates its file's record here: finished, the row (and any
    // never-answered mark) goes; unfinished, it restarts at one attempt.
    // GM-515 presence batches: every created file still gets its own
    // `fileChanged` and per-file pass, and the owed files ride along on each
    // one, so a batch of `MAX_OWED_ATTEMPTS` or more creations can use up an
    // owed file's attempts at once - the same bound, spent sooner; a created
    // file itself has no owed row.
    let owed = store.step(|conn| schema::owed_files(conn, language)).unwrap_or_else(|err| {
        crate::log_line!("g-mesh: failed to read {language}'s owed semantic files ({err:#})");
        Vec::new()
    });
    if let Err(err) = apply_semantic_pass_in(
        reader,
        writer,
        store,
        language,
        None,
        Scope::Files { requested: vec![file_path.clone()], owed },
        semantic_pass_id(&request_id),
        embedding,
        semantic_pass_timeout,
        on_timeout,
    ) {
        crate::log_line!(
            "g-mesh: the semantic pass over {file_path} failed after its reparse ({err:#}) - \
             its edges keep whatever the structural pass resolved"
        );
    }
    Ok(())
}

/// Asks the plugin's semantic layer what it can now resolve, and commits
/// the answer exactly like a file-change diff.
///
/// `file_paths` scopes the pass: one entry after an incremental reparse,
/// **empty** for the whole project once the cold-start bulk walk is done -
/// see `ControlMessage::SemanticPass` on why "empty" means everything
/// rather than nothing. `language` is the plugin's: the request carries the
/// structural edges of that language in scope that linking already moved
/// (`ControlMessage::SemanticPass::linked_edges`, [`linked_edges`]).
///
/// The answer is an ordinary `FileChangeDiff`, applied through
/// `apply_diff`, whose edges have `source = 'semantic'`. What it holds
/// depends on the plugin: an edge re-sent under a structural edge's own id
/// overwrites that edge in place (the TypeScript tier's upgrades); an edge
/// onto a placeholder the pass adds, under an id no structural walk emits,
/// is a new row that linking then points at its target (the SDK's LSP bridge
/// and the Go tier); `deleteEdgeIds` retracts both the pass's own earlier
/// edges and structural edges it contradicts. A plugin retracts only the ids
/// it remembers emitting in its own process.
///
/// After a complete whole-project pass, `sweep_language`'s semantic edges
/// the pass did not send are deleted ([`sweep_semantic_edges`]): they are
/// what an earlier process emitted and this one no longer does, and so are
/// the placeholders the language's last workspace reindex kept that nothing
/// has re-sent since (`IndexStore::sweep_unclaimed_nodes`). `None` (a
/// plugin whose manifest leaves `capabilities.semantic_sweep` off), an
/// incomplete pass and a per-file pass sweep nothing.
///
/// An incomplete whole-project pass whose answer names its
/// `unfinishedFiles` is recorded as residual (GM-521,
/// [`schema::record_language_semantic_residual`]) and returns
/// [`SemanticPassOutcome::Residual`] or [`SemanticPassOutcome::Settled`];
/// one that does not name them is still an `Err`, so the next start asks
/// the whole project again.
#[allow(clippy::too_many_arguments)]
pub fn apply_semantic_pass<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &IndexStore,
    language: &str,
    sweep_language: Option<&str>,
    file_paths: Vec<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<SemanticPassOutcome> {
    let scope = if file_paths.is_empty() {
        Scope::WholeProject
    } else {
        Scope::Files { requested: file_paths, owed: Vec::new() }
    };
    store.unit(Unit::WatcherApply, |store| {
        apply_semantic_pass_in(
            reader,
            writer,
            store,
            language,
            sweep_language,
            scope,
            request_id,
            embedding,
            timeout,
            on_timeout,
        )
    })
}

/// Asks `language`'s residual files again (GM-521): `file_paths` are the owed
/// rows an incomplete whole-project pass left
/// ([`schema::semantic_residual_files`]). Every file the answer does not
/// finish costs one more attempt, and so does every file of a pass that
/// fails outright (a timeout, a crash): a file that always times out is asked
/// on at most [`schema::MAX_OWED_ATTEMPTS`] starts. Returns
/// [`SemanticPassOutcome::Settled`] once no owed file is left, for the caller
/// to record the pass (owner decision Q3), and
/// [`SemanticPassOutcome::Residual`] otherwise. Sweeps nothing: the pass is
/// not over the whole project.
#[allow(clippy::too_many_arguments)]
pub fn apply_residual_semantic_pass<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &IndexStore,
    language: &str,
    file_paths: Vec<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<SemanticPassOutcome> {
    store.unit(Unit::WatcherApply, |store| {
        apply_semantic_pass_in(
            reader,
            writer,
            store,
            language,
            None,
            Scope::Residual(file_paths),
            request_id,
            embedding,
            timeout,
            on_timeout,
        )
    })
}

/// What a semantic pass left for its caller to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticPassOutcome {
    /// A complete whole-project pass - the caller records it
    /// (`schema::record_language_semantic_pass`) - or any per-file pass,
    /// which has nothing to record.
    Complete,
    /// An incomplete whole-project pass, or a residual pass, left `left`
    /// owed files; the language is recorded residual and stays owed, and the
    /// next start asks only those files.
    Residual { left: usize },
    /// A residual (or a listed incomplete whole-project) pass left no owed
    /// file: every file is answered or given up. The caller records the pass
    /// with `schema::record_language_semantic_pass_settled` (owner decision
    /// Q3), keeping the given-up files for `g-mesh status`.
    Settled,
}

impl SemanticPassOutcome {
    fn left(left: usize) -> Self {
        if left == 0 {
            Self::Settled
        } else {
            Self::Residual { left }
        }
    }
}

/// Which files a semantic pass is sent, and how its answer settles them.
enum Scope {
    /// Every file: sent no list.
    WholeProject,
    /// A per-file pass: `requested` (the edited file) and the language's
    /// `owed` files ([`schema::owed_files`]) riding along. The pass's scope
    /// is their union, and a `FileChangeResponse::unfinished_files` list
    /// settles exactly that scope less the files it names; a requested file
    /// left unfinished restarts at one attempt, its content having changed.
    Files { requested: Vec<String>, owed: Vec<String> },
    /// A residual pass at start: these owed files, each costing one attempt
    /// when not finished.
    Residual(Vec<String>),
}

/// [`apply_semantic_pass`] and [`apply_residual_semantic_pass`] inside an
/// open unit.
#[allow(clippy::too_many_arguments)]
fn apply_semantic_pass_in<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &mut Writer<'_>,
    language: &str,
    sweep_language: Option<&str>,
    scope: Scope,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<SemanticPassOutcome> {
    let whole_project = matches!(scope, Scope::WholeProject);
    // The scope this pass is sent: the requested files, then every owed file
    // not among them. A whole-project pass is sent no list at all.
    let (sent, file_paths, residual) = match scope {
        Scope::WholeProject => (Vec::new(), Vec::new(), false),
        Scope::Files { requested, owed } => {
            let mut sent = requested.clone();
            for path in owed {
                if !sent.contains(&path) {
                    sent.push(path);
                }
            }
            (sent, requested, false)
        }
        Scope::Residual(files) => (files, Vec::new(), true),
    };
    // Read inside the same unit that committed the reparse (or after the
    // whole-project link), so these are the links of exactly the text the
    // plugin is about to answer for.
    let linked = store.step(|conn| linked_edges(conn, language, &sent))?;
    if whole_project {
        crate::log_line!(
            "g-mesh: {language}'s whole-project semantic pass carries {} linked edge(s)",
            linked.len()
        );
    }
    let outcome = match round_trip(
        reader,
        writer,
        store,
        ControlMessage::SemanticPass {
            file_paths: sent.clone(),
            linked_edges: linked,
            // The plugin is told the timeout this round trip is held to, so a
            // residual pass (many files, the project timeout) and a per-file
            // one (the flat per-file timeout) can be told apart by budget
            // rather than by file count (GM-521).
            budget_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        },
        None,
        request_id,
        embedding,
        timeout,
        on_timeout,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            // A residual pass that failed outright finished none of its
            // files: each costs an attempt, or a file whose pass always times
            // out would be asked on every start for good. Best-effort: the
            // pass's own error is what the caller records.
            if residual {
                if let Err(settle) =
                    store.step(|conn| schema::settle_owed_files(conn, language, &[], &[], &sent))
                {
                    crate::log_line!(
                        "g-mesh: failed to count an attempt for {language}'s residual files ({settle:#})"
                    );
                }
            }
            return Err(err);
        }
    };

    // The diff is committed by now, deliberately: an incomplete pass is a
    // *partial* answer, not a failed one, and everything it did resolve is as
    // real as any other semantic edge (`protocol::types::
    // FileChangeResponse::incomplete` says why the plugin does not report this
    // as a JSON-RPC error instead). What is left to do is refuse to call the
    // pass finished. For a whole-project pass that does not name its
    // unfinished files, that is exactly what an `Err` here means to
    // `daemon::semantic`: `language_state.semanticPassAt` stays unset and the
    // next daemon start asks the whole project again. One that names them is
    // recorded residual instead (GM-521): the named files are owed, the next
    // start asks only those, and nothing is recorded as a failure.
    //
    // A per-file pass has no completion flag to protect, so an incomplete one
    // is worth a line and nothing more - failing it would only make
    // `apply_file_change` log the same thing twice.
    if outcome.incomplete && whole_project {
        let reason = outcome.incomplete_reason.as_deref().unwrap_or("the plugin gave no reason");
        let Some(unfinished) = outcome.unfinished_files else {
            bail!("the plugin reported an incomplete whole-project semantic pass: {reason}");
        };
        let left = store
            .step(|conn| schema::record_language_semantic_residual(conn, language, &unfinished, reason))?;
        crate::log_line!(
            "g-mesh: {language}'s whole-project semantic pass was incomplete ({reason}) - {left} file(s) left, \
             the next start asks only those"
        );
        return Ok(SemanticPassOutcome::left(left));
    }
    if residual {
        // No list: a complete answer finished every file sent, an incomplete
        // one is not known to have finished any.
        let unfinished: Vec<String> = match &outcome.unfinished_files {
            Some(named) => sent.iter().filter(|path| named.contains(path)).cloned().collect(),
            None if outcome.incomplete => sent.clone(),
            None => Vec::new(),
        };
        let settled: Vec<String> = sent.iter().filter(|path| !unfinished.contains(path)).cloned().collect();
        let left = store.step(|conn| {
            schema::settle_owed_files(conn, language, &[], &settled, &unfinished)?;
            schema::owed_files(conn, language).map(|owed| owed.len())
        })?;
        crate::log_line!(
            "g-mesh: {language}'s residual semantic pass finished {} of {} file(s) - {left} left",
            settled.len(),
            sent.len()
        );
        return Ok(SemanticPassOutcome::left(left));
    }
    if !whole_project {
        if outcome.incomplete {
            crate::log_line!(
                "g-mesh: the plugin reported an incomplete per-file semantic pass - its edges keep whatever \
                 this pass did resolve"
            );
        }
        match outcome.unfinished_files {
            // The plugin named what it did not finish, so every other file
            // sent is settled, whatever `incomplete` says: no longer
            // pending (ADR 0009) and no longer owed. A named file outside the
            // scope sent is ignored - this pass was not asked about it.
            Some(unfinished) => {
                let unfinished: Vec<String> =
                    sent.iter().filter(|path| unfinished.contains(path)).cloned().collect();
                let settled: Vec<String> =
                    sent.iter().filter(|path| !unfinished.contains(path)).cloned().collect();
                // Best-effort: a pending row left behind only over-warns
                // until the whole-project pass clears it, and an owed row
                // left as it was is asked once more.
                if let Err(err) = store.step(|conn| {
                    schema::clear_semantic_pending_files(conn, &settled)?;
                    schema::settle_owed_files(conn, language, &file_paths, &settled, &unfinished)
                }) {
                    crate::log_line!(
                        "g-mesh: failed to settle the semantic-pending and owed files of a per-file pass ({err:#})"
                    );
                }
            }
            // No list: a complete per-file pass has refreshed the files it
            // was sent, an incomplete one is not known to have refreshed any.
            // Best-effort, as above.
            None if !outcome.incomplete => {
                if let Err(err) = store.step(|conn| schema::clear_semantic_pending_files(conn, &file_paths)) {
                    crate::log_line!(
                        "g-mesh: failed to clear the semantic-pending files of a per-file pass ({err:#})"
                    );
                }
            }
            None => {}
        }
    } else if let Some(language) = sweep_language {
        let swept = store.step(|conn| sweep_semantic_edges(conn, language, &outcome.upserted_edges))?;
        if swept > 0 {
            crate::log_line!(
                "g-mesh: {language}'s whole-project semantic pass no longer stands behind {swept} \
                 semantic edge(s) - deleted"
            );
        }
        let swept = store.sweep_unclaimed_nodes(language)?;
        if swept > 0 {
            crate::log_line!(
                "g-mesh: {language}'s whole-project semantic pass did not re-send {swept} placeholder(s) \
                 its last workspace reindex kept - deleted"
            );
        }
    }
    Ok(SemanticPassOutcome::Complete)
}

/// The structural edges of `language` that linking moved onto a declaration,
/// each with the target it moved it to - what a `semanticPass` request
/// carries as `linkedEdges`. `file_paths` scopes them by the file the edge
/// starts in; empty means every file, as for the pass itself. A semantic
/// edge is left out even when linked: the tier compares its answers with
/// what the structural pass and linking settled, not with its own.
pub(crate) fn linked_edges(
    conn: &rusqlite::Connection,
    language: &str,
    file_paths: &[String],
) -> Result<Vec<LinkedEdge>> {
    let mut sql = String::from(
        "SELECT e.id, e.toId FROM edges e JOIN nodes f ON f.id = e.fromId
         WHERE e.linkedFrom IS NOT NULL AND e.source = 'syntactic' AND f.language = ?1",
    );
    if !file_paths.is_empty() {
        let slots: Vec<String> = (0..file_paths.len()).map(|i| format!("?{}", i + 2)).collect();
        sql.push_str(&format!(" AND f.filePath IN ({})", slots.join(", ")));
    }
    sql.push_str(" ORDER BY e.id");
    let params = std::iter::once(&language as &dyn rusqlite::ToSql)
        .chain(file_paths.iter().map(|path| path as &dyn rusqlite::ToSql));
    conn.prepare(&sql)
        .and_then(|mut statement| {
            statement
                .query_map(rusqlite::params_from_iter(params), |row| {
                    Ok(LinkedEdge { edge_id: row.get(0)?, to_id: row.get(1)? })
                })?
                .collect()
        })
        .context("failed to read the linked edges of a semantic pass")
}

/// Deletes every edge with `source = 'semantic'` whose `fromId` is a node of
/// `language` and whose id is not in `kept`, in one transaction, and returns
/// how many went. Placeholder nodes the deleted edges pointed at stay, as
/// every linked-away placeholder does (`graph::symbol_links`, "Why the
/// placeholder is kept").
pub(crate) fn sweep_semantic_edges(
    conn: &mut rusqlite::Connection,
    language: &str,
    kept: &std::collections::HashSet<String>,
) -> Result<usize> {
    let tx = conn.transaction().context("failed to start the semantic-edge sweep")?;
    let candidates: Vec<String> = tx
        .prepare(
            "SELECT e.id FROM edges e JOIN nodes n ON n.id = e.fromId
             WHERE e.source = 'semantic' AND n.language = ?1",
        )
        .and_then(|mut statement| {
            statement.query_map(rusqlite::params![language], |row| row.get(0))?.collect()
        })
        .context("failed to read the semantic edges to sweep")?;
    let mut swept = 0;
    for id in candidates.iter().filter(|id| !kept.contains(*id)) {
        swept += tx
            .execute("DELETE FROM edges WHERE id = ?1", rusqlite::params![id])
            .context("failed to delete a semantic edge the pass no longer sends")?;
    }
    tx.commit().context("failed to commit the semantic-edge sweep")?;
    Ok(swept)
}

/// What one round trip reported about itself, beyond the diff it already
/// committed - [`FileChangeResponse::incomplete`] and the plugin's reason for
/// it, meaningless for a `fileChanged` and load-bearing for a `semanticPass`.
struct RoundTrip {
    incomplete: bool,
    incomplete_reason: Option<String>,
    /// [`FileChangeResponse::unfinished_files`]: the files of a
    /// `semanticPass`'s scope the plugin did not finish, or `None` when it
    /// did not say.
    unfinished_files: Option<Vec<String>>,
    /// The ids of every edge the diff upserted.
    upserted_edges: std::collections::HashSet<String>,
}

/// The id for the semantic pass that follows a file change, derived from
/// that file change's own id.
///
/// Both round trips happen back to back on one stream that the caller
/// (`daemon::plugin::PluginProcess`) holds locked for their whole duration,
/// so the second needs an id the plugin cannot confuse with the first, not
/// a globally unique one. Deriving beats threading a second id through
/// every caller, and cannot collide with the counter-issued ids either:
/// those are always `Number`, these are always `String`, and `RequestId`
/// compares across variants.
///
/// Which variant it was derived from is part of the derived id, so the two
/// id spaces cannot alias: without it a `Number(3)` and a `String("3")`
/// base - both legal per JSON-RPC 2.0, and both reachable, since
/// `PluginProcess` issues numbers while callers may pass strings - would
/// derive the very same id.
fn semantic_pass_id(base: &RequestId) -> RequestId {
    RequestId::String(match base {
        RequestId::Number(n) => format!("semanticPass:num:{n}"),
        RequestId::String(s) => format!("semanticPass:str:{s}"),
    })
}

/// One control-plane round trip that answers with a diff: write the
/// request, read the response, check that it answers *this* request, then
/// commit and link the diff it carried.
///
/// Shared by [`apply_file_change`] and [`apply_semantic_pass`], which
/// differ only in the message they send. Everything after the response is
/// identical, which is the whole reason a semantic upgrade needed no
/// storage or schema work of its own.
///
/// The write above is not timed: this module's whole concern is the read
/// having no timeout (see this module's own doc comment and
/// `docs/architecture/multi-language-plugins.md`'s "Semantic engine hangs or
/// is slow" failure mode), and a write blocking would need the plugin's own
/// stdin pipe buffer to be full *and* the plugin to have stopped reading it -
/// a materially different, and much rarer, failure than a plugin that reads a
/// request and simply never answers it.
///
/// `timeout` bounds only the read; `on_timeout` is this function's entire
/// interface to whatever makes that read actually give up - see
/// `protocol::jsonrpc::read_message_with_timeout`'s doc comment for the
/// mechanism and why it lives there instead of here. This module stays
/// transport-agnostic on purpose (see its own doc comment): it has no idea
/// whether `reader`/`writer` are a spawned plugin's pipes or a test's bare
/// `std::io::pipe()`, so it cannot itself know what "make the peer stop being
/// silent" means - only the caller that owns the transport does (for a real
/// plugin, `daemon::plugin::PluginProcess` kills the child).
///
/// # Lock holds
///
/// Two steps of the caller's [`Unit::WatcherApply`]: one commits and links
/// the diff, the other stores its vectors. Embedding inference runs between
/// them, so under that unit's per-step policy no other store user waits on
/// it. The plugin round trip itself stays serialized by
/// `daemon::plugin::PluginProcess`'s own `state` lock.
///
/// In the gap, another writer can commit newer content for a node this
/// round trip is about to store a vector for. [`EmbeddingPipeline::store`]
/// re-checks each node's current content before writing, so only a node
/// still holding the exact text the embedding was computed from gets it.
#[allow(clippy::too_many_arguments)]
fn round_trip<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    store: &mut Writer<'_>,
    message: ControlMessage,
    project_root: Option<&Path>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<RoundTrip> {
    let method = method_name(&message);
    let request =
        ControlEnvelope { jsonrpc: JSONRPC_VERSION.to_string(), id: Some(request_id.clone()), message };
    write_message(writer, &request).with_context(|| format!("failed to write {method} request to plugin"))?;

    let response: FileChangeResponse = read_message_with_timeout(reader, timeout, on_timeout)
        .with_context(|| format!("failed to read plugin's {method} response"))?
        .with_context(|| format!("plugin closed its output before responding to {method}"))?;

    if response.id != request_id {
        bail!(
            "{method} response id {:?} does not match request id {:?} - refusing to apply a diff that answers a different request",
            response.id,
            request_id,
        );
    }

    let complete = response.result.complete;
    let mut diff = to_storage_diff(response.result, &mut PathWarnings::default());
    match (&request.message, project_root) {
        (ControlMessage::FileChanged { file_path, .. }, Some(root)) => {
            let scope = file_scope(root, file_path, &diff, complete);
            store.apply_file_diff_linked(&mut diff, file_path, scope, method)?;
        }
        (ControlMessage::SemanticPass { .. }, _) => store.apply_semantic_diff_linked(&mut diff, method)?,
        _ => store.apply_diff_linked(&diff, method)?,
    }

    hold_compute_open_for_tests();

    // A semantic answer carries no new text: a node it re-sends that was
    // already stored kept its text and its vector (`apply_semantic_diff`).
    // Embedding it again would recompute an unchanged vector, so only a node
    // that has none yet is embedded. The diff is
    // committed already, and from here on only its edges are read.
    if matches!(request.message, ControlMessage::SemanticPass { .. }) {
        let embedded = store.step(|conn| nodes_with_vectors(conn, &diff));
        diff.upsert_nodes.retain(|node| !embedded.contains(&node.id));
    }

    // Runs between the unit's steps - see "Lock holds" above.
    let started = std::time::Instant::now();
    let mut stats = EmbedStats::default();
    let computed = embedding.compute(&diff, &mut stats);
    embedding.finish_file_change(method, &stats, started.elapsed());

    // Best-effort, like a failed semantic pass: a diff that is already
    // committed and linked is not undone by an optional layer on top of it.
    store.store_vectors(embedding, &computed);
    Ok(RoundTrip {
        incomplete: response.incomplete,
        incomplete_reason: response.incomplete_reason,
        unfinished_files: response.unfinished_files,
        upserted_edges: diff.upsert_edges.iter().map(|edge| edge.id.clone()).collect(),
    })
}

/// The upserted nodes of `diff` that already have a vector. A failed lookup
/// counts as none, which embeds them all, as before GM-486.
fn nodes_with_vectors(conn: &rusqlite::Connection, diff: &Diff) -> HashSet<String> {
    let lookup = || -> rusqlite::Result<HashSet<String>> {
        let mut stmt = conn.prepare_cached("SELECT EXISTS (SELECT 1 FROM vectors WHERE nodeId = ?1)")?;
        let mut found = HashSet::new();
        for node in &diff.upsert_nodes {
            if stmt.query_row([&node.id], |row| row.get::<_, bool>(0))? {
                found.insert(node.id.clone());
            }
        }
        Ok(found)
    };
    lookup().unwrap_or_else(|err| {
        crate::log_line!("g-mesh daemon: failed to check a semantic answer's nodes for vectors ({err:#})");
        HashSet::new()
    })
}

/// What a `fileChanged` answer says about its file as a whole: gone when it
/// upserts nothing and the file is not on disk, complete when the plugin says
/// so, otherwise a change against what the plugin last sent. The disk is read
/// after the plugin answered; a file re-created in between gets its own
/// change event.
fn file_scope(project_root: &Path, file_path: &str, diff: &Diff, complete: bool) -> FileScope {
    let upserts_nothing = diff.upsert_nodes.is_empty() && diff.upsert_edges.is_empty();
    let gone = upserts_nothing
        && matches!(std::fs::metadata(project_root.join(file_path)), Err(err) if err.kind() == std::io::ErrorKind::NotFound);
    if gone {
        FileScope::Gone
    } else if complete {
        FileScope::Complete
    } else {
        FileScope::Partial
    }
}

/// Test-only: holds this round trip open between its commit step and its
/// vector-store step, where embedding inference runs, for as long as the
/// file named by [`HOLD_COMPUTE_FILE_ENV`] exists. Lets a test prove a
/// concurrent store user is not blocked on an incremental reparse's
/// embedding step without real model weights. A no-op unless set.
fn hold_compute_open_for_tests() {
    let Some(path) = std::env::var_os(HOLD_COMPUTE_FILE_ENV).filter(|p| !p.is_empty()) else { return };
    let path = std::path::PathBuf::from(path);
    crate::log_line!(
        "g-mesh: holding a reparse's lock-free embedding window open until {} is removed \
         ({HOLD_COMPUTE_FILE_ENV})",
        path.display()
    );
    // Bounded, so a test that forgets to delete the file fails as a timeout
    // instead of wedging the daemon.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Path whose *deletion* releases a round trip that is holding its lock-free
/// embedding window open for a test - see [`hold_compute_open_for_tests`].
pub const HOLD_COMPUTE_FILE_ENV: &str = "G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE";

/// The wire `method` string for a control message, for error messages that
/// name the request that failed. Matched rather than round-tripped through
/// serde so that adding a variant to `ControlMessage` fails to compile here
/// instead of silently reporting the wrong method name.
fn method_name(message: &ControlMessage) -> &'static str {
    match message {
        ControlMessage::Reindex { .. } => "reindex",
        ControlMessage::FileChanged { .. } => "fileChanged",
        ControlMessage::Status => "status",
        ControlMessage::SemanticPass { .. } => "semanticPass",
        ControlMessage::WorkspaceChanged { .. } => "workspaceChanged",
        ControlMessage::PrepareSemanticPass => "prepareSemanticPass",
        ControlMessage::FilesCreated { .. } => "filesCreated",
        ControlMessage::ResolutionChanged { .. } => "resolutionChanged",
    }
}

/// Converts the wire-level diff (nested `range: {start, end}`) into the
/// storage layer's flat `start_line`/`start_col`/`end_line`/`end_col`
/// fields. `delete_node_ids`/`delete_edge_ids` pass through unchanged since
/// both sides already agree on `Vec<String>`.
fn to_storage_diff(wire: FileChangeDiff, warnings: &mut PathWarnings) -> Diff {
    Diff {
        upsert_nodes: wire.upsert_nodes.into_iter().map(|node| to_node_record(node, warnings)).collect(),
        delete_node_ids: wire.delete_node_ids,
        upsert_edges: wire.upsert_edges.into_iter().map(to_edge_record).collect(),
        delete_edge_ids: wire.delete_edge_ids,
    }
}

/// Warns about a plugin's invalid paths at most once per (language, file),
/// so a plugin that gets every path in a file wrong logs one line for it.
#[derive(Debug, Default)]
pub(crate) struct PathWarnings {
    warned: HashSet<(String, String)>,
    /// Every line written, for tests.
    pub(crate) emitted: Vec<String>,
}

impl PathWarnings {
    fn warn(&mut self, node: &WireNode, what: &str, error: &PathError) {
        if !self.warned.insert((node.language.clone(), node.file_path.clone())) {
            return;
        }
        let line = format!(
            "g-mesh daemon: the {} plugin sent an invalid {what} for {:?} in {}: {error}; \
             dropped it and kept the node (later invalid paths in this file are dropped without a warning)",
            node.language, node.qualified_name, node.file_path
        );
        crate::log_line!("{line}");
        self.emitted.push(line);
    }
}

/// The node's paths that pass their rules. A `qualifiedPath` that fails
/// drops with all its aliases; an alias that fails, including one sent
/// without a `qualifiedPath`, drops alone. The node itself is always kept.
fn checked_paths(
    node: &WireNode,
    warnings: &mut PathWarnings,
) -> (Option<QualifiedPath>, Vec<QualifiedPath>) {
    if let Err(error) = node.check_qualified_path() {
        warnings.warn(node, "qualifiedPath", &error);
        return (None, Vec::new());
    }
    let aliases = node
        .alias_paths
        .iter()
        .filter(|alias| match node.check_alias_path(alias) {
            Ok(()) => true,
            Err(error) => {
                warnings.warn(node, "aliasPaths entry", &error);
                false
            }
        })
        .cloned()
        .collect();
    (node.qualified_path.clone(), aliases)
}

/// Wire node -> storage record. Shared with the cold-start bulk index
/// (`daemon::bulk_index`), which ingests the very same `WireNode` shape off
/// an NDJSON stream instead of out of a diff response - the two paths must
/// never disagree about how a wire node becomes a row.
pub(crate) fn to_node_record(node: WireNode, warnings: &mut PathWarnings) -> NodeRecord {
    let (visibility, visibility_container) = to_storage_visibility(&node.visibility);
    let (qualified_path, alias_paths) = checked_paths(&node, warnings);
    let key_path = node.target.as_ref().and_then(|target| match target.check_key_path() {
        Ok(()) => target.key_path.clone(),
        Err(error) => {
            warnings.warn(&node, "keyPath", &error);
            None
        }
    });
    NodeRecord {
        id: node.id,
        kind: format!("{:?}", node.kind),
        name: node.name,
        qualified_name: node.qualified_name,
        file_path: node.file_path.clone(),
        start_line: node.range.start.line as i64,
        start_col: node.range.start.col as i64,
        end_line: node.range.end.line as i64,
        end_col: node.range.end.col as i64,
        signature: node.signature,
        // The read-side mirror of the database's `GENERATED ALWAYS exported`
        // column - see `NodeRecord.exported`'s own doc comment for why this
        // still has to be set correctly here even though `apply_diff` never
        // writes it: `graph::symbol_links::link_diff` filters this very
        // `Diff` in memory, before anything is read back from storage.
        exported: matches!(node.visibility, Visibility::Public),
        visibility,
        visibility_container,
        container: node.container,
        // Carried through to `apply_diff`, where `graph::containers` stores it
        // on the container's `containers.parentKey` - it describes the
        // container, not this member, so it has no `nodes` column of its own.
        container_parent: node.container_parent,
        target: node.target.as_ref().map(|target| to_placeholder_target_record(target, key_path)),
        doc_comment: node.doc_comment,
        language: node.language,
        native_kind: node.native_kind,
        has_syntax_errors: node.has_syntax_errors,
        // Absent means "one declaration", which is no rows in the child table
        // - the same thing an empty list means to `apply_diff`, which replaces
        // whatever was stored either way.
        declarations: node
            .declarations
            .unwrap_or_default()
            .into_iter()
            .map(|declaration| DeclarationRecord {
                ordinal: declaration.ordinal as i64,
                start_line: declaration.start_line as i64,
                start_col: declaration.start_col as i64,
                end_line: declaration.end_line as i64,
                end_col: declaration.end_col as i64,
                signature: declaration.signature,
                has_body: declaration.has_body,
            })
            .collect(),
        qualified_path,
        alias_paths,
        untyped_calls: node.untyped_calls,
    }
}

/// `protocol::types::Visibility` -> `(nodes.visibility, nodes.
/// visibilityContainer)`. See `storage::schema`'s DDL comment on those two
/// columns for why the container's key gets its own nullable column rather
/// than being packed into `visibility` itself.
fn to_storage_visibility(visibility: &Visibility) -> (String, Option<String>) {
    match visibility {
        Visibility::Public => ("public".to_string(), None),
        Visibility::File => ("file".to_string(), None),
        Visibility::Container(key) => ("container".to_string(), Some(key.clone())),
    }
}

/// `protocol::types::PlaceholderTarget` -> `storage::write::
/// PlaceholderTargetRecord`. Notably does *not* set a `from_file` - the wire
/// type has no such field, because it is already available for free as the
/// placeholder node's own `filePath` (the existing "a placeholder's filePath
/// is the importing file" convention - `graph::symbol_links`'s module doc),
/// so `storage::write::apply_diff` fills `placeholder_targets.fromFile` from
/// `NodeRecord.file_path` directly rather than threading it through this
/// record - see that table's own DDL comment.
/// `key_path` is the target's own, already checked by the caller.
fn to_placeholder_target_record(
    target: &PlaceholderTarget,
    key_path: Option<QualifiedPath>,
) -> PlaceholderTargetRecord {
    let (scope_kind, scope) = match &target.scope {
        TargetScope::File(path) => ("file".to_string(), path.clone()),
        TargetScope::Container(key) => ("container".to_string(), key.clone()),
    };
    let (key_kind, key) = match &target.key {
        TargetKey::Name(name) => ("name".to_string(), name.clone()),
        TargetKey::QualifiedName(qualified_name) => ("qualifiedName".to_string(), qualified_name.clone()),
    };
    PlaceholderTargetRecord {
        scope_kind,
        scope,
        key_kind,
        key,
        from_container: target.from_container.clone(),
        key_path,
    }
}

/// Wire edge -> storage record; see [`to_node_record`] on why this is shared.
pub(crate) fn to_edge_record(edge: WireEdge) -> EdgeRecord {
    let mut record = EdgeRecord::new(
        edge.id,
        edge.from_id,
        edge.to_id,
        edge_kind_wire_value(&edge.kind),
        edge_source_tier_wire_value(&edge.source),
        edge.resolved,
    );
    // `EdgeRecord::new`'s legacy-string inference (`storage::write::
    // normalize_legacy_source`) sets `.engine` to a copy of the tier string
    // passed above, which is only ever right for a v1 caller with no real
    // engine to report. A `WireEdge` always has a real one - the protocol's legacy
    // normalization already synthesizes `"tree-sitter"`/`"ts-compiler"` for a
    // v1 sender, so `edge.engine` is populated regardless of which protocol
    // version produced this edge - so it overwrites the guess here, the same
    // way `to_declaration` below is set post-construction rather than
    // threaded through `new`.
    record.engine = edge.engine;
    record.to_declaration = edge.to_declaration.map(|ordinal| ordinal as i64);
    record.specifier = edge.specifier;
    record
}

/// `nodes.kind` in the schema is a plain string matching `NodeKind`'s Rust
/// variant name (see `WireNode`'s doc comment in `protocol::types`); no
/// custom serde attributes are attached to `NodeKind`, so `{:?}` already
/// gives the exact variant name (e.g. "Function").
///
/// `edges.kind`, by contrast, has a custom serde rename
/// (`SCREAMING_SNAKE_CASE`) - reuse that exact wire string by round-tripping
/// through serde_json rather than re-deriving the mapping by hand, so the
/// storage string always matches what the wire format (and therefore what
/// the plugin actually sent) says, and any future rename attribute change on
/// this enum doesn't silently desync this file.
fn edge_kind_wire_value(kind: &crate::protocol::types::EdgeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// `SourceTier` -> `edges.source`'s own two values (`storage::schema`'s DDL:
/// `CHECK (source IN ('syntactic', 'semantic'))`). A direct 1:1 mapping, not
/// the legacy engine-conflating one `storage::write::normalize_legacy_source`
/// still carries for callers that only have a v1 string - a `WireEdge`
/// always has a real, separate `engine` (see [`to_edge_record`]'s own
/// comment), so the tier alone is all this needs to produce.
fn edge_source_tier_wire_value(source: &SourceTier) -> String {
    match source {
        SourceTier::Syntactic => "syntactic".to_string(),
        SourceTier::Semantic => "semantic".to_string(),
    }
}

#[cfg(test)]
mod tests;
