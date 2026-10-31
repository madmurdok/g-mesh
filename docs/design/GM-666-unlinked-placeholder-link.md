# GM-666: link an unresolved alias, workspace or `#` import when its first target is created

Status: design (S1). No production or test change in this slice. Line
numbers are at `f467fbb` (release 4.21.0; GM-661 merged).

## The problem in one example

`tsconfig.json` declares `"paths": { "@/*": ["src/*", "generated/*"] }`.
`src/a.ts` has `import { f } from "@/x"`. Neither `src/x.ts` nor
`generated/x.ts` exists, so the import resolves to nothing.

Create `generated/x.ts`. TS now resolves `@/x` to it. g-mesh leaves the
import unlinked until `src/a.ts` is edited or TypeScript is reindexed.

The same happens for a workspace package (`import "@scope/p"` before
`packages/p/src/index.ts` exists) and for a `#` specifier
(`"imports": { "#x": "./src/x.ts" }` before `src/x.ts` exists).

## 1. Today (with the code path)

### How an unresolved import is stored

`extractor::imports::Declarer::record_specifier`
(`plugins/typescript/src/extractor/imports.rs` 201-222) asks the
`SpecifierResolver` (`TsProject::resolve`). On a miss:

- a relative specifier gets an **address**, the path it names before any
  extension is tried (`relative_address`, resolve.rs 281-284): `./x` in
  `src/a.ts` -> `src/x`. The placeholder is a `resolved_module` with
  target `{file: "src/x", name: "*"}`; core stores the scope in
  `placeholder_targets` and leaves the edge unlinked.
- **any other specifier** gets no address: an `external_module` named by
  the raw specifier, with no target. Core never links one
  (`graph::imports::EXTERNAL_MODULE_NATIVE_KIND`, core/src/graph/imports.rs
  115, and the conformance exemption, core/src/protocol/conformance.rs
  15-25).

### How a created file re-selects importers

1. `TsProject::source_changed` (`plugins/typescript/src/project/mod.rs`
   179-201) runs once per path per model (`announced`). It returns
   `ResolutionDelta::Affected` with one `ImportMatch::Target { File,
   Exact(key) }` selector per key: the file's addresses
   (`addresses_resolving_to`, resolve.rs 154-174), the relative candidates
   it shadows (`candidates_shadowed_by`, 186-201) and, since GM-661, the
   alias/package/`#` fallbacks it shadows (`fallbacks_shadowed_by`,
   222-276).
2. Core: `watcher::apply::apply_file_change_in` (core/src/watcher/apply.rs
   234-300) -> `reextract_affected` (367-420) ->
   `daemon::config_reindex::select_with` (config_reindex.rs 205-228) ->
   `select_affected` (243-275).
3. `select_affected` answers an `Exact` `Target` selector with
   `indexed_importers` (306-348): keyed probes on `idx_targets_scope`
   (unlinked placeholders waiting at that scope) and `idx_nodes_filePath`
   (edges linked to that file). **Every other selector** (`Specifier(..)`,
   `Under`, `StartsWith`) goes to `scan_imports` (350-404), one read of
   every `IMPORTS` edge of the language. GM-655 measured that scan at
   159 ms user / 287 ms real on a 2.7k-import (rust-sized) index and moved
   creates off it on purpose.
4. `select_with` turns a selection above 30% of the language's indexed
   files (`FALLBACK_SHARE_PERCENT`, config_reindex.rs 61) into
   `Selection::TooMany` -> language reindex.

### Why the waiting import is missed

The only thing a create can name is a `Target` key, and an
`external_module` has no target, so no key reaches it. `fallbacks_shadowed_by`
already derives exactly the specifiers that can resolve to the new file
(`@/x` from `generated/x.ts`), but it names only the previous winner, and
there is none.

Note: in the simplest case the gap is already half-closed by accident of
address arithmetic: if the alias miss stored the address `src/x`, the
existing `addresses_resolving_to(src/x.ts)` key `src/x` would select it.
That does not cover a later base (`generated/x.ts`), a workspace entry or
a `#` target.

## 2. Options

### (A) Give an alias/package/`#` miss an address, and name it on create (recommended)

**Today:** a non-relative miss is an `external_module` with no target.

**Change:**

- A new `resolve::waiting_address(project, specifier, config, imports) ->
  Option<String>`: the **first base** of the list the resolver would walk
  for `specifier`, from the same list functions `resolve` uses:
  - `#x` -> `imports_targets(imports, specifier)[0]`;
  - a bare specifier naming a workspace package ->
    `package_entry_targets(package, subpath)[0]` (workspace is tried
    before `paths` in `resolve`, so its list comes first);
  - otherwise -> `expand_paths_candidates(config, specifier)[0]`;
  - `None` when no family matches (an npm package, a builtin): stays an
    `external_module`, as today. Also `None` when the only matching
    `paths` entry is the catch-all `"*"` (decision D1).
- Record side: `record_specifier`'s miss branch becomes
  `relative_address(..).or_else(|| miss_address(specifier, path))`, where
  `miss_address` is `waiting_address` with the importer's nearest config
  (`tsconfig_for` / `imports_for`). An address makes the placeholder a
  `resolved_module` with target `{file: <base>, name: "*"}`, exactly like
  a relative miss. No bindings are recorded (the specifier did not
  resolve), as for a relative miss.
- Create side: a new `resolve::waiting_addresses_for(project, path)`
  reuses `fallbacks_shadowed_by`'s inversion (every config's
  `specifiers_targeting`, every package's `subpaths_targeting`, every
  `imports` map's `imports_specifiers_targeting`, over `path` and its
  addresses) and, per derived specifier, names `waiting_address` under
  that config. `source_changed` adds those as extra `Exact` keys.

Because both sides compute the key with one function from the same
config, they cannot drift; the create side asks every config (as GM-661
D1), so whichever config governed the importer is among them.

**Example:** `@/x` in `src/a.ts` is stored with target `src/x`. Create
`generated/x.ts`: `specifiers_targeting(config, "generated/x")` gives
`@/x`; `waiting_address` gives `src/x`; the key `src/x` hits
`idx_targets_scope`, selects `src/a.ts`; its re-extract resolves `@/x` to
`generated/x.ts` and records the bindings.

**Benefit:** keyed lookups only, no import scan, no threshold risk: the
selection is the importers waiting at that address (plus the importers
already linked to a file at that exact path, normally none since a base
is extensionless). Works the same for all three families. Same shape as
the relative case, which core already handles.

**Risk:**

- Touches the extractor: an alias miss changes from `external_module` to
  an unlinked `resolved_module`. Visible in `deps` export (it moves from
  the `external` tally to `unresolved`, core/src/cli/deps.rs 493-498) and
  anywhere that switches on the kind (`queries::NON_DECLARATION_NATIVE_KINDS`
  already lists both, so lookups are unaffected). Arguably more correct:
  `@/missing` is a project path that does not exist, not a package.
- Existing indexes keep the old target-less placeholders until the
  importer is re-extracted or the language reindexed (must-confirm M2).
- A workspace package's first base is often a build-output entry
  (`main: dist/index.js`), so the stored address can name a file that
  never exists. Harmless: it is only a key; the create side computes the
  same base. If a build does create `dist/index.js`, core links it by
  path equality, which is also what the resolver would pick (`main`
  precedes `src/index`).

### (B) Specifier selector scoped by alias prefix / package name (GM-661 option B)

**Change:** on a create, return `ImportMatch::Specifier(StartsWith("@/"))`
for every alias whose targets can name `path`, `Exact(name)` per package.

**Benefit:** plugin-only, no extractor or stored-data change.

**Risk:** selects every importer of the alias, linked or not. In a
project that imports `@/` from most files, the first sighting of any file
under `src/` selects most files and crosses the 30% threshold into a
language reindex: the acceptance fails exactly where it matters. Also
always an import scan.

### (B') Exact specifier selector per derived specifier

**Change:** `source_changed` adds `ImportMatch::Specifier(Exact(spec))`
for each specifier `fallbacks_shadowed_by`'s inversion derives (`@/x`,
`@/x.js`, ...), optionally only when no candidate of that specifier
existed before (so the importers are all waiting ones).

**Benefit:** plugin-only (resolve.rs + one line in mod.rs); works on
existing indexes without a re-extract; selection is narrow.

**Risk:** every `Specifier` selector costs one `scan_imports` (there is
no index on `edges.specifier`, core/src/storage/schema.rs 326-337). In an
alias project nearly every new file is the first target of `@/<name>`, so
every create, and after a restart the first edit of every file, pays a
full import scan: 159 ms user on a 2.7k-import index, seconds on a
20k-file repo. That re-introduces the cost GM-655 removed. Without the
"nothing existed before" filter, a new lower candidate of a widely
imported specifier (`src/utils/index.ts` beside `src/utils.ts`) selects
all its linked importers and can cross 30% too. A core index on
`edges.specifier` would fix the cost but is a schema change and a core
change.

### (C) Answer `Unknown` when the new file is any alias target

**Change:** `source_changed` returns `ResolutionDelta::Unknown`.
**Risk:** a language reindex per such create (apply.rs 270-274).
Violates the acceptance. Listed for completeness.

**Recommendation: (A).** It keeps creates on keyed lookups, which is
the property GM-655 bought, and needs no core change. (B') is the
fallback if the owner rejects changing the stored kind of alias misses
(D2).

## 3. Decisions

- **D1. Catch-all `"*"` gives no address.** Today
  `"paths": { "*": ["src/types/*"] }` matches every bare specifier, so
  under (A) `react` would turn from `external_module` into an unresolved
  `resolved_module` at `src/types/react`. tsc treats `"*"` as a lookup
  before `node_modules`, not as "this is a project module". Change: a
  specifier whose only matching entry is the pattern `"*"` stays external.
  Example: `react` under `"*"` stays `external_module`; creating
  `src/types/react.ts` does not link it (it would be linked after the
  importer's next edit, as today). Consequence: a catch-all `paths`
  project keeps today's gap for its catch-all, in exchange for not
  reclassifying every npm import. Recommended.
- **D2. Accept the kind change of alias misses** (`external_module` ->
  unresolved `resolved_module`) in `deps` and other outputs. Required by
  (A); if rejected, take (B') with its scan cost.
- **D3. Include `#imports` and workspace packages** (acceptance names
  both): yes, same function, one branch each.
- **D4. Extractor plumbing: a second closure, not a new resolver return
  type.** `Declarer` gets an optional `miss_address:
  &dyn Fn(&str, &RelPath) -> Option<String>` (default: none) through a
  setter, so the two test constructors (`imports/tests.rs::with_imports`,
  `tests/imports.rs::extract`) and `SpecifierResolver` stay unchanged.
  Alternative: change `SpecifierResolver` to return
  `File | Waiting(address) | External`; cleaner, but touches every
  resolver in tests. Recommended: the setter.

## 4. Cost per created file in a project using the alias widely

Setup: 2,000 TS files, every one importing several `@/...` specifiers
(linked), one waiting `@/new`.

- **(A):** per first sighting, `waiting_addresses_for` adds one key per
  (config x derived specifier), typically 1-4 for `@/*` (the address set
  of `path` is 2-3 entries and they mostly give the same base). Each key
  is one `indexed_importers` call: 0.35 ms real / 0.2 ms user on the
  GM-655 TS index (that table's floor is process start). Plugin side: the
  inversion GM-661 already runs, plus one `waiting_address` per
  specifier (a list function and a `[0]`). Re-extracts: only importers
  waiting at that address (here 1). No scan, no threshold exposure.
  Restart: each file's first event costs the same few probes.
- **(B):** one scan (O(import edges)) plus ~2,000 selected -> TooMany ->
  language reindex.
- **(B'):** one scan per create: 1.1 ms on 37 import rows, 159 ms user on
  2.7k rows (GM-655 table), linear in import edges.

**How S5/verify measures it:**

1. Core integration test (behaviour 14): a generated fixture of ~40
   files each importing `@/m<i>` (all linked) and one importer of
   `@/new`; create `src/new.ts`; assert the outcome is `Applied {
   reextracted: 1 }` and the import is linked. Under (B) it is
   `ReindexLanguage`; that is the cheap observation telling the arms apart.
2. Plugin unit test (behaviour 9): every selector `source_changed`
   returns is `Target { File, Exact }`, i.e. no selector that would make
   core scan.
3. Optional timing (verify, not CI): the same fixture at 2,000 files,
   `uptime` first, then `/usr/bin/time -p` around one create applied
   through the harness, reporting `user`/`sys` against `real`; expect the
   create within a few ms of the same create with no waiting import.

## 5. Edge cases

- **Several aliases mapping to one dir** (`@/*` and `~/*` -> `src/*`):
  the inversion derives both `@/x` and `~/x`; both give the base `src/x`,
  so one key selects both importers. Pinned by behaviour 15.
- **Several targets** (`["src/*", "generated/*"]`): the address is the
  first base whichever base the new file is in; the create of a later
  base still names it (behaviour 11). A later create of the earlier base
  is GM-661's re-point.
- **Higher-priority entry still wins:** `"@/x": ["special/x"]` and
  `"@/*": ["src/*"]` both match `@/x` (`expand_paths_candidates` walks
  every matching entry in declaration order). If `special/x.ts` exists,
  `@/x` is linked, not waiting, and creating `src/x.ts` changes nothing
  (no placeholder waits at `special/x`; the linked edge's file is not a
  key). If neither exists, the address is `special/x`; creating
  `src/x.ts` derives `@/x` from the `@/*` entry, whose `waiting_address`
  under the same config is `special/x`: selected, re-extract links it to
  `src/x.ts` (correct by our resolver's order).
- **Workspace package with an alias of the same name:** `resolve` tries
  the package first, so the address is the package's first entry even
  when the created file is the alias target; the tsconfig branch of the
  create side calls the same `waiting_address`, which checks the package
  first. Pinned by a unit test (behaviour 7 variant).
- **Workspace package whose entries point into `dist/`:** see risk above.
- **`#imports` wildcard** (`"#internal/*": "./src/internal/*.ts"`): the
  base has an extension (`src/internal/x.ts`), equal to the created
  path. Core links an address equal to a new file's path by itself; the
  key also selects the importer for a re-extract so its bindings are
  recorded (must-confirm M4).
- **Nested configs** (`apps/web/tsconfig.json` and root both `@/*`): the
  importer's nearest config fixes the address (`apps/web/src/x`); the
  create side asks every config, so it is among the keys (behaviour 8).
  Extra keys from other configs probe empty scopes.
- **npm packages and builtins:** no family matches, still
  `external_module` with no target (behaviour 2).
- **Batch create of two candidates:** each file's own event computes the
  same key; the first re-extract links to whichever exists and wins.
- **Delete of the linked target later:** GM-659's gone-file path
  re-extracts the importer; it goes back to an addressed placeholder,
  which a later create links again.

## 6. Edit map

Change:

- `plugins/typescript/src/project/resolve.rs`
  - new `pub fn waiting_address(project: &TsProject, specifier: &str,
    config: Option<&EffectiveConfig>, imports: Option<&PackageImports>) ->
    Option<String>` next to `resolve` (316-327); D1's catch-all rule.
  - new `pub fn miss_address(project: &TsProject, specifier: &str, from:
    &RelPath) -> Option<String>`: `None` for a relative specifier, else
    `waiting_address` with `tsconfig_for`/`imports_for` of `from`'s dir
    (`importer_dir`, 124-131).
  - new `pub fn waiting_addresses_for(project: &TsProject, path: &str) ->
    Vec<String>` after `fallbacks_shadowed_by` (205-276). Factor the
    specifier derivation of `fallbacks_shadowed_by` (configs dedup,
    the three inversions) into one private helper both call, so the two
    cannot derive different specifier sets.
- `plugins/typescript/src/extractor/decls.rs` — `Declarer` (51-63): field
  `miss_address: Option<&'a MissAddress<'a>>`; `new` (68-85) sets `None`;
  new setter `with_miss_address`.
- `plugins/typescript/src/extractor/imports.rs`
  - `pub type MissAddress<'r>` next to `SpecifierResolver` (49-50).
  - `record_specifier` (201-222): the miss branch adds
    `.or_else(|| self.miss_address.and_then(|f| f(specifier, self.path)))`.
  - module header (5-13): a non-relative miss of an alias, workspace
    package or `#` key is a `resolved_module` targeted at its first base.
- `plugins/typescript/src/extractor/mod.rs` `extract` (113-133): a
  `miss` closure over `resolve::miss_address(project, ..)` passed with
  `with_miss_address`.
- `plugins/typescript/src/project/mod.rs` `TsProject::source_changed`
  (179-201): one line, `keys.extend(resolve::waiting_addresses_for(self,
  path.as_str()))`, and one doc paragraph in 156-178.

Footprint on files GM-669/GM-670 later touch:
`project/tsconfig.rs` — **none**. `project/mod.rs` — one line in
`source_changed` plus its doc comment; nothing in `load`,
`tsconfig_for`, `imports_for` or the struct.

No wire, SDK, core or schema change.

Read for context: `fallbacks_shadowed_by`, `displaced_by` (114-122),
`expand_paths_candidates` (tsconfig.rs 96-130), `specifiers_targeting`
(tsconfig.rs 138), `package_entry_targets` (workspace.rs 135),
`subpaths_targeting` (179), `package_may_name` (207),
`parse_bare_specifier` (89), `imports_targets` (exports.rs 86),
`imports_specifiers_targeting` (127); core `indexed_importers`
(config_reindex.rs 306-348).

Tests to extend: `plugins/typescript/tests/resolution_delta.rs`
(GM-661's alias fixtures, 733-1047); a record-side test in
`plugins/typescript/tests/project_model.rs` or `tests/imports.rs` with a
real `TsProject`; `core/tests/it/created_file_links_waiting_import.rs`
(`indexed_with_configs` 87, `create` 108,
`assert_only_the_importer_re_extracted` 113).

## 7. Behaviours for tests

Plugin, record side:

1. Unresolved `@/x` under `["src/*","generated/*"]` is a `resolved_module` targeted at `src/x`, with no bindings.
2. Unresolved `react` with no matching alias stays `external_module` with no target.
3. `react` under a catch-all `"*"` entry stays `external_module` (D1).
4. Unresolved `@scope/p` (package without an existing entry) is targeted at `package_entry_targets(p, ".")[0]`, also when an alias of the same name exists.
5. Unresolved `#x` is targeted at `imports_targets(config, "#x")[0]`.

Plugin, create side (`resolution_delta.rs`):

6. New `generated/x.ts` (later base) offers the key `src/x`.
7. New `packages/p/src/index.ts` offers the package's waiting address.
8. Nested configs: new `apps/web/src/x.ts` offers `apps/web/src/x`.
9. Every selector `source_changed` returns is `Target { File, Exact }`.

Core integration (real plugin):

10. Waiting `@/x`, create `src/x.ts`: linked; only the importer re-extracted.
11. Waiting `@/x`, create `generated/x.ts`: linked; only the importer re-extracted.
12. Waiting `@scope/p`, create `packages/p/src/index.ts`: linked.
13. Waiting `#x`, create its target: linked, bindings recorded.
14. Widely used alias (~40 linked `@/` importers) plus one waiting `@/new`: create `src/new.ts` is `Applied { reextracted: 1 }`, not a reindex.
15. `@/x` and `~/x` both waiting on `src/*`: one create links both.
16. A waiting `@/y` is not re-extracted by the create of `src/x.ts`.

## 8. Suggested controls (7)

| # | revert (code only) | must fail |
|---|---|---|
| C1 | `record_specifier` ignores `miss_address` | 1, 4, 5, 10, 11, 12, 13 |
| C2 | drop the `waiting_addresses_for` extend in `source_changed` | 6, 7, 8, 11, 12 (10 still passes through `addresses_resolving_to`, by design) |
| C3 | `waiting_address` returns the last base instead of the first | 1 (both sides agree, so only the stored-value test catches it) |
| C4 | `waiting_address` skips the workspace branch | 4, 12 |
| C5 | `waiting_address` returns `None` for `#` specifiers | 5, 13 |
| C6 | catch-all `"*"` not excluded | 3 |
| C7 | create side asks only the root config | 8 |

Behaviours 9, 14, 15 and 16 have no revert of their own: 9 and 14 tell
(A) from (B) (an arm comparison, not a revert of this fix); 15 and 16 are
exercised by C2's failures. C1/C2 land in both the plugin and the core
binaries: group per binary in one build.

## 9. Must confirm

- **M1.** D2: accept that alias/package/`#` misses become unresolved
  `resolved_module`s (`deps` export: `external` -> `unresolved`). If no,
  fall back to (B').
- **M2.** Existing indexes: old target-less placeholders are not fixed
  until the importer is re-extracted or the language reindexed. Is a
  forced TS reindex on upgrade wanted (is there a plugin-version
  trigger?), or is "fixed on next edit/reindex" acceptable?
- **M3.** D1 (catch-all `"*"` stays external) and D4 (setter, not a new
  resolver return type).
- **M4.** Core ordering when the address equals the created path
  (`#internal/*` with `.ts` targets): confirm the direct link and the
  keyed re-extract both happen, so bindings are recorded (behaviour 13
  pins it).
- **M5.** `find_definition`'s import note for a scoped package
  (`core/src/mcp/find_definition/tests.rs::a_scoped_package_keeps_its_import_note`)
  keys on `external_module`: check that an alias miss showing as
  `resolved_module` keeps a sensible answer.

## Cross-file answers relied on

| question | answer | g-mesh call |
|---|---|---|
| who calls `select_with` | `select` (config reload), `reextract_affected` (watch path), one core unit test | `find_callers(select_with)` |
| how keys are produced for creates | `TsProject::source_changed` is the only caller of `fallbacks_shadowed_by` (+ `resolution_delta.rs`) | `find_callers(fallbacks_shadowed_by)` |
| how a created file triggers re-resolution | `apply_file_change_in` is the only caller of `reextract_affected` | `find_callers(reextract_affected)` |
| where `external_module` placeholders are created | only `Declarer::record_specifier` (plugin const `extractor::keys::EXTERNAL_MODULE_NATIVE_KIND`) | `find_references(id of plugin const)`, `find_definition(Declarer::record_specifier)` |
| where they are consumed | core `queries::NON_DECLARATION_NATIVE_KINDS`, `cli::deps::export`, `plugin_check::checks::is_placeholder`, find_definition tests | `find_references(id of core const)` |
| who constructs `Declarer` (plumbing cost of D4) | `TypeScriptExtractor::extract`, `imports::tests::with_imports`, `tests/imports.rs::extract` | `find_callers(Declarer::new)` |
| who uses `SpecifierResolver` | `extractor/decls.rs` only | `find_references(SpecifierResolver, answer=files)` |
| who calls `TsProject::resolve` | `extract`, `tests/project_model.rs::resolve` | `find_callers(TsProject::resolve)` |
| selector cost (scan vs keyed) | read in one known file: `select_affected`, `indexed_importers`, `scan_imports` | `find_definition(select_with)` + `sed` on config_reindex.rs |
