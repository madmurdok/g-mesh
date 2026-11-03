# GM-695: the wall-time cost of the tiered backfill (diagnosis)

Status: diagnosis and design (S1). No production code changed. Raw data:
`docs/results/GM-695-data/`.

## Verdict

**The 7-10% cost from GM-441/S25 does not reproduce. The tiering's real
cost is about 0.3% of the pass.**

- In an order-balanced block of daemon runs (A1 A0 A0 A1), A1's embedding
  pass averaged 234.6 s and A0's 234.1 s (+0.2%).
- Within one arm, passes vary by up to 32 s (14%). That variation follows the
  load and the machine's thermal speed limit, not the arm.
- All of the tier code (the three keyset scans, `handed`, the extra
  `compute`/`store` calls, the tier boundaries) adds about 0.66 s per pass.
- Every other second of the pass is ONNX inference on the same 5,411
  candidates (4,237 embedded) in both arms.
- The order of the texts does not make inference slower. In an offline A/B
  on identical texts, tier order was not slower than id order.

**Recommendation: F0.** Make no production change, and correct the
measurement: run arms in balanced order and record the thermal state. The
owner decides whether that closes GM-695 (see "Decisions").

## What was measured

### Arms and builds

- **A0** is `2727ea5`, the first parent of the GM-441 merge `4a9fe58` on
  release-4.24.0. `git diff 2727ea5 4a9fe58` contains only GM-441's files.
- **A1** is `de338a6`, this branch. `4a9fe58..de338a6` changes only the
  version.
- Both arms were built in release in throwaway worktrees, seeded from a clone
  of the main checkout's `target/release`. Each got the same temporary trace
  in `embedding::backfill::run` (`a0-trace.patch`, `a1-trace-and-bench.patch`).
  The worktrees were removed afterwards.
- The trace measures five things. Per `compute` call, it logs the wall time
  of the candidate-page fetch (lock wait included), of `compute`, and of
  `store_vectors`. It also logs process CPU (`getrusage`) and involuntary
  context switches across each call. Per pass, it logs a total and
  `other` = wall − (fetch + compute + store).
- The driver is `gm441_confirm.py` with two additions
  (`driver-vs-gm441_confirm.diff`). A sampler records load1, daemon CPU and
  descendant CPU every 2 s. A `--no-tier-search` flag gives an A1N arm without
  the driver's `search_code` calls at tier boundaries.
- The setup is the confirm3 setup: fresh corpus copy, isolated
  `G_MESH_HOME`, cache off, cross-encoder installed. Each run starts only
  when load1 ≤ 4.

### Traced daemon passes (seconds of the backfill pass)

| run | arm | pass wall | inference (`compute`) | fetch | store | other | pages | load1 mean / max during pass | CPU_Speed_Limit start → end |
|---|---|---|---|---|---|---|---|---|---|
| r1 | A0 | 224.1 | 222.5 | 0.14 | 1.49 | 0.02 | 23 | 16.0 / 42.2 | not recorded |
| r2 | A1 | 237.9 | 235.5 | 0.50 | 1.82 | 0.07 | 51 | 8.3 / 15.7 | not recorded |
| r3 | A1N | 236.6 | 233.6 | 1.37 | 1.64 | 0.07 | 51 | 10.2 / 15.9 | not recorded |
| s1 | A1 | 245.6 | 243.1 | 0.65 | 1.80 | 0.08 | 51 | 16.2 / 24.4 | 58 → 58 |
| s2 | A0 | 250.0 | 248.1 | 0.27 | 1.63 | 0.03 | 23 | 15.8 / 51.6 | 92 → 53 |
| s3 | A0 | 218.2 | 215.8 | 0.29 | 2.05 | 0.03 | 23 | 12.6 / 20.9 | 100 → 63 |
| s4 | A1 | 223.5 | 221.0 | 0.61 | 1.89 | 0.07 | 51 | 16.9 / 30.9 | 100 → 53 |

Machine state (`uptime`, `/usr/bin/time -p` of the driver) for every run is
in `traced-A0-A1-A1N.txt` and `traced-balanced-A1-A0-A0-A1.txt`. The driver's
user/sys (8-11 s / 29-38 s) excludes the daemon. Daemon CPU comes from the
trace and from `ps`.

- **Arm means:** A0 230.8 s (218.2-250.0). A1 235.7 s (223.5-245.6, A1N
  included).
- **Balanced block s1-s4:** A1 234.6 s, A0 234.1 s, a difference of +0.2%.
  The spread within each arm (A0: 31.8 s) is about 60 times that difference.
- **Non-inference time (fetch + store + other):** A0 averages 1.98 s, A1
  2.64 s. The tiering's own cost is therefore about +0.66 s (0.3%).
  - It comes from 51 keyset pages instead of 23: the public scan reads every
    candidate, the private scan reads the rest, and the test scan reads only
    test rows.
  - It also comes from 48 `compute`/`store` calls instead of 22.
- **Tier boundaries:**
  - `other` is at most 0.08 s, so no stall shows at a tier boundary.
  - Without the driver's searches at tier boundaries (A1N), the pass took
    236.6 s, against 237.9 s with them (A1). The searches cost nothing
    measurable.
- **CPU against waiting:**
  - During `compute`, CPU/wall is 3.8-3.97 in both arms. ORT's 4 intra-op
    threads spin, so CPU follows wall in either arm.
  - S25's "equal daemon CPU" was a whole-daemon figure. The structural
    phase (about 900 s of CPU) dominates it. It did not show that A1 was
    waiting.

### Control: the trace tells the arms apart

The trace did measure the change, so its null total is not "measured
nothing". Both arms received the same patch:

- pages: 23 in every A0 pass, 51 in every A1 pass;
- `compute` calls: 22 against 48;
- fetch time: 0.14-0.29 s against 0.50-1.37 s;
- the tier log lines appear only in A1.

That difference sits in the overhead columns, and it is 0.66 s.

### Offline order bench (inference only, same process, same texts)

`bench.sh` and the `gm695_bench` test in `a1-trace-and-bench.patch` do the
following:

- They load the production model with the daemon's options (`batch_size`
  1, so each text is one `embed` call).
- They embed every candidate text of a finished index: 7,217 texts, a
  superset of the backfill's.
- They run the texts in id order (A0) and in tier order (A1), in the order
  A0 A1 A1 A0 A0 A1.

| round | arm | wall s | public / private / test s | load1 start → end | CPU_Speed_Limit start → end |
|---|---|---|---|---|---|
| 0 | A0 | 701.5 | 157.0 / 263.3 / 281.1 | 4.0 → 53.8 | 51 → 43 |
| 1 | A1 | 631.4 | 167.1 / 213.4 / 251.0 | 53.8 → 105.0 | 43 → 51 |
| 2 | A1 | 580.2 | 135.4 / 215.2 / 229.6 | 105.0 → 55.0 | 51 → 56 |
| 3 | A0 | 686.9 | 149.4 / 264.9 / 272.6 | 55.0 → 166.0 | 56 → 46 |
| 4 | A0 | 749.2 | 164.3 / 283.4 / 301.5 | 166.0 → 117.7 | 46 → 53 |
| 5 | A1 | 697.9 | 149.4 / 276.4 / 272.1 | 117.7 → 72.2 | 53 → 51 |

- In every adjacent pair, the tier-order round was faster. Total ratios
  A1/A0 were 0.900, 0.845 and 0.932; the median ratio per text was 1.031,
  0.872 and 0.958.
- Load (54-166) and thermal throttling (43-56%) make the size of this
  effect untrustworthy.
- Its sign does contradict the remaining hypothesis, that tier order makes
  ONNX inference slower. No mechanism for that exists anyway. With
  `batch_size` 1, each text is an independent `[1, L]` run
  (`EmbeddingModel::embed_batch_with_stats`, `model.rs:763`), so the order of
  the texts cannot change the work.
- Bench build: `cargo test --release --no-run`, 349 s at load 5.7.

### Why S25 saw +7-10%

- **The confirm3 order favoured A0.** Run start times can be estimated as end
  time minus `real`. In both clean pairs, the A0 run started after a longer
  idle period than its A1 partner:
  - r3 (A0) after about 19 min, r4 (A1) after about 7 min;
  - r5 (A0) after about 6.5 min, r6 (A1) after about 2 min.
- **Thermal state moves the pass time by more than the reported effect.** On
  this machine, the thermal CPU_Speed_Limit ranged from 43 to 100 within
  minutes during these runs. s3 and s4 both started at 100 and ended at 63
  and 53.
- **The S1 runs repeat the S25 pattern.** In the unbalanced A0-first runs,
  r1 (A0, after a 9,450 s wait) was 6% faster than r2 (A1, after 480 s).
- **Balancing the order removes it:** +0.2%.

The thermal explanation is likely but not proven, because confirm3 did not
record pmset. The claim that does not depend on it is this: the tier code
adds about 0.66 s, and the rest of each pass is inference whose length
varies by 14% between runs of the same arm.

## Answers to the brief's code questions (g-mesh, project `g-mesh-wt-gm695`)

| question | answer | g-mesh call |
|---|---|---|
| Who drives the tiered loop | `embedding::backfill::run` is called by `daemon::activation::ActivationCtx::activate` (`activation.rs:230`, after `set_phase(Embedding)`), `cli::init::init` (`init.rs:231,268`), `cli::reindex::reindex` (`reindex.rs:143`), the unit tests in `backfill.rs`, and `core/tests/it/embedding_generation_pipeline.rs` (`Project::walk_then_backfill`). It also has 2 unlinked usages in `plugins/typescript/tests/semantic_runs_no_project_code.rs`. | `find_callers embedding::backfill::run` |
| Who calls the tier scan | `fetch_candidate_page` is called only by `run` and by backfill tests. | `find_callers fetch_candidate_page` |
| Who calls the embedder | `EmbeddingPipeline::compute` is called by `pipeline.rs` (`apply` and tests), `backfill.rs`, `daemon/workspace_reindex.rs` and `watcher/apply.rs`. | `find_callers EmbeddingPipeline::compute` (`files`) |
| Where `handed` is referenced | It is a local in `run` at `backfill.rs:207` and `:235`; the test control is near `backfill.rs:1045-1056`. g-mesh does not index locals, so this came from grep. | grep, one known file |
| What the embedder waits on between batches | Nothing. No embedder worker thread exists: `compute` runs synchronously on the activation thread between two store steps (`Hold::PerStep`). `embed_batch` with the default `batch_size` 1 is a plain loop over `embed`. The trace's `other` (≤ 0.08 s) confirms it. | `find_definition EmbeddingPipeline::compute`, `find_definition embed_batch` (4 declarations), `find_definition EmbeddingPipeline::store`, `get_file_outline core/src/embedding/backfill.rs` |

## Fix options

| option | what | benefit | risk |
|---|---|---|---|
| **F0 (recommended)** | No production change. The measurement gets fixed: `gm441_confirm3.sh` runs a balanced order (A1 A0 A0 A1 per block) and records `pmset -g therm` and load during the pass. GM-441's results note gets a correction: "+7-10%" is replaced by "+0.2% balanced; tier overhead ≈ 0.66 s". | Keeps GM-441's earlier answers with no new code. It removes a false regression from the record and stops future A/Bs from repeating the order bias. | Rests on one balanced block plus the overhead decomposition. A real effect of 1-2% could hide in the 14% noise, but the trace bounds the tier code itself at 0.66 s. |
| F1 | Single scan: one keyset pass collects the ids of each tier, then each tier embeds its own ids in pages. | Saves about 0.4-0.6 s per pass on this corpus: 23 pages instead of 51, and `compute` batches of 256 again. | Holds every candidate id in memory (about 19k ids on the live index, under 2 MB). The design must be reworked: a node that changes mid-pass, and `handed` against an id-list snapshot. Every GM-441 backfill test must be re-pinned. That is a lot of risk for 0.3%. |
| F2 | Filter `visibility` in SQL for the public and private scans; the test rule stays in Rust. | Fewer rows cross into Rust in the public and private scans. | Splits the tier rule between SQL and `text::embed_tier`, which GM-441's design rejected. The gain is at most the fetch time (≤ 1.4 s). |

The inference time cannot be changed by any of these options, because the
order does not affect it. If the owner wants the pass itself shorter, that is
a different task: batching or thread options (GM-453/GM-439).

## Edit map

**F0 (recommended):**

- `eval/embedding/gm441_confirm3.sh`:
  - default `ORDER` → `"A1 A0 A0 A1"` (or repeated blocks);
  - log `pmset -g therm` CPU_Speed_Limit at each run's start and end;
  - add the per-run embedding-phase load mean from the driver's samples.
- `eval/embedding/gm441_confirm.py`:
  - add the 2 s sampler (load1, daemon/descendant CPU, phase) and
    `--no-tier-search`, as in `docs/results/GM-695-data/driver-vs-gm441_confirm.diff`.
- `docs/results/GM-441-prioritised-embedding.md`:
  - "Daemon confirm" verdict bullet and "Things for the owner" item 3: add the
    GM-695 correction and point to this note.
- Must pass shellcheck, which GM-696 enforces for eval scripts.

**F1 (only if chosen):**

- Functions to change, in `core/src/embedding/backfill.rs`:
  - `run` (179-261);
  - `fetch_candidate_page` (417-446): a light id+tier scan plus a fetch by id
    chunk;
  - the tests that pin the scan semantics
    (`the_pass_embeds_public_then_private_then_test_nodes_once_each_when_ids_interleave`,
    `a_no_vector_node_moved_to_a_later_tier_mid_pass_is_handed_to_compute_once`,
    `a_node_the_watcher_embeds_mid_pass_is_not_embedded_again`,
    `the_tiered_pass_stores_the_vectors_of_an_id_order_pass`, from about 888
    on).
- Read for context:
  - in `backfill.rs`: `candidate_tier` (385), `count_candidates` (403),
    `PAGE_SIZE` (114) and the module doc (43-63);
  - `text.rs`: `EmbedTier` (158), `embed_tier` (182), `is_test` (194);
  - `pipeline.rs`: `compute` (449) and `store` (705);
  - `daemon/activation.rs` 172-247.

## Must confirm (tests/measure slices)

- **F0:**
  - Run at least two balanced blocks (A1 A0 A0 A1) on the confirm3 harness
    with the trace, each run load-gated and with its thermal state recorded.
  - Acceptance: A1 − A0 within the spread inside each arm, and the trace's
    non-inference overhead under 1% of the pass.
  - Run the harness change through shellcheck.
- **F1:**
  - Same measurement as F0. The pages and fetch columns must drop to A0's
    level: 23 pages, under 0.3 s.
  - Every GM-441 tier-order test must still fail when its control is reverted.
  - Time-to-full-gold (tier boundary times) must not regress.

## Decisions for the owner

1. Accept F0: no production fix, the cost is inherent and about 0.3%, and
   the harness and GM-441 results get corrected. Or ask for F1 anyway.
2. Choose whether GM-695 closes on this evidence (one balanced block plus
   the trace) or needs the second balanced block from "Must confirm" first.
