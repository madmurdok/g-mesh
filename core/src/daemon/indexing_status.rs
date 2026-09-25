//! Whether the daemon's index is ready to answer, and how much of "ready" a
//! given tool call needs. Design: `docs/architecture/lazy-indexing.md` (D2
//! activation, D3 phases, D13 the phase file).
//!
//! The socket is bound before any walk, so readiness is enforced here, at the
//! response layer: a tool call is never answered "not ready" or off a partial
//! graph; it waits ([`IndexingStatus::wait_for`]) for the phase it needs. The
//! handshake and `tools/list` check [`IndexingStatus::phase`], a lock-free
//! read, before taking the index store lock. Nothing moves until the first
//! index-needing call runs [`IndexingStatus::request_activation`].
//!
//! Only the bulk walk and the embedding backfill move the phase, never an
//! incremental watcher edit (the phase is project-wide; why:
//! `docs/architecture/g-mesh-v1.md`, "Ideas surfaced while comparing kungfu").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

/// One step of the cold-start machine a project's index moves through, once,
/// left to right. The one step backwards: [`Failed`](Phase::Failed) returns to
/// [`Walking`](Phase::Walking) when the next tool call asks again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// No walk has run or is running: the startup phase of a daemon whose
    /// project owes its walk, until the first index-needing tool call asks.
    Unindexed,
    /// The structural walk is running. No tool call may answer off the graph
    /// yet: it may have nodes with no edges linked, or no rows at all.
    Walking,
    /// The walk is done and linked; structural tools ([`Need::Structural`])
    /// answer. Also covers "embedding backfill owed but not started yet".
    Structural,
    /// The embedding backfill pass is running. Structural tools still answer;
    /// `search_code` ([`Need::Embeddings`]) waits.
    Embedding,
    /// The backfill pass is done (or found no model to embed with). Everything
    /// answers.
    Ready,
    /// The walk failed; carries the message so a waiter reports why (every
    /// waiter turns it into a tool error). Not terminal: the next
    /// [`IndexingStatus::request_activation`] retries.
    Failed(String),
}

impl Phase {
    /// The word published to the `index.phase` file: lowercase and one word, so
    /// readers compare it to a string literal. `Failed`'s message is left out;
    /// it goes to the daemon log.
    fn word(&self) -> &'static str {
        match self {
            Phase::Unindexed => "unindexed",
            Phase::Walking => "walking",
            Phase::Structural => "structural",
            Phase::Embedding => "embedding",
            Phase::Ready => "ready",
            Phase::Failed(_) => "failed",
        }
    }
}

/// What a tool call needs from the index before it may read it: the seven
/// structural tools need [`Need::Structural`], `search_code` alone
/// [`Need::Embeddings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Satisfied by [`Phase::Structural`], [`Phase::Embedding`] or
    /// [`Phase::Ready`]: the walk is done and linked.
    Structural,
    /// Satisfied only by [`Phase::Ready`] - every embeddable node the walk
    /// found has had its chance to be embedded.
    Embeddings,
}

impl Need {
    fn satisfied_by(self, phase: &Phase) -> bool {
        match self {
            Need::Structural => matches!(phase, Phase::Structural | Phase::Embedding | Phase::Ready),
            Need::Embeddings => matches!(phase, Phase::Ready),
        }
    }
}

/// What [`IndexingStatus::wait_for`] resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The phase this call needed was reached.
    Satisfied,
    /// The walk failed ([`Phase::Failed`]) first; carries the same message.
    Failed(String),
    /// The deadline, if one was given, passed first.
    TimedOut,
}

const UNINDEXED: u8 = 0;
const WALKING: u8 = 1;
const STRUCTURAL: u8 = 2;
const EMBEDDING: u8 = 3;
const READY: u8 = 4;
const FAILED: u8 = 5;

/// Shared cold-start phase state: a lock-free atomic for the phase, a `Notify`
/// to wait on, and small locked state that does not fit in one atomic.
///
/// The phase must stay a bare atomic: a handler reads it while the walk holds
/// the index store for a batch commit. Waiters are async tool handlers, so
/// they wait on `Notify` (suspends the task), never a `Condvar` (blocks a worker).
#[derive(Clone)]
pub struct IndexingStatus(Arc<Inner>);

struct Inner {
    phase: AtomicU8,
    /// The message [`Phase::Failed`] carries; meaningful only while `phase`
    /// reads [`FAILED`].
    failure: Mutex<Option<String>>,
    /// Fired on every phase transition, so a task parked in
    /// [`wait_for`](IndexingStatus::wait_for) wakes instead of polling.
    notify: Notify,
    /// When the current phase was entered.
    phase_since: Mutex<Instant>,
    // Progress counters: written by the walk and the embedding backfill, read
    // only for display. Relaxed throughout: each is an independent display value
    // and nothing decides anything off a combination of them.
    items_ingested: std::sync::atomic::AtomicU64,
    languages_done: std::sync::atomic::AtomicU32,
    languages_total: std::sync::atomic::AtomicU32,
    current_language: Mutex<Option<String>>,
    embed_done: std::sync::atomic::AtomicU64,
    embed_total: std::sync::atomic::AtomicU64,
    semantic_done: std::sync::atomic::AtomicU32,
    semantic_total: std::sync::atomic::AtomicU32,
    current_semantic_language: Mutex<Option<String>>,
    /// Where the counters are published for outside readers, and when they last
    /// were. Unset until [`IndexingStatus::attach_progress_file`].
    progress_file: Mutex<ProgressFile>,
    /// Lazy activation's trigger. A `Mutex`, not an atomic flag: "already
    /// requested" and "record a failure, allow a retry" change together
    /// ([`activation_failed`](IndexingStatus::activation_failed)).
    activation: Mutex<Activation>,
    /// Where [`Phase`] transitions are published for outside readers
    /// (`cli::status`, tests). Set only by `daemon::run`
    /// ([`IndexingStatus::attach_phase_file`]); a status with no daemon behind it
    /// writes nothing.
    phase_file: Mutex<Option<PathBuf>>,
}

/// The attached progress file and the time of its last write, under one lock
/// so two counters updated at once never both decide to write.
#[derive(Default)]
struct ProgressFile {
    path: Option<PathBuf>,
    interval: Duration,
    last_write: Option<Instant>,
}

/// The least time between two progress-file writes caused by counter
/// updates. Phase transitions and stage boundaries write regardless.
const PROGRESS_WRITE_INTERVAL: Duration = Duration::from_millis(500);

/// The progress counters as `daemon::progress_path_in`'s file holds them -
/// what `cli::status` renders per stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSnapshot {
    /// The daemon that wrote this. A reader treats the snapshot as live only
    /// while this is the pid of the daemon serving the project right now.
    pub pid: u32,
    /// Milliseconds since the Unix epoch at the time of the write.
    pub updated_at_ms: u64,
    /// The phase word at the time of the write (same words as `index.phase`).
    pub phase: String,
    pub walk: WalkProgress,
    pub semantic: SemanticProgress,
    pub embeddings: EmbedProgress,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalkProgress {
    pub languages_done: u32,
    pub languages_total: u32,
    pub current_language: Option<String>,
    /// Nodes and edges ingested so far.
    pub items: u64,
}

/// The whole-project semantic pass, one language at a time.
/// `current_language` is set only while a language's pass is running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticProgress {
    pub languages_done: u32,
    pub languages_total: u32,
    pub current_language: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbedProgress {
    pub done: u64,
    pub total: u64,
}

/// The sending half of `daemon::activation`'s trigger channel, and whether
/// the activation it wakes has already been asked for.
#[derive(Default)]
struct Activation {
    /// Set only by [`IndexingStatus::attach_activation`] (`daemon::run`); without
    /// it (CLI in-process walks, unit tests) nothing is ever sent.
    trigger: Option<mpsc::Sender<()>>,
    /// Set by the first [`IndexingStatus::request_activation`] and cleared only by
    /// [`IndexingStatus::activation_failed`]; after a successful activation it
    /// stays set for the rest of the process.
    requested: bool,
}

impl IndexingStatus {
    fn starting_at(phase: u8) -> Self {
        Self(Arc::new(Inner {
            phase: AtomicU8::new(phase),
            failure: Mutex::new(None),
            notify: Notify::new(),
            phase_since: Mutex::new(Instant::now()),
            items_ingested: std::sync::atomic::AtomicU64::new(0),
            languages_done: std::sync::atomic::AtomicU32::new(0),
            languages_total: std::sync::atomic::AtomicU32::new(0),
            current_language: Mutex::new(None),
            embed_done: std::sync::atomic::AtomicU64::new(0),
            embed_total: std::sync::atomic::AtomicU64::new(0),
            semantic_done: std::sync::atomic::AtomicU32::new(0),
            semantic_total: std::sync::atomic::AtomicU32::new(0),
            current_semantic_language: Mutex::new(None),
            progress_file: Mutex::new(ProgressFile::default()),
            activation: Mutex::new(Activation::default()),
            phase_file: Mutex::new(None),
        }))
    }

    /// A daemon that owes its project a walk and has not been asked for it yet.
    /// Every [`Need`] is unsatisfied until a tool call runs
    /// [`request_activation`](Self::request_activation) and the walk finishes.
    pub fn unindexed() -> Self {
        Self::starting_at(UNINDEXED)
    }

    /// A status whose walk is under way; every [`Need`] is unsatisfied until it
    /// reaches [`Phase::Structural`]. Used by tests; the daemon starts at
    /// [`unindexed`](Self::unindexed).
    pub fn walking() -> Self {
        Self::starting_at(WALKING)
    }

    /// A daemon whose walk was complete when it started (every restart of an
    /// already-walked project). [`Need::Structural`] is satisfied at once;
    /// [`Need::Embeddings`] waits for the embedding backfill the first tool call
    /// starts.
    pub fn structural() -> Self {
        Self::starting_at(STRUCTURAL)
    }

    /// Moves to `phase` and wakes every waiter. Waiters loop, so a phase that
    /// advances several steps between two checks is never missed.
    ///
    /// `Release`, paired with `Acquire` in [`phase`](Self::phase): a reader that
    /// observes the new phase sees everything written before this call (the
    /// walk's committed rows, the backfill's vectors).
    pub fn set_phase(&self, phase: Phase) {
        let discriminant = match &phase {
            Phase::Unindexed => UNINDEXED,
            Phase::Walking => WALKING,
            Phase::Structural => STRUCTURAL,
            Phase::Embedding => EMBEDDING,
            Phase::Ready => READY,
            Phase::Failed(message) => {
                *self.0.failure.lock().unwrap() = Some(message.clone());
                FAILED
            }
        };
        self.0.phase.store(discriminant, Ordering::Release);
        *self.0.phase_since.lock().unwrap() = Instant::now();
        self.0.notify.notify_waiters();
        self.publish_phase_file(phase.word());
        self.publish_progress(true);
    }

    /// Writes `word` to the attached phase file, if any. Best effort: a reader
    /// that finds nothing treats it as "nothing recorded".
    fn publish_phase_file(&self, word: &str) {
        let guard = self.0.phase_file.lock().unwrap();
        if let Some(path) = guard.as_ref() {
            super::write_state_file_atomic(path, word, "phase file");
        }
    }

    /// The current phase, with [`Phase::Failed`]'s message. Lock-free except in
    /// the `Failed` case.
    pub fn phase(&self) -> Phase {
        match self.0.phase.load(Ordering::Acquire) {
            UNINDEXED => Phase::Unindexed,
            WALKING => Phase::Walking,
            STRUCTURAL => Phase::Structural,
            EMBEDDING => Phase::Embedding,
            READY => Phase::Ready,
            FAILED => Phase::Failed(self.0.failure.lock().unwrap().clone().unwrap_or_default()),
            other => unreachable!("indexing status phase discriminant out of range: {other}"),
        }
    }

    /// When the current phase was entered.
    pub fn phase_since(&self) -> Instant {
        *self.0.phase_since.lock().unwrap()
    }

    /// Waits for `need`, a failed walk, or `deadline` (if given). `notified()` is
    /// created before the phase check in every iteration: `notify_waiters` wakes
    /// only tasks already registered, so checking first would lose a transition
    /// landing in between. The loop re-checks the current phase because it may
    /// move several steps before this task runs again.
    pub async fn wait_for(&self, need: Need, deadline: Option<Instant>) -> WaitOutcome {
        loop {
            let notified = self.0.notify.notified();
            match self.phase() {
                Phase::Failed(message) => return WaitOutcome::Failed(message),
                phase if need.satisfied_by(&phase) => return WaitOutcome::Satisfied,
                _ => {}
            }

            match deadline {
                None => notified.await,
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining == Duration::ZERO {
                        return WaitOutcome::TimedOut;
                    }
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return WaitOutcome::TimedOut;
                    }
                }
            }
        }
    }

    /// Connects this status to `daemon::activation`'s parked thread. Called once,
    /// by `daemon::run`, before any session can see this status, so no tool call
    /// finds it unattached in a daemon.
    pub fn attach_activation(&self) -> mpsc::Receiver<()> {
        let (trigger, triggered) = mpsc::channel();
        self.0.activation.lock().unwrap().trigger = Some(trigger);
        triggered
    }

    /// Connects this status to a phase file at `path` and publishes the current
    /// phase at once. Called once, by `daemon::run`, right after the pid file is
    /// written, so the file exists from the start; every later
    /// [`set_phase`](Self::set_phase) publishes too.
    pub fn attach_phase_file(&self, path: PathBuf) {
        *self.0.phase_file.lock().unwrap() = Some(path);
        self.publish_phase_file(self.phase().word());
    }

    /// Connects this status to a progress file at `path` and publishes at once, so
    /// a file left by an earlier daemon is replaced by one carrying this pid.
    pub fn attach_progress_file(&self, path: PathBuf) {
        self.attach_progress_file_every(path, PROGRESS_WRITE_INTERVAL);
    }

    /// [`attach_progress_file`](Self::attach_progress_file) with `interval`
    /// in place of [`PROGRESS_WRITE_INTERVAL`] between throttled writes.
    pub(crate) fn attach_progress_file_every(&self, path: PathBuf, interval: Duration) {
        {
            let mut file = self.0.progress_file.lock().unwrap();
            file.path = Some(path);
            file.interval = interval;
        }
        self.publish_progress(true);
    }

    /// The counters as they stand now.
    pub fn progress_snapshot(&self) -> ProgressSnapshot {
        let (languages_done, languages_total, current_language) = self.language_progress();
        let (done, total) = self.embed_progress();
        let updated_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        ProgressSnapshot {
            pid: std::process::id(),
            updated_at_ms,
            phase: self.phase().word().to_string(),
            walk: WalkProgress {
                languages_done,
                languages_total,
                current_language,
                items: self.items_ingested(),
            },
            semantic: SemanticProgress {
                languages_done: self.0.semantic_done.load(Ordering::Relaxed),
                languages_total: self.0.semantic_total.load(Ordering::Relaxed),
                current_language: self.0.current_semantic_language.lock().unwrap().clone(),
            },
            embeddings: EmbedProgress { done, total },
        }
    }

    /// Writes [`progress_snapshot`](Self::progress_snapshot) to the attached file,
    /// if any. Unless `force`d, skipped within the attached interval of the last
    /// write, so a throttled counter's final value reaches disk only with the
    /// next forced write (every phase transition is one).
    fn publish_progress(&self, force: bool) {
        let mut file = self.0.progress_file.lock().unwrap();
        let Some(path) = file.path.clone() else { return };
        let now = Instant::now();
        if !force && file.last_write.is_some_and(|last| now.duration_since(last) < file.interval) {
            return;
        }
        match serde_json::to_string(&self.progress_snapshot()) {
            Ok(json) => super::write_state_file_atomic(&path, &json, "progress file"),
            Err(err) => eprintln!("g-mesh daemon: failed to serialize indexing progress: {err}"),
        }
        file.last_write = Some(now);
    }

    /// Asks the activation thread for whatever this project still owes (D2).
    /// Called at the top of every tool handler's `prepare`; returns `true` only
    /// for the call that sent the trigger. Check-and-set under one lock, so
    /// concurrent sessions start it exactly once; the activation does not stop
    /// when the caller is cancelled. A call that (re)starts a walk moves the phase
    /// to [`Phase::Walking`] here, synchronously, so its own wait does not read
    /// the previous attempt's `Failed`.
    pub fn request_activation(&self) -> bool {
        let mut activation = self.0.activation.lock().unwrap();
        if activation.requested {
            return false;
        }
        if activation.trigger.is_none() {
            return false;
        }
        activation.requested = true;
        if matches!(self.phase(), Phase::Unindexed | Phase::Failed(_)) {
            self.set_phase(Phase::Walking);
        }
        // A send only fails once the activation thread has returned, which
        // it does only after a successful activation - and then `requested`
        // is never cleared again, so this line is not reached.
        if let Some(trigger) = &activation.trigger {
            let _ = trigger.send(());
        }
        true
    }

    /// Records a failed activation as [`Phase::Failed`] and re-arms
    /// [`request_activation`](Self::request_activation), both under the activation
    /// lock, so a waiter told `Failed` can always trigger a retry.
    pub fn activation_failed(&self, message: String) {
        let mut activation = self.0.activation.lock().unwrap();
        self.set_phase(Phase::Failed(message));
        activation.requested = false;
    }

    // --- Progress counters - see `Inner`'s own comment on them. ---

    /// Resets the walk's counters. Called at the start of every walk: a retried
    /// walk reuses this status and must not inherit the failed attempt's counts.
    pub fn start_walk_progress(&self, languages_total: u32) {
        self.0.items_ingested.store(0, Ordering::Relaxed);
        self.0.languages_done.store(0, Ordering::Relaxed);
        self.0.languages_total.store(languages_total, Ordering::Relaxed);
        *self.0.current_language.lock().unwrap() = None;
        self.publish_progress(true);
    }

    pub fn add_items_ingested(&self, count: u64) {
        self.0.items_ingested.fetch_add(count, Ordering::Relaxed);
        self.publish_progress(false);
    }

    pub fn items_ingested(&self) -> u64 {
        self.0.items_ingested.load(Ordering::Relaxed)
    }

    pub fn mark_language_started(&self, language: &str) {
        *self.0.current_language.lock().unwrap() = Some(language.to_string());
        self.publish_progress(true);
    }

    pub fn mark_language_done(&self) {
        self.0.languages_done.fetch_add(1, Ordering::Relaxed);
        self.publish_progress(true);
    }

    pub fn language_progress(&self) -> (u32, u32, Option<String>) {
        (
            self.0.languages_done.load(Ordering::Relaxed),
            self.0.languages_total.load(Ordering::Relaxed),
            self.0.current_language.lock().unwrap().clone(),
        )
    }

    /// Starts a backfill pass's progress: `done` restarts at zero, so a later
    /// pass never reports the counts of an earlier one.
    pub fn set_embed_total(&self, total: u64) {
        self.0.embed_done.store(0, Ordering::Relaxed);
        self.0.embed_total.store(total, Ordering::Relaxed);
        self.publish_progress(true);
    }

    pub fn add_embed_done(&self, count: u64) {
        self.0.embed_done.fetch_add(count, Ordering::Relaxed);
        self.publish_progress(false);
    }

    pub fn embed_progress(&self) -> (u64, u64) {
        (self.0.embed_done.load(Ordering::Relaxed), self.0.embed_total.load(Ordering::Relaxed))
    }

    /// Starts a whole-project semantic pass over `languages_total` languages.
    pub fn start_semantic_progress(&self, languages_total: u32) {
        self.0.semantic_done.store(0, Ordering::Relaxed);
        self.0.semantic_total.store(languages_total, Ordering::Relaxed);
        *self.0.current_semantic_language.lock().unwrap() = None;
        self.publish_progress(true);
    }

    pub fn mark_semantic_language_started(&self, language: &str) {
        *self.0.current_semantic_language.lock().unwrap() = Some(language.to_string());
        self.publish_progress(true);
    }

    /// Counts the running language's pass as over, whatever its outcome.
    pub fn mark_semantic_language_done(&self) {
        self.0.semantic_done.fetch_add(1, Ordering::Relaxed);
        *self.0.current_semantic_language.lock().unwrap() = None;
        self.publish_progress(true);
    }

    /// The counters rendered for a person, prefixed with the project root: what a
    /// waiting tool call puts in its progress notifications and its "still
    /// indexing" error (D6, D7). Only real numbers are shown: the file total is
    /// unknown while walking, and linking has no counter.
    pub fn progress_message(&self, root: &Path) -> String {
        format!("indexing {}: {}", root.display(), self.progress_detail())
    }

    /// [`progress_message`](Self::progress_message) without the root prefix.
    pub fn progress_detail(&self) -> String {
        match self.phase() {
            Phase::Unindexed => "starting".to_string(),
            Phase::Walking => {
                let (done, total, current) = self.language_progress();
                let items = group_thousands(self.items_ingested());
                if total > 0 && done >= total {
                    format!("linking imports and symbols ({items} nodes and edges walked)")
                } else if total > 0 {
                    let current = current.unwrap_or_else(|| "the first language".to_string());
                    format!(
                        "walking {current} ({done}/{total} languages done), {items} nodes and edges so far"
                    )
                } else {
                    format!("walking, {items} nodes and edges so far")
                }
            }
            Phase::Structural => "structural index built, embedding pass not started yet".to_string(),
            Phase::Embedding => match self.embed_progress() {
                (_, 0) => "embeddings: counting what needs embedding".to_string(),
                (done, total) => format!("embeddings {}/{}", group_thousands(done), group_thousands(total)),
            },
            Phase::Ready => "ready".to_string(),
            Phase::Failed(message) => format!("failed: {message}"),
        }
    }
}

/// `48210` as `"48,210"`.
pub(crate) fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_that_owes_a_walk_starts_at_walking_and_not_yet_structural() {
        let status = IndexingStatus::walking();
        assert_eq!(status.phase(), Phase::Walking);
    }

    /// The fast path task 96/99 left intact: a restart against an
    /// already-walked index starts satisfying `Need::Structural` from its
    /// first instant, with no wait ever observed by a structural caller.
    /// GM-395 slice 2's lazy startup: a project that owes its walk sits at
    /// `Unindexed` until something asks, and nothing is satisfied there.
    #[tokio::test]
    async fn an_unindexed_project_satisfies_no_need() {
        let status = IndexingStatus::unindexed();
        assert_eq!(status.phase(), Phase::Unindexed);
        let deadline = Instant::now() + Duration::from_millis(20);
        assert_eq!(status.wait_for(Need::Structural, Some(deadline)).await, WaitOutcome::TimedOut);
    }

    /// With no activation thread attached (the CLI's in-process walks, unit
    /// tests), a request is a no-op rather than a phase change nothing will
    /// ever follow up on.
    #[test]
    fn a_request_with_no_activation_attached_changes_nothing() {
        let status = IndexingStatus::unindexed();
        assert!(!status.request_activation());
        assert_eq!(status.phase(), Phase::Unindexed);
    }

    /// The first request sends exactly one trigger and moves `Unindexed` to
    /// `Walking` synchronously; every later one is a no-op.
    #[test]
    fn only_the_first_request_triggers_and_it_moves_to_walking() {
        let status = IndexingStatus::unindexed();
        let triggered = status.attach_activation();

        assert!(status.request_activation());
        assert_eq!(status.phase(), Phase::Walking);
        assert!(!status.request_activation());
        assert!(!status.clone().request_activation());

        assert!(triggered.try_recv().is_ok());
        assert!(triggered.try_recv().is_err(), "a second request must not send a second trigger");
    }

    /// An already-walked project's request starts the owed background work
    /// but must not move a structural caller's phase backwards.
    #[test]
    fn a_request_on_a_walked_project_triggers_without_leaving_structural() {
        let status = IndexingStatus::structural();
        let triggered = status.attach_activation();

        assert!(status.request_activation());
        assert_eq!(status.phase(), Phase::Structural);
        assert!(triggered.try_recv().is_ok());
    }

    /// D2's retry: a failed activation re-arms the trigger, and the retrying
    /// request moves `Failed` back to `Walking` before it returns - so the
    /// caller's own wait never reads the previous attempt's failure.
    #[test]
    fn a_failed_activation_is_retried_by_the_next_request() {
        let status = IndexingStatus::unindexed();
        let triggered = status.attach_activation();
        assert!(status.request_activation());
        assert!(triggered.try_recv().is_ok());

        status.activation_failed("the plugin is missing".to_string());
        assert_eq!(status.phase(), Phase::Failed("the plugin is missing".to_string()));

        assert!(status.request_activation());
        assert_eq!(status.phase(), Phase::Walking);
        assert!(triggered.try_recv().is_ok());
        assert!(!status.request_activation());
    }

    /// Several sessions asking at once start the activation exactly once.
    #[test]
    fn concurrent_requests_trigger_exactly_once() {
        let status = IndexingStatus::unindexed();
        let triggered = status.attach_activation();
        let winners: usize = (0..16)
            .map(|_| {
                let status = status.clone();
                std::thread::spawn(move || status.request_activation())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum();
        assert_eq!(winners, 1);
        assert_eq!(triggered.try_iter().count(), 1);
    }

    #[test]
    fn a_project_with_a_complete_walk_starts_at_structural() {
        let status = IndexingStatus::structural();
        assert_eq!(status.phase(), Phase::Structural);
    }

    /// Every connection the accept loop serves holds its own clone, so a
    /// transition has to be visible through all of them at once.
    #[test]
    fn every_clone_sees_the_same_transition() {
        let status = IndexingStatus::walking();
        let seen_by_a_connection = status.clone();

        status.set_phase(Phase::Structural);

        assert_eq!(seen_by_a_connection.phase(), Phase::Structural);
    }

    /// `Need::Structural`'s own acceptance criterion: satisfied by
    /// `Structural` itself and by both phases after it, never by `Walking`.
    #[tokio::test]
    async fn wait_for_structural_resolves_at_structural_and_at_embedding() {
        let status = IndexingStatus::walking();
        status.set_phase(Phase::Structural);
        assert_eq!(status.wait_for(Need::Structural, None).await, WaitOutcome::Satisfied);

        let status = IndexingStatus::walking();
        status.set_phase(Phase::Embedding);
        assert_eq!(status.wait_for(Need::Structural, None).await, WaitOutcome::Satisfied);
    }

    /// `Need::Embeddings`'s own acceptance criterion: satisfied only by
    /// `Ready`, not by `Structural` or `Embedding` even though a structural
    /// caller is already happy at either of those.
    #[tokio::test]
    async fn wait_for_embeddings_resolves_only_at_ready() {
        let status = IndexingStatus::walking();
        status.set_phase(Phase::Structural);
        status.set_phase(Phase::Embedding);

        let marker = status.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.set_phase(Phase::Ready);
        });

        assert_eq!(status.wait_for(Need::Embeddings, None).await, WaitOutcome::Satisfied);
    }

    /// A transition straight to `Failed` resolves *both* kinds of wait, with
    /// the same message either way - a failed walk has no structural graph
    /// to offer either need.
    #[tokio::test]
    async fn a_failed_phase_resolves_both_needs_with_its_message() {
        let structural_status = IndexingStatus::walking();
        let marker = structural_status.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.set_phase(Phase::Failed("the plugin crashed".to_string()));
        });
        assert_eq!(
            structural_status.wait_for(Need::Structural, None).await,
            WaitOutcome::Failed("the plugin crashed".to_string())
        );

        let embeddings_status = IndexingStatus::walking();
        let marker = embeddings_status.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.set_phase(Phase::Failed("the plugin crashed".to_string()));
        });
        assert_eq!(
            embeddings_status.wait_for(Need::Embeddings, None).await,
            WaitOutcome::Failed("the plugin crashed".to_string())
        );
    }

    /// The huge-project case task 105 exists for, now expressed against a
    /// deadline rather than an unconditional wait: nothing ever satisfies the
    /// need, so the wait must give up once `deadline` passes rather than hang
    /// - the shape a future progress-notification loop (GM-395 slice 3) will
    /// poll this in.
    #[tokio::test]
    async fn a_deadline_returns_timed_out_once_it_passes_with_nothing_satisfied() {
        let status = IndexingStatus::walking();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert_eq!(status.wait_for(Need::Structural, Some(deadline)).await, WaitOutcome::TimedOut);
    }

    /// A `deadline` that has already passed by the time this is called must
    /// not be treated as "wait forever" - the `Duration::ZERO` fast path in
    /// [`IndexingStatus::wait_for`].
    #[tokio::test]
    async fn a_deadline_already_in_the_past_times_out_without_waiting() {
        let status = IndexingStatus::walking();
        let deadline = Instant::now() - Duration::from_millis(20);
        assert_eq!(status.wait_for(Need::Structural, Some(deadline)).await, WaitOutcome::TimedOut);
    }

    /// The race this type exists to absorb: a transition that lands while
    /// something is already inside `wait_for` must wake it rather than making
    /// it sit out the rest of the deadline.
    #[tokio::test]
    async fn wait_for_resolves_as_soon_as_the_phase_transitions_even_with_a_deadline() {
        let status = IndexingStatus::walking();
        let marker = status.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.set_phase(Phase::Structural);
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        assert_eq!(status.wait_for(Need::Structural, Some(deadline)).await, WaitOutcome::Satisfied);
    }

    #[test]
    fn the_progress_file_throttles_counter_updates_and_every_phase_change_flushes_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.progress");
        let read = || -> ProgressSnapshot {
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
        };
        let status = IndexingStatus::structural();
        status.attach_progress_file_every(path.clone(), Duration::from_secs(3600));
        assert_eq!(read().pid, std::process::id());
        assert_eq!(read().phase, "structural");

        status.set_phase(Phase::Embedding);
        status.set_embed_total(200);
        assert_eq!(read().embeddings, EmbedProgress { done: 0, total: 200 });
        status.add_embed_done(50);
        assert_eq!(read().embeddings.done, 0, "a counter update inside the interval must not write");

        status.set_phase(Phase::Ready);
        let snapshot = read();
        assert_eq!(snapshot.phase, "ready");
        assert_eq!(snapshot.embeddings, EmbedProgress { done: 50, total: 200 });
    }

    #[test]
    fn the_semantic_pass_counts_languages_and_names_only_the_running_one() {
        let status = IndexingStatus::structural();
        status.start_semantic_progress(2);
        status.mark_semantic_language_started("python");
        let running = status.progress_snapshot().semantic;
        assert_eq!(running.current_language.as_deref(), Some("python"));
        assert_eq!((running.languages_done, running.languages_total), (0, 2));
        status.mark_semantic_language_done();
        let between = status.progress_snapshot().semantic;
        assert_eq!(between.current_language, None);
        assert_eq!(between.languages_done, 1);
    }

    #[test]
    fn group_thousands_groups_by_three_from_the_right() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(48_210), "48,210");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    /// D6's message, stage by stage: the counters that exist are shown, and
    /// a restarted walk does not inherit the previous attempt's counts.
    #[test]
    fn progress_message_renders_the_real_counters_per_stage() {
        let root = Path::new("/abs/root");
        let status = IndexingStatus::walking();
        status.start_walk_progress(4);
        status.mark_language_done();
        status.mark_language_done();
        status.mark_language_started("rust");
        status.add_items_ingested(48_210);
        assert_eq!(
            status.progress_message(root),
            "indexing /abs/root: walking rust (2/4 languages done), 48,210 nodes and edges so far"
        );

        status.mark_language_done();
        status.mark_language_done();
        assert_eq!(status.progress_detail(), "linking imports and symbols (48,210 nodes and edges walked)");

        status.start_walk_progress(1);
        status.mark_language_started("typescript");
        assert_eq!(
            status.progress_detail(),
            "walking typescript (0/1 languages done), 0 nodes and edges so far"
        );

        status.set_phase(Phase::Embedding);
        assert_eq!(status.progress_detail(), "embeddings: counting what needs embedding");
        status.set_embed_total(40_113);
        status.add_embed_done(12_400);
        assert_eq!(status.progress_message(root), "indexing /abs/root: embeddings 12,400/40,113");
    }
}
