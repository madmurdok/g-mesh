# GM-424 embedding cache: measurement

Slice S8 (measure) of GM-424. It measures the machine-wide embedding cache
(`docs/adr/0007-embedding-cache.md`) at `c4afbbe`
(`feat/GM-424-shared-embedding-cache`) against the S1 baseline in the ADR
(3.13.0 at `a40d040`). Every arm has a control with the cache off
(`G_MESH_EMBEDDING_CACHE=off`) or empty. No code was changed to produce
these numbers.

## Summary

| Arm | Cache on | Control (cache off) | S1 baseline (3.13) |
|---|---|---|---|
| 1. Cold `g-mesh reindex`, worktree A, cache empty | real 871s; 6,675 texts, **5,933 embedded**, 742 hits (duplicate texts within the run); backfill 740.8s | (this arm is itself the empty-cache case) | real 668s, embedding 604s |
| 2. `core/Cargo.toml` edit under a live daemon, A | **5,818 texts, 5,818 hits, 0 embedded, 6.3s**; rust bulk + vectors done at t=7.1s | 5,818 texts, 0 hits, 5,818 embedded, 686.9s; vectors done at t=687s | ~510s embedding of ~578s |
| 2. not-found window, `find_definition EmbeddingPipeline` | **t=1.2s to t=2.2s** (2 samples at 1s spacing) | **t=1.2s to t=224.6s** (223 samples) | t=3.3s to t=140.2s |
| 2. `search_code` top hit wrong | t=1.2s to t=2.2s | t=1.2s to t=225.6s (TS node first until t=38s, then unrelated rust nodes) | TS node first while rust partial |
| 3. Cold `g-mesh reindex`, second worktree B, cache warm from A | real **117.8s** (user 16.9s); **6,675 hits, 0 embedded**; backfill 2.1s | real 900.2s (user 3,165s); 0 hits, 6,675 embedded; backfill 827.0s | - |
| 4. Upgrade (stale `meta.indexer_version`), daemon restart, A | index wiped; all vectors at **t=77.0s**; **6,675 hits, 0 embedded**, backfill 3.9s | index wiped; all vectors at t=872.7s; 0 hits, 6,675 embedded, backfill 799.0s | - |
| 5. Correctness | top-10 ids and scores of 3 queries identical on A and on B; B: 6,678 of 6,678 vector blobs byte-identical | (the reference) | - |

Cache file after all arms: `cache.sqlite` **27,811,840 bytes (26.5 MiB)**,
5,921 entries, 1 model row, schema `user_version` 1 (6,790 pages of 4 KiB).
The ADR estimated ~20 MB for one full index of this repo.

What is left in every cache-on arm is not embedding: the structural walk
(~6s), the semantic passes (rust-analyzer 50-105s), and on the CLI path
the rust semantic pass dominates (arm 3: 98.5s of 117.8s).

## Machine and method

- 8-CPU macOS (x86_64), the same machine as S1. `uptime` was recorded before
  and after every run. Load averages below are the 1-minute value at the
  start and end of each run.
- Release binaries built once in worktree A (`cargo build --workspace
  --release`, then `npm install && npm run build` in `plugins/typescript`).
  Build time is not counted anywhere. The same binary served both worktrees;
  the TS plugin was A's `dist/`.
- Throwaway worktrees A and B at `c4afbbe` (detached), both indexed as
  single projects. B was never built, so the two differ only in untracked
  build output, which the index ignores: both reported 13,107 nodes, 25,641
  edges, 6,675 embedding candidates.
- Isolated `G_MESH_HOME=/tmp/gm424s8` with only `models` linked to
  `~/.g-mesh/models` (jina-embeddings-v2-base-code, revision `516f4ba`).
  The cache therefore started empty at arm 1 and lived at
  `/tmp/gm424s8/embedding-cache/cache.sqlite`.
- The cache switch was checked in each daemon's environment
  (`ps eww <pid>`): `G_MESH_EMBEDDING_CACHE=off` present in the control
  daemons, absent in the treatment daemons.
- Probe (the S1 approach): an MCP stdio client over `g-mesh mcp-shim`,
  issuing `find_definition {symbol_name: EmbeddingPipeline}` then
  `search_code {query: "compute embedding vectors for a batch of texts with
  the onnx model", limit: 3}` once a second, timestamps relative to the
  moment just before the `sed -i` that changed `version = "3.14.0"` to
  `"3.14.1"` in `core/Cargo.toml`. The daemon's `daemon.log` was followed
  with `tail -F` and every line stamped on arrival, which gives the
  unit timings below.
- Arms 1 and 3 time `g-mesh reindex` with `/usr/bin/time -p`. Arm 4 times
  from the shim bootstrap to the `embedding backfill -` log line.
- Upgrade simulation (arm 4): with the daemon stopped,
  `UPDATE meta SET indexer_version = '2+simulated-previous-release'` in the
  project's `index.db`. That is exactly the state an index from a previous
  indexer version or plugin build is in when a new daemon opens it:
  `schema::ensure_current` (`storage/schema.rs:530`) compares the stored
  string against `registry::indexer_version` (`daemon/registry.rs:283`,
  `CURRENT_INDEXER_VERSION` + plugin digest), finds them different, and
  wipes. The log confirms it in both arms: `index (re)initialized - a full
  reindex is needed` at t=0.3-0.5s. No scratch build was needed. (An organic
  digest change also happened during the run; see "Incidental finding".)
- Correctness (arm 5): `search_code` top-10 for three queries
  ("compute embedding vectors for a batch of texts with the onnx model",
  "wipe the index when the indexer version changes", "parse typescript
  source files into symbols"). Pair A: after arm 2's treatment (rust vectors
  served from the cache) against after arm 4's control (every vector
  computed by the model). Pair B: after arm 3's treatment (every vector
  from the cache) against after arm 3's control. For B the `vectors` tables
  of the two indexes were also compared blob by blob.

## Per-arm detail

### Arm 1: cold index, cache on and empty (A)
Load 27.6 at start (the tail of the build), 7.1 at end.
- `real 871.41 user 2909.91 sys 19.46`: CPU-bound (user ~3.3x real).
- `embeddings [backfill]: 6675 texts, 742 cache hits, 5933 embedded, 0 cache
  errors, 740.8s`. Even an empty cache saves 11%: 742 texts repeat within
  the project (the same doc comment and signature on several nodes). The
  cache then held 5,921 entries, 12 fewer than 5,933 embedded, most likely
  duplicates embedded in the same batch before either was written.
- 125ms per embedded text against S1's ~92ms. The load here was higher than
  in S1, and arm 3's control (124ms) and arm 4's control (120ms) agree, so
  this is the machine at the time, not the cache. Cache-off arms are the
  comparison for timing, not S1's absolute numbers.

### Arm 2: `Cargo.toml` edit under a live daemon (A)
Treatment: daemon started with the cache on, on arm 1's ready index. Load
4.7 at start, 5.1 at end.
- +0.80s `[rust] workspace changed (core/Cargo.toml)`; +6.66s rust bulk index
  complete; +7.08s `embeddings [workspace-reindex rust]: 5818 texts, 5818
  cache hits, 0 embedded, 0 cache errors, 6.3s`; +71.14s rust semantic pass
  done (62.2s).
- Probe, 239 samples over 240s: `find_definition` answered not-found at
  t=1.17 and t=2.17, found again from t=3.18. `search_code` put
  `flowSequenceItems` (TS) first at t=1.17 and a rust test at t=2.17,
  then `EmbeddingModel::embed` from t=3.18 on, the same top hit as before
  the edit. `find_definition` median 4ms (max 281ms), `search_code` median
  59ms (max 116ms).

Control: the daemon restarted with `G_MESH_EMBEDDING_CACHE=off` on a ready
index (the one arm 4's control had just rebuilt, all 6,675 vectors present).
Load 8.1 at start, 8.8 at end.
- +0.36s workspace changed; +681.08s rust bulk index complete; +687.28s
  `embeddings [workspace-reindex rust]: 5818 texts, 0 cache hits, 5818
  embedded, 0 cache errors, 686.9s`.
- Probe, 586 samples over 590s: `find_definition` not-found from t=1.18 to
  t=224.61, found from t=225.62. `search_code` top hit: `flowSequenceItems`
  (TS) until t=38, then the file node `core/src/cli/model.rs`, then
  `cli::reindex::tests::…` from t=76.6, then the file node
  `core/src/embedding/backfill.rs` from t=183.4, and the right hit only from
  t=225.6. `find_definition` median 5ms (max 202ms), `search_code` median
  77ms (max 335ms). No lock stall in either arm, as in S1.
- The control reproduces S1's gap, longer on this load (224s against
  137s). With the cache the gap is 1-2s: the rust rows are still deleted up
  front (`workspace_reindex.rs:276`), so it is not zero, but they come back
  as fast as the walk can write them.

A first control run was discarded: see "Discarded run".

### Arm 3: second worktree, cache warm from A (B)
Treatment, load 5.9 at start and 9.2 at end: `real 117.83 user 16.94 sys
4.01`; `embeddings: 6675 of 6675 nodes got a vector - 6675 from the embedding
cache, 0 embedded`; backfill 2.1s. The rust semantic pass took 98.5s of it.

Control, load 7.3 at start and 106.6 at end (a SentinelOne `log show` and
`launchctl dumpstate` burst arrived in its last minutes): `real 900.15 user
3165.40 sys 19.23`; `0 from the embedding cache, 6675 embedded`; backfill
827.0s. The control did not reuse duplicates within the run either (6,675
embedded, against 5,933 in arm 1): with the cache off there is no dedupe.

### Arm 4: upgrade, the generation check wipes the index (A)
Control first (the daemon then served arm 2's clean control). Load 14.1 at
start, 9.1 at end.
- +0.54s `index (re)initialized`; +6.28s `initial index built - 13107 nodes`;
  +73.75s all four semantic passes done; +872.74s `embeddings [backfill]:
  6675 texts, 0 cache hits, 6675 embedded, 0 cache errors, 799.0s`. First
  `find_definition` answered at 6.4s.

Treatment, load 5.4 at start and 6.2 at end.
- +0.29s `index (re)initialized`; +6.03s initial index built; +73.08s
  semantic passes done; +77.02s `embeddings [backfill]: 6675 texts, 6675
  cache hits, 0 embedded, 0 cache errors, 3.9s`.
- In both arms the daemon runs the semantic passes before the backfill, so
  the time to complete vectors is ~73s of semantic passes plus the backfill.

### Arm 5: correctness
- A: all three queries, top 10, `symbolId` and `score` identical between the
  cache-served index (after arm 2's treatment) and the cache-off index
  (after arm 4's control).
- B: identical top 10 for all three queries between arm 3's treatment and
  control. The `vectors` tables: 6,678 rows each, all 6,678 blobs
  byte-identical (`o.embedding = v.embedding` on `nodeId`), 0 different; one
  length (3,072 bytes) and one `embeddingVersion`
  (`jina-embeddings-v2-base-code`).
- Node ids are the same across the two worktrees too: A's cache-off top 10
  and B's cache-on top 10 list the same ids.

## Discarded run

The first control of arm 2 restarted A's daemon with the cache off and
edited `Cargo.toml` 60s later. The new daemon's start had wiped the index
(`index (re)initialized` 13s before the edit), so the edit landed on a daemon
still doing its own cold start: the rust workspace reindex queued behind a
backfill, started at t≈238s, and embedded 5,818 texts in ~567s. Its
not-found window (t=237.8s to t=342.5s, ~105s after the reindex began) is
consistent with the clean control, but its timeline is not comparable, so
the table uses the rerun.

## Incidental finding: a `Cargo.toml` edit in a dev checkout wipes the index at the next daemon start

Why that daemon start wiped: the first `Cargo.toml` edit (arm 2's treatment,
06:03:5x) made rust-analyzer, which the rust plugin runs for its semantic
tier, reload the workspace and run `core/build.rs`. That rebuilt
`plugins/typescript/dist/**` (06:04:15) and `plugins/go/g-mesh-plugin-go`
(06:04:18) in place. The plugin digest in `indexer_version` covers those
files, so the stored generation no longer matched and the next daemon
start threw the index away. A later edit did not rebuild them (mtimes
unchanged), so it is once per build-script invalidation, not every edit.

This is not the cache's doing and only affects a dev checkout whose bundled
plugins live next to the source rust-analyzer builds. With the cache on, the
wipe costs ~77s instead of ~870s (arm 4). Without it, a developer who edits
`Cargo.toml` pays a full cold index at the next daemon start. Worth its own
task: keep rust-analyzer from running `core/build.rs`, or keep an
identical rebuild from changing the digest. Which of the rebuilt files
actually changed content was not checked.
