pub mod identity;

use std::fs;
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::protocol::jsonrpc::{read_frame, write_frame};
use crate::storage::connection::{self, project_dir};
use crate::storage::schema;
use crate::watcher::ProjectWatcher;

const SOCKET_FILE: &str = "daemon.sock";
const PID_FILE: &str = "daemon.pid";

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

    let conn = connection::open(root).context("failed to open the project's SQLite index")?;
    if schema::ensure_current(&conn).context("failed to check schema version")? {
        eprintln!("g-mesh daemon: schema (re)initialized - a full reindex is needed");
    }
    let conn = Arc::new(Mutex::new(conn));

    let watcher = ProjectWatcher::new(root).context("failed to start the file watcher")?;
    // Wiring watcher events into plugin notifications + reindex diffs is a
    // separate ticket; for now the watcher just needs to stay registered
    // (dropping it would stop watching) without its queue growing forever.
    thread::spawn(move || loop {
        watcher.next_change(Duration::from_secs(3600));
    });

    let socket = dir.join(SOCKET_FILE);
    // A socket file left behind by a crashed daemon makes bind() fail with
    // AddrInUse forever, so it is cleared first. Two daemons racing to bind
    // the same project is the concurrent-first-start race that the pid/socket
    // file lock covers - that guard is a separate ticket.
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
