# GM-703: CodeRankEmbed fp32 vs jina int8-structured

Arm `coderankembed-fp32-structured` against reference `jina-v2-base-code-int8-structured` on the v2 eval (6 corpora, 1,320 NL queries), measured at branch `research/GM-703-coderankembed-base-model`, commit 05781269. Raw output: `~/Projects/ClaudeProjects/g-mesh-data/gm703/measure/` (`s6-report.json`, `report/results.md`, `costs/summary.md`).

## Verdict

**No-go.** The decision rule reads s6-report: `coderankembed-fp32-structured ... verdict Fail(["Q5"])`. Q1-Q4 pass; Q5 (false alarm) fails. Per the orchestrator ruling, "beats jina" means the quality gates only, so fp32 does not beat jina.

Owner decision: pending

## Method

- `ARM=coderankembed-fp32-structured eval/embedding/gm703/measure.sh --from 3` (release build with `embed-eval`, six corpus runs, P3 parity per corpus, s6 report, cost pass, GM-434 report set). Every step rc=0, P3 passed at cosine >= 0.99999 on all six corpora.
- Control arms `random` and `shuffled` reused from the v2 runs; validity: controls and floor parity pass.
- Wall time 19,769 s (5 h 29 min; the cost pass alone 16,228 s). Load: 16 at the start, 17-18 during the quality runs, up to 522 during the cost pass (`uptime` before 20:46 load 16.4, after 02:16 load 464). The cost-pass timings were taken under that load (see Cost pass).

## Parity summary (S2)

From `/private/tmp/claude-502/gm703s2/summary.md`:

- fp32 `model.onnx` 548,110,535 B, sha256 a7e74b4942a631b0f51acf186949b3b9d2aa8e10dade1067bace06b0a7c4bf2d; int8 138,964,289 B, sha256 e626b8fd5a1eb7b3e52d050bf4f51c31eb5f431fb67bf1831f465ff0d7f03cc0; safetensors 546,938,168 B.
- P1/P1b/P1c/P1d pass (P1d max 2.5e-4 at 1024 tokens); Q-F pass.
- P3 (Rust vs Python query embeddings) on fp32 passes: min cosine 0.9999999999991, 444 queries (S2, task-tracker-mcp); repeated here on all six corpora, all pass.

## Q-D int8 stop numbers

The int8 CodeRankEmbed file fails Q-D: median 0.9625, p1 0.9193, against jina int8's 0.9905 / 0.9490. The int8 arm was stopped at S2; this run is fp32 only.

## fp32 vs jina int8-structured under D9

Pooled, languages weighted equally, with per-model refit floors (jina go 0.51 / python 0.47 / rust 0.57 / typescript 0.46; fp32 go 0.32 / python 0.32 / rust 0.42 / typescript 0.31).

| arm | r@1 | r@5 | r@10 [lo, hi] | r@100 | MRR [lo, hi] | CW comb % | CW pos % | CW abs % |
|---|---|---|---|---|---|---|---|---|
| coderankembed-fp32-structured | 0.346 | 0.585 | 0.673 [0.653, 0.694] | 0.897 | 0.453 [0.435, 0.472] | 45.6 | 48.6 | 32.9 |
| jina-v2-base-code-int8-structured | 0.269 | 0.506 | 0.608 [0.587, 0.630] | 0.866 | 0.380 [0.362, 0.397] | 61.4 | 63.7 | 51.8 |
| random | 0.002 | 0.004 | 0.010 | - | 0.006 | 89.7 | 89.3 | 91.7 |
| shuffled | 0.000 | 0.007 | 0.011 | - | 0.005 | 22.2 | 27.0 | 0.0 |

Gates for fp32:

- Q1 PASS: recall@10 lower bound +4.4 pts.
- Q2 PASS: MRR lower bound +0.057.
- Q3 PASS: worst language typescript +3.0 pts.
- Q4 PASS: confident-wrong delta -16.0 pts (upper -13.1).
- **Q5 FAIL**: false-alarm delta +4.7 pts, upper +9.2 pts (must be <= +5 and <= 0); own held-out false-alarm rates go 15.5%, python 12.0%, rust 25.0%, typescript 11.2% against jina 4.1 / 0.0 / 8.7 / 6.1%.

Floors and own rates are refit per model, so fp32's lower cosine range (floors 0.31-0.42 vs 0.46-0.57) is already accounted for; Q5 still fails.

## Per language

| language | jina r@10 | fp32 r@10 | Q5 delta (n, [lo, hi]) | fp32 own FA | jina own FA |
|---|---|---|---|---|---|
| go | 0.679 | 0.712 | +2.8 pts (36, [-5.6, 11.1]) | 15.5% | 4.1% |
| python | 0.658 | 0.712 | +10.3 pts (29, [3.4, 20.7]) | 12.0% | 0.0% |
| **rust** | **0.415** | **0.558** | **+0.0 pts (20, [-10.0, 10.0])** | **25.0%** | **8.7%** |
| typescript | 0.682 | 0.712 | +5.7 pts (53, [1.9, 11.3]) | 11.2% | 6.1% |

Rust: the largest recall gain (+14.3 pts r@10) and the smallest Q5 delta (0.0 pts), but fp32's own Rust false-alarm rate is the highest of the four (25.0%, 11 of 44) and the Rust interval is wide (n=20).

Per corpus r@10 (jina / fp32): excalidraw 0.62 / 0.65, g-mesh 0.39 / 0.62, gin 0.68 / 0.71, py-requests 0.66 / 0.71, ripgrep 0.44 / 0.50, task-tracker-mcp 0.78 / 0.81.

## GM-434 columns

From `report/results.md`:

| arm | MRR | r@1 | r@10 | r@100 | r@10 GM-398 | r@10 confirm |
|---|---|---|---|---|---|---|
| jina-v2-base-code-int8-structured | 0.380 | 0.269 | 0.608 | 0.866 | 0.637 | 0.627 |
| coderankembed-fp32-structured | 0.453 | 0.346 | 0.673 | 0.897 | 0.708 | 0.710 |

The WS gap@10 column is empty (no words-shuffled arm for fp32). Splits (fp32 vs jina r@10 delta): targetDoc=doc +10.9 pts [8.3, 13.5]; targetDoc=sig -2.8 pts [-6.6, +0.8]; targetDoc=mixed -1.0 pt [-4.8, +2.9]; pathOverlap=false +6.2 pts; pathOverlap=true +8.3 pts. The gain comes from doc-described targets; signature-only targets do not improve.

## Confirm subset

False alarm and confident-wrong rates, NL held-out (GM-398 subset, confirm subset), at each model's own refit floors:

| set | arm | false alarm | CW positives | CW absent |
|---|---|---|---|---|
| full v2 | jina | 4.3% (8/184) | 63.7% | 51.8% |
| full v2 | fp32 | 15.1% (37/245) | 48.6% | 32.9% |
| GM-398 subset | jina | 4.5% (3/66) | 60.9% | 60.9% |
| GM-398 subset | fp32 | 10.7% (9/84) | 46.0% | 43.5% |
| confirm subset | jina | 5.3% (5/95) | 63.0% | 42.7% |
| confirm subset | fp32 | 18.2% (22/121) | 50.9% | 28.0% |

The confirm subset agrees in direction with the full set: fp32 has fewer confident-wrong results and more false alarms. Per-language tables: `report/floors-*.json`.

## Recall

recall@10 0.673 vs 0.608 (+6.5 pts; lower bound +4.4); recall@100 0.897 vs 0.866; recall@1 0.346 vs 0.269. MRR 0.453 vs 0.380. Tokens over 512: at most 0.2% of nodes (py-requests), none over 1024.

## Cost pass

Run on the g-mesh corpus (D11), three rounds, medians. Information only: the C-quality line in `s9-report-costs` is ignored for the verdict. Timing is unreliable: the machine load rose from 6-136 to 526 during the rounds (jina r1 597.6 s vs r3 793.8 s; fp32 r1 1511 s vs r3 1123 s).

| variant | pass s | x jina | max RSS MB | x jina | model size MB | x jina | query ms | x jina |
|---|---|---|---|---|---|---|---|---|
| jina-v2-base-code-int8-structured | 718.6 | 1.00 | 520 | 1.00 | 164.5 | 1.00 | 27.61 | 1.00 |
| coderankembed-fp32-structured | 1147.9 | 1.60 | 1324 | 2.55 | 548.8 | 3.34 | 66.86 | 2.42 |

Cost gate C-quality (not part of the verdict): pass 1.60x (limit 1.5), RSS 2.55x (limit 1.2), size 549 MB (limit 1000).

## Informational

`report_gm436` (no control arms) reports `Undecided("no control arm among the runs")` for fp32; informational only.

## Owner decision

Owner decision (2026-11-21): "Задачи в бэклог: int8 + дообучение CRE (Recommended)".

- No-go for CodeRankEmbed as a drop-in replacement; jina stays the default model.
- The size gap is fp32-only: the CRE int8 export is 139 MB (jina int8 162 MB). Its
  quality was never measured end to end because it failed Q-D (median cosine 0.9625).
- The GM-730 J model (fine-tuned jina, int8) reaches v2 MRR 0.501 and r@10 0.733,
  above CRE fp32 here (0.453 / 0.673).
- Follow-ups in the backlog: a quality-keeping CRE int8 recipe measured on v2, then
  fine-tuning CRE with the GM-730 recipe and comparing it with J.
