# GM-699/S1: separability of the top-1 score on v2 NL FIT

Script: `eval/embedding/gm699_separability.py --work ../g-mesh-wt-gm463/eval/embedding/v2/work` (reads only; no dump needed, see Inputs). Measurement only, no verdict.

## Setup

- Split: v2 authored NL queries with harness `held_out == False` (FIT): 639 positives + 168 absent = 807 queries. Held-out (845: 681 + 164) is given as a second block.
- Top-1 = first row after the shipped rerank: top 30 cosine rows (int8-structured run) ordered by `logit + 40 * cosine` (ties by window position), ship text E+P+B48, one pair per run; the exact-name skip rule (`confident`, rule (a) of `gm463_bakeoff.skip_flags`) keeps cosine order. Skip rule fired on 0 of 807 FIT queries (0 of 845 held-out), so every FIT top-1 is the blend order's.
- Right = the top row is an expected row. Wrong = positive query with another top row. Absent = query with no answer, counted as wrong.
- Signals are those of the reranked top-1 row: its cosine, its CE logit, its blend (logit + 40 * cosine).
- AUROC = P(right row scores above a wrong/absent row), ties 0.5. CI: percentile 95% of 2000 bootstrap resamples of queries (within the language; pooled = over all queries), one resample serves all three signals (paired). Seed 699.

## AUROC, NL FIT (reranked top-1)

| language | right | wrong | absent | cosine | CE logit | blend |
|---|---|---|---|---|---|---|
| go | 56 | 90 | 42 | 0.616 [0.532, 0.702] | 0.672 [0.583, 0.759] | 0.670 [0.585, 0.750] |
| python | 59 | 116 | 43 | 0.671 [0.587, 0.756] | 0.741 [0.669, 0.810] | 0.753 [0.682, 0.824] |
| rust | 42 | 127 | 38 | 0.694 [0.593, 0.790] | 0.784 [0.699, 0.863] | 0.792 [0.718, 0.862] |
| typescript | 58 | 91 | 45 | 0.640 [0.551, 0.727] | 0.690 [0.609, 0.770] | 0.706 [0.627, 0.789] |
| pooled | 215 | 424 | 168 | 0.637 [0.595, 0.678] | 0.696 [0.653, 0.737] | 0.705 [0.664, 0.744] |

Paired difference to cosine (same resamples), 95% CI:

| language | logit - cosine | blend - cosine |
|---|---|---|
| go | [-0.050, +0.164] | [-0.009, +0.117] |
| python | [-0.029, +0.167] | [+0.030, +0.136] |
| rust | [-0.029, +0.205] | [+0.026, +0.171] |
| typescript | [-0.049, +0.151] | [+0.003, +0.132] |
| pooled | [+0.008, +0.110] | [+0.036, +0.099] |

## Same, NL held-out (not the D5 split; for reference)

| language | right | wrong | absent | cosine | CE logit | blend |
|---|---|---|---|---|---|---|
| go | 71 | 113 | 41 | 0.577 [0.497, 0.659] | 0.715 [0.640, 0.781] | 0.696 [0.623, 0.764] |
| python | 46 | 109 | 40 | 0.732 [0.647, 0.810] | 0.739 [0.657, 0.819] | 0.772 [0.692, 0.846] |
| rust | 47 | 114 | 45 | 0.616 [0.511, 0.715] | 0.771 [0.688, 0.844] | 0.765 [0.684, 0.837] |
| typescript | 83 | 98 | 38 | 0.703 [0.631, 0.771] | 0.744 [0.669, 0.814] | 0.774 [0.705, 0.838] |
| pooled | 247 | 434 | 164 | 0.637 [0.595, 0.676] | 0.731 [0.697, 0.766] | 0.732 [0.696, 0.768] |

## Supplementary: the verdict's own top-1 (cosine order, no rerank)

What `handle` judges today: cosine of the cosine-order top-1, right = that row is right. FIT AUROC [CI]: go 0.656 [0.574, 0.737], python 0.671 [0.569, 0.771], rust 0.758 [0.664, 0.848], typescript 0.601 [0.510, 0.691], pooled 0.615 [0.565, 0.664]. Right counts there: 42 / 46 / 25 / 58 = 171 of 807 (reranked: 215).

## Control 1: shuffled labels

The right/wrong labels of the 807 FIT queries permuted (pooled), 100 permutations, 300 bootstrap resamples each. Mean AUROC over permutations [2.5%, 97.5% of permutations], share of permutations whose bootstrap CI contains 0.5:

| language | cosine | CE logit | blend |
|---|---|---|---|
| go | 0.504 [0.397, 0.597] 0.93 | 0.509 [0.418, 0.585] 0.98 | 0.510 [0.429, 0.584] 0.95 |
| python | 0.506 [0.427, 0.593] 0.91 | 0.501 [0.426, 0.581] 0.96 | 0.504 [0.426, 0.594] 0.94 |
| rust | 0.505 [0.437, 0.590] 0.97 | 0.501 [0.428, 0.571] 0.97 | 0.505 [0.424, 0.585] 0.98 |
| typescript | 0.496 [0.387, 0.587] 0.91 | 0.513 [0.425, 0.595] 0.93 | 0.507 [0.429, 0.593] 0.96 |
| pooled | 0.503 [0.465, 0.551] 0.93 | 0.506 [0.469, 0.545] 0.96 | 0.506 [0.468, 0.544] 0.95 |

Passes: means 0.50-0.51, CI coverage 0.91-0.98. A first single permutation (seed 700, 2000 resamples) gave pooled cosine 0.555 [0.511, 0.599], a CI that excludes 0.5; that is one of the expected ~5% and about 2.4 sd of the permutation spread (0.023), which is why the control was widened to 100 permutations. Its other cells: pooled logit 0.485 [0.438, 0.530], blend 0.520 [0.473, 0.566].

## Control 2: cross-check against gm-697-floor-refit.md

Cosine order, int8-structured, shipped floors (0.57 / 0.59 / 0.57 / 0.53, floor of the top row's language, "clears" = score >= floor), NL confident-wrong = top clears and is wrong, or absent query clears:

| split | CW positives | CW absent | right (cosine order) |
|---|---|---|---|
| held-out | 338/681 (49.6%) | 37/164 (22.6%) | 184/681 (27.0%) |
| FIT | 311/639 (48.7%) | 46/168 (27.4%) | 171/639 (26.8%) |

The 49.6% / 22.6% figures in GM-697 (design D5, table S2: "NL CW pos 338/681", "CW absent 37/164") are the held-out half, not FIT; the held-out row reproduces them exactly, so this measures the same set and machinery. They are also confident-wrong rates (wrong and above the floor), not "right" rates. The brief's "on FIT" check cannot match by construction (FIT is 639 + 168).

## Inputs

- Cross-encoder: `models/ms-marco-MiniLM-L6-v2-quint8/model.onnx` in the v2 work dir = shipped `onnx/model_quint8_avx2.onnx`, sha256 recomputed by the script: `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9` (script stops if different).
- Logits: Rust `CrossEncoder::score` cache `v2/work/rerank_cache_gm700_rust` (key `82d82801c3afcafb`, variant `ce-minilm-quint8-rust`), from GM-700's S22 dump. It holds all 102,917 K=30 pairs of all 3431 queries; every FIT and held-out pair was found (a missing one raises KeyError), so nothing was dumped or rebuilt. Logits exist for every window including skipped ones; no FIT query was skipped.
- Cosine/windows: `v2/work/runs/jina-v2-base-code-int8-structured`; queries `eval/embedding/v2/queries` (via the harness loader). Floors parsed from `core/src/mcp/similarity.rs`.
- Run: 162 s real / 158 s user on a quiet machine (`load 3.8 11.4 19.8` at the end), Python 3 with numpy, no ORT run.

## Verdict

**Conditional yes.** A blend-score threshold is worth designing as an *additional* soft signal on the rerank-on path, not as a replacement for the GM-697 cosine floors. It must first beat the shipped floor on the same page (condition below). It is not worth turning reranking on for.

### What a threshold buys (operating points)

Method: for each signal, the threshold is the score that keeps a given share of right reranked top rows on FIT (the 10th percentile of the right rows' scores for 90%). "Caught" is the share of wrong-or-absent tops below it. The same threshold is then applied unchanged to held-out. Rows are those of S1's `collect` (FIT 215 right / 592 wrong+absent; held-out 247 / 598). The helper script was a scratch file and is not committed. It ran in 7.7 s real / 5.6 s user (`load 2.49 8.93 17.98`). If refusals were random, keeping 90% of right tops would catch 10% of bad ones.

| keep right (FIT) | threshold | cosine caught FIT / HO | blend caught FIT / HO | logit caught FIT / HO |
|---|---|---|---|---|
| 90% | one pooled | 18.9% / 18.2% | 25.7% / 24.7% | 25.3% / 23.7% |
| 90% | per language | 19.4% (115/592) / 17.4% (104/598) | 32.3% (191/592) / 30.4% (182/598) | 28.4% / 29.8% |
| 80% | one pooled | 35.6% / 32.1% | 49.3% / 43.1% | 40.0% / 39.0% |
| 80% | per language | 36.3% (215/592) / 33.8% (202/598) | 50.5% (299/592) / 46.8% (280/598) | 49.8% / 48.2% |

The held-out shares of right tops kept, at the per-language 90% points, are cosine 92.3% (228/247) and blend 93.5% (231/247). At the 80% points they are 81.8% and 86.2%.

Paired bootstrap on FIT (2000 resamples, seed 699, threshold refitted in every resample), blend caught minus cosine caught, 95% CI:
- keep 90%, one pooled threshold: [-2.4, +16.9] points. Not separated.
- keep 90%, per language: [+2.8, +22.8] points.
- keep 80%, one pooled threshold: [0.0, +23.4] points.
- keep 80%, per language: [+3.1, +24.5] points.

Per language at 90% (FIT, blend vs cosine): rust 44.8% vs 22.4%, python 31.4% vs 18.9%, go 28.0% vs 16.7%, typescript 22.1% vs 19.1%. Rust gains most and typescript barely moves. The per-language thresholds rest on 42-59 right rows each. Held-out mostly keeps them, but rust held-out keeps only 85.1% at the 90% point.

### Reading

- **Effect size.** At about 90% of right tops kept, the blend flags about 30% of wrong/absent tops against about 18% for cosine on the same reranked row. That is roughly +12 points, or 1.7x, and it replicates on held-out. Still, about 70% of wrong tops pass unflagged. Refused pages are about 88-92% wrong at that point, against a 73% base rate. This is a better warning, not a no-answer detector.
- **Against what ships today.** The shipped floors judge the *cosine-order* page (`judged` in `handle`). On held-out NL they give the soft note at about 78% kept (144/184 right tops unflagged) and catch 43.3% (286/661 = 159 wrong positives + 127 absent; Control 2 / GM-697 table S2). The blend at a matched ~80% FIT keep catches 46.8% held-out, keeping 86.2%. That is a different page with a different right rate (247 vs 184 of 845 right), so the two are not paired. The blend's margin over today's verdict is therefore unproven and may be small. Most of the visible gain in right rows comes from the rerank itself, not from the refusal signal.
- **Where it could act.** All of these are prose (authored) queries, and the product gives prose pages only the soft `lowSimilarity` sentence, never `noMatch`. A blend signal would therefore tune a soft note. Name queries, the hard-verdict population, already meet the 3% misled budget at the shipped floors (GM-697 exit verdict) and were not measured here.
- **Cost.** The logits exist only when reranking runs, which costs about +285 ms at p50 at K=30 (`gm-700-latency.md`, new vs off). On that path the signal costs nothing extra, because the logit and blend are already computed in `blended_order`. Off that path it does not exist, so the cosine floors stay the only verdict for rerank-off users and for the name path. The ~12-point gain does not justify enabling reranking for it.

### Condition and follow-up scope (if taken)

Condition: on held-out NL, the combined rule "shipped floor on the cosine page, **or** blend below a per-language threshold on the reranked page" must catch more wrong/absent pages than the floor alone, at a matched kept-right share. That needs a paired bootstrap CI that excludes 0, and a check that name-query misled stays at or below 3%.

Follow-up task: (1) fit per-language blend thresholds on FIT for the rerank-on path and measure the combined rule against the floor alone on held-out, paired, NL and names. (2) If the condition holds, design the soft-note wiring: `handle` needs the reranked top-1 blend next to `judged`, and the ADR 0012 addendum documents the constants. If it fails, close with a "floors only" note.
