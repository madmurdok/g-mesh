# GM-455: structural context in the embedded text

Slice S4 (measure) of GM-455, per the design
[`gm-455-structural-context.md`](../architecture/gm-455-structural-context.md)
(arms, go bar, controls). This covers the shipped int8 model
(`jina-v2-base-code-int8`, 1024 tokens) with GM-423's `first-paragraph` form.
Each arm puts an unlabelled header before the text: the file path, the
parent type or trait impl, or both. The eval is GM-398's
([`embedding-eval.md`](../architecture/embedding-eval.md), D5-D9 unchanged).

| arm | variant row | header |
|---|---|---|
| none (control C0) | `fp-ctx-none` | none: GM-423's first-paragraph text byte for byte |
| path | `fp-path` | `filePath` |
| parent | `fp-parent` | `struct Foo` / `class Session` / `impl T for X`, when the symbol has one |
| path-parent | `fp-path-parent` | both, path first |
| shuffled (control C1) | `fp-path-parent-shuffled` | path-parent's header shape with paths deranged across files and parent lines across nodes |

This page only measures. The call is under Go / no-go at the end.

**Headline: no-go ("inconclusive = keep").** The file path helps
retrieval. Against the same text without context, `path` scores
recall@10 +5.0 points (lower bound +2.2) and MRR +0.032 (lower bound
+0.010). `path-parent` scores +4.7 (+1.8) and +0.044 (+0.020). Both pass Q1
and Q2 of the go bar, and C1 shows the gain comes from the right path, not
from the header's shape. But every real arm **fails Q4**: at its own
re-fitted floors (which drop from 0.55-0.59 to 0.45-0.51), confident-wrong
rises by 12.3 points (upper bound +17.2) for `path` and 14.8 points (+20.0)
for `path-parent`. Most of the gain also sits in queries whose words
appear in the target's path or parent name (`pathOverlap = true`, n=111:
+10.9 / +15.2 points). On the other 289 queries, `path` gains +3.1 points
(lower bound +0.1) and `path-parent` +1.2 (lower bound -2.1). `parent` alone
does nothing for recall (-0.5, lower bound -2.5). No arm passes the go bar,
so there is no stage 2 (crossing with `full` / `structured`).

## Method

- **Harness**: `g-mesh debug-embed-eval` at branch
  `perf/GM-455-structural-context` (a7984e1: GM-423's harness, GM-459's
  progress, GM-465's `structured` form, GM-455's `context` field and `churn`
  command), release build, the same six snapshots and int8 model files
  as GM-398/GM-423/GM-465, batch of one, `G_MESH_EMBEDDING_CACHE=off`.
- **Scripts**: `eval/embedding/gm455_measure.sh` (adapted from
  `gm465_measure.sh`) runs quality, churn and the gated timing;
  `gm455_summary.py` (imports `gm423_summary.py`) writes the controls,
  `costs-*.toml`, the reports and the summary; `gm455_token_lengths.py`
  (imports `gm423_token_lengths.py`) counts tokens with the header. Runs are
  in `eval/embedding/work/runs-gm455/`, which is new; the stored runs were
  only read.
- **Quality**: five arms on all six corpora, one pass each, full mode,
  run regardless of machine load (vectors are deterministic, see C0).
  Reports: `report --reference fp-ctx-none` (the context effect),
  `--reference fp-path-parent-shuffled` (C1), and
  `--reference jina-v2-base-code-int8` against the stored GM-398 int8 run
  (the shipped model, as GM-423 did). Floors are re-fitted per arm (D6) on
  the fit half; Q4 and Q5 use the held-out half.
- **Timing**: g-mesh embed-only, round 1 only (owner cut); see
  [Pass time](#pass-time).
- **Stage 2 was not run**, because no stage-1 arm passes the go bar. The
  callees and body-head arms are out (owner decisions 3 and 5).

### Controls

- **C0 (empty context)**: `fp-ctx-none` reproduces GM-423's stored
  first-paragraph run on all six corpora. Vectors match with max |Δ| 0
  (12,000,000 floats). Rankings are byte-identical once `jq 'del(.targetDoc,
  .pathOverlap)'` drops the two labels GM-455 added. Every manifest key
  except `variant` and `variantFingerprint` is equal. The fingerprint
  differs by design, because the variant name is hashed into it. The unit
  test (no header gives `text_for(form)` unchanged) is S2's.
- **C1 (broken context)**: `path-parent` against the shuffled arm: Δ
  recall@10 +11.0 points (lower bound +7.8), Δ MRR +0.095 (+0.070), and
  confident-wrong -9.7 (upper -5.2). A header with the wrong path and
  parent costs 6.3 points of recall against no header at all (0.562 vs
  0.625). So the model reads the header's content, and the path-parent
  gain is a real effect rather than "measured nothing". The derangement
  left 47 of 1,360 g-mesh parent lines on their own node (`churn` prints
  this). C1's report lists validity notes because its reference is not the
  shipped model: shuffled's floors (0.42-0.49) are not near the shipped
  ones. Those notes only concern that comparison.
- **D7 (broken arms)**: validity passes in the vs-none and vs-int8 reports
  (no validity errors). Random scores 0.005 recall@10 and shuffled 0.015.
- **Token counts**: `gm455_token_lengths.py` rebuilds every embedded
  text in Python: the body from `gm423_token_lengths.py`'s forms, then a
  port of `context.rs`'s header. The **sha256 of every rebuilt text equals
  the harness's own** (`churn --dump`, id and sha256 of the text `run`
  embeds) for all 15,625 nodes x 4 arms on the six corpora. The shares
  above 512 and 1024 match the run manifests' `tokenShareOver512/1024` to
  the node. The shuffled arm is not ported (its derangement is Rust's
  seeded RNG); its header has path-parent's shape.
- **Churn simulator against a real edit (E4)**: `watcher::debounce::Debouncer`
  (5 methods) was renamed with a word-boundary `sed` in a scratch clone of
  the g-mesh corpus at the snapshot commit, and both clones were
  re-indexed. Diffing the `(id, sha256)` dumps gives cache misses of
  none 6 / path 6 / parent 11 / path-parent 11. The simulator gives
  4 / 4 / 9 / 9 for that instance, on the snapshot and on the scratch
  "before" index alike. The +2 in every arm is the same two texts:
  `BurstBatcher::record` and `flush_if_ready`, whose **doc comments**
  mention `Debouncer`. The `sed` renamed them, and the simulator by design
  rewrites only signatures. The context-dependent part (+5 for the parent
  arms: the five members' parent lines) agrees exactly.

## Context effect (D9 as a quality candidate against `fp-ctx-none`)

Δ = arm - `fp-ctx-none`, pooled over the 400 positives. One-sided 95%
bounds come from 10,000 paired resamples (seed 398). Go bar (owner
decision 2): both lower bounds > 0, Q3-Q5 pass, pass time <= 1.5x.

| | none | path | parent | path-parent | shuffled (C1) |
|---|---:|---:|---:|---:|---:|
| recall@10 | 0.625 | **0.675** | 0.620 | 0.672 | 0.562 |
| MRR | 0.419 | 0.452 | 0.435 | **0.463** | 0.368 |
| recall@1 | 0.320 | 0.330 | 0.335 | 0.348 | 0.273 |
| Q1 Δ recall@10 (lower > 0) | | +5.0 (+2.2) pass | -0.5 (-2.5) **FAIL** | +4.7 (+1.8) pass | |
| Q2 Δ MRR (lower > 0) | | +0.032 (+0.010) pass | +0.015 (+0.002) pass | +0.044 (+0.020) pass | |
| Q3 worst language Δ recall@10 (>= -10) | | go +4.0 pass | rust -2.0 pass | go +1.0 pass | |
| Q4 Δ confident-wrong (<= 0, upper <= +5) | | +12.3 (+17.2) **FAIL** | +8.6 (+12.0) **FAIL** | +14.8 (+20.0) **FAIL** | |
| Q5 Δ false alarm (<= 0, upper <= +5) | | -5.7 (-0.3) pass | -6.4 (-1.8) pass | -4.3 (+1.0) pass | |
| fitted floors go/py/rust/ts | .57/.59/.58/.53 | .49/.49/.49/.51 | .55/.54/.55/.53 | .49/.45/.49/.49 | |
| pass time vs none (<= 1.5x), one run each* | 1.00x | 1.15x | 0.97x | 1.20x | |
| **verdict** | | Fail (Q4) | Fail (Q1, Q4) | Fail (Q4) | |

Q4 is the design's "hubness / file clustering" risk, and it showed up.
The header is text that many symbols share, so it moves every similarity,
and the fitted floors drop by 0.06-0.12. Q4 pools held-out positives and absent
queries. At the new floors, confident-wrong goes from 44.4% to 56.7%
(path) and 59.4% (path-parent). The jump is mostly in **absent** queries,
whose answer is not in the corpus at all: 19.6% -> 52.2% / 58.7% of them
now clear the floor with a wrong hit. Positives move less: 49.8% -> 57.7% /
59.5%. The path header makes wrong symbols look confidently similar, and
Q5 improving on the same floors is the other side of that shift.

### Splits (descriptive, not gated)

Δ against `fp-ctx-none`, point (lower, upper) in points of recall@10 /
MRR.

| split | n | path recall@10 | path MRR | parent recall@10 | parent MRR | path-parent recall@10 | path-parent MRR |
|---|---:|---|---|---|---|---|---|
| targetDoc = doc | 254 | +4.6 (+1.6, +7.7) | +.045 (+.017, +.073) | -1.0 (-3.5, +1.4) | +.007 (-.009, +.023) | +4.5 (+1.2, +8.0) | +.048 (+.017, +.080) |
| targetDoc = sig | 130 | +6.2 (-0.2, +12.7) | +.008 (-.029, +.046) | +2.0 (-2.5, +6.7) | +.048 (+.018, +.080) | +5.7 (-1.6, +12.8) | +.035 (-.005, +.076) |
| targetDoc = mixed | 16 | +0.0 | +.013 (-.019, +.044) | +0.0 | -.018 (-.061, +.009) | +0.0 | +.028 (-.044, +.102) |
| pathOverlap = true | 111 | **+10.9 (+4.5, +17.8)** | **+.113 (+.067, +.159)** | +6.1 (+1.1, +11.8) | +.095 (+.055, +.137) | **+15.2 (+8.0, +22.9)** | **+.156 (+.105, +.208)** |
| pathOverlap = false | 289 | +3.1 (+0.1, +6.2) | +.003 (-.021, +.027) | -2.2 (-4.8, +0.2) | -.005 (-.019, +.009) | +1.2 (-2.1, +4.6) | +.006 (-.020, +.031) |

`pathOverlap = true` means a query shares a sub-token of length >= 4 with
an expected symbol's file path or parent name (D3's rule). Those 111
queries carry the effect. They are also where the design expects an
"author leak": D3's authors read the target file, so path words can land
in the query. On the other 289 queries, only `path`'s recall@10 lower bound
is above zero, by 0.1 points, and none of the MRR bounds is.

## Against the shipped model (int8 1024, `full`)

`report --reference jina-v2-base-code-int8` against the stored GM-398 run.
The first-paragraph row is GM-423's stored run, and C0 shows it is
identical to `fp-ctx-none`.

| | int8 1024 | first-paragraph | path | parent | path-parent |
|---|---:|---:|---:|---:|---:|
| recall@10 | 0.637 | 0.625 | 0.675 | 0.620 | 0.672 |
| MRR | 0.424 | 0.419 | 0.452 | 0.435 | 0.463 |
| Q1 Δ recall@10 | | -1.2 (-2.5) | +3.8 (+1.0) | -1.7 (-4.0) | +3.5 (+0.5) |
| Q2 Δ MRR | | -0.005 (-0.014) | +0.028 (+0.004) | +0.011 (-0.005) | +0.039 (+0.014) |
| Q3 worst language | | rust -4.0 | rust +3.0 | rust -6.0 | go +1.0 |
| Q4 Δ confident-wrong (upper) | | -2.2 (+0.3) pass | +10.0 (+14.8) **FAIL** | +6.3 (+9.4) **FAIL** | +12.5 (+17.6) **FAIL** |
| Q5 Δ false alarm (upper) | | -1.2 (+0.0) | -7.3 (-1.8) | -7.7 (-2.8) | -6.1 (-0.5) |
| pass time vs int8* | | 0.65x | 0.72x | 0.60x | 0.74x |
| role / verdict | | quality gates pass | Fail (Q4) | Fail (Q1, Q2, Q4) | Fail (Q4) |

\* See [Pass time](#pass-time). For "vs none", the baseline is
first-paragraph's valid run in the same window (the same text as
`fp-ctx-none`, per C0). "vs int8" divides by GM-465's round-1 int8 pass
(539.8 s, another evening), because no int8 run in this window was valid.
first-paragraph's 0.65x is GM-465's two-round median. These ratios are
indicative; GM-466 supplies the cost ratios.

As a quality candidate against int8, `path` and `path-parent` pass Q1-Q3
and fail Q4, so they fail in any role. None of the context arms passes
(2).

### GM-434 columns: the shipped int8 floors, not re-fitted

`shipped_floor_rates.py --floors shipped-int8` (go .57, python .57,
rust .55, typescript .53), NL held-out half, all languages:

| arm | false alarm | confident-wrong (positives) | confident-wrong (absent) | mechanical false alarm |
|---|---:|---:|---:|---:|
| none | 13.4% (9/67) | 53.5% (115/215) | 23.9% (11/46) | 1.0% (6/605) |
| path | 25.0% (17/68) | 47.9% (103/215) | 21.7% (10/46) | 8.0% (53/665) |
| parent | 9.2% (6/65) | 55.8% (120/215) | 23.9% (11/46) | 2.1% (12/574) |
| path-parent | 17.6% (12/68) | 47.0% (101/215) | 19.6% (9/46) | 8.6% (55/641) |
| shuffled | 24.5% (13/53) | 43.7% (94/215) | 17.4% (8/46) | 11.7% (70/596) |

At the shipped floors, the path arms look better on confident-wrong and
much worse on false alarm: NL 13% -> 18-25%, mechanical 1% -> 8-9%. So a
path header cannot ship on today's floors. The re-fitted floors in the D9
tables are what a product change would use, and at those floors Q4 fails.

## Tokens per text

Tokenizer `work/models/jina-v2-base-code-int8/tokenizer.json`, special
tokens included, no truncation. Pooled over the six corpora (15,625
texts). The header column is tokens added over the same node's
no-context text.

| arm | p50 | p90 | p99 | max | >512 | >1024 | header tokens mean / p50 / max |
|---|---:|---:|---:|---:|---:|---:|---|
| none | 24 | 79 | 164 | 741 | 0.01% | 0.00% | 0 |
| path | 36 | 91 | 177 | 757 | 0.01% | 0.00% | 12.0 / 12 / 26 |
| parent | 26 | 80 | 165 | 745 | 0.01% | 0.00% | 1.8 / 0 / 11 |
| path-parent | 38 | 92 | 177 | 760 | 0.01% | 0.00% | 13.5 / 13 / 30 |

On g-mesh (the timed corpus) path-parent adds 13.3 tokens to a 37-token
median text. Most symbols have no parent line: the parent rule covers
members of a type found in the snapshot (1,360 of g-mesh's 6,774 texts)
and Rust trait impls.

## Pass time

Owner decision during the run: timing round 2 for GM-455 was **not run**.
Wall-clock pass times on this laptop varied by up to 33% per arm that day,
so a second round could not change a decision that Q4 already makes. The
cost ratios for GM-423's ADR come from GM-466, a token-based cost model
calibrated in short cool bursts. It uses these runs only to validate the
model. What follows is round 1, with that caveat.

**Method.** g-mesh only, `--embed-only`, one invocation at a time. Before
each one, the script's gate waits until `pmset -g therm` shows
CPU_Speed_Limit = CPU_Scheduler_Limit = 100, the 1-minute load is below
4, and no `jamf policy`, cargo or rustc process runs, all held for 2
minutes. **Validity**: an invocation counts if it started from the open
gate and the 1-minute load stayed <= 20 during it. The pass itself holds
the load near 5, so a peak above 20 means about 15 came from outside
(jamf and the like). An invalid invocation was retried once.
Throttling *during* a pass is not grounds for a retry. This 4-core laptop
throttles under every 4-thread pass: each valid run started at 100/100,
bottomed at CPU_Speed_Limit 56-75 and ended at 78-80, and 73-95% of the
10 s samples were below 100. So the ratios compare arms under the same
conditions: a cool start, then throttling during the run. A slower arm
spends more of its run throttled, which slightly exaggerates its ratio.
The first rule (after-state throttled = invalid) was dropped after the
first invocation, because it would have rejected every pass. The gate's
waiting totalled 10,400 s, inside the 3-hour cap.

| stage | arm | attempt | embed s | real | user | sys | user/real | max RSS MiB | load before -> after (1/5/15) | pmset start / min / end | max load1 in run | valid |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|---:|---|
| GM-455 r1 | none | 1 | 361.7 | 363.7 | 1363.1 | 7.0 | 3.75 | 505 | 2.93 4.86 8.85 -> 10.29 12.20 11.20 | 100 / 63 / 73 | 36.8 | no (load) |
| GM-455 r1 | none | 2 | 481.1 | 483.1 | 1740.9 | 10.7 | 3.60 | 500 | 3.41 5.05 7.69 -> 18.91 21.60 15.96 | 100 / 46 / 51 | 45.2 | no (load) |
| GM-455 r1 | path | 1 | 427.3 | 429.4 | 1640.4 | 8.2 | 3.82 | 544 | 2.53 7.14 9.93 -> 8.16 9.95 10.82 | 100 / 63 / 75 | 28.8 | no (load) |
| GM-455 r1 | path | 2 | **386.1** | 388.6 | 1493.1 | 8.9 | 3.84 | 524 | 2.77 6.07 8.90 -> 7.40 7.94 8.88 | 100 / 73 / 78 | 12.3 | yes |
| GM-455 r1 | parent | 1 | **324.7** | 326.8 | 1269.4 | 5.9 | 3.88 | 502 | 3.11 5.21 7.40 -> 13.83 7.98 7.85 | 100 / 75 / 80 | 8.1 | yes |
| GM-455 r1 | path-parent | 1 | 422.4 | 424.5 | 1633.0 | 8.2 | 3.85 | 519 | 2.71 3.79 5.38 -> 7.56 9.02 7.93 | 100 / 56 / 80 | 31.2 | no (load) |
| GM-455 r1 | path-parent | 2 | **401.1** | 403.2 | 1569.5 | 7.1 | 3.89 | 503 | 3.28 5.65 6.70 -> 7.71 7.32 7.14 | 100 / 75 / 78 | 10.6 | yes |
| GM-465 r2 | first-paragraph (= none's text) | 1 | **335.6** | 337.7 | 1305.1 | 7.0 | 3.86 | 495 | 2.56 4.23 4.96 -> 8.64 8.14 6.65 | 100 / 65 / 80 | 13.6 | yes |

`fp-ctx-none` got no valid run: outside load hit both attempts, and the
gate did not open again within its budget (the load sat at 4-5.6 for 50
minutes). first-paragraph embeds the same texts, and C0 shows the vectors
are identical, so its valid run from the GM-465 round in the same window
(below, and in [`gm-465-structured-trim.md`](gm-465-structured-trim.md))
is the no-context baseline:

| arm | embed s | vs no context (335.6 s) | header tokens (g-mesh mean) |
|---|---:|---:|---:|
| path | 386.1 | 1.15x | 12.5 |
| parent | 324.7 | 0.97x | 1.0 |
| path-parent | 401.1 | 1.20x | 13.3 |

All three are within the go bar's 1.5x. The ordering follows the header
tokens: path and path-parent add about a third to g-mesh's 37-token
median text. But one run per arm is inside the day's noise:
`fp-ctx-none` ran 362 s and 481 s at the same gate, and parent ran faster
than no context although it adds tokens. Use GM-466 for the ratios.

**Controls.** Every timing pass reproduced its quality run's vectors
bit for bit (max |Δ| 0 over 5,202,432 floats, including the invalid
attempts), so the timed work is the measured work. user/real was 3.60-3.89
in every invocation, so the pass kept its four cores while `real` ran long,
and no run was stalled waiting.

## Churn

`debug-embed-eval churn --corpus g-mesh`, every instance of each edit,
cache misses per instance (distinct new text hashes), mean / p50 / p90 /
max. 1,650 s wall time for all five arms (E5 on the path arms dominates).

| edit | instances | none | path | parent | path-parent | shuffled |
|---|---:|---|---|---|---|---|
| E1 body edit adding a call | 5,041 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 | 0 / 0 / 0 / 0 |
| E2 rename a function | 5,041 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 |
| E3 add a method to a type | 243 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1 / 1 / 1 / 1 | 1305* |
| E4 rename a type | 243 | 11.9 / 5 / 30 / 217 | 19.0 / 5 / 68 / 240 | 17.3 / 9 / 39 / 220 | 31.6 / 10 / 115 / 243 | 31.9 / 10 / 115 / 243 |
| E5 rename a file | 333 | 0 / 0 / 0 / 0 | 20.2 / 16 / 47 / 128 | 0 / 0 / 0 / 0 | 20.3 / 16 / 47 / 128 | 26.0 / 18 / 58 / 206 |

- E1, the common edit, costs zero embeddings in every arm: no arm reads
  the body.
- Parent adds the renamed type's members to E4 (p50 5 -> 9). Path makes
  E5 cost every symbol in the file (p50 16, max 128). Both changes stay
  inside the file the diff already re-extracts, except that E4 also
  rewrites other files' signatures, as it does today.
- E4's path mean (19.0 vs 11.9) is higher than none's even though the path
  doesn't change. The likeliest reason is that identical texts in
  different files (the same signature) share one cache entry without a
  path and become distinct with one. This was not checked instance by
  instance.
- \* Shuffled E3 is an artefact of the control: adding a node changes the
  set the parent lines are deranged over, so every parent line moves. It
  is not a product cost.

## Go / no-go

**No-go: "inconclusive = keep".** The embedded text stays GM-423's form
with no context.

- No arm passes (1) as a quality candidate against `fp-ctx-none`. `path`
  and `path-parent` pass both lower bounds but fail Q4 by a wide margin
  (upper bounds +17 and +20 against +5). `parent` fails Q1 too.
- The second no-go clause also applies: `path-parent`'s gain is only in
  `pathOverlap = true`, with nothing on the rest (+1.2, lower bound -2.1).
  `path`'s gain on the rest is +3.1 with a lower bound of +0.1, which is
  too thin to carry a product change on its own.
- C0 and C1 pass, and E1 churn is 0 for every arm. So the no-go does not
  come from a broken measurement or a churn problem.
- Under-power is not the problem here. It was the known risk (400
  positives, and the sig split n=130 is too small to gate on), but the
  gate failure is Q4, not a lower bound near zero.
- What would change the answer: a query set whose authors did not read the
  target file (GM-460's larger eval). If `pathOverlap = false` then showed
  a gain of a few points with its lower bound above zero and Q4 held, the
  cost side would be small: about 13 header tokens and file-local churn.

## Reproduce

```sh
cargo build --release
ln -s <main checkout>/eval/embedding/work eval/embedding/work   # in a worktree
# quality (any load), then churn and dumps
QARMS='fp-ctx-none fp-path fp-parent fp-path-parent fp-path-parent-shuffled' \
  SKIP_TIMING=1 SKIP_REPORTS=1 bash eval/embedding/gm455_measure.sh
# timing only: waits for pmset 100/100, load < 4, no jamf/cargo/rustc
SKIP_QUALITY=1 SKIP_CHURN=1 SKIP_REPORTS=1 caffeinate -ims bash eval/embedding/gm455_measure.sh
OUT=$PWD/eval/embedding/work/runs-gm455 python3 eval/embedding/gm455_summary.py reports
```
