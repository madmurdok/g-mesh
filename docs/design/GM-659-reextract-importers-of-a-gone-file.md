# GM-659: re-extract the importers of a file that leaves the index

Status: design (S1). No production or test change in this slice; the probe
below was reverted. Line numbers are at `2189b5d` (release-4.19.0, GM-658
merged).

## The problem in one example

`src/a.ts` has `import { f } from "./m"`. `src/m.ts` and `src/m/index.ts`
both exist; TS resolves `./m` to `src/m.ts`, and core links the import:
the `IMPORTS` edge `a.ts -> File(src/m.ts)`, `resolved = 1`, and the
placeholder that carried it is dropped (`graph::imports::link`,
`imports.rs` 307-420).

Delete `src/m.ts`. The import should fall back to `src/m/index.ts`. It does
not: `find_references`/`get_dependencies` show `src/a.ts` importing nothing
until `src/a.ts` itself is edited or the language is reindexed. Create
`src/m/index.ts` after the delete (instead of having it from the start):
same result.

## 1. Today (diagnosis, with evidence)

### Probe

Three throwaway tests in `core/tests/it/created_file_links_waiting_import.rs`
(the real TS plugin through `typescript_registry::Harness`, the same
`indexed()` fixture: six fillers, every file rewritten to `mark_v2` unrouted
so a re-extract is visible), dumping every `IMPORTS` row out of `src/a.ts`
with a `LEFT JOIN` onto its target node. Output (abridged):

| case | after delete | after the next event | `src/a.ts` re-extracted |
|---|---|---|---|
| `./m`, only `src/m.ts`; delete, recreate `src/m.ts` | edge kept, `toId` = the deleted file id, `resolved=1`, target node absent | same edge, target node present again (`src/m.ts`) | no |
| `./m`, only `src/m.ts`; delete, create `src/m/index.ts` | as above | still dangling | no |
| `./m`, `src/m.ts` + `src/m/index.ts`; delete `src/m.ts` | as above | (no event) still dangling | no |

Build/run: `real 228.70 user 2.56 sys 160.77` for the APFS target clone at
load 14.6 (the waiting was the clone's metadata, not a slow machine); the
three tests took 4.09 s.

### Where the row goes

The edge row is **not** deleted; it is left **dangling**, which every query
hides because they join `edges.toId` onto `nodes`:

1. The plugin answers the delete with the file's own deletes
   (`plugins/sdk/src/run.rs` `Session::file_changed`, the `let Some(source)
   = source else` arm). `watcher::apply::round_trip` (apply.rs 901-971) sees
   `FileScope::Gone` (`file_scope`, 995-1011) and calls
   `Writer::apply_file_diff_linked` (index_store.rs 456-482).
2. `storage::file_rows::widen` (file_rows.rs 46-97) deletes the file's nodes
   and their *outgoing* edges; edges *into* them are left alone by design
   (file_rows.rs 20-23, `storage::write::write_diff` 417-426,
   `storage::connection::open` 84-86).
3. Nothing remains to say what `src/a.ts` was importing: the placeholder
   (with its `placeholder_targets` row) was dropped when the import was
   linked (`imports::link`, the `remaining == 0` branch), and the edge only
   holds a node id.

### Why nothing relinks or falls back

- **Same path recreated:** works today by accident. The `File` node id is a
  pure function of the path, so the re-upserted `File(src/m.ts)` revives the
  dangling edge. No re-extract happens (probe row 1).
- **Other candidate created (`src/m/index.ts`):** the plugin's first-sighting
  delta (`TsProject::source_changed`, project/mod.rs 175-196) asks core for
  importers stored at the address `src/m`
  (`config_reindex::indexed_importers`, config_reindex.rs 287-327). `a.ts`'s
  edge has no placeholder (so no stored scope) and points at no `File` node
  (so no linked target); neither half of the query can see it.
- **Fallback on delete:** a delete starts nothing. `source_changed(path,
  None)` returns `None` (it only forgets `announced`), and core starts no
  re-extract of its own for a gone file. Even if the plugin answered
  `Affected { Target File Exact("src/m.ts") }`, core would select after the
  delete is applied, and the linked half of `indexed_importers` joins the
  deleted `File` node: zero rows.

### The "deliberate" comment, and whether it holds

`graph::imports::link_diff`'s doc (imports.rs 195-202), mirrored in
`graph::symbol_links::link_diff` (symbol_links.rs 761-771), says a deleted
target is not covered "deliberately": the importers' edges "go when
something deletes the node" and "come back as fresh placeholders the next
time those importers are reindexed". Two of its premises no longer hold:

- The edges do not go; they dangle (probe). The comment also contradicts
  `write_diff`'s and `file_rows`' own docs.
- "The next time those importers are reindexed" was the only re-extract
  trigger when it was written. Since GM-507/GM-509 core has an owed
  re-extract mechanism (`reextract_affected`, `schema::owe_reextracts`,
  `config_reindex::resume_owed_reextracts`) with a 30% whole-language
  fallback, and GM-655 made the selection keyed. Re-extracting the
  importers of a gone file now costs what re-extracting the importers of a
  created file already costs.

The reasoning that still holds: core must stay language-agnostic (it cannot
pick the fallback candidate itself), and edges into a deleted node must stay
lazy for the *same-diff* delete+upsert of a changed symbol (write.rs
417-426). Neither is affected by the fix below.

The GM-658 test comment ("Core drops an edge into a deleted file with its
node", created_file_links_waiting_import.rs 202-205) is wrong for the same
reason and is corrected by this task.

## 2. Options

### (A) Core re-extracts the importers of a gone file (recommended)

In the same commit that removes a gone file
(`apply_file_diff_linked`, `scope == Gone`), *before* `widen`, read the
importers of the file's `File` node:

```sql
SELECT DISTINCT src.filePath FROM nodes t
JOIN edges e ON e.toId = t.id AND e.kind = 'IMPORTS'
JOIN nodes src ON src.id = e.fromId
WHERE t.filePath = ?1 AND t.kind = 'File' AND src.filePath <> ?1
```

(`idx_nodes_filePath` then `idx_edges_toId`), record them with
`schema::owe_reextracts(conn, <file's language>, <gone path>, importers)`
in the same transaction, and return them. `round_trip` hands them to
`apply_file_change_in`, which re-extracts them like an `Affected` selection
(merged with whatever the plugin's own delta selected, same 30%
`Selection::TooMany` rule, same semantic pass, same `settle_reextracts`).
The plugin re-resolves each importer in its own candidate order:

- a remaining candidate exists -> the import links to it (fallback);
- none -> the plugin emits its ordinary unresolved placeholder at the
  *address* (`src/m`, imports.rs `record_specifier`), which the existing
  first-sighting delta finds when `src/m.ts` or `src/m/index.ts` appears.

`retire_file` (seeded deletion, no plugin) gets the owed rows from the same
commit and the next start's `resume_owed_reextracts` finishes them.

- Benefit: one language-agnostic mechanism; no plugin change, no schema
  change, works on existing indexes for every new delete; reuses the owed /
  fallback / resume machinery that GM-507/GM-509/GM-655 already test; the
  import ends listed as unresolved (or relinked), never edge-less.
- Risk: a delete now costs a plugin round trip per importer; deleting a
  widely-imported file (or a branch switch deleting many) can hit the 30%
  rule and reindex the language, where today it did nothing. Between the
  delete's commit and the re-extract's commit the edge still dangles (a
  query in that window sees no import, as today); a crash in the window is
  covered by the owed rows.

### (B) Keep import placeholders and unlink back onto them on delete

Do what `symbol_links` already does (GM-491): `imports::link` sets
`edges.linkedFrom` and keeps the placeholder; `write_diff` unlinks an edge
whose target is deleted (and not re-upserted in the same diff) back onto
`linkedFrom`, `resolved = 0`.

- Benefit: no window: the import is unresolved in the same commit; a same
  path recreate relinks through `link_diff`'s added-file trigger with no
  re-extract; calls get the same treatment for free.
- Risk: does **not** satisfy the fallback criterion alone (the kept
  placeholder's scope is the deleted path `src/m.ts`, not the address
  `src/m`, so neither `src/m/index.ts` nor the first-sighting delta matches
  it): still needs (A) or a plugin delete delta. One kept node + target row
  per linked import (index growth); existing indexes have no `linkedFrom` on
  import edges until reindexed; `link_all`, `link_diff`'s repoint
  (`toId = ?2` -> `OR linkedFrom = ?2`), `indexed_importers` and
  `sweep_unclaimed_nodes` all change. A semantics change visible to every
  tool that counts placeholder nodes.

### (C) Plugin-side delete delta only

`TsProject::source_changed(path, None)` answers
`Affected { Target File Exact(path) }`.

- Benefit: tiny diff.
- Risk: does not work: core selects after the delete is applied and the
  importer is invisible to `indexed_importers` (section 1). TS only.

**Recommendation: (A).** (B) is a possible follow-up if the window matters;
it is not needed for the acceptance criteria.

### Visible semantics

No change to link semantics: a linked import still points at its `File`
node, an unresolved one at its placeholder. What changes is *when*: after a
target's delete the importer shows (via `get_dependencies` /
`find_references` / `imports_of(resolved=false)`) the fallback target or an
unresolved import, where today it shows nothing. A same-path recreate now
goes delete -> unresolved placeholder -> first-sighting delta -> re-extract
-> linked, instead of reviving the dangling edge.

## 3. Edit map

| # | fn | file | lines | change |
|---|---|---|---|---|
| 1 | `Writer::apply_file_diff_linked` | `core/src/storage/index_store.rs` | 456-482 | for `Gone`: before `widen`, read the importers (query above) and `owe_reextracts` them in the same `in_one_commit`; return `Vec<String>` (empty otherwise). Callers below adapt |
| 2 | new `file_rows::importers_of(conn, file_path) -> Vec<String>` | `core/src/storage/file_rows.rs` | after `widen` (97) | the query; the language comes from the `File` node (`SELECT language`), needed for `owe_reextracts` |
| 3 | `round_trip`, `RoundTrip` | `core/src/watcher/apply.rs` | 901-971, 815-830 | carry `gone_importers: Vec<String>` |
| 4 | `apply_file_change_in` | same | 222-325 | when `!reextract` and `gone_importers` non-empty, re-extract them: union with the plugin's selection, then the existing loop |
| 5 | `reextract_affected` | same | 333-400 | take the extra paths; apply the 30% rule to the union (via a `config_reindex` helper, item 6); skip paths not indexed any more |
| 6 | `config_reindex::select` | `core/src/daemon/config_reindex.rs` | 188-210 | add an `extra: &[String]` (or a sibling `select_with`) unioned before the `TooMany` check, so both triggers share one rule |
| 7 | `retire_file`, `purge_language` | `core/src/daemon/registry.rs` 1552-1556, `core/src/daemon/bulk_index.rs` 283- | - | ignore the returned list; confirm owed rows are right (must-confirm 3, 4) |
| 8 | docs | `graph/imports.rs` 195-202, `graph/symbol_links.rs` 761-771, `storage/file_rows.rs` 20-23, `storage/connection.rs` 84-86, `storage/write.rs` 417-426, `core/tests/it/created_file_links_waiting_import.rs` 1-10 and 202-205 | - | replace "deliberately not covered / edges go" with: edges into a gone file dangle until core re-extracts its importers (present tense, no ticket id: project CLAUDE.md comment rule) |

Read, not changed: `storage::file_rows::widen`, `graph::imports::link_diff`
/ `link`, `config_reindex::indexed_importers`, `schema::owe_reextracts`,
`config_reindex::resume_owed_reextracts`, `TsProject::source_changed`,
`resolve::addresses_resolving_to`, SDK `Session::file_changed`.

## 4. Behaviours for tests

In `core/tests/it/created_file_links_waiting_import.rs` (real TS plugin,
existing `indexed()` fixture; six fillers keep one importer under 30%):

1. **delete -> fallback:** `./m`, `src/m.ts` + `src/m/index.ts`; delete
   `src/m.ts`, route it: `imports("src/a.ts") == (["src/m/index.ts:index.ts"], [])`,
   only `src/a.ts` re-extracted. Also `src/m.tsx` as the remaining candidate.
2. **delete -> unresolved, listed:** `./m`, only `src/m.ts`; delete: no
   resolved import, one unresolved import (`imports_of(false)` non-empty, the
   placeholder's scope is `src/m`); no dangling `IMPORTS` row (`LEFT JOIN`
   count of edges with no target node = 0).
3. **delete -> recreate same path:** then create `src/m.ts`: linked to
   `src/m.ts`, through a re-extract (mark visible).
4. **delete -> create other candidate:** then create `src/m/index.ts`:
   linked to `src/m/index.ts` (fails today: probe row 2).
5. **root index** (GM-658): delete root `index.ts` with `index.tsx` present:
   `..` falls back to `index.tsx` *without* editing `src/a.ts` (replaces the
   `mark_v3` edit in the existing test).
6. **owed rows:** a gone file's importers are owed in the delete's commit; a
   re-extract that is interrupted leaves them for
   `resume_owed_reextracts` (unit test on `apply_file_diff_linked` in
   index_store.rs tests: rows present after a `Gone` apply, keyed by the
   importer, trigger = gone path).
7. **30% rule:** deleting a file most of the language imports returns
   `ReindexLanguage`, not a loop of re-extracts.
8. **no self/gone importers:** an importer deleted in the same batch is not
   re-extracted as a live file (or its re-extract is a harmless `Gone`).

Controls (one per fix): (a) return an empty list from item 2 -> 1, 2, 4, 5
fail; (b) drop the `owe_reextracts` call -> 6 fails; (c) skip the union in
item 6 (no `TooMany` on the extra paths) -> 7 fails. Behaviour 3 passes
today too (dangling revive); it pins that (A) does not break it, it is not
a control.

## 5. Must confirm (code/tests slice)

1. **Calls.** The acceptance list says "no call/import is left with no
   edge". A `CALLS`/`REFERENCES` edge linked into a symbol of the gone file
   (via `symbol_links`, `linkedFrom` set) dangles too. Confirm that the
   importer's re-extract re-sends those usage edges onto new placeholder ids
   (the pending-symbol placeholder's identity includes the resolved file),
   so they end unresolved or on the fallback's symbol. If it does not (an
   edge whose id did not change is not re-sent), the cheapest addition is
   (B)'s unlink restricted to symbol edges, which already have
   `linkedFrom`: `UPDATE edges SET toId = linkedFrom, resolved = 0,
   linkedFrom = NULL WHERE toId = ?deleted AND linkedFrom IS NOT NULL` in
   `write_diff` for ids not re-upserted in the same diff. Decide with the
   owner if needed.
2. **Importer selection covers a container target?** A Go/Rust container
   loses its last member (`graph::containers` GC): same lazy behaviour, out
   of this task's criteria. Leave it, but say so in the corrected docs.
3. `purge_language`: importers are purged in the same loop; confirm the
   owed rows a purge writes are cleared (`clear_gone_file_semantic_rows` /
   the gone-file owed-row cleanup at index_store.rs ~764 covers rows keyed
   by a purged *importer*) or skip owing when purging.
4. `retire_file` (seeded reconcile): confirm `resume_owed_reextracts` runs
   after the reconcile at start (`daemon::activation` 203), so the owed
   importers are re-extracted in that start, not the next.
5. Presence batches (GM-515) and the offline reconcile route each deleted
   path through `apply_file_change_in`; confirm a batch deleting `m.ts` and
   `a.ts` together does not re-extract `a.ts` as live.
6. The re-extract of an importer whose text is unchanged since the plugin's
   baseline still yields a diff when its import's resolution changed
   (`reextract: true` extracts anyway, SDK `file_changed`): pinned by 1 and 2.

## Cross-file answers relied on

- `find_callers storage::file_rows::widen` -> only
  `Writer::apply_file_diff_linked` (g-mesh, main checkout index; file
  unchanged on this branch).
- `find_callers Writer::apply_file_diff_linked` -> `watcher::apply::round_trip`,
  `PluginRegistry::retire_file`, `bulk_index::purge_language`, two
  index_store tests (g-mesh, main index).
- `find_callers watcher::apply::reextract_affected` -> only
  `apply_file_change_in` (g-mesh, main index).
- `find_callers graph::imports::link_diff` (by id) -> `index_store::link_applied`
  plus three tests (g-mesh, main index).
- `find_callers run::Session::source_changed` -> `Session::file_changed`
  (g-mesh; one untyped receiver in a test file, not relied on).
- `resume_owed_reextracts` call site (`daemon::activation` 203): grep, one
  known symbol.
