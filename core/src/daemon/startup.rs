//! [`run`](super::run)'s phases, in the order it calls them, and the pure
//! [`StartupPlan`] that decides what the index's state at open asks of the
//! start. Each ordering constraint between side effects sits inside one phase
//! or at a phase boundary (`docs/design/GM-707-watcher-ingest-startup-phases.md`,
//! section 4).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

use crate::daemon::activation;
use crate::daemon::build_stamp;
use crate::daemon::lifecycle::{self, CoreActivity, IdleTimeouts};
use crate::daemon::registry::{self, PluginRegistry};
use crate::daemon::seed;
use crate::daemon::watch_loop::{spawn_watch_consumer, ConsumerStart, WATCH_CANARY_DIR};
use crate::daemon::workspace_reindex;
use crate::daemon::{acquire_singleton_lock, serve_forever, write_pid_file};
use crate::embedding::EmbeddingPipeline;
use crate::gc::last_used;
use crate::indexing_status::{ColdCause, IndexingStatus};
use crate::ipc;
use crate::paths::state_dir::StateDir;
use crate::plugin_manifest::DiscoveredPlugins;
use crate::storage::connection;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::watcher::ProjectWatcher;

/// What the index said at open; read by [`open_index`] in `run`'s order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexFacts {
    /// `stored_generation(..).is_some()`, read before `ensure_current`.
    pub had_generation: bool,
    /// `ensure_current(..)`: the index was (re)initialized.
    pub reinitialized: bool,
    pub walk: WalkFacts,
}

/// Whether the index was walked, and what only a walked index is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WalkFacts {
    /// `bulk_index_completed` is false: nothing else was read.
    Unwalked,
    Walked {
        semantic_pass_completed: bool,
        retry_languages: Vec<String>,
        seeded: bool,
    },
}

/// What this start does, decided from [`IndexFacts`] alone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StartupPlan {
    pub initial: InitialPhase,
    pub needs_walk: bool,
    pub needs_semantic_pass_retry: bool,
    pub retry_languages: Vec<String>,
    /// A [`seed::SeedGate`] is created; [`WatcherPlan`] says who gets it.
    pub seeded: bool,
    pub watcher: WatcherPlan,
}

/// The [`IndexingStatus`] the daemon publishes before it serves.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InitialPhase {
    /// [`IndexingStatus::unindexed_because`].
    Unindexed(ColdCause),
    /// Only `Seeded`; [`IndexingStatus::walking_because`].
    Walking(ColdCause),
    /// [`IndexingStatus::structural`].
    Structural,
}

/// When the watcher and its consumer start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WatcherPlan {
    /// Unwalked: activation starts the watcher right before its walk.
    StartedByActivation,
    /// Walked, a retry owed: the watcher starts now, activation starts its
    /// consumer (and gets the gate).
    ParkedForActivation,
    /// Walked, nothing owed: watcher and consumer start now (`Seeded` if
    /// `seeded`, else `Reconcile`).
    ConsumerNow,
}

impl StartupPlan {
    pub(crate) fn decide(facts: &IndexFacts) -> StartupPlan {
        let cold_cause = match (facts.had_generation, facts.reinitialized) {
            (false, _) => ColdCause::Fresh,
            (true, true) => ColdCause::Discarded,
            (true, false) => ColdCause::Incomplete,
        };
        match &facts.walk {
            WalkFacts::Unwalked => StartupPlan {
                initial: InitialPhase::Unindexed(cold_cause),
                needs_walk: true,
                needs_semantic_pass_retry: false,
                retry_languages: Vec::new(),
                seeded: false,
                watcher: WatcherPlan::StartedByActivation,
            },
            WalkFacts::Walked { semantic_pass_completed, retry_languages, seeded } => {
                let needs_semantic_pass_retry = !semantic_pass_completed;
                // A semantic-pass retry must run before any incremental pass,
                // and a failed language's retry before any edit of it routes.
                let watcher = if needs_semantic_pass_retry || !retry_languages.is_empty() {
                    WatcherPlan::ParkedForActivation
                } else {
                    WatcherPlan::ConsumerNow
                };
                StartupPlan {
                    initial: if *seeded {
                        InitialPhase::Walking(ColdCause::Seeded)
                    } else {
                        InitialPhase::Structural
                    },
                    needs_walk: false,
                    needs_semantic_pass_retry,
                    retry_languages: retry_languages.clone(),
                    seeded: *seeded,
                    watcher,
                }
            }
        }
    }
}

impl InitialPhase {
    fn status(&self) -> IndexingStatus {
        match *self {
            InitialPhase::Unindexed(cause) => IndexingStatus::unindexed_because(cause),
            InitialPhase::Walking(cause) => IndexingStatus::walking_because(cause),
            InitialPhase::Structural => IndexingStatus::structural(),
        }
    }
}

/// Takes the singleton lock and, once held, clears a crashed rebuild's marker
/// and publishes the build stamp. `None`: another process holds the lock.
pub(super) fn claim(dir: &StateDir) -> Result<Option<File>> {
    // Singleton guard, before anything else touches the project's files.
    // Losing it is the healthy outcome: the caller connects to the incumbent.
    let Some(singleton) = acquire_singleton_lock(dir)? else {
        return Ok(None);
    };
    // A daemon holding the lock means no rebuild holds it (a rebuild takes
    // the same lock), so any marker left here is a crashed rebuild's: once
    // its pid is reused it would read as live and turn every shim away.
    dir.clear_stale_rebuild_marker();

    // After the singleton lock, before the bind (never transiently missing).
    // Not fatal: a daemon that reads as outdated gets replaced.
    match build_stamp::of_running_process() {
        Ok(stamp) => {
            if let Err(err) = build_stamp::write(&dir.build_stamp_file(), &stamp) {
                crate::log_role!("could not publish its build stamp: {err:#}");
            }
        }
        Err(err) => crate::log_role!("could not describe its own build: {err:#}"),
    }
    Ok(Some(singleton))
}

/// The opened index, the plugins it was checked against, and what it said.
pub(super) struct OpenedIndex {
    pub conn: Arc<IndexStore>,
    pub discovered: DiscoveredPlugins,
    pub facts: IndexFacts,
}

/// Discovers the plugins, seeds and opens the index, brings its schema
/// current and reads [`IndexFacts`] from it.
pub(super) fn open_index(root: &Path, dir: &StateDir) -> Result<OpenedIndex> {
    // Discovery comes before the index (the generation check below needs it)
    // and before the bind, so a malformed manifest or two plugins claiming one
    // extension fail startup before a socket or pid file is published.
    let discovered = crate::plugin_manifest::discover(&crate::plugin_manifest::default_roots())
        .context("failed to discover language plugins")?;

    // Before the index is opened and before any reindex can run: a staging
    // file left here belongs to a reindex that died.
    workspace_reindex::remove_stale_staging(dir.path());
    seed::remove_stale_seed_tmp(dir.path());
    // Before `connection::open`, which would create an empty `index.db`.
    if let Some(origin) = seed::seed_if_eligible(root, dir.path(), &registry::indexer_version(&discovered)) {
        crate::log_role!("index seeded from {} in {} ms", origin.source_root.display(), origin.copy_ms);
    }
    let conn = connection::open(root).context("failed to open the project's SQLite index")?;
    // Read before `ensure_current`, whose reset would hide that one existed:
    // the session instructions say whether a cold start throws an index away.
    let had_generation = schema::stored_generation(&conn).is_some();
    // The generation names every discovered plugin's build and core's
    // pipeline, so an index built by a since-rebuilt plugin is thrown away.
    let reinitialized = schema::ensure_current(&conn, &registry::indexer_version(&discovered))
        .context("failed to check the index's schema and indexer versions")?;
    if reinitialized {
        crate::log_role!("index (re)initialized - a full reindex is needed");
    }
    // Next to the staging cleanup above, once the tables exist: pending rows
    // no pass will clear (a removed plugin, a clear that failed) go.
    workspace_reindex::remove_stale_semantic_pending(&conn, &discovered.manifests);
    // Recorded at open, not once serving: a long cold walk must not read as
    // idleness to a concurrent GC scan.
    last_used::touch(&conn).context("failed to record that the project was used")?;
    // A recorded fact, not schema freshness: a walk killed half way leaves a
    // current schema behind a partial graph.
    let needs_bulk_index = !schema::bulk_index_completed(&conn)
        .context("failed to check whether the project has been indexed")?;
    // The rest is read only on a walked index, so an unwalked one's start
    // cannot fail on it.
    let walk = if needs_bulk_index {
        WalkFacts::Unwalked
    } else {
        // A walked project can still owe its semantic pass, if an earlier attempt
        // was interrupted after `bulkIndexedAt` was recorded (`daemon::semantic`).
        let semantic_pass_completed = schema::semantic_pass_completed(&conn)
            .context("failed to check whether the project's semantic pass has completed")?;
        // The languages the last full walk failed that a start still retries
        // (ADR 0021): only on a walked project, whose walk would otherwise never
        // revisit them. Unreadable, nothing is retried this start.
        let discovered_languages: Vec<&str> = discovered.manifests.keys().map(String::as_str).collect();
        let retry_languages =
            schema::languages_owed_a_retry(&conn, &discovered_languages).unwrap_or_else(|err| {
                crate::log_role!("could not read which failed languages to retry - none is: {err:#}");
                Vec::new()
            });
        // A seed copied before this open (or by a start that died before its
        // reconcile finished) is served only after that reconcile.
        let seeded = schema::seed_pending(&conn).unwrap_or_else(|err| {
            crate::log_role!("could not read whether the index is an unreconciled seed: {err:#}");
            false
        });
        if seeded {
            crate::log_role!(
                "the index is a seed - reconciling it with this worktree before tool calls answer"
            );
        }
        WalkFacts::Walked { semantic_pass_completed, retry_languages, seeded }
    };
    let conn = Arc::new(
        IndexStore::new(conn)
            .with_link_rules(crate::plugin_manifest::link_rules(discovered.manifests.values())),
    );
    Ok(OpenedIndex { conn, discovered, facts: IndexFacts { had_generation, reinitialized, walk } })
}

/// `root` canonicalized like `ProjectWatcher`'s paths, so the watcher loop's
/// `relative_wire_path` can strip it. Before [`publish`]: a failure must not
/// leave a bound socket.
pub(super) fn canonical_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize().with_context(|| format!("failed to canonicalize project root {}", root.display()))
}

/// What [`publish`] made visible: the bound listener, the published indexing
/// status, and the trigger a first tool call fires.
pub(super) struct Published {
    pub listener: ipc::Listener,
    pub indexing: IndexingStatus,
    pub activation_trigger: mpsc::Receiver<()>,
}

/// Binds the endpoint, writes the pid file, publishes the plan's indexing
/// phase and records that this daemon got as far as serving.
pub(super) fn publish(dir: &StateDir, plan: &StartupPlan) -> Result<Published> {
    let endpoint = crate::control::lock::endpoint(dir)?;
    // A socket file left by a crashed daemon makes bind() fail forever; the
    // singleton lock guarantees any socket here is a dead one's.
    endpoint.clear_stale();
    // Suspension lasts until the daemon restarts, and this is that restart:
    // every stale marker is cleared, like the socket.
    dir.clear_suspension_markers();
    // From here a shim's `connect()` succeeds (queued on the backlog until the
    // accept loop is up), which is what its bootstrap timeout waits for.
    let listener = ipc::Listener::bind(&endpoint)
        .with_context(|| format!("failed to bind the daemon endpoint at {endpoint}"))?;

    // Right after the bind, so "the pid file exists" means "something is
    // listening"; `meta.bulkIndexedAt`, not this, records a complete index.
    let pid_file = dir.pid_file();
    write_pid_file(&pid_file, std::process::id());

    // Unwalked: `Unindexed` until the first index-needing tool call; walked:
    // `Structural` (the embedding backfill is still owed); an unreconciled
    // seed: `Walking` until its reconcile opens the gate. The phase file is
    // published next to the pid file, before its absence could mean anything.
    let indexing = plan.initial.status();
    indexing.attach_phase_file(dir.phase_file());
    indexing.attach_progress_file(dir.progress_file());
    // Attached before the accept loop can hand `indexing` to any session, so
    // no tool call ever finds it with nothing to trigger.
    let activation_trigger = indexing.attach_activation();

    // After the bind: the record means the holder got as far as serving.
    dir.record_serving_owner(std::process::id());

    Ok(Published { listener, indexing, activation_trigger })
}

/// What [`start_services`] started, for the watcher, activation and the
/// supervisor.
pub(super) struct Services {
    pub conn: Arc<IndexStore>,
    pub registry: Arc<PluginRegistry>,
    pub embedding: Arc<EmbeddingPipeline>,
    pub core_activity: Arc<CoreActivity>,
    pub timeouts: IdleTimeouts,
    pub accept_loop: mpsc::Receiver<Result<()>>,
}

/// Reads the project's config, builds the embedding pipeline and the plugin
/// registry, and starts the accept loop on its own thread. Also returns the
/// bulk walk's copy of the discovered plugins, which activation takes.
pub(super) fn start_services(
    root: &Path,
    dir: &StateDir,
    opened: OpenedIndex,
    canonical_root: &Path,
    listener: ipc::Listener,
    indexing: &IndexingStatus,
) -> Result<(Services, DiscoveredPlugins)> {
    let OpenedIndex { conn, discovered, facts: _ } = opened;

    // Resolved once, from config.toml or its defaults (`daemon::lifecycle`).
    let project_config =
        crate::config::read_project_config(root).context("failed to read the project's config.toml")?;
    let timeouts = IdleTimeouts::from_config(&project_config);

    // Not loaded here, not even in the background (a startup thread broke the
    // restart budgets): the first `apply` pays for it on its own thread. A
    // missing model does not stop the daemon, which never fetches one.
    let embedding = Arc::new(EmbeddingPipeline::load(crate::config::embedding::embedding_settings(
        &project_config.embedding,
    )));
    let _ = fs::remove_file(dir.rerank_outcome_file());
    embedding.reranker().record_outcome_to(dir.rerank_outcome_file());

    // The bulk walk spawns one-shot processes from the manifests, not the
    // registry's supervisors, so it takes its own copy before `discovered` moves.
    let discovered_for_bulk_index = discovered.clone();

    // Spawns nothing: each language's plugin starts lazily (`daemon::registry`).
    let registry = Arc::new(PluginRegistry::new(
        canonical_root,
        dir.path().to_path_buf(),
        discovered,
        timeouts.plugin,
        project_config.plugin.memory_limit_mb,
        Arc::clone(&embedding),
    ));
    // Before the tool listener and the watcher's consumer below: both route
    // files, and neither may reach a language the last walk failed (ADR 0021,
    // section 2).
    registry.seed_failed_languages(&conn);

    // Starts ticking now, so a daemon nobody connects to still exits.
    let core_activity = CoreActivity::new();

    // The accept loop and activation get threads of their own; this thread
    // supervises. The accept outcome comes over a channel, not `join`, so the
    // supervisor can wake on its own schedule.
    let (accept_result, accept_loop) = mpsc::channel();
    {
        let conn = Arc::clone(&conn);
        let registry = Arc::clone(&registry);
        let core_activity = Arc::clone(&core_activity);
        let indexing = indexing.clone();
        let embedding = Arc::clone(&embedding);
        thread::spawn(move || {
            let _ = accept_result.send(serve_forever(
                listener,
                conn,
                registry,
                core_activity,
                indexing,
                embedding,
            ));
        });
    }

    Ok((
        Services { conn, registry, embedding, core_activity, timeouts, accept_loop },
        discovered_for_bulk_index,
    ))
}

/// Starts the watcher (and its consumer) as `plan` says and spawns the parked
/// activation. Returns the indexing status `supervise` detaches at teardown.
pub(super) fn start_watcher_and_activation(
    root: &Path,
    plan: StartupPlan,
    services: &Services,
    discovered_for_bulk_index: DiscoveredPlugins,
    indexing: IndexingStatus,
    activation_trigger: mpsc::Receiver<()>,
    canonical_root: &Path,
) -> Result<IndexingStatus> {
    let StartupPlan { initial: _, needs_walk, needs_semantic_pass_retry, retry_languages, seeded, watcher } =
        plan;

    // The watcher (D8 in `docs/architecture/lazy-indexing.md`): an unindexed
    // project gets it from activation, right before its walk. A walked one gets
    // it now, and its consumer too unless a semantic-pass retry is owed, which
    // must run before any incremental pass, or a failed language's retry is,
    // so that no edit of it routes while it is still failed (activation
    // starts the consumer after those).
    // A watcher failure here is fatal: this is startup, with no session to lose.
    // A seeded start's gate goes with the watcher to whoever starts its
    // consumer; the activation waits on `seed_reconciled` before its backfill.
    let (mut seed_gate, seed_reconciled) = if seeded {
        let (gate, reconciled) = seed::SeedGate::new(indexing.clone());
        (Some(gate), Some(reconciled))
    } else {
        (None, None)
    };
    let pending_watcher = match watcher {
        WatcherPlan::StartedByActivation => None,
        WatcherPlan::ParkedForActivation => Some(
            ProjectWatcher::with_canary(root, services.registry.state_dir().join(WATCH_CANARY_DIR))
                .context("failed to start the file watcher")?,
        ),
        WatcherPlan::ConsumerNow => {
            let watcher =
                ProjectWatcher::with_canary(root, services.registry.state_dir().join(WATCH_CANARY_DIR))
                    .context("failed to start the file watcher")?;
            let start = match seed_gate.take() {
                Some(gate) => ConsumerStart::Seeded(gate),
                None => ConsumerStart::Reconcile,
            };
            spawn_watch_consumer(
                watcher,
                Arc::clone(&services.conn),
                Arc::clone(&services.registry),
                canonical_root.to_path_buf(),
                start,
            );
            None
        }
    };

    // Kept for `supervise`, which detaches the state files before releasing
    // them (GM-605); the original moves into the activation thread.
    let indexing_for_teardown = indexing.clone();
    // Parked until the first tool call - see `daemon::activation`.
    activation::spawn(
        activation::ActivationCtx {
            conn: Arc::clone(&services.conn),
            registry: Arc::clone(&services.registry),
            embedding: Arc::clone(&services.embedding),
            discovered_for_bulk_index,
            canonical_root: canonical_root.to_path_buf(),
            root: root.to_path_buf(),
            indexing,
            core_activity: Arc::clone(&services.core_activity),
            needs_walk,
            needs_semantic_pass_retry,
            retry_languages,
            watcher: pending_watcher,
            seed_gate,
            seed_reconciled,
        },
        activation_trigger,
    )?;
    Ok(indexing_for_teardown)
}

/// Supervises the daemon until it exits, then releases the singleton lock and
/// purges the state directory.
pub(super) fn supervise_and_tear_down(
    canonical_root: &Path,
    dir: &StateDir,
    singleton: File,
    services: Services,
    indexing_for_teardown: &IndexingStatus,
) -> Result<()> {
    // `canonical_root`: the orphan check must stat the same spelling
    // everything else resolved against.
    let outcome = lifecycle::supervise(
        canonical_root,
        dir.path(),
        &services.registry,
        &services.core_activity,
        Some(indexing_for_teardown),
        services.timeouts,
        services.accept_loop,
    );

    // Released explicitly: the lock is now the only claim on this project, and
    // a slow teardown while holding it would wedge the project.
    drop(singleton);
    let exit = outcome?;
    // After the lock, so the purge judges the directory as `clean` would; the
    // index this process still has open is no obstacle (see the function).
    lifecycle::purge_state_dir(dir.path(), &exit);
    Ok(())
}

#[cfg(test)]
mod tests;
