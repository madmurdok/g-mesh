# GM-423: shorter embedding input

Slice S1 (measure) of GM-423. The shipped model, jina-v2-base-code int8
(ADR 0011), scored on GM-398's eval
([`embedding-eval.md`](../architecture/embedding-eval.md), D5-D9 unchanged)
with three shorter inputs against its own 1024-token baseline:

| arm | variant row | input |
|---|---|---|
| baseline | `jina-v2-base-code-int8` (stored GM-398 run) | `text_to_embed`, truncated to 1024 tokens (`DEFAULT_MAX_SEQUENCE_LENGTH`) |
| seq512 | `jina-v2-base-code-int8-seq512` | same text, truncated to 512 |
| seq256 | `jina-v2-base-code-int8-seq256` | same text, truncated to 256 |
| first-paragraph | `jina-v2-base-code-int8-first-paragraph` | doc comment cut at its first blank line, then the signature (GM-398 option 3), truncated to 1024 |

The decision (and any change to `DEFAULT_MAX_SEQUENCE_LENGTH`,
`text_to_embed` or `embeddingVersion`) is the next slice. This page only
measures.

**Headline.** Only first-paragraph passes D9 against the int8 baseline, as a
cost candidate: pass time 0.58x (gate 0.60x), recall@10 -1.2 points (lower
bound -2.5), confident-wrong -2.2 points, false alarm -1.2 points. seq512 and
seq256 keep recall (Δ 0.0 and -0.2 points) but fail Q4 on its point estimate
(confident-wrong +0.9 and +0.4 points, which is 2 and 1 of 215 held-out
positives). seq512 also fails the cost win (0.88x pass time, 0.75x RSS).
seq256 wins on RSS (0.65x). Against fp32, every arm fails Q5 the way int8
itself does.

## Method

- **Harness**: `g-mesh debug-embed-eval` at branch
  `perf/GM-423-embedding-input-length` (from release-3.17.0 at 2d86ccf),
  release build. The same six snapshots, the int8 model files the stored run
  used (`work/models/jina-v2-base-code-int8`, sha256 in each manifest), batch
  of one, production session options.
- **Harness changes (eval only, no product default touched)**:
  - `variants.toml` gains a `text` field (`full` by default,
    `first-paragraph`), applied to the node texts a model arm embeds. The
    variant fingerprint leaves out a default `text`, so the stored GM-398 and
    GM-422 runs still load (checked: the report below reads them).
  - `report --reference <variant>` scores the candidates against a run other
    than `settings.reference` (fp32), here the stored int8 run.
  - The shorter limits need no code: a row's `max_tokens` is already the
    `EncoderSpec::max_sequence_length` the loader truncates to.
  - `shipped_floor_rates.py --floors shipped-int8`: the release-3.17.0
    floors (go 0.57, python 0.57, rust 0.55, typescript 0.53, default 0.53),
    for the GM-434 columns.
- **Scripts**: `eval/embedding/gm423_measure.sh` runs everything as one
  script; `gm423_summary.py` writes the control, `costs.toml` and the
  summary; `gm423_token_lengths.py` the token distribution. Runs are under
  `eval/embedding/work/runs-gm423/` (new directory; the stored runs were only
  read).
- **Order**: round 1 runs every arm on every corpus (quality and timing from
  the same pass), the arm order rotated by corpus, so no arm is always first.
  Round 2 re-times g-mesh (embed only) in the reverse of round 1's g-mesh
  order. `G_MESH_EMBEDDING_CACHE=off` (the harness calls
  `EmbeddingModel::embed` directly anyway).
- **Cost gates**: pass time = g-mesh `embedNodesMs`, median of the two
  rounds; max RSS from `/usr/bin/time -l`, median of the two rounds; model
  size is the same file for every arm (1.00x); query latency = median of the
  g-mesh queries in round 1.

### Controls

- **1024 re-run reproduces the stored baseline**: the int8 row re-run with
  this build gives bit-identical vectors (max |Δ| 0) and byte-identical
  rankings on all six corpora, with the same variant fingerprint. So the
  harness, the model files and the new `text` plumbing at its default change
  nothing, and the stored int8 run is a valid baseline.
- **Broken arms (D7)**: random and shuffled, as in GM-398. Validity passes in
  both reports: int8 as reference clears both broken arms, the random arm
  sits at chance, and int8's re-derived floors equal the shipped ones
  (go/python/rust within 0.03). Every candidate clears the broken arms by
  the same margins.
- **Token counts**: the script's shares above 512 and 1024 equal, to the
  node, the `tokenShareOver512/1024` the harness computed in Rust for the
  stored int8 run (same tokenizer, special tokens included).
- **GM-434 script**: `--floors shipped-int8` on the stored int8 run gives
  held-out false alarm 29.4 / 10.0 / 14.3 / 7.7% (go / python / rust /
  typescript), the figures `similarity.rs` quotes for the shipped floors.

## Token counts per embedded symbol

jina tokenizer, special tokens included, no truncation; nearest-rank
percentiles.

| corpus | form | n | p50 | p90 | p95 | p99 | max | >256 | >512 | >1024 |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| g-mesh | full | 6774 | 44 | 166 | 260 | 705 | 8591 | 5.11% | 1.73% | 0.52% |
| g-mesh | first-paragraph | 6774 | 37 | 99 | 123 | 189 | 359 | 0.18% | 0.00% | 0.00% |
| excalidraw | full | 2758 | 24 | 70 | 93 | 160 | 741 | 0.36% | 0.04% | 0.00% |
| ripgrep | full | 3428 | 17 | 87 | 137 | 249 | 3714 | 0.96% | 0.23% | 0.09% |
| gin | full | 1547 | 16 | 42 | 57 | 101 | 173 | 0.00% | 0.00% | 0.00% |
| py-requests | full | 961 | 19 | 84 | 129 | 255 | 722 | 0.94% | 0.31% | 0.00% |
| task-tracker-mcp | full | 157 | 20 | 170 | 219 | 396 | 418 | 1.91% | 0.00% | 0.00% |
| **pooled** | full | 15625 | 26 | 117 | 174 | 438 | 8591 | 2.57% | 0.83% | 0.24% |
| **pooled** | first-paragraph | 15625 | 24 | 79 | 102 | 164 | 741 | 0.13% | 0.01% | 0.00% |

The median symbol is 26 tokens; a limit of 512 touches 0.83% of symbols
(1.73% on g-mesh), 256 touches 2.57% (5.11%). Cutting at the first paragraph
shortens the tail far more than either limit: g-mesh's p99 goes from 705 to
189 tokens.

## D9 against the int8 1024 baseline

Candidates as cost candidates; Δ = arm - int8, pooled over languages,
one-sided 95% bounds (10,000 paired resamples, seed 398). Floors re-fitted
per arm (D6) on the fit half; Q4/Q5 read on the held-out half.

| | seq512 | seq256 | first-paragraph |
|---|---|---|---|
| recall@10 (int8: 0.637) | 0.637 | 0.635 | 0.625 |
| MRR (int8: 0.424) | 0.420 | 0.421 | 0.419 |
| Q1 Δ recall@10 (≥ -2.0, lower ≥ -5.0) | +0.0 (lower +0.0) pass | -0.2 (-0.8) pass | -1.2 (-2.5) pass |
| Q2 Δ MRR (≥ -0.02, lower ≥ -0.05) | -0.004 (-0.008) pass | -0.003 (-0.008) pass | -0.005 (-0.014) pass |
| Q3 worst language Δ recall@10 (≥ -10) | go +0.0 pass | rust -1.0 pass | rust -4.0 pass |
| Q4 Δ confident-wrong (≤ 0, upper ≤ +5) | **+0.9** (+2.2) FAIL | **+0.4** (+1.3) FAIL | -2.2 (+0.3) pass |
| Q5 Δ false alarm (≤ 0, upper ≤ +5) | -1.2 (+0.0) pass | +0.0 (+0.0) pass | -1.2 (+0.0) pass |
| fitted floors go/py/rust/ts | .57/.56/.55/.53 | .57/.57/.55/.53 | .57/**.59/.58**/.53 |
| pass time vs int8 | 0.88x | 0.77x | **0.58x** |
| max RSS vs int8 | 0.75x | **0.65x** | 0.71x |
| query latency vs int8 | 0.89x | 1.00x | 1.01x |
| C-win (pass ≤ 0.60x or RSS ≤ 0.70x or size ≤ 0.50x) | FAIL | pass (RSS) | pass (pass time) |
| C-no-worse (each ≤ 1.10x) | pass | pass | pass |
| **verdict** | Fail (Q4, C-win) | Fail (Q4) | **Pass** |

int8's own floors re-fitted on this run: go 0.57, python 0.57, rust 0.55,
typescript 0.53, the shipped values.

### The same arms against fp32 (for reference)

| | int8 (shipped) | seq512 | seq256 | first-paragraph |
|---|---|---|---|---|
| Q1 Δ recall@10 | +0.5 (-1.0) | +0.5 (-1.0) | +0.2 (-1.2) | -0.8 (-2.5) |
| Q2 Δ MRR | -0.003 (-0.012) | -0.007 (-0.017) | -0.006 (-0.016) | -0.008 (-0.019) |
| Q4 Δ confident-wrong | -1.4 (+1.0) | -0.5 (+1.9) | -1.0 (+1.5) | -3.6 (-0.4) |
| Q5 Δ false alarm | +2.1 (**+6.2**) | +0.9 (**+5.2**) | +2.1 (**+6.2**) | +0.9 (**+5.2**) |
| verdict | Fail (Q5) | Fail (Q5) | Fail (Q5) | Fail (Q5) |

No arm is worse than the shipped int8 against fp32; all inherit int8's Q5
failure (go +12.5 points: 2 of 16 queries), which GM-422's confirmation eval
already weighed when int8 shipped.

### GM-434 columns: the shipped int8 floors, not re-fitted

What an agent would see if the input changed and the floors did not.
Held-out authored (NL) queries, all languages.

| arm | false alarm | confident-wrong (positives) | confident-wrong (absent) | mechanical false alarm |
|---|---:|---:|---:|---:|
| int8 1024 | 14.3% (10/70) | 51.6% (111/215) | 21.7% (10/46) | 1.6% (9/577) |
| seq512 | 14.5% (10/69) | 52.1% (112/215) | 21.7% (10/46) | 1.7% (10/577) |
| seq256 | 14.5% (10/69) | 52.1% (112/215) | 21.7% (10/46) | 1.6% (9/575) |
| first-paragraph | 13.4% (9/67) | 53.5% (115/215) | 23.9% (11/46) | 1.0% (6/605) |

At the shipped floors first-paragraph trades one fewer false alarm for four
more confident-wrong answers; at its own fitted floors (python 0.59, rust
0.58) it is better on both (table above). Switching to it means shipping new
floors (D6), as any model switch does.

## Embedding time

Total `embedNodesMs` per arm; g-mesh is 6,774 of the 15,625 nodes and
two thirds of the time, and is the only corpus timed twice.

| arm | round 1, all corpora (s) | g-mesh round 1 (s) | g-mesh round 2 (s) | all corpora vs 1024 | g-mesh vs 1024 (mean of rounds) | max RSS g-mesh (MiB) |
|---|---:|---:|---:|---:|---:|---:|
| int8 1024 | 622.5 | 400.1 | 384.0 | 1.000x | 1.000x | 808 |
| seq512 | 563.3 | 345.1 | 345.0 | 0.905x | 0.880x | 579 |
| seq256 | 505.3 | 302.1 | 303.5 | 0.812x | 0.772x | 518 |
| first-paragraph | 404.6 | 225.8 | 227.9 | 0.650x | 0.579x | 557 |

Round 1 sums of `/usr/bin/time -p`: int8 real 656 s / user 2548 s / sys
9.5 s; seq512 596 / 2315 / 8.3; seq256 537 / 2093 / 7.2; first-paragraph
436 / 1687 / 6.5. user/real is 3.9 in every arm and every invocation
(ONNX Runtime's intra-op threads on 4 cores): the process computed the whole
time and never waited, and `real` exceeds `embedNodesMs` only by the model
load and the query embeddings (~5-7 s per invocation).

**Machine state.** Owner's working machine (Intel i7-1068NG7, 4 cores / 8
threads, macOS). Before the run the main checkout's g-mesh daemon (PID
21082) was at 400% CPU for ~10 minutes, running a pass started by this
slice's own g-mesh queries; the run waited until it had been idle for 80 s
and the 1-minute load was below 4 (3.77 at start). Three daemons stayed
alive (not checked for activity during the run). The 1-minute load was 4-7 for most of the run (one embedding
process alone keeps ~4 threads busy) but rose to 8-11 during
py-requests/excalidraw of round 1 (e.g. py-requests seq512 38.0 s vs int8
26.1 s, excalidraw first-paragraph 73.7 s vs int8 61.4 s, although both arms
embed fewer tokens there). What caused that load was not captured, so the
small corpora's per-arm times carry ±30% noise and are not used for gates.
The g-mesh passes ran at load 5-7 and repeat within 4% across rounds
(400.1/384.0, 345.1/345.0, 302.1/303.5, 225.8/227.9); they are the cost
basis. The absolute times are lower than GM-398's stored run (g-mesh 585 s)
on a busier machine; only ratios within this run are compared.

**Why so much time for so few tokens cut.** seq512 truncates 1.7% of
g-mesh's symbols yet saves 12% of the pass: attention cost grows with the
square of the input, so the ~117 g-mesh symbols above 512 tokens (35 above
1024) were a disproportionate share of the pass. first-paragraph shortens
many more texts (p99 705 -> 189) and saves 42%.

## Reading

- The long tail, not the typical symbol, is what the limit buys: the median
  symbol is 26 tokens and even 256 truncates 2.6% of symbols, yet the tail
  costs 12-42% of the pass time and 25-35% of peak RSS.
- Plain truncation is close to free in recall (Δ recall@10 0.0 / -0.2
  points) but fails Q4 by one to two held-out queries' confident-wrong. The
  gate is a point-estimate `Δ <= 0`, both upper bounds are far inside +5, so
  this is a strict-rule failure rather than a measured harm; the rule is
  applied as written ("inconclusive = keep").
- first-paragraph is the only arm that passes every gate, and does so on
  pass time (0.58x vs a 0.60x gate, both rounds under it: 0.56x and 0.59x),
  at a cost of -1.2 points recall@10 (rust -4.0) and floors that move for
  python and rust.
- Not measured here: the agent-level check (D10) for first-paragraph, and
  whether a limit of 512 *combined* with first-paragraph changes anything
  (first-paragraph already leaves one symbol above 512 tokens pooled).

## Reproduce

```sh
cargo build --release
ln -s <main checkout>/eval/embedding/work eval/embedding/work   # in a worktree
bash eval/embedding/gm423_measure.sh          # ~1 h; writes work/runs-gm423/summary.md
python3 eval/embedding/gm423_token_lengths.py
```
