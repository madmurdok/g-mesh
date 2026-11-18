# GM-436: laptop-CPU training routes (design, release 4.29.0)

Design slice GM-436/S1. No production code, no training runs. Scope of this
batch: the query-side linear adapter (gte-small and jina), jina -> gte-small
distillation in adapter form, the learning curve, and a decision on
hierarchical retrieval, TSDAE and CodeRankEmbed. Production does not change
in this task.

**Deferred to GPU (GM-457), stays in GM-436:** reranker distillation from
gte-reranker-modernbert-base (task section 10), LoRA and full fine-tuning of
jina. Owner, 2026-11-13: "Оставить в GM-436". Nothing below designs them.

## Owner decisions (2026-11-13)

Quoted verbatim from the owner's answers:

- D1: "Только int8, поправить AC (Recommended)". The jina arm is int8-structured only; the AC is amended.
- D2: "jina int8 (Recommended)".
- D3: "Rust-арм adapter (Recommended)".
- D4: "Rust-харнесс GM-456 (Recommended)".
- D5: "Отложить до GPU (GM-457)". No full gte-small student fine-tune in this batch; R3 waits for GM-457 with the other GPU routes. R1 and R2 (adapter form) only.
- D6: "Диагностика запаса сейчас (Recommended)".
- D7/D8: "Записать «не пробовали» + задача на CodeRankEmbed (Recommended)". CodeRankEmbed becomes its own backlog bake-off task.
- D9: "Сетка на dev GM-456, sha256 до eval (Recommended)".

## 0. What exists today (facts this design builds on)

- **Shipped model and text.** Production embeds with jina-v2-base-code
  **int8**, not fp32 (ADR 0011, accepted 2026-09-29). It embeds the
  `structured` text form (ADR 0012). The harness constant is
  `PRODUCTION_VARIANT = "jina-v2-base-code-int8-structured"`
  (`core/src/cli/embed_eval.rs:164`). The task text "production stays on
  jina fp32" predates ADR 0011. D9's reference row R is still the fp32 run.
- **Training data.** GM-456 `v0.4-full` has 23,798 queries over 3,990
  targets: train 20,600, dev 3,198. The split is by repo. The dev repos are
  burntsushi-toml and groupcache (go), http and mini-redis (rust), ky (ts),
  and more-itertools (python). Each query has its `positives` (the target
  plus the judge's `answers`; 3,726 queries have more than one), its
  `answersAlso`, and 0-12 judged hard negatives (median 10). The frame is
  21,994 nodes. Structured texts are in `texts/embed-structured@70a41666b656/`,
  with a mean of 98 characters. Queries average 73 characters.
- **Stored vectors to reuse.**
  - **Training repos, jina int8-structured: complete.**
    `g-mesh-data/gm456/work/runs/jina-v2-base-code-int8-structured/<repo>/`
    holds `vectors.bin` for 37,307 nodes (every embeddable node, tests
    included) and `query_vectors.bin` for all 23,946 mined queries, a
    superset of the 23,798 kept ones. The query-file sha256 in each run
    manifest matches `v0.4-full/mine/queries/<repo>.jsonl`, valibot's v0.4
    regeneration included. Checked in this slice for attrs and valibot.
  - **Eval v2 (GM-460):** `g-mesh-eval-data/v2/work/runs/` has
    `jina-v2-base-code-int8-structured`, `jina-v2-base-code-fp32`, `-int8`,
    `gte-small`, `random` and `shuffled`. 15,625 nodes and 3,431 queries
    over the 6 corpora. **gte-small and fp32 there are full-form**
    (`tokenShareOver512` > 0), not structured. No words-shuffled arm exists
    on v2.
  - Nothing exists for gte-small-structured or fp32-structured, on either
    side.
- **Confirm set.** v2 contains GM-422's confirm queries (`-cNNN` ids), and
  they follow the D6 split. `gm460_subset.py --eval-dir eval/embedding/confirm`
  cuts them out of any v2 run, and likewise the GM-398 set.
- **Harness.** Every metric script (`report`, `recall_at_k.py`,
  `shipped_floor_rates.py`, `gm697_floor_refit.py`, `gm460_subset.py`) reads
  only a run dir's `manifest.json` + `rankings.jsonl`. `report` refits D6
  floors per arm from that arm's own outcomes (`floors_and_rates`,
  embed_eval.rs:1451). So any arm that writes harness-format rankings gets
  D9, the per-model floor refit and the GM-434 columns unchanged.
- **Machine.** Intel i7-1068NG7: **4 physical cores**, 8 logical, 32 GB RAM,
  ~44 GB free disk. The probe is in section 7.

## 1. Decisions for the owner

Each decision has a recommended option, listed first.

### D1. Which jina is the adapter's jina arm trained on?

**Today:** the AC says "adapter for gte-small and for jina fp32". But the
product ships jina int8 (ADR 0011). The int8-structured vectors already
exist for both the training repos and eval v2. **Change:** pick the jina
base(s). **Example:** an adapter learned on int8 vectors is the one that
could ship; one learned on fp32 vectors describes a model the product no
longer runs. **Consequence:** fp32 adds one full embedding pass over the
training repos and the eval, plus two more arm families.

| option | benefit | risk |
|---|---|---|
| **a. int8-structured only; amend the AC's "jina fp32" to "shipped jina (int8)" (recommended)** | Zero embedding cost (vectors exist). The result applies directly to the product | Not literally the AC; needs an AC amendment |
| b. int8 and fp32-structured | Satisfies the AC as written; shows whether quantisation limits the adapter | ~1-3 h background CPU for training and eval vectors, +0.3 GB, about 8 more eval arms |
| c. fp32-structured only | Literal AC | Trains on a model that does not ship; still pays b's cost |

### D2. Teacher for the jina -> gte-small distillation

**Today:** the task names "jina fp32 (the teacher)". The teacher is a
bi-encoder, so its score for (query, code) is the cosine of two cached
vectors. **Change:** choose which jina's vectors give the teacher scores.
**Example:** with int8, teacher scores for 23.8k queries x ~60 candidates
are one numpy pass over vectors that already exist (seconds). **Consequence:**
fp32 as the teacher requires D1b's fp32 vectors.

| option | benefit | risk |
|---|---|---|
| **a. jina int8-structured (recommended)** | Free; it is the model the student must match in the product (int8 r@10 0.613 vs fp32 0.608 on v2) | Distils quantisation noise too (small: GM-422 Q1/Q2 equivalence) |
| b. jina fp32-structured | Literal task text | Only with D1b; no measured quality advantage on v2 |

### D3. Where the eval applies the adapter

**Today:** `debug-embed-eval run` embeds queries, scores cosine against node
vectors and writes `rankings.jsonl` (`run_corpus`, embed_eval.rs:854-1111).
A `shuffled` arm already reuses another arm's vectors through
`reference_run` (embed_eval.rs:1113). **Change:** add a way to score
"query vector x W". **Example:** variant `gte-small-structured-ad-full`,
`arm = "adapter"`, `reference = "gte-small-structured"`,
`adapter = "adapters/gte-full-100.f32"`, `adapter_sha256 = "..."`. It reads
the reference's `vectors.bin` and `query_vectors.bin`, multiplies each query
by W, and ranks with the same `top_hits` and labels. **Consequence:** every
report and script runs unchanged on the new arm, and the same Rust
`QueryAdapter` is the later production hook.

| option | benefit | risk |
|---|---|---|
| **a. New Rust arm `adapter` in `debug-embed-eval` + `core/src/embedding/adapter.rs` (recommended)** | Unchanged harness downstream; production-identical f32 math; identity control is byte-exact; one adapter format shared with a future production task | Rust work in a research batch (one arm, ~150 lines + tests); the new Variant fields must not change stored fingerprints (pinned test) |
| b. Python writes `rankings.jsonl` from stored vectors | No Rust | Must re-implement `top_hits` tie order, f32 cosine rounding, `top_language`; and `load_arm` needs a declared variant anyway. A silent mismatch biases D9 |

### D4. Who computes training vectors (gte-small, and fp32 if D1b)

**Today:** GM-456 mined with `debug-embed-eval run` over an eval-dir layout
of the training repos (`v0.4-full/mine/`). That run is resumable per repo
and produced the int8 vectors above. **Change:** add `gte-small-structured`
(and `jina-v2-base-code-fp32-structured` if D1b) to
`eval/training/variants.toml` and run the same command. **Consequence:**
training vectors come from the exact ONNX, tokenizer, pooling and text the
eval and product use.

| option | benefit | risk |
|---|---|---|
| **a. Rust harness over the GM-456 mine dir (recommended)** | Byte-identical to eval/product vectors; already resumable; no onnxruntime in Python | Needs the mine dir's symlinks re-pointed (they point into `g-mesh-wt-gm456`, see Risks) |
| b. Python (onnxruntime or torch) encoder | One language for training | Tokenizer/pooling/truncation parity risk; an adapter trained on slightly different vectors silently underperforms |

### D5. Full fine-tuning of the gte-small student in this batch

**Today:** the task allows a full student fine-tune "only if the CPU
estimate fits in nights". **Change:** decide whether it runs now.
**Example:** the probe (section 7) measured 1.1-3.1 s per training query
in the foreground with 4 threads, and 7.8-26 s under `taskpolicy -b` with 2
threads. That is 6.5-18 h per epoch in the foreground.
1 epoch over 20.6k train queries, the learning-curve points (25/50/100%,
1.75 epochs in all) and the shuffled-teacher control (1 more epoch) add up
to ~2.75 epochs. **Consequence:** it also needs an ONNX export with a
PyTorch-vs-ONNX parity control, and a full re-embed of eval and dev with
the student (gte cost, minutes).

| option | benefit | risk |
|---|---|---|
| **a. Gated: run only if the best gte adapter leaves D9 Q1/Q2 failing, and the distilled adapter's dev MRR gain over base gte is >= +0.02 with its CI excluding 0 (headroom shown); then as its own measure slice (recommended)** | Spends nights only where the cheap route showed signal (task's own order: "only if the adapter shows headroom") | One more slice later; batch may end without the student |
| b. Run now in parallel with the adapter | Answer sooner | 3-7 foreground nights (section 7) on a route with no evidence yet; it cannot run at background priority (weeks), so it blocks the machine at night |
| c. Defer to GPU (GM-457) | No laptop nights | GPU quota still pending (2026-11-13); gte-small is CPU-feasible |

### D6. Hierarchical retrieval (task scenario 9)

**Today:** not measured. It is untrained: the file stage would rank files by
coarse vectors (centroids of node vectors) or the graph, then rank symbols
within the top files. **Change:** decide whether this batch tries it.
**Example:** a headroom diagnostic over existing `rankings.jsonl` +
snapshots. Among misses (expected node not in the top 10), what share has
the expected node's file among the files of the top-10 hits? Among hits,
what share would a file stage drop? It costs seconds. **Consequence:** a
full arm needs its own scoring code (a two-stage rank is not "query x W").

| option | benefit | risk |
|---|---|---|
| **a. Headroom diagnostic now (int8-structured and gte-structured, v2); full arm only if a file-first oracle gains >= 5 points r@10 (recommended)** | Seconds of CPU; turns "maybe" into a number before writing a ranker | The oracle is an upper bound, not a result |
| b. Full arm now (centroid file stage, F=5 files) | A measured answer | New ranking code + its controls in a training batch |
| c. Record not tried: "independent of training; GM-444 owns retrieval fusion" | No work | AC is silent on it; the question stays open |

### D7. TSDAE

**Today:** listed as "likely skip" in the task. **Recommendation:**
**record as not tried.** Reasons: it tunes the whole encoder, so it has the
student route's CPU cost (section 7) plus an ONNX export and parity. Its
typical gains are small next to supervised pairs, and this batch has 20.6k
judged supervised pairs plus hard negatives (GPL-style data beats TSDAE in
the literature the task cites). jina is already code-trained.

| option | benefit | risk |
|---|---|---|
| **a. Record not tried, with the reasons above (recommended)** | No CPU nights for an unsupervised baseline | The AC closes it by argument, not data |
| b. Try on gte-small (1 epoch of denoising over the 37k training nodes) | A data point | Nights of CPU + export/parity, for a route expected to lose to the supervised student |

### D8. CodeRankEmbed

**Today:** not tried in GM-398 (no published ONNX). It is a 137M-parameter
NomicBERT model (768-d, `trust_remote_code`), about jina's cost, so it is a
**quality** base, not a cost one. **Recommendation:** **record as not
tried in this batch.** It is a base-model bake-off (GM-398 D8 candidate):
ONNX export + parity + a full eval embedding at about jina cost. It does
not depend on training, and its custom architecture is a risk for the Rust
`EmbeddingModel` loader.

| option | benefit | risk |
|---|---|---|
| **a. Record not tried; propose a separate bake-off task (recommended)** | Keeps this batch about training | The question stays open |
| b. Export + parity + v2 eval now | Answers whether a better base exists | ~1-2 h export/parity work + ~1 h eval CPU; NomicBERT rotary/ONNX opset issues are likely |

### D9. Hyperparameter selection

**Today:** GM-398 forbids tuning on the eval; the confirm set is a pure
test set. **Change:** fix how arms are tuned. **Example:** τ, learning rate,
λ, rank r and epochs are chosen on the GM-456 dev split only. The chosen
config file's sha256 is written into the results doc **before** any v2 run.

| option | benefit | risk |
|---|---|---|
| **a. Small pre-registered grid on GM-456 dev, frozen by sha256 before eval (recommended)** | No eval leakage; reproducible | Dev (6 repos) may pick a config that is not optimal on eval corpora |
| b. Fixed literature defaults, no tuning | Simplest | Under-tuned adapter can read as "no headroom" |

## 2. Python environment

Created in this slice for the timing probe. It lives outside the repo:
`~/Projects/ClaudeProjects/g-mesh-data/gm436/venv` (925 MB).

| package | pin | why |
|---|---|---|
| CPython | 3.11.15 (uv-managed) | System python is 3.9.6; torch 2.2.2 has cp311 macOS-x86_64 wheels |
| torch | 2.2.2 | **Last release with macOS x86_64 wheels**; an unpinned resolve also lands here |
| numpy | 1.26.4 (`<2`) | torch 2.2.2 is built against NumPy 1.x; the unpinned resolve picks numpy 2.4.6 (incompatible) |
| transformers | 4.46.3 | Imports cleanly with torch 2.2.2 (checked). The unpinned resolve picks 5.23.0, whose torch floor is unverified |
| sentence-transformers | 3.3.1 | Only for the student route (D5); imports cleanly |
| onnx, onnxruntime | pin at the student slice | Only for the student's ONNX export + parity; not needed for adapters |

- Recreate with `uv venv --python 3.11 venv && uv pip install -r eval/training/gm436/requirements.txt`.
  The committed requirements file is generated with
  `uv pip compile --python-platform x86_64-apple-darwin --generate-hashes`.
- The adapter route needs only torch + numpy. It touches no HF model.

## 3. Cache design

Root `$GM436_DATA = ~/Projects/ClaudeProjects/g-mesh-data/gm436/`, outside
git (GM-456 D5a convention).

**Encoder passes are Rust runs** (D4a). Each (variant, repo) run dir is
already a resumable unit: `run_corpus` reuses `vectors.bin` and
`query_vectors.bin` when the manifest's fingerprint, snapshot, node ids,
dimension, EP and query files match. Python never re-embeds. It **imports**
runs into a keyed table:

```
$GM436_DATA/cache/vec/<modelKey>/<side>/
    index.jsonl        {key, textSha256, chunk, row}  (one row per node or query)
    chunk-00000.npy    float32 [<=4096, d], raw harness vectors (not normalised)
    manifest.json      {sourceRuns: [{dir, manifestSha256}], renderId, chunks: [{file, rows, sha256}]}
```

- `modelKey = <variant>@<sha256(model.onnx)[:12]>`, read from the run
  manifest's `modelFiles`.
- `side` is `node@<renderId>` (`embed-structured@70a41666b656`) or
  `query@<sha256(query_prefix)[:12]>`.
- Node `key` = GM-456 `<repo>:<filePath>#<qualifiedName>@<startLine>`,
  joined from the run's `node_ids.txt` through the repo's snapshot sqlite
  (`mine.py::snapshot_maps`). `textSha256` is the sha256 of the GM-456
  rendered text for frame nodes, and of the snapshot-rendered text for the
  non-frame nodes.
- Query `key` = query id; `textSha256` = sha256(prefix + text).
- **Lookup is by (key, textSha256).** A re-render or a query edit misses the
  cache instead of serving a stale vector.
- **Resumable:** each chunk is written to `*.tmp`, fsynced, then renamed. On
  restart, chunks listed in `manifest.json` with a matching sha256 are
  skipped. The import is seconds, but the same layer serves the student
  route, whose Python encoder passes are long.

**Teacher scores** (distillation, D2a):

```
$GM436_DATA/cache/teacher/<teacherModelKey>/<renderId>/<candSetId>/
    chunk-00000.npz    qids[int32], cand[int32, n, K], score[float32, n, K]   (4096 queries per chunk)
```

- `candSetId = sha256(rule + K + GM-456 version)[:12]`.
- Rule: GM-456 positives + negatives ∪ teacher top-K=50 over all nodes of
  the query's repo. `cand` indexes the node index.
- Same tmp+rename resume.

**Disk estimate** (float32):

| item | size |
|---|---|
| venv | 0.93 GB (exists) |
| gte-small-structured training vectors (37.3k nodes + 23.9k queries, 384-d) | 0.09 GB |
| int8 import copy (768-d) | 0.19 GB (or memory-map the Rust `.bin` files directly: 0) |
| fp32-structured training vectors (D1b only) | 0.19 GB |
| teacher scores (23.8k x ~62 x 8 B) | 0.01 GB |
| eval v2 new base arms: gte-structured, +words-shuffled; int8 words-shuffled; (fp32 pair if D1b) | 0.06 + 0.10 (+0.12) GB |
| adapter arms: rankings + adapted query vectors, ~25-30 MB each, ~30 arms | ~0.9 GB |
| adapter weights + checkpoints | < 0.1 GB (2.4 MB per 768x768 W) |
| student route (only if D5 runs): 3 checkpoints (weights + AdamW) per run x 4 runs, kept until done; ONNX 0.13 GB per run | peak ~5 GB, 1 GB after cleanup |
| **total** | **~2.5 GB without the student, ~7.5 GB peak with it** |

## 4. Trainer design per route

Common to all routes:

- Seed 436 (python `random`, numpy, `torch.manual_seed`), and
  `torch.use_deterministic_algorithms(True)`.
- `torch.set_num_threads(2)` for adapters. Runs go under
  `caffeinate -i -s taskpolicy -b`.
- Inputs: the train split for fitting, dev for selection, early stopping and
  the learning curve.

**Masks.** A "negative" never includes any of the query's `positives` or
`answersAlso`, nor another in-batch query's positive that equals one of
them. The 6 queries of a target share positives. A query with several
positives uses a multi-positive InfoNCE (log-sum-exp over its positives).

### R1. Query adapter, gold labels (gte-small-structured; jina int8-structured; fp32 if D1b)

Encoders are frozen, so a "batch" costs only matmuls. Batch 512 queries, no
gradient accumulation.

The in-batch-does-not-grow problem (handoff) does not arise: the logits
row for query i is

[its positives; its GM-456 hard negatives (<=12); every other in-batch
query's positives and hard negatives (cross-batch hard negatives, ~5.6k
columns)], minus masked columns.

Loss: InfoNCE at τ (grid {0.02, 0.05}).

| form | parametrisation | init | regulariser | fit |
|---|---|---|---|---|
| full | W ∈ R^{d x d}, q' = W q | I | λ‖W - I‖_F², λ ∈ {0, 1e-3, 1e-2} | AdamW lr 1e-3, <= 30 epochs, early stop on dev MRR (patience 3) |
| low-rank | W = I + U Vᵀ, r ∈ {32, 128} | U = 0, V ~ N(0, 0.01) | none | same |
| orthogonal | W = Procrustes(Q, C) = U Vᵀ from SVD(Qᵀ C), Q = train query vectors, C = their target vectors (normalised) | closed form | none | one SVD (seconds); no hyperparameters |

All three are exported as a dense d x d float32 row-major W, so Rust has one
format.

- Checkpoint after every epoch: W (or U, V), AdamW state, epoch, the
  epoch's permutation index, RNG states, best dev MRR.
- Kill-and-resume must give an identical final W.

### R2. Distillation in adapter form (student gte-small-structured, teacher D2a)

- Frozen gte vectors, as in R1.
- For query i over candidate set S_i (section 3):
  - student p_s = softmax(cos(W q_i^gte, c_j^gte)/τ_s);
  - teacher p_t = softmax(cos(q_i^jina, c_j^jina)/τ_t), τ_t = τ_s = 0.05.
- Loss = KL(p_t ‖ p_s) + α · InfoNCE_gold(R1's row), α ∈ {0, 0.5} (α = 0 is
  pure distillation).
- Form: whichever of R1's forms wins on gte dev MRR (fixed before R2 runs).
- Same checkpointing as R1.

### R3. Full gte-small student fine-tune (only if D5 opens it)

- Model: HF `thenlper/gte-small` at revision `17e1f347…` (the ONNX's
  revision), mean pooling, max_len 256 (to be checked against the token
  p99 of the structured texts in that slice).
- Loss: gold InfoNCE with explicit GM-456 hard negatives (7 per query,
  sampled with the seed) + the R2 KL to cached teacher scores (α from R2).
- Micro-batch 8 queries x (1 + 7) docs, so 64 docs per step.
- **No gradient accumulation for the contrastive term.** Accumulation does
  not add in-batch negatives. The explicit hard negatives carry the signal.
  If dev shows a batch-size effect, GradCache is the next step.
- Gradient checkpointing only if RSS exceeds 8 GB (33M params, so it should
  not).
- AdamW lr 2e-5, 5% warmup, linear decay, 1 epoch per learning-curve point.
- Checkpoint every 500 steps: weights, optimizer, scheduler, RNG states, data
  cursor (permutation + offset). Keep the last 2 plus the best.
- Threads 4 (physical cores), **foreground** `nice -n 10` under
  `caffeinate -i -s`, in night windows only. Background QoS is several-fold
  slower (probe).
- Then ONNX export (opset 17, dynamic axes). Parity on 1,000 eval texts
  (PyTorch vs ONNX via Rust `EmbeddingModel`): max cosine distance ≤ 1e-4
  between paired vectors. Then a `model` variant pointing at the exported
  dir.

### Learning curve (AC last bullet)

- Subsets by **target**, nested: one seeded permutation of the 3,457 train
  targets; 25% = its first quarter, 50% = its first half. All queries of a
  target go together. Same seed and the same frozen config at every point.
- Applies to every trained route that is reported: the best R1 form per
  base, and R2.
- **Dev metric** (defined here; GM-456 did not define one):
  - Each dev query is ranked against every node of its dev repo in the
    mining run (the harness's candidate set). A positive or `answersAlso`
    hit counts.
  - Metrics: MRR (primary) and recall@10.
  - CI: 95% percentile bootstrap, **clustered by target**, 10,000 resamples,
    seed 436.
  - The 50%->100% gain is a paired bootstrap over the same dev targets.
- **Rule:** if the 50->100 gain's CI excludes 0, the report recommends more
  GM-456 data. The size is extrapolated from the 25/50/100 points with a
  log-linear fit, labelled as an extrapolation.

## 5. Eval bridge

1. **Base arms on v2** (Rust `run`, new rows appended to
   `eval/embedding/v2/variants.toml`):
   - `gte-small-structured` (model, `text = "structured"`);
   - `gte-small-structured-words-shuffled`;
   - `jina-v2-base-code-int8-structured-words-shuffled`;
   - fp32-structured and its words-shuffled twin only under D1b.

   The int8-structured base exists. A `words-shuffled` arm takes its text
   form from its own row (`candidate_texts(nodes, variant, …)`, embed_eval.rs:721),
   so each words-shuffled row needs `text = "structured"`.
2. **Adapter arms** (D3a): `arm = "adapter"`, `reference` = a base `model`
   or `words-shuffled` arm, plus `adapter` + `adapter_sha256`.
   - Run: like `shuffled`, it resolves the reference via `reference_run`
     (same nodes, queries, EP). It symlinks the reference's `vectors.bin`,
     reads its `query_vectors.bin`, applies W, and writes the adapted
     `query_vectors.bin` and `rankings.jsonl`.
   - No encoder, no `--ep` other than cpu.
   - Scoring: `cosine_similarity` (model.rs:1045) on (W q, c), as today.
3. **Reports per arm, all from the same v2 runs:**
   - `debug-embed-eval report` (D9; D6 floors refit per arm on v2 FIT;
     NL/name splits; strict held-out view; broken-arm validity);
   - `shipped_floor_rates.py --run <arm> --eval-dir eval/embedding/v2 --floors fitted`
     (GM-434 columns: misled %, CW positives/absent);
   - `recall_at_k.py` (r@1..100);
   - `gm460_subset.py` for the **GM-398 subset** (the table that sits next
     to GM-398's) and the **confirm subset** (eval/embedding/confirm),
     reported at the floors refit on v2 FIT;
   - per corpus, with Rust (ripgrep, g-mesh) and the words-shuffled gap
     (r@10 adapted-on-base minus adapted-on-words-shuffled-base) for every
     arm.
4. **Which D9 row:**
   - gte adapters are **cost candidates** (cost = gte pass + one 384x384
     matmul per query, ~0.1 ms). Pass time, RSS and size are gte's, from
     GM-398 D11.
   - jina adapters are **quality candidates**: lower bounds of Δr@10 and
     ΔMRR must be > 0, plus Q3-Q5.
5. **Dev ranking in Python** (training-time only) reads the cache. Its
   identity-adapter output must equal the mine run's `rankings.jsonl` top
   hits (control C0b). This proves the key/vector join.

**Why a Rust hook and not Python over vectors:** see D3. The production
hook is a separate future task. It would be
`search_code::handle` (search_code.rs:319), right after
`embedding.embed_query(&params.query)` at search_code.rs:328, and **not
inside** `EmbeddingPipeline::embed_query` (pipeline.rs:396). The reason:
`embed_query` is also called by `find_definition::resolve_lazily` (:593)
and `resolve_lazily_off_worker` (:611). Those embed a bare symbol name and
judge it with the same `similarity::floor`
(`find_definition::by_semantic_neighbours`, :965). An adapter fitted to NL
queries would shift those scores too.

## 6. Controls

| id | control | pass rule |
|---|---|---|
| C0 | Identity adapter W = I on every base (v2, and the words-shuffled bases) | `rankings.jsonl` byte-identical to the reference run's, all 6 corpora |
| C0b | Python dev ranking with W = I vs the mine run's `rankings.jsonl` (int8) | top-100 ids identical for all dev queries; dry-run with a planted one-row offset in the join, which must fail |
| C1 | Shuffled-pairs adapter: each train query is paired with the positives and negatives of another target (seeded derangement, no fixed points), same config | Does not beat its base: lower bound of Δr@10 ≤ 0 and of ΔMRR ≤ 0 on v2; dev MRR CI overlaps base. A drop below base is reported, not a failure |
| C2 | Shuffled-teacher student: teacher scores permuted within each query's candidate list (multiset kept) | Does not beat base gte-structured (C1's rule) |
| C3 | Broken arms (random, shuffled) | `report` validity passes for R, and every adapted arm beats them by D7's margins |
| C4 | Unseen corpus | All 6 eval corpora are disjoint from training (GM-456 leak guard: clean, recorded in card.md). Plus one **language-held-out** adapter (best gte form, trained without Go targets), reported on gin and Go dev: informational transfer check |
| C5 | Determinism | Retraining the best arm with the same seed and threads gives an identical W sha256 |
| C6 | Overfit | Train vs dev MRR reported per arm |
| C7 | Planted-fault dry runs | The C0 comparator is shown to fail on a rankings file with one swapped pair; the C1/C2 "does not beat" check is shown to fail on a planted +5-point arm |

## 7. Arms for the measure slice, with CPU and disk

**Probe** (this slice). Random-init BERT with gte-small's shape (384-d, 12
layers, 1536 FFN), fwd+bwd+AdamW. One step = 8 queries x 8 docs
(1 pos + 7 hard negatives). Script:
`~/Projects/ClaudeProjects/g-mesh-data/gm436/probe/probe.py` (output `out.txt` beside it). Results:

| run | doc length (tokens) | s/step | s/query | `/usr/bin/time -p` |
|---|---|---|---|---|
| `taskpolicy -b`, 2 threads | 64 | 62.4 | 7.8 | real 1138, user 304, sys 163 (whole script) |
| `taskpolicy -b`, 2 threads | 128 | 207.3 | 25.9 | (same) |
| foreground, 4 threads | 64 | 9.1 | 1.13 | real 156, user 228, sys 43 |
| foreground, 4 threads | 128 | 24.8 | 3.09 | (same) |
| adapter step, 256 q x 12 + in-batch, d=768, 2 / 4 threads | - | 0.19 / 0.14 | - | - |

Machine state: `uptime` load 3.9 at the start and 32.0 at the end (other
agents were running). Under `taskpolicy -b` the run spent **real 1138 s at
user 304 s**: it was *waiting* (background QoS throttling plus contention),
not computing. Foreground with 4 threads, user/real was 1.5.

**Student estimate (R3).** 20.6k train queries per epoch:
- foreground, 4 threads: 6.5 h (64-token docs) to 17.7 h (128-token docs)
  per epoch;
- `taskpolicy -b`, 2 threads: 45-148 h per epoch.

The full D5 programme (~2.75 epochs) is 18-49 h foreground, which is 3-7
nights of ~7 h with the machine otherwise idle. Under background QoS it is
weeks. So the student fits only as **foreground night runs** (caffeinate,
nice 10, 4 threads), not as low-priority background work. This supports D5a.
The doc-length mix (structured mean 98 characters, ~25-40 tokens) puts the
real cost near the 64-token row. The student slice confirms this with the
real tokenizer.

**Encoder passes** (estimates from GM-460/S19 stage timings: gte-small
143 s, int8-structured 667 s and fp32 1,187 s real for the 6 v2 corpora,
19k texts, full threads, load ~5). Structured texts are shorter than full;
`taskpolicy -b` at 2 threads is slower, by a factor not measured:

| run | texts | estimate (labelled) | disk |
|---|---|---|---|
| gte-small-structured, training repos | 61.2k | 10-30 min | 0.09 GB |
| gte-small-structured, v2 | 19.1k | 3-8 min | 0.03 GB |
| gte-structured words-shuffled, v2 (one text per call, unbatched) | 15.6k | 5-15 min | 0.03 GB |
| int8-structured words-shuffled, v2 | 15.6k | 15-40 min | 0.05 GB |
| (D1b) fp32-structured, training repos + v2 + words-shuffled | 96k | 1.5-4 h | 0.31 GB |

**Training and adapter arms.** An adapter step (512 queries x ~5.6k columns,
d = 768) is a few hundred ms; 30 epochs x 40 steps is under 10 min per
config. Scoring an adapter arm on v2 is ~20 s (GM-460: gte arm 21.8 s real,
queries included).

| family | arms (per base: gte; int8; fp32 if D1b) | train CPU | eval CPU |
|---|---|---|---|
| dev grid (D9a), R1 | full 6 configs, low-rank 4, Procrustes 1 | ~1 h per base | none (dev only) |
| R1 final | best of each form at 100% | minutes | 3 arms x 20 s |
| learning curve | best form at 25%, 50% | minutes | 2 x 20 s |
| controls | C0 identity, C1 shuffled pairs, C4 no-Go (gte only), C5 rerun | minutes | 3 x 20 s |
| words-shuffled gap | best form + identity on the words-shuffled base | none | 2 x 20 s |
| R2 distill (gte) | α grid 2 on dev; final at 25/50/100%; C2 shuffled teacher | ~30 min | 4 x 20 s |
| hierarchical headroom (D6a) | int8-structured, gte-structured | seconds | seconds |

- Per base that is about 12 eval arms. With D1a, all adapter arms fit in one
  evening: about 2-3 h of wall time including the base encoder passes.
- Disk ~2.5 GB (section 3).
- **Student route (D5), if opened:** 18-49 h of foreground 4-thread CPU,
  i.e. 3-7 nights (probe above); peak disk ~5 GB.
- **Background QoS:** adapters and Rust encoder passes may run under
  `taskpolicy -b`, but expect a several-fold slowdown (probe: user 304 s of
  real 1138 s). Long Rust passes should run foreground at night under
  `caffeinate -i -s nice -n 10`.

## 8. Edit map

Line numbers are 1-based, at `af833c69`.

**Rust (D3a):**

| file | where | change |
|---|---|---|
| `core/src/embedding/adapter.rs` (new) | - | `QueryAdapter { dim, w: Vec<f32> }`; `load(path, expected_sha256)` reads the raw little-endian f32, row-major d x d file, checks the size and sha256; `apply(&[f32]) -> Vec<f32>` (q' = W q). Unit tests |
| `core/src/embedding/mod.rs` | module list | `pub mod adapter;` |
| `core/src/cli/embed_eval/config.rs` | `enum Arm` 60-71 | add `Adapter` |
| same | `struct Variant` 158-~200 | add `adapter: Option<String>`, `adapter_sha256: Option<String>` **after `filter`**, `#[serde(default)]` |
| same | `validate` 225-257 | `Adapter`: needs `reference` whose arm is `Model` or `WordsShuffled`, plus `adapter` and `adapter_sha256` |
| same | `encoder_of` 261-268 | `Adapter` runs no encoder (with `Random`/`Bm25`) |
| `core/src/cli/embed_eval.rs` | `fingerprint_text` 581-594 | strip `, adapter: None, adapter_sha256: None` when both are None, before the existing strips, so every stored fingerprint is unchanged |
| same | `run_variant` 795-801 | `--ep` non-cpu refused for `Adapter` (no encoder) |
| same | `run_corpus` 882-887 | dimension of `Adapter` = the reference variant's dimension |
| same | `run_corpus` node vectors 941-1010 | `Adapter`: `reference_run`, read its `vectors.bin`, symlink it into the run dir, write `node_ids` as `shuffled` does |
| same | `run_corpus` query vectors 1041-1065 | `Adapter`: read the reference's `query_vectors.bin`, `QueryAdapter::apply` each, write `query_vectors.bin` |
| same | `reference_run` 1113-1131 | unchanged; reused |

**Config rows (appended, never editing existing rows):**

- `eval/embedding/v2/variants.toml`: the base and words-shuffled rows of
  section 5.1, then the adapter rows.
- `eval/training/variants.toml`: `gte-small-structured`; and
  `jina-v2-base-code-fp32-structured` for D1b.

**Python, new dir `eval/training/gm436/`:**

| file | content |
|---|---|
| `requirements.txt` | pins of section 2, with hashes |
| `cache.py` | import Rust runs -> section 3 tables; teacher-score builder |
| `data.py` | GM-456 loader, masks, target-nested subsets, derangements (C1), within-list permutation (C2) |
| `train_adapter.py` | R1 + R2, full / low-rank / Procrustes, resume |
| `dev_metric.py` | dev ranking, MRR/r@10, target-clustered bootstrap, curve rule |
| `export_adapter.py` | writes `W.f32` + sha256 + the variants.toml row |
| `hier_headroom.py` | D6a diagnostic |
| `report_gm436.py` | gathers `report --json`, `shipped_floor_rates`, subsets, per-corpus and words-shuffled columns into the results table |
| `run_measure.sh` | one background, fail-fast script per measure slice |
| `test_gm436_*.py` | stdlib + numpy tests for the behaviours below |

**Results:** `docs/results/gm-436-laptop-adapter.md` (measure slice).

**Read for context:** `run_corpus`, `reference_run`, `load_arm`
(embed_eval.rs:1263), `fingerprint_text`, `config.rs::validate`;
`eval/training/mine.py::snapshot_maps` (113), `mined_candidates` (154);
`common.py::key_of` (55); `gm460_subset.py::main`;
`shipped_floor_rates.py::main`; `search_code::handle` (search_code.rs:319).

### Behaviours the tests must pin

1. An identity adapter's rankings equal the reference run's, byte for byte.
2. A non-identity W is applied as q' = W q, row-major. On a 2-d toy, a W
   that swaps the axes changes the top hit to the hand-computed one. Using
   Wᵀ, or skipping the multiply, must fail this. This test is what makes
   (1) meaningful.
3. A wrong `adapter_sha256` or a wrong-sized file fails the run, naming the
   file.
4. Adding the two Variant fields leaves every existing fingerprint unchanged
   (`stored_runs_keep_their_fingerprints`, embed_eval.rs:2111, still
   passes; plus a new case with `adapter` set changing the fingerprint).
5. An adapter arm refuses a reference that is not `model`/`words-shuffled`,
   and refuses a reference run over other nodes, queries or EP.
6. An adapter arm embeds nothing: no model load, and the reference's node
   vectors are used.
7. Python W export -> Rust load round-trip. A fixture of 3 query vectors and
   Python's W q must match Rust `apply` within 1e-6.
8. Python key<->vector join (C0b) reproduces the mine run's dev rankings.
9. Masks: no positive, `answersAlso` or same-target positive appears as a
   negative.
10. Subsets are nested by target (25 ⊂ 50 ⊂ 100) and keep a target's
    queries together.
11. The C1 derangement has no fixed points; C2 keeps each list's multiset.
12. Kill-at-checkpoint + resume gives the same final W as an uninterrupted
    run.
13. The dev bootstrap clusters by target. On a planted fixture of 6
    identical queries per target, a query-level resample gives a narrower
    CI.
14. The curve rule: a planted 50->100 gain whose CI excludes 0 produces the
    "generate more data" line.

## 9. Must confirm (later slices), and risks

**Must confirm:**

- The owner's answers to D1-D9.
- The GM-456 `mine/` symlinks (`corpora.toml`, `variants.toml`) point into
  `g-mesh-wt-gm456`. If that worktree is cleaned up, re-point them to this
  worktree's `eval/training/` before any training-repo run. Otherwise
  `debug-embed-eval run` fails on a missing `variants.toml`.
- That `debug-embed-eval run` over the mine dir reuses the int8 vectors as-is
  (the manifest matches; the expected output is "0 nodes embedded").
- That reporting the confirm subset at floors refit on v2 FIT is acceptable.
  v2 FIT contains some confirm queries (GM-460 made them follow the D6
  split). The adapter itself never sees them. The alternative is to refit
  floors on FIT minus `-c` ids.
- Before D5: the token p99 of the structured texts under gte's tokenizer
  (max_len 256), and an onnxruntime version with macOS x86_64 wheels for
  the parity step.
- The verify slice runs C0 on all 6 corpora and C7's planted faults.

**Risks:**

- **The adapter cannot fix the encoder's blind spots.** A linear map on the
  query side reshapes the query space only. If jina's Rust weakness is in
  the node vectors (bag-of-words: words-shuffled = unshuffled), the adapter
  may lift NL->code alignment but leave Rust and the words-shuffled gap
  unchanged. That is a valid "stop or go to fine-tuning" signal, not a
  failure.
- **Train/eval shift.** GM-456 queries are LLM-written and cleaner than eval
  or agent queries (card: `overlap = false` share 54-66%). An adapter can
  overfit that style; C4 and the per-corpus columns show it.
- **Floor coupling.** An adapter shifts the score distribution. D6 refits
  floors per arm in the eval, but a production adapter would need its own
  `similarity::floor` table. That table must stay separate from
  `find_definition`'s name queries, which do not go through the adapter
  (section 5). The F4 reranker blend (logit + BETA·cosine) would need BETA
  re-tuned. All of this is the production task's problem; recorded here so
  the recommendation does not understate it.
- **Multiple comparisons.** About 12 arms per base on v2. Only the
  pre-registered final arm per form is a D9 candidate. Grid configs are
  never run on v2.
- **Disk.** ~2.5 GB, up to ~7.5 GB with the student, against 44 GB free and
  other worktrees' `target/` dirs. The measure script checks `df` before
  each encoder pass and stops below 10 GB free.
- **Mac sleep and background QoS.** Without `caffeinate -i -s` the runs
  mostly sleep. Background QoS slows CPU work (probe in section 7).
  Checkpoints make both survivable.
- **torch 2.2.2 is frozen.** It is the last x86_64 macOS build, so there are
  no fixes after it. The adapter route does not care. The student route
  might hit an ONNX export bug, with no newer torch to move to.

## 10. Code facts relied on (and how they were found)

| fact | how found |
|---|---|
| `run_corpus` is called only by `run_variant`; `cli::embed_eval::run` only by `cli::dispatch` (core/src/cli/mod.rs:365) | g-mesh `find_callers` on `cli::embed_eval::run_corpus` and `cli::embed_eval::run`. The second had `unlinkedUsages`: 2 in `plugins/typescript/tests/semantic_runs_no_project_code.rs`, a test of a different `run` (name match), not this path |
| `reference_run` is called only by `run_corpus` | g-mesh `find_callers` on `cli::embed_eval::reference_run` |
| `Arm` is referenced in embed_eval.rs, config.rs, cost.rs, probe.rs, schedule.rs; the latter three only test `!= Arm::Model`, so no exhaustive match needs a new branch there | g-mesh `find_references` (answer=files) on `cli::embed_eval::config::Arm`, then grep `Arm::` in those 3 files |
| Search-query embedding: `EmbeddingPipeline::embed_query` (core/src/embedding/pipeline.rs:396), called by `search_code::handle` (search_code.rs:319, call at :328), `find_definition::resolve_lazily` (:593), `resolve_lazily_off_worker` (:611), and 2 integration tests | g-mesh `search_code` ("embed the search_code query text…") -> `find_definition` + `find_callers` on its id; grep for the call lines |
| The per-language floors `similarity::floor` (similarity.rs:290) are referenced by `below_floor` (:350), `find_definition::by_semantic_neighbours` (find_definition.rs:965), `embed_eval::floor_parity` (embed_eval.rs:1425), two tests, and `core/src/mcp/mod.rs`. `shipped_floor_rates.py::structured_floors` parses the function's source text (not code-tracked) | g-mesh `get_file_outline` on similarity.rs, then `find_references` on `mcp::similarity::floor`; the Python parse read from the script |
| `run_corpus` reuses `vectors.bin`/`query_vectors.bin` on a manifest match, and `load_arm` reads only manifest + rankings | read `run_corpus` (embed_eval.rs:854-1111) and `load_arm` (:1263-1329) by symbol |
| A new Variant field changes `fingerprint_text` unless stripped | read `fingerprint_text` (:581-594) and `struct Variant` (config.rs:158) |
| Stored vectors, counts and query-file sha256 matches | `ls`/python over `g-mesh-data/gm456/work/runs/…` and `g-mesh-eval-data/v2/work/runs/…` manifests |
| Python env | `uv pip compile` (unpinned resolve: torch 2.2.2, numpy 2.4.6, transformers 5.23.0), then an install + import smoke test of the pins |
