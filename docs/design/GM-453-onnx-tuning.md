# GM-453: ONNX Runtime tuning: threads, batch size, graph optimisation, length bucketing, background priority

Design note for slice GM-453/S1. Release 4.17.0, branch `release-4.17.0` at `96c0822`.
Line numbers are 1-based, from that commit.

**Code search used for this note.** The g-mesh MCP indexes nothing in this
checkout today: `select_project g-mesh` returned "Nothing is indexed here: no
plugin installed (rust 406 files ...)", the state GM-645 is fixing. All
cross-file answers below come from `grep -rn` over `core/src`, `core/tests` and
`eval/embedding`. The commands are listed in §1.4.

---

## 0. Summary for the owner

- **Today the embedder runs one text per ONNX call** (batch of one). It uses
  ORT's default thread count, graph optimisation `Level3` and deterministic
  compute. No knob can be changed without editing the code.
- **Change.** One `InferenceOptions` value, read from `G_MESH_EMBED_*` env
  vars. Its defaults reproduce today's session exactly. The model gets a
  batched `embed_batch` with optional length bucketing. The eval harness gets a
  vector-parity subcommand and a query-latency probe.
- **The vector-parity tolerance.** Per node, the L2 distance between the
  candidate vector and the baseline vector is at most 1e-4. With unit vectors,
  that bounds any search score shift by 1e-4. This is the same
  `PARITY_TOLERANCE` the eval already uses for scores.
- **Biggest risk found.** The production weights are int8, and the int8 graph
  quantises activations dynamically: one scale is computed per tensor. With
  batch > 1, the scale comes from the whole padded batch, so a text's vector
  depends on which other texts share its batch. Bucketing narrows this effect
  but does not remove it. Batching may fail parity on int8 even if it passes on
  fp32. The measure slice settles this cheaply in its first phase.
- **Second finding.** The fp32 reference weights are no longer on disk.
  `~/.g-mesh/models/jina-embeddings-v2-base-code/model.onnx` is the int8 file
  (161,895,621 bytes = `DEFAULT_ONNX_SIZE`). The `jina-v2-base-code-fp32` eval
  row points at that directory. Run today, the row would silently embed with
  int8 under the fp32 name. GM-438 and GM-440 require fp32 (R), so the code
  slice adds a separately fetched fp32 row (§5.4).
- **Background priority is measured first and built afterwards.** The measure
  slice approximates it from outside the process, with `taskpolicy -b` on the
  harness process and fewer intra-op threads. A per-thread mechanism inside the
  daemon is designed only if OS priority wins clearly (§4).
- **Measure time.** About 3.5 h of runs (213 min). With quiet-machine waits
  and the fp32 download, plan for 4 h. A lean variant takes about 2 h (§7).

---

## 1. Today

### 1.1 How the session is built

`EmbeddingModel::load_with_spec` (`core/src/embedding/model.rs:312-362`) is
the only place that builds an ORT session. Every other loader goes through it:
`load` (`:293`) calls `load_with_max_sequence_length` (`:306`), which calls
`load_with_spec`.

```rust
// model.rs:349-359
Session::builder()
    .with_optimization_level(GraphOptimizationLevel::Level3)
    .with_deterministic_compute(true)
    .commit_from_file(&onnx_path)
```

- **Threads.** None are set. ORT's `intra_op_num_threads = 0` means "pick a
  default", which ORT documents as the physical core count. GM-398 measured
  `user/real` of 3.82-3.95 on this 4-core/8-thread i7-1068NG7
  (`sysctl hw.physicalcpu=4 hw.logicalcpu=8`). So today's default already
  behaves as "physical cores". The measure slice confirms this with an
  explicit `intra=4` arm (§9, must-confirm M1).
- **Inter-op threads.** None are set. Execution mode is sequential, so
  inter-op threads are not used at all.
- **Spinning.** ORT's default: intra-op workers spin while waiting. This burns
  CPU that the user's other work could have used.
- **Graph optimisation.** `Level3` (`ORT_ENABLE_ALL`), the highest level.
- **Determinism.** `with_deterministic_compute(true)`.
  `model.rs:60-66` documents that `embed` is a pure function of its input for
  a loaded model.
- **Tokenizer.** Truncates at `max_sequence_length` and has padding disabled
  (`:345-347`: "a batch of one needs none").

### 1.2 How batches are formed: there are none

`EmbeddingModel::embed` (`model.rs:388-437`) tokenises one text and builds
`[1, L]` tensors. It runs the session once and pools with `pool`
(`model.rs:440-488`). `pool` checks that the output shape is exactly
`[1, L, D]`, takes the mean over the masked positions in a fixed order, and
L2-normalises.

The only place where several texts meet is `EmbeddingPipeline::compute`
(`core/src/embedding/pipeline.rs:434-488`):

1. It filters the diff's nodes, looks them up in the cache in bulk, and then
   calls `model.embed(&text)` one node at a time (`:465`).
2. If one node fails, the error is logged and that node is skipped
   (best-effort per node).
3. Fresh vectors are inserted into the cache keyed by `keys[index]`
   (`:461-486`).

The `Embedder` trait (`pipeline.rs:78-86`) has only `embed(&self, &str)`. The
test fake implements the same trait (`pipeline.rs:~886-930`, `test_support`).

### 1.3 Who calls the embedder

| caller | path | when | foreground or background |
|---|---|---|---|
| Daemon backfill | `daemon/activation.rs:224` calls `embedding::backfill::run` (`backfill.rs:155-212`), which calls `compute` per page of 256 nodes (`PAGE_SIZE`, `backfill.rs:90`) | every daemon activation, after the structural walk (`Phase::Embedding`) | **background**: competes with the user's interactive work and the daemon's own structural tools |
| Daemon incremental | `watcher/apply.rs:956` (`round_trip`) calls `compute` per file-change diff | every edit or semantic pass | background, small diffs (1-50 nodes) |
| Daemon query | `EmbeddingPipeline::embed_query` (`pipeline.rs:381`) calls `embed`, one query | every `search_code` call | **interactive**, about 15 ms on fp32 and 10 ms on int8 (GM-398, run timings) |
| CLI init and reindex | `cli/init.rs:216,247,231,268` and `cli/reindex.rs:116,143` call `backfill::run` | `g-mesh init` and `g-mesh reindex`, in the foreground | foreground |
| Eval harness | `cli/embed_eval.rs` `run_variant` (`:660-689`, loads with `load_with_spec` at `:678`) and `run_corpus`'s `Arm::Model` branch (`:795-806`) call `embed` one by one; `parity` (`:1489+`); `cost.rs:472,487` (calibrate, deliberately batch of one) | `g-mesh debug-embed-eval run --embed-only` is the GM-398 "pass time" | foreground, cache never touched |

The daemon builds one `EmbeddingPipeline` per project (`daemon/mod.rs:461`).
Backfill, incremental edits and `search_code` all share **one ORT session**.
This matters for §4: a session-wide thread or priority setting also applies to
the interactive query embedding.

The embedding cache key is `cache::text_hash(text)` plus
`cache::fingerprint(onnx_sha256, tokenizer_sha256)` (`cache.rs:117,123`;
`pipeline.rs:713-717`). **Session options are not part of the key.** The same
goes for `embedding_version` (`model.rs:124`), which names only the model,
weights tag and text form. So a setting that changes vectors beyond the
tolerance would silently mix old and new vectors in the cache and the indexes.
That is why the parity gate in §6 is binding.

### 1.4 Cross-file lookups (grep, since g-mesh is unindexed)

- Who calls `embed`, `load*`, or uses `EmbeddingModel`:
  `grep -rn -E '\.embed\(|EmbeddingModel::load|load_with_spec|load_with_max_sequence_length|EmbeddingModel\b' core` (the rows in the table above).
- Who calls `compute` or `backfill::run`:
  `grep -rn -E '\.compute\(|backfill::run\(|EmbeddingPipeline::load'`.
  The results are `watcher/apply.rs:956`, `backfill.rs:203`,
  `daemon/activation.rs:224`, `cli/init.rs`, `cli/reindex.rs` and
  `daemon/mod.rs:461`.
- Who builds the ORT session: `grep -rn 'Session::builder'`. The only result
  is `model.rs:349`. The reranker has its own session in `rerank.rs` and is
  out of scope.
- What reads the cache key: `cache::text_hash` and `cache::fingerprint`, used
  in `pipeline.rs:455` and `:713-717`.

---

## 2. How each knob becomes configurable

### 2.1 One options value

A new type in `model.rs`:

```rust
pub struct InferenceOptions {
    pub intra_threads: Option<usize>,      // None = ORT default (today)
    pub inter_threads: Option<usize>,      // None = today; Some(n) also turns on parallel execution
    pub optimization: OptLevel,            // Disable | Basic | Extended | All (today: All = Level3)
    pub intra_spinning: Option<bool>,      // None = ORT default (today)
    pub batch_size: usize,                 // 1 (today)
    pub token_budget: usize,               // max padded tokens per batch; 8192 is a placeholder default
    pub bucket_by_length: bool,            // false (today)
}
```

- `InferenceOptions::default()` is today's session and calling pattern.
- `InferenceOptions::from_env()` reads the variables below.
- `EmbeddingModel::load_with_options(dir, spec, &opts)` becomes the one
  builder.
- `load`, `load_with_max_sequence_length` and `load_with_spec` stay, and pass
  `InferenceOptions::default()`. Every existing test and the cost calibration
  therefore keep today's session.

### Decision D1: where the knobs are read

**Today:** nothing is configurable.

| option | benefit | risk |
|---|---|---|
| **(a) Env vars `G_MESH_EMBED_*`, read in two places: the pipeline's loader (`pipeline.rs:236-241`) and the harness's `run_variant` (Recommended)** | No config schema change. Measurement-only knobs stay undocumented, like `G_MESH_EMBEDDING_CACHE`. The same variable drives the harness and a real `g-mesh reindex`, so S5 can confirm end to end. | The daemon only sees the variables if its spawner passes the environment through (must-confirm M7). A typo in a variable name goes unnoticed, so the harness echoes the effective options into `timings.json`. |
| (b) `debug-embed-eval run` flags only | Explicit per run, and visible in the command line. | The real index pass cannot be measured. S6 would still need a production path. |
| (c) `[embedding]` keys in `config.toml` | Per-project and documented. | Adds to the config schema for knobs whose defaults may end up "unchanged". It also touches selective-reindex logic (GM-509). Too early. |

Recommendation: (a). The adopted values become code defaults in S6, as a
rule, not as a constant. The env vars stay as an override.

Variables (absent or empty = default; an invalid value logs one line and falls
back to the default; it never panics):

| var | values | default (today) |
|---|---|---|
| `G_MESH_EMBED_INTRA_THREADS` | `0` = ORT default, `n`, `physical`, `logical` | `0` |
| `G_MESH_EMBED_INTER_THREADS` | `n` (turns on parallel execution) | unset |
| `G_MESH_EMBED_OPT_LEVEL` | `disable` / `basic` / `extended` / `all` | `all` |
| `G_MESH_EMBED_SPIN` | `0` / `1` | unset (ORT default) |
| `G_MESH_EMBED_BATCH` | `n >= 1` | `1` |
| `G_MESH_EMBED_TOKEN_BUDGET` | `n >= max_sequence_length` | `8192` |
| `G_MESH_EMBED_BUCKET` | `0` / `1` | `0` |

`physical` and `logical` exist so that S5 can test the rules, not only
constants. `logical` comes from `std::thread::available_parallelism()`.
`physical` comes from `sysctlbyname("hw.physicalcpu")` through `libc`, which
is already a dependency (`core/Cargo.toml:156`). On Linux it comes from the
distinct `core id`s in `/proc/cpuinfo`, and elsewhere it falls back to
`logical`. No new crate is needed.

### Decision D2: what "defaults unchanged" means for the code slice

`batch_size = 1` must take **the exact same code path as today**: the `embed`
loop, not a batched call with n = 1. Then today's vectors stay bit-identical by
construction, and S2/S4 can pin that with an equality check rather than a
tolerance. The batched path is used only when `batch_size > 1`.

### 2.2 Per-knob notes

- **Intra-op threads.** `SessionBuilder::with_intra_threads(n)` (ort rc.9
  `impl_options.rs:49`). Today = ORT default. Candidates for the rule: physical
  (believed to be today's behaviour), logical, physical-1, physical/2.
- **Inter-op threads.** `with_parallel_execution(true)` plus
  `with_inter_threads(n)` (`:61,71`). The encoder graph is mostly a linear
  chain, so little gain is expected. It gets one screening arm.
- **Graph optimisation level.** `with_optimization_level` (`:83`). Today is
  already the highest level, so lower levels can only lose speed. They are
  measured once, because the acceptance criteria list this knob, and the
  parity check shows whether fusions change any bits.
  `with_approximate_gelu` and similar options change the maths and are
  excluded.
- **Spinning.** `with_intra_op_spinning(false)` (`impl_config_keys.rs:96`).
  It is mainly a background-mode knob: it cuts CPU burned while waiting, at a
  possible small throughput cost.
- **Batch size and token budget.** See §3. The budget caps padded tokens per
  call. Attention memory grows with `batch × L²`: 32 texts × 1,024 tokens would
  allocate gigabytes, which `embedding_peak_memory.rs` exists to prevent. The
  default budget of 8192 equals 8 texts at full length and stays within that
  test's spirit. S4 checks it (§9, B5).

---

## 3. Length bucketing: where the reorder happens and how order is restored

**Today:** each text is its own call, so there is no reorder and no padding.

**Change.** A new method `EmbeddingModel::embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>>`
returns results **in input order**. A pure planning function inside it does
all the reordering:

```rust
/// Groups text indices into batches. Each index appears exactly once.
fn plan_batches(lengths: &[usize], batch_size: usize, token_budget: usize, bucket: bool) -> Vec<Vec<usize>>
```

1. Tokenise every text (as `embed` does), keeping `(original_index, ids, mask)`.
2. Build an order: `0..n` if `bucket` is false; otherwise a stable sort by
   token length, with ties broken by original index (deterministic).
3. Fill batches greedily in that order. Add the next text while
   `count < batch_size` and `(count+1) * max_len_in_batch <= token_budget`. A
   text longer than the budget alone forms a batch of one.
4. For each batch, pad to the batch's longest text with `<pad>`. Its id is 1
   in the jina tokenizer; read it with `tokenizer.token_to_id("<pad>")` and
   fall back to 0. Padding gets mask 0. Build `[B, Lmax]` tensors, plus
   `token_type_ids` when the spec asks for them. Run once.
5. Pool each row `b` with its own mask. `pool` is generalised to take
   `(values, row b, Lmax)`. The shape check becomes `[B, Lmax, D]`, and the
   arithmetic is unchanged.
6. **Order restoration.** Each result is written into
   `out[original_index]`, a `Vec<Option<Result<..>>>` preallocated to `n`.
   Result `i` therefore always belongs to text `i`, whatever the batch order.
7. **Failure isolation (keeps today's per-node best effort).** If a batch's
   `session.run` fails, each text of that batch is retried with `embed` (batch
   of one). One pathological text cannot take its batch-mates down with it.

The `Embedder` trait gets
`fn embed_batch(&self, texts: &[&str]) -> Vec<Result<Vec<f32>>>`, whose
default implementation is `texts.iter().map(|t| self.embed(t)).collect()`.
The test fake keeps counting one embed per text. `EmbeddingModel` overrides
it, and calls the default loop when `batch_size == 1` (D2).

**`EmbeddingPipeline::compute` (`pipeline.rs:458-480`)** first collects the
cache misses as `(index, node_id, text)`, then calls `embed_batch` once on
their texts, then walks the results in the same order. Hits and misses are
pushed into `computed` **in pending order, as today**. `fresh` keeps pairing
`keys[index]` with the position of that node's vector. The reorder never
leaves `embed_batch`, so the cache-key pairing cannot break (S4 pins this,
B10).

- **Backfill.** A batch holds at most one page (256 nodes), so bucketing
  works within a page.
- **Watcher.** Batches are the diff's few nodes.
- **`embed_query`.** Stays `embed`, a single text, so query vectors are not
  affected.

**Harness.** `run_corpus`'s `Arm::Model` branch (`embed_eval.rs:795-806`)
builds the prefixed texts, calls `embed_batch` in chunks of 256 (the backfill
page, so the eval batches exactly as production does) and ticks progress per
chunk. `WordsShuffled` stays one at a time (it is a control arm).

**Example.** Take texts with token lengths `[900, 12, 15, 880, 14]`, batch 4,
budget 8192, bucketing on. The sorted order is `[1(12), 4(14), 2(15), 3(880), 0(900)]`.

- Batch A is `[1, 4, 2, 3]`: 4 × 880 = 3,520 ≤ 8,192, so it fits. Batch B is
  `[0]`.
- Without bucketing, batch A is `[0, 1, 2, 3]`, padded to 900, so three short
  texts run at 900 tokens each. That padding waste is what GM-438 wants to cut.
- Outputs are written to `out[1]`, `out[4]`, `out[2]`, `out[3]`, `out[0]`.
  The caller sees `out[0..5]` in input order.

**Consequence.** Padding positions never enter the pooled vector, because the
mask excludes them in attention and in `pool`. But they do change the shapes
of the matrix multiplications, so fp32 results may differ in the last bits.
On **int8**, the graph's `DynamicQuantizeLinear` computes one scale per tensor
over the whole `[B, L, ·]` activation, padded rows included. A text's vector
then depends on its batch-mates. That can push it past the tolerance, and it
breaks the "pure function of its input" property (`model.rs:60-66`). S5's
phase A answers this in minutes (M2). If int8 fails parity, batching is not
adopted for the production weights (decision D6).

---

## 4. Background priority (GM-440)

**Today:** backfill runs at normal priority on 4 intra-op threads (ORT
default) inside the daemon. It shares one session with `search_code`'s query
embedding (§1.3).

Mechanisms considered:

| option | benefit | risk |
|---|---|---|
| (i) Fewer intra-op threads for the daemon's session (`G_MESH_EMBED_INTRA_THREADS`, rule e.g. physical-1 or physical/2) | Portable. No OS code. One knob already built in S2. | It is session-wide, so query embedding also gets fewer threads. Queries are about 10 ms on int8, so +5-10 ms is expected. The probe measures it. |
| (ii) Per-thread low priority: ORT workers created through `SessionBuilder::with_thread_manager` (ort rc.9 `impl_options.rs:205`, trait `ThreadManager` in `environment.rs:167`) at `QOS_CLASS_UTILITY`/`BACKGROUND` on macOS (`pthread_set_qos_class_self_np`), or `setpriority(PRIO_PROCESS, tid, 10)` on Linux, with the backfill thread lowering itself around `compute` | Only embedding is deprioritised. Structural tools keep their priority. | Because the session is shared, a `search_code` query embedding runs on the same low-priority workers. The calling thread does `1/n` of the work, the workers do the rest. Windows needs separate code. It is the most code and the hardest to test. |
| (iii) Two sessions: background (low priority, few threads) and interactive | Clean separation. | Twice the model in memory: about +700 MiB for int8 (GM-398: int8 RSS ≈ 760 MiB). Rejected. |
| (iv) Whole process at background priority | Trivial. | The daemon also answers structural tools. Not acceptable in production. |

### Decision D3: measure before building a mechanism (Recommended)

**Change.** S5 approximates (ii) from outside the process, with no new
daemon code:

- `taskpolicy -b` runs the harness process at Darwin background policy (lower
  CPU priority and throttled I/O; the pass does almost no I/O).
- The harness process only embeds, so this equals "every embedding thread at
  low priority". It is the **upper bound** of what (ii) can deliver.
- Option (i) is measured directly with `G_MESH_EMBED_INTRA_THREADS`.

Outcomes:

- (i) gives most of the latency benefit: S6 adopts it as a daemon-only
  default. The daemon's `EmbeddingPipeline::load` gets the background rule,
  and the CLI init and reindex keep the foreground rule. No OS-specific code.
- `taskpolicy -b` is clearly better than (i): a new design and implement
  slice for (ii) is inserted before S6 (§10).
- Neither helps: nothing is adopted.

The alternative was to build (ii) in S2 now. It would be implemented before
we know it is needed, and it is the riskiest piece of code in the task.

### How interactive latency is measured

The probe is a new subcommand,
`debug-embed-eval probe --variant V --corpus g-mesh --burst 5 --interval-ms 2000 --until <file> --out <jsonl>`.

- It loads the model at **normal priority with default options** (today's
  `search_code` setup).
- It loops until the stop file appears. Each iteration embeds `burst` queries
  from the corpus's query set, timing each one, then sleeps `interval-ms`.
  Each query is written as a line `{t, query_id, ms}`.
- Bursts with idle gaps mimic an agent's `search_code` calls. A back-to-back
  loop would turn the probe into a second batch job.
- The summary reports the probe's p50, p95 and max query ms per arm, plus the
  pass's wall-clock time with the probe running.

Arms, all on fp32 (R), g-mesh corpus:

| arm | what runs |
|---|---|
| P-idle | the probe alone, 5 min |
| P0 | normal-priority pass and the probe |
| P1 | the pass under `taskpolicy -b` and the probe |
| P2 | the pass with the intra-thread background rule, normal priority, and the probe |

The pass's wall-clock cost is the arm's time with the probe against R's time
without it (§7, phase C). The benefit is the probe's p95 against P0 and
P-idle.

**Not measured:** latency through the daemon's shared session, because the
probe is a separate process. For (i) the effect on the shared session is
"queries also get fewer threads". The probe can reproduce that with
`G_MESH_EMBED_INTRA_THREADS` set to the background rule, as one extra short
probe-only arm.

---

## 5. Timing harness

### 5.1 What it times

The existing GM-398 pass: `debug-embed-eval run --variant V --corpus C --embed-only --force`.

- `timings.json.embedNodesMs` covers the embed-all step only, excluding model
  load.
- With `G_MESH_EMBEDDING_CACHE=off`, which the harness never touches anyway.
- Under `/usr/bin/time -lp`, which gives real, user, sys and max RSS.

### 5.2 What S2 adds to the harness (`core/src/cli/embed_eval.rs`)

- `run_variant` (`:660-689`) loads with
  `load_with_options(.., InferenceOptions::from_env())`.
- `Timings` (`:507-514`) gains:
  - `options` (the effective `InferenceOptions`, with threads resolved to the
    number the rule produced);
  - `embeddingsPerSecond = node_count / (embedNodesMs/1000)`;
  - `batches` and `paddedTokens` against `realTokens` (padding waste, from
    `embed_batch`'s plan, returned in a small stats struct or counted in the
    harness).
- **The variant fingerprint does NOT include options.** Arms go to separate
  `--out` directories. `--force` always recomputes, so reuse cannot mix arms.
- The `run` (non-embed-only) path is reused for quality.
  - After a timed pass, S5 runs `run` again without `--force` and without
    `--embed-only` in the same out directory. The manifest matches, so
    `vectors.bin` is reused, and only the 363 g-mesh queries are embedded and
    ranked (about 30 s).
  - `report --reference <baseline run>` then gives recall@10 and MRR per arm
    on the g-mesh corpus.
- New subcommands:
  - `compare` (§6);
  - `probe` (§4).

### 5.3 Measure script

`eval/embedding/gm453_measure.sh` and `gm453_summary.py`, in the pattern of
`measure_costs.sh` and `gm465_measure.sh`. They are written in S2 so that S3
reviews them, and S5 only runs them.

- One script with fail-fast for unexpected failures (a non-zero harness exit),
  but **not** on parity failures. Those are results; they are recorded and
  the run continues.
- Arms alternate within each round.
- Before and after every run it records `uptime` and the `time -lp` lines.
- It reuses `measure_costs.sh`'s `wait_quiet`. The thresholds come from the
  owner-approved noisy-machine rule in GM-398 (start below 7.0, redo once
  above 8.0 plus the run's own parallelism).
- It writes `summary.md`, with one row per arm and round:
  `arm, round, embedNodesMs, emb/s, real, user, sys, user/real, maxRSS MB, load1 before/after, parity verdict, min cos, max L2, bit-identical %`.
  It also writes medians and ×baseline per arm, plus a phase table each for
  recall@10/MRR and probe latency.

### 5.4 fp32 (R) weights

**Today:** the `jina-v2-base-code-fp32` row (`eval/embedding/variants.toml:16-26`)
points at `~/.g-mesh/models/jina-embeddings-v2-base-code`. That directory now
holds the int8 weights, because production switched to int8 (ADR 0011).

**Change.** S2 adds a row `jina-v2-base-code-fp32-r`:

- same repository, revision and spec as the reference;
- `onnx_file = "onnx/model.onnx"`;
- **no** `model_dir`, so `fetch_models.py jina-v2-base-code-fp32-r`
  downloads about 640 MB into `work/models/`.

The reference row stays as it is, because a test pins it. S5 checks the
fetched file's size and sha256 against the GM-398 run's manifest
(`work/runs/jina-v2-base-code-fp32/g-mesh/manifest.json` records the model
files' sha).

The production arm for int8 is `jina-v2-base-code-int8-structured` (the
shipped weights and text form).

---

## 6. Vector-parity check

### 6.1 Tool

`debug-embed-eval compare --baseline <run>/<corpus> --candidate <run>/<corpus> [--max-l2 1e-4] [--json out]`
(new, in `embed_eval.rs`, next to `parity`):

- It reads both `manifest.json` files. It fails if the variant fingerprint,
  snapshot sha, node-id hash, count or dimension differ. Comparing different
  node sets or text forms would be meaningless.
- It reads both `vectors.bin` files and computes, per node, the L2 distance,
  `1 - cos` and bit-equality.
- Output: node count, number of bit-identical nodes, max L2 and the node id
  where it occurs, min cos, and count above the tolerance.
- Exit code 1 if any node exceeds the tolerance, naming up to 10 such nodes.

### 6.2 Node sample

**Every candidate node of the corpus**, not a subsample: all 6,774 on g-mesh,
and gin plus py-requests in screening. The vectors already exist from the
timed pass, so a full comparison takes a few seconds.

"A fixed node sample" (acceptance criteria) is therefore the corpus snapshot,
pinned by `snapshot_sha256`. For the unit tests in S4 the fixed sample is a
short list of texts in the test: a 1-token text, a typical signature, and a
text longer than 1,024 tokens (it gets truncated).

### 6.3 Tolerance: max L2 distance ≤ 1e-4 per node

Justification:

- **Bit-identity cannot be required.** With batching, padding or a different
  thread split, fp32 matrix multiplications run with different shapes and
  blocking (MLAS picks its kernel and tiling by `M = B×L`). That changes the
  order of rounding within reductions. `with_deterministic_compute` fixes
  only nondeterministic algorithm choices for *identical* shapes. Expected
  differences are 1e-7 to 1e-6 per element on unit vectors, after 12 layers
  of accumulation.
- **What the tolerance protects.** Vectors are unit length and scores are dot
  products. So `|q·v' − q·v| ≤ ‖v' − v‖`. An L2 distance ≤ 1e-4 means **no
  search score can move by more than 1e-4**. That is the same bound the
  eval's `PARITY_TOLERANCE` (`embed_eval.rs:1487`) already accepts for
  harness-versus-production scores. Mixed old and new vectors in one index or
  cache then rank alike, except for exact near-ties.
- **It still catches real changes.** fp32 against int8 is about 1e-1 in L2
  (cos about 0.99, GM-422). A text-form change is larger still. A
  batch-dependent int8 quantisation scale is expected around 1e-3 to 1e-2.
  All of these are far above 1e-4.
- The summary also reports the **bit-identical share**, so the owner can see
  whether a setting is "identical" or "within tolerance".
- If an adopted setting passes only "within tolerance", nothing is
  re-embedded and `embedding_version` is unchanged.
- If it fails, it is either rejected or adopted together with a weights or
  version tag change, which forces a re-embed. That is decision D6, made after
  S5.

### 6.4 Controls

- **Unit control (S4).** `compare` on two run directories with identical
  vectors except one node offset by 2e-4 in L2 must exit non-zero and name
  that node. An offset of 5e-5 passes. Revert check: if `compare`'s threshold
  comparison is reverted to always pass, the test fails.
- **Real control (S5, phase A).** `compare` of the int8-structured baseline
  against the fp32 R arm. A cross-variant comparison is refused by the
  fingerprint check, so the control instead compares
  `int8-structured` round 1 with a run made with `G_MESH_EMBED_OPT_LEVEL=disable`.
  If that run happens to be within tolerance, the fallback is a deliberately
  wrong arm: the `-noise` row (`jina-v2-base-code-int8-noise`, a different
  text filter on the same nodes). Must-confirm M5: the fingerprint check may
  need a `--allow-variant-mismatch` switch to let the control compare two
  rows over the same node ids. If the node ids differ, compare by node id
  over the intersection.

Recommendation: implement `compare` keyed by node id over the intersection,
with `--allow-variant-mismatch`. The real control is then simply "int8
baseline against the fp32-r baseline on g-mesh", which must fail. It costs
nothing extra, because both passes are run anyway.

---

## 7. Variant matrix for S5 and run time

Timing basis on this machine, from earlier results (noisy, owner's working
machine):

- int8 g-mesh pass: 540 s (GM-465).
- int8-structured (production) pass: about 0.68× = **370 s**.
- fp32 g-mesh pass: **750 s** (GM-398).
- Screening corpora gin (1,547 nodes) plus py-requests (961): about 85 s int8,
  so about **60 s for int8-structured**.

At the time of writing `uptime` showed load **425** (other agents' builds), so
S5 must start on a quiet machine and must not overlap GM-646.

### Phase A: screening on int8-structured (production weights), gin + py-requests, 2 alternating rounds

Variables are those of §2.1. A blank means the default.

| arm | INTRA | INTER | OPT | SPIN | BATCH / BUDGET | BUCKET |
|---|---|---|---|---|---|---|
| A0 baseline | | | | | | |
| A1 | 1 | | | | | |
| A2 | 2 | | | | | |
| A3 | 4 (physical, explicit; must equal A0, M1) | | | | | |
| A4 | 8 (logical) | | | | | |
| A5 | | | basic | | | |
| A6 | | | extended | | | |
| A7 | | | | 0 | | |
| A8 | | 2 | | | | |
| A9 | | | | | 8 / 8192 | 0 |
| A10 | | | | | 8 / 8192 | 1 |
| A11 | | | | | 32 / 16384 | 1 |
| A12 | 4 + BATCH 8 + BUCKET 1 | | | 0 | | |

That is 13 arms × 2 rounds × about 65 s. Including parity per arm,
**about 30 min**.

**Gate before phase B.** Any arm that fails parity is dropped from the
"adoptable" set but still reported. If A9-A11 fail on int8, phase C shows
whether batching is an fp32-only gain.

### Phase B: confirmation on int8-structured, g-mesh corpus, 3 alternating rounds

Three arms: A0, the best combined arm, and the runner-up. Each takes about
370 s, plus about 30 s for the quality re-run once per arm.
9 × 6.5 min + 2 min = **about 60 min**.

Outputs: emb/s, total time, RSS, recall@10/MRR on g-mesh against A0, and
parity on all 6,774 nodes.

### Phase C: fp32 (R), GM-438 and GM-440 acceptance criteria, g-mesh corpus, 2 alternating rounds

| arm | setting |
|---|---|
| C0 R | batch of one |
| C1 R | best bucketing setting (batch and budget from A) |
| C2 R | background thread rule (from A: physical-1 or physical/2), no probe |

That is 3 × 2 × 12.5 min, plus quality once per arm. **About 78 min.**

The fp32 download (about 640 MB) is done before the timed runs and is not
counted.

### Phase D: interactive latency (GM-440), fp32, g-mesh, 1 round

Arms P-idle (5 min), P0, P1 and P2 (§4), each a full fp32 pass with the
probe. That is 5 + 3 × about 13.5 min. **About 45 min.**

### Total

About 30 + 60 + 78 + 45 = **213 min, about 3.5 h of runs**. Plan for **4 h**
including quiet waits and the fetch.

### Decision D4: matrix size

| option | benefit | risk |
|---|---|---|
| **Full matrix as above, about 3.5-4 h (Recommended)** | Meets every acceptance criterion with repeated rounds where the gain decides adoption (phase B). | One long background run. Machine noise is real (GM-398: load 5-11). |
| Lean: phase B with 2 rounds and 2 arms (A0 and best), phase C with 1 round, phase D without P2 if A shows that threads help nothing in the background, about 2 h | Half the wall-clock. | A single fp32 round does not separate a gain under 10% from noise. P2 may be missed. |

---

## 8. Edit map for the code slice (S2)

### Change

| file | function or item | lines | change |
|---|---|---|---|
| `core/src/embedding/model.rs` | new `InferenceOptions`, `OptLevel`, `from_env`, `resolve_threads` (physical/logical) | after `EncoderSpec` (`:237-258`) | §2.1. Parsing is pure and testable: `from_env_with(get: impl Fn(&str) -> Option<String>)`. |
| same | `EmbeddingModel` struct | `:267-271` | add `options: InferenceOptions` and `pad_id: i64` |
| same | `load_with_spec` becomes `load_with_options`; `load_with_spec` delegates to it | `:312-362` (builder `:349-359`) | apply each option only when it is `Some` or differs from the default, so the default builder chain is literally today's |
| same | `embed` | `:388-437` | unchanged; drop the now-stale "Batch of one ... not needed yet" comment, or point it to `embed_batch` |
| same | new `embed_batch`, `plan_batches` | after `embed` | §3 |
| same | `pool` | `:440-488` | take a row offset and `Lmax`; check shape `[B, Lmax, D]`; same arithmetic |
| `core/src/embedding/pipeline.rs` | `Embedder` trait and impl | `:78-86` | add `embed_batch` with a default loop; `EmbeddingModel` forwards to its own |
| same | `build`'s loader | `:236-241` | `EmbeddingModel::load_with_options(dir, EncoderSpec::production(), &InferenceOptions::from_env())` |
| same | `compute` | `:458-480` | collect misses, call `embed_batch` once, keep the pending order and the `fresh` key/position pairing (§3) |
| `core/src/cli/embed_eval.rs` | `RunArgs` unchanged; `run_variant` | `:660-689` (`:678`) | load with `from_env()` options |
| same | `run_corpus`, `Arm::Model` branch | `:795-806` | `embed_batch` per 256-text chunk |
| same | `Timings` | `:507-514` | `options`, `embeddingsPerSecond`, `batches`, `paddedTokens`, `realTokens` |
| same | `EmbedEvalCommand` and `run` dispatch | `:60-75`, `:161-170` | add `Compare(CompareArgs)`, `Probe(ProbeArgs)` |
| `core/src/cli/embed_eval/compare.rs` (new) | `compare` | n/a | §6.1 |
| `core/src/cli/embed_eval/probe.rs` (new) | `probe` | n/a | §4 |
| `eval/embedding/variants.toml` | new row `jina-v2-base-code-fp32-r` | after `:26` | §5.4 |
| `eval/embedding/gm453_measure.sh`, `gm453_summary.py` (new) | n/a | n/a | §5.3, §7 |

### Read for context (not changed)

- `embed_eval.rs`:
  - `parity` and `PARITY_TOLERANCE` (`:1487-1560`);
  - `read_vectors` and `write_vectors` (`:193-212`);
  - `fingerprint` and `fingerprint_text` (`:516-535`);
  - the manifest fields (around `:420-476`).
- `embed_eval/cost.rs:433-560` (`calibrate`, which keeps batch-of-one
  `load_with_spec` on purpose).
- `pipeline.rs` `test_support` (`~:884-975`).
- `backfill.rs:155-212`.
- `core/tests/it/embedding_peak_memory.rs`.
- `eval/embedding/measure_costs.sh` and `gm465_measure.sh`.
- ort rc.9 `session/builder/impl_options.rs` and `impl_config_keys.rs`.

Out of scope for S2: any per-thread priority code (D3), and any change to
defaults (S6).

---

## 9. What the tests slice (S4) must pin, and must-confirm items

### Behaviours, one line each

- B1. `InferenceOptions::default()` equals today's session: intra and inter
  unset, `All`, spin unset, batch 1, budget 8192, bucket off.
- B2. `from_env_with` parses every variable, including `physical` and
  `logical`. An invalid or zero value falls back to the default (and logs)
  instead of panicking.
- B3. With `batch_size = 1`, `embed_batch` returns vectors **bit-identical** to
  `embed` for each text (real model; skipped if the model is absent, like the
  existing tests).
- B4. With batch > 1 and bucketing, `embed_batch` returns results in input
  order for texts of shuffled lengths, each within L2 1e-4 of `embed` (on the
  fp32 model if it is present; on int8 the result is recorded, not asserted,
  pending D6).
- B5. `plan_batches`: every index appears exactly once, no batch exceeds
  `batch_size`, no multi-text batch exceeds `token_budget` in padded tokens,
  an over-budget text forms its own batch, and with bucketing the order is
  non-decreasing by length.
- B6. A failing batch falls back to per-text calls: only the bad text errors
  and the others get vectors.
- B7. `pool` on row `b` of a padded `[B, L, D]` tensor equals `pool` of that
  text alone as `[1, l, D]` (pure, without weights).
- B8. `compute` with a batching fake keeps each node's vector paired with its
  node id and its cache key (`fresh`), and with cache hits mixed in.
- B9. `compare`: identical runs pass; one node offset by L2 2e-4 fails and is
  named; one offset by 5e-5 passes; runs with different node-id hashes are
  refused (or intersected, per M5).
- B10. The harness `timings.json` records the effective options and
  `embeddingsPerSecond`.
- B11. The probe stops on the stop file and writes one line per query.

### Controls

6-8 controls, one per behaviour group:

- revert the order restoration (write results in batch order): B4 and B8 fail;
- make `plan_batches` ignore the budget: B5 fails;
- make the default batch 8: B1 and B3 fail;
- drop the per-text fallback: B6 fails;
- make `compare`'s threshold always pass: B9 fails;
- make `pool` use row 0 for every row: B7 fails;
- make `from_env` ignore one variable: B2 fails.

### Must-confirm for implementation and measure

- M1. ORT's default intra-op thread count on macOS equals the physical cores.
  A0 and A3 must have the same `user/real` and timing.
- M2. int8 dynamic quantisation together with padding or batch composition
  (§3): does batching stay within L2 1e-4 on int8? Phase A answers this.
- M3. The jina pad id is 1 (`tokenizer.json` added token `<pad>`, id 1). Only
  mask-0 positions are padded.
- M4. `with_inter_threads` has no effect without
  `with_parallel_execution(true)`. Set both together.
- M5. Whether `compare` should intersect by node id or refuse on a
  fingerprint mismatch (needed for the real control).
- M6. Peak RSS at budget 8192 and 16384 (phase A records max RSS). Keep the
  default budget within `embedding_peak_memory.rs`'s bound.
- M7. Whether the daemon inherits `G_MESH_EMBED_*` from its spawner. This
  matters only for an end-to-end check and for S6's override story.
- M8. Whether `report` accepts runs that contain only the g-mesh corpus. If
  not, recall@10 and MRR are computed by `gm453_summary.py` from
  `rankings.jsonl`.
- M9. A `physical` probe on Linux and Windows (fallback to logical). It is
  only needed if S6 adopts a physical-based rule.

---

## 10. Proposed re-slicing

The draft (S2 code, S3 review, S4 tests, S5 measure, S6 adopt, S7 verify)
mostly holds. Changes:

1. **S2 scope.** Add the `compare` and `probe` subcommands, the
   `fp32-r` variant row and the `gm453_measure.sh` and `gm453_summary.py`
   scripts. The scripts are reviewed in S3, so S5 only runs them. **No
   priority mechanism** in S2 (D3).
2. **S5 phase A is a gate.** If every batching arm fails parity on int8 and
   no thread or opt arm gains more than 5%, S5 stops after phase C/D for the
   acceptance-criteria reporting, and S6 changes no default. That outcome
   still has to be written up in docs/results.
3. **Conditional slice S5b (opus, design and implement).** Inserted only if
   phase D shows that `taskpolicy -b` (P1) clearly beats the thread rule (P2)
   on probe p95: the in-daemon per-thread priority (§4 option ii), followed
   by a short re-measure of P1 against the implementation.
4. **S6 model.** Sonnet stays right if S6 is "set defaults plus a physical
   rule". It becomes opus if D6 is "adopt batching with a version-tag bump"
   (re-embed implications).

### Decisions for the owner

- **D1.** Env vars `G_MESH_EMBED_*` as the knob surface (Recommended), or
  flags, or `config.toml`.
- **D3.** Measure background priority with `taskpolicy -b` and the thread
  rule first, and build the in-daemon mechanism only if it wins
  (Recommended).
- **D4.** Full matrix of about 3.5-4 h (Recommended), or lean, about 2 h.
- **Tolerance.** Per-node L2 ≤ 1e-4, so search scores move by at most 1e-4
  (Recommended), or bit-identical only.
- **D6** (after S5). If batching fails parity on int8: keep int8 at batch 1
  (Recommended), or adopt batching with an `embedding_version` bump and a
  re-embed.
