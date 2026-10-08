# GM-489: one caller row for a typed Rust receiver call

Design note for GM-489, slice S1. Base: `release-3.21.0` at `feab93a`.
Found by GM-485/S3: after an edit, a typed receiver call `x.m()` briefly
has two `CALLS` rows, one structural and one semantic.

## 1. Mechanism

### 1.1 Structural tier (Rust plugin)

`plugins/rust/src/extractor/bodies.rs`:

- `receiver_call` (bodies.rs:553-579). When `receiver_type` knows the
  receiver's type `T`, it binds the call as `T::m` through `member_of`
  (1199-1201) and `tail_in` (1209-1221). It then emits the edge with
  `edge` (1246-1259) and records an open site whose `replaces` holds that
  edge's id (`open_site`, 1261-1285, called at 578).
- `tail_in` returns one of two bounds:
  - `Bound::Here(decl.id)`: `T::m` is declared in this file. `edge` calls
    `resolved_edge(CALLS, from, decl.id)`, an edge straight onto the
    declaration.
  - `Bound::There { target }`: `T::m` is declared in another file. `edge`
    calls `placeholder_edge` onto a `PendingSymbol` placeholder with
    target `Container(T's container)` and key
    `QualifiedName(qualified_in(container, "T::m"))`.
- Edge id: `ids::edge_id(from, kind, to, None)` (plugins/sdk/src/ids.rs:155)
  is `digest(from, kind, to)`. It encodes no call site, so every call from
  `F` onto the same `to` shares one row.
- Placeholder id: `graph::placeholder_id` (plugins/sdk/src/graph.rs:638)
  uses `render_target` (618-628), which reads scope and key only. It
  ignores `from_container` and `key_path`.

### 1.2 Semantic tier (SDK LSP bridge)

`plugins/sdk/src/lsp/bridge.rs`:

- `record_answer` (1361-1449), `Ask::Definition`. It finds the answered
  declaration node `D` and calls `Answers::record` (928-974). `record`
  always lands on a placeholder addressed by `address_of(D)` (1014-1024):
  `Container(D.container)` plus `QualifiedName(D.qualified_name)`. The
  edge id is `edge_id(from, kind, placeholder)`. `record` stores the id in
  `by_file`, which `answer` later saves as `self.emitted`.
- The contradiction rule (1414-1420): if `site.replaces != edge`, the
  structural id goes into `answers.retract`.
- `Answers::finish` (976-1002) builds the diff, with
  `delete_edge_ids = retract - edges re-emitted`.
- `answer` (1629-1771) calls `retract_stale` (1776-1791). For every file
  the pass finished, it retracts each id from `emitted` that this pass did
  not produce again (1726-1728).

### 1.3 Core

- `watcher::apply::apply_file_change_in` (core/src/watcher/apply.rs:120-170)
  runs two round trips in one unit:
  1. a `fileChanged` round trip (141);
  2. a per-file `apply_semantic_pass_in` (153; definition at 226-295).

  A failed second round trip is only logged (161-166). Callers of
  `apply_file_change` are `daemon::plugin::PluginProcess::send_one` and
  `cli::plugin_check::session::Driver::step` (g-mesh `find_callers`).
- `round_trip` (400-465) commits the `fileChanged` diff with
  `apply_file_diff_linked` (433, core/src/storage/index_store.rs:390-406)
  as a separate step. Readers see it before the semantic pass begins.
- `storage::file_rows::widen` (core/src/storage/file_rows.rs:46-98),
  `FileScope::Complete`. It deletes every stored edge out of the file that
  the diff does not upsert, **except a `semantic` edge from a node that
  stays** (`keep_semantic`, 85-89). The semantic placeholder is a node of
  the file, and the reparse removes it. The semantic edge survives anyway,
  because linking has already moved its `toId` onto `D`.
- `sweep_semantic_edges` (apply.rs:302-325) runs only after a complete
  whole-project pass. It deletes every `semantic` edge of the language
  that the pass did not send.

### 1.4 Why the ids differ, and when they do not (measured)

I ran a throwaway test in `plugins/rust/src/extractor/tests.rs` (reverted,
not committed). It used a two-file crate with `src/a.rs` declaring
`P::m`, and `src/lib.rs` with `run(p: P) { p.m() }` and
`here(q: Q) { q.m() }`, where `Q::m` is declared in `lib.rs`. For each
call it computed the id the bridge would give an answer landing on the
declaration, using the same formula as `address_of` + `placeholder_id` +
`edge_id`:

| Call | Structural `to` | Semantic `to` | Same id? |
|---|---|---|---|
| `here`: `q.m()`, `Q::m` in the same file (`Bound::Here`) | `Q::m` node itself | placeholder `krate::Q::m` | **no** |
| `run`: `p.m()`, `P::m` in another file (`Bound::There`) | placeholder `krate::a::a::P::m` | the same placeholder | **yes** |

This gives two bugs, not one:

- **Same-file (the GM-489 report).** The ids differ. The bridge emits
  `E_sem` and retracts the structural edge `X`. An edit then reparses the
  file: `X` is upserted again and `E_sem` survives (`keep_semantic`). Both
  link to `D`, so there are two rows until the per-file pass retracts `X`
  again. The pass waits on rust-analyzer's readiness and settle, which is
  why the window lasts about 30s. If the pass fails or is incomplete, both
  rows stay until a later pass succeeds.
- **Cross-file (new finding).** The ids are equal, so there is no
  duplicate. But the bridge re-sends `X` as its own semantic edge: the
  upsert overwrites `source` to `semantic`, and the id goes into
  `emitted`. A later pass over that file that gets an empty answer for the
  site then retracts the id in `retract_stale`. That deletes the
  structural edge too: **the call is lost** until the file is next
  reparsed. A complete whole-project pass that no longer sends the id also
  sweeps it.
- The module doc in bodies.rs (39-46) says "the bridge's own edge onto a
  placeholder never shares the structural edge's id". That holds only for
  `Bound::Here`.

## 2. Options

The criteria are:

- **C1**: one caller row at every point, including the post-edit window
  and after a failed pass.
- **C2**: no call is lost when a later pass is empty.

### (a) Core skips the structural upsert when a live semantic edge covers it

In `widen` (or after linking in `apply_file_diff_linked`), core would drop
an upserted syntactic `CALLS` edge `F→…` whose linked target equals a live
semantic edge's target from `F`.

- **C2 fails.** The structural edge is never written. If the next pass
  answers empty, `retract_stale` retracts `E_sem`, and nothing is left for
  the call. Core cannot restore `X`: it never stored it, and the bridge
  only knows its own ids.
- A stale semantic edge after an edit that moved the call: if the call
  moved to a receiver of type `T2`, the reparse writes `X'→D2` and the
  stale `E_sem→D` stays. That is a wrong row until the pass runs, the same
  as today for every semantic edge. If the call left `F`, the structural
  tier no longer emits `F→D` at all, so (a) changes nothing there.
- The check needs post-link targets, which means a query per upserted
  `CALLS` edge on the reparse hot path. It is also a core-wide rule. The Go
  tier also retracts structural edges it contradicts, but it has no
  restore step, so the lost call in C2 would reach Go as well.
- **Rejected.**

### (b) The bridge keeps an agreeing answer instead of retracting

There are two readings of this option.

- **(b1) Emit the semantic edge and keep the structural one.** In the
  same-file case this gives two rows permanently. Rejected.
- **(b2) On agreement, emit no semantic edge and keep the structural edge
  as the only row.** This is the chosen option, extended in §3. On its
  own it is not enough:
  - **Detecting agreement when the ids differ.** The same-file case always
    has different ids, so comparing ids is not enough. The bridge has to
    look `X` up in `index.graph(file)` and also compare `X.to_id == D.id`.
  - **A mixed caller.** One function has a typed `p.m()` and an untyped
    `ps.iter().for_each(|q| q.m())`, both reaching `D`. Under (b2) the
    typed site keeps `X`, and the untyped site still emits `E_sem(F, D)`.
    That is two rows permanently, which is worse than today.
  - **A file left unchanged.** `X` may already be missing because an
    earlier pass contradicted it. A later pass then agrees, or answers
    empty, without the file changing. `retract_stale` drops `E_sem'`, and
    nothing restores `X`, so the call is lost (C2). This hole exists today
    for an empty answer.

### (c) Upgrade in place under the structural id

On agreement, the bridge would send an edge with id `X`, `to = X.to`, and
`source = semantic`. That gives one row at every point. But the id is
remembered in `emitted`, so a later empty answer retracts the structural
row (C2 fails). This is the cross-file bug above. If the id were kept out
of `emitted`, the option would reduce to (b2) with a `source` label that
flips on every reparse. Rejected.

## 3. Decision: (b2) plus restore and coverage, in the bridge only

`replaces` is set only by the Rust plugin. Python and the toy plugin
always send `None` (grep), and Go and TypeScript have their own tiers. So
the change stays inside `plugins/sdk/src/lsp/bridge.rs` and only alters
Rust's behaviour. Three rules apply. "Finished file" means a file in both
`covered` and `asked_about`.

- **R1, restore unless contradicted.** For each open site with
  `replaces = X` in a finished file, the pass re-sends `X` unchanged, as
  the index's own `WireEdge` (`index.graph(file).edges`, matched by id,
  with `source = syntactic`). The only exception is when every answer for
  the sites naming `X` contradicted it. `X` never enters `by_file` or
  `emitted`, so `retract_stale` and core's semantic sweep (which only reads
  `source = 'semantic'`) can never delete it.
  - An empty, ambiguous or unresolved answer restores `X`, as GM-485
    requires: "when it does not answer, the structural edge stands".
  - A file that was not finished is not touched, as today.
- **R2, agreement adds nothing.** An answer on `D` agrees with `X` when
  `X.to_id == D.id` (`Bound::Here`), or when `X.id` equals the id the
  answer would get (`Bound::There` with the same address). An agreeing
  answer records no semantic edge and no placeholder, and retracts
  nothing.
  - It still counts toward `untyped_answered` exactly as today. That count
    is only used for `replaces = None` sites, so nothing changes there.
- **R3, coverage.** Any semantic edge this pass would record for
  `(F, kind, D)` is dropped when a re-sent `X` from `F` of that kind lands
  on `D` by the R2 test. Its placeholder is dropped with it, unless some
  kept edge still uses that placeholder. If an earlier pass in this
  process emitted it, `retract_stale` now retracts it.
  - This handles the mixed caller. A semantic id equal to a re-sent `X` is
    dropped from the semantic upserts and from `by_file`. That fixes the
    cross-file loss.

Contradiction stays as today: an answer that lands on `D' ≠` the
structural target records `E_sem'` and retracts `X`.

### 3.1 Walk-through against the criteria

The main case is a same-file typed call that rust-analyzer confirms.

| Point | Rows for `F→D` |
|---|---|
| Steady state | `X` only (R2) |
| Edit, after the reparse, before the pass | `X` (same id, upserted in place). No semantic edge exists. |
| Pass completes and agrees | `X` (R1 re-sends it unchanged) |
| Pass fails or is incomplete | `X` (core keeps the reparse's commit) |
| Pass answers empty | `X` (R1 restores; there is nothing to retract) |
| Cross-file call, later empty pass | `X`. It is not in `emitted`, so `retract_stale` cannot reach it. |
| Contradicted, then a later pass is empty or agrees (file unchanged) | `E_sem'` is retracted and `X` is re-sent in the same diff, so there is one row |

### 3.2 What stays open

These are pre-existing cases outside the reproduction.

1. **Re-exported head.** *Closed by GM-531*
   ([ADR 0029](../adr/0029-core-ships-its-link-result-to-the-semantic-tier.md)):
   core sends its link result with the `semanticPass`, and R2 treats an
   answer on the declaration core linked the edge to as agreement. The
   original analysis follows.
   `use crate::named::T; x: T; x.m()` addresses
   `named::T::m` at `named`. The linker reaches `D` through the head walk
   in `graph::symbol_links`, "Members of a re-exported head". The semantic
   address is `D`'s own, so both tiers link to `D` while neither R2 test
   matches. The bridge counts this as a contradiction, and the old
   post-edit duplicate window comes back for this shape only.
   - Closing it needs the linker's result, which the plugin cannot see.
     The bridge's design forbids guessing ("never guesses").
   - Proposal: open a follow-up task (core resolves `X`'s placeholder
     before deciding, or a core-side dedup that relies on R1 for restore)
     if a count shows it matters. **Needs your decision:** accept this as
     a residual for GM-489.
2. **A genuine contradiction after an edit.** The reparse re-writes the
   wrong `X→D`, while `E_sem'→D'` survives. `find_callers(D')` shows one
   row. `find_callers(D)` shows a wrong row until the pass runs. This
   happens today and is not a duplicate.
3. **Mixed caller where an edit removes the typed call.** The reparse
   deletes `X`. The untyped call has no row until the pass emits
   `E_sem(F, D)`, so a failed pass delays it. This is narrow and it is not
   an empty pass, so C2 holds.
4. **Source label.** Confirmed typed calls now read
   `source = syntactic`. Other readers of `source = 'semantic'`:
   - `core/src/mcp/untyped.rs:96`, the second `NOT EXISTS` in
     `candidate_sql`. GM-486's trim normally removes the name first, so
     the effect is limited.
   - `mcp/provenance.rs` counts.

   The verify slice should check both.
5. **Upgrade.** A store written by the old bridge can hold `E_sem` with `X`
   retracted. A plugin fingerprint change triggers the language's staging
   reindex, which writes `X` again and clears `semanticPassAt`
   (`language_swap.rs:527-530`). The whole-project pass that follows then
   sweeps the stale `E_sem`. The verify slice should confirm that a
   rebuilt Rust plugin changes its fingerprint.

## 4. Edit map

To change (`plugins/sdk/src/lsp/bridge.rs`):

- `Answers` (801-823): add `confirmed: BTreeMap<String /*X id*/, (from, kind, D id)>`,
  `contradicted: BTreeSet<String>`, `restore: BTreeSet<(RelPath, String)>`,
  and a per-recorded-edge `(from, kind, D id)` map for R3.
- `Answers::record` (928-974): return the id, but defer adding the edge
  and the placeholder into the builder until `finish`. This lets R3 drop
  them before they reach the diff (and before `by_file`).
- `record_answer`, `Ask::Definition` branch (1361-1422): R2 agreement test
  (`index.graph(&question.file)` edge lookup for `X.to_id`, plus the
  prospective id). On agreement it records `confirmed` and skips `record`.
  On a definite contradiction it records `contradicted` and retracts as
  today.
- `Answers::finish` (976-1002): materialise the kept edges and
  placeholders, apply R3, append the R1 re-sent `X` edges to
  `upsert_edges`, and remove them from `delete_edge_ids`.
- `LspBridge::answer` (1629-1771): after `run_pass`, compute R1's restore
  set from `index.open_sites(file)` for the finished files
  (`covered ∩ asked_about`, as at 1736). Pass it to `finish`, which needs
  `index`. The early return for "nothing to ask" (1650-1667) has no
  replaces sites by construction, so it needs no change.
- Docs: the `LspBridge` "Retraction (decision 5)" section (335-358) and the
  bodies.rs module doc (39-46): correct the "never shares the id" claim.

Read for context: `retract_stale` (1776-1791), `run_pass` (1142-1351),
`trim_untyped_calls` and `untyped_call_answered` (1467-1539),
`file_rows::widen`, `apply_semantic_pass_in`, `mcp/untyped.rs::candidate_sql`.

No change in core or in the Rust plugin.

## 5. Test plan

The fixtures avoid rust-analyzer:

- The unit-level `answered_by`/`site_of`/`untyped_index` helpers in
  bridge.rs tests (2133-2242) drive `record_answer` with a literal `Value`.
- `plugins/sdk/tests/lsp_bridge.rs` with `g-mesh-fake-lsp`
  (`plugins/sdk/fake-lsp/main.rs`). Answers are scripted per position, so
  one script can answer position A with `D`, answer position B with `D'`,
  and leave position C unanswered. One bridge can then run pass 1 and
  pass 2 over index variants that move the site between A, B and C (the
  pattern of `an_answer_that_is_no_longer_produced_is_retracted`).
  `fixture_with_structural_edge` (216-270) is the starting point.
- For core: `core/src/daemon/test_plugin.rs` (`set_semantic_pass_answer`,
  `gate_semantic_pass`) and `spawn_semantic_stub` in
  `core/src/watcher/apply/tests.rs` script the plugin's diffs.

Each control below is a revert of production code, never of the test:

| # | Test | Asserts | Control (revert) |
|---|---|---|---|
| T1 | Same-file agree (`X.to_id == D.id`) | no semantic edge, `X` not in deletes, `X` in upserts | drop the `to_id` comparison: an `E_sem` is emitted and `X` retracted |
| T2 | Cross-file agree, then an empty pass (site moved to C) | pass 2 does not delete `X` and re-sends it | put `X`'s id back into `by_file`: pass 2 deletes `X` |
| T3 | Contradict, then an empty pass | pass 1 deletes `X` and emits `E'`; pass 2 deletes `E'` and upserts `X` | drop R1 restore: pass 2 has no `X` |
| T4 | Contradict, then agree (file unchanged) | pass 2 deletes `E'`, upserts `X`, and emits no semantic edge | same as T3 |
| T5 | Mixed caller: typed agree + untyped site reaching `D` | one `F→D` edge (`X`), no `E_sem`; untyped site counted answered | drop R3: an `E_sem(F, D)` appears |
| T6 | Two sites share `X`, one agrees and one contradicts | `X` not deleted | retract on any contradiction |
| T7 | Existing `an_answer_for_a_site_with_a_structural_edge_leaves_one_edge_for_the_call` | unchanged (different address means contradiction) | n/a |
| T8 | Core, with stub diffs shaped like R1/R2 | after `apply_file_change` with the pass answering ok, failing (`gate`/error), and empty, `find_callers(D)` returns 1 row each time | send today's bridge shape (`E_sem` + delete `X`) with a failing pass: 2 rows |
| T9 | E2E, rust-analyzer required (a test dependency of plugins/rust, as in `conformance.rs`) | a two-file crate plus a same-file typed call. Init, edit the file, then count the `CALLS` rows from the caller to `D` after the reparse step and after the pass. Use `HOLD_COMPUTE_FILE_ENV` or the semantic gate to observe the window. Each count is 1 | revert the bridge change: 2 rows inside the window |

T9 is the reproduction the acceptance criteria ask for. If its harness is
too costly, the verify slice can run the same steps by hand against a
built daemon and record the counts. T8 alone does not count as the
reproduction, because the stub stands in for the bridge.

## 6. Scope

This stays within the acceptance criteria:

- one row at every point for the reproduced shape, including after a
  failed pass (§3.1);
- no call lost on an empty pass, which also fixes the cross-file loss;
- every test has a control.

One shape is outside: the re-exported-head residual (§3.2.1). It needs a
decision on whether a follow-up task is opened. Estimated size: about
150-250 lines in bridge.rs, plus tests.

## g-mesh calls relied on

- `get_file_outline` on `plugins/sdk/src/lsp/bridge.rs` (all symbols and
  ranges), `core/src/watcher/apply.rs`, and
  `core/src/mcp/find_callers_callees.rs`. The last one confirmed that a
  caller row is one per edge (`CallSite.edge_id`).
- `find_callers watcher::apply::apply_file_change` → `PluginProcess::send_one`,
  `plugin_check::session::Driver::step`.
- `find_callers lsp::bridge::record_answer` → `run_pass` plus two tests.
- `find_references lsp::bridge::Answers.retract` → `Answers::new`,
  `record_answer`, `finish`, `retract_stale`.
- `search_code` "find callers query" → `find_callers_callees`.

grep was used for `replaces` emitters across the plugins, for
`keep_semantic`, for `'semantic'` readers, and for `pluginFingerprint`.
These are non-symbol or single-known-symbol lookups.
