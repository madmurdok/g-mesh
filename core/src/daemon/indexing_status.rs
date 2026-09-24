//! Whether the daemon's index is ready to answer, and how much of "ready" a
//! given tool call actually needs - the one fact every MCP tool handler has
//! to consult before it reads the index.
//!
//! # Why this exists at all
//!
//! Until task 105 this fact needed no representation, because the daemon's
//! way of saying "the graph is not ready" was to be unreachable: `daemon::run`
//! bound its socket only once `bulk_index::run` had returned, so nobody could
//! ask a question there was no honest answer to. That enforced "never answer
//! off a half-built graph" at the transport layer, and it worked for as long
//! as a full walk was a once-per-project event.
//!
//! It stopped working when a full walk became a routine part of *upgrading*:
//! task 96 made a bumped `CURRENT_INDEXER_VERSION` wipe the index, and task 99
//! made a shim retire an outdated daemon and bootstrap a fresh one - so the
//! first MCP call after an upgrade now reliably lands on a daemon that owes
//! its project a cold walk. On a project big enough for that walk to outlast
//! `shim::BOOTSTRAP_TIMEOUT`, the shim gave up on a socket that was never
//! going to appear in time and the MCP client lost its tools outright. "No
//! tools at all, with a connection-timeout message" is a worse answer than
//! "not ready yet, ask again".
//!
//! So the guarantee moved from the transport layer to the response layer,
//! keeping its spirit and dropping its mechanism: the socket is bound before
//! the walk starts, and a caller who asks during the walk is *told* that the
//! graph is not ready instead of having its connection refused.
//!
//! # GM-394: no tool call is ever answered "not ready"
//!
//! Task 107 gave a call landing close to the walk's end a short grace wait
//! before refusing it - the right call for its own problem, but GM-394 found
//! a second, worse failure hiding behind it: `mcp::mod::GMeshMcpServer::
//! instructions` took the daemon's single SQLite mutex unconditionally, with
//! no indexing check at all, and that mutex is exactly what a bulk-index
//! batch commit holds for as long as its embedding inference takes - minutes,
//! on a project big enough to matter. `initialize` calls `get_info`, which
//! calls that method, so an MCP client's handshake blocked on a lock a short
//! grace wait was never going to help with.
//!
//! GM-394's fix is two-layered, and this type still carries both halves.
//! First, `get_info`/`instructions` check [`phase`](IndexingStatus::phase) -
//! a lock-free atomic read - *before* ever reaching for the mutex, so the
//! handshake and `tools/list` are answerable however long the walk's lock is
//! held for. Second, a tool call that genuinely needs the index never answers
//! "not ready" or partial - it waits for the index to actually reach the
//! phase it needs and serves the real thing
//! ([`wait_for`](IndexingStatus::wait_for)).
//!
//! # GM-395: two phases instead of one flag, so embeddings can lag structure
//!
//! Before GM-395 this type was a single "still walking" flag: a project was
//! either mid-walk or fully ready, and the walk itself computed every node's
//! embedding inline, batch by batch. That made a cold start on a
//! large project take as long as the slowest part of indexing it (embedding
//! inference), even for a caller who only ever asks structural questions
//! (`find_definition`, `find_references`, ...) and never touches
//! `search_code` at all.
//!
//! [`Phase`] splits "the walk is done" from "everything, including
//! embeddings, is done": a structural tool needs only [`Phase::Structural`]
//! (or later), so it stops waiting the moment the walk itself - now run with
//! `embedding: None`, see `daemon::bulk_index` - finishes linking, while
//! `search_code` alone needs [`Phase::Ready`] and waits out the embedding
//! backfill pass (`embedding::backfill::run`) that now runs as its own step
//! afterward. See `docs/architecture/lazy-indexing.md`'s D3 for the full
//! design this implements.
//!
//! # GM-395 slice 2: nothing moves until a tool call asks
//!
//! A daemon no longer starts its walk (or its embedding backfill pass) at
//! launch. It starts at [`Phase::Unindexed`] (or [`Phase::Structural`] for an
//! already-walked project) and stays there until the first index-needing
//! tool call runs [`request_activation`](IndexingStatus::request_activation),
//! which wakes `daemon::activation`'s parked thread exactly once. A walk that
//! fails lands in [`Phase::Failed`] instead of ending the process, and the
//! next call's `request_activation` retries it.
//!
//! # Why the incremental-edit watcher path does not re-arm this
//!
//! Task 111 asked the mirror question of 105/107's: does a query landing
//! between a file write and the watcher's `apply_file_change` commit
//! (`daemon::plugin::PluginProcess::apply_file_change`, driven by
//! `daemon::run`'s watcher thread) deserve the same honesty this type gives
//! a query landing during the cold-start walk? The answer settled on is no,
//! for reasons specific to this second window that do not hold for the
//! first:
//!
//! - **The window is bounded, and - since task 129 - deliberately so.**
//!   `daemon::run`'s watcher thread now debounces: raw events are recorded
//!   into a `watcher::debounce::Debouncer` and only routed to the plugin once
//!   a path has gone quiet for `daemon::DEBOUNCE_WINDOW` (300ms) - see
//!   `daemon::watch_and_route_once`. Before that task, the gap a query could
//!   land in was "OS file-watch event latency plus one reparse-and-commit
//!   round trip to the plugin," and it grew under a burst of near-simultaneous
//!   writes only because changes were applied one at a time, never because
//!   anything was waiting on purpose. That second half is no longer true: a
//!   deliberate wait is now exactly the point, trading a bounded amount of
//!   this staleness window for coalescing a burst's plugin round trips into
//!   one. What has not changed is that the wait is bounded and known - "OS
//!   latency plus up to one debounce window plus one round trip," not
//!   unbounded - which is what keeps the next bullet's argument (a query in
//!   this window reads stale-but-consistent data, never a torn graph) holding
//!   regardless of the window's exact width. `watcher::burst::BurstBatcher`
//!   is a different type, for a different problem, and is deliberately not
//!   wired in here at all - see `daemon::run`'s own comment on the watcher
//!   thread for why.
//! - **It cannot be answered with a torn or half-built graph.**
//!   `apply_file_change` holds the *same* `Arc<Mutex<Connection>>` every MCP
//!   handler locks to answer a query, for the entire reparse-plus-commit, and
//!   `storage::write::apply_diff` is one transaction. A query that arrives
//!   while a commit is in flight simply blocks on that mutex until it
//!   finishes and then reads the post-edit graph; only a query that arrives
//!   *before* the watcher thread has pulled the change off its channel reads
//!   pre-edit data - stale, but internally consistent. That is a strictly
//!   narrower failure mode than cold start's, where a query mid-walk can see
//!   nodes with no edges yet: a confidently *wrong* answer, not merely a
//!   delayed one.
//! - **Reusing this type's shape would widen the blast radius it is meant to
//!   narrow.** `IndexingStatus` is deliberately one project-wide phase -
//!   correct for the bulk walk, because the whole graph really is incomplete
//!   until it reaches [`Phase::Structural`]. A single incremental edit
//!   touches one file. Flipping the same project-wide phase around every
//!   watcher commit would make an unrelated query - about a file the edit
//!   never touched - pause on every save in a live-edited project, which
//!   trades a rare, narrow, internally-consistent staleness for a far more
//!   common false positive. Honestly closing this window would need a
//!   per-file signal, not a project-wide one - a different and larger
//!   mechanism than this type provides. `watcher::staleness::ensure_fresh`
//!   was written for close to that shape (an mtime/hash check before
//!   answering) but, per this investigation, is not currently called from
//!   any MCP handler - a real, separate gap worth its own task, not a reason
//!   to bend this one into a shape it does not fit.
//!
//! So a phase transition stays a once-only call from the bulk walk or the
//! embedding backfill pass. See `docs/architecture/g-mesh-v1.md`'s "Ideas
//! surfaced while comparing kungfu" subsection for the fuller writeup this
//! decision closes out.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// One step of the cold-start machine a project's index moves through, once,
/// left to right - see this module's own "GM-395" doc section.
///
/// A failed walk is the one step backwards: [`Failed`](Phase::Failed) goes
/// back to [`Walking`](Phase::Walking) when the next tool call asks again
/// ([`IndexingStatus::request_activation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// No walk has ever run, and none is running: the startup phase of a
    /// daemon whose project owes its walk, until the first index-needing
    /// tool call asks for it ([`IndexingStatus::request_activation`]).
    Unindexed,
    /// The structural walk is running (`daemon::bulk_index::run`, called
    /// with `embedding: None`). No tool call may answer off the graph yet -
    /// it may have nodes with no edges linked, or no rows at all.
    Walking,
    /// The walk is done and linked. Structural tools
    /// ([`Need::Structural`]) may answer. This phase also covers "the
    /// embedding backfill pass is owed but has not started running yet" -
    /// there is no separate phase for that, since a structural tool does not
    /// care either way.
    Structural,
    /// The embedding backfill pass (`embedding::backfill::run`) is running.
    /// Structural tools still answer; `search_code`
    /// ([`Need::Embeddings`]) waits.
    Embedding,
    /// The backfill pass is done - every embeddable node has a `vectors` row,
    /// or the pass determined none could be produced (no model available).
    /// Everything answers.
    Ready,
    /// The walk failed outright. Carries the failure's message so a waiter
    /// can report *why* rather than just "never became ready" - every waiter
    /// turns it into a tool error (`mcp::GMeshMcpServer::prepare`). Not
    /// terminal: the next [`IndexingStatus::request_activation`] moves it
    /// back to [`Walking`](Phase::Walking) and retries. Before GM-395's
    /// slice 2 a failed walk ended the daemon instead, which under a lazy
    /// trigger would drop the session of the very call that asked for it.
    Failed(String),
}

impl Phase {
    /// The word [`IndexingStatus::attach_phase_file`] and
    /// [`IndexingStatus::set_phase`] publish to the `index.phase` file
    /// (`daemon::phase_path_in`, D13 in `docs/architecture/lazy-indexing.md`),
    /// lowercase and one word, matching every other value that file can hold
    /// so `cli::status` and the test suite can compare it with a plain string
    /// literal rather than parsing one back into a [`Phase`]. `Failed`'s
    /// message is deliberately not included: the file is a status word for
    /// an outside reader, not a serialization of this type, and the message
    /// already has a home the file's own readers are pointed at instead (the
    /// daemon log - see `cli::status`'s rendering of this word).
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

/// What a tool call actually needs from the index before it may read it -
/// the caller-facing half of [`Phase`]. Two callers can be waiting on the
/// very same [`IndexingStatus`] and be satisfied at different moments: the
/// seven structural tools only ever need [`Need::Structural`], and
/// `search_code` alone needs [`Need::Embeddings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Satisfied by [`Phase::Structural`], [`Phase::Embedding`] or
    /// [`Phase::Ready`] - the walk is done and linked, whatever state the
    /// embedding backfill pass is in.
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
    /// The walk failed outright ([`Phase::Failed`]) before this need was
    /// ever satisfied - carries the same message.
    Failed(String),
    /// The deadline passed (only possible when [`IndexingStatus::wait_for`]
    /// was given one) before either of the above happened.
    TimedOut,
}

const UNINDEXED: u8 = 0;
const WALKING: u8 = 1;
const STRUCTURAL: u8 = 2;
const EMBEDDING: u8 = 3;
const READY: u8 = 4;
const FAILED: u8 = 5;

/// Shared cold-start phase state: a lock-free atomic for the phase every tool
/// handler checks, plus a wakeup a handler can wait on, plus the small amount
/// of mutable state ([`Phase::Failed`]'s message, the progress counters) that
/// does not fit in one atomic.
///
/// The phase stays a bare atomic rather than growing a `Mutex` around it -
/// the whole point is to answer *while* the walk holds the daemon's single
/// SQLite connection for a batch commit, and a handler that had to take a
/// mutex to find out whether it may take that mutex would queue behind
/// exactly the work it is trying not to wait for. The wakeup is a separate
/// `Notify` rather than a `Condvar` paired with the same atomic for the same
/// reason: every reader of this type is an async tool handler on the
/// daemon's own tokio runtime (`mcp::GMeshMcpServer`), and
/// `Notify::notified().await` suspends the calling task without holding a
/// worker thread, where a `Condvar::wait` would either block a worker
/// outright or need `spawn_blocking` - a whole borrowed thread - to wait out
/// what is, in the overwhelmingly common case, a handful of milliseconds.
#[derive(Clone)]
pub struct IndexingStatus(Arc<Inner>);

struct Inner {
    phase: AtomicU8,
    /// Only meaningful while `phase` reads [`FAILED`] - the message
    /// [`Phase::Failed`] carries. A plain `Mutex` rather than another atomic:
    /// this is written at most once per `IndexingStatus` (a walk fails at
    /// most once) and read rarely (only by a waiter that observes `FAILED`),
    /// so there is no hot path here to keep lock-free the way the phase
    /// itself has to be.
    failure: Mutex<Option<String>>,
    /// Fired on every phase transition, so a task already parked in
    /// [`wait_for`](IndexingStatus::wait_for) is woken instead of having to
    /// poll the atomic on a timer.
    notify: Notify,
    /// When the current phase was entered - for a future progress
    /// notification ("indexing for 42s") to report against; nothing in this
    /// slice reads it back yet.
    phase_since: Mutex<Instant>,
    // --- Progress counters (D3/D6 in docs/architecture/lazy-indexing.md). ---
    // Written by the walk (`daemon::bulk_index::run_with_progress`) and the
    // embedding backfill pass (`embedding::backfill::run`); read only by
    // [`IndexingStatus::progress_message`], which renders them for a waiting
    // tool call's progress notifications and its "still indexing" error.
    // Relaxed throughout: each is an independent display value, and nothing
    // decides anything off a combination of them.
    items_ingested: std::sync::atomic::AtomicU64,
    languages_done: std::sync::atomic::AtomicU32,
    languages_total: std::sync::atomic::AtomicU32,
    current_language: Mutex<Option<String>>,
    embed_done: std::sync::atomic::AtomicU64,
    embed_total: std::sync::atomic::AtomicU64,
    /// Lazy activation's trigger - see
    /// [`request_activation`](IndexingStatus::request_activation). A `Mutex`
    /// rather than an atomic flag because "is an activation already
    /// requested" and "record a failure, and allow the next retry" have to
    /// change together: see [`activation_failed`](IndexingStatus::activation_failed).
    activation: Mutex<Activation>,
    /// Where [`Phase`] transitions are published for outside readers
    /// (`cli::status`, the test suite's `wait_until_phase`) - D13 in
    /// `docs/architecture/lazy-indexing.md`. `None` until
    /// [`IndexingStatus::attach_phase_file`] is called - which only
    /// `daemon::run` does, exactly like [`Activation::trigger`] above - so a
    /// status with no daemon behind it (the CLI's in-process walks, unit
    /// tests) writes nothing.
    phase_file: Mutex<Option<PathBuf>>,
}

/// The sending half of `daemon::activation`'s trigger channel, and whether
/// the activation it wakes has already been asked for.
#[derive(Default)]
struct Activation {
    /// `None` until [`IndexingStatus::attach_activation`] is called - which
    /// only `daemon::run` does. A status with no activation thread behind it
    /// (the CLI's in-process walks, unit tests) never sends anything.
    trigger: Option<mpsc::Sender<()>>,
    /// Set by the first [`IndexingStatus::request_activation`] and cleared
    /// only by [`IndexingStatus::activation_failed`], so after a successful
    /// activation it stays set for the rest of the process: there is nothing
    /// left to trigger.
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
            activation: Mutex::new(Activation::default()),
            phase_file: Mutex::new(None),
        }))
    }

    /// A daemon that owes its project a walk and has not been asked for it
    /// yet - GM-395's lazy startup. Every [`Need`] reads as unsatisfied until
    /// a tool call runs [`request_activation`](Self::request_activation) and
    /// the walk it starts finishes.
    pub fn unindexed() -> Self {
        Self::starting_at(UNINDEXED)
    }

    /// A status whose structural walk is already under way. Every [`Need`]
    /// reads as unsatisfied until the walk finishes and moves this to
    /// [`Phase::Structural`]. The daemon itself starts at
    /// [`unindexed`](Self::unindexed) now; this remains for the unit tests
    /// that exercise the waits.
    pub fn walking() -> Self {
        Self::starting_at(WALKING)
    }

    /// A daemon whose structural walk was already complete when it started -
    /// every restart of an already-walked project, which is the
    /// overwhelmingly common case. [`Need::Structural`] is satisfied from the
    /// first instant; the embedding backfill pass the first tool call starts
    /// (`embedding::backfill::run`, via `daemon::activation`) still has to
    /// move this on to
    /// [`Phase::Embedding`] and then [`Phase::Ready`] before
    /// [`Need::Embeddings`] is - see this module's "GM-395" doc section for
    /// why an already-walked project is not simply started at `Ready`.
    pub fn structural() -> Self {
        Self::starting_at(STRUCTURAL)
    }

    /// Moves to `phase`, notifying every waiter. Not required to move
    /// forward one step at a time from the caller's point of view - a waiter
    /// loops (see [`wait_for`](Self::wait_for)) precisely so a phase that
    /// advances several steps between two checks is never missed - but every
    /// real caller in this crate does call this once per step, in order.
    ///
    /// `Release`, paired with `Acquire` in [`phase`](Self::phase): a reader
    /// that observes the new phase is guaranteed to see everything written
    /// before this call - the walk's committed rows for
    /// [`Phase::Structural`], the backfill's stored vectors for
    /// [`Phase::Ready`]. The `Notify` wakeup piggybacks on that same
    /// guarantee.
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
    }

    /// Writes `word` to the attached phase file, if one has been
    /// ([`attach_phase_file`](Self::attach_phase_file)) - a no-op otherwise,
    /// same as every other best-effort state-file write in this daemon
    /// (`daemon::write_pid_file`'s own doc comment gives the reasoning this
    /// borrows: a reader that finds nothing degrades to "nothing recorded",
    /// which every caller of `daemon::read_phase_in` already treats as a
    /// valid outcome).
    fn publish_phase_file(&self, word: &str) {
        let guard = self.0.phase_file.lock().unwrap();
        if let Some(path) = guard.as_ref() {
            super::write_state_file_atomic(path, word, "phase file");
        }
    }

    /// The current phase, including [`Phase::Failed`]'s message if that is
    /// where things stand. Lock-free except in the (rare, terminal) `Failed`
    /// case.
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

    /// Waits for `need` to be satisfied, or for the walk to fail, or - only
    /// if `deadline` is given - for time to run out. A call against a status
    /// that already satisfies `need` returns [`WaitOutcome::Satisfied`]
    /// immediately without ever touching the `Notify`.
    ///
    /// `notified()` is created *before* the phase check that follows, not
    /// after, in every iteration - the same lost-wakeup-safe pattern this
    /// type has always used: `notify_waiters` only wakes tasks that were
    /// already waiting, so if a phase transition ran between a naive phase
    /// check and a later call to `notified()`, the notification would already
    /// be gone and this future would sit out the whole wait despite the
    /// transition it wanted having already happened. Registering first closes
    /// that window.
    ///
    /// This loops, rather than checking once and then awaiting one
    /// notification, because a phase can move several steps between the
    /// moment this task is woken and the moment it gets to run again (e.g.
    /// `Structural` to `Embedding` to `Ready` while this task was merely
    /// descheduled) - each iteration re-checks against the *current* phase,
    /// not the one that triggered the wakeup.
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

    /// Connects this status to `daemon::activation`'s parked thread and
    /// returns the receiving end that thread waits on. Called once, by
    /// `daemon::run`, before the accept loop can hand this status to any
    /// session - so no tool call can ever find it unattached in a daemon.
    pub fn attach_activation(&self) -> mpsc::Receiver<()> {
        let (trigger, triggered) = mpsc::channel();
        self.0.activation.lock().unwrap().trigger = Some(trigger);
        triggered
    }

    /// Connects this status to a phase file at `path` (D13 in
    /// `docs/architecture/lazy-indexing.md`) and publishes the current phase
    /// to it immediately - called once, by `daemon::run`, right after the
    /// daemon's pid file is written, so "the phase file exists" is never
    /// transiently false the way `write_pid_file`'s own doc comment describes
    /// for a half-written file: this call's own write is what puts the
    /// *first* line in place, atomically, before anything can observe the
    /// file's absence as meaningful. Every [`set_phase`](Self::set_phase)
    /// after this call publishes too - see [`publish_phase_file`](Self::publish_phase_file).
    pub fn attach_phase_file(&self, path: PathBuf) {
        *self.0.phase_file.lock().unwrap() = Some(path);
        self.publish_phase_file(self.phase().word());
    }

    /// Asks the activation thread to do whatever this project still owes -
    /// the walk, the semantic pass (or its owed retry), and the embedding
    /// backfill pass (D2 in `docs/architecture/lazy-indexing.md`). Called at
    /// the top of every tool handler's `prepare`, so the first index-needing
    /// call starts it and every later call is a no-op. Returns `true` only
    /// for the one call that actually sent the trigger.
    ///
    /// Check-and-set under one lock, so concurrent calls from several
    /// sessions start it exactly once. The activation it starts is
    /// independent of the caller: a call that is cancelled, or whose session
    /// goes away, does not stop it.
    ///
    /// When this call is the one that (re)starts a walk - from
    /// [`Phase::Unindexed`], or from [`Phase::Failed`] on a retry - the phase
    /// moves to [`Phase::Walking`] *here*, synchronously, rather than when
    /// the activation thread gets round to it. Otherwise the caller's own
    /// wait that follows would read the previous attempt's `Failed` and
    /// report a failure the retry it just asked for has not had a chance to
    /// repeat or fix.
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
    /// [`request_activation`](Self::request_activation) so the next tool
    /// call retries. Both under the activation lock, so there is no moment
    /// in which a waiter has been told `Failed` but a new request would
    /// still be refused as "already requested".
    pub fn activation_failed(&self, message: String) {
        let mut activation = self.0.activation.lock().unwrap();
        self.set_phase(Phase::Failed(message));
        activation.requested = false;
    }

    // --- Progress counters - see `Inner`'s own comment on them. ---

    /// Resets the walk's counters for a walk over `languages_total`
    /// languages. Called at the start of every walk rather than relying on
    /// the counters' initial zeroes, because a failed walk is retried on the
    /// same status (`Phase::Failed`), and a retry that inherited the failed
    /// attempt's counts would report more languages done than exist.
    pub fn start_walk_progress(&self, languages_total: u32) {
        self.0.items_ingested.store(0, Ordering::Relaxed);
        self.0.languages_done.store(0, Ordering::Relaxed);
        self.0.languages_total.store(languages_total, Ordering::Relaxed);
        *self.0.current_language.lock().unwrap() = None;
    }

    pub fn add_items_ingested(&self, count: u64) {
        self.0.items_ingested.fetch_add(count, Ordering::Relaxed);
    }

    pub fn items_ingested(&self) -> u64 {
        self.0.items_ingested.load(Ordering::Relaxed)
    }

    pub fn mark_language_started(&self, language: &str) {
        *self.0.current_language.lock().unwrap() = Some(language.to_string());
    }

    pub fn mark_language_done(&self) {
        self.0.languages_done.fetch_add(1, Ordering::Relaxed);
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
    }

    pub fn add_embed_done(&self, count: u64) {
        self.0.embed_done.fetch_add(count, Ordering::Relaxed);
    }

    pub fn embed_progress(&self) -> (u64, u64) {
        (self.0.embed_done.load(Ordering::Relaxed), self.0.embed_total.load(Ordering::Relaxed))
    }

    /// The counters rendered for a person, prefixed with the project root:
    /// `"indexing /abs/root: walking rust (2/4 languages done), 48,210 nodes
    /// and edges so far"`. What a waiting tool call puts in each progress
    /// notification's `message` and in its "still indexing" error (D6, D7).
    ///
    /// Only the numbers that are real are shown: the file total is unknown
    /// while walking (the plugin enumerates files itself, and counting them
    /// first would be a second walk), and linking has no counter at all, so
    /// that stage is named rather than measured.
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
fn group_thousands(n: u64) -> String {
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
