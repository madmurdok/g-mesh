# GM-452/S16: fine-tuned embedder J + L6 reranker vs today's stack (D3 go/no-go)

Measure slice GM-452/S15, 2026-11-21, branch `research/GM-452-fine-tune-reconcile`.
Rule: the go rule D3, pre-registered before S4 (`docs/design/GM-452-reconcile.md`
section 10; owner decisions section 11: D1 "Домерить", D2 "Переподобрать при
равных ложных тревогах", D3 "Принять как предложено").

## Verdict: NO-GO (3 of 4 D3 clauses fail)

J (GM-730, jina-v2-base-code full fine-tune, int8) with the shipped L6 reranker
and the F4 learned verdict does not meet D3 against today's stack. Today's
embedder stays.

| D3 clause | number (J + L6 minus today, pts, paired 95% CI) | result |
|---|---|---|
| NL CW positives, pooled upper bound < 0 | +1.5 [-2.4, +5.3]; upper +5.3 | FAIL |
| NL CW positives, point <= 0 in every language | go +1.6, python +4.5, rust -3.7, typescript +3.3 | FAIL (3 of 4 positive) |
| NL CW absent, pooled upper bound <= +5 | +29.9 [+21.3, +38.2]; upper +38.2 | FAIL |
| Name misled <= 3% per language at J's floors (embedder, `shipped_floor_rates --after`) | go 2.4 (2/82), python 0.9 (1/110), rust 1.4 (2/141), typescript 0.8 (2/244) | PASS |

CW = confident wrong (a wrong top shown without a warning; absent queries
count as wrong). Misled = soft warning on a right top.

## Method

- **Today** = base cosine (`jina-v2-base-code-int8-structured`), shipped floors
  (go .57 / python .59 / rust .57 / typescript .53), L6 quint8 reranker over the
  top 30, F4 learned verdict, beta 40.
- **J + L6** = J cosine (`gm730-jina-full-int8-structured`), J's D6 floors
  (refit at equal ~5% false alarm on v2 FIT: go .49 / python .52 / rust .53 /
  typescript .48, from `report_jina.json`, reproduced by `refit_J.md`), same L6
  and F4, beta tuned on FIT: 160 (FIT r@10 0.768, MRR 0.521, 639 FIT queries).
- Held-out only. Paired bootstrap by query id, stratified by language, seed 398,
  10,000 resamples, percentile 95% CI. NaN resamples: 0 in stack and embedder modes.

## Results (held-out)

### NL, stack: today vs J + L6

| lang | metric | today | J + L6 | delta pts [95% CI] |
|---|---|---|---|---|
| go | CW positives | 41.8% (77/184) | 43.5% (80/184) | +1.6 [-6.0, +9.2] |
| go | CW absent | 9.8% (4/41) | 61.0% (25/41) | +51.2 [+35.9, +66.7] |
| go | misled | 36.6% (26/71) | 5.3% (5/95) | -31.4 [-42.7, -20.5] |
| python | CW positives | 38.7% (60/155) | 43.2% (67/155) | +4.5 [-3.7, +12.5] |
| python | CW absent | 22.5% (9/40) | 37.5% (15/40) | +15.0 [-2.0, +31.4] |
| python | misled | 23.9% (11/46) | 11.1% (7/63) | -12.8 [-26.6, +1.0] |
| rust | CW positives | 59.0% (95/161) | 55.3% (89/161) | -3.7 [-11.8, +4.3] |
| rust | CW absent | 37.8% (17/45) | 55.6% (25/45) | +17.8 [+0.0, +34.3] |
| rust | misled | 19.1% (9/47) | 5.3% (3/57) | -13.9 [-26.0, -2.3] |
| typescript | CW positives | 35.4% (64/181) | 38.7% (70/181) | +3.3 [-3.8, +10.4] |
| typescript | CW absent | 18.4% (7/38) | 55.3% (21/38) | +36.8 [+20.0, +54.3] |
| typescript | misled | 18.1% (15/83) | 6.4% (6/94) | -11.7 [-20.9, -2.8] |
| **all** | CW positives | 43.5% (296/681) | 44.9% (306/681) | +1.5 [-2.4, +5.3] |
| **all** | CW absent | 22.6% (37/164) | 52.4% (86/164) | +29.9 [+21.3, +38.2] |
| **all** | misled | 24.7% (61/247) | 6.8% (21/309) | -17.9 [-23.5, -12.5] |

### Name, stack: today vs J + L6 (pooled; per language in `tables.md`)

CW positives 18.6% (78/420) -> 21.7% (91/420), +3.1 [-0.2, +6.5]; CW absent
21.8% (91/418) -> 26.8% (112/418), +5.0 [+1.2, +9.1]; misled 2.9% (10/339) ->
2.5% (8/315), -0.4 [-2.9, +2.1]. Per language misled in the stack: go 0.0%,
python 5.0% (3/60), rust 5.1% (4/79), typescript 0.8% (see caveat a).

## Why: where the absent regression comes from

- **Not from J itself at equal false alarm.** Embedder only, both models at
  their own D6 floors (paired, NL pooled): CW absent +0.6 [-8.3, +9.5]
  (base 51.8% vs J 52.4%, design section 3.1), CW positives -15.6
  [-19.2, -12.0], misled +1.0 [-2.6, +4.6]. J ranks better and is neither
  better nor worse at saying "nothing here".
- **From the operating point.** Today's absent rate (22.6%, 37/164) is bought
  by the shipped floors, which sit high for NL: the base embedder at shipped
  floors is also 22.6% absent, at 21.7% false alarm (design 3.1), and today's
  stack shows misled 24.7%. J at D6 is held to ~5% false alarm (stack misled
  6.8%). D3 compares the two stacks as they would ship, so J's stack trades
  about 18 pts fewer false alarms for about 30 pts more confident-wrong absent.
- **L6 + F4 recovers none of it.** J + L6 absent is 86/164, exactly J's
  embedder-only count at D6 (86/164); today's stack absent equals the base
  embedder at shipped floors (37/164). The reranker and verdict move positives
  (today: 49.6% embedder -> 43.5% stack; J: 48.2% -> 44.9%), not absent.
- **Positives:** J's embedder-level gain (-15.6 pts at D6) mostly disappears
  in the stack (+1.5 vs today), because today's stack already gets part of it
  from L6 and from its high floors.

## Caveats

- **(a) gm461 stop rule on the J stack.** `gm461_learned_verdict.py` for J
  exited 2 with `STOP: NAME full: coefficient of cos1 + cos_t = -0.2035 <= 0`
  (noCE NAME -0.4162) after the NL outputs and per-query rows were written.
  NL results are used; the name half of the J stack is not a validated F4 fit.
  In that stack name misled is python 5.0%, rust 5.1%; the D3 name clause is
  measured at the embedder level (passes), as pre-registered.
- **(b) Scoring path.** 46,472 J-only pairs (3,427 queries) were scored with
  Python onnxruntime (rust-like session options), not the Rust crate. On 300
  pairs shared with the Rust cache: mean |d| 0.0052, max 0.198.
- **(c) J reranker scores lost.** The cache copy holding them was overwritten
  after the run; reproducing the J stack needs re-scoring (~92 min CPU: step
  s1 ran 15:16-16:48).
- **(d) Bootstrap.** Floors are fixed per arm, not refit per resample, so
  floor-fit variance is excluded and the CIs are somewhat narrow. Unpaired ids
  are dropped with a note; here every held-out id paired (n = 845 NL, 838 name).
- **(e) Machine load.** Load average 75-200 during the run (e.g. 144 / 126 /
  152 at s1; 197 at s4 end); timings are not performance data.

## Controls

- Base stack (gm461, no flags, exit 0) reproduces GM-461 F4: NL CW positives
  43.5% (296/681), absent 22.6% (37/164); control 1 reproduce ok, 2a/2b ok. PASS.
- J embedder (gm697 `--report`, refit_J): C1/C2 at D6 floors ok; bootstrap rows
  match design section 3 (J 328/681, 86/164 in `--after`; CW pos -15.6 pooled). PASS.

## What it means for shipping J

J alone improves ranking (GM-730: NL MRR 0.380 -> 0.501, r@10 0.608 -> 0.733)
and, at equal false alarm, cuts embedder-level CW on positives. It does not
cut CW on absent queries, and swapped into today's stack at its fair floors it
fails D3: no gain on positives, +29.9 pts CW absent, paid for by far fewer
false alarms. Options:

1. **Keep today's embedder (Recommended).** The pre-registered rule fails on
   3 of 4 clauses with a CI on absent nowhere near the +5 bound.
   Benefit: no regression on absent, no floor re-fit. Risk: leaves J's ranking
   gain (MRR +0.12) unused.
2. **Ship J for ranking only, with a separate verdict.** Use J to order
   results, keep the base cosine (or another signal) for the "nothing here"
   decision. Benefit: keeps the MRR gain. Risk: two embedders (index size,
   latency, download), a new verdict design that D3 did not measure; the F4
   name fit already failed its sign check with J's cosines (caveat a).
3. **Retrain with absent-aware negatives** (GM-456 pairs plus queries whose
   answer is not in the corpus, pushed below the floor). Benefit: targets the
   one thing J did not learn. Risk: GPU cost and a new GM-730 round; no
   evidence yet that it lowers absent CW without losing the positive gain.

A fourth reading (judge J at today's high-false-alarm operating point) is not
what D3 or D2 decided; the embedder-only data say it would trade false alarms
back, not remove the absent problem.

## Reproduction

Commits: `efbdf7ff`, `70e0b2ed`, `97a54463`; tests `5e3a9c2a`, `0452196c`,
`9fb362e9`, `51afea50`. From `eval/embedding` of the worktree, with
`D=../g-mesh-eval-data/v2/work` (absolute in the scripts), `R=$D/runs`,
`REP=../g-mesh-data/gm730/measure/report_jina.json`,
`JA=gm730-jina-full-int8-structured`, `PYTHONHASHSEED=0`:

```
cp -R $D/rerank_cache_gm700_rust $O/cache
python3 $O/score_j.py $O/cache            # ~92 min CPU, J-only pairs (caveat b, c)
python3 gm697_floor_refit.py --run $R/$JA --eval-dir v2 --report $REP --report-arm $JA --json $O/refit_J.json --table $O/refit_J.md
python3 gm697_floor_refit.py --run $R/jina-v2-base-code-int8-structured --eval-dir v2 --report $REP --report-arm jina-v2-base-code-int8-structured --json $O/refit_B.json --table $O/refit_B.md
python3 gm461_learned_verdict.py --work $D --cache-dir $O/cache --out $O/v_base.json --md $O/v_base.md --per-query-out $O/pq_base.jsonl
python3 gm461_learned_verdict.py --work $D --cache-dir $O/cache --st-runs $R/$JA --floors-json $REP --floors-arm $JA --tune-beta-on-fit --out $O/v_J.json --md $O/v_J.md --per-query-out $O/pq_J.jsonl   # exits 2 (caveat a)
python3 gm452_paired_bootstrap.py --base-stack $O/pq_base.jsonl --stack $O/pq_J.jsonl --base-arm F4 --stack-arm F4 --seed 398 --resamples 10000 --out $O/boot_stack.json
python3 gm452_paired_bootstrap.py --base-run $R/jina-v2-base-code-int8-structured --run $R/$JA --eval-dir v2 --floors d6 --seed 398 --resamples 10000 --out $O/boot_emb.json
python3 shipped_floor_rates.py --run $R/$JA --eval-dir v2 --after --custom go=0.49,python=0.52,rust=0.53,typescript=0.48
```

Scripts and raw outputs: `/private/tmp/claude-502/gm452s15/` (`run.sh`,
`run45.sh`, `score_j.py`, `tables.md`, `s*.out`, `boot_*.json`, `pq_*.jsonl`);
script changes and their byte-identical check:
`/private/tmp/claude-502/gm452s2/summary.md`. Both are scratch and may be gone.
Wall times (loaded machine): s3 base 183 s real / 46 user, s3 J 126 s / 35,
each bootstrap ~65 s / 18, s5 5.6 s / 1.4.
