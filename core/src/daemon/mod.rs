pub mod bulk_index;
pub mod identity;
pub mod plugin;

use std::fs::{self, File, TryLockError};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::daemon::plugin::PluginProcess;
use crate::gc::last_used;
use crate::mcp;
use crate::storage::connection::{self, project_dir};
use crate::storage::schema;
use crate::watcher::ProjectWatcher;

const SOCKET_FILE: &str = "daemon.sock";
const PID_FILE: &str = "daemon.pid";
/// Held by a shim while it decides whether to bootstrap a daemon and while
/// it waits for the one it spawned to come up (see `shim::connect_or_bootstrap`).
const BOOTSTRAP_LOCK_FILE: &str = "bootstrap.lock";
/// Held by a running daemon for its whole lifetime: whoever owns it owns the
/// project's socket. Deliberately a different file from the bootstrap lock -
/// the shim holds that one *while* spawning the daemon, so a daemon waiting
/// on it would deadlock against the shim waiting for the daemon.
const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// The AF_UNIX socket a project's daemon listens on. The shim derives the
/// same path from its own cwd, which is how the two find each other without
/// any configured port or discovery step.
pub fn socket_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(SOCKET_FILE))
}

/// Records the live daemon's pid next to its socket, so tooling can tell a
/// stale socket file from a running daemon.
pub fn pid_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(PID_FILE))
}

/// The file shims serialize their bootstrap on, derived exactly like the
/// socket and pid paths so every process agrees on it without configuration.
pub fn lock_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(BOOTSTRAP_LOCK_FILE))
}

/// Per-project daemon core: opens the SQLite index (checking schema
/// version), builds the initial index if the project has never been walked
/// (`bulk_index`), registers the file watcher, and serves an MCP session per
/// connection on the project's AF_UNIX socket. Simplified MVP lifecycle -
/// runs until killed; the two-tier idle-timeout model and `g-mesh stop` are
/// backlog.
///
/// Unix-only for now: AF_UNIX is available on Windows 10+ but is not wired
/// up here (see the architecture doc's shim/daemon section).
pub fn run(root: &Path) -> Result<()> {
    let dir = project_dir(root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;

    // Singleton guard, taken before anything else touches the project's
    // files: whoever holds it owns the socket. A second daemon for the same
    // project - spawned by a shim that raced ahead of this one's bind, or by
    // hand - exits here instead of going on to clear and rebind a socket its
    // predecessor is already serving. Losing this race is the expected,
    // healthy outcome, not an error: the caller connects to the incumbent.
    let _singleton = match acquire_singleton_lock(&dir)? {
        Some(lock) => lock,
        None => {
            eprintln!(
                "g-mesh daemon: another daemon already serves {} - exiting",
                root.display()
            );
            return Ok(());
        }
    };

    let conn = connection::open(root).context("failed to open the project's SQLite index")?;
    if schema::ensure_current(&conn).context("failed to check schema version")? {
        eprintln!("g-mesh daemon: schema (re)initialized - a full reindex is needed");
    }
    // Recorded here rather than once the daemon is serving: a start that gets
    // as far as opening the index is already this project being used, and a
    // cold start that then spends minutes in its bulk walk must not read as
    // minutes of idleness to a GC scan running alongside it.
    last_used::touch(&conn).context("failed to record that the project was used")?;
    // Asked before anything can answer it, and answered by a recorded fact
    // rather than by the schema being fresh: a walk killed half way through
    // leaves a current schema behind a partial graph, and that project is
    // still owed its index.
    let needs_bulk_index = !schema::bulk_index_completed(&conn)
        .context("failed to check whether the project has been indexed")?;
    let conn = Arc::new(Mutex::new(conn));

    // ProjectWatcher reports canonicalized absolute paths (see its own doc
    // comment on FSEvents' /var -> /private/var behavior); canonicalizing
    // here too is what lets `relative_wire_path` turn those back into the
    // project-relative paths the wire protocol and storage layer use.
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize project root {}", root.display()))?;

    // A protocol mismatch (or the plugin failing to start at all) is a hard
    // daemon-startup failure, matching handshake::verify's philosophy - there
    // is nothing useful this daemon can do without its plugin.
    let plugin = Arc::new(
        PluginProcess::spawn(&canonical_root).context("failed to start the JS/TS plugin")?,
    );

    // Cold start only, and before the watcher: a bulk walk racing incremental
    // updates could commit its own (older) parse of a file over one the
    // watcher had just refreshed. Nothing is accepted on the socket until
    // this returns, so a client's first query is never answered off a half-
    // built graph. A failure here is fatal for the same reason a failed
    // plugin handshake is - an empty index that looks like a working one is
    // worse than a daemon that says why it didn't start - and is recoverable:
    // the completion marker stays unset, so the next start walks again.
    if needs_bulk_index {
        let summary = bulk_index::run(&canonical_root, &conn)
            .context("failed to build the project's initial index")?;
        schema::record_bulk_index(&conn.lock().unwrap())
            .context("failed to record that the project was indexed")?;
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
    }

    let watcher = ProjectWatcher::new(root).context("failed to start the file watcher")?;
    {
        let conn = Arc::clone(&conn);
        let plugin = Arc::clone(&plugin);
        let root = canonical_root.clone();
        // Debouncer/BurstBatcher (watcher::debounce, watcher::burst) would
        // coalesce a burst of saves into fewer plugin round trips, but
        // wiring them in is nice-to-have, not required by this ticket's
        // acceptance criteria - left for a later pass rather than scope-
        // creeping into a batching rewrite here.
        thread::spawn(move || loop {
            match watcher.next_change(Duration::from_secs(3600)) {
                None => continue, // nothing arrived within the idle timeout; keep waiting
                Some(path) => {
                    let Some(file_path) = relative_wire_path(&root, &path) else {
                        // Outside the project root - shouldn't happen given how
                        // ProjectWatcher is scoped, but there is nothing to
                        // route a plugin request for if it does.
                        continue;
                    };
                    if let Err(err) = plugin.apply_file_change(&conn, file_path) {
                        eprintln!("g-mesh daemon: failed to apply file change: {err:#}");
                    }
                }
            }
        });
    }

    let socket = dir.join(SOCKET_FILE);
    // A socket file left behind by a crashed daemon makes bind() fail with
    // AddrInUse forever, so it is cleared first. That is only safe because
    // the singleton lock above guarantees no other daemon is serving this
    // project: any socket file still here belongs to a dead one.
    let _ = fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind daemon socket at {}", socket.display()))?;

    let pid_file = dir.join(PID_FILE);
    fs::write(&pid_file, std::process::id().to_string())
        .with_context(|| format!("failed to write pid file {}", pid_file.display()))?;

    serve_forever(listener, conn, plugin)
}

/// Runs the MCP accept loop until the process is killed.
///
/// This is the only async part of the daemon, and deliberately the last thing
/// that happens: `rmcp` requires tokio, but SQLite, the plugin bridge and the
/// watcher above have no use for it, so the runtime is entered here rather
/// than wrapped around a daemon that would otherwise gain nothing from it.
/// The listener is bound synchronously (above) and only then handed to tokio,
/// which keeps every bind/pid-file ordering guarantee the bootstrap race
/// depends on exactly where it was.
fn serve_forever(
    listener: UnixListener,
    conn: Arc<Mutex<Connection>>,
    plugin: Arc<PluginProcess>,
) -> Result<()> {
    // Two workers: connections are few (one per MCP client) and their work is
    // dominated by a mutex-guarded SQLite handle, so more threads would only
    // queue on the same lock.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to build the daemon's async runtime")?;

    runtime.block_on(async move {
        listener
            .set_nonblocking(true)
            .context("failed to put the daemon socket in non-blocking mode")?;
        let listener = tokio::net::UnixListener::from_std(listener)
            .context("failed to register the daemon socket with the async runtime")?;

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .context("failed to accept a daemon connection")?;
            let conn = Arc::clone(&conn);
            let plugin = Arc::clone(&plugin);
            tokio::spawn(async move {
                if let Err(err) = mcp::serve_connection(stream, conn, plugin).await {
                    eprintln!("g-mesh daemon: connection ended: {err:#}");
                }
            });
        }
    })
}

/// Converts an absolute, canonicalized path (as `ProjectWatcher` reports
/// them) into the project-relative, forward-slash path string the wire
/// protocol and storage layer use - the same convention the plugin's own
/// `toPosixPath` follows in bulkIndex.ts. `None` for a path outside `root`,
/// which is not this function's job to treat as an error.
fn relative_wire_path(root: &Path, absolute: &Path) -> Option<String> {
    let rel = absolute.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        parts.push(component.as_os_str().to_str()?.to_string());
    }
    Some(parts.join("/"))
}

/// Takes the project's daemon lock, or reports that someone else holds it.
///
/// The returned `File` must stay alive for as long as the daemon runs: the
/// lock is advisory and tied to the open file, so dropping it (or exiting,
/// or being killed) releases it - which is exactly what lets the next daemon
/// take over from a crashed one without any stale-lock cleanup.
fn acquire_singleton_lock(dir: &Path) -> Result<Option<File>> {
    let path = dir.join(DAEMON_LOCK_FILE);
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open daemon lock file {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err)
            .with_context(|| format!("failed to lock {}", path.display())),
    }
}

