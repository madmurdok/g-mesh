# 0016. search_code: a cross-encoder reorders the top 30, the verdict stays the embedding's

## Status
Accepted

## Context
GM-443 and GM-464 S1 measured an order-only rerank of the int8 embedding
ranking's top 30 by `cross-encoder/ms-marco-MiniLM-L6-v2` (F4: `logit + 80 *
cosine`, the cross-encoder reading the same structured text the embedding
model embeds). On held-out NL queries it raised r@10 by 3.7 [+1.8] and MRR by
0.047 [+0.021] over int8-structured, with every D9 gate passing
(`docs/results/gm-443-recall-rerank.md`). The design and the owner's decisions
D1-D6 are in `docs/architecture/gm-464-ce-rerank.md`.

## Decision
- **Where it runs.** Only in `search_code::handle`, on a first page
  (`cursor` none) that is complete (no embedding pass owed, D6). `search`
  itself is unchanged, so its other callers (`find_definition`'s semantic
  rung, the eval's `top_k_for_eval`) never rerank. The `Reranker` hangs off
  `EmbeddingPipeline`, loaded lazily like the embedding model.
- **The order.** The window is `search`'s first `max(limit, 30)` rows; its
  first 30 are stably sorted by `logit + 80 * cosine` (f64), so ties keep the
  embedding order; later rows keep it too. Pairs are tokenized together,
  truncated at 512 tokens longest-first, and scored in length-sorted chunks of
  16, as the Python reference (`rerank_eval.CrossEncoder.score`) does.
- **What does not change.** `score` stays the cosine. `noMatch` /
  `lowSimilarity` are judged on the embedding order's page (the rows today's
  handler would show), so the rerank can never flip a verdict: judged on the
  reranked page, a row from positions 21-30 could push out the one row that
  cleared its floor.
- **Pagination (D4).** With `limit >= 30` the whole window is on page 1 and
  `search`'s own cursor continues after it. With `limit < 30` the cursor is
  `rerank:<base64 {rest, after}>`: the unshown window ids in reranked order,
  then `search`'s cursor at the window's last row. A continuation re-reads the
  `rest` rows by id (a deleted row is skipped) and then continues with
  `search`; it never runs the cross-encoder.
- **Model (D1, D2).** `g-mesh model fetch` downloads it beside the embedding
  model (`--no-rerank` skips it, `G_MESH_RERANK_MODEL_DIR` moves it): the
  fp32 export at a pinned revision, sha256-checked at fetch and again at load.
  Apache-2.0; the MS MARCO data terms are noted in the README.
- **Switch (D3).** Global `[rerank] enabled` (default on), overridden by
  `G_MESH_RERANK=off`, read once per daemon. Off, a missing model, or a
  refused one all take today's code path; a missing model logs one line.
  A failing call (inference error, non-finite logit) falls back to the
  embedding order for that call and logs it.
- **Threads (D5).** `min(4, physical cores)` intra-op threads, Level3,
  deterministic compute.

Rejected: reranking within the page only (K = limit; at the default limit 20
it loses about a third of the gain), recomputing the window and the scores on
page 2 (another ~200 ms, and a re-index between calls would duplicate or skip
rows), exposing the logit as the score (uncalibrated, the floors could not be
read from it), and bundling the weights (91 MB per release archive, against
the "weights are not vendored" rule).

## Consequences
- The fetch grows by ~87 MiB. Installs that do not re-run `model fetch` keep
  the embedding order, and the daemon log says why once.
- A first page costs one cross-encoder pass over 30 pairs; continuation pages
  cost nothing extra. Latency is measured in GM-464 S5.
- Within the top 30 `score` no longer falls monotonically; the tool
  description says the score is the embedding cosine.
- No stored state changes: vectors, `embeddingVersion`, the pipeline epoch
  and the embedding cache's fingerprint describe the embedding model only.
- Order parity with the Python reference is pinned by
  `core/src/embedding/testdata/rerank_parity.json`
  (`eval/embedding/gm464_rerank_fixture.py`): the blend is checked against the
  reference's logits in every run, and the real model's logits and order in an
  `#[ignore]`d test that needs the weights.
