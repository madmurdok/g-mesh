# GM-693: down-weight short field/const nodes in search_code? (design)

Slice GM-693/S1. Follow-up C of GM-690
(`docs/design/GM-690-fresh-function-ranking-miss.md`). Read-only analysis at
7c4f38b (release-4.24.0 + GM-692); no builds, no new runs.

## Verdict

**Recommendation: cancel GM-693 as specified (no down-weight), and replace it
with a small follow-up that makes a missing rerank model visible.** If the
owner wants coverage for installs that never fetch the cross-encoder, the
fallback is **scope down** (option S below): a kind prior used only when no
cross-encoder scorer exists, through the rerank's own window machinery, gated
by an offline eval before any Rust change.

Why:

- With the cross-encoder, the GM-690 miss is gone: 0/27 edits, 0/19 on
  GM-690's edit set (vs 4/19 cosine-only), two edits below the cosine cutoff
  lifted to rank 1 (`docs/results/GM-692-cross-encoder-fresh-function.md`).
- Of the four cosine-only paths (table below), only (a), "no rerank model",
  is both common and on the user's main path. Its cause is a 91 MB file that
  `model fetch` has downloaded by default since v3.18.0 but that nothing
  re-fetches on upgrade. The machine GM-690 ran on (g-mesh 4.23.0) was one of
  these. Getting the model onto those machines buys the whole measured rerank
  gain: held-out NL r@10 0.640 -> 0.676, MRR 0.409 -> 0.456. A hub
  down-weight can buy at most part of that, and nobody has measured it.
- The full down-weight (option P) changes `search`. That function feeds the
  rerank window, `find_definition`'s semantic rung and the eval's parity
  check. So it changes the reranked path, which GM-692 just showed is fine,
  and it needs its own eval and floor re-check for a failure that path
  no longer has.

## Where cosine-only ranking still reaches users

The ranking is produced in one place: `search_code::search`
(core/src/mcp/search_code.rs:232-269), a brute-force
`1 - vec_distance_cosine` over `vectors JOIN nodes`, paginated by score.
`search_code::handle` (312-397) is the only place that takes a scorer
(`embedding.reranker().scorer()`, line 352), and only when
`(cursor, coverage) == (None, None)`. Every other branch ranks by cosine.

| path | how it skips the rerank (code) | how common | do hubs setting the cutoff hurt here? |
|---|---|---|---|
| **(a) no scorer** | `Reranker::scorer` (core/src/embedding/rerank.rs:340-359) returns `None` when `[rerank] enabled = false`, when `G_MESH_RERANK=off`, or when the model fails to load (missing files or a sha256 mismatch). It logs one line, once per daemon, and the `OnceLock` keeps that outcome until restart. `handle` then takes the `(None, None)` arm: plain `search`. A failing call (`Reranker::order` -> `None`, rerank.rs:364-391) or a window with fewer than 2 rows (`first_page_reranked`, search_code.rs:485-492) also falls back to plain `search`, for that call only. | **Common.** Nothing downloads a model automatically, by design (README). `install.sh`/`install.ps1` only print "the embedding model: g-mesh model fetch". `model fetch` has included the rerank model since v3.18.0 (`--no-rerank` skips it), but an upgrade does not re-fetch. So every install that fetched before 3.18.0 runs cosine-only, and the only sign is one daemon-log line. `model status` reports it, but only when someone runs it. Proof it happens: GM-690's machine, at 4.23.0, had no rerank model. Rate across users: unknown (no telemetry). Per-call failures and short windows: rare. | **Yes, fully.** This is the GM-690 setting: 4/19 fresh-function misses on one prose query, with ten Variable nodes at cosine 0.48-0.55 setting the cutoff at 0.481. Aggregate cost of having no rerank (ADR 0016 / GM-464 S1, held-out NL): r@10 0.640 vs 0.676, MRR 0.409 vs 0.456. How much of that gap is caused by hubs is **not measured**: no eval breaks misses down by the kind of the rows that outrank the positive. |
| **(b) partial pages during the backfill** | `wait_for_embeddings` (core/src/mcp/mod.rs:592-627) returns `Some(Coverage)` once the bounded wait times out while the embedding pass is still owed. `handle` then never asks for a scorer (D6) and always uses plain `search`. Later calls in the same pass do not wait (`embedding_wait_spent`). | **Transient.** Only while an embedding pass is running: first index, a model change, a re-embed after a noise purge. Edits do not trigger it: in GM-692 the rerank ran on 27/27 edit reps. | **Mildly.** The page is already marked partial (`partial`, a `Coverage::note`, no verdict), and the candidate set is incomplete anyway. Hubs crowd it the same way, but the user is told the answer is provisional. No eval data. |
| **(c) continuation pages** | `limit < 30` (default 20): page 1 has the first 20 reranked rows, and the `rerank:` cursor holds reranked rows 21-30, then `search`'s cursor. `continue_rerank` (search_code.rs:532-570) shows them in reranked order without scoring, then cosine order. `limit >= 30`: page 1 has 30 reranked rows plus cosine rows 31..limit, and later pages are plain `search`. | Whenever a caller pages past the first page. | **Barely.** Beyond the window, hubs only compete for ranks 31+. Their real cost there is upstream: a positive pushed below cosine rank 30 is never reranked. GM-690's misses ranked 11-25, inside the window. GM-443 found that a larger candidate set is not converted: "K=100 never beats K=50" on the gated numbers, even though the ceiling rises from 0.839 to 0.886. So lifting the window ceiling a little is unlikely to pay. |
| **(d) other callers of `search`** | `find_definition::by_semantic_neighbours` (core/src/mcp/find_definition.rs:964-1014) calls `search` directly (20 candidates, per-language floor) and never reranks. `top_k_for_eval` (search_code.rs:274-281) is the eval's harness-parity check (`cli/embed_eval.rs:1654`). | `by_semantic_neighbours`: whenever `find_definition` falls through to its semantic rung. `top_k_for_eval`: eval only. | **No.** The semantic rung's query is a symbol name, not prose. The hub effect is a prose-query effect, and name queries already reach r@10 0.930 on int8-structured. `top_k_for_eval` is a parity check, not a user path, but any change to `search` must change it in step. |

Conclusion: (a) is the only path that is both material and common, and its
root cause is a missing download, not the ranking.

## Options

| option | what | benefit | risk |
|---|---|---|---|
| **C. Cancel + make the missing model visible (recommended)** | Cancel GM-693. New small task: when `Reranker::scorer` resolves to "not available" (not "switched off"), say so where the user sees it: a `hint` on `search_code`'s first page, a `g-mesh status`/doctor line, and an install/upgrade message saying "`g-mesh model fetch` (again, after upgrading from < 3.18)". | No ranking change, no eval, no floor re-check. Each install that follows the hint gets the whole measured gain (+3.6 r@10, +0.047 MRR). | Depends on the user acting. Installs that stay cosine-only (offline, `--no-rerank`, `[rerank] enabled = false`) keep the hub effect. A hint costs tokens on every first page until the model is fetched (once per session would be enough). |
| **S. Scope down: a kind prior only when there is no scorer** | When `scorer()` is `None`, rerank the same 30-row window with a built-in `PriorScorer: Scorer` instead of the cross-encoder. It returns logit `-BETA * p` for hub rows and `0` for all others, so `blended_order` sorts by `cosine - p`. Everything else is reused: window, `rerank:` cursor, `judged` on the cosine page, `score` stays the cosine. | Helps exactly path (a). The cross-encoder path, `search`, the semantic rung and eval parity stay unchanged. Few new lines: no new cursor or pagination code. | Unmeasured. The closest measured analogue, GM-443's graph prior over int8, gained +3.8 r@10 but failed Q4 (confident-wrong +6.3 [+9.7]): "the extra rows it pulls up clear the floor while being wrong". Hub-like Variable positives exist in the eval (65 of 1,036 sampled targets are `Variable`), and they lose. Adds about 2-5 ms per first page to read the window's kind and text length. Only reorders within the top 30. |
| **P. Proceed: down-weight inside `search`** | Subtract `p` in the SQL score (or before paging) for hub rows, for every caller. | Also lifts positives above rank 30 into the rerank window. One rule everywhere. | Changes the reranked path (GM-692's 0/27), `find_definition`'s semantic rung (its floors are fitted to the cosine, `gm-468`), and `top_k_for_eval` parity. If `score` itself changes, the `noMatch`/`lowSimilarity` floors move. If only the order changes, `paginate_by_score`'s keyset cursor (by score) breaks. Needs the full eval plus floor re-fits. That is the most work, for the case GM-692 shows is solved. |

### What counts as a "hub" (for S or P)

There are three ways to choose the penalised rows. Each needs only `nodes`
columns, which `window_texts` already reads by id:

1. **By kind:** `kind = 'Variable'` (fields, consts, statics). The GM-690
   hubs `BUDGET`, `Shape.pending` and `Coverage.waited` are all `Variable`
   nodes. Simple, but it penalises a well-documented const as much as a
   bare field.
2. **By text length:** `text_to_embed(doc, signature)` shorter than N
   characters, or no doc comment. This targets the mechanism (a short text's
   vector sits near the "generic code" centroid, so any prose query lands
   near 0.5 on it) whatever the kind.
   Risk: a short, well-named function gets penalised too.
3. **Kind and length together, as a floor on the penalty:** penalise only
   `Variable` rows with no doc (or text < N chars), by a fixed `p`. Rows with
   a doc keep their cosine.

Option 3 is the narrowest. It matches GM-690's own wording ("short
field/const texts with no doc"). Whether the GM-690 hubs actually had no doc
comment was **not checked** here; checking it is step 1 of the eval below.
`p` comes from the FIT half of the eval, from a grid such as 0.01-0.05: the
GM-690 margins were 0.0007-0.024 below a cutoff of 0.481.

## Eval impact plan (S or P only; offline first)

1. **Offline, Python, no daemon change.** Work from the stored GM-398 / GM-465
   rankings (`work/runs-gm465/jina-v2-base-code-int8-structured`, top 100 per
   query) and the snapshots (for `kind`, `docComment`, `signature`). Add a
   `prior` variant to `eval/embedding/rerank_eval.py` next to its graph
   variant, which already reads `nodes`/`edges` from the snapshot. Arms:
   - int8-structured (cosine only, shipped floors): the baseline for S;
   - int8-structured + prior (S): the arm that decides S;
   - F4s K=30 beta=80 (shipped rerank);
   - F4s with the prior applied before the window: the arm that decides P.
   Report held-out NL r@10 / MRR with one-sided 95% bounds, name r@10 / MRR,
   and the D9 gates Q1-Q5 (especially Q4 confident-wrong, which the graph
   prior failed). Also report per language, and the rank change of
   `Variable`-kind positives specifically. Tune `p` on the FIT half and
   score it on the held-out half.
   Controls: `p = 0` must reproduce int8-structured on every cell, the
   C4-style identity control GM-464 S1 used. A shuffled-prior arm must fail
   Q1.
2. **Gate.** S proceeds only if the prior arm is non-inferior to
   int8-structured on r@10 and MRR (lower bounds above the D9 margins) and
   passes Q4/Q5. P additionally needs F4s+prior to be non-inferior to F4s,
   and the name-query false-refusal rate of the semantic rung (gm-468) to
   stay at or below about 3% per language.
3. **Product parity (Rust).** `cli::embed_eval` has no rerank arm, and its
   `top_k_for_eval` check covers `search` only. For S that is unchanged by
   design. Add a fixture-driven parity test instead (like
   `rerank_parity.json`): the Python prior's order for a few fixed windows
   must equal `blended_order` under `PriorScorer`.
4. **GM-690 case.** Rerun `eval/gm692/run.sh` with `G_MESH_RERANK=off` and
   the prior on. Target: 0/19 misses on GM-690's edit set (baseline 4/19).
   Do not compare the latency numbers: the machine was loaded.

## Edit map (S)

- `core/src/embedding/rerank.rs`: `PriorScorer` implementing `Scorer`
  (`score(query, texts) -> logits`). It needs the rows' kind and doc
  presence, and the trait only receives texts today. Either extend
  `window_texts` to return `(text, kind, has_doc)` and give the prior its own
  entry point, or make the prior a pure function
  `prior_order(cosines, kinds, has_doc, p)` that reuses `blended_order`'s
  stable sort. Do not overload `Scorer`. Constants: `PRIOR_PENALTY`, the
  kind set, the length threshold.
- `core/src/mcp/search_code.rs`: in `handle`, the `(None, None)` arm when
  `cursor` and `coverage` are both `None` and the scorer is unavailable
  calls `first_page_reranked`, generalised to take an "orderer" (the
  cross-encoder or the prior). `continue_rerank`, `RerankCursor` and
  `judged` stay unchanged. A missing scorer must stay distinguishable from
  "switched off": with `G_MESH_RERANK=off` / `[rerank] enabled = false`,
  should the prior apply? Recommendation: no. "Off" keeps today's exact
  cosine order, the escape hatch the eval and the tests rely on.
- `core/src/embedding/rerank.rs` `Reranker::scorer`: expose the outcome as
  `Off | Unavailable | Loaded(&dyn Scorer)` instead of `Option`. That lets
  `handle` (and option C's hint) tell the cases apart. `find_callers
  Reranker::scorer` returns only `search_code::handle`, so the signature
  change touches one call site.
- Do **not** touch `search`, `top_k_for_eval` or `by_semantic_neighbours`.
- Docs: ADR 0016 addendum (or a new ADR) and the `search_code` tool
  description ("score is the embedding cosine; order may differ").

Edit map for C: `Reranker::scorer` outcome enum (as above), a hint in
`search_hint` (search_code.rs) on the first page when `Unavailable`, the
`status` output in `core/src/cli/model.rs:485-520` (already reports the
files; add the "search_code cannot rerank" line when they are missing), and
the post-install text in `scripts/install.sh:844` / `install.ps1:645`.

## Behaviours to pin (S)

1. With no scorer and the prior on: a documented function at cosine 0.47
   ranks inside a 10-row page above ten doc-less `Variable` rows at
   0.48-0.55. This is GM-690's shape as a fixture. **Control:** remove the
   prior from the `(None, None)` arm; the function must drop off the page.
2. With the rerank switched off (`Reranker::off` / `G_MESH_RERANK=off`), the
   page is exactly the cosine order. Today's tests cover this; keep them.
   **Control:** apply the prior on "off" too; the test must fail.
3. With the cross-encoder loaded, the order is unchanged by the prior, and
   the existing `rerank_parity.json` and `search_code_rerank_tests` still
   pass. **Control:** stack the prior on top of the CE blend; parity must
   fail.
4. `score` stays the cosine, and `noMatch`/`lowSimilarity` are judged on the
   cosine page. **Control:** judge on the prior-ordered page; a fixture
   where a prior-lifted row from positions 11-30 would change the verdict
   must fail.
5. Continuation after a prior-ordered first page (`limit < 30`) returns the
   rest of the window and then cosine order, with no duplicate or skipped
   row. This reuses `continue_rerank` and its existing test, applied to the
   prior. **Control:** hand out `search`'s plain cursor instead of the
   `rerank:` cursor; rows duplicate.

Behaviours to pin (C): `scorer()` reports `Unavailable` for a missing model
directory and `Off` for `G_MESH_RERANK=off`. The search hint appears only
for `Unavailable`. **Control:** emit the hint for `Off`; the test must fail.

## Decision needed from the owner

1. Cancel GM-693 and open option C as a new task (recommended), **or** run
   step 1 of the eval plan (offline, Python only) to decide S on data,
   **or** proceed with P.
2. If S: should the prior apply when the rerank is switched off by config or
   env, or only when the model is unavailable? Recommendation: only when it
   is unavailable.

## Appendix: g-mesh calls this note relied on

Project `g-mesh` (main checkout; this code is unchanged on the branch).
Every answer was complete (`hasMore: false`), with `provenance.semanticTier:
"absent"` for Rust. Following the tool's guidance, that one gap (method calls
through a receiver) was checked with grep, as noted.

- **What calls the rerank:** `find_callers Reranker::scorer` returned one
  caller, `mcp::search_code::handle` (search_code.rs:312). Grep for
  `.scorer()` / `.reranker()` (the receiver-call gap) also finds only
  search_code.rs:352 (`scorer()`) and :490 (`reranker().order` inside
  `first_page_reranked`).
- **Who bypasses it (the cosine-only ordering):** `find_callers
  search_code::search` returned `handle`, `first_page_reranked`,
  `continue_rerank`, `top_k_for_eval`, `find_definition::by_semantic_neighbours`
  and tests (`files`: search_code.rs 13, find_definition.rs 1,
  find_definition/tests.rs 1). `find_callers
  find_definition::by_semantic_neighbours` returned `by_file_name` only.
  `find_definition by_semantic_neighbours` (source.text): calls `search(...,
  SEMANTIC_CANDIDATES, None)` with no rerank. Grep `top_k_for_eval(` returned
  `cli/embed_eval.rs:1654` (a free function inside `crate::mcp`, re-exported).
- **Where "rerank model not available" is decided:** `find_references
  Reranker::from_environment` returned one row, in `EmbeddingPipeline::build`
  (pipeline.rs:238), which is the production pipeline. `find_references
  Reranker::off` (`answer: files`) returned pipeline.rs with 2 refs:
  `EmbeddingPipeline::disabled` and the test-only `with_loader`. The decision
  is in `Reranker::scorer` (rerank.rs:340-359, read by line range after
  `get_file_outline`). The settings come from `RerankSettings::from_environment`
  (env `G_MESH_RERANK`, global `[rerank] enabled`, `default_rerank_model_dir`).
- **Outlines:** `get_file_outline core/src/mcp/search_code.rs` and
  `core/src/embedding/rerank.rs`, to locate `search`, `handle`,
  `first_page_reranked`, `continue_rerank`, `scorer`, `order` and `WINDOW`.
- **Grep, for non-code questions:** `model fetch` / `--no-rerank` in
  README.md, scripts/install.sh, scripts/install.ps1 and cli/model.rs (how
  installs get the model). `git tag --contains 050d95d` (GM-464 S3) returned
  v3.18.0 as the first release with the rerank. The `kind` counts in
  `eval/embedding/targets/*.jsonl`: Function 675, Type 270, Variable 65,
  File 15, Module 11.
