//! The per-project daemon: its state files, its singleton lock and [`run`].
//! Decisions: `docs/adr/0004-daemon-lifecycle.md`.
mod activation;
pub mod build_stamp;
pub mod bulk_index;
mod startup;
pub(crate) mod watch_loop;
pub use crate::project_detect as candidates;
pub mod client;
pub(crate) mod config_reindex;
pub mod front;
pub mod identity;
pub use crate::indexing_status;
pub mod lifecycle;
pub(crate) mod mcp_host;
pub use crate::plugin_manifest as manifest;
pub mod memory;
pub(crate) mod path_case;
pub mod plugin;
pub mod registry;
pub mod seed;
pub mod semantic;
pub mod workspace_reindex;

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::registry::PluginRegistry;
use crate::daemon::startup::StartupPlan;
use crate::indexing_status::IndexingStatus;
use crate::ipc;
use crate::mcp;
use crate::paths::state_dir::{OnUnreadable, StateDir};
use crate::storage::index_store::IndexStore;

pub use crate::control::lock::{inspect_daemon_lock, DaemonLock};
pub use crate::paths::state_dir::{read_pid_file, read_pid_file_result, write_pid_file};

/// Whether a process with this pid exists: a snapshot, and pids are reused, so
/// callers that care corroborate it with the endpoint.
pub fn is_process_alive(pid: u32) -> bool {
    crate::process::is_alive(pid)
}

/// A CLI rebuild of a project's index in progress, as its marker records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rebuild {
    pub pid: u32,
    pub started: SystemTime,
    /// The `g-mesh` subcommand running it: `reindex` or `init`.
    pub command: String,
}

/// The rebuild holding `root`'s index, if one is running. A marker counts only
/// while its pid is alive and the daemon lock is held: a rebuild that crashed
/// leaves a marker naming a dead (or reused) pid, and the kernel has already
/// released its lock, so the project reads as free.
pub fn rebuild_in_progress(root: &Path) -> Result<Option<Rebuild>> {
    rebuild_in_progress_in(&StateDir::of(root)?)
}

/// An unopenable lock file reads as free (no rebuild), so the shim may bootstrap.
fn rebuild_in_progress_in(state_dir: &StateDir) -> Result<Option<Rebuild>> {
    let Some(marker) = state_dir.rebuild_marker() else {
        return Ok(None);
    };
    if !crate::process::is_alive(marker.pid) || !state_dir.daemon_lock_held(OnUnreadable::Free)? {
        return Ok(None);
    }
    Ok(Some(Rebuild {
        pid: marker.pid,
        started: UNIX_EPOCH + Duration::from_secs(marker.started_unix_secs),
        command: marker.command,
    }))
}

/// Per-project daemon core: opens the index, binds the endpoint, and serves an
/// MCP session per connection until stopped or idle. The endpoint is bound
/// before the index exists and before the plugin (the shim's bootstrap budget
/// races the socket appearing); a tool call that needs the graph waits for it
/// (`daemon::indexing_status`), so no one is served off a partial graph.
pub fn run(root: &Path) -> Result<()> {
    // `StateDir::ensure`: a state directory without its `project.root` file
    // is invisible to `clean orphaned` forever.
    let dir = StateDir::ensure(root)?;
    let Some(singleton) = startup::claim(&dir)? else {
        return stand_down(root);
    };

    // D10/D11: a folder of projects is served by the front, which needs no
    // plugin discovery and never creates an `index.db`.
    let detection = candidates::detect_in(root, dir.path(), candidates::Limits::default());
    if detection.mode == candidates::Mode::Multi {
        return front::run(root, &dir, singleton, detection);
    }

    let opened = startup::open_index(root, &dir)?;
    let plan = StartupPlan::decide(&opened.facts);
    let canonical_root = startup::canonical_root(root)?;
    let startup::Published { listener, indexing, activation_trigger } = startup::publish(&dir, &plan)?;
    let (services, discovered_for_bulk_index) =
        startup::start_services(root, &dir, opened, &canonical_root, listener, &indexing)?;
    let indexing_for_teardown = startup::start_watcher_and_activation(
        root,
        plan,
        &services,
        discovered_for_bulk_index,
        indexing,
        activation_trigger,
        &canonical_root,
    )?;
    startup::supervise_and_tear_down(&canonical_root, &dir, singleton, services, &indexing_for_teardown)
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
            crate::log_line!("g-mesh daemon: another daemon already serves {} - exiting", root.display());
            Ok(())
        }
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

        // One host for the daemon's lifetime, shared by every connection.
        let host: Arc<dyn mcp::McpHost> =
            Arc::new(mcp_host::DaemonHost::new(Arc::clone(&registry), Arc::clone(&core_activity)));
        loop {
            let stream = listener.accept().await.context("failed to accept a daemon connection")?;
            let conn = Arc::clone(&conn);
            let host = Arc::clone(&host);
            let indexing = indexing.clone();
            let embedding = Arc::clone(&embedding);
            // Taken before the task spawns, so the count rises before the core can
            // be judged unattended.
            let attached = core_activity.connection_opened();
            tokio::spawn(async move {
                // Dropped when this session ends, whichever way it ends, which
                // is also what restarts the core's idle clock.
                let _attached = attached;
                if let Err(err) = mcp::serve_connection(stream, conn, host, indexing, embedding).await {
                    crate::log_line!("g-mesh daemon: connection ended: {err:#}");
                }
            });
        }
    })
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
pub(crate) fn acquire_singleton_lock(dir: &StateDir) -> Result<Option<File>> {
    let path = dir.daemon_lock_file();
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
                dir.clear_serving_owner();
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

#[cfg(test)]
mod tests;
