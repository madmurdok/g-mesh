# GM-653: overload_call_binding leaves the collapsed CALLS edge (flaky)

Status: diagnosis (S1). No production or test change in this slice.

## Failure

| Run | Test | Panic |
|---|---|---|
| CI 39970286197 attempt 1, release-4.17.0 tip 87cce90, tests (x86_64-apple-darwin), core-it, 5.7 s | `overload_call_binding::each_call_of_an_overloaded_function_is_stored_against_the_overload_it_binds` | `overload_call_binding.rs:220` "the collapsed edge must go out with the bound ones that replace it", left 2, right 0 |

Attempt 2 passed on every platform, and so did the local suite.

## Verdict

**The test reads in the middle of a refinement that production does in two
commits, by design.** Not a retraction bug, and not new in 4.17.0. The CALLS
edge count is not wrong for good; it is briefly wrong after the test's wait
condition is already true.

- The index really does hold both the bound edges and the collapsed edges for
  a moment. It is transient: the per-file semantic pass that follows retracts
  the collapsed edges again (measured below).
- Cause (a) from the brief, a test wait race, is the right category. But the
  detail is not "bound edges committed before the retraction". The bound edges
  and the retraction land **in one diff, one transaction**. What happens is
  that a *later* structural reparse puts the collapsed edges back.

## Today's behaviour

1. Cold walk (`--bulk-index`, a separate one-shot plugin process) indexes
   `lib.ts` and `use.ts`, the first round of the test's rewrite loop. That
   gives the structural `CALLS useString -> parse` and `useNumber -> parse`
   edges, `toDeclaration NULL`.
2. The whole-project semantic pass runs in the **live** plugin process. Its
   diff upserts the two bound edges (`toDeclaration` 0 and 1, new ids with the
   ordinal hashed in) and lists the two structural ids in `deleteEdgeIds`
   (`Answers::settle_overloads`, `plugins/sdk/src/lsp/bridge.rs:1767-1808`,
   `self.retract.extend(bound…)` at 1806; `finish` at 1932-1990). One
   `apply_semantic_diff_linked` gives one SQLite transaction. State: bound=2,
   unbound=0. **The test's wait condition `found.len() == 2` is now true.**
3. The live process only *hydrated* `use.ts` for that pass and never
   *reported* it, so it has no diff baseline (`SdkIndex::baseline`,
   `plugins/sdk/src/index.rs:126-131`: "None … for one only hydrated: a diff
   against nothing is complete"). The watcher delivers one of the loop's
   rewrites of `use.ts`, and `fileChanged` answers a **complete** diff that
   upserts all 7 of the file's edges, **including the two collapsed CALLS
   edges** under their structural ids.
4. Core applies the complete diff (`round_trip`,
   `core/src/watcher/apply.rs:929-935` → `apply_file_diff_linked` →
   `file_rows`, `core/src/storage/file_rows.rs:79-90`). By design it keeps
   "a `semantic` edge from a node that stays: … the semantic pass that follows
   the reparse re-sends what it still resolves". So the bound edges stay **and**
   the collapsed edges are back. That commit is its own transaction. State:
   bound=2, unbound=2.
5. `apply_file_change_in` (`core/src/watcher/apply.rs:222-332`) then runs the
   per-file semantic pass (`apply_semantic_pass_in` at 310). Its diff
   retracts the two structural ids again. State: bound=2, unbound=0.

Between the commits in steps 4 and 5, readers on other connections (the test
opens its own `rusqlite::Connection`) see bound + collapsed. The length of
that window is one per-file tsserver round trip, about 22 ms locally. On a
loaded x86_64 macOS runner it can be much longer: the whole test took 5.7 s
there.

## The interleaving that fails

```
test thread                         daemon
-----------                         ------
loop: write tsconfig, lib.ts, use.ts
sleep 250 ms                        whole-project semantic pass commits (bound 2, unbound 0)
bindings() == 2 -> break            [watcher: use.ts rewrite settles]
                                    fileChanged(use.ts): complete diff commits (bound 2, unbound 2)
...hasBody checks...
SELECT COUNT unbound -> 2   <-- panic
                                    per-file semantic pass commits (bound 2, unbound 0)
```

The test breaks at the first read after step 2. It passes as long as its
`unbound` query runs before step 4 commits, or after step 5 commits.

## Reproduction evidence

Temporary instrumentation, reverted (worktree clean, `git status` empty):
daemon stderr to a file; `log_line!`s after each structural and semantic
commit in `round_trip`; a test-side probe; an optional sleep before the
per-file semantic pass of `use.ts` only. Command for every run:
`cargo nextest run -p g-mesh --test it -E 'test(overload_call_binding)' --no-capture --retries 0`.

| Phase | Runs | Failures | Observation | `uptime` load avg (1/5/15) |
|---|---|---|---|---|
| plain | 20 | 0 | daemon log: only the whole-project semantic commit; the test exits before any `fileChanged` lands | 196 / 252 / 211 |
| under load (16 `yes` hogs on 8 CPUs) | 20 | 0 | same | 561 / 447 / 324 |
| injected 1500 ms delay on every per-file pass | 10 | 0 | the test still exits before the reparse | 146 / 335 / 294 |
| probe: keep reading 8 s after the break, no delay | 5 | n/a | **4/5 saw bound=2 and unbound=2**, first seen 158-213 ms after the break, for 20-90 ms. Log order: semantic(whole) → structural `use.ts scope=Complete upsert_edges=7` → semantic(per-file, 22-24 ms) `delete_edges=2` | 24 / 190 / 241 |
| probe + 3000 ms delay on per-file passes | 3 | n/a | 3/3 saw bound=2 and unbound=2, for ~3 s | 15 / 160 / 226 |
| **exact assertion**: 3000 ms delay on `use.ts`'s per-file pass + 1500 ms sleep before the `unbound` query | 5 | **5** | `left: 2 right: 0`, the CI panic. Afterwards unbound went back to 0 after 1.68-1.86 s; bindings stayed `[("useNumber",1),("useString",0)]` | 72 / 98 / 179 |

Timing of the exact-assertion runs (`/usr/bin/time -p`): real 13.5-16.7 s,
user 0.94-1.12 s. The test mostly waits on its own sleeps; it does not use
the CPU much.

Without injected delays the original assertion did not fail in 40 runs, with
or without load. On this arm64 machine the per-file pass is about 22 ms and
the reparse lands about 200 ms after the test has already read. The probe
shows the exact state the test checks for, without delays, in 4/5 runs. The
delayed runs show that this state, read at the wrong moment, gives the CI
panic verbatim.

Ruled out:
- **Non-atomic semantic diff.** The bound upserts and the `deleteEdgeIds`
  retraction are one diff (`finish`) applied in one `apply_semantic_diff_linked`.
- **Retraction failing for good.** Every run settled back to unbound=0, in
  20-90 ms without delay and about 1.7 s with the injected one.
- **Workspace reindex from the tsconfig rewrite.** No `reindex` log line in
  any run. The `language_swap` unchanged-node rule keeps live edges anyway.
- **4.17.0's `generated` column.** Not on this path.

## Fix proposal

**Recommended: test-only.** Wait for the settled state, not for the first sign
of the semantic pass. Read both facts in one snapshot inside the loop:
bindings == 2 **and** the unbound count of `parse` == 0. Break only when both
hold. The timeout message names both values, for example "bound {found:?},
{unbound} collapsed edge(s) still alongside". After the break, keep the exact
`assert_eq!(found, …)`, the target check and the `hasBody` checks. The final
`assert_eq!(unbound, 0)` becomes redundant: drop it, or re-read it from the
same snapshot.

- Benefit: deterministic. The test asserts the state production guarantees
  ("after the follow-up pass, a bound caller has no collapsed edge"), not an
  order between the test thread and the daemon's watcher thread that
  production does not enforce. No production risk.
- Risk/trade-off: a real retraction regression now fails as a 60 s timeout
  (`SEMANTIC_TIMEOUT`) with the counts in the message, not as an immediate
  `assert_eq`. That is slower to fail, but still a failure that names the
  cause.

**Alternatives considered (not recommended):**
- **B. Complete reparse does not reinstate a structural CALLS edge already
  superseded.** In `file_rows`, or in the plugin's complete diff, skip
  upserting a structural edge whose `(fromId, toId)` has a stored semantic
  `toDeclaration` edge from a node that stays. Benefit: queries never see
  both. Risk: if the following semantic pass fails or times out, a call that
  stopped binding is left with no edge at all, which is a worse wrong answer
  than a duplicate. It also needs core to know which structural ids the
  plugin's bridge retracted (R1's re-send already handles the
  "no longer bound" case). It touches `file_rows`' documented invariant.
- **C. Structural reparse and per-file semantic pass in one transaction.**
  This holds a SQLite write transaction across a tsserver round trip (up to
  `semantic_pass_timeout`), blocking every other writer for seconds.
  Rejected.
- **D. Stop rewriting after the first full binding.** It does not help: the
  rewrite that causes the complete reparse was usually written *before* the
  break.

Production follow-up worth a separate task, if wanted: the same window exists
for any client query right after the first edit of a hydrated file. MCP
answers can list a caller twice (bound + collapsed) for one tsserver round
trip. That is a product question, not this flake.

## Edit map (recommended fix)

| File | Function | Lines | Change |
|---|---|---|---|
| `core/tests/it/overload_call_binding.rs` | `each_call_of_an_overloaded_function_is_stored_against_the_overload_it_binds` | 152-170 (loop), 208-220 (unbound check) | Loop condition: `found.len() == 2 && unbound == 0`, read from one connection in one statement or read transaction. Timeout message names both. Move the unbound query into a helper next to `bindings()`. Replace or delete the final `assert_eq!(unbound, 0)` at 220. |
| same file | `bindings` | 108-127 | Optionally return `(bindings, unbound)` from one connection, so both come from one snapshot. |
| same file | module doc / loop comment | 1-21, 145-151 | State the invariant: a complete reparse of a file the live plugin only hydrated re-upserts the collapsed edge until its per-file pass retracts it, so the test waits for the settled state. No ticket ids (CLAUDE.md comment rule). |

Read for context (no change):
- `plugins/sdk/src/lsp/bridge.rs`: `Answers::settle_overloads` (1767-1808),
  `Answers::finish` (1932-1990), and the overload section of the type's doc
  (280-300).
- `core/src/watcher/apply.rs`: `apply_file_change_in` (222-332), `round_trip`
  (around 900-960), `file_scope` (990-1005).
- `core/src/storage/file_rows.rs`: module doc (1-22) and the `keep_semantic`
  rule (79-90).
- `plugins/sdk/src/index.rs`: `SdkIndex::baseline` (126-131).

## Behaviours a test must pin, and the controls

1. **A bound caller ends with no collapsed edge** (the retraction). Control:
   in a throwaway worktree, delete
   `self.retract.extend(bound.iter().cloned());` at
   `plugins/sdk/src/lsp/bridge.rs:1806`. Rebuild the workspace (the plugin
   binary changes, not only core), then run
   `cargo nextest run -p g-mesh --test it -E 'test(/^overload_call_binding::/)'`.
   Expected: the test fails, with a timeout naming `2 collapsed edge(s)`.
   Allow about 60 s.
2. **The wait no longer races the follow-up pass** (the flake fix; an A/B on
   the test). There is no production code to revert. The arm that tells the
   two tests apart is this slice's widening patch: a sleep before
   `apply_semantic_pass_in` in `apply_file_change_in`, gated to `use.ts`,
   3000 ms, plus a sleep before the test's unbound read. Under it the
   **current** test failed 5/5. The **fixed** test must pass 5/5 under the
   same patch (its loop waits through the window). Run the patch in the
   throwaway worktree, never on the branch.
3. The existing exact-ordinal, single-target and `hasBody` assertions stay
   unchanged.

Repeats: the test spawns a daemon and plugin processes, so the tests slice
runs it 5× and verify runs it 5× (once under load).

## Cross-file answers relied on

| Question | Source |
|---|---|
| Callers of `watcher::apply::apply_semantic_pass`: `PluginProcess::semantic_pass`, `plugin_check::session::Driver::step`, tests | g-mesh `find_callers(apply_semantic_pass)`, complete (`hasMore: false`) |
| Callers of `PluginSupervisor::semantic_pass`: `semantic::run_with_registry_and_progress`, `workspace_reindex::semantic_pass_after_swap`, tests | g-mesh `find_callers(PluginSupervisor::semantic_pass)`, complete |
| Callers of `PluginProcess::semantic_pass`: the supervisor's `semantic_pass`, `semantic::run_once` | g-mesh `find_callers(PluginProcess::semantic_pass)`, complete |
| Callers of `semantic::run_once`: `cli::init`, `cli::reindex`, a test (not on the daemon path) | g-mesh `find_callers(daemon::semantic::run_once)`, complete |
| Caller of `file_scope`: only `round_trip` | g-mesh `find_callers(file_scope)`, complete |
| Definition and doc of `apply_semantic_pass` (one `Unit::WatcherApply`; `deleteEdgeIds` retracts contradicted structural edges) | g-mesh `find_definition(apply_semantic_pass)` |
| The per-file pass runs inside `apply_file_change_in` after the structural `round_trip` commit; the bridge's settle/retract logic; `baseline` returns None for hydrated files; `FileScope::Complete` keeps semantic edges | grep and `sed -n` in single known files (`apply.rs`, `bridge.rs`, `index.rs`, `file_rows.rs`) |
| No other `core/tests/it` test asserts on `toDeclaration IS NULL` | grep over `core/tests/it/*.rs` (non-structural text) |

g-mesh note: one call without `project` was refused because the connection
had two projects selected (`g-mesh`, `g-mesh-wt-gm453`). Every later call
passed `project: "g-mesh"`. Its code is the same as this worktree's for every
file read.
