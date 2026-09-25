# 0001. IndexStore: one owner for the SQLite connection and its lock policy

## Status
Proposed (GM-407, design slice S1). Owner review required before S2.

## Context

The daemon has one SQLite `Connection` behind one `std::sync::Mutex`. Every
level of a call chain receives the raw `&Mutex<Connection>` and decides for
itself when to lock it and for how long. Moving *when* the lock is held is
therefore a signature change across a call chain: GM-396 (release the lock
during an incremental reparse's embedding step) touched 5 src files
(`watcher/apply.rs`, `watcher/staleness.rs`, `daemon/plugin.rs`,
`cli/plugin_check/session.rs`, `embedding/pipeline.rs`) plus
`tests/protocol_conformance.rs`; GM-394 did the same for the bulk path.

### Measured on 4dde9d5 (release-3.13.0)

Method, for S5 to reproduce: regex `Mutex<(rusqlite::)?Connection>` for
naming; for lock sites, every `.lock()` whose receiver (same line, or the
previous line when `.lock()` starts the line) contains `conn`. "Test" = a
file under `core/tests/`, a `tests.rs`/`test_plugin.rs` file, or a line after
the file's top-level `#[cfg(test)] mod`.

| Measure | Production | In-crate test code | `core/tests/` |
|---|---|---|---|
| Files naming `Mutex<Connection>` | 23 (one, `indexing_status.rs`, only in a doc comment) | 5 files (`daemon/tests.rs`, `daemon/plugin/tests.rs`, `daemon/test_plugin.rs`, `mcp/get_dependencies/tests.rs`, `watcher/apply/tests.rs`) + inline test modules | 4 |
| Connection `.lock()` sites | **44 in 21 files** | 47 | 12 |
| `Mutex::new(conn…)` constructions | 5 (`daemon/mod.rs:499`, `cli/init.rs:205,230`, `cli/reindex.rs:100`, `cli/plugin_check/session.rs:539`) | ~135 across both test kinds | |

The 3.12.0 figures in GM-407 (24 + 4 files, 69 sites) used a different,
unrecorded method; compare S5's "after" against the table above, not
against those.

Production lock sites by role:

| Role | Sites | Where (file:line) |
|---|---|---|
| Watcher apply (per diff) | 2 | `watcher/apply.rs:323` (apply + link), `:351` (store vectors) |
| Query-time reindex | 3 | `watcher/staleness.rs:168` (decide), `:177`, `:198` (baseline) |
| Staleness pre-check | 3 | `daemon/plugin.rs:1262`, `daemon/lifecycle.rs:715`, `daemon/registry.rs:1208` (all `is_stale`) |
| Bulk walk | 4 | `daemon/bulk_index.rs:565` (batch commit), `:271` (link all), `:414` (language bookkeeping); `watcher/staleness.rs:392` (walk baselines, one tx) |
| Per-language reindex | 4 | `daemon/workspace_reindex.rs:341` (delete), `:362` (link all + roll-up, one hold), `:382`, `:410` (bookkeeping) |
| Semantic-pass bookkeeping | 6 | `daemon/semantic.rs:189,244,316,386,462,480` |
| Embedding backfill | 3 | `embedding/backfill.rs:120` (count), `:136` (page), `:159` (store) |
| Walk roll-up | 3 | `daemon/activation.rs:234`, `cli/init.rs:246`, `cli/reindex.rs:111` |
| plugin-check harness | 4 | `cli/plugin_check/session.rs:547` (link all), `:1077` (bookkeeping), `:563`, `:581` (reads) |
| MCP read | 11 | `mcp/find_callers_callees.rs:340,402`, `find_definition.rs:757`, `find_implementations.rs:159,486,496`, `find_references.rs:169`, `get_dependencies.rs:758`, `get_file_outline.rs:110`, `search_code.rs:154`, `mcp/mod.rs:796` |
| MCP write | 1 | `mcp/mod.rs:720` (`last_used::touch`) |

Write path 31, read path 13. `cli/status.rs` opens its own `Connection` and
has none.

### Lock order today

Hierarchy, outermost first, established from the code:

1. `PluginRegistry::supervisors` - held only for a map lookup/insert,
   released before the supervisor is used (`registry.rs` "Lock order").
2. `PluginSupervisor::inner` - held across a whole replay, a query-time
   reindex, a semantic pass, and `with_exclusive_access` (the per-language
   re-walk).
3. `PluginProcess::state` / `pending` - held across one plugin round trip.
4. The connection - innermost. Taken inside 2 and 3
   (`staleness::ensure_fresh`, `watcher::apply::round_trip`,
   `workspace_reindex::run` inside `with_exclusive_access`), never around them.

No production site violates it. Every connection guard outside `mcp/` is a
block-scoped statement over storage/graph/schema functions; the three
`is_stale` pre-checks close their guard before taking a plugin lock; MCP's
`prepare` runs `mark_used` (lock, write, release) before the replay. The
`IndexingStatus` and `last_activity` mutexes are leaves (nothing is locked
under them), so taking them under the connection is harmless.

The invariant is stated twice (`daemon/lifecycle.rs` "Lock order",
`daemon/registry.rs` "Lock order") and enforced nowhere. It holds today only
because no caller runs foreign code under a guard. The one place that does
is the MCP handlers: `anchor::resolve` / `find_definition`'s
`by_semantic_neighbours` run `EmbeddingPipeline::embed_query` (one ONNX
forward pass) while holding the connection. That is a pre-existing long hold
on the read path, not an inversion.

## Decision

We will add `core/src/storage/index_store.rs` with an `IndexStore` that owns
the `Mutex<Connection>`, and nothing outside `storage/` will name
`Mutex<Connection>` or call `.lock()` on the connection. Callers pass
`&IndexStore` (`Arc<IndexStore>` at the composition roots) and call
operations. The store decides how long each operation holds the lock, and
multi-step writes look their hold policy up in one table.

### Type and operations (sketch)

```rust
pub struct IndexStore { conn: Mutex<Connection> }

impl IndexStore {
    pub fn new(conn: Connection) -> Self;

    // Read path: query tools. Deref<Target = Connection>, no DerefMut.
    pub fn read(&self) -> ReadGuard<'_>;

    // Multi-step writes: the hold policy comes from `hold(unit)` below.
    pub fn unit<T>(&self, unit: Unit, f: impl FnOnce(&mut Writer<'_>) -> T) -> T;

    // Single-hold writes whose extent matters (each is one lock hold).
    pub fn commit_batch(&self, diff: &Diff, vectors: Option<(&EmbeddingPipeline, &[ComputedEmbedding])>) -> Result<()>;
    pub fn link_all(&self) -> Result<LinkCounts>;              // imports, then symbols
    pub fn relink_after_language_reindex(&self) -> Result<()>; // link_all + record_bulk_index, one hold
    pub fn delete_language(&self, language: &str) -> Result<()>;
    pub fn record_walk_baselines(&self, rows: &[(&str, i64, String)]) -> Result<()>;

    // One-statement bookkeeping (see decision 2 for the alternative).
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> T) -> T;

    // Test support (see decision 3).
    #[doc(hidden)]
    pub fn lock(&self) -> LockResult<StoreGuard<'_>>;
}

impl Writer<'_> {
    pub fn apply_diff_linked(&mut self, diff: &Diff) -> Result<()>;     // apply_diff + imports + symbols
    pub fn store_vectors(&mut self, e: &EmbeddingPipeline, c: &[ComputedEmbedding]);
    pub fn step<T>(&mut self, f: impl FnOnce(&mut Connection) -> T) -> T; // staleness decide/baseline, backfill page
    pub fn unit<T>(&mut self, unit: Unit, f: impl FnOnce(&mut Writer<'_>) -> T) -> T; // nested unit
}

#[derive(Clone, Copy)]
pub enum Unit { WatcherApply, QueryTimeReindex, BulkWalk, Backfill }

enum Hold { PerStep, WholeUnit }

// The lock-hold policy. Changing how long a watcher apply, a bulk walk or a
// backfill holds the lock is a change to this table and nothing else.
const fn hold(unit: Unit) -> Hold {
    match unit {
        Unit::WatcherApply => Hold::PerStep,     // embedding compute runs unlocked
        Unit::QueryTimeReindex => Hold::PerStep,
        Unit::BulkWalk => Hold::PerStep,         // NDJSON reads run unlocked
        Unit::Backfill => Hold::PerStep,         // inference runs unlocked
    }
}
```

`Writer` holds either `&Mutex` (`PerStep`: each op locks and releases) or a
guard taken when the unit opens (`WholeUnit`). A nested `Writer::unit` reuses
an outer held guard, otherwise applies its own policy. Every value in the
table reproduces today's behaviour; S2 must not change any hold.

### Where units open

- `watcher::apply::apply_file_change` / `apply_semantic_pass` take
  `&IndexStore` and open `Unit::WatcherApply`; `round_trip` takes
  `&mut Writer`. An `_in(&mut Writer, ...)` variant serves callers already
  inside a unit.
- `watcher::staleness::ensure_fresh` opens `Unit::QueryTimeReindex` around
  decide, the nested apply, and the baseline write.
- `bulk_index::walk_one_language` opens `Unit::BulkWalk`;
  `embedding::backfill::run` opens `Unit::Backfill`.
- The `bulk_index::HOLD_LOCK_FILE_ENV` test hook moves into `commit_batch`
  (it has to fire inside the hold); `bulk_index` re-exports the constant so
  `tests/handshake_independent_of_indexing.rs` keeps its import.
  `HOLD_COMPUTE_FILE_ENV` stays in `watcher/apply.rs`, between two `Writer`
  steps.

Layers above `watcher/`, `bulk_index` and `backfill` (registry, lifecycle,
plugin, semantic, workspace_reindex, activation, session, mcp) only pass
`&IndexStore` through and call single operations. They never open a unit.

### Lock order: stated once, checked at runtime

The `index_store.rs` module doc states the invariant: the connection is the
innermost lock; no code may take `supervisors`, `PluginSupervisor::inner`
or `PluginProcess::state`/`pending` while this thread holds the store. The
lifecycle and registry "Lock order" sections are cut down to a link to it.

A thread-local `HELD: Cell<bool>` is set by every store guard (`ReadGuard`,
`StoreGuard`, a `WholeUnit` writer, each `PerStep` step) and cleared on drop.
It gives two checks:

- **Re-entry** (always on): acquiring the store while `HELD` panics with a
  named message instead of self-deadlocking on the non-reentrant `std` mutex.
  That is what a `WholeUnit` closure calling `store.with(...)` would do.
- **Inversion** (`debug_assert!`): `storage::index_store::assert_not_held()`
  is called where the plugin-side locks are taken (the `inner`/`state`
  accessors in lifecycle and plugin, and `get_or_spawn` in registry). Any test
  that reaches an inverted path fails deterministically. It does not need the
  racing interleaving that a real deadlock needs.

Guards contain a `MutexGuard`, so they are `!Send` and cannot be held across
an `.await`; the thread-local stays correct under tokio.

### What stays `&Connection` (out of scope)

`graph/*`, the query internals under `mcp/` (`anchor::resolve`, `search`,
`continued`, ...), `storage::{schema, vectors, write}`,
`EmbeddingPipeline::store`, `last_used::touch`, and `cli/status.rs`'s own
connection. A handler does `let conn = store.read();` and passes `&conn`
down, as it does now.

### Tests

Only construction changes: `Mutex::new(conn)` becomes `IndexStore::new(conn)`,
`Arc::new(Mutex::new(conn))` becomes `Arc::new(IndexStore::new(conn))`,
helper signatures change from `&Mutex<Connection>` to `&IndexStore`, and a
`use` line is added. No construction appears inside an assert statement
(checked: 0 of 135). Six assert statements lock the connection inline
(`watcher/staleness.rs:962,965`, `daemon/semantic.rs:782,786,822`,
`tests/repeated_edits_through_a_warm_plugin.rs:206`). They stay
byte-identical because `IndexStore::lock()` keeps `.lock().unwrap()`'s shape
and poisoning semantics (decision 3). Test variables stay named `conn`.

### Migration order (suite green after every step)

**S2**

1. Add `storage/index_store.rs` with the full API and its own unit tests:
   policy table, re-entry panic, and the inversion `debug_assert` under
   `cfg(debug_assertions)`.
2. Mechanical type swap across the whole crate: every `&Mutex<Connection>`
   and `Arc<Mutex<Connection>>` (production, mcp handler signatures, test
   helpers, test constructions) becomes `&IndexStore`/`Arc<IndexStore>`,
   and every `conn.lock()` becomes `store.lock()`. This is a rename only and
   compiles in one pass. It has to cover `mcp/` too: `mcp/mod.rs` hands the
   same `Arc` to the registry (write) and to the handlers (read), and a
   handler taking `&Arc<Mutex<Connection>>` cannot be served from an
   `IndexStore`.
3. Replace the `.lock()` calls with operations, one module per commit, leaves
   first: `watcher/apply` → `watcher/staleness` → `daemon/plugin`,
   `daemon/lifecycle`, `daemon/registry` (`is_stale`) → `bulk_index` →
   `workspace_reindex` → `semantic` → `embedding/backfill` → `activation`,
   `cli/init`, `cli/reindex`, `cli/plugin_check/session`. Add the inversion
   checks with the lifecycle/plugin/registry commit.

**S3**

4. `mcp/*` handlers call `store.read()`, `mark_used` calls
   `store.with(last_used::touch)`, and `session.rs:563,581` call
   `store.read()`.
5. Restrict `lock()` to test use (decision 3). A grep shows zero production
   `.lock()` on the store outside `storage/`.

Files touched by both slices, in sequence and without overlap:
`mcp/*.rs` and their tests (step 2 type swap, step 4 bodies),
`mcp/mod.rs`, `cli/plugin_check/{session,mod,expectations}.rs`. Also outside
GM-407's `file_paths`: `cli/init.rs`, `cli/reindex.rs`,
`daemon/activation.rs`, `cli/plugin_check/{mod,expectations}.rs`. The
composition root (`daemon/mod.rs`) moves in S2 step 2, not in S3 as the
slice brief says.

## Consequences

**Easier.** A change to how long a lock is held becomes one edit to the
`hold()` table or to one operation body in `index_store.rs`. After S3,
`Mutex<Connection>` is named only in `storage/`: the composition roots call
`IndexStore::new`. Production `.lock()` sites outside `storage/` drop from
44 to 0 (target ≤ 5). Inversion and re-entry fail loudly in tests instead of
hanging.

**Harder / costs.**
- Step 2 is a wide mechanical diff, about 25 production files plus about 20
  test files. It will conflict with anything else in flight in `daemon/`,
  `watcher/` or `mcp/` this release, so land it quickly and rebase the
  others onto it.
- `storage` gains dependencies on `graph` (it already has one through
  `write.rs`) and on `embedding` (`store_vectors` calls
  `EmbeddingPipeline::store`, whose content re-check has to stay where it
  is). That is a module-level cycle `embedding ↔ storage`. Rust accepts it,
  but it is a layering smell.
- `ReadGuard` is read-only by convention only: rusqlite writes through
  `&Connection`.
- `with()` and `Writer::step()` run caller closures under the lock. Named ops
  are reserved for the holds whose extent matters (linking, vectors,
  batches, baselines). Closures are for single statements, and the inversion
  check backs that up.

### Replay plan for S5 (GM-396's change, reversed)

The change is to hold one lock from the start of the apply to the last
vector store: across the structural diff, its embedding compute and store,
the semantic-pass diff and its store, and, on the query-time path, the
staleness decision and baseline write. That is the pre-GM-396 behaviour.

- **Before (4dde9d5).** The lock is taken per step inside
  `watcher/apply.rs::round_trip` (twice per diff) and inside
  `watcher/staleness.rs::ensure_fresh` (decide, baseline). The minimal
  replay adds `&mut Connection` variants of `round_trip` and
  `apply_file_change`, and makes `staleness::ensure_fresh` lock once and call
  them. That is **2 files** (`watcher/apply.rs`, `watcher/staleness.rs`),
  2-3 new or changed private signatures, and no public signature changes.
  Restoring the signatures GM-396 removed (callers lock, pass
  `&mut Connection`) touches `watcher/apply.rs`, `watcher/staleness.rs`,
  `daemon/plugin.rs`, `cli/plugin_check/session.rs`, and the call sites in
  `watcher/apply/tests.rs` and `tests/protocol_conformance.rs`: **4 src files
  + 2 test files**.
- **After.** Flip `Unit::WatcherApply` and `Unit::QueryTimeReindex` to
  `Hold::WholeUnit` in `hold()`: **1 file**, 0 signatures. The re-entry
  check proves that no one-shot store call remains inside those units.
- Expected red on the replay branch, and it confirms the replay: the tests
  `incremental_embed_outside_lock` and `first_query_after_walk`
  (`HOLD_COMPUTE_FILE_ENV`).

### Risks

1. **Deadlock or inversion.** None exists today. After the change, foreign
   code can run under the lock only through `ReadGuard` (the MCP handlers),
   `with`/`step` closures, and `WholeUnit` closures. The inversion
   `debug_assert` covers all three in any test that reaches the path. The four
   concurrency tests named in GM-407 do not run an MCP handler concurrently
   with a replay, so without the tripwire they would not catch an inversion.
2. **Self-deadlock on re-entry.** It becomes possible the first time a policy
   flips to `WholeUnit`. The always-on check converts it to a panic, and the
   S5 replay exercises it.
3. **Long holds.** A `WholeUnit` watcher apply holds the lock across plugin
   I/O (up to the round-trip timeouts) and ONNX inference. The regressions
   that catch this are `incremental_embed_outside_lock`,
   `handshake_independent_of_indexing`, `first_query_after_walk` and
   `replay_progress`. None of them is in GM-407's list, and S4 should run
   them. The read-path `embed_query` under the guard predates this change;
   `ReadGuard`'s doc is where a future "no inference under the read guard"
   rule would go.
4. **Silent atomicity drift.** S2 must map each current guard scope to
   exactly one operation or step (the table above), never merging two
   neighbouring scopes or splitting one. Readers can observe the difference,
   for example `workspace_reindex`'s link plus roll-up is one hold today.
   The S4 verifier should diff hold boundaries against the site table.
5. **Poisoning.** `.lock().unwrap()` panics on a poisoned mutex today. The
   store keeps that behaviour and does not silently `into_inner()`.
6. **Test shim misuse.** Production code could call `IndexStore::lock()`. S4
   greps for it (decision 3).

### Open decisions for the owner

1. **Named ops or closures for bookkeeping.** About 12 one-statement
   `schema::*`, `staleness` and `backfill` calls go through
   `with()`/`step()` (proposed), not a forwarding method each. Forwarding
   methods would guarantee that no caller code runs under the lock, at the
   cost of about 12 thin methods and SQL moving into `storage/`.
2. **Inversion tripwire.** Add `assert_not_held()` at the plugin-lock
   acquisitions (proposed, debug only), or rely on the stated invariant
   alone.
3. **Test access and the six assert lines.** Keep a `#[doc(hidden)] pub fn
   lock()` so those lines stay byte-identical (proposed; `core/tests/` needs
   `pub`, and the crate has no test-support feature). The alternative is to
   rewrite them to `store.read()`, which changes the text of an assertion but
   not its meaning.
4. **Which "before" counts for S5.** The minimal replay today is 2 files, not
   the 5 GM-396 took. Should the before/after claim be 2→1 (minimal) or
   4+2→1 (GM-396-shaped)? Decide before S5 runs.
5. **Composition root in S2.** It moves into S2 step 2, not S3. The slice
   briefs need that edit, and `cli/init.rs`, `cli/reindex.rs` and
   `daemon/activation.rs` need adding to GM-407's `file_paths`.
6. **Accept the `storage ↔ embedding` module cycle** for `store_vectors`, or
   have `Writer::store_vectors` take a closure.
