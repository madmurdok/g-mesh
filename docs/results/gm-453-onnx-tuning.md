# GM-453: ONNX Runtime tuning measurements

Design: `docs/design/GM-453-onnx-tuning.md` (§5.3, §7). Script: `eval/embedding/gm453_measure.sh`; summary: `gm453_summary.py`.
Raw run dirs (`eval/embedding/work/runs-gm453`) are not committed.

## Machine state

- Phase A: run earlier (output reused); per-run load shown in the table below.
- Phases B-D: one script run, start `22:24 up 1 day, 10:27, load 3.25 3.43 4.43`, end `06:21 up 1 day, 18:23, load 3.05 4.26 5.37`.
- Every run row carries `time -p` real/user/sys and load1 before/after. Rows whose after-load exceeded 8.0 + own parallelism were redone once (rule from GM-398); the redo is the row kept. Phase A round-1 A0 (175 s) vs round-2 A0 (72 s) shows how noisy the machine was: phase A medians over 2 rounds are screening only.
- `user/real` near 1 means one busy thread (A1, C2, P1); 3.8-4.0 means all 4 intra threads busy.

## Phase A: screening (int8-structured, gin + py-requests, 2 rounds)

Median embedNodesMs, x baseline A0, median real, median user/real, parity over all rounds.

| arm | settings | median ms | x A0 | real s | user/real | parity |
|---|---|---:|---:|---:|---:|---|
| A0 | default | 121071 | 1.000 | 123.4 | 3.30 | pass |
| A1 | intra=1 | 147691 | 1.220 | 149.7 | 0.98 | pass |
| A2 | intra=2 | 74811 | 0.618 | 76.5 | 1.95 | pass |
| A3 | intra=physical | 70481 | 0.582 | 72.4 | 3.86 | pass |
| A4 | intra=logical | 85205 | 0.704 | 87.0 | 6.53 | pass |
| A5 | opt=basic | 80013 | 0.661 | 82.0 | 3.89 | FAIL |
| A6 | opt=extended | 67038 | 0.554 | 68.8 | 3.85 | pass |
| A7 | spin=0 | 70090 | 0.579 | 71.9 | 2.60 | pass |
| A8 | inter=2 | 64136 | 0.530 | 65.9 | 4.81 | pass |
| A9 | batch=8 budget=8192 bucket=0 | 147807 | 1.221 | 149.7 | 3.91 | FAIL |
| A10 | batch=8 budget=8192 bucket=1 | 61468 | 0.508 | 63.3 | 3.88 | FAIL |
| A11 | batch=32 budget=16384 bucket=1 | 94663 | 0.782 | 96.8 | 3.89 | FAIL |
| A12 | intra=physical batch=8 bucket=1 spin=0 | 65041 | 0.537 | 67.0 | 2.99 | FAIL |

Gate: go (thread/opt arms over 5%: A2 A3 A4 A6 A7 A8; no adoptable batching arm). Picked for B: A0 A8 A6.

## Parity per arm (against the phase baseline, max L2 / bit-identical)

| arm | max L2 | min cos | bit-identical | verdict |
|---|---:|---:|---:|---|
| A1-A4, A6-A8 | 0 | 1.0 | 100% | pass |
| A5 opt=basic | 0.158 (0.14-0.16 across docs) | 0.987 | 0% | FAIL |
| A9 | 0.407 | 0.917 | 0% | FAIL |
| A10, A12 | 0.377 | 0.929 | 0% | FAIL |
| A11 | 0.642 | 0.794 | 0% | FAIL |
| B A6, A8 (3 rounds, g-mesh, 6774 nodes) | 0 | 1.0 | 100% | pass |
| C C1, C2 (fp32, 2 rounds, 6774 nodes) | 0 | 1.0 | 100% | pass |
| control int8 vs fp32 | 0.995 | 0.505 | 0% | fails as expected (6774 over 1e-4) |

Int8 batching (A9-A12, L2 up to 0.64) and opt=basic (A5, L2 up to 0.16) fail parity: padding/shape changes alter int8 dynamic-quantisation scales, so vectors differ. Adopting either would need an `embedding_version` bump and a reindex.

## Phase B: confirmation (int8-structured, g-mesh, 3 rounds)

| arm | rounds ms | median ms | x A0 | median real s | user/real |
|---|---|---:|---:|---:|---:|
| A0 | 508381 (redo, load 21.6 after), 335247, 353792 | 353792 | 1.000 | 355.9 | 3.93 |
| A6 opt=extended | 349092, 343140, 341099 | 343140 | 0.970 | 345.1 | 3.94 |
| A8 inter=2 | 377725, 363466, 348912 | 363466 | 1.027 | 365.8 | 4.87 |

On the real corpus the phase-A gains (A6 0.55, A8 0.53) are not reproduced: A6 is -3% (inside the 5% bar and the run-to-run spread), A8 is +2.7% and burns more CPU (user/real 4.9). Phase A's baseline round 1 was inflated by load (175 s vs 72 s). Quality: recall@10 0.3400, MRR 0.2206 for A0, A6 and A8 (50 queries, delta 0).

## Phase C: fp32-R (jina-v2-base-code-fp32-r, g-mesh, 2 rounds)

| arm | settings | rounds ms | median ms | x C0 | median real s | user/real |
|---|---|---|---:|---:|---:|---:|
| C0 | default | 916138, 989077 | 952607 | 1.000 | 956.6 | 3.83 |
| C1 | batch=8 budget=8192 bucket=1 | 1016328, 1148910 | 1082619 | 1.136 | 1086.6 | 3.85 |
| C2 | intra=2 | 1999845, 1668672 | 1834259 | 1.926 | 1841.1 | 1.78 |

fp32 batching is bit-identical (parity pass) but 14% slower and uses 2.2-2.8 GB RSS vs 1.5-1.8 GB. intra=2 halves throughput. Quality: recall@10 0.3600, MRR 0.2456 for all three arms (delta 0). fp32 vs int8: +0.02 recall, +0.025 MRR, 2.7x slower.

## Phase D: interactive latency (fp32 pass + probe)

Pass without probe: C0 median real 956.6 s.

| arm | probe queries | p50 ms | p95 ms | max ms | pass real s | x C0 |
|---|---:|---:|---:|---:|---:|---:|
| P-idle (alone) | 715 | 16.6 | 38.6 | 81.9 | - | - |
| P0 default, normal priority | 2350 | 33.2 | 106.5 | 554.7 | 1046.7 | 1.094 |
| P1 `taskpolicy -b` | 14870 | 15.7 | 37.0 | 118.1 | 6236.8 | 6.52 |
| P2 intra=2 | 2365 | 18.7 | 47.4 | 141.4 | 1001.4 | 1.047 |
| P2q intra=2, probe alone | 710 | 18.2 | 42.9 | 60.3 | - | - |

P0 doubles probe p50 and gives a p95 of 106 ms. Background QoS (P1) restores probe latency but slows the pass 6.5x (user/real 1.17, E-cores only). intra=2 keeps probe p50 at 18.7 ms (near idle) for a pass cost of 1.05x in this beside-the-probe setting (note C2 alone, without a probe, is 1.93x C0; P2's pass is not slower because P0's was already contended).

## Recommended defaults

- Keep the current defaults for the int8 production path: no thread/opt arm clears the 5% bar on the real corpus (phase B), and opt=basic fails parity.
- Do not adopt batching or bucketing: int8 batching fails parity; fp32 batching is bit-identical but slower.
- For an interactive-latency mode (indexing beside an active user), `intra_threads=2` is the only setting that keeps probe latency near idle without the 6.5x cost of background QoS. It must be opt-in, because the same setting halves unattended fp32 throughput (C2).

## Decision for the owner

int8 at batch 1 (current, parity-clean) versus int8 batching (up to ~2x in phase A on small corpora but L2 up to 0.64, requiring an `embedding_version` bump and reindex, and unconfirmed on g-mesh since the gate stopped phase B from testing it). Recommendation: stay at batch 1.
