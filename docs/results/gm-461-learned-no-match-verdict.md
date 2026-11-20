# GM-461: learned no-match verdict, measured on held-out

Measure slice S5. One run on the held-out split; nothing was changed after it.
Code: `eval/embedding/gm461_learned_verdict.py` at 9ea02ebb on
`research/GM-461-learned-no-match-verdict`. Design:
`docs/design/GM-461-learned-no-match-verdict.md`.

## Question

Can a logistic model over stored features (cosine, reranker logit, gaps,
language, has-doc, exact-name) give a better refuse/warn verdict than F4 at
today's stack (jina int8 + rerank on eval v2)? Two models, as the owner
decided: one for NL queries, one for name queries.

## Verdict

- **NL: wins** against F4 on the rule. L-m (matched to F4's FIT kept share) has
  Q4 dCW -16.39 pts, one-sided upper bound -13.79 (< 0, superior); Q5 passes
  (dFA -2.61, upper +2.49 <= +5); not overfit; readable. Per the design this
  is a better warning, not a no-match.
- **NAME: does not win.** L-d6 held-out name misled is 0/63 go, 2/64 python
  (3.1%), 10/94 rust (10.6%), 4/118 typescript (3.4%). The rule needs <= 3% in
  every language, so it fails in python (point, marginally), typescript and rust
  (clearly). The caught delta vs F4 is large ([+25.3, +33.4] pts) but the
  misled gate decides. Max misled across languages: rust 10.6% (L-d6), 13.8%
  (L-m).
- **Overfit: no.** Held-out AUROC is at or above FIT for every NL model
  (gap -0.021 full, -0.028 noCE, -0.006 C0) and within +0.013 on name
  (full +0.003). Held-out kept at the matched point is above FIT kept for NL
  L-m (77.7% vs 71.6%); NAME L-m kept falls 96.4% to 94.1% (2.3 pts, under the
  5 pt limit).
- Net: NL passes its rule and is a ship candidate for a follow-up design; NAME
  closes null, floors stay for name queries.

Odd points worth reading before acting on NL:

- L-m moves the NL verdict from F4's on 282 of the held-out queries, 64 of them
  right tops. Misled (wrong top shown) is not lower in every language: typescript
  28.9% (24/83) vs F4 18.1% (15/83), rust 25.5% vs 19.1%; go improves 36.6% to
  15.5%. Overall NL misled 22.3% vs 24.7%.
- The gain is the warning on wrong/absent tops (CW pos 43.5% to 26.4%, CW
  absent 22.6% to 10.4%), paid for with +2.4 pts kept (not lower), CI of kept
  delta [-3.8, +8.9].
- The NL model has a negative cos1 coefficient (-1.06) and positive cos_t
  (+1.35): collinear terms, net +0.29; passed the owner's net-cosine stop rule.

## Setup

- Baseline: F4 on today's stack, eval v2 (owner decision, 2026-11-20, verbatim:
  "F4 на сегодняшнем стеке, eval v2 (Recommended)").
- Two models: "Две модели: для текста и для имён (Recommended)".
- Win rule: "Текст обгоняет F4 по Q4, имена misled ≤3% (Recommended)".
- Stop rule: "Проверять суммарный вклад косинуса (Recommended)".
- Arm-level stop: "Остановить только эту модель, остальное измерять (Recommended)";
  NAME noCE is reported as stopped by rule.
- Held-out was read once before this run: an S2 diagnostic `--ignore-stops` run,
  before the stop-rule decision. No model, feature or threshold changed after
  it; only the stop rule did (recorded in the design note). This run is the
  first with the final stop rule; its held-out reading is therefore not
  independent of that one look.
- Full resamples: 10,000 bootstrap, 200 shuffles, 1,000 coefficient refits.
  Model sha256 c80a8b34..bf9, floors go 0.57 / python 0.59 / rust 0.57 /
  typescript 0.53. Arms: int8 (shipped floors), F4, C0 (int8-only logistic at
  shipped floors), C0-m (C0 matched), L-m (full model, kept matched to F4's
  FIT kept), L-d6 (full model at the D6 rule), L-noCE (no cross-encoder
  features).
- Interpreter: system `python3` (the script needs onnxruntime and tokenizers,
  absent in the gm436 venv; the brief's venv `-I` interpreter cannot import it).
  The sklearn fitter check ran on the gm436 venv with `-I`.
- Machine: `uptime` before: load 63.59 / 37.94 / 34.39; `/usr/bin/time -p`:
  real 142.72 s, user 44.65, sys 7.14 (real >> user: loaded machine, waiting).
  Exit 0.

## Controls

- 1_reproduce: {"NL": {"got": {"kept_k": [186, 247], "caught_k": [265, 598]}, "want": {"kept_k": [186, 247], "caught_k": [265, 598]}}, "NAME": {"got": {"kept_k": [329, 339], "caught_k": [330, 499]}, "want": {"kept_k": [329, 339], "caught_k": [330, 499]}}, "int8_nl_ho": {"got": {"misled": [40, 184], "cw_pos": [338, 681], "cw_abs": [37, 164]}, "want": {"misled": [40, 184], "cw_pos": [338, 681], "cw_abs": [37, 164]}}, "ok": true}
- 2a_C0_verdict: {"rows_checked": {"NL": 1652, "NAME": 1779}, "mismatches": {"NL": 0, "NAME": 0}, "cells_equal_F4": true}
- 2b_C0_d6: {"implied": {"typescript": 0.46, "rust": 0.57, "go": 0.51, "python": 0.47}, "fit_floors": {"typescript": 0.46, "rust": 0.57, "go": 0.51, "python": 0.47}, "v2_refit": {"go": 0.51, "python": 0.47, "rust": 0.57, "typescript": 0.46}, "ok": true}
- 3_shuffle: {"NL": {"shuffles": 200, "observed_caught_delta": 0.2274247491638796, "caught_delta": {"mean": 0.052424749163879604, "p95": 0.11889632107023403, "max": 0.15719063545150508}, "kept_delta": {"mean": -0.20149797570850203, "p5": -0.39331983805668014, "min": -0.4817813765182186}, "share_ge_observed": 0.0}, "NAME": {"shuffles": 200, "observed_caught_delta": 0.2945891783567134, "caught_delta": {"mean": -0.18295591182364732, "p95": -0.07214428857715427, "max": 0.05010020040080165}, "kept_delta": {"mean": -0.4450737463126844, "p5": -0.6196165191740414, "min": -0.7492625368731564}, "share_ge_observed": 0.0}}
- 4_grad_norm: {"NL": {"full": 1.514204817194689e-14, "C0": 3.307067225545051e-14, "noCE": 1.890543889528457e-14}, "NAME": {"full": 8.711946407857682e-15, "C0": 1.5550214506641612e-14, "noCE": 1.3147643977373433e-14}}
- 5_differ_L-m_vs_F4: {"NL": {"n": 282, "right": 64}, "NAME": {"n": 177, "right": 26}}

Plus control 4 on the S5 `fit.npz` (venv interpreter, `-I`): max |numpy -
sklearn| 2.5e-8 (NAME C0), 1.7e-7 (NAME full), 2.3e-7 (NAME noCE), 5.4e-10 (NL
C0), 1.4e-7 (NL full), 6.1e-9 (NL noCE); tolerance 1e-4, all ok.

Reading: control 1 reproduces F4 and int8 numbers exactly; control 2a shows C0
at shipped floors gives the shipped verdict on 1,652 NL + 1,779 NAME queries,
0 mismatches, and its T2/T4 cells equal F4's; 2b reproduces the v2 refit
0.51/0.47/0.57/0.46. Control 3 (shuffled labels): NL observed caught delta
+22.7 pts vs shuffle mean +5.2, max +15.7, share >= observed 0.0; NAME observed
+29.5 vs shuffle mean -18.3, max +5.0, share 0.0. Control 5: L-m differs from
F4 on 282 NL (64 right) and 177 NAME (26 right) queries, so the arm measured
something. All controls pass. FIT vs held-out AUROC gap: see T9.

## Stopped arm

- NAME noCE: NAME noCE: coefficient of cos1 = -0.3037 <= 0; coefficients {"intercept": -3.8612424742533893, "cos1": -0.30365574398141193, "gap_cos": 1.134914569022017, "doc0": 0.2550124609560665, "clang:python": 0.9156936621967721, "clang:rust": 0.1023925975646553, "clang:typescript": 0.35876778522444686, "exact": 5.634599008711781}

L-noCE for name is not measured (empty cells in T4/T6, "stopped by rule" in T7).
L-noCE for NL is measured (T4/T5/T7/T9).

## Results

### T1 quality (held-out)

| arm | NL r@10 | NL MRR | name r@10 | name MRR |
|---|---|---|---|---|
| int8 | +61.18 [+58.18, +64.18] | +38.36 [+36.01, +40.88] | +93.61 [+91.65, +95.47] | +75.68 [+72.86, +78.46] |
| F4 | +67.50 [+64.62, +70.43] | +46.33 [+43.70, +49.04] | +96.47 [+95.10, +97.70] | +87.55 [+85.39, +89.64] |
| L-m | +67.50 [+64.62, +70.43] | +46.33 [+43.70, +49.04] | +96.47 [+95.10, +97.70] | +87.55 [+85.39, +89.64] |

### T2 D9 gates vs F4 (held-out NL, shipped floors as reference)

| arm | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language |
|---|---|---|---|---|---|---|---|
| int8 | -6.32 [-8.14] | -0.0797 [-0.0980] | rust -12.4 | +4.88 [+3.17, +6.62] | +0.00 [+0.00, +0.00] | Q1,Q2,Q3,Q4 | go +0.0 (n=39); python +0.0 (n=35); rust +0.0 (n=22); typescript +0.0 (n=58) |
| F4 | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | +0.00 [+0.00, +0.00] | +0.00 [+0.00, +0.00] | none | go +0.0 (n=71); python +0.0 (n=46); rust +0.0 (n=47); typescript +0.0 (n=83) |
| C0 | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | +0.00 [+0.00, +0.00] | +0.00 [+0.00, +0.00] | none | go +0.0 (n=71); python +0.0 (n=46); rust +0.0 (n=47); typescript +0.0 (n=83) |
| C0-m | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | -1.96 [-3.82, -0.04] | +2.57 [-1.08, +6.14] | Q5 | go -11.3 (n=71); python -4.3 (n=46); rust +31.9 (n=47); typescript -6.0 (n=83) |
| L-m | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | -16.39 [-18.97, -13.79] | -2.61 [-7.72, +2.49] | none | go -21.1 (n=71); python -6.5 (n=46); rust +6.4 (n=47); typescript +10.8 (n=83) |
| L-d6 | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | +21.58 [+18.94, +24.26] | -23.37 [-27.92, -18.97] | Q4 | go -36.6 (n=71); python -23.9 (n=46); rust -14.9 (n=47); typescript -18.1 (n=83) |

### T3 D9 gates vs int8

| arm | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language |
|---|---|---|---|---|---|---|---|
| int8 | +0.00 [+0.00] | +0.0000 [+0.0000] | go +0.0 | +0.00 [+0.00, +0.00] | +0.00 [+0.00, +0.00] | none | go +0.0 (n=49); python +0.0 (n=46); rust +0.0 (n=23); typescript +0.0 (n=66) |
| F4 | +6.32 [+4.55] | +0.0797 [+0.0616] | python +1.9 | -4.88 [-6.62, -3.17] | +0.00 [+0.00, +0.00] | none | go +0.0 (n=39); python +0.0 (n=35); rust +0.0 (n=22); typescript +0.0 (n=58) |
| C0 | +6.32 [+4.55] | +0.0797 [+0.0616] | python +1.9 | -4.88 [-6.62, -3.17] | +0.00 [+0.00, +0.00] | none | go +0.0 (n=39); python +0.0 (n=35); rust +0.0 (n=22); typescript +0.0 (n=58) |
| C0-m | +6.32 [+4.55] | +0.0797 [+0.0616] | python +1.9 | -6.84 [-9.29, -4.32] | +3.81 [-0.82, +8.66] | Q5 | go -10.3 (n=39); python -2.9 (n=35); rust +31.8 (n=22); typescript -3.4 (n=58) |
| L-m | +6.32 [+4.55] | +0.0797 [+0.0616] | python +1.9 | -21.27 [-24.18, -18.33] | -4.61 [-9.26, +0.10] | none | go -23.1 (n=39); python -5.7 (n=35); rust +0.0 (n=22); typescript +10.3 (n=58) |
| L-d6 | +6.32 [+4.55] | +0.0797 [+0.0616] | python +1.9 | +16.71 [+13.52, +19.91] | -17.99 [-22.99, -13.13] | Q4 | go -30.8 (n=39); python -22.9 (n=35); rust -4.5 (n=22); typescript -13.8 (n=58) |

### T4 GM-434 columns (held-out, each row at its own threshold)

| arm | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |
|---|---|---|---|---|---|---|
| int8 | 21.7% (40/184) | 49.6% (338/681) | 22.6% (37/164) | 1.5% (4/273) | 32.9% (138/420) | 21.8% (91/418) |
| F4 | 24.7% (61/247) | 43.5% (296/681) | 22.6% (37/164) | 2.9% (10/339) | 18.6% (78/420) | 21.8% (91/418) |
| C0 | 24.7% (61/247) | 43.5% (296/681) | 22.6% (37/164) | 2.9% (10/339) | 18.6% (78/420) | 21.8% (91/418) |
| C0-m | 24.7% (61/247) | 41.0% (279/681) | 24.4% (40/164) | 2.1% (7/339) | 18.1% (76/420) | 22.2% (93/418) |
| L-m | 22.3% (55/247) | 26.4% (180/681) | 10.4% (17/164) | 5.9% (20/339) | 4.3% (18/420) | 1.0% (4/418) |
| L-d6 | 0.8% (2/247) | 58.0% (395/681) | 73.8% (121/164) | 4.7% (16/339) | 4.5% (19/420) | 1.0% (4/418) |
| L-noCE | 22.3% (41/184) | 29.4% (200/681) | 22.6% (37/164) | - (0/0) | - (0/0) | - (0/0) |

### T5 GM-434 per language, held-out NL (misled / CW pos / CW absent)

| arm | language | misled | CW pos | CW absent | misled Wilson hi |
|---|---|---|---|---|---|
| int8 | go | 34.7% (17/49) | 48.9% (90/184) | 9.8% (4/41) | 48.7% |
| int8 | python | 23.9% (11/46) | 38.7% (60/155) | 22.5% (9/40) | 37.9% |
| int8 | rust | 8.7% (2/23) | 69.6% (112/161) | 37.8% (17/45) | 26.8% |
| int8 | typescript | 15.2% (10/66) | 42.0% (76/181) | 18.4% (7/38) | 25.7% |
| F4 | go | 36.6% (26/71) | 41.8% (77/184) | 9.8% (4/41) | 48.2% |
| F4 | python | 23.9% (11/46) | 38.7% (60/155) | 22.5% (9/40) | 37.9% |
| F4 | rust | 19.1% (9/47) | 59.0% (95/161) | 37.8% (17/45) | 32.5% |
| F4 | typescript | 18.1% (15/83) | 35.4% (64/181) | 18.4% (7/38) | 27.7% |
| C0 | go | 36.6% (26/71) | 41.8% (77/184) | 9.8% (4/41) | 48.2% |
| C0 | python | 23.9% (11/46) | 38.7% (60/155) | 22.5% (9/40) | 37.9% |
| C0 | rust | 19.1% (9/47) | 59.0% (95/161) | 37.8% (17/45) | 32.5% |
| C0 | typescript | 18.1% (15/83) | 35.4% (64/181) | 18.4% (7/38) | 27.7% |
| C0-m | go | 25.4% (18/71) | 50.0% (92/184) | 22.0% (9/41) | 36.6% |
| C0-m | python | 19.6% (9/46) | 45.8% (71/155) | 25.0% (10/40) | 33.2% |
| C0-m | rust | 51.1% (24/47) | 25.5% (41/161) | 13.3% (6/45) | 64.7% |
| C0-m | typescript | 12.0% (10/83) | 41.4% (75/181) | 39.5% (15/38) | 20.8% |
| L-m | go | 15.5% (11/71) | 35.9% (66/184) | 17.1% (7/41) | 25.7% |
| L-m | python | 17.4% (8/46) | 29.0% (45/155) | 7.5% (3/40) | 30.7% |
| L-m | rust | 25.5% (12/47) | 16.8% (27/161) | 4.4% (2/45) | 39.5% |
| L-m | typescript | 28.9% (24/83) | 23.2% (42/181) | 13.2% (5/38) | 39.4% |
| L-d6 | go | 0.0% (0/71) | 59.8% (110/184) | 82.9% (34/41) | 5.1% |
| L-d6 | python | 0.0% (0/46) | 64.5% (100/155) | 77.5% (31/40) | 7.7% |
| L-d6 | rust | 4.3% (2/47) | 57.1% (92/161) | 48.9% (22/45) | 14.2% |
| L-d6 | typescript | 0.0% (0/83) | 51.4% (93/181) | 89.5% (34/38) | 4.4% |
| L-noCE | go | 18.4% (9/49) | 31.0% (57/184) | 22.0% (9/41) | 31.4% |
| L-noCE | python | 23.9% (11/46) | 23.2% (36/155) | 25.0% (10/40) | 37.9% |
| L-noCE | rust | 47.8% (11/23) | 21.7% (35/161) | 2.2% (1/45) | 67.0% |
| L-noCE | typescript | 15.2% (10/66) | 39.8% (72/181) | 44.7% (17/38) | 25.7% |

### T6 GM-434 per language, held-out name (misled / CW pos / CW absent)

| arm | language | misled | CW pos | CW absent | misled Wilson hi |
|---|---|---|---|---|---|
| int8 | go | 2.2% (1/45) | 39.5% (30/76) | 11.8% (9/76) | 11.6% |
| int8 | python | 1.9% (1/52) | 27.6% (21/76) | 13.9% (10/72) | 10.1% |
| int8 | rust | 2.9% (2/69) | 49.7% (71/143) | 23.2% (29/125) | 10.0% |
| int8 | typescript | 0.0% (0/107) | 12.8% (16/125) | 29.7% (43/145) | 3.5% |
| F4 | go | 3.2% (2/63) | 17.1% (13/76) | 11.8% (9/76) | 10.9% |
| F4 | python | 6.2% (4/64) | 15.8% (12/76) | 13.9% (10/72) | 15.0% |
| F4 | rust | 3.2% (3/94) | 32.9% (47/143) | 23.2% (29/125) | 9.0% |
| F4 | typescript | 0.8% (1/118) | 4.8% (6/125) | 29.7% (43/145) | 4.6% |
| C0 | go | 3.2% (2/63) | 17.1% (13/76) | 11.8% (9/76) | 10.9% |
| C0 | python | 6.2% (4/64) | 15.8% (12/76) | 13.9% (10/72) | 15.0% |
| C0 | rust | 3.2% (3/94) | 32.9% (47/143) | 23.2% (29/125) | 9.0% |
| C0 | typescript | 0.8% (1/118) | 4.8% (6/125) | 29.7% (43/145) | 4.6% |
| C0-m | go | 1.6% (1/63) | 17.1% (13/76) | 11.8% (9/76) | 8.5% |
| C0-m | python | 3.1% (2/64) | 15.8% (12/76) | 18.1% (13/72) | 10.7% |
| C0-m | rust | 4.3% (4/94) | 31.5% (45/143) | 20.8% (26/125) | 10.4% |
| C0-m | typescript | 0.0% (0/118) | 4.8% (6/125) | 31.0% (45/145) | 3.2% |
| L-m | go | 1.6% (1/63) | 5.3% (4/76) | 0.0% (0/76) | 8.5% |
| L-m | python | 3.1% (2/64) | 3.9% (3/76) | 1.4% (1/72) | 10.7% |
| L-m | rust | 13.8% (13/94) | 7.7% (11/143) | 1.6% (2/125) | 22.2% |
| L-m | typescript | 3.4% (4/118) | 0.0% (0/125) | 0.7% (1/145) | 8.4% |
| L-d6 | go | 0.0% (0/63) | 5.3% (4/76) | 0.0% (0/76) | 5.7% |
| L-d6 | python | 3.1% (2/64) | 3.9% (3/76) | 1.4% (1/72) | 10.7% |
| L-d6 | rust | 10.6% (10/94) | 8.4% (12/143) | 1.6% (2/125) | 18.5% |
| L-d6 | typescript | 3.4% (4/118) | 0.0% (0/125) | 0.7% (1/145) | 8.4% |
| L-noCE | go | - (0/0) | - (0/0) | - (0/0) | - |
| L-noCE | python | - (0/0) | - (0/0) | - (0/0) | - |
| L-noCE | rust | - (0/0) | - (0/0) | - (0/0) | - |
| L-noCE | typescript | - (0/0) | - (0/0) | - (0/0) | - |

### T7 paired deltas vs F4 (held-out, 95% CI)

| pop | arm | kept | caught | caught Δ CI | kept Δ CI | differ (right) |
|---|---|---|---|---|---|---|
| NL | F4 | 75.3% (186/247) | 44.3% (265/598) | - | - | - |
| NL | C0 | 75.3% (186/247) | 44.3% (265/598) | [+0.0, +0.0] | [+0.0, +0.0] | 0 (0) |
| NL | C0-m | 75.3% (186/247) | 46.7% (279/598) | [-1.2, +5.9] | [-4.3, +4.4] | 148 (30) |
| NL | L-m | 77.7% (192/247) | 67.1% (401/598) | [+18.3, +27.3] | [-3.8, +8.9] | 282 (64) |
| NL | L-d6 | 99.2% (245/247) | 13.7% (82/598) | [-34.9, -26.2] | [+18.6, +29.4] | 294 (59) |
| NL | L-noCE vs int8 | 77.7% (143/184) | 64.1% (424/661) | [+16.4, +25.3] | [-8.5, +7.5] | - |
| NAME | F4 | 97.1% (329/339) | 66.1% (330/499) | - | - | - |
| NAME | C0 | 97.1% (329/339) | 66.1% (330/499) | [+0.0, +0.0] | [+0.0, +0.0] | 0 (0) |
| NAME | C0-m | 97.9% (332/339) | 66.1% (330/499) | [-1.2, +1.2] | [-0.3, +2.3] | 15 (5) |
| NAME | L-m | 94.1% (319/339) | 95.6% (477/499) | [+25.5, +33.7] | [-5.9, +0.0] | 177 (26) |
| NAME | L-d6 | 95.3% (323/339) | 95.4% (476/499) | [+25.3, +33.4] | [-4.5, +0.9] | 172 (22) |
| NAME | L-noCE vs int8 | stopped by rule | - | - | - | - |

### T8 sensitivity (v1 subset and strict held-out, NL)

| view | arm | misled | CW pos | CW absent |
|---|---|---|---|---|
| v1 subset | F4 | 17.2% (15/87) | 43.3% (93/215) | 21.7% (10/46) |
| v1 subset | L-m | 17.2% (15/87) | 24.2% (52/215) | 19.6% (9/46) |
| strict held-out | F4 | 24.7% (61/247) | 43.4% (295/679) | 22.6% (37/164) |
| strict held-out | L-m | 22.3% (55/247) | 26.4% (179/679) | 10.4% (17/164) |

The v1 view shows the same direction: CW pos 43.3% to 24.2%, misled equal at
17.2%; the conclusion does not depend on the v2 additions.

### T9 overfitting (FIT vs held-out)

| pop | model | AUROC FIT | AUROC HO | gap | log-loss FIT | log-loss HO |
|---|---|---|---|---|---|---|
| NL | full | 0.775 | 0.796 | -0.021 | 0.473 | 0.476 |
| NL | C0 | 0.653 | 0.659 | -0.006 | 0.552 | 0.572 |
| NL | noCE | 0.772 | 0.800 | -0.028 | 0.428 | 0.420 |
| NAME | full | 0.994 | 0.991 | +0.003 | 0.090 | 0.117 |
| NAME | C0 | 0.926 | 0.913 | +0.013 | 0.339 | 0.366 |

| pop | arm | kept FIT | kept HO | caught FIT | caught HO |
|---|---|---|---|---|---|
| NL | L-m | 71.6% | 77.7% | 69.9% | 67.1% |
| NL | L-d6 | 97.2% | 99.2% | 13.7% | 13.7% |
| NL | C0-m | 71.6% | 75.3% | 50.0% | 46.7% |
| NL | L-noCE | 71.9% | 77.7% | 66.5% | 64.1% |
| NAME | L-m | 96.4% | 94.1% | 97.8% | 95.6% |
| NAME | L-d6 | 97.2% | 95.3% | 97.1% | 95.4% |
| NAME | C0-m | 96.4% | 97.9% | 71.7% | 66.1% |

### T10 model (standardised coefficients, 95% bootstrap over FIT queries)

NL: lambda 1.0, CV log-loss {'0.01': 0.48660176502713604, '0.1': 0.4865489609542116, '1.0': 0.4864695393376358, '10.0': 0.49131991092452376}, gradient norm 1.51e-14

| term | coef | 95% CI |
|---|---|---|
| intercept | -1.221 | [-1.431, -1.053] |
| cos1 | -1.061 | [-1.577, -0.596] |
| cos_t | +1.347 | [+0.923, +1.865] |
| logit_t | +0.522 | [+0.283, +0.791] |
| gap_cos | +0.235 | [+0.063, +0.432] |
| gap_logit | +0.459 | [+0.276, +0.675] |
| has_doc | +0.317 | [+0.123, +0.529] |
| lang:python | -0.089 | [-0.309, +0.131] |
| lang:rust | -0.378 | [-0.606, -0.146] |
| lang:typescript | +0.023 | [-0.181, +0.256] |

NAME: lambda 1.0, CV log-loss {'0.01': 0.09875313880754932, '0.1': 0.09816104088167779, '1.0': 0.09733502729146278, '10.0': 0.110236535453203}, gradient norm 8.71e-15

| term | coef | 95% CI |
|---|---|---|
| intercept | -1.386 | [-1.921, -0.942] |
| cos1 | +0.879 | [+0.159, +1.665] |
| cos_t | -0.298 | [-1.090, +0.480] |
| logit_t | +2.372 | [+1.868, +3.050] |
| gap_cos | -0.129 | [-0.625, +0.365] |
| gap_logit | +1.386 | [+0.834, +2.144] |
| has_doc | +0.773 | [+0.423, +1.241] |
| lang:python | +0.061 | [-0.329, +0.509] |
| lang:rust | -0.167 | [-0.611, +0.267] |
| lang:typescript | +0.080 | [-0.425, +0.577] |
| exact | +1.835 | [+1.414, +2.506] |

## Verdict inputs (script output)

```
{
 "NL": {
  "q4_cw": [
   -0.16393738142574424,
   -0.18972290141328577,
   -0.13791370692820354
  ],
  "q4_superior": true,
  "q5_pass": true,
  "overfit": false,
  "readable": true
 },
 "NAME": {
  "misled_per_lang": {
   "go": {
    "misled": [
     0,
     63
    ],
    "wilson_hi": 0.05747319034852546,
    "ok": true
   },
   "python": {
    "misled": [
     2,
     64
    ],
    "wilson_hi": 0.10697493882294248,
    "ok": false
   },
   "rust": {
    "misled": [
     10,
     94
    ],
    "wilson_hi": 0.18485723534694615,
    "ok": false
   },
   "typescript": {
    "misled": [
     4,
     118
    ],
    "wilson_hi": 0.08392886166404062,
    "ok": false
   }
  },
  "misled_ok_all": false,
  "caught_delta_ci_L_d6": [
   0.25308578987150404,
   0.33401649907047487
  ],
  "overfit": false,
  "readable": true
 }
}
```

`ok` is the script's Wilson-upper check; the rule as stated is the point
estimate <= 3% with Wilson upper reported, which only go meets.

## Reproduce

```
python3 eval/embedding/gm461_learned_verdict.py \
  --work ~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work \
  --out full.json --md full.md --dump-fit fit.npz
~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -I eval/embedding/gm461_sklearn_check.py fit.npz
```

Outputs: `/private/tmp/claude-502/gm461s5/{full.json,full.md,run.log}`.

## Owner decision

2026-11-20, verbatim: "Текст: задача на внедрение, имена: оставить F4 (Recommended)".
Natural-language queries get a follow-up task to ship the learned verdict; that task must
deal with the TypeScript/Rust misled rise on an eval not used to choose shipping parameters.
Name queries keep F4.
