# GM-444: fused retrievers against jina-v2-base-code fp32

Question: does fusing gte-small with bge-small, or an embedder with BM25,
match or beat jina-v2-base-code fp32 (NL recall@10 0.633, MRR 0.427) under the
GM-398 eval (`docs/architecture/embedding-eval.md`, D5/D6/D9)?

Answer: **no variant passes D9 as it stands.** gte+bge fusion does not beat
gte alone. The one lead is **jina + BM25 by weighted score fusion**: on the
held-out half it lifts recall@10 by +6.6 points (lower bound +3.4) with MRR
unchanged. It fails Q4 when its floors are read on jina's cosine. It passes
Q1-Q5 when they are read on its own fused score, but that score is relative to
the query (below). It is also not a cheaper model, so D9's "clearly better"
rule applies, and MRR's lower bound (-0.025) fails that rule.

Stored data only (no new embedding): the GM-398 runs under
`eval/embedding/work/runs/{jina-v2-base-code-fp32,gte-small,bge-small-en-v1.5,bm25}`
(rankings top-100 per query, vectors, query vectors) and the corpus snapshots.
Script: `eval/embedding/fusion_eval.py` (loading/scoring from
`recall_at_k.py` of GM-443, floors and the SplitMix64 bootstrap from
`q5_floor_sensitivity.py` of GM-422, confident-wrong and the gates ported from
`metrics.rs`/`decision.rs`). One run, all six corpora, 133 s wall.

## Control

Step 0 reproduces GM-398 before anything is reported; every check matched.

| check | result |
|---|---|
| vectors re-rank to the stored lists | top-10 order identical on 2279/2279 (jina), 2276/2279 (gte, three near-ties), 2279/2279 (bge); max score diff 1.3e-6 |
| pooled r@1/r@5/r@10 [lo, hi]/MRR [lo, hi]/CW combined, positives, absent | jina fp32, gte, bge, bm25: all match the "Quality, pooled" table |
| fitted floors and held-out false alarm | jina, gte, bge: match |
| D9 verdict rows (Δr@10 [lower], ΔMRR [lower], Q3 worst, ΔCW [upper], ΔFA [upper]) | gte and bge: match |
| int8 Q5 pooled (+2.1 [-1.4, +6.2]) | match (the vectorised bootstrap is bit-for-bit the Rust one) |
| fusion code: RRF and weighted fusion of jina with itself; concat of gte with itself | first-expected rank unchanged on every query |

## Variants

- **RRF**: `sum 1/(k + rank)` over the two top-100 lists, k = 60 (standard)
  and k = 10 (sensitivity; k = 10 was fixed before the run, not tuned).
- **Score fusion (min-max)**: each list min-max normalised over its own
  top-100 (1st = 1, 100th = 0), then `w * a + (1 - w) * b`. For gte+bge `w = 0.5`,
  not tuned. For the hybrids `w` (the embedder's weight) was picked on the
  **fit half** of the authored NL positives only. The grid was 0.05..0.95 and
  the objective was r@10, then MRR. The chosen weights are jina 0.65, gte 0.75
  and bge 0.60. Their gates are read on the **held-out half** only.
- **Concat (gte+bge)**: cosine over the concatenation of the two unit vectors
  equals the mean of the two cosines. It is computed over every node from
  `vectors.bin`, so no truncation applies.
- A fused list is cut to 100 hits, as `KEPT_HITS` does.

### Score used for floors (Q4, Q5)

D6 fits a floor on a similarity score, and in production that floor gives
the verdict "no match". RRF and min-max scores depend only on ranks within one
query, so they carry no absolute evidence of a match. If both lists agree on a
top hit, it scores the maximum whether or not the corpus holds an answer. Two
readings are therefore reported:

- **judge (gated)**: the embedder cosine of the fused top hit. For gte+bge it
  is the mean of the two cosines, the same as concat. The stored cosine is used
  when the hit is in that embedder's top-100, otherwise it is recomputed from
  the vectors. This is the reading production could use: rank by fusion, judge
  by the embedder. Floors are fitted on it as D6 prescribes.
- **own (reported, not a verdict)**: the fused score itself. RRF is scaled to
  [0, 1] by its maximum, 2/(k+1). Its floors mostly measure whether the two
  retrievers agree on the top hit, not how similar the top hit is.

## Results

NL = GM-398's scored set (authored positives, n = 400; held-out half n = 215).
name = the mechanical name queries (positives, n = 879, all in the fit half).
Δ is variant minus jina fp32, bounds are one-sided 95%. The gates are read on
all NL, except for tuned variants (†), which are read on the held-out NL half
paired with jina on the same half.

| variant | NL r@10 | NL MRR | held-out r@10 / MRR | name r@10 / MRR |
|---|---|---|---|---|
| **jina fp32 (R)** | **0.633** | **0.427** | 0.633 / 0.423 | 0.917 / 0.745 |
| gte-small | 0.580 | 0.373 | 0.586 / 0.351 | 0.943 / 0.836 |
| bge-small-en-v1.5 | 0.557 | 0.360 | 0.575 / 0.348 | 0.948 / 0.843 |
| bm25 | 0.448 | 0.320 | 0.431 / 0.315 | 0.952 / 0.788 |
| gte+bge RRF k=60 | 0.568 | 0.371 | 0.582 / 0.353 | 0.952 / 0.841 |
| gte+bge RRF k=10 | 0.575 | 0.371 | 0.581 / 0.353 | 0.952 / 0.841 |
| gte+bge min-max w=0.5 | 0.578 | 0.371 | 0.587 / 0.356 | 0.950 / 0.842 |
| gte+bge concat | 0.573 | 0.369 | 0.586 / 0.358 | 0.954 / 0.843 |
| jina+bm25 RRF k=60 | 0.585 | 0.392 | 0.605 / 0.387 | 0.974 / 0.823 |
| jina+bm25 RRF k=10 | 0.620 | 0.408 | 0.634 / 0.402 | 0.973 / 0.824 |
| jina+bm25 min-max w=0.65 † | (0.675) | (0.434) | **0.699** / 0.425 | 0.961 / 0.812 |
| gte+bm25 RRF k=60 | 0.530 | 0.368 | 0.532 / 0.356 | 0.963 / 0.841 |
| gte+bm25 RRF k=10 | 0.557 | 0.379 | 0.562 / 0.368 | 0.966 / 0.845 |
| gte+bm25 min-max w=0.75 † | (0.575) | (0.389) | 0.570 / 0.368 | 0.957 / 0.852 |
| bge+bm25 RRF k=60 | 0.525 | 0.364 | 0.518 / 0.354 | 0.971 / 0.843 |
| bge+bm25 RRF k=10 | 0.545 | 0.368 | 0.551 / 0.362 | 0.975 / 0.846 |
| bge+bm25 min-max w=0.60 † | (0.560) | (0.384) | 0.564 / 0.372 | 0.967 / 0.863 |

(Parenthesised all-NL values of tuned variants include the half the weight
was tuned on, and are optimistic.)

### D9 gates (judge floors)

| variant | Q1 Δr@10 [lower] | Q2 ΔMRR [lower] | Q3 worst language | Q4 ΔCW [upper] | Q5 ΔFA [upper] | own-score Q4 / Q5 | fails (non-inferiority) |
|---|---|---|---|---|---|---|---|
| gte+bge RRF k=60 | -6.5 [-10.8] | -0.057 [-0.089] | go -8.0 | +11.0 [+16.5] | -15.2 [-2.3] | fail / pass | Q1, Q2, Q4 |
| gte+bge RRF k=10 | -5.8 [-10.0] | -0.056 [-0.088] | ts -8.0 | +11.0 [+16.5] | -15.2 [-2.3] | fail / pass | Q1, Q2, Q4 |
| gte+bge min-max | -5.5 [-9.8] | -0.057 [-0.089] | ts -9.0 | +11.2 [+16.7] | -12.1 [-0.3] | fail / fail | Q1, Q2, Q4 |
| gte+bge concat | -6.0 [-10.2] | -0.059 [-0.091] | ts -8.0 | +11.3 [+16.8] | -12.1 [-0.3] | (same as judge) | Q1, Q2, Q4 |
| jina+bm25 RRF k=60 | -4.8 [-8.2] | -0.036 [-0.063] | ts -12.0 | +2.2 [+6.9] | -9.8 [-2.5] | fail / pass | Q1, Q2, Q3, Q4 |
| jina+bm25 RRF k=10 | -1.2 [-4.2] | -0.020 [-0.044] | python -6.0 | +0.8 [+5.6] | -9.5 [-2.5] | pass / pass | Q4 |
| jina+bm25 min-max † | **+6.6 [+3.4]** | +0.002 [-0.025] | ts +3.3 | +2.6 [+6.6] | -5.0 [-1.2] | **pass / pass** | Q4 |
| gte+bm25 RRF k=60 | -10.2 [-14.5] | -0.060 [-0.092] | python -17.0 | +3.7 [+9.2] | -5.7 [+5.2] | pass / pass | Q1-Q5 |
| gte+bm25 RRF k=10 | -7.5 [-11.8] | -0.048 [-0.081] | python -17.0 | +2.1 [+7.5] | -5.7 [+5.2] | pass / pass | Q1-Q5 |
| gte+bm25 min-max † | -6.3 [-12.1] | -0.054 [-0.100] | python -12.8 | +15.5 [+20.8] | -9.4 [+1.9] | pass / pass | Q1-Q4 |
| bge+bm25 RRF k=60 | -10.8 [-15.0] | -0.063 [-0.098] | python -15.0 | +11.4 [+17.1] | -7.1 [+5.7] | pass / fail | Q1-Q5 |
| bge+bm25 RRF k=10 | -8.8 [-13.0] | -0.059 [-0.093] | python -15.0 | +8.8 [+14.2] | -5.0 [+8.3] | pass / fail | Q1-Q5 |
| bge+bm25 min-max † | -6.9 [-13.2] | -0.051 [-0.099] | ts -13.3 | +10.0 [+15.7] | -9.4 [+1.8] | pass / pass | Q1-Q4 |

Judge floors, go / python / rust / typescript: gte+bge 0.77 / 0.79 / 0.78 / 0.76
(0.75 ts for min-max); jina+bm25 RRF 0.49 / 0.46 / 0.49 / 0.53; jina+bm25
min-max 0.53 / 0.53 / 0.54 / 0.53 (jina alone: 0.56 / 0.58 / 0.56 / 0.55).
When the fused top-1 is a hit BM25 pulled up, its jina cosine is lower, and so
is the fitted floor. That is why confident-wrong rises at judge floors. The
own-score floors of jina+bm25 min-max are 0.82 / 0.80 / 0.78 / 0.85.

No variant has a Q1 lower bound above 0 except jina+bm25 min-max, and none has
a Q2 lower bound above 0.

### Truncation (fusion over top-100 lists)

A document missing from one list gets nothing from that list: 0 in RRF, and
the list's minimum in min-max. In the fused order it therefore sits below
every document that both lists contain at a similar rank. How often this bites
(NL, n = 400):

| pair | expected answer in only one top-100 | in neither |
|---|---|---|
| gte+bge | 26 | 67 |
| jina+bm25 | 97 | 39 |
| gte+bm25 | 82 | 61 |
| bge+bm25 | 74 | 68 |

The embedder side can be run uncut: RRF k=60 with the embedder's full cosine
ranking (BM25 still top-100, because its scores beyond the list are not stored).
This changes NL r@10 by at most 0.010: gte+bge 0.568 → 0.568, jina+bm25
0.585 → 0.575, gte+bm25 0.530 → 0.530, bge+bm25 0.525 → 0.528. So the cut does
not explain the RRF results. The BM25 side could not be tested from stored data.

## Go / no-go against jina fp32

| variant | verdict |
|---|---|
| gte+bge (RRF k=60/10, min-max, concat) | **No-go.** Fails Q1, Q2 and Q4, and the results sit on or below gte alone. The two small models fail on the same queries (only 26 of 400 answers are in exactly one of their lists), so fusing them adds little. |
| jina+bm25 RRF k=60 | **No-go.** Fails Q1-Q4. |
| jina+bm25 RRF k=10 | **No-go.** Non-inferiority fails only Q4 (+0.8 [+5.6]; the point is above 0). Q1 and Q2 are not better. It costs more than jina, so it would have to be a quality candidate, and it is not one. k=10 is also the better of the two k values, picked after seeing the results. |
| jina+bm25 min-max w=0.65 | **No-go under D9 as written; the only lead.** Held-out r@10 +6.6 [+3.4] and MRR +0.002 [-0.025]. The quality rule needs the MRR lower bound > 0, and this fails it. Q4 fails at judge floors (+2.6 [+6.6]). At own-score floors Q4 and Q5 pass (-11.9 [-5.2] / -5.7 [+3.8]), but those floors judge agreement between the two retrievers, not similarity. |
| gte+bm25, bge+bm25 (all) | **No-go.** Fail Q1-Q3 by wide margins (worst language down 12-19 points), plus Q4. |

What a follow-up would have to settle before jina+bm25 min-max could be
proposed:

1. Which score gives the verdict. Judge floors fail Q4. Own-score floors pass,
   but they would ship a query-relative floor.
2. Whether a recall-only gain with flat MRR is worth a second retriever. D9's
   quality rule says no.
3. Cost (step 2, not run here: the owner confirms it first).
