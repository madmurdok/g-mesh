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
//! and the store's own `unclaimed` set are leaves (nothing is locked under
//! them), so taking them under the store is allowed. The `unclaimed` set is
//! more than allowed under it: it is only ever touched there
//! (`IndexStore::unclaimed`), so an upsert and its claim, a swap and the ids
//! it keeps, and a sweep's read of the set and its delete are each one
//! critical section no other writer can land between.
//!
//! Two checks back this up. Every store guard sets a thread-local flag:
//! acquiring the store again on the same thread panics (always on) instead
//! of self-deadlocking on the non-reentrant mutex, and [`assert_not_held`],
//! called where the plugin-side locks are taken, fails a debug build that
//! takes one under the store. Guards contain a `MutexGuard`, so they are
//! `!Send` and cannot cross an `.await`, which keeps the thread-local
//! correct under tokio.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::embedding::pipeline::ComputedEmbedding;
use crate::embedding::EmbeddingPipeline;
use crate::graph::{imports, symbol_links};
use crate::storage::file_rows::{self, FileScope};
use crate::storage::language_swap::{self, SwapBookkeeping};
use crate::storage::write::{apply_diff, upsert_indexed_file, Diff};

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
    /// Per language, the placeholders the last swap kept that no diff has
    /// upserted since (`storage::language_swap`, module doc). In memory only:
    /// a restart forgets them, and they stay until the language's next swap
    /// keeps them again.
    unclaimed: Mutex<HashMap<String, HashSet<String>>>,
}

impl IndexStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn: Mutex::new(conn), unclaimed: Mutex::new(HashMap::new()) }
    }

    /// The raw guard, for tests only: no production code outside `storage/`
    /// calls it; it uses the operations below. Keeps `Mutex::lock`'s shape
    /// and poisoning, so a test's `store.lock().unwrap()` reads as before.
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
    #[allow(clippy::result_large_err)]
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
        let mut conn = self.acquire();
        commit_batch_on(&mut conn, diff, vectors)?;
        self.claim(diff);
        Ok(())
    }

    /// The unclaimed set. Only under the store (module doc): the caller holds
    /// the connection's guard, which a debug build checks.
    fn unclaimed(&self) -> MutexGuard<'_, HashMap<String, HashSet<String>>> {
        debug_assert!(
            HELD.with(Cell::get),
            "the unclaimed set was touched without holding the store (module doc: it is only ever \
             changed in the same critical section as the write it describes)"
        );
        self.unclaimed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes every node `diff` upserted out of the unclaimed set: whoever sent
    /// it owns it again. Called under the store, in the step that committed
    /// `diff`.
    fn claim(&self, diff: &Diff) {
        let mut unclaimed = self.unclaimed();
        if unclaimed.is_empty() {
            return;
        }
        for ids in unclaimed.values_mut() {
            for node in &diff.upsert_nodes {
                ids.remove(&node.id);
            }
        }
        unclaimed.retain(|_, ids| !ids.is_empty());
    }

    /// The placeholders of `language` the last swap kept that nothing has
    /// upserted since, sorted.
    pub fn unclaimed_nodes(&self, language: &str) -> Vec<String> {
        let _conn = self.acquire();
        let unclaimed = self.unclaimed();
        let mut ids: Vec<String> = unclaimed.get(language).into_iter().flatten().cloned().collect();
        ids.sort();
        ids
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

    /// The file this store's connection writes, or `None` for an in-memory
    /// index.
    pub fn file_path(&self) -> Option<std::path::PathBuf> {
        self.with(|conn| conn.path().filter(|path| !path.is_empty()).map(std::path::PathBuf::from))
    }

    /// Applies a per-language reindex's plan from the staging file at
    /// `staging_path`, with its vectors and bookkeeping, in one hold and one
    /// transaction (`storage::language_swap::swap`).
    pub fn swap_language(
        &self,
        staging_path: &std::path::Path,
        vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>,
        bookkeeping: &SwapBookkeeping<'_>,
    ) -> Result<usize> {
        // The kept ids are recorded before the guard drops, so no diff can
        // upsert one between the swap and its recording and go unclaimed.
        let mut conn = self.acquire();
        let kept = language_swap::swap(&mut conn, staging_path, vectors, bookkeeping)?;
        let count = kept.len();
        let mut unclaimed = self.unclaimed();
        if kept.is_empty() {
            unclaimed.remove(bookkeeping.language);
        } else {
            unclaimed.insert(bookkeeping.language.to_string(), kept.into_iter().collect());
        }
        Ok(count)
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
        let store = self.store;
        self.step(|conn| {
            apply_and_link(conn, diff, label)?;
            store.claim(diff);
            Ok(())
        })
    }

    /// [`Self::apply_diff_linked`] for one file's `fileChanged` diff, which
    /// is first widened by `scope` to retire the file's stored rows the
    /// plugin did not name (`storage::file_rows`). A gone file also loses its
    /// `indexed_files` row. All in one step.
    pub fn apply_file_diff_linked(
        &mut self,
        diff: &mut Diff,
        file_path: &str,
        scope: FileScope,
        label: &str,
    ) -> Result<()> {
        let store = self.store;
        self.step(|conn| {
            file_rows::widen(conn, file_path, scope, diff)?;
            apply_and_link(conn, diff, label)?;
            if scope == FileScope::Gone {
                file_rows::delete_indexed_file(conn, file_path)?;
            }
            store.claim(diff);
            Ok(())
        })
    }

    /// Deletes `language`'s kept placeholders that nothing has upserted since
    /// the swap that kept them, with every row hanging on them, in one step,
    /// and returns how many went. Called after a complete whole-project
    /// semantic pass: it re-sends every placeholder it still stands behind.
    pub fn sweep_unclaimed_nodes(&mut self, language: &str) -> Result<usize> {
        let store = self.store;
        self.step(|conn| {
            let ids = store.unclaimed().remove(language);
            let Some(ids) = ids else { return Ok(0) };
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            language_swap::delete_placeholders(conn, language, &ids)
        })
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
        let store = self.store;
        self.step(|conn| {
            commit_batch_on(conn, diff, vectors)?;
            store.claim(diff);
            Ok(())
        })
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
    use crate::storage::schema;
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
    }

    /// A kept id upserted between a swap and the recording of what it kept,
    /// or between an upsert's commit and its claim, would go unclaimed and be
    /// swept. Neither gap exists: every write that claims, records or sweeps
    /// touches the unclaimed set under the guard of the write itself, which
    /// `IndexStore::unclaimed` checks. Each operation below runs in a per-step
    /// unit, the policy whose released lock the gaps lived in.
    ///
    /// Control: move `store.claim(diff)` in `Writer::apply_diff_linked` (or
    /// `Writer::commit_batch`) back after its `self.step(...)`, move the
    /// `store.unclaimed().remove(language)` in `Writer::sweep_unclaimed_nodes`
    /// before its step -> this test panics with "the unclaimed set was touched
    /// without holding the store". The swap's half: drop
    /// `IndexStore::swap_language`'s `conn` guard before `self.unclaimed()` ->
    /// `daemon::workspace_reindex`'s placeholder tests panic the same way.
    #[cfg(debug_assertions)]
    #[test]
    fn claiming_and_sweeping_happen_under_the_writes_own_guard() {
        let store = store();
        store
            .unclaimed
            .lock()
            .unwrap()
            .insert("rust".to_string(), ["a", "b", "c"].iter().map(|id| id.to_string()).collect());
        let diff = |id: &str| Diff {
            upsert_nodes: vec![NodeRecord::new(id, "Function", id, id, "src/a.rs", "rust")],
            ..Default::default()
        };
        store.unit(Unit::WatcherApply, |writer| {
            writer.apply_diff_linked(&diff("a"), "test").unwrap();
            writer.commit_batch(&diff("b"), None).unwrap();
        });
        store.commit_batch(&diff("x"), None).unwrap();
        assert_eq!(store.unclaimed_nodes("rust"), vec!["c".to_string()], "a and b claimed");
        let swept = store.unit(Unit::WatcherApply, |writer| writer.sweep_unclaimed_nodes("rust")).unwrap();
        assert_eq!(swept, 0, "c is no node in the index, so nothing to delete");
        assert!(store.unclaimed_nodes("rust").is_empty(), "the sweep empties the language's set");
        assert!(!held());
    }

    /// The debug check itself: touching the set without the store fails.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "the unclaimed set was touched without holding the store")]
    fn touching_the_unclaimed_set_without_the_store_fails_a_debug_build() {
        let store = store();
        store.claim(&Diff::default());
    }
}
