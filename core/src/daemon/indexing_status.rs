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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared, lock-free "is the cold-start walk still running?" flag.
///
/// An atomic rather than a read of `meta.bulkIndexedAt`, because the whole
/// point is to answer *while* the walk holds the daemon's single SQLite
/// connection for a batch commit: a handler that had to take that mutex to
/// find out whether it may take that mutex would queue behind exactly the
/// work it is trying not to wait for.
#[derive(Clone)]
pub struct IndexingStatus(Arc<AtomicBool>);

impl IndexingStatus {
    /// A daemon that owes its project a cold-start walk. Everything it is
    /// asked before that walk commits is answered with "still indexing".
    pub fn indexing() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    /// A daemon whose index was already complete when it started - every
    /// restart of an already-walked project, which is the overwhelmingly
    /// common case. Nothing ever reads as "still indexing" for such a
    /// project; its socket is bound and answering as before.
    pub fn ready() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
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
    /// wrote before flipping it.
    pub fn mark_ready(&self) {
        self.0.store(false, Ordering::Release);
    }

    /// Whether a query asked right now would be reading a graph the walk has
    /// not finished building.
    pub fn is_indexing(&self) -> bool {
        self.0.load(Ordering::Acquire)
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
}
