# GM-736: a CodeRankEmbed int8 recipe that keeps quality (design)

Design slice GM-736/S1, release 4.36.0. No recipe runs, no eval v2 runs, no GPU.
Builds on GM-703 (`docs/design/GM-703-coderankembed.md`,
`docs/results/gm-703-coderankembed.md`) and, for QAT, GM-734
(`docs/design/GM-734-rr-qat.md`). Six CPU probes were run for this note
(section 0.2). They change the order the task proposed: the cheapest candidate
is not mixed precision but **per-channel smoothing before the unchanged shipped
int8 recipe**. On the probe it passes the 0.99 gate at 139 MB with no slowdown.

## 0. Facts this design builds on

| fact | value | source |
|---|---|---|
| CRE fp32 on eval v2 vs jina int8-structured | MRR 0.453 vs 0.380, r@10 0.673 vs 0.608; Q1-Q4 pass, **Q5 fails** (false alarm +4.7 pts, upper +9.2) | `docs/results/gm-703-coderankembed.md` |
| GM-730 J (`gm730-jina-full-int8-structured`) on eval v2 | MRR 0.501, r@10 0.733 | `docs/results/gm-730-gpu-training-routes.md` |
| CRE plain dynamic int8 (the shipped jina recipe) | 138,964,289 B; Q-D median **0.9625**, p1 0.9193; jina int8 0.9905 / 0.9490 | `g-mesh-data/gm703/s2/parity.json` |
| The shipped recipe | `quantize_dynamic(QInt8, per_channel=True, reduce_range=True)`, MatMul + Gather: 60 `MatMulInteger` (12 layers x Wqkv, out_proj, fc11, fc12, fc2), 2 quantized Gathers | `export_coderankembed.py:42`, `:161-171` |
| Why `reduce_range` (7-bit weights) is required | A Rust process can get ORT's AVX2 `U8S8` kernel, whose `vpmaddubsw` saturates on 8-bit signed weights (GM-700). The `QUInt8`-weights form (the L6 reranker's `quint8_avx2`) does not saturate either | `docs/results/gm-700-int8-parity.md` |
| Q-D today | cosine of CLS vectors, int8 vs fp32 ONNX, over `parity_sample.jsonl` (162 texts: 40 short, 40 medium, 40 long, 20 over 1024 tokens, 20 prefixed queries, 2 hand-made; sha256 `48908b01...`); stop if median < 0.98 or p1 < jina p1 - 0.02 | `parity.py:35-47`, `:246-257` |
| GPU QAT precedent | GM-734 RR QAT: one g5.xlarge spot run, 0.73 h, **$0.51**, C-P2 pass (quint8 dev MRR 0.4848 vs fp32 0.4866) | `g-mesh-data/gpu-runs/gm730-rr-qat-27c57b8dd91d-20261121T0020-a1/cost.json`, `out/qat` |
| GPU spend so far | GM-457 ~$1.3; GM-730 $12.27; GM-734 $0.51; kill switch $50 | GM-730 results, cost.json |
| Calibration text source, disjoint from eval v2 | GM-456 repos in `eval/training/corpora.toml` (18 `train`, 6 `dev`), none of them an eval v2 corpus (gin, py-requests, ripgrep, g-mesh, excalidraw, task-tracker-mcp); snapshots `g-mesh-data/gm456/work/<repo>.sqlite` (24 files) | `corpora.toml`, `ls` |

### 0.1 Where the drift comes from (probe reading)

CRE's per-channel activation maxima before the quantized MatMuls are strongly
skewed. Measured on 81 sample texts: the LayerNorm outputs that feed Wqkv and
fc11/fc12 have max / median |x| ratios of 10-28 (max up to 47), and the SwiGLU
output that feeds fc2 in layer 8 has a ratio of 45. Dynamic quantization uses
**one range per activation tensor**, so a few outlier channels take most of the
255 levels and the remaining channels keep only a few. Weight rounding matters
less: 7-bit weights alone, with fp32 activations, give a median of 0.9876.

### 0.2 S1 probes (laptop CPU, ORT 1.19.2, gm703 venv)

Every row: Q-D statistic (int8 vs fp32 CLS cosine) on all 162 parity-sample
texts. "odd" is the median on the odd-index rows only, which no calibration
step saw (the smoothing statistics came from the even-index rows).
Scripts and raw output: scratchpad `gm736/probe.py`, `gm736/sq.py`,
`gm736/p1/results.jsonl`.

| variant | what changes vs the shipped recipe | MB | median | p1 | odd median |
|---|---|---|---|---|---|
| base | none (reproduces GM-703 exactly) | 139.0 | 0.9625 | 0.9193 | |
| rr0 | 8-bit s8 weights (`reduce_range=False`, forbidden on AVX2) | 139.0 | 0.9715 | 0.9291 | |
| u8 | 8-bit `QUInt8` symmetric weights (L6 form) | 139.1 | 0.9716 | 0.9294 | 0.9718 |
| ex:embeddings | word + token-type Gather kept fp32 | 209.3 | 0.9638 | 0.9167 | |
| ex:out_proj | 12 out_proj fp32 | 160.1 | 0.9629 | 0.9183 | |
| ex:fc11,fc12 | 24 fc11/fc12 fp32 | 308.4 | 0.9688 | 0.9206 | |
| ex:fc2 | 12 fc2 fp32 | 223.8 | 0.9736 | 0.9594 | |
| ex:Wqkv | 12 Wqkv fp32 | 202.5 | 0.9832 | 0.9506 | |
| rr0+ex:Wqkv | 12 Wqkv fp32, 8-bit rest | 202.5 | 0.9839 | 0.9486 | |
| layer:0 / layer:11 | one whole layer fp32 | 167.2 | 0.9622 / 0.9697 | 0.9136 / 0.9263 | |
| wonly (7-bit) | weights 7-bit behind DequantizeLinear, fp32 activations | 209.0* | 0.9876 | 0.9786 | |
| wonly8 | weights 8-bit, fp32 activations | 209.0* | 0.9968 | 0.9942 | |
| static-minmax | static QDQ, MatMul only, MinMax on 81 texts, u8 weights | 209.4 | **0.7605** | 0.6746 | 0.7581 |
| static-pct | the same with Percentile 99.999 | | crashed: ORT 1.19's histogram collector refuses ragged lengths | | |
| **sq0.5-s7** | **smoothing alpha 0.5, then the shipped recipe unchanged** | **139.2** | **0.9919** | **0.9786** | **0.9921** |
| sq0.5-u8 | smoothing alpha 0.5, then `QUInt8` 8-bit weights | 139.4 | 0.9925 | 0.9791 | 0.9925 |

\* embeddings left fp32 in the probe; quantizing them as today would give about 139 MB.

Timing (informational only; load 5-21 during the probes): 162 texts, batch 1,
took 34 s for base int8, 37 s for sq0.5-s7, 83 s for fp32 and 113 s for wonly8.
ORT does not fold the DequantizeLinear weights, so weight-only is slower than
fp32. Probe wall times: 405 s, 542 s, 243 s, 72 s, 243 s, 174 s
(`/usr/bin/time -p`, user 1068 / 1521 / 740 / 160 / 553 / 471 s; `uptime`
logged at the start and end of each run in `gm736/run{1..6}.log`).

## 1. Candidate recipes, cheapest first

### Decision D1: the recipe order

**Today.** The task lists mixed precision, then static calibration, then QAT.

**Change.** Run the order the probes support:

1. **R1, smoothing + the shipped dynamic recipe (SmoothQuant-style, CPU, minutes).**
2. **R2, mixed precision on top of R1** (a few sensitive MatMuls kept fp32), only if R1 misses the gate.
3. **R3, QAT** (GPU, one run), only if R2 misses the gate within the size budget, and only with the owner's go.

Full static QDQ calibration is **not run**. The probe gave a median of 0.7605,
because one static range per tensor cannot cover these outlier channels.
Smoothing is itself the static calibration step that works: per-channel
statistics from calibration texts, with the activation quantization left dynamic.

**Example.** R1 on the probe: median 0.9919, p1 0.9786, 139.2 MB, about the
speed of today's int8. Mixed precision alone needs all 12 Wqkv in fp32 to
reach 0.983, which still fails the gate and costs 202 MB.

**Consequence.** The likely path is CPU only and costs no money. QAT stays as a
fallback that is designed but not expected to run.

| option | benefit | risk |
|---|---|---|
| **a. R1 -> R2 -> R3 (recommended)** | Cheapest measured winner first; the shipped recipe, Q-F fingerprint and speed are unchanged | Smoothing adds 48 `Mul` nodes (one per distinct MatMul input); ORT 1.20 in Rust must load them (must-confirm M3). The probe calibrated on the gate sample's even rows; the code slice recalibrates on disjoint texts, and the result may be lower |
| b. The task's order (mixed -> static -> QAT) | Follows the brief literally | The probes already rule out both CPU steps within the size budget (best mixed 0.9832 at 202 MB, static 0.7605); it would lead straight to GPU spend |
| c. Go straight to QAT | Keeps the exact shipped graph, no `Mul` nodes | Costs ~$1 and a GPU day for something a CPU recipe already reaches |

### 1.1 R1: smoothing, then the shipped recipe

For every distinct input tensor X of a constant-weight MatMul (48 in CRE: emb_ln,
norm1 and norm2 outputs, the attention-output reshape, the SwiGLU product), and
for each input channel j:

- `a_j` = max |X_j| over the calibration texts (fp32 ONNX run with X added as a graph output);
- `w_j` = max |W[j, :]| over **every** MatMul that reads X (fc11 and fc12 share one input);
- `s_j = a_j^alpha / w_j^(1 - alpha)`;
- graph: insert `X' = X * (1/s)` (one `Mul`, constant vector) and rewrite each consumer's weight rows `W[j, :] *= s_j`. In fp32 this is the identity up to rounding.

Then `quantize_dynamic` with the unchanged `QUANTIZE_ARGS`. Post-LN residuals
also read the LayerNorm outputs, so the `1/s` cannot be folded into LayerNorm
gamma/beta. The explicit `Mul` costs 768 or 3,072 multiplies per token per site.

- **Calibration data (new, seed 736):** 256 texts from the 18 `train` repos of
  `corpora.toml` (`g-mesh-data/gm456/work/<repo>.sqlite`, the same
  `text_to_embed(doc, sig)` as `parity.py:69`). They use the same length bins as the parity
  sample, scaled up (64 short, 64 medium, 64 long, 32 over), plus 32 GM-456
  `train` queries with the query prefix. Written to
  `eval/embedding/gm736/calib_sample.jsonl`, sha256 recorded.
- **Selection data:** a second disjoint draw of 162 texts from the `dev` repos,
  in the parity sample's bin shape (`select_sample.jsonl`). alpha is chosen on it.
- **alpha sweep:** {0.3, 0.4, 0.5, 0.6, 0.7}, s7 recipe. The highest selection-sample
  median wins (ties go to the smaller alpha). Five quantizations at ~60 s each, plus
  5 x 162 texts: about 10 min.
- **The gate (section 2)** then runs once, on the frozen `parity_sample.jsonl`, for the chosen alpha only.

### Decision D2: weights s7 (shipped) or u8 after smoothing

**Today.** The shipped embedder recipe is QInt8 with 7-bit weights (`reduce_range`).

**Change.** Keep s7. The u8 form (8-bit `QUInt8` weights, the L6 reranker's
AVX2-safe form) gains only 0.0006 median on the probe (0.9925 vs 0.9919).

**Example.** sq0.5-s7 0.9919 / sq0.5-u8 0.9925.

**Consequence.** Q-F stays as written. The int8 file is the same kind as jina's,
with the same kernels and the same AVX2 behaviour.

| option | benefit | risk |
|---|---|---|
| **a. s7, the shipped recipe (recommended)** | No new kernel path; Q-F unchanged; GM-700's analysis holds | If disjoint calibration lands at 0.985-0.99, it lacks the small u8 margin |
| b. u8 weights | +0.0006 on the probe; still no AVX2 saturation | Q-F must change (weights in [0, 255], zp 127); the embedder path has never run u8 weights in Rust |
| c. Sweep both, pick by the selection sample | Uses the margin if it matters | Doubles the sweep; two fingerprints to test |

### 1.2 R2: mixed precision on top of R1 (only if R1 misses)

- **Sensitivity:** for each of the 60 MatMuls, quantize everything except that
  one (the R1-smoothed graph, `nodes_to_exclude=[node]`) and record the
  selection-sample median gain over R1. That is 60 x ~60 s, about 1 hour, run as one
  background script.
- **Greedy:** add nodes to the exclusion list in order of gain, re-measuring after
  each addition, until the selection median is >= 0.99 + 0.002 (margin) **or** the file
  size passes the budget (section 3). Per-node size costs: Wqkv +5.3 MB, out_proj
  +1.8 MB, fc11/fc12/fc2 +7.1 MB each, word embeddings +70 MB (never: the probe showed
  no gain).
- **Q-F:** `fp32_matmul_on_big_weights == 0` becomes `== len(excluded)`, with the
  excluded node names recorded in `EXPORT.json`.

### 1.3 R3: QAT (only after R2 fails, owner go required)

The GM-734 scheme carried over to an embedder:

- fake quantization that emulates the s7 recipe exactly, with STE: per-channel 7-bit
  weights, per-tensor dynamic uint8 activation ranges with masked padding, and the
  Gather tables per tensor, on the 60 MatMuls and 2 tables;
- **self-distillation**: the student's fake-quant CLS vectors are pulled toward
  the frozen fp32 CRE's CLS vectors (loss 1 - cosine, plus 0.1 x MSE). This needs no
  labels and keeps CRE's ranking behaviour, which is what Q-D measures;
- data: GM-456 `train` repo texts and queries, the same as the calibration source,
  max_len 512, about 50k texts, 1 epoch, lr 1e-5, fp32 (no autocast inside fake quant), seed 736;
- export through `export_coderankembed.export_onnx` and the unchanged R1 or base
  recipe, then the same gate.

**GPU cost estimate.** CRE has 137M parameters, like jina-base (J-final took 81 min
for training, two exports and int8 checks). Distillation is one forward of the student
with backward, against teacher vectors precomputed once. Fake quant in fp32 is ~3x a
bf16 step (GM-734). Estimate: 20-30 min of training plus 15 min of boot, data, export
and Q-D, so **~45 min on g5.xlarge at ~$0.7-0.92/h, about $0.6-0.7**.
Cap: `max_minutes` 90, 5 attempts, job scope. Worst case (90 + 25) min x $0.93/h =
**$1.78**. A new route `cre` in `budget.py` gets a **$3.00 cap**. Total GPU spend
would stay under $17 of the $50 kill switch.

### Decision D3: what to do with QAT

**Today.** QAT is listed as a candidate, with no money set aside.

**Change.** Design it, but launch it only if R1 and R2 both miss, and only after the owner approves the spend.

**Example.** If R1 lands at 0.988 and R2 cannot close the gap within 162 MB, the
measure slice stops and asks; it does not launch.

**Consequence.** No GPU code is written in this task unless the stop is reached.
GM-737 (fine-tune CRE with the GM-730 recipe) must quantize its own output anyway,
so a QAT stage there may be cheaper than a separate run here.

| option | benefit | risk |
|---|---|---|
| **a. Fallback only, owner go, cap $3 (recommended)** | No spend or code unless the CPU path fails | One extra owner round trip if it is needed |
| b. Pre-build the QAT code now | Ready to launch | Code that will most likely never run (GM-734's QAT code was ~1 slice of work) |
| c. Fold QAT into GM-737 | One GPU job does both fine-tuning and quantization | Delays GM-736's answer; mixes two effects in one result |

## 2. The Q-D gate and the stop rule

### Decision D4: the gate

**Today.** `parity.py:46-47`: median >= 0.98, and p1 >= jina p1 - 0.02, over the 162-text parity sample.

**Change.** Median **>= 0.99** (`QD_MIN_MEDIAN`), with the p1 rule unchanged
(>= 0.9490 - 0.02 = 0.9290), on the **frozen** `parity_sample.jsonl`
(sha256 `48908b01...`, the same texts GM-703 measured). jina int8 itself scores
0.9905 / 0.9490 there. alpha and the R2 exclusions are chosen on
`select_sample.jsonl`, never on the gate sample. The gate is evaluated once per
candidate that reaches it.

**Example.** sq0.5-s7 on the probe: median 0.9919 >= 0.99 and p1 0.9786 >= 0.9290: pass.
ex:Wqkv: 0.9832: fail.

**Consequence.** "Close to fp32" means at least as close as the jina int8 we ship
today is to its own fp32, measured on texts the recipe never saw.

| option | benefit | risk |
|---|---|---|
| **a. median >= 0.99 and the p1 rule, frozen sample, selection on a disjoint sample (recommended)** | Comparable with GM-703's numbers; no selection on the test | 162 texts: a candidate a few thousandths above 0.99 may be within sampling noise (not measured); the 0.002 R2 margin is for that |
| b. median only | Simpler | A recipe with a bad tail (e.g. long texts) could pass |
| c. A new, larger gate sample (e.g. 500) | Tighter estimate | Not comparable with GM-703's 0.9625; one more sample to freeze |

### 2.1 Stop rules

| # | condition | action |
|---|---|---|
| G1 | R1's best alpha passes the gate | go to the eval (section 4); R2/R3 are not run |
| G2 | R1 misses | run R2 (sensitivity + greedy within the size budget) |
| G3 | R2 misses within 162 MB (or 195 MB if D5 b) | **stop; owner decides** QAT (D3) or "no recipe" |
| G4 | QAT runs and its export misses the gate | **report "no recipe"** with each recipe's Q-D numbers; no second GPU run without the owner |
| G5 | the Rust harness cannot load the smoothed graph (ORT 1.20) | stop with ORT's error; a loader change is out of scope |
| G6 | eval validity (D7 controls) fails | inconclusive; report, no verdict |

"No recipe" is an acceptable outcome. It reports the best median per recipe, its
size, and the measured reason (activation outliers, per 0.1).

## 3. Size and latency budget

### Decision D5: size budget

**Today.** jina int8 is 161.9 MB (cost-pass column 164.5 MB). CRE plain int8 is 139.0 MB.

**Change.** Budget **<= 162 MB** (no larger than the shipped jina int8). R1 adds
about 0.3 MB (48 smoothing vectors). R2 may spend the remaining ~23 MB: for example
4 Wqkv (+21 MB) or 3 fc (+21 MB).

**Consequence.** A passing CRE int8 can drop in without growing the download.

| option | benefit | risk |
|---|---|---|
| **a. <= 162 MB (recommended)** | No download growth; a clean "drop-in" claim | Leaves R2 little room |
| b. <= 195 MB (1.2x jina int8) | More room for R2 | Outgrows the shipped model; needs its own adoption argument |

**Latency** (cost pass, the GM-703 gate C-quality, informational as in GM-703):
pass time <= 1.5x jina int8, max RSS <= 1.2x, query ms reported. Expected:
about 1.0x, since R1's probe run time was 37 s vs 34 s for base int8, and both models
are BERT-base size. Weight-only int8 (wonly8, which passes Q-D) is excluded
by this budget: it is slower than fp32.

## 4. The eval v2 comparison (pre-registered)

One new arm, `coderankembed-int8sq-structured` (CLS, 768, 1024, the CRE query prefix,
structured text), with model dir `g-mesh-data/gm736/models/coderankembed-int8-sq<alpha>`.
It is run by the GM-703 pipeline (`measure.sh --from 3`, `ARM=` the new arm),
unchanged apart from the arm row:

| step | what | reused from |
|---|---|---|
| 3-4 | release build, 6 corpus runs | `gm703/measure.sh` |
| 5 | P3 Rust vs Python queries: **reported**, not gated (Rust ORT 1.20 vs venv 1.19.2; int8) | `parity.py --rust-queries` |
| 6 | **Primary verdict:** `report --reference jina-v2-base-code-int8-structured` with `random`, `shuffled`: D9 gates Q1-Q5, per-model refit floors | same |
| 6b (new) | **vs J:** `report --reference gm730-jina-full-int8-structured` on the same runs, D9 gates **informational** (J is a fine-tuned model; this task only quantizes) | J's existing v2 runs |
| 6c (new) | **int8 cost of the recipe:** `report --reference coderankembed-fp32-structured`: r@10, MRR, Q5 deltas, int8 minus fp32 | GM-703 fp32 runs |
| 8 | cost pass, 3 interleaved rounds on g-mesh, vs jina int8 (owner GM-703: "Всегда замерять") | `measure_costs.sh` |
| 9 | GM-434 table set, per language, confirm and GM-398 subsets | `report_gm436.py` |

Reported, in this order, in `docs/results/gm-736-cre-int8.md`:

1. Q-D for every recipe tried (median, p1, odd/selection median, size).
2. D9 verdict vs jina int8 (the line from `s6-report.json`, verbatim).
3. Pooled table, CRE int8 / CRE fp32 / jina int8 / J: r@1, r@10 [lo, hi], r@100, MRR [lo, hi], CW comb/pos/abs, false alarm.
4. **Q5** per language, with own false-alarm rates, beside GM-703's fp32 numbers.
5. Costs table (pass s, RSS, size, query ms, x jina).
6. The J comparison (6b), labelled informational.

Pre-registered expectations, written before any run so they can be checked:

- int8 loss vs CRE fp32 within -1.0 pt r@10 and -0.010 MRR. A larger loss is reported as "Q-D did not predict quality".
- Q5 is expected to still fail, because it failed for fp32 and int8 does not change the floors' cause. A pass is reported as a surprise, not as a fix.
- vs J: J is expected to stay ahead (0.733 / 0.501 vs fp32's 0.673 / 0.453). GM-736 makes no ship claim; adoption needs Q5 fixed (GM-737 or a floor task).

Expected wall time: ~1 h of corpus runs plus a 1-4.5 h cost pass (GM-703's took
4.5 h under load 500; timing starts at load < 7).

## 5. Edit map for the code slice

New directory `eval/embedding/gm736/`. It reuses the gm703 modules by import, as `parity.py` imports `export_coderankembed`.

| file | lines | change |
|---|---|---|
| `eval/embedding/gm736/smooth.py` (new) | | `collect_absmax(fp32_path, ids_rows) -> {tensor: np.ndarray}` (graph outputs added for every const-B MatMul input); `smoothing_factors(absmax, weights_by_input, alpha)`; `apply_smoothing(fp32_path, out_path, factors)` (one `Mul` per distinct input, rows of every consumer scaled, topological insertion after the producer); `smoothed_int8(fp32_path, out_dir, alpha, calib_ids, exclude=())` = smoothing + `E.quantize_int8` (+ `nodes_to_exclude` for R2) |
| `eval/embedding/gm736/samples.py` (new) | | `build_calib_sample()` (seed 736, the `train` repos of `eval/training/corpora.toml`, refusing any eval v2 corpus name) and `build_select_sample()` (`dev` repos); writes `calib_sample.jsonl`, `select_sample.jsonl`; reuses `parity.text_to_embed`, `parity.BINS` and the tokenizer setup |
| `eval/embedding/gm736/recipe.py` (new) | | CLI: `--sweep 0.3,0.4,0.5,0.6,0.7` (selection-sample Q-D per alpha, `sweep.json`); `--export --alpha A [--exclude-file F]` writes `<models>/coderankembed-int8-sq<A>/{model.onnx,tokenizer.json,EXPORT.json}` (EXPORT.json: recipe `smooth+dynamic`, alpha, the calib sample sha256, the smoothing vectors' sha256, `QUANTIZE_ARGS`, excluded nodes, fp32 sha256); `--sensitivity` (R2 per-node gains); `--gate` (Q-D on the frozen parity sample, exit 3 on a miss) |
| `eval/embedding/gm736/measure.sh` (new) | | one background script, fail-fast, caffeinate: sweep -> export -> gate -> (G2: sensitivity + greedy) -> `ARM=coderankembed-int8sq-structured ARM_MODEL=... gm703/measure.sh --from 3` -> the steps 6b and 6c reports; `summary.txt` |
| `eval/embedding/gm703/parity.py` | `:46` | `QD_MIN_MEDIAN` stays 0.98 for GM-703; `check_qd` (`:246-257`) gains `min_median=QD_MIN_MEDIAN`, which GM-736 passes as 0.99 |
| `eval/embedding/gm703/export_coderankembed.py` | `:173-230` | `int8_fingerprint(path, hidden, allowed_fp32=0)`: the `fp32_matmul == 0` rule becomes `== allowed_fp32` (R2 only); `Mul` nodes need no change |
| `eval/embedding/gm703/measure.sh` | `:65-74`, `:143` | the `ARM` case accepts `coderankembed-int8sq-structured` with `ARM_MODEL` from the environment (it must contain `EXPORT.json`; `check_export` unchanged); step 5 `min_cos=0` for every int8 arm |
| `eval/embedding/variants.toml` | after `:556` | the `coderankembed-int8sq-structured` row (`role = "quality"`, `onnx_file = "gm736-export/model_int8_sq.onnx"`) |
| J's row | | `gm730-jina-full-int8-structured` is **not** in the committed `variants.toml` (`register_arms.py` appends it at measure time); `gm736/measure.sh` re-registers it before step 6b (must-confirm M2) |
| R3 only (not in S2 unless G3 is reached and the owner says go) | | `eval/training/gm730/qat.py` gains an embedder wrapper (NomicBERT `Wqkv/out_proj/fc11/fc12/fc2`); `train_encoder.py` gains `--stage qat-distill`; `make_jobs.py` a `cre-qat` job; `budget.py:31` `ROUTE_CAP["cre"] = 3.0` |

### 5.1 Behaviours the tests must pin (one control each; revert the code, never the test)

The fixture is GM-703's 2-layer random-init NomicBERT (`load_model(snapshot, n_layer=2)`, `TinyExport` in `test_gm703_export.py:43`).

| # | behaviour | test | control (revert) |
|---|---|---|---|
| T1 | smoothing is the identity in fp32: the smoothed graph's output equals the original's (max abs <= 1e-4) | tiny fixture, random alpha, lengths 7 and 300 | skip the weight-row scaling: outputs differ |
| T2 | one `s` per distinct input, from the max over **all** consumers (fc11 and fc12 share one) | count of `Mul` nodes == distinct inputs; `s` equals the formula on hand-made absmax/weights | use only the first consumer's `w_j`: the formula test fails |
| T3 | smoothing reduces the activation range ratio the quantizer sees | absmax/median of `X'` < that of `X` on the fixture with a planted outlier channel | `alpha = 0` path ignored (s = 1): the test fails |
| T4 | the smoothed int8 file keeps the shipped fingerprint (Q-F: 7-bit, per channel, Gather, no fp32 big MatMul) | Q-F on the quantized smoothed fixture | `reduce_range=False`: weights reach +/-127 |
| T5 | calibration and selection samples never read an eval v2 corpus, and the calibration sample reads only `train` repos | `build_calib_sample` on a temp corpora file listing `gin` refuses; split filter test | drop the refusal: the test fails |
| T6 | the gate uses 0.99 and the frozen sample; a 0.9899 median exits 3 | `check_qd(..., min_median=0.99)` on stubbed vectors; `--gate` refuses a sample whose sha256 is not `48908b01...` | default back to 0.98: the test fails |
| T7 | `EXPORT.json` records alpha, the calibration sha and the smoothing sha, and a re-export with the same inputs gives the same `model.onnx` sha256 | two exports of the fixture | write the factors from an unseeded sample: the shas differ |
| T8 | `gm703/measure.sh` accepts the new arm with `ARM_MODEL` and runs `check_export` on it | the `Arms` sandbox (`test_gm703_measure.py:108`), DRY=1 | drop the case branch: rc 2 |

8 controls, all in Python test files. Group reverts by file into one run per file.

## 6. Must confirm (code or measure slice, before the step named)

| # | what | before |
|---|---|---|
| M1 | `g-mesh-data/gm456/work/<repo>.sqlite` holds `nodes(docComment, signature)` for the 18 `train` and 6 `dev` repos, with enough texts per length bin (the `over` bin above all) | samples.py |
| M2 | J's v2 runs (`g-mesh-eval-data/v2/work/runs/gm730-jina-full-int8-structured/<6 corpora>`) were made by the current harness version, and `register_arms.py` can re-append its row without changing its fingerprint | step 6b |
| M3 | ORT 1.20 (Rust) loads the smoothed int8 graph (48 extra `Mul`), and P3 int8 Rust vs Python is in the range jina int8 shows (report both) | step 4 |
| M4 | the R1 probe result holds with disjoint calibration (the probe calibrated on the gate sample's even rows; the odd rows agreed, 0.9921) | gate |
| M5 | the 1-min load is < 7 before the cost pass (D11); `uptime` and `/usr/bin/time -lp` recorded per step | step 8 |
| M6 | GPU spend ledger (Cost Explorer + `budget.py spent`) before any R3 launch; the history doc 7Qj12xvHcKyvYzhtPWUMQi gets the entry | R3 only |
| M7 | GM-703's fp32 runs (step 6c reference) are still in `work/runs/coderankembed-fp32-structured` and match the eval dir's current query files | step 6c |

## 7. Decisions for the owner

| # | question | recommended |
|---|---|---|
| D1 | recipe order | **R1 smoothing -> R2 mixed on top -> R3 QAT; full static QDQ not run** |
| D2 | weights after smoothing | **s7, the shipped recipe** |
| D3 | QAT | **fallback only, owner go, `cre` cap $3** |
| D4 | gate | **median >= 0.99 + p1 rule, frozen parity sample, selection on a disjoint sample** |
| D5 | size budget | **<= 162 MB** |

## 8. Code facts relied on (and how they were found)

All Python and shell; read with `grep -n` and `sed -n` on one known file each (no
cross-file symbol questions, so g-mesh was not needed): `export_coderankembed.py`
(`QUANTIZE_ARGS:42`, `quantize_int8:161`, `int8_fingerprint:173`, `main:254`),
`parity.py` (`BINS:35`, `QD_*:46-47`, `check_qd:246`, `run_checks:259`, `main:331`),
`measure.sh` (`ARM:65-74`, steps `:101-170`), `variants.toml:519-556`,
`budget.py:31`, `make_jobs.py:50-82`, `corpora.toml` splits.
The graph node names (`/inner/encoder/layers.N/{attn/Wqkv,attn/out_proj,mlp/fc11,mlp/fc12,mlp/fc2}/MatMul`)
come from loading the fp32 export.
