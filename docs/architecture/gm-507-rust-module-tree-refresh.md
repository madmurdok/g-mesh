# GM-507: a Rust module file added mid-session gets its container without a Cargo.toml save

Status: design (S1). Background: GM-350 / ADR 0023
(`docs/adr/0023-project-model-tracks-file-presence.md`,
`docs/architecture/gm-350-ts-resolution-placement.md`). Depends on GM-509
(`feat/GM-509-selective-config-reindex`, note §3 and §10) and on GM-509's
Rust+Python follow-up task (owner decision Q5 there).

Line numbers are 1-based, at `a98f636` (release-4.3.0).

## 1. Today

`ProjectContext::load` (`plugins/rust/src/project/mod.rs:198-256`) builds the
module tree once: for each crate, `module_tree::scan_crate`
(`module_tree.rs:122-130`) reads the crate root from disk, follows each
`mod x;` through `handle_file_mod` (`:367-389`) and `scan_file` (`:143-175`),
and records `RelPath -> (key, parent)` in `files`. `container_for`
(`mod.rs:268-273`) answers `Orphan { key: "orphan:<path>" }` for any path not
in `files`. `has_container` (`mod.rs:295-297`) answers from the same map, and
`decls.rs:725` uses it to emit the extra IMPORTS edge for `use a::b::c;` when
`c` is a submodule.

The model is rebuilt only by `workspaceChanged`, which core sends only on a
watch-file save (`Cargo.toml`): `workspace_reindex::run_with` is the one caller
of `PluginProcess::notify_workspace_changed` (g-mesh `find_callers`). A `.rs`
save reaches the SDK as `fileChanged`, where `Session::file_changed`
(`plugins/sdk/src/run.rs:570-639`) calls `presence_changed` and then
`extract(&Project, ...)`. The Rust plugin keeps the default no-op
`file_presence_changed`. g-mesh `find_callers` on the trait method found no
static caller (the call is through the generic `E`); the SDK call sites are
`presence_caught` (`run.rs:852-870`), reached from `presence_changed`
(`:695-702`), which `file_changed`, `files_created` (`:674-691`) and
`hydrate` (`:542-568`) call. Implementers of `Extractor` (g-mesh
`find_implementations`): `RustExtractor`, `PythonExtractor`,
`TypeScriptExtractor`, `ToyExtractor` and four SDK test doubles. Only TS and
Python override `file_presence_changed`.

Core sends `filesCreated` from `PluginRegistry::announce_created` ->
`PluginSupervisor::files_created` -> `PluginProcess::notify_files_created`
(g-mesh `find_callers`, chained). Each created file then gets its own
`fileChanged` through `apply_file_change_in` (`core/src/watcher/apply.rs:122-190`),
which `apply_file_change` and `staleness::ensure_fresh` call. A
`fileChanged` answer is one file's `FileChangeDiff` (`wire/src/lib.rs:660-680`).
**A plugin cannot ask core to re-extract another file today.**

### Example

Crate `alpha`: `src/lib.rs` has `mod util;`, `src/util.rs` exists. The user
writes `src/util/fmt.rs` (it declares `pub fn pad()`), then adds `pub mod fmt;`
to `src/util.rs`.

1. `src/util/fmt.rs` created: `filesCreated`, then `fileChanged`. `container_for`
   answers `Orphan { key: "orphan:src/util/fmt.rs" }`, so `pad` is stored
   under `orphan:src/util/fmt.rs`.
2. `src/util.rs` saved: re-extracted against the same model. Its own `mod fmt;`
   member is emitted from its text, but `has_container("alpha::util::fmt")` is
   false, and `fmt.rs` is not touched.
3. `alpha::util::fmt::pad` is not findable by its module, and
   `use crate::util::fmt;` elsewhere gets no IMPORTS edge, until someone saves
   `Cargo.toml`.

The reverse order (the `mod` line first, the file second) fails the same way:
the scan noted "`mod fmt;` has no file on disk" and dropped the declaration,
so the presence of `fmt.rs` later changes nothing.

## 2. Options

### D1. Where the re-scan happens

| | Option | Benefit | Risk |
|---|---|---|---|
| A | Rust overrides `file_presence_changed` and diffs the file's `mod` items (re-reading it from disk) | No SDK change | The hook has no source text and no return value: it cannot report which *other* files moved. Second disk read per save. Stretches ADR 0023's "presence" contract into "content changed" |
| B | New SDK default hook `source_changed(&self, &mut Project, path, Option<&str>) -> Option<ResolutionDelta>`, called by `Session::file_changed` before `extract`; the SDK copies the answer into the diff | Gets the text already read, has `&mut Project`, has a return channel; other plugins keep the no-op default | One more trait method (public SDK surface) |
| C | Interior mutability (`Project = Mutex<ProjectContext>`), `extract` mutates | No trait change | `extract` stops being pure; container keys depend on extraction order, which `module_tree`'s Decision 1 rules out. Still no return channel |

### D2. A child extracted before its parent's `mod` line

Core has no plugin-initiated re-extract (§1). Options:

| | Option | Benefit | Risk |
|---|---|---|---|
| A | Ride GM-509: `FileChangeDiff.affected: Option<ResolutionDelta>` (GM-509 §3.7), core runs GM-509's select + re-extract loop after applying the diff | One mechanism for "the model moved under these files"; threshold, `reextract`, scoped semantic pass already designed | GM-507 waits for GM-509 and its Rust follow-up |
| B | The parent's diff also carries the child's nodes | No core loop | The SDK has no baseline for the child in general; core's `complete` semantics are per file. Breaks the one-file diff contract |
| C | The plugin answers "workspace changed" and core runs the whole-language reindex | Small | A Cargo.toml-sized cost (seconds) on every `mod` edit: the cost GM-509 exists to remove |

### D3. Scope of the re-scan

| | Option | Benefit | Risk |
|---|---|---|---|
| A | Whole project (`ProjectContext::load`) | Trivially equal to a cold load | Re-reads every manifest and every module file on a `mod` edit |
| B | The crate(s) whose tree holds the file, via `scan_crate`, gated on the file's `mod` items changing | Equals a cold load for that crate (scan is a pure function of disk), cost ~ that crate's module files, paid only when `mod` items changed | Needs a per-file `mod` signature; a created orphan needs a crate to try (see 3.3) |
| C | Incremental: re-scan only the subtree of the changed `mod` item | Cheapest | "First claim wins" and cfg alternatives depend on scan order; incremental can disagree with a cold load |

### D4. The reverse

Same path as the forward case: a removed `mod` item changes the parent's
signature, a deleted file is `source_changed(path, None)`. The crate re-scan
drops the subtree, and every dropped path is in `affected`, so core
re-extracts it as `Orphan`. The deleted file itself is removed by its own diff.

## 3. Recommendation

**D1 = B, D2 = A, D3 = B, D4 as above.** GM-507 lands after GM-509 and its
Rust follow-up, and adds only the `.rs` trigger and the one optional field.

### 3.1 SDK

- `Extractor::source_changed(&self, &mut Project, &RelPath, Option<&str>) -> Option<ResolutionDelta>`,
  default `None`. Called in `Session::file_changed` after the unchanged-text
  short-circuit (unchanged text cannot change `mod` items) and before
  `extract`; on the deletion branch with `None`. Panics caught like
  `presence_caught`.
- `FileChangeDiff.affected: Option<ResolutionDelta>` (serde default, skipped
  when `None`): the SDK sets it from the hook.

### 3.2 Rust plugin

- `ProjectContext` keeps, per scanned file, its `mod` signature: the ordered
  `(key segments, name, explicit #[path], cfg-free)` list `scan_body` saw,
  including inline bodies. And `pending`: candidate paths of `mod` items that
  named nothing on disk (`handle_file_mod`'s note branch), mapped to the crate.
- `source_changed`:
  - text given, path in `files`: compute the signature from the text (the
    `mask` + `read_mod_item` pass of `scan_body`, without recursion). Equal ->
    `None`. Different -> re-scan that file's crate.
  - text given, path not in `files`: if `pending` holds it, re-scan that crate;
    otherwise `None`.
  - `None` (deleted), path in `files`: re-scan its crate.
- Re-scan = `scan_crate` into a fresh map for that crate, splice it into
  `files`, rebuild `container_keys` and `pending` for that crate. The delta is
  GM-509's `container_delta(old, new, "::")` restricted to that crate:
  `files` = paths whose `(key, parent)` changed or that appeared/disappeared,
  `imports` = GM-509's selectors for added/removed keys, minus the path being
  extracted now (it is re-extracted by this same round trip).

### 3.3 Core

- `apply_file_change_in` reads `affected` from the structural round trip.
  `Some(Affected)` -> GM-509's `select_affected` + per-file `reextract`
  loop + threshold, in the caller's exclusive section, then one scoped
  `semanticPass` (it replaces the single-file pass this function sends today).
  `Unknown` -> whole-language reindex. Re-extract round trips ignore their
  own `affected` (no recursion; their text did not change, so it is `None`
  anyway).
- Crash safety: see must-confirm 3.

### 3.4 Cost

A `.rs` save with unchanged `mod` items costs one masked scan of that file's
text (already in memory). A `mod` edit costs one crate scan: reads only the
crate's module files, regex-free, linear. Measure on g-mesh `core` (the
largest crate here) in the tests slice; expected a few ms, against today's
"never" plus a Cargo.toml reindex (seconds).

## 4. Edit map

| Layer | fn / file (lines) | Change | Kind |
|---|---|---|---|
| wire | `FileChangeDiff` `wire/src/lib.rs:660-680` | `affected: Option<ResolutionDelta>` (type from GM-509) | code |
| SDK | `Extractor` `plugins/sdk/src/lib.rs:143-198` | `source_changed` default method | code |
| SDK | `Session::file_changed` `run.rs:570-639` | call the hook (both branches), set `affected` | code |
| SDK | `presence_caught` `run.rs:852-870` | a sibling `source_changed_caught` | code |
| SDK | `run.rs` tests | hook called once per changed text, not on unchanged text; `affected` reaches the wire; panic costs only the delta | tests |
| plugin | `ProjectContext` `project/mod.rs:161-177`, `load` `:198-256` | per-file signature, `pending`, per-crate splice; `rescan_crate_of(path)` | code |
| plugin | `scan_crate`/`scan_body`/`handle_file_mod` `module_tree.rs:122-389` | record signature and missing-file candidates; expose a non-recursive signature pass | code |
| plugin | `RustExtractor` `extractor/mod.rs:140-190` | implement `source_changed` | code |
| plugin | `project/mod.rs` tests, `extractor/tests` | forward both orders, reverse both ways, unchanged signature -> `None`, equals a cold `load` | tests |
| core | `apply_file_change_in` `core/src/watcher/apply.rs:122-190`, `round_trip` `:511` | surface `affected`, run GM-509's loop | code |
| core | `config_reindex.rs` (GM-509) | expose select + re-extract for a non-watch trigger | code |
| core | `core/src/daemon/tests.rs` | end to end with the real Rust plugin: create child, add `mod`, query the container; then remove | tests |

No change to Python, TypeScript or Go.

## 5. Behaviours

1. Child file first, `mod` line second: the child is re-extracted under `alpha::util::fmt` without a Cargo.toml save.
2. `mod` line first, file second: the file's own `fileChanged` places it (via `pending`), no extra re-extract.
3. Removing the `mod` line turns the child and its subtree back into orphans.
4. Deleting a module file turns its descendants into orphans.
5. A save that does not change `mod` items sends no `affected` and re-scans nothing.
6. After any sequence of edits, the model equals `ProjectContext::load` on the same disk.
7. `use crate::util::fmt;` in another file gains its IMPORTS edge (only if Q2 = A).
8. A selection above GM-509's threshold runs the whole-language reindex.
9. A re-extract round trip does not trigger another round.

## 6. Must-confirm (implementers)

1. GM-509 ships `ResolutionDelta`, `FileChangedParams.reextract`, `container_delta`
   and a select + re-extract entry point callable from `apply_file_change_in`
   (it is reached today only from `workspace_file_changed`).
2. Rust's in-crate `use crate::…` placeholders store a target scope that
   GM-509's `Target{container}` can match for an *added* key (see Q2).
3. Crash mid-loop: GM-509's `pending_reindex` resumes through
   `workspace_file_changed(trigger)`, which is wrong for a `.rs` trigger. Either
   store the selected paths as owed (GM-498's `owed_files` pattern) or accept
   that a crash leaves them stale until their next save.
4. `ensure_fresh` also calls `apply_file_change_in`: a query-time freshness
   check may now re-extract several files. Confirm the latency is acceptable.
5. The re-scan reads sibling files from disk while the trigger's text came
   from `read_source`: same save, but a concurrent write can differ.

## 7. Risks

- Stored GM-509 facts are not updated by a `.rs`-triggered move, so the next
  Cargo.toml delta compares against older facts. It over-selects (re-extracts
  files already right), never misses, because both loads read the same disk.
- `#[path]` pointing outside the package at a file created later: `pending`
  covers it; without `pending` it would wait for the declaring file's save.
- A crate root added or removed (`src/lib.rs` created) still needs a Cargo.toml
  save: the crate set comes from `resolve_targets`, not from `mod` items. Out of
  scope.
- SDK surface grows by one method; third-party plugins are unaffected (default).

## 8. Open questions for the owner

### Q1. A new SDK hook, or reuse the presence hook

Today the Rust plugin cannot change its project model on a `.rs` save: the
only hook with `&mut Project`, `file_presence_changed(path, present)`, gets no
text and returns nothing. Change: add `source_changed(path, text) -> delta`.
Example: saving `src/util.rs` with a new `pub mod fmt;` must move
`src/util/fmt.rs`.
- **A (recommended) new hook:** the plugin sees the text it is about to
  extract and names the moved files. Cost: one more public trait method.
- **B presence hook:** no SDK change, but the plugin re-reads the file from
  disk and still needs a separate way to name `fmt.rs`, so the SDK changes
  anyway.

### Q2. Re-extract the importers of a newly added module

Today `use crate::util::fmt;` written before `fmt` exists gets a REFERENCES
placeholder to `alpha::util` / `fmt`, and no IMPORTS edge, because
`has_container("alpha::util::fmt")` is false. Adding the module makes that
edge correct, but only if the importing file is re-extracted.
- **A (recommended) select importers of the parent key** (`alpha::util`):
  `get_dependencies` becomes right at once; over-selects every importer of
  `alpha::util` on a `mod` edit.
- **B skip:** only the module file and its subtree move; the missing IMPORTS
  edge appears at that importer's next save. A missing edge, never a wrong one.

### Q3. A large move: per-file re-extract or whole-language reindex

Today a `.rs` edit never re-extracts other files. With this change,
adding `mod big;` above a 300-file subtree selects 300 files.
- **A (recommended) reuse GM-509's threshold** (30% of the language's
  files -> whole-language reindex): one rule for both triggers; a big move
  costs a reindex (seconds).
- **B always per file:** no reindex on a `.rs` save; 300 sequential round
  trips (about 15 ms each, GM-324) inside one exclusive section block queries
  for several seconds.

## 9. Owner decisions (2026-10-09)

- Q1: "Новый хук source_changed (Recommended)" — add `Extractor::source_changed`.
- Q2: "Да, импортёров родителя (Recommended)" — select the importers of the parent key; behaviour 7 is in scope.
- Q3: "Порог GM-509: 30% (Recommended)" — reuse GM-509's threshold.
- Q4 (must-confirm 3): "Записать файлы как owed (Recommended)" — the selected paths are stored as owed before the loop (GM-498's `owed_files` pattern) and resumed after a crash.
