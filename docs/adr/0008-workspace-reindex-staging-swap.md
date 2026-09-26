# 0008. Workspace reindex: walk into a staging index, swap in only the difference

## Status
Accepted (2026-09-26, GM-425); the owner reviewed the proposal and accepted
it with the answers recorded under "Owner's answers" at the end.

## Context
A settled edit to a `watch_files` entry (`Cargo.toml`, `go.mod`) reindexes
one language (`core/src/daemon/workspace_reindex.rs:252` `run`). Inside the
supervisor's exclusive lock it deletes every row of the language up front
(`:276` -> `IndexStore::delete_language`, `storage/index_store.rs:242` ->
`delete_language_rows`, `storage/write.rs:514`), re-walks it in
`BATCH_ITEMS = 2_000` commits (`daemon/bulk_index.rs:47`, `:388`, each an
`apply_diff` plus that batch's vectors, `index_store.rs:106`), relinks
project-wide in one hold (`index_store.rs:233`), and only then, unlocked,
runs the semantic pass (`workspace_reindex.rs:304-339`).

Measured (`docs/results/gm-424-embedding-cache.md`, arm 2; ADR 0007
Baseline): with the cache off, `find_definition EmbeddingPipeline` answered
not-found from t=1.2s to t=224.6s and `search_code` ranked wrong hits first
until t=225.6s; with GM-424's warm cache the window is 1-2s, still not zero,
because the structural walk itself is the gap. Separately (GM-425's report),
a daemon stopped 15s into a rust reindex left 12,293 nodes instead of 13,416
while `meta.semanticPassAt` still read complete: `delete_language_rows`
removes the `language_state` row, but the meta roll-ups
(`schema::record_bulk_index`, `schema.rs:724`;
`reconcile_semantic_pass_rollup`, `:907`) only ever *set*, never clear, and
startup trusts them (`daemon/mod.rs:279-285`). An interrupted reindex is a
silently partial index, the failure ADR 0002 (2) exists to rule out.

Facts that shape the options:
- One `rusqlite` connection behind `IndexStore`'s mutex, WAL
  (`storage/connection.rs:109`), `foreign_keys` off. Reads take the same
  mutex (`index_store.rs:188` `read`), so any write hold is a query stall;
  ADR 0001 keeps every bulk unit `Hold::PerStep` (`index_store.rs:79-91`).
- `vectors` is a plain table (`nodeId` PK, `embedding` BLOB,
  `schema.rs:451`), not a sqlite-vec virtual table: rows copy like any other.
- Node ids are content-derived: `node_id(file_path, kind, qualified_name,
  native_kind)` (`plugins/sdk/src/ids.rs:145`). A version bump keeps every
  id; a crate rename changes every id in that crate.
- Upserts overwrite by id (`write.rs:331`, `:455`). The walk commits edges
  pointing at *placeholders*; `link_all` repoints them at the end
  (`graph/symbol_links.rs:520`). No edge crosses languages (a Non-goal of
  `multi-language-plugins.md`), so one language's graph links on its own.
- A vector is a pure function of the embedded text and the model
  (ADR 0007 section 1).
- The semantic pass writes no rows of its own: it answers with a diff whose
  edges carry the ids the structural pass gave them, and `apply_diff`
  upgrades each in place - `source` `syntactic` -> `semantic`, `resolved`
  -> true, possibly a better `toId` (`watcher/apply.rs:171-175`). So a
  structural walk emits the *downgraded* version of every edge the pass has
  already upgraded in live.

### Swap cost, measured
A copy of the g-mesh index (13,416 nodes; rust: 11,226 nodes, 28,823 edges,
5,676 vectors, 5,585 placeholder targets, 332 containers; a 40 MB staging
file). `sqlite3` CLI 3.50.6 (`secure_delete=1`, unlike bundled rusqlite),
load 2.8-4.0, warm page cache, three runs each:
- **Full swap** (delete all rust rows, `INSERT ... SELECT` all staged rows,
  one transaction): 1.1-1.4s timed, `real 1.9-2.5 user 0.5 sys 0.4`. Cold:
  2.9-3.2s, of which the edge and vector deletes are 2.2-2.4s.
- **Diff swap** computed inside the transaction, identical trees: 1.2-1.3s,
  all of it the comparison (`id NOT IN`, `EXCEPT` over vector blobs).
- `real` well above `user + sys`: waiting (I/O or the sandbox), not CPU.
  S2 re-measures through rusqlite. Under the store lock, either is a
  multi-second query stall per reindex.

## Decision

### 1. Staging index, plan unlocked, swap only the difference
We will stop deleting up front. A workspace reindex of language `L`:
1. **Marks** (live, one step): `INSERT OR REPLACE INTO pending_reindex
   (language, trigger, startedAt)`, a new table (`CREATE TABLE IF NOT
   EXISTS`, so no schema-version bump and no wipe).
2. **Walks into staging**: a fresh file `<project dir>/staging-<L>.db`
   (removed first if present), opened by `storage::connection::open` +
   `schema::apply`, wrapped in its own `IndexStore`. `walk_one_language`
   and `IndexStore::link_all` run on it unchanged: the walk only needs
   `L`'s own rows, and linking finds every target in `L`. No embedding
   during the walk (`WalkContext { embedding: None }`).
3. **Plans** (no live lock): the staging connection `ATTACH`es the live file
   read-only (WAL gives it a snapshot) and writes a plan into staging
   tables: live node/edge ids of `L` absent from staging (delete); staged
   rows that are new or differ (upsert), **except an edge whose id is in
   both and whose live row is `source = 'semantic'`: live's row is kept
   (the semantic-edge rule, section 3)**; containers likewise;
   `declarations`/`placeholder_targets` replaced wholesale for every
   upserted or deleted node, as `apply_diff` does (`write.rs:321-325`).
4. **Embeds only what changed** (no lock): upserted nodes whose embedded
   text (`text_to_embed`) differs from the live node's, or that are new,
   or whose live vector is missing or has another `embeddingVersion`, go
   through `EmbeddingPipeline::compute` (cache first, ADR 0007). Everything
   else keeps its live vector, correct by ADR 0007's determinism.
5. **Swaps** (one `IndexStore` step, one transaction): `ATTACH` staging,
   apply the plan, delete vectors of deleted nodes and of nodes whose text
   changed, store the computed vectors, write `L`'s `language_state`
   (`bulkIndexedAt` now, `pluginFingerprint`, `semanticPassAt` NULL),
   reconcile both meta roll-ups (section 4), delete the `pending_reindex`
   row, commit, `DETACH`, remove the staging file.

Steps 1-5 stay inside `with_exclusive_access`, as the walk does today
(`workspace_reindex.rs:265`): an edit or `ensure_fresh` of an `L` file
waits on that lock and applies to the post-swap graph, so no edit lands in
live between plan and swap and nothing is lost. Other languages never
write `L`'s rows. The one other writer that can touch them is the
embedding backfill (vectors only); step 5 deletes vectors by node id at
swap time, not from the plan, so a vector it adds meanwhile is either kept
(the node survives, same text) or removed with its node.

The swap's hold scales with the change, not the language: a version bump
with unchanged code writes `language_state` and the meta roll-ups only
(the semantic-edge rule is what keeps that plan empty); a crate rename
rewrites the crate (worst case the full swap, ~1-3s). S2 measures it via rusqlite, S4 on the probe.

**Rejected**
- *Full swap* (delete all `L`, copy all staged rows): simplest, but a 1-3s
  store hold on every reindex (above) where the diff holds ~0 usually.
- *Per-file replacement* (replace a file's rows as the walk reaches it,
  sweep absent files at the end): batches are item-counted, not
  file-bounded (`bulk_index.rs:388`); upserted edges point at placeholders
  until the final `link_all`, so cross-file `find_callers` degrades for
  the whole walk (~11 min cache-off); a module-map change leaves half the
  files on old containers and half on new; a crash leaves that mix.
- *Mark-and-sweep in place* (upsert everything, delete unseen rows at the
  end): the same edge regression, plus transient duplicates whenever ids
  change (old and new row both visible to `find_definition`).
- *Generation column*: every read in `mcp/`, `graph/` and linking would
  have to filter by the visible generation, and content-derived ids
  collide across generations (the PK becomes composite). Far too wide.
- *Shadow tables in the live file*: linking and container code name
  tables unqualified; a separate file reuses that code unchanged.

### 2. Edges and cross-file links
Staging is linked (`link_all`) before the plan, so staged edges carry
resolved `toId`s and compare equal to live ones when nothing changed. Edges
into a deleted node are removed by the plan (both endpoints are `L`'s). An
edge of `L` into a surviving node keeps its id and target. Linked and
unlinked placeholders are ordinary rows and go through the same diff.

### 3. Semantic pass and embeddings: ordering
- The semantic pass runs **after** the swap, against live, as today
  (`workspace_reindex.rs:304-339`), for every language with
  `semantic_pass = true` (rust, go, python; typescript declares no
  `watch_files` and never reaches this path). It still has to run even when
  the code did not change: a manifest edit can move where calls into
  dependencies resolve.
- **Semantic-edge rule.** The swap never downgrades a live semantic edge
  to its structural twin: for an edge id present in both, live's
  `source = 'semantic'` row wins. Queries keep the previous pass's edges
  until the new pass upgrades them in place, instead of seeing
  structural-only edges for the pass's duration (~50-105s for rust on
  g-mesh). What remains, by case:
  - node deleted: its edges are deleted with it (their ids are absent
    from staging);
  - node new: structural edges only, until the pass reaches it;
  - edge whose structural target changed: its id changes, so it is a
    delete + insert and gets the structural edge until the pass;
  - edge id unchanged but the semantic answer would now differ (e.g. a
    dependency moved): live keeps the old semantic target until the pass
    rewrites it - bounded by the pass, and only where the answer moved.
- S2 checks whether a pass can also *add* edges with no structural twin.
  If it can, those are live-only ids and the plan would delete them; the
  rule then extends to "keep a live-only semantic edge whose endpoints both
  survive", and test 8 covers it.
- Embeddings: before the swap only the changed texts are embedded (step 4),
  so with the cache off an unchanged tree embeds nothing (today: 5,818
  texts, 687s, GM-424 arm 2 control). `search_code` sees old vectors until the swap, then
  new ones; there is no window with rows but no vectors except for a node
  whose compute failed (it is left to the ordinary backfill, as today).

### 4. Crash and restart, and flags that do not lie
- Killed before or during the swap (it rolls back): live is identical to
  before, flags included, and complete for the old `Cargo.toml`; a
  `pending_reindex` row and maybe a staging file remain.
- Killed after the swap, before the semantic pass: `semanticPassAt` is
  NULL for `L` and in meta, so `daemon/mod.rs:283`'s
  `needs_semantic_pass_retry` retries it, as for any interrupted pass.
- Startup: stale `staging-*.db` files are deleted; each `pending_reindex`
  row schedules a workspace reindex of its language on activation (D2).
  Until then the old graph serves, and `g-mesh status` names the pending
  language.
- The roll-ups become reconcilers: `record_bulk_index` and
  `reconcile_semantic_pass_rollup` set the meta column when every present
  language has the fact *and clear it when one does not* (today they only
  set). The swap calls both in its transaction. That fixes the reported
  bug on this path and on any other path that resets a language row.

### 5. Visibility guarantees
Queries see the complete old graph of `L` or the complete new one: the
switch is one transaction on the connection they read through. Removed
symbols disappear at the swap; no duplicate is ever visible (staging is
another file; the swap deletes before it upserts). Semantic edges of
surviving nodes stay as they were until the pass replaces them; the only
transient states are the per-case ones in section 3, limited to what
changed.

### 6. Scope
Fixed here: the workspace reindex, and the meta roll-ups (section 4).
Out of scope, unchanged:
- `watcher/apply.rs:84` single-file change: one diff, one transaction with
  its links (`index_store.rs:95`), already atomic. Its vector is stored in
  a second step (`index_store.rs:82`): a short gap, changed nodes only.
- Embedding backfill (`embedding/backfill.rs:119`): only adds vectors.
- An interrupted *cold* walk: `bulkIndexedAt` already guards it.
- Schema or indexer-version wipe (`schema::reset`, `schema.rs:553`, from
  `daemon/mod.rs:269`) and `g-mesh reindex` / `clean`: the old index is
  unreadable by definition or wiped on request. The same staging file could
  later build a whole new index and rename over it; a separate task.

### 7. Tests (S2), each with its control
Over the fake plugin and model already used at `workspace_reindex.rs:543`
and `:963`; the pause is `G_MESH_BULK_INDEX_HOLD_FILE`
(`bulk_index.rs:65`), which already holds a walk open.
1. *Found mid-reindex*: hold the staging walk; a live read of `n1` finds it
   and its vector. Control: call `delete_language` before the walk -> not
   found.
2. *Deleted symbol gone at the end*: walk 1 emits `n1, n2` (+ edges,
   declarations, placeholder targets), walk 2 only `n1`: `n2` and every
   child row are gone, `assert_container_invariants` (`:576`) holds.
   Control: drop the plan's delete step -> `n2` survives.
3. *No duplicate when an id changes*: walk 2 emits `n2` under a new file
   path (new id). Held mid-walk: exactly the old row; after: exactly the
   new. Control: upsert staged rows straight into live -> two rows while
   held.
4. *Interrupted reindex*: the plugin exits non-zero after k items. Live
   rows (a digest over `nodes`, `edges`, `vectors`) and `language_state` /
   `meta` equal the pre-reindex ones; a `pending_reindex` row exists; the
   startup scan re-runs it and clears the row. Controls: restore the
   up-front delete -> digest differs; skip the marker -> startup does not
   re-run.
5. *Flags do not lie*: a semantic pass that fails after the swap leaves
   `language_state.semanticPassAt` and `meta.semanticPassAt` NULL.
   Control: revert the roll-up clear -> meta reads complete.
6. *Only changed text is embedded, cache off*: an unchanged tree embeds 0,
   one edited doc comment embeds 1 (counters at `:1017`, `:1043`).
   Control: embed in the staging walk -> 2.
7. *Swap writes nothing for an unchanged tree*: `total_changes()` across
   the swap equals the `language_state`/`meta` rows only, with live's edges
   already upgraded by a semantic pass. Control: full swap -> thousands.
8. *Semantic edges survive the swap*: index, run a (fake) semantic pass
   that upgrades edge `e1` and retargets it; reindex with unchanged code
   and hold before the new pass: `e1` is still `semantic` with the
   retargeted `toId`. A node whose code changed gets its structural edge.
   Control: drop the semantic-edge rule from the plan -> `e1` reads
   `syntactic` while held.
`delete_language_rows`' own tests (`:380`, `:472`) go with it if nothing
else calls it; S2 says which.

### 8. Measurement (S4)
The GM-424 arm-2 probe (1s samples of `find_definition EmbeddingPipeline`
and `search_code`), cache off (`G_MESH_EMBEDDING_CACHE=off`), on a ready
index, edit only the version string in `core/Cargo.toml`. Before: c1536a7;
after: the S3 build. Record: not-found window (expect none), `search_code`
top hit at every sample, query median and max (max bounds the swap hold),
reindex wall, texts embedded (expect ~0), `uptime` and `/usr/bin/time -p`.
Then a kill arm: SIGKILL the daemon 15s into the reindex; node count equals
the pre-edit count, `g-mesh status` shows the language pending, the next
start completes it. Control for the probe: the before build must show the
224s window again, or the run measured nothing.

## Consequences
- The not-found window, the partial index on a crash and the cache-off
  re-embed of unchanged symbols go away.
- Disk: a staging file per reindexing language while it runs (~40 MB for
  g-mesh's rust), removed at the end or on the next start.
- New code: the plan/swap module and the roll-up clearing. The plan must
  list every `L`-keyed table (a table missed keeps stale rows), so test 2
  asserts every table.
- An `L` file edit still waits for the whole reindex, as today.

## Owner's answers
1. The semantic pass is not run into staging; it runs after the swap, and
   the semantic-edge rule (section 3) keeps the old semantic edges serving
   until it replaces them.
2. The worst-case swap hold (crate rename, ~1-3s) is accepted; atomicity is
   kept.
3. `pending_reindex` is a new table; no schema-version bump.
4. A failed reindex is retried on each start with no backoff and reported
   in `g-mesh status`; the live graph stays usable meanwhile.

Follow-ups filed separately: telling callers which files' semantic edges
are still pending after a swap, and measuring where a semantic pass's time
goes.
