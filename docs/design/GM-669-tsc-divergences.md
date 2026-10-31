# GM-669: four tsc divergences in paths / exports / imports resolution

Status: design (GM-669/S1). Base: `fix/GM-669-tsc-divergences` at 5639e35
(release-4.21.0 incl. GM-666).

Evidence: tsc 5.9.3, `moduleResolution: bundler` (node16 checked where
noted), scratch projects under the session scratchpad (`gm669/a`, `gm669/b`),
`tsc -p . --traceResolution`. Paths shortened to `B/`.

## Code map (g-mesh calls, project `g-mesh`, main index valid for this branch)

| Question | Call | Answer |
|---|---|---|
| callers of `expand_paths_candidates` | `find_callers` | `resolve_tsconfig_paths`, `waiting_address`, `fallbacks_shadowed_by` (+ tests in `project_model.rs`) |
| callers of `imports_targets` | `find_callers` | same three (`resolve_package_imports`, `waiting_address`, `fallbacks_shadowed_by`) |
| callers of `package_entry_targets` | `find_callers` | `resolve_workspace`, `waiting_address`, `fallbacks_shadowed_by`, one test |
| inverse lookup on create | `find_callers specifiers_targeting` | only `derive_specifiers` (feeds `fallbacks_shadowed_by` and `waiting_addresses_for`) |
| who consumes the create-side keys | `find_callers waiting_addresses_for` | only `TsProject::source_changed` (`mod.rs:187`) |
| where candidates are expanded | `find_callers candidate_paths` | `first_existing`, `displaced_by`, `addresses_resolving_to`, `candidates_shadowed_by` |
| matcher kinds core accepts | `find_definition Matcher`, `find_references Matcher` | `Exact`, `Under`, `StartsWith`, `NonRelative`; core (`config_reindex.rs::select_affected`) indexes `Target+Exact` and scans every other selector through `StoredImport::matches` |

Legacy package fields are ranked in one place:
`workspace.rs::package_entry_targets` (lines 135-168), the literal
`["source", "main", "module", "types", "typings"]` at line 144.

Every forward list (`expand_paths_candidates`, `imports_targets`,
`package_entry_targets`) is shared by the resolver, by GM-666's
`waiting_address` (record side and create side alike) and by
`fallbacks_shadowed_by`. A change made inside a list function therefore keeps
record-side and create-side addresses consistent by construction; this note
changes nothing in `waiting_address` itself.

---

## Item 0: empty `*` capture (the task title)

### tsc evidence

```
Module name '@/', matched pattern '@/*'.
Trying substitution './src/*', candidate module location: './src/*'.
======== Module name '@/' was not resolved. ========

Module name '@x/', matched pattern '@x/*'.                       ("@x/*": ["./src/index.ts"])
Trying substitution './src/index.ts', candidate module location: './src/index.ts'.
======== Module name '@x/' was successfully resolved to 'B/src/index.ts'. ========

Trying substitution './', candidate module location: './'.        ("@r/*": ["./"])
======== Module name '@r/' was successfully resolved to 'B/index.ts'. ========
Trying substitution './src', candidate module location: './src'.  ("@s/*": ["./src"])
======== Module name '@s/' was successfully resolved to 'B/src/index.ts'. ========

Using 'imports' subpath '#r/*' with target './src/'.             ("#r/*": "./src/*")
======== Module name '#r/' was not resolved. ========
Using 'imports' subpath '#t/*' with target './lib2.js'.          ("#t/*": "./lib2.js")
======== Module name '#t/' was successfully resolved to 'B/lib2.ts'. ========
Using 'exports' subpath './w/*' with target './src/'.            (pk: "./w/*": "./src/*")
======== Module name 'pk/w/' was not resolved. ========
```

So the rule differs per family:

- **paths**: tsc substitutes only a non-empty capture
  (`matchedStar ? replaceFirstStar(...) : subst`). An empty capture leaves
  the literal `*` in the target, which names no real file. A target with no
  `*` is used as written, so it still resolves, the root `./` included.
- **imports/exports**: the empty capture is substituted (`./src/*` ->
  `./src/`); a target ending in `/` names a directory, which these maps never
  resolve. A static target still resolves.

### Today

`expand_paths_candidates` (tsconfig.rs 99-129): `@/` under `./src/*` gives
`src/` -> `src` -> `src/index.ts`, which is wrong. GM-662 sends an empty
capture through `paths::inside`, so `@r/` under `["./"]` gives nothing, while
tsc resolves `index.ts`. This regression came from GM-662's root fix.
`imports_targets`: `#r/` gives `./src/` -> `src` -> `src/index.ts`, which is
wrong. Exports `pk/w/` -> `src` -> probed, also wrong.

### Change

- paths: in `expand_paths_candidates`, `capture == Some("")` with a target
  containing `*` gives no candidate (`continue`). Every other target goes
  through `inside_or_root`, so the `capture == Some("")` arm of the
  `resolved` match goes away.
- imports/exports: no rule of their own. Item 1 drops every relative target
  without a file extension after substitution, so `./src/` (normalized
  `src`) is dropped there.

### Effect on GM-666

`@/` (only `./src/*`) now has no first base, so `waiting_address` gives
`None` and the import is classified like an npm package (external), as
GM-662 already does for the root form. Nothing on the create side derives
`@/` (`specifiers_targeting` cannot match `src` against `src/*`). `@r/`
under `["./"]` now resolves. `static_targets_naming` already offered `.`
for it, so the record and create sides now agree, where before only the
create side offered it.

---

## Item 1: extensionless exports/imports targets

### tsc evidence (bundler; node16 identical)

```
Using 'imports' subpath '#e' with target './lib'.          (lib/index.ts exists)
Import specifier '#e' does not exist in package.json scope at path 'B'.
======== Module name '#e' was not resolved. ========
Using 'imports' subpath '#f' with target './lib2'.         (lib2.ts exists)
======== Module name '#f' was not resolved. ========
Using 'exports' subpath './a' with target './lib'.
Export specifier './a' does not exist in package.json scope at path 'B/node_modules/pk'.
Using 'imports' subpath '#r/*' with target './src/index'.
======== Module name '#r/index' was not resolved. ========
Using 'imports' subpath '#r/*' with target './src/index.js'.
======== Module name '#r/index.js' was successfully resolved to 'B/src/index.ts'. ========
Using 'exports' subpath './w/*' with target './src/index.js'.
======== Module name 'pk/w/index.js' was successfully resolved to 'B/node_modules/pk/src/index.ts'. ========
```

tsc gives a relative exports/imports target **neither extension nor
directory-index candidates**: only the file as written, or its TS
substitution for a JS extension. The task text named only the index
candidates; the evidence shows the extension probing diverges too.

### Today

`candidate_paths` is already exact for a base with an extension. A JS
extension gives its substitutions plus the file itself; a parsed extension
gives the file alone. Only an **extensionless** base is probed (`x.ts` …
`x/index.ts`). So the whole divergence is extensionless relative targets
reaching `candidate_paths`.

### Change (recommended: full tsc parity, owner decision D1)

Drop a relative exports/imports target whose substituted, normalized base
has no file extension, i.e. one that `candidate_paths` would probe. A new
predicate `resolve::names_file(base) -> bool` is true when the base has a
`TS_SUBSTITUTIONS` extension or a `grammar_for` grammar. It is applied:

- in `imports_targets`, after substitution, only to targets on the relative
  branch of `imports_target_inside` (`./`, `../`). A module-name target
  (`.`, `..`, `.\\`) stays a package-style lookup, as tsc treats it and as
  GM-662 pinned, so the root index stays reachable through `#x: "."`.
- in `package_entry_targets`, to the `exports` entries only. `main`-style
  fields keep probing (tsc's trace shows `Loading module as file / folder`
  for `main`), and the `src/index` / `index` / subpath conventions keep
  probing (they are g-mesh conventions, not tsc).
- in `static_targets_naming`'s imports loop, so the create side drops a
  static extensionless imports target exactly as the forward list does
  (consistency only).

`keys_targeting` (the inverse) is **not** filtered. It inverts the target
*pattern* (`./src/*`, extensionless), and the capture supplies the extension
(`#r/x.js` -> `./src/x.js`). Filtering the pattern would break re-point for
the common ESM form.

Options for D1:
- **(a) full parity (recommended):** extensionless relative targets name
  nothing. Benefit: matches tsc; one predicate; the existing probing code is
  untouched. Risk: JS-only projects that rely on a bundler probing `#/*` ->
  `./src/*` with extensionless specifiers stop resolving those imports
  (Node and tsc reject them too).
- **(b) index only (task text):** keep extension candidates, drop only
  `x/index.*`. Benefit: a smaller behaviour change. Risk: needs a third
  candidate mode in `candidate_paths`, and `#f: "./lib2"` keeps resolving
  where tsc does not. It also needs an explicit "target ends in `/`" rule
  for item 0's imports form.

### Effect on GM-666

`waiting_address` takes the first base of the same lists, so an import
matched only by extensionless targets now has no base and gets `None`
(external). A package with an extensionless `exports` target now waits at
its next entry (`source`/`main`/convention). The record and create sides
use the same functions, so the addresses stay consistent. On create, the
inverse may still derive such a specifier (`#r/x` from address `src/x`). Its
`waiting_address` is now `None` and `displaced_by` finds `path` absent from
its list, so it adds no key.

---

## Item 2: a `*` target with `..` after the `*`

### tsc evidence

```
Trying substitution './*/..', candidate module location: './lib/x/..'.
======== Module name 'd/lib/x' was successfully resolved to 'B/lib.ts'. ========

("@m/*": ["./*/..", "./gen/*"], lib.ts absent)
Trying substitution './*/..', candidate module location: './lib/x/..'.
Trying substitution './gen/*', candidate module location: './gen/lib/x'.
======== Module name '@m/lib/x' was successfully resolved to 'B/gen/lib/x.ts'. ========
(with lib.ts present: resolved to 'B/lib.ts')

("#d/*": "./*/../lib2.js")
package.json scope 'B' has invalid type for target of specifier '#d/q'
```

The paths form is the only valid one: tsc normalizes after substitution.
For exports/imports, tsc rejects a target with `..`, so this item is
paths-only (see Out of scope).

### Today

Forward resolution is right (`normalize` folds `x/..`). The inverse
`specifiers_targeting` normalizes the target *pattern* first: `./*/..` ->
`.`, `./src/*/../x` -> `src/x`. The `*` disappears, so no specifier is
derived. Consequences on creating `lib.ts`:

- `@m/lib/x` linked to `gen/lib/x.ts` under `["./*/..", "./gen/*"]` is
  **not** re-pointed (no key `gen/lib/x.ts`).
- `@m/lib/x` unresolved under `["./gen/*", "./*/.."]` waits at
  `gen/lib/x` (first base) and is **not** linked to `lib.ts`.
- Already covered: a single `./*/..` target. The address `lib` comes from
  `addresses_resolving_to` (the relative arm in `source_changed`, which is
  address-keyed and family-agnostic), and the `lib/index.ts` -> `lib.ts`
  shadow comes from `candidates_shadowed_by`. The reverse case also works
  (a new `gen/lib/x.ts` displacing `lib.ts`: `gen/*` inverts fine).

### Why the inverse cannot just produce specifiers

Each `..` after the `*` deletes one trailing segment of the capture, so an
address `lib` is reached from `@m/lib/<y>` for **every** `<y>`. On the
waiting side nothing in the existence set names `<y>`: the import is
unresolved precisely because `gen/lib/<y>` is absent. A finite list of
exact keys cannot be complete.

### Change (recommended, owner decision D2)

Offer a **prefix** key instead of specifiers. New
`resolve::climbing_target_prefixes(project, path) -> Vec<String>`. For each
distinct config, each wildcard entry `E` (pattern `Qe*Re`), and each target
`t1 = P1*S1` whose `S1` begins with `k >= 1` `..` segments after the star's
segment (`.` segments ignored), it computes from `path` and each of its
addresses `A`:

1. `A` must end with `/R`, where `R` is `S1` after those `..` (or `R` is
   empty). Strip it to get `A'`.
2. `c_head = match_wildcard(normalize(resolve_dir/P1) + "*", A')`. When `A'`
   equals the prefix itself, `c_head` is empty.
3. The family's fixed specifier prefix is `F = Qe + c_head + "/"` (or `Qe`
   when `c_head` is empty).
4. For every other wildcard target `t2 = P2*S2` (no climb) of every entry
   `E2 = Q2*R2` of the same config:
   - if `F` starts with `Q2`, emit
     `normalize(resolve_dir/(P2 + F[Q2.len..]))`, with a trailing `/` when
     that text ended in `/`;
   - if `Q2` starts with `F`, emit `normalize(resolve_dir/P2)` (an
     over-approximation);
   - otherwise `E2` cannot match the family.
   An exact entry (no `*`) whose pattern starts with `F` adds its targets
   as ordinary exact keys.

`source_changed` (mod.rs 187-210) turns each prefix into an
`ImportSelector { by: Target { File, Matcher::StartsWith(prefix) } }`
beside today's `Exact` keys. Core needs no change: a non-`Exact` selector
goes through `scan_imports`, and `StoredImport::target` reads the
placeholder scope of an unlinked edge and the linked file of a linked one.
One selector therefore catches both the importer linked to
`gen/lib/x.ts` (re-point) and the importer waiting at `gen/lib/x`
(link). Which file wins stays the resolver's, on re-extract.

Guard: an empty prefix (one that would match every import) is not emitted.
This can happen only when `A` is the root and `P2` is `./`. The root-index
create under such a config is left uncovered and documented (must-confirm
M3).

Options for D2:
- **(a) prefix selectors (recommended):** complete for both re-point and
  waiting. Benefit: no existence scan; core is unchanged. Risk: each
  qualifying create costs one full `scan_imports` over the language's
  imports and re-extracts importers of unrelated files under the prefix.
  This only happens for configs that contain a `*`-then-`..` target, which
  no real-world config we know of does.
- **(b) existence enumeration:** exact keys from inverting other targets
  over existing files under the prefix. Benefit: the `Exact` keys keep using
  the index. Risk: it misses the waiting case entirely and needs a range
  scan of the existence set.
- **(c) won't fix:** document that a `*`-then-`..` target re-points only
  through the address arm. Benefit: zero code. Risk: fails the acceptance
  criterion as written.

### Effect on GM-666

`waiting_address` is unchanged: the family's first base is either `A`
itself (a `..` target first, already keyed by the relative arm) or a path
under an emitted prefix (another target first). The own-path arm
(`Exact(key) == trigger`) is untouched. A `StartsWith` prefix that contains
`path` itself (for example `"./*"` and `"./*/.."` with a new
`lib/index.ts`) selects every importer just linked to `path`. That costs
extra re-extracts and is correct (M4).

---

## Item 3: legacy package field order

### tsc evidence

```
'package.json' has 'typings' field './ty.d.ts' that references 'B/node_modules/lg/ty.d.ts'.
======== Module name 'lg' was successfully resolved to 'B/node_modules/lg/ty.d.ts'. ========
      (lg declares main, module, types, typings; every file exists, m.ts and mod.ts included)
'package.json' does not have a 'typings' field.
'package.json' has 'types' field './t.d.ts' that references 'B/node_modules/lg2/t.d.ts'.
======== Module name 'lg2' was successfully resolved to 'B/node_modules/lg2/t.d.ts'. ========
      (lg2: main ./m.js with m.ts present, module, types)
'package.json' has 'types' field './missing.d.ts' ...  File '.../missing.d.ts' does not exist.
'package.json' has 'main' field './m.js' ...
```

tsc reads `typings`, then `types`, then `main`, and never reads `module`.
`typings` is checked before `types`, which the task text did not say.

### Today and change

`package_entry_targets` line 144: `source, main, module, types, typings`.
The new order is **`source, typings, types, main, module`**. `source` stays
first as a g-mesh convention (tsc ignores it; owner decision D3). `module`
goes last because tsc never reads it. The existing test
`the_declared_entry_fields_are_offered_in_a_fixed_order_after_exports`
(project_model.rs ~580) changes expectation: a behaviour change to list in
S2's hand-over. `package_may_name`'s field list is a set, so it needs no
change.

Risk: a package whose `types` (`dist/index.d.ts`) is **committed and not
gitignored**, next to a `main` pointing at source, now resolves to the
`.d.ts`. That is what tsc does; a gitignored `dist/` is not in the existence
set and is unaffected (pinned by
`a_gitignored_build_entry_does_not_shadow_the_source_it_was_built_from`).

### Effect on GM-666

The record and create sides both take the first entry from
`package_entry_targets`, so they move together. A package with both `types`
and `main` now waits at the `types` base.

---

## Edit map (line numbers at 5639e35)

| # | File | Fn (lines) | Edit |
|---|---|---|---|
| 0 | `plugins/typescript/src/project/tsconfig.rs` | `expand_paths_candidates` (92-129 incl. doc) | empty capture + `*` in target -> `continue`; otherwise `inside_or_root`; doc. **The only tsconfig.rs edit** (GM-670 touches `extends_candidates`/`resolve_effective`, not this fn). |
| 1a | `plugins/typescript/src/project/resolve.rs` | new `pub fn names_file(base)` next to `candidate_paths` (77-97) | `TS_SUBSTITUTIONS` extension or `grammar_for` |
| 1b | `plugins/typescript/src/project/exports.rs` | `imports_targets` (86-110) | after `imports_target_inside`, drop a relative-branch target failing `names_file` (factor as `imports_base(dir, target)`) |
| 1c | `plugins/typescript/src/project/workspace.rs` | `package_entry_targets` (135-168) | the `exports` origin filtered by `names_file`; field order (item 3) |
| 1d | `plugins/typescript/src/project/resolve.rs` | `static_targets_naming` (302-352) | imports loop uses `imports_base` |
| 2a | `plugins/typescript/src/project/resolve.rs` | new `pub fn climbing_target_prefixes` after `static_targets_naming` | per the item 2 algorithm |
| 2b | `plugins/typescript/src/project/mod.rs` | `source_changed` (186-210) + doc (155-185) | add `StartsWith` selectors from 2a |
| 3 | `plugins/typescript/src/project/workspace.rs` | `package_entry_targets` line 144 | `source, typings, types, main, module`; doc line 130 |

Not touched: `specifiers_targeting`, `keys_targeting`, `candidate_paths`,
`waiting_address`, core.

## Behaviours to pin

0. `@/` under `"@/*": ["./src/*"]` with `src/index.ts`: unresolved, and
   `miss_address` is `None`. `@x/` under `["./src/index.ts"]` resolves.
   `@r/` under `["./"]` resolves to the root `index.ts` (the GM-662
   regression).
0'. `#r/` under `"#r/*": "./src/*"`: unresolved. `#t/` under static
   `"./lib2.js"` resolves to `lib2.ts`.
1. `#e: "./lib"` with both `lib.ts` and `lib/index.ts`: unresolved. Exports
   `"./a": "./lib"`: `pkg/a` unresolved through exports.
   `#r/x.js` under `./src/*` still resolves to `src/x.ts` and still
   re-points on create (negative control against over-dropping).
   `#x: "."` still names the root index (GM-662 pin unchanged).
2. Re-point: `["./*/..", "./gen/*"]`, `@/lib/x` linked to `gen/lib/x.ts`;
   create `lib.ts` -> relinked to `lib.ts` (core it test). Waiting:
   `["./gen/*", "./*/.."]`, `@/lib/x` unresolved; create `lib.ts` ->
   linked. Plugin delta: `source_changed` contains
   `StartsWith("gen/lib/")`.
3. A workspace package with `main` -> `m.ts`, `types` -> `t.d.ts` (both
   present) resolves to `t.d.ts`; with `typings` too, to the `typings` file.

## Controls (7), revert the code, never the test

| C | Revert | Must fail |
|---|---|---|
| C1 | drop the empty-capture `continue` in `expand_paths_candidates` | `@/` -> unresolved |
| C2 | send the empty capture through `paths::inside` again (GM-662 form) | `@r/` -> root `index.ts` |
| C3 | remove the `names_file` filter from `imports_targets` | `#e` unresolved, `#r/` unresolved |
| C4 | remove the `names_file` filter from the exports origin in `package_entry_targets` | `pkg/a` unresolved via exports |
| C5 | `climbing_target_prefixes` returns empty | the item 2 re-point and waiting tests (core it + delta) |
| C6 | restore `main`, `module` before `types` | package resolves to `t.d.ts` |
| C7 | swap `typings`/`types` | package resolves to the `typings` file |

C1-C4 and C6-C7 live in `plugins/typescript/tests` (project_model /
resolution_delta) and can be grouped into one build. C5 also needs
`core/tests/it/created_file_links_waiting_import.rs`.

## Owner decisions

- **D1** item 1: (a) full tsc parity, no extension or index for
  extensionless relative exports/imports targets (recommended), or (b)
  index only.
- **D2** item 2: (a) `StartsWith` prefix selectors (recommended), (b)
  existence enumeration (re-point only), or (c) won't fix.
- **D3** item 3: keep `source` first (recommended; g-mesh convention for
  unbuilt workspaces), or strict tsc order without `source`.

## Must-confirm (S2/S3)

- M1: existing fixtures that use extensionless exports/imports targets
  change expectation. Known: `project_model.rs:1548-1550`
  (`#r/*: "./*"`, `#r/x` -> `["x"]` becomes `[]`), the root-form loop at
  ~1544, and `resolution_delta.rs:878` (`exports {"./*": …}` variants).
  List every changed expectation as a behaviour change.
- M2: `grammar_for` covers `.mts`/`.cts`/`.d.mts`, so `names_file` keeps
  every extension `candidate_paths` treats as exact.
- M3: the empty-prefix guard (root `A`, `./` `P2`) leaves that case
  uncovered. Confirm with a test that no `StartsWith("")` is emitted.
- M4: a prefix containing `path` re-extracts importers just linked to
  `path`. Confirm there are extra re-extracts only and no wrong edge.
- M5: `scan_imports` cost runs only when a config has a `*`-then-`..`
  target. Assert `climbing_target_prefixes` is empty for an ordinary
  config (no `StartsWith` selector in the delta).

## Out of scope (found here; candidates for follow-up tasks)

- A paths target with two `*` is tsc error TS5062, and tsc substitutes only
  the first `*` (`'./src/q-*'`). g-mesh's `target.replace('*', capture)`
  substitutes all of them.
- An exports/imports target containing `..` is invalid in tsc
  (`invalid type for target`). g-mesh resolves it, and `package_may_name`
  relies on it.
