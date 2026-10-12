# GM-560: stale semantic edges of unchanged nodes when no pass follows the swap

Status: design (S1). Split from GM-546 (docs/architecture/gm-546-stale-edges.md,
open question 1). Line numbers are on `release-4.6.0` at `e44e80c`.

## 1. Today

A whole-language reindex walks the language into staging, plans the
difference (`language_swap::plan_attached`, core/src/storage/language_swap.rs
176-375) and swaps it in (`swap_attached`, 458-624). For a node whose row and
child rows did not change (an *unchanged node*), the plan keeps its live
outgoing edges. After GM-546 it drops only two kinds: an edge whose target is
gone, and a **syntactic** edge whose id staging lacks (the walk moved it).
A live **semantic** edge (`edges.source = 'semantic'`, schema.rs:311) of an
unchanged node is always kept. That is on purpose: the walk never emits
semantic edges, so "staging lacks it" says nothing about a semantic edge.
Test 12b (`a_moved_syntactic_edge_is_replaced_and_its_semantic_siblings_stay`,
workspace_reindex.rs:1859) already pins it.

The kept semantic edge is fixed later by the semantic pass: a complete
whole-project pass of a sweeping language deletes every semantic edge it
did not re-send (`watcher::apply::sweep_semantic_edges`, apply.rs:787).

**Example.** `src/a.ts` has `import { x } from "@app/x"` and calls
`x.foo()`. The TS semantic tier (vtsls, SDK LSP bridge) once resolved the call
through the receiver: semantic edge `a.main CALLS app/x.ts::foo`. Then the TS
plugin is memory-suspended, and tsconfig `paths` retargets `@app/*` to
`app2/*`. The whole-language reindex:
- moves the syntactic `a.ts IMPORTS app/x.ts` to `app2/x.ts` (GM-546) and
  lists `src/a.ts` in `plan_pending_files`;
- keeps `a.main CALLS app/x.ts::foo` (semantic, target still exists);
- runs no pass (suspended). `find_callers(app/x.ts::foo)` returns `a.main`, a
  false caller; `find_callers(app2/x.ts::foo)` misses it.

## 2. When a pass follows the swap, and when not

All whole-language swaps go through `workspace_reindex::rebuild` (399-483),
called by `run_with` (203) and `retry_failed` (334). `run_with` then calls
`semantic_pass_after_swap` (267-326). `config_reindex::run`
(config_reindex.rs:74) reaches it through its `Fallback` branch.

| Case | What happens | Recorded as | Pending rows |
|---|---|---|---|
| Manifest has no `semantic_pass` | `semantic_pass_after_swap` does nothing | nothing | not written (`swap_attached` 579) |
| Pass completes / settles | edges refreshed; sweep for Go, Rust, Python | `record_language_semantic_pass(_settled)` | cleared |
| Plugin asleep or memory-suspended | `supervisor.semantic_pass` returns `Ok(None)` | `semantic::record_not_run` (semantic.rs:585) -> failure reason | cleared (schema.rs:1467) |
| Pass errors (crash, timeout, protocol) | `Err` | `semantic::record_failure` (semantic.rs:574) | cleared |
| Pass incomplete (some files unfinished; vtsls missing gives an empty incomplete diff) | `Residual` | `record_language_semantic_residual` (schema.rs:1347) | cleared (1371) |
| Daemon dies between swap and pass | the next start re-runs the pass | (later outcome) | kept until then |

Only "suspended" is known before the plan (`is_semantic_suspended`). Asleep
is known only inside `semantic_pass`; error, timeout and residual only after.
The plan runs before any of it.

A language without `semantic_pass` cannot hold semantic edges, so it is out
of scope. Every other no-pass case leaves `semanticPassAt` NULL (the swap
sets it NULL, language_swap.rs:567).

## 3. Signals answers already carry

`mcp::provenance` (core/src/mcp/provenance.rs; ADR 0009) puts a `provenance`
block on four tools:
- `semanticTier: "pending"` plus `pendingFiles` while the post-swap pass is
  owed and has not finished. The files are the swap's `plan_pending_files`.
- `semanticTier: "absent"` when the tier did not complete and nothing is
  pending. That is every no-pass row of the table. Its session hint
  (`session_hints::PROVENANCE`, session_hints.rs:85) says: method calls
  through a variable receiver *may be missing*; grep.

So in the example the answer already says `absent`. The gap: `absent` warns
only about missing edges, not about a present edge pointing at an old target,
and it does not say which files. ADR 0009 dropped the file list when the pass
fails or does not run. The owner accepted that on 2026-09-26 (ADR 0009,
Status).

## 4. Options

**A. Keep until the next pass (today).**
- Benefit: no code. Every receiver call survives. A sweeping language
  (Go, Rust, Python) is fixed by its next complete pass.
- Risk: present edges can be silently wrong. The only signal is the
  language-wide `absent`, which says "missing", not "wrong".
- Example: `find_callers(app/x.ts::foo)` lists `a.main` with
  `provenance: {language: typescript, semanticTier: absent}`.

**B. Drop every semantic edge of an unchanged node when no pass will follow.**
- Benefit: no wrong targets.
- Risk: the walk never emits semantic edges, so this drops *all* of the
  language's receiver calls, CALLS/REFERENCES/SUPERTYPE_OF onto placeholders,
  until a pass. On a big TS repo every caller query degrades. The plan can
  know only "suspended" in advance, so error, timeout, asleep and residual
  would need a second delete after the pass outcome, outside the swap
  transaction.
- Example: `find_callers` on either `foo` returns no `a.main`, nor any other
  receiver caller in TypeScript.

**C. Drop the semantic edges of the files the walk saw move.** Like B, but
only for nodes in `plan_pending_files`, after a no-pass outcome.
- Benefit: the loss stays in files with evidence.
- Risk: the evidence is per file, not per edge. `a.ts`'s receiver calls into
  unrelated modules vanish too. A file whose semantic targets moved without
  any syntactic edge moving (it reaches `@app/x` through a re-export) keeps
  its stale edge anyway. Data loss with partial coverage.
- Example: `a.main CALLS app/x.ts::foo` is gone, and so is
  `a.main CALLS lib/y.ts::bar`, which was right.

**D. Mark stale: keep the edges and keep the swap's file list past a no-pass
outcome (recommended).** A not-run, failed or residual pass clears the
language's `semantic_pending` row as today (so the tier is `absent`). It keeps
its `semantic_pending_files` rows. `absent` then also lists the response's
files among them as `staleFiles` ("edges of these files changed in the last
reindex and no semantic pass has re-checked them"). These rows are cleared,
as today, by a complete pass, by a complete per-file pass of the file, and by
a wipe.
- Benefit: no data loss. The warning lands on the files where the walk saw
  resolution move, and on the false-caller case: the caller row's file is in
  the response. It costs zero bytes when nothing is stale. It reuses the
  table, the writer (the swap) and the bounded list code of `pendingFiles`.
- Risk: it reverses ADR 0009's accepted point (rows outlive a no-pass
  outcome), so the ADR needs an amendment. It adds a response field and needs
  new hint text. Coverage is the same per-file evidence as C (a stale edge in
  a file with no moved syntactic edge is not listed). It may over-warn: a
  file re-extracted without a pass keeps its row until a pass.
- Example: `find_callers(app/x.ts::foo)` lists `a.main` with
  `provenance: {language: typescript, semanticTier: absent, staleFiles:
  ["src/a.ts"]}`.

**E. Per-edge stale flag** (an `edges.stale` column set by the swap on kept
semantic edges of pending files). Same evidence as D, with a per-row cost, a
schema migration and every edge reader to touch. Rejected: it gives no
precision over D, because the evidence is per file.

**F. Recognise a retarget by specifier.** Drop a live semantic edge when
staging has an edge of the same `(fromId, kind, specifier)` with a different
target. Precise where it applies, but semantic CALLS/REFERENCES carry no
specifier (bridge.rs:1763 writes `specifier: None`). It would catch only
in-place upgrades of import-like edges, which the bridge does not emit
today. Rejected: it covers nothing real.

## 5. Recommendation

**D.** Today's rule (keep) stays for the edges, and the evidence the swap
already computes is no longer thrown away when no pass refreshes it. If the
owner prefers not to reopen ADR 0009, use **A** and close GM-560 with test 12b
plus one no-pass test (behaviour 1 below) as its pin.

## 6. Edit map (D)

Change:
- `core/src/storage/schema.rs` `record_language_semantic_pass_failure`
  (1459-1469) and `record_language_semantic_residual` (1347-1374): delete only
  the `semantic_pending` language row, keep `semantic_pending_files`. Split
  `clear_semantic_pending` (1490-1497) into a language-row clear and the full
  clear. Success paths (1312, 1330) keep the full clear.
- same file, `clear_stale_semantic_pending` (1684-1700): keep its rule. It
  runs on file rows of languages that are done or no longer capable, which
  still holds.
- same file, `semantic_pending` (1705-1730, read by `g-mesh status`):
  report stale-file counts for languages with file rows and no language row,
  or leave them out (status wording decision, section 8 Q3).
- `core/src/mcp/provenance.rs`: add `stale_files` (+ `stale_files_omitted`)
  to `Provenance` (198-221). `disclose`'s `Absent` arm (334-372) reads
  `semantic_pending_files_among` like the `Pending` arm, and still degrades to
  plain `absent` on a read error. `page_reserve` (322) reserves the list for
  `Absent` too. Update the module doc's "Pending" section (136-157).
- `core/src/mcp/session_hints.rs` `PROVENANCE` (85-87): one clause for
  `staleFiles` (edges there may point where the old configuration resolved
  them).
- `docs/adr/0009-semantic-pending.md`: amend Status and "Who clears" (97-124).

Read for context (no change expected): `language_swap::plan_attached`
pending-files insert (328-337); `swap_attached` pending writes (572-588);
`workspace_reindex::semantic_pass_after_swap` (267-326);
`semantic::record_not_run`/`record_failure` (574-590);
`watcher::apply` per-file clears (700-725); `schema::semantic_pending_files_among`
(1513), `clear_semantic_pending_files` (1544); the four tools calling
`provenance::resolve`/`disclose` (e.g. find_implementations.rs:206-212).

## 7. Behaviours to pin, one control each (revert the code, never the test)

1. A swap followed by a not-run pass keeps an unchanged node's semantic edge
   to a still existing target (A and D both). Test: `workspace_reindex`, a
   suspended or asleep supervisor, sibling syntactic edge moved. Control: drop
   `{x}.source = 'syntactic' AND` from `dropped` (language_swap.rs:279), and
   the semantic edge is deleted. Test 12b already covers this with a pass; the
   new test covers the no-pass path.
2. D: after the not-run outcome, the swap's `semantic_pending_files` rows
   survive and the `semantic_pending` row is gone. Control: restore the full
   clear in `record_language_semantic_pass_failure`, and the rows are gone.
3. D: same after a residual outcome. Control: restore the full clear in
   `record_language_semantic_residual`.
4. D: an `absent` answer naming a stale file carries `staleFiles` with it
   (anchor file first, capped as `pendingFiles`). An answer naming none
   carries plain `absent`. Unit test in `provenance::tests`. Control: make the
   `Absent` arm return `Provenance::absent` without reading rows, and no
   `staleFiles` appears.
5. D: a later complete pass clears the stale rows. This is existing
   behaviour (`record_language_semantic_pass`, ADR 0009 tests), so no new
   control. Assert it in test 2's continuation only if cheap.

Controls 2 and 3 are in schema.rs and control 4 in provenance.rs. They can
share one build.

## 8. Open questions for the owner

1. **D or A.** D reopens ADR 0009's accepted point "a failed, incomplete or
   not-run pass deletes the pending rows". A keeps it, and leaves the
   false-caller case flagged only by the language-wide `absent`.
2. **TypeScript can keep a stale semantic edge even after a complete pass.**
   This was found while reading the code and not reproduced. TS declares
   `semantic_sweep = false` (plugins/typescript/plugin.toml:38), and the
   bridge retracts only the ids its own process emitted (apply.rs:424-432). A
   pass in a new plugin process (after a suspension or a restart) re-sends
   `a.main CALLS app2/x.ts::foo` but leaves `CALLS app/x.ts::foo`. Then
   `semanticPassAt` is set, provenance is silent, and D's rows are cleared.
   This goes beyond "no pass follows". Proposal: a separate task (turn on the
   TS sweep, or retract by the specific edges after a swap). It is not GM-560.
3. **`g-mesh status` for stale files**: show "N file(s) stale since the last
   reindex" next to the not-run reason, or nothing? Proposal: show it, one line.
