//! The watcher's consumer loop (GM-707 R-25): it routes settled file changes
//! to the [`PluginRegistry`], which is why it lives in `daemon`, not `watcher`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::daemon::registry::{self, PluginRegistry};
use crate::daemon::seed;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::watcher::batch::{classify_settled, order_for_routing};
use crate::watcher::debounce::Debouncer;
use crate::watcher::ignore_layers::GITIGNORE_FILE_NAME;
use crate::watcher::rescan::RescanScheduler;
use crate::watcher::{Change, ProjectWatcher};

/// The watcher's debounce window and its loop's poll bound: a path is reparsed
/// once quiet this long, and a settled burst is drained only when the loop
/// wakes, so this must stay short. Unrelated to the idle timeouts.
pub(crate) const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// How long the watcher must have delivered no event before a by-name miss
/// or a `semanticNeighbours` page says so. A healthy watcher delivers an edit
/// within about a second, so an agent querying right after its own edit sees
/// no hint unless the stream has stalled.
pub(crate) const WATCHER_QUIET_HINT_AFTER: Duration = Duration::from_secs(5);

/// When the watcher's consumer last took an event (a non-ignored path or a
/// Rescan) off the watcher, or started listening. One per daemon, written by
/// the consumer thread, read by every MCP call; an atomic, no lock, no I/O.
pub(crate) struct WatcherClock {
    base: Instant,
    /// Milliseconds from `base` to the last event, plus one; `0` while no
    /// consumer has started.
    last: AtomicU64,
}

impl WatcherClock {
    pub(crate) fn new() -> Self {
        Self { base: Instant::now(), last: AtomicU64::new(0) }
    }

    /// Records an event (or the consumer's start) now.
    pub(crate) fn heard(&self) {
        let since_base = self.base.elapsed().as_millis();
        let stamp = u64::try_from(since_base).unwrap_or(u64::MAX - 1) + 1;
        self.last.store(stamp, Ordering::Relaxed);
    }

    /// How long before `now` the last event came; `None` while no consumer
    /// has started. A test ages the last event by passing a later `now`.
    pub(crate) fn quiet_for_at(&self, now: Instant) -> Option<Duration> {
        let stamp = self.last.load(Ordering::Relaxed);
        let last = self.base + Duration::from_millis(stamp.checked_sub(1)?);
        Some(now.saturating_duration_since(last))
    }

    /// [`quiet_for_at`](Self::quiet_for_at), kept only when it reaches
    /// [`WATCHER_QUIET_HINT_AFTER`]: the quiet a by-name answer reports.
    pub(crate) fn quiet_worth_saying_at(&self, now: Instant) -> Option<Duration> {
        self.quiet_for_at(now).filter(|quiet| *quiet >= WATCHER_QUIET_HINT_AFTER)
    }

    /// [`quiet_worth_saying_at`](Self::quiet_worth_saying_at) now.
    pub(crate) fn quiet_worth_saying(&self) -> Option<Duration> {
        self.quiet_worth_saying_at(Instant::now())
    }
}

/// What the watcher's consumer does before its first batch.
pub(crate) enum ConsumerStart {
    /// The index was walked in this process: only the `.gitignore` re-check.
    AfterWalk,
    /// The index may predate the files on disk (no walk in this process):
    /// the `.gitignore` re-check, then the plain reconcile.
    Reconcile,
    /// The index is an unreconciled seed (`daemon::seed`): the seeded
    /// reconcile, no `.gitignore` re-check (every `.gitignore` of a fresh
    /// checkout is newer than the copied walk), then the gate opens, the
    /// watch files are compared with the seed's source, and the seed is
    /// stamped reconciled.
    Seeded(seed::SeedGate),
}

/// Starts the watcher's consumer thread, once no structural walk or
/// whole-project semantic pass can still race it; `start` says what it does
/// before its first batch, after which every start runs the scoped semantic
/// pass over the semantic-pending files still owed
/// ([`PluginRegistry::owed_semantic_file_passes`]).
pub(crate) fn spawn_watch_consumer(
    watcher: ProjectWatcher,
    conn: Arc<IndexStore>,
    registry: Arc<PluginRegistry>,
    root: PathBuf,
    start: ConsumerStart,
) {
    thread::spawn(move || {
        // Taken before the start's own `.gitignore` recheck and reconcile,
        // which count as the first run for a later Rescan's recheck.
        let mut rescans = Rescans::new();
        // On this thread, before the first batch, as an in-session edit would
        // be; events meanwhile queue in the watcher's channel.
        match start {
            ConsumerStart::AfterWalk => registry.recheck_gitignores_since_index(&conn),
            ConsumerStart::Reconcile => {
                // `.gitignore` edits made while no daemon watched, then every
                // other file changed meanwhile.
                registry.recheck_gitignores_since_index(&conn);
                registry.reconcile_since_index(
                    &conn,
                    registry::ReconcileMode::Plain,
                    registry::ReconcileScope::Start,
                );
            }
            ConsumerStart::Seeded(gate) => {
                seed::hold_the_seeded_reconcile_for_tests();
                // The reparsed files are recorded semantic-pending; the
                // start's scoped pass below reads them back.
                registry.reconcile_since_index(
                    &conn,
                    registry::ReconcileMode::Seeded,
                    registry::ReconcileScope::Start,
                );
                gate.open();
                // After the gate, as the watcher's reindex of a live edit
                // serves the old graph until its swap; before the stamp, so a
                // daemon killed meanwhile compares them again.
                registry.reconcile_watch_files_with_seed(&conn);
                // A daemon killed before this line repeats the seeded
                // reconcile, which finds the files fresh.
                if let Err(err) = conn.with(schema::record_seed_reconciled) {
                    crate::log_role!(
                        "could not record that the seeded index was reconciled - the next start \
                         reconciles it again: {err:#}"
                    );
                }
            }
        }
        // Any start: files a per-file pass still owes (a seeded reparse, or
        // one an earlier start's death left semantic-pending).
        registry.owed_semantic_file_passes(&conn);
        let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
        // The start's reconcile read the disk, so the quiet counts from here.
        registry.watcher_clock().heard();
        loop {
            watch_and_route_once(&watcher, &mut debouncer, &mut rescans, &root, &conn, &registry);
        }
    });
}

/// The watcher loop's dropped-event state (GM-616), beside its
/// [`Debouncer`]: the scheduler holding the subtrees to reconcile, and the
/// `.gitignore` watermark.
pub(crate) struct Rescans {
    pub(crate) scheduler: RescanScheduler,
    /// Unix seconds when the previous reconcile started (at first, the
    /// consumer's start): a `.gitignore` with a newer mtime goes through
    /// [`PluginRegistry::gitignore_changed`] at the next Rescan run.
    pub(crate) gitignores_since: i64,
}

impl Rescans {
    /// The daemon's timings, the watermark at now.
    pub(crate) fn new() -> Self {
        Self::with_scheduler(RescanScheduler::new())
    }

    /// `scheduler`, the watermark at now.
    pub(crate) fn with_scheduler(scheduler: RescanScheduler) -> Self {
        Self { scheduler, gitignores_since: gitignore_watermark() }
    }
}

/// The `.gitignore` watermark for a reconcile starting now: one second before
/// now. mtimes are compared in whole seconds and strictly, so a `.gitignore`
/// written in the same second the run starts, after the run has read it, is
/// still newer than the next run's watermark. The cost is rechecking a file
/// once more, which the gate in [`PluginRegistry::gitignore_changed`] absorbs.
fn gitignore_watermark() -> i64 {
    unix_now_secs() - 1
}

/// Whole seconds since the epoch, now (0 if the clock is before it).
fn unix_now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64)
}

/// A path as a timing line names it (GM-435): project-relative when it is
/// inside `root`, as given otherwise.
fn timing_path(root: &Path, path: &Path) -> String {
    relative_wire_path(root, path).unwrap_or_else(|| path.display().to_string())
}

/// One iteration of the watcher loop: waits up to [`DEBOUNCE_WINDOW`] for a
/// raw change, records it, then routes every settled path to `registry` in
/// [`order_for_routing`]'s order (deletions, creations, modifications; ADR 0023).
/// Between the deletions and the creations, the batch's created paths are
/// announced per language ([`PluginRegistry::announce_created`]), so a plugin
/// knows all of them before it extracts the first; each is still routed once.
///
/// A settled `.gitignore`, directory or link first reloads the watcher's
/// layers and link table; the batch is then filtered again under the current
/// ones, a file link's own path placed as the walk lists it; after
/// routing, the batch's `.gitignore` paths and the links whose row the reload
/// changed go through [`PluginRegistry::gitignore_changed`], which reindexes
/// each language whose indexed files they changed.
///
/// A Rescan (events the OS dropped, GM-616) skips the debouncer: it goes to
/// `rescans`' scheduler, and after the batch, when the scheduler says a run is
/// due, [`PluginRegistry::rescan_subtrees`] reconciles the pending subtrees
/// with the disk.
pub(crate) fn watch_and_route_once(
    watcher: &ProjectWatcher,
    debouncer: &mut Debouncer,
    rescans: &mut Rescans,
    root: &Path,
    conn: &IndexStore,
    registry: &PluginRegistry,
) {
    let change = watcher.next_change(DEBOUNCE_WINDOW);
    if change.is_some() {
        registry.watcher_clock().heard();
    }
    match change {
        Some(Change::Path(path)) => {
            crate::log_timing!("fs_event path={}", timing_path(root, &path));
            debouncer.record(path)
        }
        // `next_change` widens a Rescan outside the root to the root; `""`
        // is the root here as well.
        Some(Change::Rescan(path)) => {
            rescans.scheduler.record(relative_wire_path(root, &path).unwrap_or_default())
        }
        None => {}
    }
    let mut settled_paths = debouncer.drain_ready();
    if !settled_paths.is_empty() && crate::log::timing_enabled() {
        let names: Vec<String> = settled_paths.iter().map(|path| timing_path(root, path)).collect();
        crate::log_timing!("settled paths={} names={}", names.len(), names.join(","));
    }
    let mut links = Vec::new();
    if let Some(changed_links) = watcher.reload_ignores_if_changed(&settled_paths) {
        links = changed_links;
    }
    // Always, not only after a reload: it also places a file link's own path
    // (kept by `next_change`) where the walk lists it.
    watcher.retain_unignored(&mut settled_paths);
    let mut gitignores = Vec::new();
    let mut batch = Vec::new();
    for settled in settled_paths {
        let Some(file_path) = relative_wire_path(root, &settled) else {
            // Outside the project root: nothing to route.
            continue;
        };
        if file_path.is_empty() {
            // The project root itself (macOS reports it for a write inside it): not
            // a file, and must not enter a sleeping plugin's replay queue.
            continue;
        }
        if settled.file_name() == Some(std::ffi::OsStr::new(GITIGNORE_FILE_NAME)) {
            gitignores.push(file_path.clone());
        }
        batch.push((classify_settled(conn, &settled, &file_path), file_path));
    }
    let order = order_for_routing(batch);
    // A watch file (`[plugin.workspace] watch_files`, or a stored watch path)
    // triggers that language's reindex; anything else is applied or queued by
    // its supervisor.
    for file_path in order.deleted {
        registry.route_settled_path(conn, file_path);
    }
    registry.announce_created(conn, &order.created);
    for file_path in order.created.into_iter().chain(order.modified) {
        registry.route_settled_path(conn, file_path);
    }
    // After routing: a file deleted or created in this batch is already in
    // the graph's new state, so the gate compares against it.
    if !gitignores.is_empty() || !links.is_empty() {
        registry.gitignore_changed(conn, &gitignores, &links);
    }
    if let Some(subtrees) = rescans.scheduler.due() {
        let started = std::time::Instant::now();
        let gitignores_since = std::mem::replace(&mut rescans.gitignores_since, gitignore_watermark());
        registry.rescan_subtrees(conn, &subtrees, watcher, gitignores_since);
        rescans.scheduler.ran(started.elapsed());
    }
}

/// An absolute, canonicalized path as the project-relative, forward-slash
/// wire path (like the plugin's `toPosixPath`); `None` outside `root`.
fn relative_wire_path(root: &Path, absolute: &Path) -> Option<String> {
    let rel = absolute.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        parts.push(component.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}
