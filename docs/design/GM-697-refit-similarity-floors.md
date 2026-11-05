# GM-697: re-fit the similarity floors on the GM-460 v2 eval set (design)

Slice GM-697/S1. Source: GM-460/S19 (`docs/results/gm-460-enlarged-eval.md`,
"Floors: shipped vs D6 refit on v2 FIT"), owner decision on GM-460:
"Отдельная задача на переподбор". Read-only analysis on branch
`feat/GM-697-refit-similarity-floors` (= `release-4.26.0`, no code changes);
no builds, no new runs. Every number below is copied from a results file
named next to it, or marked **(measure)**.

## Verdict

**Recommendation: keep the shipped floor values (go 0.57, python 0.59,
rust 0.57, typescript 0.53) and re-fit them the way the product uses them:
on name queries, the only queries the floor turns into a hard `noMatch`.
Do not adopt the D6 refit (0.51 / 0.47 / 0.57 / 0.46). Do not make the F4
rerank score a refusal signal in this task.**

Why, in one paragraph: the 21.7% "false alarm" S19 reports is counted on
authored NL queries, and every authored query is prose. Since GM-434
(ADR 0010) a prose page below the floor gets the soft `lowSimilarity`
sentence ("the top row may still be right: check it with one read"), never
`noMatch`. So those 40 of 184 pages cost one confirming read each; nobody is
told "no". The D6 refit removes 32 of those reads and in exchange takes the
warning off 96 positive pages whose top row is wrong (confident-wrong 338 ->
434 of 681) and 48 absent-answer pages (37 -> 85 of 164). The hard verdict's
own population, name queries, already meets the 3% budget at the shipped
floors on v2 (misled 2%, 9/594, `gm-462-richer-reranker-input.md` line 46),
and a name-only fit of the shipped model and text gives back the shipped
values within 0.01-0.04 (`gm-468-name-query-floors.md`). The refit moved
because v2 added 1,650 authored prose queries to the fit pool, not because
the model's name scores moved.

The task then delivers: a shape-aware refit script on v2 (fit half / held-out
half for names too), a results file, updated numbers in `floor()`'s comment
and the module doc, and a dated GM-697 addendum to ADR 0012. Floor values
change only if the pre-fixed rule in D2 says so.

## What the numbers mean today

S19's columns come from `rerank_eval.gm434` (`eval/embedding/rerank_eval.py`
344-354), which is **shape-blind**:

- "misled" (S19's "FA"): a rank-1-right positive whose top score is below its
  language's floor.
- "CW positives": a positive page that clears the floor with a wrong top row.
- "CW absent": an absent-answer page that clears the floor.

The product is **not** shape-blind. `similarity::verdict`
(`core/src/mcp/similarity.rs` 375-394) returns `NoMatch` only when
`!is_prose_query(query)`; `similarity::low_similarity` (403-411) returns the
soft sentence when it is prose. In the eval, prose = authored and name =
mechanical on 2,279 of 2,279 queries (`gm-434-floor-nl-queries.md`, "How an
option is scored"). So, in GM-434's agent-outcome terms:

| S19 column | on authored NL (prose) it is | on mechanical (names) it is |
|---|---|---|
| "FA" / misled | **soft-ok**: SOFT note, top row right, one confirming read | **misled**: HARD `noMatch` on a right answer (the costly error) |
| CW positives | NONE page, wrong top row | NONE page, wrong top row |
| CW absent | NONE page, no answer exists | NONE page, no answer exists |

The module doc already says the floors were "*measured* on symbol-name
queries and only *checked* on free text ... set low enough that free text
does not suffer" (`similarity.rs` doc, "Per language" section, 186-207).

## Options

Costs on v2, int8-structured arm, held-out NL (184 rank-1-right positives,
681 positives, 164 absent), from `gm-460-enlarged-eval.md` unless marked.

| option | floors go/py/rs/ts | NL soft-ok (S19 "FA") | NL CW pos | NL CW absent | names (HARD) | benefit | risk |
|---|---|---|---|---|---|---|---|
| **A. Name-shape refit, keep values unless D2 says move (recommended)** | 0.57 / 0.59 / 0.57 / 0.53 expected | 21.7% (40/184) | 49.6% (338/681) | 22.6% (37/164) | misled 2% (9/594), CW pos 30% (263/879), CW absent 20% (184/900) (GM-462 table, pooled); per language **(measure)** | Fits the population the hard verdict acts on; also the population `find_definition`'s semantic rung uses (GM-468). No product change expected; no re-embed; no new constant. | 40 of 184 right NL pages keep costing one extra read. The pooled name numbers are from GM-462's tables; per-language name misled on a held-out half has not been measured yet (S2 does). |
| a1. D6 pooled refit as S19 fitted | 0.51 / 0.47 / 0.57 / 0.46 | 4.3% (8/184) | 63.7% (434/681) | 51.8% (85/164) | **(measure)**; lower floors mean fewer misled and more confident-wrong on names | 32 fewer confirming reads on right NL pages. | +96 wrong positive pages and +48 absent pages lose the warning. `find_definition`'s semantic rung reads the same table (`find_definition.rs` 998): at python 0.47 it offers "did you mean" candidates for far more names that exist nowhere (GM-468: hopeless offers already 11-26% at the shipped floors). |
| a2. Shape-split: a second, higher prose floor | names as A; prose fitted on NL FIT at a CW target | **(measure)** | **(measure)** | **(measure)** | as A | Could turn more wrong NL pages into SOFT pages (cheap) without touching names. | A second per-language constant, re-fitted on every model or text change. GM-434 rejected shape-split constants (options e/h) for small n; v2's 639 NL FIT positives remove that objection, but this is a new behaviour, not a re-fit: out of GM-697's scope. Follow-up candidate. |
| b. F4 rerank score as the refusal signal | none: a per-language threshold on the top cross-encoder logit (or `logit + BETA*cosine`) | **(measure)** | **(measure)** | **(measure)** | n/a | The cross-encoder orders better than the cosine (v2 F4: MRR +0.043 [+0.030], `gm-460` "v2 numbers"), so its top score may separate right from wrong/absent better. | Cannot stand alone: no scorer on every install without the rerank model (common, `GM-693` design note, path (a)), on partial pages during backfill, on continuation pages, and on `find_definition`'s semantic rung, which never reranks. So (b) is really (c). Depends on GM-698 (see below). |
| c. Cosine floor + rerank threshold combined | cosine table as A, plus a logit threshold on reranked first pages | **(measure)** | **(measure)** | **(measure)** | as A | Uses the better signal where it exists. | Two tables per language, the second re-fitted whenever the CE text or `BETA` changes. Breaks today's invariant that the verdict is judged on the embedding order's page (`search_code.rs` 406-418, `Ranked::judged`), so the same query can get a different verdict with and without the model. Unmeasured: no result file evaluates a CE score as an absent detector. |

### Does the chosen signal depend on GM-698?

- **A (recommended): no.** It uses the cosine of the embedding page, which
  GM-698 does not touch: GM-698 changes the cross-encoder's input text and
  `BETA` (`core/src/embedding/rerank.rs` 56), which only reorder the reranked
  page; the verdict is judged on `Ranked::judged`, the embedding-order page
  (`search_code.rs` 365-376, 481-516). GM-697 can run in parallel with GM-698.
- **b / c: yes.** The logit and the blend both change with GM-698's enriched
  text (enclosing kind, path words, 48-token body) and its beta (~20 instead
  of 80). A threshold fitted before GM-698 merges would be fitted on a signal
  the product no longer computes. If the owner picks b or c, the fit runs
  **after GM-698 merges**, on CE scores produced from the merged text
  (a diagnostic could use `gm462_ce_enriched.py`'s all-b48 arm offline
  earlier, but the shipped constant must not).

## Decisions

### D1. Which signal: the cosine floor, fitted on names (option A)

- **Today:** one per-language cosine floor table (`similarity::floor`,
  `similarity.rs` 286-298). Below it, a name query gets `noMatch`, a prose
  query gets `lowSimilarity`. The eval's D6 fit (`metrics::fit_floors`,
  `core/src/cli/embed_eval/metrics.rs` 132-146) pools mechanical name
  queries and authored fit-half prose queries and takes the 3% quantile of
  right-top scores.
- **Change:** GM-697 re-fits the table on name queries only (the hard
  verdict's population), and reports prose pages in GM-434's shape-aware
  terms (soft-ok, soft-miss, CW). No CE score enters the verdict.
- **Example:** python. D6 on v2 pools 175 python NL fit-half positives with
  the mechanical python names; the prose cosines sit lower, so the 3%
  quantile drops to 0.47. Name queries alone give 0.5895 (page definition)
  / 0.618 (rank-1 definition) on the same model and text (GM-468), i.e. the
  shipped 0.59. At 0.47 a python NL query whose top row is wrong at cosine
  0.52 gets no warning; at 0.59 it gets "check it with one read".
- **Consequence:** the product keeps its behaviour; the 21.7% is re-labelled
  in the results as a read cost, not a false refusal. NL confident-wrong
  stays at 49.6% / 22.6%. Lowering that is a different behaviour (a2, or the
  CE route) and gets its own task if wanted.

### D2. Fit method and the pre-fixed rule for changing a value

- **Today:** names (mechanical queries) are always in the fit half
  (`metrics.rs` 132-146: `in_fit = o.mechanical || !o.held_out`), so the
  name floor has never been checked out of sample.
- **Change:** the GM-697 script splits the mechanical queries itself with the
  same hash as D6 (`is_held_out_id`: sha256(id)[0] odd = held-out), fits on
  the name fit half, verifies on the name held-out half. It does **not**
  change the harness's split. Objective: per language, the largest floor at
  which at most 3% of fit-half rank-1-right name queries score below it,
  rounded down to two decimals (`metrics::fit_floor`'s rule, mirrored by
  `shipped_floor_rates.fit_floor`, `eval/embedding/shipped_floor_rates.py`
  163-169).
- **Rule (fixed before measuring):** for each language, with F = the fitted
  name floor:
  1. shipped <= F + 0.01 and held-out name misled at the shipped floor <= 3%
     (point estimate; Wilson 95% upper reported): **keep the shipped value**.
     A shipped floor *below* F is allowed: it refuses less than the budget
     allows, which GM-468 chose deliberately for typescript (0.53 vs a
     name fit of 0.56-0.57: fewer `find_definition` refusals, more hopeless
     offers).
  2. shipped > F + 0.01, or held-out misled at shipped > 3%: **propose F**
     (or the largest value with held-out misled <= 3%) to the owner with its
     full row of the S2 table. Not applied without approval.
- **Example:** GM-468's rank-1 name fits on the same vectors are 0.594 /
  0.618 / 0.573 / 0.576 -> 0.59 / 0.61 / 0.57 / 0.57. Against shipped
  0.57 / 0.59 / 0.57 / 0.53 every language is case 1 (shipped at or below
  F). The half-split fit will move these a little; that is what S2 measures.
- **Consequence:** most likely outcome is no value change: then S3 is docs
  only and the acceptance item "floor and ADR table updated if floors change"
  is satisfied by the addendum (D4). If a value changes, `DEFAULT_FLOOR`
  (`similarity.rs` 276) must stay equal to the lowest of the four, and the
  two pinning tests change (edit map).

### D3. The eval report's floor-parity gate

- **Today:** `report` (`core/src/cli/embed_eval.rs` 1518-1534) compares the
  reference arm's D6 pooled refit with `shipped_similarity_floor` and adds a
  validity error when go/python/rust differ by more than 0.03. On v2 this
  fires on go and python (S19: "the `report` verdict line reads `FAILED`
  only because of the floor-parity error"), and it will fire on every v2
  report for as long as authored prose dominates the pool, whatever the
  floors.
- **Change (recommended, owner choice):** add `metrics::fit_name_floors`
  (mechanical outcomes only, same rule) and have the parity gate use it.
  `fit_floors` itself stays: every arm's FA/CW columns and the D9 gates keep
  their D6 definition, so past reports stay comparable.
- **Example:** v2 report today: go fitted 0.51 vs shipped 0.57 -> `FAILED`.
  After: go name-fitted ~0.59 vs 0.57 -> passes. **(measure)**: the
  reference arm is fp32, whose name-only rank-1 fit on v2 is not in any
  results file (GM-468 gives only the page definition for fp32: 0.575 /
  0.598 / 0.559 / 0.556).
- **Consequence:** a real Rust change with tests; the `validity.floorParity`
  JSON keeps its keys, but its `fitted` values change on every eval dir, so a
  C4-style "equal to the stored GM-465 report" control will differ on that
  block (expected, named in the results). Without D3, every v2 report keeps a
  false `FAILED` and GM-698's measure must explain it again.

### D4. Documentation of the numbers

- **Today:** `floor()`'s comment (`similarity.rs` 287-289) quotes held-out
  false alarms 29.4 / 0.0 / 20.0 / 8.0% that no current results file
  reproduces; ADR 0012 (`docs/adr/0012-embedded-text-structured.md` 62-66)
  quotes the GM-398 set's 12.1% / 49.8%.
- **Change:** the comment drops the stale numbers and keeps only what the
  table is (the int8 model's floors on the structured text, fitted on name
  queries) with the existing ADR 0012 link: per the project's comment rule
  (CLAUDE.md), measured numbers are decision record, not code comment, so
  they go to the ADR addendum and `docs/results/gm-697-floor-refit.md`; the module doc's
  "Per language" section (186-207) gets one sentence that on v2 the floors
  were re-fitted on names and the prose numbers are soft-ok rates; ADR 0012
  gets a dated "Update (GM-697)" paragraph after its floor paragraph with
  the v2 numbers (values unchanged, or the new table if D2 case 2 is
  approved). The ADR's decision text is not rewritten.
- **Consequence:** the source no longer carries numbers nobody can
  reproduce; ADR index (`docs/adr/README.md`) unchanged (no new ADR, since
  no decision changes; a new ADR only if a value changes).

### D5. The F4 rerank score (options b / c)

- **Today:** the cross-encoder logit never reaches the verdict. It is
  computed in `CrossEncoder::score` (`rerank.rs` 193-250), combined in
  `blended_order` (124-140) and used by `first_page_reranked`
  (`search_code.rs` 481-516) only to reorder; `handle` (316-390) judges the
  verdict on `judged`, the embedding-order page.
- **Change:** none in GM-697. If the owner wants the CE route explored, a
  follow-up task (after GM-698 merges) measures separability first: on NL FIT,
  AUROC of top-1 cosine vs top-1 logit vs top-1 blend for "top row right" vs
  "top row wrong or no answer", per language, before designing any
  threshold.
- **Consequence:** GM-697 stays independent of GM-698 and ships no new
  constant. The cost is that NL confident-wrong (49.6% / 22.6%) is not
  attacked in this task.

## Measure plan (S2)

Script: **new** `eval/embedding/gm697_floor_refit.py` (stdlib only, reads
stored rankings, no re-embedding). Inputs:
`--run <v2 work>/runs/jina-v2-base-code-int8-structured`, `--eval-dir
eval/embedding/v2`, `--json`, `--table`. Reuse by import:
`q5_floor_sensitivity.load_arm` / `load_queries` (147-174, 133-146) for
outcomes, `shipped_floor_rates.structured_floors` (48-53) for the shipped
table (parsed from source, not remembered), `shipped_floor_rates.fit_floor`
(163-169), `gm468_name_floors.collect` / `rates` / `wilson_hi` (60-102) for
the `find_definition` rung columns.

Setup: `eval/embedding/v2/setup_work.sh <main checkout>/eval/embedding/work`
(the stored `runs-gm465` exists there), `cargo build --release --bin g-mesh`
in the task worktree, `g-mesh debug-embed-eval run --eval-dir eval/embedding/v2`
for the int8-structured arm only (S19: 54.9 s real); the fp32 arm too if D3
is approved (93.1 s). No cross-encoder step. Record `uptime` and
`/usr/bin/time -p`.

Splits: authored queries by the harness's own `held_out` flag; mechanical
queries by `is_held_out_id` computed in the script (mirror of
`core/src/cli/embed_eval/queries.rs::is_held_out_id`, as
`eval/embedding/v2/build.py` 84-86 mirrors it).

Controls (each self-tested on a planted mismatch, as S19 did):

- **C1** pooled D6 fit reproduces S19: 0.51 / 0.47 / 0.57 / 0.46.
- **C2** shape-blind columns at shipped reproduce S19: 40/184, 338/681,
  37/164; and at the D6 refit 8/184, 434/681, 85/164.
- **C3** name fit over all mechanical queries, rank-1 definition,
  reproduces GM-468's strict 0.594 / 0.618 / 0.573 / 0.576 (exact raw
  quantiles; the v2 int8-structured vectors are seeded from `runs-gm465`,
  checked by S19's C0).

Table S2 must produce (`docs/results/gm-697-floor-refit.md`), one row per
language plus pooled, for each floor set {shipped, D6 v2 refit,
name-fit (FIT half)}:

| floors | name fit-half n / F (raw, 2dp) | name held-out misled k/n (Wilson up) | name held-out CW pos | name held-out CW absent | NL held-out soft-ok | NL held-out soft-miss | NL held-out CW pos | NL held-out CW absent | find_definition false refusal / hopeless offer (page def) | D2 case |
|---|---|---|---|---|---|---|---|---|---|---|

Plus, if D3 is approved, the fp32 arm's name-fit floors (the parity gate's
reference). Exit verdict: the D2 case per language, quoted.

## Edit map

Changes (S3), 1-based lines at `release-4.26.0`:

| file | lines | what | when |
|---|---|---|---|
| `core/src/mcp/similarity.rs` `floor` | 286-298 (comment 287-289) | drop the comment's stale 29.4 / 0.0 / 20.0 / 8.0% (numbers go to ADR 0012's addendum, comment rule in CLAUDE.md), keep the ADR 0012 link; values only if D2 case 2 is approved. Keep the `"lang" => 0.xx,` shape: `shipped_floor_rates.structured_floors` and `rerank_eval.shipped_floors` parse it with a regex | always (comment) |
| `core/src/mcp/similarity.rs` `DEFAULT_FLOOR` | 276 | must equal the lowest floor | only if values change |
| `core/src/mcp/similarity.rs` module doc "Per language" | 186-207 | one sentence on the v2 name-shape refit | always |
| `core/src/mcp/similarity.rs` tests `the_shipped_floors_are_the_structured_texts`, `an_unmeasured_language_uses_the_lowest_measured_floor` | 540-544, 549-556 | new values | only if values change |
| `core/src/mcp/similarity.rs` tests with literal scores 0.49/0.54 (`a_page_whose_best_row_clears_its_floor_says_nothing` 443-449, `..._below_its_floor_is_a_no` 452-464, `a_prose_query_below...` 470-488, `one_score_is_a_match_in_one_language...` 524-532, `a_mixed_language_page...` 562-565) | as listed | re-check the literals still straddle the new floors | only if values change |
| `core/src/mcp/search_code_rerank_tests.rs` | 401 | literal 0.56 / 0.555 rows near the python/typescript floors: re-check | only if values change |
| `core/src/cli/embed_eval/metrics.rs` | after `fit_floors` 132-146 | new `fit_name_floors` (mechanical only, same `fit_floor` rule) | D3 |
| `core/src/cli/embed_eval.rs` `report` | 1518-1534 | parity gate uses `fit_name_floors` of the reference arm; keep the typescript exemption and the 0.03 tolerance | D3 |
| `docs/adr/0012-embedded-text-structured.md` | after 62-66 | dated "Update (GM-697)" paragraph | always |
| `docs/results/gm-697-floor-refit.md` | new | S2's table, controls, machine state | S2 |
| `eval/embedding/gm697_floor_refit.py` | new | S2's script | S2 |

Read for context (no change): `similarity::verdict` 375-394,
`low_similarity` 403-411, `below_floor` 347-353 (each row against its own
language's floor); `search_code::handle` 316-390 (verdict on `judged`),
`Ranked` 406-418, `first_page_reranked` 481-516;
`find_definition::by_semantic_neighbours` 965-1014 (filter at 998);
`rerank::blended_order` 124-140, `BETA` 56; `metrics::fit_floor` 113-122;
`rerank_eval.gm434` 344-354; ADR 0010 and ADR 0012.

## Proposed slices

| slice | model | scope | exit criterion |
|---|---|---|---|
| S2 measure | sonnet | write `gm697_floor_refit.py`, run the int8-structured arm on v2 (plus fp32 if D3), write `docs/results/gm-697-floor-refit.md` | C1-C3 pass and the table above is filled with the D2 case per language |
| S3 code + docs | sonnet | edit map rows marked "always", plus D3 and/or new values if approved | `cargo build`, `cargo clippy -p g-mesh` clean; diff touches only edit-map rows |
| S4 tests | sonnet | **only if D3 is approved or a value changes**: tests for `fit_name_floors` and the parity gate (mixed pool where pooled and name fits differ by > 0.03), updated pinning tests; 1-2 controls (revert the gate to `fit_floors`; restore old values) | new tests pass, each control fails on revert |
| S5 verify | opus (fresh) | re-run S2's script from a clean checkout and compare the JSON; `scripts/test-local.sh --base release-4.26.0` (the sections the diff selects; a docs/comment-only diff may select none); controls of S4 | script output equal to S2's; tests and controls as stated |

If neither D3 nor a value change is approved, S4 is dropped and S3 is a
docs-only diff (comment + module doc + ADR addendum); S5 then re-runs only
the script.

## Decision needed from the owner

1. **Trade-off (D1):** A (keep shipped, re-fit on names, recommended) / a1
   (adopt the D6 refit 0.51 / 0.47 / 0.57 / 0.46) / b-c (CE score, after
   GM-698).
2. **D3:** move the report's parity gate to a name-only fit (recommended) /
   leave it (v2 reports keep reading `FAILED`).
3. **D5:** file a follow-up "CE score separability as an absent detector"
   after GM-698 / do not.

## Appendix: g-mesh calls this note relied on

Project `g-mesh` (main checkout, on `release-4.26.0`): the task worktree was
not yet indexed (a cold index is 5-8 min) and has no code changes; `diff -q`
confirmed `similarity.rs`, `search_code.rs` and `rerank.rs` are identical
in both checkouts.

| question | call | answer |
|---|---|---|
| where is `floor` | `find_definition(symbol_name="floor")` | `core/src/mcp/similarity.rs` 286-298 (the brief's `core/src/embedding/similarity.rs` does not exist) |
| who calls `floor` | `find_callers(symbol_name="floor")` | `similarity::below_floor`, `find_definition::by_semantic_neighbours`, `cli::embed_eval::report`, test `the_shipped_floors_are_the_structured_texts`; complete (`hasMore: false`); one non-call usage in `core/src/mcp/mod.rs` |
| that usage | `find_references(floor, file_paths=[core/src/mcp/mod.rs])` | file-level: the re-export `shipped_similarity_floor` (mod.rs 82), used by `report` |
| who calls `below_floor` | `find_callers(symbol_name="below_floor")` | `verdict`, `low_similarity` |
| where the verdict is made | `find_callers(symbol_id=verdict d63b1b23...)`, `find_callers(symbol_id=low_similarity a9509184...)` | `search_code::handle` (plus `partial_verdict` and tests); both names were ambiguous by `symbol_name` and re-queried by id |
| outline of `similarity.rs` | `get_file_outline(core/src/mcp/similarity.rs)` | line ranges in the edit map |
| where the F4 score is computed | `search_code(query="cross-encoder rerank ...")`, then `get_file_outline(core/src/embedding/rerank.rs)` | `CrossEncoder::score`, `blended_order`, `BETA`; `handle` passes `judged` (embedding order) to `verdict`, so the logit does not reach the refusal decision (read in `search_code.rs` 350-376 by `sed`) |
| `shipped_similarity_floor` users | `find_references(symbol_name="shipped_similarity_floor")` | no symbol of that name (it is a `use ... as` alias): fell back to grep, `core/src/cli/embed_eval.rs` 1521 only |
