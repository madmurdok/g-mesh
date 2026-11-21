# GM-452: fine-tuning the embedding model, reconciled with GM-456 / GM-730 (design)

Slice GM-452/S1, 2026-11-21, branch `research/GM-452-fine-tune-reconcile` off
release-4.35.0. No GPU, no AWS, no code change. The only runs are read-only
scripts over stored rankings (2-4 s each, listed in section 9).

## 0. Summary

- GM-452 was filed to answer: does fine-tuning the embedder on (NL query,
  code) pairs cut the confident-wrong answers GM-434 measured? GM-456 built
  the pairs, GM-730 trained the model (route J: jina-v2-base-code, full
  fine-tune, int8). J's ranking gain on eval v2 is large and clean: NL MRR
  0.380 -> 0.501, r@10 0.608 -> 0.733, every control passes.
- What GM-730 did **not** report is the GM-434 view: misled % and
  confident-wrong per language, NL vs name. This slice computed it from the
  stored v2 runs (section 3). Result: at equal false-alarm rate, J cuts NL
  confident-wrong on positives from 63.7% to 48.2% (every language, -11 to
  -20 pts), but **does not cut confident-wrong on absent queries** (51.8% ->
  52.4%). Its scores sit lower, so **at today's floors name misled rises
  1.5% -> 5.9%**: J cannot ship without new floors.
- Still open: (a) these are embedder-only numbers, while production reranks
  the top 30 with MiniLM-L6 (GM-443 F4) — J + reranker was never measured;
  (b) J's floors are not chosen; (c) no paired per-language intervals.
  All three are laptop work, about 1-2 h of CPU, no GPU.
- Recommendation: **run the remaining measurement** (section 7, option B),
  4 short slices. It is also the input the J shipping task needs anyway.

## 1. Context for a reader without it

| term | meaning |
|---|---|
| confident wrong (positives) | a query that has an answer; the top hit is wrong, and its score clears the language's floor, so the agent is not warned |
| confident wrong (absent) | a query with no answer in the repo; the top hit clears the floor anyway |
| false alarm / misled | the top hit is right but its score is below the floor. On a **name** query the product says `noMatch` (the agent is misled). On an **NL** (prose) query the product says `lowSimilarity` only (GM-434 S7), so NL misled is 0 by construction and the cost is a soft warning on a right answer |
| floors | per-language cosine thresholds in `core/src/mcp/similarity.rs::floor` (today go 0.57, python 0.59, rust 0.57, typescript 0.53; fitted on name queries, ADR 0012). They belong to one model's score scale |
| eval v2 | GM-460: 6 corpora (gin, py-requests, ripgrep, g-mesh, excalidraw, task-tracker-mcp), 1,320 NL positives + 332 NL absent (330 + 83 per language) and 1,779 name queries |
| J | `gm730-jina-full-int8-structured`, the GM-730 route J winner (`full-lr1e-5-a0`: full fine-tune, lr 1e-5, no teacher) re-quantised to int8 |
| today's stack | int8 jina top-30, reranked by MiniLM-L6 (blend logit + beta x cosine); the noMatch verdict reads int8's own top-1 cosine (GM-443 verdict, `docs/results/gm-443-recall-rerank.md:990`) |

## 2. Acceptance criteria and open questions: status

| item | status | answered by (numbers, file) |
|---|---|---|
| AC1: go/no-go with GM-434-method numbers, per language, NL and name, base vs fine-tuned, disjoint and large eval | **Partial** | Embedder-only numbers now exist (section 3, computed here). Missing: J behind the reranker, J's floors, paired per-language CIs. GM-730's go (D8 "ship") rests on MRR / r@10 and on the pooled Q4 gate (`report_jina.json`: confident-wrong delta -13.4 pts, upper bound -10.4) |
| AC2: data source, pair count, training cost | **Answered** | Source: GM-456 v0.4-full, 24 pinned permissive-licence repos, none an eval corpus; queries written by Opus 5.5, judged by Sonnet 5.5 (`docs/design/GM-456-training-data.md`, `g-mesh-data/gm456/v0.4-full/card.md`). Count: 23,798 queries over 3,990 targets (go 4,763, python 4,774, rust 9,488, typescript 4,773), split by repo 20,600 train / 3,198 dev, 7 hard negatives sampled per row. Cost: generation $162.70 list-price equivalent on the subscription plus $6.81 for the v0.4 rebuild (`S43-full.md`, `S71-rebuild.md`; not money spent); J GPU $7.13 (grid 393 min, final 81 min, g5.xlarge), GM-730 total $12.27 (`docs/results/gm-730-gpu-training-routes.md`) |
| AC3: comparison with GM-443's reranker | **Partial** | Held-out NL, same 681 queries: today's stack (int8 + L6, beta 40) r@10 0.670, MRR 0.465 (`gm-463-reranker-bakeoff.md`); J alone, no reranker, r@10 0.735, MRR 0.521 (computed here, section 5). Unpaired; J + reranker not measured |
| Q1: where pairs come from | **Answered** | GM-456 synthetic pipeline (above) |
| Q2: how many are needed | **Partial** | 20,600 train queries were enough for a dev MRR gain of +0.114 (0.431 -> 0.545). No learning curve (smaller subsets) was run, so "needed" is unknown; "sufficient" is shown |
| Q3: honest eval, disjoint from training | **Answered** | Section 4 |
| Q4: cost of shipping custom weights | **Partial** | Size: J int8 161,797,676 B vs shipped 161,895,621 B, same graph (`out/final/export.json`, `core/src/embedding/model.rs:96`). Licence: base Apache-2.0 (`docs/architecture/embedding-eval.md:271`), training repos MIT / Apache-2.0 / BSD. Download, re-embed and floors: section 6. Not measured: D11 runtime costs (`report_jina.txt` verdict `Undecided("... costs (D11) not measured")`) |
| Q5: one model across languages | **Answered for the 4 languages** | One model, gains in every language: NL r@10 go +15.7, python +14.5, rust +12.1, typescript +7.6 pts (`report_jina.json` `recall@10ByLanguage`); confident-wrong positives down in every language (section 3). Rust stays the weakest. No unseen language was tested (the product indexes only these four) |

## 3. GM-434-method numbers for J (computed in this slice)

Method: `eval/embedding/shipped_floor_rates.py` and
`eval/embedding/gm697_floor_refit.py` over the stored v2 rankings of the base
(`jina-v2-base-code-int8-structured`) and J. Embedder order only (no
reranker). Control: at each arm's D6 refit floors the script reproduces
`report_jina.json` exactly (base 434/681 and 85/164, J 328/681 and 86/164).

**Why three floor sets.** Floors trade false alarms against confident wrong.
J's cosines are lower than the base's, so one fixed floor set favours one
model. Each set answers a different question:

- *shipped*: what happens if J is dropped in and the floors are not touched;
- *D6 refit* (each model's own, fitted on v2 FIT to ~5% false alarm): the
  model-to-model comparison at equal false alarm — **the fair one**;
- *name-fit* (GM-697 method, the way production floors are fitted, ADR 0012):
  what a J ship would likely use. Not a decision yet (D2).

Floors: base D6 go 0.51 / python 0.47 / rust 0.57 / typescript 0.46; J D6
0.49 / 0.52 / 0.53 / 0.48; J name-fit 0.47 / 0.52 / 0.54 / 0.55.

### 3.1 NL held-out (n positives / absent: go 184/41, python 155/40, rust 161/45, typescript 181/38)

Cells: confident wrong positives / confident wrong absent / false alarm (soft
warning on a right top).

| lang | base @ shipped (today) | J @ shipped | base @ D6 | J @ D6 | J @ name-fit |
|---|---|---|---|---|---|
| go | 48.9 / 9.8 / 34.7 | 33.7 / 17.1 / 9.5 | 64.7 / 31.7 / 4.1 | 47.8 / 61.0 / 2.4 | 50.0 / 73.2 / 1.2 |
| python | 38.7 / 22.5 / 23.9 | 25.8 / 15.0 / 27.8 | 67.1 / 72.5 / 0.0 | 47.1 / 37.5 / 7.4 | 47.1 / 37.5 / 7.4 |
| rust | 69.6 / 37.8 / 8.7 | 47.2 / 35.6 / 11.5 | 69.6 / 40.0 / 8.7 | 58.4 / 55.6 / 5.8 | 55.3 / 51.1 / 5.8 |
| typescript | 42.0 / 18.4 / 15.2 | 34.3 / 39.5 / 16.5 | 54.7 / 65.8 / 6.1 | 40.3 / 55.3 / 6.6 | 30.4 / 28.9 / 25.3 |
| **all** | **49.6 / 22.6 / 21.7** | **35.2 / 26.8 / 15.7** | **63.7 / 51.8 / 4.3** | **48.2 / 52.4 / 5.3** | **45.4 / 48.2 / 11.0** |

Right tops (false-alarm denominator), NL held-out: base 184 of 681, J 281 of
681. That is the source of the gain: J puts the right answer first more often,
so fewer tops can be confidently wrong.

### 3.2 Name queries (all mechanical; n positives / absent: go 150/150, python 150/150, rust 300/300, typescript 279/300)

Cells: misled (noMatch on a right top) / confident wrong positives / confident wrong absent.

| lang | base @ shipped (today) | J @ shipped | base @ D6 | J @ D6 |
|---|---|---|---|---|
| go | 1.1 / 38.7 / 12.0 | 7.3 / 36.0 / 6.0 | 0.0 / 40.0 / 22.0 | 2.4 / 42.0 / 18.7 |
| python | 0.9 / 23.3 / 11.3 | 6.4 / 18.7 / 7.3 | 0.0 / 28.0 / 46.7 | 0.9 / 22.0 / 24.0 |
| rust | 2.5 / 45.0 / 26.0 | 9.9 / 33.7 / 13.7 | 2.5 / 45.0 / 27.0 | 1.4 / 43.7 / 26.7 |
| typescript | 1.2 / 12.5 / 23.7 | 2.9 / 9.7 / 13.7 | 0.0 / 14.0 / 42.7 | 0.8 / 11.1 / 26.7 |
| **all** | **1.5 / 29.9 / 20.4** | **5.9 / 23.9 / 11.3** | **0.7 / 31.4 / 34.7** | **1.2 / 29.4 / 24.9** |

GM-697's held-out-half view (the one GM-461's "misled <= 3% per language" rule
uses): base @ shipped 4/273, J @ shipped 13/281, J @ name-fit 8/281 (2.8%,
Wilson upper 5.5%; rust 3/68, typescript 5/110).

### 3.3 What the numbers say

- **Positives: a real, floor-independent gain.** At equal false alarm (D6),
  NL confident-wrong on positives falls 15.5 pts pooled and 11-20 pts in every
  language. Example: of 161 held-out Rust prose queries, 112 got a wrong top
  shown without a warning by the base, 94 by J.
- **Absent: no gain.** At equal false alarm, NL absent stays ~52%. Fine-tuning
  taught J to rank the right answer higher; it did not teach it to score
  "nothing here" lower. GM-434's absent problem is not solved by J.
- **Names: no gain at fair floors** (29.4% vs 31.4%), and a risk: at today's
  floors J's lower scores raise name misled to 5.9% (rust 9.9%). Consequence:
  shipping J means re-fitting floors in the same change; shipping it with
  today's floors would break the 3% misled rule.
- **GM-434's headline in today's terms.** GM-434 measured 51% NL / 33% name
  confident-wrong on fp32 and eval v1. On v2 today's model is at 49.6% / 29.9%
  (embedder only). J's improvement shows up only once its floors are set; at
  the GM-697-style floors J is at 45.4% NL, i.e. the gain is spent partly on
  fewer false alarms. Which point on the curve to take is D2.

## 4. Is eval v2 honest and large enough?

**Disjoint: yes.** Evidence:

- Repos: 24 training repos, none of the 6 eval corpora; checked by remote URL
  and root commit (forks caught) — leak guard check 1.
- Code: exact span hash and 5-gram shingle Jaccard / containment >= 0.7
  against every eval symbol of all three studies (GM-398, GM-422 confirm,
  GM-460 new) — checks 2-3.
- Queries: word Jaccard >= 0.4 and jina-int8 cosine >= 0.90 against every
  eval query — checks 4-5.
- Result for v0.4-full: 148 of 23,946 rows dropped before export (141 query
  Jaccard, 5 exact code, 1 query embedding), then the guard is clean on the
  export (`card.md`: all five counters 0).
- Model selection: GM-730 picked the J config on GM-456 dev only and froze its
  sha256 before any v2 run (GM-436 D9 rule; `g-mesh-data/gm730/measure/summary.txt`,
  freeze 2026-11-21 03:56, eval 05:24).

Residual risks (not leaks): training queries and eval queries are both written
by LLM agents, so style is closer than real agent traffic; and v2 has been read
by many arms (GM-436, GM-461, GM-463, GM-703, GM-730), so it is not a fresh
holdout. GM-730 gated only its one pre-registered winner, which limits the
second risk.

**Size: enough for positives per language, not for absent per language.**

| set | n per language | 95% half-width near 45% |
|---|---|---|
| NL held-out positives | 155-184 | about +-7 pts (unpaired) |
| NL held-out absent | 38-45 | about +-15 pts |
| NL all (includes the FIT half the D6 floors were fitted on) | 330 / 83 | +-5 / +-11 |
| name positives / absent | 150-300 | +-6-8 |

The per-language positive deltas at D6 (11-20 pts) are larger than these
widths, and a paired interval is tighter still. The per-language absent deltas
swing from -35 to +29 pts as the floors move: n ~40 cannot give a
per-language absent verdict. Consequence: the go/no-go should read absent
pooled (n 164 held-out, +-8) and positives and name misled per language.

## 5. Comparison with GM-443 (reranker) and the other routes

| arm, held-out NL (681) | r@10 | MRR | source |
|---|---|---|---|
| base int8, no rerank | 0.612 | 0.384 | computed here; equals `gm-463-reranker-bakeoff.md` (control) |
| GM-436 adapter, no rerank | 0.672 | 0.434 | computed here |
| today's stack: base + L6 (beta 40) | 0.670 | 0.465 | GM-463 |
| base + gte-mb reranker (too slow, rule 5) | 0.691 | 0.512 | GM-463 |
| **J, no rerank** | **0.735** | **0.521** | computed here |
| J + L6 | not measured | | |

J alone beats today's full stack and the slow gte-mb reranker on both
metrics. These rows are unpaired (same queries, separate runs). Two things
the reranker changes are unknown for J: whether L6 still adds on top of J
(it was tuned on base cosines, beta 40), and the confident-wrong rate on the
page the agent actually sees (the reranked top). GM-461 measured today's stack
on that page: NL held-out confident-wrong positives 43.5%, absent 22.6%
(`gm-461-learned-no-match-verdict.md`). J needs the same row.

Other routes, for the record: GM-444 fusion with BM25: no-go. GM-703
CodeRankEmbed: no-go (Q5). GM-730 RR (distilled reranker): closed on the
quint8 gate, continues as GM-734 (QAT, in progress). GM-730 R3 (gte student):
stopped. GM-461 learned verdict: wins for NL (confident-wrong 43.5% -> 26.4%,
absent 22.6% -> 10.4%), not for names; it is fitted on base cosines and
reranker logits, so it would need a refit on J.

## 6. Shipping J: what changes

| area | today | with J | consequence |
|---|---|---|---|
| download | `g-mesh model fetch` pulls `huggingface.co/{repo}/resolve/{revision}/{path}`, checked against pinned size and sha256 (`core/src/cli/model.rs:112`, `core/src/embedding/model.rs:92-102`) | weights must be hosted somewhere the fetcher can reach (an HF repo the owner controls, or a release asset); new path, size, sha256 | a hosting decision and an Apache-2.0 NOTICE for the derivative |
| re-embed | `DEFAULT_WEIGHTS_TAG = "int8"` tags every vector (`model.rs:110`) | new tag | every existing index re-embeds once on upgrade (backfill) |
| floors | `similarity.rs::floor`, name-fitted for the base | refit (section 3: today's floors give 5.9% name misled) | a GM-697-style refit in the same release |
| reranker blend | beta tuned on base cosines | re-tune beta on J (or show beta 40 still holds) | part of the J + L6 measurement |
| learned verdict (GM-461), if shipped | fitted on base cosines | refit | follow-up |
| runtime cost | int8 base | same graph, size within 0.1%; D11 not measured | a short cost run to close the `Undecided` |
| licence of training data | - | repos MIT / Apache-2.0 / BSD; queries generated by Claude | owner check that the provider terms allow training an embedder on generated queries |

## 7. Options

**A. Close GM-452 as covered by GM-730.**
- Today: J trained, gated and marked "ship" by D8; this note adds the
  embedder-only GM-434 numbers.
- Change: GM-452 closes now; the J shipping task inherits sections 3-6.
- Benefit: no more work under GM-452; the training questions (source, count,
  cost, disjointness, languages) are answered.
- Risk: AC1 is closed without the numbers it asked for on the real page
  (reranked), and the absent "no gain" and the floor dependency could be lost
  in a ship task that reads only MRR. Example: J ships with today's floors and
  name misled goes to ~6%.

**B. Run the remaining measurement (Recommended).**
- Change: four short slices (section 8): J + L6 windows, J floors, paired
  per-language CIs, a go/no-go.
- Benefit: closes AC1 and AC3 as written; its outputs (floors, beta, stack
  numbers) are exactly what the J ship task needs, so nothing is wasted.
  About 1-2 h laptop CPU, no GPU, no money.
- Risk: some code edits to eval scripts (hard-coded base arm, see 8); the
  result may say "J + L6 gains less than J alone", which delays shipping.

**C. Close GM-452 and move the measurement into the J ship task as its gate.**
- Benefit: one task owns floors, beta, hosting and the measurement.
- Risk: the ship task grows to mix research and production work; GM-452's
  go/no-go is never written down as such.

## 8. Draft slices for option B

Gaps found in the scripts (read in this slice): `gm697_floor_refit.py`'s
controls C1-C3 are pinned to the base arm's numbers (on J they report
"CONTROL FAILED" though the table is computed); `shipped_floor_rates.py
--after` ignores `--floors/--custom` and always uses the GM-434 fp32 floors
(`evaluate_after`, `SHIPPED`); the GM-701/GM-461 stack setup reads the
int8-structured top-30 only.

| slice | kind | model | work | exit criterion |
|---|---|---|---|---|
| S2 | implement | sonnet | Parameterise the arm: `gm697_floor_refit.py` controls read expected values from the arm's `report_*.json`; `--after` honours the chosen floors; `--st-runs` in the GM-701 stack setup; a paired per-language bootstrap (seed 398, 10,000) for base vs J deltas of misled, confident-wrong positives and absent | targeted tests pass; on the base arm every existing output is byte-identical (control); a planted arm fails each control |
| S3 | verify | opus (fresh) | Review the diff; reverts of each change make its test fail (one throwaway worktree) | 0 findings or findings fixed |
| S4 | measure | sonnet | One background script, laptop: score J's top-30 windows with ship L6 quint8 (~100k pairs, ~30 min CPU, cache reused for shared pairs); tune beta on FIT; GM-697 floor refit for J; stack tables (J + L6, verdict on J cosine at J floors) next to today's stack; paired CIs; `uptime` and `time -p` recorded | tables written; control rows reproduce GM-461's F4 numbers (43.5% / 22.6%) and this note's section 3 |
| S5 | design (results + verdict) | opus | `docs/results/gm-452-fine-tune.md`: go/no-go per the rule in D3, floors and beta proposal for the ship task | owner decision recorded |

## 9. Reproduction (this slice)

From the worktree root, `R=../g-mesh-eval-data/v2/work/runs`:

```
python3 eval/embedding/shipped_floor_rates.py --run $R/<arm> --eval-dir eval/embedding/v2 --floors shipped-structured
python3 eval/embedding/shipped_floor_rates.py --run $R/<arm> --eval-dir eval/embedding/v2 --custom go=..,python=..,rust=..,typescript=..
python3 eval/embedding/gm697_floor_refit.py --run $R/<arm> --eval-dir eval/embedding/v2
```

`<arm>` = `jina-v2-base-code-int8-structured` or `gm730-jina-full-int8-structured`.
2-4 s each (`real 1.8-3.5`, `user 0.7-1.2`; load average 43 at the time).
Held-out r@10 / MRR in section 5: a 30-line script over the same rankings
(first expected rank; mean of per-language means), which reproduces the base's
0.612 / 0.384 from GM-463 and the full-set 0.608 / 0.380 and 0.733 / 0.501
from `report_jina.txt`.

Code facts: the model download and pin sites were found with g-mesh
`search_code` ("download the embedding model ONNX file and verify its
sha256", project g-mesh: the branch changes no code) -> `core/src/embedding/model.rs`,
`core/src/cli/model.rs`; then read by line. Script behaviour was read by
grep and line ranges.

## 10. Decisions for the owner

- **D1. GM-452's fate:** A close / B run (Recommended) / C move into the J
  ship task.
- **D2. Floor policy J is judged at:** D6 refit (equal false alarm,
  Recommended for the model verdict) or GM-697 name-fit (what would ship);
  both are reported either way.
- **D3. Go rule for B (pre-registered before S4):** proposed: J + L6 vs today's
  stack, NL confident-wrong positives paired upper bound < 0 pooled and point
  <= 0 in every language; absent pooled upper bound <= +5 pts; name misled
  <= 3% per language at J's floors.
