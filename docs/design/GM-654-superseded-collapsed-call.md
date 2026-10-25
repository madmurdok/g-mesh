# GM-654: a caller listed twice after the first edit of a hydrated file

Status: design (S1). No production or test change in this slice.
Background: `docs/design/GM-653-overload-flaky-diagnosis.md` (how the window
was found and measured).

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

Two kinds of `CALLS` edge describe `useString -> parse`:

- the **collapsed** edge, from the structural (tree-sitter) pass:
  `source = 'syntactic'`, `toDeclaration IS NULL`. It says "useString calls
  parse" without knowing which overload;
- the **bound** edge, from the semantic (tsserver) pass:
  `source = 'semantic'`, `toDeclaration = 0`, an id with the ordinal hashed in.

When the semantic pass binds every overload call a collapsed edge stands for,
it retracts that collapsed edge in the same diff
(`Answers::settle_overloads`, all-or-nothing per structural edge). Settled
state: one edge, the bound one.

**Today**, the first save of `use.ts` in a daemon session does this:

1. The live plugin only *hydrated* `use.ts` (read it from disk for the
   whole-project semantic pass) and never *reported* it, so it has no diff
   baseline (`SdkIndex::baseline` is `None`; GM-487 made that deliberate:
   core's rows may come from another process).
2. `fileChanged(use.ts)` therefore answers a **complete** diff: every edge of
   the file, including the collapsed `useString -> parse`.
3. Core applies it (`round_trip` -> `apply_file_diff_linked` ->
   `file_rows::widen`, `FileScope::Complete`). `widen` keeps semantic edges
   from nodes that stay, so the bound edge stays and the collapsed edge is
   back. Committed.
4. The per-file semantic pass then retracts the collapsed edge again.
   Committed.

Between 3 and 4, `find_callers(parse)` returns `useString` **twice**, as two
byte-identical rows (`CallerSite` carries no `toDeclaration`), and `total`,
`files` tallies and `answer: count` are one too high. Same for
`find_callees(useString)` (parse twice) and `find_references(parse)`
(`USAGE_EDGE_KINDS` includes `CALLS`).

## 1. Is the window worth closing?

**User-visible impact.** A duplicated row in a structural answer. The answer
is not wrong about *who* calls `parse`, it repeats a caller. Agents are told
to trust a complete structural answer and not re-check it, so a repeated row
can be read as "two call sites" or make an agent count wrong. Low severity,
but it is exactly the kind of noise the tools promise not to have.

**Frequency.**
- Only for callers whose calls to an overloaded project function were bound
  (TypeScript via tsserver; Python via pyright's `hover` disambiguation, if
  the plugin's manifest enables it; see must-confirm M6).
- Only on a **complete** diff: the first `fileChanged` of each file per live
  plugin process (the whole-project pass hydrates every file), again after a
  plugin restart, and after `SdkIndex::clear` (`workspaceChanged`, an
  `Unknown` resolution delta). Later edits diff against a baseline that still
  holds the collapsed edge, unchanged (`WireEdge` has no position fields), so
  it is not re-sent.
- **Duration**: one per-file tsserver round trip, measured 20-90 ms
  (GM-653), longer under load or with a cold tsserver. **If the per-file pass
  fails or times out, the duplicate stays** until a later pass over that file
  succeeds (the file becomes "owed" and rides along on the next edit of any
  file, up to `MAX_OWED_ATTEMPTS`). So the window is usually tiny but has no
  upper bound.

**Cost of fixing** (recommended approach): one shared SQL predicate in
`core/src/graph/pagination.rs`, applied in three queries; one unit test per
query family, one integration test. No write-path, protocol or plugin change.

### Options

| Option | Benefit | Risk |
|---|---|---|
| **A. Fix on the read side (recommended)** | Closes the window for every MCP answer, including the unbounded failed-pass case and any other transient that produces the same pair. No stored state to keep right. A failed follow-up pass cannot leave a call with no edge: nothing is deleted or skipped. | A correlated `EXISTS` per `CALLS` row (an index probe). Hides a collapsed edge also in the rare settled state where a second structural edge onto the same callee was not bound (the caller is still listed; see A's risk below). |
| **Do not fix / document** | Zero cost. | The duplicate stays for one round trip on every first edit, and indefinitely after a failed pass. A future test or agent relying on "one row per caller edge" hits it again (GM-653's flake was this). |

**Recommendation: A.** Small and local, and the only option here whose
safety argument does not depend on the follow-up pass succeeding.

## 2. Candidate approaches

### A. Read-side: a collapsed edge with a bound sibling is not listed

**Rule.** A `CALLS` edge `e` with `toDeclaration IS NULL` is left out of an
answer when another `CALLS` edge `b` exists with `b.fromId = e.fromId`,
`b.toId = e.toId` and `b.toDeclaration IS NOT NULL`. As SQL, appended to the
`WHERE` of the edge queries:

```sql
AND NOT (e.kind = 'CALLS' AND e.toDeclaration IS NULL AND EXISTS (
  SELECT 1 FROM edges b
  WHERE b.fromId = e.fromId AND b.toId = e.toId
    AND b.kind = 'CALLS' AND b.toDeclaration IS NOT NULL))
```

**Example.** In the window the store holds `useString -> parse` (collapsed)
and `useString -> parse #0` (bound). `find_callers(parse)` lists `useString`
once. After the per-file pass retracts the collapsed edge, the answer is the
same: the filter changed nothing a reader can see.

**Edge survives a failed follow-up pass.** The rule hides a row only when a
bound row for the same caller and callee is listed instead, and it is derived
from the current rows on every read. Nothing is written, so nothing is lost:
- pass fails after the reparse: both edges stay stored, the caller is listed
  once (via the bound edge);
- a later pass stops binding (R1 re-sends the collapsed edge and drops the
  bound ones): the bound sibling is gone, so the collapsed edge is listed
  again, automatically;
- the edit removed the call: the collapsed edge is not in the complete diff,
  `widen` keeps the bound edge until the pass retracts it, same as today.

**Cost.** One `EXISTS` probe per `CALLS` row whose `toDeclaration` is NULL
(almost every `CALLS` row), served by `idx_edges_fromId` (a caller has few
edges). Must be checked with `EXPLAIN QUERY PLAN` and a timing on a
high-fan-in symbol (M4). No schema change expected; a composite
`(fromId, toId)` index only if the plan says so.

**Risk.**
- The settled store can, in principle, hold a bound edge and an unbound
  collapsed edge for the same `(fromId, toId)`: two *different* structural
  edges onto one callee (e.g. reached through two placeholders that link to
  the same node), one wholly bound and one not (`settle_overloads` is
  all-or-nothing **per structural edge**). The rule then hides the unbound
  one. The caller is still listed; the row count for that caller drops by
  one. Since these rows are byte-identical on the wire, nothing a client can
  read is lost. M2 asks the tests slice to confirm whether this state is
  reachable at all.
- `paginate_edges`, `count_edges_by_resolution` and
  `tally_edge_files_limited` must all apply it, or `total`/`files` disagree
  with the rows (M3).
- Raw-store readers outside MCP (tests that `SELECT` from `edges`, the
  `plugin_check` expectations) still see both edges in the window. That is
  correct: the store is accurate, the answer deduplicates.

### B. Write-side: mark the re-upserted collapsed edge superseded

`file_rows::widen` (or `apply_file_diff`), on a complete diff, flags an
upserted `syntactic` `CALLS` edge `superseded = 1` when a stored semantic
bound edge from a node that stays has the same `(fromId, linked toId)`.
Queries filter `superseded = 1`; the per-file pass deletes the edge anyway.

- **Survives a failed pass?** Yes while the bound edge stays. But the flag is
  stored state: when a later pass drops the bound edges and R1 re-sends the
  collapsed edge, something must clear the flag (the semantic diff's re-send
  of a stored row has to reset it). Forget one path and the call is hidden
  with no visible edge: the failure GM-653 rejected skipping for.
- **Cost.** A schema column and migration, a change in `file_rows` (whose
  documented invariant changes), a reset rule in `apply_semantic_diff`, the
  same three query filters as A.
- **Risk.** Strictly more moving parts than A for the same visible result.
  Not recommended.

### C. Close the window on the write side: one commit for reparse and pass

Hold the complete structural diff in memory, run the per-file semantic
round trip, then commit both in one transaction (no transaction held across
the round trip, so not GM-653's rejected option).

- **Survives a failed pass?** Yes: on failure, commit the structural diff
  alone (today's state, with the duplicate).
- **Why not.** The semantic pass is sent core's link result for the
  committed reparse (`apply_semantic_pass_in` reads `linked_edges` from the
  store, "the links of exactly the text the plugin is about to answer
  for"); linking an uncommitted diff means restructuring `round_trip`,
  `apply_file_diff_linked` and the pass. And every edit's structural result
  would become visible only after tsserver answers (up to
  `semantic_pass_timeout`): every reader pays latency on every first edit to
  avoid a 20-90 ms duplicate. Not recommended.

### Considered and ruled out

- **Hydrated entries as a baseline** (so the first diff is incremental):
  GM-487 made hydrated entries no baseline on purpose; core's rows for the
  file may come from another process, and an incremental diff against the
  hydrated graph can answer "nothing changed" while core keeps a stale file.
- **Skip the re-upsert in `file_rows`, or in the plugin's complete diff**
  (GM-653 option B): a pass that then stops binding leaves the call with no
  edge.
- **One transaction across the round trip** (GM-653 option C): blocks every
  writer for seconds.

## 3. Edit map (approach A)

Line numbers are 1-based, at `912c53c`.

| File | Function | Lines | Change |
|---|---|---|---|
| `core/src/graph/pagination.rs` | new `const SUPERSEDED_CALL_FILTER: &str` | next to `LOCALITY_EXPR` (344) | The predicate above, with a doc comment stating the rule and why it is safe when a follow-up pass fails (no ticket ids in code comments). Table alias `e` is shared by all three queries. |
| same | `paginate_edges` | 694-849 | Add `AND {SUPERSEDED_CALL_FILTER}` to the `WHERE`. No new placeholder, so `FIXED_PARAMS` and the cursor keyset are unchanged. Doc: the rule joins the list of what the rows exclude. |
| same | `count_edges_by_resolution` | 555-600 | Same predicate in its `WHERE`, so `total`/`unresolved` match the rows. |
| same | `tally_edge_files_limited` | 473-526 | Same predicate, so the `files` tallies match the rows. |
| same | `mod tests` | append | Unit tests: (1) collapsed + bound for one pair: `paginate_edges`, `count_edges`, `tally_edge_files` each give 1; (2) collapsed alone: listed; (3) two bound ordinals from one caller: both listed (rule never hides a bound edge); (4) collapsed onto another callee from the same caller: listed; (5) a `REFERENCES` edge with NULL `toDeclaration` beside a bound `CALLS`: listed (kind-gated). |
| `core/tests/it/` new `overload_call_window.rs` (or a case in `overload_call_binding.rs`) | new test | - | The window-widening MCP test (section 4). A new file needs `mod overload_call_window;` in `core/tests/it/main.rs` (`layout.rs` checks it). |

Read for context (no change): `core/src/mcp/find_callers_callees.rs`
`list_calls` (55-100), `handle_callers_in_covered` (562-678),
`handle_callees_in_covered` (766-~850), `excluded_references` (321-355, counts
`REFERENCES`, unaffected); `core/src/mcp/find_references.rs` `list_references`
(175-222, `USAGE_EDGE_KINDS` at 48); `core/src/mcp/answer.rs` `count`
(98-130); `core/src/storage/file_rows.rs` `widen` (`keep_semantic`);
`plugins/sdk/src/lsp/bridge.rs` `settle_overloads` (1767-1808) and the
overload doc (280-305); `core/src/storage/schema.rs` edge indexes (326-337).

## 4. Must-confirm list

- **M1.** In the window, the collapsed and the bound edge have the **same
  stored `toId`** after linking (both are linked from the same placeholder;
  GM-653's probe counted both on `parse`'s id). If linking can leave one on
  the placeholder and move the other, the predicate must compare
  `COALESCE(linkedFrom, toId)` instead. Confirm in the integration test by
  reading both rows.
- **M2.** Whether a settled store can hold a bound and an unbound edge for one
  `(fromId, toId)` (two structural edges onto one callee, split by
  `settle_overloads`). If reachable, state the hidden-row consequence in the
  predicate's doc; if not, say so.
- **M3.** All three queries apply the predicate: an MCP `find_callers` in the
  window with `answer: rows`, `answer: count` and `answer: files` agree (1).
- **M4.** `EXPLAIN QUERY PLAN` for `paginate_edges` with the predicate uses
  `idx_edges_fromId` (or `idx_edges_toId`) for the `EXISTS`, and a
  `find_callers` timing on a symbol with many callers (the g-mesh index
  itself) is not measurably slower; record `uptime` and `/usr/bin/time -p`
  with it.
- **M5.** No MCP tool other than the `paginate_edges` / `count_edges*` /
  `tally_edge_files*` users lists `CALLS` rows (g-mesh `find_callers` on all
  three returned only `list_calls`, `list_references`,
  `list_implementations`, `answer::count`, `excluded_references` and tests).
- **M6.** Which plugins emit `OverloadCall` sites (TypeScript; Python if its
  manifest sets `overload_disambiguation = "hover"`), for the release note.

### The window-widening test

Hook: `watcher::apply::hold_compute_open_for_tests`
(`core/src/watcher/apply.rs`, called in `round_trip` right after the
structural commit and before `apply_file_change_in` sends the per-file
`semanticPass`). It holds the round trip while the file named by
`G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE` (`HOLD_COMPUTE_FILE_ENV`) exists,
capped at 30 s, and readers are not blocked
(`core/tests/it/incremental_embed_outside_lock.rs` proves it and is the
template for driving MCP over the ipc endpoint while held). No new
production hook is needed.

Shape:
1. Spawn the daemon with `HOLD_COMPUTE_FILE_ENV` pointing at a path that does
   not exist yet; write `lib.ts`/`use.ts` (two callers, two overloads, as in
   `overload_call_binding.rs`).
2. Wait for the settled state: two bound edges and zero collapsed edges onto
   `parse` (one snapshot, GM-653's loop).
3. Create the hold file, then rewrite `use.ts` (a change that keeps both
   calls). The round trip parks after its commit.
4. Wait until the store shows the window open: two bound **and** two
   collapsed edges onto `parse` (proves the arm, so a pass means "measured
   something").
5. MCP `find_callers(parse)` (rows, count, files) and `find_callees(useString)`:
   each caller/callee exactly once; `total` 2 and 1.
6. Delete the hold file; wait for the settled state again (no leaked hold).

The hold also parks the semantic-pass round trip, so the hold file must not
exist before step 2 settles, and must be deleted before the test waits for
anything the pass writes.

Control: revert the predicate (code, not test) in a throwaway worktree;
step 5 must fail with `useString` listed twice. The test spawns a daemon and
plugin processes: 5 repeats in the tests slice, 5 in verify (one under load).

## Cross-file answers relied on

| Question | g-mesh call | Answer |
|---|---|---|
| Who calls the `file_rows` upsert path for a complete diff | `find_callers(apply_file_diff_linked)`, complete | `watcher::apply::round_trip` (the `fileChanged` path), `bulk_index::purge_language`, `PluginRegistry::retire_file`, a test |
| Who reads the baseline for the first `fileChanged` | `find_callers(SdkIndex::baseline)`, complete | only `run::Session::file_changed` (`diff_file(self.index.baseline(path), …)`) |
| Who creates hydrated (unreported) entries | `find_callers(SdkIndex::insert_unreported)`, complete | only `run::Session::hydrate` |
| Query paths of find_callers / find_references | `find_definition(mcp::GMeshMcpServer::find_callers)`, `find_callers(paginate_edges)`, `find_callers(count_edges_by_resolution)`, `find_callers(tally_edge_files_limited)`, all complete | `list_calls`, `list_references`, `list_implementations` -> `paginate_edges`; `answer::count` -> `count_edges_by_resolution`, `tally_edge_files`; `excluded_references` -> `tally_edge_files_limited` (`REFERENCES` only) |
| Where a `find_callers` query is located | `search_code("SQL query that lists the callers…")` | `core/src/mcp/find_callers_callees.rs` |

Single known files were read with `sed -n`: `file_rows.rs`, `index.rs`,
`run.rs` (`hydrate`, `file_changed`), `diff.rs`, `wire/src/lib.rs`
(`WireEdge`), `apply.rs`, `bridge.rs`, `pagination.rs`, `schema.rs` (grep for
edge indexes). All g-mesh calls used `project: "g-mesh"`: the worktree's HEAD
is the main checkout's (`912c53c`), so its index answers for this code.
