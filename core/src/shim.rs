use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::daemon;
use crate::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};

/// How long to keep retrying the first connect after bootstrapping a daemon,
/// and how long to wait between attempts. The daemon needs a few milliseconds
/// to bind; the bound stops a failed bootstrap from hanging the MCP client.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Set by Claude Code on every stdio MCP server it spawns, in every
/// registration scope (local/project/user) - unlike the process's cwd, which
/// Claude Code's own docs call unreliable for learning the session's project
/// root. Preferring it over cwd is what lets g-mesh be registered once,
/// globally, instead of once per project.
const PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";

/// Stateless stdio<->AF_UNIX proxy. Project identity - the only thing the
/// shim needs - comes from `CLAUDE_PROJECT_DIR` when the client set it, or
/// the shim's own cwd otherwise (the only option for an MCP client that
/// isn't Claude Code, and the historical behavior this falls back to). Once
/// resolved, the shim hashes that path, connects to the project's daemon
/// socket (bootstrapping a detached daemon if nothing is listening) and then
/// moves JSON-RPC frames between the two sides without interpreting them.
pub fn run() -> Result<()> {
    let root = resolve_project_root()?;
    let stream = connect_or_bootstrap(&root)?;
    proxy(stream)
}

fn resolve_project_root() -> Result<PathBuf> {
    match std::env::var_os(PROJECT_DIR_ENV) {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => std::env::current_dir().context("failed to resolve the current directory"),
    }
}

fn connect_or_bootstrap(root: &Path) -> Result<UnixStream> {
    let socket = daemon::socket_path(root)?;
    // A missing socket file, a refused connection and a socket left behind by
    // a dead daemon are all just "no daemon running" as far as the shim cares.
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }

    // Nothing is listening, so this shim may have to bootstrap a daemon -
    // a decision that has to be serialized across processes, because two
    // shims started at the same moment for the same project would otherwise
    // both spawn one. The lock is held across the spawn *and* the wait
    // below, so by the time it is handed on the daemon is already up.
    let lock = acquire_bootstrap_lock(root)?;

    // The re-check under the lock is what makes the lock worth taking: a
    // shim that queued behind another one finds the socket connectable here
    // and connects instead of spawning a second daemon. Returning drops the
    // lock, handing it to whoever is waiting next.
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }

    spawn_detached_daemon(root)?;
    let stream = wait_until_listening(&socket);
    // Released the moment the daemon is reachable - or, just as importantly,
    // the moment bootstrapping it failed, so one bad start does not wedge
    // every other shim on this project behind a lock nobody will release.
    drop(lock);
    stream
}

/// Opens (creating if absent) the project's bootstrap lock file and blocks
/// until this process holds it exclusively.
///
/// Blocking rather than try-locking is the point: a shim that loses the race
/// has to wait for the winner's daemon rather than start a competing one.
/// The lock is an advisory `flock()` tied to the open file, so the kernel
/// drops it if the holder exits or is killed mid-bootstrap - a dead shim
/// cannot leave the project locked.
fn acquire_bootstrap_lock(root: &Path) -> Result<File> {
    let path = daemon::lock_path(root)?;
    // First shim for a project gets here before anything has created the
    // per-project state directory (the daemon is what usually creates it).
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create project directory {}", dir.display()))?;
    }

    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open bootstrap lock file {}", path.display()))?;
    file.lock()
        .with_context(|| format!("failed to take the bootstrap lock on {}", path.display()))?;
    Ok(file)
}

fn wait_until_listening(socket: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => {
                bail!(
                    "bootstrapped daemon did not accept connections on {} within {:?}: {err}",
                    socket.display(),
                    BOOTSTRAP_TIMEOUT
                );
            }
            Err(_) => thread::sleep(BOOTSTRAP_RETRY_INTERVAL),
        }
    }
}

fn spawn_detached_daemon(root: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve the g-mesh executable path")?;

    // Detachment: null stdio keeps the daemon off the shim's stdio (which is
    // the MCP protocol channel) and stops it dying when the client closes it;
    // its own process group keeps signals aimed at the client's process group
    // away from it. The `Child` is dropped without waiting, so the daemon
    // outlives the shim and is reaped by init.
    Command::new(&exe)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("failed to spawn a daemon via {}", exe.display()))?;
    Ok(())
}

fn proxy(stream: UnixStream) -> Result<()> {
    let mut outbound = stream.try_clone().context("failed to clone the daemon socket")?;

    // stdin->socket runs on its own thread while socket->stdout drives this
    // one, so a daemon that goes away ends the proxy even while the client is
    // idle. That thread is never joined: a blocking read on stdin cannot be
    // interrupted, and process exit tears it down.
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let result = pump(&mut stdin, &mut outbound);
        let _ = match &result {
            // EOF on stdin means the client is done: half-close so the daemon
            // sees it, can still flush replies in flight, and then closes.
            Ok(()) => outbound.shutdown(Shutdown::Write),
            Err(_) => outbound.shutdown(Shutdown::Both),
        };
        if let Err(err) = result {
            eprintln!("g-mesh mcp-shim: client stream ended: {err:#}");
        }
    });

    let mut inbound = BufReader::new(stream);
    let mut stdout = io::stdout().lock();
    pump(&mut inbound, &mut stdout)
}

/// Moves whole MCP messages across, one at a time. Both legs are newline-
/// delimited JSON - that is what the MCP stdio transport mandates on the
/// client side, and the daemon speaks the same framing on the socket, so the
/// shim can stay a repacking proxy that never parses a payload.
fn pump<R: BufRead, W: Write>(reader: &mut R, writer: &mut W) -> Result<()> {
    while let Some(frame) = read_ndjson_frame(reader)? {
        write_ndjson_frame(writer, &frame)?;
    }
    Ok(())
}
