# GM-742: the 10k un-ignore guard and the reconcile paths

Status: design (S1), for owner approval. Base: `release-4.36.0` (b26815ff).

## 1. Verdict

**A real guard bypass, not a wrong test assumption.** The 10,000-file
un-ignore guard (GM-508 Q2) is applied in two places only: the `.gitignore`
gate (`PluginRegistry::gitignore_changed`) and, since GM-727 (a0074555), the
starvation sweep (`ReconcileScope::Starved`). Two other reconciles walk the
project with the *current* `.gitignore` rules and index every walked-but-not-
live file as "created", with no guard:

- **P1, dropped-event Rescan** (`ReconcileScope::Rescan`): the path the
  failing test hits under load.
- **P2, daemon restart** (`ReconcileScope::Start`, `ConsumerStart::Reconcile`):
  not exercised by the test, same bypass. The start's `.gitignore` recheck
  trips the guard and logs, then the plain reconcile right after it indexes
  the same files.

The test's expectations ("nothing under `generated/` is indexed", "exactly
one guard line") are what GM-508 section 6.8 / Q2 specified. The second
expectation can also be broken by P1 independently of the bypass: a Rescan
whose `.gitignore` watermark predates the edit re-runs the gate, which logs
the guard line a second time (section 3.3).

### Reproduction: not reproduced (26 runs); the bypass is shown by the code

| run | extra load | runs | failed | Rescan lines | load avg (1 min) at run start |
|---|---|---|---|---|---|
| A | none (other sessions only) | 4 | 0 | 0 | 38 -> 25 |
| B | 24 x `yes` on 8 cores | 10 | 0 | 0 | 21 -> 632 |
| C | 8 x `yes` + 4 file churners (2000 create+delete per loop) | 12 | 0 | 0 | 303 -> 714 |

Each run: `cargo nextest run -p g-mesh --test it -E
'test(=gitignore_reevaluation::un_ignoring_more_files_than_the_guard_logs_once_and_reindexes_nothing)'`,
with a temporary (reverted, uncommitted) `eprintln!` of the daemon log's
`rescan|reindex|reconciled|starv|.gitignore` lines and the count of indexed
files under `generated/`. Every run logged exactly one guard line, no
`dropped-event rescan`, no starvation sweep, `generated_indexed=0`; wall
8-41 s per run (`time -p`: e.g. run B10 real 51.06, user 1.31, sys 5.16 -
the cargo/nextest parent mostly waiting). Logs:
`scratchpad/gm742s1/{summary,summary2}.txt`, `idle-*.log`, `loaded-*.log`,
`churn-*.log`.

CPU load and a user-space file churn did not make FSEvents report
`MustScanSubDirs` for the test's stream. The GM-727 failures (S2: 2 runs at
load 100-260; S7: 1 run at load ~146) happened while the whole `it` binary
(37 tests, several daemons, 10k-file fixtures) ran in parallel; their logs
were not kept, so the attribution to a Rescan is GM-727's reading, not
something this slice re-observed. Ruled out as the cause:

- the starvation sweep: guarded since a0074555 and logged nothing here;
- the cold-start walk: the test starts on an empty index (`AfterWalk`), so
  no `Start` reconcile runs, and `.gitignore` is emptied only after
  `connect` awaited the walk and a probe;
- the event path for the un-ignored files: they were created before the
  daemon started, so no event names them.

What remains is P1, which needs only an OS Rescan of the root (or of an
ancestor of `generated/`) after the `.gitignore` write. That is shown
deterministically below and is pinned by S4 with an injected Rescan
(`ProjectWatcher::inject_rescan`), not by load.

## 2. What the guard guarantees

GM-508 (`docs/architecture/gm-508-gitignore-reevaluation.md` l.133-138,
Q2 l.238-245, owner: "Порог 10 000 файлов на язык (Recommended)"): when a
`.gitignore` or link change would add more than 10,000 files of one
language to the index, the daemon indexes **none** of them automatically,
logs **one** line naming the count and `g-mesh reindex`, and leaves it to the
user. Purpose: no surprise multi-minute stall and no unasked index growth
(example: un-ignoring `third_party/` with 40,000 `.ts` files). Behaviour 8
of that doc: "Above the guard, no reindex and one log line." No ADR covers
it (`ls docs/adr | grep -i ignore`: nothing). GM-727 extended it to the
starvation sweep ("guard and count starvation sweeps").

The guarantee is about **un-ignored files**, not about any burst of new
files: a 15k-file `git checkout` whose events arrive is indexed file by
file through the watcher, and must stay so.

## 3. Code path

### 3.1 P1: dropped-event Rescan

1. `forward_event` (`core/src/watcher/mod.rs:467-480`): an event with
   `need_rescan()` becomes `WatchSignal::Rescan(path)` (root when pathless).
2. `watch_and_route_once` (`core/src/daemon/watch_loop.rs:258-338`): records
   it in `rescans.scheduler` (l.281-283); after the batch and the batch's own
   gate (l.328-330), when `scheduler.due()` (l.331-336), swaps the
   `gitignores_since` watermark and calls `rescan_subtrees`.
3. `PluginRegistry::rescan_subtrees` (`core/src/daemon/registry.rs:1266-1274`)
   -> `reload_and_reconcile` (l.1293-1307):
   - reloads the ignore layers (the emptied `.gitignore` now applies);
   - `gitignores_newer_than(gitignores_since)`; when non-empty,
     `gitignore_changed` runs: guard trips, logs, skips the reindex;
   - **then always** `reconcile_counted(Plain, Rescan(subtrees))`.
4. `reconcile_counted` (l.1364-1554): walks `subtrees` with the new rules
   (l.1397-1406); `generated/f*.ts` are walked and not live, so they land in
   `created` (l.1418-1419). The guard block (l.1420-1446) is
   `if scope == ReconcileScope::Starved` only. `announce_created` and
   `ensure_fresh_with` for each created file (l.1481-1495) index all 10,001.

### 3.2 P2: daemon restart on an indexed project

`spawn_watch_consumer` (`watch_loop.rs:99-161`), `ConsumerStart::Reconcile`
(l.117-126): `recheck_gitignores_since_index` (registry.rs:1211-1231; the
`.gitignore` is newer than `bulkIndexedAt`, so the gate runs and the guard
logs), then `reconcile_since_index(Plain, Start)` -> `reconcile_counted`,
whose `Start` scope walks the whole project (l.1398-1400) and, unguarded,
indexes the same files. So after an over-guard un-ignore, the next daemon
start (idle exit, `g-mesh stop`, a crash) indexes them without `g-mesh
reindex`. Not covered by any test today.

### 3.3 The second guard line

The Rescans watermark is taken at consumer start (`Rescans::new`,
`gitignore_watermark()` = now - 1 s, `watch_loop.rs:195-196, 225-227`) and
advanced only by a Rescan run or a sweep. The test's `.gitignore` write is
after that, so the first Rescan after it re-runs the gate, which logs again
(`over_reindex_guard`, registry.rs:205-214). The watermark doc already
accepts "rechecking a file once more, which the gate absorbs"; it absorbs the
reindex but not the log line. A fix that stops the bypass but keeps this
re-log still fails assertion 1 of the test when a Rescan lands after the
edit.

### 3.4 g-mesh calls behind the cross-file answers

The g-mesh index answered for code shared with `main`; it did not know
`reload_and_reconcile` (added on the release branch: `find_callers` returned
`semanticNeighbours`, no rows), and its line numbers lagged the worktree by
~20 lines, so lines above are from `sed`/`grep` on the worktree. No
`select_project` tool was exposed in this session.

| question | call | answer |
|---|---|---|
| what references the 10k constant | `find_references daemon::registry::GITIGNORE_REINDEX_GUARD` | `gitignore_changed` only (index predates a0074555); grep on the worktree adds `over_reindex_guard` (l.206, 211), the `Starved` block via `over_reindex_guard` (l.1433) and two tests (`registry/tests.rs:1458, 1744`) |
| who calls the guard check | `find_callers gitignore_changed` | `recheck_gitignores_since_index`, `rescan_subtrees` (now via `reload_and_reconcile`, grep l.1300), `watch_and_route_once`, 11 tests; `over_reindex_guard` callers by grep: `gitignore_changed` l.1193, `reconcile_counted` l.1433 |
| Rescan into indexing | `find_callers rescan_subtrees` | `watch_and_route_once` only; then `reload_and_reconcile` -> `reconcile_counted` (grep) |
| Starved sweep into indexing | grep `starvation_sweep` | `watch_for_starvation` (`watch_loop.rs:387-388`) -> `reload_and_reconcile` -> `reconcile_counted` (guarded) |
| locating the guard | `search_code "gitignore un-ignore guard limit 10000"` | `GITIGNORE_REINDEX_GUARD`, `recheck_gitignores_since_index`, the integration test |

## 4. Proposed fix

### A (recommended): remember the files an over-guard gate left out

`gitignore_changed` already computes, per language, the exact set it would
add (`now.difference(indexed)`). Of that set, the files the edit un-ignored
are those whose mtime is not later than the edit
(`PluginRegistry::predates_unignore`); a file newer than the edit was
created after it and is a genuine new file, which a live daemon indexes by
its own event. The guard counts, logs and defers only the files that predate
the edit:

- a `.gitignore`'s edit time is the later of its mtime and, on Unix, its
  ctime (`gitignore_edit_time`): `mv`, `cp -p`, `rsync -a` and `tar x` keep
  an old mtime but set the ctime. Off Unix it is the mtime alone. The same
  edit time decides which `.gitignore` files the restart recheck and a
  Rescan pass to the gate (`gitignores_newer_than`), so a `.gitignore`
  moved in while the daemon was down is rechecked;
- the edit's time for a file is the latest edit time among the triggering
  `.gitignore` files in its own or an ancestor directory (several
  `.gitignore` files in one gate run: each file takes the ones governing its
  subtree);
- a file is counted as un-ignored (deferred) when its own mtime cannot be
  read, when a governing `.gitignore` has no readable mtime (deleted), or
  when no triggering `.gitignore` governs it (a gate triggered by links
  only);
- only when more than the guard predate the edit does the language trip it;
  the newer files are then left to their own events and to the next plain
  reconcile, which indexes them as created. Below the guard the language is
  reindexed whole as before;
- the stat runs only for a language whose whole difference exceeds the
  guard.

When the guard trips, keep the predating set in the
registry (`deferred_unignores: Mutex<BTreeMap<String /*language*/,
HashSet<String>>>`); `reconcile_counted` in `Plain` mode (every scope:
`Start`, `Rescan`, `Starved`) drops created files that are in it, logging
nothing more. Bookkeeping:

- a gate run that compares a language and does **not** trip the guard
  (reindexes it, or finds no difference) removes that language's deferred
  paths under the subtrees it compared (a whole-language reindex indexes or
  re-ignores them anyway);
- a gate run that trips the guard again for a set already deferred (same
  language, same count) does not log again, so "one log line" holds across
  Rescans (section 3.3);
- the restart recheck compares `.gitignore` mtimes with a separate
  `.gitignore` baseline (table `gitignore_baseline`, one row), not with
  `meta.bulkIndexedAt`: every per-language swap (`language_swap.rs`
  `swap_attached` -> `record_bulk_index`) moves `bulkIndexedAt` to now, so a
  recheck against it would skip the gate after any language's reindex and
  the `Start` reconcile would index the un-ignored files. Only a full walk
  writes the baseline (`record_gitignore_baseline`: daemon cold start in
  `activation.rs`, `g-mesh init`, `g-mesh reindex`); a per-language swap,
  a config reindex and an owed reindex do not. An index without the row
  (built before the table existed) adopts its `bulkIndexedAt` as the
  baseline at the first recheck (`gitignore_baseline_unix`);
- `g-mesh reindex` stops the daemon (`cli/reindex.rs:179-190`), wipes the
  index and writes a new baseline: the next daemon starts with an empty set
  and the recheck no longer sees the `.gitignore` as newer;
- a restart without reindex rebuilds the set: the `.gitignore` is still newer
  than the baseline, so `recheck_gitignores_since_index` re-runs the gate on
  the consumer thread before the `Start` reconcile (P2 closed with no
  persistence of the set). Accepted cost: until the next `g-mesh reindex`,
  every daemon start re-runs the gate for those `.gitignore` files and logs
  the guard line once more;
- a whole-language reindex for any reason (`workspace_reindex::run_with`)
  clears that language's set: it walked the language under the current
  rules;
- a file whose own change is routed (`file_changed`) leaves the set;
- the `Starved` per-language count block (l.1420-1446) is replaced by the
  same filter, removing GM-727/S7's accepted limitation (a sweep skipping
  every new file of an over-guard language).

Benefit: closes P1, P2 and the double log; keeps genuine new files (a big
checkout whose events were dropped, files created while the daemon was down)
indexed; the guard means exactly "un-ignored files", as Q2 defines it.
Risk: new in-memory state with its own invalidation rules (more code to
review, ~60-90 lines); memory proportional to the deferred files (40k paths,
a few MB); a file saved by the user inside the deferred set is still indexed
by its own event (acceptable: one file, the user touched it), and should be
dropped from the set then, or the set and the index disagree harmlessly.

### B: apply the per-language count guard to every Plain reconcile

Drop the `scope == ReconcileScope::Starved` condition (l.1421) so `Start` and
`Rescan` count created files per language too; suppress the reconcile's log
line when the gate in the same `reload_and_reconcile` already logged for that
language.

Benefit: a one-condition change plus a log tweak; same mechanism as GM-727.
Risk: guards **any** >10k burst, not only un-ignores: a branch switch with
15k new `.ts` files while the daemon was down, or while FSEvents dropped
events, stops being indexed until `g-mesh reindex` (today it is); the log
suppression covers only a gate in the same run, so a Rescan after the
watermark has passed the edit still logs a second line (with a cause other
than the `.gitignore`), and the test's "exactly one line" stays flaky. It also
keeps GM-727's limitation.

### C: exclude only what the gate in the same run skipped

`gitignore_changed` returns its over-guard languages (or paths);
`reload_and_reconcile` and the `Start` path pass them to `reconcile_counted`
as exclusions. No state across runs.

Benefit: smaller than A, no invalidation rules, closes P2 and the first
Rescan after the edit. Risk: a later Rescan or sweep whose watermark is past
the `.gitignore` mtime does not re-run the gate, so it has nothing to exclude
and indexes the files: P1 stays open for the second dropped-event episode,
which under load is the likely case. Double log not fixed.

### D: correct the test only

Relax the test (at least one guard line; tolerate files indexed after a
Rescan), document that only the gate path is guarded.

Benefit: no production change. Risk: contradicts Q2 and GM-727's own guard;
the daemon restart path (P2) keeps indexing 40k files unasked. Not
recommended; listed because the task asked "test assumption vs bypass".

### Out of scope, noted

A whole-language reindex for another reason (a `tsconfig.json` edit,
`workspace_reindex::run` via `reindex_workspace`, registry.rs:1119-1143)
walks the language's files with the current rules and indexes deferred files
too. That is a user-triggered config change, not an un-ignore; the owner
chose to let it index them and clear the language's deferred set.

Accepted known gaps:

- a `.gitignore` **deleted** while the watcher's events were dropped. The
  gate sees `.gitignore` files by mtime and a deleted file has none, so no
  gate runs, nothing is deferred, and the following `Rescan` or `Starved`
  reconcile indexes every file it un-ignored, past the guard;
- genuinely new files that keep an old mtime (extracted by `tar`, copied by
  `cp -p`) under an over-guard un-ignore are deferred at a restart or
  Rescan, while a live daemon indexes each of them by its own event; they
  stay unindexed until their next save or `g-mesh reindex`;
- un-ignored files rewritten after the edit (build output regenerated) are
  newer than the edit, so they are not deferred and a restart or Rescan
  indexes them past the guard. This matches a live daemon, which indexes
  each of them by its own event.

## 5. Edit map (option A)

Change:

| file | function / item | lines | change |
|---|---|---|---|
| `core/src/daemon/registry.rs` | `struct PluginRegistry` + `PluginRegistry::new` | l.452-, l.548- | field `deferred_unignores: Mutex<BTreeMap<String, HashSet<String>>>`, empty |
| `core/src/daemon/registry.rs` | `over_reindex_guard` | 202-214 | take an "already reported" flag (or move the log out) so a repeat of the same deferral logs nothing |
| `core/src/daemon/registry.rs` | `gitignore_changed` | 1145-1204 | count only the added files that predate the edit; on guard: defer that set per language, log its count only if new; otherwise remove the language's deferred paths under `subtrees` |
| `core/src/daemon/registry.rs` | `predates_unignore` | new | file mtime not later than the latest governing triggering `.gitignore` edit time; unreadable or ungoverned counts as predating |
| `core/src/daemon/registry.rs` | `gitignore_edit_time`, `gitignores_newer_than` | new, 1236- | edit time = max(mtime, ctime) on Unix, mtime elsewhere; used for the cutoff and for the recheck/Rescan selection |
| `core/src/daemon/registry.rs` | `reconcile_counted` | 1364-1554 | replace the `Starved` block (1420-1446) with a filter of `created` by the deferred set for every `Plain` scope (`Seeded` unchanged) |
| `core/src/daemon/registry.rs` | `GITIGNORE_REINDEX_GUARD` doc, `starvation_sweep` doc | 197-200, 1276-1283 | say the guard holds for every plain reconcile via the deferred set |
| `core/src/daemon/registry.rs` | `file_changed` | 1054- | drop a routed path from the deferred set |
| `core/src/daemon/workspace_reindex.rs` | `run_with` | after the swap | clear the language's deferred set (every whole-language reindex, whoever asked) |
| `core/src/storage/schema.rs` | DDL, `wipe` | | table `gitignore_baseline (id, walkedAt)`, dropped by `wipe` |
| `core/src/storage/state/languages.rs` | `record_gitignore_baseline`, `gitignore_baseline_unix` | | write the baseline from `bulkIndexedAt`; read it, adopting `bulkIndexedAt` when the row is missing |
| `core/src/daemon/activation.rs`, `core/src/cli/init.rs`, `core/src/cli/reindex.rs` | after `record_bulk_index` | | `record_gitignore_baseline` (full walks only; `language_swap.rs` does not) |
| `core/src/daemon/registry.rs` | `recheck_gitignores_since_index` | 1211-1231 | compare with `gitignore_baseline_unix` instead of `bulk_indexed_at_unix` |

Read for context: `reload_and_reconcile` (1293-1307),
`recheck_gitignores_since_index` (1211-1231), `rescan_subtrees` (1266-1274),
`watch_and_route_once` (`watch_loop.rs:258-338`), `spawn_watch_consumer`
(`watch_loop.rs:99-161`), `gitignore_watermark` (`watch_loop.rs:225-227`),
`cli/reindex.rs` `stop_for_rebuild` (179-), the GM-508 doc section 6, the
GM-616 and GM-727 design notes.

## 6. Behaviours to pin, and control reverts

Lib tests use `registry_over` + `walked_index` + `inject_rescan` +
`pump_until` (`core/src/daemon/tests.rs:1009-1310`) and the `sweep` helper
(`registry/tests.rs:1705`), with >10k fixture files as
`a_sweep_that_would_create_more_than_the_guard_indexes_none_of_that_language`
does. No load is needed: the Rescan is injected.

| # | behaviour | test place |
|---|---|---|
| B1 | after an over-guard gate, a root Rescan whose watermark predates the `.gitignore` indexes none of the un-ignored files and logs no second guard line | `daemon/tests.rs` |
| B2 | the same with the watermark already past the edit (the gate does not re-run): still none indexed | `daemon/tests.rs` |
| B3 | a Rescan still indexes a genuinely new file of the same language (not in the deferred set) | `daemon/tests.rs` |
| B4 | restart: recheck trips the guard, then the `Start` reconcile indexes none of them | `registry/tests.rs` |
| B5 | a later gate under the guard (re-ignore, or a smaller un-ignore) clears the deferral; the next reconcile behaves as before | `registry/tests.rs` |
| B6 | starvation sweep: deferred files skipped, another new file of the same language indexed (replaces GM-727's per-language count) | `registry/tests.rs` (adapt the existing sweep guard test) |
| B7 | integration test unchanged and passing 5x, once under load | `core/tests/it/gitignore_reevaluation.rs` |

Controls (one per fix behaviour; all in the lib binary, one build group):

| C | revert | must fail |
|---|---|---|
| C1 | `reconcile_counted` filters only for `Starved` (old condition) | B1, B2 |
| C2 | `gitignore_changed` logs on every guard trip (no "already reported") | B1 |
| C3 | no filter at all in `reconcile_counted` | B4, B6 |
| C4 | filter drops every created file of a deferred language (B-style) | B3, B6 |
| C5 | gate never removes deferred paths | B5 |
| C6 | `reindex_workspace` does not clear the language's set (if that edit is made) | its test |

## 7. Decisions for the owner

1. Fix option: A (recommended), B, C or D.
2. Should a whole-language reindex for a non-`.gitignore` reason
   (`tsconfig.json` edit) respect the deferral (out of scope above)?
3. Accept that the load-triggered failure was not reproduced here, with the
   bypass pinned deterministically by injected Rescans instead.
