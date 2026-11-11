# GM-701/S2: shipped floor alone vs floor OR per-language blend threshold (held-out, paired)

Script: `eval/embedding/gm701_floor_or_blend.py`. Command:
`python3 eval/embedding/gm701_floor_or_blend.py --work eval/embedding/v2/work --out gm701.json` (defaults: 10000 resamples, 200 shuffles).
Run: 10.74 s real / 8.41 user / 0.83 sys (`/usr/bin/time -p`), `uptime` before `load 6.85 16.61 11.48`, after `6.10 15.96 11.33`. Compute-bound, not waiting.

## Method

- Data and scoring reuse GM-699: v2 work dir rebuilt by GM-701/S1, Rust logit cache `rerank_cache_gm700_rust`, shipped CE sha256 checked (`c80a8b34...bf9`), ship text, K=30, BETA 40, exact-name skip rule. Rows with an empty window are dropped (as in GM-699).
- Per query: **right** = reranked top-1 is an expected row; wrong = other top row; absent queries count as wrong. **floor flag** = cosine of the cosine-order top-1 < shipped GM-697 floor of that row's language (0.57 go / 0.59 python / 0.57 rust / 0.53 typescript, parsed from `core/src/mcp/similarity.rs`), the page `handle` judges. **blend** = logit + 40*cosine of the reranked top-1.
- Populations: NL = authored queries (FIT/held-out by harness flag; 807 / 845 rows). NAME = mechanical queries (FIT/held-out by sha256(id)[0] odd = held-out, mirror of `is_held_out_id`; 941 / 838 rows). Thresholds are fitted on FIT of each population separately, per language, then applied unchanged to its held-out half.
- Arms (flag = page refused/noted): **A** floor alone. **B** floor OR blend < t[lang], t = GM-699 cut (blend alone keeps 90% of FIT right tops). **B2** floor OR blend < t2[lang], t2 = largest cut at which the OR rule itself keeps >= 90% of FIT right tops (B2 == A in a language where the floor alone already keeps < 90%). **C** blend alone at t (context).
- Paired bootstrap: 10,000 resamples of held-out queries, seed 701, caught(arm) - caught(A) on the resampled wrong/absent set, kept delta likewise. Percentile 95% CI.

## Results (held-out)

Thresholds t (blend, FIT) NL: go 11.13, python 12.68, rust 18.60, typescript 8.25. NAME: go 28.91, python 29.74, rust 25.52, typescript 28.73. t2 NL: rust 15.32, others -inf. t2 NAME: equals t except rust 24.70.

| pop | arm | right tops kept | wrong/absent caught | caught delta vs A, 95% CI | kept delta vs A, 95% CI |
|---|---|---|---|---|---|
| NL | A floor | 75.3% (186/247) | 44.3% (265/598) | | |
| NL | B floor OR blend | 74.9% (185/247) | 50.2% (300/598) | +5.9 pts [+4.1, +7.8] | [-1.3, 0.0] |
| NL | B2 | 75.3% (186/247) | 46.0% (275/598) | +1.7 pts [+0.7, +2.8] | [0.0, 0.0] |
| NL | C blend alone | 93.5% (231/247) | 30.4% (182/598) | -13.9 pts [-17.8, -9.9] | [+13.3, +23.2] |
| NAME | A floor | 97.1% (329/339) | 66.1% (330/499) | | |
| NAME | B floor OR blend | 89.97% (305/339) | 87.6% (437/499) | +21.4 pts [+17.9, +25.1] | [-10.0, -4.5] |
| NAME | B2 | 90.9% (308/339) | 87.2% (435/499) | +21.0 pts [+17.6, +24.8] | [-8.8, -3.8] |
| NAME | C blend alone | 90.6% (307/339) | 87.6% (437/499) | +21.4 pts [+17.9, +25.1] | [-9.4, -3.7] |

Deltas are in points of caught/kept share; the kept CI is paired on the resampled right set.

## Controls

- (a) Shuffled labels: right/wrong permuted within language on FIT, thresholds refitted, evaluated on real held-out labels, 200 shuffles (seed 702). Caught delta B - A: NL mean +0.4 pts, p95 +1.2, max +1.5 (real +5.9, outside the shuffle range); B2 mean 0.0, max +1.5 (real +1.7). NAME: 0.0 in every shuffle (real +21.4): shuffled "right" rows contain wrong tops with low blend, the 10th-percentile cut falls to about 3 to 5 and adds nothing beyond the floor. The gain vanishes, so it depends on the labels.
- (b) Held-out queries where the arms differ: NL B vs A 36 (35 wrong/absent, 1 right), B2 vs A 10; NAME B vs A 131 (107 wrong/absent, 24 right), B2 vs A 126. Nonzero, so the test measured something.

## Verdict: null (do not ship)

Owner decision, 2026-11-11: "Закрыть как null (Recommended)". The shipped GM-697 floors stay as the only signal; no soft note, no ADR 0012 addendum.

Rule: ship only if the paired gain CI excludes 0 and right tops kept >= 90%.
- NL: the gain CI excludes 0 (B +5.9 pts), but the floor alone already keeps only 75.3% of right tops on the reranked page, so no OR rule can reach 90% (B 74.9%, B2 75.3%). Fails the kept condition. The variant that holds kept (B2) gains only +1.7 pts.
- NAME: gains are large (+21 pts), but kept drops from 97.1% to 89.97% (B: 305/339, just below 90) or 90.9% (B2). Each lost right top is a hard noMatch on a right answer; the floor meets the GM-697 3% misled budget (2.9%), B/B2 give about 9-10%, breaching the GM-699 follow-up condition (name-query misled at or below 3%). B2 meets the letter of 90% but not that budget.
- The comparison is not matched on kept: the blend buys catches by refusing more right tops. Blend alone (C) is at 90-93% kept and catches less than the floor on NL.
- Decision for the owner: close with "floors only" per the GM-699 follow-up, unless a name-path signal with a different kept budget is wanted.
