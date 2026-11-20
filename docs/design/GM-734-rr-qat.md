# GM-734: quantization-aware fine-tune of the RR cross-encoder (design)

Design slice GM-734/S1. No code, no GPU spend. Builds on GM-730 route RR
(`docs/design/GM-730-gpu-training-routes.md`, sections 1.1, 2, 4 and 5) and on
the GM-730/S53 re-quantisation arms. The C-P2 formula and the export /
quantisation recipe stay unchanged (owner: "Отдельная задача: дообучение с
учётом квантования (Recommended)").

## 0. Facts this design builds on

| fact | value | source |
|---|---|---|
| RR final (lr 5e-5, tau_t 1, alpha 0.5, 2 epochs, bf16, 1,288 steps) | full dev MRR 0.5188, r@10 0.861; base L6 0.4036 / 0.731 | `gm730-rr-final-3e75efa5b2cc-20261119T2040/out/final/train.json` |
| C-P2 on the instance (first 300 dev windows, chunk 1) | fp32 0.48747, quint8 0.47536, base fp32 0.38176, base quint8 0.38950, base gap -0.00775, floor 0.48522: **FAIL** | same run, `export.json` |
| What the gate really asks | quint8 >= own fp32 - (base gap + 0.01) = own fp32 - **0.0023** (instance base gap); the trained model loses 0.0121 | arithmetic |
| Instance vs laptop noise of the same quint8 file | 0.47536 (instance) vs 0.47683 (laptop), same sha256: **0.0015** | S53 `summary.md`, A0 |
| S53 re-quantisation arms (laptop) | A0 0.4768, A1 (jina recipe) 0.4783, A2 0.4749, A3b (classifier fp32) 0.4767: none passes | `/private/tmp/claude-502/gm730s53/summary.md` |
| RR final cost | $0.456 (0.49 h g5.2xlarge spot at $0.92/h); RR route spent **$3.670** of cap $4.50 | `cost.json`, `budget.spent` |
| C-S (shuffled teacher) of RR | **never ran**: the final step exited 1 on C-P2 and the runner stopped the job before step `shuffled` | `status.json` (`steps: final exit 1`) |
| The final torch checkpoint | **not on S3**: every upload after 21:09 failed on `s3:GetObjectTagging` (the launcher task already open); S3 holds a mid-epoch-1 checkpoint only | `logs/attempt-1.log` |
| The fp32 ONNX holds the exact best parameters | rebuilding the HF model's 105 parameters from `onnx-fp32/model.onnx` (69 by initializer name `m.<param>`, 36 MatMul weights transposed) gives `params_sha256` = **337e0d1e...c3a2** = `train.json` `bestParamsSha256` | S1 probe (scratchpad `recon.py`) |
| torch / transformers / onnxruntime | 2.2.2 / 4.48.3 / 1.19.2 on laptop and instance | `requirements-linux-cu121.txt`, venv-probe |

### 0.1 What the l6 recipe quantises (read from the RR quint8 graph)

`export.quantize("l6")` = `quantize_dynamic(QUInt8, per_channel=True,
reduce_range=False, WeightSymmetric, MatMulConstBOnly=False)` + the closed-form
rewrite. In the RR quint8 graph that is:

- **38 MatMulInteger with a constant B**: the 36 encoder Linears (Q, K, V,
  attention output, intermediate, output x 6 layers), the pooler dense and the
  classifier (ORT splits each Gemm into `<name>_MatMul` + `_Add`). Weights:
  closed form, per output channel, s = maxabs / 127, q = rint(w / s) + 127,
  zp 127 (the classifier has one output channel, so one scale).
- **12 MatMulInteger with two activations**: per layer `attention/self/MatMul`
  (scaled Q x scaled K^T, the SDPA decomposition of the export) and
  `MatMul_1` (softmax probabilities x V).
- **3 Gather on quantised tables**: word, position and token-type embeddings,
  closed form **per tensor** (one scale per table), then DequantizeLinear.
- **Every MatMulInteger input activation** goes through DynamicQuantizeLinear:
  uint8, per tensor, range [min(0, x_min), max(0, x_max)], s = range / 255,
  zp = saturate(round(-min / s)), q = saturate(round(x / s) + zp), ties to
  even. At chunk 1 (one pair per ORT run, as `quint8_check` scores it) the
  tensor is one pair's real tokens, no padding.
- Not quantised: LayerNorm, GELU (Erf), Softmax, biases, Tanh.

### 0.2 Where the loss comes from (S1 diagnostic, laptop, 300 windows)

Diagnostic arms on the RR fp32 export (`scratchpad/abl/abl.py`; the recipe
itself is not changed by this task). Trained fp32 0.48747, full recipe A0 0.47683.

| arm | what is quantised | quint8 MRR | loss vs fp32 |
|---|---|---|---|
| W | weights only (38 Linears + 3 tables dequantised into the fp32 graph, fp32 activations) | 0.48621 | -0.0013 |
| EC | the 38 Linears with their input activations (no act x act, no tables) | 0.48524 | -0.0022 |
| C | EC + the 3 tables (`MatMulConstBOnly`) | 0.48055 | -0.0069 |
| E | all 50 MatMuls (no tables) | 0.47810 | -0.0094 |
| A0 | everything (the recipe) | 0.47683 | -0.0106 |

Per-weight relative error (‖deq - w‖ / ‖w‖): 0.4-4 % for Linears and the word
table, **14 % for the position table** (per tensor, outlier rows).

**Reading:** weight rounding alone costs ~0.001; the loss is in the dynamic
activation quantisation, mostly the 12 act x act MatMuls (~0.004-0.007, the
softmax probabilities get 255 levels of [0, max p]) and the embedding tables
(~0.001-0.005). A weight-only scheme cannot close a 0.012 drop to 0.0023.
Machine was loaded during the run (load 100 -> 42, `real 2064 s`, `user
3057 s`); the MRR values are deterministic, the timings are not used.

## 1. Chosen scheme

**Exact emulation of the l6 recipe as fake quantisation with a straight-through
estimator (STE), applied to the same 50 MatMuls and 3 tables the export
quantises, with per-pair (masked) dynamic activation ranges; a short
distillation fine-tune from the RR final parameters; the plain model goes
through the unchanged export.**

### 1.1 Quantisers (new `eval/training/gm730/qat.py`)

- `fq_weight(w, axis)`: the closed form of `export.closed_form` in torch
  (s = maxabs / 127 per output channel; per tensor for the 3 tables and the
  1-channel classifier; q = clamp(round(w / s) + 127, 0, 255); deq = (q - 127) s),
  forward value deq, backward identity (STE: `w + (deq - w).detach()`).
  Same function as the export, so the weights the job trains are the weights
  the file gets, bit for bit (pinned, section 6).
- `fq_act(x, mask)`: DynamicQuantizeLinear in fp32, one range per pair:
  padded positions are set to 0 before `amin`/`amax` over all non-batch
  dims. Because the range always contains 0, this gives exactly the chunk-1
  range of the pair. Scale and zero point detached; STE on the rounding.
- `FakeQuantLinear(linear)`: `F.linear(fq_act(x, mask), fq_weight(W), b)`;
  wraps the 38 Linears by module path (encoder `query/key/value/
  attention.output.dense/intermediate.dense/output.dense`, `bert.pooler.dense`,
  `classifier`).
- Patched self-attention (eager form; the model is loaded with
  `attn_implementation="eager"` for training): `fq_act` on Q, K, the softmax
  probabilities (padded query rows zeroed) and V before the two matmuls.
  Scaling Q and K by d^-1/4 each (the export's SDPA form) or Q.K^T by d^-1/2
  (eager) is the same quantisation: the per-tensor quantiser is scale
  equivariant (equal up to fp32 rounding; covered by the fidelity check).
- `FakeQuantEmbedding`: `fq_weight(table, None)` on the 3 tables.
- `qat.wrap(model)` / `qat.unwrap(model)` / `qat.enabled(model, on)`:
  install, remove, and bypass (bypass = the plain fp32 model, for the fp32
  dev rows). The attention mask reaches the wrappers through a
  `qat.mask_context(attention_mask)` set around each forward.

Everything inside the fake-quant path runs **fp32, without autocast**: in
bf16, x / s would round before `round()` and the 255-level grid would blur
(bf16 has 8 significant bits). TF32 stays off (`setup_torch`).

### 1.2 Export stays the GM-730 path

`export_final` is unchanged in what it calls: after training, a **fresh**
`load_model(device)` (default SDPA attention, as RR final) gets the selected
parameters via `load_trainable` (same names; wrappers keep the original
parameters) and goes through `X.export_dir` -> `X.quantize("l6")` ->
`parity_ce` (C-P) -> `max_logit_diff` -> `quint8_check` (C-P2, unchanged
formula, `CP2_WINDOWS` 300, `CP2_MARGIN` 0.01, base re-exported on the
instance). The fresh model guarantees the same ONNX graph as RR final; a
zero-step QAT export must reproduce RR final's `onnx-fp32/model.onnx` sha256.
`export.json` gets one informational record, `qatFidelity`: emulated (torch)
vs ORT quint8 logits on the `QUANT_N` pairs at chunk 1 (max and median |diff|).

### 1.3 Alternatives considered

| option | benefit | risk / why not |
|---|---|---|
| **a. Exact recipe emulation, STE, 50 MatMuls + 3 tables, masked per-pair ranges (chosen)** | Trains against the error the gate measures; diagnostic shows activations dominate | Custom code (attention patch, mask plumbing); fp32 training is ~3x slower than bf16 |
| b. Weight-only fake quant (STE) | Small code, keeps bf16 | Weights cost ~0.001 of the 0.012 drop (arm W): cannot pass |
| c. `torch.ao` QAT (`prepare_qat`, fake-quant observers) | Library code | Observers are static moving-average ranges, not ORT's per-call dynamic ones; qint8 symmetric scale is maxabs/127.5, not the closed form's /127; no act x act MatMuls, no quantised Gather; export through torch.ao would change the export path |
| d. Weight-range / kurtosis regulariser | Cheap | Indirect; acts on weights, the loss is in activations |
| e. Keep the classifier fp32 at export (A3b) | No training | Measured 0.4767, no help; changes the shipped recipe |
| f. Another recipe (jina's QInt8 / reduce_range: A1, A2) | No training | Measured, no pass; `rerank.rs` pins the `model_quint8_avx2` form |
| g. Integer-domain bf16 matmuls (exact q - zp in bf16, scale in fp32) | bf16 speed | bf16 GEMM output rounds the accumulated sum to 8 bits; fallback only if the fp32 probe is over the ETA limit (section 3) |
| h. Re-run RR from the base with QAT for 2 epochs | One clean run | ~3x the RR final's 19 min of training in fp32: over the money left |

## 2. Training stage

- **Start:** RR final parameters, rebuilt from `onnx-fp32/model.onnx` by
  `trainlib.init_from_onnx` (initializer `m.<param>` by name; Linear weights
  as the transposed B input of the MatMul node `/<module path>/MatMul`, by
  graph structure, not by value), refused unless `params_sha256` equals
  `--init-params-sha256` (337e0d1e...c3a2). Shipped to the instance as a
  `[[data]]` set (91 MB), since the final checkpoint.pt is not on S3.
- **Loss:** unchanged `train_step`: KL(softmax(teacher / tau_t) ||
  softmax(student)) + alpha CE(gold), tau_t 1, alpha 0.5 (the RR final
  config). Teacher scores: `$GM730_TEACHER` train split, as RR (dev has no
  teacher; none needed). Student logits come from the fake-quant forward.
- **Data:** the RR train windows, batch 32 queries, the epoch-1 bucketed order
  of seed 730 (`bucketed_batches(lengths, 32, 730, 1)`), first `--steps` batches.
- **Steps / lr:** 400 steps (0.62 epoch, 31 % of RR's 1,288), AdamW
  lr **1e-5** (1/5 of RR's peak; QAT starts from a converged model), 5 %
  warmup, linear decay to 0, weight decay 0.01, max_len 256, fp32 (no
  `--bf16`), `--micro-queries 4 --micro-batch 128` (fp32 activations are 2x
  bf16's; accumulation gives the same gradient, test `Accumulation`).
- **Dev rows every `--eval-every 100` steps and at step 0:** emulated quint8
  dev MRR / r@10 over the **full dev** (3,198 windows, fake quant on, masked
  ranges, fp32), plus fp32 (bypass) and emulated quint8 on the first 300
  windows. Also once at start: base L6 fp32 and emulated quint8 on full dev
  (acceptance rows).
- **Selection (pre-registered):** predicted C-P2 margin on the first 300
  windows m = q300 - (f300 - 0.0023) - safety, safety 0.005 (`--cp2-safety`;
  owner 2026-11-20: "Строже, как сделал агент (Recommended)" for the sign,
  "Принять шум, запас 0.005 (Recommended)" for the value, which covers the
  ~0.0026 SD level-flip noise of the 300-window MRR, section 3.1.2). Among eval points with
  m >= 0, the highest full-dev emulated quint8 MRR (ties to the earlier);
  if none, the highest full-dev emulated quint8 MRR (the gate will most
  likely fail; stop rule S-G). Step 0 (= RR final) is a candidate.
  The 300-window subset only filters; ranking on the gate subset itself would
  select on the test.
- **Checkpoint contract:** GM-457 (`Checkpointer`, every `--ckpt-minutes`,
  SIGTERM, `--stop-after-steps`), state = step, history, best params, optimiser,
  scheduler, RNG.

## 3. Runs, cost and stop rules

### 3.1 CPU smoke (laptop, before any GPU spend; free)

1. `init_from_onnx` on the RR fp32 export: sha 337e0d1e...c3a2.
2. Fidelity (gate, owner 2026-11-20: "Принять шум, запас 0.005
   (Recommended)"): the ORT A0 quint8 MRR on the first 300 dev windows lies
   within 2 SD of the emulation's realisations (fake quant on; batch 64, 256,
   chunk 1 and fp32-ulp parameter jitter, each a different set of level flips).
   Measured (GM-734/S2, laptop): emulation 0.48180 (batch 256 and 64),
   0.48528 (chunk 1), 0.48257 / 0.48007 / 0.47835 (jitter seeds 1-3): mean
   ~0.4816, SD ~0.0026; ORT 0.47683 (1.8 SD below the mean): **pass**.
   A value within fp32 rounding of a quantisation boundary changes one level,
   so no non-bit-exact implementation matches ORT pair for pair (ORT laptop vs
   instance differ by 0.0015 on the same file). Per layer, fed ORT's own
   layer inputs, every tensor equals ORT's to ~1e-6 apart from such flips;
   pooler and classifier to 1e-7. Informational, not a gate: on 200 pairs at
   chunk 1, emulated vs ORT logits max 0.137, median 0.0142, p90 0.0474 (the
   emulation's own batch-16 vs chunk-1 flip noise: max 0.096, median 0.0131,
   p90 0.0468; ORT fp32 vs quint8: max 0.18, median 0.031). Masked ranges:
   batch-64 vs chunk-1 range ends of the layer-0 quantisers agree to 4e-7
   (later ones differ only after a flip). Bypass = plain eager model: 0.
3. Zero-step export: `--steps 0` on the RR init reproduces RR final's
   `onnx-fp32/model.onnx` sha256 and its laptop C-P2 numbers (quint8 0.47683).
4. Tiny run: `--max-train-queries 64 --max-dev-queries 40 --steps 4
   --eval-every 2`, kill at step 2 and resume = uninterrupted (params sha),
   loss finite, all output files.
5. Optional pilot (decision D3): 40 steps on the laptop at lr 1e-5, emulated
   quint8 on the 300 windows before/after: a direction signal, not a gate.

### 3.2 GPU job

`make_jobs.py qat --init-run <RR final run dir>` writes
`eval/gpu/jobs/gm730-rr-qat.toml`: one step `qat`, `train_ce.py --stage qat
--init-onnx $GM730_INIT/model.onnx --init-params-sha256 <bestParamsSha256>
--steps 400 --eval-every 100 --lr 1e-5 --tau-t 1 --alpha 0.5 --batch 32
--max-len 256 --micro-queries 4 --micro-batch 128 --cp2-safety 0.005 --seed 730
--job-steps 1 --eta-limit-minutes 52`, device cuda; `max_minutes = 60`, `max_attempts = 5`,
`max_minutes_scope = "job"`, data sets windows, teacher, hf l6 and
`gm730-rr-final-onnx-fp32` (`env GM730_INIT`, local_root the run's
`out/final/onnx-fp32`, include `model.onnx`). g5 family; `--family` as today.
`make_jobs.py qat` refuses a run whose `onnx-fp32/model.onnx` differs from
`export.json` or whose rebuilt parameters do not hash to `bestParamsSha256`.

ETA (g5.2xlarge, A10G): RR final measured 0.909 s/step in bf16; fp32 without
TF32 plus fake-quant estimated **2.5-3.5 s/step** -> 400 steps 17-23 min.
The emulated eager forward is ~3.15x the plain fp32 SDPA one (GM-734/S3, CPU)
and RR final's bf16 full-dev pass took 22 s, so an emulated full-dev pass is
~2-3 min: base rows (two full-dev passes) ~4 min, 5 dev rows ~12-17 min;
export + C-P + C-P2 + qatFidelity ~7 min (RR: 338 s without qatFidelity);
boot + venv + data ~5 min. **≈ 40-50 min wall.**

Time limits (owner 2026-11-20 on the S3 review: "Лимит 60 мин и $5.00, ETA с
учётом проверок (Recommended)"): `max_minutes = 60` (job scope). The S2 probe
(after the third timed step) forecasts the whole process: time elapsed so far
(data, base rows, the step-0 row), the remaining training steps at the
measured s/step, the remaining dev rows at the step-0 row's measured cost, and
`--export-minutes` (7). Over `--eta-limit-minutes 52` (= 60 - 5 boot - 3
margin) it exits 76 before the job can run out of time mid-export. The
checkpoint is saved after the step-0 row and after every dev row, so a spot
interruption repeats at most the steps since the last row (or 10 min).
A relaunched attempt's forecast does not know the minutes earlier attempts
used; the job-scope lifetime still bounds them.

Cost (owner, same answer): RR cap **$5.00**; the g5 planning price in
`budget.py` is the observed $0.92/h of the RR final's g5.2xlarge (was $0.75).
Worst case for `max_minutes 60`, 5 attempts: (60 + 5 x 5) min x ($0.92 + EBS
$0.0104)/h = **$1.318**; RR spent $3.670 + $1.318 = **$4.988 <= $5.00**.
Expected ~45 min at $0.92/h ≈ $0.70. GM-730 total $5.95 + $1.32 stays under
the $20 S3 rule. Earlier: cap $4.60 (D1) at the $0.75 price and 45 min.

### 3.3 Stop rules

- **S-F:** smoke fidelity (3.1.2) or zero-step export (3.1.3) fails -> no
  GPU launch; fix the emulation first.
- **S-B:** `budget.py check` refuses -> no launch.
- **S2:** probe ETA of the whole job (elapsed + training + remaining dev rows
  at the measured step-0 cost + `--export-minutes` 7) > 52 min -> exit 76; no relaunch
  without the owner (fallback g: bf16 integer-domain, or 200 steps).
- **S-G:** C-P2 fails on the instance -> no second QAT run without the owner
  (options then: more steps / higher lr, ship fp32 L6 with its cost gate,
  or drop RR).
- **S-A (acceptance, laptop after pull):** ORT quint8 full-dev MRR (chunk 1,
  3,198 windows, the pulled `onnx-quint8`) must be > base L6 (fp32 0.4036
  and its quint8, both reported) and r@10 >= base - 1 pt (D8); else fail.
- Cost logged (`cost.json`, history doc 7Qj12xvHcKyvYzhtPWUMQi).

## 4. Edit map

| file | where (current lines) | change |
|---|---|---|
| `eval/training/gm730/qat.py` | new | `fq_weight`, `fq_act`, `FakeQuantLinear`, `FakeQuantEmbedding`, attention patch, `wrap` / `unwrap` / `enabled` / `mask_context`, `TARGETS` (module paths of the 38 Linears + 3 tables) |
| `train_ce.py` | docstring 1-29 | `--stage qat` paragraph |
| `train_ce.py` | `load_model` 52-56 | `attn_implementation` arg (eager for qat training, default for export) |
| `train_ce.py` | `dev_eval` 84-102 | pass the attention mask to `qat.mask_context` (needs `forward_sorted`'s feed; no metric change); `windows` arg for the 300 subset |
| `train_ce.py` | `train_step` 105-130 | unchanged (mask context set inside `logits_fn` 59-63) |
| `train_ce.py` | `export_final` 171-196 | export a fresh `load_model` with the selected params; add `qatFidelity` record |
| `train_ce.py` | `run` 199-282 | `if a.stage == "qat": return run_qat(a, argv)` |
| `train_ce.py` | new `run_qat` after `run` | init, base rows, step loop with `Checkpointer` / `Probe`, eval rows, selection (2), `result.json` (base = base L6 rows, best = selected step, quint8-emulated evals), `train.json` (history, initSha, selected step, determinism), `export_final` |
| `train_ce.py` | `parse` 285-308 | stage choice `qat`; `--init-onnx`, `--init-params-sha256`, `--steps`, `--eval-every`, `--cp2-allowance` (0.0023), `--cp2-safety` (0.005) |
| `trainlib.py` | after `load_trainable` 262-271 | `init_from_onnx(model, path, want_sha)` |
| `trainlib.py` | `forward_sorted` 322-334 | hand the feed's mask to a callback (or `qat.mask_context` inside the fn); behaviour unchanged |
| `make_jobs.py` | docstring 1-30, `MAX_MINUTES` 47, `ETA_LIMIT_MINUTES` 52 | `rr-qat`: 60 / 52 (S3 review; was 45 / 30) |
| `make_jobs.py` | `data_sets` 114-129 | `rr-qat` adds the init set (`GM730_INIT`) |
| `make_jobs.py` | new `qat_job` after `final_job` 196-215; `main` 231-253 | `qat --init-run DIR [--family F]`; reads `out/final/train.json` `bestParamsSha256` and checks `onnx-fp32/model.onnx` sha against `export.json` |
| `budget.py` | `ROUTE_CAP` 31, docstring 15, g5 price | rr 4.5 -> 5.00, g5 $0.92/h (S3 review, owner: "Лимит 60 мин и $5.00, ETA с учётом проверок (Recommended)") |
| `export.py`, `eval_checks.py`, `register_arms.py`, `freeze.py` | - | unchanged: `eval_checks.py cp2 --export <run>/out/qat/export.json`; `register_arms.py ce --name ce-gm730-l6-distill-qat-quint8 --src <run>/out/qat/onnx-quint8 --chunk1`; no freeze (single pre-registered config) |
| `test_gm734_qat.py` | new | section 6 |
| `test_gm730_train.py` | `Resume` 112-125 | qat kill / resume case on `TinyWorld` |
| `test_gm730_freeze.py` (make_jobs / budget tests live there and in `test_gm730_checks.py`) | - | qat job TOML, budget route rr |

## 5. Callers and references relied on

| claim | how found |
|---|---|
| `quint8_check` has one caller, `export_final` (train_ce.py) | g-mesh `find_callers quint8_check` (project g-mesh-wt-gm734; python semantic tier absent, `provenance` hint) |
| two `export_final` and two `train_step` (train_ce.py, train_encoder.py); only train_ce's change | g-mesh `find_references` (ambiguous result lists both) |
| `export.quantize` callers via the `X.` receiver (train_ce 157/180, train_encoder 264/272, test_gm730_export) | g-mesh `find_references quantize` returned only `recipe_control` / `ours_vs_official` (receiver calls missing, `provenance` absent tier) -> grep `\.quantize(` |
| `cp2` consumers: `eval_checks.cp2`, `run_measure.sh` 362, tests | grep (non-code shell + receiver) |

## 6. Tests slice: what to pin (controls = revert the code, the test must fail)

1. `fq_weight` equals `export.closed_form` dequantised, bit for bit, per
   channel and per tensor. Control: scale maxabs / 127.5.
2. `fq_act` equals ORT `DynamicQuantizeLinear` (one-node graph) on tensors
   incl. all-positive and all-negative. Control: drop the 0 from the range.
3. Masked ranges: a pair's emulated logit in a padded batch equals its
   chunk-1 logit (<= 1e-5). Control: range over the padded tensor.
4. Emulation vs ORT on the fixture CE exported through `export_dir` +
   `quantize("l6")`: max |logit diff| at chunk 1 under tolerance. Control:
   skip the act x act quantisers (or the tables).
5. `init_from_onnx` reproduces the fixture's `params_sha256`; a wrong sha
   exits. Control: no transpose of MatMul weights.
6. Export after `--steps 0` is byte-identical to the plain export (fresh
   model, SDPA). Control: export the wrapped / eager model.
7. STE: weight gradients through `FakeQuantLinear` are non-zero and equal
   the unwrapped Linear's for a fixed input. Control: no `.detach()` trick.
8. `--stage qat` kill at step 2 + resume = uninterrupted (params sha,
   history). Control: resume without restoring the step index.

Plus without controls: selection rule picks the first maximum among
m >= 0 and falls back as in section 2; `make_jobs.py qat` TOML loads with
`gpu_train.load_job` and `budget.py` maps `gm730-rr-qat` to route rr.

## 7. Decisions for the owner

### D1. Money: RR cap

**Today:** RR spent $3.670 of $4.50. **Change:** the QAT job's worst case is
$0.887 (45 min, 5 attempts). **Consequence:** without a change `budget.py`
refuses.

| option | benefit | risk |
|---|---|---|
| **A. Raise the RR cap to $4.60 (recommended)** | 5 attempts survive spot interruptions | $0.10 more ceiling |
| B. Keep $4.50, `max_attempts = 3` | No cap change | A third interruption ends the job |

### D2. Selection subset

**Today:** RR picks the best epoch on full dev fp32. **Change:** QAT picks
on emulated quint8. **Consequence:** the gate subset (first 300 windows) is
part of dev.

| option | benefit | risk |
|---|---|---|
| **A. Full dev ranks, 300 windows only filter by predicted margin (recommended)** | No selection on the gate's own windows | A point that passes on 300 but ranks lower on full dev is not taken |
| B. Rank by the 300-window emulated quint8 (as the brief suggested) | Directly aims at the gate | Selects on the test subset; optimistic gate |

### D3. Laptop CPU pilot before the GPU job

| option | benefit | risk |
|---|---|---|
| **A. Skip; smoke 3.1.1-3.1.4 only (recommended)** | No hours of laptop CPU; GPU job is ~$0.6 | No early signal that QAT moves the 300-window number |
| B. 40-step CPU pilot | Free direction signal | ~1 h laptop, 40 steps may stay inside noise |

### D4. RR shuffled-teacher control (C-S)

RR's C-S never ran (the job stopped after `final` failed C-P2). It is a
GM-730 ship-rule input, not a GM-734 acceptance item.

| option | benefit | risk |
|---|---|---|
| **A. Separate GM-730 follow-up, not in this job (recommended)** | QAT job stays small and under the cap | RR's ship rule still lacks C-S |
| B. Add a `shuffled` step to the QAT job (full RR shuffled run, ~25 min) | One launch | +~$0.35, cap raise to ~$5.00 |
