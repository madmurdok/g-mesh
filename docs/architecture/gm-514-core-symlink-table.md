# GM-514: core learns the plugins' symlink table

Status: design (S1), for owner review. Base: `release-4.3.0` at `a02e43f`
(GM-349, GM-508, GM-521 merged). Inputs: [GM-349 note](gm-349-sdk-walk-symlinks.md)
D2, D4, 2.3, Q3/Q4; [ADR 0025](../adr/0025-project-walk-follows-symlinks.md);
[GM-508 note](gm-508-gitignore-reevaluation.md) section 8 and 9.

Terms. *Alias-only file*: a file the plugin walk reaches only through a link
(its target is gitignored), indexed under the link spelling. *Aliased file*:
reachable both ways, indexed under its real spelling (ADR 0025 "real wins").
Only alias-only files are this task's problem.

## 1. Today

Example. Root `.gitignore` holds `gen/`; `src/api -> ../gen` (in-root link);
`gen/client.ts` exists. The SDK walk follows `src/api` (target in root, not
excluded, not entered by the plain walk) and indexes `src/api/client.ts`.

| Who | What it does with `src/api/client.ts` | Code |
|---|---|---|
| plugin bulk walk | indexes it under `src/api/client.ts` | `plugins/sdk/src/walk.rs` `walk_project_detailed` 156-182, `LinkGuard::finish` 457-509; Go `symlinks.go`, `walk.go` (same policy) |
| core walk | never sees it: `follow_links(false)` | `core/src/project_walk.rs` `walk_builder` 114-150 |
| `g-mesh status` | does not count it (neither discovered nor indexed) | `cli::status::discover_source_files` 558-579 via `project_files` |
| watcher, macOS | edit arrives as `<root>/gen/client.ts` (probed, below); `IgnoreLayers::is_ignored` says ignored (`gen/`) -> dropped in `next_change` 193-207 | `core/src/watcher/mod.rs` |
| watcher, Linux | notify's inotify walks with `follow_links(true)` (`notify-6.1.1/src/inotify.rs` 376) and keeps one path per watch descriptor, the last one added (`paths.insert(w, path)`, 428); `gen/` and `src/api` share an inode, so the edit arrives under either spelling depending on readdir order: real -> dropped, alias -> routed (works by luck) | same |
| GM-508 gate | the indexed `src/api/client.ts` is not in core's walk -> "removed" -> a no-op TypeScript reindex on every `.gitignore` edit whose subtree covers `src/` | `PluginRegistry::gitignore_changed` registry.rs 1143-1190 |

So the file goes stale until `g-mesh reindex`, and status undercounts.

**FSEvents probe** (this slice, macOS 25.6, notify 6.1.1, scratch program, not
committed): a root watched recursively with `src/linked -> ../gen` (gen
gitignored) and `ext -> <outside dir>`:
- write through `src/linked/a.ts` -> one event, `<root>/gen/a.ts` (real path);
- write to `gen/a.ts` -> the same real path;
- write through `ext/b.ts` or directly to the outside file -> **no event**;
- after an explicit `watch(<outside dir>)` -> event under the outside real path.

**Latent Linux bug found here (must-confirm M1).** For an *outside-root* link
the walk refuses, inotify still watches the target through the link and
reports `<root>/ext/b.ts`. Core routes it, and `Session::file_changed`
(`run.rs`) indexes any claimed path it is handed: the index gains a file the
walk refused. macOS is not affected (no event).

Who consumes the SDK's link table today: nobody in production.
`find_references walk_project_detailed` -> `walk_project` (drops `links`),
`walk::tests::detailed`, the `lib.rs` re-export; `find_references
walk::LinkOutcome` -> `lib.rs` and `walk.rs` only.

## 2. Options

### Mechanism (decide 1)

**(a) Plugins report the table over the wire.** `--bulk-index` output gains
`links: [{alias, real}]`; core stores it per language (new table), the watcher
remaps real -> alias from it.
- Plus: core uses exactly the walk that built the index; no guard in core.
- Minus: wire + schema + SDK + Go (+ TypeScript once on the SDK) changes. The
  table is only as fresh as the last bulk index: a link created mid-session
  has no row, so its target's events are still dropped. `status` and the gate
  still need to *walk* the targets (new files, mtimes), so core needs link
  following anyway.

**(b) Core's walk follows links with the SDK guard's rules (recommended).**
Move the guard (`walker`, `LinkGuard`, the winner pass) out of
`plugins/sdk/src/walk.rs` into a new workspace crate `walk/`
(`g-mesh-walk`, depends on `ignore` only); the SDK keeps its public API as
thin wrappers, core depends on the crate. Core's walk then *is* the plugins'
walk minus the extension filter, and the link table falls out of it.
- Plus: status and the gate see alias-only files by construction; the table
  is rebuilt by core whenever the watcher reloads its layers, so it is never
  staler than the `.gitignore` rules; no wire change; Go untouched.
- Minus: an SDK refactor (behaviour must stay byte-identical; the SDK walk
  tests pin it); core's walk gains the guard's cost (ADR 0025: 0.5-1.3 ms per
  walk). Per-language `exclude_dirs` differ from core's walk (see 3.4).

**(b') Copy the guard into core.** A third copy (SDK Rust, Go, core). Rejected:
drift between two Rust copies in one workspace has no excuse.

**(c) Core judges links statelessly**: follow a link iff its target is in root,
not excluded, and ignored by the layers. Equivalent to the guard in the common
case, but differs on duplicates (two links to one ignored target: the guard
keeps the first in sorted walk order) and nested links. A re-implementation,
so rejected for the same reason as (b').

### Event routing (decide 2)

**Remap in `ProjectWatcher::next_change`, before the ignore filter.** The real
spelling of an alias-only file is gitignored by construction, so a remap after
the filter (GM-508 note 9 assumed "after") never sees it. Rule for a reported
path `P`, longest prefix wins:

| `P` lies under | becomes | why |
|---|---|---|
| the real target of a *Followed* link (alias-only) | link spelling + suffix | macOS always, Linux sometimes |
| a *Duplicate* link's spelling | the winning link's spelling + suffix | Linux, last-added inotify spelling |
| an *Aliases* link's spelling | the plain spelling + suffix | Linux; the SDK/Go remap (GM-349 2.3) already does this, core doing it too makes it language-independent |
| a *Refused* link's spelling (outside root, excluded target, dangling) | dropped | Linux ghost files (M1); the walk indexes nothing there |
| none | unchanged | |

Then `is_ignored` runs on the remapped path (the layers are read along the
link spelling, as the walk reads them).

Cases:
- **Gitignored target inside the root** (the only alias-only shape under
  ADR 0025): FSEvents and inotify both watch it already (the watch is the
  whole root, ignored directories included); the remap is all that is needed.
- **Target outside the watched dirs.** With the in-root boundary, a target is
  always inside the root, which is watched recursively. Only the work-tree
  boundary (W1, section 8 Q2) creates targets outside the watch: core must
  then `watch()` each followed outside target (the probe shows FSEvents
  delivers nothing otherwise) and unwatch it when the link goes.
- **Poll fallback** (inotify limit): notify's `PollWatcher` walks with
  `follow_links(true)` (`poll.rs` 259), so it may report both spellings; both
  remap to one, the debouncer dedupes.

**When the table changes.** The table is rebuilt with the layers
(`reload_ignores`). Today's trigger (`reload_ignores_if_changed`: a
`.gitignore` or a directory) misses a deleted link (`is_dir()` false) and a
file link; add "is a symlink, or is a link spelling in the current table". A
link created, removed or retargeted changes indexed files without per-file
events, so its parent directory joins the gate's subtrees (decide 5 / Q4).

### Status (decide 3)

`discover_source_files` keeps calling `project_files`, which now follows links:
alias-only files are discovered under the link spelling, `fs::metadata`
follows the link (same mtime the plugin's baseline records), and `indexed`
matches the index row. Aliased files collapse onto the real spelling (winner
pass), as in the plugins. `count_absent_files_in` gets the same walk.

### Work-tree boundary (decide 4)

- **W0. Keep the root as the boundary.** Nothing new is followed.
- **W1. Widen to the enclosing git work tree** (nearest ancestor with `.git`,
  file or directory) in every walk: SDK, Go, core. Core watches each followed
  outside target. Serves "sub-project opened as root links a sibling"
  (`apps/web/shared -> ../../libs/shared`); still refuses bazel/nix/`$HOME`
  targets (outside the work tree).
- **W2. W1 behind a project setting**, off by default.

Risks of W1/W2 (why W0 is recommended): (1) the language servers answer in real
paths, and `semanticPass`'s path mapping strips the root, so every semantic
edge into an outside file is lost until that mapping learns the table (GM-324
territory); (2) notify's FSEvents backend restarts its stream on every
`watch()`, so each link change risks a short event gap; (3) the target's
`.gitignore` context differs from git's (layers above the target are not read
along the link); (4) Go's guard changes too; (5) W2 adds a setting ADR 0025
argued against (the tree should not depend on configuration).

### GM-508's gate (decide 5)

With (b) the gate's walk lists alias-only files, so the false "removed"
disappears. One new hazard: `project_files_under` prunes directories outside
the subtrees, so the guard can miss the plain spelling of a target and call an
*aliasing* link *alias-only* (link inside the subtree, target outside it and
not ignored) -> spurious "added". Fix: if the pruned walk follows any link,
redo it unpruned and filter the output to the subtrees. Projects without
followed links keep GM-508's pruning; with them, the gate costs one full walk.

## 3. Recommendation

(b) + remap before the filter + W0. Concretely:
1. **3.1 Crate.** `walk/` (`g-mesh-walk`): `walk(root, excluded) -> Walk {
   files: Vec<WalkedEntry { path, relative, real_relative: Option<String> }>,
   links: Vec<Link { at, real, outcome }> }` plus a directories-only mode for
   the layers. Sorted (`sort_by_file_name`) as the SDK walk is, so "first in
   walk order" agrees. The SDK's `walk_project`, `walk_project_detailed`,
   `walk_scope` become wrappers; `LinkOutcome` unchanged.
2. **3.2 Core walk.** `project_files`/`project_files_under` use it (excluded =
   baseline + `pruned`); `WalkedFile` gains `real_relative`.
3. **3.3 Table.** `LinkTable` (in `project_walk.rs`) built from `Walk::links`;
   `IgnoreLayers::load` collects layers and table in its one walk;
   `ProjectWatcher` keeps both under the existing `RwLock`.
4. **3.4 Per-language excludes.** Core's walk prunes only the names *every*
   language excludes, so it can follow a link a language refuses
   (`src/dep -> ../node_modules/foo`, gitignored). `indexing_language` gains
   the real path: a file is not language L's if either spelling is under L's
   `exclude_dirs` (ADR 0025's ExcludedTarget, per language). Routing gets the
   real path with one `canonicalize` (as `Session::real_spelling` does); the
   walks carry `real_relative`.
5. **3.5 Gate.** Link changes feed the gate; the pruned walk falls back as in 2.
6. Go, wire, schema: unchanged. Docs: ADR 0025 Consequences and the
   `project_walk` module doc ("diverges only toward doing less") are rewritten.

## 4. Edit map

Lines are 1-based on `a02e43f`.

Code:

| File | fn (lines) | Change |
|---|---|---|
| `Cargo.toml` | `[workspace] members` | add `walk` |
| `walk/` (new) | `Cargo.toml`, `src/lib.rs` | guard, walker, winner pass moved from the SDK; `real` carried per link and per file |
| `plugins/sdk/src/walk.rs` | `walk_project_detailed` 156-182, `walk_scope` 213-251, `walker` 270-305, `LinkGuard` + `impl` 310-509, `innermost` 513-526 | wrappers over `g-mesh-walk`; module doc points at the crate |
| `plugins/sdk/Cargo.toml`, `core/Cargo.toml` | deps | `g-mesh-walk = { path = ... }` |
| `core/src/project_walk.rs` | module doc 1-17, `WalkedFile` 26-32, `project_files` 37-39, `project_files_under` 47-55, `walked_files` 57-69, `gitignore_files` 73-88, `project_walk_builder` 106-108, `walk_builder` 114-150 | follow links via the crate; `real_relative`; `LinkTable` + `to_indexed(&Path) -> Remap { Keep, To(PathBuf), Drop }`; pruned-walk fallback |
| `core/src/watcher/ignore_layers.rs` | `load` 37-63 | one walk yields layers and `LinkTable` |
| `core/src/watcher/mod.rs` | `ProjectWatcher` 41-54, `next_change` 193-207, `reload_ignores_if_changed` 171-180, `reload_ignores` 161-164 | remap before `is_ignored`; reload on a symlink or a tabled link path; return the changed link spellings |
| `core/src/daemon/mod.rs` | `watch_and_route_once` 483-529 | link changes join `gitignores` as gate subtrees |
| `core/src/daemon/registry.rs` | `gitignore_subtrees` 327-338, `gitignore_changed` 1143-1190, `file_changed` 1081-1105 | gate takes link subtrees; routing passes the real path to `indexing_language` |
| `core/src/daemon/manifest.rs` | `indexing_language` 593-599 | real-path exclusion (3.4) |
| `core/src/cli/status.rs` | `discover_source_files` 558-579 | real-path exclusion; doc comment |
| `core/src/languages.rs` | `count_absent_files_in` 223-254 | real-path exclusion |
| `docs/adr/0025-...md`, `docs/architecture/...` | | Consequences: alias-only files counted and refreshed |

Tests: `walk/` gets no new behaviour tests (the SDK walk tests stay in the SDK
and pin the move); new tests in `core/src/project_walk.rs`,
`core/src/watcher/mod.rs` tests (210-568), `core/src/cli/status/tests.rs`,
`core/src/daemon/registry/tests.rs`, and one core integration test with the
toy plugin (`g-mesh-plugin-toy`) for B1/B2.

## 5. Behaviour list (tests slice; each with its control)

macOS = FSEvents, Linux = inotify (CI). "Both" means one test, run on both.

| # | Behaviour | OS | Control (revert the fix) |
|---|---|---|---|
| B1 | edit `gen/a.ts` (real) of alias-only `src/api/a.ts` -> one `fileChanged` for `src/api/a.ts`, index holds one row, under the alias | both | remap removed from `next_change` -> macOS: no event; Linux: test forces the real spelling via `ProjectWatcher` fed a real path (unit) |
| B2 | delete `gen/a.ts` -> `src/api/a.ts` removed | both | same revert -> row survives |
| B3 | create `gen/b.ts` -> indexed as `src/api/b.ts` | both | same revert |
| B4 | `status`: alias-only file discovered and indexed, `dirty` 0 | both | `follow_links(false)` in core's walk -> discovered short by one |
| B5 | aliased file (`lib/x.ts`, `src/l -> ../lib`) counted once, real spelling | both | winner pass skipped in core -> counted twice |
| B6 | gate: `.gitignore` edit unrelated to `gen/` -> no reindex (GM-508 false positive gone) | both | `follow_links(false)` -> reindex logged |
| B7 | gate: pruned walk, link inside subtree to a non-ignored target outside it -> no reindex | both | fallback removed -> spurious reindex |
| B8 | link created mid-session -> its language reindexed, alias files indexed; link removed -> they are removed | both | link paths not fed to the gate -> index unchanged |
| B9 | un-ignoring `gen/` -> files move from `src/api/*` to `gen/*` | both | table not rebuilt on reload -> later `gen/` events remapped to the stale alias |
| B10 | `src/dep -> ../node_modules/foo` (gitignored): TS file under it not routed to TS, not counted by status for TS | both | real-path exclusion removed -> routed/counted |
| B11 | Linux: event spelled through an outside-root link -> dropped, index unchanged (M1) | Linux | `Drop` arm removed -> ghost row |
| B12 | Linux: event spelled through the Duplicate link -> the winner's spelling | Linux | Duplicate arm removed -> second row |
| B13 | nested `.gitignore` inside the target edited -> layers reload, gate subtree is the alias directory | both | remap removed -> event dropped as ignored |

Controls to run (6-8): B1, B4, B5, B7, B8, B10, B11, B13.

## 6. Must-confirm items (implement / tests slice)

- **M1.** Linux: an edit inside an outside-root link target reaches the plugin
  today and is indexed (the ghost row). Reproduce on CI before B11 is written;
  if inotify does not report it, B11 becomes a guard-only unit test.
- **M2.** Linux: which spelling inotify reports for an alias-only file (last
  added watch). B1 must pass for either; assert on the index, not the event.
- **M3.** The SDK walk tests pass unchanged after the move to `walk/`
  (`cargo nextest -p g-mesh-plugin-sdk walk::`), and Go's walk tests are
  untouched.
- **M4.** Cost: `IgnoreLayers::load` with link following on g-mesh and one
  corpus, `/usr/bin/time -p` with `uptime`; it runs on every settled
  directory (GM-508).
- **M5.** The status mtime of an alias file equals the plugin's recorded
  baseline (both `fs::metadata`, which follows the link).
- **M6.** `classify_settled` gets the remapped absolute path, so "absent"
  means the alias no longer resolves (target file or link gone).

## 7. Risks

- **Refactor blast radius**: every Rust plugin links the moved guard. The SDK
  walk tests and the conformance kit are the net.
- **Table staleness inside one batch**: events for a new link's target that
  arrive before the reload are dropped as ignored; the gate (B8) reindexes the
  language, so nothing is lost, at whole-language cost.
- **Whole-language reindex per link change** (GM-508 cost: ~7-55 s for Rust on
  g-mesh). Link changes are rare; same guard (10,000 files) applies.
- **Per-event cost**: one longest-prefix scan over the table (links only; no
  syscall) in `next_change`; one `canonicalize` per routed file in
  `file_changed`.
- **Gate fallback**: projects with followed links lose GM-508's subtree pruning
  (one full walk per `.gitignore` edit).
- **Go policy copy** remains the second implementation; drift is caught only by
  the parallel test names (ADR 0025).

## 8. Open questions for the owner

**Q1. Mechanism: shared walk crate (b) or a wire table (a)?**
Today core's walk does not follow links; only the plugins know the table.
Change (b): the SDK's guard moves into a crate core also uses; core follows
links itself. Example: the user creates `src/api -> ../gen` mid-session; under
(b) core rebuilds its table on the directory event and reindexes; under (a)
core has no row for it until the next bulk index, so `gen/` edits stay
dropped. Answers: *(b) (recommended)*: status, watcher and gate agree by
construction, no wire change; risk is an SDK refactor. *(a)*: no guard in
core, but stale for new links, and status/gate still need a following walk.

**Q2. Boundary for following links: the project root or the git work tree?**
Today a link whose target is outside the project root is refused by every walk.
Change: W1 accepts targets inside the enclosing git work tree. Example: the
user opens `apps/web` as the project; `apps/web/shared -> ../../libs/shared`.
Answers: *W0, root (recommended)*: no change; `shared/` stays unindexed, no
watch-set or semantic-path risk. *W1, work tree*: `shared/*.ts` indexed under
`apps/web/shared/`, core watches `libs/shared` (FSEvents needs it), but
semantic edges into those files are lost until the language-server path
mapping learns the table, and Go's guard changes too. *W2, W1 behind a
setting*: same as W1 for those who enable it; adds a setting ADR 0025 argued
against.

**Q3. Should core also normalise aliased and duplicate spellings, and drop
events under refused links?** Today on Linux an event can arrive under any
link spelling; the SDK and Go remap aliased spellings to the real one, nobody
handles a duplicate spelling or a refused (outside-root) link. Example: on
Linux `ext -> /opt/shared`, an edit to `/opt/shared/b.ts` arrives as
`ext/b.ts` and is indexed although the walk refused it (M1). Answers: *yes
(recommended)*: one rule in core for every language, fixes the ghost row;
the plugin remaps stay as a second line. *No, alias-only only*: smaller; the
Linux ghost row needs its own task.

**Q4. A link created or removed mid-session: reindex its language?** Today
nothing happens until `g-mesh reindex`. Change: the link's directory joins
GM-508's gate, which reindexes each language whose indexed files it changes.
Example: `ln -s ../gen src/api` with 300 `.ts` files under `gen/`: TypeScript is
reindexed (seconds, then its semantic pass). Answers: *yes (recommended)*:
consistent with GM-508's `.gitignore` handling; risk is a whole-language
reindex per link change. *No*: the link is only picked up by `g-mesh reindex`;
until then its target's edits are remapped to files the index does not hold,
and plugins index them one by one as they are edited.

## 9. Owner decisions (2026-10-09)

Owner asked first: "Не является ли это регрессией? мы ведь наоборот хотели
вынести языко-специфичные вещи в плагины?" Answer given: link following is
filesystem policy, not language logic; (b) moves the guard into a shared crate
both use (no third copy); per-language `exclude_dirs` stay plugin-declared in
manifests. Then:

- Q1: "Общий crate обхода (Recommended)" -> (b).
- Q2: "Корень проекта, W0 (Recommended)" -> boundary unchanged.
- Q3: "Да, одно правило в ядре (Recommended)" -> core remaps duplicates and
  drops events under refused links (B11, B12 in scope).
- Q4: "Да, как .gitignore в GM-508 (Recommended)" -> link changes feed the gate (B8).

## Appendix: g-mesh calls this note relied on

| Question | Call | Answer |
|---|---|---|
| Callers of core's walk | `find_callers project_walk::project_files` | `languages::count_absent_files_in`, `cli::status::discover_source_files` (complete) |
| Callers of the subtree walk | `find_callers project_walk::project_files_under` | `PluginRegistry::gitignore_changed` |
| Callers of the shared builder | `find_callers project_walk::project_walk_builder` | `gitignore_files`, `IgnoreLayers::load` |
| Settled-path routing | `find_callers route_settled_path` | `daemon::watch_and_route_once` (production); workspace_reindex/registry tests; `core/tests/typescript_registry` `Harness::route` |
| Who reads the SDK link table | `find_references walk_project_detailed`, `find_references walk::LinkOutcome` | only `walk_project`, walk tests, `lib.rs` re-export: no production reader |
| Status path | `find_callers discover_source_files`, `find_definition index_status` | `index_status` compares walk vs `indexed_files` |
| Gate, routing, layers | `find_definition` `watch_and_route_once`, `gitignore_changed`, `route_settled_path`, `IgnoreLayers::load`, `IgnoreLayers::is_ignored`, `count_absent_files_in` | read for the edit map |
| Layers loaders | `find_callers IgnoreLayers::load` | `new_inner`, `reload_ignores` (one untyped receiver in `ipc/windows.rs`, unrelated) |
| Outlines | `get_file_outline` `project_walk.rs`, `watcher/mod.rs`, `ignore_layers.rs`, `plugins/sdk/src/walk.rs` | line ranges above |

grep/sed were used for: the notify crate sources (`inotify.rs`, `poll.rs`),
`indexing_language`/`under_excluded_dir` line numbers, `Cargo.toml` files, and
the SDK `run.rs` remap (read by line after a grep).
