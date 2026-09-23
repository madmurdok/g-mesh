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
//! design this slice implements (the phase machine here; nothing about
//! *when* a walk starts becomes lazy until a later slice - this daemon still
//! starts every phase eagerly, at launch).
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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// One step of the cold-start machine a project's index moves through, once,
/// left to right - see this module's own "GM-395" doc section.
///
/// `Unindexed` is not produced by anything in this slice (a cold start begins
/// at [`Walking`](Phase::Walking) - see [`IndexingStatus::walking`]); it
/// exists in the enum now because a later slice's lazy activation needs a
/// phase for "nothing has ever asked this project to index itself yet",
/// distinct from "a walk is in progress".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// No walk has ever run, and none is running - reachable only once lazy
    /// activation exists (a later slice); nothing in this daemon produces it
    /// yet.
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
    /// can report *why* rather than just "never became ready". Nothing in
    /// this slice's eager startup path ever sets this - a failed cold-start
    /// walk is still fatal to the whole daemon (`daemon::run`'s `?` on
    /// `bulk_index::run`) rather than recorded here - but the phase exists
    /// now because a later slice's lazy activation runs the walk *after* the
    /// daemon (and its socket) already exist, where a walk failure has
    /// nowhere else to go but here.
    Failed(String),
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
    // --- Progress counters (D3 in docs/architecture/lazy-indexing.md). ---
    // Nothing in this slice consumes these yet - the progress notifications
    // that would read them are GM-395's slice 3 (D6) - but the type and its
    // setters exist now, as part of the phase machine's full API, so that
    // slice has somewhere to write and read rather than growing this struct
    // again later.
    items_ingested: std::sync::atomic::AtomicU64,
    languages_done: std::sync::atomic::AtomicU32,
    languages_total: std::sync::atomic::AtomicU32,
    current_language: Mutex<Option<String>>,
    embed_done: std::sync::atomic::AtomicU64,
    embed_total: std::sync::atomic::AtomicU64,
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
        }))
    }

    /// A daemon that owes its project a cold-start structural walk. Every
    /// [`Need`] reads as unsatisfied until the walk finishes and moves this
    /// to [`Phase::Structural`].
    pub fn walking() -> Self {
        Self::starting_at(WALKING)
    }

    /// A daemon whose structural walk was already complete when it started -
    /// every restart of an already-walked project, which is the
    /// overwhelmingly common case. [`Need::Structural`] is satisfied from the
    /// first instant; the embedding backfill pass this same startup runs
    /// (`embedding::backfill::run`) still has to move this on to
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

    // --- Progress counters - see `Inner`'s own doc comment on why these are
    // unused within this crate for now. ---

    pub fn add_items_ingested(&self, count: u64) {
        self.0.items_ingested.fetch_add(count, Ordering::Relaxed);
    }

    pub fn items_ingested(&self) -> u64 {
        self.0.items_ingested.load(Ordering::Relaxed)
    }

    pub fn set_languages_total(&self, total: u32) {
        self.0.languages_total.store(total, Ordering::Relaxed);
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

    pub fn set_embed_total(&self, total: u64) {
        self.0.embed_total.store(total, Ordering::Relaxed);
    }

    pub fn add_embed_done(&self, count: u64) {
        self.0.embed_done.fetch_add(count, Ordering::Relaxed);
    }

    pub fn embed_progress(&self) -> (u64, u64) {
        (self.0.embed_done.load(Ordering::Relaxed), self.0.embed_total.load(Ordering::Relaxed))
    }
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
}
