# GM-509: a resolution-config edit re-extracts only the importers it affects

Design note for GM-509/S1. Branch `feat/GM-509-selective-config-reindex`
(off `release-4.3.0` at `a98f636`, which includes GM-498 and GM-495). No code.
Line numbers are 1-based and taken from this worktree.

## 1. Today

### 1.1 Routing a watch-file save

1. `daemon::watch_and_route_once` (core/src/daemon/mod.rs:473-506) drains the
   debouncer, classifies each settled path, and calls
   `PluginRegistry::route_settled_path` once per path: deletions first, then
   creations, then modifications.
2. `route_settled_path` (core/src/daemon/registry.rs:955-967) asks
   `workspace_language_matches` (registry.rs:718-733). That function matches the
   file *name* against every manifest's `[plugin.workspace] watch_files` and
   skips the manifest's `exclude_dirs`. A match calls
   `workspace_file_changed` for each matching language (registry.rs:1069-1088)
   instead of `file_changed`.
3. `workspace_file_changed` spawns or wakes the supervisor and calls
   `workspace_reindex::run` (core/src/daemon/workspace_reindex.rs:178-185 ->
   `run_with` 188-272 -> `rebuild` 276-346).
4. `run_with`, inside `PluginSupervisor::with_exclusive_access`
   (core/src/daemon/lifecycle.rs:436):
   - sends the `workspaceChanged` **notification**
     (`PluginProcess::notify_workspace_changed`, core/src/daemon/plugin.rs:1229-1238);
   - marks `pending_reindex`;
   - walks the whole language into `staging-<lang>.db` with a fresh
     `--bulk-index` child, links it, plans the difference against live, and
     swaps it in (ADR 0008).

   After the lock is released it runs the whole-language semantic pass.
5. `resume_pending` (workspace_reindex.rs:145-159) reruns an interrupted
   reindex at the next activation.

g-mesh calls behind this:

- `find_callers(route_settled_path)` returned `watch_and_route_once`, 9 tests in
  workspace_reindex.rs, 3 in registry/tests.rs, and
  `core/tests/typescript_registry/mod.rs::Harness::route`.
- `find_callers(workspace_language_matches)` returned `route_settled_path` and
  `announce_created`.
- `find_callers(workspace_file_changed)` returned `route_settled_path` and
  `resume_pending`.
- `find_callers(workspace_reindex::run)` returned `workspace_file_changed` and 14
  tests.
- `find_callers(notify_workspace_changed)` returned only `run_with`.
- `find_references` on the enum variant `ControlMessage::WorkspaceChanged`
  returned nothing, because g-mesh does not track enum variants. grep found the
  rest: wire/src/lib.rs:592, plugins/sdk/src/run.rs:477,
  plugins/go/control.go:305.

### 1.2 What the plugin does with `workspaceChanged`

- **SDK plugins (TS, Rust, Python).** In plugins/sdk/src/run.rs:477-498 the
  plugin logs the edit and calls `index.clear()`. It sets
  `project_hydrated = false`, reruns `load_project`, and tells a started
  semantic engine (`engine.workspace_changed()`, GM-433). Nothing is answered.
- **Go** (plugins/go/control.go:305-322) runs `state.reloadWorkspace()` and drops
  its per-file cache.

### 1.3 What each plugin's model actually reads from its watch files

**TypeScript.** `TsProject` (plugins/typescript/src/project/mod.rs:53-67, load
77-122) holds:

- `existence`: the SDK walk, kept current by `file_presence_changed` (ADR 0023);
- `packages`: name -> `WorkspacePackage { dir, manifest: Json }`;
- `tsconfig_by_dir`: dir -> effective `paths` after `extends`, as an
  `EffectiveConfig { resolve_dir, paths }`;
- `imports_by_dir`: dir -> package.json `imports`.

`resolve` (project/resolve.rs:150-158) tries these in order:

1. A relative specifier, which uses only `existence`.
2. A `#...` specifier, which uses the nearest package.json's `imports`.
3. A bare specifier, which tries the workspace package first
   (`package_entry_targets`, workspace.rs:135-160, which reads only `exports`,
   `source`, `main`, `module`, `types` and `typings`), then the nearest
   tsconfig's `paths`.

So `version`, `dependencies` and `scripts` are read and never used. A
version bump changes `manifest: Json`, but nothing that resolution reads.

**Go.** The `workspace` value (plugins/go/workspace.go:55-65, `loadWorkspace`
78) is a list of `moduleRoot { dir, path }` taken from every go.mod's `module`
line and go.work's `use`. `require`/`replace` are never parsed (the module doc,
workspace.go:13-31). The container key of a file is its import path, which is
`module path + dir`.

**Rust.** `ProjectContext` (plugins/rust/src/project/mod.rs:162-178) holds:

- `crates` (key, root, package_dir);
- `files` (path -> `ContainerInfo::Member{key,parent}` or `Orphan`);
- `container_keys`;
- `notes`.

`Cargo.toml` is read as data (cargo_manifest.rs). Dependency versions are not
modelled.

**Python.** `ProjectContext` (plugins/python/src/project/mod.rs:392-425) holds:

- `roots` / `declared_roots` (from `pyproject.toml` only);
- `files` (path -> `ContainerInfo`);
- `containers` (key -> count);
- notes.

`setup.cfg` and `setup.py` are watched but never read (plugin.toml:201-209).

Both the Rust and Python models derive `PartialEq`.

### 1.4 What the index holds about imports

Each plugin emits one `Module` placeholder per import plus an `IMPORTS` edge
from the file:

- **TypeScript** (plugins/typescript/src/extractor/imports.rs:195-210): the
  placeholder's `name` is the raw specifier. Its `qualifiedName` is the resolved
  path, or the specifier when nothing resolved. `nativeKind` is
  `resolved_module` or `external_module`.
- **Go** (plugins/go/extract.go:461-495): `qualifiedName` is the import path, and
  `name` is only its last segment.
- **Rust and Python:** an `external_module` named by the crate name or dotted
  key (rust emit.rs:268, python decls.rs:602). A resolved import is a placeholder
  with a `container` target.

`graph::imports::link` (core/src/graph/imports.rs:322-422) repoints a resolved
placeholder's edge onto the `File` or container node
(`UPDATE edges SET toId = ?1, resolved = 1`, :343) and **deletes the
placeholder**. A linked import therefore no longer records which specifier it
came from. Only the target remains. Unlinked placeholders keep their `name`,
their `qualifiedName` and their `placeholder_targets` row (`scopeKind`, `scope`,
`fromFile`). The `edges` table (schema.rs:294-304) has no specifier column.

`find_callers(link_diff)` showed that the only production caller is
`storage::index_store::link_applied`. Linking happens inside every
`apply_diff`, so a per-file re-extract relinks on its own.

### 1.5 Cost today (GM-324, docs/results/gm-324-ts-rust-port-measurements.md §1)

The corpus is excalidraw `1acf66ed`: 658 TS/JS files, 16,650 nodes and 27,064
edges. Per `packages/tsconfig.base.json` touch, with the Rust TS plugin, the
median is:

| Measure | Value |
|---|---|
| Save -> swap | 4.26 s |
| Re-walk child real / user / sys | 2.60 / 1.57 / 0.11 s |
| `fileChanged`, typical | 8-37 ms |
| `fileChanged`, largest file | 128 ms |

GM-425 (docs/results/gm-425-workspace-reindex-swap.md) measured the Rust
language on this repo: swapped in 2.98 s after the edit, with the semantic pass
done at +60.6 s.

## 2. Options

### A. The plugin diffs its resolution facts and names selectors; core expands them (owner's shape, **recommended**)

1. The plugin reloads its model.
2. It compares the model's *resolution facts* with the facts the index was
   built from. Resolution facts are the model minus the presence-tracked
   existence set, reduced to the fields resolution reads.
3. It answers with one of:
   - `unchanged`;
   - `unknown` (core then reindexes);
   - `affected`: a list of selectors.
4. Core turns the selectors into a set of files using stored rows, and
   re-extracts those files.

The facts the index was built from must survive plugin sleep and restart.
Supervisors idle out, so "the plugin's previous model in memory" is usually
absent exactly when someone edits `package.json`. Core therefore stores the
facts as an opaque per-language blob in the index:

- The bulk walk emits it as one trailing NDJSON line.
- Each answer carries the new blob.
- The blob is swapped with the language's rows.

Benefits:

- The cost of an unchanged edit is one config parse plus one round trip.
- The diff works on the same struct the extractor reads, so a missing fact is
  visible in one place per plugin.
- The facts survive plugin sleep and restart, and are consistent with the rows
  they describe, because they come from the same load as the walk.

Risks:

- New persisted state: a table, a bulk trailer line and a swap step.
- A selector language that core has to interpret.
- A fact a plugin forgets to diff gives silently stale edges until the next
  edit of the importer. Mitigation: the tests compare against a cold index
  (§7).
- Exactness for TypeScript needs the specifier on linked edges (decision 2).

### B. The plugin answers only `unchanged` or `unknown`; no selection

Same request, but without selectors. A no-op edit is skipped, and anything else
reindexes the whole language.

- Benefits: about a third of A's code. No schema change and no selector
  language. It meets acceptance 1 (version bump) and covers the most frequent
  edit.
- Risks: it fails acceptance 2. An `exports`/`paths` edit still costs 4.26 s.
  It still needs the persisted facts.

### C. Core re-derives against the index; the plugin needs no history

1. Core sends the plugin what it stored about each file: each import's
   specifier and stored target, and each file's container key.
2. The plugin re-derives each of them under the reloaded model and names the
   files where they differ.

- Benefits: no persisted facts. It is exact by construction. It also heals
  drift, such as ADR 0023's residual presence windows.
- Risks:
  - The payload is O(imports) per save: about 0.4 MB on excalidraw, and tens of
    MB on a 200k-import monorepo.
  - Each language needs a second resolver beside its extractor. TypeScript
    already has one (`TsProject::resolve`). Rust's `use` resolution depends on
    local names in scope (`extractor/keys.rs::resolve`), so it cannot be
    re-derived from `(file, specifier)` and would degrade to checking which
    keys exist.

### D. Core hashes the watched file minus "irrelevant" keys

Rejected: it puts language knowledge (which package.json keys matter) into
core.

## 3. Recommendation

Option A, staged so that B's value ships first, inside the same mechanism:
**the `unchanged` answer is implemented and measured before `affected` is.**

### 3.1 Wire (wire/src/lib.rs)

- A new request, `ControlMessage::ResolutionChanged { file_path, previous_facts: Option<String> }`.
  It has an id, and the answer is `ResolutionChangedResult { delta: ResolutionDelta, facts: Option<String> }`.
- `ResolutionDelta` is one of:
  - `Unchanged`;
  - `Unknown { reason }`;
  - `Affected { files: Vec<PathScope>, imports: Vec<ImportSelector> }`.
- `PathScope { under: String, not_under: Vec<String> }`, where `""` means the
  whole project.
- `ImportSelector { importers: PathScope, by: ImportMatch }`, where
  `ImportMatch` is one of:
  - `Specifier(Matcher)`: the raw specifier, see 3.3;
  - `Target { scope_kind: file|container, matcher: Matcher }`: the stored
    target. That is the linked edge's `toId` node path or container key, or an
    unlinked placeholder's `placeholder_targets.scope`.
- `Matcher` is one of:
  - `Exact(s)`;
  - `Under { prefix, separator }`: `s == prefix || s starts with prefix+separator`.
    It is boundary-safe: `pkg.sub` does not match `pkg.subtle`;
  - `StartsWith(s)`, for paths patterns such as `@app/`;
  - `NonRelative`, for TypeScript: not `.`, `/` or `#`.
- `WireEdge.specifier: Option<String>` (serde default) is set by plugins on
  `IMPORTS` edges to the raw text of the import.
- The bulk stream gets one more line kind: `{"resolutionFacts": "<opaque>"}`,
  written last.
- `FileChangedParams` gains `reextract: bool` (default false). It bypasses
  the SDK's unchanged-text short-circuit (run.rs:595-601). The baseline is kept,
  so the diff stays minimal.

### 3.2 Manifest

`[plugin.capabilities] resolution_delta = true` (core/src/daemon/manifest.rs,
`Capabilities`, next to `files_created` :184). Absent means false, and today's
path applies: notify, then the whole-language reindex. A third-party plugin
needs nothing.

A plugin that declares the capability must:

- answer `resolutionChanged`;
- write the bulk trailer;
- set `specifier` on `IMPORTS` edges.

`g-mesh plugins check` gains a conformance check: a version-bump-shaped edit to
a fixture watch file must answer `unchanged`.

### 3.3 The specifier on linked edges (decision 2)

Add `edges.specifier TEXT` (nullable). `apply_diff` writes it from
`WireEdge.specifier`. `graph::imports::link` only updates `toId`, so the value
survives linking. It is copied by `language_swap` (`EDGE_COLUMNS`,
language_swap.rs:502). This bumps `CURRENT_SCHEMA_VERSION` to 14
(schema.rs:86), which rebuilds every index once at upgrade.

Without the column, TypeScript has to select by target. "Importers of any file
under `packages/math/`" then also re-extracts the package's own relative
importers: an over-selection, never a miss.

### 3.4 Core flow

A new module, core/src/daemon/config_reindex.rs. It is reached from
`workspace_file_changed` when the language's manifest has `resolution_delta`.

1. Get or spawn the supervisor. Inside `with_exclusive_access`, read
   `resolution_facts(language)`. Send `resolutionChanged { filePath, previousFacts }`.
   The timeout is the `fileChanged` timeout.
2. `Unchanged` → store the new facts and stop. Nothing is re-extracted, no
   semantic pass is owed, and one log line records the result.
3. `Unknown`, `previousFacts` = `None`, an error or a timeout → the existing
   `workspace_reindex::run`. Its bulk trailer stores fresh facts.
4. `Affected` → `select_affected(conn, language, &delta)` builds the union of:
   - the language's `File` paths inside each `files` scope;
   - importer paths from `IMPORTS` edges of the language, both linked
     (`edges.specifier`, `toId` → `File.filePath` or `containers.key`) and
     unlinked (placeholder `placeholder_targets.scope` / `fromFile`). They are
     filtered by `importers` scope and matcher in Rust: one scan of the
     language's `IMPORTS` edges, a few thousand rows on excalidraw.
5. If the selection is larger than the threshold (decision 3; proposed: more
   than 30% of the language's indexed files), run the whole-language reindex.
   The staging swap is cheaper and atomic at that size: by GM-324, per-file at
   about 15 ms against 4.26 s breaks even near 280 of 658 files.
6. Otherwise:
   1. `mark_pending_reindex(language, file)`.
   2. For each selected file, one structural round trip
      (`apply_file_change_in` with `reextract = true` and
      `semantic_pass_capable = false`), all inside the same exclusive section.
   3. Then **one** `semanticPass` scoped to the selected files plus the owed
      files (GM-498's `owed_files`).
   4. Then, in one transaction, store the new facts and clear `pending_reindex`.
7. If the daemon dies mid-loop, `pending_reindex` stays. `resume_pending` calls
   `workspace_file_changed` again, and `selective` sees the row and falls back
   to the whole-language reindex, whose swap clears it. Asking again with the
   old facts is not enough: a config reverted while the daemon was down reads
   as `unchanged` and would keep the rows re-extracted under the new config.

### 3.5 SDK (plugins/sdk)

- `Extractor` gains two default methods:
  - `resolution_facts(&self, &Project) -> Option<String>`, default `None`;
  - `resolution_delta(&self, previous: &str, &Project) -> ResolutionDelta`,
    default `Unknown`.
- `Session` handles `resolutionChanged` as follows:
  1. Load the new model.
  2. If the load fails, answer `Unknown` and keep the old model.
  3. Compute the delta, and swap in the new model.
  4. On `Unknown` only, do today's `index.clear()` and set
     `project_hydrated = false`. Otherwise keep both: unaffected files' cached
     graphs remain valid by definition, and the affected files arrive with
     `reextract`.
  5. Always call `engine.workspace_changed()`. It is cheap and only resets
     readiness trust (GM-433).
- `bulk_index` (run.rs:167-216) writes the trailer when `resolution_facts`
  returns `Some`.
- A shared helper for container-keyed models, `ContainerFacts { files: path->key, keys }`,
  with `container_delta(old, new, separator)`. Its result:
  - `files`: each path whose key changed, as an exact `PathScope`;
  - `imports`: `Target{container, Exact(k)}` for every removed or added key,
    plus `Specifier(Under{k, sep})` for every added key. The second covers an
    `external_module` that would now resolve.

### 3.6 Per language

**TypeScript.** The facts are:

- `packages` projected to `{dir, exports, source, main, module, types, typings}`;
- `tsconfig_by_dir`;
- `imports_by_dir`.

The delta:

- A package name added, removed or with a changed projection →
  `Specifier(Under{name,"/"})`, with the whole project as scope.
- A tsconfig at dir D changed, added or removed → `Specifier(StartsWith(p))`
  for each old ∪ new pattern prefix (`NonRelative` when a pattern is `*` or
  `resolve_dir` changed). The scope is under D, not under any deeper config dir,
  old or new.
- `imports` at D changed → `Specifier(StartsWith("#"))`, with the scope under D,
  not under any deeper package.json.

`existence` is excluded from the diff, and the new walk's set is adopted. The
load-once rule of ADR 0023 is unchanged: the model is still rebuilt only on a
watch-file save. A presence window stays as today.

**Go** (control.go, not the SDK). The facts are the sorted
`[(dir, modulePath)]`.

- Equal → `Unchanged`. This covers `require`/`go` directive edits.
- A changed, added or removed module → `files: PathScope{under: dir, not_under: nested module dirs}`,
  because every member's container key moves. Add `imports: Specifier(Under{old,"/"})` ∪
  `Specifier(Under{new,"/"})`.

A single-module repo's rename selects every file and so crosses the threshold.
That is a whole-language reindex by design, and the stated Go exception.

**Rust.** Facts are `ContainerFacts` from `files` and `container_keys`, plus
`crates`.

- A dependency or version edit → `Unchanged`.
- A `[lib] path`, member or crate-name edit → the member files whose key moved,
  plus importers via `Target{container}` and `Specifier(Under{crate,"::"})`.
- Exception: in-crate `use crate::…`/`super::…` imports are matched by their
  stored *target key*, never by their text.

**Python.** Facts are `ContainerFacts` from `files` and `containers`, plus
`declared_roots`, with the separator `.`.

- `pyproject.toml` dependency or version edits → `Unchanged`.
- `setup.cfg` and `setup.py` are not read, so they are always `Unchanged`. This
  is correct, because today's reindex rebuilds the same model.

### 3.7 GM-507 (Rust module orphaned until Cargo.toml is saved)

GM-509 makes a `Cargo.toml` save re-extract exactly the files whose
`ContainerInfo` changed under the reloaded module tree. That includes today's
orphans, which the reindex used to fix by brute force.

GM-507's open question is whether a plugin can make core re-extract *other*
files after a `.rs` edit that adds a `mod` item. GM-509's core half answers it:

- `ResolutionDelta::Affected` plus `config_reindex::reextract`;
- an optional `FileChangeDiff.affected: Option<ResolutionDelta>` that core
  honours after applying the diff.

Recommendation: GM-507 rides on GM-509's wire type and core re-extract (S2), is
sequenced after it, and adds only the `.rs`-triggered module-tree rescan and
that one optional field. GM-509 does not implement the `.rs` trigger.

## 4. Edit map (provisional slices)

| Slice | Function / item | File | Lines today |
|---|---|---|---|
| S2 core | `ControlMessage::ResolutionChanged`, `ResolutionDelta`, `PathScope`, `ImportSelector`, `Matcher`, `WireEdge.specifier`, `FileChanged.reextract` | wire/src/lib.rs | 499-519, 547-620 |
| S2 | `Capabilities.resolution_delta` | core/src/daemon/manifest.rs | 154-190 |
| S2 | `CURRENT_SCHEMA_VERSION` 14, `edges.specifier`, `resolution_facts(language PK, facts)` table, `resolution_facts`/`set_resolution_facts` | core/src/storage/schema.rs | 86, 294-304, 461-467 |
| S2 | write `specifier` | core/src/storage/write.rs | 577 (edge insert), `EdgeRecord` 188 |
| S2 | copy `specifier` + facts at swap | core/src/storage/language_swap.rs | 502 (`EDGE_COLUMNS`), `swap` 405-540 (coordinate with GM-498's edits here) |
| S2 | `BulkItem::ResolutionFacts`, store it in the walk's store | core/src/daemon/bulk_index.rs | `ingest_in` 475-524, `purge_language` 305 |
| S2 | `send_resolution_changed`; `reextract` flag through `send_one` | core/src/daemon/plugin.rs | 1229-1238, 1326-1370 |
| S2 | `apply_file_change_in` gains `reextract` | core/src/watcher/apply.rs | 117-175 |
| S2 | new `config_reindex::{run, select_affected, reextract}` | core/src/daemon/config_reindex.rs | new |
| S2 | branch on the capability | core/src/daemon/registry.rs | `workspace_file_changed` 1069-1088 |
| S2 | test plugin answers `resolutionChanged` with a scripted delta | core/src/daemon/test_plugin.rs | (harness) |
| S4 SDK | `resolution_facts`/`resolution_delta` defaults; `ContainerFacts`, `container_delta` | plugins/sdk/src/lib.rs | 174-198 |
| S4 | `"resolutionChanged"` arm; `reextract` in `file_changed`; bulk trailer | plugins/sdk/src/run.rs | 477-498, 570-620, 167-216 |
| S4 | `FileGraphBuilder` sets `specifier` on IMPORTS | plugins/sdk/src/graph.rs | add_edge site |
| S4 Rust | facts + delta; `specifier` on import edges | plugins/rust/src/project/mod.rs 162-260; extractor/emit.rs 266-272, decls.rs 782 |
| S4 Python | facts + delta; `specifier` | plugins/python/src/project/mod.rs 392-480; extractor/decls.rs 602 |
| S6 TS | `TsFacts` projection + delta | plugins/typescript/src/project/mod.rs 53-122 (+ workspace.rs 135, tsconfig.rs 32) |
| S6 | `specifier` on the IMPORTS edge | plugins/typescript/src/extractor/imports.rs 195-210 |
| S6 | manifest `resolution_delta = true` | plugins/typescript/plugin.toml 32-38 |
| S7 Go | `resolutionChanged` case, facts/delta, bulk trailer, `specifier`, `reextract` | plugins/go/control.go 255-340, workspace.go 55-140, extract.go 461-495, bulkindex.go, wire.go 284 |

## 5. Behaviours (each needs one control, 6-8 total chosen in the tests slices)

1. A `package.json` `version`/`dependencies` edit in a TS workspace answers
   `Unchanged`, re-extracts 0 files and runs no bulk child. The control is
   the reverted capability check, which brings back the whole-language reindex.
2. A change to one workspace package's `exports` re-extracts exactly the files
   with a non-relative import naming that package. Their `IMPORTS` edges then
   equal a cold index of the edited tree. The control: drop `exports` from the
   projection.
3. Intra-package relative importers are not re-extracted (the specifier
   column). The control: match by target.
4. A tsconfig `paths` change at D selects only importers under D that are not
   shadowed by a deeper tsconfig.
5. `Unknown`, a missing previous facts blob, an error or a timeout falls back to
   the whole-language reindex. Its bulk trailer stores facts, and the next edit
   is selective.
6. A selection above the threshold runs the whole-language reindex.
7. A daemon killed mid re-extract leaves `pending_reindex`. The next start
   reruns it with the old facts and converges.
8. A plugin without `resolution_delta` behaves exactly as today.
9. Go: a `require` bump answers `Unchanged`. A `module` rename in a
   single-module repo falls back to the reindex (threshold).
10. Rust: a dependency version bump answers `Unchanged`. Changing `[lib] path`
    re-extracts the crate's members whose key moved, plus their cross-crate
    importers.
11. Python: a `pyproject.toml` version bump and any `setup.cfg`/`setup.py` edit
    answer `Unchanged`.
12. After `Affected`, exactly one `semanticPass` is sent, scoped to the selected
    files plus the owed files.

## 6. Must confirm (implementers)

- The semantic tier never re-sends `IMPORTS` edges. If it does, `apply_diff`
  must keep `specifier` when a re-sent edge lacks it.
- What the Rust and Python `external_module` `qualifiedName`/`name` hold exactly
  (assumed: the crate name, or the dotted key).
- Whether `apply_file_change_in` can run N times inside one
  `with_exclusive_access` without re-entering the supervisor lock
  (lifecycle.rs:436 vs 303).

## 7. Measurement plan (S11)

**Machine and arms.** Record `uptime` before and after every run, and
`/usr/bin/time -p` for the daemon and plugin CPU. Embeddings are off
(`G_MESH_MODEL_DIR` missing), as in GM-324. The arms:

- base: the release-4.3.0 merge-base, which runs the whole-language reindex;
- task: the branch head.

Both arms use release builds. Use one background script, and dry-run one arm
first.

**Corpus.** excalidraw `1acf66ed` (GM-324's), as a scratch `git clone --shared`.
`packages/*` are workspace packages with `exports`. 94 files import
`@excalidraw/math` (`git grep -l`).

**Edits,** 3 reps each, 5 s apart:

| Edit | Expected |
|---|---|
| E1. root `package.json` `"version"` bump | 0 files re-extracted; done ≪ 4.26 s |
| E2. `packages/math/package.json` `exports` remapping one subpath to an existing source file | re-extracted == the `@excalidraw/math` importers (independent oracle: `git grep -l "@excalidraw/math"`) |
| E3. `packages/tsconfig.base.json` `paths` entry edit | importers of that alias under `packages/` |

For E2 and E3, the task arm's `IMPORTS` edges of every file must equal a cold
bulk index of the edited tree.

**g-mesh repo:**

| Edit | Expected |
|---|---|
| E4. `Cargo.toml` dependency version bump | `Unchanged` |
| E5. `plugins/go/go.mod` `go` directive edit | `Unchanged` |
| E6. a Python fixture `pyproject.toml` version bump | `Unchanged` |
| E7. a `setup.cfg` touch | `Unchanged` |

**Columns:**

- files re-extracted;
- save → done (real);
- daemon user/sys;
- plugin user/sys;
- bulk child (yes/no);
- load1.

**Source.** A new daemon log line:
`<lang> resolution change after <file>: <delta kind>, <n> file(s) re-extracted in <ms>`,
stamped by a pump thread as in GM-324. Compare against GM-324's 4.26 s and 1.57 s
user, and GM-425's 2.98 s for Rust.

**Control.** The base arm must show a bulk child on E1. If it does not, the
probe measured nothing.

## 8. Split proposal

The tracker has 11 provisional slices. Proposal:

- **GM-509** keeps the wire, core, SDK and TypeScript: S1, S2, S3, S4 (SDK
  only), S6, S8, then verify and measure. This covers acceptance 1, 2 and 4, and
  states the exceptions.
- **A new task, "Rust and Python answer resolutionChanged",** takes the SDK
  `ContainerFacts` users, with code, tests and verify.
- **A new task, "Go answers resolutionChanged",** takes the Go control plane,
  facts, specifier and trailer, with code, tests and verify.

Until each one lands, that language keeps today's reindex (capability off), so
nothing regresses in between. GM-507 is sequenced after GM-509/S2.

## 9. Questions for the owner

**Q1. Which approach?**

- *Today:* any save of a watched config (`package.json`, `tsconfig*.json`,
  `go.mod`, `Cargo.toml`, `pyproject.toml`) re-walks the whole language. That is
  4.3 s on excalidraw, even for a version bump.
- *Choices:*
  - A: the plugin compares old and new resolution facts and names what changed;
    core re-extracts the matching files.
  - B: the plugin only says "nothing changed" or "something changed"; the
    latter still reindexes.
  - C: core sends every stored import to the plugin, which re-resolves them.
- *Example:* changing `exports` of `@excalidraw/math`.
  - A re-extracts the 94 importers.
  - B reindexes 658 files.
  - C re-extracts only the importers whose target actually moved.
- *Consequences:*
  - A meets every acceptance bullet but adds persisted facts and a selector
    language.
  - B is about a third of the work and fails acceptance 2.
  - C needs no persisted state and is exact, but ships every import per save
    and needs a second resolver per language, with Rust inexact.
- **Recommended: A, with `unchanged` landing first.**

**Q2. Store the raw import specifier on edges?**

- *Today:* after linking, an import edge knows its target file but not the text
  `@excalidraw/math/vector` that produced it.
- *Change:* add an `edges.specifier` column, which bumps the schema to 14.
- *Example:* an `exports` change in `packages/math`.
  - With the column, core picks only files importing `@excalidraw/math…`.
  - Without it, core picks every importer of any file under `packages/math/`.
    That includes math's own `./vector` relative importers: more files, but
    never a miss.
- *Consequences:*
  - With the column: exact, and every user's index is rebuilt once at upgrade.
  - Without it: no upgrade rebuild, an over-selection, and acceptance 2's
    "exactly" fails.
- **Recommended: add the column.**

**Q3. When should a selective re-extract give way to the whole-language reindex?**

- *Today:* always the whole language.
- *Change:* re-extract the selected files one by one. Each takes about 15 ms in
  GM-324, so roughly 280 of excalidraw's 658 files cost as much as one 4.26 s
  reindex. Per-file updates are also visible to queries one at a time, while the
  reindex swaps atomically.
- *Example:* renaming a Go module in a single-module repo selects every file.
- *Options:*
  - (a) A fixed 30% of the language's files, recalibrated by S11.
  - (b) Always selective.
  - (c) An absolute cap, such as 200 files.
- *Consequences:*
  - (a) is cheapest at both ends.
  - (b) can be slower than today on a big change.
  - (c) does not scale with project size.
- **Recommended: (a).**

**Q4. Where do the facts the index was built from live?**

- *Today:* none are kept.
- *Change:* core stores an opaque per-language blob, written by the bulk walk
  and by each answer.
- *Alternative:* the plugin keeps the previous model only in memory.
- *Example:* the TS plugin idled out after 10 minutes and you then edit
  `package.json`.
  - With the stored blob, the edit is still selective.
  - In memory only, the plugin starts fresh with no "before", so core reindexes
    the whole language.
- *Consequences:*
  - Stored: a new table, a bulk trailer line, and copying at the swap.
  - In memory: simpler, but the saving disappears whenever the plugin slept or
    restarted.
- **Recommended: stored.**

**Q5. Split GM-509?**

- *Today:* one task with 11 provisional slices across core, SDK, TS, Go, Rust
  and Python.
- *Change:* GM-509 keeps wire, core, SDK and TS. Two new tasks take Rust+Python
  and Go.
- *Consequences:*
  - Split: each task has its own verify and lands independently, and a language
    not yet done keeps today's reindex (capability off). Acceptance 3 moves to
    the new tasks.
  - One task: a single release gate, but a longer critical path and one large
    verify.
- **Recommended: split.**

## 10. Owner decisions (2026-10-09)

- **Q1: "A: плагин называет изменённое".** Option A, with the `unchanged`
  answer landing first.
- **Q2: "Добавить колонку specifier".** `edges.specifier`, schema 14; every
  index is rebuilt once at upgrade.
- **Q3: "30% файлов языка, уточнить замером".** Above 30% of a language's
  files, the whole-language reindex runs; the measure slice recalibrates it.
- **Q4: "В индексе core".** The facts the index was built from are stored per
  language in core.
- **Q5: "Разделить, все три в 4.3.0".** GM-509 keeps wire, core, SDK and
  TypeScript. Two new tasks in the 4.3.0 batch take Rust+Python and Go, each
  with its own tests and verify, and each turns on the `resolution_delta`
  capability for its plugins.
