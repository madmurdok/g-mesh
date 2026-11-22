# GM-741/S8: QAT-trained L6 reranker vs today's L6 (measure, local files)

Design: `docs/design/GM-741-qat-reranker.md` sections 4-7. Code commit `3eb81c35`. Model: `onnx-quint8` of `gm730-rr-qat-27c57b8dd91d-20261121T0020-a1` (model.onnx sha256 `9eb66866...a633`, matches the pin in `rerank_eval.SHIPPED_CE`), from local files; nothing uploaded.

## Verdict: GO (GM-741/S15)

Parity on the fresh sample (design 4.1, section 1b) passes P1-P4, so the S8 stack numbers (section 3: R1-R5 all pass) stand and are final, not informational. Latency/RSS (section 4): QAT is not slower than today's L6 (pooled p50 492 vs 523 ms, p95 708 vs 742 ms) and its daemon RSS is the same (max 504-509 MB vs 483-508 MB). The S8 gate failure below is kept as history; it was superseded by design 4.1 (owner: "Перевывести границу и взять новую выборку (Recommended)").

### S8 history: first parity run

Parity failed two of the five original bounds (section 1); the owner re-derived the bound (design 4.1) and took a fresh sample.

## 1. Parity, Rust vs Python ORT (section 4)

Sample: 120 whole windows, seed 741, 15 NL + 15 name per language, 3600 pairs. Rust: `dump_logits_for_the_offline_eval` (debug test profile, fresh process, `G_MESH_RERANK_MODEL=qat`, as in GM-700/S22); Python: onnxruntime 1.19.2 via `rerank_eval.CrossEncoder`, one pair per run. Blend beta = 5.0 (the fitted one).

| bound | value | limit | result |
|---|---|---|---|
| mean abs d | 0.00387 | <= 0.01 | pass |
| mean abs d vs 0.005 x sd (sd 0.7003 -> 0.00350) | 0.00387 | <= 0.00350 | **FAIL** (10% over) |
| max abs d | 0.1006 | <= 0.20 | pass |
| blend top-1 identical | 119 / 120 | >= 119 | pass |
| blend top-3 set identical | 117 / 120 | 120 / 120 | **FAIL** |

p99 abs d 0.0387; 1539 of 3600 pairs differ at all; mean signed d +0.00002 (no bias). The three top-3 windows differ by near-ties of the 3rd and 4th blend: `ttm-mn098` (Python gap 0.0175, Rust 0.0437), `gin-c245` (0.0056 / 0.0068), `gm-c001` (0.0005 / 0.0041). Per-window outputs: scratch `out/parity.json`.

For reference, today's L6 (GM-452 caveat b): mean 0.0052, max 0.198 on 300 pairs; the new model's mean is smaller in absolute terms but its logits are narrower (sd below), which is what the scaled bound tests.

## 1b. Parity on fresh windows (design 4.1, GM-741/S15)

120 windows (15 per cell, seed 74113, excluding the seed-741 draw), 3600 pairs, window-list sha256 `8b0a8972cc5ac9b8d08585b218461efd68b08c0e2a8261df19b8f7ce1f29cc7c` (written before scoring). Python onnxruntime 1.19.2 (`/usr/bin/python3` 3.9.6), one pair per run, both arms scored in this run; Rust logits from the existing caches (none missing). Tool: `gm741_parity.py fresh`. P4 margin m = 2 x p99(|d|) of the l6 arm = 0.1437. Run 280 s real, 394 s user.

| id | bound | qat | l6 reference | result |
|---|---|---|---|---|
| P1 | mean \|d\| <= l6's, and <= 0.01 | 0.003523 | 0.003874 | pass |
| P2 | max \|d\| <= 0.20 | 0.0951 | 0.2054 | pass |
| P3 | blend top-1 identical >= 119/120 | 120/120 | 119/120 | pass |
| P4 | zero non-tie top-3 differences | 0 | 0 | pass |

Reported: p99 \|d\| qat 0.0389 / l6 0.0719; mean signed d -0.00002 / +0.00018; logit sd 0.689 / 3.58; raw top-3 set agreement qat 118/120 (swap gaps 0.0055, 0.0017) / l6 117/120 (0.026, 0.0086, 0.0064). All swaps are below m, i.e. ties.

## 2. Fit (section 5, steps 1-3)

- Rust dump of all 102,917 pairs (K=30 windows of all 3431 queries) imported as arm `ce-l6-qat-quint8-rust` into `g-mesh-eval-data/v2/work/rerank_cache_gm741_qat` (not committed). Dump: `time -p` real 2741 s, user 8708 s, sys 46 s (user > real: multi-threaded, not waiting); machine load 159 at start, 99 at end.
- Logit scale, full pair set: mean **-5.919**, sd **0.681** (min -7.49, max +1.19). (Probe in the design: 1.3.)
- Fitted beta (`--tune-beta-on-fit`): **5.0** (FIT NL r@10 0.7146, MRR 0.5350, 639 FIT queries). Interior of the grid [0, 1, 2.5, 5, 10, 20, 40, 80, 160, 320]; no grid edge hit, no betas added.
- Base run reproduces GM-461 F4: NL CW positives 296/681, absent 37/164 (control passed).

## 3. Go rule, held-out, paired bootstrap (seed 741, 10,000 resamples) — final (parity passed, 1b)

Delta = candidate (qat, beta 5) minus base (L6, beta 40), points.

| id | cell | delta [95% CI] | rule | pass |
|---|---|---|---|---|
| R1 | NL pooled, CW pos | -3.8 [-6.1, -1.5] | upper <= +2.0 | yes |
| R2 | NL go / python / rust / typescript, CW pos | -4.3 / -6.5 / -2.5 / -2.2 | point <= +3.0 | yes |
| R3 | name pooled, CW pos | -3.3 [-6.2, -0.5] | upper <= +2.0 | yes |
| R4 | CW absent, every cell (8) | exactly 0.0 [0.0, 0.0]; per-query CW flags identical | exactly 0 | yes (control) |
| R5 | name misled, go / python / rust / typescript | -0.4 / +0.0 / -1.2 / +0.0 | point <= +1.0 | yes |
| R5 | name pooled misled | -0.4 [-1.4, +0.5] | upper <= +2.0 | yes |

R1-R5 all pass (GO). Full per-language CIs: scratch `out/boot.log`.

Reported only (held-out): NL misled per language go +0.3 [-7.1, +7.6], python +4.7 [-5.1, +14.9], rust +0.1 [-7.8, +8.1], typescript +0.1 [-5.9, +6.0], pooled +1.4 [-2.4, +5.3].

| held-out F4 (harness T1) | NL r@10 | NL MRR | name r@10 | name MRR |
|---|---|---|---|---|
| base (L6, beta 40) | 67.50 | 46.33 | 96.47 | 87.55 |
| qat, fitted beta 5 | 71.99 | 52.79 | 95.67 | 89.94 |
| qat, beta 40 (context) | 67.00 | 45.21 | 95.47 | 81.75 |
| qat, beta 0 (context) | 69.41 | 47.95 | 95.80 | 89.87 |

Queries whose top row differs between arms (held-out positives, K=30 F4 order): 352 (NL 286 of 681, name 66 of 420), > 0, so the A/B measures something. (Cell-level MRR/r@10 from `gm741_parity.py top` over positives only agree in direction: NL MRR 0.465 -> 0.529.)

## 4. Latency / RSS (section 7, GM-741/S15)

Driver `gm464_latency.py` at a404d9d1 (plus `GATE_STRICT=1`: raise instead of measuring past the gate cap; not triggered). Binary `target/release/g-mesh`; arms `old` = L6 (pinned copy of the model), `new` = QAT beta 5; order `old new new old old new off`; 30 queries x 3 passes after one warm-up pass; fresh daemon per arm on one shared index (warmed once before); load gate ON (1-min load < 4 held 30 s, cap 10,800 s), whole run under `caffeinate -i -s`. All 7 gates opened (old#1 waited 3816 s, the rest 90-190 s). `rerank.outcome` loaded/variant matched each arm (l6 / qat / off).

| arm | n | p50 ms | p95 ms | first-call ms | reordered (rerank-on share) | daemon cpu/wall | RSS max KiB | load start/end | time -p real/user/sys |
|---|---|---|---|---|---|---|---|---|---|
| old#1 | 90 | 584 | 788 | 908 | 90/90 | 3.56 | 507836 | 3.72/7.21 | 4013.04/0.47/0.93 |
| new#2 | 90 | 537 | 729 | 1468 | 90/90 | 3.80 | 504072 | 3.6/6.71 | 260.07/0.18/0.20 |
| new#3 | 90 | 456 | 644 | 1202 | 90/90 | 3.91 | 509240 | 3.43/7.15 | 194.34/0.16/0.18 |
| old#4 | 90 | 513 | 726 | 1103 | 90/90 | 3.86 | 502008 | 3.23/5.58 | 155.08/0.16/0.19 |
| old#5 | 90 | 483 | 695 | 1148 | 90/90 | 3.94 | 482796 | 3.34/5.54 | 176.44/0.17/0.19 |
| new#6 | 90 | 478 | 672 | 1245 | 90/90 | 3.94 | 504188 | 3.4/6.25 | 203.28/0.16/0.18 |
| off#7 | 90 | 111 | 125 | 639 | 0/90 | 3.04 | 440752 | 3.63/4.14 | 112.76/0.16/0.18 |

Pooled: old p50 523 / p95 742 ms (n 270); new p50 492 / p95 708 ms (n 270); off p50 111 / p95 125 ms. Controls: rerank-on arms reorder 90/90 pages, off 0/90, so the rerank is what the arms differ by. Machine state: uptime at each arm start load 3.2-3.7 (1-min), thermal 100 throughout; per-arm uptime lines in scratch `gm741s15/lat/summary.md`. `time -p` user/sys of the driver are tiny (it waits on the daemon); old#1's real time includes the load-gate wait.

Reading: new vs old p50 -6% pooled; per-arm p50 ranges overlap (old 483-584, new 456-537, first arm of each is the slowest), so the claim is "not slower", not "faster". RSS: new 504-509 MB vs old 483-508 MB (rerank off 441 MB), same within run-to-run spread. First call (model load) is 1.2-1.5 s for new vs 0.9-1.1 s for old.

## 5. Machine state

Python: `/usr/bin/python3` 3.9.6 (numpy, onnxruntime 1.19.2, tokenizers; the `g-mesh-data/gm436/venv` has no onnxruntime). Run `caffeinate -i -s`. Start uptime `load 159.44 185.52 137.88`; parity compare at `load 79.68 77.85 83.62`; the machine was shared with other sessions throughout (load 40-320), so only the Rust dump timing carries this caveat; logits are deterministic.

## 6. Anomalies

- S8: parity failed two bounds by small margins (see 1); superseded by 1b.
- S15: old#1 shared the cold-index period; its p50 (584) is the highest, order effects are in the table.
- `gm700_rust_pairs.py import` needed Python logits for its agreement stats (empty set crashes); the Python sample logits were put into the python arm's cache first (`gm741_parity.py sample`).

Files: `eval/embedding/gm741_parity.py` (sample / compare / top). Scratch outputs: `/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/7d447dd0-f002-4987-b1fc-55c5da05ac2c/scratchpad/gm741s8/`.
