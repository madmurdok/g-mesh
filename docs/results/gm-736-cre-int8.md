# GM-736: CodeRankEmbed int8 recipe (results)

Spec: `docs/design/GM-736-cre-int8.md`. Branch `feat/GM-736-cre-int8-recipe`, tip 3d5fd354. Driver `eval/embedding/gm736/measure.sh`, exit 0 (both weight forms passed the gate at R1; R2 and R3 were not run, rule G1). QAT is GM-737's (D3).

Run: 2026-11-22 19:04 to 2026-11-23 03:31, `/usr/bin/time -p` real 30443 s (8.5 h), user 70689 s, sys 618 s. The machine was shared with other agents' test suites: load average 15 to 380 over the run (19:04 start 379; 22:56 15.7; 03:31 121). The quality numbers do not depend on load; the cost numbers do (section 5).

Model dirs: `g-mesh-data/gm736/models/coderankembed-int8-sq-{s7,u8}`. Arms: `coderankembed-int8sq-structured` (s7), `coderankembed-int8squ8-structured` (u8).

## 1. Q-D for every recipe tried

R1 alpha sweep on the selection sample (disjoint from the gate sample); the best alpha is 0.7 for both forms, the largest value of the grid (the optimum may lie above 0.7; not explored, the design fixes the grid).

| weights | alpha | selection median | selection p1 | selection min | size B |
|---|---|---|---|---|---|
| s7 | 0.3 | 0.9724 | 0.9410 | 0.9359 | 139242905 |
| s7 | 0.4 | 0.9854 | 0.9652 | 0.9648 | 139242905 |
| s7 | 0.5 | 0.9917 | 0.9814 | 0.9767 | 139242905 |
| s7 | 0.6 | 0.9945 | 0.9880 | 0.9867 | 139242905 |
| s7 | **0.7** | 0.9960 | 0.9914 | 0.9903 | 139242905 |
| u8 | 0.3 | 0.9738 | 0.9410 | 0.9386 | 139362713 |
| u8 | 0.4 | 0.9874 | 0.9678 | 0.9657 | 139362713 |
| u8 | 0.5 | 0.9928 | 0.9815 | 0.9805 | 139362713 |
| u8 | 0.6 | 0.9954 | 0.9890 | 0.9890 | 139362713 |
| u8 | **0.7** | 0.9969 | 0.9924 | 0.9917 | 139362713 |

Gate (D4) on the frozen parity sample (n=162, sha256 48908b01...), evaluated once per arm at alpha 0.7:

| arm | median (>= 0.99) | p1 (>= 0.9290) | min | size (<= 161895621 B) | Q-F | verdict |
|---|---|---|---|---|---|---|
| s7, alpha 0.7 | 0.9957 | 0.9894 | 0.9888 | 139.2 MB | ok | pass (G1) |
| u8, alpha 0.7 | 0.9967 | 0.9906 | 0.9898 | 139.4 MB | ok | pass (G1) |
| jina int8 (reference) | 0.9905 | 0.9490 | 0.9357 | 161.9 MB | | |

R2 (mixed precision) and R3 were not run: the stop rule G1 applies. Rust-vs-Python query parity (P3, reported, not gated): min cosine 0.9965 to 0.9974 per corpus for s7 (`p3-*.json`), 0.9971 on gin for u8.

## 2. D9 verdict vs jina int8

From `s6-report.json` / `s6-report.out`, verbatim:

```
coderankembed-int8sq-structured r@10 0.670  mrr 0.453  verdict Fail(["Q5"])
winner: none - keep jina-v2-base-code-int8-structured
coderankembed-int8squ8-structured r@10 0.676  mrr 0.452  verdict Fail(["Q5"])
winner: none - keep jina-v2-base-code-int8-structured
```

Validity: controls (random, shuffled) and floor parity pass. Q1 to Q4 pass for both arms; Q5 fails.

| gate | s7 | u8 |
|---|---|---|
| Q1 r@10 lower bound | +4.2 pts PASS | +4.7 pts PASS |
| Q2 MRR lower bound | +0.057 PASS | +0.056 PASS |
| Q3 worst language | typescript +3.3 PASS | go +3.3 PASS |
| Q4 confident-wrong delta | -19.5 pts PASS | -16.8 pts PASS |
| Q5 false-alarm delta | +5.7 pts, upper +10.1 FAIL | +4.9 pts, upper +9.4 FAIL |
| C-quality (reported by s9 with costs) | PASS | FAIL (pass time 1.62x, see section 5) |

## 3. Pooled table (languages weighted equally)

| arm | r@1 | r@10 [lo, hi] | r@100 | MRR [lo, hi] | CW comb / pos / abs % | false alarm go/py/rust/ts % |
|---|---|---|---|---|---|---|
| CRE int8 s7 | 0.344 | 0.670 [0.649, 0.691] | 0.895 | 0.453 [0.435, 0.472] | 42.0 / 45.7 / 26.8 | 11.6 / 11.8 / 22.2 / 21.2 |
| CRE int8 u8 | 0.344 | 0.676 [0.655, 0.696] | 0.895 | 0.452 [0.433, 0.471] | 45.0 / 48.3 / 31.1 | 13.0 / 13.7 / 22.2 / 12.5 |
| CRE fp32 | 0.346 | 0.673 [0.653, 0.694] | 0.897 (GM-703) | 0.453 [0.435, 0.472] | 45.6 / 48.6 / 32.9 | 15.5 / 12.0 / 25.0 / 11.2 |
| jina int8 | 0.269 | 0.608 [0.587, 0.630] | 0.866 | 0.380 [0.362, 0.397] | 61.4 / 63.7 / 51.8 | 4.1 / 0.0 / 8.7 / 6.1 |
| J (gm730, informational) | 0.385 | 0.733 [0.714, 0.753] | not read | 0.501 [0.483, 0.520] | 49.0 / 48.2 / 52.4 | 2.4 / 7.4 / 5.8 / 6.6 |

r@10 by language (go / python / rust / typescript): s7 0.715 / 0.700 / 0.552 / 0.715; u8 0.712 / 0.700 / 0.561 / 0.730; fp32 0.712 / 0.712 / 0.558 / 0.712; jina 0.679 / 0.658 / 0.415 / 0.682.

Int8 minus fp32 (6c): s7 r@10 -0.3 pt, MRR 0.000; u8 r@10 +0.3 pt, MRR -0.001 (both inside the intervals).

## 4. Q5 per language

Own held-out false-alarm rate, with the Q5 delta vs jina int8 (n, [lo, hi]); fp32 rates are GM-703's.

| language | s7 own FA | s7 delta | u8 own FA | u8 delta | fp32 own FA | jina own FA |
|---|---|---|---|---|---|---|
| go | 11.6% | 0.0 pts (36, [-5.6, 5.6]) | 13.0% | 0.0 pts (37, [-5.4, 5.4]) | 15.5% | 4.1% |
| python | 11.8% | +9.7 pts (31, [3.2, 19.4]) | 13.7% | +13.8 pts (29, [3.4, 24.1]) | 12.0% | 0.0% |
| rust | 22.2% | 0.0 pts (20, [-10.0, 10.0]) | 22.2% | 0.0 pts (20, [-10.0, 10.0]) | 25.0% | 8.7% |
| typescript | 21.2% | +13.2 pts (53, [5.7, 20.8]) | 12.5% | +5.8 pts (52, [1.9, 11.5]) | 11.2% | 6.1% |
| pooled | | +5.7 pts [1.3, 10.1] | | +4.9 pts [0.6, 9.4] | | |

GM-703 fp32 pooled Q5 delta: +4.7 pts, upper +9.2. int8 does not move it beyond noise.

## 5. Costs (cost pass, corpus g-mesh, 3 interleaved rounds, medians)

| arm | pass s | x jina | max RSS MB | x jina | size MB | x jina | query ms | x jina |
|---|---|---|---|---|---|---|---|---|
| s7 | 508.8 | 0.96 | 417 | 0.80 | 140.0 | 0.85 | 31.77 | 1.65 |
| jina (s7 pass) | 532.2 | 1.00 | 519 | 1.00 | 164.5 | 1.00 | 19.28 | 1.00 |
| u8 | 812.3 | 1.62 | 441 | 0.82 | 140.1 | 0.85 | 37.84 | 3.90 |
| jina (u8 pass) | 500.1 | 1.00 | 538 | 1.00 | 164.5 | 1.00 | 9.70 | 1.00 |

Load caveat: `measure_costs.sh` gates timing at load < 7 but proceeds with a WARN after 5 min; the gate fired 4 times (s7 pass) and 6 times (u8 pass) and the machine never stayed quiet (load after each pass 10 to 450; "redo" fired repeatedly). No wrapper change was needed or made; the gate exists. The u8 pass ratio (1.62x, a C-quality fail) is not trustworthy: u8 round 2 ran at load 268 after and jina round 3 ran at load 459 (2137 s). User CPU time per run (the load-robust figure) is 1950 to 3400 s for CRE and 1740 to 2375 s for jina, with both models BERT-base size. The query-ms ratio is also load-affected (jina 19.3 ms in one pass, 9.7 ms in the other). Costs are informational as in GM-703; the size (0.85x jina, inside D5) and RSS (0.80x) numbers are reliable. A quiet-machine re-run of the cost pass is needed before any latency claim.

## 6. J comparison (informational)

`report --reference gm730-jina-full-int8-structured`: validity FAILED (reference floors go 0.49, python 0.55, rust 0.53 are not within 0.03 of shipped 0.57/0.59/0.57), so both arms' verdicts are `Undecided`, as expected for a fine-tuned reference. The fp32 reference (6c) has the same validity failure (floors 0.44 to 0.45), so 6c is a delta table, not a verdict.

| | r@10 | MRR |
|---|---|---|
| J | 0.733 | 0.501 |
| CRE int8 s7 | 0.670 (-6.3 pts) | 0.453 (-0.048) |
| CRE int8 u8 | 0.676 (-5.7 pts) | 0.452 (-0.049) |

## 7. Pre-registered expectations

1. Int8 loss vs fp32 within -1.0 pt r@10 and -0.010 MRR: **held** (s7 -0.3 / 0.000; u8 +0.3 / -0.001). Q-D (median 0.996) predicted quality.
2. Q5 still fails: **held** for both arms; not a fix.
3. J stays ahead (0.733 / 0.501): **held**. No ship claim: adoption needs Q5 fixed (GM-737 or a floor task).
4. Latency about 1.0x jina: **not confirmed**; the load-contaminated pass (s7 0.96x, u8 1.62x) is inconclusive.
5. Gate G1 (R1 passes): **held** at alpha 0.7 for both forms, both far above the 0.99 median bar; alpha 0.7 is the grid edge.

## 8. Conclusion

R1 smoothing makes CodeRankEmbed int8 equal to fp32 on eval v2 at 140 MB (0.85x jina int8, within D5). The D9 verdict vs jina int8 stays `Fail(["Q5"])` for both weight forms; the winner is none, keep jina int8. Choice between s7 and u8 is not separable on quality (r@10 0.670 vs 0.676, intervals overlap); s7 is the shipped weight form. Raw outputs: `g-mesh-data/gm736/measure/`.
