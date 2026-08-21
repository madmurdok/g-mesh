use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::cli::stop;
use crate::daemon;
use crate::daemon::build_stamp::{self, Vintage};
use crate::ipc;
use crate::process;
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
/// bootstrap timeout still gets served" can be asserted in a couple of
/// seconds - shrinking the timeout below the walk, rather than stretching the
/// walk past ten real seconds on every commit.
const BOOTSTRAP_TIMEOUT_ENV: &str = "G_MESH_BOOTSTRAP_TIMEOUT_MS";

/// Set by Claude Code on every stdio MCP server it spawns, in every
/// registration scope (local/project/user) - unlike the process's cwd, which
/// Claude Code's own docs call unreliable for learning the session's project
/// root. Preferring it over cwd is what lets g-mesh be registered once,
/// globally, instead of once per project.
///
/// It is inherited, though, like any other environment variable, and the shim
/// cannot tell "my MCP client set this for me" from "something four processes
/// up the tree was an MCP server". Anything that runs `g-mesh mcp-shim`
/// underneath a Claude Code session - a wrapper script, a test suite, a task
/// runner invoked from an MCP server of its own - therefore gets the
/// *session's* project served, whatever cwd it took care to set. `pub` so
/// those callers can name the variable they have to clear rather than
/// hard-coding a copy of it; `core/tests` does exactly that, which is task
/// 192.
pub const PROJECT_DIR_ENV: &str = "CLAUDE_PROJECT_DIR";

/// Path to append the bootstrapped daemon's stderr to, instead of discarding
/// it. Unset in every normal run, which is the only reason the daemon's stderr
/// can go to `/dev/null` at all: nobody is on the other end of a detached
/// process's console.
///
/// That default is what makes a daemon that starts and then stalls
/// undiagnosable from the outside - the shim can say "it never began
/// answering", and nothing can say why. Pointing this at a file gives the one
/// missing channel back, and costs nothing when it is unset. It captures the
/// plugin processes' stderr too, since they are spawned with
/// `Stdio::inherit()` (`daemon::bulk_index::walk_one_language`,
/// `daemon::plugin::PluginProcess::spawn`).
///
/// Appended to, never truncated, so several daemons - or several runs - can
/// share one file without erasing each other; a path that cannot be opened
/// falls back to `/dev/null` rather than failing the bootstrap, because a
/// diagnostic aid must never be the reason a daemon does not start.
pub const DAEMON_LOG_ENV: &str = "G_MESH_DAEMON_LOG";

/// Stateless stdio<->daemon proxy. Project identity - the only thing the
/// shim needs - comes from `CLAUDE_PROJECT_DIR` when the client set it, or
/// the shim's own cwd otherwise (the only option for an MCP client that
/// isn't Claude Code, and the historical behavior this falls back to). Once
/// resolved, the shim hashes that path, connects to the project's daemon
/// endpoint (bootstrapping a detached daemon if nothing is listening) and
/// then moves JSON-RPC frames between the two sides without interpreting
/// them. Which kind of endpoint that is - an AF_UNIX socket or a named pipe -
/// is `crate::ipc`'s business and appears nowhere in this file.
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
    Reusable(ipc::Stream),
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
fn connect_or_bootstrap(root: &Path) -> Result<ipc::Stream> {
    let endpoint = daemon::endpoint(root)?;
    // Checked before anything is attempted, because every step below assumes
    // the endpoint is one a daemon could listen on. An over-long AF_UNIX path
    // fails only at the daemon's `bind`, in a detached child whose stderr
    // goes to a log the caller is not reading - so without this the shim
    // spawns a daemon that dies instantly, then spends the full
    // `LISTEN_TIMEOUT` retrying a connect that cannot ever succeed, and
    // reports a timeout rather than the reason.
    if let Err(message) = endpoint.check_length() {
        bail!("{message}");
    }

    // A missing endpoint, a refused connection and (on Unix) a socket left
    // behind by a dead daemon are all just "no daemon running" as far as the
    // shim cares.
    if let Incumbent::Reusable(stream) = incumbent(root, &endpoint)? {
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
    match incumbent(root, &endpoint)? {
        Incumbent::Reusable(stream) => return Ok(stream),
        Incumbent::Outdated(vintage) => {
            if !retire_outdated_daemon(root, vintage) {
                // Nothing else this shim can safely do: bootstrapping over a
                // daemon that would not stop just produces a second one that
                // exits on the singleton lock. An outdated answer with a
                // warning beside it beats an MCP client with no tools at all.
                if let Ok(stream) = ipc::Stream::connect(&endpoint) {
                    return Ok(stream);
                }
            }
        }
        Incumbent::Absent => evict_wedged_daemon(root),
    }

    // Said out loud, once per cold start, because this is the moment the shim
    // commits to a project - and the moment its answer to "which project?" is
    // worth auditing. `PROJECT_DIR_ENV` is inherited by anything that runs the
    // shim underneath an MCP server, so a caller that set a cwd and expected
    // it to be honored can be serving a completely different tree; naming both
    // the root and where it came from turns that from an invisible
    // redirection into one line of the client's server log. Reusing an
    // already-running daemon stays silent: nothing was decided there.
    eprintln!(
        "g-mesh mcp-shim: nothing is serving {} ({}) - starting a daemon for it",
        root.display(),
        match std::env::var_os(PROJECT_DIR_ENV) {
            Some(dir) if !dir.is_empty() => format!("from {PROJECT_DIR_ENV}"),
            _ => "the current directory".to_string(),
        }
    );
    spawn_detached_daemon(root)?;
    let stream = wait_until_listening(&endpoint);
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
fn incumbent(root: &Path, endpoint: &ipc::Endpoint) -> Result<Incumbent> {
    let Ok(stream) = ipc::Stream::connect(endpoint) else {
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

/// How long a daemon that holds the singleton lock while not serving is given
/// to start serving before it is evicted.
///
/// It only has to cover the gap between a daemon publishing itself as serving
/// (`daemon::record_serving_owner`, written immediately after the bind) and
/// that socket being connectable, which is a scheduling hiccup rather than any
/// real work - a daemon that has not got that far reads as
/// `DaemonLock::Starting` and is never a candidate here in the first place.
/// Kept well inside [`BOOTSTRAP_TIMEOUT`] so the replacement this clears the
/// way for still has most of the budget left to bind in.
const WEDGE_CONFIRMATION: Duration = Duration::from_millis(500);

/// Clears a daemon that holds this project's singleton lock but has stopped
/// serving it, so the bootstrap below can succeed instead of exiting on a lock
/// no client can benefit from (task 184).
///
/// # Why this is safe to do automatically, and why only from here
///
/// The singleton lock exists so two daemons can never both serve one project,
/// and an eviction that races could break exactly that. Three things keep it
/// intact:
///
/// - Only a holder confirmed **not** to be serving is ever signalled. A daemon
///   answering on its socket is `DaemonLock::Serving` and is never touched, so
///   no client's session is taken away by this; a holder that has taken the
///   lock but not yet published itself as serving is `DaemonLock::Starting`
///   and is left to finish starting.
/// - The judgement is confirmed a second time after
///   [`WEDGE_CONFIRMATION`], because "not answering" is the one input here
///   that could be a momentary artifact rather than a state.
/// - This runs under the bootstrap lock, held across the eviction *and* the
///   spawn that follows it, so two shims cannot evict-and-replace at once -
///   the same serialization that already stops two shims from bootstrapping
///   two daemons, and the reason this lives in the shim rather than in the
///   daemon: the daemon deliberately never takes the bootstrap lock, because
///   the shim holds it *while* spawning one (see `daemon::DAEMON_LOCK_FILE`'s
///   own note on why the two locks are separate files).
///
/// Even if all of that failed, the daemon that follows still has to take the
/// singleton lock before it serves anything, so the worst an eviction can cost
/// is a bootstrap that loses the race and exits - never two daemons on one
/// project.
///
/// Best-effort: a failure to signal is reported and the bootstrap goes ahead
/// anyway, which lands back on today's behaviour (the new daemon exits on the
/// lock and the caller is told why) rather than on a worse one.
fn evict_wedged_daemon(root: &Path) {
    let daemon::DaemonLock::Wedged { pid } = lock_state(root) else {
        return;
    };
    thread::sleep(WEDGE_CONFIRMATION);
    let daemon::DaemonLock::Wedged { pid: confirmed } = lock_state(root) else {
        return;
    };
    if confirmed != pid {
        // A different process holds it now, so whatever the first look saw has
        // already gone - and signalling the newcomer on its behalf is exactly
        // what this second look exists to prevent.
        return;
    }

    eprintln!(
        "g-mesh mcp-shim: pid {pid} holds the daemon lock for {} but stopped serving it - \
         clearing it so this project can be served again",
        root.display()
    );
    if let Err(err) = stop::terminate(pid, stop::TERMINATION_TIMEOUT) {
        eprintln!("g-mesh mcp-shim: could not clear the wedged daemon (pid {pid}): {err:#}");
    }
}

/// [`daemon::inspect_daemon_lock`], with a failure to inspect reading as
/// "nothing to act on" - the direction that leaves processes alone.
fn lock_state(root: &Path) -> daemon::DaemonLock {
    daemon::inspect_daemon_lock(root).unwrap_or(daemon::DaemonLock::Free)
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

fn wait_until_listening(endpoint: &ipc::Endpoint) -> Result<ipc::Stream> {
    let timeout = bootstrap_timeout();
    let deadline = Instant::now() + timeout;
    loop {
        match ipc::Stream::connect(endpoint) {
            Ok(stream) => return Ok(stream),
            Err(err) if Instant::now() >= deadline => {
                bail!(
                    "bootstrapped daemon did not accept connections on {endpoint} within {timeout:?}: {err}"
                );
            }
            Err(_) => thread::sleep(BOOTSTRAP_RETRY_INTERVAL),
        }
    }
}

fn spawn_detached_daemon(root: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve the g-mesh executable path")?;

    // Detachment has two halves. Null stdio keeps the daemon off the shim's
    // stdio (which is the MCP protocol channel) and stops it dying when the
    // client closes it; `process::detach` keeps signals aimed at the client's
    // process group away from it (its own process group on Unix,
    // `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows). The `Child`
    // is dropped without waiting, so the daemon outlives the shim - reaped by
    // init on Unix, and needing no reaper at all on Windows.
    let mut command = Command::new(&exe);
    command
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(daemon_stderr());
    process::detach(&mut command)
        .spawn()
        .with_context(|| format!("failed to spawn a daemon via {}", exe.display()))?;
    Ok(())
}

/// `/dev/null` unless [`DAEMON_LOG_ENV`] names a file that can be appended to.
///
/// Never a pipe, whatever the setting: the shim drops the `Child` without
/// waiting, so nothing would ever drain it, and the first daemon (or plugin)
/// to fill the pipe buffer would block forever on a write it does not know is
/// unread. A file and `/dev/null` both absorb writes unconditionally, which is
/// the property the detached daemon's stderr has to keep.
fn daemon_stderr() -> Stdio {
    let Some(path) = std::env::var_os(DAEMON_LOG_ENV).filter(|value| !value.is_empty()) else {
        return Stdio::null();
    };
    match fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => Stdio::from(file),
        Err(err) => {
            eprintln!(
                "g-mesh mcp-shim: could not open {} ({DAEMON_LOG_ENV}) for the daemon's stderr, discarding it instead: {err}",
                Path::new(&path).display()
            );
            Stdio::null()
        }
    }
}

fn proxy(stream: ipc::Stream) -> Result<()> {
    let mut outbound = stream.try_clone().context("failed to clone the daemon connection")?;

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
            // Windows named pipes have no half-close, so there this ends the
            // whole connection instead - see `ipc::windows::Stream::shutdown`
            // for what that costs.
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
