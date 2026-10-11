# GM-546: whole-language reindex keeps stale edges of unchanged nodes

Status: design (S1). Scope: `storage::language_swap::plan_attached` only.

## 1. Root cause

`workspace_reindex::rebuild` (core/src/daemon/workspace_reindex.rs:400-490)
walks the language into staging and calls `language_swap::plan`
(core/src/storage/language_swap.rs:154), whose only body is
`plan_attached` (171-351). The plan applies the *unchanged-node rule*
(module doc, lines 12-24): a node whose `nodes` row and child rows are equal
in live and staging (`plan_unchanged_nodes`, 247-255) keeps its live
outgoing edges and gets none of staging's, with two exceptions:

1. a live **syntactic** edge whose staged edge **of the same id** differs is
   replaced (`plan_upsert_edges`, branch `t.source = 'syntactic'`, 289-300);
2. a live edge whose **target does not survive** the swap is deleted
   (`plan_delete_edges`, 271-283) and the staged edges of the same
   `(fromId, kind)` are taken instead (`dropped_survives` branch, 289-300).

Edge ids include the target: `ids::edge_id` (plugins/sdk/src/ids.rs:155) is
`digest("edge", [from, kind, to(, ordinal)])`, used by the TS
(`extractor/model.rs`, `sites.rs`), Rust and Python (`extractor/emit.rs`)
plugins and the SDK LSP bridge. In the paths-swap fixture the `File` node of
`src/a.ts` is unchanged (its row carries no import target), the live edge
`edge(a, IMPORTS, app/x)` and the staged `edge(a, IMPORTS, app2/x)` have
**different ids**, so exception 1 does not apply, and `src/app/x.ts` still
exists, so exception 2 does not either. The live edge is kept, the staged one
is dropped. The selective path (GM-509) re-extracts the file through
`PluginProcess::reextract`, which replaces the file's edges, so it is correct.

Confirmed by a throwaway unit test in `language_swap::tests` (live
`a->app`, staging `a->app2`, all three `File` nodes equal): after
`plan`+`swap`, `edges` holds `a->app file-app/x.ts`, not
`a->app2 file-app2/x.ts`. Test deleted, not committed.

Not TS-specific: any language whose walk moves an unchanged node's edge to a
different, still-existing target hits it (Go `go.mod` replace, Rust
`Cargo.toml` path dep, Python roots). The crash-resume case (GM-509 F2) is the
same bug: files re-extracted under v2 before the crash have rows equal to the
v1 walk, so their v2 edges survive the resume's whole-language reindex.

With a semantic pass after the swap (whole-project, `semantic_pass_after_swap`
278) TS semantic edges are redone, but a live *syntactic* edge the pass never
touches can still stick; the bug is sure-fire with the pass off or suspended.

## 2. Options

**A. Staging is authoritative for an unchanged node's syntactic edges
(recommended).** Treat a live edge from an unchanged node as *dropped* when its
target does not survive (today) **or** it is `source = 'syntactic'` and staging
lacks its id. Dropped edges are deleted, and staged edges of the same
`(fromId, kind)` that live lacks are taken in their place: exception 2 with a
wider "dropped" predicate, in both SQL statements.
- Benefit: two predicate edits in one function; every language; semantic
  edges and the pass's in-place upgrades keep today's rule (tests 7, 8, 12
  hold: in them no syntactic id disappears from staging). A walk never emits a
  semantic-source edge, so a live syntactic edge staging lacks can only mean
  "the walk no longer resolves it there". Cost on a big repo: one PK probe per
  live edge of unchanged nodes (`e.id NOT IN main.edges`), plus the existing
  `idx_edges_fromId` lookup; only files whose edges really move become pending.
- Risk: a structural edge a semantic pass *retracted* comes back when the same
  node also lost a syntactic edge of that kind in this swap (both happen only
  on a real resolution change; the post-swap pass, when it runs, retracts it
  again). Live semantic edges onto a stale target stay when no pass runs after
  the swap (open question 1).

**B. No unchanged-node rule when no semantic pass follows the swap.** Pass a
flag to `plan`; with it, every node gets exactly staging's edges.
- Benefit: simplest SQL (skip the `plan_unchanged_nodes` insert).
- Risk: with the pass suspended (not off), every semantic edge in the language
  is downgraded to structural until a later pass: on a big TS repo, all
  answers degrade for the duration. Leaves the pass-on case to the pass.

**C. Target-free edge ids** (`from, kind, specifier/site`), so exception 1
catches the retarget.
- Benefit: fixes it at the identity level.
- Risk: id scheme change across all plugins and the SDK, schema/version bump,
  full reindex for every user, `replaces` links in `sites.rs` re-derived;
  out of proportion for this bug.

Recommendation: **A**.

## 3. Edit map

Change:
- `core/src/storage/language_swap.rs` `plan_attached`, `plan_delete_edges`
  statement (lines 268-283): CASE branch for unchanged nodes becomes
  `NOT {target_survives} OR (e.source = 'syntactic' AND e.fromId IN
  plan_unchanged_nodes AND e.id NOT IN (SELECT id FROM main.edges))`
  (kept placeholders keep the target-only rule: staging has no rows for them).
- same fn, `plan_upsert_edges` (286-300): `dropped_survives` branch uses the
  same "dropped" predicate for `d`. Factor it into a closure next to
  `survives` (258) so both statements share one definition.
- module doc (lines 12-24) and `PLAN_DDL` comment (67-74): restate exception 2.

Read for context: `swap_attached` (438-600, applies the plan tables; no change
expected), `plan_pending_files` (306-316, picks up the moved files already),
`workspace_reindex::rebuild` (400-490), `config_reindex::run`/`selective`
(core/src/daemon/config_reindex.rs:74-184), `resume_pending`
(workspace_reindex.rs:161), tests 7/8/12 in workspace_reindex.rs
(1635, 1729, 1822).

## 4. Behaviours to pin (control = revert in `plan_attached`)

1. Unchanged node, syntactic edge retargeted to a still-existing node with a new
   id: live ends with only the new edge (unit test in `language_swap::tests`).
   Control: drop the `e.source = 'syntactic' ...` disjunct from
   `plan_delete_edges` -> old edge stays.
2. Same fixture: the new staged edge is taken. Control: revert the `d`
   predicate in `plan_upsert_edges` to `NOT {dropped_survives}` -> no IMPORTS
   edge from `a`.
3. Paths-swap through `config_reindex::run` with `resolution_delta` off and with
   it on but the >30% fallback forced: `a.ts` IMPORTS reads `src/app2/x.ts`.
   Control: revert both predicates (one build) -> reads `src/app/x.ts`.
4. Crash-resume (extend `an_interrupted_selective_reindex_is_resumed_as_a_whole_language_reindex`,
   config_reindex/tests.rs:625): no v2-only edge, v1 edge restored. Control: as 3.
5. Unchanged node's semantic edge and in-place upgrade survive while a sibling
   syntactic edge moves (extend test 12). Control: drop `e.source = 'syntactic'`
   from the new disjunct -> semantic edges are deleted.
6. Unchanged tree: still only bookkeeping (test 7, unchanged). Control: none new.

## 5. Open questions

1. **Stale semantic edges with no pass after the swap.** Today: a live semantic
   edge of an unchanged node is always kept. Option A keeps that. Example: TS
   pass ran once (semantic `a -> app/x`), then the plugin is suspended and the
   paths target changes: the reindex keeps the semantic edge to `app/x`, and
   adds nothing since its id differs and it is not syntactic. Change if wanted:
   also drop semantic edges staging lacks when no pass will follow. Consequence:
   correct targets, but every semantic-only edge (calls through receivers) of
   that language disappears until the next pass. Recommended: keep (out of scope;
   the post-swap pass, whole-project, normally fixes it).
2. **Retracted structural edge coming back.** Today: an edge a semantic pass
   deleted never returns while its node is unchanged. With A it returns when
   the same node also lost a syntactic edge of that kind in this swap. Example:
   pass retracted `a CALLS util.foo` (wrong target); a config edit moves
   `a`'s other CALLS edge; the reindex re-adds `a CALLS util.foo` as syntactic
   until the post-swap pass retracts it again (forever, if the pass is off).
   Accept, or restrict A's take-side to edges whose `(fromId, kind, specifier)`
   matches a dropped edge (tighter, IMPORTS-only in practice, since CALLS carry
   no specifier). Recommended: accept.
