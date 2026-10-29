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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::embedding::pipeline::ComputedBatch;
use crate::embedding::EmbeddingPipeline;
use crate::graph::symbol_links::LinkRules;
use crate::graph::{imports, symbol_links};
use crate::storage::file_rows::{self, FileScope};
use crate::storage::language_swap::{self, SwapBookkeeping};
use crate::storage::write::{apply_diff, apply_semantic_diff, upsert_indexed_file, Diff};

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

/// Runs `f` inside one transaction on `conn`, so another connection sees
/// everything `f` wrote at once, and commits it whether or not `f` failed.
/// The units inside (`apply_diff`, the import and symbol linkers) are
/// savepoints that roll themselves back on failure, so committing after an
/// inner error keeps exactly the units that succeeded before it. If the
/// commit itself fails, the transaction is rolled back and nothing `f` wrote
/// is kept; the error returned is `f`'s if it failed, else the commit's.
fn in_one_commit<T>(conn: &mut Connection, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
    conn.execute_batch("BEGIN").context("failed to begin the write's transaction")?;
    let result = f(conn);
    if let Err(commit) = conn.execute_batch("COMMIT") {
        // An I/O or full-disk error may already have rolled it back.
        if !conn.is_autocommit() {
            if let Err(rollback) = conn.execute_batch("ROLLBACK") {
                crate::log_line!(
                    "g-mesh: failed to roll back a write after its commit failed ({rollback:#})"
                );
            }
        }
        return match result {
            Err(err) => Err(err),
            Ok(_) => Err(anyhow::Error::new(commit).context("failed to commit the write's transaction")),
        };
    }
    result
}

/// `apply_diff`, then the imports and symbol links it enables, in one hold.
/// `label` names the diff in the error.
fn apply_and_link(conn: &mut Connection, diff: &Diff, label: &str, rules: &LinkRules) -> Result<()> {
    apply_diff(conn, diff).with_context(|| format!("failed to apply the {label} diff"))?;
    link_applied(conn, diff, rules)
}

#[cfg(test)]
type AfterApplyHook = Box<dyn FnMut(&Connection)>;

#[cfg(test)]
thread_local! {
    static AFTER_APPLY_FOR_TESTS: std::cell::RefCell<Option<AfterApplyHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs (or, with `None`, removes) this thread's callback that
/// [`link_applied`] runs on the writer's connection after the diff is written
/// and before it is linked.
#[cfg(test)]
pub(crate) fn set_after_apply_for_tests(hook: Option<AfterApplyHook>) {
    AFTER_APPLY_FOR_TESTS.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn after_apply_for_tests(conn: &Connection) {
    AFTER_APPLY_FOR_TESTS.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(conn);
        }
    });
}

#[cfg(not(test))]
fn after_apply_for_tests(_: &Connection) {}

/// The imports and symbol links an applied `diff` enables.
fn link_applied(conn: &mut Connection, diff: &Diff, rules: &LinkRules) -> Result<()> {
    after_apply_for_tests(conn);
    // After the write, in the same transaction: linking points edges at
    // `File` nodes, and the ones this diff brought with it have to be in the
    // index first.
    imports::link_diff(conn, diff).context("failed to link the file's resolved imports")?;
    // Symbols second: a usage edge can only be repointed at an export that
    // is already written, including the ones this diff added.
    symbol_links::link_diff(conn, diff, rules)
        .context("failed to link the file's cross-file symbol usages")?;
    Ok(())
}

fn commit_batch_on(conn: &mut Connection, diff: &Diff) -> Result<()> {
    apply_diff(conn, diff).context("failed to commit a bulk-index batch")?;
    hold_the_lock_open_for_tests();
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
    crate::log_line!(
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
    /// The discovered plugins' linking rules, fixed for the store's life.
    /// Empty unless set by [`Self::with_link_rules`]: no language's rule.
    link_rules: LinkRules,
}

impl IndexStore {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
            unclaimed: Mutex::new(HashMap::new()),
            link_rules: LinkRules::default(),
        }
    }

    /// This store, linking under `rules` - the discovered manifests' own
    /// (`daemon::manifest::link_rules`).
    pub fn with_link_rules(mut self, rules: LinkRules) -> Self {
        self.link_rules = rules;
        self
    }

    /// The rules this store links under.
    pub fn link_rules(&self) -> &LinkRules {
        &self.link_rules
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

    /// Commits one bulk batch in one hold.
    pub fn commit_batch(&self, diff: &Diff) -> Result<()> {
        let mut conn = self.acquire();
        commit_batch_on(&mut conn, diff)?;
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
        let symbols = symbol_links::link_all(&mut conn, &self.link_rules)
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
        vectors: Option<(&EmbeddingPipeline, &ComputedBatch)>,
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

    /// Commits `diff` and links its imports and symbol usages, in one step
    /// and one commit.
    pub fn apply_diff_linked(&mut self, diff: &Diff, label: &str) -> Result<()> {
        let store = self.store;
        self.step(|conn| {
            in_one_commit(conn, |conn| apply_and_link(conn, diff, label, &store.link_rules))?;
            store.claim(diff);
            Ok(())
        })
    }

    /// [`Self::apply_diff_linked`] for a `semanticPass` answer: commits it
    /// through [`apply_semantic_diff`], so a node already stored keeps its
    /// text and takes only its `untypedCalls`. Linking and claiming still
    /// see every node the answer named, kept ones included. In one step and
    /// one commit.
    pub fn apply_semantic_diff_linked(&mut self, diff: &mut Diff, label: &str) -> Result<()> {
        let store = self.store;
        self.step(|conn| {
            in_one_commit(conn, |conn| {
                apply_semantic_diff(conn, diff)
                    .with_context(|| format!("failed to apply the {label} diff"))?;
                link_applied(conn, diff, &store.link_rules)
            })?;
            store.claim(diff);
            Ok(())
        })
    }

    /// [`Self::apply_diff_linked`] for one file's `fileChanged` diff, which
    /// is first widened by `scope` to retire the file's stored rows the
    /// plugin did not name (`storage::file_rows`). A gone file also loses its
    /// `indexed_files` row and its owed-file and never-answered semantic
    /// rows. All in one step and one commit.
    ///
    /// For a gone file, returns the other files whose imports are linked to
    /// it ([`file_rows::importers_of`]), read before its rows go and recorded
    /// in the same commit as owed a re-extract (`reextract_owed_files`, with
    /// the gone file as trigger): their edges into it are left dangling, and
    /// re-extracting them re-resolves each import to another candidate or an
    /// unresolved placeholder. Each importer is owed under its own language,
    /// but only those of the gone file's language are returned: the calling
    /// plugin re-extracts them, or leaves the rows to the next start's
    /// `config_reindex::resume_owed_reextracts`, which is the only one to
    /// re-extract an importer of another language (with its own plugin).
    /// Empty for any other scope.
    pub fn apply_file_diff_linked(
        &mut self,
        diff: &mut Diff,
        file_path: &str,
        scope: FileScope,
        label: &str,
    ) -> Result<Vec<String>> {
        let store = self.store;
        self.step(|conn| {
            let importers = in_one_commit(conn, |conn| {
                let importers =
                    if scope == FileScope::Gone { file_rows::importers_of(conn, file_path)? } else { None };
                file_rows::widen(conn, file_path, scope, diff)?;
                apply_and_link(conn, diff, label, &store.link_rules)?;
                if scope == FileScope::Gone {
                    file_rows::delete_indexed_file(conn, file_path)?;
                    super::schema::clear_gone_file_semantic_rows(conn, file_path)?;
                }
                let Some((language, importers)) = importers else { return Ok(Vec::new()) };
                let mut by_language: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for (importer_language, path) in importers {
                    by_language.entry(importer_language).or_default().push(path);
                }
                for (importer_language, paths) in &by_language {
                    super::schema::owe_reextracts(conn, importer_language, file_path, paths)?;
                }
                Ok(by_language.remove(&language).unwrap_or_default())
            })?;
            store.claim(diff);
            Ok(importers)
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

    /// Stores precomputed vectors and deletes the filtered nodes' ones, in
    /// one step. Best-effort, like
    /// [`EmbeddingPipeline::store`].
    pub fn store_vectors(&mut self, embedding: &EmbeddingPipeline, computed: &ComputedBatch) {
        self.step(|conn| embedding.store(conn, computed));
    }

    /// [`IndexStore::commit_batch`] as one step of this unit.
    pub fn commit_batch(&mut self, diff: &Diff) -> Result<()> {
        let store = self.store;
        self.step(|conn| {
            commit_batch_on(conn, diff)?;
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
        store.commit_batch(&diff).unwrap();
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
            writer.commit_batch(&diff("b")).unwrap();
        });
        store.commit_batch(&diff("x")).unwrap();
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

    /// `language:filePath` of every owed-file (`semantic_owed_files`) and
    /// never-answered (`semantic_gap_files`) row, each list in order.
    fn semantic_file_rows(conn: &Connection) -> (Vec<String>, Vec<String>) {
        let column = |sql: &str| -> Vec<String> {
            conn.prepare(sql).unwrap().query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect()
        };
        (
            column("SELECT language || ':' || filePath FROM semantic_owed_files ORDER BY 1"),
            column("SELECT language || ':' || filePath FROM semantic_gap_files ORDER BY 1"),
        )
    }

    /// Gives `file_path` an owed row and a never-answered row in `language`.
    fn owe_and_give_up(conn: &Connection, language: &str, file_path: &str) {
        conn.execute(
            "INSERT INTO semantic_owed_files (language, filePath, attempts) VALUES (?1, ?2, 1)",
            rusqlite::params![language, file_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO semantic_gap_files (language, filePath) VALUES (?1, ?2)",
            rusqlite::params![language, file_path],
        )
        .unwrap();
    }

    fn apply_file_diff(store: &IndexStore, file_path: &str, scope: FileScope) {
        store
            .unit(Unit::WatcherApply, |writer| {
                writer.apply_file_diff_linked(&mut Diff::default(), file_path, scope, "test")
            })
            .unwrap();
    }

    /// A deleted file's owed and never-answered rows go with it, in every
    /// language, and `g-mesh status` stops counting it as never answered: no
    /// plugin answer would ever clear them.
    #[test]
    fn a_gone_file_loses_its_owed_and_never_answered_rows_and_status_stops_counting_it() {
        let project = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let db_path = state.path().join("index.db");
        let conn = Connection::open(&db_path).unwrap();
        schema::ensure_current(&conn, &crate::daemon::registry::fixture_indexer_version()).unwrap();
        owe_and_give_up(&conn, "typescript", "src/gone.ts");
        owe_and_give_up(&conn, "rust", "src/gone.ts");
        conn.execute(
            "INSERT INTO semantic_gap_files (language, filePath) VALUES ('typescript', 'src/kept.ts')",
            [],
        )
        .unwrap();
        let store = IndexStore::new(conn);
        let plugins =
            crate::daemon::manifest::discover(&[
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins")
            ])
            .unwrap();
        let never_answered = || -> Vec<(String, usize)> {
            crate::cli::status::index_status(
                project.path(),
                &db_path,
                &plugins,
                crate::cli::status::Mode::Light,
            )
            .unwrap()
            .semantic_leftovers
            .into_iter()
            .map(|leftover| (leftover.language, leftover.never_answered))
            .collect()
        };
        assert!(never_answered().contains(&("typescript".to_string(), 2)), "{:?}", never_answered());

        apply_file_diff(&store, "src/gone.ts", FileScope::Gone);

        assert_eq!(
            store.with(semantic_file_rows),
            (vec![], vec!["typescript:src/kept.ts".to_string()]),
            "every language's rows for the gone file are deleted"
        );
        assert!(never_answered().contains(&("typescript".to_string(), 1)), "{:?}", never_answered());
    }

    /// A file that changed but is still there keeps its rows: only an answer
    /// from the plugin settles them.
    #[test]
    fn a_partial_or_complete_file_diff_keeps_the_owed_and_never_answered_rows() {
        for scope in [FileScope::Partial, FileScope::Complete] {
            let store = store();
            store.with(|conn| owe_and_give_up(conn, "rust", "src/a.rs"));

            apply_file_diff(&store, "src/a.rs", scope);

            assert_eq!(
                store.with(semantic_file_rows),
                (vec!["rust:src/a.rs".to_string()], vec!["rust:src/a.rs".to_string()]),
                "{scope:?}"
            );
        }
    }

    /// A gone file takes only its own rows.
    #[test]
    fn a_gone_file_leaves_other_files_owed_and_never_answered_rows() {
        let store = store();
        store.with(|conn| {
            owe_and_give_up(conn, "rust", "src/a.rs");
            owe_and_give_up(conn, "rust", "src/b.rs");
            owe_and_give_up(conn, "rust", "src/a.rs.bak");
        });

        apply_file_diff(&store, "src/a.rs", FileScope::Gone);

        let left = vec!["rust:src/a.rs.bak".to_string(), "rust:src/b.rs".to_string()];
        assert_eq!(store.with(semantic_file_rows), (left.clone(), left));
    }

    /// A store whose connection enforces no foreign key, as the daemon's
    /// does: a gone file's node is deleted with other files' edges into it.
    fn store_without_foreign_keys() -> IndexStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
        schema::apply(&conn).unwrap();
        IndexStore::new(conn)
    }

    /// `lib.ts`'s `File` node, imported by `use.ts` and `other.ts` (linked
    /// `IMPORTS` edges into it), and `lib.ts`'s own import of `other.ts`.
    fn imported_lib(store: &IndexStore) {
        let file =
            |path: &str| NodeRecord::new(format!("File:{path}"), "File", path, path, path, "typescript");
        let import = |from: &str, to: &str| {
            crate::storage::write::EdgeRecord::new(
                format!("i:{from}->{to}"),
                format!("File:{from}"),
                format!("File:{to}"),
                "IMPORTS",
                "tree-sitter",
                true,
            )
        };
        let diff = Diff {
            upsert_nodes: vec![file("lib.ts"), file("use.ts"), file("other.ts")],
            upsert_edges: vec![
                import("use.ts", "lib.ts"),
                import("other.ts", "lib.ts"),
                import("lib.ts", "other.ts"),
            ],
            ..Default::default()
        };
        store.unit(Unit::WatcherApply, |writer| writer.apply_diff_linked(&diff, "seed")).unwrap();
    }

    /// A gone file's importers are returned and recorded as owed a
    /// re-extract in the delete's own commit, keyed by the importer with the
    /// gone file as trigger; its own import of another file makes nobody owed.
    #[test]
    fn a_gone_file_owes_its_importers_a_re_extract_in_the_same_commit() {
        let store = store_without_foreign_keys();
        imported_lib(&store);

        let importers = store
            .unit(Unit::WatcherApply, |writer| {
                writer.apply_file_diff_linked(&mut Diff::default(), "lib.ts", FileScope::Gone, "test")
            })
            .unwrap();

        assert_eq!(importers, vec!["other.ts".to_string(), "use.ts".to_string()]);
        assert_eq!(
            store.with(schema::owed_reextracts).unwrap(),
            vec![(
                "typescript".to_string(),
                "lib.ts".to_string(),
                vec!["other.ts".to_string(), "use.ts".to_string()]
            )]
        );
    }

    /// A file that is still there owes nobody a re-extract.
    #[test]
    fn a_partial_or_complete_file_diff_owes_no_re_extract() {
        for scope in [FileScope::Partial, FileScope::Complete] {
            let store = store_without_foreign_keys();
            imported_lib(&store);

            let importers = store
                .unit(Unit::WatcherApply, |writer| {
                    writer.apply_file_diff_linked(&mut Diff::default(), "lib.ts", scope, "test")
                })
                .unwrap();

            assert!(importers.is_empty(), "{scope:?}");
            assert!(store.with(schema::owed_reextracts).unwrap().is_empty(), "{scope:?}");
        }
    }

    // --- One commit per step: what another connection sees ---------------
    //
    // `lib.ts` declares `parse` and `format`; `use.ts`'s `useString` calls
    // both through `pending_symbol` placeholders (`e-parse`, `e-format`) and
    // has a bound semantic call `e-bound` straight onto `parse`, the shape of
    // an overload call. Every save of `use.ts` re-sends the two structural
    // edges onto their placeholders, which the step's linker moves back onto
    // the declarations.

    const USE_STRING: &str = "Function:use.ts:useString";
    const PARSE: &str = "Function:lib.ts:parse";
    const FORMAT: &str = "Function:lib.ts:format";
    const EXTRA: &str = "Function:use.ts:extra";

    /// `(id, toId, resolved, linkedFrom)` of an edge.
    type EdgeRow = (String, String, bool, Option<String>);

    fn ts_symbol(file: &str, name: &str) -> NodeRecord {
        let mut node =
            NodeRecord::new(format!("Function:{file}:{name}"), "Function", name, name, file, "typescript");
        node.exported = true;
        node.visibility = "public".to_string();
        node
    }

    fn placeholder_id(name: &str) -> String {
        format!("pending:use.ts:lib.ts#{name}")
    }

    fn placeholder(name: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            placeholder_id(name),
            "Module",
            name,
            format!("lib.ts#{name}"),
            "use.ts",
            "typescript",
        );
        node.native_kind = Some(symbol_links::PENDING_SYMBOL_NATIVE_KIND.to_string());
        node.target = Some(crate::storage::write::PlaceholderTargetRecord {
            scope_kind: "file".to_string(),
            scope: "lib.ts".to_string(),
            key_kind: "name".to_string(),
            key: name.to_string(),
            from_container: None,
            key_path: None,
        });
        node
    }

    fn call_onto_placeholder(name: &str) -> crate::storage::write::EdgeRecord {
        crate::storage::write::EdgeRecord::new(
            format!("e-{name}"),
            USE_STRING,
            placeholder_id(name),
            "CALLS",
            "tree-sitter",
            false,
        )
    }

    /// What a save of `use.ts` sends: its caller, both placeholders and a
    /// new function `extra`, with both structural calls on the placeholders.
    fn resave() -> Diff {
        Diff {
            upsert_nodes: vec![
                ts_symbol("use.ts", "useString"),
                placeholder("parse"),
                placeholder("format"),
                ts_symbol("use.ts", "extra"),
            ],
            upsert_edges: vec![call_onto_placeholder("parse"), call_onto_placeholder("format")],
            ..Default::default()
        }
    }

    /// A store over a WAL file in `dir` with the fixture applied and linked,
    /// and the database's path for a second connection. `foreign_keys` is on
    /// so a deferred violation can fail a commit.
    fn linked_fixture(dir: &std::path::Path) -> (IndexStore, std::path::PathBuf) {
        let path = dir.join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        let store = IndexStore::new(conn);
        let mut seed = resave();
        seed.upsert_nodes.retain(|node| node.id != EXTRA);
        seed.upsert_nodes.extend([ts_symbol("lib.ts", "parse"), ts_symbol("lib.ts", "format")]);
        seed.upsert_edges.push(crate::storage::write::EdgeRecord::new(
            "e-bound", USE_STRING, PARSE, "CALLS", "semantic", true,
        ));
        store.unit(Unit::WatcherApply, |writer| writer.apply_diff_linked(&seed, "seed")).unwrap();
        (store, path)
    }

    fn use_ts_edges(conn: &Connection) -> Vec<EdgeRow> {
        conn.prepare(
            "SELECT e.id, e.toId, e.resolved, e.linkedFrom FROM edges e JOIN nodes n ON n.id = e.fromId
             WHERE n.filePath = 'use.ts' ORDER BY e.id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    /// The fixture's linked state: both structural calls on their
    /// declarations, marked as linked from their placeholders.
    fn linked_state() -> Vec<EdgeRow> {
        vec![
            ("e-bound".to_string(), PARSE.to_string(), true, None),
            ("e-format".to_string(), FORMAT.to_string(), true, Some(placeholder_id("format"))),
            ("e-parse".to_string(), PARSE.to_string(), true, Some(placeholder_id("parse"))),
        ]
    }

    fn edges_on_placeholders(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM edges e JOIN nodes t ON t.id = e.toId WHERE t.nativeKind = ?1",
            [symbol_links::PENDING_SYMBOL_NATIVE_KIND],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn has_node(conn: &Connection, id: &str) -> bool {
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = ?1", [id], |row| row.get::<_, i64>(0)).unwrap()
            == 1
    }

    /// Installs `hook` for this thread and removes it again on drop, so a
    /// failed assertion cannot leave it installed.
    struct HookGuard;

    impl HookGuard {
        fn install(hook: impl FnMut(&Connection) + 'static) -> Self {
            set_after_apply_for_tests(Some(Box::new(hook)));
            HookGuard
        }
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            set_after_apply_for_tests(None);
        }
    }

    #[derive(Clone, Copy)]
    enum Step {
        Diff,
        FileDiff,
        SemanticDiff,
    }

    fn run_step(store: &IndexStore, step: Step, diff: &mut Diff) -> Result<()> {
        store.unit(Unit::WatcherApply, |writer| match step {
            Step::Diff => writer.apply_diff_linked(diff, "test"),
            Step::FileDiff => {
                writer.apply_file_diff_linked(diff, "use.ts", FileScope::Partial, "test").map(|_| ())
            }
            Step::SemanticDiff => writer.apply_semantic_diff_linked(diff, "test"),
        })
    }

    /// Between the step's apply and its link, a reader on another connection
    /// still sees the state before the step: never a call on a placeholder,
    /// never the new function. After the step it sees the linked state.
    fn a_reader_never_sees_the_applied_but_unlinked_diff(step: Step) {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());
        let reader = Connection::open(&path).unwrap();
        assert_eq!(use_ts_edges(&reader), linked_state(), "the fixture is linked");

        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = std::rc::Rc::clone(&seen);
        let hook_reader = Connection::open(&path).unwrap();
        let hook = HookGuard::install(move |_| {
            record.borrow_mut().push((
                use_ts_edges(&hook_reader),
                edges_on_placeholders(&hook_reader),
                has_node(&hook_reader, EXTRA),
            ))
        });
        run_step(&store, step, &mut resave()).unwrap();
        drop(hook);

        assert_eq!(
            *seen.borrow(),
            vec![(linked_state(), 0, false)],
            "the hook ran once and the reader saw only the state before the step"
        );
        assert_eq!(use_ts_edges(&reader), linked_state(), "after the step: linked");
        assert_eq!(edges_on_placeholders(&reader), 0);
        assert!(has_node(&reader, EXTRA), "after the step: the applied diff is visible");
    }

    /// Control: call the closure of `Writer::apply_diff_linked` without
    /// `in_one_commit` -> the hook sees `e-parse`/`e-format` on placeholders.
    #[test]
    fn apply_diff_linked_publishes_apply_and_link_in_one_commit() {
        a_reader_never_sees_the_applied_but_unlinked_diff(Step::Diff);
    }

    /// Control: the same in `Writer::apply_file_diff_linked`.
    #[test]
    fn apply_file_diff_linked_publishes_apply_and_link_in_one_commit() {
        a_reader_never_sees_the_applied_but_unlinked_diff(Step::FileDiff);
    }

    /// Control: the same in `Writer::apply_semantic_diff_linked`.
    #[test]
    fn apply_semantic_diff_linked_publishes_apply_and_link_in_one_commit() {
        a_reader_never_sees_the_applied_but_unlinked_diff(Step::SemanticDiff);
    }

    /// A hook that makes the `n`th row update of `edges` on the writer's
    /// connection fail, from the hook point on: the symbol linker's repoints.
    fn fail_the_nth_edge_update(n: u32) -> HookGuard {
        HookGuard::install(move |conn| {
            conn.execute_batch(&format!(
                "CREATE TEMP TABLE edge_updates (n INTEGER NOT NULL);
                 INSERT INTO edge_updates VALUES (0);
                 CREATE TEMP TRIGGER fail_edge_update BEFORE UPDATE ON edges BEGIN
                     UPDATE edge_updates SET n = n + 1;
                     SELECT RAISE(ABORT, 'injected link failure') WHERE (SELECT n FROM edge_updates) >= {n};
                 END;"
            ))
            .unwrap();
        })
    }

    fn claimable_extra(store: &IndexStore) {
        store.unclaimed.lock().unwrap().insert("typescript".to_string(), HashSet::from([EXTRA.to_string()]));
    }

    /// A link failure inside the step still commits the applied diff: the
    /// new function is there and the calls
    /// stay on their placeholders. The error is returned and nothing is
    /// claimed; the connection is back in autocommit.
    ///
    /// Controls: in `in_one_commit` run `ROLLBACK` instead of `COMMIT` when
    /// `f` failed -> `extra` is missing; ignore `in_one_commit`'s error in
    /// `Writer::apply_file_diff_linked` (`let _ = in_one_commit(..)`) ->
    /// `extra` is claimed.
    #[test]
    fn a_link_failure_still_commits_the_applied_diff_and_claims_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());
        claimable_extra(&store);

        let hook = fail_the_nth_edge_update(1);
        let err = run_step(&store, Step::FileDiff, &mut resave()).unwrap_err();
        drop(hook);

        assert!(format!("{err:#}").contains("injected link failure"), "{err:#}");
        let reader = Connection::open(&path).unwrap();
        assert!(has_node(&reader, EXTRA), "the applied diff is committed");
        assert_eq!(edges_on_placeholders(&reader), 2, "both calls stay where the diff put them");
        assert_eq!(
            store.unclaimed_nodes("typescript"),
            vec![EXTRA.to_string()],
            "a failed step claims nothing"
        );
        assert!(store.with(Connection::is_autocommit));
    }

    /// The symbol linker's failure rolls back its own savepoint only: the
    /// repoint it made before failing is undone, the apply before it stays.
    ///
    /// Control: give `symbol_links::link`'s savepoint
    /// `DropBehavior::Commit` -> one call is left linked.
    #[test]
    fn a_failing_unit_rolls_back_only_its_own_savepoint() {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());

        let hook = fail_the_nth_edge_update(2);
        let err = run_step(&store, Step::FileDiff, &mut resave()).unwrap_err();
        drop(hook);

        assert!(format!("{err:#}").contains("injected link failure"), "{err:#}");
        let reader = Connection::open(&path).unwrap();
        assert!(has_node(&reader, EXTRA), "the apply's savepoint is kept");
        assert_eq!(
            use_ts_edges(&reader)
                .into_iter()
                .filter(|(id, ..)| id != "e-bound")
                .map(|(id, to, ..)| (id, to))
                .collect::<Vec<_>>(),
            vec![
                ("e-format".to_string(), placeholder_id("format")),
                ("e-parse".to_string(), placeholder_id("parse")),
            ],
            "the linker's first repoint went with its savepoint"
        );
    }

    /// An apply error writes nothing, links nothing and claims nothing.
    ///
    /// Control: in `storage::write::apply_diff` ignore `write_diff`'s error
    /// (`let _ = write_diff(&tx, diff);`) -> `extra` is written.
    #[test]
    fn an_apply_failure_writes_and_claims_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());
        claimable_extra(&store);
        store.with(|conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER fail_edge_insert BEFORE INSERT ON edges BEGIN
                     SELECT RAISE(ABORT, 'injected apply failure');
                 END;
                 CREATE TEMP TRIGGER fail_edge_update BEFORE UPDATE ON edges BEGIN
                     SELECT RAISE(ABORT, 'injected apply failure');
                 END;",
            )
            .unwrap()
        });
        let hook_runs = std::rc::Rc::new(Cell::new(0));
        let count = std::rc::Rc::clone(&hook_runs);
        let hook = HookGuard::install(move |_| count.set(count.get() + 1));
        let err = run_step(&store, Step::FileDiff, &mut resave()).unwrap_err();
        drop(hook);

        assert!(format!("{err:#}").contains("injected apply failure"), "{err:#}");
        assert_eq!(hook_runs.get(), 0, "nothing is linked after a failed apply");
        let reader = Connection::open(&path).unwrap();
        assert!(!has_node(&reader, EXTRA), "the apply's nodes went with its savepoint");
        assert_eq!(use_ts_edges(&reader), linked_state());
        assert_eq!(store.unclaimed_nodes("typescript"), vec![EXTRA.to_string()]);
        assert!(store.with(Connection::is_autocommit));
    }

    /// A failed `COMMIT` rolls the step back and leaves the connection
    /// usable: the next step commits normally.
    ///
    /// Control: remove `in_one_commit`'s `ROLLBACK` branch -> the connection
    /// stays inside the transaction and the next step fails to `BEGIN`.
    #[test]
    fn a_failed_commit_rolls_back_and_leaves_the_connection_usable() {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());

        // A deferred foreign-key violation is checked at COMMIT only.
        let hook = HookGuard::install(|conn| {
            conn.execute_batch(
                "PRAGMA defer_foreign_keys = ON;
                 UPDATE edges SET toId = 'nowhere' WHERE id = 'e-bound';",
            )
            .unwrap()
        });
        let err = run_step(&store, Step::FileDiff, &mut resave()).unwrap_err();
        drop(hook);

        assert!(format!("{err:#}").contains("failed to commit the write's transaction"), "{err:#}");
        assert!(store.with(Connection::is_autocommit), "the failed transaction is rolled back");
        let reader = Connection::open(&path).unwrap();
        assert!(!has_node(&reader, EXTRA), "nothing of the step is kept");
        assert_eq!(use_ts_edges(&reader), linked_state());

        run_step(&store, Step::FileDiff, &mut resave()).unwrap();
        assert!(has_node(&reader, EXTRA));
        assert_eq!(use_ts_edges(&reader), linked_state());
    }

    /// Outside any transaction `apply_diff` and the symbol linker each commit
    /// on their own (a savepoint opened in autocommit is a transaction) and
    /// leave the connection in autocommit.
    ///
    /// Control: drop `tx.commit()` in `storage::write::apply_diff` -> the
    /// reader sees no `extra`.
    #[test]
    fn standalone_apply_and_link_still_commit_on_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let (store, path) = linked_fixture(dir.path());
        let reader = Connection::open(&path).unwrap();
        let diff = resave();

        {
            let mut guard = store.lock().unwrap();
            let conn: &mut Connection = &mut guard;
            apply_diff(conn, &diff).unwrap();
            assert!(conn.is_autocommit());
            assert!(has_node(&reader, EXTRA), "apply_diff committed");
            assert_eq!(edges_on_placeholders(&reader), 2);
            symbol_links::link_diff(conn, &diff, &LinkRules::default()).unwrap();
            assert!(conn.is_autocommit());
        }
        assert_eq!(use_ts_edges(&reader), linked_state(), "the linker committed");
    }
}
