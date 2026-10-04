# GM-350: where TypeScript's filesystem-dependent module resolution lives

Decision record: [ADR 0023](../adr/0023-project-model-tracks-file-presence.md).
This note carries the reasoning, the measurements, the test classification
and the restated scope of GM-324 (the port of the TypeScript structural tier
to Rust on `plugins/sdk`). It decides nothing about symlinks (GM-349) and
classifies tests in a form GM-323 can reuse, without doing GM-323's job.

## 1. The problem

The TypeScript plugin resolves an import specifier (`./b`, `@scope/pkg/sub`,
`#private`, a tsconfig `paths` alias) to the project file it names. To do
that it needs facts that live on disk, not in the file being extracted:

| Fact | Where it comes from today |
|---|---|
| Which candidate files exist (`b.ts`? `b.tsx`? `b/index.ts`? `.js` → `.ts` substitution) | `fs.statSync` per candidate, `createProjectFileExists` (`plugins/typescript/src/resolve.ts`) |
| Whether that file is indexable (not under `dist`/`node_modules`, not gitignored) | `createIndexabilityChecker` (`ignorePolicy.ts`) |
| Workspace packages (pnpm-workspace.yaml, `workspaces` in package.json, yarn object form, globs with `!` negation) | `readWorkspacePackages` (`workspace.ts`) |
| package.json `exports` / `imports` condition maps, wildcard subpaths, `#private` scoped to the nearest package.json | `workspace.ts`, `createPackageImportsIndex` |
| tsconfig/jsconfig `paths`, `baseUrl`, JSONC, `extends` chains | `createTsconfigPathsIndex` (`tsconfigPaths.ts`) |
| Symlinked workspace packages | `symlinks.ts` |

`extract.ts` keeps the extractor itself pure by taking these through an
injected `SpecifierResolver` ("the resolution policy is filesystem-dependent,
this walk is not"). The important detail is the **lifetime** of that
resolver: `reparseChangedFile` (`incremental.ts`) builds a fresh
`createProjectResolver(projectRoot)` **for every changed file**, and
`resolve.ts`'s own doc says why: "the long-lived plugin builds a fresh one per
changed file, so a file created since the last edit is seen". The same is true
of tsconfig and package.json: they are re-read per reparse. That is why the
TS manifest declares `watch_files = []` (`plugins/typescript/plugin.toml`):
nothing is cached, so nothing needs invalidating.

The SDK the port targets forbids this shape. `plugins/sdk/src/lib.rs`, the
`Extractor` contract: `extract` "is **pure**: it reads `source` and the project
model, and nothing else. No filesystem access, no clock". Only
`load_project` may read other files, and the SDK calls it once per process
(bulk walk) or again only on `workspaceChanged`
(`plugins/sdk/src/run.rs`, `Session::handle`). So every disk fact above has to
become part of `E::Project`, and the question is how that model stays true
while the daemon runs.

## 2. Measured facts the decision rests on

All cheap to measure, so measured rather than assumed.

**M1. Size of a file-existence set (excalidraw corpus).** The bench's
excalidraw corpus (`g-mesh-bench/corpora/registry.json` → local checkout
`/Users/Valentin_Taiurskii/Projects/excalidraw` at `1acf66ed`):

| What | Count |
|---|---|
| Files outside `node_modules`/`.git`/`dist`/`build`/`coverage` | 1,265 |
| TS-family files (`.ts .tsx .js .jsx .mjs .cjs .mts .cts`) - the existence set | **658** (all git-tracked) |
| Bytes of those 658 paths | 29,361 (~29 KB) |
| Resolution config files (`package.json`, `tsconfig*.json`, `jsconfig*.json`, `pnpm-workspace.yaml`) | 24 (incl. `packages/tsconfig.base.json`) |

Holding the set in memory is free. It is exactly the bulk walk's file list:
the Node plugin's own bulk walk of the same checkout reported
`658 files, 16650 nodes, 27064 edges`.

**M2. How often the events happen (excalidraw git history, last 1,000 commits).**

| Commit kind | Commits | Share |
|---|---|---|
| Touches a TS-family file | 843 | 84% |
| **Adds** a TS-family file | 157 | 16% |
| Touches a resolution config file | 124 | 12% |

File-level: 451 TS-family additions vs 6,272 modifications. A file
creation - the event a load-once existence set misses - is in one commit in
six. Config edits are about as frequent, and most are dependency bumps that
change no resolution.

**M3. `fileChanged` vs `workspaceChanged` in this machine's daemon logs**
(`~/.g-mesh/projects/*/daemon.log*`, all projects). Neither event is logged
per occurrence, so this is a lower bound from what is logged: 594
`fileChanged` frames were replayed through 39 plugin wakes alone (live,
awake-plugin `fileChanged` frames are not logged at all), against 101
workspace reindexes (all `rust`, i.e. `Cargo.toml` saves; no other language
declares watch files used in these projects). The ratio is at least ~6:1 and
in reality far higher. The point it supports: `fileChanged` is the hot path,
`workspaceChanged` is rare and expensive (a whole-language walk into a staging
index, ADR 0008).

**M4. Cost of a whole-language TS reindex (what a `watch_files` hit costs).**
The current Node plugin's `--bulk-index` on excalidraw: `real 24.53 user 19.68
sys 1.23` (`/usr/bin/time -p`), with `uptime` load averages 83.7/129.1/89.3 -
a heavily loaded machine, and `user ≈ real` means the time was CPU work, not
waiting. Plugin side only (core's link, plan and swap are extra). The Rust
port's number will differ; treat this as an upper reference, not a target.

**M5. The other plugins already live with a load-once file set.** Not
measured but read, and decisive (section 4): Python's `ProjectContext`
(`plugins/python/src/project/mod.rs`) walks the files at `load` and its module
doc states the consequence in so many words - "a module created since the
last `load` ... is answered `false`, so the extractor emits an
`external_module` node and the import simply does not link". Rust's
`ProjectContext` (`plugins/rust/src/project/mod.rs`) scans the module tree at
`load`; `container_for` answers `Orphan` for any file not in it, so a module
file added mid-session is an orphan until the next `Cargo.toml` save.

## 3. Options

### Option 1 - the project model computes everything once; `watch_files` grows

`load_project` computes the existence set (from the SDK walk), workspace
layout, tsconfig `paths`, `exports`/`imports` maps. Extraction consults only
that. `watch_files` gains the resolution config files so a config edit
reloads the model (by core's whole-language reindex).

- Keeps the SDK contract unchanged. Matches Python and Rust.
- **Regression, named:** a TS file created after the plugin process loaded its
  model does not exist as far as resolution is concerned, until the next
  config save or plugin restart. TypeScript has no such window today.
  What a user sees: create `b.ts`, then edit `a.ts` to `import { f } from
  "./b"` and call `f()`. The import becomes a raw-specifier placeholder, so
  - `get_dependencies` on `a.ts` does not list `b.ts`, nor `b.ts`'s dependents
    list `a.ts`;
  - `find_callers`/`find_references` on `f` miss the call in `a.ts`;
  - `find_definition` from the call in `a.ts` cannot follow the import.
  No answer is *wrong*, every one is *short*, and nothing says so. The window
  stays open until a config-file save or a plugin restart, i.e. for most
  of a working session. By M2 it opens in roughly one commit in six.

### Option 2 - the project model is updated on `fileChanged`, not only on `workspaceChanged`

The SDK tells the extractor's project model when a claimed file appears or
disappears, on the `fileChanged` it already receives for that file. The model
is otherwise built the same way as in option 1, config files included.

- Preserves today's behaviour for the common case (create a file, then import
  it) and closes the same gap for Python as a side effect if Python opts in.
- Changes a contract the SDK plugins depend on - additively (section 5).
- Leaves two narrower windows (section 6).

### Option 3 - relax the purity contract for resolution

Let `extract` (or a resolver held in `E::Project`) stat the disk, as the Node
plugin's injected resolver effectively does.

- Rejected. The purity rule is not style: `id-stability.bulk-repeat` and
  `id-stability.whitespace-edit` in the conformance kit check it, the bulk
  walk extracts in parallel against one `&Project`, and the SDK's
  "identical text cannot produce a different graph from a pure extractor"
  short-circuit in `Session::file_changed` depends on it. A disk-reading
  extractor's output depends on *when* it ran, so two walks of an unchanged
  tree are no longer comparable while files are being written (a `git
  checkout` mid-walk). It would also need a cache with an invalidation rule
  (the Node plugin's per-reparse memo is exactly that), which is option 2
  without the contract being written down. The one thing it buys - no
  ordering window (section 6, W1) - is not worth a contract every other plugin
  is held to.

## 4. Decision

**Option 2**, in an additive form: one new `Extractor` method with a no-op
default, called by the SDK's control plane on every `fileChanged`. Resolution
config files take option 1's route: they go into `watch_files`, so a config
edit is a whole-language reindex.

Why option 2 over option 1, in one line: file creation is common (M2: 16% of
commits) and option 1 turns it into a silent, session-long under-report in the
one language the bench measures, while option 2 costs one defaulted trait
method that the other plugins can ignore.

The tie-breaker is the batch's stated direction (GM-323's amendment, 2026-09-20:
"where a behaviour exists in TypeScript and not in the other plugins, the
default is now to raise the others rather than drop it"). Option 1 levels
TypeScript down to Python's and Rust's documented gap (M5). Option 2 keeps
TypeScript's behaviour and gives Python a hook to gain it.

Why config files go through `watch_files` and not through the new hook: a
tsconfig `paths` or package.json `exports` edit changes the resolution of
*every importer already indexed*, and only re-extracting them corrects that.
The whole-language reindex (ADR 0008) does exactly that, swapping in only the
difference. That is **better than today**: the Node plugin re-reads configs
per reparse, so after a config edit every importer stays resolved against the
old config until someone edits it. The cost is M4 per config save, at roughly
M2's rate (12% of commits), most of them dependency bumps that change nothing.

## 5. The contract change

### 5.1 The new method

```rust
pub trait Extractor: Send + Sync {
    // ... LANGUAGE, Project, load_project, extract unchanged ...

    /// Tells the project model that `path`, a file this plugin claims, is
    /// present on disk (`present == true`) or gone (`false`), as of the
    /// `fileChanged` core just sent for it. Called before that file is
    /// extracted, so a model that tracks file presence sees the file it is
    /// about to extract.
    ///
    /// Idempotent and cheap: it is called on every `fileChanged`, not only on
    /// creations and deletions, because the SDK does not know which files the
    /// model has seen. Infallible: a model that cannot absorb the change keeps
    /// its previous state, which is what a plugin that does not implement this
    /// gets anyway.
    ///
    /// The default does nothing, which is the behaviour every SDK plugin had
    /// before this method existed.
    fn file_presence_changed(&self, project: &mut Self::Project, path: &RelPath, present: bool) {
        let _ = (project, path, present);
    }
}
```

Exact naming is GM-324's; the semantics are this note's.

### 5.2 Where the SDK calls it (`plugins/sdk/src/run.rs`)

- `Session::file_changed`, after the `claims` check and the `read_source`:
  `present = read_source(..).is_some()`. Called before the identical-text
  short-circuit, so a model sees presence even when the text is unchanged,
  and before `extract`. On `present == false` it is called before the diff
  that removes the file.
- `Session::hydrate`, for each file it hydrates (cheap, and keeps a file
  first seen by a semantic pass consistent with one first seen by
  `fileChanged`).
- **Not** in `bulk_index`: a one-shot process whose `load_project` already
  saw every file.
- Only when `self.project` is `Some`. With no model there is nothing to
  update; the next successful `load_project` sees the disk anyway.

### 5.3 Contract text that changes in `lib.rs`

- `Extractor::Project`'s doc ("Rebuilt from scratch whenever core sends
  `workspaceChanged`") gains: "and told about each claimed file's presence on
  `fileChanged` (`file_presence_changed`)".
- `extract`'s determinism clause ("given a fixed project model") stays true
  as written and now matters: between two `fileChanged` frames the model is
  fixed; across them it may not be.
- The module doc's "Relationship to the TS plugin" ("is not being ported onto
  this crate") is stale once GM-324 lands and must be rewritten there.
- `Session::file_changed`'s identical-text short-circuit ("identical text
  cannot produce a different graph from a pure extractor") is no longer the
  whole truth: identical text against a changed model can. It stays correct
  as behaviour (it is the same as the Node plugin's, see W2) but its comment
  should say why it is kept.

The control loop is single-threaded (`Session::handle`), and the bulk walk
never calls the hook, so `&mut Self::Project` has no sharing problem.

### 5.4 Checked against all four plugins

Found with g-mesh: `find_implementations` on `Extractor`
(`plugins/sdk/src/lib.rs`, symbol id `26c82aff…`) returns exactly
`RustExtractor` (`plugins/rust/src/extractor/mod.rs`), `PythonExtractor`
(`plugins/python/src/extractor/mod.rs`), `ToyExtractor`
(`plugins/sdk/toy/main.rs`) and three test extractors in `run.rs`. Go and
TypeScript do not implement it: each has its own runtime.

| Plugin | Runtime today | How it loads and uses its project model | `watch_files` | Effect of the change | What it must do |
|---|---|---|---|---|---|
| **rust** | SDK | `ProjectContext::load`: crates from `Cargo.toml`, module tree scanned from `mod` items on disk (`module_tree::scan_crate`), `container_keys`. Reloaded only on `workspaceChanged`. | `["Cargo.toml"]` | None (default no-op). | Nothing. Its own gap (a new module file is an `Orphan` until a `Cargo.toml` save) is about `mod` items, which presence alone does not fix (the parent's `mod foo;` edit is the event). Named as owner question Q3, not fixed here. |
| **python** | SDK | `ProjectContext::load`: `walk_project` over `.py`/`.pyi`, roots from `pyproject.toml`, `files` + ancestor `containers` set; `has_container` answers imports. Reloaded only on `workspaceChanged`. | `["pyproject.toml", "setup.cfg", "setup.py"]` | None (default no-op). | Nothing required. *Can* opt in: insert/remove `container_for(path)` and its ancestor keys, closing its documented "module created since the last load" gap (M5). Owner question Q2. |
| **go** | Own Go binary (`plugins/go/control.go`), not the SDK | `loadWorkspace` reads `go.mod`/`go.work` module roots only; import paths are a function of the directory; no file set. `reloadWorkspace` on `workspaceChanged`. | `["go.mod", "go.work"]` | None: does not use the trait. | Nothing. It has no existence-dependent resolution, so it has no window to close. |
| **typescript** (after GM-324) | SDK (structural); Node semantic tier unchanged until its own task | `load_project`: existence set from the SDK walk, workspace packages, tsconfig index, imports/exports maps (section 7). | grows, section 6.2 | Implements the hook: insert/remove `path` in the existence set. | Section 7. |
| typescript (today, until GM-324 lands) | Node | Fresh resolver per reparse; no cached model. | `[]` | None. | Nothing. |
| sdk toy + `run.rs` test extractors | SDK | `()` | n/a | None (default). | Nothing. |

The wire protocol and core are unchanged: `fileChanged` already reaches the
plugin for every claimed path, creations and deletions included
(`PluginRegistry::route_settled_path` → `file_changed`, by extension), and
already skips paths under the language's `exclude_dirs`
(`core/src/daemon/registry.rs`). The conformance kit is unaffected: its
id-stability checks run bulk walks, which never call the hook.

## 6. What remains open, and what a user sees

### 6.1 Residual windows

**W1 - burst ordering (a narrow regression, named).** If one debounced batch
contains both the creation of `b.ts` and an edit of `a.ts` that imports it,
and core routes `a.ts` first, `a.ts` is extracted before the model knows
`b.ts` exists. The import stays a placeholder until `a.ts` is next edited.
Today's Node plugin stats the disk at `a.ts`'s reparse and resolves it. This
happens on bursts (`git checkout`, `git pull`, a code generator), where the
order of a drained `Debouncer` batch decides. Symptom as in option 1, but
limited to importers routed ahead of their target in the same burst, and
closed by the importer's next edit. A fix is cheap and outside this decision:
core routes a batch's creations before its modifications, or re-sends the
importers. Owner question Q1.

**W2 - importers indexed before their target existed (not a regression).**
`a.ts` imports `./b` while `b.ts` does not exist; later `b.ts` is created.
`a.ts` is not re-extracted, so its edge stays unresolved until `a.ts` is
edited. The Node plugin behaves identically today (it re-resolves only the
file being reparsed). Unchanged, stated so it is not mistaken for new.

**W3 - config edits.** A save of a watched config file runs a whole-language
reindex (ADR 0008): queries see the complete old TS graph until the swap and
the complete new one after it, at M4's cost per save. Today there is no
reindex and importers stay stale until edited, so correctness improves; the
CPU cost is new. A config file whose name the globs below do not match (a
tsconfig `extends` target called `base.json`) is not watched: after editing
it, resolution follows the old content until another watched file is saved or
the plugin restarts. Today it is re-read on the next reparse. Narrow
regression, named.

**W4 - `.gitignore` edits.** Whether a path is indexable depends on
`.gitignore`. Today the Node plugin re-reads it per reparse. After the port
the existence set is built at load from the SDK walk, and the hook is only
called for paths core routes, which core's watcher already filters through
`.gitignore`. A file that becomes ignored or un-ignored without its own
change event is not re-evaluated until reload. Core's own file population has
the same question; not this note's to answer (Q4).

### 6.2 `watch_files` for the TypeScript manifest

```toml
[plugin.workspace]
watch_files = ["package.json", "tsconfig*.json", "jsconfig*.json", "pnpm-workspace.yaml"]
```

- `package.json`: workspace `workspaces`, `exports`, `imports`, entry
  fields, and the "nearest package.json" scope of `#private`.
- `tsconfig*.json` / `jsconfig*.json`: `paths`, `baseUrl`, `extends`. The glob
  also covers `tsconfig.base.json`-style `extends` targets (M1:
  `packages/tsconfig.base.json` on excalidraw), not arbitrary names (W3).
- `pnpm-workspace.yaml`: workspace globs.
- Not `.gitignore` (W4) and not lockfiles (they move no resolution).

Matching is by file name in any directory, and core already drops matches
under the language's `exclude_dirs` (`workspace_language_matches`,
`core/src/daemon/registry.rs`), so `npm install` writing package.json files
under `node_modules` reindexes nothing. A watched file is routed as a
workspace event *instead of* as a source file (`route_settled_path`); none of
these is a TS source file, so nothing is lost.

## 7. GM-324's scope, restated

**GM-324 builds**

- `E::Project` for TypeScript: the existence set (from `walk_project` with
  the manifest's extensions and `exclude_dirs`, so it is by construction the
  set of files the bulk walk indexes), workspace packages, tsconfig/jsconfig
  `paths` index with JSONC and `extends`, package.json `exports`/`imports`
  maps with conditions and wildcards, `#private` scoped to the nearest
  package.json. All of it read in `load_project`; no disk access in
  `extract`.
- The resolver as a pure function of (specifier, importer path, `&Project`),
  the Rust counterpart of `createSpecifierResolver` with an in-memory
  `FileExists`.
- `file_presence_changed` for TypeScript: insert/remove in the existence set.
- The SDK change of section 5: the trait method with its default, the two
  call sites in `run.rs`, the doc updates of 5.3, and an SDK test that a
  model sees a created file before that file is extracted (with its control:
  remove the call, the test fails).
- The manifest's `watch_files` (6.2), and a test that a config save reaches
  the TS reindex the way `a_glob_watch_files_pattern_triggers_the_same_reindex`
  does for its fixture.
- A plugin-level test of the decision itself: in one control-plane session,
  create `b.ts`, then `fileChanged` an `a.ts` importing it; the import
  resolves. Control: make the hook a no-op; it must not resolve.

**GM-324 inherits (does not re-decide)**

- Placement of resolution: this note.
- `.gitignore` and hard-excluded-directory semantics: the SDK walk
  (`plugins/sdk/src/walk.rs`), which already layers `.gitignore` as git does
  and excludes by `exclude_dirs`. `ignorePolicy.ts` is not ported.
- Symlinks: GM-349. Whatever the SDK walk decides there, the existence set
  follows, because it *is* the walk's output. A side effect worth recording:
  the known gap in `createProjectFileExists`'s doc (task 87: a path reachable
  through a symlink alias stats as existing but was never indexed) disappears,
  since existence now means "the walk listed it".
- The test classification of section 8, refined by GM-323.

**GM-324 no longer has to**

- Port `createProjectFileExists`'s stat-and-memo, `createIndexabilityChecker`
  or the per-reparse resolver lifetime: the existence set replaces all three.
- Port `ignorePolicy.ts` (SDK walk) or decide `symlinks.ts`'s future (GM-349).
- Invent a cache-invalidation rule for configs: `watch_files` plus the
  existing ADR 0008 reindex is the rule.
- Make the semantic tier consistent with any of this: it stays on Node
  (`ProjectIndex` in `semanticPass.ts` builds its own `createProjectResolver`)
  until its own task.

## 8. Classification of the project-model tests

90 tests in four files (`grep -cE '^\s*(describe|it|test)\('`: resolve 33,
tsconfigPaths 15, workspace 33, ignorePolicy 9; no `describe` blocks).
`symlinks.ts` has no test file of its own; its 6 tests live in
`bulkIndex.test.ts` and are listed at the end for GM-323.

Buckets, chosen to map onto GM-323's four:

- **S-port** - behaviour that must survive, ported as a Rust test of the TS
  project model/resolver (GM-323: *language behaviour, must survive*).
- **S-sdk** - behaviour that must survive, but its home after the port is the
  SDK walk, which already owns it; GM-324 checks the SDK covers it rather than
  porting it (GM-323: *must survive*).
- **G349** - depends on GM-349's symlink decision (GM-323: *a capability the
  other plugins lack*).
- **Moot** - tests the Node implementation this design deletes (GM-323:
  *implementation-specific*).

Whether any of these is also *bench-load-bearing* (GM-323's first bucket)
needs g-mesh-bench evidence and is GM-323's to establish; excalidraw is a pnpm
monorepo with 24 config files (M1), so the workspace and tsconfig rows are
the likely candidates.

### `plugins/typescript/test/resolve.test.ts` (33)

| Line | Test | Bucket |
|---|---|---|
| 34 | a specifier that already names a source file resolves to itself | S-port |
| 39 | an extensionless specifier picks up the source extension | S-port |
| 46 | TypeScript's own extensions win over the JS ones for the same stem | S-port |
| 56 | an ESM `.js` specifier resolves to the `.ts` source it is compiled from | S-port |
| 62 | a real `.js` file next to TS sources still resolves to itself | S-port |
| 67 | the other emitted-extension pairs substitute the same way | S-port |
| 73 | a directory specifier resolves to its index file | S-port |
| 83 | a file wins over a same-named directory's index | S-port |
| 88 | `..` segments are resolved against the importing file's directory | S-port |
| 94 | relative resolution never claims a bare or package specifier | S-port |
| 103 | a dangling relative import resolves to nothing rather than throwing | S-port |
| 111 | a specifier climbing out of the project root resolves to nothing | S-port |
| 122 | a target this plugin does not parse is not claimed as resolved | S-port |
| 131 | an importer at the project root resolves against the root | S-port |
| 139 | the fs-backed predicate answers about real files under the project root | Moot (stat probe replaced by the existence set) |
| 162 | the fs-backed predicate treats a hard-excluded directory as non-existent even when the file is there | S-sdk (rule survives as "the existence set is the walk's output"; one TS test asserts that equality) |
| 178 | the fs-backed predicate treats a gitignored file as non-existent | S-sdk (same) |
| 219 | a workspace package resolves to the entry file it actually has | S-port |
| 224 | a declared entry wins when the build output is really there | S-port |
| 229 | an exports map decides the entry, for the package root and its subpaths | S-port |
| 235 | a subpath with no exports entry falls back onto the package's source tree | S-port |
| 240 | a package outside the workspace stays unresolved | S-port |
| 255 | a workspace package whose entry is missing is not invented | S-port |
| 259 | a workspace with no packages resolves nothing | S-port |
| 266 | the project resolver resolves workspace imports and leaves real packages alone | S-port (becomes `load_project` + resolve on a fixture tree) |
| 316 | a paths alias resolves through the same extension guessing as a relative import | S-port |
| 332 | the workspace answer wins over an alias for the same specifier | S-port |
| 348 | the project resolver resolves aliases and workspace packages side by side | S-port (fixture tree) |
| 381 | an alias is refused when its target is outside the project or unparseable | S-port |
| 425 | a `#` specifier resolves through the importing file's own package imports map | S-port |
| 436 | a `#` specifier is refused when the importing file has no enclosing imports map | S-port |
| 442 | the project resolver resolves a `#private` import to the real file it names | S-port (fixture tree) |
| 460 | the project resolver leaves an unmatched `#private` import unresolved | S-port (fixture tree) |

### `plugins/typescript/test/tsconfigPaths.test.ts` (15)

| Line | Test | Bucket |
|---|---|---|
| 55 | a baseUrl-anchored wildcard alias resolves to the directory it names | S-port |
| 74 | an exact alias key matches only that literal specifier | S-port |
| 90 | a key's targets are offered in declaration order | S-port |
| 105 | every matching key contributes, in the order the config declares them | S-port |
| 124 | without a baseUrl, targets resolve against the config file's own directory | S-port |
| 142 | a config with no paths of its own inherits the ones it extends | S-port |
| 159 | a config's own paths replace an inherited map whole, key by key included | S-port |
| 181 | a baseUrl without paths of its own leaves an inherited map's directory alone | S-port |
| 200 | an unreadable or package-named extends entry is skipped, not fatal | S-port |
| 224 | the nearest config wins over a more distant one | S-port |
| 245 | comments and trailing commas are read the same as strict JSON | S-port (JSONC input support) |
| 279 | a baseUrl climbing out of the project voids the aliases it anchors | S-port |
| 293 | an alias target climbing out of the project is not offered | S-port |
| 319 | a jsconfig.json is read the same way, when there is no tsconfig.json | S-port |
| 352 | the resolved config is shared by every file it governs | Moot (per-walk memo; a load-time model is shared by construction) |

### `plugins/typescript/test/workspace.test.ts` (33)

| Line | Test | Bucket |
|---|---|---|
| 57 | a package specifier splits into its name and the subpath it addresses | S-port |
| 73 | what is not a package specifier at all is refused | S-port |
| 85 | the declared entry fields are all offered, most authoritative first | S-port |
| 97 | an exports map is read for the subpath asked about | S-port |
| 109 | a condition map without subpath keys describes the package root | S-port |
| 117 | a wildcard exports subpath substitutes the matched part | S-port |
| 125 | the source-tree conventions are offered after whatever the manifest declares | S-port |
| 134 | a subpath falls back to the same path inside the package and its src | S-port |
| 141 | an entry pointing outside the project is not offered | S-port |
| 151 | pnpm workspace globs are expanded to the packages they name | S-port |
| 172 | the root package.json `workspaces` field is read the same way | S-port |
| 187 | yarn's object form of `workspaces` is read too | S-port |
| 199 | a `!` pattern removes a package the globs had picked up | S-port |
| 212 | node_modules is never walked into, so a vendored copy is not a workspace package | S-port |
| 226 | a project with no workspace manifest has no workspace packages | S-port |
| 238 | a malformed or nameless manifest is skipped rather than thrown on | S-port |
| 252 | pnpm-workspace.yaml keys other than `packages` are ignored | S-port (input format; the Rust port may use a YAML crate instead of the hand parser, the behaviour stays) |
| 274 | a flow-sequence `packages` list is read as well | S-port (same) |
| 291 | a workspace glob matches a symlinked package directory, under the path the glob matched | G349 |
| 310 | a symlink cycle under a `**` pattern neither hangs nor invents packages | G349 |
| 335 | a real package directory and a symlink alias of it collapse to one entry, sorted-first winning | G349 |
| 350 | a workspace-glob symlink resolving outside the project root is refused | G349 |
| 372 | a dangling symlink under a workspace glob is skipped, the real package still resolving | G349 |
| 391 | an exact imports-map key resolves to its declared target | S-port |
| 396 | a wildcard imports-map key substitutes the captured part | S-port |
| 401 | a specifier with no matching imports-map key resolves to nothing | S-port |
| 406 | a condition-object imports value is ranked the same way exportsTargets ranks one | S-port |
| 411 | all matching imports keys contribute, not just the first | S-port |
| 423 | an imports target escaping the package directory is refused | S-port |
| 428 | nearest package.json wins: a sub-package's own imports map shadows the root's | S-port |
| 448 | a plain, non-monorepo project's own root package.json is still read for `imports` | S-port |
| 469 | a package.json with no `imports` field leaves a `#specifier` unresolved, not inherited from a grandparent | S-port |
| 492 | an exports map with both an import and a require target, both real files, always picks import | S-port |

### `plugins/typescript/test/ignorePolicy.test.ts` (9)

| Line | Test | Bucket |
|---|---|---|
| 29 | hasHardExcludedSegment sees a hard-excluded name at any depth | Moot (function replaced by the SDK walk's `exclude_dirs`) |
| 37 | hasHardExcludedSegment only matches whole segments | Moot (same) |
| 47 | a hard-excluded directory is non-indexable regardless of depth or .gitignore | S-sdk |
| 68 | a root .gitignore excludes the paths it names, and only those | S-sdk |
| 88 | a nested .gitignore applies to its own subtree only | S-sdk |
| 105 | a negation re-includes a path an earlier rule excluded | S-sdk |
| 120 | a deeper .gitignore's negation overrides a broader rule from the root | S-sdk |
| 136 | a project with no .gitignore anywhere calls everything outside a hard-excluded dir indexable | S-sdk |
| 172 | plugin.toml's exclude_dirs equals HARD_EXCLUDED_DIRS minus the baseline | Moot (no TS-side copy of the list remains to drift) |

For S-sdk, GM-324 confirms each rule has an SDK walk test
and adds the missing ones in `plugins/sdk/src/walk.rs`, not in the TS crate.
Its tests today: `skips_the_manifests_excluded_directories_at_any_depth`
(covers row 47's depth rule) and
`honours_gitignore_outside_a_git_repository_including_negations` (rows 68
and 105). No SDK test names the nested-`.gitignore`-subtree (88) or
deeper-negation-overrides-root (120) cases, nor row 47's "regardless of
`.gitignore`" re-include, so those three are likely additions.

### Totals

| Bucket | resolve | tsconfigPaths | workspace | ignorePolicy | Total |
|---|---|---|---|---|---|
| S-port | 30 | 14 | 28 | 0 | **72** |
| S-sdk | 2 | 0 | 0 | 6 | **8** |
| G349 | 0 | 0 | 5 | 0 | **5** |
| Moot | 1 | 1 | 0 | 3 | **5** |
| Total | 33 | 15 | 33 | 9 | **90** |

### Related, outside the 90: symlink walk tests in `bulkIndex.test.ts`

All G349: line 467 "a symlinked package directory is walked, and indexed under
its own apparent path"; 493 "two paths onto the same real directory index it
exactly once, sorted-first path winning even when that is the symlink"; 505 "a
symlinked file aliasing an already-walked real file is skipped, not indexed
twice"; 525 "a symlink pointing back at its own containing directory does not
loop forever"; 541 "a symlink resolving outside the project root is refused";
558 "a dangling symlink is skipped rather than throwing".

### New tests this decision requires (not in the 90)

1. SDK: the hook runs before extraction on `fileChanged` (section 7).
2. TS plugin: create-then-import resolves within one session (section 7).
3. TS plugin: the existence set equals the bulk walk's file list (replaces
   rows 139/162/178's intent).
4. Core: a `tsconfig.base.json` save under the TS manifest triggers the TS
   reindex; one under `node_modules` does not.

## 9. Owner questions

Each changes scope or criteria, so none is decided here.

- **Q1 (W1, burst ordering).** Accept the burst-ordering window as named, or
  add a task to route a drained batch's creations before its modifications in
  core? Without it, a `git checkout` that adds a file and its importer can
  leave the import unresolved until the importer's next edit.
- **Q2 (level up Python).** Python can implement the same hook to close its
  documented "module created since the last load" gap. In this release (a
  small task beside GM-324), or later?
- **Q3 (Rust's orphan window).** A Rust module file added mid-session is an
  `Orphan` (wrong container, not just a missing edge) until a `Cargo.toml`
  save. Presence does not fix it; re-scanning the module tree on `mod`-item
  edits would. File it as its own task?
- **Q4 (`.gitignore`).** Should a `.gitignore` edit re-evaluate indexability
  (for core's file population as much as for TS resolution), or stay
  restart-only?
- **Q5 (reindex cost).** Every `package.json` save, dependency bumps included,
  costs a whole-language TS reindex (M4, ~20 s of CPU on the Node plugin under
  load). Accept, or ask GM-324 to measure the Rust port's walk and set a bound?
  A finer mechanism (diff the parsed resolution facts and skip the reindex
  when they are unchanged) would be new core work.

## Appendix: how the facts were found

- Implementors of `Extractor`: g-mesh `find_implementations`
  (symbol id of `plugins/sdk/src/lib.rs`'s trait) - rust, python, toy, three
  `run.rs` tests; Go and TS absent.
- Callers of `createProjectResolver`: g-mesh `find_callers` -
  `reparseChangedFile` (`incremental.ts`), `bulkIndexProject`
  (`bulkIndex.ts`), `ProjectIndex#constructor` (`semanticPass.ts`), plus tests.
- `route_settled_path`: g-mesh `find_definition` (source returned).
- `fileChanged`/`workspaceChanged` handling in the SDK: `grep -n` on
  `plugins/sdk/src/run.rs` (one known file), then `Session::handle`,
  `hydrate`, `file_changed` read by line range of those functions.
- `watch_files` declarations: `grep` over manifests (non-code TOML): go
  `["go.mod","go.work"]`, rust `["Cargo.toml"]`, python
  `["pyproject.toml","setup.cfg","setup.py"]`, typescript `[]`.
- Go's workspace: `grep -n` outline of `plugins/go/control.go` and
  `workspace.go`.
- Test names: `grep -nE '^\s*(describe|it|test)\('` per test file.
- M1, M2: `find` and `git log --name-status -n 1000` in the excalidraw
  checkout. M3: `grep` over `~/.g-mesh/projects/*/daemon.log*`. M4:
  `/usr/bin/time -p node plugins/typescript/dist/src/index.js --bulk-index`
  in the excalidraw checkout, with `uptime`.
