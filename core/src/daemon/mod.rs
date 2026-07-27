pub mod identity;

use std::fs;
use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};

use crate::protocol::jsonrpc::{read_frame, write_frame};
use crate::storage::connection::project_dir;

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

/// PLACEHOLDER daemon core. It binds the project's socket and echoes every
/// frame it receives straight back - nothing more. The real core (SQLite
/// handle, file watcher, MCP tool dispatch, idle timers) is a later ticket;
/// this exists only so the shim has a real process to bootstrap, connect to
/// and proxy through.
///
/// Unix-only for now: AF_UNIX is available on Windows 10+ but is not wired
/// up here (see the architecture doc's shim/daemon section).
pub fn run(root: &Path) -> Result<()> {
    let dir = project_dir(root)?;
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create project directory {}", dir.display()))?;

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
        thread::spawn(move || {
            if let Err(err) = echo(stream) {
                eprintln!("g-mesh daemon: connection ended: {err:#}");
            }
        });
    }
    Ok(())
}

fn echo(stream: UnixStream) -> Result<()> {
    let mut writer = stream.try_clone().context("failed to clone daemon socket")?;
    let mut reader = BufReader::new(stream);
    while let Some(frame) = read_frame(&mut reader)? {
        write_frame(&mut writer, &frame)?;
    }
    Ok(())
}
