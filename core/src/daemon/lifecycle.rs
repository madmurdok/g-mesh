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
//!
//! # A third thing riding the plugin's idle-check tick (task GM-274)
//!
//! `[plugin] memoryLimitMb` (`config::PluginConfig::memory_limit_mb`,
//! `PluginSupervisor::check_memory_limit`, `daemon::memory`) is not a third
//! timer - it is a second check the *existing* idle-check tick runs, right
//! alongside [`PluginSupervisor::sleep_if_idle`]. `IdleTimeouts::tick` is
//! that period: a quarter of the shorter of the two idle timeouts, clamped
//! between [`MIN_TICK`] (50ms) and [`MAX_TICK`] (30s) - so in production
//! (default 1h plugin idle timeout) it is a flat 30s, and in a test that
//! shortens the idle timeout it scales down with it.
//!
//! A plugin that spikes past `memoryLimitMb` and is put back to sleep by
//! something else (an idle timeout, a crash) before the next 30-second tick
//! ever samples it is a spike this mechanism never sees, and that is by
//! design rather than by omission: `memoryLimitMb` is a **circuit breaker on
//! a sustained overage**, not a ceiling. GM-304 settled that question and the
//! architecture doc's "Plugin memory limit" section carries the argument;
//! [`PluginSupervisor::check_memory_limit`]'s own doc comment carries the
//! consequence, which is that it confirms an over-limit reading with a second
//! sample before suspending anything.
//!
//! What GM-274 filed as an open question about the *interval* turned out not
//! to be about the interval at all. GM-291 measured the thing being caught: a
//! real `rust-analyzer`'s RSS ramps to a plateau of 563-580MB and holds there
//! indefinitely, never given back. Any interval shorter than "forever"
//! observes that, including the production 30s. What actually bounds how soon
//! suspension happens is when `check_memory_limit` can acquire
//! [`PluginSupervisor::inner`], which `semantic_pass` holds for a whole
//! synchronous round trip - and, counter-intuitively, that blocking is what
//! makes the breaker fire *promptly* on a language's first cold pass rather
//! than what stops it. See the architecture doc's GM-304 notes.
//!
//! # A fourth thing on that same tick: is there still anything to serve (GM-320)
//!
//! Both timers above measure *silence*, and silence is the wrong question for
//! a daemon whose project has been deleted out from under it. The core's
//! timeout does eventually collect one - nothing resets a clock nobody is
//! connecting to - but "eventually" is [`DEFAULT_CORE_IDLE`], a full day, and
//! the thing being held for that day is not small: a Rust plugin's process
//! tree is 563-580MB (GM-291) and the core itself measured 1.4GB RSS on this
//! machine, so a handful of them is gigabytes. Four were found running at once
//! on one developer machine - two from throwaway `/tmp` builds, two from a
//! worktree whose `target/` had been deleted - and all four were cleared by
//! hand.
//!
//! [`orphan_check`] is the answer, and it is deliberately the narrowest one
//! that covers those cases: two `stat`s per tick, and an exit only on an
//! absence the filesystem positively reports. A daemon whose **project root**
//! is gone can never answer another useful question - every path it would
//! resolve, watch or reparse is underneath it. A daemon whose **own
//! executable** is gone cannot even be compared against a newer build
//! (`daemon::build_stamp` reads that file's mtime), so a shim can neither
//! reuse it honestly nor retire it; it is orphaned in the strongest sense the
//! word has here. Neither is a "nobody has asked lately" judgement, which is
//! why neither waits out an idle timeout and why the check runs *first* in the
//! tick, ahead of the sleep and memory-limit passes a doomed daemon has no
//! reason to pay for.
//!
//! Three things were weighed against it and are not here. Making the core
//! timeout **unconditional** was the first: the timer is not in fact being
//! reset by anything improper - `CoreActivity::request` fires on a connection
//! and on a tool call, and nothing connects to a daemon whose project is
//! gone - so shortening or un-gating it would punish healthy long-lived
//! daemons to reach a case this check reaches in one tick. A **supervisor or
//! reaper** that sweeps the machine for orphans was the second, and it is the
//! most machinery for the least specific benefit: a second long-lived process
//! to install, keep current and stop, in order to notice from outside what
//! each daemon can answer about itself from two `stat`s. And **exiting only
//! once nothing is attached**, the rule [`CoreActivity::idle_beyond`] applies
//! to the idle timeout, was the third - rejected because the reason that rule
//! exists (a client's tool surface must not vanish under it) has already
//! failed when the project root has: every tool call is now about files that
//! do not exist, and preserving that session costs a gigabyte to serve
//! nothing. The bound this buys is therefore unconditional - **at most one
//! tick**, 30s with the production defaults - rather than "one tick, unless
//! someone is holding the door".
//!
//! # Why there is no signal handling here, measured rather than assumed
//!
//! `crate::process`'s header states that this daemon installs no signal
//! handler and that `SIGTERM` kills it outright. GM-320 set out to find the
//! gap behind that claim - a request treated as a completed termination, the
//! shape GM-321 had just found one layer down in the TS plugin - and found
//! none: against a real daemon on macOS, `SIGTERM` ended the core in
//! 0.19-0.30s in every configuration tried (started by hand, bootstrapped
//! detached through the shim, project root deleted, own executable deleted,
//! and holding a live `rust-analyzer` tree), and each time the whole plugin
//! tree went with it. What forced `kill -9` was not a swallowed signal but an
//! *unreachable* one: `cli::stop` takes its project root from the current
//! directory, and for an orphan that directory is exactly what no longer
//! exists - `g-mesh stop` there fails with "failed to resolve the current
//! directory". So the polite stop works and could not be asked for, which is
//! why this module's answer is for the daemon to stop *itself*.
//! `core/tests/daemon_sigterm.rs` is what keeps the first half of that true.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
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
use crate::protocol::jsonrpc::is_timeout;
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
pub(crate) fn parse_timeout(raw: Option<&str>, default: Duration, name: &str) -> Option<Duration> {
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

    /// How many distinct files are queued right now - GM-403's progress
    /// ticker names this alongside the language while a replay is in flight,
    /// so it is read (not drained) while `replay_pending` is still running.
    fn len(&self) -> usize {
        self.order.len()
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
    /// `[plugin] memoryLimitMb` (task GM-274), resolved once at daemon
    /// startup exactly like [`idle_timeout`](Self::idle_timeout) - `None`
    /// means "off", the documented default: idle sleep only, exactly today's
    /// behaviour, and [`check_memory_limit`](Self::check_memory_limit) never
    /// samples anything. See `daemon::memory` for the sampling itself and
    /// this field's own consumer.
    memory_limit_mb: Option<u64>,
    /// Set the instant [`check_memory_limit`](Self::check_memory_limit) finds
    /// this plugin's process tree over `memory_limit_mb`, and never cleared
    /// for the rest of this supervisor's life - per the architecture doc's
    /// "Plugin memory limit" section, suspension "lasts until the daemon
    /// restarts or the config changes", and since config is read once at
    /// daemon startup (`daemon::run`, not hot-reloaded), a running daemon has
    /// no path back to `false` short of exiting. In memory, not in the index
    /// (decision 5 - see this field's doc comment on [`check_memory_limit`]
    /// for where it is *also* recorded, for `g-mesh status`), so a restart
    /// clears it for free by simply not carrying it forward: a fresh
    /// supervisor starts every language unsuspended, exactly as it starts
    /// every language awake.
    semantic_suspended: AtomicBool,
    /// Guards the "one-time log" half of [`check_memory_limit`](Self::check_memory_limit)'s
    /// contract for a platform/build where `daemon::memory::process_tree_rss_mb`
    /// never finds evidence either way (see that function's own doc comment) -
    /// logged once per supervisor, not once per idle-check tick for the rest
    /// of this daemon's life.
    sampling_unavailable_logged: AtomicBool,
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
        memory_limit_mb: Option<u64>,
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
            memory_limit_mb,
            semantic_suspended: AtomicBool::new(false),
            sampling_unavailable_logged: AtomicBool::new(false),
        }))
    }

    /// The language this supervisor's plugin speaks - its manifest's
    /// `language`, which `PluginProcess::spawn` has already checked against
    /// the live handshake, so it names the process actually running.
    pub fn language(&self) -> &str {
        &self.manifest.language
    }

    /// This supervisor's own manifest - GM-272's `daemon::workspace_reindex`
    /// needs the full `PluginManifest` (its `command`/`args` for the one-shot
    /// bulk walk, its `capabilities.semantic_pass`), not just [`language`](Self::language).
    /// Never a *different* manifest than the one this supervisor was spawned
    /// with - see this struct's own doc comment on the `manifest` field for
    /// why that invariant matters.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
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

    /// Hands the running plugin process a new round-trip budget - see
    /// [`PluginProcess::set_round_trip_timeouts`], which carries the whole
    /// argument for why this exists and why it is test-only.
    ///
    /// A no-op while the plugin is asleep: there is no process to re-budget,
    /// and the next wake spawns one that reads the env override afresh.
    #[cfg(test)]
    pub(crate) fn set_round_trip_timeouts(&self, timeouts: crate::daemon::plugin::RoundTripTimeouts) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(process) = inner.process.as_mut() {
            process.set_round_trip_timeouts(timeouts);
        }
    }

    /// Whether anything is queued for the next wake. Cheap enough to ask on
    /// every tool call.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// How many files are queued right now, for GM-403's replay progress
    /// message - `0` once [`replay_pending`](Self::replay_pending) has
    /// drained the queue, same as [`has_pending`](Self::has_pending) going
    /// false at that point. Takes the same lock `replay_pending` holds for
    /// its own read of the queue, so this is a snapshot, not a promise that
    /// the count will still be true by the time it is printed.
    pub fn pending_len(&self) -> usize {
        self.inner.lock().unwrap().dirty.len()
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
        // Kept before the call, which consumes `file_path` - only needed on
        // the timeout branch below, but cloning a file path is cheap next to
        // everything else a round trip costs.
        let retry_path = file_path.clone();
        if let Err(err) =
            process.apply_file_change(conn, file_path, &self.embedding, self.is_semantic_suspended())
        {
            if is_timeout(&err) {
                // `PluginProcess::apply_file_change`'s own doc comment: a
                // timed-out request is deliberately not replayed inline (the
                // plugin may have still been mid-write on it, and the process
                // behind it has already been killed and relaunched by the
                // time this error reaches us). Queue it exactly like a file
                // that changed while the plugin was asleep, so the next
                // request that touches this language - `replay_pending`,
                // reached the same way a wake-from-sleep is - sends it again
                // against the fresh process instead of it being silently
                // dropped.
                eprintln!(
                    "g-mesh daemon: {} plugin timed out applying a file change ({err:#}) - \
                     the plugin was relaunched and {retry_path} is queued for replay",
                    self.manifest.language
                );
                inner.dirty.push(retry_path);
                self.pending.store(true, Ordering::SeqCst);
            } else {
                eprintln!("g-mesh daemon: failed to apply file change: {err:#}");
            }
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
        let semantic_suspended = self.is_semantic_suspended();
        let mut replayed = 0;
        for file_path in &queued {
            match process.apply_file_change(conn, file_path.clone(), &self.embedding, semantic_suspended) {
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
    ///
    /// `file_count` is forwarded to `PluginProcess::semantic_pass` unchanged.
    /// See that method and `daemon::plugin::RoundTripTimeouts`'s doc comment
    /// for why the whole-project timeout has to scale with it.
    ///
    /// Suspension (decision 5) is checked first, ahead of even the sleeping
    /// check: a suspended language answers `Ok(false)` here exactly like a
    /// sleeping one, whether or not its plugin happens to be awake for
    /// structural work at the moment this is called - core never sends
    /// `semanticPass` to a suspended language (per the architecture doc's
    /// "Plugin memory limit" section), and this is the one place both this
    /// method's caller (`daemon::semantic::run_with_registry`/`run_once`'s
    /// per-language scheduler) and `daemon::workspace_reindex`'s
    /// single-language semantic phase both go through, so gating it here
    /// covers both without either caller having to know about suspension
    /// itself.
    pub fn semantic_pass(
        &self,
        conn: &Mutex<Connection>,
        file_paths: Vec<String>,
        file_count: usize,
    ) -> Result<bool> {
        if self.is_semantic_suspended() {
            return Ok(false);
        }
        let inner = self.inner.lock().unwrap();
        let Some(process) = inner.process.as_ref() else { return Ok(false) };
        self.touch();
        process.semantic_pass(conn, file_paths, file_count, &self.embedding)?;
        Ok(true)
    }

    /// Runs `f` with this supervisor's own serialization lock held - the same
    /// lock [`file_changed`](Self::file_changed)/[`replay_pending`](Self::replay_pending)/
    /// [`ensure_fresh`](Self::ensure_fresh)/[`semantic_pass`](Self::semantic_pass)
    /// already take, each for the duration of its own single round trip to
    /// the plugin.
    ///
    /// GM-272's per-language workspace reindex (`daemon::workspace_reindex`)
    /// is the motivating caller: deleting this language's rows and re-walking
    /// them has to be atomic with respect to an ordinary settled edit to one
    /// of this language's files, or the two can race - a `fileChanged` diff
    /// committed between the delete and the re-walk would either resurrect
    /// what the delete just removed (if the walk's own batches land after
    /// it) or be silently wiped (if the delete runs after it committed). This
    /// method is what lets that whole sequence share the *one* lock
    /// `file_changed` already contends on, instead of inventing a second,
    /// parallel lock a caller could take in the wrong order against this
    /// one - see this module's own doc comment ("Lock order") for the rule
    /// `f` itself must keep honoring if it goes on to touch the connection:
    /// this lock first, the connection's inside it, never the other way
    /// round.
    ///
    /// `f` is handed the live process, if the plugin is currently awake, so
    /// it can send that process something (GM-272's `workspaceChanged`
    /// notification) without a second lookup under the same lock. Nothing
    /// here wakes a sleeping plugin - matching every other method on this
    /// type, which treats "not currently needed" as a reason not to spawn a
    /// process a caller has no real work for right now; a workspace reindex
    /// still runs its delete/walk/link phase regardless (a fresh one-shot
    /// process the caller owns, not this supervisor's own `inner.process`,
    /// per `daemon::bulk_index`'s own reasoning for why a bulk walk is
    /// always its own process), and the reindex is what actually needs the
    /// language up to date - not this notification.
    pub fn with_exclusive_access<T>(&self, f: impl FnOnce(Option<&PluginProcess>) -> T) -> T {
        let inner = self.inner.lock().unwrap();
        self.touch();
        f(inner.process.as_ref())
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
        process.ensure_fresh(conn, file_path, &self.embedding, self.is_semantic_suspended())
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

    /// Whether this language's semantic passes are suspended right now - see
    /// this struct's own doc comment on the `semantic_suspended` field.
    /// [`file_changed`](Self::file_changed)/[`replay_pending`](Self::replay_pending)/
    /// [`ensure_fresh`](Self::ensure_fresh) pass this down to gate the
    /// per-file `semanticPass` round trip
    /// (`watcher::apply::apply_file_change`'s `semantic_pass_capable`), and
    /// [`semantic_pass`](Self::semantic_pass) (the whole-project scheduler)
    /// checks it directly - see decision 5.
    pub fn is_semantic_suspended(&self) -> bool {
        self.semantic_suspended.load(Ordering::SeqCst)
    }

    /// Samples this supervisor's plugin process tree's resident memory
    /// (`daemon::memory::process_tree_rss_mb`) on the same tick
    /// [`sleep_if_idle`](Self::sleep_if_idle) already runs on - see
    /// `daemon::lifecycle`'s own module doc for that tick's actual period,
    /// and the architecture doc's "Plugin memory limit" section for the
    /// behaviour this implements end to end.
    ///
    /// A deliberate series of early-outs, in order:
    ///
    /// - `memory_limit_mb` unset (`None`, the documented default): returns
    ///   immediately, before taking any lock or calling into `daemon::memory`
    ///   at all. This is what makes "no key set means no sampling side
    ///   effects" a true statement at the unit level, not just a claim about
    ///   the number compared against - see this task's own acceptance
    ///   criterion.
    /// - The plugin is already asleep (idle, or suspended by an earlier call
    ///   to this same method): nothing to sample, and nothing new to do -
    ///   suspension already happened, or idle sleep already achieved the same
    ///   "stop paying for this process" outcome this method exists to reach
    ///   for an overage.
    /// - Sampling itself answers `None` (`daemon::memory::process_tree_rss_mb`'s
    ///   own doc comment: the process already exited under us, or platform
    ///   enumeration found nothing at all): no evidence of an overage, so
    ///   this tick is a no-op - logged once per supervisor, not once per
    ///   tick, via `sampling_unavailable_logged`.
    /// - Measured, but not over the limit: nothing to do.
    /// - Measured over the limit **once**: still nothing to do yet. A second,
    ///   confirming sample is taken, and the language is suspended only if
    ///   that one is over the limit too - see "Why a confirming sample"
    ///   below.
    ///
    /// # Why a confirming sample (GM-304, GM-307)
    ///
    /// `memoryLimitMb` is a **circuit breaker**, not a ceiling: the plugin is
    /// allowed to cross the limit once and is then suspended so it cannot go
    /// on doing so (the architecture doc's "Plugin memory limit" section
    /// argues why a sampler cannot be anything else). What that decision
    /// makes enforceable is a *sustained* overage, and a single aggregate
    /// sample is not evidence of one.
    ///
    /// The reason is `daemon::memory`'s, not a general worry about noise: a
    /// process tree's membership is not stable. A `rust-analyzer` shells out
    /// to `rustc` and to build scripts; a `tsserver` forks on a project
    /// reload. Each is a genuine member of the tree while it lives, and each
    /// leaves again. GM-307 measured a tree fall 170MB between two snapshots
    /// milliseconds apart for exactly that reason, with nothing having grown
    /// or shrunk at all. So one over-limit aggregate says only "at this
    /// instant the tree included enough processes to cross the limit", which
    /// is the *transient spike* GM-274's decision 3 explicitly says this
    /// mechanism does not exist to catch.
    ///
    /// The confirming sample is asked for only on the path that is about to
    /// act, and the asymmetry is deliberate: suspension is irreversible for
    /// this daemon's life (see the `semantic_suspended` field), while
    /// *declining* to suspend costs at most one tick, because what the
    /// breaker exists to catch is by measurement a plateau that is never
    /// given back (GM-291's implementation notes: flat for 19+ seconds and
    /// counting, at 563-580MB). An irreversible decision is worth confirming;
    /// a reversible one self-corrects on the next tick for free.
    ///
    /// Nothing sleeps between the two samples - each
    /// `sysinfo::refresh_processes` is a whole-system scan that costs about
    /// 0.4s on its own (GM-291's measured `check_memory_limit` duration), so
    /// the gap is real without this method holding `inner` any longer than
    /// the work itself takes.
    ///
    /// Only past every one of those does this actually put the plugin to
    /// sleep - through the same [`put_to_sleep`](Self::put_to_sleep) tail
    /// [`sleep_if_idle`](Self::sleep_if_idle)/[`sleep_now`](Self::sleep_now)
    /// use, with a reason naming both the configured limit and the measured
    /// figure (the architecture doc's own wording: "the reason naming the
    /// limit and the measured figure") - and mark this language suspended,
    /// both in memory (`semantic_suspended`, checked by every semantic-pass
    /// gate - decision 5) and on disk (a `plugin-<language>.suspended` marker
    /// next to this supervisor's own pid file - decision 6), so a *separate*
    /// `g-mesh status` process, which has no running daemon to ask a
    /// question of, can still report it (see `cli::status`'s own doc comment
    /// on how every other runtime fact it reports is read the same way, off
    /// disk).
    pub fn check_memory_limit(&self) {
        self.check_memory_limit_sampled_by(crate::daemon::memory::process_tree_rss_mb);
    }

    /// [`check_memory_limit`](Self::check_memory_limit)'s whole body, with the
    /// sampler as a parameter so tests can drive the one thing a real sampler
    /// cannot be asked to produce on demand: a specific *sequence* of
    /// readings. The public method above is the only non-test caller and
    /// always passes `daemon::memory::process_tree_rss_mb`.
    ///
    /// A seam rather than a mock of the whole check: everything that decides
    /// anything - the early-outs, the confirming sample, the suspension and
    /// its marker - is this function, exercised for real by every test below
    /// and by production alike. Only the number comes from elsewhere.
    ///
    /// `pub(crate)` rather than private since GM-390: `cli::status`'s own
    /// suspended-language test needs the same seam, for the same reason this
    /// module's tests already do - see that test's doc comment.
    pub(crate) fn check_memory_limit_sampled_by(&self, sample: impl Fn(u32) -> Option<u64>) {
        let Some(limit_mb) = self.memory_limit_mb else { return };
        let mut inner = self.inner.lock().unwrap();
        let Some(process) = inner.process.as_ref() else { return };
        let pid = process.pid();

        let Some(measured_mb) = sample(pid) else {
            self.log_sampling_unavailable_once(pid);
            return;
        };
        if measured_mb <= limit_mb {
            return;
        }

        // One over-limit reading is one instant, and an instant can hold a
        // transient member of the tree that is gone again by the next scan -
        // see this method's "Why a confirming sample" section. Suspension is
        // irreversible, so it waits for a second reading that agrees.
        let Some(confirmed_mb) = sample(pid) else {
            self.log_sampling_unavailable_once(pid);
            return;
        };
        if confirmed_mb <= limit_mb {
            eprintln!(
                "g-mesh daemon: the {} plugin's process tree read {measured_mb}MB against a \
                 memoryLimitMb of {limit_mb}MB, but a confirming sample read {confirmed_mb}MB - \
                 treating that as a transient member of the tree rather than a sustained overage, \
                 and leaving the plugin running",
                self.manifest.language
            );
            return;
        }

        let process = inner.process.take().expect("checked Some above");
        self.semantic_suspended.store(true, Ordering::SeqCst);
        let reason = format!(
            "memoryLimitMb {limit_mb}MB exceeded - measured {measured_mb}MB across its process \
             tree and confirmed at {confirmed_mb}MB by a second sample"
        );
        self.write_suspended_marker(&reason);
        self.put_to_sleep(process, &reason);
    }

    /// The "no evidence either way" log, emitted once per supervisor rather
    /// than once per idle-check tick for the rest of this daemon's life - see
    /// the `sampling_unavailable_logged` field's own doc comment.
    fn log_sampling_unavailable_once(&self, pid: u32) {
        if !self.sampling_unavailable_logged.swap(true, Ordering::SeqCst) {
            eprintln!(
                "g-mesh daemon: could not sample the {} plugin's process-tree memory \
                 (pid {pid}) - memoryLimitMb has nothing to enforce against until a later \
                 sample succeeds; logged once",
                self.manifest.language
            );
        }
    }

    /// Where this language's suspension marker lives - decision 6's "smallest
    /// consistent mechanism": a file next to this supervisor's own pid file,
    /// named the same way (`daemon::registry::plugin_suspended_marker_file_name`
    /// mirrors `plugin_pid_file_name`), so `cli::status`/`cli::stop` find it
    /// the same way they already find `self.pid_file` - by listing the
    /// project's state directory, not by asking a live daemon.
    fn suspended_marker_path(&self) -> PathBuf {
        self.pid_file
            .parent()
            .expect("a pid file is always inside the project's state directory")
            .join(super::registry::plugin_suspended_marker_file_name(&self.manifest.language))
    }

    /// Best-effort, atomic-rename write of this language's suspension marker -
    /// the same temp-then-rename shape [`super::write_pid_file`] already
    /// uses, so a `cli::status` reading this file never sees a half-written
    /// one. `reason` is the human-readable string `check_memory_limit` built,
    /// persisted verbatim because nothing else about *why* a language was
    /// suspended survives outside the daemon process that decided it.
    fn write_suspended_marker(&self, reason: &str) {
        let path = self.suspended_marker_path();
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        if let Err(err) = fs::write(&temporary, format!("{reason}\n")) {
            eprintln!("g-mesh daemon: failed to write suspension marker {}: {err}", temporary.display());
            return;
        }
        if let Err(err) = fs::rename(&temporary, &path) {
            eprintln!("g-mesh daemon: failed to put suspension marker {} in place: {err}", path.display());
            let _ = fs::remove_file(&temporary);
        }
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

/// A reason this daemon can never serve anyone again, as opposed to merely
/// having nobody asking right now - see this module's own "A fourth thing on
/// that same tick" section for why those are different questions and why only
/// these two absences count as this one.
///
/// Carries the path it judged so the log line names it: an operator reading
/// "the project root no longer exists" wants to know *which* root, and this is
/// the only place that fact is still held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Orphaned {
    /// The canonicalized project root [`supervise`] was given is gone. Every
    /// path this daemon would resolve, watch or reparse lives under it.
    ProjectRootGone(PathBuf),
    /// The executable this process was started from is gone - a `cargo clean`,
    /// a deleted worktree, a `/tmp` build swept away. Nothing can compare this
    /// daemon's build against a newer one any more (`daemon::build_stamp`
    /// reads that file's mtime), so no shim can honestly reuse or retire it.
    ExecutableGone(PathBuf),
}

impl fmt::Display for Orphaned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectRootGone(path) => {
                write!(f, "the project root {} no longer exists", path.display())
            }
            Self::ExecutableGone(path) => {
                write!(f, "the executable this daemon was started from ({}) no longer exists", path.display())
            }
        }
    }
}

/// Whether this daemon has outlived the thing it exists to serve.
///
/// `exe` is passed in rather than read here - always
/// `std::env::current_exe()` in production, and a scripted value in the tests
/// below, which is the only way to exercise the "cannot tell" branch without a
/// platform where `current_exe` actually fails.
///
/// The root is judged before the executable because it is the stronger and the
/// commoner fact: a deleted checkout takes its `target/` with it, so both arms
/// are true at once and the one worth putting in the log is the project.
///
/// A `current_exe()` that *failed* is not evidence of anything and never ends
/// the process - the same reading `shim::incumbent` gives a build stamp it
/// cannot compute: degrade to the behaviour from before the check existed
/// rather than act on a guess.
pub fn orphan_check(project_root: &Path, exe: io::Result<PathBuf>) -> Option<Orphaned> {
    if is_definitely_gone(project_root) {
        return Some(Orphaned::ProjectRootGone(project_root.to_path_buf()));
    }
    let exe = exe.ok()?;
    if is_definitely_gone(&exe) {
        return Some(Orphaned::ExecutableGone(exe));
    }
    None
}

/// `true` only for a path the filesystem positively reports as absent.
///
/// Deliberately not `Path::exists()`, which answers `false` for *every* way a
/// stat can fail - a parent directory this user may not traverse, an I/O
/// error, a network mount that timed out - and the decision on the other end
/// of this one ends a process. `NotFound` is the only answer that means what
/// the caller is asking; everything else means "could not tell", and this
/// returns `false` for all of it, which lands the daemon back on the behaviour
/// it had before this check existed.
///
/// `fs::metadata` rather than `symlink_metadata` on purpose: the question is
/// whether a file is still *reachable* at this path, and an installed
/// `g-mesh` whose symlink now dangles is as gone as a deleted one.
fn is_definitely_gone(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(_) => false,
        Err(err) => err.kind() == io::ErrorKind::NotFound,
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

/// The daemon's main thread once startup is over: runs both idle timers, and
/// [`orphan_check`] beside them, until the accept loop stops, the core's own
/// timeout says it is time to go, or there is nothing left to serve.
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
    project_root: &Path,
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

        // First in the tick, ahead of both timers (GM-320): an orphaned daemon
        // has nothing left to time, and no reason to pay for a whole-system
        // `sysinfo` scan on its way out. Two `stat`s, and an exit only on an
        // absence the filesystem positively reported - see [`orphan_check`]
        // and this module's "A fourth thing on that same tick" section. This
        // is what bounds an orphan's life at one tick rather than at
        // `coreIdleTimeoutHours`.
        if let Some(orphan) = orphan_check(project_root, std::env::current_exe()) {
            eprintln!(
                "g-mesh daemon: {orphan} - shutting down; nothing can ask this daemon for \
                 anything again, and a fresh one will be started if the project comes back"
            );
            // The same teardown the idle exit below performs, and for the same
            // reason it is a `return` rather than a `std::process::exit`: the
            // watcher thread may be mid-commit, and `sleep_all_now` cannot
            // come back until that transaction is durable.
            registry.sleep_all_now("the core has been orphaned and is shutting down");
            release_state_files(state_dir);
            return Ok(());
        }

        registry.sleep_if_idle_all();
        // `[plugin] memoryLimitMb` (task GM-274): the second thing this same
        // tick checks, right alongside idle-sleep - see this module's own
        // "A third thing riding the plugin's idle-check tick" doc comment.
        // With `memoryLimitMb` unset for every active supervisor this costs a
        // `Vec` walk and nothing more (`PluginRegistry::check_memory_limits_all`'s
        // own doc comment) - the acceptance criterion this call makes true at
        // the daemon's real tick, not just in a unit test that calls
        // `check_memory_limit` directly.
        registry.check_memory_limits_all();

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
    // D13 in `docs/architecture/lazy-indexing.md`: the phase file exists only
    // while a daemon is actually publishing it - removed here so an outside
    // reader (`cli::status`, `common::wait_until_phase`) never mistakes a
    // stale word left by a daemon that has since exited for a live one's
    // current phase.
    let _ = fs::remove_file(super::phase_path_in(state_dir));
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
mod tests;
