# 0017. The semantic rung embeds off the async workers; plain store reads stay on them

## Status
Accepted (2026-10-02)

## Context

The daemon serves every session on two tokio workers (`daemon/mod.rs`). Tool
handlers take the store lock (`IndexStore::read`, a blocking
`Mutex<Connection>`) on those workers. The name resolver's last rung,
`find_definition::by_semantic_neighbours`, embeds the query with the ONNX
model. Every name-resolving tool reaches it through `resolve_symbol_name`:
`find_definition`, `find_references`, `find_callers`, `find_callees` and
`find_implementations`. The rung embedded while it held the store lock, on a
worker.

Measured with the machine idle (load < 4), p95, on g-mesh / excalidraw
([`gm-473-store-lock-hold.md`](../results/gm-473-store-lock-hold.md)):

| | lock held | another session waits |
|---|---|---|
| structural handlers | ≤ 12.6 / ≤ 5.7 ms | ≤ 21 ms |
| semantic rung, model warm | 69 / 43 ms | 106 / 60 ms |
| first semantic call after daemon start | 636 / 614 ms | ~620 ms, `tools/list` included |

On a first call, ~570 ms of that is the model load. The query embedding costs
~7 ms, and the vector scan under the lock 30–60 ms.

## Decision

- **Plain store reads stay on the workers.** Their hold times do not justify
  a `spawn_blocking` hop per call.
- **The semantic rung is resolved lazily, in two passes.**
  1. The handler first runs on the worker with the rung deferred. If an
     earlier rung answers, nothing is embedded.
  2. If the deferred rung is reached, the name is embedded on `spawn_blocking`
     with no lock held, and the handler re-runs with the vector.

  `resolve_lazily_off_worker` does this for the server's handlers.
  `resolve_lazily` is the synchronous form for the CLI and tests.
- This applies the rule `search_code` follows since GM-448: inference never
  runs on a worker or under the store lock.

## Consequences

- A cold start no longer stalls other sessions behind the model load.
- The vector scan (30–60 ms warm) still runs under the store lock, now on
  the blocking pool rather than a worker: other sessions' store reads wait
  for it, but their worker-only calls (`tools/list`) do not.
- On the semantic path the structural rungs run twice. No other path pays
  anything, and no answer changes.
- A future store query slow enough to matter on a worker brings plain reads
  back into question. Re-measure with `eval/gm473_lock_hold.py`.

## Alternatives considered

- **Move every store read off the workers.** Rejected on the numbers above:
  ≤ 21 ms of waiting does not pay for a blocking-pool hop on every call.
- **Embed eagerly before resolution.** Rejected: most names resolve on an
  earlier rung, so every call would pay for an embedding it does not use.
