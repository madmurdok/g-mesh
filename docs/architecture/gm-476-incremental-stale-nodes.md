# GM-476: stale nodes after an incremental reindex

Status: implemented as recommended, scope (a). See "Implementation" at the end.

## Summary

Core applies a plugin's `fileChanged` diff exactly as sent
(`core/src/watcher/apply.rs:420-421` passes it to `apply_diff_linked`, and
`core/src/storage/write.rs:290` `apply_diff` deletes only the ids the diff
names). A plugin can name a deleted id only if it remembers emitting it.
Every plugin keeps that memory per process, and a plugin's long-lived
control process is not the process that ran the bulk walk. So the first
`fileChanged` for a file after the walk (or after a plugin restart) has
**no baseline**. The plugin answers with upserts only, and deletes nothing.
The bulk walk's rows for anything that disappeared stay in the index for
good, together with their child rows.

The TypeScript plugin is worse for deletes: it answers a deleted file with an
empty diff even with a warm baseline.

The three edits differ only in which plugin path hits the empty baseline:

| Edit | Plugin path that answers | Cold baseline (first fileChanged after the walk) | Warm baseline |
|---|---|---|---|
| file deleted | "gone from disk" | nothing deleted (all plugins) | SDK, Go: correct. **TS: nothing deleted** |
| file renamed away | old path: "gone"; new path: cache miss | old path's rows stay (all plugins) | SDK, Go: correct. **TS: old rows stay** |
| declaration renamed (KD→KD2) | cache miss → diff against empty | KD2 upserted, KD and its members stay | correct |

## Repro

Harness: an S1 scratch test, since replaced by
`core/tests/incremental_matches_full_reindex.rs`. Each case did the following. Bulk-walk a fixture with
`bulk_index::run`, then spawn a fresh `PluginProcess` (the daemon's control
process). Optionally warm its baseline with one `fileChanged` per file. Then
edit, and send `fileChanged` for every touched path. The watcher sends both
paths of a rename. Snapshot every node-keyed and file-keyed table, wipe the
state dir, bulk-walk the edited tree, and diff the two snapshots. The fixture
mtimes are backdated so that `record_walk_baselines` records every file.

```
cargo build -p g-mesh-plugin-rust
(cd plugins/typescript && npm ci && npm run build)
cargo test -p g-mesh --test gm476_stale_nodes_repro -- --nocapture --test-threads=1
```

Rust fixture: `src/lib.rs` (`pub mod a; pub mod b;`), `src/a.rs`
(`struct KD { f }`, `impl KD { fn m }`, `fn use_kd(&KD)`), and `src/b.rs`
(`use crate::a::KD; fn other(&KD)`). The table lists the rows that only the
incremental index has (+inc) and the rows that only the full reindex has
(+full).

| Case | nodes | edges | placeholder_targets | qualified_suffixes | containers | indexed_files |
|---|---|---|---|---|---|---|
| Rust delete b.rs, cold | +5: File `src/b.rs`, `b::other`, 2 `pending_symbol` (`krate::a::KD`, `krate::a::use_kd`), container `krate::b` | +7 (all from b.rs nodes) | +2 | 0 | +1 (`krate::b`, count 1) | +1 `src/b.rs` |
| Rust delete b.rs, warm | 0 | 0 | 0 | 0 | 0 | +1 `src/b.rs` |
| Rust rename b.rs→c.rs, cold | same +5 as delete | +7 | +2 | 0 | +1 | +1 `src/b.rs`, 1 only-full `src/c.rs` |
| Rust rename b.rs→c.rs, warm | 0 | 0 | 0 | 0 | 0 | same as cold |
| Rust KD→KD2, cold | +3: `a::KD`, `a::KD::m`, `a::KD.f` | +14 / 2 only-full | 0 | **+2** (`KD::m`, `KD.f`) | `krate::a` count 7 vs 4 | 0 |
| Rust KD→KD2, warm | 0 | 2 dangling `REFERENCES` from b.rs into the deleted `a::KD` (full: unresolved, to placeholder `krate::a::KD`) | 0 | 0 | 0 | 0 |
| TS delete b.ts, cold **and warm** | +4: File `b.ts`, `gone`, `KD`, a.ts's `b.ts#gone` placeholder; 1 only-full `./b` external_module | +6 / 1 only-full | +1 | n/a | n/a | +1 `b.ts` |
| TS KD→KD2, cold | +1 `KD` | +2 | 0 | n/a | n/a | 0 |
| TS KD→KD2, warm | 0 | 0 | 0 | n/a | n/a | 0 |

`declarations`, `vectors` and `semantic_pending_files` showed no difference.
The fixture has no overloads, and embeddings were disabled. By code they
behave the same way as `qualified_suffixes`: they are cleaned only for ids in
`delete_node_ids`.

Two kinds of difference remain even with a warm baseline. Neither is this
bug:

- `indexed_files` keeps the row of a deleted or renamed-away file. No code
  path deletes an `indexed_files` row (`git grep "DELETE FROM indexed_files"`
  finds nothing). The row is file-keyed, so the fix includes it.
- Rows owned by **another** file that pointed at what was removed:
  - b.rs's `REFERENCES` into the deleted `KD`;
  - a.ts's import placeholder `b.ts#gone` and its `IMPORTS` edge to the
    deleted file.

  These come back to the full-reindex shape only when that other file is next
  reparsed. That is the documented lazy behaviour
  (`core/src/storage/write.rs:317-327`, "documented lazy dangling edge";
  `core/src/graph/imports.rs:195-202`, "a file *deleted* from the project ...
  deliberate non-eager behaviour"). See "Decision needed" below.

## Cause per case (file:line)

Shared core cause: `core/src/watcher/apply.rs:420-421` (`round_trip`) commits
the plugin's diff verbatim. `core/src/storage/write.rs:290-337` (`apply_diff`)
removes nodes and their child rows only for `diff.delete_node_ids`, and edges
only for `diff.delete_edge_ids`. Nothing in core reconciles a file's stored
rows against what the plugin says the file now contains.
`core/src/storage/language_swap.rs` is not on this path: `swap` is reached
only from `IndexStore::swap_language` (the workspace reindex), and
`delete_placeholders` only from `Writer::sweep_unclaimed_nodes`.

Plugin side, where the deletes are lost:

- **Deleted file, renamed-away old path**
  - SDK, used by the Rust and Python plugins: `plugins/sdk/src/run.rs:511`
    runs `diff_file(self.index.graph(path), &FileGraph::default())`. On a cold
    baseline `graph(path)` is `None`, so the diff is empty.
  - Go: `plugins/go/control.go:100` (`if !had { return diff }`).
  - TS: `plugins/typescript/src/index.ts:83-86`. `readFile` throws, and the
    handler answers `EMPTY_WIRE_DIFF` **whatever the cache holds**.
    `forgetFile` exists (`incremental.ts`) but this path never calls it.
- **In-file rename on a cold baseline**
  - SDK: `plugins/sdk/src/run.rs:539` (`diff_file(None, &graph)`): upserts
    only.
  - Go: `plugins/go/control.go:116` (`previous := s.files[relPath]`, zero
    value): upserts only.
  - TS: `plugins/typescript/src/incremental.ts:430-433`
    (`diffResults(filePath, EMPTY_RESULT, result, true)`): upserts only. The
    plugin computes `fullExtraction: true` but never puts it on the wire.

The premise that fails is stated in `plugins/sdk/src/diff.rs:56-61` and
`plugins/typescript/src/incremental.ts:403-410`: "the diff against nothing is
everything ... leaves core's graph correct with nothing seeded first". That
holds for a file core has never seen. It is false for every file the bulk
walk committed.

The SDK's `hydrate` (`run.rs:475`) warms baselines during a semantic pass,
but only when an engine is configured. It narrows the window and does not
close it. A plugin restart (idle, crash, rebuild) empties every baseline
again.

## Child tables keyed to a node or a file

| Table | Key | Cleaned when a node/file goes? |
|---|---|---|
| `declarations` | nodeId | yes, for ids in `delete_node_ids` (write.rs:329); replaced on upsert (write.rs:400) |
| `placeholder_targets` | nodeId | yes, same (write.rs:331); replaced on upsert (write.rs:450) |
| `qualified_suffixes` | nodeId | yes, same (write.rs:333); replaced on upsert (write.rs:426) |
| `vectors` | nodeId | yes, same (write.rs:335) |
| `containers` | nodeId (container) | via `containers::detach`/`attach` for members in `delete_node_ids`; the container is deleted when its count reaches 0 |
| `edges` | fromId / toId | outgoing: only via `delete_edge_ids`; incoming: deliberately never (lazy) |
| `indexed_files` | filePath | **never deleted by any path** |
| `semantic_pending_files` | (language, filePath) | cleared by a complete per-file semantic pass (`schema.rs:976`); not on a delete. The repro showed no difference |
| `language_state`, `semantic_pending`, `pending_reindex` | language | not per file; not affected |

So the per-node cleanup in `apply_diff` is complete. Every table is cleaned
correctly *for the ids it is given*. The defect is the id list, plus
`indexed_files`.

## Why the existing tests miss it

- `storage::write::tests::deleting_a_node_takes_*` and
  `watcher::apply::tests::diff_with_deletes_removes_rows` pass explicit
  delete ids, from hand-built diffs or a stub plugin. The cleanup they test is
  correct.
- `core/tests/repeated_edits_through_a_warm_plugin.rs` is warm by design. Its
  header says "The first edit after a plugin starts always worked - the
  plugin has no cached state, so it sends the whole file as upserts and
  deletes nothing". Deleting nothing is exactly the bug for a file the walk
  indexed. No test edits a bulk-walked file through a cold control process.
- The `g-mesh plugins check` session (`cli/plugin_check/session.rs:905-989`)
  sends `fileChanged` on the **unmodified** file first. That warms the
  baseline, and the emptied-file step, `deletes-known` and
  `incremental-matches-bulk` all run warm. The kit never deletes or renames a
  file from disk. That is why the TS warm-delete gap passes conformance.

## Recommended fix

Two parts. Both live in core's apply path, and both feed the existing
`apply_diff`. They reuse its child-table cleanup, container detach/attach,
linking, `store.claim` and embedding untouched.

**1. A file that is gone is removed by core (delete and rename-away; no
protocol change).** In `round_trip`, for `FileChanged` only, between
`to_storage_diff` and `apply_diff_linked`: when the diff upserts nothing and
`project_root/file_path` is `NotFound`, add to the diff:

- the id of every node with `filePath = file_path`. Containers have
  `filePath ''` and so are not touched. Placeholders carry the importer's
  path, so they belong to this file and are included.
- the id of every edge whose `fromId` is one of those nodes.

Then delete the file's `indexed_files` row in the same unit. This fixes every
plugin, cold or warm, including TS and third-party plugins. `round_trip` needs
the project root, which `apply_file_change`'s callers have.

**2. A full extraction says so (in-file rename on a cold baseline; additive
wire field).** Add `#[serde(default)] complete: bool` to
`g_mesh_wire::FileChangeDiff`: "`upsertNodes`/`upsertEdges` are the whole
file". The plugins set it on a baseline miss:

- SDK: `run.rs:539` when `previous` is `None`.
- Go: `control.go`, when `!had`.
- TS: `incremental.ts`, by putting `fullExtraction` on the wire.

When `complete` is set, core adds to the diff:

- every node of `filePath = file_path` that is not upserted;
- every non-`semantic` edge from a node of that file that is not upserted.

An absent field means `false`, which is today's behaviour. An old plugin
keeps the in-file-rename gap only on its first edit per file.

Rejected alternatives:

- **Core infers "cold" from its own count of requests per plugin process.**
  The SDK's `hydrate` warms baselines without core knowing. A warm, partial
  diff read as complete would delete every unchanged node of the file.
- **Plugins compute the deletes themselves.** On a miss they have no record
  of the old ids.
- **Core sends its known ids in the request.** That is a bigger protocol
  change, and every plugin must implement it.
- **Seeding control-process baselines from the walk.** That doubles the
  walk's memory and still breaks on restart.

Risks:

- Part 2 deletes semantic-pass placeholders whose `filePath` is the file and
  that the structural diff does not re-send. The SDK LSP bridge and the Go
  tier add such placeholders. The per-file semantic pass that follows every
  reparse re-sends what it resolves, and a full reindex would not have them
  before its semantic pass either. S2 should cover this with the Go tier's
  tests.
- Part 1 checks the disk after the plugin answers. The race is benign: a
  recreated file gets its own watcher event, and the plugin answers that event
  cold and `complete`.

**Cost.** One implement slice.

- Core: about 80-120 lines. A reconcile helper in `watcher/apply.rs`, two
  `SELECT`s and one `indexed_files` delete.
- Wire: 1 field.
- Plugins: about 5 lines each (SDK, Go, TS).
- Tests, Rust plugin, cold control process: delete, rename-away and KD→KD2,
  each compared with a full walk. Each fails with its part reverted.
  Optionally, a TS warm-delete test.
- Plugin-check (optional, could be a follow-up): a cold-`fileChanged` step
  that renames a declaration and expects `complete`.
- Plugin binaries change, so the plugin versions bump with the release.

## Interface and invariant changes (owner review needed)

- **Interface:** the wire `FileChangeDiff` gains an optional `complete`
  field, and the plugin contract changes. That includes
  `docs/architecture/multi-language-plugins.md` and the SDK's `diff.rs`
  contract doc, which today says a cold diff "leaves core's graph correct".
  No `protocol_version` bump: the field is additive and absent means `false`.
- **Invariant:** none of the documented ones changes. "Edges into a deleted
  node are deliberately not touched" (write.rs:317-327) and the
  non-eager importer behaviour (imports.rs:195-202) stay.

**Decision needed.** Because those invariants stay, the acceptance criterion
"the same nodes and child-table rows as a full reindex" holds for the rows
**owned by the edited file**. It does not hold for another file's
placeholders and edges into what was removed. Examples: the TS importer's
`b.ts#gone` placeholder, and b.rs's dangling `REFERENCES` to `KD`. Those
match a full reindex only after the other file is reparsed, which is already
true with a warm baseline today. The options:

- (a) Scope the criterion to rows owned by the edited file. Recommended.
- (b) Extend the task to eager re-placeholdering of importers. That changes
  both documented invariants and is a separate task.

## g-mesh calls used

| Question | Call | Answer relied on |
|---|---|---|
| Who calls the file-change apply path | `find_callers(watcher::apply::apply_file_change)` | `daemon::plugin::PluginProcess::send_one`, `cli::plugin_check::session::Driver::step`, tests |
| | `find_callers(watcher::apply::apply_file_change_in)` | `apply_file_change`, `watcher::staleness::ensure_fresh` |
| What `send_one` reaches | `find_callees(daemon::plugin::PluginProcess::send_one)` | `apply_file_change` (then `round_trip` → `Writer::apply_diff_linked`, via `find_definition`) |
| Who calls the node-delete writer | `find_callers(storage::write::apply_diff)` | production: `index_store.rs`, `language_swap.rs`, `watcher/burst.rs`, `workspace_reindex.rs`, `graph/*`; the rest are tests (hasMore: the `files` list is complete) |
| Does the watcher reach `language_swap` | `find_callers(storage::language_swap::swap)`, `find_callers(storage::language_swap::delete_placeholders)` | only `IndexStore::swap_language` and `Writer::sweep_unclaimed_nodes`: not the per-file path |
| Who writes `indexed_files` | `find_callers(storage::write::upsert_indexed_file)` | `record_walk_baselines`, `ensure_fresh`, `is_stale`; no deleter |
| Child-table writers | `get_file_outline(core/src/storage/write.rs)`, `get_file_outline(core/src/watcher/apply.rs)` | then `git grep "DELETE FROM"` (SQL text, non-code, so grep) for every per-table delete site |

## Implementation

- Core: `core/src/storage/file_rows.rs` widens a `fileChanged` diff
  (`FileScope::Gone` / `Complete` / `Partial`), and
  `Writer::apply_file_diff_linked` applies it, plus the `indexed_files`
  delete for a gone file, in one step. `watcher::apply::round_trip` decides
  the scope (`file_scope`); `apply_file_change` and `apply_file_change_in`
  take the project root. A `semanticPass` answer is never widened.
- Part 2 also deletes a `semantic` edge out of a node it deletes; it keeps
  only those out of nodes that stay.
- Wire: `FileChangeDiff::complete`, `#[serde(default)]`, omitted when false.
- SDK: `diff_file` sets `complete` when `previous` is `None` (both the
  extraction and the gone path). Go: `Complete = !had` on both paths. TS:
  `fullExtraction` goes on the wire as `complete`.
- TS `index.ts`/`incremental.ts`: still changed, though core now removes a
  gone file's rows without it. A gone file now answers with its cached rows
  removed and **forgets its baseline**. Without that, deleting a file and
  restoring identical text answered an empty diff (the cache still held the
  text) after core had deleted every row: the file vanished from the index.
  `typescript_file_deleted_and_restored_through_a_warm_process` covers it.
- Versions: TS plugin 2.3.0 → 2.4.0, Go plugin 0.3.0 → 0.4.0.
- Not compared, by scope (a): another file's rows into what was removed, and
  the `indexed_files` row of a file that still exists. The watcher path never
  writes baselines (query-time staleness does), so a renamed-to file has no
  row until its first query, as any newly created file.

After the fix, the owned-row difference against a full reindex is 0 in every
case of `core/tests/incremental_matches_full_reindex.rs` (Rust delete cold
and warm, Rust rename-away cold, Rust KD→KD2 cold, TS delete warm, TS
delete-and-restore warm, TS KD→KD2 cold, Go KD→KD2 cold with its semantic
pass). The "before" numbers are the S1 table above.

