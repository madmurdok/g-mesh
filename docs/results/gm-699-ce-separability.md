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
