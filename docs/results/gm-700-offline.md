# GM-700 S3: offline int8 quality, latency, confident-skip, K=20

Spec: `docs/design/GM-700-rerank-speedup.md` sections 4.1-4.4 and owner decisions (section 8).
Harness: `eval/embedding/gm463_bakeoff.py` (`quality`, `latency`), commit `6ee2b07c`. Workdir: GM-463's `v2/work`, new score cache `rerank_cache_gm700` (clone of the fp32 caches, so fp32 caches stay valid).
Machine: Intel i7-1068NG7 (4 physical cores, ORT threads 4), other sessions loading it (load 3-5 at start of each run).

## Verdict

- **Ship file: `model_qint8_avx512_vnni.onnx` (qint8), chunk 1, BETA 40, K=30.** It passes every AC bar and the Q2 non-inferiority rule against fp32 L6. quint8 also passes but is not better on quality (Q4), so qint8 stays.
- **Skip rule: ship rule (a) exact-name only. Do not ship rule (b) margin.** (a) changes no cell on held-out NL and name MRR by +0.001; (b) at the FIT-picked m = 0.08 loses held-out NL MRR (CI [-0.012, -0.003]) and takes name MRR to 0.879, below the GM-463 off floor 0.880.
- Rerank-only speed-up: p50 0.26x of fp32 at K=30 (see the throttling caveat below).

## Files (sha256 recomputed after download; match the design note)

| name | file | bytes | sha256 |
|---|---|---|---|
| ce-minilm-int8 | `onnx/model_qint8_avx512_vnni.onnx` @ 233902d2 | 23,200,716 | `3573b6b9593cb2f75987a31815d409ca3dd8808629118fd20451bb1a5d90cec7` |
| ce-minilm-quint8 | `onnx/model_quint8_avx2.onnx` @ 233902d2 | 23,200,716 | `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9` |
| ce-minilm (fp32) | `model.onnx` | | `5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a` (from the latency arms) |

The latency arms record the model sha per arm; each arm ran the file named above.

## 1. Quality (section 4.1), held-out NL and name queries, K=30, beta 40

fp32 = today's ship-L6 (chunk 16, GM-698 caches). int8 variants scored at chunk 1 (102,697 and 102,897 new pairs, 10-12 ms/pair under load).

| variant | held-out NL r@10 [CI] | held-out NL MRR [CI] | name r@10 / MRR |
|---|---|---|---|
| int8-structured (no rerank) | 0.612 [0.582, 0.642] | 0.384 [0.360, 0.409] | 0.930 / 0.768 |
| off (L6, beta 80) = GM-463 floor | 0.649 [0.620, 0.678] | 0.427 [0.401, 0.454] | 0.970 / **0.880** |
| fp32 ship-L6, beta 40 | 0.670 [0.641, 0.700] | 0.465 [0.439, 0.493] | 0.971 / 0.889 |
| **qint8 c1, beta 40** | 0.669 [0.640, 0.698] | 0.465 [0.439, 0.492] | 0.971 / 0.889 |
| quint8 c1, beta 40 | 0.675 [0.646, 0.704] | 0.463 [0.437, 0.490] | 0.971 / 0.890 |

Paired vs fp32 ship-L6 at beta 40 (b minus a):

| b | dr@10 [CI] | dMRR [CI] | name dMRR [CI] |
|---|---|---|---|
| qint8 | -0.1 pt [-0.6, +0.3] | -0.001 [-0.004, +0.003] | -0.001 [-0.003, +0.002] |
| quint8 | +0.5 pt [-0.0, +1.0] | -0.002 [-0.006, +0.001] | +0.001 [-0.002, +0.003] |

Ship rule (Q2 + Q5), beta 40, K=30:

| check | qint8 | quint8 |
|---|---|---|
| held-out NL r@10 >= 0.647 | pass (0.669) | pass (0.675) |
| held-out NL MRR >= 0.425 | pass (0.465) | pass (0.463) |
| D9 Q1-Q5 vs int8-structured at shipped floors | pass | pass |
| name MRR >= GM-463 off 0.880 (K=30) | pass (0.889) | pass (0.890) |
| dMRR CI lower >= -0.015 | pass (-0.0039) | pass (-0.006) |
| dr@10 CI lower >= -1.5 pt | pass (-0.60) | pass (~0.0) |

FIT-tuned beta (informational, decides nothing since beta 40 passes): K=30 fp32 40, qint8 40, quint8 20; K=20 all 10. Q4: quint8's quality is not better (MRR 0.463 vs 0.465, r@10 CIs overlap), so qint8.

D9 vs int8-structured passes for both at K=20 and K=30. Doc split (K=30, held-out NL): qint8 documented 0.730 / 0.553, signature-only 0.532 / 0.284 against fp32 0.728 / 0.553 and 0.538 / 0.283.

**Controls (quality):**
- Top-10 differs fp32 vs qint8 at beta 40, K=30: 1990 of 3431 queries (541 held-out NL); must be > 0: **differed**. Same at K=20: 1901 of 3431.
- C3' at chunk 1 (200 pairs rescored in other batches): max|diff| 0 for both int8 files (tol 1e-5): exact.
- Chunk-16 control (same 200 pairs at chunk 16 vs cached chunk 1): max|diff| 0.511 (qint8), 0.812 (quint8), must exceed 1e-3: **differed**. This is what makes the per-pair cache valid.
- C0 (fp32 reproduces GM-698 rows, 0 pairs scored), C1, C5, C8, C7 batch: pass.

## 2. Offline latency (section 4.2), mirrored arms a b c c b a

`latency --ks 20,30 --passes 5`, 30 queries x 5 passes = 150 calls per cell, fresh process per arm under `/usr/bin/time -p`, threads 4, Rust-like session options. L6 mean pair tokens at K=30 = 101.9 in both fp32 arms (control, pass). Start of run: `uptime` 21:42 load 4.98 9.20 14.46; `pmset` CPU_Speed_Limit 100; end of run 22:05 load 5.30 6.05 8.83, **CPU_Speed_Limit 65**.

| arm | chunk | K=20 p50 / p95 ms | K=30 p50 / p95 ms | ms/pair K=30 | CPU_Speed_Limit samples (4 per arm) | time -p real / user / sys | load start/end |
|---|---|---|---|---|---|---|---|
| fp32 #1 | 16 | 353.7 / 881.7 | 820.1 / 1243.8 | 27.27 | 100/100/48/100 | 208.4 / 786.8 / 4.8 | 2.98/7.78 |
| qint8 #2 | 1 | 112.3 / 170.8 | 213.0 / 279.7 | 6.89 | 100/100/97/100 | 55.6 / 215.9 / 1.3 | 3.71/5.44 |
| quint8 #3 | 1 | 102.8 / 149.7 | 169.7 / 239.3 | 5.57 | 100/100/82/100 | 48.7 / 187.8 / 1.3 | 5.05/9.34 |
| quint8 #4 | 1 | 103.8 / 148.7 | 175.2 / 245.2 | 5.75 | 100/100/100/100 | 49.1 / 190.3 / 1.2 | 3.03/4.68 |
| qint8 #5 | 1 | 110.0 / 148.5 | 166.1 / 226.3 | 5.49 | 100/100/100/100 | 48.3 / 188.0 / 1.2 | 3.29/4.36 |
| fp32 #6 | 16 | 293.4 / 453.2 | 642.0 / 933.9 | 20.88 | 100/100/63/100 | 154.7 / 608.4 / 3.0 | 3.10/5.30 |

user/real is 3.8-3.9 in every arm (all four threads busy; nothing was waiting).

Pooled (mean of the two arm p50s) and ratio vs fp32:

| model | K=20 p50 | x fp32 | K=30 p50 | x fp32 |
|---|---|---|---|---|
| fp32 c16 | 323.6 | 1.00 | 731.1 | 1.00 |
| qint8 c1 | 111.1 | 0.34 | 189.6 | 0.26 |
| quint8 c1 | 103.3 | 0.32 | 172.4 | 0.24 |

Adjacent pairs (#1,#2) and (#5,#6) at K=30 give 0.26 and 0.26; at K=20 0.32 and 0.37. **Caveat:** the machine throttled mid-arm in #1 (limit 48), #6 (63), #3 (82) and #2 (97), and other sessions loaded it. The two fp32 arms differ by 1.28x at K=30, so absolute numbers carry about that spread; the ratio is stable between pairs. S9 measures the product on a quiet machine. On this Ice Lake the signed qint8 file runs 10% slower than quint8 at K=30 (189.6 vs 172.4) but the arms are not clean enough to call it; the AVX2-only risk in design 2.1 is not tested here.

Planning figure for the product: p50 about 100 + 190 = 290 ms against 726 today (design assumed 410).

**Controls (latency):** per-arm model sha recorded and distinct (table in the summary: fp32 `5d3e70fd...`, qint8 `3573b6b9...`, quint8 `c80a8b34...`); ms/pair differs between files far more than the fp32 arm-pair spread (27.3 vs 20.9 fp32; 5.5-6.9 int8): **differed**.

## 3. Confident-skip simulation (section 4.3), qint8 scores, beta 40, K=30

Rules: (a) `split_words(query)` equals `split_words` of the top-1 row's last qualified-name segment (segments split on `::`, `.`, `/`, `#`, `\`); (b) cos(top-1) - cos(top-2) >= m. m picked on FIT NL (639 queries): the largest FIT skip share with FIT MRR loss <= 0.005, grid 0.005-0.30 (log in the harness output). **m = 0.08** (FIT skip share 0.138, FIT MRR loss +0.0015; m = 0.06 loses 0.0090 and fails).

| arm | held-out NL r@10 | held-out NL MRR | name r@10 / MRR | skip share held-out NL | skip share name | skip share all |
|---|---|---|---|---|---|---|
| no skip (blend) | 0.669 | 0.465 | 0.971 / 0.889 | 0 | 0 | 0 |
| (a) exact-name | 0.669 | 0.465 | 0.969 / 0.890 | 0 (0/845) | 0.342 (608/1779) | 0.177 |
| (b) margin m=0.08 | 0.668 | 0.458 | 0.968 / 0.879 | 0.138 (117/845) | 0.297 (528/1779) | 0.217 |
| (a)+(b) | 0.668 | 0.458 | 0.967 / 0.880 | 0.138 | 0.427 | 0.284 |
| skip all = int8-structured | 0.612 | 0.384 | 0.930 / 0.768 | 1 | 1 | 1 |

Paired vs no-skip: (a) held-out NL dr@10 0.0, dMRR 0.000; name dMRR +0.001 [-0.001, +0.004]. (b) held-out NL dr@10 -0.1 pt [-0.5, +0.3], dMRR -0.007 [-0.012, -0.003]; name dMRR -0.010 [-0.014, -0.006]. D9 vs int8-structured passes for every arm; GM-434 misled-share columns are in the harness table (rule (a) leaves NL columns unchanged; name CW-positive 16% -> 16%).

Effect on mean latency (rerank part x (1 - skip share)): rule (a) removes the rerank for 34% of name queries and none of NL. In this eval mix (name queries are over-represented: 1779 of 3431) that is 17.7% of all calls, about 190 ms x 0.177 = 34 ms off the mean rerank cost at K=30; the real share depends on production traffic, which this set does not represent. p50 moves only if more than half of calls skip, and they do not; p95 belongs to non-skipped (NL) calls and does not move.

**Controls (skip):** m = +inf with the name rule off equals the no-skip arm on every cell: pass. Every query skipped equals int8-structured on every outcome and cell: pass (both required by 4.3).

## 4. K=20 rows (section 4.4, informational; K=30 ships per Q3)

| variant | held-out NL r@10 | held-out NL MRR | name r@10 / MRR |
|---|---|---|---|
| fp32 ship-L6 beta 40 | 0.659 | 0.459 | 0.965 / 0.886 |
| qint8 beta 40 | 0.659 | 0.459 | 0.965 / 0.886 |
| quint8 beta 40 | 0.662 | 0.457 | 0.965 / 0.887 |
| off (L6, beta 80) | 0.644 | 0.425 | 0.964 / 0.878 |

The qint8 ship rule also passes at K=20 (dMRR CI lower -0.0030, dr@10 lower -0.45 pt, name floor 0.878). K=20 latency: qint8 p50 111 ms vs 190 at K=30.

**Control:** held-out NL queries whose top-10 differs between K=20 and K=30 (qint8, beta 40): 376 of 845 (1289 of 3431 overall); must be > 0: **differed**.

## Notes and decisions needed

- No owner decision is blocked: the rule passed at BETA 40, so no tuned beta was picked.
- Skip rule (b) fails its own quality check; the design's 2.2 "ship if it passes" therefore ships (a) alone. The name-query gain of the rerank is not lost because (a) matches the cases where the top-1 already equals the query name.
