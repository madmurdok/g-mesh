# GM-698/S7: ship arm (E + P + B48, all queries) and beta re-tune on v2

Driver: `eval/embedding/gm462_ce_enriched.py --arms off,ship --k 30 --eval-dir eval/embedding/v2`, new arm `"ship": (frozenset("EPB"), 48, False)` (switches, body budget, authored-NL-only flag; False = name queries also get the enriched text, unlike all-b48). BETAS grid unchanged, objective `fit_obj` on FIT, verified on held-out NL.

## Verdict

**PASS on all four pre-registered bars; chosen beta 40.** (Bars are the numbers from design note D5; judged on the point estimate of the ship row, lower CI bounds also shown.)

| bar | needed | ship beta 40 | result |
|---|---|---|---|
| held-out NL r@10 | >= 0.647 | 0.670 [0.641, 0.700] | PASS |
| held-out NL MRR | >= 0.425 | 0.465 [0.439, 0.493] | PASS |
| D9 vs int8-structured, shipped floors | pass | Q1 +5.9 [+4.1], Q2 +0.082 [+0.064], Q3 worst python +1.3, Q4 -5.2 [-3.5], Q5 +0.0 [+0.0]: pass | PASS |
| name-query MRR paired vs off (beta 80), lower bound | >= -0.010 | +0.010 [+0.002, +0.017] | PASS |

Note: beta 40 is a different optimum from GM-462's all-b48 (20), because ship also reranks name queries with the enriched text. `BETA` in rerank.rs and `rerank_parity.json` `beta` should become 40 (a Rust-side change, not made here).

## Results (held-out NL unless stated)

| variant | r@10 [lo, hi] | MRR [lo, hi] | name r@10 / MRR |
|---|---|---|---|
| int8-structured (no rerank) | 0.612 [0.582, 0.642] | 0.384 [0.360, 0.409] | 0.930 / 0.768 |
| off beta 80 (FIT-tuned 80) | 0.649 [0.620, 0.678] | 0.427 [0.401, 0.454] | 0.970 / 0.880 |
| ship beta 40 (FIT-tuned) | 0.670 [0.641, 0.700] | 0.465 [0.439, 0.493] | 0.971 / 0.889 |
| ship beta 80 | 0.668 [0.639, 0.697] | 0.452 [0.426, 0.478] | 0.964 / 0.864 |

Paired ship(beta 40) minus off(beta 80): held-out NL dr@10 +2.2 [+0.7, +3.7], dMRR +0.038 [+0.026, +0.051]; name queries dr@10 +0.1 [-0.3, +0.6], dMRR +0.010 [+0.002, +0.017]; strict held-out NL +2.2 [+0.7, +3.6] / +0.038 [+0.026, +0.051].

Flips right-to-wrong / wrong-to-right (beta 40): NL held-out 19 / 64 (off 14 / 38), name 5 / 123 (off 1 / 108).

## Beta grid (FIT objective = NL r@10, then MRR, then smaller beta)

| beta | off FIT r@10 | off FIT MRR | ship FIT r@10 | ship FIT MRR | ship ho r@10 | ship ho MRR | ship name r@10 / MRR |
|---|---|---|---|---|---|---|---|
| 0 | 0.540 | 0.352 | 0.635 | 0.395 | 0.647 | 0.416 | 0.968 / 0.863 |
| 1 | 0.556 | 0.360 | 0.648 | 0.404 | 0.658 | 0.423 | 0.968 / 0.879 |
| 2.5 | 0.583 | 0.370 | 0.656 | 0.413 | 0.669 | 0.436 | 0.968 / 0.890 |
| 5 | 0.607 | 0.383 | 0.658 | 0.426 | 0.667 | 0.448 | 0.970 / 0.904 |
| 10 | 0.626 | 0.394 | 0.667 | 0.438 | 0.680 | 0.457 | 0.972 / 0.907 |
| 20 | 0.639 | 0.411 | 0.672 | 0.453 | 0.681 | 0.460 | 0.972 / 0.902 |
| 40 | 0.653 | 0.417 | 0.674 | 0.456 | 0.670 | 0.465 | 0.971 / 0.889 |
| 80 | 0.655 | 0.420 | 0.661 | 0.446 | 0.668 | 0.452 | 0.964 / 0.864 |
| 160 | 0.645 | 0.410 | 0.642 | 0.419 | 0.653 | 0.435 | 0.955 / 0.830 |
| 320 | 0.631 | 0.391 | 0.629 | 0.397 | 0.641 | 0.413 | 0.947 / 0.807 |

Off peaks at 80 on FIT (as shipped), ship at 40 (FIT r@10 0.674, MRR 0.456). Ship's beta 20 and 40 are within 0.002 on FIT; held-out is flat from 10 to 40.

## Controls

| # | result |
|---|---|
| C1 | Python structured port vs Rust fixture 20/20; build(all off) == structured text on 12839/12839 window nodes |
| C2 | the off pairs were NOT all in the old gm464 cache: 70407/102930 held; the other 32523 were scored fresh (deviation from GM-462's 102930/102930, which had the v2 cache) |
| C3 | 200 off pairs rescored: max abs diff 0.000e+00 (so the fresh off pairs equal what a cache would hold) |
| C4a | off row at beta 80 reproduces GM-462's published off row (0.649 [0.620, 0.678] / 0.427 [0.401, 0.454] / 0.970 / 0.880), off FIT-tuned beta 80: MATCH. Compared as numbers via new flag `--c4-gm462-numbers`, because GM-460's `F4s K=30 beta=80` table row is not in `docs/results/gm-460-enlarged-eval.md` on this branch (the driver's default C4a found no row and stopped) |
| C4b | off row restricted to GM-398 ids reproduces GM-464 S1's row: MATCH (6 lines) |
| C5 | rerank disabled == int8-structured on every outcome and cell: pass |
| C6 | ship text differs from base on 12839/12839 window nodes; ship row differs from off row |

## Deviations and caveats

- Work dir: the GM-460 v2 work dir no longer exists as such. Used a copy (scratchpad) of the v2 work seeded in `g-mesh-wt-gm697/eval/embedding/v2/work`, which has rankings only for `jina-v2-base-code-fp32` and `jina-v2-base-code-int8-structured`. `rerank_eval.Corpus` also loads the `gte-small` and `jina-v2-base-code-int8` arms; in the copy these were symlinked to the int8-structured run. This driver reads only the fp32 and int8-structured lists (verified by grep), so no number here depends on them. Query-file hashes match the fp32 manifests.
- `--st-runs runs/jina-v2-base-code-int8-structured` in that work dir (not `runs-gm465/...`); identical lists role.
- The score cache was a copy of the main checkout's `rerank_cache`; the main checkout was not written.
- Bars are compared on the ship row's point estimate against the numbers in D5 (which are GM-462's lower bounds).

## Machine state

- Dry run (100 pairs plus 32523 off pairs): 36 min wall, load average 535 at start, 47 at end; 37.45 ms/pair on ship pairs.
- Scoring run: 102817 new ship pairs in 4758 s (46.28 ms/pair) under load 54-62 (3 agents on the machine); not a latency result.
- Final metrics run (cached scores): `/usr/bin/time -p` real 993.74, user 2851.68, sys 46.51 (user/real ~2.9, i.e. compute-bound, not waiting). uptime before: load 19.45 59.51 61.59; after: 41.11 53.07 59.67.
