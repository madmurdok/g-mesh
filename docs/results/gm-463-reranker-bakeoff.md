# GM-463: off-the-shelf reranker bake-off, quality x CPU latency

Design: `docs/design/GM-463-reranker-bakeoff.md` (section 1 candidates, 3.6 decision rules). Inputs: quality `eval/embedding/v2/work/s15-quality.md` (MiniLM-L12, jina-turbo) and `s19-quality.md` (gte-mb, jina-v2, bge-m3, K=50 rows), full tables in `work/s15/` and `work/s19/`; latency `work/s20-latency.md` (raw `gm463-latency/arms.jsonl`); run history `work/s18-diagnosis.md`. All quality figures: held-out NL, GM-698 ship text (E + P + B48), each model at its FIT-tuned beta, recall stage int8-structured, K=30 unless stated.

## Verdict

**No candidate passes all five pre-registered rules; MiniLM-L6 stays the shipped reranker.**
- **gte-reranker-modernbert-base** passes rules 1-4 with the largest gain (dMRR vs ship-L6 +0.047 [+0.031, +0.063], dr@10 +2.0 [+0.5, +3.6] pt) but its offline p50 at K=30 is **9.50x** MiniLM-L6's (rule 5: <= 2.0x). It fails rule 5.
- **jina-reranker-v1-turbo-en** passes rule 5 (1.16x) but fails rules 1, 2 and 4 (no MRR gain, name MRR -0.032). **MiniLM-L12** fails rules 1 and 5.
- GM-436 ceiling rule: gte-mb reaches 0.691 / 0.758 = **91.2%** of the K=30 window ceiling (>= 90%); the shipped L6 reaches 88.4%. The reranker side can get near the ceiling, but only with a model that is too slow today.

## Quality x latency

Paired deltas are candidate minus ship-L6 (K=30 beta 40), held-out NL, bootstrap 95% CI. Latency: offline Python ORT CPU EP, 4 threads, pooled p50 = mean of the two mirrored arms' p50 (30 queries x 3 passes per arm; bge 1 pass). Rules: 1 dMRR lo > 0; 2 dr@10 lo >= -1.0 pt; 3 D9 vs int8-structured passes; 4 name dMRR lo >= -0.010; 5 p50 at K=30 <= 2.0x L6.

| model (beta) | r@10 | MRR | dr@10 vs L6 (pt) | dMRR vs L6 | rules 1/2/3/4 | p50 K=30 ms | x L6 | rule 5 | licence / shippable |
|---|---|---|---|---|---|---|---|---|---|
| ms-marco-MiniLM-L6 ship (40) | 0.670 [0.641, 0.700] | 0.465 [0.439, 0.493] | - | - | (baseline) | 645.6 | 1.00 | - | Apache-2.0 / shipped |
| ms-marco-MiniLM-L12 (40) | 0.672 [0.643, 0.701] | 0.461 [0.435, 0.488] | +0.2 [-0.7, +1.1] | -0.004 [-0.010, +0.001] | FAIL / pass / pass / pass | 1504.0 | 2.33 | FAIL | Apache-2.0 / yes |
| jina-reranker-v1-turbo-en (10) | 0.670 [0.641, 0.700] | 0.469 [0.443, 0.496] | +0.0 [-1.3, +1.3] | +0.004 [-0.007, +0.015] | FAIL / FAIL / pass / FAIL | 748.9 | 1.16 | pass | Apache-2.0 / yes |
| **gte-reranker-modernbert-base (5)** | **0.691 [0.662, 0.720]** | **0.512 [0.486, 0.540]** | **+2.0 [+0.5, +3.6]** | **+0.047 [+0.031, +0.063]** | pass / pass / pass / pass | 6131.7 | 9.50 | FAIL | Apache-2.0 / yes |
| jina-reranker-v2-base-multilingual (10) | 0.686 [0.658, 0.715] | 0.478 [0.452, 0.505] | +1.6 [+0.4, +2.8] | +0.013 [+0.002, +0.024] | pass / pass / pass / pass | 4715.0 | 7.30 | FAIL | CC-BY-NC-4.0 / **no** (reference) |
| bge-reranker-v2-m3 (20), NL only | 0.683 [0.654, 0.711] | 0.487 [0.460, 0.513] | +1.2 [-0.3, +2.7] | +0.021 [+0.007, +0.036] | pass / pass / n/a / n/a | 16260.4 | 25.19 | FAIL | Apache-2.0 / reference (0.57B) |
| *K=50:* ship-L6 (40) | 0.687 [0.658, 0.716] | 0.467 [0.441, 0.494] | - | - | (K=50 baseline) | 965.5 | 1.00 (K=50) | - | |
| *K=50:* gte-mb (2.5) | 0.709 [0.680, 0.737] | 0.540 [0.513, 0.568] | +2.2 [+0.3, +4.1] | +0.073 [+0.055, +0.091] | pass / pass / pass / pass | 8685.0 | 9.00 (K=50); 13.45 vs L6 K=30 | FAIL | |
| *K=50:* jina-v2 (10) | 0.701 [0.673, 0.730] | 0.483 [0.457, 0.510] | +1.5 [+0.0, +2.9] | +0.016 [+0.004, +0.027] | pass / pass / pass / pass | 6906.8 | 7.15 (K=50); 10.70 vs L6 K=30 | FAIL | not shippable |

K=50 deltas are vs ship-L6 at K=50. L12 and jina-turbo were not run at K=50 (they fail rules at K=30); bge-m3 K=50 was excluded (cost).

Unpaired context: int8-structured (no rerank) 0.612 / 0.384; off (L6, structured text, beta 80) 0.649 / 0.427. Every candidate beats off; gte-mb vs off: dr@10 +4.2 [+2.3, +6.1], dMRR +0.085 [+0.068, +0.103]. Strict held-out deltas equal held-out deltas within 0.001 for every model.

### Latency at K=20, 30, 50 (pooled p50 / p95 ms, ratio to L6 at the same K)

p95 pooled the same way as p50 (mean of the two arms' p95; computed from the per-arm table in `s20-latency.md`).

| model | K=20 p50 / p95 | K=30 p50 / p95 | K=50 p50 / p95 | mean pair tokens (latency set, K=30) |
|---|---|---|---|---|
| MiniLM-L6 | 364.5 / 645.5 | 645.6 / 927.0 | 965.5 / 1331.7 | 101.9 |
| MiniLM-L12 | 741.8 (2.03x) / 1367.2 (2.12x) | 1504.0 (2.33x) / 2227.0 (2.40x) | 2097.7 (2.17x) / 3099.5 (2.33x) | 101.9 |
| jina-turbo | 429.1 (1.18x) / 861.4 (1.33x) | 748.9 (1.16x) / 1117.5 (1.21x) | 1030.0 (1.07x) / 1417.7 (1.06x) | 98.3 |
| gte-mb | 3327.5 (9.13x) / 4946.8 (7.66x) | 6131.7 (9.50x) / 8613.5 (9.29x) | 8685.0 (8.99x) / 12124.5 (9.10x) | 104.4 |
| jina-v2 | 2596.8 (7.12x) / 4072.4 (6.31x) | 4715.0 (7.30x) / 6879.4 (7.42x) | 6906.8 (7.15x) / 9478.8 (7.12x) | 110.6 |
| bge-m3 | 9418.9 (25.84x) / 13940.2 (21.60x) | 16260.4 (25.19x) / 23766.9 (25.64x) | 23158.1 (23.98x) / 33192.9 (24.93x) | 110.6 |

Mean pair tokens on the quality set (K=30): L6 77.1, L12 77.1, jina-turbo 72.6, gte-mb 76.6, jina-v2 82.0, bge-m3 93.7 (NL scope). Token counts are within ~10% of L6 except bge, so the latency ratios are model cost, not longer inputs.

**Machine state (S20, 2026-11-08 07:13-11:06 +0400, under `caffeinate -i -s`):** 4 physical cores (i7-1068NG7, 32 GB); 1-min load 2.8-3.6 at every arm start (gate < 4 held 30 s, never hit the 1800 s cap); `CPU_Speed_Limit` 100 at every arm start but **51-95 at arm end** (L6 arms 73 and 51, gte arms 58 and 56); user/real 3.81-3.94 in every arm (computing, not waiting). Absolute ms are therefore inflated by thermal throttling, more for longer arms; the mirrored order (`L6 L12 turbo gte jv2 bge bge jv2 gte turbo L12 L6`) balances it, and **the pooled ratios are the robust figure, not the ms.** Rough cross-check against the product path: gm-698-latency.md's enr minus off p50 is 783.4 - 103.7 = 679.7 ms (that difference also includes text building and source reads), the same order as offline L6's 645.6 ms.

## Per-rule verdicts (section 3.6)

| model | 1 dMRR lo > 0 | 2 dr@10 lo >= -1.0 | 3 D9 vs int8-structured (Q1 dr@10 [lo] / Q2 dMRR [lo]) | 4 name dMRR lo >= -0.010 | 5 <= 2.0x | all |
|---|---|---|---|---|---|---|
| MiniLM-L12 | FAIL (-0.004 [-0.010, +0.001]) | pass (lo -0.7) | pass (+6.1 [+4.2] / +0.078 [+0.060]) | pass (-0.001 [-0.006, +0.004]) | FAIL (2.33x) | no |
| jina-turbo | FAIL (+0.004 [-0.007, +0.015]) | FAIL (lo -1.3) | pass (+5.9 [+3.9] / +0.086 [+0.066]) | FAIL (-0.032 [-0.041, -0.024]) | pass (1.16x) | no |
| gte-mb | pass (+0.047 [+0.031, +0.063]) | pass (lo +0.5) | pass (+7.9 [+5.9] / +0.129 [+0.110]) | pass (+0.014 [+0.006, +0.023]) | FAIL (9.50x) | no |
| jina-v2 (not shippable) | pass (+0.013 [+0.002, +0.024]) | pass (lo +0.4) | pass (+7.4 [+5.6] / +0.095 [+0.075]) | pass (+0.006 [-0.000, +0.013]) | FAIL (7.30x) | no |
| bge-m3 (NL only) | pass (+0.021 [+0.007, +0.036]) | pass (lo -0.3) | pass (+7.1 [+5.1] / +0.103 [+0.084]) | n/a (names not scored) | FAIL (25.19x) | no |

Ship-L6 for reference: D9 Q1 +5.9 [+4.1], Q2 +0.082 [+0.064], pass. D9 vs jina fp32 (full text) fails Q5 for every row, ship-L6 and int8-structured included, as in GM-698; it is not a rule here. The rule-3 gate is vs int8-structured at the shipped floors.

**Name queries** (r@10 / MRR; ship-L6 0.971 / 0.889): L12 0.972 / 0.888; jina-turbo 0.967 / 0.857; gte-mb 0.975 / 0.904; jina-v2 0.972 / 0.896; bge-m3 not scored (its row keeps int8-structured order, 0.930 / 0.768). gte-mb is the only model with a name-MRR gain whose CI excludes 0 (+0.014 [+0.006, +0.023]; at K=50 +0.038 [+0.028, +0.048]). jina-turbo's name loss (-0.032) is what sinks it beyond the flat NL result.

**Documented vs signature-only** (n = 426 / 236 / 19 mixed; candidate minus ship-L6, dr@10 pt / dMRR; reported, not gated). Ship-L6 itself: documented 0.728 / 0.553, signature-only 0.538 / 0.283.

| model | documented | signature-only | mixed |
|---|---|---|---|
| MiniLM-L12 | +1.0 [+0.1, +2.1] / -0.006 [-0.013, +0.000] | -1.4 [-3.9, +0.8] / -0.001 [-0.010, +0.009] | +0.0 / -0.007 |
| jina-turbo | +0.9 [-0.4, +2.3] / +0.007 [-0.006, +0.020] | -2.5 [-5.4, +0.4] / +0.003 [-0.013, +0.019] | +0.0 / -0.026 |
| gte-mb | -0.1 / +0.011 [-0.007, +0.030] | **+4.7 [+2.3, +7.2] / +0.111 [+0.082, +0.142]** | 0 / +0.029 [-0.013, +0.083] |
| jina-v2 | +1.6 [+0.3, +3.0] / +0.013 [-0.000, +0.027] | +0.5 / +0.023 [+0.004, +0.046] | 0 / -0.010 |
| bge-m3 | +1.4 / +0.006 [-0.012, +0.025] | +1.1 / +0.054 [+0.029, +0.081] | 0 / +0.052 [+0.005, +0.107] |
| gte-mb K=50 | dMRR +0.041 [+0.020, +0.063] | dMRR +0.131 [+0.100, +0.163] | |

gte-mb's gain is almost all on **signature-only** nodes, the bucket where L6 is weakest (MRR 0.283 vs 0.553 documented). On documented nodes no model separates from L6 on MRR.

**Flips** (int8-structured top right->wrong / wrong->right; NL held-out n=482, name n=848): L6 19/64, 5/123; L12 21/60, 5/123; jina-turbo 24/69, 21/96; gte-mb 12/79, 3/146; jina-v2 21/69, 11/134; bge-m3 18/76 (names untouched). gte-mb breaks the fewest correct tops and fixes the most, on both query types.

**FIT-tuned beta:** L6 40, L12 40, jina-turbo 10, gte-mb 5 (K=50: 2.5), jina-v2 10, bge-m3 20. All optima are interior to `R.BETAS` (grid includes 0, 1, 2.5, 5), so the edge-extension rule never fired. At the shared beta 40, gte-mb drops to 0.655 / 0.420 (below ship-L6): its logit scale is smaller, so beta must be tuned per model.

### Window ceiling and the GM-436 rule

Ceiling = share of held-out NL positives (n=681) with an expected node in int8-structured's top K: **K=20 0.715, K=30 0.758, K=50 0.803.** 90% of the K=30 ceiling = 0.682.

| model | r@10 K=30 | share of K=30 ceiling |
|---|---|---|
| ship-L6 | 0.670 | 88.4% |
| MiniLM-L12 | 0.672 | 88.7% |
| jina-turbo | 0.670 | 88.4% |
| **gte-mb** (best shippable) | **0.691** | **91.2%** |
| jina-v2 (not shippable) | 0.686 | 90.5% |
| bge-m3 | 0.683 | 90.1% |

Verdict by the pre-registered rule: the best shippable model (gte-mb) reaches >= 90%, so **for the reranker side, GM-436 is unnecessary on r@10**: the remaining r@10 loss is mostly recall (positives outside the window), which only the embedding side moves. Two qualifiers: (a) gte-mb fails rule 5, and the model that ships today (L6) is at 88.4%, below the bar; (b) the rule is about r@10, while MRR still has room (gte-mb +0.047 over L6, +0.073 at K=50), i.e. ordering inside the top 10 is not saturated. bge-m3 passes rule 1, so the clause "if even bge-m3 does not pass rule 1, size is not the lever" does not fire; but the 150M gte-mb has higher point estimates than the 568M bge-m3 on every metric (not a paired comparison), so training recipe matters at least as much as size.

## Recommendation for GM-700 and GM-436

What gte-mb would need for rule 5: p50 at K=30 must fall from 9.50x to <= 2.0x L6, a **4.75x speed-up** relative to fp32 L6 (6131.7 ms -> <= 1291 ms offline). If GM-700 also makes L6 itself faster by a factor s (int8 L6), the bar moves with it: gte must then be 4.75 x s faster than fp32 gte. A smaller window is not free: at K=20 gte-mb is still 5.15x L6's K=30 p50 (needs 2.58x more) and **fails rule 2 at K=20** (dr@10 -0.4 [-1.9, +1.1] vs ship-L6 K=20). A confident-skip lowers mean latency by the skipped share but moves p50 only if more than half of queries skip, and leaves p95 to the non-skipped queries. Int8 speed-ups were **not measured here** (the harness has no int8 switch; S20's optional int8 rows were not done).

| option | benefit | risk |
|---|---|---|
| **A. Keep L6; GM-700 speeds L6 up as planned (int8 L6 + confident-skip) and adds one gte-mb probe: int8 gte latency (and its quality if latency is close) vs the 4.75x bar** (recommended) | No regression; product latency improves anyway; one cheap measurement settles whether the +0.047 MRR model is reachable off the shelf | The probe probably misses: int8 + skip must together give >= 4.75x on p50 (more if L6 also goes int8); int8 may cost quality, which needs a re-score |
| B. Keep L6; skip the gte probe; GM-436 distils gte-mb (teacher) into an L6-size cross-encoder on code pairs | Targets the gain where it lives (signature-only nodes) at L6 cost; no latency bar problem | Needs a training environment (no torch on this machine), labelled/teacher-scored pairs and a new eval; weeks, not days; gain not guaranteed to transfer |
| C. Relax rule 5 / ship gte-mb as an opt-in "quality" rerank | +0.047 MRR, +2.0 pt r@10 available now | ~6 s offline p50 per query at K=30 (throttled machine; still ~9.5x L6): unusable for interactive search; a second model download (~600 MB fp32) |

**Recommended: A, with B as the follow-up if A's probe misses the 4.75x bar.** Reason: it costs one measurement inside a task already planned, and its outcome decides between shipping gte-mb (A hits) and distilling it (A misses) on data rather than estimates.

## Owner decision

2026-11-08, answer typed by the owner (no option label):

> "gte int8 - проверить в gm-700, так же давай попробуем дистиллировать gte а gm-436"

So both A and B: GM-700 adds the int8 gte-mb probe against the 4.75x bar; GM-436 distils gte-mb into an L6-size cross-encoder regardless of the probe's outcome. MiniLM-L6 stays the shipped reranker until one of them passes rules 1-5.

## Method and controls

Data: `eval/embedding/v2` (1652 NL + 1779 name queries); FIT / held-out split `R.held_out`; beta tuned on FIT NL by `E.fit_obj`; bootstrap as `E.main`. Driver `eval/embedding/gm463_bakeoff.py` (`quality`, `latency`); `gm462_ce_enriched.py` and `gm464_latency.py` unchanged. Owner decisions (design note, 2026-11-05): Q1 NL-only for bge-m3 at K=30; Q2 jina-v2 as reference only; Q6 rule-5 bar 2.0x L6; Q3, Q4, Q5, Q7 as recommended.

| control | what it checks | result |
|---|---|---|
| C0 | ship-L6 reproduces gm-698-ship-arm.md: beta 40, r@10 0.670 [0.641, 0.700], MRR 0.465 [0.439, 0.493], name 0.971 / 0.889, off 0.649 / 0.427, 0 pairs scored | pass in all 5 K=30 runs (L12, turbo, gte, jina-v2, bge) |
| C1 | Python structured-text port vs Rust fixture; build(all switches off) == structured text | 20/20 equal; 12839/12839 and 14117/14117 window nodes equal: pass in every run |
| C5 | rerank disabled == int8-structured on every outcome and cell | pass in every run |
| C3' | 200 cached ship pairs rescored in halves (fp32 batch independence; tol 1e-4, 1e-5 for L6) | max abs diff 0 for every model except gte-mb 7.153e-07: pass |
| C7 | card known answer | L12 4.747e-06 (tol 1e-3), gte-mb 0.004645 (tol 0.05), bge-m3 0.003691 (tol 0.05): pass; jina-turbo and jina-v2 **rank only** (cards print no scores): relevant docs on top, pass (weaker); L6 n/a (C0 covers it); batch variant max diff 0 for all |
| C8 | ONNX input names, pair template, no pad inside an unpadded encoding, 700 tokens -> 512 | pass for all six models |
| separation | each candidate's ranking differs from ship-L6's | flip counts differ for every model (above) |
| token control | L6 mean pair tokens on the latency set at K=30 = gm-698-latency.md's 101.9 | 101.9 in both L6 arms: pass |
| sleep check | `pmset -g log` has no Sleep entry inside any run window | S19: none in 13:51-01:03 and 01:23-06:23 (one Idle Sleep at 01:04 fell in the gap between stages); S20: none in 07:13-11:06 (one DarkWake at 09:16, arms kept user/real ~3.9) |

### Run history

- **S15** ran L12 and jina-turbo at K=30 (quality valid; C0, C1, C5, C3', C7, C8 pass). Its gte-mb run scored at 0.5-3 s/pair against a 136 ms dry run and was stopped by the owner after ~41k of ~103k pairs; jina-v2, bge-m3 and K=50 were not run.
- **S18 diagnosis:** the Mac idle-slept for most of the run (`pmset` `sleep 1`, no sleep assertion held). Wall time per pair rose while compute per pair did not; throttling (`CPU_Speed_Limit` 51-78) added ~1.6x. The same cause inflated S15's L12 rate: 69.9 ms/pair wall includes 58.0 min of sleep, (7179 - 3480) s / 102,677 pairs = 36 ms/pair awake, matching the dry run. So **S15's gte and L12 rates were inflated by idle sleep, and ms/pair from quality runs are throughput under shared load, not latency.** Quality numbers are unaffected (scores are cached per pair and batch independent, C3'). Fix: every long run under `caffeinate -i -s`.
- **S19** re-ran under caffeinate: gte-mb resumed from cache (49,397 remaining pairs, 128 ms/pair), jina-v2 (102,697 pairs), bge-m3 NL only (49,339 pairs, 471 ms/pair), then gte-mb and jina-v2 at K=50. Start load was ~92 (other jobs) and `CPU_Speed_Limit` fell to 63-75 late in the runs. A stage-1 script bug read the K=20 rules table and wrongly skipped K=50; fixed to read K=30 and K=50 was run.
- **S20** latency run as described above; rule 5 comes only from S20.

### Limitations

- **Offline latency, not product latency.** Python ORT 1.19.2 vs the product's ort 2.0.0-rc.9; excludes recall, text building and source reads. Only L6 can run through `rerank.rs` today (pinned sha256, `token_type_ids` always fed, const WINDOW 30). Ratios between models are the comparable figure.
- **Throttling:** `CPU_Speed_Limit` 51-95 at arm ends; absolute ms are upper bounds. bge-m3 had 1 timed pass per arm (n=30 per K, not 90).
- **jina C7 is rank-only** (no card scores), weaker than the logit checks.
- **bge-m3 is NL only** (owner Q1): no name-query rule 4, no K=50.
- **No int8 rows** (latency or quality); GM-700 owns int8.
- L12 and jina-turbo have no K=50 rows. The gte-mb vs bge-m3 comparison is unpaired.
