//! Whether the daemon is still working through its cold-start bulk walk -
//! the one fact every MCP tool handler has to consult before it reads the
//! index.
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
//! graph is not ready instead of having its connection refused. What is not
//! given up is the part that matters - no caller is ever served a partial or
//! subtly-wrong answer off a walk in progress.
//!
//! Task 107 added one more thing to be told: a caller that arrives close
//! enough to the walk's end can be made to wait a short, bounded moment
//! instead - see [`IndexingStatus::wait_ready`] and
//! `mcp::GMeshMcpServer::still_indexing`'s `INDEXING_GRACE_WINDOW`. That is
//! strictly a refinement of the same answer, not a third option: it either
//! resolves to the real answer a few milliseconds later than a poll would
//! have, or to the exact same "still indexing" this module already gave.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

/// Shared "is the cold-start walk still running?" state: a lock-free flag for
/// the check every tool handler makes, plus a wakeup a handler can wait on
/// for a bounded time before it commits to "still indexing".
///
/// The flag stays a bare atomic rather than growing a `Mutex` around it - the
/// whole point is to answer *while* the walk holds the daemon's single SQLite
/// connection for a batch commit, and a handler that had to take a mutex to
/// find out whether it may take that mutex would queue behind exactly the
/// work it is trying not to wait for. The wakeup is a separate `Notify`
/// rather than a `Condvar` paired with that same atomic for the same reason:
/// every reader of this type is an async tool handler on the daemon's own
/// tokio runtime (`mcp::GMeshMcpServer`), and `Notify::notified().await`
/// suspends the calling task without holding a worker thread, where a
/// `Condvar::wait` would either block a worker outright or need
/// `spawn_blocking` - a whole borrowed thread - to wait out what is, in the
/// overwhelmingly common case, a handful of milliseconds.
#[derive(Clone)]
pub struct IndexingStatus(Arc<Inner>);

struct Inner {
    indexing: AtomicBool,
    /// Fired once, by [`mark_ready`](IndexingStatus::mark_ready), so a task
    /// already parked in [`wait_ready`](IndexingStatus::wait_ready) is woken
    /// instead of having to poll the atomic on a timer.
    ready: Notify,
}

impl IndexingStatus {
    /// A daemon that owes its project a cold-start walk. Everything it is
    /// asked before that walk commits is answered with "still indexing" -
    /// modulo the grace wait `wait_ready` gives a call that arrives close to
    /// the end of it.
    pub fn indexing() -> Self {
        Self(Arc::new(Inner { indexing: AtomicBool::new(true), ready: Notify::new() }))
    }

    /// A daemon whose index was already complete when it started - every
    /// restart of an already-walked project, which is the overwhelmingly
    /// common case. Nothing ever reads as "still indexing" for such a
    /// project; its socket is bound and answering as before.
    pub fn ready() -> Self {
        Self(Arc::new(Inner { indexing: AtomicBool::new(false), ready: Notify::new() }))
    }

    /// Flipped once, by `daemon::run`, after the walk's *final* commit - not
    /// once per committed batch.
    ///
    /// Per-batch would be the bug this type exists to prevent: the walk
    /// commits in batches of `bulk_index::BATCH_ITEMS`, and cross-file edges
    /// are linked only after the last of them (`graph::imports`,
    /// `graph::symbol_links`), so a graph that is k batches in is a graph in
    /// which a real symbol can have no callers, no references and no
    /// importers yet. Every one of those is a well-formed, confident, wrong
    /// answer - the exact failure `storage::schema::CURRENT_INDEXER_VERSION`
    /// was introduced to end, and not one worth reintroducing at a finer
    /// grain.
    ///
    /// `Release`, paired with `Acquire` in [`is_indexing`](Self::is_indexing):
    /// a reader that sees `false` is guaranteed to see everything the walk
    /// wrote before flipping it. The `Notify` wakeup that follows piggybacks
    /// on that same guarantee - anything woken by it observes the store,
    /// because the store happened-before the notification that woke it.
    pub fn mark_ready(&self) {
        self.0.indexing.store(false, Ordering::Release);
        self.0.ready.notify_waiters();
    }

    /// Whether a query asked right now would be reading a graph the walk has
    /// not finished building.
    pub fn is_indexing(&self) -> bool {
        self.0.indexing.load(Ordering::Acquire)
    }

    /// Waits up to `timeout` for the walk to finish. Returns `true` if it
    /// finished within the window - the caller should go on to serve the
    /// real answer - or `false` if `timeout` elapsed first, in which case the
    /// caller should answer exactly as it would have without calling this at
    /// all.
    ///
    /// Meant for a caller that has already seen [`is_indexing`](Self::is_indexing)
    /// return `true` and wants to give the walk a short chance to finish
    /// before refusing; calling it against an already-ready status just
    /// returns `true` immediately.
    ///
    /// `notified()` is created *before* the check that follows, not after:
    /// `notify_waiters` only wakes tasks that were already waiting, so if
    /// `mark_ready` ran between a naive `is_indexing` check and a later call
    /// to `notified()`, the notification would already be gone and this
    /// future would sit out the whole timeout despite the walk having
    /// already finished. Registering first closes that window - by the time
    /// the walk is asked about at all, this task is already able to hear
    /// about it.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let notified = self.0.ready.notified();
        if !self.is_indexing() {
            return true;
        }
        tokio::time::timeout(timeout, notified).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_that_owes_a_walk_reads_as_indexing_until_the_walk_is_marked_done() {
        let status = IndexingStatus::indexing();
        assert!(status.is_indexing());

        status.mark_ready();
        assert!(!status.is_indexing());
    }

    /// The fast path task 96/99 left intact: a restart against an index that
    /// was already fully walked never shows an agent a "still indexing"
    /// answer, because there is no walk to be in the middle of.
    #[test]
    fn a_project_with_a_complete_index_never_reads_as_indexing() {
        let status = IndexingStatus::ready();
        assert!(!status.is_indexing());
    }

    /// Every connection the accept loop serves holds its own clone, so the
    /// flip has to be visible through all of them at once.
    #[test]
    fn every_clone_sees_the_same_flip() {
        let status = IndexingStatus::indexing();
        let seen_by_a_connection = status.clone();

        status.mark_ready();

        assert!(!seen_by_a_connection.is_indexing());
    }

    /// The fast path `wait_ready` must not lose either: an already-ready
    /// status resolves without ever touching the `Notify`.
    #[tokio::test]
    async fn waiting_on_an_already_ready_status_returns_true_immediately() {
        let status = IndexingStatus::ready();
        assert!(status.wait_ready(Duration::from_secs(10)).await);
    }

    /// The race this type exists to absorb: a `mark_ready` that lands while
    /// something is already inside `wait_ready` must wake it rather than
    /// making it sit out the rest of the timeout.
    #[tokio::test]
    async fn waiting_returns_true_as_soon_as_mark_ready_is_called() {
        let status = IndexingStatus::indexing();
        let marker = status.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            marker.mark_ready();
        });

        assert!(status.wait_ready(Duration::from_secs(10)).await);
    }

    /// The huge-project case task 105 exists for: nothing ever marks the walk
    /// ready, so the wait must give up at `timeout` rather than hang.
    #[tokio::test]
    async fn waiting_returns_false_once_the_timeout_elapses_with_no_mark_ready() {
        let status = IndexingStatus::indexing();
        assert!(!status.wait_ready(Duration::from_millis(20)).await);
    }
}
