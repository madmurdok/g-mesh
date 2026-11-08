# GM-700 S9: gte-reranker-modernbert-base int8 latency probe

Owner request: "gte int8 - проверить в gm-700". Design section 3 (its S4). Verdict: **FAIL the pre-registered bar; quality not run.**

## Arms and files

| arm | file | sha256 (recomputed with `shasum -a 256`, equals the pin) | chunk |
|---|---|---|---|
| L6 fp32 | `model.onnx` | `5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a` | 16 |
| L6 qint8 (ship) | `onnx/model_qint8_avx512_vnni.onnx` | `3573b6b9593cb2f75987a31815d409ca3dd8808629118fd20451bb1a5d90cec7` | 1 |
| gte int8 | `onnx/model_int8.onnx`, 150,871,837 bytes, revision `f7481e60` | `ecc6a0ae67cee3d898167802383112d9185ca9250e07bd5d1fa65019b050179d` | 1 |

No gte fp32 arm: section 3.2 does not list one (GM-463's 6131.7 ms is the reference). Fetched with `fetch_models.py --ce ce-gte-mb-int8` into GM-463's `v2/work/models/gte-reranker-modernbert-base-int8/`.

## Run

`latency --ks 30 --passes 3`, 30 queries x 3 passes = 90 calls per arm, fresh process per arm under `/usr/bin/time -p`, threads 4, mirrored order L6-fp32, L6-qint8, gte, gte, L6-qint8, L6-fp32. Load gate (1-min load < 4, held) open for every arm; cumulative gate wait 755 s. Run start 00:42 load 19.81 48.03 32.54, CPU_Speed_Limit 46 (the gate and therm wait held the first arm until 00:46); end 01:04 load 4.58 5.10 10.59, CPU_Speed_Limit 100. Per-arm CPU_Speed_Limit samples: 100 for all four samples in every arm except gte #3 (100/100/90/100). user/real 3.69-3.92 in every arm (no waiting).

| arm | p50 ms | p95 ms | ms/pair | pair tokens | time -p real / user / sys | load start/end |
|---|---|---|---|---|---|---|
| L6 fp32 #1 | 678.5 | 986.2 | 22.75 | 101.9 | 83.15 / 313.09 / 1.99 | 3.27/6.80 |
| L6 qint8 #2 | 196.9 | 257.5 | 6.32 | 101.9 | 25.92 / 95.75 / 0.82 | 3.51/4.16 |
| gte int8 #3 | 1646.4 | 2363.8 | 54.68 | 104.4 | 200.19 / 782.68 / 3.44 | 3.77/6.55 |
| gte int8 #4 | 1490.6 | 2072.2 | 49.15 | 104.4 | 184.23 / 722.76 / 2.76 | 3.11/5.49 |
| L6 qint8 #5 | 164.6 | 266.0 | 5.84 | 101.9 | 22.83 / 85.53 / 0.64 | 3.24/4.04 |
| L6 fp32 #6 | 492.4 | 714.9 | 16.77 | 101.9 | 64.30 / 249.29 / 1.34 | 3.07/4.58 |

## Bar (rule 5)

Pooled K=30 p50 (mean of the two arm p50s): L6 fp32 585.5, L6 qint8 180.8, gte int8 1568.5 ms.

- **gte int8 / L6 qint8 = 8.68x** against the bar <= 2.0x: **fail** (even the best gte arm 1490.6 vs the best qint8 arm 164.6 is 9.1x; the worst-case pairing of the bar's own spread cannot reach 2.0x).
- Not gating: gte int8 / L6 fp32 = 2.68x; gte int8 1568.5 ms vs GM-463's gte fp32 6131.7 ms = 0.26x (different machine state, GM-463's figure was under load). So int8 sped gte up about 3.9x, L6 about 3.2x (585.5 -> 180.8); design 3.4's prediction (gte stays ~9.5x int8-L6, fails) holds.
- Arm spread: fp32 L6 678.5 vs 492.4 (1.38x; first arm was right after the throttled start), gte 1646.4 vs 1490.6 (1.10x), qint8 L6 196.9 vs 164.6. The verdict does not depend on it.

## Control (section 3.3) and checks

- Per-arm sha distinct and as pinned (table above): **differed**.
- ms/pair gte 49-55 vs qint8 L6 5.8-6.3, far beyond the L6 arm-to-arm spread: **differed**.
- Mean pair tokens: gte 104.4 vs L6 101.9 (2.5% apart, within 10%); L6 equals gm-698-latency.md's 101.9: pass. The ratio is model cost.
- `eval/embedding/gm700_gte_int8_control.py`: C8 input format pass; C7 rank-only pass (card pairs, top two documents on top; batch vs single max|diff| 0); gte int8 vs gte fp32 logits on 5 pairs differ, max|diff| 1.7166 (int8 2.4583, 2.2560, 0.0391, 1.4546, -0.9613 vs fp32 2.1383, 2.4610, -1.6775, 1.5707, -2.6493): **differed**. The quality harness's `--controls-only` was not run (it requires scoring the ce-minilm reference pairs first, ~190k pairs); it belongs to the quality step, which the verdict skips.

## Verdict

Fail: gte int8 costs 8.68x the shipped qint8 L6 at K=30 (about 1.5 s p50 against 0.18 s). Per design 3.2 step 5 no quality re-score was run. GM-436 (distilling gte) proceeds as already decided. Adding gte int8 would also mean a 151 MB download.
