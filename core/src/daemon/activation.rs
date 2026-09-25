//! Lazy activation (D2 and D8 in
//! `docs/architecture/lazy-indexing.md`): everything `daemon::run` used to do
//! eagerly after binding its socket - register the watcher, walk, record
//! `bulkIndexedAt`, run the semantic pass (or its owed retry), start the
//! watcher's consumer, run the embedding backfill pass, mark ready - now
//! runs here, on a thread of its own that parks until the first
//! index-needing tool call asks for it
//! (`IndexingStatus::request_activation`, from `mcp::GMeshMcpServer::prepare`).
//!
//! A session that connects and never calls a tool therefore costs nothing:
//! no `--bulk-index` child, no plugin, and - for a project that has never
//! been walked - no watcher either.
//!
//! # Independent of the caller
//!
//! The call that sends the trigger only ever *waits* on the outcome
//! (`IndexingStatus::wait_for`). The work itself runs here, so a caller that
//! is cancelled, times out or disconnects does not stop it: the next call
//! finds it running or finished.
//!
//! # Failure is reported, not fatal
//!
//! A failed walk used to end the daemon. Under a lazy trigger that would drop
//! the session of the very call that asked, so instead the phase becomes
//! `Phase::Failed(message)` - every waiter turns it into a tool error that
//! carries the message (for example `plugin::missing_plugin_binary_hint`) -
//! and this thread goes back to waiting. The next tool call re-requests
//! activation, and the walk is retried. The old rationale for exiting ("a
//! daemon that stayed up would hold the singleton lock while serving
//! nothing, and every later shim would reuse it forever") no longer holds,
//! because this daemon now says why on every call and retries.
//!
//! Embedding-pass failures stay best-effort, as before: they never produce
//! `Failed`.

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

use crate::daemon::bulk_index;
use crate::daemon::indexing_status::{IndexingStatus, Phase};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::DiscoveredPlugins;
use crate::daemon::registry::PluginRegistry;
use crate::daemon::semantic;
use crate::embedding::EmbeddingPipeline;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::watcher::ProjectWatcher;

/// Everything the activation thread needs, handed over once by `daemon::run`.
pub(super) struct ActivationCtx {
    pub conn: Arc<IndexStore>,
    pub registry: Arc<PluginRegistry>,
    pub embedding: Arc<EmbeddingPipeline>,
    /// The bulk walk's own copy of discovery - see `daemon::run`'s comment
    /// where it is cloned for why it is not read out of `registry`.
    pub discovered_for_bulk_index: DiscoveredPlugins,
    /// What the watcher's consumer and the walk resolve paths against.
    pub canonical_root: PathBuf,
    /// What the watcher is registered on - the same spelling `daemon::run`
    /// always handed `ProjectWatcher::new`.
    pub root: PathBuf,
    pub indexing: IndexingStatus,
    pub core_activity: Arc<CoreActivity>,
    /// The project owes its structural walk (`!bulk_index_completed` at
    /// startup). Cleared once a walk has been recorded, so a retry after a
    /// later failure does not walk again.
    pub needs_walk: bool,
    /// The project was walked but its semantic pass never completed - see
    /// `daemon::semantic`'s module doc. Never set together with `needs_walk`:
    /// a walk runs the pass itself.
    pub needs_semantic_pass_retry: bool,
    /// A watcher whose consumer this activation still owes (D8). `Some` for
    /// an already-walked project whose semantic retry is owed: `daemon::run`
    /// registered it at startup but left it undrained until the retry is
    /// done. `None` both for a project whose consumer already runs and for
    /// an unindexed project - `needs_walk` tells those apart, and for the
    /// latter this activation registers the watcher itself, before its walk.
    pub watcher: Option<ProjectWatcher>,
}

/// Starts the parked activation thread. It does nothing until `triggered`
/// receives - see [`IndexingStatus::request_activation`].
pub(super) fn spawn(ctx: ActivationCtx, triggered: Receiver<()>) -> Result<()> {
    thread::Builder::new()
        .name("g-mesh-activation".to_string())
        .spawn(move || run(ctx, triggered))
        .context("failed to start the daemon's activation thread")?;
    Ok(())
}

/// The activation loop: wait for a trigger, do the owed work, and either
/// return for good (success - nothing is ever owed again for the life of
/// this process) or record the failure and wait for the next trigger.
fn run(mut ctx: ActivationCtx, triggered: Receiver<()>) {
    while triggered.recv().is_ok() {
        // Held for the whole activation, so the core's idle timer cannot
        // end the daemon mid-walk once the triggering session has gone away
        // (D2's "independent of the caller"). Before slice 2 the same
        // guarantee came for free: `lifecycle::supervise` only started after
        // the eager startup work had finished.
        let _busy = ctx.core_activity.connection_opened();
        let attempt = panic::catch_unwind(AssertUnwindSafe(|| ctx.activate()));
        let failure = match attempt {
            Ok(Ok(())) => return,
            Ok(Err(err)) => format!("{err:#}"),
            Err(payload) => {
                let what = payload
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                format!("the index build panicked: {what}")
            }
        };
        eprintln!("g-mesh daemon: {failure} - the next tool call retries");
        ctx.indexing.activation_failed(failure);
    }
}

impl ActivationCtx {
    /// One activation attempt: whatever of walk, semantic pass, watcher
    /// consumer and embedding backfill this project still owes, in that
    /// order. `Err` only for a failure of the walk (or of what it strictly
    /// needs); the semantic and embedding passes stay best-effort.
    fn activate(&mut self) -> Result<()> {
        if self.needs_walk {
            self.walk()?;
        } else if self.needs_semantic_pass_retry {
            // The walk this project was owed already happened, in some
            // earlier start or in `cli::init` / `cli::reindex`, so the branch
            // above that would normally run this pass never will. Run here
            // instead, before the watcher's consumer (below) can start any
            // incremental pass of its own - see the ordering comment in
            // `walk` - and at most once per daemon start.
            eprintln!(
                "g-mesh daemon: the project was walked but its semantic pass never completed - retrying it"
            );
            semantic::run_with_registry_and_progress(&self.registry, &self.conn, Some(&self.indexing))
                .log("the previously-interrupted index");
            self.needs_semantic_pass_retry = false;
        }

        // After the structural walk and its semantic pass, never before: a
        // bulk walk racing incremental updates could commit its own (older)
        // parse of a file over one the watcher had just refreshed. The
        // watcher was *registered* before the walk, so no event is lost in
        // between - it has only queued in the watcher's channel.
        //
        // Before the embedding backfill pass, though: that pass can run for
        // minutes, it does not hold the store between its own batches
        // (see `embedding::backfill::run`), and an edit made during it
        // should still be applied rather than left queued until it returns.
        if let Some(watcher) = self.watcher.take() {
            super::spawn_watch_consumer(
                watcher,
                Arc::clone(&self.conn),
                Arc::clone(&self.registry),
                self.canonical_root.clone(),
            );
        }

        // Runs on every activation, not only after a walk: "the structural
        // graph is complete" says nothing about whether every embeddable node
        // has a `vectors` row yet (a previous start with no model available,
        // an interrupted pass, the staleness skip on an incremental
        // edit). On a project with nothing left to embed it is one `COUNT(*)`.
        // A panic here must not reach `run`'s catch_unwind: that would turn a
        // complete structural index into `Failed`, so every structural tool
        // would error over a pass only `search_code` needs, and each retry
        // would re-run the same pass. Nodes left without a vector are picked
        // up by the next start's backfill.
        self.indexing.set_phase(Phase::Embedding);
        let backfill = panic::catch_unwind(AssertUnwindSafe(|| {
            crate::embedding::backfill::run(&self.conn, &self.embedding, &self.indexing)
        }));
        match backfill {
            Ok(summary) if summary.candidates > 0 => eprintln!(
                "g-mesh daemon: embedding backfill - {} of {} candidate nodes embedded",
                summary.embedded, summary.candidates
            ),
            Ok(_) => {}
            Err(_) => eprintln!(
                "g-mesh daemon: the embedding backfill pass panicked - structural tools are unaffected, \
                 search_code covers only the nodes embedded so far"
            ),
        }
        self.indexing.set_phase(Phase::Ready);
        Ok(())
    }

    /// The structural walk, its completion marker and the semantic pass that
    /// follows it.
    fn walk(&mut self) -> Result<()> {
        // Already `Walking` if `request_activation` came from `Unindexed` or
        // `Failed` (it moves the phase itself); set here too so this method
        // does not depend on how it was reached.
        self.indexing.set_phase(Phase::Walking);

        // Registered before the walk, and deliberately not *drained* until
        // after it (`activate`). `ProjectWatcher` is backed by an mpsc
        // channel, so from this line on every event queues up whether or
        // not anyone is reading. Without it, an edit made between the walk's
        // enumeration and the watcher's existence had no observer at all and
        // was lost. Kept across a failed attempt: a retry reuses it.
        if self.watcher.is_none() {
            self.watcher = Some(ProjectWatcher::new(&self.root).context("failed to start the file watcher")?);
        }

        // `embedding: None` - the walk is structural-only;
        // embedding is the backfill pass's job (`activate`). A tool call
        // issued while this runs waits for it (`mcp::GMeshMcpServer::prepare`)
        // rather than being answered off a half-built graph.
        let summary = bulk_index::run_with_progress(
            &self.canonical_root,
            &self.conn,
            None,
            &self.discovered_for_bulk_index,
            Some(&self.indexing),
        )
        .context("failed to build the project's initial index")?;

        // Flipped *before* the completion marker is written. The phase
        // governs what this process answers; `bulkIndexedAt` governs whether
        // the *next* process walks again. Writing the marker second means an
        // outside observer of it (`cli::status`, the integration tests) only
        // ever sees it once structural answers are already being given.
        self.indexing.set_phase(Phase::Structural);
        self.conn.with(schema::record_bulk_index).context("failed to record that the project was indexed")?;
        self.needs_walk = false;
        // A walk that took minutes is minutes the core spent working - the
        // same reasoning `last_used::touch` applies to a GC scan, applied to
        // the core's own idle timer.
        self.core_activity.request();
        eprintln!(
            "g-mesh daemon: initial index built - {} nodes, {} edges ({} imports linked to their target file)",
            summary.nodes, summary.edges, summary.linked_imports
        );
        if summary.skipped_lines > 0 {
            eprintln!(
                "g-mesh daemon: {} unreadable lines were skipped - the index may be incomplete",
                summary.skipped_lines
            );
        }

        // The graph is linked, so it is worth asking the type checker about -
        // after `Structural`, because the semantic layer's job is to make
        // existing answers better, never to delay the first one. Best-effort:
        // a checker that cannot start leaves a project served by its
        // structural graph.
        //
        // Inline, not backgrounded, deliberately (task 164, and
        // `core/tests/overload_call_binding.rs`): this pass and the watcher's
        // incremental per-file passes serialize on the same plugin process,
        // but nothing orders which of two independently-triggered passes
        // commits last, and a whole-project pass landing after a newer
        // incremental one can overwrite its fresher edges with stale ones.
        // Running it before the watcher's consumer starts (`activate`) keeps
        // it strictly first. Which plugins it asks is `daemon::semantic`'s to
        // say.
        semantic::run_with_registry_and_progress(&self.registry, &self.conn, Some(&self.indexing))
            .log("the freshly built index");
        Ok(())
    }
}
