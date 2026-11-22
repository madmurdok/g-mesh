# GM-741/S8: QAT-trained L6 reranker vs today's L6 (measure, local files)

Design: `docs/design/GM-741-qat-reranker.md` sections 4-7. Code commit `3eb81c35`. Model: `onnx-quint8` of `gm730-rr-qat-27c57b8dd91d-20261121T0020-a1` (model.onnx sha256 `9eb66866...a633`, matches the pin in `rerank_eval.SHIPPED_CE`), from local files; nothing uploaded.

## Verdict: STOPPED at the section 4 parity gate (no GO/NO-GO issued)

Parity fails two of the five bounds (table below), so by design section 4 the stack numbers "would come from a kernel path that does not match the reference". The stack numbers were already computed by the scripted run before the gate was evaluated (the script needs beta first); they are given below for information only. The latency run (section 7) was **not run**. The owner decides whether to (a) re-derive the parity bound, (b) re-draw the sample, or (c) accept the near-ties.

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

## 2. Fit (section 5, steps 1-3)

- Rust dump of all 102,917 pairs (K=30 windows of all 3431 queries) imported as arm `ce-l6-qat-quint8-rust` into `g-mesh-eval-data/v2/work/rerank_cache_gm741_qat` (not committed). Dump: `time -p` real 2741 s, user 8708 s, sys 46 s (user > real: multi-threaded, not waiting); machine load 159 at start, 99 at end.
- Logit scale, full pair set: mean **-5.919**, sd **0.681** (min -7.49, max +1.19). (Probe in the design: 1.3.)
- Fitted beta (`--tune-beta-on-fit`): **5.0** (FIT NL r@10 0.7146, MRR 0.5350, 639 FIT queries). Interior of the grid [0, 1, 2.5, 5, 10, 20, 40, 80, 160, 320]; no grid edge hit, no betas added.
- Base run reproduces GM-461 F4: NL CW positives 296/681, absent 37/164 (control passed).

## 3. Go rule, held-out, paired bootstrap (seed 741, 10,000 resamples) — informational, parity gate failed

Delta = candidate (qat, beta 5) minus base (L6, beta 40), points.

| id | cell | delta [95% CI] | rule | pass |
|---|---|---|---|---|
| R1 | NL pooled, CW pos | -3.8 [-6.1, -1.5] | upper <= +2.0 | yes |
| R2 | NL go / python / rust / typescript, CW pos | -4.3 / -6.5 / -2.5 / -2.2 | point <= +3.0 | yes |
| R3 | name pooled, CW pos | -3.3 [-6.2, -0.5] | upper <= +2.0 | yes |
| R4 | CW absent, every cell (8) | exactly 0.0 [0.0, 0.0]; per-query CW flags identical | exactly 0 | yes (control) |
| R5 | name misled, go / python / rust / typescript | -0.4 / +0.0 / -1.2 / +0.0 | point <= +1.0 | yes |
| R5 | name pooled misled | -0.4 [-1.4, +0.5] | upper <= +2.0 | yes |

Had parity held, R1-R5 would all pass (GO). Full per-language CIs: scratch `out/boot.log`.

Reported only (held-out): NL misled per language go +0.3 [-7.1, +7.6], python +4.7 [-5.1, +14.9], rust +0.1 [-7.8, +8.1], typescript +0.1 [-5.9, +6.0], pooled +1.4 [-2.4, +5.3].

| held-out F4 (harness T1) | NL r@10 | NL MRR | name r@10 | name MRR |
|---|---|---|---|---|
| base (L6, beta 40) | 67.50 | 46.33 | 96.47 | 87.55 |
| qat, fitted beta 5 | 71.99 | 52.79 | 95.67 | 89.94 |
| qat, beta 40 (context) | 67.00 | 45.21 | 95.47 | 81.75 |
| qat, beta 0 (context) | 69.41 | 47.95 | 95.80 | 89.87 |

Queries whose top row differs between arms (held-out positives, K=30 F4 order): 352 (NL 286 of 681, name 66 of 420), > 0, so the A/B measures something. (Cell-level MRR/r@10 from `gm741_parity.py top` over positives only agree in direction: NL MRR 0.465 -> 0.529.)

## 4. Latency / RSS (section 7)

Not run: the section 4 gate failed. Plumbing was dry-run: per-arm wrapper `g-mesh` scripts set `G_MESH_RERANK_MODEL`/`_DIR`/`_BETA` (the driver `gm464_latency.py` has no `old` arm and no model/beta knob, and does not record RSS; no script was edited), and a sampler script records daemon RSS every 2 s. The dry run's first index took > 20 min under load 100-300, so a gated run would have waited on the load < 4 gate.

## 5. Machine state

Python: `/usr/bin/python3` 3.9.6 (numpy, onnxruntime 1.19.2, tokenizers; the `g-mesh-data/gm436/venv` has no onnxruntime). Run `caffeinate -i -s`. Start uptime `load 159.44 185.52 137.88`; parity compare at `load 79.68 77.85 83.62`; the machine was shared with other sessions throughout (load 40-320), so only the Rust dump timing carries this caveat; logits are deterministic.

## 6. Anomalies

- Parity fails two bounds by small margins (see 1).
- `gm700_rust_pairs.py import` needed Python logits for its agreement stats (empty set crashes); the Python sample logits were put into the python arm's cache first (`gm741_parity.py sample`).
- `gm464_latency.py` gaps listed in 4.

Files: `eval/embedding/gm741_parity.py` (sample / compare / top). Scratch outputs: `/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/7d447dd0-f002-4987-b1fc-55c5da05ac2c/scratchpad/gm741s8/`.
