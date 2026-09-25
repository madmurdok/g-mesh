//! The per-project daemon: its state files, its singleton lock and [`run`].
//! Decisions: `docs/adr/0004-daemon-lifecycle.md`.
mod activation;
pub mod build_stamp;
pub mod bulk_index;
pub mod candidates;
pub mod front;
pub mod identity;
pub mod indexing_status;
pub mod lifecycle;
pub mod manifest;
pub mod memory;
pub mod plugin;
pub mod registry;
pub mod semantic;
/// A fake, protocol-speaking plugin for unit tests that need two languages.
#[cfg(test)]
pub(crate) mod test_plugin;
pub mod workspace_reindex;

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::lifecycle::{CoreActivity, IdleTimeouts};
use crate::daemon::registry::PluginRegistry;
use crate::gc::last_used;
use crate::ipc;
use crate::mcp;
use crate::storage::connection::{self, ensure_project_dir, project_dir};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::watcher::debounce::Debouncer;
use crate::watcher::ProjectWatcher;

/// Unix only: on Windows the endpoint is a pipe name (see `ipc::windows`).
#[cfg(unix)]
const SOCKET_FILE: &str = "daemon.sock";
const PID_FILE: &str = "daemon.pid";
/// The build the live daemon started from (see `daemon::build_stamp`).
const BUILD_STAMP_FILE: &str = "daemon.build";
/// Held by a shim while it bootstraps a daemon (`shim::connect_or_bootstrap`).
const BOOTSTRAP_LOCK_FILE: &str = "bootstrap.lock";
/// Held by a running daemon for its whole lifetime: its holder owns the
/// project's socket. Not the bootstrap lock: the shim holds that one while
/// spawning the daemon, so sharing it would deadlock.
const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Where the lock's holder records that it has begun serving. Beside the lock,
/// not inside it: on Windows a second handle cannot read a locked file. Only
/// the holder writes it: after taking the lock, and the next taker clears it.
const DAEMON_SERVING_FILE: &str = "daemon.serving";

/// The cold-start phase, one lowercased [`indexing_status::Phase`] word (D13 in
/// `docs/architecture/lazy-indexing.md`). Written on every transition, removed
/// on the way out, so outside readers tell "never indexed" from "building".
const PHASE_FILE: &str = "index.phase";

/// The progress counters as JSON, written atomically and throttled by
/// [`indexing_status::IndexingStatus`]. A killed daemon leaves it behind, so a
/// reader must check its `pid` against the live daemon before trusting it.
const PROGRESS_FILE: &str = "index.progress";

/// The watcher's debounce window and its loop's poll bound: a path is reparsed
/// once quiet this long, and a settled burst is drained only when the loop
/// wakes, so this must stay short. Unrelated to the idle timeouts.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);

/// Where a project's daemon listens; the shim derives the same endpoint from
/// its cwd. A socket file on Unix, a pipe name on Windows (`ipc::windows`).
pub fn endpoint(root: &Path) -> Result<ipc::Endpoint> {
    let dir = project_dir(root)?;
    endpoint_in(&dir).with_context(|| format!("failed to derive the daemon endpoint from {}", dir.display()))
}

/// [`endpoint`] from an already-known state directory, which is named after
/// the project hash. `None` only for a path with no final component.
pub fn endpoint_in(state_dir: &Path) -> Option<ipc::Endpoint> {
    #[cfg(unix)]
    {
        Some(ipc::Endpoint::at_path(state_dir.join(SOCKET_FILE)))
    }
    #[cfg(windows)]
    {
        Some(ipc::Endpoint::named(state_dir.file_name()?.to_str()?))
    }
}

/// The AF_UNIX socket file a project's daemon listens on. Callers that only
/// need to reach the daemon use [`endpoint`].
#[cfg(unix)]
pub fn socket_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(SOCKET_FILE))
}

/// Records the live daemon's pid next to its socket, so tooling can tell a
/// stale socket file from a running daemon.
pub fn pid_path(root: &Path) -> Result<PathBuf> {
    Ok(pid_path_in(&project_dir(root)?))
}

/// The bundled JS/TS plugin's pid file. Multi-language tooling lists every
/// `plugin-*.pid` instead (`daemon::registry::discovered_pid_files`).
pub fn plugin_pid_path(root: &Path) -> Result<PathBuf> {
    Ok(plugin_pid_path_in(&project_dir(root)?))
}

/// The same paths from an already-known state directory, for callers whose
/// project root may no longer exist (`cli::clean`).
pub fn pid_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(PID_FILE)
}

pub fn plugin_pid_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(registry::plugin_pid_file_name(plugin::BUNDLED_LANGUAGE))
}

/// Where [`PHASE_FILE`] lives for a given state directory.
pub fn phase_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(PHASE_FILE)
}

/// Where [`PROGRESS_FILE`] lives for a given state directory.
pub fn progress_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(PROGRESS_FILE)
}

/// The progress snapshot a daemon last published for this state directory,
/// whoever wrote it: `None` for a missing or unparseable file. The snapshot's
/// `pid` says which daemon wrote it; this does not check that it is alive.
pub fn read_progress_in(state_dir: &Path) -> Option<indexing_status::ProgressSnapshot> {
    let contents = fs::read_to_string(progress_path_in(state_dir)).ok()?;
    serde_json::from_str(&contents).ok()
}

/// The phase word a running daemon published here; `None` when no daemon is
/// running (it removes the file on exit) or none ever ran.
pub fn read_phase_in(state_dir: &Path) -> Option<String> {
    fs::read_to_string(phase_path_in(state_dir)).ok().map(|contents| contents.trim().to_string())
}

/// Where the live daemon records the build it started from.
pub fn build_stamp_path(root: &Path) -> Result<PathBuf> {
    Ok(build_stamp_path_in(&project_dir(root)?))
}

pub fn build_stamp_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(BUILD_STAMP_FILE)
}

/// The file shims serialize their bootstrap on, derived exactly like the
/// socket and pid paths so every process agrees on it without configuration.
pub fn lock_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(BOOTSTRAP_LOCK_FILE))
}

/// The singleton lock a running daemon holds for its whole lifetime.
pub fn daemon_lock_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(DAEMON_LOCK_FILE))
}

pub fn daemon_lock_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(DAEMON_LOCK_FILE)
}

fn serving_owner_path_in(state_dir: &Path) -> PathBuf {
    state_dir.join(DAEMON_SERVING_FILE)
}

/// Reads a pid from one of the files above; `None` for "nothing recorded". See
/// [`read_pid_file_result`] where "could not tell" must differ from that.
pub fn read_pid_file(path: &Path) -> Option<u32> {
    read_pid_file_result(path).ok().flatten()
}

/// Like [`read_pid_file`], but an unreadable file is an error: a caller deciding
/// a deletion must not read "could not tell" as "safe". A present but
/// unparseable file stays `Ok(None)`, which callers depend on.
pub fn read_pid_file_result(path: &Path) -> std::io::Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse().ok()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Records a pid atomically (a per-process temporary renamed into place), so a
/// reader never sees a truncated file, which reads as "no daemon". Best-effort:
/// every reader handles "nothing recorded".
pub fn write_pid_file(path: &Path, pid: u32) {
    write_state_file_atomic(path, &format!("{pid}\n"), "pid file");
}

/// The temp-then-rename write behind [`write_pid_file`], also used for the phase file.
pub(crate) fn write_state_file_atomic(path: &Path, contents: &str, what: &str) {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if let Err(err) = fs::write(&temporary, contents) {
        eprintln!("g-mesh daemon: failed to write {what} {}: {err}", temporary.display());
        return;
    }
    if let Err(err) = fs::rename(&temporary, path) {
        eprintln!("g-mesh daemon: failed to put {what} {} in place: {err}", path.display());
        let _ = fs::remove_file(&temporary);
    }
}

/// Whether a process with this pid exists: a snapshot, and pids are reused, so
/// callers that care corroborate it with the endpoint.
pub fn is_process_alive(pid: u32) -> bool {
    crate::process::is_alive(pid)
}

/// Whether something accepts connections on this project's endpoint now; not
/// fooled by a recycled pid. The probe connection is dropped at once.
pub fn is_listening(root: &Path) -> Result<bool> {
    Ok(ipc::Stream::connect(&endpoint(root)?).is_ok())
}

/// Per-project daemon core: opens the index, binds the endpoint, and serves an
/// MCP session per connection until stopped or idle. The endpoint is bound
/// before the index exists and before the plugin (the shim's bootstrap budget
/// races the socket appearing); a tool call that needs the graph waits for it
/// (`daemon::indexing_status`), so no one is served off a partial graph.
pub fn run(root: &Path) -> Result<()> {
    // `ensure_project_dir`: a state directory without its `project.root` file
    // is invisible to `clean orphaned` forever.
    let dir = ensure_project_dir(root)?;

    // Singleton guard, before anything else touches the project's files.
    // Losing it is the healthy outcome: the caller connects to the incumbent.
    let singleton = match acquire_singleton_lock(&dir)? {
        Some(lock) => lock,
        None => return stand_down(root),
    };

    // After the singleton lock, before the bind (never transiently missing).
    // Not fatal: a daemon that reads as outdated gets replaced.
    match build_stamp::of_running_process() {
        Ok(stamp) => {
            if let Err(err) = build_stamp::write(&build_stamp_path_in(&dir), &stamp) {
                eprintln!("g-mesh daemon: could not publish its build stamp: {err:#}");
            }
        }
        Err(err) => eprintln!("g-mesh daemon: could not describe its own build: {err:#}"),
    }

    // D10/D11: a folder of projects is served by the front, which needs no
    // plugin discovery and never creates an `index.db`.
    let detection = candidates::detect_in(root, &dir, candidates::Limits::default());
    if detection.mode == candidates::Mode::Multi {
        return front::run(root, &dir, singleton, detection);
    }

    // Discovery comes before the index (the generation check below needs it)
    // and before the bind, so a malformed manifest or two plugins claiming one
    // extension fail startup before a socket or pid file is published.
    let discovered =
        manifest::discover(&manifest::default_roots()).context("failed to discover language plugins")?;

    let conn = connection::open(root).context("failed to open the project's SQLite index")?;
    // The generation names every discovered plugin's build and core's
    // pipeline, so an index built by a since-rebuilt plugin is thrown away.
    if schema::ensure_current(&conn, &registry::indexer_version(&discovered))
        .context("failed to check the index's schema and indexer versions")?
    {
        eprintln!("g-mesh daemon: index (re)initialized - a full reindex is needed");
    }
    // Recorded at open, not once serving: a long cold walk must not read as
    // idleness to a concurrent GC scan.
    last_used::touch(&conn).context("failed to record that the project was used")?;
    // A recorded fact, not schema freshness: a walk killed half way leaves a
    // current schema behind a partial graph.
    let needs_bulk_index = !schema::bulk_index_completed(&conn)
        .context("failed to check whether the project has been indexed")?;
    // A walked project can still owe its semantic pass, if an earlier attempt
    // was interrupted after `bulkIndexedAt` was recorded (`daemon::semantic`).
    let needs_semantic_pass_retry = !needs_bulk_index
        && !schema::semantic_pass_completed(&conn)
            .context("failed to check whether the project's semantic pass has completed")?;
    let conn = Arc::new(IndexStore::new(conn));

    // Canonicalized like `ProjectWatcher`'s paths, so `relative_wire_path` can strip it.
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;

    let endpoint = endpoint_in(&dir)
        .with_context(|| format!("failed to derive the daemon endpoint from {}", dir.display()))?;
    // A socket file left by a crashed daemon makes bind() fail forever; the
    // singleton lock guarantees any socket here is a dead one's.
    endpoint.clear_stale();
    // Suspension lasts until the daemon restarts, and this is that restart:
    // every stale marker is cleared, like the socket.
    registry::clear_stale_suspension_markers(&dir);
    // From here a shim's `connect()` succeeds (queued on the backlog until the
    // accept loop is up), which is what its bootstrap timeout waits for.
    let listener = ipc::Listener::bind(&endpoint)
        .with_context(|| format!("failed to bind the daemon endpoint at {endpoint}"))?;

    // Right after the bind, so "the pid file exists" means "something is
    // listening"; `meta.bulkIndexedAt`, not this, records a complete index.
    let pid_file = dir.join(PID_FILE);
    write_pid_file(&pid_file, std::process::id());

    // Unwalked: `Unindexed` until the first index-needing tool call; walked:
    // `Structural` (the embedding backfill is still owed). The phase file is
    // published next to the pid file, before its absence could mean anything.
    let indexing = if needs_bulk_index { IndexingStatus::unindexed() } else { IndexingStatus::structural() };
    indexing.attach_phase_file(phase_path_in(&dir));
    indexing.attach_progress_file(progress_path_in(&dir));
    // Attached before the accept loop can hand `indexing` to any session, so
    // no tool call ever finds it with nothing to trigger.
    let activation_trigger = indexing.attach_activation();

    // After the bind: the record means the holder got as far as serving.
    record_serving_owner(&dir);

    // Resolved once, from config.toml or its defaults (`daemon::lifecycle`).
    let project_config =
        crate::config::read_project_config(root).context("failed to read the project's config.toml")?;
    let timeouts = IdleTimeouts::from_config(&project_config);

    // Not loaded here, not even in the background (a startup thread broke the
    // restart budgets): the first `apply` pays for it on its own thread. A
    // missing model does not stop the daemon, which never fetches one.
    let embedding = Arc::new(crate::embedding::EmbeddingPipeline::load(&project_config.embedding));

    // The bulk walk spawns one-shot processes from the manifests, not the
    // registry's supervisors, so it takes its own copy before `discovered` moves.
    let discovered_for_bulk_index = discovered.clone();

    // Spawns nothing: each language's plugin starts lazily (`daemon::registry`).
    let registry = Arc::new(PluginRegistry::new(
        &canonical_root,
        dir.clone(),
        discovered,
        timeouts.plugin,
        project_config.plugin.memory_limit_mb,
        Arc::clone(&embedding),
    ));

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

    // The watcher (D8 in `docs/architecture/lazy-indexing.md`): an unindexed
    // project gets it from activation, right before its walk. A walked one gets
    // it now, and its consumer too unless a semantic-pass retry is owed, which
    // must run before any incremental pass (activation starts it after that).
    // A watcher failure here is fatal: this is startup, with no session to lose.
    let pending_watcher = if needs_bulk_index {
        None
    } else {
        let watcher = ProjectWatcher::new(root).context("failed to start the file watcher")?;
        if needs_semantic_pass_retry {
            Some(watcher)
        } else {
            spawn_watch_consumer(watcher, Arc::clone(&conn), Arc::clone(&registry), canonical_root.clone());
            None
        }
    };

    // Parked until the first tool call - see `daemon::activation`.
    activation::spawn(
        activation::ActivationCtx {
            conn: Arc::clone(&conn),
            registry: Arc::clone(&registry),
            embedding: Arc::clone(&embedding),
            discovered_for_bulk_index,
            canonical_root: canonical_root.clone(),
            root: root.to_path_buf(),
            indexing,
            core_activity: Arc::clone(&core_activity),
            needs_walk: needs_bulk_index,
            needs_semantic_pass_retry,
            watcher: pending_watcher,
        },
        activation_trigger,
    )?;

    // `canonical_root`: the orphan check must stat the same spelling
    // everything else resolved against.
    let outcome =
        lifecycle::supervise(&canonical_root, &dir, &registry, &core_activity, timeouts, accept_loop);

    // Released explicitly: the lock is now the only claim on this project, and
    // a slow teardown while holding it would wedge the project.
    drop(singleton);
    outcome
}

/// What a daemon that lost the singleton race does: quiet success against a
/// healthy incumbent; against a holder that is not serving, a non-zero exit
/// naming its pid (for whoever ran `g-mesh daemon` by hand).
fn stand_down(root: &Path) -> Result<()> {
    match inspect_daemon_lock(root)? {
        DaemonLock::Wedged { pid } => anyhow::bail!(
            "pid {pid} holds the daemon lock for {} but is not serving it - nothing can connect \
             to it and no other daemon can take over while it lives; run `g-mesh stop` in that \
             project to clear it",
            root.display()
        ),
        // `Free`: the incumbent released the lock after the failed attempt; the
        // next bootstrap wins.
        DaemonLock::Free | DaemonLock::Serving | DaemonLock::Starting => {
            eprintln!("g-mesh daemon: another daemon already serves {} - exiting", root.display());
            Ok(())
        }
    }
}

/// Starts the watcher's consumer thread, once no structural walk or
/// whole-project semantic pass can still race it.
fn spawn_watch_consumer(
    watcher: ProjectWatcher,
    conn: Arc<IndexStore>,
    registry: Arc<PluginRegistry>,
    root: PathBuf,
) {
    thread::spawn(move || {
        let mut debouncer = Debouncer::new(DEBOUNCE_WINDOW);
        loop {
            watch_and_route_once(&watcher, &mut debouncer, &root, &conn, &registry);
        }
    });
}

/// One iteration of the watcher loop: waits up to [`DEBOUNCE_WINDOW`] for a
/// raw change, records it, then routes every settled path to `registry`.
fn watch_and_route_once(
    watcher: &ProjectWatcher,
    debouncer: &mut Debouncer,
    root: &Path,
    conn: &IndexStore,
    registry: &PluginRegistry,
) {
    if let Some(path) = watcher.next_change(DEBOUNCE_WINDOW) {
        debouncer.record(path);
    }
    for settled in debouncer.drain_ready() {
        let Some(file_path) = relative_wire_path(root, &settled) else {
            // Outside the project root: nothing to route.
            continue;
        };
        if file_path.is_empty() {
            // The project root itself (macOS reports it for a write inside it): not
            // a file, and must not enter a sleeping plugin's replay queue.
            continue;
        }
        // A workspace file (`[plugin.workspace] watch_files`) triggers that
        // language's reindex; anything else is applied or queued by its supervisor.
        registry.route_settled_path(conn, file_path);
    }
}

/// Runs the MCP accept loop until the process is killed: the daemon's only
/// async part, on this thread. The listener is bound synchronously by [`run`]
/// before tokio gets it, keeping the bind/pid-file ordering. Every connection
/// holds a [`CoreActivity`] guard, so only an unattended project is ever idle.
fn serve_forever(
    listener: ipc::Listener,
    conn: Arc<IndexStore>,
    registry: Arc<PluginRegistry>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
    embedding: Arc<crate::embedding::EmbeddingPipeline>,
) -> Result<()> {
    // Two workers: connections are few and serialize on the SQLite lock.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to build the daemon's async runtime")?;

    runtime.block_on(async move {
        let mut listener =
            listener.into_async().context("failed to register the daemon endpoint with the async runtime")?;

        loop {
            let stream = listener.accept().await.context("failed to accept a daemon connection")?;
            let conn = Arc::clone(&conn);
            let registry = Arc::clone(&registry);
            let indexing = indexing.clone();
            let embedding = Arc::clone(&embedding);
            // Taken before the task spawns, so the count rises before the core can
            // be judged unattended.
            let attached = core_activity.connection_opened();
            let core_activity = Arc::clone(&core_activity);
            tokio::spawn(async move {
                // Dropped when this session ends, whichever way it ends, which
                // is also what restarts the core's idle clock.
                let _attached = attached;
                if let Err(err) =
                    mcp::serve_connection(stream, conn, registry, core_activity, indexing, embedding).await
                {
                    eprintln!("g-mesh daemon: connection ended: {err:#}");
                }
            });
        }
    })
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

/// How long [`acquire_singleton_lock`] retries a contended lock: well under
/// the shim's shortest bootstrap budget (1s in tests), so it only covers a
/// release already in flight, never a real incumbent.
const SINGLETON_LOCK_RETRY_BUDGET: Duration = Duration::from_millis(300);

/// Short next to the budget, so a just-killed predecessor's release is caught quickly.
const SINGLETON_LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Takes the project's daemon lock, or reports that someone else holds it. The
/// returned `File` must live as long as the daemon: the lock is tied to the
/// open file, so exit or death releases it. A contended lock is retried because
/// the kernel releases a `kill -9`'d holder's `flock` slightly after it dies.
fn acquire_singleton_lock(dir: &Path) -> Result<Option<File>> {
    let path = dir.join(DAEMON_LOCK_FILE);
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open daemon lock file {}", path.display()))?;

    let deadline = std::time::Instant::now() + SINGLETON_LOCK_RETRY_BUDGET;
    loop {
        match file.try_lock() {
            Ok(()) => {
                // Cleared as soon as the lock is taken, so the state reads "not
                // serving yet" for exactly as long as that is true.
                clear_serving_owner(dir);
                return Ok(Some(file));
            }
            Err(TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Ok(None);
                }
                thread::sleep(SINGLETON_LOCK_RETRY_INTERVAL);
            }
            Err(TryLockError::Error(err)) => {
                return Err(err).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    }
}

/// What holds a project's daemon lock right now, judged from outside the
/// holder. The lock alone decides who may serve a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonLock {
    /// Nobody holds it: the project has no daemon and a bootstrap may go ahead.
    Free,
    /// Held by a daemon answering on the project's socket: the only state in
    /// which another daemon must stand down.
    Serving,
    /// Held, but the holder has not published itself as serving yet. Entitled to
    /// a moment: nothing may evict it on this alone.
    Starting,
    /// Held by a live process that published itself as serving and no longer
    /// answers: no client can reach it, and it keeps the project until it exits.
    Wedged { pid: u32 },
}

/// Diagnoses [`DaemonLock`] for `root`: the socket first (an answer means
/// healthy), then the lock, probed by taking and dropping it. A daemon racing
/// for the lock does not notice the probe only because it retries for
/// `SINGLETON_LOCK_RETRY_BUDGET`, orders of magnitude longer than the probe
/// holds it.
pub fn inspect_daemon_lock(root: &Path) -> Result<DaemonLock> {
    let listening = is_listening(root)?;
    inspect_daemon_lock_in(&project_dir(root)?, listening)
}

/// [`inspect_daemon_lock`] with the state directory and "is anything
/// listening" given, so the judgement is testable without a real socket.
fn inspect_daemon_lock_in(state_dir: &Path, listening: bool) -> Result<DaemonLock> {
    if listening {
        return Ok(DaemonLock::Serving);
    }
    if !daemon_lock_is_held(state_dir)? {
        return Ok(DaemonLock::Free);
    }
    match serving_owner_in(state_dir).filter(|&pid| is_process_alive(pid)) {
        Some(pid) => Ok(DaemonLock::Wedged { pid }),
        None => Ok(DaemonLock::Starting),
    }
}

/// Whether anything holds the project's daemon lock. A lock file that cannot be
/// opened reads as "not held": eviction must fail towards leaving processes alone.
fn daemon_lock_is_held(state_dir: &Path) -> Result<bool> {
    let path = daemon_lock_path_in(state_dir);
    let Ok(file) = File::options().write(true).truncate(false).open(&path) else {
        return Ok(false);
    };
    match file.try_lock() {
        // Held by nobody: taken here for an instant and released by this drop.
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("failed to probe {}", path.display()))
        }
    }
}

/// Records that the lock's holder is now serving: its pid, beside the lock.
/// Called after the socket is bound, never before: a holder with no pid
/// recorded is starting up and must be left alone; one whose recorded pid is
/// alive while nothing answers is wedged and can be evicted. The record is
/// newline-terminated and [`serving_owner_in`] refuses one without it: a
/// reader may signal the pid it reads, so a partial record must read as
/// nothing. Best-effort: a failure reads as `Starting`, which is left alone.
fn record_serving_owner(state_dir: &Path) {
    write_pid_file(&serving_owner_path_in(state_dir), std::process::id());
}

/// Removes the record, so a fresh holder does not inherit its predecessor's
/// claim. Called under the lock, which keeps "only the holder writes this" true.
fn clear_serving_owner(state_dir: &Path) {
    let path = serving_owner_path_in(state_dir);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("g-mesh daemon: failed to clear {}: {err}", path.display()),
    }
}

/// The pid the lock's holder recorded once serving. `None` for a missing,
/// empty, unparseable or newline-less record: all mean "nothing recorded".
pub fn serving_owner_in(state_dir: &Path) -> Option<u32> {
    let recorded = fs::read_to_string(serving_owner_path_in(state_dir)).ok()?;
    recorded.strip_suffix('\n')?.trim().parse().ok()
}

#[cfg(test)]
mod tests;
