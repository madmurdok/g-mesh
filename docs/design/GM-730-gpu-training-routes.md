# GM-730: GPU training routes for code search (design, release 4.35.0)

Design slice GM-730/S1. No code, no GPU spend. Split from GM-436 (its
section 12 "not tried": full gte-small student, LoRA/full jina, reranker
distillation). Production does not change in this task.

**Baseline to beat:** the GM-436 query adapter (full form) on
`jina-v2-base-code-int8-structured`, v2 eval (6 corpora, 1320 queries):
r@10 0.608 -> 0.655 [0.634, 0.675], MRR 0.380 -> 0.423 [0.406, 0.441]
(`docs/results/gm-436-laptop-adapter.md` section 3). Its dev MRR (GM-456
dev, int8 vectors) is 0.4303 -> 0.4640 (+0.034).

**Reranker baseline:** shipped MiniLM-L6 (ship text, beta 40), held-out NL,
K=30: r@10 0.670, MRR 0.465. The teacher, gte-reranker-modernbert-base
(gte-mb), passes GM-463 rules 1-4 with dMRR +0.047 [+0.031, +0.063] but is
9.50x L6's latency (rule 5 fails) (`docs/results/gm-463-reranker-bakeoff.md`).

## 0. Facts this design builds on

- **D9** (`docs/architecture/embedding-eval.md` D9): candidate vs reference
  jina fp32, one-sided 95% bounds from D5's paired bootstrap over queries,
  10,000 resamples, stratified by language, seed 398. Cost candidate: Q1-Q5
  (Q1 Δr@10 >= -2.0 pts point, lower >= -5.0) plus a cost win per D11.
  Quality candidate: lower bounds of Δr@10 and ΔMRR > 0, Q3-Q5, cost <= 1.5x
  pass time / 1.2x RSS, size < 1 GB.
- **`debug-embed-eval report --reference <arm>`** already exists
  (`core/src/cli/embed_eval.rs`, `ReportArgs::reference`, read in `report`
  at :1585): it scores every arm against any run instead of
  `settings.reference`. So "paired CI vs the adapter" needs **no new code**:
  a second `report` with `--reference` = the GM-436 adapter arm.
- **GM-436 D9 rule (sha256 freeze):** hyperparameters are chosen on GM-456
  dev only; the chosen config file's sha256 is written into the results doc
  before any v2 run. Dev metric: `eval/training/gm436/dev_metric.py`
  (each dev query ranked against every node of its dev repo; MRR primary,
  r@10; CI clustered by target, 10,000 resamples, seed 436;
  `paired_bootstrap` for paired gains).
- **Data:** GM-456 v0.4-full, 23,798 rows; split by repo: 20,600 train
  queries / 3,198 dev queries (538 dev targets), 24 repos, none of them in
  the 6 eval corpora (GM-456 leak guard). Each row carries explicit hard
  negatives. Structured node texts are short (mean ~98 chars, ~25-40
  tokens; GM-436 section 7).
- **GM-463 harness:** `eval/embedding/gm463_bakeoff.py` on top of
  `rerank_eval.py` (CE registry `CES`/`CE_BAKEOFF`, HF-pinned, sha256 per
  file), recall stage `--st-runs runs/jina-v2-base-code-int8-structured`,
  FIT/held-out split `R.held_out`, beta tuned on FIT NL. Rules 1-5 of its
  design section 3.6 (paired vs ship-L6: 1 dMRR lower > 0; 2 dr@10 lower >=
  -1.0 pt; 3 D9 vs int8-structured at the shipped floors; 4 name dMRR lower
  >= -0.010; 5 offline p50 at K=30 <= 2.0x L6).
- **Production reranker** is the official dynamic `quint8` AVX2 export of
  MiniLM-L6 (`core/src/embedding/rerank.rs:42-46`, `RERANK_ONNX_SHA256`
  pinned; `token_type_ids` always fed). A student with L6's architecture
  and tokenizer is a file swap for `rerank.rs`; any other architecture is
  a product change.
- **Production jina int8** is the official `onnx/model_quantized.onnx` at
  revision `516f4baf…` (`eval/embedding/v2/variants.toml` row
  `jina-v2-base-code-int8-structured`, `token_type_ids = false`,
  `max_tokens = 1024`). Its quantisation recipe is not documented by the
  publisher; a fine-tuned jina must be re-quantised by a recipe that is
  first shown to reproduce the official file (section 4.2).
- **GM-457 launcher** (`eval/gpu/`): one spot g5.xlarge (A10G 24 GB, 4
  vCPU, 16 GB RAM) or g5.2xlarge at a time (quota 8 vCPU); effective cost
  ~$0.70 per instance-hour including EBS; boot + venv ~2-3 min per attempt.
  GM-457 spent $0.99 (`cost.json` sum) + ~$0.30 FIS + ~$0.02 kill-switch
  test = **~$1.3**. The `gpu-training-50` budget kill switch is $50/month
  with hours of lag, so it is a backstop, not the GM-730 cap.

## 1. Routes

Three routes, run one at a time (one instance). Each route = **job A**
(dev grid) -> laptop freeze (sha256) -> **job B** (final train at 100%,
shuffled control, export, quantisation, instance-side parity) -> laptop v2
eval. The reranker route has one extra job before its grid (teacher
scoring), whose output also feeds the two encoder routes (D5).

### 1.1 Route RR: reranker distillation (gte-mb -> small cross-encoder)

- **Student:** `cross-encoder/ms-marco-MiniLM-L6-v2` @ `233902d2…` (D4a).
- **Windows:** for every GM-456 train and dev query, the top 30 nodes of
  its repo by int8-structured cosine, computed on the laptop from the
  GM-436 vector cache (`cache/vec/jina-v2-base-code-int8-structured@*`).
  The gold positive is added if missing (marked `forced`, so the window has
  31 entries; ~20.6k x 30 = ~618k train pairs, ~96k dev pairs).
- **Pair text:** GM-698 ship text (E + P + B48, `gm462_ce_text.py`), the
  text the shipped reranker and gte-mb were judged on (must confirm M6).
- **Job RR-T (teacher):** gte-mb fp32 logits for every window pair, plus
  1,000 cached GM-463 v2 pairs (parity, C-T below). Output `teacher.f32`
  (~0.7M floats, 3 MB) + `pairs.sha256`. Pulled, then uploaded as a
  `[[data]]` set for every later job (D7a).
- **Loss:** listwise KL(softmax(teacher/τ_t) || softmax(student)) over the
  window + α · cross-entropy of the gold positive in the window.
- **Grid (job RR-A, dev):** lr {2e-5, 5e-5} x τ_t {1, 2} x α {0, 0.5} =
  8 configs, up to 2 epochs, best epoch by dev MRR; batch 32 queries x 31
  pairs; AdamW, 5% warmup, linear decay; max_len 256 (M7); seed 730.
- **Dev metric:** MRR / r@10 of the student's ranking inside each dev
  window (pure CE score, no beta blend; the blend's beta is tuned later on
  v2 FIT by the harness, as for every GM-463 model), CI clustered by target
  as `dev_metric.py`. Base row: off-the-shelf L6 on the same windows,
  computed in job RR-A at epoch 0.
- **Job RR-B:** frozen config at 100%; the shuffled-teacher control (same
  config, teacher scores permuted within each window, multiset kept);
  export both to ONNX fp32 with L6's inputs (`input_ids`, `attention_mask`,
  `token_type_ids`), then `quint8` with the recipe that reproduces the
  official file (C-Q); parity on the instance.

### 1.2 Route J: jina-v2-base-code fine-tune (LoRA or full)

- **Model:** `jinaai/jina-embeddings-v2-base-code` @ `516f4baf…`, custom
  JinaBert code vendored from its `auto_map` repo at a pinned revision
  (M9), mean pooling, `HF_HUB_OFFLINE=1` on the instance.
- **Loss:** InfoNCE over (query, gold positive, 7 GM-456 hard negatives
  sampled with the seed, in-batch docs), τ in the grid; plus α · KL to the
  RR-T teacher over the query's 30-window (D5a; α = 0 if RR-T failed).
- **Grid (job J-A, dev), LoRA and full in one job (D3a):**
  - full: lr {1e-5, 2e-5}, τ 0.05, α {0, 0.5} -> 4 configs;
  - LoRA (q, k, v, output dense; module names M9): r {16, 32}, lr 2e-4,
    α {0, 0.5} -> 4 configs;
  - batch 32 queries x 8 docs (+ in-batch), up to 2 epochs, best epoch by
    dev MRR, max_len 128 docs / 64 queries (M7), bf16 autocast, seed 730.
  - `--grad-checkpoint` on every J job (owner 2026-11-20, after the alpha
    0.5 steps, ~970 docs per step with the teacher windows, ran out of
    memory on an L40S 44 GiB): non-reentrant per-layer activation
    checkpointing (`train_encoder.grad_checkpoint`), same loss and
    gradients bit for bit on CPU (tiny BERT with dropout, real jina full
    and LoRA), step ~1.4x slower. Saved activations, measured on CPU:
    ~593 KB/token + 864 B per (doc x len^2) without it, ~40-80 KB/token
    with it; the largest alpha 0.5 step (58k padded tokens) needs ~13 GB
    instead of ~48 GB.
- **Dev:** `dev_metric.py` on vectors from the torch model (fp32). The
  base row is the same torch path at epoch 0, so gains are compared within
  one path (the GM-436 dev numbers came from int8 vectors).
- **Job J-B:** the dev winner (one form only, D3a); LoRA merged into the
  weights before export (same graph as the base); shuffled control (below);
  ONNX fp32 export, int8 with the recipe reproducing
  `model_quantized.onnx` (C-Q); dev MRR at int8 on the instance CPU (C-P2).

### 1.3 Route R3: full gte-small student (distillation)

- **Model:** `thenlper/gte-small` @ `17e1f347…` (the revision of the eval's
  ONNX), mean pooling, fp32 ONNX as the existing `gte-small-structured` row.
- **Loss:** as route J (InfoNCE with 7 hard negatives + α · KL to the
  teacher). Teacher: RR-T gte-mb window scores (D5a) or the GM-436 jina
  int8 cache (D5b).
- **Grid (job R3-A):** lr {2e-5, 5e-5, 1e-4} x α {0.5, 1.0}, τ 0.05,
  batch 64 queries x 8 docs, up to 3 epochs, best epoch by dev MRR -> 6
  configs.
- **Job R3-B:** frozen config at 100% + shuffled control + fp32 export and
  parity. R3 is a **cost candidate** (D9): it must pass Q1-Q5 vs jina fp32,
  i.e. r@10 >= 0.588 with lower bound >= 0.558, from a base at 0.505 (the
  GM-436 gte adapter reached 0.536). Its D11 costs equal gte-small's by
  construction (same graph and size; checked by file size and op list).

## 2. Controls (every route)

| id | control | pass rule | where |
|---|---|---|---|
| C-R (recipe) | Export the **unmodified** base with our export code; quantise the official fp32 ONNX with our quant recipe | jina: our fp32 export vs official `model.onnx`, max cosine distance <= 1e-4 on 1,000 structured texts; our int8 vs official `model_quantized.onnx` <= 1e-5 (or identical initializers). gte: export vs eval ONNX <= 1e-4. L6: fp32 logits max abs diff <= 1e-4; quint8 vs official quint8 <= 1e-5 at chunk 1 (`INT8_MODELS` rule). **Fail = the route is not started** (no GPU spend) | laptop, before job A |
| C-R2 | The C-R arm on v2 (jina int8 requant of the base; gte export) | v2 metrics equal the official arm's within 0.002 r@10 / MRR; rankings top-10 identical on >= 99% of queries | laptop v2 |
| C-P (parity) | Trained model: torch vs exported fp32 ONNX | max cosine distance <= 1e-4 on 1,000 eval-corpus texts (CE: max abs logit diff <= 1e-4 on 1,000 pairs) | instance, job B |
| C-P2 | Quantised trained model vs its fp32 | dev MRR (int8, CPU) >= dev MRR (fp32) - (base fp32-vs-int8 dev gap + 0.01) | instance, job B |
| C-S (shuffled) | Route J and R3: positives deranged across targets (seeded, no fixed points) **and** teacher scores permuted within each window, same frozen config. Route RR: teacher permuted within each window | Does not beat its base: on v2 (or held-out NL for RR) paired lower bound of ΔMRR <= 0 and of Δr@10 <= 0 vs the base arm (int8 official / gte-small / ship-L6). A drop is reported, not a failure. **Beats base = route invalid** (stop S5) | job B + laptop eval |
| C-T (teacher) | RR-T logits on 1,000 cached GM-463 v2 ship pairs vs the laptop's cached gte-mb logits | max abs diff <= 0.05 (GPU vs CPU fp32; GM-463 C7 tolerance) | job RR-T |
| C-0 | GM-436 runs re-reported | `report` reproduces int8 base 0.608/0.380 and adapter full 0.655/0.423 exactly | laptop, before eval |
| C-F (planted fault) | Each comparator dry-run on a planted fault | C-P fails on an ONNX with one weight row scaled by 1.01; C-S "does not beat" check fails on a planted +5-pt arm; C-R fails on the official fp32 ONNX quantised with `per_channel` flipped | tests slice + laptop |
| C-D | Determinism note | Seeds, `CUBLAS_WORKSPACE_CONFIG`, `torch.use_deterministic_algorithms` state recorded. No GPU rerun (GM-457 parity showed ~3e-6 drift; not worth $) | job B |
| C-L | Leak | The 6 v2 corpora are disjoint from the 24 GM-456 repos (card.md leak guard), restated in the results doc | laptop |

## 3. Eval arms and the comparison method

### 3.1 Embedding routes (J, R3): unchanged GM-398 harness

Rows appended to `eval/embedding/v2/variants.toml` (no edits to existing
rows), all `text = "structured"`:

| arm | role | source |
|---|---|---|
| jina-v2-base-code-fp32 | D9 reference | existing runs |
| jina-v2-base-code-int8-structured | base | existing runs (C-0) |
| GM-436 int8 adapter full | comparison reference for paired CIs | existing runs (C-0) |
| gte-small-structured | R3 base | existing runs |
| random, shuffled | broken arms | existing runs |
| `gm730-jina-<form>-int8-structured` | J candidate (quality) | J-B export |
| `gm730-jina-shuf-int8-structured` | C-S | J-B |
| `gm730-jina-requant-base-int8-structured` | C-R2 | laptop |
| `gm730-gte-student-structured` | R3 candidate (cost) | R3-B |
| `gm730-gte-shuf-structured` | C-S | R3-B |
| `gm730-gte-export-base-structured` | C-R2 | laptop |
| `gm730-jina-<form>-int8-structured-words-shuffled` | informational (word-order gap, as GM-436) | J-B |

Laptop wall per arm (GM-436 section 7): jina int8 over v2 15-40 min; gte
3-8 min. About 6 jina-size arms -> 2-4 h of laptop CPU, run as one
background script.

Reports, all from the same runs:
1. `debug-embed-eval report` (D9 vs jina fp32, D6 floors refit per arm on
   v2 FIT, NL/name splits, strict held-out, validity). The C-S and C-R2
   arms are exported as informational arms, not `role = control` (GM-436
   section 8 bug).
2. `debug-embed-eval report --reference <GM-436 adapter arm>`: the **paired
   comparison vs the adapter**, same D5 bootstrap (paired over queries,
   stratified by language, 10,000 resamples, seed 398). Δr@10 and ΔMRR with
   two-sided 95% CIs and one-sided bounds.
3. `report --reference <own base>` for C-S (shuffled vs its base).
4. `shipped_floor_rates.py`, `recall_at_k.py`, `gm460_subset.py` (GM-398
   and confirm subsets), per corpus, as GM-436.
5. D11: route J's int8 model has the base's graph and size, so its costs
   equal the int8 base (checked: same op list and file size within 1%);
   R3 likewise equals gte-small. The existing `costs.toml` rows are reused
   (M13), so the harness can print a verdict instead of `Undecided`.

### 3.2 Route RR: GM-463 harness and rules

`gm463_bakeoff.py quality --k 30`, held-out NL (rules 1-3, 5) and all
queries (rule 4: name MRR), arms:

- ship-L6 at beta 40 (cached, C0 of GM-463 reproduced first);
- `ce-gm730-l6-distill` fp32 at its FIT-tuned beta (and at beta 40);
- `ce-gm730-l6-distill-quint8` (the shippable form; C-Q);
- `ce-gm730-l6-shuf` (C-S);
- gte-mb (teacher, cached) as the ceiling row;
- with D6a: ship-L6 and the student over the **adapter's** top-30 windows
  (`--st-runs` = the GM-436 adapter run, M5).

Rules 1-5 unchanged. Rule 5: the student has L6's graph, so it should be
1.0x; it is still measured with a short mirrored latency run
(`L6 student-quint8 student-quint8 L6`, GM-463 3.5 method).
New scoring: ~103k pairs per new model at K=30 (~26 min at ~15 ms/pair
for L6), x3 models, + ~103k x 2 for the adapter windows (D6a).

### 3.3 Recommendation rule (pre-registered, D8a)

- **Ship route J as its own production task** if: D9 quality-candidate
  gates pass vs jina fp32 **and**, paired vs the adapter, ΔMRR lower bound
  > 0 and Δr@10 lower bound >= -1.0 pt (mirrors GM-463 rules 1-2), and C-S,
  C-P, C-P2, C-R2 pass.
- **Ship R3 as its own task (a cheaper model)** if it passes Q1-Q5 as a
  cost candidate with the D11 win of gte-small. Paired vs adapter is
  reported, not gated (a cost candidate is not meant to beat it).
- **Ship RR as its own task** if the quint8 student passes GM-463 rules 1-5
  vs ship-L6. It stacks with the adapter (different layer), so the 2x2 of
  D6a decides whether both ship: if (adapter + student) beats (adapter +
  L6) with paired dMRR lower > 0, recommend both.
- **Otherwise ship the laptop adapter** (GM-436's recommendation stands);
  **stop** GPU training if no route beats its dev gate (S4).
- Among passing routes of one layer: higher ΔMRR point vs the adapter,
  ties to the cheaper. Only the pre-registered dev winner of each route is
  gated; other forms are informational (multiple comparisons).

## 4. Export and re-quantisation

### 4.1 Export (job B, on the instance, inside the training step)

`torch.onnx.export`, opset 17, dynamic axes (batch, sequence), inputs
exactly those of the variant row / CE registry entry, output the
last hidden state (embedders; pooling stays in Rust as today) or the logit
(CE). Tokenizer files copied from the pinned snapshot unchanged
(sha256 recorded; must equal the base's). The same code exports the
unmodified base on the laptop for C-R.

### 4.2 Quantisation

- **jina:** `onnxruntime.quantization.quantize_dynamic` with the settings
  that reproduce `model_quantized.onnx` from the official fp32
  `model.onnx` (weight type, `per_channel`, `reduce_range`, op types,
  nodes excluded). M2 reads the official int8 graph to find them. If no
  setting reproduces it within C-R's tolerance, route J **stops before
  GPU spend** and the owner decides (export fp32 only, quality-candidate
  cost gate 1.5x, or drop).
- **L6:** the same for `model_quint8_avx2.onnx` (unsigned, AVX2), which
  is the file `rerank.rs` pins.
- **gte-small:** none (the eval row is fp32).

## 5. GPU budget (estimates; replaced by each job's throughput probe)

Remaining under the $50 kill switch: ~$48.7 (GM-457 ~$1.3; M14 confirms
with Cost Explorer before the first job). **GM-730 cap $20; hard stop at
$25 cumulative** (D2a). `max_minutes` per job = the job's cap / $0.70 per
hour, so the in-OS lifetime enforces it; the orchestrator sums `cost.json`
after every job.

Throughput assumptions (not measured; A10G, bf16 autocast, short texts):
gte-small ~0.15 s per 512-doc step; jina ~5x gte; gte-mb inference ~1,000
fp32 pairs/s; L6 training ~5,000 pairs/s. Each job's first step is a probe
that prints s/step and the ETA (stop rule S2).

| route | job | content | est. instance-h | est. $ | `max_minutes` |
|---|---|---|---|---|---|
| RR | RR-T | gte-mb on ~714k window pairs + 1,000 parity pairs | 0.4 | 0.3 | 60 |
| RR | RR-A | 8 configs x <= 2 epochs + epoch-0 base | 0.8 (measured bf16: 2.6) | 0.6 (2.3) | 190 (ETA limit 170, owner 2026-11-19) |
| RR | RR-B | final + shuffled + export + quint8 + parity | 0.4 | 0.3 | 60 |
| RR | **route cap** | | 1.6 | 1.2 | **$4.50** (was $4; owner 2026-11-20: grid spend $3.21 left the final job $0.22 short) |
| J | J-A | 8 configs (4 full, 4 LoRA) x <= 2 epochs | 1.8 (measured probe ETA 139.5 min on g6e without checkpointing; x 1.4 with `--grad-checkpoint` = 195 min) | 1.3 | 395 (ETA limit 360; g6e 237/216, g6 632/576). Owner 2026-11-20 after the alpha 0.5 OOM on L40S 44 GiB: "Включить gradient checkpointing и перезапустить (Recommended)" (hyperparameters and grid unchanged); GM-730/S62 sized the limits from 139.5 min x 1.4 (CPU fwd+bwd ratio 1.36) with the earlier margins. Worst case g5 $5.32, g6e $8.78, g6 $7.78: with $0.81 J spend, g6e and g6 are refused by the $8 cap |
| J | J-B | final + shuffled + 2 exports + 2 int8 + CPU int8 dev | 1.5 | 1.1 | 180 |
| J | **route cap** | | 3.3 | 2.4 | **$8.95** (was $8; owner 2026-11-21: "Для,финала бюджет расширим"; J spend $6.05 after the grid + final worst case $2.60 + $0.30) |
| R3 | R3-A | 6 configs x <= 3 epochs | 0.5 (measured: 1.85) | 0.4 (1.4) | 140 (ETA limit 125, owner 2026-11-20) |
| R3 | R3-B | final + shuffled + 2 exports + parity | 0.4 | 0.3 | 60 |
| R3 | **route cap** | | 0.9 | 0.7 | **$3** |
| all | reserve | relaunches after spot interruption, probe overruns, one rerun | | | **$5** |
| | **total** | | ~5.8 | ~4.3 | **$20** |

Each attempt adds ~3-5 min (boot, venv, data sync). The caps are 3-4x the
estimates because the throughput figures are not measured.

**How the caps are enforced.** Every GM-730 job sets
`max_minutes_scope = "job"`: `max_minutes` is the job's total lifetime across
relaunches, and each relaunch gets what earlier attempts left (from
`launches.json`); below 10 min the launch is refused. Before each launch
`eval/training/gm730/budget.py` refuses (exit 3) when the route's recorded
spend plus the job's worst case passes the route cap, when the GM-730 total
plus the worst case passes $20, or, for R3, $15 (S3); a run dir without
`cost.json` counts as unknown spend and is refused. Each step's probe stops
the job (exit 76) when its job-level ETA passes `--eta-limit-minutes`.
Worst cases at $0.70/h plus EBS: RR $3.73 (cap $4), J $5.57 (cap $8),
R3 $2.02 (cap $3).

**GPU family fallback (g5 -> g6e -> g6, owner 2026-11-19).** When g5 spot
capacity is short, a job runs on g6e (L40S 48 GB) and then g6 (L4 24 GB),
one family per run, xlarge/2xlarge only (8 vCPU G/VT quota). `make_jobs.py
--family g6e|g6` writes `gm730-<job>-<family>.toml`, pinned by
`instance_types` to that family, with `max_minutes` and
`--eta-limit-minutes` = the g5 values x the family's time factor, rounded
up: g6e 0.6 (~1.7x an A10G), g6 1.6 (~0.6x). `run_measure.sh` writes and
checks every family's TOMLs; the g5 TOMLs are unchanged. `budget.py` prices
a job's worst case by the family of its `instance_types` (config.toml's,
g5, when absent) at planning prices that are the upper end of the spot
prices seen on 2026-11-19, rounded up: g5 $0.75 (seen 0.60-0.73), g6e
$2.00 (1.1-2.0), g6 $0.70 (0.42-0.68). Launch a fallback TOML without
`--instance-types`, so the launch and the check see the same family; the
manifest records `instanceTypes`. Worst cases with no prior spend: RR-A
g5 $2.72, g6e $4.66 (over the $4 RR cap: refused), g6 $3.90; J-A g5 $3.36,
g6e $5.66, g6 $4.84; RR-T and R3-A g5 $1.08, g6e $2.04, g6 $1.43. The
five attempts' boot minutes (25 min) do not scale, so g6e's price dominates
its short jobs.

## 6. Route order and stop rules

Order (D1a): **RR -> J -> R3**. RR first: cheapest, strongest prior
(teacher +0.047 dMRR at 9.5x cost; a distilled L6 keeps 1.0x), and RR-T's
teacher scores feed J and R3 (D5a). J second: the production model, the
direct comparator of the adapter. R3 last: cheap but lowest prior (needs
+8 pts r@10 over gte-small).

Stop rules (pre-registered):

- **S1 recipe:** C-R fails for a route -> that route does not start (no
  GPU spend); later routes continue.
- **S2 probe:** a job's ETA > 2x its estimate -> the job stops after the
  probe; report and ask.
- **S3 money:** route spend reaches its cap -> the route stops with what it
  has. Cumulative GM-730 >= $15 before R3 -> R3 is skipped. Cumulative >=
  $20 -> no new job; >= $25 -> everything stops, `verify-clean`.
- **S4 dev gates (before job B, so before export and laptop eval):**
  - RR: paired dev MRR (student - off-the-shelf L6, same windows) lower
    bound <= 0 -> stop the route ("no dev gain").
  - J: best dev MRR gain over its own epoch 0 < the adapter's dev gain
    (+0.034) -> stop ("does not beat the adapter on dev").
  - R3: best dev MRR < 0.41 (jina int8 dev 0.4303 minus 0.02, Q2's point
    margin) -> stop ("cannot reach a cost candidate").
- **S5 control:** C-S's dev MRR lower bound above its base's dev point, or
  C-S beating its base on v2 -> the route's result is invalid; stop that
  route, diagnose; it gives no recommendation.
- **S6 launcher incident:** an instance left running, `verify-clean`
  failing beyond the known spot-request lag (GM-457 finding 1), or a
  `cost.json` above its cap -> halt all routes until the owner reviews.
- **Early stop of later routes:** S6 halts everything; an RR-T failure
  (C-T) does not stop J/R3, they run with α = 0 (gold only) and the grid
  halves; S1 on J (jina recipe) does not stop R3.

## 7. Launcher fit (GM-457 job format and runner) and gaps

What fits: per-step `--out` + `--device`, checkpoint resume contract
(atomic `checkpoint.pt`, exit 75), steps skipped once done, `[[data]]`
content-addressed subsets (uploaded once), `[vars]`, `pull` of
`runs/<id>/out/**` (checkpoints excluded), `cost.json` per run.

| id | gap | effect on GM-730 | proposal |
|---|---|---|---|
| G1 | A step cannot read an earlier step's outputs after a relaunch: `Runner.run` restores only the current step's checkpoint (`restore_checkpoint`), and `fetch_inputs` fetches only `[[data]]`; earlier steps' `out/` exists only on the first instance | RR-T -> RR-A, grid -> final, train -> export cannot be separate steps of one job | D7a: one job per stage, outputs pulled and re-uploaded as `[[data]]`; export runs at the end of the training step itself (a checkpoint holds final weights, so an interrupted export redoes only the export) |
| G2 | Env lock `eval/training/gm436/requirements-linux-cu121.txt` has torch 2.2.2, transformers 4.46.3, sentence-transformers; no `onnx`, `onnxruntime`, `peft`; ModernBERT (gte-mb) needs transformers >= 4.48 | No export, quantisation, LoRA or teacher scoring on the instance | New `eval/training/gm730/requirements.in` + linux cu121 and macOS hash locks; ORT pinned to the laptop's 1.19.2 for parity. If JinaBert's custom code breaks on transformers >= 4.48, two locks (teacher, encoders) (M10) |
| G3 | No model-weights mechanism; the instance would download from HF at run time (unpinned network dependency, custom code for jina) | Reproducibility, and jina's `trust_remote_code` | HF snapshots (`snapshot_download(local_dir=…)`, plain files, pinned revisions) as `[[data]]` sets: jina ~0.6 GB, gte-mb ~0.6 GB, gte-small ~0.13 GB, L6 ~0.09 GB; `HF_HUB_OFFLINE=1` set by the trainers (runner's `job_env` does not set it) |
| G4 | Large checkpoints: jina full fine-tune state is ~1.9 GB (fp32 weights 0.64 GB + AdamW 1.3 GB). `upload_checkpoint` stages a copy, server-side copies the previous one, then uploads, at every `checkpoint_minutes` and after a spot notice | On a spot notice, SIGTERM grace + a ~2 GB upload must fit the 2-minute reclaim; otherwise the previous upload (<= `checkpoint_minutes` old) is used | `grace_seconds = 30`, `checkpoint_minutes = 10`; M12 measures the upload time. LoRA checkpoints are small |
| G5 | `runs/` in the bucket never expires; every file in `--out` is uploaded | jina ONNX fp32 + int8 ~0.8 GB per exported model | Export only the frozen winner, its control and words-shuffled twin (job B); grid steps write metrics only. ~3 GB total, ~$0.07/month |
| G6 | No $ cap per run or cumulative in the launcher; only `max_minutes`; the budget alarm lags hours | Route caps rely on lifetime + orchestrator arithmetic | Accept for GM-730 (`max_minutes` derived from each cap, section 5). Optional small change: `run --max-usd` refusing to launch when the bucket's `cost.json` sum for `gm730-*` exceeds it (D7 option b) |
| G7 | No Rust harness on the instance | v2 eval runs on the laptop from pulled ONNX (2-4 h CPU) | Accept; no change |
| G8 | Grid jobs are many near-identical `[[steps]]` | Hand-written TOMLs error-prone | `make_jobs.py` generates `eval/gpu/jobs/gm730-*.toml` from a grid spec (code slice) |
| G9 | Instance CPU (4 vCPU g5.xlarge) for the int8 dev check (C-P2) | ~20-30k dev nodes through jina int8 on 4 vCPU: estimated 10-20 min | Accept; or `--instance-types g5.2xlarge` for J-B (8 vCPU, same GPU, ~2x price) |

## 8. Edit map for the code slice

New directory `eval/training/gm730/` (Python, PEP 723 / uv, as gm436):

| file | contents |
|---|---|
| `requirements.in`, `requirements-linux-cu121.txt`, `requirements.txt` (macOS) | G2 locks |
| `snapshots.py` | `fetch(name)`: `snapshot_download` at the pinned revision into `g-mesh-data/gm730/hf/<name>@<rev>/` (plain files), sha256 manifest; jina custom-code repo included (M9) |
| `windows.py` | `build_windows(split)`: int8-structured top-30 per GM-456 train/dev query from the GM-436 cache (`gm436/cache.py` loaders), gold forced in; `pair_texts()`: ship text via `gm462_ce_text.py` (M6) and structured texts; writes `windows.jsonl`, `pairs.jsonl`, sha256s. Join check (C0b analog): dev windows equal the mine run's int8 `rankings.jsonl` top 30 |
| `score_teacher.py` | Job RR-T: gte-mb logits in chunks, checkpoint = chunk cursor + partial array (resume contract), C-T parity |
| `train_ce.py` | RR-A/RR-B: listwise KL + α gold CE; dev MRR in window; `--control shuffled-teacher`; at the end of a final step: export + quint8 + C-P |
| `train_encoder.py` | J and R3: `--model jina|gte-small`, `--mode full|lora`, InfoNCE + 7 hard negatives + in-batch + α KL; dev via `gm436/dev_metric.py`; `--control shuffled` (derangement + teacher permutation; reuse gm436 derangement code); GM-457 checkpoint contract; at the end of a final step: LoRA merge, export, int8, C-P, C-P2 |
| `export.py` | `export_onnx`, `quantize(recipe)`, `parity_embed`, `parity_ce`; CLI `recipe-control --model jina|gte|l6` (C-R, laptop) |
| `freeze.py` | pick each route's dev winner from pulled `result.json`, write `final_configs.json` + sha256 (D9 rule) |
| `make_jobs.py` | write `eval/gpu/jobs/gm730-{rr-teacher,rr-grid,rr-final,jina-grid,jina-final,gte-grid,gte-final}.toml` (`max_minutes` from section 5) |
| `register_arms.py` | append v2 `variants.toml` rows (local `model_dir`, file sha256) and CE entries; arms informational, never `role = control` |
| `run_measure.sh` | laptop side: C-R, C-0, v2 runs, the reports of 3.1, gm463 runs of 3.2, one background script with fail-fast and a summary file |
| `test_gm730_*.py` | resume equality on a tiny random BERT (kill + resume == uninterrupted, CPU); derangement has no fixed points and keeps multisets; teacher permutation keeps each window's multiset; C-P fails on a planted perturbed ONNX; C-R fails on a flipped quant flag; freeze writes a stable sha256; windows join check fails on a planted offset |

Additive changes elsewhere:

- `eval/embedding/rerank_eval.py`: a local CE registry (`CE_LOCAL = {name:
  (dir, {file: sha256})}`) so students load without HF; defaults unchanged.
- `eval/embedding/gm463_bakeoff.py`: model keys `ce-gm730-*` (BERT inputs
  and pair template as `ce-minilm`; quint8 entry in `INT8_MODELS`-style
  chunk-1 handling); `--st-runs` for the adapter recall stage if M5 shows
  it is not already accepted.
- `eval/embedding/v2/variants.toml`: appended rows only.
- `eval/gpu/`: only new job TOMLs under D7a. Under D7b also `runner.py`
  (`Runner.run`: restore `runs/<id>/out/<done step>/` before a step that
  declares `needs = [...]`) and `gpu_train.py` (`load_job` validates
  `needs`; optional `--max-usd`).
- `docs/results/gm-730-gpu-routes.md`: written by the measure slice.
- **Not touched:** `core/`, `rerank.rs`, production floors, any existing
  variant row, `gm462_ce_enriched.py`, `gm464_latency.py`.

## 9. Must confirm (measure slice, before the first GPU job unless noted)

1. **M1 (C-0):** GM-436 v2 runs for int8 base and adapter full are present
   (`eval/embedding/v2/work` symlink, GM-436 deviation 1); `report`
   reproduces 0.608/0.380 and 0.655/0.423; `report --reference <adapter>`
   runs.
2. **M2:** the official jina `model_quantized.onnx` graph: op types
   (`MatMulInteger`/`DynamicQuantizeLinear`), weight type, per-channel,
   reduce_range, excluded nodes; a `quantize_dynamic` setting that
   reproduces it (C-R). Same for L6 `model_quint8_avx2.onnx`.
3. **M3:** our torch -> ONNX export of the unmodified jina, gte-small and
   L6 reproduces the official ONNX outputs (C-R tolerances).
4. **M4:** a v2 `model` row can point at a local `model_dir` without an HF
   download and with sha256 pins; how fetch/verify treats it.
5. **M5:** `gm463_bakeoff.py --st-runs` accepts the GM-436 adapter run dir
   (rankings with scores) for D6a.
6. **M6:** GM-698 ship text (E + P + B48) can be built for the 24 GM-456
   repos (source at the mined commit available); else RR uses structured
   text for train pairs and says so (train/eval text mismatch).
7. **M7:** token p99 of structured node texts and queries (jina and gte
   tokenizers) and of ship pairs (L6) -> max_len 64/128/256.
8. **M8:** node counts of the GM-456 dev repos (dev ranking cost; C-P2 on
   instance CPU).
9. **M9:** jina's `auto_map` code repo and revision; LoRA target module
   names in that code; it loads offline.
10. **M10:** one transformers version serves both gte-mb (>= 4.48) and
    JinaBert custom code; otherwise two locks.
11. **M11:** per job, the probe's s/step vs section 5 (S2).
12. **M12:** upload time of a ~1.9 GB checkpoint from g5 to S3 (G4).
13. **M13:** `costs.toml` D11 rows for gte-small and int8-structured exist
    and apply by construction (op list, file size).
14. **M14:** Cost Explorer for 2026-11 (GM-457 actuals) and whether the
    $50 budget is per calendar month; restate the remaining amount before
    RR-T.
15. **M15:** spot capacity (us-east-1, fallback us-west-2), `verify-clean`
    after every job.

### 9.1 Answers (S8)

Laptop only, no AWS calls. Run artefacts: `/private/tmp/claude-502/gm730s8/`
(`summary.md`, `m1_report*.{txt,json}`, `m2_*.jsonl`, `m10.json`,
`tokens.json`); scripts in the S8 scratchpad. Model files fetched into
`~/Projects/ClaudeProjects/g-mesh-data/gm730/hf/<name>@<rev>/` (1.5 GB, each
with `MANIFEST.sha256.txt`; LFS files checked against the Hub's oid). Probe
env `g-mesh-data/gm730/venv-probe`: torch 2.2.2, transformers 4.48.3,
tokenizers 0.21.4, peft 0.21.3, onnx 1.16.2, onnxruntime 1.19.2, numpy
1.26.4, safetensors 0.8.0, accelerate 1.15.0. **[EDIT MAP]**, **[PARAM]**
and **[BUDGET]** mark answers that change section 8, a section 1 parameter
or section 5.

| item | answer and evidence |
|---|---|
| M1 | **Holds.** Runs present in `g-mesh-eval-data/v2/work/runs/` (symlinked as `eval/embedding/v2/work`, GM-436 deviation 1). `debug-embed-eval report` needs a `--features embed-eval` build (the default build rejects the subcommand; first attempt failed on this). `report` over fp32, random, shuffled, int8-structured, int8-structured-adapter-full: validity pass; int8-structured r@10 0.608 / MRR 0.380, adapter full 0.655 / 0.423: exact. `report --reference int8-structured-adapter-full` runs and prints the paired gates (int8 base vs adapter: Δr@10 -4.6 pts, lower -6.0; ΔMRR -0.044, lower -0.055), but its **validity line FAILS on floor parity** (adapter floors go 0.49 / python 0.50 / rust 0.47 vs shipped 0.57 / 0.59 / 0.57), so every verdict in that report is `Undecided`. Section 3.3 already gates on the paired lower bounds, not on the verdict: read them from the JSON (`arms.<arm>.gates`, `splitDeltas`). |
| M2 | **jina: reproduced bit-exactly.** Official `model_quantized.onnx` (sha256 `ed4587…`, = local eval copy): opset 11, producer `onnx.quantize 0.1.0`; 72 `MatMulInteger` (int8 weights, per channel, zero point 0, values in [-64, 64]) + 48 `DynamicQuantizeLinear` (uint8 activations); 24 float `MatMul` left (activation x activation, QK^T and attn·V); word and token-type `Gather` tables uint8 (zero point 128, 2 `DequantizeLinear`). ORT 1.19.2 `quantize_dynamic(fp32 onnx/model.onnx, weight_type=QInt8, per_channel=True, reduce_range=True)` (defaults otherwise; explicit `MatMulConstBOnly=True` gives the same) gives identical op counts and **74/74 quantised tensors bit-equal**. Control: the same with `reduce_range=False` leaves 72/74 tensors different (max diff 63), so C-F's flipped-flag control works. **L6: no `quantize_dynamic` setting reproduces it** (tried ORT 1.19.2, 1.16.3, 1.15.1, 1.14.1, 1.12.1; QUInt8, per channel, `WeightSymmetric=True`, `MatMulConstBOnly=False`): op counts equal on 1.19.2, but 0/41 tensors equal, because ORT's symmetric uint8 uses zero point 128 and scale 2·maxabs/255. The official file is a closed form: every one of the 41 tensors (36 encoder `MatMul`s, pooler and classifier, both stored transposed, 3 embedding `Gather` tables per tensor) is `q = rint(w/s) + 127`, `s = maxabs/127` per output channel, zero point 127; 41/41 bit-equal. **[EDIT MAP]** `export.py quantize(recipe)`: jina = plain ORT call above; L6 = ORT 1.19.2 graph (QUInt8, per channel, `reduce_range=False`, `WeightSymmetric=True`, `MatMulConstBOnly=False`), then rewrite each `<w>_quantized/_scale/_zero_point` with the closed form; C-R then compares initializers byte for byte. |
| M4 | **Holds, with two caveats.** `Variant.model_dir` (core/src/cli/embed_eval/config.rs:177, `model_dir()` :305) takes `~/`, absolute or eval-dir-relative paths and must hold `model.onnx` + `tokenizer.json` (+ `model.onnx_data`); `hf_repo`/`revision`/`onnx_file` are optional (`validate` :230 checks only pooling/dimension/max_tokens via `encoder_spec`). `fetch_models.py` skips any row with `model_dir` ("uses …, not fetched", :98). `run` never downloads; it records sha256 + size of the model files in the manifest (`model_files`, embed_eval.rs:804) but **checks no pin**: a model row has no sha256 field (only adapters have `adapter_sha256`), and run reuse (embed_eval.rs:1007) compares the variant fingerprint, not `modelFiles`. **[EDIT MAP]** `register_arms.py`: give each exported model its own row name and a `model_dir` whose path contains the file sha256 (`…/<arm>@<sha16>/`), verify the sha256 itself before `run`, and check the run manifest's `modelFiles` after it; no core change. |
| M5 | **Accepted structurally; a full run STOPs.** `--st-runs` takes an absolute dir (gm463_bakeoff.py:624); `add_structured` (gm462_ce_enriched.py:129) checks only snapshot / node-id / query-id keys, which the adapter runs share with fp32 (checked on gin: all four equal; 100 hits with scores per query, all 6 corpora). But the adapter's top-30 windows hold nodes int8's do not, so the ce-minilm reference has missing pairs, which requires `ce-minilm` in `--models` (:663), and then `c0_check` (:817, K=30) STOPs: it requires GM-698's rows and 0 scored reference pairs. Also, the GM-698 fp32 L6 score caches (`gm462-ce-minilm.*`) are **not on this laptop** (find under `~/Projects/ClaudeProjects`, depth 7: only `rerank_cache_gm700_rust/gm463-ce-minilm-quint8*`), so even the default recall stage fails C0 until those pairs are rescored. **[EDIT MAP]** `gm463_bakeoff.py`: run C0 only when `--st-runs` is the default int8 dir, label the recall arm by its dir name; the measure slice budgets a CPU rescore of the L6 reference pairs (or restores the GM-698 cache). |
| M6 | **Holds: RR trains on ship text.** All 24 GM-456 checkouts (`g-mesh-data/gm456/work/corpora/<repo>`) are at their snapshot's `revision`, clean (`git rev-parse HEAD`, `status --porcelain`). `gm462_ce_text.build(row, src, "EPB", L6 counter, 48)` over every embeddable node of the 24 repos: 37,307 nodes, 0 missing source files; on the 21,994 nodes that GM-456's `texts/ce-epb48@a1509bb47335` render holds, **21,994/21,994 identical**. No train/eval text mismatch. |
| M7 | Tokens with special tokens, no truncation. Queries (23,798): jina p99 27 / max 35, gte p99 30 / max 38 -> **max_len 64 for queries holds**. Structured node texts (37,307 embeddable nodes): jina p50 15, p95 72, p99 152, max 3,811 (1.4% > 128, 0.4% > 256); gte p99 174 (1.8% > 128, 0.5% > 256). **[PARAM]** route J/R3 docs `max_len 128` (section 1.2) truncates ~1.5-2% of node texts; 256 covers 99.5%; recommend 256 with length-bucketed batches (padding is per batch, so the cost is in the tail only). L6 ship pairs (query + ship text) over the int8 top-30 windows: 713,940 pairs (618,000 train, 95,940 dev), p50 72, p95 132, p99 207, max 5,429 (0.6% > 256) -> **RR max_len 256 holds**. |
| M8 | GM-456 dev repos (6): burntsushi-toml 490, groupcache 237, http 1,249, ky 212, mini-redis 283, more-itertools 1,380 embeddable nodes = **3,851 dev nodes**, 3,198 dev queries (train: 18 repos, 33,456 nodes, 20,600 queries); counts equal the int8 runs' `nodeCount`. **[BUDGET]** G9's 20-30k was 5-8x high: C-P2's int8 dev pass on 4 vCPU is a few minutes, so g5.2xlarge for J-B is not needed. |
| M9 | `config.json` at `516f4baf…` maps `auto_map` to `jinaai/jina-bert-v2-qk-post-norm--{configuration,modeling}_bert.*` with **no revision** (transformers takes that repo's `main`). Pin `main` = `3baf9e3ac750e76e8edd3019170176884695fb94` (2024-05-07); `modeling_bert.py` sha256 `8f96ef0576401f6b996f9b72ad34e13190139eec1195767516485876434bb612`, `configuration_bert.py` `d018de11bcb62cedf436c8876459e5cd119d1faf72bc107338576a6d1b8eeea5`. Linear modules (73): `query`, `key`, `value`, `dense` (attention output and the unused pooler), `up_gated_layer` (GEGLU, 768 -> 6144), `down_layer`; peft LoRA r=16 on those six names: 3,268,608 trainable, `merge_and_unload` gives back `JinaBertModel`. Offline load (`HF_HUB_OFFLINE=1`, empty `HF_HOME`): as published **fails** (`OSError: couldn't connect … jinaai/jina-bert-v2-qk-post-norm`); with the two code files copied into the snapshot and `auto_map` rewritten to `configuration_bert.JinaBertConfig` etc., it **loads** (160,869,120 params). Smoke check: mean-pooled torch fp32 vs official fp32 ONNX on 2 texts, max abs diff 3.8e-6 (M3 proper is S-later). Note: `model.safetensors` is fp16 (0.32 GB, `torch_dtype float16`); trainers load with `torch_dtype=float32`. **[EDIT MAP]** `snapshots.py`: copy the pinned code files into the jina snapshot and rewrite `auto_map` (record both sha256s); jina's `[[data]]` set is ~0.33 GB (safetensors + tokenizer + code), not ~0.6 GB, plus 0.64 GB if fp32 `onnx/model.onnx` is shipped for C-R on the instance. |
| M10 | **One lock serves both**: transformers 4.48.3 + torch 2.2.2 loads jina's custom code offline (as M9) and `ModernBertForSequenceClassification` for gte-mb `f7481e60…` (sdpa attention on CPU; logits 2.30 related vs 0.44 unrelated pair). Lock pins: transformers 4.48.3, tokenizers 0.21.4, peft 0.21.3, onnx 1.16.2, onnxruntime 1.19.2, numpy 1.26.4 (linux cu121 still to be resolved by the code slice). No second lock. |

## 10. Decisions for the owner

**Owner answers, 2026-11-19** (option labels verbatim):

- D1: "RR → J → R3 (Recommended)"
- D2: "$20, жёсткий стоп $25 (Recommended)"
- D8: "MRR > 0, r@10 ≥ −1 пт (Recommended)"
- D3-D7: "Да, все рекомендованные (Recommended)" (D3a, D4a, D5a, D6a, D7a)

Status: approved.

### D1. Route order

**Today:** three GPU routes are queued with no order; only one instance can
run at a time. **Change:** fix the order. **Example:** RR first costs ~$1
and produces gte-mb teacher scores that J and R3 reuse; J first answers the
"beat the adapter" question soonest. **Consequence:** the order decides
which routes still run if the money or a stop rule ends the batch early.

| option | benefit | risk |
|---|---|---|
| **a. RR -> J -> R3 (recommended)** | Cheapest and strongest-prior route first; its teacher feeds J and R3 | The direct adapter comparison (J) waits ~1 day |
| b. J -> RR -> R3 | Adapter question answered first | J runs without the gte-mb teacher (α = 0) or waits anyway |
| c. J only, others later | Smallest batch | Leaves the route with the best evidence (RR) unanswered |

### D2. Money cap for GM-730

**Today:** the only limit is the $50/month kill switch (~$48.7 left), which
lags by hours. **Change:** a GM-730 cap enforced by job lifetimes and
`cost.json` sums. **Example:** cap $20 = ~28 instance-hours at ~$0.70;
estimates total ~6 h. **Consequence:** the cap bounds the worst case of
wrong throughput guesses and spot relaunches.

| option | benefit | risk |
|---|---|---|
| **a. $20 cap, hard stop $25, per-route caps RR $4 / J $8 / R3 $3 + $5 reserve (recommended)** | 3-4x the estimates; leaves > $23 for later work this month | A slow jina (5x the guess) stops at its cap with partial results |
| b. $35 cap | Room for a second jina grid | Less left for other GPU work this month |
| c. $10 cap | Minimal spend | J likely capped before job B if throughput is 2x worse |

### D3. Jina: LoRA, full, or both

**Today:** the task lists "jina LoRA or full fine-tune". **Change:** decide
which reach v2. **Example:** both in one dev grid (4 + 4 configs), only the
dev winner exported and evaluated. **Consequence:** each extra jina arm on
v2 costs 15-40 min laptop CPU, an export and a quantisation, and adds a
multiple comparison.

| option | benefit | risk |
|---|---|---|
| **a. Both in the dev grid, only the dev winner to v2 (recommended)** | One gated arm; the form is chosen on dev, as GM-436 did | The loser's v2 number is unknown |
| b. Both to v2 | Both numbers known | 2x export/quant/eval; two gated arms (multiple comparisons) |
| c. LoRA only | Small checkpoints (G4 gone), cheaper | Full fine-tune may be the larger gain |

### D4. Reranker student

**Today:** MiniLM-L6 ships (quint8); L12 failed rule 5 (2.33x) and
jina-turbo failed rules 1, 2, 4 off the shelf. **Change:** choose what the
gte-mb teacher distils into. **Example:** a distilled L6 is a file swap in
`rerank.rs` (same inputs, tokenizer, quint8 path). **Consequence:** any
other student needs a product change in `rerank.rs` to ship.

| option | benefit | risk |
|---|---|---|
| **a. MiniLM-L6 (recommended)** | Rule 5 ~1.0x by construction; drop-in | Smallest capacity; may capture little of +0.047 |
| b. jina-reranker-v1-turbo (1.16x) | More capacity, 8k context | No `token_type_ids`: `rerank.rs` change; failed name-query rule off the shelf |
| c. L6 and turbo both | Capacity question answered | ~2x RR cost and laptop scoring |

### D5. Teacher for the encoder routes (J, R3)

**Today:** GM-436 distilled gte from cached jina int8 scores. **Change:**
pick the teacher signal added to the gold InfoNCE. **Example:** gte-mb
scores over the int8 top-30 windows come free from RR-T; gte-mb is +0.13
MRR over int8 on v2 held-out NL. **Consequence:** with jina as teacher, the
jina route learns from itself, and R3 is capped near jina.

| option | benefit | risk |
|---|---|---|
| **a. gte-mb window scores from RR-T, α in the grid {0, 0.5} (recommended)** | Strongest teacher; α = 0 arm keeps gold-only as a fallback in the grid | Couples J/R3 to RR-T (if C-T fails, α = 0 only) |
| b. Jina int8 cache (GM-436 R2) for R3, gold-only for J | Continuity with GM-436 | Weak signal for J; R3 bounded by its teacher |
| c. Gold only everywhere | Simplest, smallest grids | Ignores the best available signal |

### D6. Reranker eval on the adapter's recall stage (2x2)

**Today:** GM-463 reranks int8-structured's top 30. If the adapter ships,
the reranker sees the adapter's top 30 instead. **Change:** also score L6
and the student on the adapter's windows. **Example:** arms (int8 + L6),
(int8 + student), (adapter + L6), (adapter + student). **Consequence:**
without it, "ship both" cannot be recommended from data.

| option | benefit | risk |
|---|---|---|
| **a. 2x2, gated on int8 recall (GM-463 rules), adapter rows to decide "both" (recommended)** | Answers whether the layers stack; no GPU cost | ~2 x 103k extra pairs (~1 h laptop); M5 may need a small harness change |
| b. Int8 recall only | Unchanged harness | "Adapter + student" untested |

### D7. Multi-stage jobs vs launcher gap G1

**Today:** the runner restores only the current step's checkpoint; an
earlier step's outputs are lost if a later step is relaunched on a new
instance. **Change:** decide how stages (teacher -> grid -> final ->
export) pass outputs. **Example:** RR-T's 3 MB teacher file pulled and
re-uploaded as `[[data]]` for RR-A. **Consequence:** a runner change needs
its own tests and a GM-457-style resume check before use.

| option | benefit | risk |
|---|---|---|
| **a. One job per stage, outputs via `[[data]]`; export inside the training step (recommended)** | No launcher change; every stage is a GM-457-tested path | ~3-5 min boot overhead per extra job (~$0.05 each); more manual hops |
| b. Add `needs = [...]` to steps and restore done steps' `out/` (+ optional `--max-usd`) | One job per route; cap enforced by the tool | Runner change in a merged, tested component; needs its own resume test with AWS spend |

### D8. "Beat the adapter": the comparison rule

**Today:** GM-436's adapter has no paired CI against other routes; D9 only
compares with jina fp32. **Change:** pre-register when a GPU route is
recommended over the adapter. **Example:** `report --reference <adapter>`
gives Δr@10 / ΔMRR with the D5 bootstrap. **Consequence:** the rule decides
between "ship adapter", "ship a GPU route" and "stop".

| option | benefit | risk |
|---|---|---|
| **a. D9 quality gates vs fp32 plus, vs the adapter, ΔMRR lower > 0 and Δr@10 lower >= -1.0 pt (recommended)** | Same shape as GM-463 rules 1-2; tolerates r@10 noise | A route with a small r@10 loss and MRR gain ships |
| b. Both lower bounds > 0 vs the adapter | Strict: better on both | Needs a large gain; may reject a real MRR win |
| c. Point estimates better + D9 pass | Easy to meet | Noise can decide; not CI-based as the AC asks |

## 11. Numbers for the history doc (orchestrator)

- Remaining before GM-730: ~$48.7 of $50 (GM-457 ~$1.3: $0.99 `cost.json`
  + ~$0.30 FIS + ~$0.02 kill-switch test); M14 confirms.
- GM-730 estimate: ~5.8 instance-hours, ~$4.3; caps RR $4 / J $8 / R3 $3
  + $5 reserve = $20, hard stop $25 (D2a).
- Recorded per job by the measure slice: run id, attempts, instance-hours,
  `cost.json` $, probe s/step vs estimate, dev winner + config sha256,
  control outcomes, v2 / GM-463 deltas vs the adapter or ship-L6.

## 12. Code facts relied on (and how they were found)

- `dev_metric.paired_bootstrap` (eval/training/gm436/dev_metric.py:107):
  g-mesh `search_code` ("paired bootstrap confidence interval…"), project
  g-mesh-wt-gm730; `cli::embed_eval::split_deltas` came from the same call.
- `ReportArgs::reference` and its use in `report` (core/src/cli/embed_eval.rs
  :158, :1585): grep in that one file.
- `Runner.run`, `restore_checkpoint`, `upload_checkpoint`, `upload_out`,
  `fetch_inputs`, `step_cmd` (eval/gpu/runner.py): outline by grep, read by
  line range. `data_manifest`, `sync_data`, `pull` (eval/gpu/gpu_train.py):
  same.
- `RERANK_ONNX_REMOTE_PATH` = `onnx/model_quint8_avx2.onnx`
  (core/src/embedding/rerank.rs:44): grep.
- `CES`, `CE_BAKEOFF`, `INT8_MODELS`, `BETAS` (eval/embedding/rerank_eval.py),
  `MODELS`, `INPUTS` table (gm463_bakeoff.py): grep.
- Lock contents (torch 2.2.2, transformers 4.46.3, no onnx/peft):
  `eval/training/gm436/requirements-linux-cu121.txt`, grep.
