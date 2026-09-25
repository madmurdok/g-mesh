//! The one owner of the index's SQLite connection and of how long each write
//! holds it. Design: [ADR 0001](../../../docs/adr/0001-index-store.md).
//!
//! Callers get a connection only through an operation: [`IndexStore::read`]
//! for queries, [`IndexStore::with`] for one-statement bookkeeping, a named
//! operation ([`IndexStore::link_all`], [`IndexStore::commit_batch`], ...)
//! for a write whose hold extent matters, and [`IndexStore::unit`] for a
//! multi-step write whose hold policy comes from [`hold`].
//!
//! # Lock order
//!
//! The connection is the innermost lock. While this thread holds the store,
//! no code may take `PluginRegistry::supervisors`, `PluginSupervisor::inner`
//! or `PluginProcess::state`/`pending`; those are always taken first, and the
//! store only inside them. The `IndexingStatus` and `last_activity` mutexes
//! are leaves (nothing is locked under them), so taking them under the store
//! is allowed.
//!
//! Two checks back this up. Every store guard sets a thread-local flag:
//! acquiring the store again on the same thread panics (always on) instead
//! of self-deadlocking on the non-reentrant mutex, and [`assert_not_held`],
//! called where the plugin-side locks are taken, fails a debug build that
//! takes one under the store. Guards contain a `MutexGuard`, so they are
//! `!Send` and cannot cross an `.await`, which keeps the thread-local
//! correct under tokio.

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::embedding::pipeline::ComputedEmbedding;
use crate::embedding::EmbeddingPipeline;
use crate::graph::{imports, symbol_links};
use crate::storage::schema;
use crate::storage::write::{apply_diff, delete_language_rows, upsert_indexed_file, Diff};

thread_local! {
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// Panics in a debug build if this thread holds the store. Called where a
/// lock that must be taken before the store (see the module doc) is taken.
pub fn assert_not_held() {
    debug_assert!(
        !HELD.with(Cell::get),
        "lock order violated: a plugin-side lock was taken while this thread holds the IndexStore \
         (the connection must be the innermost lock)"
    );
}

/// A multi-step write whose lock-hold policy is looked up in [`hold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// One plugin round trip: commit and link its diff, then store the
    /// vectors computed from it.
    WatcherApply,
    /// `watcher::staleness::ensure_fresh`: decide, reindex, record the baseline.
    QueryTimeReindex,
    /// One language's bulk walk: its batch commits and its bookkeeping row.
    BulkWalk,
    /// The embedding backfill pass: count, then page and store.
    Backfill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// Each step locks and releases; work between steps runs unlocked.
    PerStep,
    /// One guard is taken when the unit opens and held until it closes.
    #[allow(dead_code)]
    WholeUnit,
}

/// The lock-hold policy. Changing how long a watcher apply, a query-time
/// reindex, a bulk walk or a backfill holds the lock is a change to this
/// table and nothing else.
const fn hold(unit: Unit) -> Hold {
    match unit {
        // Embedding compute runs unlocked between the commit and the store.
        Unit::WatcherApply => Hold::PerStep,
        // The plugin round trip runs unlocked between decide and baseline.
        Unit::QueryTimeReindex => Hold::PerStep,
        // NDJSON reads and embedding compute run unlocked between batches.
        Unit::BulkWalk => Hold::PerStep,
        // Inference runs unlocked between a page read and its store.
        Unit::Backfill => Hold::PerStep,
    }
}

/// `apply_diff`, then the imports and symbol links it enables, in one hold.
/// `label` names the diff in the error.
fn apply_and_link(conn: &mut Connection, diff: &Diff, label: &str) -> Result<()> {
    apply_diff(conn, diff).with_context(|| format!("failed to apply the {label} diff"))?;
    // After the commit: linking points edges at `File` nodes, and the ones
    // this diff brought with it have to be in the index first.
    imports::link_diff(conn, diff).context("failed to link the file's resolved imports")?;
    // Symbols second: a usage edge can only be repointed at an export that
    // is already committed, including the ones this diff added.
    symbol_links::link_diff(conn, diff).context("failed to link the file's cross-file symbol usages")?;
    Ok(())
}

fn commit_batch_on(
    conn: &mut Connection,
    diff: &Diff,
    vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
) -> Result<()> {
    apply_diff(conn, diff).context("failed to commit a bulk-index batch")?;
    hold_the_lock_open_for_tests();
    // Best-effort: a failed vector store must not undo a durable batch.
    if let Some((embedding, computed)) = vectors {
        embedding.store(conn, computed);
    }
    Ok(())
}

/// Path whose deletion releases a batch commit that is holding the store's
/// lock open, for tests that need the lock itself held (not merely the
/// indexing phase reported). A no-op unless set; bounded at 30 s so a test
/// that forgets to delete the file fails as a timeout.
pub const HOLD_LOCK_FILE_ENV: &str = "G_MESH_BULK_INDEX_HOLD_LOCK_FILE";

/// Honors [`HOLD_LOCK_FILE_ENV`], inside [`IndexStore::commit_batch`]'s hold.
fn hold_the_lock_open_for_tests() {
    let Some(path) = std::env::var_os(HOLD_LOCK_FILE_ENV).filter(|p| !p.is_empty()) else { return };
    let path = std::path::PathBuf::from(path);
    eprintln!(
        "g-mesh daemon: holding a bulk-index batch's lock open until {} is removed ({HOLD_LOCK_FILE_ENV})",
        path.display()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Imports and symbol usages linked by [`IndexStore::link_all`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkCounts {
    pub imports: usize,
    pub symbols: usize,
}

/// Owns the index's connection. Shared as `Arc<IndexStore>` at the
/// composition roots and passed down as `&IndexStore`.
pub struct IndexStore {
    conn: Mutex<Connection>,
}

impl IndexStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn) }
    }

    /// The raw guard. Test support: production code uses the operations
    /// below. Keeps `Mutex::lock`'s shape and poisoning.
    #[doc(hidden)]
    pub fn lock(&self) -> LockResult<StoreGuard<'_>> {
        if HELD.with(Cell::get) {
            panic!(
                "IndexStore re-entered: this thread already holds the store, and taking it again \
                 would self-deadlock"
            );
        }
        match self.conn.lock() {
            Ok(guard) => Ok(StoreGuard::new(guard)),
            Err(poisoned) => Err(PoisonError::new(StoreGuard::new(poisoned.into_inner()))),
        }
    }

    /// Test support: the connection back, with `Mutex::into_inner`'s
    /// poisoning.
    #[doc(hidden)]
    pub fn into_inner(self) -> LockResult<Connection> {
        self.conn.into_inner()
    }

    fn acquire(&self) -> StoreGuard<'_> {
        self.lock().unwrap()
    }

    /// The read path's guard, held for as long as the caller keeps it.
    pub fn read(&self) -> ReadGuard<'_> {
        ReadGuard(self.acquire())
    }

    /// One hold around `f`, for one-statement bookkeeping. `f` must not
    /// reach the store or a plugin-side lock.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.acquire())
    }

    /// Runs a multi-step write under `unit`'s policy from [`hold`].
    pub fn unit<T>(&self, unit: Unit, f: impl FnOnce(&mut Writer<'_>) -> T) -> T {
        self.open(hold(unit), f)
    }

    fn open<T>(&self, hold: Hold, f: impl FnOnce(&mut Writer<'_>) -> T) -> T {
        let held = match hold {
            Hold::PerStep => None,
            Hold::WholeUnit => Some(self.acquire()),
        };
        f(&mut Writer { store: self, held })
    }

    /// Commits one bulk batch and stores its precomputed vectors in one hold.
    pub fn commit_batch(
        &self,
        diff: &Diff,
        vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    ) -> Result<()> {
        commit_batch_on(&mut self.acquire(), diff, vectors)
    }

    /// Links every import, then every cross-file symbol usage, in one hold.
    pub fn link_all(&self) -> Result<LinkCounts> {
        let mut conn = self.acquire();
        let imports =
            imports::link_all(&mut conn).context("failed to link the walk's resolved imports")?.linked_edges;
        let symbols = symbol_links::link_all(&mut conn)
            .context("failed to link the walk's cross-file symbol usages")?
            .linked_edges;
        Ok(LinkCounts { imports, symbols })
    }

    /// Links everything and updates the project-wide bulk-index roll-up in
    /// one hold, so no reader sees a relinked graph without its roll-up.
    pub fn relink_after_language_reindex(&self) -> Result<()> {
        let mut conn = self.acquire();
        imports::link_all(&mut conn).context("failed to link imports after a per-language reindex")?;
        symbol_links::link_all(&mut conn).context("failed to link symbols after a per-language reindex")?;
        schema::record_bulk_index(&conn).context("failed to update the project-wide bulk-index roll-up")?;
        Ok(())
    }

    /// Deletes every row `language` owns, in one transaction.
    pub fn delete_language(&self, language: &str) -> Result<()> {
        delete_language_rows(&mut self.acquire(), language)
    }

    /// Writes `(filePath, mtimeMillis, contentHash)` baselines in one
    /// transaction. Hashing happens before, unlocked.
    pub fn record_walk_baselines(&self, rows: &[(&str, i64, String)]) -> Result<()> {
        let mut conn = self.acquire();
        let tx = conn.transaction().context("failed to start the walk-baseline transaction")?;
        for (file_path, mtime, hash) in rows {
            upsert_indexed_file(&tx, file_path, *mtime, hash)?;
        }
        tx.commit().context("failed to commit the walk's indexed_files baselines")?;
        Ok(())
    }
}

/// A multi-step write in progress. Under `PerStep` each operation locks and
/// releases; under `WholeUnit` it holds the guard taken when the unit opened.
pub struct Writer<'a> {
    store: &'a IndexStore,
    held: Option<StoreGuard<'a>>,
}

impl Writer<'_> {
    /// One step: `f` runs under the lock (this unit's guard, or a fresh one).
    /// `f` must not reach the store or a plugin-side lock.
    pub fn step<T>(&mut self, f: impl FnOnce(&mut Connection) -> T) -> T {
        match &mut self.held {
            Some(guard) => f(guard),
            None => f(&mut self.store.acquire()),
        }
    }

    /// A nested unit: reuses this unit's guard if it holds one, otherwise
    /// applies `unit`'s own policy.
    pub fn unit<T>(&mut self, unit: Unit, f: impl FnOnce(&mut Writer<'_>) -> T) -> T {
        self.nest(hold(unit), f)
    }

    fn nest<T>(&mut self, hold: Hold, f: impl FnOnce(&mut Writer<'_>) -> T) -> T {
        if self.held.is_some() {
            f(self)
        } else {
            self.store.open(hold, f)
        }
    }

    /// Commits `diff` and links its imports and symbol usages, in one step.
    pub fn apply_diff_linked(&mut self, diff: &Diff, label: &str) -> Result<()> {
        self.step(|conn| apply_and_link(conn, diff, label))
    }

    /// Stores precomputed vectors, in one step. Best-effort, like
    /// [`EmbeddingPipeline::store`].
    pub fn store_vectors(&mut self, embedding: &EmbeddingPipeline, computed: &[ComputedEmbedding]) {
        self.step(|conn| embedding.store(conn, computed));
    }

    /// [`IndexStore::commit_batch`] as one step of this unit.
    pub fn commit_batch(
        &mut self,
        diff: &Diff,
        vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
    ) -> Result<()> {
        self.step(|conn| commit_batch_on(conn, diff, vectors))
    }
}

/// A held store. Clears the thread's held flag on drop.
pub struct StoreGuard<'a> {
    guard: MutexGuard<'a, Connection>,
}

impl<'a> StoreGuard<'a> {
    fn new(guard: MutexGuard<'a, Connection>) -> Self {
        HELD.with(|held| held.set(true));
        Self { guard }
    }
}

impl Drop for StoreGuard<'_> {
    fn drop(&mut self) {
        HELD.with(|held| held.set(false));
    }
}

impl Deref for StoreGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.guard
    }
}

impl DerefMut for StoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }
}

/// The read path's guard: `Deref<Target = Connection>`, no `DerefMut`.
/// Read-only by convention, since rusqlite also writes through
/// `&Connection`. Whatever runs under it (including an embedding query)
/// blocks every writer for as long as the guard lives.
pub struct ReadGuard<'a>(StoreGuard<'a>);

impl Deref for ReadGuard<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::write::NodeRecord;

    fn store() -> IndexStore {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        IndexStore::new(conn)
    }

    fn held() -> bool {
        HELD.with(Cell::get)
    }

    fn node_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_per_step_unit_releases_the_lock_between_steps() {
        let store = store();
        store.open(Hold::PerStep, |writer| {
            assert!(!held());
            writer.step(|_| assert!(held()));
            assert!(!held());
            assert!(store.conn.try_lock().is_ok());
        });
    }

    #[test]
    fn a_whole_unit_holds_the_lock_across_steps_and_nested_units() {
        let store = store();
        store.open(Hold::WholeUnit, |writer| {
            assert!(held());
            writer.step(|_| ());
            writer.nest(Hold::PerStep, |inner| inner.step(|_| ()));
            assert!(held());
            assert!(store.conn.try_lock().is_err());
        });
        assert!(!held());
    }

    #[test]
    fn a_nested_unit_under_a_per_step_unit_applies_its_own_policy() {
        let store = store();
        store.open(Hold::PerStep, |writer| {
            writer.nest(Hold::WholeUnit, |inner| {
                assert!(held());
                inner.step(|_| ());
                assert!(held());
            });
            assert!(!held());
        });
    }

    #[test]
    #[should_panic(expected = "IndexStore re-entered")]
    fn re_entering_the_store_panics_instead_of_deadlocking() {
        let store = store();
        store.with(|_| store.with(|_| ()));
    }

    #[test]
    #[should_panic(expected = "IndexStore re-entered")]
    fn a_one_shot_call_inside_a_whole_unit_panics() {
        let store = store();
        store.open(Hold::WholeUnit, |_| store.with(|_| ()));
    }

    #[test]
    fn the_held_flag_clears_when_a_guard_drops() {
        let store = store();
        {
            let _read = store.read();
            assert!(held());
        }
        assert!(!held());
        assert_not_held();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "lock order violated")]
    fn taking_a_plugin_lock_under_the_store_fails_a_debug_build() {
        let store = store();
        store.with(|_| assert_not_held());
    }

    #[test]
    fn the_held_flag_is_per_thread() {
        let store = store();
        let _guard = store.lock().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| assert!(!held()));
        });
    }

    #[test]
    fn a_poisoned_store_stays_poisoned() {
        let store = store();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.lock().unwrap();
            panic!("poison it");
        }));
        assert!(!held());
        assert!(store.lock().is_err());
    }

    #[test]
    fn commit_batch_writes_the_diff() {
        let store = store();
        let diff = Diff {
            upsert_nodes: vec![NodeRecord::new("a", "Function", "a", "a", "src/a.rs", "rust")],
            ..Default::default()
        };
        store.commit_batch(&diff, None).unwrap();
        assert_eq!(store.with(node_count), 1);
        store.delete_language("rust").unwrap();
        assert_eq!(store.with(node_count), 0);
    }
}
