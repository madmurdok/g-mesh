# GM-692: fresh-function search_code ranking with the cross-encoder (INCOMPLETE)

Status: **no miss rate measured**. The harness is built and the reranker is proven active, but the full run
failed at rep 0 twice (see Blocker). Follow-up B of GM-690 is still open.

## Setup

- Harness: `eval/gm692/` (GM-690 driver + GM-435 `measure.py`, unchanged), run with `run.sh dry|full`.
  Project = detached worktree of db1074a (GM-690's), binary = release build of this branch (4.24.0 candidate).
- Cross-encoder: `g-mesh model fetch` into `G_MESH_RERANK_MODEL_DIR`; sha256 of `model.onnx`
  5d3e70fd...d4d4a, `tokenizer.json` d241a60d...5c66 (the pinned one); `model status` says it can rerank.
- Plan: 19 edits in one session (GM-690's order), then 4 restarts of (first-query rep + 1 rep) = 27 edits,
  5 first-query reps. Per rep: uptime, real s, daemon cpu s.
- Rerank-active evidence per rep: the first page is not in cosine order and the next cursor starts with
  `rerank:`; the daemon log has no "rerank model ... is not available" line.

## Harness fixes (committed)

- Restart warm gate: the old gate waited for a `rust semantic pass ... complete` log line, which a restarted
  daemon never logs on an index that already has one (python's pyright is missing here, so the pass is
  retried and never completes). Now also accepted: `status --json` index ready (bulk indexed, phase ready,
  all embeddings done) on two consecutive checks. Verified: `warm_via: status-ready` after 25-43 s.
- Watcher canary (added after the first failed run): rewrite a file with its own bytes until an `fs_event`
  shows in the daemon log. Seen on attempt 1, but did not cure the blocker.

## Dry run (1 edit, cold index, load1 ~394)

Rep 0 (first query): rank 1 of the top 10, alpha cosine 0.5314, cutoff (10th cosine in the window) 0.4899,
page not in cosine order, `rerank:` cursor present, first search 16.7 s (includes the model load),
real 37.3 s, daemon cpu 34.2 s. Build `real 1839 user 129 sys 84`. One edit says nothing about the miss rate.

## Blocker

On a restarted daemon (existing index) the full run's rep 0 never saw the edit: no tier reached within 60 s,
no `fs_event` for the edit or the restore, although the canary rewrite minutes earlier was seen and its
semantic pass finished. Cold start (dry run) worked. Machine load1 was 650-760 (other agents), uptime
recorded in `raw.jsonl`. Not determined whether it is load, the watcher after a restart, or a g-mesh bug.
A manual probe daemon logged nothing, so it proved nothing. Suggested next step: run session 1 from a cold
index again (as GM-690 did, ~30 min warm-up) on a quieter machine, and investigate the restart watcher
separately.

## Verdict

Unknown. Whether the rerank closes the 4/19 miss is not answered.
