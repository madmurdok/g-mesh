# GM-425 workspace reindex staging swap: measurement

Slice S10 (measure) of GM-425. It measures the staging-and-swap workspace
reindex (`docs/adr/0008-workspace-reindex-staging-swap.md`) at `fccd632`
(`fix/GM-425-keep-rows-during-workspace-reindex`) against `c1536a7`
(`release-3.14.0`: GM-424's embedding cache, still delete-first). The
embedding cache was off (`G_MESH_EMBEDDING_CACHE=off`) in every measured
daemon, so the before window is at its longest. No code was changed to
produce these numbers.

## Summary

| | BEFORE `c1536a7` (control) | AFTER `fccd632` |
|---|---|---|
| `find_definition EmbeddingPipeline` not-found window | **t=1.0s to t=221.0s** (221 of 221 samples in it) | **none** (405 of 405 found; repeat run 63 of 63) |
| `search_code` top hit wrong | **t=1.0s to t=220.0s** (220 samples) | **never** (0 of 400 post-edit samples; repeat 0 of 60) |
| `find_callers get_or_spawn` (23 rows: 20 semantic, 3 syntactic) | not found t=1.0-175.0s; 3 rows (structural only) t=176.0-745.0s; 23 from t=746.0s | **23 at every sample** |
| Rust rows back / swap | bulk complete +669.0s (668.3s after the change was seen) | **swapped in +2.98s** (2.44s after) |
| Texts embedded by the reindex | **5,910** (0 cache hits), 687.1s | **0** ("0 texts owed a vector", no embedding pass) |
| Semantic pass done | +745.5s (55.9s) | +60.6s (56.5s) |
| Query median / max, `find_definition` | 5.0ms / 227.5ms | 4.3ms / 49.7ms |
| Query median / max, `search_code` | 69.4ms / 310.8ms | 55.4ms / 127.4ms |
| Query median / max, `find_callers` | 5.4ms / 133.8ms | 5.5ms / 28.6ms |
| Daemon CPU over the probe | 2,789s over 905s real | 125.5s over 405s real |
| SIGKILL mid-reindex: nodes (pre-edit 13,865) | **2,201** (rust 0 of 11,664), vectors 864 of 6,774 | **13,865**, vectors 6,774 (kill before the swap) |
| `g-mesh status` after the kill | coverage 28.6%, 237 dirty, `semantic pass: complete` | `pending reindex: rust (after core/Cargo.toml changed)` |
| Next start | (not run) | reran the reindex, swapped, 13,865 nodes, pending row and `staging-rust.db` gone |

The control reproduced the window (221s against GM-424's 224s), so the
probe measured what it was meant to. The after build has no window at all:
every sample of all three queries answered as before the edit.

## Machine and method

- 8-CPU macOS (x86_64), the machine of GM-424 S8. `uptime` was recorded
  before and after every run (below). No other agent was running; the two
  pre-existing daemons for `…/ClaudeProjects` and `…/g-mesh` (pids 19172,
  20014) were left alone.
- Three throwaway detached worktrees: build worktrees at `c1536a7` and
  `fccd632` (`npm ci && npm run build` in `plugins/typescript`,
  `npm ci` in `plugins/python`, `cargo build --workspace --release`; build
  time not counted), and a separate project worktree P at `fccd632` that
  was the indexed project in every arm. Keeping the indexed checkout apart
  from the binaries' checkouts means rust-analyzer's `core/build.rs` run
  after a `Cargo.toml` edit rebuilds P's plugins, never the plugins whose
  digest is in `indexer_version` (the GM-424 incidental finding); no
  restart in this run wiped the index unexpectedly.
- `G_MESH_HOME=/tmp/gm425m` with only `models` linked to
  `~/.g-mesh/models`. Daemons were started directly
  (`g-mesh daemon --project-root P`, `nohup`, real pid recorded) with the
  environment checked by `ps eww`: `G_MESH_EMBEDDING_CACHE=off` in every
  measured daemon. The cache was on only for the index builds before the
  arms (the first cold build filled it: 6,771 texts, 745 hits, 6,026
  embedded, 751.0s; later rebuilds after switching binaries took 6,771
  hits). Switching binaries wipes the index (plugin digests differ), so
  each arm started from a fresh build with that binary, then a restart with
  the cache off on the ready index (`index.phase` = `ready`, daemon idle).
- Index: 13,865 nodes (rust 11,664), 35,009 edges (2,809 semantic), 6,774
  vectors (6,771 candidates) in every pre-edit state.
- Probe (the GM-424 arm-2 probe): an MCP stdio client over `g-mesh
  mcp-shim` (`CLAUDE_PROJECT_DIR` removed, cwd P) issuing, about once a
  second, `find_definition {symbol_name: EmbeddingPipeline}`,
  `search_code {query: "compute embedding vectors for a batch of texts
  with the onnx model", limit: 3}` (expected top hit
  `embedding::model::EmbeddingModel::embed`, a Rust function) and
  `find_callers {symbol_name: get_or_spawn, limit: 200}`
  (`daemon::registry::PluginRegistry::get_or_spawn`: 23 callers, 20 of
  them from semantic `CALLS` edges and 3 syntactic, checked in `index.db`).
  t=0 is just before the probe rewrote `version = "…"` in P's
  `core/Cargo.toml` (patch +1). The daemon's stderr was followed and every
  line stamped on arrival. `/usr/bin/time -p` wraps the probe; the daemon's
  own CPU is `ps -o time` before and after.
- Kill arms: the same probe sends SIGKILL to the daemon's pid, either at
  t=15s (the ADR's plan) or 1.0s after the daemon logs `workspace changed`.
  Counts are read from `index.db` with `sqlite3` right after the kill,
  then `g-mesh status` (cwd P), then a restart.

## Per-arm detail

### Arm 1: BEFORE (`c1536a7`), cache off
Load 4.06 at start, 4.89 at end. Probe `real 905.43 user 1.63 sys 0.66`
(the probe waits); daemon CPU 0:01.95 -> 46:30.73, i.e. ~2,789s of CPU in
905s: the embedding.
- Log: +0.75s `[rust] workspace changed (core/Cargo.toml)`; +669.03s rust
  bulk index complete (11,158 nodes); +687.73s `embeddings
  [workspace-reindex rust]: 5910 texts, 0 cache hits, 5910 embedded, 0
  cache errors, 687.1s`; +745.47s rust semantic pass (55.9s).
- 905 samples, t=-5.0 to 899.0. `find_definition`: found to t=0.01,
  `no symbol named 'EmbeddingPipeline' found` t=1.01 to t=221.01 (221
  samples), found from t=222.01.
- `search_code` top hit: `flowSequenceItems` (TS) t=1-36, the file node
  `core/src/cli/model.rs` t=37-72,
  `cli::reindex::tests::the_embedding_pass_reports_how_many_vectors_came_from_the_cache`
  t=73-175, the file node `core/src/embedding/backfill.rs` t=176-220, the
  right hit from t=221.01. The same sequence as GM-424's control.
- `find_callers get_or_spawn`: no anchor (the `semanticNeighbours`
  fallback: "Nothing is named 'get_or_spawn'", 1-3 unrelated suggestions)
  t=1.01-175.01; anchored with 3 rows (the syntactic callers only) from
  t=176.01 to t=745.0 (570 samples, ~9.5 minutes); 23 rows from t=746.0,
  right after the semantic pass.
- Latency: `find_definition` median 5.0ms, max 227.5ms (t=1.01);
  `search_code` median 69.4ms, max 310.8ms (t=221.01); `find_callers`
  median 5.4ms, max 133.8ms (t=175.01).

### Arm 2: AFTER (`fccd632`), cache off
Load 4.89 at start, 3.64 at end. Probe `real 405.43 user 0.78 sys 0.31`;
daemon CPU 0:01.86 -> 2:07.35 (~125.5s).
- Log: +0.54s workspace changed; +2.37s rust bulk index complete (11,158
  nodes); +2.98s `rust reindex swapped in - nodes -1161 +5, edges -9 +0,
  containers -0 +0, 0 texts owed a vector`; +60.60s rust semantic pass
  (56.5s). No `embeddings [...]` line: nothing was embedded.
- 405 samples, t=-5.0 to 399.0: `find_definition` found at all 405,
  `search_code` top hit `EmbeddingModel::embed` at all 405,
  `find_callers` 23 rows at all 405.
- Latency: `find_definition` median 4.3ms, max 49.7ms (t=58.05);
  `search_code` median 55.4ms, max 127.4ms (t=8.01); `find_callers` median
  5.5ms, max 28.6ms (t=224.01). The samples around the swap (t=1.01 to
  t=4.0): `find_definition` 4.3-6.7ms, `search_code` 52.9-82.5ms,
  `find_callers` 4.9-11.8ms. No sample waited on the swap's transaction;
  the swap hold is below the 1s sampling resolution and below every
  query's max.
- Repeat (the first pre-swap kill attempt, whose trigger did not fire, so a
  plain 60s run): load 5.53 -> 4.81; swapped in at +2.86s (+0.41 changed),
  semantic pass +58.89s; 63 of 63 samples correct for all three queries;
  max 97.6ms / 119.4ms / 11.4ms.

### Arm 3: kill
**AFTER, SIGKILL before the swap** (1.0s after `workspace changed`,
t=+1.70s; the plugin then logged `core closed the bulk stream's lifeline`,
so the walk was in progress). Load 4.84 -> 5.23.
- `index.db` after the kill: nodes 13,865, rust nodes 11,664, edges
  35,009, semantic edges 2,809, vectors 6,774: all equal to the pre-edit
  counts, and the node id sets are identical (0 missing, 0 added).
  `pending_reindex`: `rust|core/Cargo.toml|2026-09-26 13:45:42`.
  `staging-rust.db` (6.7 MB) left in the state directory.
- `g-mesh status`: `semantic pass: complete` and `pending reindex: rust
  (after core/Cargo.toml changed) - interrupted; its previous graph serves
  until the daemon runs it again on its next start`.
- Next start (load 5.05 -> 6.62): `the rust workspace reindex after
  core/Cargo.toml was interrupted - rerunning it`, swapped in (`nodes -1161
  +5, 0 texts owed a vector`), semantic pass 56.3s. Then 13,865 nodes,
  2,809 semantic edges, 6,774 vectors, `pending_reindex` empty,
  `staging-rust.db` gone, status `ready` / `semantic pass: complete`.

**AFTER, SIGKILL at t=15s** (the ADR's timing; run twice, identical). The
swap is at ~+3s now, so 15s lands after it, inside the semantic pass (the
ADR's "killed after the swap" case, not the pending case). Load 3.72 ->
7.21 and 4.87 -> 5.60.
- Nodes 12,704 (rust 10,503), edges 35,000, semantic edges 2,800, vectors
  6,774; no `pending_reindex` row (correct: the swap had committed).
- `g-mesh status`: `semantic pass: never completed - run g-mesh reindex
  to repair it`.
- Next start: `the project was walked but its semantic pass never
  completed - retrying it`, semantic pass 49.6s, back to 13,865 nodes and
  2,809 semantic edges.

**CONTROL: BEFORE, SIGKILL at t=15s.** Load 4.84 -> 9.41.
- Nodes **2,201** (rust **0** of 11,664), edges 4,894, semantic edges 191,
  vectors 864. All 11,664 rust node ids gone. (GM-424's earlier kill left
  12,293 of 13,416; there the kill came later in the walk. At 15s the
  delete-first build has deleted rust and not yet written any of it back:
  it writes after embedding.)
- `g-mesh status`: coverage 28.6% (95/332 source files), 237 dirty files,
  and `semantic pass: complete`: the flag the ADR's section 4 fixes.
- Not restarted; the next start's recovery is not part of the control.

## Findings

1. **The swap deletes 1,161 semantic-pass nodes every time, on an unchanged
   tree, until the semantic pass re-adds them (~57s here).** `nodes -1161
   +5, edges -9` in every after run. The 1,161 are all rust `Module` nodes
   with no vector and 9 semantic edges between them, created by the rust
   semantic pass (qualified names like
   `g_mesh::graph::queries::graph::queries::find_by_qualified_name`,
   `orphan:core/tests/wedged_daemon.rs::Project::bootstrap_core`); the
   walk does not emit them, so the staging diff reads them as deleted. None
   of the three probe queries touched them, and a kill in that gap leaves
   12,704 nodes until the next start's semantic retry. Worth a look:
   whether the swap should keep rows the semantic tier owns (the
   `semantic_sweep` rule in the ADR) instead of treating them as walk rows.
2. **The ADR's "15s into the reindex" kill no longer hits the pending
   case**, because the reindex finishes its swap in ~2.5s. The pending
   case was measured with a kill 1.0s after `workspace changed`.
   `G_MESH_BULK_INDEX_HOLD_FILE` does not hold a workspace-reindex walk
   (only the cold walk calls `hold_the_walk_open_for_tests`,
   `bulk_index.rs:217`); a run with it set swapped at +3.8s as usual.
3. After a kill in the semantic pass, `g-mesh status` says `run g-mesh
   reindex to repair it`, while the daemon repairs it by itself on the next
   start (it did, both times). The advice is heavier than needed.
4. A freshly started daemon does not index until its first client
   connects (the first cold start sat at `unindexed` for 21 minutes until
   the probe connected). Not a GM-425 matter; noted because it cost time.

## GM-431 re-measure

Slice S6 of GM-431: the node count across the swap with the placeholder fix
(`6c55669`, `fix/GM-431-reindex-swap-keeps-semantic-nodes`) against its
merge base `20a4ed3` (`release-3.15.0`, before the fix) as the control.
`d180aa4` was not used: GM-428/429/433 changed the reindex and semantic pass
after it, and `20a4ed3` is the fix's direct parent. No code was changed.

Method: S10's, reduced to the counts. The indexed project was a detached
throwaway worktree at `20a4ed3` (separate from both build worktrees, both
`cargo build --workspace --release`). Each arm had its own
`G_MESH_HOME=/tmp/gm431-<arm>` (models linked, embedding cache a copy of
`~/.g-mesh`'s), cold-built with the cache on, then restarted with
`G_MESH_EMBEDDING_CACHE=off` and `G_MESH_CORE_IDLE_MS=0` on the ready index.
The script then bumped the patch version in the project's `core/Cargo.toml`
and read `index.db` every 0.2s (total nodes; rust `Module` nodes with
`nativeKind = 'pending_symbol'`, the placeholders) until the
`[rust] semantic pass:` line plus 20s. Daemons were started without a shell
(real pids recorded) and stopped with SIGTERM; `ps` afterwards showed none of
them left, and only the pre-existing daemons for other roots were running.

| | control `20a4ed3` | fix `6c55669` |
|---|---|---|
| Nodes before the edit (placeholders) | 14,623 (5,042) | 14,623 (5,042) |
| Swap log line | +5.78s: `nodes -1240 +6, edges -17 +0` | +4.54s: `nodes -0 +6, edges -0 +0 ... 1240 placeholder(s) kept for it` |
| First sample after the swap | **13,383 (3,802)**, at +5.81s | **14,623 (5,042)** |
| Between the swap and the end of the pass | 13,383 until +61.19s | 14,623 in all 286 samples |
| Rust semantic pass done | +60.85s (53.4s) | +67.72s (61.6s) |
| After the pass | 14,623 (5,042) | 14,623 (5,042) |
| Lowest count after the edit | 13,383 (3,802) | 14,623 (5,042), 410 samples |

The control reproduces S10's gap: 1,240 placeholders (1,161 at S10's
`fccd632`; the tree differs) and 17 edges gone for ~55s. The fix keeps all
of them: the count never moves.

Machine: 8-CPU macOS x86_64. Control: `uptime` load 14.59 / 58.16 / 46.77
at start (another workload had just finished), 9.18 at the edit, 19.11 at
the end; `real 1064.71 user 41.11 sys 34.72` (the script waited out its 900s
cap because its pass-done pattern did not match the log line; the samples
cover the whole window). Fix: load 14.41 at start, 5.82 at the edit, 5.74 at
the end; `real 201.89 user 14.83 sys 7.90`. Daemon CPU over the measured
window: 3.1s (control), 2.8s (fix); the pass runs in the plugin and
rust-analyzer.
