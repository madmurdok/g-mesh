# GM-422: confirmatory int8 study, frozen query set

The query set of the confirmatory study
([protocol](../architecture/embedding-eval-int8-confirm.md)), frozen on
2026-09-29 before any embedding run on it.

The queries were authored blind, one fresh agent per corpus, following D3
and section 3 of the protocol. A fresh verifier then checked them: the
expected sets, 60 sampled positives (1 error, since fixed), 18 absent
queries re-grepped, near-duplicates against GM-398, and blindness. The
verifier's fixes were applied before the freeze, as recorded in the
protocol's Deviations section:

- 10 documented constants that were wrongly skipped were restored;
- 3 ripgrep expected sets were completed;
- 1 gin query was retargeted.

## Files

| file | sha256 | queries | positive | absent |
|---|---|---|---|---|
| `eval/embedding/confirm/queries/gin.jsonl` | `50757662698de019c9450615dee79b48196b1373ff828bcbf25e4ed03718ba3b` | 250 | 200 | 50 |
| `eval/embedding/confirm/queries/py-requests.jsonl` | `7fedbf196f826e17776f0a330e11d9cd50f8b227159880b149fc693cd81670ce` | 188 | 150 | 38 |
| `eval/embedding/confirm/queries/ripgrep.jsonl` | `51b126d72394ddc8b1076697bb077052e0e63b1d37a9414cd00ceecc3705e3cc` | 94 | 75 | 19 |
| `eval/embedding/confirm/queries/g-mesh.jsonl` | `39a6d97af21589eaa76fb1d98681bb90f9aa8784597d106e5fe64ee3d51d26b1` | 94 | 75 | 19 |
| `eval/embedding/confirm/queries/excalidraw.jsonl` | `0e703f65f9a616028c9b9cf56cc8c89635119a9e7e69280538cbd9537da1eb41` | 109 | 90 | 19 |
| `eval/embedding/confirm/queries/task-tracker-mcp.jsonl` | `c698ace39f4a2c337702e197cd022c0e5a1541665f226cef4a43d043e25a8ee2` | 79 | 60 | 19 |
| **total** | | **814** | **650** | **164** |

Any later change to these files invalidates the study.

## Runs (S7)

2026-09-29, branch `docs/GM-422-int8-q5-floor-fit` at `d696ee5` (queries
unchanged: all six sha256 re-checked against the table above before the
runs). `cargo build --release --bin g-mesh`, then
`eval/embedding/confirm/setup_work.sh $W` and the runbook loop, as one
script, in runbook order. Runs are in the gitignored
`eval/embedding/confirm/work/runs/`.

| variant | real s | user s | sys s | uptime load (1/5/15 min) before | embedNodesMs (all 6 corpora) |
|---|---|---|---|---|---|
| jina-v2-base-code-fp32 | 65.57 | 200.59 | 3.25 | 33.90 / 57.37 / 33.23 | 0 |
| jina-v2-base-code-int8 | 53.31 | 155.10 | 1.89 | 16.10 / 47.55 / 31.32 | 0 |
| random | 7.36 | 6.83 | 0.22 | 19.94 / 42.98 / 30.61 | 0 |
| shuffled | 6.94 | 6.35 | 0.22 | 18.82 / 42.37 / 30.46 | 0 |

The machine was loaded (other work running); `user` well above `real` for
the model arms is query embedding on several threads, not waiting. The
timings are not a cost measurement; cost stays GM-398's.

### Controls (blind to int8)

`int8_confirm_report.py` gained `--controls-only`: controls A and 2-5 only;
the int8 verdict arm, the int8-at-fp32-floors view and the verdict line are
not computed, and int8-derived arms (control 3's int8 lines, control 4's
planted-harm arms) print pass/fail without figures. The gated verdict is
left to S9, which runs the script without the flag.

```sh
python3 eval/embedding/int8_confirm_report.py \
    --old-runs $W/runs --old-eval-dir eval/embedding \
    --runs eval/embedding/confirm/work/runs --eval-dir eval/embedding/confirm \
    --controls-only --json eval/embedding/confirm/work/controls.json
```

Exit 0, "controls A, 2, 3, 4, 5: OK". Confirm queries: positives go 200,
python/rust/typescript 150 each; absent 50/38/38/38.

| control | result |
|---|---|
| A frozen-floor reproduction (GM-398 runs) | pass: refitted floors equal the frozen ones; int8 (GM-398) Q5 +2.1 [-1.4, +6.2]; D7 gaps 62.8/61.8, ratios 0.008/0.024, random 0.005 vs chance 0.015 |
| 2 vectors.bin sha256 == GM-398's | pass: same for fp32, int8, random, shuffled |
| 3 D7 broken arms | pass: fp32 vs random gap 60.9, ratio 0.027; fp32 vs shuffled gap 62.3, ratio 0.005; bounds separate; int8 vs random and vs shuffled pass (figures withheld); random recall@10 0.017 vs chance 0.0122 (limit 3x + 0.02); random and shuffled fail the gates as candidates |
| 4 planted harm, seed 4223 | pass: int8-harm10 fails Q4 and Q5; int8-harm5 fails Q4 and Q5 (reported) |
| 5 null arm fp32 vs fp32 | pass: exactly 0 on Q1-Q5 (n4 814, n5 176), passes |

## Verdict (S9)

**GO: int8 passes every gate under the pre-registered rule.** Per the owner
decisions of 2026-09-29, this verdict replaces GM-398's int8 verdict
(Fail: Q5), and jina-v2-base-code int8 is switched in. The decision is
recorded in [ADR 0011](../adr/0011-embedding-model-int8.md).

2026-09-29, branch `docs/GM-422-int8-q5-floor-fit` at `ffdb249`. All six
query sha256 re-checked against the Files table first: all match. The
runbook's "Gated verdict" command, without `--controls-only`:

```sh
python3 eval/embedding/int8_confirm_report.py \
    --old-runs $W/runs --old-eval-dir eval/embedding \
    --runs eval/embedding/confirm/work/runs --eval-dir eval/embedding/confirm \
    --json eval/embedding/confirm/work/report.json
```

Exit 0; `uptime` before: load 3.72 / 9.22 / 17.26; `time -p`: real 6.36,
user 4.73, sys 1.20. The report reads stored rankings only.

### Controls (same run)

"controls A, 2, 3, 4, 5: OK". Control A, 2, 3's fp32 lines and 5 give the
same figures as the blind S7 run above. Figures that S7 withheld:

- **3, int8 vs broken arms**: vs random gap 61.5, ratio 0.027; vs shuffled
  gap 62.9, ratio 0.005; bounds separate: pass.
- **4, planted harm** (seed 4223; harm5 shares Q5 5.5-6.2%, Q4 8.2-18.1%,
  doubled for harm10):

| arm | Q4 | Q5 | result |
|---|---|---|---|
| int8-harm10 | +13.4 [+11.2, +15.7] | +4.2 [-1.2, +9.9] n=125 | fails Q4 and Q5 (required): pass |
| int8-harm5 | +7.6 [+5.7, +9.4] | +3.0 [-1.6, +7.7] n=143 | fails Q4 and Q5 (reported) |

The planted Q5 effect reads smaller than its nominal size (+4.2 against
int8's own -2.0, not +10). A Q4-harmed right-first positive is demoted and
leaves the Q5 pairs (Deviation 5), so part of the planted harm goes to Q4.
The control still does its job: both harm arms fail Q5 on the bound.

### Gates: int8 vs fp32, frozen GM-398 floors, rule B

814 new queries (650 positives, 164 absent), every one held-out. Δ = int8 -
fp32; points for Q1, Q3-Q5, raw for Q2. Bounds are one-sided 95%
(SplitMix64, seed 398, 10,000 resamples), languages weighted equally.

| gate | point | bound | limit | result |
|---|---|---|---|---|
| Validity (D7, control 3) | int8 above random by 61.5, shuffled by 62.9 | bounds separate | gap >= 20, ratio <= 0.25 | pass |
| Q1 recall@10 | +0.6 | lower -0.6 | point >= -2.0, lower >= -5.0 | pass |
| Q2 MRR | +0.005 | lower -0.004 | point >= -0.02, lower >= -0.05 | pass |
| Q3 recall@10 per language | min 0.0 | - | >= -10 in every language | pass |
| Q4 confident-wrong (n = 814) | +1.5 | upper +2.9 | upper <= +5 | pass |
| Q5 false alarm, pooled (n = 161) | -2.0 | upper +1.2 | upper <= +5 | pass |
| Cost (GM-398/S14, D11) | size 0.26x, RSS 0.48x, pass time 0.69x, query 0.54x | - | one of time <= 0.60, size <= 0.50, RSS <= 0.70; none > 1.10 | pass |

Q4's lower bound is +0.1: int8 is measurably, slightly more often
confidently wrong at its shipped floors. Rule B gates on the upper bound
only, so this passes; rule A (D9 unchanged) would have failed it on the
point. At fp32's floors int8's Q4 is -0.9 [upper +0.4] (secondary view
below), so the excess comes from int8's lower TypeScript and Python floors,
not from its rankings.

**Per language** (Q3 is gated per language; Q5 per language is reported,
not gated; Q4 is gated combined only and the script does not split it):

| language | Q3 Δ recall@10 | Q5 Δ false alarm [lower, upper] | Q5 pairs | int8 worse / better |
|---|---|---|---|---|
| go | +0.5 | +2.1 [-4.2, +8.3] | 48 | 2 / 1 |
| python | 0.0 | -8.8 [-17.6, -2.9] | 34 | 0 / 3 |
| rust | +2.0 | +4.2 [0.0, +12.5] | 24 | 1 / 0 |
| typescript | 0.0 | -5.5 [-10.9, -1.8] | 55 | 0 / 3 |

**Go.** The accepted Go false-alarm excess (owner decision 4) shows as
+2.1 points, upper bound +8.3, from 2 queries worse and 1 better. The
protocol expected about +6.6. A per-language +5 gate would fail Go (and
Rust) on the bound; the gate is pooled by decision.

### Secondary views (not gated; they do not change the verdict)

- **int8 at fp32's floors** (S1's shared-floor view): Q1 +0.6 [-0.6],
  Q2 +0.005 [-0.004], Q3 min 0.0, Q4 -0.9 [upper +0.4], Q5 -0.1
  [-3.5, +3.3]. Passes; agrees with the gated verdict.
- **Old + new, descriptive only, does not decide.** GM-398's runs (harness
  parity split: its held-out half for Q4/Q5) plus the new queries, at the
  frozen floors, through the same `gates` function (a scratch script,
  not committed). Pooling them for a verdict is what section 4 forbids.

| view | Q1 | Q2 | Q3 min | Q4 (n) | Q5 (n) |
|---|---|---|---|---|---|
| old + new, not gated | +0.6 [-0.3] | +0.002 [-0.004] | -0.4 (ts) | +0.9 [upper +2.1] (1,075) | -0.6 [upper +1.9] (228) |

  Per language Q5 (old + new): go +4.7 [0.0, +10.9] n=64 (4 worse / 1
  better), python -5.6 [-11.1, -1.9] n=54, rust +3.3 [0.0, +10.0] n=30,
  typescript -5.0 [-8.8, -1.2] n=80.
- **Harness `report`, floors refit** (section 4; runbook "Secondary"
  command; load 3.56 / 7.35 / 15.41, real 0.60, user 0.31, sys 0.03). Floors
  are refit on the parity fit half of the new queries plus the mechanical
  ones: fp32 go/python/rust/typescript 0.54 / 0.46 / 0.56 / 0.48, int8
  0.51 / 0.45 / 0.55 / 0.48. Its verdict is **Undecided**: the harness's
  floor-parity check finds fp32's refit Go (0.54) and Python (0.46) floors
  more than 0.03 from the floors shipped in `similarity.rs` (0.59, 0.57).
  Its gates, under D9 as written (point <= 0 for Q4/Q5): Q1 +0.6 [-0.6],
  Q2 +0.005 [-0.004], Q3 worst 0.0, Q4 +1.2 [upper +3.3] fails on the point
  (it would pass rule B's bound), Q5 -5.1 [upper +0.3]. **This disagrees
  with the gated verdict** (Undecided, and Q4 fails rule A), which the
  protocol requires to be reported. It does not change the verdict
  (section 4). The floor-parity miss is against the shipped `similarity.rs`
  floors, which GM-398's own fit had already moved away from (0.56 / 0.58 /
  0.56 / 0.55); the frozen-floor path checks its floors by control A
  instead.

### Context: S8's gap analysis

[gm-422-int8-gap.md](gm-422-int8-gap.md) found no quantization-specific
loss at rank or margin: int8 lowers cosine scores by a common-mode median
of -0.004, and floor crossings follow the frozen floor gaps (Go +0.01,
TypeScript -0.02) rather than the score shift. That is consistent with
the per-language Q5 signs above. It is context only; it did not enter the
verdict.

## D10 (agent-level)

The agent-level veto of [embedding-eval.md](../architecture/embedding-eval.md)
D10, applied to int8 before the switch (GM-422/S12). This subsection's task
list was written and committed before any run.

### Task list (fixed before the run)

Arms: R = g-mesh `release-3.17.0` at `5a8b5a7` (fp32), C = the same commit
with int8 weights and ADR 0011's floors. Both `gmesh-configured`, nothing else.

**Source 1, GMB-150's saved logs.** GMB-150's records were written by the
token-economy harness, not by the `hooks/tool-use-logger.mjs` hook that
`scripts/analyzeToolUseLog.ts` parses, so the equivalent cut was taken
directly from its saved transcripts
(`results/transcripts/2026-08-26T12-31-03-311Z` and the partial
`2026-08-25T23-03-13-702Z`): every task with at least one
`mcp__g-mesh__search_code` call in a `gmesh-configured` transcript. 18 tasks
(110 calls):

| task | runs with search_code / 5 | calls |
|---|---|---|
| ex-implement-mutateelement-elbow-zero-position | 5 | 5 |
| ex-semantic-arrow-endpoint-grid-align | 5 | 5 |
| ex-semantic-arrow-zorder-above-bound | 5 | 13 |
| ex-semantic-cjk-charclass-check | 5 | 5 |
| ex-semantic-collab-conflict-keep-local | 5 | 6 |
| ex-semantic-drag-text-anchor | 5 | 5 |
| ex-semantic-fractional-index-mutate-repair | 5 | 7 |
| ex-semantic-library-diff-update | 5 | 5 |
| ex-semantic-scroll-lock-clamp | 5 | 5 |
| ex-stale-name-canvas-search | 5 | 5 |
| tt-deps-incoming-db-connection | 1 | 1 |
| tt-feature-bulk-cancel-epic-tasks | 5 | 10 |
| tt-implement-release-cancelled-task-bug | 5 | 9 |
| tt-implement-split-task-cancelled-release | 5 | 9 |
| tt-semantic-board-stale-task-flag | 5 | 5 |
| tt-semantic-dedupe-prefix-collision | 5 | 5 |
| tt-semantic-doc-drift-check | 5 | 5 |
| tt-stale-name-doc-sync-check | 5 | 5 |

**Source 2, GMB-180's semantic-tier bucket** (8 tasks, from
`g-mesh-bench/docs/results/v0.24.0-gmb180-the-semantic-tier-bucket.md`, "The
bucket"): gin-find-impl-render, gin-callers-writeheadernow-dispatch,
gin-scenario-abort-callers, py-callers-prepare-two-classes,
py-callers-register-hook-mixin, py-scenario-callers-httpadapter-send,
rs-callers-flag-name-long-dyn, rs-callers-sink-matched. Their "semantic tier"
is g-mesh's LSP edge tier, not embeddings; D10 names the bucket, so it is
included as specified, and it serves as a set where the arms should not differ.

**26 tasks** (not low-power), 5 repetitions per task and arm, arms alternating.

### Setup

- **R**: `release-3.17.0` at `5a8b5a7`, built in a detached worktree, fp32
  weights from `~/.g-mesh/models/jina-embeddings-v2-base-code`
  (`model.onnx` sha256 `63363fc1…6733b`), shipped floors
  go/python/rust/typescript 0.59 / 0.57 / 0.55 / 0.50.
- **C**: the same commit, throwaway (never merged). Weights through g-mesh's
  existing runtime switch `G_MESH_MODEL_DIR` (`embedding::model::default_model_dir`),
  no code change: a directory holding `onnx/model_quantized.onnx`, downloaded
  from the same repository at `MODEL_REVISION` `516f4ba`, renamed
  `model.onnx` (sha256 `ed458702…2cb16d`, identical to the eval harness's
  int8 copy), plus the same `tokenizer.json`. Floors: the only code change,
  `similarity::floor` go 0.59 → 0.57 and typescript 0.50 → 0.53 (ADR 0011's
  0.57 / 0.57 / 0.55 / 0.53).
- Located with g-mesh: `find_definition floor` (the floor table),
  `find_definition default_model_dir` / `resolve_model_dir` (the
  `G_MESH_MODEL_DIR` switch), `find_definition EmbeddingModel`,
  `find_references MODEL_REVISION` (no references outside `cli/model.rs`;
  the pin is used only by `g-mesh model fetch`, not at load time, so C needs
  no sha change to load).
- Harness: g-mesh-bench `chore/GM-422-d10-veto` (`138ee41`),
  `scripts/d10-int8-veto.sh` (adapted from GM-434's `ab-prose-floor.sh`),
  `gmesh-configured` only, `claude-sonnet-5`. Per-arm `G_MESH_HOME`
  (`~/.gm422R`, `~/.gm422C`). 5 rounds; each round runs every task once per
  arm (`REPS=low` per invocation), arm order R C, C R, R C, C R, R C.
  Tokens = input + output + cache read + cache creation. Per task: pass
  count, and median C vs median R; the rules take the median over tasks.

### Control: C served int8

- **Probe** (`scripts/probe-d10-int8.ts`, before the run, task-tracker-mcp
  corpus, same three queries per arm): every top-3 score differs, e.g.
  "detect a dependency cycle between tasks" `addDependency` R 0.4999 /
  C 0.4791; "check whether docs drifted from the code" `isDocDrifted`
  R 0.6546 / C 0.6715. Same top-3 names, shifted scores: the int8 signature
  S8 found.
- **In the run's transcripts**: 14 `search_code` queries were issued
  verbatim in both arms, and all 14 have different top scores (e.g.
  "cancel task status transition" R 0.5538 / C 0.5660).

### Results

260 records (26 tasks × 2 arms × 5), all `ok`, no missing transcripts.

| task | R pass | C pass | R med tokens | C med tokens | tok Δ | R med turns | C med turns | turn Δ | sc calls R/C |
|---|---|---|---|---|---|---|---|---|---|
| ex-implement-mutateelement-elbow-zero-position | 5/5 | 5/5 | 184,231 | 172,791 | -6.2% | 11 | 10 | -1 | 3/4 |
| ex-semantic-arrow-endpoint-grid-align | 5/5 | 5/5 | 63,139 | 63,050 | -0.1% | 3 | 3 | +0 | 5/5 |
| ex-semantic-arrow-zorder-above-bound | 5/5 | 5/5 | 213,022 | 114,948 | -46.0% | 12 | 7 | -5 | 9/8 |
| ex-semantic-cjk-charclass-check | 5/5 | 5/5 | 61,638 | 62,252 | +1.0% | 3 | 3 | +0 | 5/5 |
| ex-semantic-collab-conflict-keep-local | 5/5 | 5/5 | 62,143 | 62,150 | +0.0% | 3 | 3 | +0 | 5/5 |
| ex-semantic-drag-text-anchor | 5/5 | 5/5 | 63,100 | 63,894 | +1.3% | 3 | 4 | +1 | 6/5 |
| ex-semantic-fractional-index-mutate-repair | 5/5 | 5/5 | 62,700 | 62,621 | -0.1% | 3 | 3 | +0 | 5/5 |
| ex-semantic-library-diff-update | 5/5 | 5/5 | 84,539 | 86,852 | +2.7% | 4 | 4 | +0 | 5/5 |
| ex-semantic-scroll-lock-clamp | 5/5 | 5/5 | 86,676 | 108,976 | +25.7% | 5 | 5 | +0 | 5/5 |
| ex-stale-name-canvas-search | 5/5 | 5/5 | 38,435 | 39,925 | +3.9% | 3 | 3 | +0 | 0/2 |
| gin-callers-writeheadernow-dispatch | 5/5 | 5/5 | 164,138 | 179,306 | +9.2% | 11 | 11 | +0 | 0/0 |
| gin-find-impl-render | 5/5 | 5/5 | 187,113 | 226,003 | +20.8% | 11 | 12 | +1 | 0/0 |
| gin-scenario-abort-callers | 5/5 | 5/5 | 57,408 | 57,409 | +0.0% | 3 | 3 | +0 | 0/0 |
| py-callers-prepare-two-classes | 5/5 | 5/5 | 148,701 | 126,953 | -14.6% | 7 | 8 | +1 | 0/0 |
| py-callers-register-hook-mixin | 5/5 | 5/5 | 154,137 | 168,515 | +9.3% | 8 | 10 | +2 | 0/0 |
| py-scenario-callers-httpadapter-send | 5/5 | 5/5 | 88,173 | 112,650 | +27.8% | 7 | 8 | +1 | 0/0 |
| rs-callers-flag-name-long-dyn | 4/5 | 5/5 | 208,548 | 176,054 | -15.6% | 14 | 14 | +0 | 5/4 |
| rs-callers-sink-matched | 5/5 | 4/5 | 259,159 | 215,432 | -16.9% | 12 | 10 | -2 | 0/0 |
| tt-deps-incoming-db-connection | 5/5 | 5/5 | 81,821 | 83,341 | +1.9% | 5 | 6 | +1 | 1/0 |
| tt-feature-bulk-cancel-epic-tasks | 5/5 | 5/5 | 226,816 | 291,705 | +28.6% | 13 | 14 | +1 | 8/10 |
| tt-implement-release-cancelled-task-bug | 5/5 | 5/5 | 358,469 | 489,783 | +36.6% | 12 | 15 | +3 | 0/0 |
| tt-implement-split-task-cancelled-release | 3/5 | 4/5 | 1,063,122 | 1,166,467 | +9.7% | 26 | 29 | +3 | 2/7 |
| tt-semantic-board-stale-task-flag | 5/5 | 5/5 | 60,258 | 60,349 | +0.2% | 4 | 4 | +0 | 5/5 |
| tt-semantic-dedupe-prefix-collision | 5/5 | 5/5 | 59,925 | 59,917 | -0.0% | 3 | 3 | +0 | 5/5 |
| tt-semantic-doc-drift-check | 5/5 | 5/5 | 61,163 | 62,176 | +1.7% | 3 | 3 | +0 | 5/5 |
| tt-stale-name-doc-sync-check | 5/5 | 5/5 | 60,564 | 60,668 | +0.2% | 3 | 3 | +0 | 5/5 |

`sc calls` counts `search_code` calls; 18 of the 26 tasks used it in this run
(R 84 calls, C 90). The eight GMB-180 tasks plus a few others used none, so
their deltas are agent noise, not the model. On the 18 tasks that did, the
medians are +0.6 % tokens and +0 turns.

### Veto rules (D10)

| rule | threshold | measured | result |
|---|---|---|---|
| total oracle passes, C below R | > 2 | R 127, C 128 (C +1) | no veto |
| a task R passes 5/5, C ≤ 3/5 | any | none (C's lowest: 4/5 on rs-callers-sink-matched, tt-implement-split-task-cancelled-release) | no veto |
| median per-task token Δ | > +10 % | +1.1 % | no veto |
| median per-task turn Δ | > +0.5 | +0 | no veto |

**D10: NO VETO.** As D10 says, this is a veto, not a win condition: 5 runs per
task cannot show an improvement, and the per-task spread (-46 % to +37 %,
several on tasks with no `search_code` call) is the agent's own variance.

### Timing and cost

- 10 invocations, 3 h 26 m wall (`/usr/bin/time -p` real 12,384 s, user
  5,270 s, sys 1,156 s); per invocation 1,088-1,610 s. Round 1 carried the
  cold index builds (R user 2,102 s, C 872 s); later invocations user
  265-305 s.
- Load: 1-minute load 7.3 at the start (5-minute 37.7, another agent's work
  finishing), 2.7-11.8 across invocations, 4.5 at the end. Timing does not
  enter any rule; tokens and turns do not depend on load.
- Cost: R $14.51, C $14.51, total **$29.02** (plus the $0.07 dry run, one
  task on C). No g-mesh daemon under either arm's `G_MESH_HOME` before the
  run or after it; the probe's daemons stopped on SIGTERM.
