# GM-563: TypeScript keeps a stale semantic edge after a complete pass in a new plugin process

Status: design (S1). Found in GM-560's design
(`docs/architecture/gm-560-stale-semantic-edges.md` on
`fix/GM-560-stale-semantic-edges-after-swap`, section 8 Q2). Line numbers are
on `release-4.6.0` at `e44e80c`.

## 1. Today

Two mechanisms remove a semantic edge (`edges.source = 'semantic'`) that the
semantic tier no longer stands behind:

1. **Per-process retraction, in the plugin.** The SDK's LSP bridge
   (`plugins/sdk/src/lsp/bridge.rs`, `LspBridge`, used by TypeScript, Python
   and Rust) remembers every edge id it emitted, keyed by the asking file
   (`LspBridge::emitted`, bridge.rs:533-535). A later pass that finishes that
   file sends `deleteEdgeIds` for the ids it did not emit again
   (`retract_stale`, bridge.rs:2984, called only from
   `<LspBridge as SemanticEngine>::answer`, bridge.rs:2794). `emitted` lives
   in memory and starts empty in every new plugin process (bridge.rs:617).
2. **The sweep, in core.** After a *complete whole-project* pass, core
   deletes the language's semantic edges the pass did not upsert
   (`watcher::apply::sweep_semantic_edges`, apply.rs:787-810, called only from
   `apply_semantic_pass_in`, apply.rs:730). It runs only when the manifest
   declares `[plugin.capabilities] semantic_sweep = true`
   (`PluginProcess::semantic_pass`, daemon/plugin.rs:1234). The same flag
   decides whether a workspace reindex keeps semantic placeholders across the
   swap (`workspace_reindex::rebuild`, workspace_reindex.rs:435-436).

Rust, Go and Python declare the sweep. TypeScript declares
`semantic_sweep = false` (plugins/typescript/plugin.toml:36-38). So for
TypeScript only mechanism 1 exists, and it forgets everything at a restart.

**Example.** `src/a.ts` imports `x` from `@app/x` and calls `x.foo()`. vtsls
answers the receiver call: semantic edge `a.main CALLS app/x.ts::foo`. The TS
plugin is then memory-suspended (or the daemon restarts). tsconfig `paths`
retargets `@app/*` to `app2/*`. `a.main` is an unchanged node, so the
reindex swap keeps its semantic edge (GM-546 / ADR 0008). The pass after the
swap runs in a **new** plugin process: it re-sends `a.main CALLS
app2/x.ts::foo` (a different id: the placeholder is addressed by the new
declaration's container) and retracts nothing, because this process never
emitted the old id. Core does not sweep TypeScript. Result: both edges stay,
`find_callers(app/x.ts::foo)` lists `a.main` falsely, `semanticPassAt` is set,
provenance is silent, and GM-560 option D's stale-file rows (if adopted) are
cleared by the complete pass. Nothing warns. The edge lasts until `a.ts`
itself is reparsed.

### Reproduced

- Plugin half: `plugins/sdk/tests/lsp_bridge.rs`,
  `a_new_bridge_does_not_retract_what_an_earlier_process_emitted` (written in
  S1, kept uncommitted for the tests slice to adopt). Bridge 1 answers the
  site onto `pkg::add`; it is dropped; bridge 2 (same index, server now
  answers `pkg2::add`) runs a complete pass, upserts a new edge id, and its
  `delete_edge_ids` does not contain the old id. Passes today. The existing
  `an_answer_that_is_no_longer_produced_is_retracted` (same bridge) does
  retract: the difference is exactly the process boundary.
- Core half: `core/src/daemon/workspace_reindex.rs`, test 13's other arm
  `a_complete_pass_sweeps_nothing_for_a_language_that_does_not_declare_it`
  (already on release-4.6.0): after a complete pass that does not re-send a
  semantic edge, a language without `semantic_sweep` keeps it.

Together: a stale edge from an earlier TS process survives a complete pass.

### Why TypeScript has the sweep off

`git log -L36,38:plugins/typescript/plugin.toml`:

- 20b1bfe (2026-09-26, ADR 0008 owner answer 6): the sweep became opt-in.
  TS was off because the **old Node checker tier** (`src/semantic.ts`)
  upgraded structural edges in place, under the structural edge's own id, and
  never reported a pass incomplete. A sweep after a checker failure would
  delete the structural edges themselves, not downgrade them.
- 8fbec71 (GM-430): the Node tier learned to report incomplete; the reason
  narrowed to "in-place upgrades".
- 01ae5e9 / 02b2f6d (2026-10-05, GM-324/325): the Node plugin was replaced by
  the Rust plugin on the SDK's LSP bridge with vtsls. The comment became
  "Off until the semantic tier states that a whole-project pass re-sends
  every edge it stands behind". Nobody revisited it.

Both original reasons are gone. The bridge never upgrades in place: every
answer is a new edge onto a placeholder in the asking file
(bridge.rs:185-215); a contradicted structural edge is retracted by
`deleteEdgeIds` and re-sent unchanged (`source` still syntactic) when no
longer contradicted (R1, bridge.rs:461-468, which already names "core's
semantic sweep" as something a re-sent structural edge must survive). The
bridge reports incomplete with `unfinishedFiles` on timeouts, refusals, an
unready server and the site ceiling (bridge.rs:2820-2980). Python and Rust
run the very same bridge and declare the sweep with the statement TS's
comment waits for ("Every complete whole-project pass re-sends each semantic
edge it stands behind ... (the SDK LSP bridge's `Answers::finish`)",
plugins/python/plugin.toml:63-68). Re-export hop sites are asked by a rule
that depends only on the index (`unsettled_hop`, bridge.rs:1081), so a
complete pass asks the same hops every time and re-sends their answers.

## 2. Options

### (a) Turn on `semantic_sweep` for TypeScript

- Change: `semantic_sweep = true` in plugins/typescript/plugin.toml with
  Python's comment; no code change. Core's existing paths follow the flag:
  the sweep after a complete pass, `sweep_unclaimed_nodes`, and
  `keep_semantic_placeholders` at the swap.
- Example: the pass in the new process upserts `CALLS app2/x.ts::foo`; the
  sweep deletes `CALLS app/x.ts::foo` because the pass did not upsert it.
- Consequence: TS behaves like Python and Rust. Stale edges from an earlier
  process last until the next complete whole-project pass, not until a
  reparse. Also at a TS reindex, unlinked semantic placeholders are kept
  across the swap instead of deleted and re-added (GM-431's rule, already on
  for the other three).
- Residual: an incomplete pass (timeouts, ceiling) sweeps nothing, so on a
  project whose whole-project pass never completes the stale edge stays,
  exactly as for Python/Rust today. GM-325's measurements: with the 120 s
  warm-up, 6 of 6 cold excalidraw passes were complete (gm-325 results,
  section 5).

### (b) Persist per-file emitted ownership so a new process retracts

- Change: the bridge saves `emitted` to disk per project, or core sends the
  file's current semantic edge ids in each `semanticPass` request (a protocol
  field) and the bridge seeds `emitted` from it.
- Example: process 2 starts with `b.ts -> [old id]`, finishes `b.ts`, sends
  `deleteEdgeIds = [old id]`.
- Consequence: works for residual passes too (any finished file is judged).
  Cost: a protocol change or a plugin-side cache that can drift from the
  database (a reindex, a `cargo clean`-style reset, a second daemon). Core's
  `edges` table already *is* this ownership record (a bridge edge always
  starts at a node of the asking file), so the plugin would be keeping a
  second copy of what core holds. Rejected in favour of (c) if residual
  coverage is wanted.

### (c) Core sweeps per finished file

- Change: in `apply_semantic_pass_in`, for every whole-project pass, complete
  or residual, delete the language's semantic edges whose `fromId` node is
  in a file the pass finished (scope minus `unfinishedFiles`) and that the
  pass did not upsert. Gate it on `semantic_sweep` too.
- Example: a residual pass that finished `a.ts` but not `z.ts` still deletes
  `CALLS app/x.ts::foo`.
- Consequence: closes the residual gap (a) leaves. Risk: it relies on "a
  semantic edge starts in the file whose questions produced it". That holds
  for TS (`implementation_kinds = []`) and Python, but not for Rust's
  implementation sweep, whose `SUPERTYPE_OF` edges start at the implementing
  declaration in another file (bridge.rs:205-215). It needs a per-language
  rule or a new manifest promise, plus a file -> node join on every residual
  pass. More design than this bug needs.

### (d) Do nothing; document

TS's stale semantic edges last until their file is reparsed (ADR 0008
consequence, line 404, says exactly this). Rejected: GM-325 made TS
semantics resolve receiver calls, so false callers after a tsconfig change
are now an ordinary event, and nothing warns.

## 3. Cost on a large TS repo (option a)

- Rows re-sent: none extra. The bridge already re-sends every edge it stands
  behind on each complete pass (excalidraw: 13 234-13 309 edges upserted per
  pass, about 6 000 of them semantic; gm-325 results, section 2). The `kept`
  set is the pass's `upserted_edges`, which `round_trip` already builds.
- Sweep query: one `SELECT e.id FROM edges e JOIN nodes n ... WHERE
  e.source = 'semantic' AND n.language = ?` (no index on `source`, so a
  scan of `edges` with a primary-key lookup into `nodes` per row), then a
  `HashSet` filter and one `DELETE` per stale id in one
  transaction. On excalidraw that is ~6 000 candidate ids and, in steady
  state, ~0 deletes: milliseconds next to an 80-110 s pass. On a project ten
  times larger (60 000 semantic edges) it stays well under a second; not
  measured, and not worth a slice: the other three languages pay it today.
- First pass after the upgrade: deletes every stale edge accumulated since
  the TS semantic tier shipped (bounded by the number of retargeted sites).
- Residual / per-file passes: sweep nothing; same as Python/Rust.
- Placeholders: `sweep_unclaimed_nodes` deletes kept placeholders the pass
  did not re-send; zero rows unless a reindex kept some.

## 4. Interaction with GM-560 option D

D keeps `semantic_pending_files` rows when a pass fails, is not run, or is
residual, and clears them on a complete pass. With (a), a complete TS pass
now really refreshes every semantic edge (re-sent or swept), so clearing the
rows on a complete pass becomes correct for TS, as it already is for the
three sweeping languages. Without (a), D's clear would hide exactly this
bug. A residual pass keeps D's rows, which is right: residual sweeps nothing
under (a). So (a) is what makes D's rule hold for every bundled language;
if GM-563 were not fixed, D would need a TS exception (keep rows after a
complete pass of a non-sweeping language).

## 5. Recommendation

**(a).** A one-line manifest change with an existing, tested core path; the
reasons TS was excluded belonged to the Node tier that no longer exists, and
the bridge it now runs is the one Python and Rust already sweep. (c) is a
separate follow-up only if residual TS passes turn out to be common on large
repos.

## 6. Edit map (a)

Change:
- `plugins/typescript/plugin.toml` 36-38: `semantic_sweep = true`, comment
  as Python's (plugins/python/plugin.toml:63-67).
- `core/src/daemon/manifest/tests.rs`
  `the_bundled_js_ts_plugin_manifest_parses_once_directory_named_correctly`
  (895-929): line 925 `assert!(!...semantic_sweep)` becomes `assert!(...)`.
- `core/src/daemon/workspace_reindex.rs` test 13 doc (1919-1921): drop
  "(as TypeScript's does)". The test stays (the default-off arm).
- `docs/adr/0008-workspace-reindex-staging-swap.md` 198-204 (Sweep): Rust,
  Go, Python and TypeScript declare it; drop the TS paragraph. 404-405
  (Consequences): replace "TypeScript is not swept" with the new rule. Add an
  owner answer for GM-563.
- `docs/architecture/multi-language-plugins.md` only if it names TS as
  unswept (it does not today: grep found no such line).

Read, no change expected: `watcher::apply::apply_semantic_pass_in` (565-745),
`sweep_semantic_edges` (787-810); `PluginProcess::semantic_pass`
(daemon/plugin.rs:1212-1250); `workspace_reindex::rebuild` (399-483);
`language_swap::plan_attached` placeholder keep (195-207); `LspBridge::answer`
(bridge.rs:2794-2980).

## 7. Behaviours to pin, one control each (revert code, never the test)

1. **A complete TS pass in a new process removes the old process's edge.**
   Core test with the real TypeScript manifest's capabilities (or the
   fixture plugin with `semantic_sweep` read from
   plugins/typescript/plugin.toml): seed `a.main CALLS old` (semantic), run a
   complete pass whose diff upserts `a.main CALLS new` and deletes nothing;
   expect only `new`. Control: set `semantic_sweep = false` in the TS
   manifest -> `old` survives.
2. **The bridge does not retract across processes** (documents why core must
   sweep): the S1 test in `plugins/sdk/tests/lsp_bridge.rs`. Control: seed a
   new bridge's `emitted` from the previous one (not reachable from outside;
   use the same bridge instead, which is
   `an_answer_that_is_no_longer_produced_is_retracted`) - so this test needs
   no separate control; it pins a fact, not a fix.
3. **An incomplete TS pass sweeps nothing.** Already pinned generically
   (ADR 0008 test 11, residual tests). No new control.
4. **The shipped manifest declares the sweep.** The manifest test's flipped
   assertion. Control: the manifest line reverted -> it fails.

Controls 1 and 4 revert the same line: one build serves both.

## 8. Open questions for the owner

1. **(a) alone, or (a) plus (c) later?** (a) leaves stale edges after a
   residual pass, as for Python/Rust. Proposal: (a) now; file (c) only if a
   large-repo measurement shows TS passes residual in steady state.
2. **Pin 1 against the real manifest or the fixture plugin?** The real
   manifest makes control 1 a one-line revert; the fixture plugin keeps core
   tests independent of plugins/. Proposal: real manifest's capabilities
   parsed into the fixture (one read, no vtsls).
