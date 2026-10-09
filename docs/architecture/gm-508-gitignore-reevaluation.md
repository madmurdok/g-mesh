# GM-508: a `.gitignore` edit re-evaluates what is indexed

Status: design, for owner review. No code yet.

Owner decision already made: nested `.gitignore` files at any depth are in
scope, not only the root one.

Code facts below come from g-mesh calls (named inline) on `release-4.3.0`
(`a98f636`), the same code as this branch.

## 1. Today

Four walks decide indexability from `.gitignore`, all at load time:

| Where | Reads | Who uses it |
|---|---|---|
| `plugins/sdk/src/walk.rs` `walker` (270-305) | every directory's `.gitignore`, layered | `--bulk-index` of every SDK plugin (the actual population), TS `TsProject::load` (existence set), Python `ProjectContext::load` |
| `plugins/go/walk.go` `walkDir` | layered `.gitignore` (own Go walk) | Go `--bulk-index` |
| `core/src/project_walk.rs` `project_files` (37-66) | every directory's `.gitignore` | only `cli::status::discover_source_files` and `languages::count_absent_files_in` (`find_callers project_files`, symbol_id of `project_walk::project_files`) |
| `core/src/watcher/mod.rs` `ProjectWatcher::new_inner` (65-150) | **root `.gitignore` only** + `.git/`, `.claude/` | `is_ignored` (152-158), called only by `next_change` (`find_callers is_ignored`) |

None of them reads `.git/info/exclude`, the global excludes file or `.ignore`
(`git_exclude(false)`, `git_global(false)`, `ignore(false)` in all three Rust
walks). A change to `.git/info/exclude` is also under `.git/`, which the
watcher drops. So only `.gitignore` files matter here.

The task's pointer is right and it is a bug of its own (**B0**): the watcher
honours only the root `.gitignore`, the walks honour every level. With
`src/.gitignore` = `gen/`, the bulk walk skips `src/gen/a.ts`, but a save to
it passes `is_ignored`, reaches `PluginRegistry::file_changed` and is indexed.
The index then holds a file the next walk would not.

`ProjectWatcher::new` has two production callers, `daemon::run` and
`ActivationCtx::walk` (`find_callers ProjectWatcher::new`); `next_change` has
one, `daemon::watch_and_route_once` (`find_callers next_change`).

**Example, today.** Root `.gitignore` holds `generated/`. The daemon indexed
the project. The user deletes that line.

- No event arrives for `generated/*.ts`: the files did not change.
- The watcher's matcher still ignores `generated/` until the daemon restarts.
- **A restart does not fix the index either.** `daemon::run` sets
  `needs_walk` only when `schema::bulk_index_completed` is false
  (`core/src/daemon/mod.rs` ~286, `activation.rs` 133), so an indexed project
  is not walked again. After a restart the watcher matcher and the plugins'
  presence sets (TS existence, Python files: rebuilt by `load`) see
  `generated/`, the graph does not. Only `g-mesh reindex` re-walks.
- Reverse edit (add `generated/`): its files stay in the graph until
  `g-mesh reindex`; after a restart the watcher drops their events, so they
  go stale.

So "restart-only" is not today's behaviour; today is "`g-mesh reindex`-only",
and a restart makes the plugins' presence sets disagree with the graph.

How a create/delete reaches plugins today (`find_definition
watch_and_route_once`, `route_settled_path`, `PluginRegistry::file_changed`,
`workspace_file_changed`; SDK `run.rs` read by symbol):
`next_change` -> `Debouncer` -> `classify_settled` (absent = Deleted, no
`indexed_files` row = Created) -> `order_for_routing` -> `route_settled_path`
-> either `workspace_file_changed` (name matches a manifest's `watch_files`)
or `file_changed` -> supervisor -> `fileChanged` frame -> SDK
`Session::file_changed` -> `presence_changed(path, source.is_some())` ->
`Extractor::file_presence_changed`. Creations are first announced in one
`filesCreated` frame (`announce_created` -> `Session::files_created`).
Presence is read **from the disk**, not from `.gitignore`.

## 2. Options

**A. Per-file feed (the task's Option A).** On a settled `.gitignore` change,
walk the affected subtree, diff against the indexed files, route adds as
creations and removes as deletions.
- Adds work through the existing path (`filesCreated` + `fileChanged`).
- Removes do not: the file is still on disk, so `Session::file_changed`
  reads it, applies `present = true` and re-extracts it. A remove needs a new
  wire verb (`fileForgotten`: presence false, `index.remove`, delete diff) in
  `wire`, the SDK and the Go plugin's `control.go`.
- One `fileChanged` round trip plus a per-file semantic pass per file; an
  un-ignored directory of 5,000 files is 5,000 round trips.
- A subtree walk must still apply the ancestors' `.gitignore` files:
  `WalkBuilder` on a subtree with `parents(false)` ignores them, with
  `parents(true)` it reads above the project root too.

**B. Restart-only, documented.** Today does not even satisfy this (section 1).
To keep it, the measurement would be: on a 50k-file project, time of the
gate walk in C (`/usr/bin/time -p`, `uptime`) and of one language's
workspace reindex; B wins only if the reindex blocks the watcher longer than
the owner accepts (Q1). Also needs a fix to the restart path (re-walk on
start when any `.gitignore` is newer than `bulkIndexedAt`).

**C. Gate, then the existing per-language workspace reindex (recommended).**
Treat a settled `.gitignore` (any depth, created, modified or deleted) as a
workspace file for every language whose indexed file set it actually changes:
1. Rebuild the watcher's matcher (layered, which also fixes B0).
2. Gate: walk the project with core's `project_files`, pruned to the
   directories that hold a changed `.gitignore` and their ancestors, split by
   `DiscoveredPlugins::indexing_language`, and compare per language with
   `SELECT filePath FROM nodes WHERE kind='File' AND language=?` restricted to
   the same subtrees.
3. Every language with a non-empty difference goes through
   `PluginRegistry::workspace_file_changed(conn, language, ".../.gitignore")`
   -> `workspace_reindex::run`: `workspaceChanged` (plugin drops its cache and
   re-runs `load_project`, so TS existence and Python files are rebuilt from
   a walk that sees the new rules), a fresh `--bulk-index` into staging,
   plan, one-transaction swap (adds and removes), then the language's
   semantic pass.

Why C over A: every piece after the gate exists and is tested (ADR 0008,
`workspace_reindex.rs` tests); removes need no new wire verb; the swap is
atomic, so queries never see half a re-evaluation; the plugin's model is
reloaded rather than patched, so presence sets match the walk by
construction. Cost: a whole-language reindex for a one-directory change.

## 3. Recommendation

C. Fix B0 inside it (the layered matcher is step 1). A per-file fast path
(A) can be added later behind the same gate if measured reindex time hurts.

## 4. Cost estimate

Measured on this machine, load average 102-112 during the run (so `real` is
inflated; compare `user`):
- `git ls-files -co --exclude-standard` on the g-mesh checkout (stand-in for
  an ignore-honouring walk): **776** files, `real 0.18 user 0.03 sys 0.05`.
  A plain `find` of the same tree: **52,850** files (mostly `target/`),
  `real 10.91 user 0.42 sys 3.19` (waiting on I/O under load, not CPU).
  The gate walk stays on the ignore-honouring side, and pruning to changed
  subtrees makes it smaller still.
- Workspace reindex of Rust on g-mesh, from
  `docs/results/gm-429-semantic-pass-time.md` (bump rows): `workspaceChanged`
  +0.2-0.7 s, bulk index +2.5-3.9 s, swap +3.4-5.3 s; then the semantic pass
  (rust-analyzer) ~35-50 s. Runs on the watcher thread, as a `Cargo.toml`
  edit does today: other edits queue behind it.
- Guard for large un-ignores: language `exclude_dirs` (`node_modules`,
  `dist`, `target`, `vendor`, `.venv`, ...) are applied by the gate and by
  every walk regardless of `.gitignore`, so un-ignoring those adds nothing.
  For anything else: if one language's gate shows more than **10,000 added
  files**, skip the automatic reindex and log one line naming the count and
  `g-mesh reindex` (Q2).

Implementation size, estimated: ~250 lines of production code in core, no
plugin or wire change; tests ~300 lines.

## 5. Edit map

| fn | file | lines | kind |
|---|---|---|---|
| new `IgnoreLayers` (map dir -> `Gitignore` per `.gitignore`, layered `is_ignored`, `reload`) | `core/src/watcher/ignore_layers.rs` (new) | - | code |
| `ProjectWatcher` struct, `new_inner`, `is_ignored`, `next_change`; add `reload_ignores(&self)` (matcher behind `RwLock`) | `core/src/watcher/mod.rs` | 37-47, 65-158, 163-177 | code |
| `watch_and_route_once`: detect settled `.gitignore` names, reload, re-filter the batch, route, then gate + reindex | `core/src/daemon/mod.rs` | 473-505 | code |
| new `gitignore_changed(conn, dirs)` gate | `core/src/daemon/registry.rs` near `workspace_file_changed` | 1069-1085 | code |
| `project_files`: optional subtree pruning (`filter_entry` keeps ancestors and the changed dirs) | `core/src/project_walk.rs` | 37-66 | code |
| reuse unchanged | `core/src/daemon/workspace_reindex.rs` `run` | 178-186 | none |
| watcher tests: nested `.gitignore` drops events (B0); reload after edit | `core/src/watcher/mod.rs` tests | ~195-320 | tests |
| gate unit tests (no diff -> no reindex; diff -> named languages only; guard) | `core/src/daemon/registry.rs` tests or `daemon/tests.rs` | - | tests |
| end to end, TS: un-ignore indexes and resolves; ignore removes; nested level | `core/tests/gitignore_reevaluation.rs` (new, pattern of `files_created_typescript.rs`) | - | tests |

## 6. Behaviour list

1. A save under a directory ignored by a nested `.gitignore` produces no
   routed change (B0).
2. Deleting `generated/` from the root `.gitignore` indexes
   `generated/*.ts` within one debounce window plus the reindex; `find_definition`
   finds their symbols.
3. A TS file importing `../generated/x` resolves after (2): the existence set
   holds `generated/x.ts`.
4. Adding `generated/` removes its files' nodes and edges in one swap, and
   imports of them become unresolved.
5. Same as 2 and 4 with the rule in `src/.gitignore`, and with a
   `.gitignore` created or deleted (not edited).
6. A `.gitignore` edit that changes no indexed file (e.g. `*.log`) runs no
   reindex.
7. Only languages whose files changed are reindexed.
8. Above the guard, no reindex and one log line.
9. A file edited in the newly ignored directory in the same batch as the
   `.gitignore` is not routed; one created in the newly un-ignored directory
   before the settle is indexed (by the reindex's walk).
10. A sleeping plugin is not woken by `workspaceChanged`; it is spawned by the
    reindex and loads its model from the new rules.
11. `.git/info/exclude` and global excludes still change nothing.

## 7. Must-confirm items (implement slice)

- `workspace_file_changed` is safe to call with a non-manifest file name
  (`mark_pending_reindex` stores it; `resume_pending` re-runs it after a
  crash): confirm nothing parses `changed_file` as a manifest.
- `indexed_files` is untouched by the swap (module doc): un-ignored files then
  have no baseline (a later edit is classified Created, harmless); removed
  ones keep a stale row. Confirm GM-498's owed-file logic does not route a
  stale row's file back in.
- The Rust plugin keeps no presence model of its own beyond what `load_project`
  rebuilds; Go has none (`manifest_test.go`, GM-515).
- No path other than `ActivationCtx::walk` and `g-mesh reindex` walks an
  indexed project (section 1's restart claim).
- `File` nodes are written for every walked file, so the gate's live side is
  `nodes WHERE kind='File'`.

## 8. Risks

- **Watcher thread blocked** for the reindex plus semantic pass (~40-55 s for
  Rust on g-mesh), as for any manifest edit today.
- **Spurious gate hits from alias-only files** (GM-514): core's walk does not
  follow links, so a file indexed under a link spelling looks "removed" and
  triggers a no-op reindex on every `.gitignore` edit until GM-514 lands.
- **Directory moves** carrying a nested `.gitignore` emit no event for that
  file on some backends; the matcher stays stale until the next `.gitignore`
  event. Mitigation: also reload the layers when a settled path is a
  directory (cheap: it lists `.gitignore` files only).
- **A `.gitignore` that ignores itself** must not hide its own events: a path
  named `.gitignore` is dropped only when its directory is ignored.
- Two languages reindexed back to back double the stall.

## 9. GM-514 ordering and shared seams

GM-514 (core learns the symlink table) touches `project_walk.rs`,
`core/src/watcher` and `plugins/sdk/src/walk.rs`. Recommended order:
**GM-508 first.** Seams:
- `ProjectWatcher`'s event filter: GM-508 replaces the matcher with
  `IgnoreLayers`; GM-514 remaps real-path events to alias entries *after* that
  filter, so it builds on the new type.
- `project_files`: GM-508 adds subtree pruning; GM-514 may add link following.
  Both change `WalkBuilder` options in one function; GM-514 rebases onto it.
- The gate's alias false positive (section 8) disappears once GM-514's walk
  sees aliases.
- `plugins/sdk/src/walk.rs` is not edited by GM-508.

## 10. Open questions for the owner

**Q1. Is a whole-language reindex an acceptable price for a `.gitignore`
edit?** Today: an edit changes nothing until `g-mesh reindex`. Change: when
the edit changes which files of a language are indexed, that language is
reindexed as after a `Cargo.toml` edit. Example: un-ignoring `fixtures/` with
3 `.rs` files reindexes all of Rust: ~7-10 s structural, then ~40 s semantic
pass, during which other saves queue. Answers: *yes (C)*: correct and atomic,
but slow for tiny changes; *no, per-file (A)*: fast for small diffs, needs a
new `fileForgotten` wire verb in SDK and Go and N round trips for big diffs;
*C now, A as a fast path later if measured*: recommended.

**Q2. What happens when un-ignoring would add a huge number of files?**
Today: nothing is ever added mid-session. Change: a guard. Example: someone
removes `third_party/` (40,000 `.ts` files) from `.gitignore`.
Answers: *guard at 10,000 added files per language, log and require
`g-mesh reindex` (recommended)*: no surprise multi-minute stall, but the user
must act; *no guard*: always consistent, risks a long stall and index growth;
*guard as a ratio (e.g. > 2x the language's files)*: scales with project
size, harder to explain.

**Q3. Should a daemon start also catch `.gitignore` edits made while it was
down?** Today: no, a started daemon never re-walks an indexed project.
Change (optional): at start, compare every `.gitignore`'s mtime with
`bulkIndexedAt` and run the same gate. Example: user edits `.gitignore`, then
opens a new session the next day; without this, the graph is stale until the
first in-session `.gitignore` edit or `g-mesh reindex`. Answers: *include
(recommended)*: one cheap listing at start, closes the gap; *leave out*:
smaller task, the gap stays and needs its own task.
