# GM-443 S1: can a small embedder serve as the recall stage?

Question: gte-small and bge-small-en-v1.5 cost ~0.22x jina's indexing
(GM-398's cost table). If their top-K holds the right answer about as often
as jina's top 10, a reranker over that top-K could restore jina-level
quality at small-model indexing cost. This slice measures only the
candidate sets, from the stored GM-398 runs. Nothing was re-embedded, and no
reranker was chosen or run.

## Method

- Data: the stored `g-mesh embed-eval run` output
  (`eval/embedding/work/runs/<variant>/<corpus>/rankings.jsonl`, 6 corpora),
  the same runs GM-398 reports. Each query keeps its top 100 hits
  (`KEPT_HITS`), so **the largest K available is 100**. Every query in every
  arm holds exactly 100 hits. All three arms share each corpus's snapshot and
  node-id hash, so their hit lists can be compared and unioned.
- Script: `eval/embedding/recall_at_k.py --runs <runs dir>`. It uses the
  definitions from `embed_eval.rs` / D5: a query's rank is the first expected
  node id among the kept hits, recall@K is 1 if that rank is <= K, and the
  pooled value is the mean of per-language means, summed in language order.
  It also checks the query files against each manifest (D3 freeze).
- Strata: **NL** is the authored positives, GM-398's scored set (n=400, 100
  per language). **name** is the mechanical name queries' positives (D6;
  n=879), which GM-398 never scores and which appear here only for
  information.

### Control: GM-398 reproduced

Before reporting anything, the script parses GM-398's pooled table (r@1,
r@5, r@10, MRR) and per-language r@10 from
`docs/results/gm-398-model-comparison.md`. It exits if any value differs at
the table's precision. All three arms match exactly: jina fp32
0.335 / 0.542 / 0.633 / 0.427, gte-small 0.275 / 0.478 / 0.580 / 0.373 and
bge-small 0.258 / 0.485 / 0.557 / 0.360, plus every per-language cell.

The check does catch real differences. The first version summed the
language means in insertion order and got gte r@5 = 0.4775 → 0.477 against
GM-398's 0.478, and the script stopped. Summing in BTreeMap order, as Rust
does, fixed it.

## recall@K, NL (pooled over languages)

| arm | @10 | @20 | @50 | @100 |
|---|---|---|---|---|
| jina fp32 (reference) | **0.633** | 0.718 | 0.810 | 0.880 |
| gte-small | 0.580 | 0.657 | **0.725** | 0.807 |
| bge-small-en-v1.5 | 0.557 | 0.625 | **0.718** | 0.792 |
| union gte ∪ bge top-K (aside) | 0.610 (13 cand.) | 0.680 (26) | 0.748 (65) | - |

Per language, NL:

| arm | lang | @10 | @20 | @50 | @100 |
|---|---|---|---|---|---|
| jina | go / python / rust / ts | 0.74 / 0.69 / 0.34 / 0.76 | 0.85 / 0.77 / 0.44 / 0.81 | 0.94 / 0.86 / 0.58 / 0.86 | 0.95 / 0.92 / 0.72 / 0.93 |
| gte-small | go / python / rust / ts | 0.70 / 0.63 / 0.32 / 0.67 | 0.74 / 0.74 / 0.39 / 0.76 | 0.79 / 0.82 / 0.48 / 0.81 | 0.82 / 0.91 / 0.61 / 0.89 |
| bge-small | go / python / rust / ts | 0.67 / 0.59 / 0.31 / 0.66 | 0.72 / 0.68 / 0.36 / 0.74 | 0.79 / 0.78 / 0.49 / 0.81 | 0.85 / 0.85 / 0.60 / 0.87 |

## recall@K, name queries (not in GM-398's score)

| arm | @10 | @20 | @50 | @100 |
|---|---|---|---|---|
| jina fp32 | 0.917 | 0.957 | 0.979 | 0.986 |
| gte-small | 0.943 | 0.963 | 0.976 | 0.987 |
| bge-small-en-v1.5 | 0.948 | 0.971 | 0.987 | 0.992 |

On name queries both small models already match or beat jina at every K,
so the recall stage costs nothing there. Rust is the weakest language for
every model: at @10, 0.83-0.87 against >= 0.9 elsewhere.

## Gate: small recall@50 >= jina fp32 recall@10 (0.633), NL pooled

| arm | r@50 | Δ vs jina r@10 [one-sided 95% lower] | first K reaching 0.633 | verdict |
|---|---|---|---|---|
| gte-small | 0.725 | +0.092 [+0.050] | 14 | **GO** |
| bge-small-en-v1.5 | 0.718 | +0.085 [+0.043] | 22 | **GO** |

The bound comes from a paired bootstrap over queries, stratified by
language, with 10,000 resamples and seed 398 (D5's method). It runs on
Python's RNG, so its values are not bit-identical to the Rust bootstrap.
The gate also holds in every language: small r@50 against jina r@10 is
go 0.79 vs 0.74, python 0.82/0.78 vs 0.69, rust 0.48/0.49 vs 0.34 and ts 0.81
vs 0.76.

## For S2: the ceiling

A reranker only reorders the candidate set, so its recall@10 (and any
recall@k) can be no higher than the set's recall@K. The table gives that
ceiling for NL queries:

| candidates | ceiling | vs jina r@10 | small r@10 today |
|---|---|---|---|
| gte top-20 | 0.657 | +0.025 | 0.580 |
| gte top-50 | 0.725 | +0.092 | 0.580 |
| gte top-100 | 0.807 | +0.175 | 0.580 |
| bge top-50 | 0.718 | +0.085 | 0.557 |
| bge top-100 | 0.792 | +0.160 | 0.557 |

Points for S2:

- **Clearing the gate says the ceiling is high enough, not that a reranker
  will reach it.** To match jina, a reranker over gte's top-50 must put the
  right answer in its top 10 for 0.633 / 0.725 ≈ 87% of the queries whose
  answer is in the set. Over top-100 the figure is ≈ 78%, but that means
  twice the reranker work per query.
- The candidate set loses some ground that jina has: 32 of 400 NL queries
  that jina ranks in its top 10 are outside gte's top 50. On the other
  side, 28 of 400 are outside all three arms' top 100, and no reranker can
  recover those.
- For comparison, jina's own top-50 ceiling is 0.810, so a reranker over
  jina would have more headroom. The trade this task studies is 0.725 vs
  0.810 ceiling at ~0.22x indexing cost.
- The union of both small models barely helps. Top-50 of each goes to 0.748
  from 0.725, with ~65 candidates and two indexes, so it is not worth
  pursuing.
- Rust is the weak spot. gte's rust top-50 reaches 0.48, against 0.58 for
  jina's rust top-50, and gte's rust ceiling at K=100 is only 0.61.

## Reproduce

```
python3 eval/embedding/recall_at_k.py --runs <main checkout>/eval/embedding/work/runs
```

This runs in about 5 s (`real 5.45`, `user 5.26`, load average 7.6). Most
of that is the bootstrap.

## S4: recall stage + rerank, measured

Two recall stages, each from the stored GM-398 rankings (top 100 per query):
gte-small, and jina-v2-base-code int8, the shipped model (ADR 0011). Each
arm's top-K (K = 50, 100) is reranked by: identity and shuffled (controls),
two general-English cross-encoders (`cross-encoder/ms-marco-MiniLM-L6-v2`,
`jinaai/jina-reranker-v1-tiny-en`, on the exact text the embedders saw),
cross-encoder + arm cosine (beta tuned on the FIT half), and a graph rerank
(arm cosine + in-degree / top-K neighbours / same-file hits, weights tuned on
the FIT half). Every variant is gated (D9) against two baselines: jina fp32
at its D6 floors, and shipped jina int8 at the floors
`similarity.rs::floor` ships (read from the source). Script:
`eval/embedding/rerank_eval.py`.

### Controls

- **Identity reproduces GM-398 exactly**, for both arms and both K: rank,
  top score and top language equal the stored run on 2279/2279 queries, and
  r@1/r@5/r@10 [lo, hi], MRR [lo, hi], CW combined/positives/absent, floors,
  per-language held-out false alarm and the D9 row vs fp32 all match
  `gm-398-model-comparison.md` (gte and int8).
- **int8's D6-fitted floors equal the shipped floors** (0.57 / 0.57 / 0.55 /
  0.53), so "int8 at shipped floors" is the GM-398 int8 arm.
- **Shuffled fails** Q1 against both baselines, over both arms (r@10 0.185
  over gte, 0.182 over int8).

### Results

Queries: 2279 over excalidraw, g-mesh, gin, py-requests, ripgrep, task-tracker-mcp. Tuned variants (beta, graph weights) are scored and gated on the held-out NL half only; the others on all NL positives. Bounds are one-sided 95% (SplitMix64 bootstrap, languages weighted equally).

#### Quality

| variant | K | on | NL r@10 [lo, hi] | NL MRR [lo, hi] | kept of recall@K | held-out r@10 / MRR (n) | name r@10 / MRR |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | - | all NL | 0.633 [0.595, 0.670] | 0.427 [0.395, 0.460] | - | 0.633 / 0.423 (215) | 0.917 / 0.745 |
| jina int8 (baseline, shipped floors) | - | all NL | 0.637 [0.600, 0.675] | 0.424 [0.392, 0.457] | - | 0.645 / 0.419 (215) | 0.915 / 0.750 |
| gte (stored) | - | all NL | 0.580 [0.540, 0.617] | 0.373 [0.340, 0.406] | - | 0.586 / 0.351 (215) | 0.943 / 0.836 |
| identity over gte K=50 | 50 | all NL | 0.580 [0.540, 0.617] | 0.373 [0.340, 0.406] | 0.80 of 0.725 | 0.586 / 0.351 (215) | 0.943 / 0.836 |
| identity over gte K=100 | 100 | all NL | 0.580 [0.540, 0.617] | 0.373 [0.340, 0.406] | 0.72 of 0.807 | 0.586 / 0.351 (215) | 0.943 / 0.836 |
| shuffled over gte K=50 | 50 | all NL | 0.185 [0.152, 0.217] | 0.072 [0.061, 0.084] | 0.26 of 0.725 | 0.168 / 0.065 (215) | 0.222 / 0.094 |
| identity over int8 K=50 | 50 | all NL | 0.637 [0.600, 0.675] | 0.424 [0.392, 0.457] | 0.78 of 0.820 | 0.645 / 0.419 (215) | 0.915 / 0.750 |
| identity over int8 K=100 | 100 | all NL | 0.637 [0.600, 0.675] | 0.424 [0.392, 0.457] | 0.73 of 0.878 | 0.645 / 0.419 (215) | 0.915 / 0.750 |
| shuffled over int8 K=50 | 50 | all NL | 0.182 [0.150, 0.215] | 0.090 [0.075, 0.105] | 0.22 of 0.820 | 0.188 / 0.088 (215) | 0.207 / 0.090 |
| ce-minilm over gte K=50 | 50 | all NL | 0.530 [0.490, 0.570] | 0.350 [0.316, 0.383] | 0.73 of 0.725 | 0.534 / 0.350 (215) | 0.962 / 0.885 |
| ce-minilm+gte over gte K=50 (beta=320) | 50 | held-out NL | 0.611 [0.557, 0.666] | 0.364 [0.321, 0.409] | 0.81 of 0.751 | 0.611 / 0.364 (215) | 0.962 / 0.876 |
| ce-minilm over gte K=100 | 100 | all NL | 0.518 [0.478, 0.557] | 0.340 [0.307, 0.373] | 0.64 of 0.807 | 0.524 / 0.339 (215) | 0.968 / 0.887 |
| ce-minilm+gte over gte K=100 (beta=320) | 100 | held-out NL | 0.606 [0.551, 0.660] | 0.364 [0.321, 0.409] | 0.73 of 0.825 | 0.606 / 0.364 (215) | 0.966 / 0.877 |
| ce-minilm over int8 K=50 | 50 | all NL | 0.587 [0.550, 0.627] | 0.371 [0.338, 0.404] | 0.72 of 0.820 | 0.613 / 0.373 (215) | 0.961 / 0.891 |
| ce-minilm+int8 over int8 K=50 (beta=40) | 50 | held-out NL | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.83 of 0.839 | 0.693 / 0.477 (215) | 0.970 / 0.888 |
| ce-minilm over int8 K=100 | 100 | all NL | 0.542 [0.502, 0.583] | 0.360 [0.327, 0.394] | 0.62 of 0.878 | 0.556 / 0.363 (215) | 0.970 / 0.892 |
| ce-minilm+int8 over int8 K=100 (beta=40) | 100 | held-out NL | 0.693 [0.642, 0.745] | 0.474 [0.428, 0.521] | 0.78 of 0.886 | 0.693 / 0.474 (215) | 0.977 / 0.894 |
| ce-jina-tiny over gte K=50 | 50 | all NL | 0.542 [0.502, 0.582] | 0.343 [0.309, 0.376] | 0.75 of 0.725 | 0.537 / 0.335 (215) | 0.954 / 0.816 |
| ce-jina-tiny+gte over gte K=50 (beta=80) | 50 | held-out NL | 0.622 [0.569, 0.675] | 0.358 [0.315, 0.403] | 0.83 of 0.751 | 0.622 / 0.358 (215) | 0.954 / 0.863 |
| ce-jina-tiny over gte K=100 | 100 | all NL | 0.522 [0.482, 0.562] | 0.335 [0.302, 0.368] | 0.65 of 0.807 | 0.526 / 0.326 (215) | 0.954 / 0.815 |
| ce-jina-tiny+gte over gte K=100 (beta=80) | 100 | held-out NL | 0.622 [0.569, 0.675] | 0.358 [0.315, 0.403] | 0.75 of 0.825 | 0.622 / 0.358 (215) | 0.956 / 0.863 |
| ce-jina-tiny over int8 K=50 | 50 | all NL | 0.583 [0.542, 0.622] | 0.361 [0.328, 0.394] | 0.71 of 0.820 | 0.577 / 0.352 (215) | 0.957 / 0.824 |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | 50 | held-out NL | 0.671 [0.618, 0.723] | 0.433 [0.387, 0.479] | 0.80 of 0.839 | 0.671 / 0.433 (215) | 0.962 / 0.861 |
| ce-jina-tiny over int8 K=100 | 100 | all NL | 0.560 [0.520, 0.600] | 0.354 [0.321, 0.388] | 0.64 of 0.878 | 0.542 / 0.340 (215) | 0.963 / 0.821 |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | 100 | held-out NL | 0.663 [0.609, 0.715] | 0.432 [0.386, 0.479] | 0.75 of 0.886 | 0.663 / 0.432 (215) | 0.967 / 0.864 |
| graph over gte K=50 (a,b,c=0,0.02,0) | 50 | held-out NL | 0.634 [0.580, 0.688] | 0.343 [0.301, 0.386] | 0.84 of 0.751 | 0.634 / 0.343 (215) | 0.952 / 0.838 |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | 100 | held-out NL | 0.605 [0.551, 0.659] | 0.343 [0.302, 0.386] | 0.73 of 0.825 | 0.605 / 0.343 (215) | 0.946 / 0.836 |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | 50 | held-out NL | 0.683 [0.631, 0.734] | 0.434 [0.390, 0.479] | 0.81 of 0.839 | 0.683 / 0.434 (215) | 0.929 / 0.776 |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | 100 | held-out NL | 0.689 [0.637, 0.740] | 0.434 [0.390, 0.479] | 0.78 of 0.886 | 0.689 / 0.434 (215) | 0.934 / 0.776 |

#### D9 gates vs jina fp32 at its D6 floors

Δ = variant - baseline, points (MRR absolute). Q1 Δr@10 [lo], Q2 ΔMRR [lo], Q3 worst language, Q4 Δ confident-wrong [up], Q5 Δ false alarm [up] (each arm at its own floors; paired held-out rank-1 positives).

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | - | - | - | - | - | - | - |
| jina int8 (baseline, shipped floors) | +0.5 [-1.0] | -0.003 [-0.012] | go -1.0 | -1.4 [+1.0] | +2.1 [+6.2] | **Q5** | go +12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty -4.0 n=25 |
| gte (stored) | -5.3 [-9.2] | -0.055 [-0.086] | typescript -9.0 | +11.1 [+16.7] | -10.3 [+0.0] | **Q1,Q2,Q4** | go +0.0 n=6, py -9.1 n=11, ru -25.0 n=4, ty -7.1 n=14 |
| identity over gte K=50 | -5.3 [-9.2] | -0.055 [-0.086] | typescript -9.0 | +11.1 [+16.7] | -10.3 [+0.0] | **Q1,Q2,Q4** | go +0.0 n=6, py -9.1 n=11, ru -25.0 n=4, ty -7.1 n=14 |
| identity over gte K=100 | -5.3 [-9.2] | -0.055 [-0.086] | typescript -9.0 | +11.1 [+16.7] | -10.3 [+0.0] | **Q1,Q2,Q4** | go +0.0 n=6, py -9.1 n=11, ru -25.0 n=4, ty -7.1 n=14 |
| shuffled over gte K=50 | -44.8 [-49.2] | -0.356 [-0.390] | typescript -57.0 | +42.6 [+48.2] | -100.0 [-100.0] | **Q1,Q2,Q3,Q4** | go -100.0 n=1 |
| identity over int8 K=50 | +0.5 [-1.0] | -0.003 [-0.012] | go -1.0 | -1.4 [+1.0] | +2.1 [+6.2] | **Q5** | go +12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty -4.0 n=25 |
| identity over int8 K=100 | +0.5 [-1.0] | -0.003 [-0.012] | go -1.0 | -1.4 [+1.0] | +2.1 [+6.2] | **Q5** | go +12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty -4.0 n=25 |
| shuffled over int8 K=50 | -45.0 [-49.7] | -0.337 [-0.374] | typescript -57.0 | +39.1 [+45.0] | - | **Q1,Q2,Q3,Q4,Q5** |  |
| ce-minilm over gte K=50 | -10.2 [-14.5] | -0.078 [-0.114] | typescript -19.0 | -30.8 [-24.8] | +31.7 [+50.0] | **Q1,Q2,Q3,Q5** | go +66.7 n=3, py +10.0 n=10, ru +20.0 n=5, ty +30.0 n=10 |
| ce-minilm+gte over gte K=50 (beta=320) | -2.2 [-8.2] | -0.059 [-0.105] | typescript -8.3 | -7.2 [-1.9] | +4.1 [+18.9] | **Q1,Q2,Q5** | go +25.0 n=4, py +7.1 n=14, ru -25.0 n=4, ty +9.1 n=11 |
| ce-minilm over gte K=100 | -11.5 [-16.0] | -0.088 [-0.124] | typescript -21.0 | -30.7 [-24.9] | +27.2 [+43.6] | **Q1,Q2,Q3,Q5** | go +66.7 n=3, py +20.0 n=10, ru +0.0 n=5, ty +22.2 n=9 |
| ce-minilm+gte over gte K=100 (beta=320) | -2.7 [-8.6] | -0.058 [-0.105] | typescript -8.3 | -7.2 [-1.9] | +4.1 [+18.9] | **Q1,Q2,Q5** | go +25.0 n=4, py +7.1 n=14, ru -25.0 n=4, ty +9.1 n=11 |
| ce-minilm over int8 K=50 | -4.5 [-8.5] | -0.057 [-0.090] | typescript -17.0 | -24.3 [-18.5] | +21.2 [+37.9] | **Q1,Q2,Q3,Q5** | go +50.0 n=4, py +18.2 n=11, ru +16.7 n=6, ty +0.0 n=9 |
| ce-minilm+int8 over int8 K=50 (beta=40) | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | -22.2 [-17.3] | +5.7 [+12.1] | **Q5** | go +0.0 n=12, py +0.0 n=18, ru +0.0 n=6, ty +22.7 n=22 |
| ce-minilm over int8 K=100 | -9.0 [-13.2] | -0.067 [-0.101] | typescript -21.0 | -26.5 [-20.7] | +26.8 [+44.4] | **Q1,Q2,Q3,Q5** | go +50.0 n=4, py +18.2 n=11, ru +16.7 n=6, ty +22.2 n=9 |
| ce-minilm+int8 over int8 K=100 (beta=40) | +6.0 [+1.9] | +0.051 [+0.018] | typescript -3.3 | -21.8 [-16.9] | +4.2 [+10.6] | **Q5** | go +0.0 n=12, py -5.9 n=17, ru +0.0 n=6, ty +22.7 n=22 |
| ce-jina-tiny over gte K=50 | -9.0 [-13.5] | -0.085 [-0.121] | typescript -16.0 | -7.1 [-1.3] | +2.9 [+19.2] | **Q1,Q2,Q3,Q5** | go +20.0 n=5, py -25.0 n=8, ru +0.0 n=4, ty +16.7 n=12 |
| ce-jina-tiny+gte over gte K=50 (beta=80) | -1.1 [-6.8] | -0.065 [-0.111] | typescript -5.0 | -6.9 [-1.6] | -4.3 [+6.1] | **Q1,Q2,Q5** | go +0.0 n=5, py +0.0 n=12, ru -25.0 n=4, ty +7.7 n=13 |
| ce-jina-tiny over gte K=100 | -11.0 [-15.5] | -0.092 [-0.128] | python -20.0 | -5.7 [+0.0] | +2.9 [+19.2] | **Q1,Q2,Q3,Q5** | go +20.0 n=5, py -25.0 n=8, ru +0.0 n=3, ty +16.7 n=12 |
| ce-jina-tiny+gte over gte K=100 (beta=80) | -1.1 [-6.8] | -0.064 [-0.110] | typescript -5.0 | -6.9 [-1.6] | -4.3 [+6.1] | **Q1,Q2,Q5** | go +0.0 n=5, py +0.0 n=12, ru -25.0 n=4, ty +7.7 n=13 |
| ce-jina-tiny over int8 K=50 | -5.0 [-9.0] | -0.067 [-0.100] | typescript -16.0 | -9.8 [-4.0] | +0.7 [+16.4] | **Q1,Q2,Q3,Q5** | go +20.0 n=5, py -25.0 n=8, ru +0.0 n=5, ty +7.7 n=13 |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | +3.8 [-0.7] | +0.010 [-0.029] | python -2.1 | -18.2 [-13.3] | +6.7 [+15.7] | **Q5** | go -10.0 n=10, py +13.3 n=15, ru +0.0 n=5, ty +23.5 n=17 |
| ce-jina-tiny over int8 K=100 | -7.3 [-11.5] | -0.073 [-0.109] | typescript -17.0 | -9.0 [-3.0] | +2.9 [+19.0] | **Q1,Q2,Q3,Q5** | go +20.0 n=5, py -25.0 n=8, ru +0.0 n=5, ty +16.7 n=12 |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | +2.9 [-1.5] | +0.009 [-0.030] | python -2.1 | -15.7 [-10.8] | +6.7 [+15.7] | **Q5** | go -10.0 n=10, py +13.3 n=15, ru +0.0 n=5, ty +23.5 n=17 |
| graph over gte K=50 (a,b,c=0,0.02,0) | +0.1 [-5.8] | -0.080 [-0.129] | python -4.3 | +17.2 [+22.5] | -12.8 [-1.9] | **Q1,Q2,Q4** | go +0.0 n=4, py -7.7 n=13, ru -33.3 n=3, ty -10.0 n=10 |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | -2.8 [-9.0] | -0.079 [-0.128] | python -6.4 | +15.3 [+20.5] | -12.7 [-2.1] | **Q1,Q2,Q4** | go +0.0 n=4, py -8.3 n=12, ru -33.3 n=3, ty -9.1 n=11 |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | +5.0 [+2.0] | +0.011 [-0.013] | go +1.6 | +4.9 [+8.4] | -2.4 [+0.0] | **Q4** | go +0.0 n=10, py -5.6 n=18, ru +0.0 n=6, ty -4.0 n=25 |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | +5.6 [+2.4] | +0.011 [-0.016] | go +0.0 | +7.0 [+10.9] | -1.4 [+1.9] | **Q4** | go +0.0 n=10, py -5.6 n=18, ru +0.0 n=5, ty +0.0 n=23 |

#### D9 gates vs shipped jina int8 at its shipped floors

Δ = variant - baseline, points (MRR absolute). Q1 Δr@10 [lo], Q2 ΔMRR [lo], Q3 worst language, Q4 Δ confident-wrong [up], Q5 Δ false alarm [up] (each arm at its own floors; paired held-out rank-1 positives).

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | -0.5 [-2.0] | +0.003 [-0.005] | python -2.0 | +1.4 [+3.8] | -2.1 [+1.4] | **Q4** | go -12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty +4.0 n=25 |
| jina int8 (baseline, shipped floors) | - | - | - | - | - | - | - |
| gte (stored) | -5.8 [-9.5] | -0.051 [-0.083] | python -8.0 | +12.5 [+17.8] | -5.7 [+1.8] | **Q1,Q2,Q4** | go +0.0 n=7, py -10.0 n=10, ru -20.0 n=5, ty +7.1 n=14 |
| identity over gte K=50 | -5.8 [-9.5] | -0.051 [-0.083] | python -8.0 | +12.5 [+17.8] | -5.7 [+1.8] | **Q1,Q2,Q4** | go +0.0 n=7, py -10.0 n=10, ru -20.0 n=5, ty +7.1 n=14 |
| identity over gte K=100 | -5.8 [-9.5] | -0.051 [-0.083] | python -8.0 | +12.5 [+17.8] | -5.7 [+1.8] | **Q1,Q2,Q4** | go +0.0 n=7, py -10.0 n=10, ru -20.0 n=5, ty +7.1 n=14 |
| shuffled over gte K=50 | -45.2 [-49.8] | -0.352 [-0.386] | python -57.0 | +44.0 [+49.6] | -100.0 [-100.0] | **Q1,Q2,Q3,Q4** | go -100.0 n=1 |
| identity over int8 K=50 | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=17, py +0.0 n=20, ru +0.0 n=7, ty +0.0 n=26 |
| identity over int8 K=100 | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=17, py +0.0 n=20, ru +0.0 n=7, ty +0.0 n=26 |
| shuffled over int8 K=50 | -45.5 [-50.2] | -0.334 [-0.370] | python -56.0 | +40.5 [+46.4] | - | **Q1,Q2,Q3,Q4,Q5** |  |
| ce-minilm over gte K=50 | -10.8 [-15.0] | -0.075 [-0.110] | typescript -18.0 | -29.4 [-23.5] | +33.6 [+51.1] | **Q1,Q2,Q3,Q5** | go +66.7 n=3, py +11.1 n=9, ru +16.7 n=6, ty +40.0 n=10 |
| ce-minilm+gte over gte K=50 (beta=320) | -3.4 [-9.0] | -0.055 [-0.100] | python -8.5 | -5.8 [-0.7] | +6.5 [+19.2] | **Q1,Q2,Q5** | go +20.0 n=5, py +7.7 n=13, ru -20.0 n=5, ty +18.2 n=11 |
| ce-minilm over gte K=100 | -12.0 [-16.5] | -0.084 [-0.121] | typescript -20.0 | -29.3 [-23.5] | +30.6 [+47.2] | **Q1,Q2,Q3,Q5** | go +66.7 n=3, py +22.2 n=9, ru +0.0 n=6, ty +33.3 n=9 |
| ce-minilm+gte over gte K=100 (beta=320) | -3.9 [-9.5] | -0.054 [-0.099] | python -8.5 | -5.8 [-0.7] | +6.5 [+19.2] | **Q1,Q2,Q5** | go +20.0 n=5, py +7.7 n=13, ru -20.0 n=5, ty +18.2 n=11 |
| ce-minilm over int8 K=50 | -5.0 [-8.8] | -0.054 [-0.086] | typescript -16.0 | -22.9 [-17.3] | +26.3 [+41.3] | **Q1,Q2,Q3,Q5** | go +60.0 n=5, py +20.0 n=10, ru +14.3 n=7, ty +11.1 n=9 |
| ce-minilm+int8 over int8 K=50 (beta=40) | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | -20.8 [-16.2] | +6.5 [+12.7] | **Q5** | go +0.0 n=13, py +0.0 n=17, ru +0.0 n=7, ty +26.1 n=23 |
| ce-minilm over int8 K=100 | -9.5 [-13.5] | -0.064 [-0.098] | typescript -20.0 | -25.1 [-19.3] | +31.9 [+47.5] | **Q1,Q2,Q3,Q5** | go +60.0 n=5, py +20.0 n=10, ru +14.3 n=7, ty +33.3 n=9 |
| ce-minilm+int8 over int8 K=100 (beta=40) | +4.8 [+1.2] | +0.056 [+0.024] | typescript -3.3 | -20.4 [-15.8] | +5.0 [+10.7] | **Q5** | go +0.0 n=13, py -6.2 n=16, ru +0.0 n=7, ty +26.1 n=23 |
| ce-jina-tiny over gte K=50 | -9.5 [-13.8] | -0.082 [-0.117] | python -17.0 | -5.7 [-0.1] | -0.9 [+13.9] | **Q1,Q2,Q3,Q5** | go +0.0 n=5, py -28.6 n=7, ru +0.0 n=5, ty +25.0 n=12 |
| ce-jina-tiny+gte over gte K=50 (beta=80) | -2.3 [-7.7] | -0.061 [-0.105] | typescript -5.0 | -5.5 [-0.3] | -1.2 [+8.0] | **Q1,Q2,Q5** | go +0.0 n=6, py +0.0 n=11, ru -20.0 n=5, ty +15.4 n=13 |
| ce-jina-tiny over gte K=100 | -11.5 [-15.8] | -0.089 [-0.124] | python -22.0 | -4.3 [+1.3] | -0.9 [+13.9] | **Q1,Q2,Q3,Q5** | go +0.0 n=5, py -28.6 n=7, ru +0.0 n=4, ty +25.0 n=12 |
| ce-jina-tiny+gte over gte K=100 (beta=80) | -2.3 [-7.7] | -0.060 [-0.105] | typescript -5.0 | -5.5 [-0.3] | -1.2 [+8.0] | **Q1,Q2,Q5** | go +0.0 n=6, py +0.0 n=11, ru -20.0 n=5, ty +15.4 n=13 |
| ce-jina-tiny over int8 K=50 | -5.5 [-9.5] | -0.064 [-0.096] | typescript -15.0 | -8.4 [-2.9] | -3.3 [+10.8] | **Q1,Q2,Q3,Q5** | go +0.0 n=5, py -28.6 n=7, ru +0.0 n=6, ty +15.4 n=13 |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | +2.6 [-1.4] | +0.014 [-0.023] | python -4.3 | -16.8 [-12.1] | +7.4 [+17.3] | **Q5** | go -18.2 n=11, py +14.3 n=14, ru +0.0 n=6, ty +33.3 n=18 |
| ce-jina-tiny over int8 K=100 | -7.8 [-11.8] | -0.070 [-0.104] | typescript -16.0 | -7.6 [-1.8] | -0.9 [+13.3] | **Q1,Q2,Q3,Q5** | go +0.0 n=5, py -28.6 n=7, ru +0.0 n=6, ty +25.0 n=12 |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | +1.8 [-2.2] | +0.013 [-0.024] | python -4.3 | -14.3 [-9.6] | +7.4 [+17.3] | **Q5** | go -18.2 n=11, py +14.3 n=14, ru +0.0 n=6, ty +33.3 n=18 |
| graph over gte K=50 (a,b,c=0,0.02,0) | -1.1 [-6.5] | -0.076 [-0.123] | python -6.4 | +18.6 [+23.7] | -10.4 [+0.0] | **Q1,Q2,Q4** | go +0.0 n=5, py -8.3 n=12, ru -33.3 n=3, ty +0.0 n=11 |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | -4.0 [-9.6] | -0.075 [-0.122] | python -8.5 | +16.7 [+21.7] | -8.5 [+0.0] | **Q1,Q2,Q4** | go +0.0 n=5, py -9.1 n=11, ru -25.0 n=4, ty +0.0 n=12 |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | +3.8 [+1.4] | +0.015 [-0.006] | python +0.0 | +6.3 [+9.7] | -3.9 [+0.0] | **Q4** | go -10.0 n=10, py -5.6 n=18, ru +0.0 n=6, ty +0.0 n=25 |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | +4.4 [+1.6] | +0.015 [-0.009] | python +0.0 | +8.4 [+12.2] | -2.8 [+1.1] | **Q4** | go -10.0 n=10, py -5.6 n=18, ru +0.0 n=5, ty +4.3 n=23 |

#### GM-434 columns (option a: below the floor is noMatch), at each row's own floors

misled = rank-1 positives the floor calls no-match; CW = clears the floor with a wrong top (positives) / at all (absent). NL = held-out NL queries; name = mechanical name queries.

| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | 0.56 / 0.58 / 0.56 / 0.55 | 14% (10/71) | 53% (113/215) | 26% (12/46) | 1% (7/569) | 32% (283/879) | 20% (183/900) |
| jina int8 (baseline, shipped floors) | 0.57 / 0.57 / 0.55 / 0.53 | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% (9/577) | 32% (282/879) | 23% (204/900) |
| gte (stored) | 0.86 / 0.86 / 0.85 / 0.84 | 9% (5/54) | 62% (133/215) | 50% (23/46) | 1% (7/657) | 23% (205/879) | 29% (262/900) |
| identity over gte K=50 | 0.86 / 0.86 / 0.85 / 0.84 | 9% (5/54) | 62% (133/215) | 50% (23/46) | 1% (7/657) | 23% (205/879) | 29% (262/900) |
| identity over gte K=100 | 0.86 / 0.86 / 0.85 / 0.84 | 9% (5/54) | 62% (133/215) | 50% (23/46) | 1% (7/657) | 23% (205/879) | 29% (262/900) |
| shuffled over gte K=50 | 0.96 / 0.88 / 0.94 / 0.94 | 0% (0/2) | 90% (193/215) | 96% (44/46) | 0% (0/16) | 94% (826/879) | 95% (857/900) |
| identity over int8 K=50 | 0.57 / 0.57 / 0.55 / 0.53 | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% (9/577) | 32% (282/879) | 23% (204/900) |
| identity over int8 K=100 | 0.57 / 0.57 / 0.55 / 0.53 | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% (9/577) | 32% (282/879) | 23% (204/900) |
| shuffled over int8 K=50 | 0.96 / 0.88 / 0.96 / 0.94 | 20% (1/5) | 87% (186/215) | 93% (43/46) | 0% (0/16) | 91% (797/879) | 93% (835/900) |
| ce-minilm over gte K=50 | 2.36 / -0.14 / 1.17 / 1.05 | 44% (24/55) | 20% (42/215) | 9% (4/46) | 1% (9/711) | 17% (151/879) | 3% (29/900) |
| ce-minilm+gte over gte K=50 (beta=320) | 279.29 / 273.71 / 273.39 / 269.27 | 18% (9/51) | 47% (101/215) | 17% (8/46) | 2% (12/699) | 19% (164/879) | 14% (122/900) |
| ce-minilm over gte K=100 | 2.61 / 0.55 / 1.02 / 1.05 | 42% (22/53) | 20% (42/215) | 9% (4/46) | 1% (10/711) | 17% (152/879) | 3% (28/900) |
| ce-minilm+gte over gte K=100 (beta=320) | 279.29 / 273.71 / 273.39 / 269.27 | 18% (9/51) | 47% (101/215) | 17% (8/46) | 2% (12/699) | 19% (164/879) | 14% (122/900) |
| ce-minilm over int8 K=50 | -2.10 / -0.14 / 0.97 / -0.09 | 34% (19/56) | 29% (63/215) | 7% (3/46) | 1% (8/720) | 16% (137/879) | 6% (50/900) |
| ce-minilm+int8 over int8 K=50 (beta=40) | 19.48 / 17.02 / 24.91 / 17.10 | 25% (20/80) | 31% (66/215) | 9% (4/46) | 1% (10/724) | 14% (127/879) | 9% (81/900) |
| ce-minilm over int8 K=100 | -1.05 / 0.07 / 0.87 / 1.05 | 39% (22/56) | 27% (57/215) | 4% (2/46) | 1% (9/717) | 17% (147/879) | 4% (32/900) |
| ce-minilm+int8 over int8 K=100 (beta=40) | 19.48 / 16.72 / 24.55 / 17.10 | 24% (19/79) | 31% (67/215) | 9% (4/46) | 2% (11/728) | 15% (128/879) | 9% (84/900) |
| ce-jina-tiny over gte K=50 | 0.60 / 0.58 / 0.48 / 1.19 | 19% (10/53) | 44% (95/215) | 24% (11/46) | 2% (12/630) | 26% (227/879) | 11% (98/900) |
| ce-jina-tiny+gte over gte K=50 (beta=80) | 69.20 / 70.16 / 69.71 / 69.15 | 17% (9/52) | 47% (102/215) | 20% (9/46) | 2% (12/685) | 19% (165/879) | 11% (98/900) |
| ce-jina-tiny over gte K=100 | 0.60 / 0.58 / 0.52 / 1.19 | 18% (9/51) | 45% (97/215) | 28% (13/46) | 2% (12/629) | 26% (227/879) | 11% (99/900) |
| ce-jina-tiny+gte over gte K=100 (beta=80) | 69.20 / 70.16 / 69.71 / 69.15 | 17% (9/52) | 47% (102/215) | 20% (9/46) | 2% (12/685) | 19% (165/879) | 11% (98/900) |
| ce-jina-tiny over int8 K=50 | 0.57 / 0.61 / 0.52 / 0.82 | 16% (9/55) | 41% (88/215) | 26% (12/46) | 2% (10/642) | 23% (205/879) | 10% (86/900) |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | 2.53 / 3.63 / 3.78 / 3.82 | 23% (16/71) | 33% (72/215) | 17% (8/46) | 1% (9/689) | 17% (153/879) | 5% (47/900) |
| ce-jina-tiny over int8 K=100 | 0.57 / 0.54 / 0.68 / 1.07 | 20% (11/54) | 42% (90/215) | 26% (12/46) | 2% (11/637) | 24% (209/879) | 11% (96/900) |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | 2.53 / 3.63 / 3.66 / 3.82 | 23% (16/71) | 36% (78/215) | 17% (8/46) | 1% (10/692) | 18% (154/879) | 6% (50/900) |
| graph over gte K=50 (a,b,c=0,0.02,0) | 0.86 / 0.86 / 0.86 / 0.87 | 2% (1/46) | 70% (150/215) | 46% (21/46) | 1% (8/655) | 23% (202/879) | 24% (216/900) |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | 0.86 / 0.85 / 0.87 / 0.87 | 6% (3/48) | 68% (146/215) | 43% (20/46) | 1% (9/652) | 22% (192/879) | 21% (189/900) |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | 0.59 / 0.57 / 0.58 / 0.56 | 9% (6/68) | 56% (121/215) | 37% (17/46) | 1% (8/600) | 29% (259/879) | 22% (195/900) |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | 0.63 / 0.60 / 0.60 / 0.59 | 12% (8/67) | 59% (126/215) | 39% (18/46) | 2% (9/596) | 30% (260/879) | 19% (175/900) |

#### GM-434 per language, held-out NL (misled / CW pos / CW absent)

| variant | go | python | rust | typescript |
|---|---|---|---|---|
| jina fp32 (baseline) | 19% (3/16) / 51% (31/61) / 9% (1/11) | 10% (2/21) / 45% (21/47) / 36% (4/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 15% (4/27) / 42% (25/60) / 15% (2/13) |
| jina int8 (baseline, shipped floors) | 29% (5/17) / 41% (25/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 8% (2/26) / 47% (28/60) / 15% (2/13) |
| gte (stored) | 25% (3/12) / 61% (37/61) / 18% (2/11) | 8% (1/13) / 53% (25/47) / 18% (2/11) | 0% (0/9) / 74% (35/47) / 82% (9/11) | 5% (1/20) / 60% (36/60) / 77% (10/13) |
| identity over gte K=50 | 25% (3/12) / 61% (37/61) / 18% (2/11) | 8% (1/13) / 53% (25/47) / 18% (2/11) | 0% (0/9) / 74% (35/47) / 82% (9/11) | 5% (1/20) / 60% (36/60) / 77% (10/13) |
| identity over gte K=100 | 25% (3/12) / 61% (37/61) / 18% (2/11) | 8% (1/13) / 53% (25/47) / 18% (2/11) | 0% (0/9) / 74% (35/47) / 82% (9/11) | 5% (1/20) / 60% (36/60) / 77% (10/13) |
| shuffled over gte K=50 | 0% (0/2) / 74% (45/61) / 91% (10/11) | - / 100% (47/47) / 91% (10/11) | - / 96% (45/47) / 100% (11/11) | - / 93% (56/60) / 100% (13/13) |
| identity over int8 K=50 | 29% (5/17) / 41% (25/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 8% (2/26) / 47% (28/60) / 15% (2/13) |
| identity over int8 K=100 | 29% (5/17) / 41% (25/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 8% (2/26) / 47% (28/60) / 15% (2/13) |
| shuffled over int8 K=50 | 100% (1/1) / 77% (47/61) / 91% (10/11) | - / 100% (47/47) / 91% (10/11) | 0% (0/3) / 79% (37/47) / 91% (10/11) | 0% (0/1) / 92% (55/60) / 100% (13/13) |
| ce-minilm over gte K=50 | 50% (5/10) / 18% (11/61) / 0% (0/11) | 36% (5/14) / 21% (10/47) / 0% (0/11) | 42% (5/12) / 26% (12/47) / 27% (3/11) | 47% (9/19) / 15% (9/60) / 8% (1/13) |
| ce-minilm+gte over gte K=50 (beta=320) | 33% (3/9) / 41% (25/61) / 0% (0/11) | 25% (4/16) / 30% (14/47) / 0% (0/11) | 0% (0/10) / 62% (29/47) / 36% (4/11) | 12% (2/16) / 55% (33/60) / 31% (4/13) |
| ce-minilm over gte K=100 | 50% (5/10) / 16% (10/61) / 0% (0/11) | 43% (6/14) / 19% (9/47) / 0% (0/11) | 27% (3/11) / 30% (14/47) / 27% (3/11) | 44% (8/18) / 15% (9/60) / 8% (1/13) |
| ce-minilm+gte over gte K=100 (beta=320) | 33% (3/9) / 41% (25/61) / 0% (0/11) | 25% (4/16) / 30% (14/47) / 0% (0/11) | 0% (0/10) / 62% (29/47) / 36% (4/11) | 12% (2/16) / 55% (33/60) / 31% (4/13) |
| ce-minilm over int8 K=50 | 36% (4/11) / 48% (29/61) / 0% (0/11) | 40% (6/15) / 21% (10/47) / 0% (0/11) | 36% (5/14) / 19% (9/47) / 9% (1/11) | 25% (4/16) / 25% (15/60) / 15% (2/13) |
| ce-minilm+int8 over int8 K=50 (beta=40) | 22% (4/18) / 39% (24/61) / 0% (0/11) | 10% (2/20) / 36% (17/47) / 9% (1/11) | 38% (5/13) / 19% (9/47) / 9% (1/11) | 31% (9/29) / 27% (16/60) / 15% (2/13) |
| ce-minilm over int8 K=100 | 45% (5/11) / 44% (27/61) / 0% (0/11) | 40% (6/15) / 21% (10/47) / 0% (0/11) | 23% (3/13) / 23% (11/47) / 9% (1/11) | 47% (8/17) / 15% (9/60) / 8% (1/13) |
| ce-minilm+int8 over int8 K=100 (beta=40) | 22% (4/18) / 39% (24/61) / 0% (0/11) | 5% (1/19) / 36% (17/47) / 9% (1/11) | 38% (5/13) / 21% (10/47) / 9% (1/11) | 31% (9/29) / 27% (16/60) / 15% (2/13) |
| ce-jina-tiny over gte K=50 | 29% (4/14) / 49% (30/61) / 18% (2/11) | 10% (1/10) / 45% (21/47) / 18% (2/11) | 10% (1/10) / 66% (31/47) / 45% (5/11) | 21% (4/19) / 22% (13/60) / 15% (2/13) |
| ce-jina-tiny+gte over gte K=50 (beta=80) | 33% (4/12) / 59% (36/61) / 18% (2/11) | 21% (3/14) / 23% (11/47) / 9% (1/11) | 0% (0/9) / 57% (27/47) / 27% (3/11) | 12% (2/17) / 47% (28/60) / 23% (3/13) |
| ce-jina-tiny over gte K=100 | 23% (3/13) / 52% (32/61) / 18% (2/11) | 10% (1/10) / 45% (21/47) / 18% (2/11) | 11% (1/9) / 66% (31/47) / 55% (6/11) | 21% (4/19) / 22% (13/60) / 23% (3/13) |
| ce-jina-tiny+gte over gte K=100 (beta=80) | 33% (4/12) / 59% (36/61) / 18% (2/11) | 21% (3/14) / 23% (11/47) / 9% (1/11) | 0% (0/9) / 57% (27/47) / 27% (3/11) | 12% (2/17) / 47% (28/60) / 23% (3/13) |
| ce-jina-tiny over int8 K=50 | 29% (4/14) / 46% (28/61) / 18% (2/11) | 11% (1/9) / 43% (20/47) / 18% (2/11) | 8% (1/12) / 55% (26/47) / 36% (4/11) | 15% (3/20) / 23% (14/60) / 31% (4/13) |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | 6% (1/18) / 52% (32/61) / 18% (2/11) | 27% (4/15) / 28% (13/47) / 9% (1/11) | 17% (2/12) / 36% (17/47) / 27% (3/11) | 35% (9/26) / 17% (10/60) / 15% (2/13) |
| ce-jina-tiny over int8 K=100 | 29% (4/14) / 49% (30/61) / 27% (3/11) | 11% (1/9) / 47% (22/47) / 18% (2/11) | 8% (1/12) / 53% (25/47) / 36% (4/11) | 26% (5/19) / 22% (13/60) / 23% (3/13) |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | 6% (1/18) / 54% (33/61) / 18% (2/11) | 27% (4/15) / 30% (14/47) / 9% (1/11) | 17% (2/12) / 45% (21/47) / 27% (3/11) | 35% (9/26) / 17% (10/60) / 15% (2/13) |
| graph over gte K=50 (a,b,c=0,0.02,0) | 0% (0/10) / 72% (44/61) / 27% (3/11) | 6% (1/16) / 57% (27/47) / 55% (6/11) | 0% (0/7) / 83% (39/47) / 82% (9/11) | 0% (0/13) / 67% (40/60) / 23% (3/13) |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | 10% (1/10) / 72% (44/61) / 18% (2/11) | 0% (0/15) / 60% (28/47) / 64% (7/11) | 12% (1/8) / 77% (36/47) / 73% (8/11) | 7% (1/15) / 63% (38/60) / 23% (3/13) |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | 8% (1/13) / 56% (34/61) / 18% (2/11) | 0% (0/18) / 53% (25/47) / 45% (5/11) | 33% (3/9) / 74% (35/47) / 55% (6/11) | 7% (2/28) / 45% (27/60) / 31% (4/13) |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | 14% (2/14) / 62% (38/61) / 27% (3/11) | 0% (0/18) / 51% (24/47) / 45% (5/11) | 25% (2/8) / 77% (36/47) / 55% (6/11) | 15% (4/27) / 47% (28/60) / 31% (4/13) |

#### GM-434 per language, name (misled / CW pos / CW absent)

| variant | go | python | rust | typescript |
|---|---|---|---|---|
| jina fp32 (baseline) | 1% (1/86) / 40% (60/150) / 11% (17/150) | 1% (1/103) / 28% (42/150) / 13% (19/150) | 1% (2/149) / 47% (142/300) / 28% (84/300) | 1% (3/231) / 14% (39/279) / 21% (63/300) |
| jina int8 (baseline, shipped floors) | 1% (1/87) / 39% (58/150) / 12% (18/150) | 1% (1/102) / 28% (42/150) / 13% (20/150) | 3% (4/153) / 47% (142/300) / 32% (96/300) | 1% (3/235) / 14% (40/279) / 23% (70/300) |
| gte (stored) | 1% (1/141) / 6% (9/150) / 3% (5/150) | 2% (2/117) / 19% (29/150) / 18% (27/150) | 2% (3/170) / 41% (123/300) / 35% (105/300) | 0% (1/229) / 16% (44/279) / 42% (125/300) |
| identity over gte K=50 | 1% (1/141) / 6% (9/150) / 3% (5/150) | 2% (2/117) / 19% (29/150) / 18% (27/150) | 2% (3/170) / 41% (123/300) / 35% (105/300) | 0% (1/229) / 16% (44/279) / 42% (125/300) |
| identity over gte K=100 | 1% (1/141) / 6% (9/150) / 3% (5/150) | 2% (2/117) / 19% (29/150) / 18% (27/150) | 2% (3/170) / 41% (123/300) / 35% (105/300) | 0% (1/229) / 16% (44/279) / 42% (125/300) |
| shuffled over gte K=50 | 0% (0/5) / 88% (132/150) / 85% (128/150) | 0% (0/2) / 99% (148/150) / 100% (150/150) | 0% (0/8) / 94% (283/300) / 97% (290/300) | 0% (0/1) / 94% (263/279) / 96% (289/300) |
| identity over int8 K=50 | 1% (1/87) / 39% (58/150) / 12% (18/150) | 1% (1/102) / 28% (42/150) / 13% (20/150) | 3% (4/153) / 47% (142/300) / 32% (96/300) | 1% (3/235) / 14% (40/279) / 23% (70/300) |
| identity over int8 K=100 | 1% (1/87) / 39% (58/150) / 12% (18/150) | 1% (1/102) / 28% (42/150) / 13% (20/150) | 3% (4/153) / 47% (142/300) / 32% (96/300) | 1% (3/235) / 14% (40/279) / 23% (70/300) |
| shuffled over int8 K=50 | 0% (0/5) / 88% (132/150) / 85% (128/150) | 0% (0/2) / 99% (148/150) / 100% (150/150) | 0% (0/7) / 85% (255/300) / 89% (268/300) | 0% (0/2) / 94% (262/279) / 96% (289/300) |
| ce-minilm over gte K=50 | 0% (0/139) / 6% (9/150) / 1% (2/150) | 0% (0/133) / 11% (16/150) / 3% (4/150) | 3% (6/195) / 32% (95/300) / 5% (15/300) | 1% (3/244) / 11% (31/279) / 3% (8/300) |
| ce-minilm+gte over gte K=50 (beta=320) | 1% (2/146) / 3% (4/150) / 1% (1/150) | 2% (2/124) / 16% (24/150) / 10% (15/150) | 3% (5/189) / 33% (100/300) / 13% (39/300) | 1% (3/240) / 13% (36/279) / 22% (67/300) |
| ce-minilm over gte K=100 | 1% (1/138) / 7% (10/150) / 1% (2/150) | 0% (0/133) / 10% (15/150) / 1% (2/150) | 3% (6/195) / 32% (96/300) / 5% (16/300) | 1% (3/245) / 11% (31/279) / 3% (8/300) |
| ce-minilm+gte over gte K=100 (beta=320) | 1% (2/146) / 3% (4/150) / 1% (1/150) | 2% (2/124) / 16% (24/150) / 10% (15/150) | 3% (5/189) / 33% (100/300) / 13% (39/300) | 1% (3/240) / 13% (36/279) / 22% (67/300) |
| ce-minilm over int8 K=50 | 0% (0/138) / 8% (12/150) / 6% (9/150) | 0% (0/134) / 10% (15/150) / 2% (3/150) | 3% (6/202) / 26% (79/300) / 6% (17/300) | 1% (2/246) / 11% (31/279) / 7% (21/300) |
| ce-minilm+int8 over int8 K=50 (beta=40) | 1% (1/140) / 7% (10/150) / 2% (3/150) | 1% (1/125) / 15% (23/150) / 10% (15/150) | 3% (6/204) / 24% (73/300) / 7% (21/300) | 1% (2/255) / 8% (21/279) / 14% (42/300) |
| ce-minilm over int8 K=100 | 0% (0/138) / 8% (12/150) / 3% (5/150) | 0% (0/135) / 10% (15/150) / 2% (3/150) | 3% (6/198) / 30% (89/300) / 5% (16/300) | 1% (3/246) / 11% (31/279) / 3% (8/300) |
| ce-minilm+int8 over int8 K=100 (beta=40) | 1% (1/140) / 7% (10/150) / 2% (3/150) | 2% (2/126) / 15% (23/150) / 12% (18/150) | 3% (6/207) / 25% (74/300) / 7% (21/300) | 1% (2/255) / 8% (21/279) / 14% (42/300) |
| ce-jina-tiny over gte K=50 | 2% (3/122) / 18% (27/150) / 3% (5/150) | 1% (1/110) / 23% (35/150) / 13% (19/150) | 3% (5/168) / 41% (123/300) / 21% (64/300) | 1% (3/230) / 15% (42/279) / 3% (10/300) |
| ce-jina-tiny+gte over gte K=50 (beta=80) | 1% (2/142) / 5% (8/150) / 1% (2/150) | 2% (2/122) / 15% (22/150) / 7% (11/150) | 3% (5/182) / 34% (101/300) / 13% (38/300) | 1% (3/239) / 12% (34/279) / 16% (47/300) |
| ce-jina-tiny over gte K=100 | 2% (3/122) / 18% (27/150) / 3% (5/150) | 1% (1/109) / 24% (36/150) / 14% (21/150) | 3% (5/168) / 41% (122/300) / 21% (62/300) | 1% (3/230) / 15% (42/279) / 4% (11/300) |
| ce-jina-tiny+gte over gte K=100 (beta=80) | 1% (2/142) / 5% (8/150) / 1% (2/150) | 2% (2/122) / 15% (22/150) / 7% (11/150) | 3% (5/182) / 34% (101/300) / 13% (38/300) | 1% (3/239) / 12% (34/279) / 16% (47/300) |
| ce-jina-tiny over int8 K=50 | 2% (2/120) / 19% (29/150) / 3% (4/150) | 1% (1/109) / 23% (35/150) / 9% (14/150) | 3% (5/182) / 32% (96/300) / 18% (53/300) | 1% (2/231) / 16% (45/279) / 5% (15/300) |
| ce-jina-tiny+int8 over int8 K=50 (beta=5) | 2% (2/126) / 16% (24/150) / 7% (10/150) | 0% (0/122) / 15% (23/150) / 3% (5/150) | 3% (5/194) / 27% (81/300) / 10% (29/300) | 1% (2/247) / 9% (25/279) / 1% (3/300) |
| ce-jina-tiny over int8 K=100 | 2% (2/120) / 19% (29/150) / 3% (5/150) | 2% (2/110) / 24% (36/150) / 13% (20/150) | 3% (5/177) / 34% (102/300) / 19% (57/300) | 1% (2/230) / 15% (42/279) / 5% (14/300) |
| ce-jina-tiny+int8 over int8 K=100 (beta=5) | 2% (2/126) / 16% (24/150) / 7% (10/150) | 0% (0/122) / 15% (23/150) / 3% (5/150) | 3% (6/197) / 27% (82/300) / 11% (32/300) | 1% (2/247) / 9% (25/279) / 1% (3/300) |
| graph over gte K=50 (a,b,c=0,0.02,0) | 0% (0/138) / 8% (12/150) / 19% (29/150) | 1% (1/122) / 16% (24/150) / 21% (32/150) | 2% (3/165) / 42% (126/300) / 30% (91/300) | 2% (4/230) / 14% (40/279) / 21% (64/300) |
| graph over gte K=100 (a,b,c=-0.005,0.02,0) | 1% (1/139) / 7% (11/150) / 12% (18/150) | 1% (1/120) / 20% (30/150) / 34% (51/150) | 2% (3/163) / 38% (114/300) / 19% (58/300) | 2% (4/230) / 13% (37/279) / 21% (62/300) |
| graph over int8 K=50 (a,b,c=0.02,0.02,0) | 1% (1/93) / 34% (51/150) / 13% (19/150) | 1% (1/112) / 23% (35/150) / 18% (27/150) | 2% (3/164) / 43% (129/300) / 28% (83/300) | 1% (3/231) / 16% (44/279) / 22% (66/300) |
| graph over int8 K=100 (a,b,c=0.02,0.04,0.005) | 1% (1/93) / 34% (51/150) / 12% (18/150) | 2% (2/113) / 22% (33/150) / 15% (22/150) | 2% (3/161) / 43% (130/300) / 26% (78/300) | 1% (3/229) / 16% (46/279) / 19% (57/300) |

#### Rerank latency (K = 50, per query, excludes the recall stage)

200 seeded-sampled queries, one warm-up per model, onnxruntime CPU with its
default thread count.

| reranker | over gte p50 / p95 | over int8 p50 / p95 |
|---|---|---|
| ce-minilm | 701 / 2094 ms | 594 / 1838 ms |
| ce-jina-tiny | 673 / 1673 ms | 476 / 1517 ms |
| graph (SQL features) | 5.7 / 44.6 ms | 5.2 / 23.8 ms |

**The machine was not quiet**: load average 22 at start, 31-51 during the
run (8 cores), `/usr/bin/time -p` real 626 s, user 2100 s, sys 26 s, with
`spindump` on top and little other CPU in `ps`, so part of the load was
processes waiting, not computing. These times are superseded by
[S12: latency on an idle machine](#s12-latency-on-an-idle-machine), which
re-measures the carried-forward variant (F4, ce-minilm over int8) at K=50
and K=20; the graph rerank costs single-digit milliseconds in both.

### Reading

- **The cross-encoders alone make things worse.** Both models are
  general-English passage rerankers. Over either arm they lose recall
  against their own recall stage (for example ce-minilm over int8 K=50 has
  NL r@10 0.587 against int8's 0.637), and they fail Q1/Q2/Q3/Q5 against both
  baselines. They do rank name queries better (name r@10 about 0.96 against
  0.92).
- **Over gte, nothing reaches jina.** The best is graph K=50 (held-out r@10
  0.634, level with fp32), but its MRR is -0.080 and it fails Q4. The CE+gte
  blends fail Q1, Q2 and Q5. ce-minilm+gte's beta = 320 sits on the grid's
  edge, so that blend is almost gte alone. S1's question, whether cheap gte
  recall plus a rerank can stand in for jina, gets a no on this data.
- **Over int8, the blend beats both baselines on retrieval.** ce-minilm+int8
  K=50 (beta 40), on the 215 held-out NL queries: r@10 0.693, which is +6.0
  [lower +1.8] vs fp32 and +4.8 [+1.2] vs int8. MRR is +0.054 [+0.020] and
  +0.058 [+0.027]. Both lower bounds are above 0, so it is superior, not just
  non-inferior. Confident-wrong falls sharply: Q4 is -22.2 / -20.8 points,
  NL CW on positives 31% against 52% for int8, CW on absent 9% against 22%.
  **But it fails Q5 against both baselines** (+5.7 [+12.1] and +6.5
  [+12.7]). All of that comes from TypeScript (+22.7 n=22 and +26.1 n=23).
  The GM-434 misled rate rises to 25% (20/80) from int8's 14% (10/70). A
  per-language floor on the blend's logit scale, fitted on the FIT half, does
  not transfer to TypeScript. The per-language n is small (5-23 paired
  queries).
- **ce-jina-tiny+int8** is weaker: +3.8 [-0.7] r@10 vs fp32, +0.010 MRR.
  It fails Q5 too.
- **Graph over int8** gains recall with a cost in confidence. It gets +5.0
  [+2.0] r@10 vs fp32 and +3.8 [+1.4] vs int8, MRR is flat, and Q5 passes,
  but it fails Q4 (+4.9 [+8.4] and +6.3 [+9.7]). The extra rows it pulls up
  clear the floor while being wrong.
- **K=100 never beats K=50** on the gated numbers. The extra ceiling (0.839
  to 0.886 over int8) is not converted.
- Caveats: the tuned variants are judged on the held-out half only (215 NL
  positives; the per-language NL held-out n is 47-61 positives and 11-13
  absent). The beta and graph grids are coarse. Cross-encoder scores are
  cached per (model, snapshot, node ids) pair in
  `<work>/rerank_cache/`.

### Reproduce

```
python3 eval/embedding/rerank_eval.py --work <main checkout>/eval/embedding/work --table out.md --json out.json
python3 eval/embedding/rerank_eval.py --work <main checkout>/eval/embedding/work --latency 200
```

The cold full run took real 6341 s, user 22524 s and sys 212 s, with load
4.5 at start and 34 at the end. 537k cross-encoder pairs were scored; a
cached rerun takes seconds.

## S8: TypeScript Q5 diagnosis

Script: `eval/embedding/rerank_q5_diag.py`. It reads only stored data and
S4's cross-encoder cache (`<work>/rerank_cache/`), and it stops if a pair is
missing, so it never scores anything (a full run takes about 25 s). Variant
under study: ce-minilm + int8, K = 50, blend = CE logit + beta * int8 cosine.

**Control.** Re-tuning on the FIT half gives beta = 40 again. The script
then reproduces S4's row exactly: held-out r@10 0.693, MRR 0.477; vs int8
Q1 +4.8 [+1.2], Q2 +0.058 [+0.027], Q5 +6.5 [+12.7], TS +26.1 n=23; vs fp32
Q5 +5.7 [+12.1], TS +22.7 n=22; floors 19.48 / 17.02 / 24.91 / 17.10; NL
misled 20/80. Each fix below also has a revert (F1: floor on the blend; F2:
cosine floor = +inf; F3: c0 = -inf), and each revert reproduces every S4
cell.

A correction to the question as it was posed: Q5's false alarm is not about
absent queries. It counts **held-out NL positives that both arms rank
correctly at rank 1, where the top score falls below the arm's floor**, so a
correct answer is reported as "no match". On TypeScript absent queries the
blend is no worse than int8 (CW absent 2/13 for both).

### 1. Noise or real

Paired Q5 against int8 at its shipped floors, per language:

| language | n (paired) | int8 FA | blend FA | 0->1 | 1->0 | Δ [lo, up] |
|---|---|---|---|---|---|---|
| go | 13 | 3 | 3 | 0 | 0 | +0.0 |
| python | 17 | 2 | 2 | 2 | 2 | +0.0 [-17.6, +17.6] |
| rust | 7 | 1 | 1 | 0 | 0 | +0.0 |
| typescript | 23 | 2 | 8 | **6** | **0** | **+26.1 [+13.0, +43.5]** |

The TypeScript effect is real in direction: 6 queries flip and none flip
back (sign test p = 2/2^6 ≈ 0.03), and the per-language lower bound is +13.0.
Its size rests on 6 queries. **The Q5 verdict does not depend on one or two
of them.** When k of the six TypeScript flips are removed, the pooled upper
bound is +11.3 at k=1, +10.2 at k=2, +8.8 at k=3, +7.7 at k=4 and +6.2 at
k=5. Only k=6 (no TypeScript flip at all) gives +4.4, which passes. With
TypeScript dropped from the pool the upper bound is +5.9, which would still
fail, because python's 2-up / 2-down on n=17 makes the interval wide by
itself.

### 2. What the flipped queries hit

The top row of every flipped query is the correct answer. The int8 cosine
clears the shipped TS floor (0.53) every time; the cross-encoder gives a
strongly negative logit, and that drags the blend below its floor of 17.10.

| query | top row (the expected symbol) | text the CE sees | CE | int8 cos | blend |
|---|---|---|---|---|---|
| exc-013 "contract for storing text-to-diagram chat history" | type `TTDPersistenceAdapter` | "Interface for TTD chat persistence. Preferably ..." | -10.64 | 0.606 | 13.58 |
| exc-030 "Return what every item in a list has in common ..." | fn `reduceToCommonValue` | `reduceToCommonValue<T, R = T>(collection: ...` (signature only) | -6.59 | 0.574 | 16.39 |
| ttm-029 "tally task states for one release" | fn `releaseTaskCounts` | `releaseTaskCounts(db: DatabaseInstance, releaseId: string): ProjectStatus` | -10.56 | 0.654 | 15.60 |
| ttm-031 "confirm then ask dashboard server to exit" | fn `wireShutdownButton` (picker.js) | `wireShutdownButton()` | -11.13 | 0.600 | 12.86 |
| ttm-045 "fetch full task record or throw" | fn `requireTask` | `requireTask(db: DatabaseInstance, taskId: string): Task` | -10.64 | 0.600 | 13.37 |
| ttm-047 "display fatal message in board page" | fn `showError` (board.js) | `showError(message)` | -9.95 | 0.586 | 13.47 |

(ttm-003 and ttm-046 are false alarms in both arms: their int8 cosines of
0.462 and 0.436 are below 0.53.)

**Pattern: undocumented TypeScript symbols.** In five of the six, the CE
sees a bare signature with no doc comment. `exc-013` is a documented
interface that the CE still scores at -10.6. MS MARCO MiniLM is a passage
reranker, and a lone identifier with a parameter list is out of its
distribution. It gives such text about -10 whether the match is right or
not. TypeScript is where this concentrates: of the 29 held-out NL rank-1
tops, 10 have no doc comment (median CE -8.58), and 6 of those 10 fall
below the floor. Among documented TS tops the median CE is +0.33 and 3 of
19 fall below. The other languages have few undocumented tops: go 1 of 18,
python 5 of 20, rust 0 of 13. Four of the six flips come from
task-tracker-mcp, whose JS/TS functions largely lack JSDoc.

### 3. The floor

The floor is on the **blend score of the new top row**, per top-row language,
fitted by D6: the 3% quantile (rounded down to 0.01) of the rank-1 positives
among **all name queries plus the FIT half of NL**. For TypeScript that is
255 name queries and 24 FIT NL. Name queries make up 91% of the fit, so the
floor is effectively a name-query floor:

| TS rank-1 positives | n | blend p3 | blend median | CE median | int8 cos median |
|---|---|---|---|---|---|
| name | 255 | 26.22 | 37.34 | +7.06 | 0.76 |
| NL, FIT half | 24 | 6.37 | 22.18 | -1.87 | 0.62 |
| NL, held-out | 29 | 6.37 | 21.82 | -0.94 | 0.61 |

The floor from names alone would be 26.22 and from NL-FIT alone 6.37; the
pooled 3% quantile, 17.10, sits between them. So the TS floor is not a
small-sample misestimate: the FIT and held-out NL halves have the same
distribution (p3 6.37 in both). The floor fails on NL because **the blend is
bimodal across query types**. On name queries the CE sees its own
vocabulary (median +7), on NL-to-signature pairs it does not (median -1 to
-2, with a tail near -11). The cosine alone shows the same gap but a much
smaller one. For int8's own ranking the TS name p3 is 0.576 and the NL-FIT
p3 is 0.409, both on a scale where the shipped floor is 0.53. Python shows
the same bimodality (floor 17.02; NL-FIT only 9.38, names only 26.65), but
its NL tops are more often documented, so its held-out NL mass sits above
the floor. Rust's floor (24.91) is set by names, and its NL-FIT
p3 is higher (28.19, n=7).

### 4. Where the extra misled share comes from

Held-out NL misled (rank-1 positives called no-match), int8 at its shipped
floors against the blend: go 5/17 -> 4/18, python 2/20 -> 2/20, **rust 1/7 ->
5/13**, **typescript 2/26 -> 9/29**; total 10/70 -> 20/80. On name queries
misled is 1-2% for both (9/577 against 10/724). So the extra share comes
from TS NL (+7) and rust NL (+4). The two have different causes:

- **TypeScript**: 6 of its 9 misled are queries that int8 also had at rank 1
  and cleared. These are the Q5 flips from section 2.
- **Rust**: 4 of its 5 misled are queries that int8 did **not** have at rank
  1. The blend moves the right answer to the top but leaves it below the
  rust floor. These are recall gains that stay silent, not regressions, and
  they fall outside paired Q5.

**Q4 and Q5 are one effect.** Shifting every floor by d traces each arm's
operating points on held-out NL (misled / CW pos / CW absent):

| arm, floor shift | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|
| int8, shipped | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% | 32% | 23% |
| int8, +0.04 | 21% (15/70) | 40% (86/215) | 15% (7/46) | 4% | 30% | 14% |
| blend, -2 | 20% (16/80) | 40% (87/215) | 20% (9/46) | 0% | 15% | 13% |
| blend, D6 (S4) | 25% (20/80) | 31% (66/215) | 9% (4/46) | 1% | 14% | 9% |
| blend, -4 | 9% (7/80) | 48% (104/215) | 30% (14/46) | 0% | 16% | 18% |

At a matched NL misled rate, the blend's NL confident-wrong is about int8's
(blend -2 against int8 +0.04: 40% against 40% on positives, 20% against 15%
on absent). **Most of S4's NL Q4 gain (CW pos 52% -> 31%, absent 22% -> 9%)
is a stricter NL operating point**, which the name-dominated floor imposes.
The Q5 failure is the price of that same point. The gain that does not
depend on the floor is ranking (held-out r@10 +4.8, MRR +0.058) and name
queries (name CW pos 32% -> 14%, name misled 2% -> 1%).

### 5. Fix candidates

Chosen after 1-4, each gated on the held-out half. F1 and F2 have nothing
to tune (their floors are D6-fitted, and D6 fits on names plus the FIT half
only). F3's c0 is tuned on the FIT half: the best FIT r@10/MRR with FIT-half
false alarm at or below int8's (11.2%). No c0 on the grid
{-10, -8, -6, -4, -2, 0, 2} meets that bound; the lowest FIT FA is 14.3% at
c0 = -10, so c0 = -10 is used. The baseline rows here are gated on
held-out NL, so int8 vs fp32 Q1 reads +1.2 here against +0.5 on all NL in S4.

- **F1**: rank by the blend, and floor on the int8 cosine of the new top row
  (D6-fitted on that cosine).
- **F2**: a joint rule. The top clears if blend >= its D6 floor **or** cosine
  >= F1's cosine floor.
- **F3**: a CE minimum. blend = max(CE, c0) + 40 * cos, which caps how far the
  CE can pull down a bare signature.

#### S8 quality (held-out NL)

| variant | held-out NL r@10 [lo, hi] | held-out NL MRR [lo, hi] | name r@10 / MRR |
|---|---|---|---|
| jina fp32 (baseline) | 0.633 [0.582, 0.685] | 0.423 [0.379, 0.469] | 0.917 / 0.745 |
| jina int8 (baseline, shipped floors) | 0.645 [0.593, 0.697] | 0.419 [0.374, 0.464] | 0.915 / 0.750 |
| S4 ce-minilm+int8 K=50 (beta=40), floor on blend | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.970 / 0.888 |
| F1 floor on int8 cosine of the reranked top | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.970 / 0.888 |
| F2 clears if blend >= its floor OR cosine >= its floor | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.970 / 0.888 |
| F3 CE minimum c0=-10 | 0.702 [0.651, 0.753] | 0.477 [0.430, 0.524] | 0.970 / 0.888 |

#### S8 D9 gates vs jina fp32 at its D6 floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=16, py +0.0 n=21, ru +0.0 n=7, ty +0.0 n=27 |
| jina int8 (baseline, shipped floors) | +1.2 [-0.9] | -0.004 [-0.016] | go -1.6 | -1.4 [+1.0] | +2.1 [+6.2] | **Q5** | go +12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty -4.0 n=25 |
| S4 ce-minilm+int8 K=50 (beta=40), floor on blend | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | -22.2 [-17.3] | +5.7 [+12.1] | **Q5** | go +0.0 n=12, py +0.0 n=18, ru +0.0 n=6, ty +22.7 n=22 |
| F1 floor on int8 cosine of the reranked top | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | +2.0 [+6.8] | -12.9 [-4.9] | **Q4** | go -25.0 n=12, py -5.6 n=18, ru -16.7 n=6, ty -4.5 n=22 |
| F2 clears if blend >= its floor OR cosine >= its floor | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | +4.5 [+9.2] | -14.3 [-6.0] | **Q4** | go -25.0 n=12, py -11.1 n=18, ru -16.7 n=6, ty -4.5 n=22 |
| F3 CE minimum c0=-10 | +6.8 [+2.8] | +0.054 [+0.022] | typescript -1.7 | -21.1 [-16.3] | +7.0 [+13.4] | **Q5** | go +0.0 n=12, py +5.3 n=19, ru +0.0 n=6, ty +22.7 n=22 |

#### S8 D9 gates vs shipped jina int8 at its shipped floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | -1.2 [-3.4] | +0.004 [-0.008] | rust -4.3 | +1.4 [+3.8] | -2.1 [+1.4] | **Q4** | go -12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty +4.0 n=25 |
| jina int8 (baseline, shipped floors) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=17, py +0.0 n=20, ru +0.0 n=7, ty +0.0 n=26 |
| S4 ce-minilm+int8 K=50 (beta=40), floor on blend | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | -20.8 [-16.2] | +6.5 [+12.7] | **Q5** | go +0.0 n=13, py +0.0 n=17, ru +0.0 n=7, ty +26.1 n=23 |
| F1 floor on int8 cosine of the reranked top | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | +3.4 [+8.2] | -10.8 [-3.8] | **Q4** | go -23.1 n=13, py -5.9 n=17, ru -14.3 n=7, ty +0.0 n=23 |
| F2 clears if blend >= its floor OR cosine >= its floor | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | +5.9 [+10.5] | -12.3 [-5.0] | **Q4** | go -23.1 n=13, py -11.8 n=17, ru -14.3 n=7, ty +0.0 n=23 |
| F3 CE minimum c0=-10 | +5.6 [+2.2] | +0.059 [+0.029] | typescript -1.7 | -19.7 [-15.2] | +7.9 [+14.3] | **Q5** | go +0.0 n=13, py +5.6 n=18, ru +0.0 n=7, ty +26.1 n=23 |

#### S8 GM-434 columns (option a), at each row's own floors

| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | 0.56 / 0.58 / 0.56 / 0.55 | 14% (10/71) | 53% (113/215) | 26% (12/46) | 1% (7/569) | 32% (283/879) | 20% (183/900) |
| jina int8 (baseline, shipped floors) | 0.57 / 0.57 / 0.55 / 0.53 | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% (9/577) | 32% (282/879) | 23% (204/900) |
| S4 ce-minilm+int8 K=50 (beta=40), floor on blend | 19.48 / 17.02 / 24.91 / 17.10 | 25% (20/80) | 31% (66/215) | 9% (4/46) | 1% (10/724) | 14% (127/879) | 9% (81/900) |
| F1 floor on int8 cosine of the reranked top | 0.48 / 0.50 / 0.52 / 0.50 | 18% (14/80) | 53% (113/215) | 39% (18/46) | 2% (15/724) | 15% (133/879) | 31% (276/900) |
| F2 clears if blend >= its floor OR cosine >= its floor | 0.00 / 0.00 / 0.00 / 0.00 | 9% (7/80) | 55% (119/215) | 41% (19/46) | 1% (4/724) | 16% (142/879) | 32% (285/900) |
| F3 CE minimum c0=-10 | 19.48 / 16.72 / 24.91 / 17.10 | 26% (21/81) | 32% (68/215) | 11% (5/46) | 1% (10/724) | 14% (127/879) | 10% (88/900) |

#### S8 GM-434 per language, held-out NL (misled / CW pos / CW absent)

| variant | go | python | rust | typescript |
|---|---|---|---|---|
| jina fp32 (baseline) | 19% (3/16) / 51% (31/61) / 9% (1/11) | 10% (2/21) / 45% (21/47) / 36% (4/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 15% (4/27) / 42% (25/60) / 15% (2/13) |
| jina int8 (baseline, shipped floors) | 29% (5/17) / 41% (25/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 8% (2/26) / 47% (28/60) / 15% (2/13) |
| S4 ce-minilm+int8 K=50 (beta=40), floor on blend | 22% (4/18) / 39% (24/61) / 0% (0/11) | 10% (2/20) / 36% (17/47) / 9% (1/11) | 38% (5/13) / 19% (9/47) / 9% (1/11) | 31% (9/29) / 27% (16/60) / 15% (2/13) |
| F1 floor on int8 cosine of the reranked top | 17% (3/18) / 62% (38/61) / 18% (2/11) | 10% (2/20) / 51% (24/47) / 55% (6/11) | 31% (4/13) / 64% (30/47) / 45% (5/11) | 17% (5/29) / 35% (21/60) / 38% (5/13) |
| F2 clears if blend >= its floor OR cosine >= its floor | 6% (1/18) / 64% (39/61) / 18% (2/11) | 0% (0/20) / 53% (25/47) / 55% (6/11) | 31% (4/13) / 64% (30/47) / 45% (5/11) | 7% (2/29) / 42% (25/60) / 46% (6/13) |
| F3 CE minimum c0=-10 | 22% (4/18) / 39% (24/61) / 0% (0/11) | 14% (3/21) / 36% (17/47) / 18% (2/11) | 38% (5/13) / 19% (9/47) / 9% (1/11) | 31% (9/29) / 30% (18/60) / 15% (2/13) |

### S8 reading

No candidate passes every gate. **F1 and F2 pass Q5**: vs int8 -10.8
[-3.8] and -12.3 [-5.0]; vs fp32 -12.9 [-4.9] and -14.3 [-6.0]. They keep
all of the ranking gain, since the ranking is unchanged: r@10 +4.8 [+1.2],
MRR +0.058 [+0.027] vs int8. **But they fail Q4** (+3.4 [+8.2] and +5.9
[+10.5] vs int8), and NL CW on absent queries rises to 39-41% against
int8's 22%. So they trade the confident-wrong gain for the false-alarm fix,
which is what section 4 predicts: on NL, S4's Q4 gain and its Q5 loss are
the same stricter operating point. **F3 does nothing useful.** No CE
minimum brings FIT-half false alarm down to int8's. The c0 it picks (-10)
adds +0.8 r@10 and leaves Q5 failing (+7.9 [+14.3]).

The rerank's floor-independent value is ranking (r@10 +4.8, MRR +0.058, name
r@10 0.97) and name-query confidence. For NL confidence it adds little that
a stricter floor on int8 would not also give. The TypeScript failure is
real, but it is a symptom, not the disease. The disease is a
general-English cross-encoder scoring undocumented signatures near -10,
together with a floor fitted mostly on name queries. The option still open
is F1 (rank by the blend, gate on the cosine). It fails Q4 by a margin of
+8.2 on the upper bound. A stricter cosine floor would move it along the
same curve, trading Q5 margin for Q4 margin; that sweep was not run here. Whether that trade is acceptable is the owner's decision; these
gates do not settle it.

### S8 reproduce

```
python3 eval/embedding/rerank_q5_diag.py --work <main checkout>/eval/embedding/work --fixes --table out.md
```

Reads S4's CE cache only (stops on a missing pair); real 25.6 s, user 15.2 s,
sys 5.5 s at load 23.

## S11: order-only rerank (F4)

The blend (ce-minilm + 40 * int8 cosine, K = 50) only orders the rows shown;
the no-match verdict is not taken from the blend. Nothing is refit: both
variants use the shipped int8 floors (0.57 / 0.57 / 0.55 / 0.53, read from
`similarity.rs::floor`), the same D9 gates and the same GM-434 option-a
columns as S4/S8. Cached CE scores only; nothing rescored.

- **F4**: the verdict is int8's own: its un-reranked top-1 cosine and that
  row's language against the shipped floor. It equals int8's verdict on
  every query (checked), and only the rows shown change.
- **F4'**: the verdict is the int8 cosine (and language) of the *reranked*
  top-1, at the shipped floors (F1 without its refit).

### Controls

- S4's row reproduces (step 0 MATCH), and the F1, F2, F3 reverts equal S4;
  all 30 S8 table rows are reproduced unchanged.
- F4 and F4' with the rerank disabled (int8 order through the same code
  path) equal the int8 baseline on every outcome and every reported cell.

How Q4 can still move under F4: CW counts a query that clears the floor
with a wrong top row; the verdict is fixed, but the top row is the blend's,
so a cleared query whose top the rerank fixes leaves CW and one it breaks
enters it (NL held-out: 111 - 15 + 8 = 104).

#### S11 quality (held-out NL)

| variant | held-out NL r@10 [lo, hi] | held-out NL MRR [lo, hi] | name r@10 / MRR |
|---|---|---|---|
| jina fp32 (baseline) | 0.633 [0.582, 0.685] | 0.423 [0.379, 0.469] | 0.917 / 0.745 |
| jina int8 (baseline, shipped floors) | 0.645 [0.593, 0.697] | 0.419 [0.374, 0.464] | 0.915 / 0.750 |
| F4 blend order, verdict on int8's own top-1 cosine (shipped floors) | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.970 / 0.888 |
| F4' blend order, verdict on int8 cosine of the reranked top (shipped floors) | 0.693 [0.642, 0.744] | 0.477 [0.430, 0.523] | 0.970 / 0.888 |

#### S11 D9 gates vs jina fp32 at its D6 floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=16, py +0.0 n=21, ru +0.0 n=7, ty +0.0 n=27 |
| jina int8 (baseline, shipped floors) | +1.2 [-0.9] | -0.004 [-0.016] | go -1.6 | -1.4 [+1.0] | +2.1 [+6.2] | **Q5** | go +12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty -4.0 n=25 |
| F4 blend order, verdict on int8's own top-1 cosine (shipped floors) | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | -4.2 [-0.5] | -1.1 [+0.0] | pass | go +0.0 n=12, py +0.0 n=18, ru +0.0 n=6, ty -4.5 n=22 |
| F4' blend order, verdict on int8 cosine of the reranked top (shipped floors) | +6.0 [+1.8] | +0.054 [+0.020] | typescript -3.3 | -16.3 [-11.8] | -1.1 [+0.0] | pass | go +0.0 n=12, py +0.0 n=18, ru +0.0 n=6, ty -4.5 n=22 |

#### S11 D9 gates vs shipped jina int8 at its shipped floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | -1.2 [-3.4] | +0.004 [-0.008] | rust -4.3 | +1.4 [+3.8] | -2.1 [+1.4] | **Q4** | go -12.5 n=16, py +0.0 n=20, ru +0.0 n=6, ty +4.0 n=25 |
| jina int8 (baseline, shipped floors) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=17, py +0.0 n=20, ru +0.0 n=7, ty +0.0 n=26 |
| F4 blend order, verdict on int8's own top-1 cosine (shipped floors) | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | -2.8 [+0.2] | +0.0 [+0.0] | pass | go +0.0 n=13, py +0.0 n=17, ru +0.0 n=7, ty +0.0 n=23 |
| F4' blend order, verdict on int8 cosine of the reranked top (shipped floors) | +4.8 [+1.2] | +0.058 [+0.027] | typescript -3.3 | -14.9 [-10.7] | +0.0 [+0.0] | pass | go +0.0 n=13, py +0.0 n=17, ru +0.0 n=7, ty +0.0 n=23 |

#### S11 GM-434 columns (option a), at each row's own floors

| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|---|
| jina fp32 (baseline) | 0.56 / 0.58 / 0.56 / 0.55 | 14% (10/71) | 53% (113/215) | 26% (12/46) | 1% (7/569) | 32% (283/879) | 20% (183/900) |
| jina int8 (baseline, shipped floors) | 0.57 / 0.57 / 0.55 / 0.53 | 14% (10/70) | 52% (111/215) | 22% (10/46) | 2% (9/577) | 32% (282/879) | 23% (204/900) |
| F4 blend order, verdict on int8's own top-1 cosine (shipped floors) | 0.57 / 0.57 / 0.55 / 0.53 | 16% (13/80) | 48% (104/215) | 22% (10/46) | 2% (18/724) | 16% (144/879) | 23% (204/900) |
| F4' blend order, verdict on int8 cosine of the reranked top (shipped floors) | 0.57 / 0.57 / 0.55 / 0.53 | 29% (23/80) | 35% (75/215) | 15% (7/46) | 5% (39/724) | 14% (127/879) | 20% (179/900) |

#### S11 GM-434 per language, held-out NL (misled / CW pos / CW absent)

| variant | go | python | rust | typescript |
|---|---|---|---|---|
| jina fp32 (baseline) | 19% (3/16) / 51% (31/61) / 9% (1/11) | 10% (2/21) / 45% (21/47) / 36% (4/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 15% (4/27) / 42% (25/60) / 15% (2/13) |
| jina int8 (baseline, shipped floors) | 29% (5/17) / 41% (25/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 14% (1/7) / 77% (36/47) / 45% (5/11) | 8% (2/26) / 47% (28/60) / 15% (2/13) |
| F4 blend order, verdict on int8's own top-1 cosine (shipped floors) | 28% (5/18) / 39% (24/61) / 0% (0/11) | 10% (2/20) / 47% (22/47) / 27% (3/11) | 23% (3/13) / 68% (32/47) / 45% (5/11) | 10% (3/29) / 43% (26/60) / 15% (2/13) |
| F4' blend order, verdict on int8 cosine of the reranked top (shipped floors) | 44% (8/18) / 25% (15/61) / 0% (0/11) | 15% (3/20) / 38% (18/47) / 27% (3/11) | 46% (6/13) / 53% (25/47) / 27% (3/11) | 21% (6/29) / 28% (17/60) / 8% (1/13) |

#### S11 flips among positives the verdict clears (int8 top right -> reranked top wrong / wrong -> right; n = cleared positives)

| variant | queries | go | python | rust | typescript | total |
|---|---|---|---|---|---|---|
| F4 | NL held-out | 2 / 3 (n=37) | 3 / 3 (n=40) | 0 / 4 (n=42) | 3 / 5 (n=52) | 8 / 15 (n=171) |
| F4 | name | 0 / 49 (n=144) | 0 / 20 (n=143) | 1 / 52 (n=291) | 2 / 20 (n=272) | 3 / 141 (n=850) |
| F4' | NL held-out | 2 / 0 (n=25) | 3 / 2 (n=35) | 0 / 1 (n=32) | 1 / 2 (n=40) | 6 / 5 (n=132) |
| F4' | name | 0 / 41 (n=136) | 0 / 17 (n=139) | 1 / 44 (n=273) | 2 / 18 (n=264) | 3 / 120 (n=812) |

### S11 reading

**Both pass every D9 gate against both baselines.** Ranking is the blend's
in both (r@10 +4.8 [+1.2], MRR +0.058 [+0.027] vs int8). **F4** keeps int8's
verdict, so Q5 is +0.0 [+0.0] vs int8 by construction and NL CW absent stays
at int8's 22% (10/46); its Q4 gain is small and only just inside the bound
(-2.8 [+0.2] vs int8), coming from the rerank fixing more cleared tops than
it breaks: 15 vs 8 on held-out NL, 141 vs 3 on names (name CW pos 32% ->
16%). The cost is 8 held-out NL queries (and 3 name) where int8's top was
right, the verdict says "match", and the rerank now shows a wrong top. NL
misled rises 10/70 -> 13/80 only on queries the rerank newly made rank-1.
**F4'** gets a much larger Q4 gain (-14.9 [-10.7] vs int8; NL CW absent 15%,
CW pos 35%) with Q5 also +0.0 [+0.0], but that Q5 is paired on queries
rank-1 in both arms: on all rank-1 positives its NL misled is 29% (23/80)
against int8's 14% (10/70), name misled 5% vs 2%. That is the S8 mechanism
again (the reranked top's own cosine is lower), just outside Q5's pairing.
F4 is the conservative option: int8's confidence behaviour unchanged, the
ranking gain kept, 8 new confident-wrong NL tops. Which one, if any, ships
is the owner's call.

### S11 reproduce

Same command as S8 (the F4/F4' rows, controls and flip table are in its
output and `--table` file).

## S12: latency on an idle machine

What F4 adds per query on top of today's int8 search: ce-minilm
(ms-marco-MiniLM-L6-v2, ONNX, CPU) scores int8's top-K (tokenization and
model, length-sorted chunks of 16), then the blend `ce + 40 * cosine` and the
sort. The verdict is int8's own top-1 cosine, so it costs nothing. The int8
query embedding and vector search are excluded: they run today already. The
graph rerank over int8 K=50 (SQL features) is timed for reference.

Sample: 200 queries, seeded (`random.Random(50)`), stratified 100 NL
(`queries/<corpus>.jsonl`, positives and absent) and 100 name queries
(`queries/mechanical`), over all corpora. One full untimed warm-up pass per
K, then the timed pass. Machine: Intel Core i7-1068NG7, 4 physical cores / 8
logical; onnxruntime 1.19.2, CPUExecutionProvider.

Before the gate (after a 90 s settle from a smoke test), top process
PrinterInstallerClient at 6.4% CPU:

```
1:32  up 12:55, 7 users, load averages: 2.74 3.31 3.91
```

| F4 / graph, int8 recall | queries | n | default threads p50 / p95 / max ms | 1 thread p50 / p95 / max ms |
|---|---|---|---|---|
| F4 K=50 | NL | 100 | 498.7 / 1280.9 / 2147.5 | 1005.4 / 2990.4 / 4762.3 |
| F4 K=50 | name | 100 | 254.0 / 892.6 / 1116.8 | 626.8 / 2101.4 / 2784.6 |
| F4 K=50 | all | 200 | 355.2 / 1137.2 / 2147.5 | 828.7 / 2656.4 / 4762.3 |
| F4 K=20 | NL | 100 | 158.5 / 559.6 / 925.4 | 384.8 / 1353.0 / 2090.8 |
| F4 K=20 | name | 100 | 88.5 / 319.0 / 537.9 | 225.0 / 868.7 / 1458.3 |
| F4 K=20 | all | 200 | 131.1 / 460.2 / 925.4 | 326.9 / 1215.8 / 2090.8 |
| graph K=50 | NL | 100 | 1.1 / 2.4 / 2.8 | 0.9 / 1.9 / 2.7 |
| graph K=50 | name | 100 | 1.1 / 2.1 / 3.0 | 1.2 / 2.3 / 3.6 |
| graph K=50 | all | 200 | 1.1 / 2.3 / 3.0 | 1.1 / 2.2 / 3.6 |

Mean cost per scored pair (total F4 time / pairs): default threads 9.62 ms
at K=50 (10000 pairs), 8.84 ms at K=20 (4000 pairs); 1 thread 21.43 ms and
21.74 ms. Time is dominated by pair length: NL queries are longer than
names, and the p95/max tail is queries whose top-K carries long doc
comments (up to the 512-token truncation).

Runs, each one warm-up + timed pass for both K and the graph reference:

| intra-op threads | uptime at start | uptime at end | `time -p` real / user / sys |
|---|---|---|---|
| 0 (onnxruntime default = physical cores, 4) | 2.60 3.27 3.89 | 7.02 5.49 4.71 | 244.41 / 953.11 / 5.46 s |
| 1 | 7.02 5.49 4.71 | 3.78 4.26 4.44 | 602.92 / 598.51 / 3.81 s |

The load rise during the default run is the run itself (user/real 3.9, four
busy intra-op threads); spotlightknowledged at 30.9% was the top other
process right after it. The 1-thread run started on that residual load but
was CPU-bound throughout (user ≈ real), and no other process was above 50%
at its end (two Chrome renderers at ~28%). Neither run was waiting.

**Reading.** The realistic setting for an MCP server is the default
(onnxruntime uses all physical cores; g-mesh's product code sets no thread
count), on a laptop that is otherwise mostly idle between tool calls; the
1-thread column is what the user gets when the machine is busy. At K=50 F4
adds a median 0.36 s (NL 0.50 s) and a p95 of 1.1 s (NL 1.3 s), with a 2.1 s
worst case, per `search_code` call; at K=20 it adds a median 0.13 s and a p95
of 0.46 s (worst 0.93 s). For an agent tool call, where the model's own turn
takes seconds, K=20 is comfortably acceptable and K=50 is acceptable but
noticeable on NL queries, and doubles to 2.5-3 s p95 on a busy machine. The
graph rerank is free by comparison (about 1 ms). Whether K=20 keeps F4's
quality gain was not measured here (S11 used K=50); that is the question if
the latency of K=50 is judged too high.

### S12 reproduce

```
python3 eval/embedding/rerank_eval.py --work <main checkout>/eval/embedding/work --latency-f4 200
python3 eval/embedding/rerank_eval.py --work <main checkout>/eval/embedding/work --latency-f4 200 --ort-threads 1
```
