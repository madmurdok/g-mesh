pub mod identity;
pub mod plugin;

use std::fs::{self, File, TryLockError};
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::daemon::plugin::PluginProcess;
use crate::protocol::jsonrpc::{read_frame, write_frame};
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
/// version), registers the file watcher, and serves shim connections on
/// the project's AF_UNIX socket. Simplified MVP lifecycle - runs until
/// killed; the two-tier idle-timeout model and `g-mesh stop` are backlog.
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

    for stream in listener.incoming() {
        let stream = stream.context("failed to accept a daemon connection")?;
        let conn = Arc::clone(&conn);
        thread::spawn(move || {
            if let Err(err) = serve(stream, &conn) {
                eprintln!("g-mesh daemon: connection ended: {err:#}");
            }
        });
    }
    Ok(())
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

/// The shim<->daemon socket carries whatever JSON-RPC the MCP client sends;
/// real tool dispatch is later tickets (MCP server scaffolding, find_*).
/// For now only `status` is handled for real, so a manual client has
/// something genuine to check the daemon is alive and its DB is open -
/// anything else gets a proper JSON-RPC "method not found" rather than a
/// placeholder echo.
fn serve(stream: UnixStream, conn: &Arc<Mutex<Connection>>) -> Result<()> {
    let mut writer = stream.try_clone().context("failed to clone daemon socket")?;
    let mut reader = BufReader::new(stream);
    while let Some(frame) = read_frame(&mut reader)? {
        if let Some(response) = handle_request(&frame, conn) {
            write_frame(&mut writer, &response)?;
        }
    }
    Ok(())
}

fn handle_request(frame: &[u8], conn: &Arc<Mutex<Connection>>) -> Option<Vec<u8>> {
    let request: Value = match serde_json::from_slice(frame) {
        Ok(v) => v,
        Err(_) => return Some(error_response(Value::Null, -32700, "parse error")),
    };
    // No `id` means a notification (JSON-RPC 2.0): no response is sent.
    let id = request.get("id")?.clone();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");

    let response = match method {
        "status" => {
            let node_count: i64 = conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
                .unwrap_or(0);
            success_response(id, json!({ "status": "ok", "pid": std::process::id(), "nodeCount": node_count }))
        }
        other => error_response(id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

fn success_response(id: Value, result: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .expect("response is always serializable")
}

fn error_response(id: Value, code: i32, message: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .expect("response is always serializable")
}
