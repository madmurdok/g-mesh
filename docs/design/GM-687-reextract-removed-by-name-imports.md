# GM-687: removing functions imported by name never triggers re-extract (design)

Slice GM-687/S1. Source of the question: docs/results/GM-435-edit-to-tier-latency.md,
"No re-extract in any arm", and the Q3 premise in
docs/design/GM-435-edit-to-tier-latency-method.md ("pick a file whose removed
fns are imported by name, which exercises the re-extract path").

## Answer

**No.** The importers do not need a re-extract when a function they import by
name is removed, in Rust or in TypeScript, and the plugins are right to answer
`affected=none` (Rust) / nothing selectable (TS). The GM-435 Q3 premise was
wrong: `affected` is not about symbols at all. **No fix code is needed.**

The TS "affected=2 without a reextract line" is **not a product bug**: the
re-extract path did run (`reextract_start ... imports=2`, `reextract_done n=0`);
the GM-435 measure script read the file count from a key `reextract_start`
does not carry, so it printed `None`.

## Why: what `affected` means, per plugin

`affected` is `FileChangeDiff::affected`, a `ResolutionDelta`
(wire/src/lib.rs:714-730, GM-507): "the edit changed what other files
*resolve to*" - which file or module an import specifier lands on. It is set in
one place, `run::Session::file_changed` (plugins/sdk/src/run.rs:682, assigned at
:704 for a delete and :723/:739 for a save) from `Extractor::source_changed`.

- **Rust** - `ProjectContext::source_changed`
  (plugins/rust/src/project/mod.rs:361-392). It returns `None` at once unless
  the file's `mod` signature changed (`module_tree::mod_signature`, :373-376)
  or the file is a crate root that appeared/disappeared (:366-368). Removing two
  `pub fn`s changes no `mod` line, so the answer is `None` -> logged
  `affected=none` (core/src/watcher/apply.rs:1031-1040). By design: the module
  tree, not the item list, decides where `use a::b::...` lands.
- **TypeScript** - `TsProject::source_changed`
  (plugins/typescript/src/project/mod.rs:212-236). It offers the addresses a
  path could newly answer (`resolve::addresses_resolving_to`,
  plugins/typescript/src/project/resolve.rs:166-188, plus shadowed candidates,
  waiting addresses), **once per path per plugin process** (`announced`,
  :216-218). For `src/domain/taskStatus.ts` those are two exact `File` keys:
  `src/domain/taskStatus` and `src/domain/taskStatus.js` (the path itself is
  removed at :185). The first save of the file in a daemon's life sends them
  (`affected=2`); every later save sends nothing (`affected=none`). That is the
  "warm-up once, then none" GM-435 saw.

In core, `apply_file_change_in` (core/src/watcher/apply.rs:234-381) starts
`reextract_affected` (:406-469) when `files`, `imports` or `gone_importers` is
non-empty (:285-319). `select_affected` (core/src/daemon/config_reindex.rs:257-298)
answers each exact `File` key with `indexed_importers`: the imports *linked* to a
`File` whose path is the key, or *unlinked* ones waiting at it. Every importer of
`taskStatus.ts` is linked to `src/domain/taskStatus.ts`, not to the extensionless
address, and nothing waits there, so the selection is empty -> `reextract_done
n=0`. Correct: an import that already resolves to an existing file is not
re-resolved by an edit inside that file.

## Why no re-extract is needed: the graph after the edit

Every cross-file usage edge hangs on a `pending_symbol` placeholder in the
*importing* file, never on the declaration's id directly:

- structural: Rust `use a::b::C` -> a placeholder in container `a::b`
  (plugins/rust/src/extractor/decls.rs:57-79); TS named import ->
  `imported_symbol` (plugins/typescript/src/extractor/imports.rs:266-268);
- semantic (both plugins go through the SDK LSP bridge): an engine answer is "a
  `qualifiedName`-keyed placeholder in the *asking* file ... Not a direct edge
  onto the declaration's node id" (plugins/sdk/src/lsp/bridge.rs:200-210).

Core's linker repoints such an edge onto the declaration and keeps the
placeholder in `edges.linkedFrom` (core/src/graph/symbol_links.rs:13-20). When
the declaration's node is deleted and not re-upserted by the same diff,
`write_diff` puts every edge with a `linkedFrom` back onto its placeholder,
`resolved = 0` (core/src/storage/write.rs:420-445, the `UPDATE edges SET toId =
linkedFrom ...`). Re-adding the declaration seeds `link_diff`'s triggers
(core/src/graph/symbol_links.rs:760-811, `seeds`), which wake the waiting
placeholders and relink them. This is pinned already by
`deleting_a_node_puts_the_edges_linked_onto_it_back_on_their_placeholders`
(core/src/storage/write.rs:1010) and
`link_all_and_link_diff_agree_on_the_same_end_state` (symbol_links tests).

So a stale edge to a removed node cannot survive for a symbol edge: the only
edges `write_diff` leaves dangling are those with no `linkedFrom` (:426-433),
which for a cross-file target means an import linked to a *gone file* - and that
case is already handled by `gone_importers` (`Writer::apply_file_diff_linked`).
Re-extracting an importer would re-emit the same placeholder (its source text
did not change) and find nothing to link it to: zero information.

The importers' edges after the removal are exactly what the code is: a call to
a name nothing in the index declares (the compiler would reject it too). Queries
show this as an unlinked usage, not as a call into a ghost.

## Evidence

Build: debug, commit db1074a (this branch, 4.23.0), `G_MESH_EDIT_TIMING=1`,
isolated `G_MESH_HOME=/tmp/gm687-{rs,ts}` (a scratchpad path exceeded the
103-byte socket limit), daemon started directly, warm (semantic pass complete,
15 s quiet) before the edit. Projects: detached worktrees of g-mesh (db1074a)
and task-tracker-mcp (08b72e7) in the scratchpad, removed afterwards. Edit:
in-place rewrite removing the two functions with their doc comments, then
restore; done twice per language. Graph read from `index.db` with sqlite
(script and raw JSON: scratchpad `gm687/repro.py`, `out-rs.json`,
`out-ts.json`). Machine: load 36.7 at Rust start, 9.6 at TS start; Rust run
`real 1600.33 user 99.50 sys 22.93` (1548 s of it the cold rust-analyzer warm-up
under that load), TS `real 69.19 user 8.17 sys 1.19`.

### Rust: core/src/daemon/identity.rs, `project_hash` + `record_project_root`

Timing (edit 1; edit 2 identical in shape):

```
rt_sent id=2 method=fileChanged file=core/src/daemon/identity.rs
rt_answered id=2 affected=none
rt_committed id=2 method=fileChanged nodes=18 edges=22 deleted_nodes=4 deleted_edges=6
semantic_start id=semanticPass:num:2 file=core/src/daemon/identity.rs requested=1 owed=0
lsp_done language=rust files=1 nodes=9 edges=0 retracted=5 incomplete=false
semantic_done id=semanticPass:num:2 ok=true
```

Graph, the 30 edges into the two declarations before the edit
(`(from file, kind, source)`, all `resolved=1`):

| from | edges | `linkedFrom` |
|---|---|---|
| identity.rs itself (DEFINES/EXPORTS/semantic CALLS) + container DEFINES | 11 | no / yes for same-file semantic CALLS |
| cli/clean.rs (2 semantic CALLS, 3 CALLS, 1 REFERENCES) | 6 | yes |
| storage/connection.rs | 3 | yes |
| tests/it/{cli_clean, cli_clean_sweeping, edit_timing_lines, worktree_daemon_lifecycle}.rs | 10 | yes |

After each removal: the declarations are gone; the 11 same-file edges are gone
(deleted by the plugin's own diff); all **19 cross-file edges (12 CALLS, 2 of
them semantic, 7 REFERENCES) point at their `pending_symbol` placeholder,
`resolved=0`, target node present**. Edges whose `toId` has no node: **0 before,
0 after** (whole index). After each restore: all 30 edges are back on the
declarations, `resolved=1`, including the 2 semantic CALLS from clean.rs - with
no re-extract of clean.rs. `find_references project_hash` after the removal:
"Nothing is named 'project_hash'" (no ghost definition).

### TypeScript: task-tracker-mcp src/domain/taskStatus.ts, `findUnblockedDependents` + `displayStatus`

Timing, first edit of the file in this daemon, then the second:

```
rt_answered id=2 affected=2
rt_committed id=2 method=fileChanged nodes=13 edges=29 deleted_nodes=2 deleted_edges=11
reextract_start id=2 file=src/domain/taskStatus.ts files=0 imports=2 gone_importers=0
reextract_done id=2 n=0 ok=true
semantic_start id=semanticPass:num:2 file=src/domain/taskStatus.ts requested=1 owed=0
...
rt_answered id=4 affected=none            (second removal: path already announced)
```

Graph: 7 cross-file CALLS (board.ts 1, lifecycle.ts 2, plans.ts 1, status.ts 3),
all linked (`linkedFrom` set). After each removal: all 7 on their
`pending_symbol` placeholders, `resolved=0`; dangling edges 0 before and after;
restore relinks all 7.

### Why GM-435 printed `reextract n = None`

GM-435's measure.py takes the count as `rs["kv"].get("n")` from the
`reextract_start` line (line 571 of the scratch script), but that line carries
`files=`/`imports=`/`gone_importers=`; only `reextract_done` has `n=`. Its raw
row for the TS warm-up has `"reextract": 0.0113, "reextract_n": null,
"reextract_n_done": "0"`: the path ran and selected nothing, exactly as here.

## Edit map

None for the product. No behaviour to pin beyond the existing
`deleting_a_node_puts_the_edges_linked_onto_it_back_on_their_placeholders`; the
TS selection rule is pinned by
`an_own_path_file_key_with_nobody_waiting_selects_nothing` /
`a_file_key_that_is_not_the_trigger_still_reads_the_index`
(core/src/daemon/config_reindex/tests.rs).

Docs-only follow-up (owner's call, not done here): correct the GM-435 results
sentence "TS warm-up answered `affected=2` once but no reextract line was
logged" to "re-extract ran and selected 0 files"; and the Q3 premise: removing
a by-name-imported symbol never exercises re-extract by design - a measurement
that wants the re-extract path needs a resolution change (a Rust `mod` line
added/removed, a TS file created where an import was waiting or that shadows
a candidate).

## Cross-file questions and the g-mesh calls that answered them

- Who constructs / consumes `ResolutionDelta`: `find_references
  ResolutionDelta` (answer=files) -> plugins/sdk/src/run.rs,
  core/src/watcher/apply.rs, core/src/daemon/config_reindex.rs,
  core/src/cli/plugin_check/checks.rs; unlinked usages in
  plugins/rust/src/project/{mod,facts}.rs, plugins/typescript/src/project/{mod,facts}.rs,
  plugins/python, extractor mod.rs files.
- Who computes `affected` per plugin: `find_callers source_changed` (8
  candidates: Rust `ProjectContext::source_changed`, TS
  `TsProject::source_changed`, the SDK default, the extractor impls) and
  `find_callers` on `run::Session::source_changed` (by id) ->
  `run::Session::file_changed`.
- Who calls the re-extract path: `find_callers reextract_affected` ->
  `apply_file_change_in` only.
- Who selects importers: `find_callers select_affected` -> `select_with` plus
  8 tests in config_reindex/tests.rs.
- Where a node delete unlinks edges: `find_callers write_diff` ->
  `apply_diff`, `write_diff_unless_empty`.
- `search_code` was tried once and returned nothing (embedding pass not started
  in the fresh worktree index); grep was used for the single-file reads.
