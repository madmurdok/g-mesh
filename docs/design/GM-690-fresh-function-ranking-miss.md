# GM-690: a freshly added Rust function sometimes misses search_code's top 10 (diagnosis)

Slice GM-690/S1. Origin: GM-435 results, anomaly 3 (Rust B rep 1).

## Verdict

**Reproduced: 4 misses in 19 edits (B 3/13, A 1/6). Not a latency, cold-start,
index-refresh or merge bug: it is the ranking itself.** search_code ranks by
plain cosine here (the rerank model is not installed on this machine, so the
cross-encoder is off), and the new function's cosine to the query sits right at
the top-10 cutoff: 0.458-0.547 (median 0.502, sd 0.022) against a cutoff of
0.481 set by ten unrelated "hub" nodes (short fields/consts such as `BUDGET`,
`Shape.pending`, `Coverage.waited`, all at 0.48-0.55). The salt in each rep's
doc comment moves the cosine by a few hundredths, and that alone decides
whether the function makes the top 10. A miss is stable: the same function
is still absent 10 s later and its vector is in the store.

Recommendation: **record as expected ranking behaviour for GM-690 (no code
change)**, and open one follow-up measurement: rerun this repro with the
cross-encoder available (option B below), since the misses ranked 11-25, all
inside the rerank window of 30.

## Setup

- Binary: release build of this branch (db1074a, g-mesh 4.23.0), target seeded
  by APFS clone. Build `real 400.41 user 77.81 sys 7.95`.
- Project: a detached scratch worktree of db1074a under the scratchpad (not the
  main checkout). Home `/tmp/gm690h` (the scratchpad path made the daemon
  socket 170 bytes, over the 103-byte limit), models symlinked to
  `~/.g-mesh/models`, embedding cache copied with `sqlite3 .backup`; daemon
  started directly with `G_MESH_EDIT_TIMING=1`, killed by recorded pid
  (`left_processes` empty for both sessions).
- Harness: the GM-435 `measure.py` (imported unchanged) plus a GM-690 driver:
  same edit (remove `languages`, `outcome`; append `edit_probe_alpha/beta/gamma`
  with a salted doc comment), same query
  `tally lantern shipments waiting in quartz harbour ledger`, same pollers
  (find_definition every 25 ms, search_code every 100 ms from the moment all 3
  vectors are in the db, find_callees). Added per rep: alpha's rank and score
  on every search page, the stored vectors (cosine to rep 0's alpha vector),
  a `limit: 60` search after the tiers, and on a miss a search with alpha's
  exact doc sentence and a repeat 10 s later.
- Order: session 1 = rep 0 as the very first search_code call after daemon
  start, B reps 1-6, then A/B interleaved reps 7-12 (GM-435's order). Session
  2 (restart, 4 more B reps) did not run: after a restart the daemon re-runs
  no Rust semantic pass on an index that already has one, so the warm gate
  waiting for that log line timed out (harness gap, not a g-mesh finding).
- Machine: load 57-60 at start (another agent's builds), 4.7-11.7 during the
  reps. Whole run `real 4498.74 user 177.37 sys 192.07` (1029 s of it the cold
  index warm-up).

## Results

`t_vec`: all 3 vectors in the db, s after save. `rank`: alpha's rank in the
`limit: 10` page (absent = not within 5 s of polling). `cos`: alpha's cosine
(from the `limit: 60` search after the tiers; rank there in parentheses).
`11th`: the best cosine below the top 10 of the other nodes' list.

| arm | rep | first query | load1 | t_vec | polls | rank | cos (rank@60) | cos to rep-0 vector |
|---|---|---|---|---|---|---|---|---|
| B | 0 | yes | 10.5 | 2.28 | 1 | 4 | 0.5042 (4) | 1.0 |
| B | 1 | | 8.5 | 0.51 | 20 | **absent** | 0.4731 (15) | 0.9855 |
| B | 2 | | 6.5 | 0.67 | 1 | 4 | 0.5090 (4) | 0.9844 |
| B | 3 | | 5.8 | 0.56 | 1 | 3 | 0.5177 (3) | 0.9865 |
| B | 4 | | 5.9 | 0.86 | 20 | **absent** | 0.4737 (15) | 0.9857 |
| B | 5 | | 8.5 | 0.86 | 1 | 4 | 0.5096 (4) | 0.9828 |
| B | 6 | | 10.2 | 0.53 | 23 | **absent** | 0.4576 (25) | 0.9839 |
| A | 7 | | 11.7 | 0.71 | 1 | 7 | 0.4929 (7) | 1.0 |
| B | 7 | | 8.8 | 0.54 | 1 | 7 | 0.4936 (7) | 0.9863 |
| A | 8 | | 9.3 | 0.68 | 23 | **absent** | 0.4804 (11) | 0.9922 |
| B | 8 | | 6.4 | 0.50 | 1 | 5 | 0.5020 (5) | 0.9869 |
| A | 9 | | 6.2 | 0.66 | 1 | 10 | 0.4859 (10) | 0.9894 |
| B | 9 | | 5.3 | 0.58 | 1 | 10 | 0.4877 (10) | 0.9849 |
| A | 10 | | 5.1 | 0.65 | 1 | 10 | 0.4848 (10) | 0.9902 |
| B | 10 | | 5.0 | 0.66 | 1 | 3 | 0.5287 (3) | 0.9850 |
| A | 11 | | 5.1 | 0.74 | 1 | 3 | 0.5173 (3) | 0.9872 |
| B | 11 | | 4.7 | 0.54 | 1 | 3 | 0.5142 (3) | 0.9848 |
| A | 12 | | 6.2 | 0.71 | 1 | 2 | 0.5475 (2) | 0.9891 |
| B | 12 | | 5.1 | 0.52 | 1 | 3 | 0.5226 (3) | 0.9871 |

The other nodes' top 10 is the same in every rep (0.5509, 0.5439, 0.5106,
0.5026, 0.4982, 0.4970, 0.4914, 0.4901, 0.4899, 0.4811): the miss is exactly
`cos < 0.4811`. Every found rep has cos >= 0.4848, every miss <= 0.4804. The
GM-435 miss page lists the same ten nodes with the same scores (0.5509,
0.5439, 0.5106, ...), so it is the same effect. When found, it is found on the
first search call, 0.10-0.18 s after the vectors land.

## Hypotheses checked

| hypothesis | result | how |
|---|---|---|
| Cold state: first query after start | ruled out | rep 0 was the first search_code call of the daemon and found it (rank 4); the misses were reps 1, 4, 6 and A 8, all warm. |
| Lexical/FTS arm not refreshed; hybrid merge with an empty arm | does not exist | `search_code::handle` (core/src/mcp/search_code.rs:307-392) has one arm: `search` (227-264) is a brute-force `1 - vec_distance_cosine` over `vectors JOIN nodes`, paginated by score; there is no FTS/BM25 table or merge. |
| Vector index cache / stale reader snapshot | ruled out | no ANN index; `IndexStore` (core/src/storage/index_store.rs:212) is one `Mutex<Connection>` shared by writer and reader, so a read sees every committed write. On every miss the db already held all 3 vectors (`has_vec`) and the miss persisted 10 s later. |
| Vector stored but node text written later / wrong text embedded | ruled out | `round_trip` (core/src/watcher/apply.rs:991-1110) commits the node rows (`apply_file_diff_linked`), then embeds (`EmbeddingPipeline::compute`), then `store_vectors`; the semantic pass re-embeds only nodes with no vector. The miss reps' alpha vectors are 0.984-0.992 cosine to rep 0's, the same spread as found reps: the stored vector is the salted text's normal vector. |
| Score tie at the cutoff | ruled out | no ties: misses are 0.0007-0.024 below the 11th score. |
| Ranking: cosine near the cutoff, rerank off | **confirmed** | above. The daemon logged `rerank model ms-marco-MiniLM-L6-v2 is not available (... model.onnx not found) - search_code keeps the embedding order` (`Reranker::scorer`, core/src/embedding/rerank.rs:341-363), and every first page is in strictly descending cosine order. |

Why the margin is so small: even the function's own doc sentence as a query
scores it only 0.46-0.49 (rank 1-2 of a page whose top is 0.46-0.49), while
short field/const nodes act as hubs at ~0.55 for this prose query. The stored
text is `structured_doc + signature` (`text_to_embed`,
core/src/embedding/text.rs:99-102) with the salt inside the doc; for this
model (jina-embeddings-v2-base-code int8) a prose query against a doc+code
text lands near 0.5 whether relevant or not.

### g-mesh calls this relied on

- `find_callers handle_off_worker` -> only `GMeshMcpServer::search_code`
  (core/src/mcp/mod.rs:915): the tool's sole entry.
- `find_callers search_code::search` -> `handle`, `first_page_reranked`,
  `continue_rerank`, `top_k_for_eval`, `find_definition::by_semantic_neighbours`
  (+1 test): the one ranking function, shared with find_definition's semantic
  rung.
- `find_callers EmbeddingPipeline::store` -> `apply`, `Writer::store_vectors`;
  `find_callers Writer::store_vectors` -> `watcher::apply::round_trip` only
  (one untyped-receiver hint in embedding/backfill.rs, the backfill path,
  not on an edit).
- `find_definition IndexStore::read` -> `ReadGuard(self.acquire())` on the
  single connection mutex.
- Vector write sites (grep, non-code-graph question: SQL text):
  `storage/vectors.rs:74` insert, deletes in `storage/write.rs:456`,
  `graph/imports.rs:370`, `graph/containers.rs:544`, `storage/language_swap.rs`.

## Is a fix needed?

Not for GM-690's acceptance: the cause is named and it is ranking behaviour,
not a defect in the edit-to-search path. The user-visible risk is real but
general (any well-documented function can lose to hub nodes on a prose
query), so it belongs to the ranking-quality work, not to this bug.

### Options

| option | benefit | risk |
|---|---|---|
| **A. Record as expected (recommended for GM-690)**; fix the GM-435 method instead: judge "searchable" by rank within `limit: 30` or by a query without the hub margin | zero code risk; GM-435-style measurements stop reporting a latency anomaly that is a ranking one | the hub effect stays for users without the rerank model |
| B. Make the cross-encoder present: a follow-up that reruns this repro with the rerank model in a scratch `G_MESH_RERANK_MODEL_DIR`; if it fixes the misses, surface the missing model louder (status/doctor line) or fetch it with the embedding model | the misses ranked 11-25, inside the 30-row window that the rerank reorders (`first_page_reranked`, search_code.rs:469-505, `WINDOW` rerank.rs:52), so this is the likely remedy and needs no ranking change | 91 MB download; rerank latency per call; unmeasured until the follow-up runs (BETA=80 weights cosine heavily, a 0.08 cosine gap is 6.4 logits) |
| C. Down-weight hub nodes (short field/const texts with no doc) in `search` | helps without the rerank model | changes every ranking; needs the embedding eval (`cli::embed_eval`) before/after; touches the shared `search` used by find_definition's semantic rung |

If B or C is taken up later, the edit map:

- B: `Reranker::scorer` (core/src/embedding/rerank.rs:341-363) and the status/
  doctor output that reports model availability; read `first_page_reranked`
  (core/src/mcp/search_code.rs:469-505), `blended_order` (rerank.rs:123-140).
- C: `search` (core/src/mcp/search_code.rs:227-264) or the embed text
  (`text_to_embed`/`node_text`, core/src/embedding/text.rs:99-140); read
  `pagination::paginate_by_score`, `find_definition::by_semantic_neighbours`.

Behaviours tests would pin (B or C): a documented function whose doc matches
the query ranks above doc-less field/const nodes that score higher by raw
cosine (fixture: one target at cosine 0.47, ten hubs at 0.48-0.55, page of
10); with the scorer unavailable, the page is the plain cosine order
(today's behaviour, already covered by search_code's tests); a control
reverting the change must drop the target out of the page.

## Artifacts

Scratchpad (session 221b5b90): `gm690/repro.py`, `gm690/run.sh`,
`gm690/summarize.py`, `gm690/out/raw.jsonl`, `gm690/out/summary.md`, both
daemon logs in `gm690/out/`.
