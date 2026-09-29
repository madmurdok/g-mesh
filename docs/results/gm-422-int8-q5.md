# GM-422: jina int8's Q5 failure under changed floors

Slice S1 (measure) of GM-422. [GM-398](gm-398-model-comparison.md#q5-false-alarm-pooled-and-per-language)
found that jina-v2-base-code int8 matches fp32 on retrieval but fails Q5, the
held-out false-alarm gate of [`embedding-eval.md`](../architecture/embedding-eval.md)
D9. Pooled it is +2.1 points [-1.4, +6.2] against a +5 limit. All of the
excess is in Go: 2 of the 16 paired Go queries fall below int8's floor while
fp32 keeps them above its own. This slice asks whether that is an artifact of
the floor fit and the small sample, or a real loss of quality. It uses only
the stored rankings; nothing was re-embedded.

**Verdict: mostly an artifact of the floor fit, plus a small sample.** One of
the two Go failures comes only from the floors: the query's scores lie
between fp32's Go floor (0.56) and int8's (0.57). It fails under both arms at
0.57 and passes under both at 0.56. The other failure is a real per-query
score drop (-0.015), and it is the only discordant query when both arms use
the same floors: 1 of 67 paired queries. The pooled upper bound is then +4.7,
with a lower bound of 0.0. A 0.01 change to a single floor moves the pooled
upper bound anywhere between +3.7 and +6.2. Nothing in the data shows
systematic quality loss. What remains is one query whose score moved by
about 1.5x the typical quantization shift.

## Method

- Script: [`eval/embedding/q5_floor_sensitivity.py`](../../eval/embedding/q5_floor_sensitivity.py),
  run as `python3 eval/embedding/q5_floor_sensitivity.py <main>/eval/embedding/work/runs <main>/eval/embedding`.
  It is a port, line for line, of the code paths `g-mesh debug-embed-eval report`
  uses for Q5 (`core/src/cli/embed_eval.rs` `load_arm` and
  `false_alarm_deltas`; `metrics.rs` `fit_floors`, `false_alarm_indicator`,
  `paired_at_own_floors` and `bootstrap`; `rng.rs` SplitMix64). It uses
  variants.toml's seed of 398 and 10,000 resamples, with a fresh generator
  for each bound. The script checks that each run's `queryFiles` hashes
  match the query files it reads.
- Inputs: the stored `rankings.jsonl` of `jina-v2-base-code-fp32` and
  `-int8` for all six corpora (GM-398/S14's runs).
- Runtime: real 110 s, user 54 s, sys 0.5 s (machine load not recorded).

## Control: reproduce S14 at the fitted floors

The script stops unless all of these match the GM-398 report.

| check | S14 reported | this script |
|---|---|---|
| fp32 floors go / python / rust / ts | 0.56 / 0.58 / 0.56 / 0.55 | same (also equal to `report-phaseB.json`) |
| int8 floors | 0.57 / 0.57 / 0.55 / 0.53 | same |
| fp32 held-out false alarm | 18.8 / 9.5 / 14.3 / 14.8% | same |
| int8 held-out false alarm | 29.4 / 10.0 / 14.3 / 7.7% | same |
| Q5 pooled | +2.1 [-1.4, +6.2] | +2.1 [-1.4, +6.2] (unrounded +2.12 [-1.44, +6.25]) |
| Q5 go / python / rust / ts | +12.5 [0, +25] n=16 / 0 n=20 / 0 n=6 / -4.0 [-12, 0] n=25 | same |

**Result: it passes.** No per-arm report JSON for int8 was stored, so the
comparison is against the published values, which are rounded to 0.1
points. Changing the seed changes the result: seed 399 gives a pooled lower
bound of -1.0 instead of -1.44. So a port with the wrong generator would
have been caught.

## 1. The two failing Go queries

In both, both arms rank the gold symbol first. Only the top score relative
to the floor differs.

| query | text | gold | fp32 top (rank) | int8 top (rank) | Δ score | fp32 @0.56 | int8 @0.57 | both @0.56 | both @0.57 |
|---|---|---|---|---|---|---|---|---|---|
| gin-066 | slice type holding every allowed user's precomputed basic authorization header, built from the accounts map | `authPairs` | 0.5742 (1) | 0.5597 (1) | -0.0146 | clears | below | fp32 clears, int8 below | fp32 clears, int8 below |
| gin-083 | redirect to case-corrected cleaned path | `redirectFixedPath` | 0.5680 (1) | 0.5634 (1) | -0.0046 | clears | below | both clear | both below |

- **gin-083 is a pure floor artifact.** Both of its scores lie between 0.56
  and 0.57, so both arms agree at either floor. It is discordant only because
  each arm is judged at its own floor.
- **gin-066 is a real score drop.** int8 scores it 0.0146 lower, which puts
  it below 0.57 and just under 0.56 (0.5597). It is discordant at any shared
  floor between 0.5597 and 0.5742.

The one discordant query outside Go runs the other way, in int8's favour:
ttm-025 (typescript) has fp32 at 0.5339 and int8 at 0.5329. That is below
fp32's 0.55 floor and above int8's 0.53, so it is also caused by the floors.

**Why the Go floors differ.** Before rounding down, the fitted Go floors are
0.5626 (fp32) and 0.5710 (int8). Each is the 4th-lowest right-first score
(k=3) out of about 100 fit-half queries. The two are 0.008 apart, and
rounding down to two decimals puts them on different sides of 0.57.

| language | fp32 raw → floor (k, n) | int8 raw → floor (k, n) |
|---|---|---|
| go | 0.5626 → 0.56 (3, 100) | 0.5710 → 0.57 (3, 101) |
| python | 0.5879 → 0.58 (3, 124) | 0.5763 → 0.57 (3, 121) |
| rust | 0.5678 → 0.56 (4, 157) | 0.5530 → 0.55 (4, 161) |
| typescript | 0.5533 → 0.55 (7, 251) | 0.5343 → 0.53 (7, 255) |

**How far quantization moves scores**: int8 minus fp32 top score, over the
paired held-out right-first queries.

| language | n | median | min | max | mean abs |
|---|---|---|---|---|---|
| go | 16 | -0.0001 | -0.0146 | +0.0294 | 0.0097 |
| python | 20 | -0.0039 | -0.0269 | +0.0128 | 0.0086 |
| rust | 6 | +0.0005 | -0.0194 | +0.0026 | 0.0088 |
| typescript | 25 | -0.0035 | -0.0195 | +0.0292 | 0.0097 |

Most shifts are about 0.01 (mean abs 0.009-0.010), and they go in both
directions. The tails reach ±0.03. This is larger than the 0.001-0.01 the
hypothesis assumed. gin-066's -0.0146 is within the range of the other
languages' shifts. Any query whose fp32 score lies within about 0.01 of a
floor can cross it in either direction.

## 2. Q5 with the floors swapped or shared

Δ = int8 - fp32 in points, with one-sided 95% bounds. `+w/-b` counts the
paired queries on which int8 is worse / better.

| floors | pooled | go | python | rust | typescript |
|---|---|---|---|---|---|
| each arm at its own (S14) | +2.1 [-1.4, +6.2] | +12.5 [0.0, +25.0] n=16 +2/-0 | 0.0 n=20 | 0.0 n=6 | -4.0 [-12.0, 0.0] n=25 +0/-1 |
| **swapped**: int8 at fp32's, fp32 at int8's | +1.0 [-3.1, +5.1] | 0.0 [-12.5, +12.5] +1/-1 | 0.0 | 0.0 | +4.0 [0.0, +12.0] +1/-0 |
| both at fp32's floors | +1.6 [0.0, +4.7] | +6.2 [0.0, +18.8] +1/-0 | 0.0 | 0.0 | 0.0 |
| both at int8's floors | +1.6 [0.0, +4.7] | +6.2 [0.0, +18.8] +1/-0 | 0.0 | 0.0 | 0.0 |

With the floors swapped, Go comes out even: gin-066 goes against int8, and
gin-083 now goes against fp32. The disadvantage moves to TypeScript
(ttm-025). This shows that the discordance comes from the floors, not from
either arm. With both arms at the same floors, only gin-066 is discordant,
and the pooled upper bound (+4.7) is below +5.

## 3. Sensitivity: one floor moved by ±0.01

In each row, one arm's floor in one language moves by 0.01 and every other
floor stays at its fitted value. "own FA" is that arm's held-out false alarm
in that language at the moved floor.

| arm, language, move | own FA | Q5 pooled | Q5 in that language |
|---|---|---|---|
| fp32 go 0.55 | 3/16 | +2.1 [-1.4, +6.2] | +12.5 [0.0, +25.0] |
| fp32 go 0.57 | 4/16 | +0.6 [-2.0, +3.7] | +6.2 [0.0, +18.8] |
| fp32 python 0.57 / 0.59 | 2/21 / 2/21 | +2.1 [-1.4, +6.2] | 0.0 |
| fp32 rust 0.55 / 0.57 | 1/7 / 1/7 | +2.1 [-1.4, +6.2] | 0.0 |
| fp32 typescript 0.54 | 4/27 | +2.1 [-1.4, +6.2] | -4.0 [-12.0, 0.0] |
| fp32 typescript 0.56 | 5/27 | +1.1 [-2.9, +5.2] | -8.0 [-16.0, 0.0] |
| int8 go 0.56 | 4/17 | +0.6 [-2.0, +3.7] | +6.2 [0.0, +18.8] |
| int8 go 0.58 | 5/17 | +2.1 [-1.4, +6.2] | +12.5 [0.0, +25.0] |
| int8 python 0.56 | 1/20 | +0.9 [-3.2, +5.2] | -5.0 [-15.0, 0.0] |
| int8 python 0.58 | 2/20 | +2.1 [-1.4, +6.2] | 0.0 |
| int8 rust 0.54 / 0.56 | 1/7 / 1/7 | +2.1 [-1.4, +6.2] | 0.0 |
| int8 typescript 0.52 | 2/26 | +2.1 [-1.4, +6.2] | -4.0 [-12.0, 0.0] |
| int8 typescript 0.54 | 3/26 | +3.1 [0.0, +6.2] | 0.0 |

Under a 0.01 change to one floor, the pooled upper bound ranges from +3.7 to
+6.2 and the point estimate from +0.6 to +3.1. The bound falls below +5 in
two of the 16 moves: either arm's Go floor moved by 0.01 so that the two Go
floors match. It falls to +5.2 in two more (fp32 typescript 0.56, int8
python 0.56). Where the gate lands
depends on floor differences smaller than the rounding step.

The point estimate is above 0 in every variant above, including both
shared-floor rows, where it is +1.6. That is D9's other Q5 condition
(Δ <= 0), which S14's summary did not mention. At n=67 paired queries,
one adverse discordant query is enough to make it positive.

## Reading

- **The floor fit accounts for most of the failure.** Of the two Go
  failures, gin-083 comes only from the 0.01 gap between the rounded Go
  floors. That gap is itself fit-half noise: the 4th-lowest score out of
  about 100, 0.008 apart before rounding. Swapping the floors moves the
  disadvantage to another language rather than removing it.
- **The rest is one query.** At shared floors, only gin-066 is discordant:
  1 of 67. The pooled bound is [0.0, +4.7] and passes the +5 limit. The
  lower bound is 0.0, so the data do not show that int8 is worse. The score
  drop (-0.0146) is within the ±0.03 range of shifts measured in every
  language, and int8's median shift is about 0 in Go.
- **This is not a real quality loss by these numbers.** GM-398's retrieval
  gates already agree (Q1-Q4 pass, per-language recall@10 within ±2
  points). No sign of loss appears here: the shifts are about as often
  upward as downward, and crossings happen in both directions (gin-066
  down; ttm-025 and the swapped-floor gin-083 against fp32). What this
  analysis cannot rule out is a small downward shift in python and
  typescript (medians -0.004), which is below what n=67 can resolve.

Any rule consequences are for S2 and the owner to decide.
