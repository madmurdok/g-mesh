//! The daemon's two idle timers, and the state each one owns. Decisions:
//! `docs/adr/0004-daemon-lifecycle.md`.
//!
//! - The **plugin** sleeps after `plugin.idleTimeoutMinutes` (default 1h)
//!   without work. While it is asleep, watcher events go into a dirty-file
//!   queue *unprocessed*; the next request that would read a stale graph wakes
//!   the plugin and replays exactly that queue, not a rescan of the project.
//! - The **core** (socket listener, SQLite handle, fs watcher) exits only on
//!   `g-mesh stop`, a reboot, or `daemon.coreIdleTimeoutHours` (default 24h)
//!   with no MCP traffic and no attached client.
//!
//! # Lock order
//!
//! The supervisor's lock is taken before the store, never under it: see
//! `storage::index_store`'s module doc.
//!
//! [`supervise`] wakes every `IdleTimeouts::tick` (30s with the defaults) and
//! runs, in order: [`orphan_check`] (an orphan exits within one tick, client
//! attached or not), each plugin's idle sleep, each plugin's `memoryLimitMb`
//! check (a breaker on a sustained overage, not a ceiling), then the core's
//! idle exit. The daemon installs no signal handler: `SIGTERM` kills it and
//! its plugin tree (`core/tests/daemon_sigterm.rs`).

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::config::ProjectConfig;
use crate::daemon::manifest::PluginManifest;
use crate::daemon::plugin::PluginProcess;
use crate::embedding::EmbeddingPipeline;
use crate::protocol::jsonrpc::is_timeout;
use crate::storage::index_store::{self, IndexStore};
use crate::watcher::staleness::{self, StalenessOutcome};

/// `plugin.idleTimeoutMinutes`'s default.
pub const DEFAULT_PLUGIN_IDLE: Duration = Duration::from_secs(60 * 60);

/// `daemon.coreIdleTimeoutHours`'s default.
pub const DEFAULT_CORE_IDLE: Duration = Duration::from_secs(24 * 60 * 60);

/// Overrides the plugin's idle timeout, in milliseconds, for the test suite;
/// real installs never set it. `0` disables the timer (the plugin never sleeps).
pub const PLUGIN_IDLE_ENV: &str = "G_MESH_PLUGIN_IDLE_MS";

/// The core's equivalent of [`PLUGIN_IDLE_ENV`]; `0` means never exit on idleness.
pub const CORE_IDLE_ENV: &str = "G_MESH_CORE_IDLE_MS";

/// Lower bound on the tick, so a very short timeout never becomes a spin loop.
const MIN_TICK: Duration = Duration::from_millis(50);
/// Upper bound on the tick, so shutdown conditions are noticed within 30s.
const MAX_TICK: Duration = Duration::from_secs(30);

/// Grace for a sleeping plugin to exit after its stdin closes, before it is signalled.
const PLUGIN_EXIT_GRACE: Duration = Duration::from_millis(500);

/// Both idle timeouts, resolved once at daemon startup. `None` means the timer
/// is off, which is also how a configured `0` reads.
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
    /// `config`'s timeouts (`ProjectConfig::default()`'s 60 / 24 equal this module's
    /// defaults), unless the test-only env overrides name something else.
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

    /// How often [`supervise`] wakes: a quarter of the shorter timeout, clamped
    /// to [`MIN_TICK`]..[`MAX_TICK`], so a timeout overshoots by at most a quarter.
    fn tick(&self) -> Duration {
        match [self.plugin, self.core].into_iter().flatten().min() {
            Some(shortest) => (shortest / 4).clamp(MIN_TICK, MAX_TICK),
            // Nothing to time, but the loop still has to notice a stopped accept loop.
            None => MAX_TICK,
        }
    }
}

/// `None` for a configured zero (timer off); the default for anything unparseable,
/// so a typo neither silently turns a timer off nor stops the daemon starting.
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

/// The files changed while the plugin was asleep, in first-seen order and
/// without repeats. Replay must follow edit order so cross-file links end up
/// as the last edit meant; a file saved many times is one reparse, since the
/// plugin diffs against the file on disk now.
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

    /// Distinct files queued; read without draining by the replay progress ticker.
    fn len(&self) -> usize {
        self.order.len()
    }

    fn drain(&mut self) -> Vec<String> {
        self.seen.clear();
        std::mem::take(&mut self.order)
    }
}

struct SupervisedPlugin {
    /// `None` while the plugin is asleep.
    process: Option<PluginProcess>,
    dirty: DirtyQueue,
}

/// Owns one language's plugin process across its sleep/wake cycles, and the
/// dirty-file queue that makes sleeping safe. "Is it awake" and "what is
/// queued" live under one lock and are read together, or a file change could
/// be both applied and queued, or neither.
pub struct PluginSupervisor {
    /// Canonicalized: what the plugin is spawned against.
    project_root: PathBuf,
    /// Set once and never replaced: every spawn (first start, the wakes in
    /// `replay_pending` and `ensure_fresh`) must produce the same plugin, or a
    /// project's files get reindexed with the wrong extractor.
    manifest: PluginManifest,
    /// Rewritten on every wake and removed on every sleep, so `cli::status` and
    /// `cli::stop` never read the pid of a plugin that deliberately exited.
    pid_file: PathBuf,
    idle_timeout: Option<Duration>,
    /// Shared with the cold-start bulk walk; construction does no I/O, and the
    /// first [`apply`](EmbeddingPipeline::apply) loads the model lazily.
    embedding: Arc<EmbeddingPipeline>,
    inner: Mutex<SupervisedPlugin>,
    last_activity: Mutex<Instant>,
    /// Mirrors `inner.dirty.is_empty()` so a tool call with nothing queued never
    /// waits behind a reparse holding `inner`. Set inside the lock that queues, so
    /// a stale `false` defers a replay but never loses one.
    pending: AtomicBool,
    /// `[plugin] memoryLimitMb`, resolved once at startup; `None` (default) is off.
    memory_limit_mb: Option<u64>,
    /// Set when [`check_memory_limit`](Self::check_memory_limit) suspends this
    /// language; never cleared for this supervisor's life (config is read once at
    /// startup), so only a restart unsuspends. In memory, not in the index.
    semantic_suspended: AtomicBool,
    /// Makes the "could not sample" log once per supervisor, not once per tick.
    sampling_unavailable_logged: AtomicBool,
}

impl PluginSupervisor {
    /// The supervised plugin. Taken before the store, never under it.
    fn inner(&self) -> MutexGuard<'_, SupervisedPlugin> {
        index_store::assert_not_held();
        self.inner.lock().unwrap()
    }

    /// Spawns `manifest`'s plugin and records its pid. The manifest is kept for
    /// every later spawn (a wake, a crash relaunch), so the plugin never changes.
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

    /// The manifest's `language`, already checked against the live handshake by
    /// `PluginProcess::spawn`.
    pub fn language(&self) -> &str {
        &self.manifest.language
    }

    /// This supervisor's manifest: always the one it was spawned with.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// The plugin's pid right now, or `None` while it is asleep. Changes across a
    /// crash relaunch and a sleep/wake cycle.
    pub fn pid(&self) -> Option<u32> {
        self.inner().process.as_ref().map(PluginProcess::pid)
    }

    /// Hands the running plugin a new round-trip budget; a no-op while asleep
    /// (the next wake reads the env override afresh).
    #[cfg(test)]
    pub(crate) fn set_round_trip_timeouts(&self, timeouts: crate::daemon::plugin::RoundTripTimeouts) {
        let mut inner = self.inner();
        if let Some(process) = inner.process.as_mut() {
            process.set_round_trip_timeouts(timeouts);
        }
    }

    /// Whether anything is queued for the next wake; cheap enough for every tool call.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// How many files are queued: a snapshot under the lock `replay_pending`
    /// holds, `0` once it has drained the queue.
    pub fn pending_len(&self) -> usize {
        self.inner().dirty.len()
    }

    /// The watcher thread's entry point: reindex `file_path` now if the plugin is
    /// awake, or queue it for the next wake. Failures are logged and dropped, so
    /// one bad file never takes the watcher thread down.
    pub fn file_changed(&self, conn: &IndexStore, file_path: String) {
        let mut inner = self.inner();
        let Some(process) = inner.process.as_ref() else {
            inner.dirty.push(file_path);
            self.pending.store(true, Ordering::SeqCst);
            return;
        };
        // Stamped before the round trip, so an idle check during a long reparse
        // measures from its start.
        self.touch();
        let retry_path = file_path.clone();
        if let Err(err) =
            process.apply_file_change(conn, file_path, &self.embedding, self.is_semantic_suspended())
        {
            if is_timeout(&err) {
                // A timed-out request is not replayed inline (the plugin may have been
                // mid-write, and it has already been killed and relaunched). Queue it
                // like a change seen while asleep, so the next `replay_pending` sends it
                // to the fresh process instead of dropping it.
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

    /// Brings the index up to date with everything queued while the plugin was
    /// asleep, waking it if needed. Returns how many queued files were replayed:
    /// `0`, without touching the plugin, when nothing is queued.
    pub fn replay_pending(&self, conn: &IndexStore) -> Result<usize> {
        let mut inner = self.inner();
        if inner.dirty.is_empty() {
            self.pending.store(false, Ordering::SeqCst);
            return Ok(0);
        }

        // Spawned before the queue is drained, so a plugin that fails to start
        // leaves the queue intact for the next request to retry.
        if inner.process.is_none() {
            let process = PluginProcess::spawn(&self.project_root, &self.manifest, self.pid_file.clone())
                .with_context(|| format!("failed to wake the {} plugin", self.manifest.language))?;
            super::write_pid_file(&self.pid_file, process.pid());
            inner.process = Some(process);
        }

        let queued = inner.dirty.drain();
        self.pending.store(false, Ordering::SeqCst);
        // Lists the paths, so the log tells a queue replay from a project rescan.
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
                // One unreadable file does not cost the rest of the queue its replay.
                Err(err) => {
                    eprintln!("g-mesh daemon: failed to replay queued change to {file_path}: {err:#}")
                }
            }
        }
        Ok(replayed)
    }

    /// Runs a whole-project semantic pass if the plugin is awake. Returns whether
    /// it ran; a sleeping plugin is left asleep. A suspended language answers
    /// `Ok(false)` first: core never sends `semanticPass` to a suspended
    /// language, and both `daemon::semantic` and `daemon::workspace_reindex` go
    /// through this gate. The timeout scales with `file_count`.
    pub fn semantic_pass(
        &self,
        conn: &IndexStore,
        file_paths: Vec<String>,
        file_count: usize,
    ) -> Result<bool> {
        if self.is_semantic_suspended() {
            return Ok(false);
        }
        let inner = self.inner();
        let Some(process) = inner.process.as_ref() else { return Ok(false) };
        self.touch();
        process.semantic_pass(conn, file_paths, file_count, &self.embedding)?;
        Ok(true)
    }

    /// Runs `f` under this supervisor's serialization lock, the one every plugin
    /// round trip here takes. A workspace reindex must be atomic against a file
    /// change, or a `fileChanged` diff committed between its delete and its
    /// re-walk is resurrected or wiped. `f` may use the store (this lock first,
    /// the store inside it) but must not take this lock again. `f` gets the live
    /// process if the plugin is awake; nothing here wakes a sleeping one.
    pub fn with_exclusive_access<T>(&self, f: impl FnOnce(Option<&PluginProcess>) -> T) -> T {
        let inner = self.inner();
        self.touch();
        f(inner.process.as_ref())
    }

    /// Synchronously reindexes `file_path` if it changed since it was last
    /// indexed: the safety net for a change the watcher never saw (made while the
    /// daemon was down, or dropped by the backend). The common case resolves off
    /// `indexed_files` alone, without the plugin lock; only a reindex wakes it.
    pub fn ensure_fresh(&self, conn: &IndexStore, file_path: &str) -> Result<StalenessOutcome> {
        if !conn.with(|conn| staleness::is_stale(conn, &self.project_root, file_path))? {
            return Ok(StalenessOutcome::AlreadyFresh);
        }

        let mut inner = self.inner();
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
        // Checked outside the lock so a busy plugin's tick never queues behind
        // the reparse keeping it busy...
        if self.idle_for() < timeout {
            return false;
        }
        let mut inner = self.inner();
        // ...and again inside it, because the round trip just waited on is
        // activity that calls the sleep off.
        if self.idle_for() < timeout {
            return false;
        }
        let Some(process) = inner.process.take() else { return false };
        self.put_to_sleep(process, &format!("idle for {timeout:?}"));
        true
    }

    /// Stops the plugin however idle it is: the core's way out, so the plugin is
    /// reaped deliberately rather than left to notice its parent's pipes closing.
    pub fn sleep_now(&self, reason: &str) {
        let mut inner = self.inner();
        let Some(process) = inner.process.take() else { return };
        self.put_to_sleep(process, reason);
    }

    /// Whether this language's semantic passes are suspended. Gates the per-file
    /// `semanticPass` round trip and [`semantic_pass`](Self::semantic_pass).
    pub fn is_semantic_suspended(&self) -> bool {
        self.semantic_suspended.load(Ordering::SeqCst)
    }

    /// Samples this plugin's process-tree RSS (`daemon::memory::process_tree_rss_mb`)
    /// and suspends the language on a confirmed overage. Early-outs, in order:
    ///
    /// - `memory_limit_mb` unset: returns before taking any lock or sampling.
    /// - The plugin is asleep (idle, or already suspended).
    /// - A sample answers `None`: no evidence; logged once per supervisor.
    /// - Measured at or under the limit.
    /// - Over the limit once: suspends only if an immediate second sample is over
    ///   too. Suspension is irreversible for this daemon's life, and one reading of
    ///   an unstable process tree is not evidence of a sustained overage.
    ///
    /// Suspending sleeps the plugin with a reason naming the limit and the
    /// measured figures, sets `semantic_suspended`, and writes a
    /// `plugin-<language>.suspended` marker so `g-mesh status` can report it.
    pub fn check_memory_limit(&self) {
        self.check_memory_limit_sampled_by(crate::daemon::memory::process_tree_rss_mb);
    }

    /// [`check_memory_limit`](Self::check_memory_limit)'s body with the sampler
    /// as a parameter, so tests can script a sequence of readings.
    pub(crate) fn check_memory_limit_sampled_by(&self, sample: impl Fn(u32) -> Option<u64>) {
        let Some(limit_mb) = self.memory_limit_mb else { return };
        let mut inner = self.inner();
        let Some(process) = inner.process.as_ref() else { return };
        let pid = process.pid();

        let Some(measured_mb) = sample(pid) else {
            self.log_sampling_unavailable_once(pid);
            return;
        };
        if measured_mb <= limit_mb {
            return;
        }

        // Suspension is irreversible, so it waits for a second reading that agrees.
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

    /// The "no evidence either way" log, emitted once per supervisor.
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

    /// This language's suspension marker: next to the pid file and named the same
    /// way, so `cli::status`/`cli::stop` find it by listing the state directory.
    fn suspended_marker_path(&self) -> PathBuf {
        self.pid_file
            .parent()
            .expect("a pid file is always inside the project's state directory")
            .join(super::registry::plugin_suspended_marker_file_name(&self.manifest.language))
    }

    /// Best-effort write of the suspension marker, temp-then-rename like
    /// [`super::write_pid_file`] so a reader never sees a half-written one;
    /// `reason` is the only record of why a language was suspended.
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

    /// Shared tail of every sleep path. The caller has already taken the process
    /// out of `inner`, so this cannot fail.
    fn put_to_sleep(&self, process: PluginProcess, reason: &str) {
        let pid = process.pid();
        if let Err(err) = process.shutdown(PLUGIN_EXIT_GRACE) {
            eprintln!("g-mesh daemon: the plugin (pid {pid}) did not shut down cleanly: {err:#}");
        }
        // Removed: a pid file naming a deliberately exited process reads as a
        // crashed daemon to `cli::stop` and `cli::status`.
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

/// When the core last had anything to do, and how many clients are attached.
/// A live connection holds the core open and the idle clock starts only once
/// the last one goes away: a core that exits under a connected client takes
/// its tool surface with it, and the client cannot know to reconnect.
pub struct CoreActivity {
    last_request: Mutex<Instant>,
    live_connections: AtomicUsize,
}

impl CoreActivity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { last_request: Mutex::new(Instant::now()), live_connections: AtomicUsize::new(0) })
    }

    /// Records that the core just did something a user asked for; called per tool
    /// call rather than per connection, like `mcp::GMeshMcpServer::mark_used`.
    pub fn request(&self) {
        *self.last_request.lock().unwrap() = Instant::now();
    }

    /// Registers an accepted connection for as long as the returned guard lives.
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

/// A reason this daemon can never serve anyone again, as opposed to nobody
/// asking right now. Carries the path it judged so the log line names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Orphaned {
    /// The canonicalized project root, under which every path this daemon serves lives, is gone.
    ProjectRootGone(PathBuf),
    /// The executable this process was started from is gone, so no shim can
    /// compare its build (`daemon::build_stamp` reads that file's mtime).
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

/// Whether this daemon has outlived the thing it exists to serve. `exe` is
/// passed in (`std::env::current_exe()` in production) so tests can script
/// it. The root is judged first: a deleted checkout takes its `target/` with
/// it, and the project is the fact worth logging. A failed `current_exe()` is
/// not evidence and never ends the process.
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

/// `true` only for a path the filesystem positively reports as absent
/// (`NotFound`). Not `Path::exists()`, which is `false` for every stat failure
/// (permissions, I/O errors, a timed-out mount) and would end the process on a
/// guess. `fs::metadata` follows symlinks: a dangling one counts as gone.
fn is_definitely_gone(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(_) => false,
        Err(err) => err.kind() == io::ErrorKind::NotFound,
    }
}

/// Keeps the core alive for one connection's lifetime, and restarts the idle
/// clock when it ends: a disconnect is recent activity, not the start of silence.
pub struct ConnectionGuard(Arc<CoreActivity>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.live_connections.fetch_sub(1, Ordering::SeqCst);
        self.0.request();
    }
}

/// The daemon's main thread once startup is over: runs the tick (see the
/// module doc) for every language in the registry until the accept loop
/// stops, the core's idle timeout expires, or the daemon is orphaned.
/// `Ok(())` means the process should end; the next shim cold-starts a fresh
/// daemon. Every exit is a `return` after [`sleep_now`] on every plugin, never
/// a `std::process::exit`: the watcher thread holds the supervisor's lock for
/// a file change's whole round trip including its commit, so the sleep cannot
/// return until that transaction is durable.
///
/// [`sleep_now`]: PluginSupervisor::sleep_now
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
            // The accept loop only ever ends by failing; its error is the daemon's.
            Ok(result) => return result,
            Err(RecvTimeoutError::Disconnected) => bail!("the daemon's MCP accept loop panicked"),
            Err(RecvTimeoutError::Timeout) => {}
        }

        // First in the tick: an orphan has nothing left to time and no reason to
        // pay for a `sysinfo` scan on its way out.
        if let Some(orphan) = orphan_check(project_root, std::env::current_exe()) {
            eprintln!(
                "g-mesh daemon: {orphan} - shutting down; nothing can ask this daemon for \
                 anything again, and a fresh one will be started if the project comes back"
            );
            // The same teardown as the idle exit below.
            registry.sleep_all_now("the core has been orphaned and is shutting down");
            release_state_files(state_dir);
            return Ok(());
        }

        registry.sleep_if_idle_all();
        // A `Vec` walk and nothing more while `memoryLimitMb` is unset everywhere.
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
/// one. Unconditional, since this process owns every one of them. Failures
/// are ignored: a leftover file is cosmetic, and refusing to exit over one
/// would be worse. Plugin pid files are found by listing the state directory,
/// not from the registry's live supervisors.
fn release_state_files(state_dir: &Path) {
    let _ = fs::remove_file(super::pid_path_in(state_dir));
    for (_, pid_file) in super::registry::discovered_pid_files(state_dir) {
        let _ = fs::remove_file(pid_file);
    }
    let _ = fs::remove_file(super::build_stamp_path_in(state_dir));
    // The phase file exists only while a daemon publishes it, so no outside
    // reader mistakes a stale phase for a live one's (D13 in
    // `docs/architecture/lazy-indexing.md`).
    let _ = fs::remove_file(super::phase_path_in(state_dir));
    let _ = fs::remove_file(super::progress_path_in(state_dir));
    // Derived through the parent module so what a daemon binds and what it
    // releases are the same endpoint by construction. No-op on Windows.
    if let Some(endpoint) = super::endpoint_in(state_dir) {
        endpoint.clear_stale();
    }
}

#[cfg(test)]
mod tests;
