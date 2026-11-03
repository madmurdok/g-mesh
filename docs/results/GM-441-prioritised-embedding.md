# GM-441 S24: prioritised embedding (int8 + structured, rerank installed)

Branch `feat/GM-441-prioritised-embedding`, tip `e944dcb` (code), release 4.24.0. Spec: `docs/design/GM-441-prioritised-embedding.md`, "Measurement plan". Owner decisions: arms A0 `id`, A1 `tier`, informational C `oracle`, control `tier-reversed`; no A2 (D), no A3; rerank at 100% only; g-mesh pass time 353.8 s (GM-453, in the code as an override). Raw data: `docs/results/GM-441-data/` (`summary.json`, `summary.txt`, `confirm-A0.json`, `confirm-A1.json`, parity logs). Scripts: `eval/embedding/gm441_measure.sh` (one fail-fast background script), `gm441_summary.py`, `gm441_confirm.py`, `gm441_parity_diag.py`.

## Verdict

- **Critical path: met.** On the production stack A1 puts every answer node on a partial page much earlier than A0. g-mesh: **183 s (55% of the pass) vs 333 s (100%)**. Pooled over the six corpora: **67.9% vs 98.9%** of pass seconds (macro mean 73.6% vs 98.9%). Recall@10 at 90% of the reachable value: g-mesh 35% vs 90% of the pass, pooled 51.2% vs 88.3%. Area under recall-vs-time (macro) 0.427 vs 0.350; paired bootstrap A1-A0 recall@10 is positive at 25/50/75% with CIs excluding 0 (below).
- **Staleness window: unchanged by P, as designed.** Pages stay partial and unreranked until Ready (the whole pass, 333 s predicted for g-mesh; 294 s measured in the daemon). What changes is what those pages hold: with A1, from 183 s on (g-mesh) they hold all the answers, and the remaining 150 s (45%) is the test tier. Tier ends (g-mesh, predicted): public 60.9 s (18.3%), private 183.8 s (55.2%), test 333.1 s.
- **Rerank unchanged at 100%: met.** The reranked rankings are bit-identical across all four arms on all six corpora, and recall@10/MRR before and after rerank are identical per arm (tables below).
- **Daemon confirm (3 runs per arm, interleaved): A1 is slower, modestly.** Embedding phase median 250.2 s (A1, range 234.2-278.6) vs 233.8 s (A0, 213.6-468.7), +7%; without A0's first run, an outlier at load 100+, A0 is 224 s (213.6-233.8). Adjacent pairs: A1 faster in the first (278.6 vs 468.7, outlier), +16.5 s (+7.0%) and +20.6 s (+9.6%) in the other two. Daemon CPU is about equal (1834 vs 1826 s median), so the cost is mostly real time, not work. The public tier is still about 28% of the pass against 18.3% predicted. Details below; for an owner decision.
- **Control: t90 failed; area under the recall curve chosen afterwards.** The spec's t90 control did **not** separate the arms: `tier-reversed` reached t90 *earlier* than A0 pooled (84.1% vs 88.3% of the pass; later on only 3 of 6 corpora). The owner then chose area under the recall curve as the arm-separating control, replacing t90. **That choice was made after seeing the data**, so it is not an independent pre-registered check. On it, reversed is worse than A0 (pooled 0.265 vs 0.350; worse on 4 of 6 corpora; ripgrep and task-tracker-mcp are the exceptions) and A0 is worse than A1 (order reversed 0.265 < id 0.350 < tier). The t90 result stays here as failed.

## Setup and parity

Binary: release build of the branch tip (A1) and of `b4f96e1` (4.24.0 version bump, before the tier commit) as the A0 daemon. Replay: `debug-embed-eval schedule --filter noise` on `runs-gm465/jina-v2-base-code-int8-structured`, curve `runs-gm466/rel.curve.json`, `--eval-dir` the main checkout's `eval/embedding`. Rerank: `gm441_rerank.py` (F4: top 30, `logit + 80 * cosine`) over the GM-464 cache; 1,191 pairs the cache lacked (g-mesh 634, ripgrep 417, py-requests 133, gin 5, excalidraw 2) were scored with the same cross-encoder and added to the cache.

**Parity.** `debug-embed-eval parity` on task-tracker-mcp against a production 4.24 int8 index (fresh `reindex`, isolated `G_MESH_HOME`) reports FAILED, two findings: production has 149 vectors vs 157 harness candidates, and `ttm-003`'s top 10 differs (max score diff 7.7e-4). Diagnosis (`parity-diag.txt`): all 149 production vectors equal the stored run's vectors (min cosine 1.000000000, max |delta| 0), no production id is missing from the harness, and the 8 harness-only nodes are the `setup()` stubs in `tests/*.test.ts` that production's noise mask drops. The command compares an unmasked run with a filtered production index, so it cannot pass since GM-437's filter. **The owner accepted the deviation** (the 149 shared vectors are bit-identical; the 8 extra nodes are test `setup()` stubs the noise mask drops), overriding the "if parity fails, stop" rule.

Controls run in the script: with the mask off, arm `id`'s 100% rankings equal `runs-gm465/.../rankings.jsonl` on all six corpora (same order, max score diff 0). `test_gm441_rerank.py` (its logit-corruption and F4 controls) passed.

## Per arm and corpus (replay; seconds of the pass, % of the pass)

Time to full gold = first checkpoint where every answer node of a scored query is embedded. Pass = predicted seconds rescaled to the measured run (g-mesh 353.8 s override, minus the noise mask: 333.1 s).

| corpus | pass s | A0 full gold | A1 full gold | C oracle | tier-reversed |
|---|---|---|---|---|---|
| excalidraw | 94.5 | 93.9 (99.3%) | 85.1 (90.0%) | 4.7 (5.0%) | 94.5 (100%) |
| g-mesh | 333.1 | 332.9 (100%) | 183.2 (55.0%) | 16.7 (5.0%) | 333.1 (100%) |
| gin | 36.0 | 36.0 (99.9%) | 21.3 (59.1%) | 5.4 (15.0%) | 36.0 (100%) |
| py-requests | 33.3 | 33.3 (100%) | 15.7 (47.1%) | 5.0 (15.0%) | 33.3 (100%) |
| ripgrep | 119.6 | 113.6 (95.0%) | 111.5 (93.3%) | 6.0 (5.0%) | 113.6 (95.1%) |
| task-tracker-mcp | 6.9 | 6.8 (99.1%) | 6.7 (97.1%) | 2.5 (35.7%) | 6.9 (100%) |
| **pooled (sum of seconds)** | 623 | 98.9% | **67.9%** | 6.5% | 99.1% |

t90 / t100 (partial-page recall@10 reaching 90% / 100% of the no-rerank complete value), % of the pass:

| corpus | A0 t90 | A1 t90 | A0 t100 | A1 t100 | rev t90 |
|---|---|---|---|---|---|
| excalidraw | 90.0 | 65.1 | 95.0 | 90.0 | 85.0 |
| g-mesh | 90.0 | 35.0 | 100 | 35.0 | 85.0 |
| gin | 90.0 | 55.0 | 99.9 | 59.1 | 90.0 |
| py-requests | 95.2 | 40.2 | 100 | 45.2 | 100 |
| ripgrep | 80.0 | 85.0 | 80.0 | 85.0 | 75.0 |
| task-tracker-mcp | 80.1 | 90.2 | 99.1 | 97.1 | 80.6 |
| pooled (sec share) | 88.3 | **51.2** | 95.4 | 55.6 | 84.1 |

On ripgrep and task-tracker-mcp A1 does not help (answers are spread across public and private nodes, tests are few): their t90 is later than A0's. Share of answer nodes embedded at 20/60/120 s, g-mesh: A0 8.9/19.6/48.2%, A1 16.1/30.4/75.0%.

Paired bootstrap, recall@10 of A1 minus A0 at the checkpoint (macro over corpora, seed 398, 10,000 resamples): 25% **+0.113** [+0.068, +0.158]; 50% **+0.135** [+0.078, +0.192]; 75% **+0.058** [+0.020, +0.097].

## Recall and MRR at 100%, before and after rerank

Identical for all four arms (and the reranked rankings files are byte-identical across arms: `rerankedRankingsIdenticalAcrossArms` true for all six corpora).

| corpus | recall@10 | after rerank | MRR | after rerank |
|---|---|---|---|---|
| excalidraw | 0.620 | 0.620 | 0.4222 | 0.4863 |
| g-mesh | 0.380 | 0.420 | 0.2405 | 0.2994 |
| gin | 0.760 | 0.810 | 0.4459 | 0.5034 |
| py-requests | 0.720 | 0.740 | 0.5033 | 0.5269 |
| ripgrep | 0.360 | 0.440 | 0.2161 | 0.2414 |
| task-tracker-mcp | 0.900 | 0.920 | 0.7136 | 0.7703 |
| macro mean | 0.6233 | 0.6583 | 0.4236 | 0.4713 |

## Daemon confirm (g-mesh 3.15.0 checkout copy, 3 cold runs per arm)

Same setup as S24's single runs (isolated `G_MESH_HOME`, `G_MESH_EMBEDDING_CACHE=off`, `G_MESH_SEARCH_EMBEDDING_WAIT_MS=200`, cross-encoder from `work/models/ms-marco-MiniLM-L6-v2`, fresh corpus copy per run). Script `eval/embedding/gm441_confirm3.sh`, order A0 A1 A0 A1 A0 A1, each run gated at load1 <= 4 (3.7-4.0 at start), daemon killed by recorded PID. A0 is the 4.24.0 bump commit `b4f96e1` (no tiers), A1 the branch tip `f2c59b1`. Raw data: `docs/results/GM-441-data/confirm3/`. The driver's `/usr/bin/time -p` user/sys (0.3-0.5 s) does not include the daemon, so daemon CPU comes from `ps cputime`.

| run | arm | embedding phase s | tiers public / private / test s | daemon CPU s | real s (driver) | load1 before / after |
|---|---|---|---|---|---|---|
| 1 | A0 | 468.7 | none | 2539 | 858.6 | 4.0 / 103.8 |
| 2 | A1 | 278.6 | 81.6 / 119.5 / 76.9 | 2068 | 670.7 | 4.0 / 21.9 |
| 3 | A0 | 233.8 | none | 1825 | 574.9 | 4.0 / 17.8 |
| 4 | A1 | 250.2 | 70.9 / 114.5 / 64.3 | 1834 | 573.7 | 3.9 / 11.2 |
| 5 | A0 | 213.6 | none | 1675 | 512.7 | 3.7 / 8.1 |
| 6 | A1 | 234.2 | 62.2 / 98.7 / 72.2 | 1784 | 548.8 | 3.8 / 12.3 |

| per arm, median [min-max] | A0 | A1 |
|---|---|---|
| embedding phase s | 233.8 [213.6-468.7] | 250.2 [234.2-278.6] |
| daemon CPU s | 1826 [1675-2539] | 1834 [1784-2068] |
| tier s (public / private / test) | none | 70.9 / 114.5 / 72.2 |

- **Verdict: A1 is slower than A0, by about 7-10% in the clean pairs, but the evidence is thin.** The raw medians (250.2 vs 233.8 s, +7%) sit inside A0's range because run 1 hit a load spike (load1 104 at its end); without it A0 is 213.6-233.8 and A1 234.2-278.6, ranges that touch only at 234. Paired adjacent runs 3/4 and 5/6 give +7.0% and +9.6%. Run 2 is not comparable to run 1. Load fell over the session (all later runs end at load 8-18 against 25 in S24), and A1 always ran after its A0 partner on a calmer machine, which favors A1, not A0. S24's +14.6% was an overestimate; the cost is single-digit percent. Only three runs per arm: this is a consistent sign, not a tight estimate.
- **CPU vs waiting.** Daemon CPU is the same in pair 3/4 (1825 vs 1834 s) and +6.5% in pair 5/6 (1675 vs 1784 s). A1's extra time is therefore mostly waiting or scheduling, not additional compute, with a possible small CPU part.
- **Likely source (not profiled).** Three keyset scans instead of one, plus the `handed` set bookkeeping between tiers, or tier-boundary batching leaving the embedder idle at each boundary. Unconfirmed.
- **Tier shares.** Median seconds per tier 70.9 / 114.5 / 72.2 (sum 257.6, from per-tier medians): public about 28% of the pass, public plus private about 72%; predicted 18.3% and 55.2%. Public texts are slower per node than test texts, so the replay understates the public tier. Order and direction hold.
- **Partial before Ready, reranked after** (S24 run, unchanged): `search_code` at the public and private boundaries returned partial pages with monotone scores (unreranked); after Ready every page was complete and reranked.
- **Single-scan control not available.** The A0 binary has no tier log, so the interleaved-tiers check cannot be shown by log lines.

## Machine state

`uptime` and `/usr/bin/time -p` for every timed step are in `docs/results/GM-441-data/summary.txt`. Replay and rerank steps ran at load1 4-8 (schedule 18.2 s real / 13.1 user; rerank 45.2 s real / 123 user for the 1,191 new pairs). The daemon runs are the only wall-clock measurements. S24's single runs were gated at load1 <= 8 (6.3 and 7.6) and ended at 13.7 and 24.6. The S25 runs were gated at load1 <= 4 and ended at 8-104 (table above); other agents' work loaded the machine during every run.

## Things for the owner

1. Parity: accepted by the owner (noise mask, 8 test-stub nodes; shared vectors bit-identical).
2. Control: t90 failed; area under the recall curve was chosen after seeing the data and passes (0.265 < 0.350 < tier).
3. Decision needed: the tiered backfill costs about 7-10% more wall time in the embedding phase on this machine (3 runs per arm), with about equal CPU. Accept as the price of the earlier answers (g-mesh answers on partial pages from 183 s instead of 333 s), or investigate the tier scans.
