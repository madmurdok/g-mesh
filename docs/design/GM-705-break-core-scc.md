# GM-705: move misplaced leaves out of daemon/watcher/graph

Findings: `docs/review/4.30/README.md` R-1, R-2, R-3, R-4 (step 1), R-6, R-8, R-10, R-30.
Owner decisions taken as given: old paths are re-exported for one release; R-3 goes to
`g-mesh-wire`; R-8 moves `LanguageOutcome` and its error encoding to `storage`; R-4 step 2
(`StateDir`) is GM-706. GM-717 lands first and adds `.claude` to `plugins/go/ignore.go`
`hardExcludedDirs`.

Line numbers are 1-based, on `release-4.31.0` at 944167ab. Reference counts come from
g-mesh (`find_references` with `answer: files`/`count`, `get_dependencies` Incoming, depth 1)
on project `g-mesh` (same code as this branch); the call that produced each is named.

## 0. Cycle checker and baseline

`scripts/module-cycles.py` (stdlib only) recomputes module-map.md's edge set: top-level
module = first path component under `core/src`; an edge is a `use crate::X` (group `use
crate::{a, b}` counts once per first-level target) or an inline `crate::X::` path; test files
(`tests/`, `testdata/`, `tests.rs`, `*_tests.rs`, `*_test.rs`) and `#[cfg(test)]` items are
excluded unless `--include-tests`. It prints SCCs (Tarjan), `--edges` prints the edge list,
`--absent 'A->B'` and `--max-scc N` exit 1 on failure, `--drop`/`--add 'A->B'` project a planned
move. Quote edges in a shell (`>` is a redirection).

Baseline (this branch, non-test):

```
SCC (13): cli, config, daemon, embedding, gc, graph, languages, mcp, project_walk, protocol, shim, storage, watcher
```

Same 13 modules as module-map.md section 5. The edge list matches section 3 except two inline
counts: `cli->embedding` 18/1 (map 18/2) and `mcp->daemon` 23/0 (map 23/1). The map counted a
`crate::embedding::` intra-doc link in a `//!` comment (`cli/model.rs:7`/`:43`) and
`mcp/query_shapes.rs:91`, which sits in a `#[cfg(test)] fn` (`:78`); the map's parser only
strips `#[cfg(test)] mod` blocks and does not strip comments. No edge appears or disappears, so
the SCC is the same. With `--use-only` the script gives {config, daemon, embedding, gc, graph,
languages, mcp, project_walk, protocol, storage, watcher} + {cli, shim}, consistent with the
map's "cycle exists with `use` statements alone: no" for cli<->daemon.

### Projection after this task

Edges that disappear entirely (each is the whole edge today, `--edges`):

| edge | sites now | removed by |
|---|---|---|
| project_walk->watcher | 1/0 | R-1 |
| protocol->graph | 3/0 | R-3 |
| shim->mcp | 1/0 | R-10 |
| languages->daemon | 1/0 | R-8 (with R-2) |
| storage->languages | 1/0 | R-8 |
| storage->daemon | 1/0 | R-4 step 1 |
| config->storage | 1/1 | R-4 step 1 (`config/mod.rs:54,251`) |
| embedding->daemon | 1/0 | R-6 |

New edges: `plugin_manifest->{graph, protocol, project_walk, languages}`, `languages->storage`
(re-export), `{daemon, mcp, cli}->plugin_manifest`, `indexing_status->paths`,
`{daemon, mcp, cli, embedding}->indexing_status`, `project_detect->{paths, storage}`,
`{shim, mcp, cli, daemon}->project_detect`.

`scripts/module-cycles.py` with those `--drop`/`--add` flags:

```
SCC (5): cli, daemon, mcp, shim, watcher
SCC (3): embedding, graph, storage
```

No new cycle: none of the new edges closes a loop (the re-export `languages->storage` is safe
because nothing reachable from `storage` reaches `languages` once `storage->languages` is
gone). `config`, `gc`, `languages`, `project_walk`, `protocol` leave the component. What keeps
the two remaining ones (out of scope, for the next review): `daemon->mcp` 2/2, `daemon->cli`
0/4, `cli<->shim`, `shim->daemon` 4/0 (3 left after R-30), `watcher->daemon` 1/0
(`watcher/apply.rs:422`, `daemon::config_reindex::select_with`, production code);
`graph<->storage`, `embedding<->storage`.

Order matters for one finding only: R-2 without R-8 leaves `languages -> plugin_manifest ->
graph -> storage -> languages` (review R-8), so S3 lands them together.

## R-1. `BASELINE_EXCLUDED_DIRS` into `g-mesh-walk`

- **Home:** `walk/src/lib.rs`, `pub const BASELINE_EXCLUDED_DIRS: [&str; 2] = [".git", ".claude"];`
  with the doc from `core/src/watcher/mod.rs:26-33` (drop the "same pair as ... sdk" sentence).
  The definition stays on one line: the Go test parses it (state that in its doc).
- **walk() prunes it itself** (review's pick). One place: `walker()` (`walk/src/lib.rs:235`)
  builds its name list as the baseline plus each `excluded` name not already in it. Every
  production caller already passes the baseline (`project_walk::excluded_names`,
  `core/src/project_walk.rs:168-171`; SDK `excluded_names`, `plugins/sdk/src/walk.rs:207-215`),
  so no walk result changes. Update the crate doc (`walk/src/lib.rs:6-7`, `:22`) and
  `walk()`'s doc (`:149-150`, "this crate adds no baseline of its own").
  Callers keep their `excluded_names` (the SDK's feeds `WalkScope::exclude_dirs`, which reports
  the baseline on the wire; core's is shared with `watcher::ignore_layers:45`).
- **Re-exports:** `core/src/watcher/mod.rs:34` becomes `pub use g_mesh_walk::BASELINE_EXCLUDED_DIRS;`;
  `plugins/sdk/src/walk.rs:50` the same (the SDK's public `lib.rs:117` re-export keeps working).
- **Call sites** (`find_references watcher::BASELINE_EXCLUDED_DIRS`: 2 files, complete):
  `core/src/project_walk.rs:31` -> `use g_mesh_walk::BASELINE_EXCLUDED_DIRS;` (merge into the
  existing `use g_mesh_walk::{...}` at `:28`); `core/src/watcher/ignore_layers.rs:20` ->
  `use g_mesh_walk::BASELINE_EXCLUDED_DIRS;`. Doc links in `project_walk.rs:17,53,168` resolve
  unchanged.
- **Go pin:** new `plugins/go/baseline_test.go`,
  `TestHardExcludedDirsAreRustBaselinePlusManifest`: read `../../walk/src/lib.rs`, find the line
  matching `pub const BASELINE_EXCLUDED_DIRS: [&str; N] = [...];`, parse the quoted names, fail
  if none; assert `hardExcludedDirs` equals exactly baseline ∪ `manifestExcludeDirs`
  (`ignore.go:43,47`). Same pattern as `manifest_test.go:20-38`. Control: drop `.claude` from
  `ignore.go`, or add a third name to the Rust constant: the test fails. Update the
  `ignore.go:45` comment to name the Rust constant. If GM-717 has not landed when S2 starts, the
  test fails on purpose; S2 then stops and reports.
- **Edge removed:** project_walk->watcher.

## R-3. Native kinds into `g-mesh-wire`

- **Shape: plain consts, not a `PlaceholderKind` enum.** Core compares stored strings
  (`native_kind` columns, SQL filters in `graph/queries.rs`, `storage/index_store.rs`), so an
  enum would add `.as_str()` at every core site for no type safety core uses; `CONTAINER` and
  `REEXPORT_ALL_NAME` are not placeholder kinds and would not fit the enum anyway. The SDK keeps
  its own `PlaceholderKind` enum (`plugins/sdk/src/graph.rs:208`), now returning the wire
  consts from `native_kind()` (`:236-241`) instead of literals, so its divergence is impossible
  by construction rather than caught by the conformance kit.
- **Home:** new `wire/src/native_kind.rs`, `pub mod native_kind;` in `wire/src/lib.rs`. Names
  kept as today so every move is a path change: `PENDING_SYMBOL_NATIVE_KIND`,
  `REEXPORT_NATIVE_KIND`, `REEXPORT_ALL_NAME`, `RESOLVED_MODULE_NATIVE_KIND`,
  `EXTERNAL_MODULE_NATIVE_KIND`, `CONTAINER_NATIVE_KIND`, each with its doc. Core imports them
  as `g_mesh_wire::native_kind::...` (an external-crate path, so no module edge; not through
  `protocol::types`, whose `pub use g_mesh_wire::*` would add a graph->protocol site).
- **Moves (const + doc):** `graph/symbol_links.rs:302-305`, `:307-311`, `:313-317`;
  `graph/imports.rs:94-97`, `:99-116`; `graph/containers.rs:195-197`.
- **Re-exports:** each old place keeps `pub use g_mesh_wire::native_kind::<NAME>;`.
- **Call sites to migrate** (all of `core/src`, tests included; `find_references`,
  `answer: files`): PENDING_SYMBOL 17 refs / 9 files (`graph/containers/tests.rs`,
  `mcp/overrides_tests.rs`, `graph/queries.rs`, `protocol/conformance.rs`,
  `storage/index_store.rs`, `graph/symbol_links.rs`, `mcp/unlinked.rs`,
  `storage/language_swap.rs`, `watcher/apply/tests.rs`); REEXPORT 9/6 (`graph/containers/tests.rs`,
  `graph/queries.rs`, `protocol/conformance.rs`, `graph/symbol_links.rs`, `mcp/unlinked.rs`,
  `watcher/apply/tests.rs`); RESOLVED_MODULE 9/5 (`protocol/conformance.rs`,
  `graph/containers/tests.rs`, `graph/queries.rs`, `cli/deps/tests.rs`, `graph/imports.rs`);
  EXTERNAL_MODULE 11/6 (`cli/deps.rs`, `cli/plugin_check/checks.rs`, `graph/containers/tests.rs`,
  `graph/queries.rs`, `mcp/find_definition/tests.rs`, `graph/symbol_links/tests.rs`); CONTAINER
  5/3 (`graph/queries.rs`, `protocol/conformance.rs`, `storage/language_swap.rs`);
  REEXPORT_ALL_NAME 7/3 (`graph/symbol_links.rs`, `graph/imports.rs`, `watcher/apply/tests.rs`).
  Must-change for the cycle: `protocol/conformance.rs:7-9`. Also `graph/queries.rs:4-6` (the
  intra-`graph` cycle the review notes) and `storage/language_swap.rs:57-58`.
- **Go pin (same pattern as R-1):** `plugins/go/native_kind_test.go` reads
  `../../wire/src/native_kind.rs` and asserts `pendingSymbolNativeKind`,
  `resolvedModuleNativeKind`, `externalModuleNativeKind`, `reexportAllName`
  (`plugins/go/extract.go:107-124`) equal the Rust values. Update the `extract.go:104-106`
  comment. Cheap and closes the third copy the review lists.
- **Edge removed:** protocol->graph. `conformance` stays in `protocol`.

## R-10. Front-mode constants into `protocol`

- **Home:** new `core/src/protocol/front.rs` (`pub mod front;` in `protocol/mod.rs`) with
  `SELECT_PROJECT` (`mcp/front.rs:36-37`), `SWITCH_PROJECT_META` (`:39-41`), `PROJECT_ARG`
  (`:48-51`). The two `*_DESCRIPTION` consts stay in `mcp/front.rs` (MCP text, not protocol).
- **Re-export:** `mcp/front.rs` keeps `pub use crate::protocol::front::{PROJECT_ARG, SELECT_PROJECT, SWITCH_PROJECT_META};`.
- **Call sites** (`find_references`: SELECT_PROJECT 4 refs / 2 files, PROJECT_ARG 5/3,
  SWITCH_PROJECT_META 3/2): `shim/router.rs:129` -> `use crate::protocol::front::{...}`;
  `mcp/front.rs` uses them through its own re-export; `mcp/tools_list_tests.rs` migrates.
- **Edge removed:** shim->mcp.

## R-2. Plugin-manifest model to top-level `plugin_manifest`

- **Move:** `core/src/daemon/manifest.rs` (921 lines) -> `core/src/plugin_manifest.rs`;
  `core/src/daemon/manifest/tests.rs` -> `core/src/plugin_manifest/tests.rs`. `pub mod
  plugin_manifest;` in `lib.rs`. Crate imports stay `graph::symbol_links::LinkRules` and
  `protocol::types::CURRENT_PROTOCOL_VERSION` (`:20-21`).
- **Trap:** `daemon/manifest/tests.rs:896` `include_str!("../../../../plugins/typescript/plugin.toml")`
  loses one `../` after the move. `env!("CARGO_MANIFEST_DIR")` paths (`manifest.rs:87`,
  `tests.rs:6,647,1216`) are unaffected.
- **Re-export:** `daemon/mod.rs:13` `pub mod manifest;` -> `pub use crate::plugin_manifest as manifest;`.
- **Call sites** (`get_dependencies` Incoming on `g_mesh::daemon::manifest`, depth 1: 61 files,
  complete): every `core/src` site migrates to `crate::plugin_manifest`, including daemon's own
  `super::manifest` (`daemon/front.rs:30` and the rest); bare `manifest::` inside
  `daemon/mod.rs` may resolve through the re-export. The 15 `core/tests/it` and
  `core/tests/scripts` importers stay on `g_mesh::daemon::manifest`, which exercises the
  re-export. Acceptance: `grep -rn "daemon::manifest\|super::manifest" core/src` lists only
  `daemon/mod.rs`. Non-test importers outside daemon: `languages.rs`, `mcp/{instructions,
  find_implementations, overrides, find_callers_callees, mod, provenance, query_shapes,
  similarity, find_references}.rs`, `cli/{init, reindex, deps, status, plugin_install, plugins}.rs`,
  `cli/plugin_check/{expectations, session, mod, checks}.rs`.
- **Path helpers to `project_walk`** (with R-8): `extension_of` (`manifest.rs:644-647`) and
  `under_excluded_dir` (`:649-660`), both `pub(crate)`, next to `under_any`
  (`project_walk.rs:85`). `find_references`: extension_of 7 refs in `cli/plugin_check/checks.rs`,
  `daemon/registry.rs`, `languages.rs`, `manifest.rs`; under_excluded_dir 9 refs, same files.
  No re-export (crate-private).
- **Edge removed:** none alone; `languages->daemon` goes with R-8.

## R-8. `languages` decoupled

- **Trait, in `languages.rs`**, the only two questions `languages` asks of discovery
  (`languages.rs:157-260`: `discovered.manifests.contains_key` at `:205,:217`,
  `discovered.language_for` at `:213`):

  ```rust
  /// The plugins a walk found, as the catalogue needs them.
  pub trait DiscoveredLanguages {
      /// Whether a plugin for `language` was discovered.
      fn has_plugin(&self, language: &str) -> bool;
      /// The language whose plugin claims `file_path` by extension.
      fn routed_language(&self, file_path: &str) -> Option<&str>;
  }
  ```

  `impl languages::DiscoveredLanguages for DiscoveredPlugins` lives in `plugin_manifest.rs`
  (delegating to `manifests.contains_key` and the inherent `language_for`), so the edge is
  `plugin_manifest -> languages`, never the reverse. `missing`, `absent_for_path`,
  `count_absent_files` (`:157,174,187`) and the private `*_in` helpers (`:204-260`) take
  `discovered: &impl DiscoveredLanguages`; callers passing `&DiscoveredPlugins` compile
  unchanged. Distinct method names avoid shadowing the inherent `language_for`.
  `languages.rs:42` becomes `use crate::project_walk::{self, extension_of, under_excluded_dir};`.
- **`LanguageOutcome` to storage:** new `core/src/storage/language_outcome.rs` (`pub mod
  language_outcome;` in `storage/mod.rs`) with `LanguageOutcome` (`languages.rs:75-90`),
  `failed_error` (`:92-109`), `error_on_one_line` (`:111-115`). The doc link to
  `CatalogueEntry::install_command` becomes `crate::languages::CatalogueEntry::install_command`
  (doc only, no edge). `storage/schema.rs:6` imports from `super::language_outcome`.
- **Re-export:** `languages.rs` keeps `pub use crate::storage::language_outcome::{error_on_one_line, failed_error, LanguageOutcome};`
  (edge languages->storage; no cycle, see section 0).
- **Call sites** (`find_references`): LanguageOutcome 24 refs / 13 files (`cli/mod.rs`,
  `cli/status.rs`, `daemon/bulk_index.rs`, `storage/schema.rs`, `cli/deps.rs`,
  `mcp/instructions.rs`, `core/tests/it/one_plugin_binary_missing.rs`, `cli/status/json.rs`,
  `daemon/activation.rs`, `daemon/registry.rs`, `daemon/workspace_reindex.rs`,
  `mcp/not_indexed.rs`, `storage/schema/tests.rs`); failed_error 5/5 (`cli/mod.rs`,
  `daemon/activation.rs`, `daemon/bulk_index.rs`, `languages/tests.rs`,
  `mcp/instructions/tests.rs`); error_on_one_line 1/1 (`cli/deps.rs`). Migrate `core/src`;
  the `core/tests/it` file stays on the re-export.
- **Edges removed:** languages->daemon, storage->languages.

## R-4 step 1. Identity and project directories into `paths::state_dir`

- **Home:** `core/src/paths/state_dir.rs`, `pub mod state_dir;` in `paths.rs` (`paths.rs` +
  `paths/` directory). `paths` stays a leaf: `state_dir` imports std, anyhow, sha2 only.
- **Moves:** all of `daemon/identity.rs` (`project_hash` `:6-18`, `ROOT_FILE` `:20-30`,
  `DEVICE_FILE` `:32-38`, `record_project_root` `:56`, `read_root_device` `:80`,
  `read_project_root` `:92`, tests `:101-201`); from `storage/connection.rs`:
  `projects_root` / `projects_root_under` (`:10-21`), `project_dir` / `project_dir_under`
  (`:23-35`), `ensure_project_dir` / `ensure_project_dir_under` (`:37-72`), with the
  connection.rs tests that exercise them (one, `:206`, reads `read_project_root`). Docs move
  verbatim (keeps `git diff -M` a move; comment cleanup is not part of this task).
- **Pinned hash test:** split `project_hash` into the filesystem step and a pure
  `pub(crate) fn hash_of_canonical(canonical: &Path) -> String` (sha256 of
  `to_string_lossy()`, first 8 bytes, lowercase hex). Test
  `a_project_hash_never_changes`: `hash_of_canonical(Path::new("/srv/project")) ==
  "4f3f91f6b0b47e3b"` (computed: `python3 -c "import hashlib;
  print(hashlib.sha256(b'/srv/project').digest()[:8].hex())"`), plus `project_hash(tmp) ==
  hash_of_canonical(&tmp.canonicalize())`. `to_string_lossy` does not rewrite separators, so
  the literal holds on Windows too. A changed value orphans every index; the test doc says so.
  Control: change `take(8)` to `take(7)` -> fails.
- **Re-exports:** `daemon/identity.rs` becomes `pub use crate::paths::state_dir::{project_hash, read_project_root, read_root_device, record_project_root, DEVICE_FILE, ROOT_FILE};`;
  `storage/connection.rs` gets `pub use crate::paths::state_dir::{ensure_project_dir, project_dir, projects_root};`
  and `pub(crate) use` of the three `_under` variants.
- **Call sites to migrate.** Production, all of them: `storage/connection.rs:7` (gone with the
  move), `config/mod.rs:54,251` (removes config->storage), `cli/clean.rs:50` and its
  `daemon::identity::` paths (`:80-:1734`, 25 sites incl. tests in the same file),
  `shim.rs:667`, and the non-test `connection::project_dir` importers (`shim.rs`,
  `cli/{deps, reindex, stop, status, init}.rs`, `daemon/{registry, semantic,
  workspace_reindex, candidates, mod, seed}.rs`); `ensure_project_dir` 10 refs
  (`daemon/mod.rs`, `shim.rs`, `cli/reindex.rs`, `config/mod.rs`); `projects_root` 7 refs
  (`cli/clean.rs`, `gc/warning.rs`); `project_hash` 15 refs / 7 files (`find_references`).
  **Test code stays on the re-exports**: `project_dir` has 243 references in 87 files
  (`find_references ... count`, grep for files), mostly tests, and GM-706 (`StateDir`)
  rewrites those same call sites; migrating them twice is churn. `core/tests/it` (4 files on
  `daemon::identity`) stays too.
- **Edges removed:** storage->daemon, config->storage.

## R-6. `IndexingStatus` to top-level `indexing_status`

- **Move:** `core/src/daemon/indexing_status.rs` (1020 lines, tests inline `:677-1020`) ->
  `core/src/indexing_status.rs`; `pub mod indexing_status;` in `lib.rs`.
- **`write_state_file_atomic`** (`daemon/mod.rs:278-289`, `pub(crate)`) moves to
  `paths::state_dir` (it writes a file into a state directory; GM-706's `StateDir` will own it).
  Its log prefix "g-mesh daemon:" stays (same messages). `daemon/mod.rs` keeps
  `write_pid_file` (`:271-276`) calling `crate::paths::state_dir::write_state_file_atomic`.
  `find_references`: 5 refs in `daemon/front.rs` (2), `daemon/indexing_status.rs` (2,
  `:354,:492` `super::write_state_file_atomic`), `daemon/mod.rs` (1). No re-export (crate-private).
  Rejected: leaving it in `daemon` (keeps indexing_status->daemon) or duplicating it.
- **Re-export:** `daemon/mod.rs:11` `pub mod indexing_status;` -> `pub use crate::indexing_status;`.
- **Call sites** (`get_dependencies` Incoming on `g_mesh::daemon::indexing_status`, depth 1:
  19 importers): `embedding/backfill.rs:102` (+ doc links `:127,:174`), `mcp/{instructions,
  search_code, mod}.rs`, `mcp/{search_code_worker, search_code_wait,
  semantic_rung_worker}_tests.rs`, `mcp/instructions/tests.rs`, `cli/{init, reindex, deps,
  status}.rs`, `cli/status/tests.rs`, `daemon/{semantic, mod, activation, seed, bulk_index}.rs`,
  `daemon/seed/tests.rs`. Migrate all; same grep acceptance as R-2.
- **Edge removed:** embedding->daemon.

## R-30. `daemon::candidates` to top-level `project_detect`

- **Move:** `core/src/daemon/candidates.rs` (979 lines, tests inline from `:477`) ->
  `core/src/project_detect.rs`; `pub mod project_detect;` in `lib.rs`. Its imports
  (`:26-27`) become `crate::paths::state_dir::project_dir` and `crate::storage::schema`. The
  inline test at `:584` uses `crate::daemon::registry::fixture_indexer_version` (test-only
  edge project_detect->daemon; acceptable, not counted).
- **Re-export:** `daemon/mod.rs:6` `pub mod candidates;` -> `pub use crate::project_detect as candidates;`.
- **Call sites** (grep for one known module path; g-mesh `get_dependencies` not run for this
  one): `shim.rs:18`, `shim/router.rs:128`, `mcp/instructions.rs:23`, `mcp/front.rs:32`,
  `mcp/instructions/tests.rs:411`, `cli/debug_candidates.rs:14`, `cli/deps.rs:32`,
  `cli/status.rs:407`, `daemon/front.rs:28` (`super::candidates`), `daemon/mod.rs:337-338`
  (bare path, may use the re-export).
- **Edge removed:** none entirely (`shim->daemon` drops from 4 to 3 sites).

## Slicing check

Planned: S2 sonnet (R-1, R-3, R-10), S3 opus (R-2, R-8), S4 sonnet (R-4 step 1, R-6, R-30),
S5 review, S6 tests, S7 verify. The order holds: R-6 and R-30 import from `paths::state_dir`
(R-4) and sit in the same slice after it; R-2/R-8 are independent of S2 and S4. Edits
overlap only in import lines (`shim.rs`, `cli/{deps, status}.rs`, `mcp/instructions.rs`,
`daemon/mod.rs`), and slices run in sequence.

- **S2 is mechanical** with two small non-path pieces fully specified here: the baseline merge
  in `walker()` and two Go tests. It depends on GM-717 having landed (R-1 Go test).
- **S4 is mechanical** except `hash_of_canonical` (a pure extraction, value given) and moving
  `write_state_file_atomic`; both specified. Its migration list is the largest (production
  `project_dir` importers, 13 files, plus `cli/clean.rs`).
- **S3 needs opus** as planned: the trait and the 61-file migration with the `include_str!`
  trap.
- **S6** should add, besides the controls named above: a `walk` crate test that `walk(root,
  &[])` skips `.git/` and `.claude/` (control: remove the baseline merge); and a
  `core/tests/scripts` test that runs `scripts/module-cycles.py --absent` on the eight removed
  edges and `--max-scc 5` (control: re-add `use crate::watcher::...` in `project_walk.rs`).
  `scripts/module-cycles.py` changes select `core-it core-scripts core-cli`
  (`scripts/test-select.sh:197`).
- **S7** runs `scripts/module-cycles.py` and expects exactly the two projected SCCs above.

## g-mesh calls behind this note

`find_references` (answer `files` unless noted): `watcher::BASELINE_EXCLUDED_DIRS` (2 files,
complete); the six native-kind consts; `mcp::front::{SELECT_PROJECT, PROJECT_ARG,
SWITCH_PROJECT_META}`; `daemon::manifest::{extension_of, under_excluded_dir}`;
`languages::{LanguageOutcome, failed_error, error_on_one_line}`;
`storage::connection::project_dir` (`count`: 243); `daemon::identity::project_hash`;
`storage::connection::{ensure_project_dir, projects_root}`; `daemon::write_state_file_atomic`.
`get_dependencies` Incoming depth 1: `core/src/daemon/manifest.rs` (resolved to
`g_mesh::daemon::manifest`, 61 files, `truncated: false`), `g_mesh::daemon::indexing_status`
(19 files). One failed call: `find_references g_mesh_walk::walk` returned only a semantic
neighbour (the walk crate's functions are not resolved by that name), so `g_mesh_walk::`
callers came from grep (core: `project_walk.rs:57,145,428`, `watcher/ignore_layers.rs:45`;
SDK: `walk.rs:126,172`). `daemon::candidates` importers and `daemon::identity::` paths also
came from grep (single known module path).
