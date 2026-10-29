# GM-661: re-point alias and workspace imports across bases when a higher-priority target is created

Status: design (S1). No production or test change in this slice. Line
numbers are at `7f7036e` (release-4.20.0; TS plugin code equal to main).

## The problem in one example

`tsconfig.json` declares `"paths": { "@/*": ["src/*", "generated/*"] }`.
`src/a.ts` has `import { f } from "@/x"`. Only `generated/x.ts` exists, so
TS resolves `@/x` to it, and core links the import (`IMPORTS` edge
`a.ts -> File(generated/x.ts)`).

Create `src/x.ts`. TS now resolves `@/x` to `src/x.ts` (the first
substitution wins). g-mesh keeps the edge on `generated/x.ts` until
`src/a.ts` is edited or the language is reindexed.

The same for a workspace package: `packages/p/package.json` names no
entry, `packages/p/index.ts` exists, `import "@scope/p"` is linked to it.
Create `packages/p/src/index.ts`: TS (and our resolver) prefer
`src/index` over `index`, but the edge stays on `packages/p/index.ts`.

## 1. Today (with the code path)

### How a create re-points a link at all (GM-655/GM-657)

1. The SDK calls the plugin's `source_changed` for the new file
   (`plugins/typescript/src/extractor/mod.rs` 90-97 ->
   `TsProject::source_changed`, `project/mod.rs` 175-196). It runs once per
   path per model (`announced`).
2. It builds `Exact` keys: the file's own addresses
   (`resolve::addresses_resolving_to`, resolve.rs 136-158) plus the
   candidates the file shadows (`resolve::candidates_shadowed_by`,
   resolve.rs 168-183), and returns
   `ResolutionDelta::Affected { imports: [Target { File, Exact(key) }] }`.
3. Core (`watcher::apply::apply_file_change_in`, apply.rs 234-300 ->
   `reextract_affected`, 367-420 -> `daemon::config_reindex::select_with`,
   config_reindex.rs 205) selects every importer whose stored import points
   at one of those keys: an unlinked placeholder's scope, or a **linked
   edge's target file path**. Those importers are re-extracted; the
   resolver, against the updated existence set, picks the new winner.

So re-pointing a linked import only needs the plugin to name, as a key,
**the file the import is linked to today**.

### Why aliases and workspace entries are missed

`candidates_shadowed_by(path)` looks only at the candidate list of one
**address** (`candidate_paths(address)`, resolve.rs 72-91): `src/x` ->
`src/x.ts, src/x.tsx, ..., src/x/index.ts, ...`. For `src/x.ts` it finds
nothing existing after it in that list, so nothing is named.

But the resolver's list for an alias or a package is a **concatenation over
several bases**: `first_existing(existence, bases)` (resolve.rs 94-103)
flat-maps `candidate_paths` over

- `expand_paths_candidates(config, specifier)` (tsconfig.rs 96-122) for
  tsconfig `paths` (`src/x`, then `generated/x`);
- `package_entry_targets(package, subpath)` (workspace.rs 135-161) for a
  workspace package (`exports`, `source`/`main`/..., `src/index`, `index`;
  for a subpath `./u`: `u`, then `src/u`);
- `imports_targets(config, specifier)` (exports.rs 83-108) for `#private`
  specifiers (an array or condition object gives several targets).

`generated/x.ts` sits in a different base's list, which
`candidates_shadowed_by` never looks at. No key names it, core selects
nobody, the edge stays.

### Neighbouring gap (not this task)

An alias or package import that resolves to **nothing** is stored as an
`external_module` placeholder with no target
(`extractor::imports::Declarer::record_specifier`, imports.rs 200-221: only
relative specifiers get an address). Creating its first target does not
link it either: no `Target` key can match a placeholder without a target.
GM-661's acceptance is about already linked imports; see must-confirm M1.

## 2. Options

### (A) Invert each alias/entry family for the new file (recommended)

**Today:** only the relative candidate list of one address is searched.

**Change:** a new `resolve::fallbacks_shadowed_by(project, path)` names,
for every alias or package specifier whose resolver list contains `path`,
the first existing candidate after `path` in that list (the previous
winner). `source_changed` adds those as extra `Exact` keys next to
`candidates_shadowed_by`.

How it finds "every specifier whose list contains `path`" without
enumerating specifiers: invert each target pattern against the file's
addresses (`addresses_resolving_to(path)` plus `path` itself, since a
target may name the file with its extension, `./src/*.ts`):

- tsconfig: for every distinct `EffectiveConfig` in `tsconfig_by_dir`, for
  every entry and target, `paths::inside(resolve_dir, target)` gives the
  target pattern (`src/*`); `match_wildcard(pattern, address)` gives the
  capture (`x`); the specifier is the entry's pattern with that capture
  (`@/x`). A key without `*` gives its own pattern when the target equals
  the address.
- workspace: for every package, subpath `.`, every exact `exports` subpath
  key, `./<rel>` and (when `<rel>` starts with `src/`) `./<rel minus src/>`
  for an address under the package dir, and every wildcard `exports` key
  inverted as above through `key_targets(value, None)`.
- `#imports`: as tsconfig, over `imports_by_dir` and `key_targets`
  (decision D2).

Then, per derived specifier, the resolver's **own** list function is
re-run (`expand_paths_candidates` / `package_entry_targets` /
`imports_targets`, flat-mapped through `candidate_paths`), `path`'s
position found, and the first existing candidate after it named. The
inversion only decides *which specifiers to ask about*; the order and the
winner come from the same functions `resolve` uses, so they cannot drift.

**Example:** new `src/x.ts`; addresses `src/x`, `src/x.js`, `src/x.ts`;
target `src/*` captures `x` -> `@/x` -> list `src/x.ts, ..., generated/x.ts,
...`; first existing after `src/x.ts` is `generated/x.ts` -> key
`generated/x.ts` -> core selects `src/a.ts` -> re-extract links `@/x` to
`src/x.ts`.

**Benefit:** plugin-only, no wire or core change; exact on the winner;
re-extracts only importers linked to the shadowed file; same shape as
GM-657, so the review/probe approach carries over.

**Risk:** an over-approximation in two harmless places: a config is
applied without checking which importers it governs, and an alias
specifier that a workspace package would win anyway is still asked about.
Both only add re-extracts of importers linked to a file that really was a
lower candidate of some specifier. An under-approximation would be a
missed re-point: a target pattern with two `*` (tsc rejects those) or an
`exports` condition shape `key_targets` does not flatten. Cost per first
sighting: configs x entries x targets x ~4 addresses, plus packages x
`exports` keys; once per path per model. After a restart, each file's
first event re-extracts once the importers linked to what it shadows (the
same restart cost GM-657 already accepted).

### (B) Offer a specifier selector for the alias family

**Change:** for each alias whose targets can name `path`, return
`ImportMatch::Specifier(StartsWith(<alias prefix>))` (`@/`), or `Exact`
of each workspace package name.

**Benefit:** simpler code (no position search); would also fix the
neighbouring gap (unlinked `external_module` alias imports), because the
selector matches on specifier, not target.

**Risk:** selects every importer of the alias, linked anywhere. A project
using `@/*` everywhere re-extracts most of its files on the first sighting
of any file under `src/`, and crosses `select_with`'s too-many threshold
(30% of files) into a language reindex: the acceptance's "without a full
reindex" fails in exactly the projects that use aliases most.

### (C) Answer `Unknown` when the new file is in a multi-base family

**Change:** `source_changed` returns `ResolutionDelta::Unknown` when any
config entry has two or more targets, or any package has a fallback, that
can name `path`.

**Benefit:** smallest change, always correct.

**Risk:** core turns `Unknown` into `ReindexLanguage` (apply.rs 270-274):
a full TS reindex per such create. Violates the acceptance.

**Recommendation: (A).** (B) as a separate task if M1 is wanted.

## 3. Decisions

- **D1. Ask about every config, not only the importer's nearest.**
  Today the resolver picks the config nearest the importer
  (`tsconfig_for`). The new file has no importer yet, so (A) inverts every
  config. Example: `apps/web/tsconfig.json` and the root one both alias
  `@/*`; a new `apps/web/src/x.ts` is inverted under both. Consequence: at
  worst an extra re-extract of an importer that a different config
  governs; the re-extract resolves it with its real config and nothing
  moves.
- **D2. Include `#imports` fallbacks.** Today `"#x": ["./src/x.ts",
  "./gen/x.ts"]` has the same gap. Change: the same inversion over
  `imports_by_dir`. Example: `#x` linked to `gen/x.ts`, create
  `src/x.ts` -> re-points. Consequence: one more branch and one more test
  and control; without it the gap stays for `#` specifiers. Recommended:
  include (the task title does not name it; owner to confirm).
- **D3. Keep `candidates_shadowed_by` as is.** The relative case stays
  its own function (pinned by GM-657 tests); the new function covers only
  the multi-base kinds. Consequence: the two may name the same key; the
  key set in `source_changed` is a `BTreeSet`, so no duplicate selector.

## 4. Edit map

Change:

- `plugins/typescript/src/project/resolve.rs`
  - new `pub fn fallbacks_shadowed_by(project: &TsProject, path: &str) ->
    Vec<String>` after `candidates_shadowed_by` (160-183): the address set,
    the three inversions, the position search (shared helper: "first
    existing after `path` in `bases.flat_map(candidate_paths)`", next to
    `first_existing` 94-103).
- `plugins/typescript/src/project/tsconfig.rs` — new
  `pub fn specifiers_targeting(config: &EffectiveConfig, address: &str) ->
  Vec<String>` next to `expand_paths_candidates` (96-122), its inverse.
- `plugins/typescript/src/project/workspace.rs` — new
  `pub fn subpaths_targeting(package: &WorkspacePackage, address: &str) ->
  Vec<String>` next to `package_entry_targets` (135-161).
- `plugins/typescript/src/project/exports.rs` — new
  `pub fn imports_specifiers_targeting(config: &PackageImports, address:
  &str) -> Vec<String>` next to `imports_targets` (83-108) (D2), and the
  `exports` wildcard inversion used by `subpaths_targeting`.
- `plugins/typescript/src/project/mod.rs` `TsProject::source_changed`
  (157-196): `keys.extend(resolve::fallbacks_shadowed_by(self,
  path.as_str()))` after line 184; doc comment paragraph 166-170.

No signature changes; no wire, SDK or core change.

Read for context: `first_existing`, `resolve`, `resolve_workspace`,
`resolve_tsconfig_paths`, `resolve_package_imports` (resolve.rs 94-236);
`exports::match_wildcard` (152-161), `key_targets` (113-121),
`exports_targets` (45-81); `paths::inside` (paths.rs 54-67);
`Declarer::record_specifier` (extractor/imports.rs 200-221);
core `reextract_affected` (apply.rs 367-420), `select_with`
(config_reindex.rs 205). Tests to extend:
`plugins/typescript/tests/resolution_delta.rs` (fixtures `existing` 548,
`source_changed` 421, `paths_fixture` 205);
`core/tests/it/created_file_links_waiting_import.rs` (`indexed` 61,
`create` 78, `assert_only_the_importer_re_extracted` 83).

### Overlap with GM-662

GM-662 lets `paths`/`imports`/package targets that name the project root
resolve to the root `index` (today `paths::inside` drops them through
`escapes`, paths.rs 50-52 and 60-67). (A) calls `paths::inside` and
`candidate_paths` (which already maps `.` to the root `index` files), and
`addresses_resolving_to` already yields `.` for a root `index.ts`. So once
GM-662 lands, a root-index target is inverted with no extra code, as long
as GM-662 changes `inside` (or the list functions) rather than adding a
separate path for root targets. Conflict risk: both edit resolve.rs near
the list functions; GM-661 adds new functions only, so the merge is
textual, not semantic. Order: either; if GM-662 goes second, it should add
one test "a created root `index.ts` re-points an alias linked to its
fallback".

## 5. Behaviours for tests

Plugin unit (`resolution_delta.rs`):

1. `paths` `["src/*","generated/*"]`: new `src/x.ts` with `generated/x.ts`
   existing names `generated/x.ts`.
2. Reverse: new `generated/x.ts` with `src/x.ts` existing names nothing.
3. Three targets `a/*`,`b/*`,`c/*`, `b/x.ts` and `c/x.ts` existing: new
   `a/x.ts` names only `b/x.ts`.
4. Exact alias `"cfg": ["src/cfg", "gen/cfg"]`: new `src/cfg.ts` names
   `gen/cfg.ts`.
5. Directory index across bases: new `src/x/index.ts` names
   `generated/x.ts`.
6. Workspace root: package without entries, `pkg/index.ts` existing, new
   `pkg/src/index.ts` names `pkg/index.ts`.
7. Workspace subpath: `pkg/src/u.ts` existing, new `pkg/u.ts` names
   `pkg/src/u.ts`.
8. `exports` wildcard `"./*": ["./src/*.ts", "./lib/*.ts"]`: new
   `pkg/src/u.ts` with `pkg/lib/u.ts` existing names `pkg/lib/u.ts` (pins
   the file's own path in the address set).
9. `#imports` array (D2): `"#x": ["./src/x.ts","./gen/x.ts"]`, new
   `src/x.ts` names `gen/x.ts`.
10. `source_changed` offers the alias fallback key (behaviour 1 through
    the public entry point).

Core integration (`created_file_links_waiting_import.rs`, real plugin):

11. `@/x` linked to `generated/x.ts` moves to a created `src/x.ts`; only
    the importer is re-extracted.
12. `@scope/p` linked to `packages/p/index.ts` moves to a created
    `packages/p/src/index.ts`; only the importer is re-extracted.
13. An importer of an unrelated alias (`@/y` linked to `generated/y.ts`)
    is not re-extracted by the create of `src/x.ts`.

## 6. Suggested controls (7)

| # | revert (code only) | must fail |
|---|---|---|
| C1 | drop the `fallbacks_shadowed_by` extend in `source_changed` | 10, 11, 12 |
| C2 | tsconfig inversion returns empty | 1, 3, 4, 5, 11 |
| C3 | workspace inversion returns empty | 6, 7, 8, 12 |
| C4 | name the last existing candidate after `path` instead of the first | 3 |
| C5 | search the whole list, not only after `path` | 2 |
| C6 | address set without `path` itself | 8 |
| C7 | `#imports` inversion returns empty (D2) | 9 |

Behaviour 13 has no revert of its own (no code limits selection beyond
the key; C1's "only the importer re-extracted" assertion shows the case is
exercised), as GM-657 accepted for its "stays put" check. C1 + C11/C12 and
the unit controls land in different binaries: group per binary in one
build.

## 7. Must confirm

- **M1.** Unlinked alias/package imports (`external_module`, no target)
  are not linked when their first target is created. Out of GM-661's
  acceptance; file a backlog task (likely option B's specifier selector,
  scoped by alias prefix, with its own cost question)?
- **M2.** D2: include `#imports` fallbacks in GM-661 (recommended) or
  leave them to a follow-up.
- **M3.** The core harness `indexed()` fixture builds `TsProject` at
  index time: confirm a `tsconfig.json` / `package.json` passed in
  `extra` is loaded (it is a watch file, not a source file); if not, the
  tests slice needs a fixture variant that writes configs first.
- **M4.** `exports` condition objects inside a wildcard key:
  `key_targets(value, None)` returns the flattened patterns unchanged
  (exports.rs 113-121 substitutes only with `Some(capture)`), so the
  inversion sees `*`. Read in S1; the code slice confirms with test 8.
- **M5.** Review probe (as GM-657/S2): enumerate small configs (2-3
  targets, with/without `*`, extensions in targets) x before-sets x new
  file, comparing re-extract outcome with a fresh resolve; 0 misses
  required, over-selections reported.

## Cross-file answers relied on

| question | answer | g-mesh call |
|---|---|---|
| callers of `candidates_shadowed_by` | `TsProject::source_changed` (+ test file `resolution_delta.rs`) | `find_callers(candidates_shadowed_by)` |
| callers of `candidate_paths` | `first_existing`, `addresses_resolving_to`, `candidates_shadowed_by` (+ `project_model.rs`) | `find_callers(candidate_paths)` |
| where `paths` substitutions resolve | `tsconfig::expand_paths_candidates` <- `resolve_tsconfig_paths` <- `resolve` | `find_callers(expand_paths_candidates)`, `find_callers(resolve_tsconfig_paths)` |
| where package entry fallback resolves | `workspace::package_entry_targets` <- `resolve_workspace` <- `resolve` | `find_callers(package_entry_targets)`, `find_callers(resolve_workspace)` |
| who calls the plugin's `source_changed` | `<TypeScriptExtractor as Extractor>::source_changed` | `find_callers(TsProject::source_changed)` |
| core path selecting importers | `ImportSelector` used in `core/src/watcher/apply.rs`, `core/src/daemon/config_reindex.rs` (`reextract_affected` -> `select_with`) | `find_references(ImportSelector, answer=files)` |
| why unresolved aliases have no target | `relative_address` <- `Declarer::record_specifier` only | `find_callers(relative_address)`, `find_definition(Declarer::record_specifier)` |
| selector shapes available | `ImportMatch::{Specifier, Target}`, `Matcher::{Exact, Under, StartsWith, NonRelative}` | `find_definition(ImportMatch)`, `find_definition(Matcher)` |
