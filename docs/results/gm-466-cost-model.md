# GM-466: calibrating and validating the embedding cost model

**Verdict: the validation does not hold at the 0.05 target.** The calibrated
curve is reproducible (two cool calibrations give ratios within 0.002 of each
other), but its predicted pass-time ratios miss the measured ones by up to
0.094 (median 0.047-0.052). A linear-only control misses them by about as
much (max 0.099, median 0.055), so the measured rounds cannot tell the
quadratic model from a linear one. Only the flat model (cost proportional to
the number of texts) is clearly rejected (max 0.436, median 0.19). The
measured ratios disagree among themselves by about as much: the same pair,
first-paragraph / int8 on g-mesh, measured 0.564, 0.593 and 0.674 in three
rounds. D9's pass-time gate stays on measured passes; see
[embedding-eval.md, D11](../architecture/embedding-eval.md#d11-measuring-pass-time-rss-and-size).

## Method

- `g-mesh debug-embed-eval cost calibrate` (core/src/cli/embed_eval/cost.rs)
  for `jina-v2-base-code-int8`, the shipped model, with the default grid
  (8 ... 1024 tokens, 14 points), 3 warm-up and 15 timed calls per point, batch
  of one, texts cut from g-mesh's node texts. The gate was on: each point
  started only at `pmset -g therm` CPU_Speed_Limit = CPU_Scheduler_Limit =
  100 and a 1-minute load below 4.
- Three calibrations, A, B and C. The brief asked for two; A's 1024-token
  point was an outlier (below), so C was run to decide between A and B
  before any prediction was made.
- The curve used for predictions is the **combined** one: the per-point median
  of A, B and C, fitted `t(n) = a + b*n + c*n^2` with relative weights (the
  default). Absolute, linear-only (`c = 0`) and flat (`t = 1`) fits of the same
  points are the controls. `eval/embedding/gm466_validate.py` refits the curves
  (its relative refit reproduces `calibrate`'s stored coefficients exactly),
  writes one curve file per fit, and builds the validation table from
  `cost predict --json` output.
- Predictions: `cost predict` over all six corpora, reference int8 1024. The
  predicted ratios are taken per corpus (g-mesh) and pooled.
- Validation: each measured ratio is a pair of arms timed in **one round**,
  compared with the predicted ratio for the same pair and corpus.

## Calibration curve

Per-point medians (ms per embed call) and the pmset speed limit before ->
after each point. All 42 points started at speed limit 100 and load1 3.49-3.96.

| n | A | B | C | combined | B vs A | C vs A | relative fit (combined) | residual |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 8 | 8.88 | 9.44 | 8.90 | 8.90 | +6.3% | +0.1% | 9.50 | +6.8% |
| 16 | 17.06 | 18.38 | 19.19 | 18.38 | +7.7% | +12.5% | 16.32 | -11.2% |
| 24 | 25.75 | 25.46 | 25.50 | 25.50 | -1.1% | -1.0% | 23.18 | -9.1% |
| 32 | 29.84 | 29.48 | 28.18 | 29.48 | -1.2% | -5.6% | 30.07 | +2.0% |
| 48 | 42.37 | 41.31 | 39.91 | 41.31 | -2.5% | -5.8% | 43.92 | +6.3% |
| 64 | 55.29 | 60.89 | 60.06 | 60.06 | +10.1% | +8.6% | 57.90 | -3.6% |
| 96 | 86.66 | 83.89 | 87.54 | 86.66 | -3.2% | +1.0% | 86.29 | -0.4% |
| 128 | 109.84 | 112.13 | 116.98 | 112.13 | +2.1% | +6.5% | 115.15 | +2.7% |
| 192 | 163.16 | 168.95 | 172.32 | 168.95 | +3.5% | +5.6% | 174.38 | +3.2% |
| 256 | 222.27 | 241.21 | 249.27 | 241.21 | +8.5% | +12.1% | 235.64 | -2.3% |
| 384 | 356.17 | 377.43 | 346.34 | 356.17 | +6.0% | -2.8% | 364.25 | +2.3% |
| 512 | 494.22 | 484.74 | 483.59 | 484.74 | -1.9% | -2.2% | 500.86 | +3.3% |
| 768 | 808.80 | 800.27 | 828.45 | 808.80 | -1.1% | +2.4% | 798.62 | -1.3% |
| 1024 | 1457.76 | 1154.79 | 1146.27 | 1154.79 | -20.8% | -21.4% | 1128.26 | -2.3% |

pmset speed limit before -> after was 100 -> 100 at every point except B's
1024 (100 -> 95).

**Reproducibility.** B vs A: median |diff| 3.4%, max 20.8%; C vs A: median
5.6%, max 21.4%. The max in both is A's 1024 point. Its 15 calls ranged
1,214-2,047 ms, and the 1-minute load rose from 3.71 to 6.12 during it. B and
C agree at 1024 (1,155 / 1,146 ms), and A's first three calls (1,214-1,314)
are in their range, so A's point is taken as disturbed. Below 1024 the
points differ by up to 12.5% (16 and 256 tokens), with no sign that holds
across points. In ratio terms the curve choice barely matters: B and C
predict the same ratios to 0.002, and A (with its high 1024 point) moves them
by at most 0.023 (fp-path g-mesh 0.720 vs 0.743).

## Fits

| fit (points) | a ms | b ms/token | c ms/token^2 | t(16) | t(256) | t(1024) | max \|residual\| |
|---|---:|---:|---:|---:|---:|---:|---:|
| A relative (stored) | 3.2283 | 0.792731 | 4.1480e-4 | 16.02 | 233.35 | 1249.94 | 14.3% |
| B relative (stored) | 3.2461 | 0.836591 | 2.6721e-4 | 16.70 | 234.93 | 1140.11 | 9.1% |
| C relative (stored) | 2.6810 | 0.847478 | 2.5162e-4 | 16.31 | 236.13 | 1134.34 | 15.0% |
| **combined relative** | **2.7127** | **0.846790** | **2.4646e-4** | **16.32** | **235.64** | **1128.26** | **11.2%** |
| combined absolute | 5.6224 | 0.792030 | 3.2279e-4 | 18.38 | 229.54 | 1155.13 | 34.7% (at n = 8) |
| control: combined linear (c = 0) | 1.6175 | 0.927558 | 0 | 16.46 | 239.07 | 951.44 | 17.6% (at n = 1024) |
| control: flat | 1 | 0 | 0 | - | - | - | - |

On the curve itself the quadratic term is real: the linear fit undershoots
768 and 1024 by 12% and 18%, and the quadratic one fits every point from 64
up within 3.6%. The largest relative residuals of the quadratic fit are at
16-24 tokens (-9% to -11%), where the run-to-run spread is about as large.
The relative and absolute fits differ only at 8 tokens (+35% absolute), and
their predicted ratios differ by at most 0.017.

## Predictions

Combined relative curve, ratio of predicted pass time to int8 1024's.

| arm | g-mesh | pooled (6 corpora) | g-mesh predicted s | g-mesh tokens |
|---|---:|---:|---:|---:|
| int8 1024 (reference) | 1.000x | 1.000x | 504.2 | 532,310 |
| seq512 | 0.924x | 0.947x | 465.8 | 500,359 |
| seq256 | 0.823x | 0.874x | 414.9 | 450,617 |
| first-paragraph | **0.597x** | **0.663x** | 300.8 | 325,744 |
| structured | **0.630x** | **0.712x** | 317.8 | 344,780 |
| fp-ctx-none | 0.597x | 0.663x | 300.8 | 325,744 |
| fp-path | 0.743x | 0.868x | 374.5 | 410,085 |
| fp-parent | 0.608x | 0.694x | 306.7 | 332,603 |
| fp-path-parent | 0.752x | 0.894x | 379.3 | 415,584 |

fp-ctx-none embeds exactly first-paragraph's texts, so its prediction is the
same, as GM-455's control C0 requires. Against fp-ctx-none, the context arms
are predicted at 1.245x (path), 1.020x (parent) and 1.261x (path-parent) on
g-mesh, and 1.309x / 1.047x / 1.347x pooled. The pooled ratios are higher
than g-mesh's because the other corpora have fewer long symbols for the
trims to cut.

The predicted int8 g-mesh pass, 504 s, lies between the measured ones
(384-400 s in GM-423, 540 s in GM-465 round 1). The absolute number is not
used; only ratios are.

## Validation

Measured = ratio of `embedNodesMs` within one round. Each model cell gives
the predicted ratio and (predicted - measured).

| round | corpus | pair | measured | relative fit | absolute fit | linear (control) | flat (control) |
|---|---|---|---:|---:|---:|---:|---:|
| GM-423 r1 | g-mesh | seq512 / int8 | 0.863 | 0.924 (+0.061) | 0.921 (+0.058) | 0.941 (+0.079) | 1.000 (+0.137) |
| GM-423 r1 | g-mesh | seq256 / int8 | 0.755 | 0.823 (+0.068) | 0.820 (+0.065) | 0.850 (+0.095) | 1.000 (+0.245) |
| GM-423 r1 | g-mesh | first-paragraph / int8 | 0.564 | 0.597 (+0.032) | 0.603 (+0.038) | 0.620 (+0.056) | 1.000 (+0.436) |
| GM-423 r2 | g-mesh | seq512 / int8 | 0.898 | 0.924 (+0.026) | 0.921 (+0.022) | 0.941 (+0.043) | 1.000 (+0.102) |
| GM-423 r2 | g-mesh | seq256 / int8 | 0.790 | 0.823 (+0.033) | 0.820 (+0.030) | 0.850 (+0.059) | 1.000 (+0.210) |
| GM-423 r2 | g-mesh | first-paragraph / int8 | 0.593 | 0.597 (+0.003) | 0.603 (+0.009) | 0.620 (+0.027) | 1.000 (+0.407) |
| GM-423 r1 | all 6, pooled | seq512 / int8 | 0.905 | 0.947 (+0.042) | 0.946 (+0.041) | 0.959 (+0.054) | 1.000 (+0.095) |
| GM-423 r1 | all 6, pooled | seq256 / int8 | 0.812 | 0.874 (+0.063) | 0.874 (+0.062) | 0.894 (+0.082) | 1.000 (+0.188) |
| GM-423 r1 | all 6, pooled | first-paragraph / int8 | 0.650 | 0.663 (+0.013) | 0.675 (+0.025) | 0.680 (+0.030) | 1.000 (+0.350) |
| GM-465 r1 | g-mesh | first-paragraph / int8 | 0.674 | 0.597 (-0.078) | 0.603 (-0.071) | 0.620 (-0.054) | 1.000 (+0.326) |
| GM-465 r1 | g-mesh | structured / int8 | 0.716 | 0.630 (-0.086) | 0.635 (-0.081) | 0.655 (-0.061) | 1.000 (+0.284) |
| GM-465 r1 | g-mesh | structured / first-paragraph | 1.062 | 1.056 (-0.006) | 1.053 (-0.009) | 1.056 (-0.006) | 1.000 (-0.062) |
| GM-455 r1 + GM-465 r2 | g-mesh | structured / first-paragraph | 1.031 | 1.056 (+0.025) | 1.053 (+0.022) | 1.056 (+0.025) | 1.000 (-0.031) |
| GM-455 r1 + GM-465 r2 | g-mesh | fp-path / first-paragraph | 1.151 | 1.245 (+0.094) | 1.229 (+0.078) | 1.250 (+0.099) | 1.000 (-0.151) |
| GM-455 r1 + GM-465 r2 | g-mesh | fp-parent / first-paragraph | 0.968 | 1.020 (+0.052) | 1.018 (+0.051) | 1.020 (+0.053) | 1.000 (+0.032) |
| GM-455 r1 + GM-465 r2 | g-mesh | fp-path-parent / first-paragraph | 1.195 | 1.261 (+0.066) | 1.244 (+0.049) | 1.266 (+0.071) | 1.000 (-0.195) |

|predicted - measured|, in ratio points:

| model | all 16 pairs: max | median | 13 g-mesh pairs: max | median |
|---|---:|---:|---:|---:|
| relative fit | 0.094 | 0.047 | 0.094 | 0.052 |
| absolute fit | 0.081 | 0.045 | 0.081 | 0.049 |
| control: linear | 0.099 | 0.055 | 0.099 | 0.056 |
| control: flat | 0.436 | 0.192 | 0.436 | 0.195 |

Which pairs are in, and why:

- **GM-423**: both g-mesh rounds and the all-corpora round 1, from
  [gm-423-sequence-length.md](gm-423-sequence-length.md#embedding-time). The
  all-corpora row carries the small corpora's ±30% load noise that GM-423
  itself excluded from gates; it is shown, and the g-mesh-only statistics
  leave it out.
- **GM-465 round 1** (`work/runs-gm465/timing.tsv`): all three arms valid.
- **GM-455 round 1 + GM-465 round 2**: one `gm455_measure.sh` window, with the
  valid rows from `work/runs-gm455/timing.tsv`: structured 346.0 s,
  first-paragraph 335.6 s, fp-path 386.1 s (attempt 2), fp-parent 324.7 s,
  fp-path-parent 401.1 s (attempt 2). int8 (both attempts) and fp-ctx-none
  (both attempts) were invalid by load, so no pair uses them. As in
  GM-455's doc, first-paragraph is the no-context baseline: it embeds the
  same texts as fp-ctx-none, and C0 shows identical vectors.
- GM-465's discarded attempts (the throttled 712 s structured row,
  `timing-attempt1.tsv`) are not used.

## Reading

- **The target is missed, and the measurements cannot confirm a tighter
  model.** For the same pair in the same corpus, measured rounds disagree with
  each other by as much as the model does. first-paragraph / int8 measured
  0.564, 0.593 and 0.674 (spread 0.110); seq512 0.863 / 0.898, seq256 0.755 /
  0.790, structured / first-paragraph 1.062 / 1.031. The model sits inside
  each of these ranges (0.597, 0.924, 0.823, 1.056), except seq512 and seq256,
  which it puts 0.026-0.033 above both GM-423 rounds. Its largest misses are
  pairs measured once (fp-path +0.094, the GM-465 r1 pairs -0.08). One run of
  fp-ctx-none at the same gate took 362 s and another 481 s, so a single run
  varies by more than these misses.
- **The controls tell different things.** The flat control fails visibly
  (+0.10 to +0.44): cost does follow tokens, not texts. The linear control
  does not separate from the quadratic: it is 0.001-0.027 further from the
  measured ratios on 12 of the 16 pairs, level on the two structured /
  first-paragraph pairs, and closer on GM-465 r1's two pairs against int8. The
  quadratic term is clear on the curve itself (the linear fit misses 1024
  tokens by 18%) but too small in the pass ratios to show through the round
  noise.
- **Direction of the misses.** In GM-423 the model predicts smaller savings
  from truncation than were measured, on every pair (+0.003 to +0.068). If
  that held up, the batch-of-one curve would undercount the long texts'
  share of a sustained pass. GM-465 r1 goes the other way (-0.08), so these
  rounds show no systematic bias.
- **For GM-423's ADR.** first-paragraph's predicted g-mesh ratio, 0.597x,
  is 0.003 under the 0.60x pass-time gate. The model's own error is about
  0.05-0.09, so it cannot decide that gate, and measured passes can't
  either (0.564-0.674). structured is predicted at 0.630x, over the gate. The
  model ranks the arms consistently with every round: structured > first-
  paragraph by 5-6% (measured 3-6%), and parent costs much less than
  path/path-parent.

## Machine state

Owner's working machine: Intel i7-1068NG7, 4 cores / 8 threads, macOS,
`available_parallelism` 8, ONNX Runtime defaults from the production loader.

| run | start (EEST) | uptime load before | pmset before | gate wait: first point / total | embed calls (warm-up + timed) | `/usr/bin/time -p` real / user / sys | end load / pmset |
|---|---|---|---|---:|---:|---|---|
| A | 03:15:03 | 156.74 61.71 43.44 | speed 87, sched 100 | 690 / 765 s | ~71 s | 845.5 / 279.8 / 3.2 | 6.12 8.70 19.58 / 100 |
| B | 03:29:27 | 5.66 8.43 19.23 | 100 / 100 | 150 / 225 s | ~65 s | 293.3 / 262.1 / 1.8 | 4.68 5.88 14.92 / speed 95 |
| C | 03:34:44 | 4.22 5.68 14.58 | 100 / 100 | 15 / 270 s | ~65 s | 338.1 / 260.7 / 1.7 | 13.43 6.78 11.99 / 100 |

Before A something outside the run held the 1-minute load at 157 (not
identified). The gate made A wait 690 s, until 100/100 and load 3.96, before
its first point. In each run, `real` minus the gate's waiting and the model
load (0.6-0.8 s) is 65-80 s, while `user` is 260-280 s. So a batch-of-one call
keeps about four cores busy (ONNX Runtime's intra-op threads), as a full pass
does. The calls computed, not waited. Each calibration's compute took just
over a minute, short enough that pmset stayed at 100 for 41 of 42 points. The
`cost predict` runs (tokenization only, about 11 s each, user ≈ real) came
after all three calibrations.

## Reproduce

```bash
cargo build --release
O=eval/embedding/work/runs-gm466
for r in A B C; do
  target/release/g-mesh debug-embed-eval cost calibrate --eval-dir eval/embedding --out $O/curve-$r.json
done
python3 eval/embedding/gm466_validate.py curves $O $O/curve-A.json $O/curve-B.json $O/curve-C.json
bash -c 'for k in rel abs linear flat; do
  target/release/g-mesh debug-embed-eval cost predict --eval-dir eval/embedding --curve '$O'/$k.curve.json \
    --variant jina-v2-base-code-int8-seq512 --variant jina-v2-base-code-int8-seq256 \
    --variant jina-v2-base-code-int8-first-paragraph --variant jina-v2-base-code-int8-structured \
    --variant fp-ctx-none --variant fp-path --variant fp-parent --variant fp-path-parent \
    --json '$O'/predict-$k.json; done'
python3 eval/embedding/gm466_validate.py validate $O
```

Outputs are in `eval/embedding/work/runs-gm466/` (`curve-{A,B,C}.json`,
`{rel,abs,linear,flat}.curve.json`, `predict-*.json|txt`, `curves.txt`,
`validate.txt`, `cal*.state|err|out`).
