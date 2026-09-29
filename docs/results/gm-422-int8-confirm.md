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
