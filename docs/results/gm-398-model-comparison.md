# GM-398: embedding model comparison

Slice S14 (measure) of GM-398. The candidates of
[`embedding-eval.md`](../architecture/embedding-eval.md) D8 scored against the
reference R = jina-v2-base-code fp32 on the approved eval, and D9's gates
applied as written.

**Result: no candidate passes D9. Keep jina-v2-base-code fp32. There is no
finalist, so D10's agent-level check does not run.** The closest is jina int8:
it passes Q1-Q4 and every cost gate, and fails only Q5's bound (+6.2 points
against a +5 limit). It fails Q5 whether the gate is pooled or per language,
so the owner's pending pooled-vs-per-language choice does not change any
verdict (see [Q5](#q5-false-alarm-pooled-and-per-language)).

## Method

- **Harness**: `g-mesh debug-embed-eval` (`core/src/cli/embed_eval.rs`) at
  branch `docs/GM-398-embedding-eval-model-choice`, release build. D4: the same
  six snapshots for every arm, the crate's own `text_to_embed`,
  `EmbeddingModel` with production session options and batch of one.
- **Variants**: the rows of `eval/embedding/variants.toml`, pinned to Hugging
  Face commits, each with its model card's pooling and prefixes (D8): bge and
  snowflake use the query prefix "Represent this sentence for searching
  relevant passages: ", gte and jina use none. The three small models cap at
  512 tokens and jina at 1024. The share of g-mesh node texts longer than 512
  tokens is 1.7% for every tokenizer, so truncation cannot explain the gaps
  below.
- **Not run**: CodeRankEmbed (the owner has not approved its ONNX export; it
  is one more `[[variant]]` row when he does) and nomic-embed-code (out per D8).
- **Quality**: all six corpora, 500 authored queries (400 positives, 100 absent).
  Floors are re-fitted per model and language on the fit half (D6).
  Confident-wrong and false alarm are read on the held-out half. Paired
  bootstrap against R: 10,000 resamples, seed 398, stratified by language,
  one-sided 95% bounds.
- **Controls (D7)**: the random and shuffled arms are the gated broken arms.
  words-shuffled and bm25 are informational.
- **Cost (D11)**: `G_MESH_EMBEDDING_CACHE=off /usr/bin/time -lp g-mesh
  debug-embed-eval run --variant <v> --corpus g-mesh --embed-only --force`,
  three rounds interleaved R, int8, bge, gte, snowflake. The harness calls
  `EmbeddingModel::embed` directly and never touches the machine-wide cache,
  so every timed pass embeds all 6,774 nodes. Reported:
  - pass time: the harness's embed-all step (`embedNodesMs`), the median of
    three rounds;
  - max RSS: from `time -l`, the median of three rounds;
  - size: `model.onnx` plus `tokenizer.json`, as downloaded;
  - query latency: the median of the 363 g-mesh queries, embedded one by one
    in one process, run back to back for all five arms.
- **Scripts**: `eval/embedding/measure_costs.sh` runs quality, latency, timed
  passes and the report as one script. `eval/embedding/measure_summary.py`
  writes `costs.toml` and the summary tables.
- **Report addition**: `report --json` now also writes
  `falseAlarmDelta` and `falseAlarmDeltaByLanguage` (point, bounds and n per
  language) for each candidate. These fields are reported only and gate
  nothing. The per-language bounds below come from them.

### Machine state: timing did not run on a quiet machine

The runs used the owner's working machine: an Intel i7-1068NG7 (4 cores, 8
threads), 32 GB of RAM, macOS 26.6.2. The owner's apps were open, and the
owner's two g-mesh daemons (PIDs 95527, 95578) were alive and left alone.
Spotlight (`mds`) used about 30% of one core.

D11 asks for a 1-minute load below 2.0 at the start of a run and a redo when
the load after the run exceeds 3.0. On this machine, the idle load is 4-6. The
first attempt waited 30 minutes before each step and never got below 2.0.

**The owner approved going ahead on the noisy machine. Timing ran with load1
of 5.0-6.8 at the start of each run and 6.1-11.7 at the end.** The start
threshold was raised to 7.0. The redo threshold became 8.0 plus the run's own
parallelism: every pass keeps about 3.9 cores busy (user/real 3.82-3.95), so
its own threads alone push load1 above 3.0. No run was redone.

The user/real ratio held at 3.8-3.95 in every run. So each process ran on its
threads the whole time and did not wait on I/O or locks. The noise comes from
sharing CPUs with other work, not from stalls.

## Validity

| check | result |
|---|---|
| R vs random | gap 62.8 points, ratio 0.008, bounds separate: pass |
| R vs shuffled | gap 61.8 points, ratio 0.024, bounds separate: pass |
| random vs analytic chance | 0.005 observed vs 0.015 chance: pass |
| floor parity (go / python / rust) | fitted 0.56 / 0.58 / 0.56 vs shipped 0.59 / 0.57 / 0.55: pass (TypeScript 0.55 vs 0.50, reported, not gated) |
| each candidate vs broken arms | every candidate's recall@10 is 0.55-0.64 against 0.005 (random) and 0.015 (shuffled), with bounds separate: all valid (no `Undecided` verdict) |

D7's top-10 harness-parity check against a production daemon was not re-run
in this slice. The report's floor-parity check above covers the harness side.

## Quality, pooled

Languages are weighted equally. Ranges are the one-sided 95% bounds.

| arm | r@1 | r@5 | r@10 [lo, hi] | MRR [lo, hi] | CW combined | CW positives | CW absent |
|---|---|---|---|---|---|---|---|
| **jina fp32 (R)** | 0.335 | 0.542 | **0.633** [0.595, 0.670] | **0.427** [0.395, 0.460] | 47.9% | 52.6% | 26.1% |
| jina int8 | 0.328 | 0.532 | 0.637 [0.600, 0.675] | 0.424 [0.392, 0.457] | 46.4% | 51.6% | 21.7% |
| gte-small | 0.275 | 0.478 | 0.580 [0.540, 0.617] | 0.373 [0.340, 0.406] | 59.8% | 61.9% | 50.0% |
| bge-small-en-v1.5 | 0.258 | 0.485 | 0.557 [0.518, 0.595] | 0.360 [0.327, 0.392] | 64.0% | 66.5% | 52.2% |
| snowflake-arctic-embed-s | 0.275 | 0.465 | 0.550 [0.510, 0.590] | 0.370 [0.336, 0.403] | 73.2% | 72.6% | 76.1% |
| *random (control)* | 0.003 | 0.003 | 0.005 [0.000, 0.013] | 0.005 | - | - | - |
| *shuffled (control)* | 0.000 | 0.013 | 0.015 [0.005, 0.025] | 0.007 | 27.0% | 31.7% | 0.0% |
| *words-shuffled (info)* | 0.282 | 0.522 | 0.637 [0.600, 0.675] | 0.396 | 59.0% | 64.2% | 34.8% |
| *bm25 (info)* | 0.245 | 0.400 | 0.448 [0.407, 0.488] | 0.320 | 73.9% | 72.6% | 80.4% |

## Quality per language and per corpus

| arm | go | python | rust | ts | excalidraw | g-mesh | gin | py-requests | ripgrep | task-tracker-mcp |
|---|---|---|---|---|---|---|---|---|---|---|
| jina fp32 (R) | 0.74 | 0.69 | 0.34 | 0.76 | 0.62 | 0.36 | 0.74 | 0.69 | 0.32 | 0.90 |
| jina int8 | 0.73 | 0.71 | 0.36 | 0.75 | 0.62 | 0.36 | 0.73 | 0.71 | 0.36 | 0.88 |
| gte-small | 0.70 | 0.63 | 0.32 | 0.67 | 0.52 | 0.38 | 0.70 | 0.63 | 0.26 | 0.82 |
| bge-small-en-v1.5 | 0.67 | 0.59 | 0.31 | 0.66 | 0.52 | 0.36 | 0.67 | 0.59 | 0.26 | 0.80 |
| snowflake-arctic-embed-s | 0.68 | 0.57 | 0.35 | 0.60 | 0.50 | 0.44 | 0.68 | 0.57 | 0.26 | 0.70 |

The table shows recall@10. Each model's floors were re-fitted per D6, in the
order go / python / rust / ts, and are listed below with its own held-out
false-alarm rate.

| arm | floors | held-out false alarm |
|---|---|---|
| jina fp32 (R) | 0.56 / 0.58 / 0.56 / 0.55 | 18.8 / 9.5 / 14.3 / 14.8% |
| jina int8 | 0.57 / 0.57 / 0.55 / 0.53 | 29.4 / 10.0 / 14.3 / 7.7% |
| gte-small | 0.86 / 0.86 / 0.85 / 0.84 | 25.0 / 7.7 / 0.0 / 5.0% |
| bge-small-en-v1.5 | 0.69 / 0.71 / 0.70 / 0.67 | 8.3 / 0.0 / 0.0 / 11.8% |
| snowflake-arctic-embed-s | 0.57 / 0.57 / 0.59 / 0.58 | 0.0 / 0.0 / 10.0 / 0.0% |

## Q5 false alarm, pooled and per language

Q5 compares each arm at its own floors, paired over the held-out positives
that both arms rank first. The table gives Δ = C - R in points with the
one-sided 95% bounds. n is the number of such paired queries. The pooled row
is the gate: Δ <= 0 and upper <= +5.

| candidate | pooled Δ [lo, up] | go | python | rust | typescript |
|---|---|---|---|---|---|
| jina int8 | **+2.1 [-1.4, +6.2]: FAIL** | +12.5 [0.0, +25.0] n=16 | 0.0 [0.0, 0.0] n=20 | 0.0 [0.0, 0.0] n=6 | -4.0 [-12.0, 0.0] n=25 |
| gte-small | -10.3 [-22.3, 0.0]: pass | 0.0 [0.0, 0.0] n=6 | -9.1 [-27.3, 0.0] n=11 | -25.0 [-50.0, 0.0] n=4 | -7.1 [-28.6, +14.3] n=14 |
| bge-small-en-v1.5 | -12.1 [-25.0, -0.2]: pass | -20.0 [-60.0, 0.0] n=5 | -8.3 [-25.0, 0.0] n=12 | -20.0 [-60.0, 0.0] n=5 | 0.0 [-23.1, +23.1] n=13 |
| snowflake-arctic-embed-s | -10.8 [-21.6, -2.3]: pass | 0.0 [0.0, 0.0] n=7 | 0.0 [0.0, 0.0] n=10 | -25.0 [-50.0, 0.0] n=4 | -18.2 [-36.4, 0.0] n=11 |

Reading it for the owner's pooled-vs-per-language decision:

- **jina int8** fails both ways. The pooled upper bound is +6.2, over the +5
  limit. Per language, go is +12.5 with an upper bound of +25: 2 of 16 paired
  go queries that R leaves above its floor fall below int8's floor.
- **The three small models** pass pooled. Per language, bge's typescript upper
  bound (+23.1) and gte's (+14.3) would fail a per-language +5 bound. They
  already fail Q1, Q2 and Q4, so the choice does not change their verdicts.
- The per-language n runs from 4 to 25. One query moves a language's Δ by 4-25
  points, which is the design note's reason for pooling.

## Cost (D11, g-mesh corpus, 6,774 nodes)

### Timed passes

| variant | round | pass s | real | user | sys | user/real | max RSS MB | load1 before | load1 after |
|---|---|---|---|---|---|---|---|---|---|
| jina fp32 (R) | r1 | 785.5 | 789.4 | 3092.7 | 9.9 | 3.92 | 1570 | 5.76 | 10.03 |
| jina fp32 (R) | r2 | 741.8 | 745.3 | 2946.6 | 6.9 | 3.95 | 1589 | 5.02 | 7.26 |
| jina fp32 (R) | r3 | 749.4 | 752.8 | 2970.9 | 7.3 | 3.95 | 1626 | 6.77 | 11.68 |
| jina int8 | r1 | 512.4 | 514.6 | 2027.1 | 5.1 | 3.94 | 744 | 5.14 | 6.13 |
| jina int8 | r2 | 527.9 | 530.4 | 2075.3 | 5.6 | 3.91 | 765 | 5.25 | 10.63 |
| jina int8 | r3 | 514.3 | 516.8 | 2037.8 | 4.8 | 3.94 | 786 | 6.34 | 7.21 |
| bge-small-en-v1.5 | r1 | 166.6 | 168.6 | 658.1 | 2.1 | 3.90 | 447 | 6.13 | 8.31 |
| bge-small-en-v1.5 | r2 | 173.9 | 176.4 | 686.5 | 2.5 | 3.89 | 450 | 6.19 | 7.77 |
| bge-small-en-v1.5 | r3 | 172.7 | 175.1 | 668.5 | 2.8 | 3.82 | 427 | 5.84 | 8.86 |
| gte-small | r1 | 160.0 | 162.1 | 636.1 | 1.7 | 3.92 | 424 | 6.30 | 7.28 |
| gte-small | r2 | 172.5 | 174.8 | 679.7 | 4.3 | 3.89 | 439 | 6.47 | 7.33 |
| gte-small | r3 | 168.0 | 170.0 | 655.6 | 2.6 | 3.86 | 434 | 6.74 | 11.37 |
| snowflake-arctic-embed-s | r1 | 158.0 | 159.8 | 628.3 | 1.8 | 3.93 | 445 | 5.26 | 7.04 |
| snowflake-arctic-embed-s | r2 | 172.7 | 174.8 | 678.1 | 2.3 | 3.88 | 434 | 5.62 | 9.37 |
| snowflake-arctic-embed-s | r3 | 160.6 | 162.6 | 630.0 | 2.1 | 3.88 | 416 | 6.26 | 6.32 |

### Medians and ratios to R

| variant | pass s | x R | max RSS MB | x R | size MB | x R | query ms | x R |
|---|---|---|---|---|---|---|---|---|
| jina fp32 (R) | 749.4 | 1.00 | 1589 | 1.00 | 644.1 | 1.00 | 14.92 | 1.00 |
| jina int8 | 514.3 | 0.69 | 765 | 0.48 | 164.5 | 0.26 | 8.03 | 0.54 |
| bge-small-en-v1.5 | 172.7 | 0.23 | 447 | 0.28 | 133.8 | 0.21 | 5.57 | 0.37 |
| gte-small | 168.0 | 0.22 | 434 | 0.27 | 133.8 | 0.21 | 4.48 | 0.30 |
| snowflake-arctic-embed-s | 160.6 | 0.21 | 434 | 0.27 | 133.8 | 0.21 | 6.53 | 0.44 |

### How the noise bears on the pass-time gate (<= 0.60x R)

- **Spread across rounds.** Max over min is 1.06 for R (741.8-785.5 s), 1.03
  for int8, 1.04 for bge, 1.08 for gte and 1.09 for snowflake. Taking every
  cross-round pairing, int8's ratio to R ranges from 0.65 to 0.71, and the
  small models' from 0.20 to 0.23.
- **int8 misses the pass-time win on every pairing:** 0.65 or higher, against
  0.60. It wins C-win on size (0.26x) and RSS (0.48x) instead, so the timing
  noise cannot flip C-win. C-no-worse (<= 1.10x) is met on all four measures
  with a wide margin.
- **The small models** sit at 0.20-0.23x. It would take roughly 2.6x of noise
  to reach 0.60x, far beyond the observed spread of 1.09.
- **No verdict depends on timing.** Every candidate passes both cost gates,
  and every failure is a quality gate.
- An earlier jina fp32 quality run took 1,645 s for the same pass while other
  work was running. That run is not used here, and it shows how far a loaded
  machine can move an absolute time. Only the interleaved ratios are
  interpreted.

## Verdicts under D9

| candidate | Δ r@10 [lower] | Δ MRR [lower] | Q3 worst language | Δ CW [upper] | Δ FA [upper] | cost | verdict |
|---|---|---|---|---|---|---|---|
| jina int8 | +0.5 [-1.0] pass | -0.003 [-0.012] pass | go -1.0 pass | -1.4 [+1.0] pass | +2.1 [+6.2] **FAIL** | pass (size 0.26x, RSS 0.48x) | **Fail: Q5** |
| gte-small | -5.3 [-9.2] **FAIL** | -0.055 [-0.086] **FAIL** | ts -9.0 pass | +11.1 [+16.7] **FAIL** | -10.3 [0.0] pass | pass (all ~0.2-0.3x) | **Fail: Q1, Q2, Q4** |
| bge-small-en-v1.5 | -7.5 [-11.8] **FAIL** | -0.068 [-0.100] **FAIL** | python -10.0 pass (at the limit) | +14.8 [+20.1] **FAIL** | -12.1 [-0.2] pass | pass | **Fail: Q1, Q2, Q4** |
| snowflake-arctic-embed-s | -8.2 [-12.5] **FAIL** | -0.058 [-0.092] **FAIL** | ts -16.0 **FAIL** | +24.6 [+30.3] **FAIL** | -10.8 [-2.3] pass | pass | **Fail: Q1, Q2, Q3, Q4** |

Deltas are in points, except MRR.

- **jina int8** matches R on retrieval (recall@10 +0.5, MRR -0.003). It is
  4x smaller, uses half the RSS, and its pass takes 0.69x R's time. It fails
  only Q5, by 1.2 points of bound, on 2 go queries. Under "inconclusive = keep
  jina" that is a fail. Q5 would fail per language too, so the provisional
  pooled-vs-per-language choice does not rescue it. int8's own follow-up is
  GM-422, which can take this row as its measurement.
- **The small 384-dimension models** are about 4.5x faster and 4x smaller,
  but they lose 5-8 points of recall@10 and 0.06 of MRR. They are also far
  more often confidently wrong: +11 to +25 points, and 50-76% of absent
  queries clear their floor, against R's 26%. Their Q5 wins come with much
  higher floors: gte's floors are 0.84-0.86 and it still has more confident
  wrong answers. The trade is between refusing too often and answering wrong
  too often, and they give up precision where the tool needs it.
- **Winner: none. Keep jina-v2-base-code fp32.** No finalist goes to D10.

## Where things live

- **Models**: `eval/embedding/work/models/<variant>/{model.onnx,tokenizer.json}`,
  540 MB in total, fetched by `eval/embedding/fetch_models.py` at the commits
  pinned in `variants.toml`. They are git-ignored (`/eval/embedding/work/` in
  `.gitignore`). R is read from `~/.g-mesh/models/jina-embeddings-v2-base-code`.
  sha256 of each `model.onnx`:
  - int8: `ed45870251c9f0cf…`
  - bge: `828e1496d7fabb79…`
  - gte: `0b01312b59bec0a2…`
  - snowflake: `579c1f1778a0993e…`
- **Runs**: `eval/embedding/work/runs/<variant>/<corpus>/`, git-ignored and
  kept for S15.

## Remaining options

Cost/quality levers other than swapping the model, checked against the
tracker before filing anything new:

- **Skip embedding noise** (generated files, trivial nodes): no existing
  task — filed as GM-437, depends on this eval.
- **Batching by similar length** (this eval used batch size one): no
  existing task — filed as GM-438, depends on this eval.
- **Execution providers** (CoreML/DirectML/CUDA instead of CPU): no
  existing task — filed as GM-439, depends on this eval.
- **Background priority / fewer intra-op threads**: no existing task —
  filed as GM-440, depends on this eval.
- **Lazy or prioritised embedding** (defer or rank nodes instead of a full
  eager pass): no existing task — filed as GM-441, depends on this eval.
- **CodeRankEmbed**: not tried here. It has no published ONNX, and the
  export was not needed for the verdict. It is recorded in GM-436 as an
  alternative base model for the adapter/fine-tuning research, with the
  same export and PyTorch-vs-ONNX parity control.
- **Int8 quantization and shorter input** are already covered: GM-422 and
  GM-423, filed before this eval and unblocked by it.
- Adapter/fine-tuning research is out of scope here and moves to GM-436;
  the similarity-floor false-alarm issue (jina int8's Q5 failure, above) is
  tracked as GM-434.
