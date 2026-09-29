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
