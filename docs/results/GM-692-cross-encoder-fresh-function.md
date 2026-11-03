# GM-692: fresh-function search_code ranking with the cross-encoder

Status: **measured. 0 misses in 27 edits (0/19 on GM-690's edit set); the rerank closes the miss.**

## Setup

- Harness: `eval/gm692/` (GM-690 driver + GM-435 `measure.py`), run once with `run.sh full` at commit ab8da00.
  Project = detached worktree of db1074a (GM-690's), binary = release build of this branch (4.24.0 candidate).
- Cross-encoder: cross-encoder/ms-marco-MiniLM-L6-v2, fetched with `g-mesh model fetch`; sha256 of `model.onnx`
  5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a, `tokenizer.json`
  d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66; `model status` says search_code can rerank.
- Plan: 19 edits in session 1 (GM-690's order, arms A/B), then 4 restarts of (first-query rep + 1 rep): 27 edits,
  5 first-query reps. Each edit waits for load1 <= 32 first (ab8da00).
- Rerank was active in every rep (27/27): the first page is not in cosine order and the next cursor starts with
  `rerank:`; the daemon logs have 0 "rerank model ... not available" lines.
- No "watcher event missed" status and no left-over processes in any session.

## Per-edit results

"cosine score" is the edited function's cosine; "cutoff" is the 10th cosine in the window (without rerank a
function below the cutoff is not in the top 10). rank = position in the reranked top 10; rank@60 was 1 as well.
load1 is the uptime load at that rep.

| sess | arm | rep | first query | rank | top-10 hit | cosine score | cutoff (10th cosine) | load1 | real s | daemon cpu s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | B | 0 | Y | 1 | yes | 0.5010 | 0.4899 | 28.14 | 6.2 | 8.53 |
| 1 | B | 1 |  | 1 | yes | 0.4971 | 0.4899 | 26.08 | 5.0 | 7.38 |
| 1 | B | 2 |  | 1 | yes | 0.5002 | 0.4899 | 22.84 | 5.4 | 7.73 |
| 1 | B | 3 |  | 1 | yes | 0.5059 | 0.4899 | 24.88 | 5.2 | 7.63 |
| 1 | B | 4 |  | 1 | yes | 0.5027 | 0.4899 | 24.09 | 5.4 | 7.31 |
| 1 | B | 5 |  | 1 | yes | 0.5054 | 0.4899 | 21.16 | 4.9 | 7.0 |
| 1 | B | 6 |  | 1 | yes | 0.5019 | 0.4899 | 18.59 | 5.8 | 8.36 |
| 1 | A | 7 |  | 1 | yes | 0.4941 | 0.4899 | 24.09 | 7.2 | 9.74 |
| 1 | B | 7 |  | 1 | yes | 0.4886 | 0.4886 | 21.22 | 5.0 | 7.21 |
| 1 | A | 8 |  | 1 | yes | 0.4989 | 0.4899 | 18.79 | 6.4 | 7.64 |
| 1 | B | 8 |  | 1 | yes | 0.4836 | 0.4837 | 16.73 | 5.5 | 8.12 |
| 1 | A | 9 |  | 1 | yes | 0.4876 | 0.4877 | 16.18 | 6.7 | 8.57 |
| 1 | B | 9 |  | 1 | yes | 0.5076 | 0.4899 | 14.54 | 5.0 | 7.12 |
| 1 | A | 10 |  | 1 | yes | 0.5031 | 0.4899 | 13.59 | 6.7 | 7.55 |
| 1 | B | 10 |  | 1 | yes | 0.5003 | 0.4899 | 12.51 | 4.9 | 7.24 |
| 1 | A | 11 |  | 1 | yes | 0.4923 | 0.4899 | 11.2 | 7.1 | 8.94 |
| 1 | B | 11 |  | 1 | yes | 0.4911 | 0.4899 | 10.08 | 5.5 | 8.3 |
| 1 | A | 12 |  | 1 | yes | 0.5130 | 0.4899 | 19.86 | 7.1 | 9.32 |
| 1 | B | 12 |  | 1 | yes | 0.5119 | 0.4899 | 17.72 | 5.7 | 9.04 |
| 2 | B | 13 | Y | 1 | yes | 0.4701 | 0.4811 | 8.67 | 5.3 | 7.85 |
| 2 | B | 14 |  | 1 | yes | 0.4903 | 0.4899 | 8.31 | 4.5 | 6.16 |
| 3 | B | 15 | Y | 1 | yes | 0.5105 | 0.4899 | 9.24 | 6.3 | 9.19 |
| 3 | B | 16 |  | 1 | yes | 0.4988 | 0.4899 | 9.63 | 5.8 | 8.74 |
| 4 | B | 17 | Y | 1 | yes | 0.4922 | 0.4899 | 26.12 | 8.2 | 11.83 |
| 4 | B | 18 |  | 1 | yes | 0.5183 | 0.4899 | 29.91 | 6.5 | 9.61 |
| 5 | B | 19 | Y | 1 | yes | 0.4827 | 0.4828 | 26.06 | 5.4 | 7.53 |
| 5 | B | 20 |  | 1 | yes | 0.4772 | 0.4811 | 23.36 | 4.9 | 7.07 |

## Miss rate

| | misses |
|---|---|
| all edits | 0/27 |
| GM-690's 19 edits (session 1) | **0/19** (GM-690 without rerank: 4/19) |
| first-query-after-start reps | 0/5 |
| other reps | 0/22 |

Every edited function ranked 1. Reps 13 (cosine 0.4701 vs cutoff 0.4811) and 20 (0.4772 vs 0.4811) had a cosine
below the 10th-place cosine, so cosine ordering alone would have missed them (reps 8, 9, 19 sit right at the cutoff);
the rerank still put them first. Caveat: the 4/19 baseline was not re-run in this session, the comparison is
against GM-690's recorded numbers.

## Machine state

Run 17:03-17:15 (`real 722.93 user 210.36 sys 59.30`), load averages 19.02 at start, 21.45 at end; per-rep load1
8-30 (peak 29.9, under the gate of 32). Other agents were running (GM-441), so absolute timings (first search
0.5-1.9 s, rep real 4.5-8.2 s) are indicative only.

## S2's load finding

The earlier failed full runs (load1 ~700) were machine load, not a g-mesh bug: S2 showed a restarted daemon on an
existing index sees every edit within 0.3 s when load1 < 60. With the load gate and its own status for a missed
watcher event, all five sessions (including four restarts) saw all edits.

## Verdict

**Yes, the rerank closes the miss on this set:** 0/27 (0/19 on GM-690's edits, vs 4/19 without rerank),
including 5/5 first-query reps. GM-693 (down-weighting short field/const nodes) may not be needed for this
failure; it is only worth doing if a different query set shows misses with the cross-encoder installed.
Limits: one project, one query, one run; 0/27 bounds the true miss rate below ~10% at 95% confidence.
