# GM-462: richer reranker input, measure (S13)

Design: `docs/design/GM-462-richer-reranker-input.md`. Driver: `eval/embedding/gm462_ce_enriched.py`.
Setup A (shipped F4, K=30), eval set v2 (`eval/embedding/v2`, work dir GM-460's, read-only except its `rerank_cache`). Arms: off, all (E I P B), and leave-one-out -E -I -P -B (authored NL only). Body budget 96. Beta 80 and FIT-tuned beta per arm (FIT half of v2).

## Verdict

**no effect, by the pre-fixed rule, but only because of its D9 clause.** all minus off at the tuned betas (all beta 40, off beta 80), held-out NL: dr@10 +2.5 [+1.0, +4.0], dMRR +0.030 [+0.017, +0.043]. Both lower bounds are above 0, so the metric clauses of "helps" hold. D9 vs int8-structured passes for every row. D9 vs fp32 fails Q5 for every row, including int8-structured with no rerank and off (F4 today): that is the floor-parity finding of GM-460 (shipped floors fall short on v2), not an effect of the richer text. The rule as written needs D9 vs fp32 to pass, so the driver prints "no effect". The owner should decide whether to read the D9 clause against fp32 as "no worse than off" (then: helps). The body-48 row was not run: the owner's decision conditions it on an effect under the rule. It is ~15k pairs, ~7 min if wanted.

Strict held-out view (queries sharing an expected symbol with a FIT query dropped) gives numbers identical to the plain held-out view for off vs all.

## Controls

| # | result |
|---|---|
| C0 | builder unit tests: V-slice's (not rerun here) |
| C1 | 12839/12839 window nodes build(all off) == structured text; Python port vs Rust fixture 20/20 |
| C2 | gm464 cache holds every off pair: 102930/102930 |
| C3 | 200 off pairs rescored: max abs diff 0.000e+00 (onnxruntime 1.19.2, tokenizers 0.22.2, model sha256 5d3e70fd...d4a) |
| C4a | off row reproduces GM-460's `F4s K=30 beta=80 (FIT-tuned), CE on structured text` (6 lines): MATCH, off tuned beta 80 |
| C4b | off row on the GM-398 id subset reproduces GM-464 S1's row: MATCH, tuned beta 80 |
| C5 | rerank disabled == int8-structured on every outcome and cell: True |
| C6 | all-arm text != base on 12839/12839 nodes; all row differs from off: True |

Driver fix (small, in this commit): C4a read GM-460's results doc for a table row, but that doc publishes the F4 row only in prose (line 272), so the first run STOPPED at C4 after scoring everything. `--c4-doc` now also feeds C4a on a non-current eval dir (it used to replace C4a and skip C4b). C4a was run against GM-460's table output (`scratchpad/gm460/v2-ce.txt`, the rendered `F4s` rows the doc's prose quotes), not a committed file. The rerun used the cached scores (78 s).

## Results (v2 held-out NL, 95% bootstrap)

| variant | r@10 | MRR | name r@10 / MRR |
|---|---|---|---|
| int8-structured (no rerank) | 0.612 [0.582, 0.642] | 0.384 [0.360, 0.409] | 0.930 / 0.768 |
| off, beta 80 (F4) | 0.649 [0.620, 0.678] | 0.427 [0.401, 0.454] | 0.970 / 0.880 |
| all, beta 40 | 0.674 [0.645, 0.703] | 0.457 [0.431, 0.483] | 0.973 / 0.877 |
| all, beta 80 | 0.670 [0.641, 0.699] | 0.443 [0.418, 0.469] | 0.967 / 0.850 |
| -E, beta 40 | 0.680 | 0.454 | |
| -I, beta 40 | 0.680 | 0.470 | |
| -P, beta 40 | 0.673 | 0.448 | |
| -B, beta 40 | 0.661 | 0.441 | |

D9 vs int8-structured at shipped floors: off Q1 +3.7, Q2 +0.043, Q4 -2.8, Q5 +0.0; all (beta 40) Q1 +6.2 [+4.4], Q2 +0.073 [+0.055], Q4 -4.0 [-2.3], Q5 +0.0 [+0.0]; pass. Q3 worst language: all python +3.2 (beta 80), typescript +2.2 (beta 40). vs fp32: all Q1 +5.8, Q2 +0.075, Q4 -22.0, Q5 +9.6 [+13.8]; Q5 fails, as for every row incl. off (+9.9) and int8-structured (+11.1).

GM-434 (NL misled / CW pos / CW absent; name misled / CW pos / CW absent): int8-structured 22% (40/184) / 50% (338/681) / 23% (37/164); 2% (9/594) / 30% (263/879) / 20% (184/900). off 24% (52/220) / 46% (314/681) / 23% (37/164); 3% (22/714) / 18% (156/879) / 20% (184/900). all beta 40 25% (60/238) / 45% (304/681) / 23% (37/164); 3% (22/715) / 18% (155/879) / 20% (184/900). Flips (right to wrong / wrong to right), NL held-out: off 14 / 38, all 21 / 55 (n=482); name: off 1 / 108, all 3 / 111 (n=848).

Mean tokens per window node: off 38.4, all 110.2, -E 106.4, -I 101.2, -P 102.8, -B 58.5.

Beta: all arm peaks at 40 on FIT (held-out r@10 0.674 at 40, 0.679 at 20, 0.670 at 80), so the richer text wants a smaller beta than 80. Full sweep in the tables file.

### Leave-one-out (paired vs all, held-out NL, tuned betas)

| arm | dr@10 [lo, hi] | dMRR [lo, hi] | reading |
|---|---|---|---|
| -E | +0.6 [-0.0, +1.3] | -0.003 [-0.008, +0.003] | E adds nothing measurable |
| -I | +0.6 [-0.3, +1.4] | +0.013 [+0.006, +0.020] | removing I helps MRR: I hurts |
| -P | -0.1 [-1.0, +0.8] | -0.009 [-0.015, -0.003] | P carries a small gain |
| -B | -1.3 [-2.5, -0.2] | -0.016 [-0.026, -0.007] | B (body) carries the most |

### Buckets (held-out NL positives, off vs all at tuned betas)

| bucket | scope | n | off r@10 / MRR | all r@10 / MRR | dr@10 [lo, hi] | dMRR [lo, hi] | median logit off / all | expected nodes < -8 off / all |
|---|---|---|---|---|---|---|---|---|
| documented | all | 426 | 0.710 / 0.506 | 0.737 / 0.539 | +2.7 [+1.1, +4.4] | +0.033 [+0.016, +0.050] | -2.98 / -1.64 | 92 / 71 |
| documented | typescript | 83 | 0.783 / 0.676 | 0.783 / 0.694 | +0.0 [+0.0, +0.0] | +0.018 [-0.014, +0.052] | -0.94 / -1.15 | 12 / 14 |
| signature-only | all | 236 | 0.489 / 0.261 | 0.515 / 0.291 | +2.6 [-0.8, +6.3] | +0.030 [+0.010, +0.050] | -9.89 / -7.71 | 127 / 82 |
| signature-only | typescript | 98 | 0.643 / 0.396 | 0.714 / 0.407 | +7.1 [+2.0, +12.2] | +0.011 [-0.020, +0.044] | -10.03 / -7.86 | 60 / 39 |
| mixed | all | 19 | 0.767 / 0.662 | 0.767 / 0.652 | +0.0 [+0.0, +0.0] | -0.011 [-0.024, -0.000] | -5.16 / -3.30 | 9 / 8 |

The signature-only bucket is where S8's mechanism shows: the median logit of the best expected node rises from -9.89 to -7.71 and nodes under -8 fall from 127 to 82. The documented bucket gains as much in r@10 and MRR, so the gain is not confined to signature-only. Typescript signature-only: r@10 +7.1 [+2.0, +12.2], MRR not significant.

Cheapest LOO arm within noise of all: the driver prints none (it names one only under "helps"). By the table: -E is within noise on both metrics and costs 3.8 fewer tokens; -I improves MRR; both would be port candidates, -B and -P are not droppable.

## Cost and machine state

Dry run (gin): 19.64 ms/pair on 200 new pairs, ETA 5733 s; actual full scoring 291,771 pairs: all 26.3, -E 27.9, -I 26.0, -P 27.1, -B 16.7 ms/pair, 7452 s real (`/usr/bin/time -p`: user 29084, sys 156; user/real ~3.9, so CPU-bound on ~4 cores, shared machine). Start 03:47, load 4.94 5.56 7.11, `pmset -g therm`: no thermal or performance warning, CPU_Speed_Limit 100. The first run's end state was not recorded (stopped by C4); it finished about 05:51 (second run start 05:52, load 4.49 6.43 7.47, same thermal state). Not a latency result; ms/pair are only ETA inputs.

Not done in this slice: M4 (enclosing-prefix counts per language); the verify slice owns the builder's field rules.

## Port task draft (not created; the verdict above awaits the owner's reading of the D9 clause)

"Port the enriched reranker text to the product: build the CE text with the E (enclosing), P (path/name words) and B (96-token body) fields, and the I field only if re-measured, on top of the structured header; re-tune the rerank beta (measured optimum 40 on v2, not 80). B carries the gain per the leave-one-out (-B: dMRR -0.016 [-0.026, -0.007]), then P; E and I are within noise or negative. Gate: median added latency per query at K=30 against the 2.7x token growth (110 vs 38 tokens per node; ~26 ms/pair here vs 9.6 ms), and D9 vs int8-structured on v2 at the shipped floors."

## Tables

Full driver output (all arms, D9 vs fp32 and int8-structured, GM-434, flips, beta sweep):

| variant | held-out NL r@10 [lo, hi] | held-out NL MRR [lo, hi] | name r@10 / MRR |
|---|---|---|---|
| jina fp32, full text (baseline, D6 floors) | 0.616 [0.586, 0.646] | 0.381 [0.358, 0.406] | 0.917 / 0.745 |
| int8-structured (no rerank, shipped floors) | 0.612 [0.582, 0.642] | 0.384 [0.360, 0.409] | 0.930 / 0.768 |
| off K=30 beta=80 (FIT-tuned) | 0.649 [0.620, 0.678] | 0.427 [0.401, 0.454] | 0.970 / 0.880 |
| all K=30 beta=40 (FIT-tuned) | 0.674 [0.645, 0.703] | 0.457 [0.431, 0.483] | 0.973 / 0.877 |
| all K=30 beta=80 | 0.670 [0.641, 0.699] | 0.443 [0.418, 0.469] | 0.967 / 0.850 |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.680 [0.651, 0.709] | 0.454 [0.429, 0.481] | 0.974 / 0.906 |
| -E K=30 beta=80 (name queries: off order) | 0.673 [0.644, 0.702] | 0.442 [0.416, 0.468] | 0.970 / 0.880 |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.680 [0.651, 0.709] | 0.470 [0.444, 0.497] | 0.974 / 0.906 |
| -I K=30 beta=80 (name queries: off order) | 0.671 [0.642, 0.700] | 0.452 [0.426, 0.478] | 0.970 / 0.880 |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.673 [0.644, 0.702] | 0.448 [0.423, 0.475] | 0.974 / 0.906 |
| -P K=30 beta=80 (name queries: off order) | 0.670 [0.641, 0.699] | 0.443 [0.418, 0.469] | 0.970 / 0.880 |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.661 [0.632, 0.690] | 0.441 [0.415, 0.467] | 0.974 / 0.906 |
| -B K=30 beta=80 (name queries: off order) | 0.663 [0.635, 0.693] | 0.434 [0.409, 0.460] | 0.970 / 0.880 |

D9 gates vs jina fp32 (full text) at its D6 floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32, full text (baseline, D6 floors) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=44, py +0.0 n=43, ru +0.0 n=27, ty +0.0 n=71 |
| int8-structured (no rerank, shipped floors) | -0.4 [-1.8] | +0.002 [-0.007] | typescript -2.2 | -18.0 [-15.7] | +11.1 [+15.3] | **Q5** | go +12.2 n=41, py +20.0 n=40, ru +4.5 n=22, ty +7.7 n=65 |
| off K=30 beta=80 (FIT-tuned) | +3.3 [+1.5] | +0.046 [+0.030] | typescript -1.7 | -20.8 [-18.2] | +9.9 [+13.9] | **Q5** | go +5.7 n=35, py +20.6 n=34, ru +4.0 n=25, ty +9.2 n=65 |
| all K=30 beta=40 (FIT-tuned) | +5.8 [+3.8] | +0.075 [+0.057] | typescript +2.2 | -22.0 [-19.3] | +9.6 [+13.8] | **Q5** | go +8.8 n=34, py +15.6 n=32, ru +4.0 n=25, ty +10.0 n=60 |
| all K=30 beta=80 | +5.4 [+3.5] | +0.062 [+0.047] | typescript +1.7 | -20.9 [-18.3] | +10.3 [+14.5] | **Q5** | go +10.5 n=38, py +17.6 n=34, ru +4.0 n=25, ty +9.1 n=66 |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | +6.4 [+4.3] | +0.073 [+0.054] | typescript +1.7 | -21.7 [-19.0] | +10.5 [+14.9] | **Q5** | go +8.8 n=34, py +18.8 n=32, ru +4.0 n=25, ty +10.3 n=58 |
| -E K=30 beta=80 (name queries: off order) | +5.7 [+3.8] | +0.060 [+0.045] | typescript +1.7 | -20.9 [-18.3] | +10.3 [+14.5] | **Q5** | go +10.5 n=38, py +17.6 n=34, ru +3.8 n=26, ty +9.4 n=64 |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | +6.4 [+4.3] | +0.088 [+0.069] | typescript +2.2 | -23.4 [-20.6] | +8.4 [+12.3] | **Q5** | go +5.9 n=34, py +15.6 n=32, ru +4.0 n=25, ty +8.1 n=62 |
| -I K=30 beta=80 (name queries: off order) | +5.5 [+3.8] | +0.070 [+0.055] | typescript +1.7 | -21.7 [-19.1] | +10.2 [+14.4] | **Q5** | go +10.8 n=37, py +17.1 n=35, ru +4.0 n=25, ty +9.0 n=67 |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | +5.7 [+3.6] | +0.067 [+0.048] | typescript +1.7 | -21.1 [-18.4] | +10.0 [+14.4] | **Q5** | go +8.8 n=34, py +17.2 n=29, ru +4.0 n=25, ty +10.0 n=60 |
| -P K=30 beta=80 (name queries: off order) | +5.4 [+3.5] | +0.062 [+0.046] | typescript +2.8 | -20.8 [-18.3] | +10.3 [+14.5] | **Q5** | go +10.5 n=38, py +17.6 n=34, ru +3.8 n=26, ty +9.2 n=65 |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | +4.5 [+2.5] | +0.059 [+0.041] | typescript +0.0 | -21.4 [-18.7] | +8.4 [+12.3] | **Q5** | go +5.7 n=35, py +15.2 n=33, ru +4.0 n=25, ty +8.6 n=58 |
| -B K=30 beta=80 (name queries: off order) | +4.8 [+3.0] | +0.053 [+0.038] | typescript +1.7 | -20.6 [-18.1] | +9.6 [+13.7] | **Q5** | go +8.3 n=36, py +16.7 n=36, ru +4.2 n=24, ty +9.4 n=64 |

D9 gates vs int8-structured (shipped) at the shipped floors

| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |
|---|---|---|---|---|---|---|---|
| jina fp32, full text (baseline, D6 floors) | +0.4 [-0.9] | -0.002 [-0.012] | python -0.6 | +18.0 [+20.3] | -11.1 [-7.2] | **Q4** | go -12.2 n=41, py -20.0 n=40, ru -4.5 n=22, ty -7.7 n=65 |
| int8-structured (no rerank, shipped floors) | +0.0 [+0.0] | +0.000 [+0.000] | go +0.0 | +0.0 [+0.0] | +0.0 [+0.0] | pass | go +0.0 n=49, py +0.0 n=46, ru +0.0 n=23, ty +0.0 n=66 |
| off K=30 beta=80 (FIT-tuned) | +3.7 [+2.3] | +0.043 [+0.030] | typescript +0.6 | -2.8 [-1.4] | +0.0 [+0.0] | pass | go +0.0 n=40, py +0.0 n=38, ru +0.0 n=23, ty +0.0 n=64 |
| all K=30 beta=40 (FIT-tuned) | +6.2 [+4.4] | +0.073 [+0.055] | python +3.2 | -4.0 [-2.3] | +0.0 [+0.0] | pass | go +0.0 n=37, py +0.0 n=36, ru +0.0 n=22, ty +0.0 n=57 |
| all K=30 beta=80 | +5.8 [+4.1] | +0.060 [+0.046] | python +3.2 | -2.8 [-1.5] | +0.0 [+0.0] | pass | go +0.0 n=42, py +0.0 n=38, ru +0.0 n=22, ty +0.0 n=63 |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | +6.8 [+4.9] | +0.071 [+0.052] | typescript +3.9 | -3.7 [-2.0] | +0.0 [+0.0] | pass | go +0.0 n=37, py +0.0 n=36, ru +0.0 n=22, ty +0.0 n=55 |
| -E K=30 beta=80 (name queries: off order) | +6.1 [+4.4] | +0.058 [+0.044] | python +3.2 | -2.8 [-1.5] | +0.0 [+0.0] | pass | go +0.0 n=42, py +0.0 n=38, ru +0.0 n=23, ty +0.0 n=61 |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | +6.8 [+4.9] | +0.086 [+0.068] | python +2.6 | -5.3 [-3.6] | +0.0 [+0.0] | pass | go +0.0 n=37, py +0.0 n=36, ru +0.0 n=22, ty +0.0 n=60 |
| -I K=30 beta=80 (name queries: off order) | +5.9 [+4.3] | +0.068 [+0.054] | python +3.2 | -3.6 [-2.2] | +0.0 [+0.0] | pass | go +0.0 n=40, py +0.0 n=39, ru +0.0 n=22, ty +0.0 n=65 |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | +6.1 [+4.2] | +0.065 [+0.047] | typescript +3.9 | -3.1 [-1.5] | +0.0 [+0.0] | pass | go +0.0 n=38, py +0.0 n=33, ru +0.0 n=22, ty +0.0 n=57 |
| -P K=30 beta=80 (name queries: off order) | +5.8 [+4.1] | +0.059 [+0.045] | python +3.2 | -2.8 [-1.5] | +0.0 [+0.0] | pass | go +0.0 n=42, py +0.0 n=38, ru +0.0 n=23, ty +0.0 n=62 |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | +4.9 [+3.2] | +0.057 [+0.040] | typescript +2.2 | -3.3 [-1.8] | +0.0 [+0.0] | pass | go +0.0 n=38, py +0.0 n=37, ru +0.0 n=22, ty +0.0 n=57 |
| -B K=30 beta=80 (name queries: off order) | +5.2 [+3.7] | +0.051 [+0.037] | python +3.2 | -2.6 [-1.3] | +0.0 [+0.0] | pass | go +0.0 n=41, py +0.0 n=40, ru +0.0 n=22, ty +0.0 n=61 |

GM-434 columns (option a), at each row's own floors

| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|---|
| jina fp32, full text (baseline, D6 floors) | 0.53 / 0.46 / 0.56 / 0.46 | 9% (17/185) | 64% (438/681) | 53% (87/164) | 0% (2/569) | 34% (298/879) | 36% (324/900) |
| int8-structured (no rerank, shipped floors) | 0.57 / 0.59 / 0.57 / 0.53 | 22% (40/184) | 50% (338/681) | 23% (37/164) | 2% (9/594) | 30% (263/879) | 20% (184/900) |
| off K=30 beta=80 (FIT-tuned) | 0.57 / 0.59 / 0.57 / 0.53 | 24% (52/220) | 46% (314/681) | 23% (37/164) | 3% (22/714) | 18% (156/879) | 20% (184/900) |
| all K=30 beta=40 (FIT-tuned) | 0.57 / 0.59 / 0.57 / 0.53 | 25% (60/238) | 45% (304/681) | 23% (37/164) | 3% (22/715) | 18% (155/879) | 20% (184/900) |
| all K=30 beta=80 | 0.57 / 0.59 / 0.57 / 0.53 | 25% (57/225) | 46% (314/681) | 23% (37/164) | 3% (21/681) | 21% (188/879) | 20% (184/900) |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 25% (60/236) | 45% (306/681) | 23% (37/164) | 3% (23/747) | 14% (124/879) | 20% (184/900) |
| -E K=30 beta=80 (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 25% (57/225) | 46% (314/681) | 23% (37/164) | 3% (22/714) | 18% (156/879) | 20% (184/900) |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 24% (61/251) | 43% (292/681) | 23% (37/164) | 3% (23/747) | 14% (124/879) | 20% (184/900) |
| -I K=30 beta=80 (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 25% (57/232) | 45% (307/681) | 23% (37/164) | 3% (22/714) | 18% (156/879) | 20% (184/900) |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 26% (60/231) | 46% (311/681) | 23% (37/164) | 3% (23/747) | 14% (124/879) | 20% (184/900) |
| -P K=30 beta=80 (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 26% (59/227) | 46% (314/681) | 23% (37/164) | 3% (22/714) | 18% (156/879) | 20% (184/900) |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 24% (55/227) | 46% (310/681) | 23% (37/164) | 3% (23/747) | 14% (124/879) | 20% (184/900) |
| -B K=30 beta=80 (name queries: off order) | 0.57 / 0.59 / 0.57 / 0.53 | 25% (56/222) | 46% (316/681) | 23% (37/164) | 3% (22/714) | 18% (156/879) | 20% (184/900) |

Flips among positives the verdict clears (int8-structured top right -> reranked top wrong / wrong -> right; n = cleared positives)

| variant | queries | go | python | rust | typescript | total |
|---|---|---|---|---|---|---|
| off K=30 beta=80 (FIT-tuned) | NL held-out | 5 / 8 (n=122) | 7 / 7 (n=95) | 0 / 12 (n=133) | 2 / 11 (n=132) | 14 / 38 (n=482) |
| off K=30 beta=80 (FIT-tuned) | name | 0 / 39 (n=144) | 0 / 13 (n=142) | 1 / 38 (n=290) | 0 / 18 (n=272) | 1 / 108 (n=848) |
| all K=30 beta=40 (FIT-tuned) | NL held-out | 6 / 16 (n=122) | 7 / 8 (n=95) | 1 / 17 (n=133) | 7 / 14 (n=132) | 21 / 55 (n=482) |
| all K=30 beta=40 (FIT-tuned) | name | 0 / 29 (n=144) | 0 / 14 (n=142) | 2 / 47 (n=290) | 1 / 21 (n=272) | 3 / 111 (n=848) |
| all K=30 beta=80 | NL held-out | 4 / 8 (n=122) | 6 / 8 (n=95) | 1 / 11 (n=133) | 2 / 10 (n=132) | 13 / 37 (n=482) |
| all K=30 beta=80 | name | 0 / 17 (n=144) | 0 / 11 (n=142) | 3 / 33 (n=290) | 1 / 18 (n=272) | 4 / 79 (n=848) |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | NL held-out | 6 / 17 (n=122) | 8 / 8 (n=95) | 1 / 15 (n=133) | 9 / 16 (n=132) | 24 / 56 (n=482) |
| -E K=30 beta=40 (FIT-tuned) (name queries: off order) | name | 0 / 49 (n=144) | 1 / 19 (n=142) | 1 / 53 (n=290) | 1 / 21 (n=272) | 3 / 142 (n=848) |
| -E K=30 beta=80 (name queries: off order) | NL held-out | 4 / 8 (n=122) | 6 / 8 (n=95) | 0 / 11 (n=133) | 4 / 11 (n=132) | 14 / 38 (n=482) |
| -E K=30 beta=80 (name queries: off order) | name | 0 / 39 (n=144) | 0 / 13 (n=142) | 1 / 38 (n=290) | 0 / 18 (n=272) | 1 / 108 (n=848) |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | NL held-out | 5 / 20 (n=122) | 7 / 8 (n=95) | 1 / 19 (n=133) | 5 / 17 (n=132) | 18 / 64 (n=482) |
| -I K=30 beta=40 (FIT-tuned) (name queries: off order) | name | 0 / 49 (n=144) | 1 / 19 (n=142) | 1 / 53 (n=290) | 1 / 21 (n=272) | 3 / 142 (n=848) |
| -I K=30 beta=80 (name queries: off order) | NL held-out | 5 / 12 (n=122) | 5 / 8 (n=95) | 1 / 11 (n=133) | 1 / 12 (n=132) | 12 / 43 (n=482) |
| -I K=30 beta=80 (name queries: off order) | name | 0 / 39 (n=144) | 0 / 13 (n=142) | 1 / 38 (n=290) | 0 / 18 (n=272) | 1 / 108 (n=848) |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | NL held-out | 6 / 14 (n=122) | 10 / 8 (n=95) | 1 / 14 (n=133) | 7 / 15 (n=132) | 24 / 51 (n=482) |
| -P K=30 beta=40 (FIT-tuned) (name queries: off order) | name | 0 / 49 (n=144) | 1 / 19 (n=142) | 1 / 53 (n=290) | 1 / 21 (n=272) | 3 / 142 (n=848) |
| -P K=30 beta=80 (name queries: off order) | NL held-out | 4 / 9 (n=122) | 6 / 8 (n=95) | 0 / 9 (n=133) | 3 / 11 (n=132) | 13 / 37 (n=482) |
| -P K=30 beta=80 (name queries: off order) | name | 0 / 39 (n=144) | 0 / 13 (n=142) | 1 / 38 (n=290) | 0 / 18 (n=272) | 1 / 108 (n=848) |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | NL held-out | 5 / 10 (n=122) | 6 / 8 (n=95) | 1 / 19 (n=133) | 8 / 11 (n=132) | 20 / 48 (n=482) |
| -B K=30 beta=40 (FIT-tuned) (name queries: off order) | name | 0 / 49 (n=144) | 1 / 19 (n=142) | 1 / 53 (n=290) | 1 / 21 (n=272) | 3 / 142 (n=848) |
| -B K=30 beta=80 (name queries: off order) | NL held-out | 4 / 6 (n=122) | 4 / 7 (n=95) | 1 / 11 (n=133) | 4 / 11 (n=132) | 13 / 35 (n=482) |
| -B K=30 beta=80 (name queries: off order) | name | 0 / 39 (n=144) | 0 / 13 (n=142) | 1 / 38 (n=290) | 0 / 18 (n=272) | 1 / 108 (n=848) |

Mean text tokens per window node (no special tokens)

| arm | tokens |
|---|---|
| off | 38.4 |
| all | 110.2 |
| -E | 106.4 |
| -I | 101.2 |
| -P | 102.8 |
| -B | 58.5 |

Beta sweep, all arm, K=30: FIT-half objective and held-out point estimates

| beta | FIT NL r@10 | FIT NL MRR | held-out NL r@10 | held-out NL MRR | name r@10 / MRR |
|---|---|---|---|---|---|
| 0 | 0.627 | 0.406 | 0.641 | 0.424 | 0.971 / 0.876 |
| 1 | 0.643 | 0.413 | 0.649 | 0.428 | 0.972 / 0.886 |
| 2.5 | 0.648 | 0.419 | 0.657 | 0.434 | 0.973 / 0.895 |
| 5 | 0.658 | 0.436 | 0.662 | 0.443 | 0.975 / 0.901 |
| 10 | 0.662 | 0.449 | 0.669 | 0.453 | 0.976 / 0.900 |
| 20 | 0.673 | 0.455 | 0.679 | 0.458 | 0.976 / 0.896 |
| 40 | 0.677 | 0.455 | 0.674 | 0.457 | 0.973 / 0.877 |
| 80 | 0.662 | 0.438 | 0.670 | 0.443 | 0.967 / 0.850 |
| 160 | 0.647 | 0.418 | 0.666 | 0.434 | 0.956 / 0.822 |
| 320 | 0.634 | 0.397 | 0.642 | 0.413 | 0.949 / 0.803 |

Paired, held-out NL, at each arm's FIT-tuned beta (b minus a)

| a | b | Δr@10 [lo, hi] | ΔMRR [lo, hi] |
|---|---|---|---|
| off | all | +2.5 [+1.0, +4.0] | +0.030 [+0.017, +0.043] |
| all | -E | +0.6 [-0.0, +1.3] | -0.003 [-0.008, +0.003] |
| all | -I | +0.6 [-0.3, +1.4] | +0.013 [+0.006, +0.020] |
| all | -P | -0.1 [-1.0, +0.8] | -0.009 [-0.015, -0.003] |
| all | -B | -1.3 [-2.5, -0.2] | -0.016 [-0.026, -0.007] |
| off | all (strict held-out) | +2.5 [+1.0, +4.0] | +0.030 [+0.017, +0.043] |

Buckets of held-out NL positives by the expected symbols' doc comments (off and all at their tuned betas)
r@10 and MRR are pooled over languages (the mean of per-language means), as the Δ columns are

| bucket | scope | n | off r@10 / MRR | all r@10 / MRR | Δr@10 [lo, hi] | ΔMRR [lo, hi] | median logit off / all | expected nodes logit < -8 off / all |
|---|---|---|---|---|---|---|---|---|
| documented | all languages | 426 | 0.710 / 0.506 | 0.737 / 0.539 | +2.7 [+1.1, +4.4] | +0.033 [+0.016, +0.050] | -2.98 / -1.64 | 92 / 71 |
| documented | typescript | 83 | 0.783 / 0.676 | 0.783 / 0.694 | +0.0 [+0.0, +0.0] | +0.018 [-0.014, +0.052] | -0.94 / -1.15 | 12 / 14 |
| signature-only | all languages | 236 | 0.489 / 0.261 | 0.515 / 0.291 | +2.6 [-0.8, +6.3] | +0.030 [+0.010, +0.050] | -9.89 / -7.71 | 127 / 82 |
| signature-only | typescript | 98 | 0.643 / 0.396 | 0.714 / 0.407 | +7.1 [+2.0, +12.2] | +0.011 [-0.020, +0.044] | -10.03 / -7.86 | 60 / 39 |
| mixed | all languages | 19 | 0.767 / 0.662 | 0.767 / 0.652 | +0.0 [+0.0, +0.0] | -0.011 [-0.024, -0.000] | -5.16 / -3.30 | 9 / 8 |
| mixed | typescript | 0 | - | - | - | - | - | - |

VERDICT all vs off: no effect (Δr@10 +2.5 [+1.0, +4.0], ΔMRR +0.030 [+0.017, +0.043], D9 vs int8-structured and fp32 FAIL)
LOO arms within noise of all: ['-E']; cheapest: -
