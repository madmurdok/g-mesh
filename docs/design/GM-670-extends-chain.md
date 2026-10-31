# GM-670: tsconfig `extends` chain gaps vs tsc 5.9

Status: design (GM-670/S1), revised for the owner's decisions. Base:
`release-4.21.0` at 5639e35 (GM-666 merged). Line numbers are 1-based at
5639e35.

Owner decisions (verbatim labels):
1. "C: динамический список в core": the plugin declares the exact files of
   its extends chains, and core routes them.
2. "Не следить, задокументировать (Recommended)": package targets under
   `node_modules` stay unwatched, and the gap is documented.
3. and 4. "Сделать и их": self-name extends, absolute entries and non-`.json`
   relative targets are resolved as tsc 5.9 resolves them, and watched.

tsc evidence: `typescript@5.9.3 --showConfig`, scratch cases in the session
scratchpad `gm670-tsc/run{,2,3,4,5,6}.sh`. In each case the candidate files
carry distinct `paths` markers, and the case reports which marker tsc
inherited.

## 1. Today

### 1.1 Who decides that a saved file reloads the TS model

- `core/src/daemon/registry.rs` `PluginRegistry::workspace_language_matches`
  (824-839) decides. A path is a workspace file of a language when its **file
  name alone** matches one of the manifest's static `[plugin.workspace]
  watch_files` globs, and the path is not under that manifest's
  `exclude_dirs`.
- It has 3 callers (g-mesh `find_callers`):
  - `route_settled_path` (1061-1073): on a match the path goes to
    `workspace_file_changed`, then `reindex_workspace`, then
    `config_reindex::run`, which sends `resolutionChanged` with the stored
    facts. Without a match it goes to `file_changed`, and a `.json` file
    with no language is dropped there.
  - `announce_created` (1089-1118): a workspace file is left out of
    `filesCreated`. Its callers are `watch_and_route_once` (mod.rs 744) and
    `reconcile_since_index` (1418), plus 3 tests.
  - `reconcile_watch_files_with_seed` (1582-1640).
- The SDK side is `plugins/sdk/src/run.rs` `Session::resolution_changed`
  (626-661). It reloads the whole model, answers `delta` plus new `facts`
  (wire `ResolutionChangedResult`, `wire/src/lib.rs` 682-690), and the bulk
  walk ends with a `{"resolutionFacts": ...}` trailer (run.rs 208-217).
- Core parses the trailer in `core/src/protocol/ndjson.rs`
  `ResolutionFactsLine` (20-24, `deny_unknown_fields`). It stores the facts
  in the `resolution_facts(language, facts)` table (schema.rs 651-654, via
  `set_resolution_facts` 1198-1206). There are 4 writers (g-mesh
  `find_callers`):
  - `bulk_index::walk_one_language_in` at line 417;
  - `bulk_index::purge_language` at line 304;
  - `config_reindex::selective` at lines 145 and 154;
  - `config_reindex::reextract` at line 507.

  The language swap copies the table too (`language_swap.rs` 644-649).
- TS facts (`project/facts.rs`) hold each config directory's effective
  `paths`. Once the model is reloaded, a change anywhere in a chain shows in
  the delta. The only gap is that core never asks for the reload.

### 1.2 Where extends is resolved

In `plugins/typescript/src/project/tsconfig.rs`,
`TsconfigReader::resolve_effective` (297-343) walks `raw.extends` and takes
the first `extends_candidates(config, entry)` (162-221) that passes
`config_is_file`. `resolve_effective` is the only caller.

What `extends_candidates` does today:
- `.` and `..` become `<dir>/tsconfig.json`.
- `./x` and `../x` become `x`, then `x.json`.
- Rooted entries (absolute paths), entries starting with `.` such as
  `.hidden/x`, and empty entries are dropped.
- A package entry is looked up only as `node_modules/<entry>`,
  `<entry>.json` or `<entry>/tsconfig.json`. `package.json` is never read.

### 1.3 tsc 5.9.3 behaviour (observed)

| case | setup | tsc inherits |
|---|---|---|
| c1 | `pkg/package.json` `tsconfig: ./cfg/base.json`; `pkg/tsconfig.json` too | `cfg/base.json` |
| c12 / a8 | field `./base` / field `./sub` (dir) | `base.json` / `sub/tsconfig.json` |
| c13 | field names a missing file | `pkg/tsconfig.json` |
| c2, a4 | `node_modules/pkg.json` beside `pkg/` (tsconfig.json / field) | `pkg.json` (file before directory) |
| c4 | `exports {"./node": "./cfg/node.json"}`, `pkg/node.json` too | `cfg/node.json` |
| c5, a7 | `exports` present, subpath / `.` not exported, file exists | TS6053 |
| c6, e4 | `exports` and `tsconfig` field | `exports` wins; no `.` key blocks the field |
| b1 | `exports "."` vs sibling `pkg.json` | `exports` |
| b3, d4 | exports target `./e` (no ext) / missing | TS6053 (exact, no fallback) |
| d3 | exports array `[missing, ./tsconfig.json]` | second element |
| a1, a2, c11, d1, d2 | condition objects | first key in declaration order among `node`, `require`, `types`, `default` (`import`, `browser`, `source` never) |
| d5, c17 | wildcard keys | longest prefix |
| c3 | `pkg/sub`: `sub.json` and `sub/tsconfig.json` | `sub.json` |
| c16 | `pkg/sub/package.json` `tsconfig` field | field target |
| a5 | `pkg/x` exact file, no `.json` | TS6053 |
| c14, e1-e3 | nearest `node_modules` fails | next ancestor's |
| c7, d6 | `.hidden/x` | `node_modules/.hidden/x.json`; local never |
| c9, a13, a14 | `main` / `types` / `@types/pkg` | TS6053 (ignored) |
| a10 | `node_modules/cfg` symlink to `packages/cfg` | `packages/cfg/base.json` |
| a11, f2 | self-name `me/b`, nearest `package.json` `name: me` + `exports` (also from a subdir) | export target |
| f1 | self-name, no `exports` (field only) | TS6053 |
| f3 | nearer `package.json` of another name | TS6053 (scope is the nearest package.json only) |
| f4 / f5 | self-name vs `node_modules/me` / self-name subpath not exported | self wins / falls through to `node_modules` |
| a12 | field `../other.json` leaves the package | resolves |
| a15, f6, f7 | absolute `<dir>/base` (file `base` and/or `base.json`) | exact `base`, then `base.json` |
| c18 | `sub/tsconfig.json` `"../"`, `<root>.json` and `<root>/tsconfig.json` | `<root>.json` (= `<parent>.json`) |
| c19 | `./base`: file `base` and `base.json` | exact `base` |

## 2. Design (option C)

### 2.1 Wire: carry the paths beside the facts

New optional field `watchPaths: [string]`, with the same lifetime as the
facts. They describe one model, so they are produced, stored, swapped and
dropped together.

- **Bulk trailer:** `{"resolutionFacts": "...", "watchPaths": [...]}`.
  `ResolutionFactsLine` gains `#[serde(default)] watch_paths: Vec<String>`,
  and `BulkItem::ResolutionFacts(String)` becomes
  `ResolutionFacts { facts, watch_paths }`.
- **`ResolutionChangedResult`:** gains `#[serde(default, skip_serializing_if
  = "Vec::is_empty")] watch_paths: Vec<String>`.

Alternative considered: a new message (`watchPathsChanged`). Rejected. It
can arrive out of order with the facts it belongs to, so core would need a
second consistency rule. The two existing carriers already reach every
point where the model changes, because a model reload happens only in
`load_project` at bulk start or in `resolutionChanged`.

Contract (wire doc): each path is project-relative with forward slashes,
normalized and inside the project. It is not under the plugin's
`exclude_dirs`, and it is never a file of one of the plugin's own
`[plugin.languages] extensions`. Core replaces the language's whole set
with each delivery. Only a `resolution_delta` plugin's values are stored,
since only that path stores facts. Absent means empty.

Compatibility: an older core rejects a trailer with an unknown field
(`deny_unknown_fields`). The SDK writes `watchPaths` only when the list is
non-empty (TS alone), and bundled plugins ship with their core, so no
`protocol_version` bump is needed. A third-party plugin built on a newer SDK
that declares paths needs a core at least this new; the wire doc says so.

### 2.2 Core storage: a table, not memory

New table, in the same DDL block as `resolution_facts`:

```sql
CREATE TABLE IF NOT EXISTS resolution_watch_paths (
    path     TEXT NOT NULL,
    language TEXT NOT NULL,
    PRIMARY KEY (path, language)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_resolution_watch_paths_language
    ON resolution_watch_paths(language);
```

- `set_resolution_facts(conn, language, facts, watch_paths)` deletes the
  language's rows and inserts the new ones in the caller's transaction. With
  `facts == None`, the rows go too. All 4 writers pass the paths they
  received; `purge_language` passes none.
- `language_swap` copies the language's rows from `staging` next to the
  facts (644-649).
- A new `schema::watch_path_languages(conn, path) -> Vec<String>` does a
  keyed lookup.
- `CURRENT_SCHEMA_VERSION` becomes "16", which resets the index on upgrade.
  This costs nothing extra: the TS plugin build changes, so
  `indexer_version` resets the index anyway.

Weighed against in-memory sets rebuilt at daemon start: facts are opaque to
core, so the sets could come back only by asking every `resolution_delta`
plugin at start (spawning TS and loading its model on every start), and
every writer would also have to update the in-memory copy. A table instead
follows the facts through the swap, the seed copy and a kill between
writes. Per-path lookups hit the primary key.

### 2.3 Routing

New `PluginRegistry::watch_languages(&self, conn, file_path) -> Vec<String>`
returns `workspace_language_matches(file_path)` (static, unchanged) plus
`watch_path_languages(conn, file_path)`. The dynamic rows are filtered by
each manifest's `exclude_dirs` and by `is_failed_language`, then sorted and
deduplicated.

| site | change |
|---|---|
| `route_settled_path` (1061-1073) | `watch_languages(conn, ..)` instead of the static matcher |
| `announce_created` (1089-1118) | gains `conn: &IndexStore`, skips `watch_languages` matches; 2 production callers already hold `conn` (mod.rs `watch_and_route_once`, `reconcile_since_index`), 3 tests updated |
| `reconcile_watch_files_with_seed` (1582-1640) | the closure filters by `watch_languages(conn, ..)`; the seeded index's rows are the source's, so a chain file differing from the seed's source triggers like a static one |
| `workspace_language_matches` (824-839) | unchanged (static, still used by the above and by tests) |

A deleted or created chain file is routed the same way. The set includes
missing candidates (section 3.1), so a target created later reloads the
model.

**Stale entries** are dropped by construction: every delivery replaces the
language's set. The set is a projection of the model just loaded, so an
`extends` removed by an edit (which reloads the model, because the editing
config is itself a watch file) drops its paths at that answer. A whole
reindex replaces the set through the swap, a purge deletes it, and an
answer without facts deletes it with them.

### 2.4 plugin-check

- In `checks.rs`, extend the bulk-trailer check: every `watchPaths` entry is
  normalized, relative and inside the workspace, not under `exclude_dirs`,
  and has no extension from `[plugin.languages]`. A violation is a Fail
  naming the entry.
- In `mod.rs` 271 and `checks.rs` 379/542, the pattern matches follow the
  new `BulkItem` shape.
- The chooser (`choose_version_bump`) is unchanged: it bumps a static watch
  file.

### 2.5 Re-point effect on waiting addresses (GM-666)

Nothing new is built. A reload that changes a directory's effective `paths`
yields `Affected` selectors by specifier pattern (`facts::delta`).
`config_reindex::scan_imports` joins `placeholder_targets`, so a waiting
import is matched by its `specifier`, re-extracted, and its
`waiting_address` recomputed under the new config (or linked). W7 pins
this.

## 3. TS plugin

### 3.1 Which paths it declares

`TsProject` gains `watch_paths: BTreeSet<String>`, filled by the
`TsconfigReader`. The set holds every path the extends resolution probed,
found or not, minus paths under `EXCLUDE_DIRS` and paths with a TS source
extension. It also holds the canonical project-relative path of each found
target whose realpath differs and lies inside the root (a10: the save lands
on `packages/cfg/base.json`). It covers:

- relative entries: both candidates (`x`, `x.json`), including an exact
  non-`.json` file (owner item 4: routed by its exact path);
- `.` / `..`: `<dir>/tsconfig.json`, which the static glob already matches
  (harmless duplicate);
- absolute entries inside the root (item 3): their candidates;
- self-name: the export target inside the project, since the `package.json`
  itself is static;
- package targets under `node_modules`: dropped by the `EXCLUDE_DIRS`
  filter (decision 2).

`resolution_facts`/`watch_paths` come from the same `TsProject`, so they
always describe one model.

### 3.2 Resolver (replaces the package branch of `extends_candidates`)

```
resolve_extends(config, entry):
  ".", "..", "./x", "../x"     -> extends_candidates (unchanged order: exact, then .json)
  rooted (POSIX root or drive) -> path inside root ? candidates [p, p.json] (exact first) : None
  ""                           -> None
  else (package name, incl. ".hidden/x"):
    self_name(config_dir, name, rest)              // nearest package.json only (f3)
      -> if its `name` == name and it has `exports`: config_exports_target; on a miss, go on (f5)
    for dir in ancestors(config_dir):              // nearest first, inside root
      pkg = dir/node_modules/name
      found = pkg/package.json has non-null `exports`
                ? config_exports_target(pkg, exports, "." | "./rest")
                : rest == "" ? file(dir/node_modules/name.json) or directory(pkg)
                             : file(rest ends .json ? pkg/rest : pkg/rest.json) or directory(pkg/rest)
      if found: return found                       // else next ancestor (e1-e3)
directory(d): d/package.json `tsconfig` field -> f (exact, else f.json) or f/tsconfig.json;
              missing -> d/tsconfig.json (c13)
config_exports_target: exact key, else the wildcard key with the longest prefix
  (exports::match_wildcard, read-only); string -> exact "./" target that exists;
  array -> first element that resolves; object -> first key in declaration order
  among node/require/types/default, recursive; else None (no fallback in this package).
```

Every result stays inside the project (`paths::escapes`). A target outside
it (an absolute path elsewhere, a field climbing out) is a note, as today.
Absolute handling reuses `own_resolve_dir`'s rule: `\` is a separator, and
a drive-less root takes the root's drive (`with_roots_drive`).

Documented, not built: watching `node_modules` targets (decision 2), and
absolute entries outside the project. Side finding, out of scope:
`TsProject::load` reads `pnpm-workspace.yml`, but `watch_files` lists only
`.yaml`.

## 4. Edit map: three slices, in order

### Slice A: wire + core (no behaviour change until a plugin declares paths)

| file | item | lines | change |
|---|---|---|---|
| `wire/src/lib.rs` | `ResolutionChangedResult` | 682-690 | `watch_paths` field + contract doc |
| `core/src/protocol/ndjson.rs` | `BulkItem::ResolutionFacts`, `ResolutionFactsLine`, `parse` | 14-41 | struct variant with `watch_paths` |
| `core/src/storage/schema.rs` | `CURRENT_SCHEMA_VERSION` + doc | 87-94 | "16" |
| same | DDL beside `resolution_facts` | 645-654 | new table + index |
| same | `set_resolution_facts`; new `watch_path_languages` | 1189-1206 | paths param; lookup |
| `core/src/storage/language_swap.rs` | facts copy | 640-650 | copy the rows |
| `core/src/daemon/bulk_index.rs` | `purge_language` 304; `walk_one_language_in` 417; stream read 496-499 | | pass paths |
| `core/src/daemon/config_reindex.rs` | `selective` 145, 154; `reextract` 507 | | pass the answer's paths |
| `core/src/daemon/registry.rs` | new `watch_languages`; `route_settled_path`; `announce_created` (+conn); `reconcile_watch_files_with_seed` | 824-839 (beside), 1061-1073, 1089-1118, 1582-1640 | section 2.3 |
| `core/src/daemon/mod.rs` | `watch_and_route_once` call of `announce_created` | ~791 | pass `conn` |
| `core/src/cli/plugin_check/{mod.rs 271, checks.rs 379, 542, trailer check ~1060-1085}` | | | new shape + path check |
| `core/src/protocol/conformance.rs` | match at 68 | | new shape |
| `core/src/daemon/test_plugin.rs` | scripted plugin | | can emit `watchPaths` (for core tests) |

### Slice B: SDK

| file | item | lines | change |
|---|---|---|---|
| `plugins/sdk/src/lib.rs` | `trait Extractor`: new defaulted `watch_paths(&self, project) -> Vec<String>` | after 240-260 | default empty; contract doc |
| `plugins/sdk/src/run.rs` | bulk trailer | 208-217 | add `watchPaths` when non-empty |
| same | `Session::resolution_changed` | 626-661 | fill `watch_paths` from the reloaded model (empty on `Unknown`/load failure) |

### Slice C: TS plugin (after GM-669 merges, or rebased on it)

| file | item | lines | change |
|---|---|---|---|
| `plugins/typescript/src/project/tsconfig.rs` | `extends_candidates` | 162-221 | drop the package branch; rooted entries go to the resolver |
| same | `TsconfigReader` + `new` | 258-272 | `watch_paths` field; `into_watch_paths(self)` |
| same | `resolve_effective` | 297-343 | `self.resolve_extends(..)`; record probes |
| same | new `resolve_extends`, `resolve_package_extends`, `self_name_target`, `package_dir_target`, `config_exports_target` | appended after `read_raw` (346-366) | section 3.2 |
| `plugins/typescript/src/project/mod.rs` | `TsProject` struct / `load` | 61-80 / 89-135 | `watch_paths` field, filled from the reader |
| `plugins/typescript/src/extractor/mod.rs` | `impl Extractor` | beside 101-109 | `watch_paths` |

Not touched: `WATCH_FILES`/`plugin.toml` globs, `expand_paths_candidates`,
`specifiers_targeting`, `paths_entries`, `own_resolve_dir`, `resolve.rs`,
`exports.rs`, `facts.rs`. GM-669 changes `expand_paths_candidates` in
`tsconfig.rs` plus `resolve.rs`/`exports.rs`, so the overlap is file-level
only. If GM-669 changes the signature of `match_wildcard`, slice C adapts.
Slices A and B do not touch GM-669's files.

Tests to update: `tests/project_model.rs` `extends_candidates_follow_tscs_lookup`
(1839-1866) and 676-685 (the package rows move to the resolver, and the
`.hidden` and `/etc/base.json` rows change). Also
`a_drive_lettered_extends_entry_is_dropped_like_an_absolute_one` (1884) and
the UNC test (1891): both now apply only outside the root.

## 5. Behaviours

Core / wire (slice A):
- K1: a trailer and a `resolutionChanged` answer with `watchPaths`
  round-trip; with none, they are empty (wire and ndjson units).
- K2: a stored path routes its save to the language's `resolutionChanged`;
  an unstored `.json` file is dropped as today (registry unit with the
  scripted plugin).
- K3: a later delivery replaces the set; an answer without facts or a purge
  clears it (schema unit).
- K4: the language swap carries the set (language_swap unit).
- K5: a created chain file is left out of `filesCreated` and routed as a
  workspace file (daemon unit).
- K6: a seeded start whose chain file differs from the source triggers the
  language (registry unit).
- K7: plugin-check fails a `watchPaths` entry outside the workspace or with
  a source extension.

End to end (`core/tests/it/config_reindex_typescript.rs`):
- W1: the child has `extends: "./base.json"`. Edit the base's `paths`: the
  alias importer is re-pointed, and only it is re-extracted.
- W2: the chain is child -> `./a.json` -> `./b.json`. Editing `b.json`
  re-points.
- W3: `./base.json` is missing at load, then created: the child inherits.
- W4: switch the child's extends from `./a.json` to `./b.json`. A later edit
  of `a.json` re-extracts nothing, and `a.json` is no longer stored.
- W5: `extends: "./base"`, an exact non-`.json` file. Editing it re-points.
- W6: an absolute `extends` inside the root. Editing the target re-points.
- W7: a waiting `@/x` (GM-666). A base edit maps `@/*` to `lib/*`, where
  `lib/x.ts` exists, and the import links.

TS resolver (`tests/project_model.rs`, tsc table):
- P1: `tsconfig` field (c1, c8). P2: `.json` appended, directory form (c12,
  a8). P3: missing field target falls back (c13).
- P4: `<name>.json` before the directory (c2, a4).
- P5: `exports`: subpath (c4); beats and blocks the field (c6, e4, c5, a7);
  exact target (b3, d4); array (d3); conditions (a1, a2, d1); longest
  wildcard (d5).
- P6: `rest.json` before `rest/` (c3); nested field (c16); no exact
  no-extension file (a5).
- P7: nearest `node_modules` first, fall-through (c14, e1, e3).
- P8: `.hidden/x` goes to `node_modules` only (c7, d6).
- P9: `main` / `types` / `@types` are ignored (c9, a13, a14).
- S1: self-name through `exports` from a subdir (a11, f2). S2: no `exports`
  means no self-name (f1). S3: a nearer `package.json` of another name
  blocks it (f3). S4: self-name wins over `node_modules` and falls through
  on a miss (f4, f5).
- A1: absolute inside the root: exact, then `.json` (f6, f7). A2: absolute
  outside the root gives a note and inherits nothing.
- D1: `watch_paths` holds missing relative candidates and the canonical
  path of a symlinked found target (a10, unix only), and nothing under
  `node_modules`.
- R1: `sub/tsconfig.json` `"../"` resolves to `<parent>.json` even when
  `<parent>/tsconfig.json` exists (c18).
- R2: `./base` with files `base` and `base.json` resolves to `base` (c19).

## 6. Controls (8, plus 2 with a reason)

| id | revert (code) | must fail | binary |
|---|---|---|---|
| C1 | `route_settled_path` uses the static matcher only | K2, W1 | core lib + it |
| C2 | `set_resolution_facts` appends instead of replacing | K3, W4 | core lib + it |
| C3 | the swap does not copy `resolution_watch_paths` | K4 | core lib |
| C4 | SDK `resolution_changed` leaves `watch_paths` empty | W2 (the paths are stored by the bulk walk, so the first edit works; a chain changed by an edit then goes unwatched: the test first edits the child to add `./b.json`, then edits `b.json`) | it |
| C5 | TS records only found candidates | W3, D1 | TS project_model + it |
| C6 | `tsconfig` field not read | P1 | project_model |
| C7 | `exports` ignored | P5 | project_model |
| C8 | self-name skipped | S1 | project_model |
| C9* | absolute entries dropped | A1, W6 | project_model |
| C10* | `extends_candidates` tries `x.json` before `x` | R2 | project_model |

\* Reason for 10 rather than 8: the owner made absolute entries a
deliverable (item 3), and R1/R2 are named GM-668/S3 leftovers in this
task's text. C11 (`"../"` read like `".."`, must fail R1) can share C10's
build. C6-C11 sit in one binary (`--test project_model`) as one group, C1-C3
in the core lib, and C1, C2, C4, C5 and C9 also in `--test it`, which is one
build per group.

## 7. Cost and risk

- **Footprint:** wire (one struct, one trailer), core in 8 files plus a
  schema bump, SDK (one trait method, 2 sites), TS (resolver). Roughly 3x
  the TS-only option. Core's routing gains one SQLite point lookup per
  settled path; watcher batches are small, and the lookup hits the primary
  key.
- **Upgrade:** schema "16" resets the index. The TS plugin version bump
  already forces that through `indexer_version`.
- **Compatibility:** an older core with a newer TS plugin rejects the
  trailer (the bulk walk fails). Bundled versions move together; the wire
  doc states the requirement for third-party plugins.
- **Coverage limits:** `node_modules` targets are not watched (decision 2).
  An edit made while no daemon runs is not compared at a plain start; this
  is already true for static watch files (`reconcile_since_index` compares
  indexed files only). A gitignored chain file is not watched: the watcher
  drops every event for a path a `.gitignore` ignores before routing
  (`ProjectWatcher::next_change` and `retain_unignored`, both through
  `IgnoreLayers::is_ignored`), and a seeded start's comparison walks with
  `.gitignore` honoured too (must-confirm 3).
- **Case-insensitive file systems:** a missing candidate is declared as
  spelled in `extends`. On macOS, a later file created with a different
  case does not match the stored key (must-confirm 4). Found targets are
  declared by realpath, so this affects only targets created after the load.

## 8. Must-confirm

1. Carry `watchPaths` on the existing trailer and `resolutionChanged`
   answer, not a new message (section 2.1).
2. A table plus schema "16", not in-memory sets (section 2.2).
3. Does the watcher deliver settled paths in gitignored directories? If
   not, document that gitignored chain files are not watched.
4. Case-insensitive FS: accept that a missing candidate created later in a
   different case is missed, or lowercase the stored keys on
   case-insensitive volumes (more core code).
5. Slice C waits for GM-669's merge (file-level overlap in `tsconfig.rs`).
