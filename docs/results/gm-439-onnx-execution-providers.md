# GM-439: ONNX Runtime execution providers (CoreML) on the eval path

Verdict: **CoreML loses on this machine. Do not ship it.** The encoder pass
is 5.8x slower than CPU (4.4x-6.7x across the individual runs) and quality
is unchanged. The early stop fired, so rounds 2-3 and the other five quality
corpora were skipped. An Apple Silicon host with a Neural Engine is the case
this machine cannot measure.

## Method

- Branch `feat/GM-439-onnx-execution-providers`, tip `2ab9786`. Two release
  binaries from that commit: the default build is the **cpu arm**; the build
  with `--features ep-coreml` (separate target dir, seeded by `cp -c -R`) is
  the **coreml arm** (`--ep coreml`, GPU+CPU). `otool -L`: the default binary
  links no CoreML, the feature binary does.
- Machine: Intel i7-1068NG7 (8 threads), macOS 26.6.2, no Neural Engine,
  shared with two other agents. Variant `jina-v2-base-code-fp32-r`, corpus
  `g-mesh` (6774 nodes), `--embed-only --force`, `G_MESH_EMBEDDING_CACHE=off`,
  `/usr/bin/time -lp`. Models and snapshots read from the main checkout's
  `eval/embedding/work`; queries and config from the branch.
- Order: the balanced block `cpu coreml coreml cpu`. Each run waited for
  load1 < 7 before starting; never two timed runs at once. `pmset -g therm`
  `CPU_Speed_Limit` and `uptime` recorded at the start and end of every run.
- Early stop (note's rule): round-1 CoreML median > 1.5x CPU median.
  **Deviation:** the block was cut after its third run. The second CoreML run
  (about 93 min) was killed before finishing and not used, because the first
  already showed 6.6x against thermal swings that move the CPU pass by 1.3x;
  a second CoreML run could not flip the verdict. The fourth run (cpu) was
  kept. The script was restarted once after killing the run (CPU runs 1-2 are
  from the first launch); one earlier launch failed in 3 s on a script bug
  and ran nothing.
- Dry run of the whole script on `task-tracker-mcp` (157 nodes) preceded it.

## Pass time (`embedNodesMs`), corpus g-mesh, 6774 nodes

| Run | Arm | embedNodesMs (s) | real (s) | user / sys (s) | max RSS | loadMs (s) | thermal start/end | load1 before/after |
|---|---|---|---|---|---|---|---|---|
| r1-p1 | cpu | 844.0 | 847.9 | 3289.9 / 17.1 | 1.57 GB | 1.49 | 100 / 58 | 4.97 / 6.92 |
| r1-p2 | coreml | 5587.5 | 5595.6 | 17858.4 / 2087.8 | 10.02 GB | 3.74 | 58 / 48 | 6.85 / 17.61 |
| r1-p4 | cpu | 1082.0 | 1085.8 | 4021.8 / 28.0 | 1.71 GB | 1.49 | 100 / 51 | 5.58 / 18.86 |

- CPU median (2 runs) 963.0 s; CoreML 5587.5 s (one run). **Delta +4624 s,
  ratio 5.80x** (5587/1082 = 5.2x to 5587/844 = 6.6x).
- The two CPU runs differ by 28% (844 vs 1082 s). Both ended with the
  thermal limit down to 51-58, and p4 ended at load1 18.9 (neighbours), so the
  CPU spread is the machine, not the arm. The CoreML run itself ended at
  limit 48, which may have cost it some time; it cannot explain 5-6x.
- Setup `loadMs`: CPU 1.5 s, CoreML 3.7 s (the quality runs: 1.5 / 2.9 s).
  Setup is not the cost.
- The untimed-for-the-verdict quality runs, same gate, agree: CPU
  `embedNodesMs` 839.1 s vs CoreML 4729.3 s (5.6x); median query embed
  15.6 ms vs 233.5 ms (15x). Max RSS 1.84 GB vs 10.02 GB.
- First call per shape: `embedNodesMs` includes every per-length CoreML
  compile, as production would pay it. The encoder ran one call per text
  (6774 calls). Per node: CoreML 0.70-0.83 s vs CPU 0.12-0.16 s on g-mesh;
  on the 157-node dry run CoreML 0.55-0.73 s vs CPU 0.07 s. Per-node cost
  does not fall with 43x more nodes, so the cost is not a one-off compile
  amortised over the corpus; steady state is slow too (user+sys 5.4x CPU's).

## Placement

Each CoreML run's stderr holds the `GetCapability` line, identical in the
timed run and the quality run: `number of partitions supported by CoreML: 97
number of nodes in the graph: 1012 number of nodes supported by CoreML: 795`.
97 partitions, 795 of 1012 nodes (78.6%), matching the design-time probe. The
many partitions mean frequent CPU/GPU hand-offs, the likely cause of the
slowdown (not separately isolated; the `coreml-cpu-only` diagnostic arm was
dropped since the first two decided).

## Quality (g-mesh corpus only, per the early stop)

- `debug-embed-eval report` (reference `jina-v2-base-code-fp32`, control
  `random`), CPU run vs CoreML run of `-fp32-r`: **recall@10 0.360 and MRR
  0.246 in both**, identical to 3 decimals; validity and floor parity pass;
  both arms report the same verdict. Each report was run with the g-mesh
  corpus only (the reference/random runs linked down to it).
- `compare` CPU vs CoreML, 6774 nodes: 0 bit-identical, **max L2 5.449e-3,
  min cosine 0.999985**; all 6774 exceed the default 1e-4 L2 tolerance, so
  `compare` exits 1. That is the expected FP16/GPU difference (probe cosine
  ~0.99999), not a defect; the ranking metrics above are what "unchanged"
  means in the note. Of 363 queries, the order of the full hit list changed
  for 356 and the top-1 hit for 0.
- The other five corpora were not run (early stop; a slower EP will not ship).

## Notes on validity

- Single CoreML pass; no CoreML repeat. The conclusion rests on the size of
  the gap (4.4x-6.7x against a 1.3x CPU spread), the same-gate quality-run
  pair (5.6x) and the dry-run pair (about 8-10x on 157 nodes), not on a
  median of CoreML runs.
- Not measured: Apple Silicon with a Neural Engine, `coreml-cpu-only`.
- Raw outputs (timing.tsv, stderr per run, reports) are in the scratchpad of
  the measure slice, not committed.
