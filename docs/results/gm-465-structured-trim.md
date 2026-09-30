# GM-465: structured doc-comment trim

Slice S3 (measure) of GM-465. The shipped model, jina-v2-base-code int8
(ADR 0011), scored on GM-398's eval
([`embedding-eval.md`](../architecture/embedding-eval.md), D5-D9 unchanged)
with the `structured` input form, against its own 1024-token baseline and
against GM-423's `first-paragraph`
([`gm-423-sequence-length.md`](gm-423-sequence-length.md)):

| arm | variant row | input |
|---|---|---|
| baseline | `jina-v2-base-code-int8` (stored GM-398 run) | `text_to_embed`, truncated to 1024 tokens |
| first-paragraph | `jina-v2-base-code-int8-first-paragraph` (stored GM-423 run) | doc comment cut at its first blank line, then the signature, 1024 |
| structured | `jina-v2-base-code-int8-structured` | doc comment with its summary, headings and short prose kept, code blocks, parameter lists and link lines dropped (`core/src/cli/embed_eval/structured.rs`), then the signature, 1024 |

This page only measures; the choice for GM-423's ADR is the Recommendation
at the end.

**Headline.** structured passes D9 against int8 with **no recall loss**
(recall@10 0.637 = int8, Δ +0.0, lower bound -0.5; rust -1.0 vs
first-paragraph's -4.0), confident-wrong -1.8 points, false alarm -1.3
points. Its cost win is narrow: pass time 0.72x (fails the 0.60x gate), max
RSS 0.696x (passes the 0.70x gate by 0.004, one round). In this run
first-paragraph costs 0.67x pass time / 0.68x RSS, so structured spends
about 6% more embedding time than first-paragraph for +1.2 points recall@10.
**Only one clean timing round exists** (round 2 was discarded for thermal
throttling, below); a second clean round is owed before GM-423's ADR relies
on any cost ratio.

## Method

- **Harness**: `g-mesh debug-embed-eval` at branch
  `perf/GM-465-structured-doc-trim` (e9f24e5: GM-423's harness, GM-459's
  stderr progress, GM-465's `structured` text form), release build. Same
  six snapshots and int8 model files as GM-398/GM-423, batch of one,
  production session options, `G_MESH_EMBEDDING_CACHE=off`.
- **Scripts**: `eval/embedding/gm465_measure.sh` (adapted from
  `gm423_measure.sh`) runs the set; `gm465_summary.py` (imports
  `gm423_summary.py`) writes the controls, `costs.toml` and the summary;
  `gm423_token_lengths.py` gained `--runs` so its harness-manifest control
  finds the GM-423 and GM-465 run directories. Runs are under
  `eval/embedding/work/runs-gm465/` (new; stored runs only read).
- **Quality**: structured on all six corpora, one pass (its g-mesh pass is
  timing round 1). first-paragraph's quality is GM-423's stored run; its
  g-mesh re-run here is bit-identical (Controls). `report --reference
  jina-v2-base-code-int8` against the stored int8 run, and against fp32 for
  reference, exactly as GM-423.
- **Timing**: g-mesh only, all three arms in this run so ratios are
  comparable. Round 1 in full mode (gives query latency): int8,
  first-paragraph, structured. Round 2 embed-only, reversed. Before every
  timed invocation the script waits for 1-minute load < 5 and no
  jamf/cargo/rustc process, and records `uptime` before/after and
  `/usr/bin/time -lp`. The rerun ran under `caffeinate -ims`.
- **Cost gates**: pass time = g-mesh `embedNodesMs` and max RSS, from
  **round 1 only** (`COST_ROUNDS=1`, see Timing); query latency = median of
  round-1 g-mesh queries; model size the same file (1.00x).

### Controls

- **int8 re-run reproduces the stored baseline**: g-mesh round 1 vs the
  stored GM-398 int8 run: vectors max |Δ| 0, rankings byte-identical,
  variant fingerprint equal.
- **first-paragraph re-run reproduces GM-423's stored run**: g-mesh, max
  |Δ| 0, rankings byte-identical, fingerprint equal. So comparing structured
  with the stored first-paragraph quality run is valid.
- **structured is reproducible**: a second pass on task-tracker-mcp gives
  max |Δ| 0 and byte-identical rankings; structured's round-2 g-mesh vectors
  (embed-only, throttled machine) equal round 1's, max |Δ| 0 over
  5,202,432 floats. (int8/first-paragraph have no round-2 vectors: that
  round was stopped.)
- **Broken arms (D7)**: validity passes in both reports (no validity
  errors); random 0.005 and shuffled 0.015 recall@10, int8's re-derived
  floors equal the shipped ones.
- **Token counts**: for every corpus and form, the script's shares above 512
  and 1024 equal the `tokenShareOver512/1024` the harness computed in Rust
  for that run, including structured's own runs; so the Python port of the
  structured rules agrees with the Rust ones on these shares.
- **Machine**: quiet gate above; per-invocation load and user/real in the
  Timing table.

## Token counts per embedded symbol

jina tokenizer, special tokens included, no truncation; nearest-rank
percentiles. `full` and `first-paragraph` rows repeat GM-423's (same
script, same numbers).

| corpus | form | n | p50 | p90 | p95 | p99 | max | >256 | >512 | >1024 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| g-mesh | full | 6774 | 44 | 166 | 260 | 705 | 8591 | 5.11% | 1.73% | 0.52% |
| g-mesh | first-paragraph | 6774 | 37 | 99 | 123 | 189 | 359 | 0.18% | 0.00% | 0.00% |
| g-mesh | structured | 6774 | 40 | 108 | 131 | 201 | 359 | 0.21% | 0.00% | 0.00% |
| ripgrep | full | 3428 | 17 | 87 | 137 | 249 | 3714 | 0.96% | 0.23% | 0.09% |
| ripgrep | first-paragraph | 3428 | 17 | 44 | 57 | 92 | 315 | 0.09% | 0.00% | 0.00% |
| ripgrep | structured | 3428 | 17 | 68 | 88 | 151 | 549 | 0.18% | 0.03% | 0.00% |
| **pooled** | full | 15625 | 26 | 117 | 174 | 438 | 8591 | 2.57% | 0.83% | 0.24% |
| **pooled** | first-paragraph | 15625 | 24 | 79 | 102 | 164 | 741 | 0.13% | 0.01% | 0.00% |
| **pooled** | structured | 15625 | 25 | 87 | 113 | 175 | 741 | 0.17% | 0.01% | 0.00% |

structured is a little longer than first-paragraph everywhere (g-mesh p99
201 vs 189, pooled p95 113 vs 102) and removes the same tail: nothing above
1024, and above 512 only excalidraw's 741-token symbol (both trims) plus,
for structured, one ripgrep symbol at 549. The
other corpora (excalidraw, gin, py-requests, task-tracker-mcp) differ
between the two trims by at most 9 tokens at p99; full table in
`work/runs-gm465/token_lengths.md`.

## D9 against the int8 1024 baseline

Δ = arm - int8, pooled over languages, one-sided 95% bounds (10,000 paired
resamples, seed 398). Floors re-fitted per arm (D6) on the fit half; Q4/Q5
on the held-out half. Cost ratios from this run's round 1.

| | structured | first-paragraph (comparison) |
|---|---|---|
| recall@10 (int8: 0.637) | **0.637** | 0.625 |
| MRR (int8: 0.424) | 0.421 | 0.419 |
| Q1 Δ recall@10 (≥ -2.0, lower ≥ -5.0) | +0.0 (-0.5) pass | -1.2 (-2.5) pass |
| Q2 Δ MRR (≥ -0.02, lower ≥ -0.05) | -0.003 (-0.010) pass | -0.005 (-0.014) pass |
| Q3 worst language Δ recall@10 (≥ -10) | rust -1.0 pass | rust -4.0 pass |
| Q4 Δ confident-wrong (≤ 0, upper ≤ +5) | -1.8 (+0.3) pass | -2.2 (+0.3) pass |
| Q5 Δ false alarm (≤ 0, upper ≤ +5) | -1.3 (+0.0) pass | -1.2 (+0.0) pass |
| fitted floors go/py/rust/ts | .57/**.59**/**.57**/.53 | .57/**.59/.58**/.53 |
| pass time vs int8 | 0.72x | 0.67x (GM-423: 0.58x) |
| max RSS vs int8 | **0.696x** | 0.68x (GM-423: 0.71x) |
| query latency vs int8 | 0.92x | 1.71x* (GM-423: 1.01x) |
| C-win (pass ≤ 0.60x or RSS ≤ 0.70x or size ≤ 0.50x) | pass (RSS, margin 0.004) | pass (RSS) |
| C-no-worse (each ≤ 1.10x) | pass | FAIL* (query latency) |
| **verdict** | **Pass** | Fail (C-no-worse)* |

\* first-paragraph's query latency (16.1 ms vs int8 9.4 ms) was measured at
the end of its pass while the 1-minute load climbed to 53.9 (load after the
invocation; 3.3 before). Queries are embedded after the nodes, so they took
the hit; GM-423 measured 1.01x for the same text. Read this C-no-worse
failure as noise, not a property of first-paragraph; its quality gates are
unchanged from GM-423.

int8's own floors re-fitted on this run: go 0.57, python 0.57, rust 0.55,
typescript 0.53, the shipped values.

### The same arms against fp32 (for reference)

| | int8 (shipped) | first-paragraph | structured |
|---|---|---|---|
| Q1 Δ recall@10 | +0.5 (-1.0) | -0.8 (-2.5) | +0.5 (-1.0) |
| Q2 Δ MRR | -0.003 (-0.012) | -0.008 (-0.019) | -0.007 (-0.016) |
| Q4 Δ confident-wrong | -1.4 (+1.0) | -3.6 (-0.4) | -3.2 (-0.2) |
| Q5 Δ false alarm | +2.1 (**+6.2**) | +0.9 (**+5.2**) | +0.8 (**+5.2**) |
| verdict | Fail (Q5) | Fail (Q5) | Fail (Q5) |

All three inherit int8's Q5 failure (go +12.5 points, 2 of 16 queries),
already weighed when int8 shipped. structured matches int8's recall against
fp32 and is better on confident-wrong and false alarm.

### GM-434 columns: the shipped int8 floors, not re-fitted

Held-out authored (NL) queries, all languages.

| arm | false alarm | confident-wrong (positives) | confident-wrong (absent) | mechanical false alarm |
|---|---:|---:|---:|---:|
| int8 1024 | 14.3% (10/70) | 51.6% (111/215) | 21.7% (10/46) | 1.6% (9/577) |
| first-paragraph | 13.4% (9/67) | 53.5% (115/215) | 23.9% (11/46) | 1.0% (6/605) |
| structured | 12.1% (8/66) | 53.0% (114/215) | 21.7% (10/46) | 1.0% (6/594) |

At the shipped floors structured trades two fewer false alarms for three
more confident-wrong positives (first-paragraph: one for four); at its own
fitted floors (python 0.59, rust 0.57) it is better than int8 on both.
Either trim means shipping new python/rust floors (D6).

## Embedding time (g-mesh)

| arm | round 1 embed (s) | vs int8 | real | user | sys | user/real | max RSS (MiB) | query median (ms) | load before -> after (1/5/15 min) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| int8 1024 | 539.8 | 1.000x | 548.7 | 2126.8 | 9.7 | 3.88 | 737 | 9.41 | 4.79 15.02 55.40 -> 6.82 9.02 33.01 |
| first-paragraph | 363.9 | 0.674x | 377.0 | 1436.6 | 7.2 | 3.81 | 501 | 16.08 | 3.29 6.45 27.47 -> 53.91 19.34 24.97 |
| structured | 386.5 | 0.716x | 395.5 | 1518.3 | 8.2 | 3.84 | 513 | 8.68 | 3.46 9.46 18.70 -> 7.48 10.69 16.42 |

structured / first-paragraph = 1.062x pass time, 1.025x RSS.

**Round 2 discarded (thermal throttling).** Round 2 (embed-only, order
structured, first-paragraph, int8) started at 18:06 with 1-minute load 4.26,
but the laptop was thermally throttled: `pmset -g therm` read
CPU_Speed_Limit 24 and CPU_Scheduler_Limit 54 with load ~725. structured
took 712.0 s (round 1: 386.5 s) at user/real 2.74 (round 1: 3.84), load
after 18.27 / 101.46 / 76.20; first-paragraph ran 27+ minutes before the
orchestrator stopped the script with the owner's approval; int8 never ran.
The structured row stays in `work/runs-gm465/timing.tsv` but is left out of
every gate (`COST_ROUNDS=1`).

**Earlier attempts, also discarded.**
- Dry run and first full run (15:07-15:39): a corporate `jamf policy` run
  pushed the 1-minute load to 600+ (65% sys CPU, 300 runnable processes);
  task-tracker-mcp took 84 s instead of ~6 s. Only the structured quality
  passes on the five small corpora and the reproducibility pass were kept
  from that run (quality is deterministic; the controls confirm it). Their
  timings are in `timing-attempt1.tsv` and not used.
- The first attempt's int8 g-mesh pass (682.4 s, 15:41) started with the
  5-minute load at 96 after jamf; then the machine slept ~94 minutes inside
  the quiet-gate loop and the run hit the tool's 2 h limit. Discarded; the
  timing rounds were redone from scratch under `caffeinate`.

**Machine state.** Owner's working machine (Intel i7-1068NG7, 4 cores / 8
threads, macOS). Two g-mesh daemons (main checkout) alive and idle at start.
Round 1's three passes each started at 1-minute load 3.3-4.8 and computed
the whole time (user/real 3.8-3.9, ONNX Runtime's intra-op threads on 4
cores; `real` exceeds `embedNodesMs` by model load plus queries, 9-13 s).
Absolute times are slower than GM-423's (int8 540 s vs 392 s median) and
varied by 26% across today's attempts (682 vs 540 s for the same int8
pass), so only ratios inside one round are compared, and even those rest on
a single round: GM-423's two rounds agreed within 4%, which is the only
evidence of this machine's round-to-round spread.

## Reading

- structured keeps int8's retrieval quality where first-paragraph gives
  some away: recall@10 0.637 vs 0.625, worst language rust -1.0 vs -4.0,
  MRR 0.421 vs 0.419. On the calibration gates the two trims are equal
  (Q4 -1.8 vs -2.2, Q5 -1.3 vs -1.2, both upper bounds +0.3 / +0.0).
- It keeps most of first-paragraph's cost saving: the tail it removes is the
  same (nothing above 1024, p99 201 vs 189 on g-mesh), and it costs 6% more
  pass time and 2.5% more RSS than first-paragraph in the same round.
- The D9 cost gate is the weak point for both trims, not quality. In this
  round neither reaches the 0.60x pass-time gate (0.72x, 0.67x); both pass
  C-win only on RSS, structured by 0.004. GM-423's 0.58x for first-paragraph
  was not reproduced (0.67x here), and one round cannot tell which is the
  machine.
- The difference that matters for the ADR is therefore quality (+1.2 points
  recall@10, rust +3.0) against ~6% of embedding time, with the cost gate
  itself unsettled for either trim.

## Recommendation for GM-423's ADR

**Adopt `structured`**, conditional on a second clean timing round.

- Quality decides it: structured is the only trim with no recall loss
  (Δ recall@10 +0.0 vs -1.2; rust -1.0 vs -4.0), with the same
  confident-wrong/false-alarm improvement as first-paragraph and the same
  floor changes (python 0.59, rust 0.57 vs 0.58).
- Its price over first-paragraph is small and measured in the same round:
  1.06x pass time, 1.03x RSS.
- Both trims beat full input by a wide margin (0.67-0.72x pass time,
  0.68-0.70x RSS); full input stays only if the ADR wants no floor change.
- **Owed before the ADR relies on the cost numbers**: one more g-mesh round
  (int8, first-paragraph, structured, alternated) on a cool, quiet machine
  (`pmset -g therm` with no speed limit). If structured's RSS ratio lands
  above 0.70x and pass time above 0.60x, it fails C-win as a cost candidate
  and the ADR must either accept it on the quality argument (it is no worse
  than int8 on every quality gate) or fall back to first-paragraph, whose
  own 0.60x pass-time pass is now also in doubt (0.58x vs 0.67x).
- GM-455 (structural context prefix) is measured next on first-paragraph;
  whichever trim the ADR adopts is then crossed with the GM-455 winner.

## Reproduce

```sh
cargo build --release
ln -s <main checkout>/eval/embedding/work eval/embedding/work   # in a worktree
caffeinate -ims bash eval/embedding/gm465_measure.sh     # quality + timing; waits for a quiet machine
SKIP_QUALITY=1 caffeinate -ims bash eval/embedding/gm465_measure.sh  # timing rounds only
SKIP_RUNS=1 COST_ROUNDS=1 bash eval/embedding/gm465_measure.sh       # reports from round 1 only (as here)
```
