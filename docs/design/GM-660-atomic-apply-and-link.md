# GM-660: apply and link a file's diff in one commit

Status: design (S1). No production or test change in this slice; the probe
used for the numbers below was reverted.
Background: `docs/design/GM-654-superseded-collapsed-call.md` section 5 (the
M1 diagnosis that found the pre-link sub-window) and its approach C (one
commit for reparse *and* semantic pass, rejected; not what this task does).

## The problem in one example

```ts
// lib.ts
export function parse(x: string): string;
export function parse(x: number): number;
export function parse(x: any) { return x; }
// use.ts
import { parse } from "./lib";
export function useString() { return parse("a"); }
```

The plugin does not know where `parse` lives, so `use.ts`'s structural
`CALLS` edge `useString -> parse` points at a **placeholder** node in
`use.ts` (`kind = Module`, `nativeKind = pending_symbol`). Core's linker
(`graph::symbol_links`) then moves the edge onto the real declaration in
`lib.ts` (`toId = <decl>`, `linkedFrom = <placeholder>`).

Every save of `use.ts` re-sends that edge on the placeholder. Core writes it,
then links it again. **Today those are separate SQLite commits**, so for a
few milliseconds the edge is on the placeholder again. A reader that lands
in that gap sees `useString` calling an unresolved `parse` (or, with
overloads, `parse` twice: the bound semantic edge on the declaration plus the
collapsed edge on the placeholder; GM-654's read-side filter correlates on
`toId`, so it cannot hide this pair).

## 1. Today

### The commit sequence

`Writer::apply_file_diff_linked` (`core/src/storage/index_store.rs`
398-415) runs one `Writer::step` (348-353). The step holds the
`IndexStore` mutex for its whole closure, and inside it:

| # | What | Where | Transaction |
|---|---|---|---|
| 0 | `file_rows::widen` (reads stored rows, widens the diff in memory) | `core/src/storage/file_rows.rs` | none (reads) |
| 1 | `write::apply_diff` | `core/src/storage/write.rs` 309-318 | its own `conn.transaction()` (314), **commit 1** (316) |
| 2 | `imports::link_diff` -> `imports::link` | `core/src/graph/imports.rs` 203 / 322-421 | its own `conn.transaction()` (328), **commit 2** (419); none when no placeholder is woken |
| 3 | `symbol_links::link_diff` -> `symbol_links::link` | `core/src/graph/symbol_links.rs` 772-811 / 1295-1386 | its own `conn.transaction()` (1303), **commit 3** (1380); none when nothing is pending |
| 4 | Gone file only: `file_rows::delete_indexed_file`, `schema::clear_gone_file_semantic_rows` (`schema.rs` 1714) | | autocommit statements |
| 5 | `IndexStore::claim` (in-memory) | | none |

Steps 1-3 are `apply_and_link` (102-105) and `link_applied` (108-117). The
same pair runs for `Writer::apply_diff_linked` (370-377) and, with
`apply_semantic_diff` (`write.rs` 345-363, its own transaction at 352)
instead of `apply_diff`, for `Writer::apply_semantic_diff_linked` (383-391).
So the semantic pass has the same gap: its edges sit on the pass's own
placeholders (GM-654 §5, `fd039a45…`) until commit 3.

### Who can see the gap (important correction)

**In-daemon MCP readers cannot.** Every MCP tool reads through
`IndexStore::read()` (`find_references(IndexStore::read)`: `mcp/find_callers_callees.rs`,
`find_references.rs`, `find_definition.rs`, `find_implementations.rs`,
`get_dependencies.rs`, `get_file_outline.rs`, `search_code.rs`, `mcp/mod.rs`,
plus `cli/plugin_check/session.rs`). `read()` takes the **same mutex** the
step holds across commits 1-3, and the daemon has exactly one index
connection (`daemon/mod.rs` 349 opens it, 409 wraps it; no non-test
`Connection::open` under `core/src/mcp`, `shim*` or `ipc`). An MCP query
therefore runs wholly before commit 1 or wholly after commit 3. GM-654 §5's
"`find_callees(useString)` lists `parse` twice in the sub-window" is true of
the store's committed state, not of an MCP answer.

**Readers on other SQLite connections can** (WAL shows each commit at once):

- `g-mesh deps` (`cli/deps.rs` 107/355, `open_read_only`, another process
  while the daemon runs): exports the dependency graph, so an export taken
  in the gap has the file's usages unlinked.
- `g-mesh status` (`cli/status.rs` 506 `open_index`) and `gc::last_used`:
  counts only, no edge semantics; harmless.
- Integration tests that read the raw store (`superseded_collapsed_call.rs`
  `edges_onto_parse` / `calls_from`): that is how M1 flaked.
- Any future read path on its own connection (e.g. a reader pool so queries
  stop blocking writers): the gap would then become MCP-visible.

### Callers of `apply_file_diff_linked` (all paths)

`find_callers(apply_file_diff_linked)` (g-mesh, complete):
`watcher::apply::round_trip`, `daemon::bulk_index::purge_language`,
`daemon::registry::PluginRegistry::retire_file`, and a unit test.
`round_trip` (`watcher/apply.rs` 901-971) is reached from
`apply_file_change_in` and `apply_semantic_pass_in`, which come from:

| Path | Chain (g-mesh `find_callers`, each complete) |
|---|---|
| Watcher save | `daemon::lifecycle` -> `PluginProcess::apply_file_change` -> `watcher::apply::apply_file_change` -> `apply_file_change_in` -> `round_trip` |
| Query-time reindex | `daemon::lifecycle` -> `PluginProcess::ensure_fresh` -> `watcher::staleness::ensure_fresh` -> `apply_file_change_in` |
| Config reindex | `daemon::config_reindex` -> `PluginProcess::reextract` -> `reextract_file` -> `apply_file_change_in` |
| Affected re-extracts | `apply_file_change_in` -> `reextract_affected` -> `apply_file_change_in` |
| Semantic passes | `apply_semantic_pass` / `apply_residual_semantic_pass` / `apply_scoped_semantic_pass` / `apply_file_change_in` -> `apply_semantic_pass_in` -> `round_trip` -> `apply_semantic_diff_linked` |
| Gone file, no round trip | `purge_language`, `retire_file` |

The per-language workspace reindex writes into a staging store
(`workspace_reindex.rs` 416) that readers never see until its swap, so it is
not affected.

### Can the linkers run inside the apply transaction?

Yes. `imports::link_diff` and `symbol_links::link_diff` read only through the
`conn` they are given (a connection sees its own uncommitted writes), call no
plugin, and open no other connection. They take `&mut Connection` only to
call `conn.transaction()`. The obstacle is purely mechanical: rusqlite's
`transaction()` issues `BEGIN DEFERRED`, which SQLite refuses inside an open
transaction. `apply_diff` / `apply_semantic_diff` have the same shape, and
their helpers (`write_diff_unless_empty` 365, `stored_node_ids` 374,
`write_diff` 392, `replace_untyped_calls` 622) are typed `&Transaction<'_>`.

## 2. Options

### Measurements (probe, reverted)

A throwaway `#[ignore]` unit test opened a copy of the cached g-mesh index
(`912c53c`, 35,685 nodes, 96,331 edges; the cache itself was only read), and
for 11 files (top two and median per language by linked edges) re-sent each
file's edges as the plugin does (`toId = COALESCE(linkedFrom, toId)`,
unresolved), then:

- **A (today):** `apply_diff`, `imports::link_diff`, `symbol_links::link_diff`,
  three commits;
- **B (option a):** raw `BEGIN`, the same three calls with their inner
  `transaction()` changed to `savepoint()`, `COMMIT`.

A second connection counted the file's edges on `pending_symbol`
placeholders after step 1 (`P0` = settled count). 5 rounds per arm, median,
two runs. Debug build, `synchronous = FULL` (WAL default, as the daemon
runs). Machine heavily loaded: `uptime` load 60-150 during the runs; the
test binary ran `real 23.85 / user 11.47 / sys 4.76` and `real 24.42 /
user 12.03 / sys 4.85`, so roughly half the wall time was waiting, and the
absolute numbers are inflated. Run 1:

| file | edges | linked | A apply | A imports | A symbols | **A total** | **B total** | P0 | A mid | B mid |
|---|---|---|---|---|---|---|---|---|---|---|
| plugins/sdk/src/lsp/bridge.rs | 1659 | 833 | 309 ms | 0.01 | 260 ms | 569 ms | 554 ms | 50 | **883** | 50 |
| plugins/sdk/tests/lsp_bridge.rs | 1856 | 607 | 339 ms | 0.01 | 179 ms | 518 ms | 492 ms | 136 | **743** | 136 |
| plugins/go/semantic.go | 648 | 150 | 81 ms | 0.01 | 97 ms | 177 ms | 268 ms | 203 | **353** | 203 |
| plugins/go/uses.go | 273 | 137 | 32 ms | 0.01 | 57 ms | 89 ms | 95 ms | 93 | **230** | 93 |
| core/src/cli/status/json.rs (median rust) | 36 | 25 | 6.2 ms | 0.01 | 10.9 ms | 17.0 ms | 21.6 ms | 0 | **25** | 0 |
| ts conformance main.ts | 19 | 7 | 3.9 ms | 0.01 | 5.5 ms | 9.5 ms | 10.1 ms | 0 | **7** | 0 |
| ts conformance esm/use.ts | 8 | 2 | 1.9 ms | 0.01 | 3.0 ms | 4.9 ms | 7.0 ms | 0 | **2** | 0 |

Run 2 had the same shape (bridge.rs 615 vs 652 ms, lsp_bridge.rs 332 vs
283 ms, semantic.go 246 vs 330 ms, json.rs 12.6 vs 14.9 ms, use.ts 3.3 vs
3.0 ms). In every round the final edge state equalled the settled one in
both arms (`same = true`).

What this shows:

- **The gap is real on other connections, on every file with usages:** A's
  mid count is above `P0` for all 11 files; B's mid count equals `P0`
  (readers keep the pre-commit snapshot) for all 11.
- **Its length** is apply + link, about 5-20 ms on a median file and
  0.2-0.6 s on the largest files in this debug, loaded run.
- **B costs about the same as A**: the differences go both ways, ±20-50%,
  inside the noise of a load-60-150 machine. One commit saves two fsyncs
  and adds nothing. The imports step is ~0 here because the probe re-sent
  edges only (no node upserts, so no import placeholder was woken); in a
  real reparse it is one more short statement batch.
- The probe also checked the failure shape: an inner savepoint dropped
  without release rolled back only itself, and the outer `COMMIT` kept the
  applied diff (353 placeholder edges committed, i.e. "apply kept, link not
  done", today's outcome for a link error).

The release measure slice should repeat this on a release build and a quiet
machine with whole-file diffs (nodes too), and record `uptime` and
`/usr/bin/time -p` beside it.

### (a) One transaction for apply + link_diff + link (recommended)

**Today:** three commits under one mutex hold. **Change:** the step opens
one outer transaction around everything it writes; the four inner
`conn.transaction()` sites become `conn.savepoint()`. **Example:** a save of
`use.ts` becomes visible to `g-mesh deps` as one change, with the call
already on `lib.ts#parse`. **Consequence:**

- *Reader-visible atomicity under WAL:* a reader on another connection sees
  the state before the outer `BEGIN` until `COMMIT`, then everything. The
  in-daemon readers were already serialized by the mutex and stay so.
- *Writer lock:* the `IndexStore` mutex is held exactly as long as today
  (the step already spans all three). The SQLite write lock is now held for
  that same span instead of three slices of it; the daemon's connection is
  the only writer while it runs, so nobody waits on it. No plugin round trip
  is inside it (the step contains none today, and this does not add one).
  Measured cost: same as today within noise (table above), minus two fsyncs.
- *Standalone callers unchanged:* outside a transaction, `SAVEPOINT` behaves
  as `BEGIN DEFERRED` and `RELEASE` as `COMMIT` (SQLite docs), and
  rusqlite's `Savepoint` rolls back on drop like `Transaction`
  (`DropBehavior::Rollback` -> `ROLLBACK TO` + `RELEASE`). Every other caller
  of `apply_diff` (bulk `commit_batch_on`, `watcher::burst`,
  `graph::queries` helpers, ~150 tests) and of `imports::link` /
  `symbol_links::link` (`link_all`) keeps exactly today's behaviour.
- *Failure/rollback:* each inner unit still rolls itself back on error (its
  savepoint). The outer transaction is then **committed anyway**, and the
  error is returned as today. Result for each failure: apply fails ->
  nothing written (today: same); `imports` fails -> apply kept, nothing
  linked (today: same); `symbols` fails -> apply + import links kept (today:
  same). Never worse than today. Only if the final `COMMIT` itself fails (I/O,
  disk full) does the outer `ROLLBACK` drop the applied diff too, where today
  commit 1 might already have landed; that is a failure of the write itself,
  reported as an error, after which the file's next save re-sends it.
  The same holds if SQLite itself rolls back the whole transaction during
  linking (`SQLITE_FULL`, `SQLITE_IOERR`, `SQLITE_NOMEM`): the applied diff
  is lost too, nothing is written, and `claim` is not called.
- *Panic inside the step:* the store mutex is poisoned and the daemon's
  store is never used again (`a_poisoned_store_stays_poisoned`), so an open
  outer transaction is moot; same as an inner panic today.
- *GM-654's filter:* unchanged and still needed. It covers the reparse ->
  semantic-pass window, which stays open by design (approach C rejected);
  inside that window both edges are already on the declaration, which is
  what the filter correlates on. This change removes the only state the
  filter could not handle (collapsed edge on the placeholder).
- *Per-file semantic pass:* unchanged. `apply_semantic_pass_in` reads
  `linked_edges` after the step returns, i.e. after the single commit, so
  it still sends the plugin the links of exactly the text it answers for.
  The pass's own `apply_semantic_diff_linked` gains the same atomicity.

### (b) Staged visibility (write to staging, then swap)

**Change:** write the file's diff into a staging area (attached database or
shadow tables), link there, then copy into the live tables in one
transaction; or tag rows with a generation and filter readers by the last
committed generation. **Consequence:** the copy/swap is itself one
transaction over the same rows, i.e. (a) plus a second write of every row;
linking in staging needs the rest of the graph (declarations in other
files), so it would read across both. A generation column changes every
query. More code, more writes, same guarantee. Rejected.

### (c) Cheaper alternatives found

- **c1. No production change.** In-daemon MCP answers never show the gap
  (§1). Treat it as "out-of-process readers and tests only", keep tests
  reading the raw store tolerant of it (as GM-654 S11 did). Cost: zero.
  Risk: `g-mesh deps` exports and future reader pools can still see
  unlinked usages; the acceptance criterion is not met.
- **c2. Outer transaction only in `apply_and_link`, inner code untouched,
  using `unchecked_transaction()`:** not possible; the inner
  `transaction()` calls fail inside it. Changing them to savepoints is (a).

**Recommendation: (a).** It is four one-line changes plus one wrapper, keeps
today's failure outcomes, needs no plugin round trip inside a lock, and
costs nothing measurable. c1 is the fallback if the owner judges
out-of-process readers not worth a write-path change.

## 3. Test hook

**Where a hold can be injected today:** nowhere between commits 1 and 3.
`HOLD_COMPUTE_FILE_ENV` (`hold_compute_open_for_tests`, `apply.rs`
1013-1028, called in `round_trip` after the step) holds *after* commit 3,
and `HOLD_LOCK_FILE_ENV` only in bulk `commit_batch_on`. A new test-only
hook is needed.

**Proposed hook:** a `#[cfg(test)]` thread-local callback in
`index_store.rs`, called in `apply_and_link` between `apply_diff` and
`link_applied` (and in `apply_semantic_diff_linked` at the same point).
`#[cfg(test)]` so nothing ships; thread-local so parallel unit tests do not
see each other's hooks; a callback, not an env sleep, so the test is
deterministic (no processes, threads or timers).

**The test** (`index_store.rs` tests or `watcher/apply/tests.rs`):

1. File-backed temp db (WAL, as `connection::open`), `IndexStore` over it,
   plus a second raw `Connection` (the out-of-process reader).
2. Seed `lib.ts` (declaration `parse`) and `use.ts` (function `useString`,
   placeholder `parse`, `CALLS` edge onto the placeholder) and link, so the
   edge sits on the declaration with `linkedFrom` set. Snapshot `(toId,
   resolved, linkedFrom)` of `use.ts`'s edges.
3. Install the hook: it reads `use.ts`'s edges through the second
   connection and records them.
4. `apply_file_diff_linked` with a `Partial` diff re-sending the edge on the
   placeholder (what every save does).
5. Assert the hook's reading equals the snapshot (no edge on a
   `pending_symbol` node), and the state after the call equals it too.

Today step 5 fails: the hook sees the edge on the placeholder
(`resolved = 0`, `linkedFrom NULL`). With (a) the hook point lies inside
the outer transaction, so the second connection sees the snapshot. A
second case drives `apply_semantic_diff_linked` the same way.

**Control:** in a throwaway worktree, remove the outer transaction wrapper
(code, not test; inner savepoints left as they are, since they behave like
today's transactions standalone) and the test must fail on the hook
assertion. A second, cheaper control for the failure rule: make the wrapper
roll back instead of commit on a link error; a unit test that injects a
link failure (e.g. via the same hook dropping a table the linker needs, or
a `cfg(test)` failure flag) and asserts the applied diff is kept must fail.

The M1 it test (`superseded_collapsed_call.rs`) needs no change; with (a)
its raw-store reads can no longer land in the sub-window.

## 4. Edit map

| # | File | fn (lines today) | Change |
|---|---|---|---|
| 1 | `core/src/storage/write.rs` | `apply_diff` (309-318) | `conn.transaction()` -> `conn.savepoint()` (314); `.commit()` stays |
| 2 | `core/src/storage/write.rs` | `apply_semantic_diff` (345-363) | same at 352 |
| 3 | `core/src/storage/write.rs` | `write_diff_unless_empty` 365, `stored_node_ids` 374, `write_diff` 392, `replace_untyped_calls` 622; `use` line 4 | `tx: &Transaction<'_>` -> `tx: &Connection`; drop the `Transaction` import (probe compiled with exactly this) |
| 4 | `core/src/graph/imports.rs` | `link` (322-421) | `conn.transaction()` -> `conn.savepoint()` (328) |
| 5 | `core/src/graph/symbol_links.rs` | `link` (1295-1386) | same at 1303 |
| 6 | `core/src/storage/index_store.rs` | new `in_one_commit(conn, f)` near `apply_and_link` (102) | `BEGIN`; `let r = f(conn)`; `COMMIT` (on `COMMIT` error: `ROLLBACK`, return that error); return `r`. Doc: inner units are savepoints, so committing after an inner error keeps exactly what today's per-step commits kept |
| 7 | `core/src/storage/index_store.rs` | `apply_diff_linked` (370-377), `apply_semantic_diff_linked` (383-391), `apply_file_diff_linked` (398-415) | wrap the step's writes (widen + apply + link + Gone deletes) in `in_one_commit`; `claim` after it, on success only, as today |
| 8 | `core/src/storage/index_store.rs` | `apply_and_link` (102-105), `link_applied` (108-117) | call the `#[cfg(test)]` hook between apply and link; update the "After the commit" comments (now "after the write, in the same commit") |
| 9 | `core/src/watcher/apply.rs` | `round_trip` comments (946-948, 959-960) | "committed already" stays true; no code change |
| 10 | tests | `index_store.rs` tests | the hook test (§3), the failure-rule test, and one asserting a standalone `apply_diff` outside any transaction still commits (savepoint-as-transaction) |

Not touched: `commit_batch_on` / `link_all` (bulk walk: batches are linked
once at the end of a walk, a different and longer window, reported as
"indexing"); `sweep_semantic_edges` (`apply.rs` 787-808, its own
transaction, called outside the step); the staging reindex.

## 5. Must-confirm list

1. No caller reaches `apply_diff`, `apply_semantic_diff`, `imports::link` or
   `symbol_links::link` while already inside a rusqlite `Transaction` it
   relies on committing (today that would fail, so none should; confirm
   with `find_callers` on the branch after the edit).
2. `file_rows::widen`, `delete_indexed_file` and
   `clear_gone_file_semantic_rows` open no transaction of their own (grep
   found none in `file_rows.rs`; `schema.rs` 1714 is plain statements).
3. `containers::detach`/`attach` (`&Connection`) still compile and run
   inside a savepoint (the probe compiled all of `write.rs` with
   `&Connection`; confirm under the full suite).
4. `IndexStore::claim` still runs only after a successful write, so a
   failed step claims nothing (today's behaviour).
5. Release-build measurement of A vs B on a quiet machine, whole-file
   diffs (nodes too), in the measure slice; record `uptime` and
   `/usr/bin/time -p`.
6. The ~150 test callers of `apply_diff` pass unchanged (they call it
   standalone).

## Cross-file answers relied on

All g-mesh calls used `project: "g-mesh"` (the worktree has no code change
yet, HEAD = release-4.19.0 = main checkout).

| Question | g-mesh call | Answer |
|---|---|---|
| The step's body | `find_definition(apply_file_diff_linked)` | `index_store.rs` 398-415, one `self.step` |
| Who calls it | `find_callers(apply_file_diff_linked)`, complete | `round_trip`, `purge_language`, `retire_file`, a test |
| Who calls `round_trip` | `find_callers(round_trip)`, complete | `apply_file_change_in`, `apply_semantic_pass_in` |
| Paths into those | `find_callers` on `apply_file_change_in`, `apply_semantic_pass_in`, `apply_file_change`, `reextract_file`, `ensure_fresh`, `PluginProcess::{apply_file_change,ensure_fresh,reextract}`, all complete | table in §1 |
| Other users of the two Writer methods | `find_callers(apply_diff_linked)`, `find_callers(apply_semantic_diff_linked)`, complete | `round_trip` + tests |
| Callers of the inner writers/linkers | `find_callers(storage::write::apply_diff)` (files + rows), `imports::link_diff`, `symbol_links::link_diff`, `imports::link`, `symbol_links::link`, all complete | §4 "Standalone callers unchanged" |
| Who reads through the store mutex | `find_references(IndexStore::read)`, complete | the MCP tool modules listed in §1 |

Grep (single known files or non-call questions): `transaction()`/`commit()`
sites in `write.rs`, `imports.rs`, `symbol_links.rs`; `Connection::open`
and `connection::open` across `core/src` (to find every index connection);
`Transaction` in `write.rs`; rusqlite 0.31 `transaction.rs` for
`Savepoint`'s drop behaviour.

## 6. Write cost diagnosis

The verify measurement showed the one-commit write about 16% slower per call,
with system time doubled even though it makes fewer commits. The cause is
SQLite's statement sub-journal, which it writes to a temporary file.

### Setup

The scratch harness from verify (kept outside the repo) times 1600
`apply_file_diff_linked` calls: 400 files x 4 saves, 15 functions per file,
3 calls each onto pending placeholders, `FileScope::Complete`. The index is a
file-backed WAL database with foreign keys off, the same pragmas as
`connection::open`. Two new probes were added to the harness, not to the
product: a VFS shim that counts `xOpen` by file kind, and
`sqlite3_db_status`. The old arm is release-4.19.0 at `0a32898`; the new arm
is this branch. Both are release builds. There were 3 pairs per variant, run
in alternating order (old/new, new/old, old/new), with `/usr/bin/time -l` for
each run. Load average ranged from 11 to 94 over the 13-minute run. The
machine is shared, so the absolute numbers are inflated, but the alternating
order cancels drift.

Pragmas in effect (both arms): `synchronous=FULL`, `journal_mode=wal`,
`wal_autocheckpoint=1000`, `cache_size=-2000`, `mmap_size=0`,
`temp_store=DEFAULT` (the bundled build sets no `SQLITE_TEMP_STORE`, so
temporary files go to disk), `cache_spill=483`.

### Results

| variant | arm | median ms (p1/p2/p3) | sys s | user s | sub-journal opens | pages written (`CACHE_WRITE`) |
|---|---|---|---|---|---|---|
| default | old | 39.7 / 30.9 / 29.7 | 13.6 / 10.6 / 10.7 | 35-45 | 0 | 310 495 |
| default | new | 47.8 / 39.3 / 44.3 | 26.1 / 21.8 / 23.6 | 36-43 | **1600** | 264 460 |
| `temp_store=MEMORY` | old | 32.2 / 33.0 / 31.9 | 11.3 / 11.3 / 10.9 | 35-38 | 0 | 310 495 |
| `temp_store=MEMORY` | new | 31.1 / 34.0 / 34.2 | 9.2 / 9.8 / 9.7 | 38-40 | 0 | 264 460 |

The dry-run pair agrees (old 38.3 ms and 12.8 s sys; new 55.1 ms and
28.8 s sys). Other counters:
- Page faults and block I/O were 0 in every run.
- WAL size was the same in both arms (max 4.93 MB old, 4.94 MB new). The WAL
  is never truncated, so checkpoints do not show in its size.
- No spills from the page cache (`CACHE_SPILL` 0).
- The new arm has about 10 000 fewer voluntary context switches (about 68k
  against 78k). That fits one commit and one WAL sync per call instead of
  three.
- Peak RSS is 10-11 MB, or 15-16 MB for the new arm with `MEMORY`.

### Cause

Inside the outer `BEGIN`, each inner unit is now a `SAVEPOINT`. A savepoint
nested in an open transaction makes SQLite copy every page the unit changes
for the first time into a statement sub-journal, so that this unit alone can
be rolled back. Before this change each unit was its own transaction, so
rollback used the WAL and no sub-journal was written. The sub-journal is kept
in memory until it reaches `SQLITE_STMTJRNL_SPILL` (64 KiB). Past that it
moves to a temporary file in `$TMPDIR` that is created, unlinked, written and
closed again for every write transaction. The counting shim shows exactly
1600 sub-journal opens in the new arm, one per call, and none in the old arm.
That is about 12 s of extra system time, roughly 7.5 ms per call under this
load. The commit itself is cheaper: 15% fewer pages written and fewer
syncs. The file churn outweighs that saving.

### Recommendation: cheap fix, `PRAGMA temp_store = MEMORY` in `connection::open`

The pragma keeps the sub-journal in memory. With it the new arm has no file
opens. Its system time falls below the old arm's (9.5 s against 11.2 s on
average), and its median per call is within noise of the old arm (+2% on the
pair averages, 33.1 against 32.4 ms; the pairs go -1.1, +1.0 and +2.3 ms). It
removes the whole regression. The old arm itself also gains about 0.5 s of
system time under the pragma.

The pragma does more than move the sub-journal:
- Every temporary structure on this connection moves to RAM: sorter spills,
  temporary indexes for `ORDER BY`/`GROUP BY`/`DISTINCT` without an index,
  `CREATE TEMP TABLE`, and VACUUM's temporary database. The production code
  has no VACUUM and no temporary tables (the one `CREATE TEMP TABLE` is in a
  test). A very large sort or rebuild would now use memory instead of disk.
- Sub-journal memory is bounded by the pages one Writer step touches. That
  was about 5 MB of peak RSS here. A large single-file diff costs
  proportionally more, and only for the length of that commit.
- The pragma is per connection, so it must be set wherever a writing
  connection is opened (`connection::open`). The staging connection already
  keeps its rollback journal in memory.

An alternative that changes nothing at run time is to rebuild the bundled
SQLite with a larger `-DSQLITE_STMTJRNL_SPILL` (via `LIBSQLITE3_FLAGS`). That
is a build-wide setting, harder to see, and still spills past its threshold.
It is not recommended over the pragma.

If the fix were declined: the cost without it is about +10 ms median and
+7.5 ms system time per file save at this machine's load (about +30% per
call). It hits every caller listed in section 1 (watcher save, query-time reindex, language purge).
