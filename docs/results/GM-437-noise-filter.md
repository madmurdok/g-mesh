# GM-437: embedding noise filter measurement

Slice S15 (measure). Arms: R = `jina-v2-base-code-fp32`, R+N =
`jina-v2-base-code-fp32-noise` (production's `is_noise` filter), plus an int8
pair for cost only (`jina-v2-base-code-int8`, `jina-v2-base-code-int8-noise`).
Protocol: `docs/design/GM-437-embedding-noise-filter.md`, "Measurement protocol".

**Result: the filter ships under the owner's rule.** It removes 17.2% of the
vectors on g-mesh and 16.9% on ripgrep (1.2% to 17.2% across the six corpora)
with no recall loss: pooled recall@10 is +1.7 points (lower bound +0.8), MRR
+0.017 (lower bound +0.011), no corpus loses recall@10, and Q1-Q5 all pass.
The pass-time gain cannot be resolved on this machine: run-to-run noise on one
arm (up to 60%) is larger than the 17% of work removed.

## Setup

- Commit `1024e362da63fdb5665afea9c8067eb75b8f7aae`, branch
  `perf/GM-437-embedding-noise-filter`; `cargo build --release -p g-mesh` once.
- Machine: Intel i7-1068NG7 (4 cores, 8 threads), 32 GB, macOS 26.6.2; the
  owner's working machine, other apps open.
- Harness: `g-mesh debug-embed-eval`, snapshots and the int8 model files
  cloned (`cp -c`) from the main checkout's `eval/embedding/work` (read only).
  Runs written to a scratch directory, not into the tree.
- Quality: all six corpora, 500 queries, floors refitted per arm, paired
  bootstrap seed 398, 10,000 resamples, one-sided 95% bounds. Controls
  (random, shuffled, words-shuffled, bm25) re-run on the same build.
- Cost: `G_MESH_EMBEDDING_CACHE=off /usr/bin/time -lp g-mesh debug-embed-eval
  run --variant <v> --corpus <c> --embed-only --force`, 3 rounds, order per
  round and corpus: fp32, fp32-noise, int8, int8-noise. Each run gated on
  1-min load < 4.0 (waited up to 20 min; no run hit the limit; waits of 0 to
  690 s).

### Deviations and harness findings

1. **Query gm-m074 reverted for the run.** Commit `c0168ca` (release base)
   repointed it to `QueryShapes::refused_by_all`, which is not in the pinned
   g-mesh snapshot (805686f), so `run` fails on g-mesh for every variant. The
   run used the line from `c0168ca^` (`is_module_specifier`, present in the
   snapshot); the same query set for both arms. The tree is restored.
2. **Gold resolution is correct.** `resolve_expected` indexes the full
   snapshot node list; a gold on a filtered node resolves to no candidate and
   scores as a miss (`first_expected_rank` = none). The quality numbers are
   not biased toward the filter.
3. **Label bug in the filtered arm (needs a fix).** `query_labels` is called
   with `expected_nodes`, the candidate-set subset of the gold, so a query
   whose gold includes a filtered node gets different `targetDoc`/`pathOverlap`
   labels from R (8 of 2,279 queries differ), and `report` refuses to merge
   (`rg-m117 has labels ... Doc ... Mixed`). Workaround for this measurement:
   the R+N `rankings.jsonl` labels were overwritten with R's (originals kept
   as `.orig` in the scratch directory). Labels feed only the split tables,
   not the primary metrics. `chance_inputs` in `load_arm` also uses the
   reduced `expected.len()`.

## Arm check

| corpus | R nodeCount | R+N nodeCount | cut | nodeIdsSha256 differs |
|---|---|---|---|---|
| g-mesh | 6,774 | 5,608 | -17.2% | yes (fp32 and int8 pairs, quality and timed runs) |
| ripgrep | 3,428 | 2,850 | -16.9% | yes |
| excalidraw | 2,758 | 2,726 | -1.2% | yes |
| gin | 1,547 | 1,478 | -4.5% | yes |
| py-requests | 961 | 830 | -13.6% | yes |
| task-tracker-mcp | 157 | 149 | -5.1% | yes |

Expected 6,774 / 5,608 and 3,428 / 2,850 observed exactly. Every pair differs,
so every pair measured something. The int8 pairs have the same counts.

## Quality (fp32, R vs R+N)

Validity: random and shuffled controls separate (gaps 63.2 and 61.7 points),
floor parity passes.

| | R | R+N | delta (lower bound, one-sided 95%) |
|---|---|---|---|
| recall@10 pooled | 0.6375 [0.600, 0.675] | 0.6550 [0.617, 0.693] | +1.7 pts (+0.8) |
| MRR pooled | 0.424 [0.392, 0.457] | 0.441 [0.408, 0.474] | +0.017 (+0.011) |
| recall@5 | 0.533 | 0.560 | |
| recall@1 | 0.328 | 0.338 | |
| confident-wrong, combined | 46.4% | 44.8% | -1.4 pts (upper +1.0) |
| false alarm, held-out | | | -1.2 pts (upper +0.0) |

recall@10 per corpus (R -> R+N): excalidraw 0.62 -> 0.62, g-mesh 0.36 -> 0.42,
gin 0.73 -> 0.76, py-requests 0.71 -> 0.71, ripgrep 0.36 -> 0.36,
task-tracker-mcp 0.88 -> 0.90. Per language: go 0.73 -> 0.76, python 0.71 ->
0.71, rust 0.36 -> 0.39, typescript 0.75 -> 0.76. No corpus or language loses.
The report's per-language bootstrap covers false alarm only (go 0.0, python
-5.0, rust 0.0, typescript 0.0 points); recall deltas have the pooled bound
above and the Q3 worst-language gate (+0.0, passes).

Floors refitted (go / python / rust / ts): R 0.57 / 0.57 / 0.55 / 0.53; R+N
0.57 / 0.51 / 0.54 / 0.53. Held-out false alarm: R 29.4 / 10.0 / 14.3 / 7.7%;
R+N 29.4 / 5.0 / 22.2 / 7.7% (rust n=7, one query is 14 points). Gates Q1-Q5
all pass; the report's verdict is "Undecided" only because no cost file was
given.

By target: undocumented ("sig") targets gain most, recall@10 0.548 -> 0.587
(delta +3.8 pts, bounds +1.0 to +7.4). Documented targets +1.0 pt.

Test functions and name-only modules are gone from the candidate set, and the
eval has no queries aimed at them, so this cannot show what `search_code`
loses for a user who searches for a test (design question 1).

## Cost, fp32

Medians of 3 rounds; user/real 3.60-3.92 in all but one run (no stalls).

| corpus | arm | vectors | vectors.bin bytes | median embedNodesMs | median max RSS MB | load1 before | load1 after |
|---|---|---|---|---|---|---|---|
| g-mesh | R | 6,774 | 20,809,728 | 678,415 | 773 | 3.71-3.85 | 6.7-14.9 |
| g-mesh | R+N | 5,608 | 17,227,776 | 606,463 | 768 | 3.65-3.88 | 10.7-120.0 |
| ripgrep | R | 3,428 | 10,530,816 | 96,571 | 707 | 3.77-3.87 | 5.1-5.4 |
| ripgrep | R+N | 2,850 | 8,755,200 | 93,694 | 707 | 3.47-3.83 | 5.4-7.6 |

Per-round embedNodesMs (r1, r2, r3): g-mesh R 751,010 / 462,238 / 678,415, R+N
770,358 / 524,358 / 606,463; ripgrep R 102,609 / 96,571 / 88,864, R+N 93,694 /
122,797 / 89,842.

## Cost, int8 (owner-approved addition, no quality arm)

| corpus | arm | vectors | vectors.bin bytes | median embedNodesMs | median max RSS MB | load1 before | load1 after |
|---|---|---|---|---|---|---|---|
| g-mesh | int8 | 6,774 | 20,809,728 | 612,801 | 775 | 3.73-3.94 | 8.4-13.7 |
| g-mesh | int8+N | 5,608 | 17,227,776 | 547,468 | 760 | 3.68-3.88 | 8.8-18.4 |
| ripgrep | int8 | 3,428 | 10,530,816 | 145,390 | 720 | 3.60-3.86 | 6.3-70.9 |
| ripgrep | int8+N | 2,850 | 8,755,200 | 100,791 | 737 | 3.61-3.94 | 5.3-15.8 |

Per-round embedNodesMs: g-mesh int8 983,419 / 612,801 / 541,143, int8+N
760,436 / 547,468 / 529,377; ripgrep int8 145,390 / 222,859 / 90,756, int8+N
100,791 / 139,867 / 89,926.

### What the timing shows

- Median deltas (R+N vs R): fp32 g-mesh -10.6%, ripgrep -3.0%; int8 g-mesh
  -10.7%, ripgrep -30.7%. The ripgrep int8 figure is noise: R's round 2 took
  222,859 ms against 90,756 ms in round 3.
- The same arm varies 46% (fp32 g-mesh R), 82% (int8 g-mesh R) and 146%
  (int8 ripgrep R) between rounds, with user/real near 3.6-3.9, so the
  process kept its threads busy while other work on the machine took cores.
  Load before each run was gated below 4, but load after rose to 120 in one
  run and the owner's apps were running throughout.
- Best round per arm (least disturbed): g-mesh fp32 462,238 vs 524,358 ms
  (+13%), int8 541,143 vs 529,377 (-2.2%); ripgrep fp32 88,864 vs 89,842
  (+1.1%), int8 90,756 vs 89,926 (-0.9%). Against an expected -17% from the
  vector count alone, these do not confirm a pass-time gain; they do not
  refute one either.
- Vectors (the deterministic part): -17.2% g-mesh, -16.9% ripgrep, and
  `vectors.bin` shrinks by the same share (3.58 MB and 1.78 MB).
- Max RSS is unchanged within 2% (peak is the model and batch, not the vector
  count).

## Conclusion against the ship rule

"Fewer vectors at 0 recall loss ships it, even if pass time gains are
small": met. Vectors -17% on the two largest cuts, recall@10 and MRR not
lower in any corpus (pooled +1.7 points and +0.017), Q1-Q5 pass, int8 pair
cuts the same vectors. The pass-time saving is unmeasured on this machine; a
quiet-machine rerun would be needed to put a number on it. Before shipping:
fix the label computation in the filtered arm (finding 3) so `report` needs no
workaround, and repoint gm-m074 at a symbol in the pinned snapshot.
