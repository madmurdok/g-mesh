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

use crate::cli::stop;
use crate::daemon;
use crate::daemon::build_stamp::{self, Vintage};
use crate::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};

/// How long to keep retrying the first connect after bootstrapping a daemon,
/// and how long to wait between attempts. The daemon needs a few milliseconds
/// to bind; the bound stops a failed bootstrap from hanging the MCP client.
///
/// This is a budget for the daemon *binding*, and nothing else. It used to be
/// a budget for the daemon binding **and** walking the whole project, which is
/// what made a big enough project exceed it and cost its MCP client every tool
/// it had; since task 105 the bind happens before the walk (`daemon::run`), so
/// ten seconds is generous rather than nearly enough.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(10);
const BOOTSTRAP_RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Shortens [`BOOTSTRAP_TIMEOUT`] for the test suite. Real installs never set
/// it: the point of the constant is to be longer than any honest bind, and
/// nothing outside a test wants it shorter.
///
/// It exists so that "a project whose cold-start walk outlasts the shim's
/// bootstrap timeout still gets served" can be asserted in a couple of seconds
/// - shrinking the timeout below the walk, rather than stretching the walk
/// past ten real seconds on every commit.
const BOOTSTRAP_TIMEOUT_ENV: &str = "G_MESH_BOOTSTRAP_TIMEOUT_MS";

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
///
/// The shim is also the only process in the system that routinely holds both
/// halves of the "is this daemon still the current build?" question - it is
/// itself the newly installed binary, and it is about to talk to whatever has
/// been running since before that install - which is why the staleness check
/// lives here rather than in the daemon (see [`connect_or_bootstrap`]).
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

/// What this shim found already serving the project.
enum Incumbent {
    /// Listening, and running a build no older than this one's - the
    /// overwhelmingly common case, and the connection is already open.
    Reusable(UnixStream),
    /// Listening, but started from a build this one supersedes. Carries the
    /// verdict so the warning can say which of the two ways it was reached.
    Outdated(Vintage),
    /// Nothing is listening.
    Absent,
}

/// Finds the project's daemon and returns a connection to it, replacing or
/// bootstrapping one as needed.
///
/// # Why the check is here, before a single byte is proxied
///
/// Retiring a daemon means killing a process a client may be talking to, so
/// the only safe moment to consider it is *before* this shim's client has
/// sent anything: no `initialize` has gone out, no `tools/call` is in flight,
/// and the session that ends up on the socket only ever sees one daemon. A
/// check anywhere later - sniffing the `initialize` response, polling
/// mid-session - would mean tearing a live request down and replaying it, and
/// would cost the shim its one real invariant: that it never parses a payload.
/// The client's whole cost here is that the first connection took a moment
/// longer.
///
/// The other party's session is not so lucky: a *different* client already
/// connected to the retired daemon loses its connection and its shim exits,
/// which its MCP client sees as the server going away. That is the deliberate
/// trade - it happens once, at the moment a build changes, and the
/// alternative is every session on the machine going on being answered by a
/// build that has been replaced.
fn connect_or_bootstrap(root: &Path) -> Result<UnixStream> {
    let socket = daemon::socket_path(root)?;
    // A missing socket file, a refused connection and a socket left behind by
    // a dead daemon are all just "no daemon running" as far as the shim cares.
    if let Incumbent::Reusable(stream) = incumbent(root, &socket)? {
        return Ok(stream);
    }

    // Nothing usable is listening, so this shim may have to bootstrap a daemon
    // - a decision that has to be serialized across processes, because two
    // shims started at the same moment for the same project would otherwise
    // both spawn one. The lock is held across the spawn *and* the wait
    // below, so by the time it is handed on the daemon is already up.
    //
    // Retiring an outdated daemon is serialized by the same lock, and for a
    // sharper reason: two shims that independently decided to replace the
    // incumbent would have the second one kill the *replacement* the first
    // had just started.
    let lock = acquire_bootstrap_lock(root)?;

    // The re-check under the lock is what makes the lock worth taking: a
    // shim that queued behind another one finds the socket connectable here
    // and connects instead of spawning a second daemon - and, when the shim
    // ahead of it was replacing an outdated daemon, finds the replacement
    // current instead of retiring it all over again. Returning drops the
    // lock, handing it to whoever is waiting next.
    match incumbent(root, &socket)? {
        Incumbent::Reusable(stream) => return Ok(stream),
        Incumbent::Outdated(vintage) => {
            if !retire_outdated_daemon(root, vintage) {
                // Nothing else this shim can safely do: bootstrapping over a
                // daemon that would not stop just produces a second one that
                // exits on the singleton lock. An outdated answer with a
                // warning beside it beats an MCP client with no tools at all.
                if let Ok(stream) = UnixStream::connect(&socket) {
                    return Ok(stream);
                }
            }
        }
        Incumbent::Absent => {}
    }

    spawn_detached_daemon(root)?;
    let stream = wait_until_listening(&socket);
    // Released the moment the daemon is reachable - or, just as importantly,
    // the moment bootstrapping it failed, so one bad start does not wedge
    // every other shim on this project behind a lock nobody will release.
    drop(lock);
    stream
}

/// Connects to whatever is serving the project and judges whether it is still
/// the build this shim came from.
///
/// The connection is opened first and the stamp consulted second, so a project
/// with no daemon running pays nothing for the check at all.
fn incumbent(root: &Path, socket: &Path) -> Result<Incumbent> {
    let Ok(stream) = UnixStream::connect(socket) else {
        return Ok(Incumbent::Absent);
    };

    // A shim that cannot describe its own executable has no evidence about
    // anyone else's, and must degrade to exactly the behavior it had before
    // this check existed rather than act on a guess.
    let Ok(ours) = build_stamp::of_running_process() else {
        return Ok(Incumbent::Reusable(stream));
    };

    let published = build_stamp::read(&daemon::build_stamp_path(root)?);
    match build_stamp::vintage(published.as_ref(), &ours) {
        Vintage::Current => Ok(Incumbent::Reusable(stream)),
        // Dropped rather than kept for a possible fallback: the retirement
        // below is about to stop this daemon, and holding a connection open
        // to a process being asked to exit only gives it a reason to linger.
        outdated => {
            drop(stream);
            Ok(Incumbent::Outdated(outdated))
        }
    }
}

/// Stops a daemon this build supersedes, so the bootstrap below can put a
/// current one in its place. Reports whether it is safe to bootstrap over.
///
/// `cli::stop::stop` rather than a shutdown path of this module's own: it
/// already escalates `SIGTERM` to `SIGKILL`, already waits for the plugin the
/// daemon leaves behind, and already promises not to return until everything
/// it stopped is genuinely gone - which is exactly the guarantee bootstrapping
/// immediately afterwards depends on. A second implementation of that would
/// be a second thing to keep correct.
fn retire_outdated_daemon(root: &Path, vintage: Vintage) -> bool {
    // On stderr, which is where an MCP client collects its server's log: this
    // is the one place a human or agent gets told that the upgrade they just
    // installed had a running daemon in the way of it.
    eprintln!(
        "g-mesh mcp-shim: the daemon serving {} {} - replacing it so this build answers",
        root.display(),
        build_stamp::describe(vintage)
    );

    match stop::stop(root) {
        Ok(_) => true,
        Err(err) => {
            eprintln!(
                "g-mesh mcp-shim: could not stop the outdated daemon ({err:#}) - \
                 continuing with it, run `g-mesh stop` to force the upgrade through"
            );
            false
        }
    }
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

/// [`BOOTSTRAP_TIMEOUT`], unless [`BOOTSTRAP_TIMEOUT_ENV`] names a shorter
/// one. An unparseable value is ignored rather than fatal - a malformed knob
/// nobody in production sets must not be a reason an MCP client fails to
/// start.
fn bootstrap_timeout() -> Duration {
    std::env::var(BOOTSTRAP_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .map_or(BOOTSTRAP_TIMEOUT, Duration::from_millis)
}

fn wait_until_listening(socket: &Path) -> Result<UnixStream> {
    let timeout = bootstrap_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => {
                bail!(
                    "bootstrapped daemon did not accept connections on {} within {:?}: {err}",
                    socket.display(),
                    timeout
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
