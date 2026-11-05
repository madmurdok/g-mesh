# GM-697/S6: name-only floor refit on the GM-460 v2 eval set

Script: `eval/embedding/gm697_floor_refit.py` (stdlib; reads stored rankings; reuses `q5_floor_sensitivity.load_arm/fit_floors`, `shipped_floor_rates.structured_floors/fit_floor`, `gm468_name_floors.wilson_hi`). Binary: `cargo build --release --bin g-mesh` of branch `feat/GM-697-refit-similarity-floors`; `debug-embed-eval run --eval-dir eval/embedding/v2 --variant <arm>` for `jina-v2-base-code-int8-structured` and `jina-v2-base-code-fp32` (fp32 with a throwaway `HOME` linking the stored fp32 weights, as in GM-460 deviation 1). Shipped floors are parsed from `core/src/mcp/similarity.rs` (0.57 / 0.59 / 0.57 / 0.53); S19 and GM-468 constants are parsed from their results files.

Command: `python3 eval/embedding/gm697_floor_refit.py --run eval/embedding/v2/work/runs/jina-v2-base-code-int8-structured --fp32-run .../jina-v2-base-code-fp32 --eval-dir eval/embedding/v2`.

Definitions: name = mechanical queries, split by `sha256(id)[0]` odd = held-out (fit half = even); NL = authored, harness `held_out`. Floor looked up by the language of the top row. Misled = held-out name rank-1-right positive below floor. Soft-ok = NL held-out rank-1-right below floor; soft-miss = NL held-out positive with wrong/absent top, below floor (denominator: all positives of that language); CW pos / CW absent = top clears the floor with a wrong top row / on an absent-answer page. find_definition columns use all name queries, page definition (an expected row in the top 3): false refusal = such a positive with top below floor; hopeless offer = absent page whose top clears the floor. F = 3% quantile of fit-half rank-1-right name scores, rounded down to 2dp. D2 case column is filled on the shipped rows only.

## Machine state

Run 14:48-15:03 (date 2026-11-05). `uptime` at start: load 8.38 6.54 5.32; at the structured run start 74.32 76.77 48.08; at the fp32 start 58.11 62.62 46.44; at the end 55.29 92.40 71.54. `pmset -g therm`: no thermal or performance warning recorded, but CPU_Speed_Limit 58 at start and 48 at end (below 100). The machine was heavily loaded by other processes during the runs (contrary to the one-measurement-at-a-time intent; nothing else was started by this slice). `/usr/bin/time -p` real / user / sys seconds: release build 461.47 / 90.56 / 39.00; int8-structured run 143.78 / 385.21 / 6.05 (S19 on a quiet machine: 54.9 real); fp32 run 323.56 / 680.09 / 14.69 (S19: 93.1). user is 2.7x and 2.1x real, so the runs were CPU-contended; timings are state, not a cost measurement. Quality numbers do not depend on timing.

## Controls

| control | result | got | expected |
|---|---|---|---|
| self-test: planted mismatches are rejected | PASS | ok | ok |
| C1 pooled D6 fit == S19 | PASS | {'typescript': 0.46, 'rust': 0.57, 'go': 0.51, 'python': 0.47} | {'go': 0.51, 'python': 0.47, 'rust': 0.57, 'typescript': 0.46} |
| C2 shape-blind (FA, CW pos, CW abs) at shipped == S19 | PASS | [(40, 184), (338, 681), (37, 164)] | [(40, 184), (338, 681), (37, 164)] |
| C2 shape-blind (FA, CW pos, CW abs) at D6 refit == S19 | PASS | [(8, 184), (434, 681), (85, 164)] | [(8, 184), (434, 681), (85, 164)] |
| C3 name fit, all mechanical, rank-1 raw == GM-468 strict | PASS | ['0.594', '0.618', '0.573', '0.576'] | ['0.594', '0.618', '0.573', '0.576'] |

## Table S2

| floors | lang | floor | name fit-half n / F (raw, 2dp) | name held-out misled k/n (Wilson up) | name CW pos | name CW absent | NL soft-ok | NL soft-miss | NL CW pos | NL CW absent | find_definition false refusal / hopeless offer (page def) | D2 case |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| shipped | go | 0.57 | 42 / 0.673, 0.67 | 1/45 (11.6%) | 30/76 (39.5%) | 9/76 (11.8%) | 17/49 (34.7%) | 45/135 (33.3%) | 90/184 (48.9%) | 4/41 (9.8%) | 3/131 (2.3%) / 18/150 (12.0%) | case 1 (keep shipped) |
| shipped | python | 0.59 | 56 / 0.629, 0.62 | 1/52 (10.1%) | 21/76 (27.6%) | 10/72 (13.9%) | 11/46 (23.9%) | 49/109 (45.0%) | 60/155 (38.7%) | 9/40 (22.5%) | 4/127 (3.1%) / 17/150 (11.3%) | case 1 (keep shipped) |
| shipped | rust | 0.57 | 90 / 0.573, 0.57 | 2/69 (10.0%) | 71/143 (49.7%) | 29/125 (23.2%) | 2/23 (8.7%) | 26/138 (18.8%) | 112/161 (69.6%) | 17/45 (37.8%) | 6/209 (2.9%) / 78/300 (26.0%) | case 1 (keep shipped) |
| shipped | typescript | 0.53 | 133 / 0.552, 0.55 | 0/107 (3.5%) | 16/125 (12.8%) | 43/145 (29.7%) | 10/66 (15.2%) | 39/115 (33.9%) | 76/181 (42.0%) | 7/38 (18.4%) | 4/267 (1.5%) / 71/300 (23.7%) | case 1 (keep shipped) |
| shipped | pooled | 0.57 / 0.59 / 0.57 / 0.53 | 321 / - | 4/273 (3.7%) | 138/420 (32.9%) | 91/418 (21.8%) | 40/184 (21.7%) | 159/497 (32.0%) | 338/681 (49.6%) | 37/164 (22.6%) | 17/734 (2.3%) / 184/900 (20.4%) |  |
| D6 v2 refit | go | 0.51 | 42 / 0.673, 0.67 | 0/45 (7.9%) | 31/76 (40.8%) | 18/76 (23.7%) | 2/49 (4.1%) | 16/135 (11.9%) | 119/184 (64.7%) | 13/41 (31.7%) | 1/131 (0.8%) / 33/150 (22.0%) |  |
| D6 v2 refit | python | 0.47 | 56 / 0.629, 0.62 | 0/52 (6.9%) | 24/76 (31.6%) | 32/72 (44.4%) | 0/46 (0.0%) | 5/109 (4.6%) | 104/155 (67.1%) | 29/40 (72.5%) | 0/127 (0.0%) / 70/150 (46.7%) |  |
| D6 v2 refit | rust | 0.57 | 90 / 0.573, 0.57 | 2/69 (10.0%) | 71/143 (49.7%) | 30/125 (24.0%) | 2/23 (8.7%) | 26/138 (18.8%) | 112/161 (69.6%) | 18/45 (40.0%) | 6/209 (2.9%) / 81/300 (27.0%) |  |
| D6 v2 refit | typescript | 0.46 | 133 / 0.552, 0.55 | 0/107 (3.5%) | 18/125 (14.4%) | 67/145 (46.2%) | 4/66 (6.1%) | 16/115 (13.9%) | 99/181 (54.7%) | 25/38 (65.8%) | 0/267 (0.0%) / 128/300 (42.7%) |  |
| D6 v2 refit | pooled | 0.51 / 0.47 / 0.57 / 0.46 | 321 / - | 2/273 (2.6%) | 144/420 (34.3%) | 147/418 (35.2%) | 8/184 (4.3%) | 63/497 (12.7%) | 434/681 (63.7%) | 85/164 (51.8%) | 7/734 (1.0%) / 312/900 (34.7%) |  |
| name-fit FIT half | go | 0.67 | 42 / 0.673, 0.67 | 5/45 (23.5%) | 24/76 (31.6%) | 2/76 (2.6%) | 37/49 (75.5%) | 107/135 (79.3%) | 28/184 (15.2%) | 0/41 (0.0%) | 15/131 (11.5%) / 3/150 (2.0%) |  |
| name-fit FIT half | python | 0.62 | 56 / 0.629, 0.62 | 3/52 (15.6%) | 20/76 (26.3%) | 8/72 (11.1%) | 16/46 (34.8%) | 62/109 (56.9%) | 47/155 (30.3%) | 5/40 (12.5%) | 7/127 (5.5%) / 12/150 (8.0%) |  |
| name-fit FIT half | rust | 0.57 | 90 / 0.573, 0.57 | 2/69 (10.0%) | 71/143 (49.7%) | 28/125 (22.4%) | 2/23 (8.7%) | 30/138 (21.7%) | 108/161 (67.1%) | 17/45 (37.8%) | 6/209 (2.9%) / 75/300 (25.0%) |  |
| name-fit FIT half | typescript | 0.55 | 133 / 0.552, 0.55 | 1/107 (5.1%) | 15/125 (12.0%) | 35/145 (24.1%) | 13/66 (19.7%) | 47/115 (40.9%) | 68/181 (37.6%) | 5/38 (13.2%) | 6/267 (2.2%) / 59/300 (19.7%) |  |
| name-fit FIT half | pooled | 0.67 / 0.62 / 0.57 / 0.55 | 321 / - | 11/273 (7.1%) | 130/420 (31.0%) | 73/418 (17.5%) | 68/184 (37.0%) | 246/497 (49.5%) | 251/681 (36.9%) | 27/164 (16.5%) | 34/734 (4.6%) / 149/900 (16.6%) |  |

## fp32 reference arm

| fp32 arm name-fit | go | python | rust | typescript |
|---|---|---|---|---|
| FIT half n / raw / 2dp | 42 / 0.663 / 0.66 | 52 / 0.593 / 0.59 | 83 / 0.572 / 0.57 | 129 / 0.585 / 0.58 |
| all mechanical, rank-1, raw / 2dp | 0.594 / 0.59 | 0.598 / 0.59 | 0.572 / 0.57 | 0.612 / 0.61 |

## Case-2 proposals

none

## Control notes

C1-C3 pass on the int8-structured arm (and on the fp32 re-run, which printed the same controls on the int8-structured rankings). Self-test: with the expected values perturbed (go 0.52; shipped FA 41/184; refit CW absent 86/164; C3 go 0.595) the script exits 3 and all four comparisons print FAIL. The in-script "self-test" line is a trivial sanity assertion; the planted-mismatch test above was run by hand once.

## Reading

- Name fit-half F: go 0.67 (n 42), python 0.62 (56), rust 0.57 (90), typescript 0.55 (133). The fit halves are small (42-133), and F moves a lot between halves: go all-mechanical raw is 0.594 (C3) against 0.673 on the fit half. Name-fit floors applied to the held-out half are worse than shipped for go (5/45 misled, 23.5%) and python (3/52), i.e. the half-fit does not generalise at these n; the shipped values give 1/45, 1/52, 2/69, 0/107 misled.
- Shipped is at or below F + 0.01 in every language (go 0.57 vs 0.67, python 0.59 vs 0.62, rust 0.57 vs 0.57, typescript 0.53 vs 0.55) and held-out name misled at shipped is 2.2% (1/45), 1.9% (1/52), 2.9% (2/69), 0.0% (0/107), all at or under 3% (Wilson uppers 11.6 / 10.1 / 10.0 / 3.5%).
- D6 v2 refit lowers NL soft-ok to 4.3% but raises NL CW absent to 51.8% and name CW absent to 35.2% (shipped 21.8%); it is not a name-population fit.
- fp32 reference arm, name-fit floors (go / python / rust / typescript): FIT half 0.66 / 0.59 / 0.57 / 0.58 (n 42 / 52 / 83 / 129); all mechanical rank-1 0.59 / 0.59 / 0.57 / 0.61 (raw 0.594 / 0.598 / 0.572 / 0.612). Against shipped 0.57 / 0.59 / 0.57 / 0.53, the parity gap (go/python/rust) is, on all-mechanical fits, 0.02 / 0.00 / 0.00, and on FIT-half fits 0.09 / 0.00 / 0.00 (go exceeds 0.03 only through the small fit half). Typescript (0.08 / 0.05) is not gated by parity.

## Exit verdict (D2 case per language)

- go: "shipped <= F + 0.01 and held-out name misled at the shipped floor <= 3%": F 0.67, shipped 0.57, misled 1/45 = 2.2% (Wilson upper 11.6%) -> case 1, keep the shipped value.
- python: F 0.62, shipped 0.59, misled 1/52 = 1.9% (10.1%) -> case 1, keep.
- rust: F 0.57, shipped 0.57, misled 2/69 = 2.9% (10.0%) -> case 1, keep (2.9% is 0.1 point under the 3% line; one more miss would flip it).
- typescript: F 0.55, shipped 0.53, misled 0/107 = 0.0% (3.5%) -> case 1, keep.

No language is case 2: no floor value change is proposed.
