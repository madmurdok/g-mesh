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

## 10. Addendum (S6): own-path keys in core

Owner decision (review finding F2): "Правка в ядре (Recommended)". This
section supersedes §6's "No wire, SDK, core or schema change": core
changes, the wire and schema do not.

### Today, with an example

`tsconfig.json`: `"paths": {"@/*": ["src/*"]}`. `src/hub.ts` exists and
40 of 100 TS files import it as `@/hub`. The plugin restarts, so
`announced` is empty. The user saves `src/hub.ts`:

1. `TsProject::source_changed` offers `Target{File, Exact(k)}` for
   k in {`src/hub`, `src/hub.js`, `src/hub.ts`}. `src/hub.ts` comes from
   `waiting_addresses_for` (derived `@/hub`, whose waiting address is
   `src/hub.ts`), and it equals the edited file's own path.
2. `round_trip` commits the diff and links it
   (`Writer::apply_file_diff_linked` -> `apply_and_link` ->
   `link_applied` -> `imports::link_diff`). Nothing is repointed.
3. `apply_file_change_in` -> `reextract_affected` -> `select_with` ->
   `select_affected` -> `indexed_importers(File, "src/hub.ts")`. Its half
   (b) (linked edges into the `File` node at that path) returns all 40
   importers. 40 > 30% of 100, so `Selection::TooMany`, and the whole TS
   language is reindexed. This happens once per hub file per restart.

Before a062632 no TS key was ever the trigger's own path:
`addresses_resolving_to`, `candidates_shadowed_by` and
`fallbacks_shadowed_by` only name other paths.

### The change

When `select_affected` meets a `Target{File, Exact(key)}` selector whose
`key` is the trigger's own path, it skips `indexed_importers`. It selects
the trigger's **waiting importers** instead: the importers whose
`IMPORTS` edges pointed at a file-scoped placeholder with scope == trigger
path **just before this change's link pass**, read inside the same
commit. Each one is still filtered by `selector.importers`. Any other key
keeps both halves, unchanged.

Why this set: `link` repoints exactly the edges into placeholders whose
scope equals a `File` node the diff upserted
(`link_diff`'s `added_files` lookup, then `find_file` + `repoint`). So the
pre-link half (a) of the trigger's path = (post-link half (a)) ∪ (edges
repointed by this pass). This is the owner's rule, computed with one
indexed read (`idx_targets_scope`, the same index `link_diff` already hits
per added file) and no change to `graph::imports`. The read follows the
gone-file precedent: `file_rows::importers_of` is also read inside
`apply_file_diff_linked`'s commit, before the write.

Rejected alternative: return the repointed importers from
`imports::link`. This needs a new `LinkSummary` field (12 `assert_eq!`
literals in `graph/imports.rs` tests), and `link_applied`/`apply_and_link`
would have to return it through `apply_diff_linked` and
`apply_semantic_diff_linked`, which do not use it. It gives the same
answer.

### How core identifies an own-path key

Core identifies it by plain string equality `key == trigger`. The
trigger is `apply_file_change_in`'s `file_path`, which `select_with`
already receives as `trigger: Option<&str>` and removes from the
selection. The key must also have `scope_kind == File` and
`Matcher::Exact`. The config-reload path (`selective` -> `select`) passes
`trigger: None` (`config_reindex.rs` 152), so it never takes the new
branch. Container keys never take it either.

### F2 after the change

Same example. The waiting read at `src/hub.ts` is empty, because every
`@/hub` import was linked when its importer was extracted. The own-path
branch therefore selects nothing. `src/hub` and `src/hub.js` have neither
a placeholder nor a `File` node. Result: `Selection::Nothing`, and the
edit is `Applied { reextracted: 0 }`.

### M4 still works (walk-through)

`package.json` `"imports": {"#internal/*": "./src/internal/*.ts"}`.
`src/app.ts` has `import {f} from "#internal/a"` while
`src/internal/a.ts` is missing. The record side stores a
`resolved_module` placeholder with target `File` scope
`src/internal/a.ts`: the first candidate equals the path (§9 M4).

1. Create `src/internal/a.ts`. The plugin answers with the diff and the
   keys {`src/internal/a`, `src/internal/a.js`, `src/internal/a.ts`}.
   The last key is the own path, derived from `#internal/a`.
2. `apply_file_diff_linked` (scope `Complete`) reads the waiting importers
   at `src/internal/a.ts`: [(`typescript`, `src/app.ts`)]. `apply_and_link`
   then runs `link_diff`, whose file trigger repoints `src/app.ts`'s edge
   onto the new `File` node and drops the placeholder. Both happen in one
   commit.
3. `round_trip` returns `waiting_importers = [src/app.ts]` (same
   language). `reextract_affected` passes it to `select_with`.
4. `select_affected`: the own key selects `src/app.ts`. The other two keys
   find neither a placeholder nor a `File` node. Selection
   {`src/app.ts`}, which is under the threshold. `src/app.ts` is
   re-extracted, and its bindings for `f` are recorded (behaviour 13).

F3 (`"@env/*": ["src/env.ts"]`) takes the same route once the plugin
offers `src/env.ts` (fix below).

Crash window: a daemon killed between the commit (link done) and
`owe_reextracts` in `reextract_affected` loses the re-extract. This window
already exists for every `affected` selection. Before this change, the
next edit after a restart happened to cover it through half (b). Now it
does not (must-confirm M7).

### Effect on other plugins

| plugin | `Target` selectors sent | own-path File key? |
|---|---|---|
| python (`project/facts.rs` 152, 223) | `Target{Container, Exact(parent/key)}` | no |
| rust (`project/facts.rs` 135, 206) | `Target{Container, Exact(parent/key)}` | no |
| sdk `container_delta` (`resolution.rs` 48) | `Target{Container, Exact(key)}` | no |
| go (`facts.go` 246-250) | `specifierUnder` only | no |
| typescript (`project/mod.rs` 201) | `Target{File, Exact(address)}` | yes, only `waiting_addresses_for` (since a062632) and F3's static targets |

No other plugin is affected. Within TS, only the keys a062632 added change
behaviour.

### Edit map

Core:

- `core/src/storage/index_store.rs` `Writer::apply_file_diff_linked`
  (473-506). Return a struct `LinkedFileDiff { gone_importers:
  Vec<String>, waiting_importers: Vec<(String, String)> }` instead of
  `Vec<String>`. When `scope != Gone`, read the waiting importers before
  `widen`/`apply_and_link` with a new `file_rows::waiting_importers(conn,
  file_path)`. Update the callers: `watcher::apply::round_trip` (973),
  `daemon::registry::PluginRegistry::retire_file` (1556-1560),
  `daemon::bulk_index::purge_language` (300), and the
  `index_store::tests` sites (781, 914, 938, 976, 1240).
- `core/src/storage/file_rows.rs`: new `waiting_importers` next to
  `importers_of` (116-139). It returns `(language, path)` pairs, `SELECT
  DISTINCT src.language, src.filePath FROM placeholder_targets pt JOIN
  edges e ON e.toId = pt.nodeId AND e.kind = 'IMPORTS' JOIN nodes src ON
  src.id = e.fromId WHERE pt.scopeKind = 'file' AND pt.scope = ?1 AND
  src.filePath <> ?1`.
- `core/src/watcher/apply.rs`:
  - `RoundTrip` (847-864): field `waiting_importers: Vec<String>`
    (plugin language only, like `gone_importers`).
  - `round_trip` (938-1011): fill it from `LinkedFileDiff`.
  - `apply_file_change_in` (234-348): for a re-extract round trip, pass
    an empty list, like `gone_importers` (behaviour 9). Otherwise pass
    `structural.waiting_importers` to `reextract_affected`. The "nothing to
    do" test (277) is unchanged: the list only matters through an
    own-path key.
  - `reextract_affected` (367-429): new parameter `own_waiting:
    &[String]`, passed to `select_with`. Doc paragraph.
- `core/src/daemon/config_reindex.rs`:
  - `select` (191-199): passes `&[]`.
  - `select_with` (205-229): new parameter `own_waiting: &[String]`.
    It passes `trigger.map(|t| (t, own_waiting))` to `select_affected`.
  - `select_affected` (243-275): new parameter `own: Option<(&str,
    &[String])>`. In the `Target{scope_kind: File, Exact(key)}` arm,
    `Some((t, w))` with `key == t` inserts `w` filtered by
    `selector.importers` and does not call `indexed_importers`. Doc
    paragraph.
  - `indexed_importers` (306-346): unchanged.
  - `config_reindex/tests.rs`: callers `select` (124), 334, 358, 393 pass
    `None`. `select_with`'s test at 452 passes `&[]`.

Plugin (F1, F3, from gm666-s3.md):

- `plugins/typescript/src/project/resolve.rs` `waiting_addresses_for`
  (260-282):
  - **F1**: drop every address `a` with `a != path &&
    project.existence.contains(a)`. Never drop `path` itself, which the
    core rule now makes cheap.
  - **F3**: also offer the resolved static target of every wildcard
    `paths` entry whose target has no `*`, and of every `imports` key with
    a static target, when that target is `path` or one of
    `addresses_resolving_to(path)`. Put this in a private helper next to
    `derive_specifiers` (308). Enumerate the entries as
    `tsconfig::specifiers_targeting` (138-160) and
    `exports::keys_targeting` (152) do. Those functions stay unchanged:
    they serve `fallbacks_shadowed_by` too.
- `plugins/typescript/src/project/mod.rs` `source_changed` doc (156-178):
  one sentence saying that an own-path key selects only the importers
  that were waiting at it (core).

### Behaviours tests must pin

Numbering continues §7.

17. (core unit, `config_reindex/tests.rs`) A `Target{File, Exact(t)}` key equal to the trigger `t`, with linked importers of `t` and an empty `own_waiting`, selects nothing.
18. (core unit) The same key with `own_waiting = [x]` selects exactly `x`, still filtered by the selector's `importers` scope.
19. (core unit) A `File` key that is not the trigger still selects linked importers through half (b) (existing `an_exact_target_delta_runs_no_scan_and_selects_what_the_scan_does` keeps passing).
20. (core unit, `index_store` tests) `apply_file_diff_linked` reports the importers waiting at the file's path as read before the link, and none for `Gone`.
21. (F2, core integration) After a plugin restart, the first edit of an `@/`-imported hub file (more than 30% of files import it) is `Applied { reextracted: 0 }`, not `ReindexLanguage`.
22. (M4, core integration) Behaviour 13 unchanged: a waiting `#x` with a `.ts` target, then create: linked, importer re-extracted, bindings recorded.
23. (F3, core integration) A waiting `@env/a` under `"@env/*": ["src/env.ts"]`, then create `src/env.ts`: linked, importer re-extracted, bindings recorded.
24. (F1, plugin `resolution_delta.rs`) Creating `packages/p/src/other.ts` in a package whose entry `src/index.ts` exists offers no key `packages/p/src/index.ts`.
25. (F1, plugin) Under `@/*: ["src/*","generated/*"]` with an existing `src/x.ts`, creating `generated/x.ts` offers no key `src/x.ts`.
26. (F1, plugin) The filter keeps `path` itself, and an address created in the same batch is still offered by its own event.
27. (F3, plugin) Under `"@env/*": ["src/env.ts"]` (and an `imports` key with a static target), creating `src/env.ts` offers `src/env.ts`.

### Controls (whole task: 8)

§8 had 7. This addendum keeps C1, C2, C4, C5 and drops C3, C6, C7.
Those three pin single-branch details (first vs last base, catch-all, root
config only) that behaviours 1, 3 and 8 still pin without a control.

| # | revert (code only) | must fail |
|---|---|---|
| C1 | `record_specifier` ignores `miss_address` | 1, 4, 5, 10-13 |
| C2 | drop the `waiting_addresses_for` extend in `source_changed` | 6-8, 11, 12, 22, 23 |
| C4 | `waiting_address` skips the workspace branch | 4, 12 |
| C5 | `waiting_address` returns `None` for `#` specifiers | 5, 13, 22 |
| C8 (replaces C3) | own-path arm removed: an own key goes to `indexed_importers` again | 17, 21 |
| C9 (replaces C6) | `round_trip` passes empty `waiting_importers` | 18, 20, 22, 23 |
| C10 (replaces C7) | F1 existence filter removed | 24, 25 |
| C11 (extra slot, total 8) | F3 static-target offer removed | 23, 27 |

C8 and C9 tell the two halves of the core rule apart. C8 restores the
F2 cost, so 22 still passes. C9 breaks M4, so 21 still passes. C1, C2 and
C9 land in both the plugin and the core `it` binary: group the reverts per
binary in one build.

### Must confirm

- **M6.** The waiting set is read before the link inside
  `apply_file_diff_linked`. Rejected alternative: plumb the repointed set
  out of `imports::link`. Both give the same selection; the first touches
  neither `graph::imports` nor `LinkSummary`.
- **M7.** Accept the crash window above (link committed, re-extract not
  yet owed) as the same window every `affected` selection already has. The
  alternative is to owe the waiting importers in the same commit, which
  needs the own-path decision before the commit (`affected` is in hand
  there, the threshold is not).
- **M8.** Own-path detection is string equality with the trigger. A
  plugin that sends a differently spelled path for the same file
  (`./src/x.ts`) would bypass it and fall back to the old, wider
  selection. That is safe, only slower. TS sends normalized `RelPath`
  strings.
- **M9.** F3's offer may add an extensionless address of `path` that
  never exists as a file. It is harmless: neither half matches it, and F1
  never drops it.

Cross-file answers for this section (g-mesh, project `g-mesh-wt-gm666`):
`find_callers(indexed_importers)` gives `select_affected` only.
`find_callers(select_with)` gives `select`, `reextract_affected` and one
test. `find_callers(select_affected)` gives `select_with` and 4 tests.
`find_callers(select)` gives `selective` (trigger `None`).
`find_callers(reextract_affected)` gives `apply_file_change_in` only.
`find_callers(graph::imports::link_diff by id)` gives
`index_store::link_applied` and 3 tests. `find_callers(link_applied)`
gives `apply_and_link` and `apply_semantic_diff_linked`.
`find_callers(Writer::apply_file_diff_linked)` gives `round_trip`,
`retire_file`, `purge_language` and 5 tests.
`find_references(LinkSummary)` finds `graph/imports.rs` only.
`find_references(ImportMatch, answer=files)` finds `wire` and
`config_reindex`, plus unlinked usages in the python, rust and TS
`facts.rs` files, sdk `resolution.rs`, TS `project/mod.rs` and
`resolution_delta.rs`. Those were read with `grep -n`; go has no
`ImportMatch` and was checked by grep in `facts.go`.
Ordering: read in `apply_file_change_in`. The link runs inside
`round_trip`'s commit, and `reextract_affected`'s selection comes after
it.
