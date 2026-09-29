# GM-422: per-query gap between jina int8 and fp32

Slice S8 (measure) of GM-422. It describes, query by query, where
jina-v2-base-code int8 loses score or rank against fp32. It covers both the
OLD GM-398 query set and the NEW frozen confirmatory set. **It is
descriptive only.** It computes no gate and states no go/no-go: the gated
verdict at the frozen floors is slice S9's (`int8_confirm_report.py`), and
this slice did not run it. Nothing was re-embedded; only stored rankings were
read.

**In short:** int8 lowers cosine scores by a small common-mode amount, a
median of -0.004 over all positives (-0.001 on authored queries, -0.006 on
mechanical name queries). The loss hits the gold and its competitors alike,
so the margin between the gold and the best non-gold hit does not move
(median +0.0003, 874 lower / 909 higher, sign p = 0.42). The rank does not
move either. No rank or margin loss survives a multiple-test correction in any
stratum. The shift is larger on short, signature-only texts, but that is
still common-mode. Rank-1 losses and gains balance (23 vs 22), and every lost
rank 1 is back within int8's top 5. Crossings of the frozen floor follow the
**floor gaps** more than the score shift: int8's Go floor is 0.01 above
fp32's, and its TypeScript floor is 0.02 below.

## Method

- Script: [`eval/embedding/int8_gap_analysis.py`](../../eval/embedding/int8_gap_analysis.py).
  It reuses the loaders and indicators of `q5_floor_sensitivity.py` (`load_arm`,
  `load_queries`, `fit_floors`, `fa_indicator`), the frozen floors of
  `int8_confirm_power.py` (fp32 go/python/rust/ts 0.56/0.58/0.56/0.55, int8
  0.57/0.57/0.55/0.53), and `check_queries.overlap` for the lexical stratum.
  ```
  python3 eval/embedding/int8_gap_analysis.py \
    --old-runs <main>/eval/embedding/work/runs --old-eval-dir eval/embedding \
    --runs eval/embedding/confirm/work/runs --eval-dir eval/embedding/confirm \
    --snapshots <main>/eval/embedding/work \
    --tokenizer <main>/eval/embedding/work/models/jina-v2-base-code-int8/tokenizer.json \
    --json out.json
  ```
  Run: `real 27.74 user 25.30 sys 1.35` (CPU-bound), load average 3.97 before
  and 3.56 after, uptime 2:24.
- **Pairs.** Each query is paired across fp32 and int8. The **gold** is fp32's
  best-ranked expected node, and its kind and text length come from the
  snapshot sqlite. The embedded text is `text_to_embed`
  (doc comment + signature). Its length is counted in jina tokens with
  special tokens, as `token_shares` counts them; 1024 is the input cap.
- **Metrics** (int8 - fp32): the gold's score (its best-ranked expected node
  in each arm's top 100), the gold's rank (censored at 101), the top-1 score,
  and the **margin** (the gold score minus the best non-gold score).
- **Floor crossing.** A crossing is scored on the top-1 score, each arm at its
  own frozen floor for the top language. It is also scored with int8 at fp32's
  floor, which isolates the score effect. Held-out queries only: OLD by
  sha256 parity, NEW all authored.
- **Sets.** OLD has 400 authored positives and 100 absent queries. NEW has
  650 positives and 164 absent. The 879 mechanical positives are the same
  files in both runs. The script checks that both runs score them
  identically and then counts them once, as set `mech`. (A first run that
  counted them twice was discarded.)
- **Noise.** Each stratum gets a sign test (exact binomial) and a bootstrap
  95% CI of its median (2,000 resamples). Each factor gets a permutation test
  of the between-stratum spread of mean deltas (10,000 permutations,
  seed 4228). There were 376 tests, so the Bonferroni threshold is
  1.33e-4. Tables in the script output that are not reproduced here are in
  its stdout.

## Control

The S1 score-shift table of [gm-422-int8-q5.md](gm-422-int8-q5.md) is
reproduced on the OLD set before anything else is reported: int8 - fp32
top score over paired held-out right-first queries, with the floors refitted
(they equal the frozen floors). The script also checks that its own pairing
gives the same per-query shifts on that subset. On any mismatch it exits 2.

| language | n | median | min | max | mean abs |
|---|---|---|---|---|---|
| go | 16 | -0.0001 | -0.0146 | +0.0294 | 0.0097 |
| python | 20 | -0.0039 | -0.0269 | +0.0128 | 0.0086 |
| rust | 6 | +0.0005 | -0.0194 | +0.0026 | 0.0088 |
| typescript | 25 | -0.0035 | -0.0195 | +0.0292 | 0.0097 |

`CONTROL OK`: every value matches S1 to 4 dp.

## 1. Overall (positives)

| subset | n | gold score median | int8 lower/higher (sign p) | top-1 median | rank worse/better/same (sign p) | margin median | margin lower/higher (sign p) |
|---|---|---|---|---|---|---|---|
| OLD authored | 400 | -0.0018 | 206/144 (0.0011) | -0.0039 | 88/77/235 (0.44) | +0.0010 | 159/189 (0.12) |
| NEW authored | 650 | -0.0004 | 300/275 (0.32) | -0.0015 | 123/182/345 (0.0009) | +0.0010 | 269/303 (0.17) |
| mechanical | 879 | -0.0059 | 593/261 (<1e-29) | -0.0058 | 70/88/721 (0.18) | -0.0009 | 446/417 (0.34) |
| all | 1929 | -0.0037 | 1099/680 (<1e-22) | -0.0042 | 281/347/1301 (0.009) | +0.0003 | 874/909 (0.42) |

The score shift is real, but it is small and common-mode: the margin is flat.
Rank is unchanged in 67% of positives. Where it does change, int8 is more
often *better* on NEW (raw p = 0.0009, which does not survive the
correction).

## 2. Where the score shift concentrates

Gold score delta, all positives. Strata with n < 10 are omitted.

| factor | stratum | n | median [95% CI] | lower/higher | sign p | factor perm. p |
|---|---|---|---|---|---|---|
| set | mech | 863 | -0.0059 [-0.0070, -0.0049] | 593/261 | <1e-29 | 1e-4 |
| | old | 350 | -0.0018 [-0.0037, -0.0008] | 206/144 | 0.0011 | |
| | new | 575 | -0.0004 [-0.0021, +0.0012] | 300/275 | 0.32 | |
| query shape | name | 863 | -0.0059 | 593/261 | <1e-29 | 1e-4 |
| | phrase | 459 | -0.0022 [-0.0036, -0.0006] | 267/192 | 0.0005 | |
| | sentence | 466 | -0.0002 [-0.0017, +0.0012] | 239/227 | 0.61 | |
| embedded text | signature only | 683 | -0.0061 [-0.0073, -0.0050] | 477/197 | <1e-26 | 1e-4 |
| | doc | 1105 | -0.0019 [-0.0029, -0.0010] | 622/483 | <1e-4 | |
| tokens | <64 | 1352 | -0.0043 | 864/479 | <1e-25 | 3e-4 |
| | 64-127 | 277 | -0.0019 | 151/126 | 0.15 | |
| | 128-255 | 123 | +0.0008 | 60/63 | 0.86 | |
| | 256-511 | 26 | -0.0018 | 15/11 | 0.56 | |
| language | typescript | 510 | -0.0056 [-0.0074, -0.0043] | 351/159 | <1e-16 | 0.029 |
| | go | 432 | -0.0035 | 261/171 | <1e-4 | |
| | python | 375 | -0.0033 | 226/140 | <1e-4 | |
| | rust | 471 | -0.0019 | 261/210 | 0.021 | |
| corpus | excalidraw | 271 | -0.0071 | 190/81 | <1e-10 | 0.029 |
| | ripgrep | 238 | -0.0002 | 120/118 | 0.95 | |
| overlap | no overlap | 1247 | -0.0040 | 784/458 | <1e-19 | 0.48 |
| | overlap | 541 | -0.0028 | 315/222 | 1e-4 | |
| node kind | arrow_function | 175 | -0.0077 | 123/52 | <1e-7 | 0.50 |
| | function | 559 | -0.0037 | 351/208 | <1e-8 | |
| | method | 496 | -0.0041 | 295/201 | <1e-4 | |
| | struct | 151 | -0.0011 | 83/68 | 0.25 | |

On **authored (NL) queries only**, the same pattern holds:
- signature-only texts: -0.0040 [-0.0062, -0.0018], 181/110, p < 1e-4;
- doc texts: -0.0003, 325/309, p = 0.55;
- <64 tokens: -0.0018, 405/303, p = 1e-4;
- factor permutation p is 5e-4 (embedded text) and 8e-4 (tokens).

Language, node kind and overlap are not heterogeneous on NL queries
(p = 0.14, 0.84, 0.46). The shift is a property of **short embedded texts**,
i.e. signatures without a doc comment. The factors "mechanical", "name
shape", "TypeScript/excalidraw" and "arrow function" largely co-vary with it:
mechanical targets and excalidraw's arrow functions are mostly bare
signatures.

**The margin does not follow.** Margin delta by embedded text, all
positives: doc +0.0017 [+0.0002, +0.0034], signature only -0.0020
[-0.0035, +0.0001], perm. p = 1e-4. That is the only margin result that
survives the correction. On NL queries only it is p = 0.0075, which does not.
Rank delta is heterogeneous by nothing after correction (smallest perm.
p = 0.019, by tokens). The signature-only margin tilt is 0.002, a fifth of the
typical per-query quantization noise (mean abs 0.009).

## 3. Frozen-floor crossings (top-1 score, held-out)

"down" = fp32 clears its floor and int8 does not (on right-first queries,
a new false alarm); "up" = the reverse (on absent queries, a new
confident-wrong). "@fp32" = int8 held to fp32's floor.

| set | subset | n | down | up | sign p | down @fp32 | up @fp32 |
|---|---|---|---|---|---|---|---|
| OLD | authored positives, right-first in both | 67 | 2 | 1 | 1.0 | 1 | 0 |
| OLD | authored positives | 215 | 7 | 4 | 0.55 | 4 | 1 |
| OLD | absent | 46 | 2 | 0 | 0.50 | 2 | 0 |
| NEW | authored positives, right-first in both | 161 | 3 | 7 | 0.34 | 4 | 6 |
| NEW | authored positives | 650 | 14 | 22 | 0.24 | 18 | 13 |
| NEW | absent | 164 | 1 | 4 | 0.38 | 4 | 2 |

By language (authored positives, own floors → @fp32 floor):
- **Go**, where int8's floor is +0.01 higher: OLD down 7 / up 0 (raw p = 0.016)
  → 2/0; NEW 9/4 → 6/8.
- **TypeScript**, where int8's floor is -0.02 lower: OLD 0/4 → 0/1; NEW 1/7 → 4/2.
- **Python**, -0.01: NEW 1/8 → 2/2.

No crossing test is significant. The direction follows the sign of the floor
gap, and the gap is 2-5x the median score shift:

| set | language | n right-first in both | median top-1 shift | int8 - fp32 floor |
|---|---|---|---|---|
| OLD | go | 16 | -0.0003 | +0.01 |
| OLD | python | 20 | -0.0044 | -0.01 |
| OLD | rust | 6 | -0.0059 | -0.01 |
| OLD | typescript | 25 | -0.0035 | -0.02 |
| NEW | go | 48 | -0.0020 | +0.01 |
| NEW | python | 34 | -0.0047 | -0.01 |
| NEW | rust | 24 | -0.0017 | -0.01 |
| NEW | typescript | 55 | -0.0009 | -0.02 |

Few queries sit near a floor: 0-2 per language have an fp32 top score within
0.01 above it.

## 4. Length, truncation, recoverability

- Truncation is negligible. 3 of 1,929 gold texts reach the 1024-token cap
  (gm-018, gm-045, rg-c058); their gold delta is -0.012 median, but n = 3.
  Spearman(tokens, gold delta) = +0.11 (longer texts lose *less*), and
  Spearman(tokens, |gold delta|) = +0.03. Longer texts do not scatter more.
- Rank 1 on authored positives: fp32 1 → int8 > 1 on 23 queries, and the
  reverse on 22. Of the 23, 19 are at int8 rank 2, 20 within 3, and all 23
  within 5. Leaving the top 10: 15; entering it: 21.

## 5. Largest losses

The 15 largest gold-score losses (of 1,788 with a gold score in both arms),
then the largest rank losses:

| id | set | shape | kind | tokens | gold fp32 → int8 (rank) | Δ gold | Δ top-1 | text |
|---|---|---|---|---|---|---|---|---|
| gin-m149 | mech | name | function | 26 | 0.614 (4) → 0.536 (4) | -0.078 | -0.068 | NoMethod |
| gm-033 | old | phrase | function | 305 | 0.665 (1) → 0.619 (1) | -0.046 | -0.046 | stop spawned processes inheriting our standard streams |
| req-m084 | mech | name | method | 94 | 0.302 (44) → 0.258 (65) | -0.044 | +0.004 | `__nonzero__` |
| req-c141 | new | phrase | method | 29 | 0.562 (2) → 0.519 (5) | -0.044 | -0.009 | compute Authorization value for digest challenge |
| req-c080 | new | sentence | File | 10 | 0.479 (14) → 0.436 (25) | -0.044 | -0.018 | Module that gathers platform, Python implementation, TLS ... |
| ttm-c034 | new | sentence | function | 99 | 0.462 (2) → 0.419 (2) | -0.043 | +0.013 | Fetch only the owning codebase and version-batch ids ... |
| ttm-c022 | new | sentence | function | 8 | 0.368 (1) → 0.326 (1) | -0.042 | -0.042 | Read a lane's saved folded-or-expanded preference ... |
| ttm-m124 | mech | name | function | 11 | 0.617 (1) → 0.575 (1) | -0.042 | -0.042 | withLease |
| rg-c062 | new | sentence | struct | 598 | 0.749 (1) → 0.707 (1) | -0.042 | -0.042 | configurable builder for a recursive directory iterator ... |
| gin-m004 | mech | name | const | 14 | 0.819 (1) → 0.778 (1) | -0.041 | -0.041 | EnvGinMode |
| rg-m110 | mech | name | module | 4 | 0.778 (1) → 0.737 (1) | -0.041 | -0.041 | strip |
| gin-039 | old | phrase | method | 15 | 0.431 (8) → 0.391 (18) | -0.041 | -0.013 | find routing tree root for verb |
| gm-m045 | mech | name | const | 80 | 0.742 (1) → 0.701 (1) | -0.041 | -0.041 | REEXPORT_NATIVE_KIND |
| gm-m061 | mech | name | function | 58 | 0.614 (3) → 0.574 (5) | -0.040 | +0.003 | same_file_violation |
| req-c129 | new | phrase | variable | 15 | 0.498 (29) → 0.459 (41) | -0.039 | -0.004 | status codes treated as redirects |
| gm-c022 | new | sentence | function | 892 | 0.485 (41) → out of top 100 | - | -0.013 | fallback that offers declarations close in meaning ... |
| rg-m026 | mech | name | function | 112 | 0.373 (50) → out | - | -0.015 | preceding |
| gm-005 | old | phrase | enum | 5 | 0.434 (53) → out | - | +0.042 | default versus project-configured embedding choice |
| gm-m101 | mech | name | method | 61 | 0.488 (39) → 0.469 (87) | -0.019 | -0.033 | bare_use |

The pattern: most of the largest score losses keep their rank, because
Δ gold ≈ Δ top-1 (the whole list shifts down). They are mostly short texts
(10 of 15 under 64 tokens). The rank losses are all queries whose gold was
already deep in fp32's list (rank 18-75), where neighbours are packed within
thousandths of each other.

## Findings

1. **The only systematic loss is a common-mode score drop.** It is short
   texts that lose score (signature-only -0.006, doc -0.002; NL
   signature-only -0.004, p < 1e-4). The gold-vs-competitor margin and the
   rank do not follow it: no margin or rank stratum survives the correction,
   and rank-1 losses and gains balance, 23 vs 22.
2. **Floor crossings are driven by the frozen floor gaps, not by the score
   shift.** int8's floors differ from fp32's by -0.02 to +0.01, which is 2-5x
   the median shift. Go (int8 floor +0.01) crosses mostly downward (OLD 7/0,
   NEW 9/4). TypeScript and Python (int8 floor lower) cross mostly upward
   (NEW 1/7, 1/8). At a shared floor these even out (Go NEW 6/8, TS 4/2).
   None of the crossing tests is significant.
3. **No quantization-specific tail.** The largest per-query losses are about
   0.04-0.08, about 4-8x the mean abs shift. They come from all corpora,
   shapes and kinds; truncation reaches 3 golds only; and every lost rank 1
   is within int8's top 5.

## Hypotheses: what would close the gap

| # | hypothesis | expected effect | data that tests it |
|---|---|---|---|
| H1 | **Per-model floor calibration as fp32 floor + offset.** Set int8's floor to fp32's floor plus int8's median shift (≈ -0.004; per language -0.001 to -0.006) instead of refitting a 3rd-percentile floor on about 100 queries (a 0.01-granular fit whose noise, ±0.01, exceeds the shift). | Removes the Go-down / TS-up asymmetry of section 3; FA and confident-wrong become equal between arms up to per-query noise. | OLD: derive the offset from the fit half only, then compare held-out FA and confident-wrong at the offset floors. NEW: the same, after S9's verdict, as a secondary analysis, never a re-gate. Also an R-vs-R check of the offset's false-fail rate (S2's self-test). |
| H2 | **Rerank the top k (GM-443).** | Closes little of the *int8-specific* gap, because there is almost none at rank. It would recover the 23 lost rank-1s (all within int8's top 5) only if the reranker also fixes them under fp32, so it acts on the shared gap and would lift both arms. | Rerank both arms' top 5/10 on the 23 + 22 discordant rank-1 queries and on all authored positives. Compare int8+rerank with fp32+rerank on MRR and Q5; if the reranker's score replaces the cosine for the floor, refit the floors on it. |
| H3 | **Input-length changes (GM-423).** | Not supported for truncation: 3 of 1,929 golds are at the cap, and |Δ| does not grow with length. The data points the other way: the *shortest* texts shift most. Longer embedded texts, such as a doc summary or body prefix added to bare signatures, might damp the common-mode shift, but the margin is flat anyway. | Re-embed OLD with the GM-423 text variant under both arms; compare the per-stratum gold delta and margin for "signature only" vs "doc". Success means the signature-only shift shrinks toward doc's -0.002 *and* the margin does not worsen. |
| H4 | **Score recentring for short texts** (new): add a per-model, length-bucketed offset to int8 cosine scores before comparing with the floor. | Same aim as H1, but targets the short-text excess; it matters only if floors are shared across models. | Section 2's per-bucket medians on OLD's fit half; check that NEW's held-out crossings at the recentred scores match fp32's. |
| H5 | **Larger or pooled calibration sample** (new): fit int8's floors on OLD fit half + mechanical positives, or across languages. | Reduces the ±0.01 fit noise that drives finding 2. | Bootstrap the floor fit (S2's `resampled_floors`) with and without the extra rows; compare floor SD with the 0.004 shift. |

The data do not suggest any fix aimed at node kind, query shape (beyond the
name/short-text link) or lexical overlap: once the common-mode shift is
removed, none of them concentrates a loss.
