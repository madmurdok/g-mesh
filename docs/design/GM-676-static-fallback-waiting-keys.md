# GM-676: link a waiting import when the static fallback target of its `paths` entry is created

Status: design (S1). No production or test change in this slice. Line
numbers are at `98c4033` (release-4.22.0 tip, GM-666 merged).

## The problem in one example

`tsconfig.json`:

```json
{ "compilerOptions": { "paths": { "@env/*": ["generated/*", "src/env.ts"] } } }
```

`src/app.ts` has `import { v } from "@env/a"`. Neither `generated/a.ts`
nor `src/env.ts` exists, so the import resolves to nothing and waits.

Create `src/env.ts`. TypeScript now resolves `@env/a` to it (the second
target names one file for every capture). g-mesh leaves the import
unlinked until `src/app.ts` is edited or TypeScript is reindexed. Not a
regression: GM-666 never covered a static target behind a wildcard one.

## 1. Today (with the code path)

### Record side: one address per unresolved import

`Declarer::record_specifier` (`plugins/typescript/src/extractor/imports.rs`
210-232) asks the resolver; on a miss of a non-relative specifier it asks
`MissAddress` (imports.rs 59), which is `resolve::miss_address`
(`plugins/typescript/src/project/resolve.rs` 715-721) ->
`waiting_address` (675-699). For a `paths` specifier that returns the
**first** candidate of `expand_paths_candidates`
(`project/tsconfig.rs` 101-127): `generated/a`. The placeholder carries
exactly one `PlaceholderTarget` (`wire/src/lib.rs` 169-184, built by
`extractor/keys.rs::file_target` 120-127), stored as exactly one
`placeholder_targets` row (`core/src/storage/schema.rs` 398-409,
`nodeId` is the PRIMARY KEY).

### Create side: what `src/env.ts` offers

`TsProject::source_changed` (`project/mod.rs` ~207-230) offers
`waiting_addresses_for(src/env.ts)` (resolve.rs 275-302), which already
includes `src/env.ts` itself through `static_targets_naming` (309-336,
GM-666 F3). In core, the key equals the trigger's own path, so
`select_affected` (`core/src/daemon/config_reindex.rs` 257-) answers it
with the importers that were waiting at `src/env.ts` before the link
(`storage::file_rows::waiting_importers`, `core/src/storage/file_rows.rs`
146-157, read inside `Writer::apply_file_diff_linked`).

### Why the import is missed

The import waits at `generated/a`, the key offered is `src/env.ts`. No
row has scope `src/env.ts`, so the waiting read is empty, the selection
is `Nothing`, and `src/app.ts` is never re-extracted. The F3 offer
(behaviour 23 of GM-666) only works when the static target is the
**first** target (`"@env/*": ["src/env.ts"]`), because then it is also
the recorded address.

Which configs hit this: a wildcard `paths` entry (not the catch-all
`"*"`) with a target without `*` that is not its first candidate. The
same shape exists for a wildcard `imports` key whose value lists a static
target after a wildcard one (`imports_targets`, `project/exports.rs` 84).
An exact entry (`"@env": ["a.ts", "b.ts"]`) is not affected: creating
`b.ts` derives `@env` (`specifiers_targeting`) and offers its waiting
address `a.ts`. A second wildcard target is not affected either:
`generated/x.ts` derives `@env/x` and offers its first candidate.

## 2. Options

### (A) Several waiting addresses per import: extra file scopes on the placeholder (recommended)

**Today.** A placeholder names one scope; the create of any other file
that would resolve the import has to reach that scope by derivation,
which a static target cannot do (it names one path for every capture,
so no capture can be recovered from it).

**Change.** The record side also lists every static target of the
entries matching the specifier (other than the primary address) as
**extra waiting scopes** on the same placeholder. Core stores them in a
new child table and both "who waits at X" reads also look there. The
primary scope, linking and every other reader stay as they are.

**Example.** `@env/a` records primary `generated/a` plus extra
`src/env.ts`. Create `src/env.ts`: the plugin offers `src/env.ts` (own
path, already today); `apply_file_diff_linked` reads the waiting
importers at `src/env.ts`, now including the extra-scope row ->
[`src/app.ts`]; `src/app.ts` is re-extracted, resolves `@env/a` to
`src/env.ts`, its placeholder becomes a resolved one and core links it.
Create `generated/a.ts` instead: unchanged, key `generated/a` selects
exactly `src/app.ts` and not the importer of `@env/b`.

**Consequence.** Exact selection in both directions; no language
reindex. Costs a wire field, a table, and a row lifecycle that has to be
kept in step with `placeholder_targets` at every site that deletes,
replaces or swaps it (edit map). Core does **not** link at an extra
scope: link stays keyed by the primary scope, and the re-extract does
the linking (same as a relative miss whose address is extensionless).

### (B) Record the static target as the (single) waiting address

**Change.** `waiting_address` returns the first static target of the
matched entries when there is one, else the first candidate. Both sides
call it, so they stay consistent. Plugin-only: no wire, schema or core
change.

**Example.** `@env/a` waits at `src/env.ts`. Creating `src/env.ts`
selects every `@env/*` waiter: correct, every one of them now resolves
there. Creating `generated/a.ts` offers `src/env.ts` (not its own path),
so `indexed_importers` selects **every** `@env/*` waiter, `@env/b`'s
importer included.

**Consequence.** Cheap, but it trades the exactness of the wildcard path
for the static one. A codegen writing N files under `generated/` with M
waiting importers re-extracts N x M files, or crosses the 30% threshold
and reindexes the language each time: today that path is exact. Also
misses a second static target (`["generated/*", "src/env.ts",
"lib/env.ts"]`), and the unlinked report would show the import waiting at
`src/env.ts` instead of the first place TS looks.

### (C) One extra placeholder node + `IMPORTS` edge per extra scope (rejected)

No wire or schema change, but every waiting import shows N `IMPORTS`
edges (`get_dependencies`, the unlinked report), and after one links the
others stay as stale unlinked placeholders until re-extract.

### (D) Offer a prefix key (`StartsWith("generated/")`) when the static target is created (rejected)

`StartsWith` goes to `scan_imports` (one read of every TS `IMPORTS` edge,
GM-655 moved creates off it on purpose) and also selects every importer
linked to any file under `generated/`, so a used directory crosses the
threshold.

## 3. Storage and wire impact (option A)

### Wire

`PlaceholderTarget` (`wire/src/lib.rs` 169-184) gains

```rust
/// More file paths this placeholder waits at besides `scope` ...
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub also_scopes: Vec<String>,
```

Same precedent as `from_container`/`key_path`: an absent field reads as
empty, so the Go plugin (own JSON) and older plugins need no change and
`CURRENT_PROTOCOL_VERSION` is not bumped. Rust literal sites must add
`also_scopes: Vec::new()`: about 40 of them (grep `PlaceholderTarget {`:
core `workspace_reindex.rs` 2, `watcher/apply/tests.rs` 5, python
`emit.rs` 2, rust `emit.rs` 2, sdk `graph.rs` 7, `lsp/bridge.rs` 9, sdk
tests 5, `toy/main.rs` 2, TS `keys.rs` 2, TS tests 4, wire 2).
Mechanical; alternatively a `PlaceholderTarget::new(scope, key)`
constructor, but that is a wider refactor than this task needs.

Conformance (`core/src/protocol/conformance.rs` ~130-140, next to
`check_key_path`): `also_scopes` non-empty only with a `File` scope on a
`resolved_module` placeholder; otherwise a shape violation. Optional,
must-confirm M3.

### Schema

New table, keyed like a child of the placeholder node:

```sql
CREATE TABLE IF NOT EXISTS placeholder_also_scopes (
    nodeId TEXT NOT NULL,
    scope  TEXT NOT NULL,
    PRIMARY KEY (nodeId, scope)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_also_scopes_scope ON placeholder_also_scopes(scope);
```

Why a new table and not more `placeholder_targets` rows: its PRIMARY KEY
is `nodeId`, and every `JOIN placeholder_targets` in `graph::imports`,
`graph::symbol_links` (571, 951-1051, 1478) and `mcp::unlinked` assumes
1:0-or-1. Why keyed by `nodeId` and not by importer file: it then lives
and dies with the placeholder at the sites that already delete its
`placeholder_targets` row.

`CURRENT_SCHEMA_VERSION` "16" -> "17" (schema.rs 97), as "11"
(`untyped_calls`) did: without it an existing index keeps an empty table
and the bug until each importer is edited. Cost: one wipe and full
reindex per project on upgrade (must-confirm M1).

Storage record: `PlaceholderTargetRecord` (`core/src/storage/write.rs`
38-52) gains `also_scopes: Vec<String>`, filled by
`watcher::apply::to_placeholder_target_record` (apply.rs 1266-).

## 4. Edit map (option A)

Plugin (TypeScript):

- `project/resolve.rs`:
  - new `fn static_fallbacks(project, specifier, config, imports) ->
    Vec<String>`: the static targets (no `*`) of the wildcard `paths`
    entries of `config` that match `specifier` (catch-all excluded, as
    `matched_beyond_catch_all` 701-711), resolved with
    `paths::inside_or_root(config.resolve_dir, target)`, plus for a `#`
    specifier the static targets of the matching wildcard `imports` key
    (`key_targets` + `imports_base`, exports.rs 195/129), in declaration
    order, deduplicated. Must produce byte-identical strings to
    `static_targets_naming` (309-336); share the per-entry resolution with
    it (extract a helper used by both).
  - `miss_address` (715-721): return `Option<MissAddresses { primary:
    String, also: Vec<String> }>`; `also` = `static_fallbacks` minus
    `primary`. `waiting_address` (675-699) unchanged (the create side
    still derives primaries with it).
  - `waiting_addresses_for` (275-302): unchanged (F3 already offers the
    static target; F1 filter unchanged).
- `extractor/imports.rs`: `MissAddress` type (59) returns the new struct;
  `record_specifier` (210-232) puts `also` into the target
  (`file_target` -> a variant taking `also_scopes`, `extractor/keys.rs`
  120-127). `extractor/mod.rs` 132 and `extractor/decls.rs` 61/94 follow
  the type.
- `project/mod.rs` `source_changed` doc (~190-200): one sentence.

Wire: `wire/src/lib.rs` 169-184 (field + doc); every literal site listed
in §3.

Core:

- `storage/schema.rs`: DDL (after 409), version 17 with its paragraph.
- `storage/write.rs`: `PlaceholderTargetRecord` (38-52); `apply_diff`
  replace block (557-597): delete and re-insert the node's also-scope rows
  with its target; node delete (445): delete them.
- `graph/imports.rs` drop of a linked placeholder (364-370 list): add the
  table.
- `graph/containers.rs` (546 list): add the table.
- `storage/language_swap.rs`: child-table list (222-227, used for the
  staging diff and copy), delete list (524), kept-placeholder delete
  (704); add the column list constant beside `TARGET_COLUMNS`.
- `watcher/apply.rs` `to_placeholder_target_record` (1266-).
- `protocol/conformance.rs`: the rule in §3 (if M3 accepted).
- Reads:
  - `storage/file_rows.rs::waiting_importers` (146-157): `pt.nodeId` from
    `placeholder_targets` (scope = ?1) UNION `nodeId` from
    `placeholder_also_scopes` (scope = ?1), then the same joins. This is
    what the acceptance case goes through (own-path key).
  - `daemon/config_reindex.rs::indexed_importers` (327-369), `File` arm:
    a third UNION branch over `placeholder_also_scopes`. Needed when the
    static target is written without extension (`"src/env"`): the key is
    then the address `src/env`, not the own path.
- Tests-only: `daemon/workspace_reindex.rs` `GRAPH_TABLES` digest list
  (~1275) gains the table.

Read, not changed: `graph::imports::link_diff` (links by primary scope
only, by design), `mcp/unlinked.rs` 189 (shows the primary scope),
`select_affected`'s own-path arm.

## 5. Behaviours tests must pin

1. (plugin, `extractor/imports/tests.rs` or `tests/imports.rs`) A miss of
   `@env/a` under `["generated/*", "src/env.ts"]` emits primary
   `generated/a` and `also_scopes = ["src/env.ts"]`.
2. (plugin) No `also_scopes` when: the entry has no static target; the
   only static target is the primary (`["src/env.ts", "generated/*"]`);
   the entry is the catch-all `"*"`; the specifier resolves.
3. (plugin) Two static targets give both, in declaration order; a static
   target written without extension is kept as written (`src/env`), equal
   to what `static_targets_naming` offers for `src/env.ts`.
4. (plugin) A `#` specifier with a wildcard `imports` key and a static
   target gets it as `also` (if M2 accepted).
5. (wire) Round trip: absent field deserializes empty, empty is not
   serialized, non-empty survives.
6. (core unit, `write` tests) Re-upserting the placeholder replaces its
   rows; deleting the node, the link drop in `graph::imports`, and a
   language swap delete/copy them (no orphan after the node is gone).
7. (core unit) An importer waiting only through an extra scope is
   selected by (7a) `file_rows::waiting_importers(src/env.ts)` and by
   (7b) `indexed_importers(File, "src/env")`.
8. (core integration, acceptance) Waiting `@env/a`, create `src/env.ts`:
   `Applied { reextracted: 1 }` (not `ReindexLanguage`), the import is
   linked to `src/env.ts`, its binding `v` is recorded.
9. (core integration) Same config, create `generated/a.ts` with a second
   importer of `@env/b`: only the `@env/a` importer is re-extracted.
10. (core integration) Static target written `"src/env"`: creating
    `src/env.ts` links the import (goes through `indexed_importers`).

## 6. Controls (7)

| # | revert (code only) | must fail |
|---|---|---|
| K1 | `miss_address` returns no `also` | 1, 3, 8 |
| K2 | `waiting_importers` without the also-scope UNION | 7a, 8 |
| K3 | `indexed_importers` without the also-scope UNION | 7b, 10 |
| K4 | `apply_diff` does not clear also-scope rows on replace/delete | 6 |
| K5 | `graph::imports` link drop omits the table | 6 (link case) |
| K6 | language swap omits the table | 6 (swap case) |
| K7 | wire field without `serde(default)` / skipped on deserialize | 5, 8 |

K1 lands in the plugin and the core `it` binary: group per binary in one
build. Behaviour 9 has no control: it pins that A keeps today's exact
path (it would fail under option B, which is the comparison it exists
for).

## 7. Must confirm

- **M0 (decision).** Option A (exact, core + wire + schema) over B
  (plugin-only, over-selects on every wildcard-target create of such an
  entry and misses a second static target).
- **M1.** Schema bump to 17 (one full reindex per project on upgrade).
  Alternative: no bump, rows appear as importers are re-extracted; the
  bug then persists in old indexes until each importer is edited.
- **M2.** Cover `imports` keys with a static target too (symmetry with
  GM-666 F3), or `paths` only as the task states.
- **M3.** Add the conformance rule for `also_scopes` (only `File` scope,
  only `resolved_module`), or leave it unchecked.
- **M4.** Core never links at an extra scope; the re-extract links. A
  daemon killed between commit and `owe_reextracts` loses the re-extract
  (the same window GM-666 M7 accepted).
- **M5.** Whether the extractor generation needs a bump in addition to
  the schema bump (moot if M1 is accepted).

## 8. Risks

- Lifecycle drift: a delete/replace/swap site that forgets the new table
  leaves orphan rows that a later node with the same content-derived id
  inherits (GM-294 history). K4-K6 pin the three paths; any new delete
  site added later must include the table.
- ~40 mechanical wire literal edits across plugins and sdk: noisy diff,
  low logic risk.
- Row growth: one row per waiting import per static fallback; zero for
  configs without such entries.
- The unlinked report keeps showing only the primary scope; an owner may
  expect the fallback there too (out of scope).

## Cross-file answers relied on

- `find_callers(waiting_address)` (g-mesh): `waiting_addresses_for`,
  `miss_address` only.
- `find_callers(miss_address)`: ambiguous (field vs fn); the fn's only
  caller is the closure in `extractor/mod.rs` 132 (read by grep, a single
  known symbol).
- `find_callers(static_targets_naming)`: `waiting_addresses_for` only.
- `find_callers(expand_paths_candidates)`: `fallbacks_shadowed_by`,
  `resolve_tsconfig_paths`, `waiting_address`, plus a top-level use in
  `tests/project_model.rs`.
- `find_callers(waiting_importers)`: ambiguous; candidates
  `RoundTrip.waiting_importers`, `LinkedFileDiff.waiting_importers`,
  `file_rows::waiting_importers` (confirms GM-666 §10 is merged).
- `find_definition(PlaceholderTarget)`: `wire/src/lib.rs` 168-183.
- `find_references(PlaceholderTarget, answer=files)`: 25 resolved refs in
  6 files plus 32 unlinked usages in 11 files (not exact), so the literal
  sites were counted with `grep -rn "PlaceholderTarget {"`.
- `placeholder_targets` delete/copy sites: grep (SQL strings, not
  symbols): `write.rs` 445/570, `graph/imports.rs` 365,
  `graph/containers.rs` 546, `language_swap.rs` 226/524/564/704.
