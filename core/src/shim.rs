use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::daemon;
use crate::protocol::jsonrpc::{read_frame, write_frame};

/// How long to keep retrying the first connect after bootstrapping a daemon,
/// and how long to wait between attempts. The daemon needs a few milliseconds
/// to bind; the bound stops a failed bootstrap from hanging the MCP client.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Stateless stdio<->AF_UNIX proxy. The MCP client spawns this with the
/// project directory as cwd, which is the only project identity the shim
/// needs: it hashes that path, connects to the project's daemon socket
/// (bootstrapping a detached daemon if nothing is listening) and then moves
/// JSON-RPC frames between the two sides without interpreting them.
pub fn run() -> Result<()> {
    let root = std::env::current_dir().context("failed to resolve the current directory")?;
    let stream = connect_or_bootstrap(&root)?;
    proxy(stream)
}

fn connect_or_bootstrap(root: &Path) -> Result<UnixStream> {
    let socket = daemon::socket_path(root)?;
    // A missing socket file, a refused connection and a socket left behind by
    // a dead daemon are all just "no daemon running" as far as the shim cares.
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }

    // TOCTOU: two shims starting concurrently on the same project can both
    // get here and both spawn a daemon. The file lock that closes this gap is
    // a separate ticket - deliberately not guarded here.
    spawn_detached_daemon(root)?;

    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    loop {
        match UnixStream::connect(&socket) {
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

fn pump<R: BufRead, W: Write>(reader: &mut R, writer: &mut W) -> Result<()> {
    while let Some(frame) = read_frame(reader)? {
        write_frame(writer, &frame)?;
    }
    Ok(())
}
