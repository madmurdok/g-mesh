# GM-464 S8: product-path latency of the search_code rerank

What a `search_code` call costs with the cross-encoder rerank (ADR 0016,
`docs/architecture/gm-464-ce-rerank.md`) on and off, end to end through the
product path, at 4 and 1 ONNX intra-op threads.

## Setup

- **Build:** release, commit f18a197 (`feat/GM-464-ce-rerank`), all workspace
  binaries (`g-mesh`, `g-mesh-plugin-rust`, `g-mesh-plugin-python`).
- **Path:** `g-mesh mcp-shim` over stdio, which bootstraps the per-project
  daemon, on an index of g-mesh's own checkout (the branch worktree). Each
  call is timed from writing the JSON-RPC `tools/call` to reading its
  response, so it includes the query embedding, the vector scan, the
  cross-encoder (when on), the verdict and serialization. Default `limit`
  (20), so every timed call is a complete first page and reranks its top 30.
- **Isolation:** a separate `G_MESH_HOME` (copied embedding cache, the
  embedding model by symlink), `G_MESH_RERANK_MODEL_DIR` pointed at the eval
  copy of `ms-marco-MiniLM-L6-v2` (fp32, 91,011,230 bytes), plugin roots
  pinned to the worktree's `plugins/`. The user's own daemons were not
  touched.
- **Arms**, each in a fresh daemon:
  - `off`: `G_MESH_RERANK=off`, the branch binary.
  - `t4`: rerank on, the branch binary. `intra_threads()` is
    `min(4, physical cores)`; this machine has 4 physical cores (8 logical),
    so 4.
  - `t1`: rerank on, a binary built from the same commit with
    `MAX_THREADS = 1` in `core/src/embedding/rerank.rs`, in a throwaway
    worktree (never committed). The design has no runtime thread knob.
- **Queries:** the first 30 `positive` queries of
  `eval/embedding/queries/g-mesh.jsonl` (natural-language phrases and
  sentences).
- **Per arm:** wait for `index.phase == ready`; gate; one first call (timed
  alone: includes the cross-encoder load in the rerank arms); one untimed
  warm-up pass over the 30 queries; 3 timed passes (n = 90).
- **Gate:** 1-minute load average below 4, held for 30 s, sampled every 5 s;
  total waiting across the run capped at 60 minutes, after which the
  remaining arms run anyway and are marked. Order `off t4 t1 t1 t4 off`.
- **Machine:** 4-core Intel MacBook, AC power, battery charged. The load
  average was ~45-60 before the run (other work); the gate opened for the
  first four arms, then the 60-minute cap ran out (t1#4 alone waited 46 min)
  and the last two arms ran at a load of ~5.
- **Script:** `eval/embedding/gm464_latency.py run` (the docstring lists its
  environment). Raw per-call samples: `eval/embedding/work/runs-gm464-latency/arms.jsonl`
  (not committed).

## Results

Latency in milliseconds per `search_code` call. "Reordered" counts pages whose
rows are not in descending cosine order (the arm control: the rerank moved
rows). "Daemon cpu/wall" is the daemon's CPU seconds over the timed passes
divided by their wall time (the thread control). `therm` is pmset's
CPU_Speed_Limit/CPU_Scheduler_Limit at start and end.

| arm | n | p50 | p95 | first call | reordered | daemon cpu/wall | gate | load start/end | therm start, end | time -p real/user/sys (s) |
|---|---|---|---|---|---|---|---|---|---|---|
| off#1 | 90 | 68 | 85 | 666 | 0/90 | 3.75 | open | 3.83/4.16 | 100/100, 100/100 | 203.56/0.14/0.17 |
| t4#2 | 90 | 606 | 869 | 1557 | 90/90 | 3.98 | open | 3.85/12.58 | 100/100, 75/100 | 225.81/0.16/0.18 |
| t1#3 | 90 | 1130 | 1743 | 2698 | 90/90 | 1.15 | open | 3.67/5.04 | 100/100, 100/100 | 337.56/0.19/0.20 |
| t1#4 | 90 | 1104 | 1680 | 2826 | 90/90 | 1.19 | open | 3.27/6.53 | 100/100, 100/100 | 2909.14/0.33/0.60 |
| t4#5 | 90 | 461 | 684 | 1474 | 90/90 | 4.24 | closed (cap) | 5.07/5.78 | 100/100, 100/100 | 349.28/0.17/0.23 |
| off#6 | 90 | 63 | 83 | 663 | 0/90 | 3.79 | closed (cap) | 5.40/10.65 | 100/100, 100/100 | 11.86/0.15/0.15 |

Pooled over both rounds:

| arm | n | p50 | p95 |
|---|---|---|---|
| off | 180 | 65 | 85 |
| t4 | 180 | 510 | 805 |
| t1 | 180 | 1123 | 1715 |

`uptime` per arm (start -> end):

- off#1: `13:09 load averages: 3.83 19.83 39.75` -> `4.16 19.38 39.36`
- t4#2: `13:12 load averages: 3.85 13.16 33.37` -> `12.58 13.65 32.05`
- t1#3: `13:16 load averages: 3.67 8.88 26.17` -> `5.04 7.48 23.06`
- t1#4: `14:05 load averages: 3.27 5.01 6.56` -> `6.53 5.74 6.64`
- t4#5: `14:12 load averages: 5.07 5.52 6.30` -> `5.78 5.69 6.32`
- off#6: `14:13 load averages: 5.40 5.61 6.29` -> `10.65 6.70 6.67`

`time -p` wraps one arm's driver process (Python plus the shim); the daemon is
detached and not its child, so `user`/`sys` near zero against seconds of
`real` is the driver waiting on the daemon, as expected. `real` also includes
the gate wait and the index catch-up (~3 s per arm). The daemon's own CPU is
the cpu/wall column.

## Reading

- **Controls hold.** Every rerank-on page was reordered (90/90) and no
  rerank-off page was (0/90), so the arms differ in what they claim to. The
  daemon used ~4 cores in `t4` and ~1.2 in `t1` (the query embedding is not
  capped), so the 1-thread build is what it claims.
- **Cost of the rerank at 4 threads:** about +450 ms at p50 and +720 ms at
  p95 over the embedding-only path (65/85 ms). Under the open gate (t4#2):
  606/869 ms. t4#5 ran outside the gate and was faster (461/684 ms), and t4#2
  ended throttled (speed limit 75) with the load at 12.6, so the true idle
  figure is likely between the two; this run cannot pin it closer.
- **Against the design's estimate (~210/700 ms p50/p95 at 4 threads):** the
  p50 is 2.2-2.9x the estimate, the p95 1.0-1.2x. Reported, not acted on (S8
  brief).
- **1 thread:** ~1.1 s p50, ~1.7 s p95, both rounds under the gate and within
  3% of each other: about 2.2x the 4-thread figures.
- **First call** (fresh daemon, includes loading the 91 MB cross-encoder):
  ~1.5 s at 4 threads and ~2.7-2.8 s at 1 thread, against ~0.66 s for the
  first embedding-only call.
- **Trust:** ratios are trustworthy for the gated arms only (off#1, t4#2,
  t1#3, t1#4). The machine was coming down from a load of ~45-60; the
  1-minute gate opened while the 15-minute average was still 26-40 for the
  first three arms. An idle-machine rerun would tighten the t4 numbers.

## Reproduce

```sh
# release binaries for t4 (this commit) and t1 (MAX_THREADS = 1 in a worktree)
cargo build --release --workspace --bins
# dirs holding g-mesh + g-mesh-plugin-{rust,python}: BIN_T4, BIN_T1
H=/short/path/home BIN_T4=... BIN_T1=... python3 eval/embedding/gm464_latency.py run
cat eval/embedding/work/runs-gm464-latency/summary.md
```

`H` must be short: the daemon's AF_UNIX socket lives under it. It needs
`models/jina-embeddings-v2-base-code` (and, to index faster, a copy of
`~/.g-mesh/embedding-cache`).
