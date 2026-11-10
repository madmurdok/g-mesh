# GM-700 S22: held-out quality of the quint8 cross-encoder as the Rust product scores it

Question: does the ship rule of `gm-700-offline.md` hold when the logits come from `CrossEncoder::score` (Rust, shipped AVX2 kernel path), not Python ORT? Parity cause: `gm-700-int8-parity.md`.

## Method

- Pairs: the K=30 window of all 3431 queries (1652 NL + 1779 name) with the quint8 model's ship texts, built by the harness (`gm700_rust_pairs.py dump`): 102,917 pairs, exactly the pairs `gm463_bakeoff.py quality` scores for `ce-minilm-quint8` (all already in S8's cache).
- Scorer: ignored test `embedding::rerank::tests::dump_logits_for_the_offline_eval` (core/src/embedding/rerank.rs), `CrossEncoder::load` + `Scorer::score` unchanged, one fresh process, no AVX512 touch, debug test profile, `onnx/model_quint8_avx2.onnx` (sha256 recomputed: `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9`). 1884 s real, user 7061 s; machine load 11-22 (other sessions), uptime at start `load 10.31 18.81 17.52`.
- Harness: new variant `ce-minilm-quint8-rust` (same files and texts as `ce-minilm-quint8`, its own score cache file) in a cloned cache dir `v2/work/rerank_cache_gm700_rust`; `rerank_cache` and `rerank_cache_gm700` untouched. Quality run: `--models ce-minilm,ce-minilm-quint8,ce-minilm-quint8-rust --k 30 --report-k 20,30 --skip-sim ce-minilm-quint8-rust`. C3'/C3'-16 (Python rescoring) are not applicable to the Rust variant and are skipped; the Python quint8 row still ran them (C3' max|diff| 0, chunk-16 control 0.812).
- g-mesh calls: `select_project` only; the Rust side needed one known symbol (`CrossEncoder::score`), read directly.

## Rust vs Python quint8 logits (102,917 pairs)

| mean abs d | p99 | max abs d | pairs with d > 0 | pairs d > 1e-3 | top-10 set equal (per query) |
|---|---|---|---|---|---|
| 0.0044 | 0.078 | 0.382 | 39,645 | 25,597 | 94.0% (205 of 3431 queries differ) |

## Ship rule, K=30, BETA 40 (held-out NL, name queries)

| variant | NL r@10 [CI] | NL MRR [CI] | name r@10 / MRR |
|---|---|---|---|
| off (L6, beta 80) | 0.649 | 0.427 | 0.970 / 0.880 |
| fp32 ship-L6 | 0.670 [0.641, 0.700] | 0.465 [0.439, 0.493] | 0.971 / 0.889 |
| quint8 Python | 0.675 [0.646, 0.704] | 0.463 [0.437, 0.490] | 0.971 / 0.890 |
| **quint8 Rust** | **0.675 [0.646, 0.704]** | **0.463 [0.437, 0.490]** | **0.971 / 0.891** |

| check | Rust quint8 | Python quint8 |
|---|---|---|
| NL r@10 >= 0.647 | pass (0.675) | pass |
| NL MRR >= 0.425 | pass (0.463) | pass |
| D9 Q1-Q5 vs int8-structured at shipped floors | pass | pass |
| name MRR >= 0.880 (GM-463 off, K=30) | pass (0.891) | pass (0.890) |
| Q2: dMRR CI lower >= -0.015 vs fp32 L6 | pass (-0.0053; dMRR -0.002 [-0.005, +0.001]) | pass (-0.0055) |
| Q2: dr@10 CI lower >= -1.5 pt | pass (-0.01 pt; +0.5 [-0.0, +1.0]) | pass |

Name dMRR vs fp32: +0.002 [-0.001, +0.004]. FIT-tuned beta (informational): Rust 20 (NL r@10 0.681, MRR 0.458), same as Python quint8; beta 40 passes, so 40 stands. Doc split (beta 40 numbers in the harness table) matches Python: documented 0.748 / 0.552, signature-only 0.532 / 0.274.

The harness's old section 3.6 rule 1 (dMRR lower > 0) fails for both quint8 variants as it did in S8; it is not the GM-700 ship rule. The "D9 vs jina fp32" table fails Q5 for every row including fp32; the shipped-floor D9 vs int8-structured passes.

## Skip rule, Rust scores, beta 40, K=30 (FIT-picked m = 0.08)

| arm | NL r@10 | NL MRR | name r@10 / MRR |
|---|---|---|---|
| no skip | 0.675 | 0.463 | 0.971 / 0.891 |
| (a) exact-name | 0.675 [0.646, 0.704] | 0.463 [0.437, 0.490] | 0.969 / 0.892 |
| (b) margin 0.08 | 0.674 | 0.457 | 0.968 / 0.882 |

(a) changes no held-out NL cell (0 of 845 NL queries skipped), name MRR +0.001 [-0.002, +0.003], skips 608 of 1779 name queries: ships. (b) costs NL MRR 0.006 (and name MRR 0.009, to 0.882 here); same verdict as S8, not shipped.

## Controls

- Rust differs from Python quint8 on 39,645 pairs (> 0; the import step aborts at 0): passed. Top-10 after rerank differs fp32 vs Rust on 2052 queries (547 held-out NL), > 0: passed.
- fp32 reference row reproduces S8 / GM-698 (C0 pass, 0 pairs scored): 0.670 [0.641, 0.700] / 0.465 [0.439, 0.493] / 0.971 / 0.889; off 0.649 / 0.427.
- Smoke (10 pairs) ran before the full dump; C1, C5, C8, C7, skip-sim controls pass.

## Verdict

The ship rule holds on Rust-produced quint8 scores: every bar passes, and every headline number equals the Python quint8 row to the third decimal (differences at most 0.001 in name MRR). The 0.38 maximum logit gap does not move the metrics. No owner decision is needed. Caveat: the dump ran the debug test profile; kernel choice is the same, only speed differs.

## Files

`eval/embedding/gm700_rust_pairs.py` (dump / import), `eval/embedding/rerank_eval.py` and `gm463_bakeoff.py` (variant registration), the ignored test in `core/src/embedding/rerank.rs`. Raw outputs: session scratchpad `s22/` (`table.md`, `quality.sum`, `agreement.json`).
