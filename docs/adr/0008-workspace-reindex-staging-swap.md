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
- The semantic pass answers with an ordinary diff (`watcher/apply.rs`
  `apply_semantic_pass`), and what it holds depends on the tier. The
  TypeScript tier re-sends a structural edge under its own id with
  `source = 'semantic'`, an upgrade in place. The SDK's LSP bridge
  (rust-analyzer, pyright; `plugins/sdk/src/lsp/bridge.rs` `Answers`) and
  the Go tier (`plugins/go/semantic.go` `semanticDiff`) never do: they add
  placeholder nodes and semantic edges onto them under ids no structural
  walk emits, which linking then points at their targets, and they retract
  (`deleteEdgeIds`) structural edges they contradict (Go: a conversion
  mistaken for a call). Every tier retracts its own earlier edges only by
  the ids it remembers emitting in its own process, so after a restart
  nothing retracts them. A whole-project pass re-sends every edge it stands
  behind, not a diff against that memory: the upserts are the full answer,
  only the retractions are a diff.

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
   rows that are new or differ (upsert), **except the outgoing edges of an
   unchanged node, which come from live (the unchanged-node rule,
   section 3)**; containers likewise;
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
(the unchanged-node rule is what keeps that plan empty); a crate rename
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
- **Unchanged-node rule.** A node is *unchanged* when it is in both
  indexes with every `nodes` column equal (`id`, `kind`, `name`,
  `qualifiedName`, `filePath`, the four range columns, `signature`,
  `visibility`, `visibilityContainer`, `docComment`, `language`,
  `nativeKind`, `hasSyntaxErrors`, `container`) and its `declarations` and
  `placeholder_targets` rows equal: exactly "in both and not upserted". An
  unchanged node keeps **all** its live outgoing edges - structural,
  semantic, and the pass's retractions (a staged structural edge from it
  that live lacks is not inserted). Two exceptions: a live `syntactic`
  edge whose staged edge of the same id differs is replaced by it (the
  walk is the authority on structural edges; a live `semantic` edge, an
  in-place upgrade included, is never replaced this way); and a live edge
  whose target does not exist after the swap is deleted, and the staged
  edges from that node of the same kind, which live lacks or holds only
  with a vanished target, are taken in its place. A changed or new node
  gets exactly staging's outgoing edges; the pass after the swap refines
  them.
  Why these columns: every edge a node emits describes a site inside its
  range, so a different range, signature or container means different
  sites or a different scope to resolve them in. A same-size body edit
  leaves the row equal, but then the file's bytes differ from its
  `indexed_files` baseline, which the swap does not touch, so the watcher
  event waiting on the reindex lock or `ensure_fresh` reparses the file and
  replaces its edges; the reindex does not own that correction. What
  remains, by case:
  - node deleted: its edges are deleted with it;
  - node new or changed: structural edges only, until the pass reaches it;
  - unchanged node whose live edge's target was deleted: that edge goes,
    the walk's edge of the same kind replaces it, until the pass;
  - unchanged node whose structural link would now resolve differently
    (a dependency added or moved, same node rows): a still-`syntactic`
    live edge takes the walk's answer; a `semantic` one keeps the old
    answer until the pass, or a reparse of that file, rewrites it.
- **Sweep.** After a *complete whole-project* semantic pass for a
  language whose manifest declares `[plugin.capabilities] semantic_sweep =
  true`, core deletes that language's `source = 'semantic'` edges the
  pass did not re-send (`watcher/apply.rs` `sweep_semantic_edges`): what
  an earlier process emitted and no process retracted. An incomplete pass
  (a partial answer) and a per-file pass (one file's answer) sweep
  nothing. Placeholder nodes the swept edges pointed at stay, as every
  linked-away placeholder does (`graph/symbol_links.rs`, "Why the
  placeholder is kept"); `graph::queries` already hides them.
  The sweep trusts "complete", so it is opt-in and absent means off
  (ADR 0005's conservative default): a third-party plugin that never says
  `incomplete` is not swept until it declares it can be. Rust, Go and
  Python declare it. TypeScript declares `false`: its pass upgrades
  structural edges in place and never reports incomplete, so a checker
  failure part-way would sweep upgraded structural edges it did not
  repeat (gone, not downgraded, until the file is reparsed). The Go tier
  reports incomplete only when every module fails to load.
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
unchanged nodes stay as they were, the pass's retractions included, until
the pass replaces them; the only transient states are the per-case ones in
section 3, limited to what changed. Semantic edges no complete pass stands
behind any more do not outlive the next complete whole-project pass.

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
8. *Unchanged node keeps its live edges*: index, run a (fake) semantic
   pass that rewrites `e1` onto another target, adds `sem` and retracts
   the structural `r`, all from an unchanged node; reindex with unchanged
   code and hold before the new pass: `e1` and `sem` read as the pass left
   them, `r` stays absent. Control: leave the unchanged set empty -> `e1`
   is structural again, `sem` gone, `r` back.
9. *Changed node takes the walk's edges*: a node whose signature changed
   gets its new structural edge and loses the pass's edges. Control: count
   every node in both as unchanged -> the new edge is missing.
10. *Edge into a dropped node is replaced*: an unchanged node's edge into a
   node the walk dropped goes, and its new edge of the same kind comes in.
   Control: drop the replacement branch -> the new edge is missing.
11. *Sweep*: a complete whole-project pass deletes a semantic edge of its
   language it did not re-send and keeps its structural edges and another
   language's semantic ones; an incomplete whole-project pass and a
   per-file pass delete nothing. Controls: remove the sweep -> the stale
   edge survives; ignore `incomplete` or the scope -> it is gone.
12. *Differing syntactic twin*: an unchanged node's live syntactic edge
   whose staged twin links elsewhere is taken from staging; its semantic
   edges (an in-place upgrade included) and a retracted edge stay as live
   has them. Test 7 adds a live syntactic edge with an identical twin,
   which must not be written. Controls: drop the twin branch -> the old
   target stays; drop the plan's `EXCEPT` -> test 7 counts one more change.
13. *Sweep is opt-in*: after a complete pass, a language declaring
   `semantic_sweep` loses a semantic edge the pass did not re-send; one
   that does not (as TypeScript) keeps it. Controls: never pass the
   language to the sweep -> the first keeps it; always pass it -> the
   second loses it.
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
- Stale semantic edges from an earlier process last until the next complete
  whole-project pass of their language, not forever; a pass's retracted
  structural edges no longer come back at a swap.
- TypeScript is not swept (section 3, Sweep): its stale semantic edges
  from an earlier process last until their files are reparsed.

## Owner's answers
1. The semantic pass is not run into staging; it runs after the swap, and
   the unchanged-node rule (section 3) keeps the old semantic edges serving
   until it replaces them.
2. The worst-case swap hold (crate rename, ~1-3s) is accepted; atomicity is
   kept.
3. `pending_reindex` is a new table; no schema-version bump.
4. A failed reindex is retried on each start with no backoff and reported
   in `g-mesh status`; the live graph stays usable meanwhile.
5. (2026-09-26, after S2 found that the LSP bridge and Go tiers add edges
   rather than upgrade them) An unchanged node keeps all its live outgoing
   edges, retractions included, except an edge into a vanished target,
   which the walk's edge replaces; changed and new nodes take staging's.
   A complete whole-project pass sweeps its language's semantic edges it
   did not re-send. This replaces "keep a live semantic edge whose
   endpoints both survive".
6. (2026-09-26, S5 review) The sweep is a manifest capability,
   `semantic_sweep`, off unless declared; TypeScript stays off until its
   pass can report incomplete (GM-430). An unchanged node's live syntactic
   edge whose staged twin differs is taken from staging.

Follow-ups filed separately: telling callers which files' semantic edges
are still pending after a swap, and measuring where a semantic pass's time
goes.
