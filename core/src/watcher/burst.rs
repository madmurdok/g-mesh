use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;

use crate::storage::write::{apply_diff, Diff};

/// Coalesces the diffs from a burst of watcher events across *many
/// different files* (a `git checkout`/`git pull` touching dozens of files
/// at once) into a single accumulated `Diff`, so the whole burst is applied
/// via one call to `apply_diff` - one SQLite transaction - instead of one
/// per file.
///
/// This mirrors `Debouncer`'s trailing-edge shape (a timer reset on every
/// new event, fired once it's gone quiet for `window`), but scoped to the
/// whole batch rather than per-path: there is a single shared "last event
/// seen" timestamp, not one per file, since the goal here is grouping
/// events *across* files rather than coalescing repeats of the same file.
///
/// Unlike `Debouncer`, this type is not constructed by `daemon::run`'s
/// watcher thread - see that thread's own comment (where `Debouncer` is
/// wired in instead) for the documented reasoning: the plugin's wire
/// protocol has no batch `FileChanged` request, so batching commits here
/// would not reduce the plugin round trips a burst of *different* files
/// costs, only the SQLite commit count, which was judged separate work
/// (task 129) rather than something to fold into that ticket's wiring pass.
pub struct BurstBatcher {
    window: Duration,
    pending: Diff,
    last_seen: Option<Instant>,
}

impl BurstBatcher {
    pub fn new(window: Duration) -> Self {
        Self { window, pending: Diff::default(), last_seen: None }
    }

    /// Records an incoming per-file diff, folding its node/edge changes
    /// into the accumulated batch and resetting the shared batch-window
    /// timer. `path` isn't used for grouping - unlike `Debouncer` this
    /// batcher tracks one shared timer for the whole batch, not per-path -
    /// it's accepted purely so callers can identify/trace which file each
    /// diff came from.
    pub fn record(&mut self, _path: PathBuf, diff: Diff) {
        self.pending.upsert_nodes.extend(diff.upsert_nodes);
        self.pending.delete_node_ids.extend(diff.delete_node_ids);
        self.pending.upsert_edges.extend(diff.upsert_edges);
        self.pending.delete_edge_ids.extend(diff.delete_edge_ids);
        self.last_seen = Some(Instant::now());
    }

    /// True once there is at least one pending diff *and* the shared
    /// window has elapsed since the last recorded event - the trailing-edge
    /// condition meaning the burst has gone quiet and is safe to flush.
    pub fn ready_to_flush(&self) -> bool {
        match self.last_seen {
            Some(seen) => !self.pending.is_empty() && Instant::now().duration_since(seen) >= self.window,
            None => false,
        }
    }

    /// If `ready_to_flush`, applies the accumulated batch to `conn` via
    /// `apply_diff` - a single SQLite transaction covering every file's
    /// diff folded in since the last flush - and clears the batch. Returns
    /// whether a flush happened. A no-op (returns `Ok(false)`) if the batch
    /// isn't ready yet, mirroring `Debouncer::drain_ready`'s "returns and
    /// clears only when ready" pattern.
    pub fn flush_if_ready(&mut self, conn: &mut Connection) -> Result<bool> {
        if !self.ready_to_flush() {
            return Ok(false);
        }
        let diff = std::mem::take(&mut self.pending);
        self.last_seen = None;
        apply_diff(conn, &diff)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use crate::storage::write::NodeRecord;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    /// Registers a real SQLite `commit_hook` on `conn` and returns a counter
    /// it increments on every actual commit. This counts commits at the
    /// SQLite engine level, independent of how many times our own code
    /// happens to call `apply_diff`/`flush_if_ready` - the honest way to
    /// verify "one SQLite transaction," not a tautology about call counts.
    fn install_commit_counter(conn: &Connection) -> Arc<AtomicUsize> {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_hook = counter.clone();
        conn.commit_hook(Some(move || {
            counter_for_hook.fetch_add(1, Ordering::SeqCst);
            false // false = allow the commit to proceed
        }));
        counter
    }

    fn node_diff(id: &str) -> Diff {
        Diff {
            upsert_nodes: vec![NodeRecord::new(id, "Function", id, id, "src/lib.rs", "rust")],
            ..Default::default()
        }
    }

    #[test]
    fn a_burst_of_fifty_files_flushes_as_exactly_one_sqlite_transaction() {
        let mut conn = setup();
        let commits = install_commit_counter(&conn);
        let mut batcher = BurstBatcher::new(Duration::from_millis(50));

        for i in 0..50 {
            let id = format!("n{i}");
            batcher.record(PathBuf::from(format!("file{i}.rs")), node_diff(&id));
        }

        assert!(
            !batcher.flush_if_ready(&mut conn).unwrap(),
            "burst is still within the window, must not flush yet"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 0);

        thread::sleep(Duration::from_millis(60));
        assert!(batcher.flush_if_ready(&mut conn).unwrap());

        assert_eq!(commits.load(Ordering::SeqCst), 1, "all 50 files' diffs must land in exactly one commit");
        assert_eq!(count(&conn, "nodes"), 50, "no node from the combined diff may be dropped or truncated");
    }

    #[test]
    fn a_single_isolated_file_flushes_promptly_without_waiting_for_a_burst() {
        let mut conn = setup();
        let commits = install_commit_counter(&conn);
        let mut batcher = BurstBatcher::new(Duration::from_millis(50));

        batcher.record(PathBuf::from("a.rs"), node_diff("n1"));
        assert!(!batcher.flush_if_ready(&mut conn).unwrap(), "must not flush before the window elapses");

        thread::sleep(Duration::from_millis(60));
        assert!(
            batcher.flush_if_ready(&mut conn).unwrap(),
            "an isolated change flushes on its own after one window"
        );

        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(count(&conn, "nodes"), 1);
    }

    #[test]
    fn a_gap_longer_than_the_window_keeps_two_files_in_separate_commits() {
        let mut conn = setup();
        let commits = install_commit_counter(&conn);
        let mut batcher = BurstBatcher::new(Duration::from_millis(50));

        batcher.record(PathBuf::from("a.rs"), node_diff("n1"));
        thread::sleep(Duration::from_millis(60));
        assert!(
            batcher.flush_if_ready(&mut conn).unwrap(),
            "a's window elapsed with nothing else pending, so it must flush on its own"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 1);

        // b arrives well after a already flushed - it must not be held open
        // waiting to be combined with something that already left.
        batcher.record(PathBuf::from("b.rs"), node_diff("n2"));
        assert!(!batcher.flush_if_ready(&mut conn).unwrap(), "b's own window hasn't elapsed yet");

        thread::sleep(Duration::from_millis(60));
        assert!(batcher.flush_if_ready(&mut conn).unwrap());

        assert_eq!(commits.load(Ordering::SeqCst), 2, "two separate commits, not coalesced into one");
        assert_eq!(count(&conn, "nodes"), 2);
    }

    #[test]
    fn a_fresh_event_during_the_window_extends_it_and_combines_into_one_commit() {
        let mut conn = setup();
        let commits = install_commit_counter(&conn);
        let mut batcher = BurstBatcher::new(Duration::from_millis(50));

        batcher.record(PathBuf::from("a.rs"), node_diff("n1"));
        thread::sleep(Duration::from_millis(30));
        batcher.record(PathBuf::from("b.rs"), node_diff("n2")); // resets the shared timer before it would have fired
        thread::sleep(Duration::from_millis(30));

        assert!(
            !batcher.flush_if_ready(&mut conn).unwrap(),
            "30ms since the second record is still under the 50ms window"
        );

        thread::sleep(Duration::from_millis(30));
        assert!(batcher.flush_if_ready(&mut conn).unwrap());

        assert_eq!(commits.load(Ordering::SeqCst), 1, "both files combined into a single commit");
        assert_eq!(count(&conn, "nodes"), 2);
    }
}
