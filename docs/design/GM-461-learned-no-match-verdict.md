# GM-461: learned no-match verdict (design)

Slice GM-461/S1, design only. Branch `research/GM-461-learned-no-match-verdict`
(off `release-4.35.0`). No code, no runs in this slice; every number below is
copied from the results file named next to it, or counted from the query files
on this branch (marked **(counted)**), or left to the measure slice
(**(measure)**).

## Question

GM-434 and GM-443 S8/S11 found that a stricter floor on one score (int8
cosine, or the ce-minilm + int8 blend) only slides along one curve: fewer
confident-wrong (CW) tops, more misled (right tops refused). GM-699 measured
the separability of each single score of the reranked top-1 on v2 (NL FIT
AUROC: cosine 0.637, CE logit 0.696, blend 0.705), and GM-701 found that
"floor OR blend threshold" catches more only by refusing more right tops
(closed as null, owner 2026-11-11). GM-461 asks the remaining question: does a
small classifier over several signals at once (cosine, CE logit, both top1-top2
gaps, doc comment, language, query type) move *off* that curve, i.e. catch more
wrong/absent tops **at the same share of right tops kept** as the shipped
verdict?

## What changed since the task was written (read first)

The task names GM-443's F4 (v1 eval, K=50, ce-minilm fp32 scored in Python,
plain node text, floors 0.57 / 0.57 / 0.55 / 0.53) as the baseline. None of
that is the shipped stack any more:

| | GM-443 S11 F4 (task text) | shipped today (`release-4.35.0`) |
|---|---|---|
| eval | v1: 500 NL + 1,779 name queries | v2 (GM-460): 1,652 NL + 1,779 name |
| node text | plain (`text_to_embed`) | structured (ADR 0012), CE on "ship" text E+P+B48 (GM-698) |
| recall run | `jina-v2-base-code-int8` | `jina-v2-base-code-int8-structured` |
| CE | ce-minilm fp32, Python ORT | ce-minilm quint8, Rust `CrossEncoder::score` (GM-700) |
| K / beta | 50 / 40 | 30 / 40, exact-name skip rule (`rerank.rs confident`) |
| floors | 0.57 / 0.57 / 0.55 / 0.53 | 0.57 / 0.59 / 0.57 / 0.53 (`similarity.rs:290`, kept by GM-697) |

GM-699 and GM-701 already rebuilt and measured exactly the shipped stack on
v2, and GM-701's arm **A** ("floor alone": blend order shown, verdict = cosine
of the cosine-order top-1 against the shipped floor of that row's language) is
F4 on today's stack. **Proposal (D1): the baseline is GM-701 arm A on v2,
named F4 below; GM-443 S11's v1 numbers are not reproduced.** The v1 queries
are a subset of v2 (500 of 500 NL ids, 1,779 of 1,779 name ids, same
FIT/held-out sides **(counted)**), so a v1-subset row on today's stack is
reported as a sensitivity row (see Metrics, T8).

## Data inventory

### On disk (checked to exist in this slice)

| what | path | covers |
|---|---|---|
| v2 work dir (read-only) | `~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work` (also symlinked as `g-mesh-wt-gm730/eval/embedding/v2/work`); snapshots, corpora, models link into `.../v2/stored` | 6 corpora |
| int8-structured rankings | `v2/work/runs/jina-v2-base-code-int8-structured/<corpus>/rankings.jsonl` | all 3,431 v2 queries, top-100 (we read top 30) |
| fp32 reference run (corpus list, manifest checks) | `v2/work/runs/jina-v2-base-code-fp32` | all v2 queries |
| CE logits | `v2/work/rerank_cache_gm700_rust` (key `82d82801c3afcafb`, variant `ce-minilm-quint8-rust`, 8.1 MB) | all 102,917 K=30 pairs of all 3,431 v2 queries (GM-699 Inputs; GM-701-S1-rebuild.md) |
| shipped CE model | `v2/work/models/ms-marco-MiniLM-L6-v2-quint8/model.onnx`, sha256 `c80a8b34...bf9` checked by `gm701.setup` | sha check only, no ORT run |
| node metadata (doc comment, language, qualified name) | `v2/work/<corpus>.sqlite` -> `stored` | all nodes |
| v1 GM-398 runs + GM-443 CE cache | `~/Projects/ClaudeProjects/g-mesh/eval/embedding/work/{runs,rerank_cache/ce-minilm.*}` (an APFS clone sits in `g-mesh-eval-data/v2/stored`, same file list and sizes) | v1 queries only, plain text, K=50, Python fp32 CE |

**Coverage answer.** The stored v2 runs and the Rust CE cache cover v2 fully
(every NL and name query, every K=30 pair). GM-443's CE cache covers only v1
(500 NL) and only the old plain-text, K=50, Python-fp32 stack, so it cannot
score the shipped page and cannot be used on v2. Consequence: everything is
measured on v2 with the v2 cache; nothing is embedded or scored (a missing
pair raises `KeyError` in `score_of_for`, which stops the run, as in GM-699).
v2 gives about 3x the held-out NL rows of v1 (845 vs 261), which is what makes a
9-feature model readable at all.

### Rows (one row per query; FIT/held-out = `sha256(id)[0]` odd, `rerank_eval.held_out` for NL, `gm701.name_held_out` for names) **(counted)**

| population | language | FIT pos / absent | held-out pos / absent |
|---|---|---|---|
| NL (authored, prose) | go | 146 / 42 | 184 / 41 |
| | python | 175 / 43 | 155 / 40 |
| | rust | 169 / 38 | 161 / 45 |
| | typescript | 149 / 45 | 181 / 38 |
| | **all** | **639 / 168 = 807** | **681 / 164 = 845** |
| NAME (mechanical) | go | 74 / 74 | 76 / 76 |
| | python | 74 / 78 | 76 / 72 |
| | rust | 157 / 175 | 143 / 125 |
| | typescript | 154 / 155 | 125 / 145 |
| | **all** | **459 / 482 = 941** | **420 / 418 = 838** |

Label = "the reranked top-1 (the row shown) is an expected row"; absent
queries are always 0. From GM-701 (`gm-701-floor-or-blend.md`): NL right tops
FIT 215 / wrong-or-absent 592, held-out 247 / 598; NAME held-out 339 / 499
(FIT **(measure)**). Rows with an empty window are dropped, as in GM-699/701.

## Features (per query, all from stored data)

Computed in one loop shaped like `gm701.collect` (`gm701_floor_or_blend.py:59`):
`lst = c.lists[B.ST][q][:30]`, `cos`, `logit = sc(c, q, nid)`,
`blend = logit + 40 * cos`, reranked top index `t` (cosine order when the
exact-name skip flag fires).

| # | feature | computation | why |
|---|---|---|---|
| f1 | `cos1` | `lst[0][1]`: cosine of the cosine-order top-1 | F4's own verdict input; the int8-only control uses only this |
| f2 | `cos_t` | `cos[t]`: cosine of the shown (reranked) top | the shown row's own similarity |
| f3 | `logit_t` | `logit[t]` | CE evidence (GM-699: best single signal with blend) |
| f4 | `gap_cos` | `lst[0][1] - lst[1][1]` (0 when n = 1) | a confident cosine winner vs a tie |
| f5 | `gap_logit` | logit of the reranked top-1 minus the reranked top-2 (0 when n = 1) | CE-side margin ("both scores" of the task) |
| f6 | `has_doc` | `c.trimmed_doc[lst[t][0]]` (set by `gm462_ce_enriched.load_nodes`, `gm462_ce_enriched.py:250`) | doc-less symbols score lower on both models (GM-463 doc split) |
| f7-f9 | language | one-hot of the shown top row's language (`c.node_lang`), go = reference | per-language offsets, as the floors have |
| f10 | `exact` (NAME model only) | `flags[(c.name, q)]`, the exact-name skip rule | the product already computes it; on NL it never fires (GM-699: 0 of 1,652) |

Query type (prose vs name) is not a column: it selects the model (D2). The
script asserts `shipped_floor_rates.rust_is_prose(text) == not mechanical`
on every v2 query (GM-434 found 2,279 of 2,279 on v1); if any query
disagrees, the split is by `rust_is_prose`, since that is what the product can
see, and the count is reported.

Not used: the blend itself (a fixed linear combination of f2 and f3, so
redundant in a linear model), corpus identity (not available to the product),
file path or symbol kind (not asked; would add 5+ dummies for ~215 positives).

## Split and leakage

- FIT / held-out are GM-460's sides, unchanged: one row per query, side by
  `sha256(id)[0]`, so no query contributes to both sides.
- All fitted quantities come from FIT only: feature standardisation (means
  and SDs), the L2 strength (CV inside FIT), coefficients, and every
  threshold. Held-out is read once per arm.
- GM-460 lists 2 held-out NL queries that share an expected symbol with a FIT
  query (`req-c025`, `rg-042`). The main tables keep them (as GM-699/701 did);
  one sensitivity line repeats the NL headline with both dropped ("strict
  held-out view").

## Classifier (D3)

L2-regularised logistic regression, one model per population (NL, NAME),
fitted in numpy by Newton/IRLS (about 25 lines; no new dependency).

- Inputs standardised with FIT means/SDs (one-hot columns too, so one penalty
  scale fits all); intercept not penalised.
- Objective: sum of log-losses + (lambda / 2) * ||w||^2. Lambda from
  {0.01, 0.1, 1, 10} by 5-fold stratified CV on FIT (mean log-loss), seed 461;
  the chosen lambda and the CV curve are reported.
- Why this and not trees/boosting: 215 NL right tops on FIT, 9 features, so
  about 24 events per parameter, enough for a linear model and too few for a
  tree ensemble to beat it without tuning on held-out. Logistic regression is
  monotone in every feature, its coefficients can be read and sanity-checked
  (cosine and logit must come out positive), and restricted to one feature it
  is exactly a floor, which is what makes the int8-only control below exact.
  A shippable version is a dot product and a sigmoid: 10-11 constants per
  population.
- Why two models and not one with a prose indicator: the two populations get
  different verdicts (prose below the line gets the soft `lowSimilarity`
  sentence, a name gets the hard `noMatch`, `similarity.rs::verdict`), have
  different budgets (names: misled <= 3% per language, GM-697 D2), and 941
  name rows would dominate a pooled fit. Two models are a pooled model with
  every interaction with query type; the pooled variant is not run.
- Score = the linear predictor z (log-odds); thresholds are on z.

## From score to verdict (thresholds fitted on FIT only)

The verdict is "refuse" (SOFT for NL, HARD for NAME) when `z < t`. One
threshold per population; language already enters through f7-f9.

- **Matched point (primary, NL and NAME):** t = the largest value at which
  the classifier keeps at least as many FIT right tops as F4 keeps on FIT
  (F4 kept = right top whose cosine-order top-1 clears the shipped floor).
  This is the comparison GM-701 lacked: same right tops kept, more or fewer
  wrong ones caught.
- **D6 point (NL and NAME):** t = `fit_floor`'s rule on the FIT right tops'
  z (sorted, index floor(0.03 n), no 2-dp rounding since z is not a cosine).
  For NAME this is the GM-697 budget point (misled <= 3%).

Plumbing: each arm is a list of `rerank_eval.outcome` dicts (the same objects
`R.gates` and `R.gm434` already score). A classifier arm is the F4 outs with
`top = z` and `top_language = "clf"`, scored at floors `{"clf": t}`; so the
existing `clears`, `cw_indicator`, `fa_indicator`, `gm434` and `gates` apply
unchanged. Ranks (and so r@10, MRR) are F4's by construction.

## Arms

| arm | page shown | verdict |
|---|---|---|
| **int8** | cosine order (int8-structured) | cos1 vs shipped floor of the top row's language |
| **F4** (baseline, = GM-701 A) | blend order, K=30, beta 40, skip rule | cos1 vs shipped floor (cosine-order top's language) |
| **C0** int8-only control | as F4 | logistic on f1 + f7-f9 only (see Controls) |
| **L-m** | as F4 | full model, matched point |
| **L-d6** | as F4 | full model, D6 point |
| **L-noCE** (informative) | cosine order | model on f1, f4, f6-f9 (+f10), label = cosine top right, matched to int8; answers "does anything help the rerank-off path" |

## Metrics table layout (results doc `docs/results/gm-461-learned-verdict.md`)

- **T1 quality, held-out NL** (r@10, MRR with CI; name r@10 / MRR): int8, F4,
  L-m. L-m equals F4 by construction; the row is there to show it.
- **T2 D9 gates vs F4** and **T3 D9 gates vs int8** (shipped floors as the
  reference floors): Q1 Δr@10 [lo], Q2 ΔMRR [lo], Q3 worst, Q4 ΔCW [up],
  Q5 ΔFA [up], fails, Q5 per language (Δ, n). Rows: int8, F4, C0, L-m, L-d6.
  Via `rerank_q5_diag.fixed_floors_gates` (one-sided 95%, language-stratified
  bootstrap, seed/resamples from `variants.toml`, as GM-443 S11).
- **T4 GM-434 columns** at each row's own threshold: NL misled, NL CW pos,
  NL CW absent, name misled, name CW pos, name CW absent (held-out halves;
  name held-out by `name_held_out`). Rows as T2 plus L-noCE.
- **T5 per language, held-out NL** and **T6 per language, held-out name**:
  misled / CW pos / CW absent per language, counts shown (x/n).
- **T7 paired deltas vs F4** (GM-701 `boot`, 10,000 resamples of held-out
  queries, seed 461): caught delta and kept delta with 95% CI, NL and NAME,
  for C0, L-m, L-d6; plus the number of held-out queries whose verdict
  differs from F4 (and how many of them are right tops).
- **T8 sensitivity:** the T4 NL row of F4 and L-m on the v1 subset of
  held-out (215 + 46) and on the strict held-out view.
- **T9 overfitting:** per arm and population, FIT vs held-out: AUROC of z
  (`gm699_separability.auroc`), mean log-loss, kept share, caught share; and
  the CV log-loss per lambda.
- **T10 model:** standardised coefficients per population with 95% bootstrap
  intervals over FIT queries (1,000 refits), chosen lambda.

## Controls

1. **F4 reproduces GM-701.** F4 on held-out: NL kept 186/247, caught 265/598;
   NAME kept 329/339, caught 330/499 (`gm-701-floor-or-blend.md`, arm A).
   int8 on held-out NL: CW pos 338/681, CW absent 37/164, FA 40/184
   (`gm-697-floor-refit.md` / `gm-460-enlarged-eval.md`). Any mismatch stops
   the run.
2. **int8-only control (C0), exact.** Logistic on f1 plus the language
   one-hot (language of the cosine-order top, as the floor uses) is monotone
   in f1 within each language when its f1 coefficient is positive (asserted).
   Then (a) thresholds set to z(shipped floor, language) must give the
   shipped verdict on every FIT and held-out query, NL and NAME, and C0's
   T2/T4 rows must equal F4's cell for cell; (b) at the D6 rule applied per
   language on z, the implied cosine cut, rounded down to 2 dp, must equal
   `R.fit_floors` on the int8 outs, i.e. the v2 D6 refit 0.51 / 0.47 / 0.57 /
   0.46 (`gm-460-enlarged-eval.md`). (a) proves the verdict plumbing, (b) the
   threshold rule.
3. **Shuffled labels.** FIT labels permuted within language, model and
   matched threshold refitted, scored on real held-out labels; 200 shuffles
   (seed 462). The caught gain over F4 must vanish (GM-701's control (a)).
4. **Fitter check.** On the real FIT matrix, the numpy fit's gradient norm at
   the solution is < 1e-8, and its coefficients equal scikit-learn's
   `LogisticRegression(C=1/lambda)` to 1e-4 (one-off, run with the
   `g-mesh-data/gm436/venv` interpreter on a dumped `.npz`; not a dependency
   of the script).
5. **Differ count.** Number of held-out queries where L-m's verdict differs
   from F4's is reported; 0 means the arm measured nothing.

## Overfitting check

T9 gives FIT vs held-out for AUROC, log-loss, kept and caught. The model is
called overfit if held-out AUROC is more than 0.05 below FIT, or if the
held-out kept share at the matched point falls more than 5 points below the
FIT kept it was matched to. GM-701's per-language thresholds dropped from 90%
FIT to 85.1% held-out kept in rust, which is the size of drift this guards
against. Overfit means the verdict is null whatever T2-T7 say.

## Verdict rule (fixed before measuring)

- **NL:** L-m wins if, against F4, Q4 ΔCW < 0 with its one-sided upper bound
  < 0 (superiority, not just non-inferiority) and Q5 passes (Δ <= 0, upper
  <= +5). NL refusals are soft, so this is a better warning, not a no-match.
- **NAME:** L-d6 wins if held-out name misled <= 3% in every language (GM-697
  D2 rule, Wilson upper reported) **and** T7's caught delta vs F4 has a lower
  bound > 0.
- Ship candidate only if a population wins and is not overfit; then a
  follow-up task designs the Rust side (coefficients in `similarity.rs`, the
  reranked top's logit and gaps passed to `handle`, ADR 0012 addendum). The
  logit exists only with reranking on, so the rerank-off path keeps the
  floors unless L-noCE wins on its own against int8.
- Otherwise: close null, floors stay.

## Stop rules

- Any control 1-2 mismatch, a missing CE pair, a model sha mismatch, or a
  non-positive net cosine coefficient (cos1 + cos_t where both are inputs;
  cos1 alone in C0 and L-noCE) or logit coefficient: stop, report, no tables.
  The two cosines are collinear (equal whenever the rerank keeps the cosine
  top), so only their sum is checked. Owner decision (2026-11-20, verbatim):
  "Проверять суммарный вклад косинуса (Recommended)".
- During S2 one diagnostic `--ignore-stops` run read held-out once (it found
  the per-coefficient rule firing on the collinear cosines); no model,
  feature or threshold change was made after it, only the stop rule above.
- Fewer than 50 held-out wrong-or-absent tops clearing F4 in a population, or
  fewer than 100 held-out right tops: that population's verdict is
  "unreadable" (expected: NL 333 cleared wrong/absent, 186 kept right; NAME
  169 and 329, from GM-701's arm A).
- Per-language cells with fewer than 20 right tops are shown but gate
  nothing (NAME go/python held-out have 76 positives each; the per-language
  name misled budget is gated as GM-697 did, with its Wilson upper).
- The measure slice runs once. A rerun with other features, other
  thresholds or another model family after seeing held-out is a new task
  (held-out would then be spent).

## Python environment

The script runs under the same interpreter as `gm699_separability.py` and
`gm701_floor_or_blend.py`: system `python3`
(`/Library/Developer/CommandLineTools/usr/bin/python3`, numpy 2.0.2,
tokenizers, onnxruntime present; scikit-learn absent). The logistic fit is
written in numpy, so nothing is added. `~/Projects/ClaudeProjects/g-mesh-data/gm436/venv`
has scikit-learn 1.9.1 (numpy 1.26.4, no onnxruntime) and is used only for
control 4, on a dumped matrix.

## Edit map

New files only; no existing script changes.

`eval/embedding/gm461_learned_verdict.py`:

| function | does | reuses (file:line, 1-based) |
|---|---|---|
| `main(--work, --out, --resamples, --shuffles, --dump-fit)` | setup, rows, arms, controls, JSON + markdown tables | |
| setup | `gm701.setup(work, None)` unchanged: sha check, ship texts, Rust cache, skip flags | `gm701_floor_or_blend.py:44`; `gm463_bakeoff.skip_flags` `:471`; `score_of_for` `:241` |
| `features(cs, sc, flags)` | one row per query: f1-f10, label, population, side, language, ids | loop of `gm701.collect` `:59`; `c.trimmed_doc` from `gm462_ce_enriched.load_nodes` `:243/:250`; `shipped_floor_rates.rust_is_prose` `:295`; `gm701.name_held_out` `:40` |
| `arm_outs(cs, sc, flags)` | int8 outs (cosine order) and F4 outs | `gm463_bakeoff.f4_skip` `:489`; `rerank_eval.outcome` `:216` |
| `standardise(X_fit)` / `fit_logreg(X, y, lam)` / `cv_lambda(X, y, grid, seed)` | numpy IRLS with L2 (intercept free), 5-fold stratified CV | new |
| `matched_threshold(z_right, kept_target)` / `d6_threshold(z_right)` | thresholds on FIT | rule of `rerank_eval.fit_floors` `:274` without rounding |
| `as_verdict(outs, z, t)` | copy of outs with `top=z`, `top_language="clf"`, floors `{"clf": t}` | `rerank_eval.clears` `:287` |
| tables | T2/T3 via `rerank_q5_diag.fixed_floors_gates` `:280` -> `rerank_eval.gates` `:361`; T4-T6 via `rerank_eval.gm434` `:392`; T7 via `gm701.boot` `:111`; T9 AUROC via `gm699_separability.auroc` `:31` | |
| controls | 1: compare to the GM-701/GM-697 constants above; 2: C0 exact; 3: shuffles like `gm701.shuffled` `:129`; 5: differ count | `shipped_floor_rates.structured_floors` `:48` for the floors |

`eval/embedding/test_gm461_learned_verdict.py` (unittest, `python3 -I`, style
of `test_gm453_summary.py`), on small synthetic arrays, no work dir.

Run (measure slice): `python3 eval/embedding/gm461_learned_verdict.py --work
~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work --out <scratch>/gm461.json`
with `uptime` and `/usr/bin/time -p`; GM-701's run of the same setup took
10.7 s, so one run in the background, one wakeup.

## What the tests slice must pin (controls: revert the code, the test must fail)

| behaviour pinned by a test | control (revert in a throwaway worktree) |
|---|---|
| fit is the L2 optimum: gradient ~0 including the penalty term; intercept unpenalised | drop the `lam * w` term from the gradient |
| standardisation uses FIT statistics only | compute means/SDs over FIT + held-out |
| matched threshold keeps >= the target count on FIT and is the largest such cut (`>=` at the boundary) | change `z >= t` to `z > t` |
| `as_verdict` reproduces a floor verdict exactly for a 1-D monotone model (C0 logic on synthetic data) | key the threshold by the row's language instead of `"clf"` |
| `gap_logit` is taken in rerank order (reranked top-1 minus reranked top-2), `gap_cos` in cosine order | compute `gap_logit` in cosine order |
| no query appears on both sides; fit uses FIT rows only | fit on all rows |
| `has_doc` reads the shown row (`lst[t]`), not the cosine top | read `lst[0]` |

Seven controls, grouped into one test binary (one file), so one interpreter
run per control.

## Proposed slices

S2 implement (sonnet, this edit map), S3 tests + controls, S4 verify and
measure (one background run, results doc with T1-T10 and the verdict),
S5 owner decision recorded in the results doc.

## Decisions for the owner

1. **Baseline (D1):** F4 on today's shipped stack on v2 (= GM-701 arm A),
   instead of GM-443 S11's v1 F4 whose stack no longer ships; v1 appears only
   as a subset sensitivity row. Risk: no literal tie to the GM-443 numbers.
2. **Two models (NL soft, NAME hard) (D2)** rather than one pooled model with
   a prose feature. Risk: query type is not tested as a feature in its own
   right; benefit: each model is judged against its own verdict and budget.
3. **Win rule:** NL superiority on Q4 at matched kept; NAME at the 3% point
   with a positive paired caught gain. Risk: strict, so a small real gain may
   read as null.
4. **numpy logistic regression, no new dependency** (sklearn only as a
   one-off cross-check). Risk: own fitter code, covered by control 4 and the
   tests.

## Appendix: where each claim came from

- g-mesh `select_project` (g-mesh-wt-gm461), then `get_file_outline` on
  `gm701_floor_or_blend.py`, `gm699_separability.py`,
  `shipped_floor_rates.py`, `gm463_bakeoff.py`: the helper lists above.
- g-mesh `find_definition`: `fit_floor` (both copies), `bucket_of`
  (`gm462_ce_enriched.py`), `fixed_floors_gates`, `gates` (three
  same-named functions; `rerank_eval.gates` is the one used).
- g-mesh `find_callers`: `gm701.setup` (only `gm701.main`), `f4_skip`
  (only `skip_sim`), `fixed_floors_gates` (only `fixes.row`); no shared
  helper changes signature, so no other caller is affected. These answers
  carried `provenance: python semanticTier absent` (method calls through a
  receiver may be missing); none is relied on beyond "we only call these".
- grep: 1-based line numbers in the edit map, `trimmed_doc`, `docComment`
  handling, query counts (a short script over `eval/embedding{,/v2}/queries`).
