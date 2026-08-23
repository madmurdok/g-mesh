//! The daemon's two idle timers, and the state each one owns.
//!
//! "The daemon" is two components with very different costs, so the
//! architecture doc gives them two independent idle timeouts rather than one
//! (see its Lifecycle & Operational Model section):
//!
//! - The **language plugin** (Node.js, tree-sitter plus the TS compiler API in
//!   one process) is the expensive half. It sleeps after
//!   `plugin.idleTimeoutMinutes` (default 1h) with no plugin work. While it is
//!   asleep the core keeps receiving watcher events and accumulates them into a
//!   dirty-file queue *without processing them*; the next MCP request that
//!   would otherwise be answered off a stale graph wakes the plugin and replays
//!   exactly that queue - not a rescan of the project.
//! - The **core** (socket listener, SQLite handle, fs watcher) is the cheap
//!   half, and registering fs watchers is a one-time cost per core lifetime,
//!   so it deliberately survives the plugin's short timeout. It exits only on
//!   `g-mesh stop`, a reboot, or a much longer `daemon.coreIdleTimeoutHours`
//!   (default 24h) with no MCP traffic at all. That long timeout exists to
//!   bound OS resource accumulation (inotify watchers, sockets, SQLite
//!   handles) across many projects touched over a long uptime - not to save
//!   memory during normal use.
//!
//! # Where the numbers come from
//!
//! From [`IdleTimeouts::from_config`]: a project's `config.toml`
//! (`config::read_project_config`) supplies `plugin.idleTimeoutMinutes` /
//! `daemon.coreIdleTimeoutHours`, or this module's own documented defaults if
//! the project has no config.toml - `config::PluginConfig` /
//! `config::DaemonConfig` default to the same 60 / 24 this module does, so
//! the two can never quietly disagree. The `G_MESH_*_IDLE_MS` env overrides
//! above config for the same reason they always have: a test can drive the
//! real timer directly without a config.toml, and real installs never set
//! them. Every consumer below still takes the resolved timeouts as data
//! rather than reading either source itself.
//!
//! # Lock order
//!
//! Two mutexes are involved in a replay: the supervisor's own, and the
//! daemon's single SQLite `Connection`. The plugin lock is always taken
//! *first* and the connection lock inside it, never the other way round - the
//! MCP handlers that trigger a replay finish with their `lastUsed` write and
//! release the connection before asking the supervisor for anything.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use crate::config::ProjectConfig;
use crate::daemon::manifest::PluginManifest;
use crate::daemon::plugin::PluginProcess;
use crate::embedding::EmbeddingPipeline;
use crate::watcher::staleness::{self, StalenessOutcome};

/// `plugin.idleTimeoutMinutes`'s documented default: long enough to survive
/// the pauses inside one working session (an agent that goes quiet for a
/// coffee must not pay to warm tsserver up again), short enough to give the
/// memory back on a genuinely long idle stretch.
pub const DEFAULT_PLUGIN_IDLE: Duration = Duration::from_secs(60 * 60);

/// `daemon.coreIdleTimeoutHours`'s documented default. Two orders of magnitude
/// above the plugin's on purpose: this one is about a project nobody has
/// touched for a day, not about a pause in a conversation.
pub const DEFAULT_CORE_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

/// Shortens the plugin's idle timeout for the test suite, in milliseconds.
/// Real installs never set it - the same escape hatch, and the same rationale,
/// as [`bulk_index::WALK_DELAY_ENV`](crate::daemon::bulk_index::WALK_DELAY_ENV):
/// a test can drive the real timer instead of faking the subsystem around it,
/// and nobody has to wait an hour to watch a plugin fall asleep.
///
/// `0` disables the timer outright (the plugin never sleeps), which is also
/// what a project's config will be able to say once it exists.
pub const PLUGIN_IDLE_ENV: &str = "G_MESH_PLUGIN_IDLE_MS";

/// The core's equivalent of [`PLUGIN_IDLE_ENV`]; `0` means "never exit on
/// idleness", which is exactly the MVP behaviour this module replaces.
pub const CORE_IDLE_ENV: &str = "G_MESH_CORE_IDLE_MS";

/// Never poll faster than this, however short the timeouts are - a test that
/// asks for a 10ms timeout still must not turn the monitor into a spin loop.
const MIN_TICK: Duration = Duration::from_millis(50);
/// Never poll slower than this, however long the timeouts are: with the
/// production defaults a quarter of the shorter timeout would be 15 minutes,
/// which is a needlessly coarse grain to notice `g-mesh stop`-less shutdown
/// conditions on, and the wakeups cost nothing.
const MAX_TICK: Duration = Duration::from_secs(30);

/// How long a plugin being put to sleep is given to exit on its own once its
/// stdin is closed, before it is signalled. It exits on the `end` event of its
/// stdin (index.ts), so this is the ordinary path, not a fallback.
const PLUGIN_EXIT_GRACE: Duration = Duration::from_millis(500);

/// Both idle timeouts, resolved once at daemon startup.
///
/// `None` means "this timer is off", which is the only way to ask for the
/// pre-task-38 behaviour (a plugin held for the core's whole life, a core held
/// until it is stopped) and the only sane reading of a configured `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleTimeouts {
    pub plugin: Option<Duration>,
    pub core: Option<Duration>,
}

impl Default for IdleTimeouts {
    fn default() -> Self {
        Self { plugin: Some(DEFAULT_PLUGIN_IDLE), core: Some(DEFAULT_CORE_IDLE) }
    }
}

impl IdleTimeouts {
    /// `config`'s timeouts (a project's `config.toml`, or
    /// `ProjectConfig::default()` for a project with none), unless the
    /// test-only environment overrides name something else.
    ///
    /// `config.plugin.idle_timeout_minutes` / `config.daemon
    /// .core_idle_timeout_hours` default to 60 / 24 - the same values
    /// [`DEFAULT_PLUGIN_IDLE`] / [`DEFAULT_CORE_IDLE`] hold here - so a
    /// project with no config.toml resolves to exactly what this module
    /// produced before config existed.
    pub fn from_config(config: &ProjectConfig) -> Self {
        let plugin_default = Duration::from_secs(config.plugin.idle_timeout_minutes.saturating_mul(60));
        let core_default = Duration::from_secs(config.daemon.core_idle_timeout_hours.saturating_mul(60 * 60));
        Self {
            plugin: parse_timeout(
                std::env::var(PLUGIN_IDLE_ENV).ok().as_deref(),
                plugin_default,
                PLUGIN_IDLE_ENV,
            ),
            core: parse_timeout(std::env::var(CORE_IDLE_ENV).ok().as_deref(), core_default, CORE_IDLE_ENV),
        }
    }

    /// How often [`supervise`] wakes up to re-examine both timers.
    ///
    /// A quarter of the shorter timeout, clamped: fine enough that a timeout
    /// overshoots by a quarter of itself at worst, coarse enough that the
    /// production defaults do not buy a wakeup nobody needs.
    fn tick(&self) -> Duration {
        match [self.plugin, self.core].into_iter().flatten().min() {
            Some(shortest) => (shortest / 4).clamp(MIN_TICK, MAX_TICK),
            // Nothing left to time, but the loop still has to notice an accept
            // loop that stopped, so it polls at its coarsest.
            None => MAX_TICK,
        }
    }
}

/// `None` for a configured zero (the timer is off), the default for anything
/// unparseable - a typo in a setting must not silently turn a timer off, and
/// must not stop the daemon from starting either.
fn parse_timeout(raw: Option<&str>, default: Duration, name: &str) -> Option<Duration> {
    let Some(raw) = raw else { return Some(default) };
    match raw.trim().parse::<u64>() {
        Ok(0) => None,
        Ok(millis) => Some(Duration::from_millis(millis)),
        Err(_) => {
            eprintln!(
                "g-mesh daemon: ignoring {name}={raw:?} - not a whole number of milliseconds; \
                 using the default {default:?}"
            );
            Some(default)
        }
    }
}

/// The files a change was seen for while the plugin was asleep, in the order
/// they were first seen and without repeats.
///
/// Order-preserving because replay order is the order the edits happened in,
/// which is the only order guaranteed to leave cross-file links pointing the
/// way the last edit meant them to. De-duplicating because a file saved fifty
/// times during a long sleep is still one reparse - the plugin diffs against
/// the file on disk *now*, so replaying it fifty times would produce forty-nine
/// empty diffs and pay for each of them.
#[derive(Debug, Default)]
struct DirtyQueue {
    order: Vec<String>,
    seen: HashSet<String>,
}

impl DirtyQueue {
    fn push(&mut self, file_path: String) {
        if self.seen.insert(file_path.clone()) {
            self.order.push(file_path);
        }
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn drain(&mut self) -> Vec<String> {
        self.seen.clear();
        std::mem::take(&mut self.order)
    }
}

struct SupervisedPlugin {
    /// `None` while the plugin is asleep. The whole point of the two-tier
    /// model lives in this `Option`.
    process: Option<PluginProcess>,
    dirty: DirtyQueue,
}

/// Owns the plugin process across its sleep/wake cycles, and the dirty-file
/// queue that makes sleeping safe.
///
/// Everything that used to hold an `Arc<PluginProcess>` holds one of these
/// instead: a process that can be absent cannot be handed out as a plain
/// reference, and the two facts that decide what to do with a file change -
/// "is it awake" and "what is already queued" - have to be read together or
/// an event can be applied and queued, or neither.
///
/// One of these per *language*, not one per daemon: `daemon::registry
/// ::PluginRegistry` creates them lazily, and every instance owns exactly the
/// one plugin its [`manifest`](Self::manifest) names, for its whole lifetime.
pub struct PluginSupervisor {
    /// Canonicalized, because that is what the plugin is spawned against and
    /// what `daemon::run` resolves its wire paths from.
    project_root: PathBuf,
    /// Which plugin this supervisor is *the* supervisor for - handed in once
    /// at construction and never replaced, because every one of this type's
    /// three spawn points (the first start, the wake inside
    /// [`replay_pending`](Self::replay_pending), the wake inside
    /// [`ensure_fresh`](Self::ensure_fresh)) has to produce the same plugin.
    /// A supervisor that could spawn a *different* language after a sleep
    /// than it did at startup would silently reindex a project's files with
    /// the wrong extractor, which is why this is a field rather than an
    /// argument to whichever call happens to do the spawning.
    manifest: PluginManifest,
    /// Rewritten on every wake and removed on every sleep, so tooling outside
    /// this process (`cli::status`, `cli::stop`) reads the truth rather than
    /// the pid of a plugin that deliberately exited.
    pid_file: PathBuf,
    idle_timeout: Option<Duration>,
    /// Handed in by `daemon::run` at construction time and shared with the
    /// cold-start bulk walk (`daemon::bulk_index::run`), which this
    /// supervisor knows nothing about - both hold the same `Arc`.
    /// Constructing an [`EmbeddingPipeline`] does no I/O and spawns no
    /// thread, so sharing it here costs `daemon::run`'s startup nothing;
    /// whichever of this supervisor or the bulk walk calls
    /// [`apply`](EmbeddingPipeline::apply) first is the one that pays to
    /// actually load the model, lazily, on its own thread - see
    /// `embedding::pipeline`'s module doc for why that must not happen any
    /// earlier.
    embedding: Arc<EmbeddingPipeline>,
    inner: Mutex<SupervisedPlugin>,
    last_activity: Mutex<Instant>,
    /// Mirrors `inner.dirty.is_empty()`, purely so the overwhelmingly common
    /// case - a tool call with nothing queued - never has to queue behind an
    /// in-flight reparse holding `inner`. A stale `false` costs one deferred
    /// replay, never a lost one: the flag is set inside the lock that queues.
    pending: AtomicBool,
}

impl PluginSupervisor {
    /// Spawns `manifest`'s plugin and records its pid.
    ///
    /// The manifest is taken by value and kept: it is what every later spawn
    /// this supervisor performs - a wake from sleep, and (inside
    /// `PluginProcess`) a crash relaunch - goes back to, so the plugin a
    /// supervisor owns can never change under it. Callers that still mean
    /// "the bundled JS/TS plugin" specifically pass
    /// `daemon::plugin::bundled_manifest()`; `daemon::registry
    /// ::PluginRegistry` passes whichever discovered manifest the language it
    /// is spawning claims.
    ///
    /// A failure here is still a hard failure for whoever asked: `daemon::run`
    /// has nothing useful to do without its plugin, and the registry reports
    /// the failure to the one file change that provoked the spawn rather than
    /// memoizing a supervisor that does not exist.
    pub fn start(
        project_root: &Path,
        manifest: PluginManifest,
        pid_file: PathBuf,
        idle_timeout: Option<Duration>,
        embedding: Arc<EmbeddingPipeline>,
    ) -> Result<Arc<Self>> {
        let process = PluginProcess::spawn(project_root, &manifest, pid_file.clone())
            .with_context(|| format!("failed to start the {} plugin", manifest.language))?;
        super::write_pid_file(&pid_file, process.pid());

        Ok(Arc::new(Self {
            project_root: project_root.to_path_buf(),
            manifest,
            pid_file,
            idle_timeout,
            embedding,
            inner: Mutex::new(SupervisedPlugin { process: Some(process), dirty: DirtyQueue::default() }),
            last_activity: Mutex::new(Instant::now()),
            pending: AtomicBool::new(false),
        }))
    }

    /// The language this supervisor's plugin speaks - its manifest's
    /// `language`, which `PluginProcess::spawn` has already checked against
    /// the live handshake, so it names the process actually running.
    pub fn language(&self) -> &str {
        &self.manifest.language
    }

    /// The pid of the plugin process right now, or `None` while it is asleep.
    ///
    /// Changes across a crash relaunch and across a sleep/wake cycle, which is
    /// exactly what makes it worth asking for: it is the one externally
    /// observable fact about *which* process a supervisor is currently
    /// serving from, and the only way anything outside this module can tell a
    /// recovered plugin from an untouched one.
    pub fn pid(&self) -> Option<u32> {
        self.inner.lock().unwrap().process.as_ref().map(PluginProcess::pid)
    }

    /// Whether anything is queued for the next wake. Cheap enough to ask on
    /// every tool call.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// The watcher thread's entry point: reindex `file_path` now if the plugin
    /// is awake, or remember it for the next wake if it is not.
    ///
    /// Failures are reported and dropped rather than propagated, which is what
    /// `daemon::run`'s watcher loop already did with them: one file the plugin
    /// could not reparse must not take the watcher thread down with it.
    pub fn file_changed(&self, conn: &Mutex<Connection>, file_path: String) {
        let mut inner = self.inner.lock().unwrap();
        let Some(process) = inner.process.as_ref() else {
            inner.dirty.push(file_path);
            self.pending.store(true, Ordering::SeqCst);
            return;
        };
        // Stamped before the round trip, not after: a reparse that takes a
        // while is the plugin being *used*, and an idle check that fired in
        // the middle of one would be measuring from the wrong end of it.
        self.touch();
        if let Err(err) = process.apply_file_change(conn, file_path, &self.embedding) {
            eprintln!("g-mesh daemon: failed to apply file change: {err:#}");
        }
    }

    /// Brings the index up to date with everything that changed while the
    /// plugin was asleep, waking it if needed. Returns how many queued files
    /// were replayed - `0`, without touching the plugin at all, when nothing
    /// is queued.
    ///
    /// This is the "next request that needs it" half of the sleep model, and
    /// "needs it" is deliberately narrow: a request that arrives with an empty
    /// queue is asking about a graph that is already current, and respawning a
    /// tsserver to tell it so would defeat the point of ever sleeping.
    pub fn replay_pending(&self, conn: &Mutex<Connection>) -> Result<usize> {
        let mut inner = self.inner.lock().unwrap();
        if inner.dirty.is_empty() {
            self.pending.store(false, Ordering::SeqCst);
            return Ok(0);
        }

        // Spawned before the queue is drained, so a plugin that fails to start
        // leaves the queue intact for the next request to retry rather than
        // swallowing the changes it was about to replay.
        if inner.process.is_none() {
            let process = PluginProcess::spawn(&self.project_root, &self.manifest, self.pid_file.clone())
                .with_context(|| format!("failed to wake the {} plugin", self.manifest.language))?;
            super::write_pid_file(&self.pid_file, process.pid());
            inner.process = Some(process);
        }

        let queued = inner.dirty.drain();
        self.pending.store(false, Ordering::SeqCst);
        // The one line that says what a wake actually did. Deliberately lists
        // the paths: "replayed the queue" and "rescanned the project" are
        // indistinguishable from a count, and the difference between them is
        // the whole reason the queue exists.
        eprintln!(
            "g-mesh daemon: waking the {} plugin to replay {} queued file change(s): {}",
            self.manifest.language,
            queued.len(),
            queued.join(", ")
        );

        let process = inner.process.as_ref().expect("just spawned or already running");
        self.touch();
        let mut replayed = 0;
        for file_path in &queued {
            match process.apply_file_change(conn, file_path.clone(), &self.embedding) {
                Ok(()) => replayed += 1,
                // One unreadable file must not cost the rest of the queue its
                // replay; it is reported and the walk carries on, matching how
                // a live watcher event's failure is handled.
                Err(err) => {
                    eprintln!("g-mesh daemon: failed to replay queued change to {file_path}: {err:#}")
                }
            }
        }
        Ok(replayed)
    }

    /// Runs a whole-project semantic pass, if the plugin is awake. Returns
    /// whether it actually ran.
    ///
    /// Called once the cold-start bulk walk has committed and the daemon has
    /// begun answering off it: the structural graph is what makes tools
    /// usable, and the semantic layer's confirmations land on top of it
    /// afterwards (see the architecture doc's cold-start sequence).
    ///
    /// A sleeping plugin is deliberately left asleep rather than woken. This
    /// pass has no queue behind it and nothing waiting on its answer, so
    /// respawning a tsserver to run one would spend exactly what sleeping
    /// exists to save - the same reasoning
    /// [`replay_pending`](Self::replay_pending) applies to an empty queue.
    /// In practice this is unreachable at the one call site there is (the
    /// plugin cannot have idled out during its own project's first walk),
    /// which is precisely why it must not be an error.
    pub fn semantic_pass(&self, conn: &Mutex<Connection>, file_paths: Vec<String>) -> Result<bool> {
        let inner = self.inner.lock().unwrap();
        let Some(process) = inner.process.as_ref() else { return Ok(false) };
        self.touch();
        process.semantic_pass(conn, file_paths, &self.embedding)?;
        Ok(true)
    }

    /// Synchronously brings `file_path` up to date if it has changed since it
    /// was last indexed, per `watcher::staleness::ensure_fresh` - the
    /// query-time safety net for a change the watcher never applied at all,
    /// as opposed to one still in flight.
    ///
    /// This is a genuinely different gap from the one
    /// `daemon::indexing_status`'s "Why the incremental-edit watcher path
    /// does not re-arm this" section (task 111) reasons about. That argument -
    /// a query blocks on the same mutex a live `apply_file_change` commit
    /// holds, so it can only ever read stale-but-consistent data, never torn
    /// data - is about a change the watcher *has already seen* and is in the
    /// middle of applying. It says nothing about a change the watcher never
    /// saw happen at all: an edit made while this project's daemon was not
    /// running (nothing re-walks an already-current index on restart - see
    /// `storage::schema::ensure_current`), or on a filesystem whose watcher
    /// backend silently drops events. No mutex is held in that case because
    /// nothing is applying anything - the staleness would persist forever,
    /// not just for a narrow, self-correcting window - which is the real,
    /// separate gap task 111 flagged as worth its own task rather than
    /// folding into `IndexingStatus`, and this method is that task's wiring.
    ///
    /// Wakes the plugin exactly like [`replay_pending`](Self::replay_pending) -
    /// spawning it if it is asleep - but only when the mtime/hash
    /// comparison actually calls for a reindex; the common case (nothing
    /// changed) resolves off the `indexed_files` table alone and never
    /// touches the plugin lock, matching the two-tier design
    /// `watcher::staleness` itself documents.
    pub fn ensure_fresh(&self, conn: &Mutex<Connection>, file_path: &str) -> Result<StalenessOutcome> {
        {
            let guard = conn.lock().unwrap();
            if !staleness::is_stale(&guard, &self.project_root, file_path)? {
                return Ok(StalenessOutcome::AlreadyFresh);
            }
        }

        let mut inner = self.inner.lock().unwrap();
        if inner.process.is_none() {
            let process = PluginProcess::spawn(&self.project_root, &self.manifest, self.pid_file.clone())
                .with_context(|| {
                    format!(
                        "failed to wake the {} plugin for a query-time staleness check",
                        self.manifest.language
                    )
                })?;
            super::write_pid_file(&self.pid_file, process.pid());
            inner.process = Some(process);
        }
        self.touch();
        let process = inner.process.as_ref().expect("just spawned or already running");
        process.ensure_fresh(conn, file_path, &self.embedding)
    }

    /// Puts the plugin to sleep if it has gone [`idle_timeout`] without work.
    /// Returns whether it actually slept.
    ///
    /// [`idle_timeout`]: IdleTimeouts::plugin
    pub fn sleep_if_idle(&self) -> bool {
        let Some(timeout) = self.idle_timeout else { return false };
        // Checked once outside the lock so a busy plugin's monitor tick never
        // queues behind the reparse that is keeping it busy...
        if self.idle_for() < timeout {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        // ...and again inside it, because the round trip this tick just waited
        // on is exactly the activity that should call the sleep off.
        if self.idle_for() < timeout {
            return false;
        }
        let Some(process) = inner.process.take() else { return false };
        self.put_to_sleep(process, &format!("idle for {timeout:?}"));
        true
    }

    /// Stops the plugin regardless of how idle it is - what the core does on
    /// its way out, so the plugin is reaped deliberately rather than left to
    /// notice its parent's pipes closing.
    pub fn sleep_now(&self, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        let Some(process) = inner.process.take() else { return };
        self.put_to_sleep(process, reason);
    }

    /// Shared tail of the two callers above; the caller has already taken the
    /// process out of `inner`, which is what makes this infallible from the
    /// rest of the daemon's point of view.
    fn put_to_sleep(&self, process: PluginProcess, reason: &str) {
        let pid = process.pid();
        if let Err(err) = process.shutdown(PLUGIN_EXIT_GRACE) {
            eprintln!("g-mesh daemon: the plugin (pid {pid}) did not shut down cleanly: {err:#}");
        }
        // Removed rather than left behind: a pid file naming a process that
        // deliberately exited is exactly the "crashed daemon" shape `cli::stop`
        // and `cli::status` read it as.
        let _ = fs::remove_file(&self.pid_file);
        eprintln!(
            "g-mesh daemon: {} plugin (pid {pid}) put to sleep - {reason}; file changes will be \
             queued until a request needs it again",
            self.manifest.language
        );
    }

    fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity.lock().unwrap().elapsed()
    }
}

/// When the core last had anything to do, and how many clients are attached
/// right now.
///
/// Both, because "no MCP requests for 24 hours" has to mean the project is
/// unattended, not that a long-lived editor session happened to ask nothing
/// overnight: a core that exited from under a connected client would take that
/// client's whole tool surface with it, and the client has no way to know it
/// should reconnect. A live connection therefore holds the core open, and the
/// idle clock only starts once the last one goes away.
pub struct CoreActivity {
    last_request: Mutex<Instant>,
    live_connections: AtomicUsize,
}

impl CoreActivity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { last_request: Mutex::new(Instant::now()), live_connections: AtomicUsize::new(0) })
    }

    /// Records that the core just did something a user asked for. Called per
    /// tool call rather than per connection, for the same reason
    /// `mcp::GMeshMcpServer::mark_used` is.
    pub fn request(&self) {
        *self.last_request.lock().unwrap() = Instant::now();
    }

    /// Registers an accepted connection for as long as the returned guard
    /// lives.
    pub fn connection_opened(self: &Arc<Self>) -> ConnectionGuard {
        self.live_connections.fetch_add(1, Ordering::SeqCst);
        self.request();
        ConnectionGuard(Arc::clone(self))
    }

    /// `Some(how long it has been idle)` once every condition for a clean
    /// idle exit holds; `None` while the timer is off, a client is attached,
    /// or the timeout has not elapsed.
    pub fn idle_beyond(&self, timeout: Option<Duration>) -> Option<Duration> {
        let timeout = timeout?;
        if self.live_connections.load(Ordering::SeqCst) > 0 {
            return None;
        }
        let idle = self.last_request.lock().unwrap().elapsed();
        (idle >= timeout).then_some(idle)
    }
}

/// Keeps the core alive for one connection's lifetime, and restarts the idle
/// clock when that connection ends - a client that just disconnected is the
/// most recent thing the core did, not the beginning of a day of silence.
pub struct ConnectionGuard(Arc<CoreActivity>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.live_connections.fetch_sub(1, Ordering::SeqCst);
        self.0.request();
    }
}

/// The daemon's main thread once startup is over: runs both idle timers until
/// the accept loop stops or the core's own timeout says it is time to go.
///
/// Structured as a poll of the accept loop's result channel rather than a
/// `join` on its thread (which is what `daemon::run` used to end with) purely
/// so this thread can wake up on a schedule of its own without giving up the
/// ability to report an accept loop that failed.
///
/// Returning `Ok(())` means the process should end: `daemon::run` returns,
/// `main` returns, and the OS closes the socket, the SQLite handle and the
/// watchers - the same teardown a `g-mesh stop` performs, just decided from
/// the inside. The next shim to look for this project finds nothing listening
/// and cold-starts a fresh daemon, which is the ordinary bootstrap path rather
/// than a recovery one.
///
/// Nothing is left half-written by that exit, and the plugin is what
/// guarantees it: the only writer that could still be running is the watcher
/// thread applying a file change, which holds the supervisor's lock for the
/// whole round trip including its commit - so [`sleep_now`] below cannot
/// return until that transaction is durable. What ends the process is a
/// `return`, after that call, not a `std::process::exit` from a timer.
///
/// [`sleep_now`]: PluginSupervisor::sleep_now
///
/// Takes the whole [`PluginRegistry`](crate::daemon::registry::PluginRegistry)
/// rather than one supervisor since task 155: every language that has ever
/// been spawned gets its own idle check
/// ([`PluginRegistry::sleep_if_idle_all`](crate::daemon::registry::PluginRegistry::sleep_if_idle_all)),
/// independently, and every one of them is put to sleep on the core's own way
/// out ([`PluginRegistry::sleep_all_now`](crate::daemon::registry::PluginRegistry::sleep_all_now)) -
/// a language that was never touched simply has nothing to do either time.
pub fn supervise(
    state_dir: &Path,
    registry: &crate::daemon::registry::PluginRegistry,
    core: &CoreActivity,
    timeouts: IdleTimeouts,
    accept_loop: Receiver<Result<()>>,
) -> Result<()> {
    let tick = timeouts.tick();
    loop {
        match accept_loop.recv_timeout(tick) {
            // The accept loop only ever ends by failing; its error is the
            // daemon's error.
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => bail!("the daemon's MCP accept loop panicked"),
            Err(RecvTimeoutError::Timeout) => {}
        }

        registry.sleep_if_idle_all();

        if let Some(idle) = core.idle_beyond(timeouts.core) {
            eprintln!(
                "g-mesh daemon: no MCP requests for {idle:?} - shutting down; the next request \
                 will start a fresh daemon for this project"
            );
            registry.sleep_all_now("the core is shutting down");
            release_state_files(state_dir);
            return Ok(());
        }
    }
}

/// Clears the files that describe a running daemon, on the way out of being
/// one.
///
/// Unconditional, unlike `cli::stop`'s equivalent: this runs *inside* the
/// process that owns every one of these files, so there is no question of
/// whose they are. Failures are ignored - a leftover file is cosmetic (the
/// shim connects and finds nothing there, `cli::status` cross-checks pids
/// against the socket), and refusing to exit over one would be worse than the
/// mess it leaves.
///
/// Every `plugin-<language>.pid` file is cleared, not just one - a listing
/// of the state directory rather than a per-language loop over the registry's
/// live supervisors, so a language that slept (and already removed its own
/// pid file - see [`PluginSupervisor::put_to_sleep`]) is not the only kind of
/// "already gone" this has to handle right.
fn release_state_files(state_dir: &Path) {
    let _ = fs::remove_file(super::pid_path_in(state_dir));
    for (_, pid_file) in super::registry::discovered_pid_files(state_dir) {
        let _ = fs::remove_file(pid_file);
    }
    let _ = fs::remove_file(super::build_stamp_path_in(state_dir));
    // Derived through the parent module rather than spelled out again here:
    // what a daemon binds and what it releases have to be the same endpoint by
    // construction. On Windows this is a no-op, because a pipe name is
    // released by the handle closing and there is no file to remove - see
    // `ipc::windows`.
    if let Some(endpoint) = super::endpoint_in(state_dir) {
        endpoint.clear_stale();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::daemon::manifest::read_manifest;
    use crate::daemon::test_plugin;

    /// The reason [`PluginSupervisor::manifest`] is a field and not an
    /// argument: a supervisor spawns *its own* plugin at every one of its
    /// spawn points, including the wake that follows a sleep. Asserted on
    /// processes, not on a return value - the fake plugin records every
    /// process it is ever started as, so a wake that went to the wrong
    /// manifest would leave this one's spawn count at 1 (and, with the
    /// pre-registry hardcoded bridge, would have started the bundled JS/TS
    /// plugin instead).
    #[test]
    fn a_supervisor_wakes_the_plugin_its_own_manifest_names() {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let plugin_dir = test_plugin::install(plugins.path(), "python", &[".python-src"]);
        let manifest = read_manifest(&plugin_dir).expect("the fixture manifest must parse");
        let conn = test_plugin::empty_index();

        let supervisor = PluginSupervisor::start(
            project.path(),
            manifest,
            plugins.path().join("plugin.pid"),
            None,
            Arc::new(EmbeddingPipeline::disabled()),
        )
        .expect("the fixture plugin must start");

        assert_eq!(supervisor.language(), "python");
        let first_pid = supervisor.pid().expect("a freshly started plugin is awake");
        assert_eq!(test_plugin::spawns(&plugin_dir), vec![first_pid]);

        supervisor.sleep_now("the test asked it to");
        assert_eq!(supervisor.pid(), None, "a sleeping supervisor has no process");

        // Queued rather than applied, exactly as the watcher's events are
        // while the plugin sleeps...
        supervisor.file_changed(&conn, "app.python-src".to_string());
        assert!(supervisor.has_pending());
        assert_eq!(test_plugin::spawns(&plugin_dir).len(), 1, "queueing must not wake anything");

        // ...and the wake that replays them goes back to this supervisor's
        // own manifest.
        assert_eq!(supervisor.replay_pending(&conn).expect("the wake must succeed"), 1);
        let woken_pid = supervisor.pid().expect("replaying wakes the plugin");
        assert_ne!(woken_pid, first_pid);
        assert_eq!(
            test_plugin::spawns(&plugin_dir),
            vec![first_pid, woken_pid],
            "the wake must have spawned this manifest's plugin, not some other one"
        );
    }

    #[test]
    fn an_unset_timeout_reads_as_its_documented_default() {
        assert_eq!(parse_timeout(None, DEFAULT_PLUGIN_IDLE, "X"), Some(DEFAULT_PLUGIN_IDLE));
    }

    /// Guards every test below that touches [`PLUGIN_IDLE_ENV`] /
    /// [`CORE_IDLE_ENV`]: they are process-wide state, and `cargo test` runs
    /// this module's tests on multiple threads by default, so two of them
    /// setting/clearing the same variable at once would be a genuine race,
    /// not just noise. Held for the lifetime of each test that needs it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A project with no config.toml (`ProjectConfig::default()`) must
    /// resolve to exactly the pre-config defaults - task #38's behavior,
    /// unchanged now that config is wired in.
    #[test]
    fn a_default_config_resolves_to_the_documented_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PLUGIN_IDLE_ENV);
        std::env::remove_var(CORE_IDLE_ENV);

        let resolved = IdleTimeouts::from_config(&ProjectConfig::default());
        assert_eq!(resolved, IdleTimeouts::default());
        assert_eq!(resolved.plugin, Some(DEFAULT_PLUGIN_IDLE));
        assert_eq!(resolved.core, Some(DEFAULT_CORE_IDLE));
    }

    /// The acceptance criterion at the unit level: a config.toml with a
    /// shortened `plugin.idleTimeoutMinutes` actually produces a shortened
    /// plugin timer, not the hardcoded default.
    #[test]
    fn a_configured_plugin_idle_timeout_overrides_the_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PLUGIN_IDLE_ENV);
        std::env::remove_var(CORE_IDLE_ENV);

        let config = ProjectConfig {
            plugin: crate::config::PluginConfig { idle_timeout_minutes: 5 },
            ..ProjectConfig::default()
        };
        let resolved = IdleTimeouts::from_config(&config);
        assert_eq!(resolved.plugin, Some(Duration::from_secs(5 * 60)));
        // The core timeout is untouched by a config that only sets [plugin].
        assert_eq!(resolved.core, Some(DEFAULT_CORE_IDLE));
    }

    /// Same claim, for `daemon.coreIdleTimeoutHours`.
    #[test]
    fn a_configured_core_idle_timeout_overrides_the_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PLUGIN_IDLE_ENV);
        std::env::remove_var(CORE_IDLE_ENV);

        let config = ProjectConfig {
            daemon: crate::config::DaemonConfig { core_idle_timeout_hours: 2 },
            ..ProjectConfig::default()
        };
        let resolved = IdleTimeouts::from_config(&config);
        assert_eq!(resolved.core, Some(Duration::from_secs(2 * 60 * 60)));
        assert_eq!(resolved.plugin, Some(DEFAULT_PLUGIN_IDLE));
    }

    /// The test-only env override still wins over a configured value - the
    /// same precedence it always had over the hardcoded default, now proven
    /// against a config that disagrees with it too.
    #[test]
    fn the_env_override_still_wins_over_a_configured_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(PLUGIN_IDLE_ENV, "250");

        let config = ProjectConfig {
            plugin: crate::config::PluginConfig { idle_timeout_minutes: 5 },
            ..ProjectConfig::default()
        };
        let resolved = IdleTimeouts::from_config(&config);
        assert_eq!(resolved.plugin, Some(Duration::from_millis(250)));

        std::env::remove_var(PLUGIN_IDLE_ENV);
    }

    #[test]
    fn a_zero_timeout_turns_the_timer_off() {
        assert_eq!(parse_timeout(Some("0"), DEFAULT_CORE_IDLE, "X"), None);
    }

    #[test]
    fn a_number_is_read_as_milliseconds() {
        assert_eq!(parse_timeout(Some(" 250 "), DEFAULT_CORE_IDLE, "X"), Some(Duration::from_millis(250)));
    }

    /// A typo must not silently disable a timer, and must not stop the daemon.
    #[test]
    fn an_unparseable_timeout_falls_back_to_the_default() {
        assert_eq!(parse_timeout(Some("10 minutes"), DEFAULT_PLUGIN_IDLE, "X"), Some(DEFAULT_PLUGIN_IDLE));
    }

    #[test]
    fn the_monitor_tick_is_clamped_at_both_ends() {
        let production = IdleTimeouts::default();
        assert_eq!(production.tick(), MAX_TICK, "a quarter of an hour is too coarse to be useful");

        let tiny = IdleTimeouts { plugin: Some(Duration::from_millis(4)), core: Some(DEFAULT_CORE_IDLE) };
        assert_eq!(tiny.tick(), MIN_TICK, "a short test timeout must not become a spin loop");

        let test_sized =
            IdleTimeouts { plugin: Some(Duration::from_millis(800)), core: Some(DEFAULT_CORE_IDLE) };
        assert_eq!(test_sized.tick(), Duration::from_millis(200));
    }

    #[test]
    fn both_timers_off_still_yields_a_sane_tick() {
        assert_eq!(IdleTimeouts { plugin: None, core: None }.tick(), MAX_TICK);
    }

    #[test]
    fn the_dirty_queue_keeps_first_sighting_order_and_drops_repeats() {
        let mut queue = DirtyQueue::default();
        assert!(queue.is_empty());

        queue.push("src/a.ts".to_string());
        queue.push("src/b.ts".to_string());
        queue.push("src/a.ts".to_string()); // saved again during the same sleep
        assert!(!queue.is_empty());

        assert_eq!(queue.drain(), vec!["src/a.ts".to_string(), "src/b.ts".to_string()]);
        assert!(queue.is_empty(), "a drained queue starts the next sleep empty");

        // And the de-duplication set is drained with it, or a file changed in
        // two consecutive sleeps would be replayed only in the first.
        queue.push("src/a.ts".to_string());
        assert_eq!(queue.drain(), vec!["src/a.ts".to_string()]);
    }

    #[test]
    fn a_disabled_core_timeout_never_reports_an_idle_core() {
        let core = CoreActivity::new();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(core.idle_beyond(None), None);
    }

    #[test]
    fn an_unattended_core_reports_idle_once_its_timeout_elapses() {
        let core = CoreActivity::new();
        assert_eq!(core.idle_beyond(Some(Duration::from_secs(60))), None, "it has only just started");

        std::thread::sleep(Duration::from_millis(20));
        let idle = core.idle_beyond(Some(Duration::from_millis(10))).expect("nothing is attached");
        assert!(idle >= Duration::from_millis(10), "reported idle time must be the real one: {idle:?}");
    }

    /// The guarantee that keeps a long-lived editor session's core alive.
    #[test]
    fn a_live_connection_holds_the_core_open_however_quiet_it_is() {
        let core = CoreActivity::new();
        let guard = core.connection_opened();
        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(
            core.idle_beyond(Some(Duration::from_millis(1))),
            None,
            "a client is attached, so the core is not idle no matter how long it has been silent"
        );

        drop(guard);
        // Dropping restarts the clock rather than exposing the silence that
        // came before it - the disconnect is itself the most recent activity.
        assert_eq!(core.idle_beyond(Some(Duration::from_millis(10))), None);
        std::thread::sleep(Duration::from_millis(20));
        assert!(core.idle_beyond(Some(Duration::from_millis(10))).is_some());
    }

    #[test]
    fn a_request_restarts_the_idle_clock() {
        let core = CoreActivity::new();
        std::thread::sleep(Duration::from_millis(20));
        core.request();
        assert_eq!(core.idle_beyond(Some(Duration::from_millis(15))), None);
    }
}
