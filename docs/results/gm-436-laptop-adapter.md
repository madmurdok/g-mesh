# GM-436 laptop adapter: measure results

Slices GM-436/S18 (measure) and S19 (report), g-mesh release 4.29.0. Branch `feat/GM-436-laptop-embedding-training`; measured at tip 38250309, stages 10-11 rerun with the arm-role fix 63e1bf63 (section 8). The recommendation is in section 11, routes not tried in section 12.
Data root: `~/Projects/ClaudeProjects/g-mesh-data/gm436/measure/` (called `M/` below).

## 1. Run and machine state

Source: `/private/tmp/claude-502/gm436/wrapper_summary.txt`, `wrapper_summary2.txt`, `time.txt`, `time2.txt`; `M/summary.txt`.

| step | wall (`time -p` real) | user | sys | uptime at start -> end |
|---|---|---|---|---|
| build + dry + stages 1-10 (first run, THREADS=4) | 2790 s | 7704 s | 946 s | load 38.9/43.3/56.6 -> 5.7/8.1/10.5 |
| resume from stage 10 (stage 10 done, stage 11 failed) | 1695 s | 5811 s | 58 s | load 5.2/7.6/10.2 -> 4.1/6.8/8.1 |
| random + shuffled baseline runs, then stage 11 (report) | not timed with `time -p` | | | stage 11 started 14:57, load 67.7/25.3/15.0 |

Stages 1-9 finished in the first run (time covers build and dry-run too). The first run's "FAILED" line is stage 10.

Deviations from the plain script (no code changed):
1. Stage 10 failed in 1 s: `eval/embedding/v2/work/models/gte-small/model.onnx` not found (the worktree had no `v2/work`). Fix: symlink `eval/embedding/v2/work` to `~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work` (gitignored path). Stage 10 then ran all 156 (arm, corpus) runs, so the Rust `adapter` arm worked.
2. Stage 11 failed: `random` and `shuffled` runs had no `rankings.jsonl`, because stage 10 does not run them and stage 11 lists them (`M/report.log`). Fix: ran `debug-embed-eval run` for `random` and `shuffled` over the 6 corpora (`M/v2_baselines.log`), then `--from 11`. Stage 11 finished, `DONE`.
3. `eval/embedding/variants.toml` is modified in the worktree by the export stage (23 adapter rows); not committed.

## 2. Data and frozen configs

Source: `M/summary.txt`, `M/final/*/c5/result.json`, `~/Projects/ClaudeProjects/g-mesh-data/gm456/v0.4-full/card.md`.
- Training data: GM-456 v0.4-full, split by repo: 20600 train queries / 3198 dev queries (538 dev targets), 24 repos encoded. No train/dev query dropped (queryVector 0, positives 0).
- `final_configs.json` sha256: `7bb49ff01fb6f5f66d2939fdbe30e5147a11ecdb1287b49472a51c58e41c4482` (equals `M/final_configs.sha256`; frozen before stage 10).
- Selection, dev MRR (grid R1, 22 runs; `M/final_configs.json`):

| base | best form | dev MRR full | lowrank | procrustes |
|---|---|---|---|---|
| gte-small-structured | full-tau0.02-lam0.01 | 0.3494 | 0.3378 (r128) | 0.3192 |
| jina-v2-base-code-int8-structured | full-tau0.05-lam0.01 | 0.4640 | 0.4620 (r32) | 0.4317 |

- R2 (distillation from the int8 teacher onto gte, alpha 0.5, tau_s/tau_t 0.05): dev MRR 0.3499, form full-tau0.02-lam0.01.
- Dev (not v2) numbers for base (epoch 0): gte 0.3080 MRR; jina-int8 0.4303 MRR.

## 3. D9 table (v2 eval, 6 corpora, 1320 queries)

Source: `M/report/results.md`, `M/report/report.json`. r@10 and MRR with 95% bootstrap CI (10000 resamples, seed 398). Gates Q1-Q5 are as printed by the report, computed against the reference arm `jina-v2-base-code-fp32` (r@10 0.608, MRR 0.376), not against the arm's own base. P = pass, F = fail.
GM-398 and GM-460 columns are the report's "r@10 GM-398" and "r@10 confirm" subsets (the baselines come out of the same report, same arms).

| arm | r@10 [CI] | MRR [CI] | r@1 | r@100 | r@10 GM-398 | r@10 confirm | Q1-Q5 |
|---|---|---|---|---|---|---|---|
| jina-v2-base-code-fp32 (reference) | 0.608 | 0.376 | 0.268 | 0.869 | 0.633 | 0.626 | n/a |
| random | 0.010 | 0.006 | 0.002 | 0.107 | 0.005 | 0.013 | n/a |
| shuffled | 0.011 | 0.005 | 0.000 | 0.123 | 0.015 | 0.003 | n/a |
| **jina-int8-structured (base)** | 0.608 [0.587, 0.630] | 0.380 [0.362, 0.397] | 0.269 | 0.866 | 0.637 | 0.627 | PPPPP |
| int8 adapter full | 0.655 [0.634, 0.675] | 0.423 [0.406, 0.441] | 0.302 | 0.888 | 0.682 | 0.679 | PPPPP |
| int8 adapter lowrank | 0.645 [0.624, 0.666] | 0.424 [0.406, 0.442] | 0.308 | 0.889 | 0.667 | 0.672 | PPPPP |
| int8 adapter procrustes | 0.614 [0.593, 0.635] | 0.381 [0.363, 0.399] | 0.268 | 0.879 | 0.645 | 0.637 | FFPPP |
| int8 curve 25% | 0.630 | 0.412 | 0.305 | 0.881 | 0.680 | 0.654 | no gates |
| int8 curve 50% | 0.652 | 0.420 | 0.300 | 0.883 | 0.677 | 0.684 | no gates |
| **gte-small-structured (base)** | 0.505 [0.483, 0.528] | 0.319 [0.302, 0.337] | 0.223 | 0.776 | 0.565 | 0.520 | FFFFP |
| gte adapter full | 0.536 [0.514, 0.559] | 0.328 [0.311, 0.346] | 0.229 | 0.814 | 0.597 | 0.570 | FFFPP |
| gte adapter lowrank | 0.520 [0.498, 0.543] | 0.315 [0.298, 0.333] | 0.215 | 0.811 | 0.578 | 0.550 | FFFFF |
| gte adapter procrustes | 0.492 [0.470, 0.514] | 0.308 [0.291, 0.326] | 0.215 | 0.779 | 0.542 | 0.523 | FFFFP |
| gte curve 25% | 0.518 | 0.323 | 0.229 | 0.787 | 0.568 | 0.549 | no gates |
| gte curve 50% | 0.527 | 0.325 | 0.231 | 0.797 | 0.568 | 0.561 | no gates |
| gte R2 distill f=0.25 | 0.523 [0.502, 0.546] | 0.319 [0.302, 0.338] | 0.226 | 0.793 | 0.573 | 0.554 | FFFPF |
| gte R2 distill f=0.5 | 0.533 [0.511, 0.555] | 0.328 [0.310, 0.346] | 0.230 | 0.803 | 0.575 | 0.570 | FFFPP |
| gte R2 distill f=1.0 | 0.533 [0.511, 0.555] | 0.327 [0.310, 0.345] | 0.228 | 0.811 | 0.587 | 0.568 | FFFPF |

Best adapter per base against its own base (v2, point estimates):
- jina-int8: full, r@10 0.608 -> 0.655 (+0.047), MRR 0.380 -> 0.423 (+0.043). Lowrank has MRR 0.424.
- gte-small: full, r@10 0.505 -> 0.536 (+0.031), MRR 0.319 -> 0.328 (+0.009).
- Gate details for int8 full (`report.json`): Q1 recall@10 lower bound +3.1 pts; Q2 MRR lower bound +0.036; Q3 worst language typescript -0.3 pts; Q4 confident-wrong delta -9.7 pts (upper -7.3); Q5 false-alarm delta -5.7 pts (upper -2.1).
- Report verdicts after the fix (section 8; `M/report.log`, last report block):
  - int8 adapter full and int8 adapter lowrank: `Undecided("quality gates pass; costs (D11) not measured")`. The int8 base gets the same string (it passes Q1-Q5 as equivalence to fp32).
  - int8 adapter procrustes: `Fail(Q1, Q2)`.
  - gte base `Fail(Q1-Q4)`; gte adapter full `Fail(Q1-Q3)`; lowrank `Fail(Q1-Q5)`; procrustes `Fail(Q1-Q4)`; R2 distill f=0.25 `Fail(Q1,Q2,Q3,Q5)`, f=0.5 `Fail(Q1-Q3)`, f=1.0 `Fail(Q1,Q2,Q3,Q5)`.
  - Report winner: `none - keep jina-v2-base-code-fp32`. No arm can win without D11 cost numbers, so this is "not decided by the harness", not a quality loss.
- The int8 adapter gain is not given a paired CI against its own int8 base. The int8 base equals the fp32 reference on v2 (r@10 0.608 both, MRR 0.380 vs 0.376), so the gate CI vs fp32 is the closest paired number. The unpaired 95% CIs of int8 full and int8 base do not overlap (r@10 0.634 > 0.630, MRR 0.406 > 0.397).

## 4. GM-434 columns, NL vs name

Source: `M/report/report.json` (`floors`, `falseAlarmDelta`, `splits`). "Misled" = confident-wrong rate; false-alarm delta is vs the reference.

| arm | confident-wrong, positives | confident-wrong, absent | false-alarm delta | r@10 doc-targets (NL) | r@10 sig-targets (name) |
|---|---|---|---|---|---|
| jina-int8 base | 0.637 | 0.518 | -0.035 | 0.651 | 0.501 |
| int8 adapter full | 0.558 | 0.390 | -0.057 | 0.705 | 0.539 |
| int8 adapter lowrank | 0.583 | 0.561 | -0.052 | 0.702 | 0.516 |
| int8 adapter procrustes | 0.648 | 0.488 | -0.035 | 0.651 | 0.524 |
| gte base | 0.674 | 0.494 | -0.007 | 0.636 | 0.229 |
| gte adapter full | 0.573 | 0.543 | -0.016 | 0.662 | 0.286 |
| gte adapter lowrank | 0.651 | 0.726 | +0.042 | 0.644 | 0.283 |
| gte adapter procrustes | 0.749 | 0.701 | -0.090 | 0.606 | 0.245 |
| gte R2 f=0.5 | 0.634 | 0.561 | -0.031 | 0.658 | 0.279 |
| gte R2 f=1.0 | 0.602 | 0.518 | -0.007 | 0.650 | 0.290 |

The tokens-over-512/1024 shares are null for every corpus in `report.json`.

## 5. Per-language (r@10) and per-corpus

Source: `M/report/report.json` (`recall@10ByLanguage`), `M/report/results.md`.

| arm | go | python | rust | typescript |
|---|---|---|---|---|
| jina-int8 base | 0.679 | 0.658 | **0.415** | 0.682 |
| int8 adapter full | 0.761 | 0.721 | **0.442** | 0.694 |
| int8 adapter lowrank | 0.748 | 0.727 | 0.430 | 0.673 |
| int8 adapter procrustes | 0.700 | 0.679 | 0.400 | 0.676 |
| gte base | 0.527 | 0.518 | **0.394** | 0.582 |
| gte adapter full | 0.561 | 0.594 | **0.406** | 0.585 |
| gte adapter lowrank | 0.548 | 0.591 | 0.385 | 0.558 |
| gte adapter procrustes | 0.506 | 0.509 | 0.391 | 0.561 |

Rust: int8 full +2.7 pts, gte full +1.2 pts over their bases. Rust false-alarm delta (int8 full): -5.0 pts (CI -0.15..0.0), n=20; Rust own false-alarm rate 6.5%.

Per-corpus r@10 (`results.md`):

| arm | excalidraw | g-mesh | gin | py-requests | ripgrep | task-tracker-mcp |
|---|---|---|---|---|---|---|
| jina-int8 base | 0.62 | 0.39 | 0.68 | 0.66 | 0.44 | 0.78 |
| int8 adapter full | 0.62 | 0.41 | 0.76 | 0.72 | 0.48 | 0.82 |
| gte base | 0.52 | 0.41 | 0.53 | 0.52 | 0.38 | 0.68 |
| gte adapter full | 0.50 | 0.42 | 0.56 | 0.59 | 0.39 | 0.73 |

Unseen-in-training corpus: all 6 eval corpora are disjoint from the GM-456 training repos (leak guard recorded in card.md and the design doc, C4), so every column above is unseen. Control C4 (language-held-out, Go targets excluded from training, gte full): v2 Go r@10 0.533 vs 0.561 for the adapter trained on all languages and 0.527 for the gte base (`report.json`, `gte-structured-adapter-c4-no-go`). Overall r@10 0.531, MRR 0.322. Its dev result: MRR 0.3398, r@10 0.5719 (`M/final/gte-small-structured/c4-no-go/result.json`).

## 6. Words-shuffled gap and confirm subset

Source: `results.md` (WS gap@10 column, confirm column).

| pair | WS gap@10 |
|---|---|
| gte adapter full | +0.036 |
| gte identity | +0.004 |
| int8 adapter full | +0.030 |
| int8 identity | +0.000 |

ws-arm r@10: gte ws-adapter 0.500, gte ws-identity 0.502, int8 ws-adapter 0.625, int8 ws-identity 0.608.
Confirm-subset r@10 is in the table of section 3 (int8 full 0.679 vs base 0.627; gte full 0.570 vs base 0.520).

## 7. Controls

| control | required behavior | observed | source |
|---|---|---|---|
| C0 identity | identity arm equals base | gte identity r@10 0.505 / MRR 0.319, int8 identity 0.608 / 0.380; both identical to their bases. Behaved as required | `results.md` |
| C0b | vectors/comparator match on 6 repos, planted offset fails | pass on burntsushi-toml groupcache http ky mini-redis more-itertools (0 mismatches); planted offset fails. Behaved | `M/summary.txt`, `M/c0b.json` |
| C1 shuffled pairs | does not beat base | training: best epoch 0 (identity), dev MRR gte 0.3080, jina 0.4303 (epoch 1 onward worse, gte 0.165, jina 0.306); v2 arms equal base (gte 0.505/0.319, int8 0.608/0.380). Behaved | `final/*/c1/result.json`, `results.md` |
| C2 shuffled teacher | does not beat real R2 | dev MRR 0.3325 vs real R2 f=1.0 0.3499; v2 r@10 0.515 / MRR 0.314 vs R2 f=1.0 0.533 / 0.327. Lower than real R2; r@10 is 1.0 pt above the gte base (0.505), MRR 0.005 below it, both inside the base's CI. Behaved. (Before the fix the report also listed it as a broken arm, section 8) | `final/r2/c2/result.json`, `report.json` |
| C4 language-held-out | informational transfer check | section 5 | `final/gte-small-structured/c4-no-go/result.json` |
| C5 rerun | W sha256 identical | summary: "C5 (W sha256 identical)". gte W sha256 014d4c4d...c532d (= R1 grid); jina 42410f83...559c7 (= R1 grid). Behaved | `M/summary.txt`, `final/*/c5/result.json` |
| dry run, resume | W sha256 equal after kill+resume | dry run passed (first run reports "dry ok") | `wrapper_summary.txt` |
| random chance | observed near chance | random r@10 0.0098 vs chance 0.0123; passes | `report.json` validity |

## 8. Report validity

`report.json` validity after the fix: `errors` is empty and the report prints "validity: controls and floor parity pass". Controls random, random-chance and shuffled pass (r@10 gap to the reference 59.8 / 59.6 pts, bounds separate; random r@10 0.0098 vs chance 0.0123). Floor parity (shipped vs fitted) passes for go, python and rust; typescript is not gated (shipped 0.53, fitted 0.61).

Bug and fix. The first stage 11 run printed `validity: FAILED` and turned every gated arm's verdict into `Undecided("reference is not clearly above broken arm ...")`, 7 times. The export stage wrote the identity, C1 and C2 arms (gte/int8 identity, ws-identity, adapter-c1, distill-c2) to `variants.toml` with `role = control`. The harness treats a control arm as a D7 broken arm that must sit clearly below the reference. These arms equal their bases by construction (C2 is near the gte base), so the check could not pass for int8 and was meaningless for gte. Commit 63e1bf63 exports them as informational arms; stages 10-11 were rerun (`M/summary.txt`: 15:51, 156 runs, `DONE`). Every number in this doc is from the rerun; the arms' metrics did not change, only the validity and verdict lines.

## 9. Learning curve (dev MRR, 25/50/100% of train queries)

Source: `M/final/*/curve.json`, `M/summary.curve.txt`, `M/final/*/curve-*/result.json`. Training queries: 25% = 5151, 50% = 10299, 100% = 20600.

| base / route | 25% | 50% | 100% | 50->100 gain [95% CI] | log-linear MRR/doubling | extrapolated 2x / 4x |
|---|---|---|---|---|---|---|
| gte r1 full | 0.3306 [0.307, 0.354] | 0.3413 [0.318, 0.364] | 0.3494 [0.326, 0.372] | +0.0081 [+0.0001, +0.0162] | 0.0094 | 0.3592 / 0.3686 |
| jina-int8 r1 full | 0.4470 [0.421, 0.472] | 0.4566 [0.431, 0.482] | 0.4640 [0.439, 0.489] | +0.0074 [+0.0001, +0.0146] | 0.0085 | 0.4729 / 0.4814 |
| gte r2 (f=0.25/0.5/1.0) | see `final/r2/f*` | | 0.3499 | +0.0108 [+0.0039, +0.0177] | 0.0081 | 0.3571 / 0.3652 |

Curve rule output (all three): "Generate more GM-456 data: the 50%->100% dev MRR gain is ... (95% CI excludes 0). Extrapolation (log-linear fit over 25/50/100%, not a measurement)".

Dev train-vs-dev MRR at 100% (overfit, C6): gte 0.397 train / 0.349 dev; jina 0.457 train / 0.464 dev.

## 10. Hierarchical headroom diagnostic

Source: `M/summary.hier.txt`, `M/report/hier_headroom.json`.

| arm | r@10 flat | top-5-files | file oracle | gain (gate 5.0) | misses with file in top 10 | hits a top-5 file stage drops |
|---|---|---|---|---|---|---|
| jina-int8-structured | 73.4 | 73.9 | 89.6 | +16.2 (build) | 56.2% | 1.8% |
| gte-small-structured | 68.0 | 68.9 | 83.5 | +15.5 (build) | 65.4% | 1.2% |

(The headroom script's r@10 differs from the report's 0.608/0.505: it pools 2199 queries (`hier_headroom.json`, `queries`), not the report's 1320, so it is a different query set; not reconciled here.)

## 11. Recommendation

**Adopt the query-side adapter on the production model (jina-v2-base-code int8-structured), full form, as a production follow-up task that first measures D11 costs.** Do not adopt any gte-small route. Fine-tuning on GPU (GM-457) stays the route for larger gains, but it is not a prerequisite for shipping the adapter.

Options considered:

| option | evidence for | evidence against |
|---|---|---|
| **a. Adopt the int8 adapter (recommended)** | Only arms that pass every quality gate (section 3). Model already in production; node vectors and the index do not change | D11 costs not measured; production integration work (floors, reranker blend) |
| b. Fine-tune on GPU (GM-457) first | Learning curve and headroom say the encoder is not saturated | GPU quota pending; nights of work and ONNX parity before any number; the adapter gain is available now |
| c. Stop | Gains are moderate (+4.7 pts r@10) | Gains are outside the CIs, pass Q1-Q5 and survive every control |

Evidence:
- **Gains (v2, 1320 queries, 6 corpora unseen in training).** int8 adapter full: r@10 0.608 -> 0.655 [0.634, 0.675], MRR 0.380 -> 0.423 [0.406, 0.441], r@1 0.269 -> 0.302. Against the fp32 reference the lower bounds are +3.1 pts r@10 and +0.036 MRR. Lowrank (r32) is close: r@10 0.645, MRR 0.424, lower bounds +2.1 pts / +0.035. Full is the pre-registered dev winner and has the better r@10 and confident-wrong numbers, so it is the candidate; lowrank is the fallback if W size matters.
- **Safety columns.** Worst language vs the reference: typescript -0.3 pts (gate -10). Confident-wrong delta -9.7 pts (upper -7.3); confident-wrong on absent targets 0.518 -> 0.390. False-alarm delta -5.7 pts (upper -2.1). GM-398 subset r@10 0.637 -> 0.682, confirm subset 0.627 -> 0.679. Gain on 5 of 6 corpora, flat on excalidraw (0.62).
- **Controls behaved** (section 7): identity equals its base on every metric, C0b vector parity on 6 repos with a planted fault caught, C1 shuffled pairs never beat epoch 0, C2 shuffled teacher below real R2, C5 rerun gives identical W sha256, random at chance. Report validity passes after the arm-role fix (section 8).
- **Learning curve says more data helps, a little.** Dev MRR 50% -> 100% gain +0.0074 [+0.0001, +0.0146] for jina, CI excludes 0. The log-linear extrapolation (not a measurement) is +0.009 dev MRR for 2x data. More GM-456 data is worth generating, but expect small steps.
- **Headroom.** The hierarchical diagnostic's file oracle gains +16.2 pts r@10 for int8 (gate 5.0: build), and 56.2% of misses have the expected file in the top 10. The simple top-5-files stage gains only +0.5 pts, so the headroom is in a better file stage, not in the one measured.
- **gte-small is not a cost candidate.** Best gte route (adapter full, R2 f=0.5) reaches r@10 0.533-0.536, still 7.1-7.5 pts below the reference, failing Q1-Q3. Distillation in adapter form adds nothing over the gold-label adapter (0.533 vs 0.536). By D5's gate (gte Q1/Q2 still failing, R2 dev MRR +0.042 over gte base, point estimate) the gte student fine-tune is warranted, and it already sits in GM-457.

Costs:
- **Query time:** one d x d matvec per query (d = 768 for jina; ~0.6M multiply-adds, expected well under 1 ms next to a jina query pass). Name queries that bypass the embedding (`find_definition`) are unaffected.
- **Index time:** none. The adapter is query-side only; node vectors, the index and the embedding cache stay as they are, so no re-embed.
- **Size:** W for the full form is 768 x 768 f32, about 2.4 MB; lowrank stores two 768 x 32 factors.
- **Not measured (D11).** The report withholds the verdict for this reason. To measure: `eval/embedding/measure_costs.sh` (GM-398 S14 recipe, `docs/architecture/embedding-eval.md` D11) on the quiet machine, with the int8 base and the int8 adapter arm interleaved: median query-embedding latency over the 500 queries, embed-all pass time and max RSS over the g-mesh snapshot under `/usr/bin/time -lp`, and model size including the W file. The quality-candidate cost gates are <= 1.5x pass time, <= 1.2x RSS, size < 1 GB. Pass time and RSS should equal the base (the adapter is not on the index path); that equality is the check.

Risks:
- **Train/eval shift.** GM-456 queries are LLM-written; agent queries may differ. v2 is unseen, and C4 (Go held out: Go r@10 0.533 vs 0.561 trained on all, 0.527 base) shows most of the gain needs the language in training. A production rollout needs the agent-level veto (D10) on real queries.
- **Floors and the reranker blend.** The adapter shifts the score distribution. Production needs its own `similarity::floor` table for adapted queries, kept apart from name queries, and the F4 reranker BETA retuned. This is integration work the eval does not cover.
- **Coupling to the model.** W is tied to the int8-structured vectors. Any change of model, quantisation or node text format means retraining W (minutes on the laptop) and rerunning C0/C0b.
- **Rust stays weak.** Rust r@10 0.415 -> 0.442 (+2.7 pts); the adapter does not fix the encoder's Rust blind spot. The words-shuffled gap +0.030 shows the adapter uses some word order, but the base nodes still behave mostly as bag of words.
- **Multiple comparisons.** Two int8 forms pass; only full was pre-registered as the dev winner, and it is the one recommended.

## 12. Not tried, and follow-ups

Not tried in this batch (owner decisions of 2026-11-13, design doc):
- **TSDAE** (D7: "record not tried"). It tunes the whole encoder, so it has the student fine-tune's CPU cost plus ONNX export and parity; full-encoder routes are deferred to GPU (D5, GM-457). With 20.6k supervised pairs available, an unsupervised denoising baseline is expected to lose to the supervised student. Closed by argument, not data.
- **CodeRankEmbed** (D8). A base-model bake-off, not training: 137M-parameter NomicBERT, about jina's cost, no published ONNX, custom architecture that is a risk for the Rust loader. Tracked as the backlog research task "Research: CodeRankEmbed as a base embedding model".
- **Full gte-small student fine-tune (R3), LoRA/full jina fine-tune, reranker distillation:** deferred to GPU (D5, GM-457).

Proposed follow-ups:
1. **Production task: int8 query adapter.** Measure D11 first (section 11, Costs); then ship W (full form) with the eval's `core/src/embedding/adapter.rs` format behind a setting, its own floor table, BETA retune, and the D10 agent-level check.
2. **More GM-456 data** (curve rule). Generate a 2x set and retrain W; expected gain is small (+0.009 dev MRR extrapolated), cheap on the laptop.
3. **Hierarchical retrieval, full arm.** File-oracle headroom +16.2 pts passes the D6 gate; design a better file stage than top-5 centroids (+0.5 pts) before writing a ranker.
4. **GM-457 (GPU).** The gte student (D5 gate met by point estimate), LoRA on jina, reranker distillation; TSDAE only if those show the encoder moves.
5. **CodeRankEmbed bake-off** via the backlog research task above.
