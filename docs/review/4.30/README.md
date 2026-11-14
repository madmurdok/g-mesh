# g-mesh 4.30 architecture review: reconciled report (GM-704)

Inputs: `module-map.md` (shared facts) and eight blind reviews, one per area and model:
A runtime, B data/indexing, C surfaces (mcp/cli/config), D plugins/wire/walk/scripts/CI;
`*-opus.md` = Opus 5.5, `*-fable.md` = Fable 5.2. Goal set by the owner: refactors that let
weakly coupled parts of g-mesh be changed independently.

Verification for this report (S10): every single-model finding, every contradiction and every
side discovery was spot-checked against the worktree code (grep on a known file or a g-mesh call
on project `g-mesh`); agreed findings were re-checked only where the two reports cited different
facts. No build or test was run. Verdict evidence is given as `file:line` or as the g-mesh call.

## 1. Summary

- All of core except `ipc`, `log`, `main`, `paths`, `process` sits in one 13-module strongly
  connected component (module-map section 5). Both models, independently and in every area, found
  that it is held together mostly by **misplaced leaves, not by the runtime's real design**: the
  plugin-manifest model, project identity / state-dir layout and `IndexingStatus` live in
  `daemon`; one exclude constant lives in `watcher`; placeholder kinds live in `graph`; daemon stop
  and orphan purge live in `cli`. R-1..R-6 are S/M moves that remove most cycle edges.
- The two real structural cycles are inside the data layer: `storage <-> graph` and
  `storage <-> embedding` (storage orchestrates linking and vector writes). R-16 breaks them.
- The surface depends on the concrete runtime: `mcp` takes `Arc<PluginRegistry>` and the registry
  builds an `mcp` type (R-17); handler entry points multiply per input (R-19); SQL against the
  schema is written in `mcp`, `cli`, `daemon`, `watcher` (R-22, R-23).
- Plugin side: cross-language contracts (exclude dirs, ids, wire, `plugin.toml`) are kept equal by
  comments; two of them have already drifted (Go lacks `.claude`; Go and SDK hash ids differently).
  Shared fixtures and shared schemas (R-21, R-24) and one source for plugin lists (R-13) fix this.
- 35 merged findings survive (26 agreed by both models, 9 single-model confirmed); 3 single-model
  proposals are rejected; 13 contradictions are resolved below.

## 2. Ranked findings

Ranked by benefit/effort, highest first. Status: **agreed** = both models found it (possibly in
different areas); **single ✓** = one model, confirmed in code here; **single ✗** = one model,
rejected. Effort S/M/L as estimated by the reviews, adjusted where noted.

| ID | Title | Area | Sources | Status | Effort | Risk | Benefit |
|---|---|---|---|---|---|---|---|
| R-1 | Baseline excluded dirs defined once in `g-mesh-walk` (and fix Go's missing `.claude`) | A,B,D | A-opus-2, A-fable-6, B-opus-2, B-fable-2, D-opus-1 | agreed | S | low | One edit for every walker; removes `project_walk<->watcher`; fixes a live drift in Go |
| R-2 | Plugin-manifest model out of `daemon` into a top-level module | A,C | A-opus-1, A-fable-1, C-opus-7 (B-fable-11 notes it) | agreed | S | low | Manifest schema changes stop being daemon changes; ~60 importers drop `daemon` |
| R-3 | Placeholder native-kind constants into `g-mesh-wire`; conformance becomes graph-free | A,B | A-fable-10, A-opus-8(b), B-opus-1, B-fable-1 | agreed | S | very low | Removes `protocol->graph` and the cycle inside `graph`; one contract for core + SDK |
| R-4 | State-dir layout and project identity into `paths` (`StateDir`) | A,B,C | A-opus-2, A-opus-7, A-fable-2, B-opus-3, B-fable-3, C-opus-8, C-fable-5 | agreed | S (step 1) / M | low-med | Removes `storage->daemon`, `config->storage`; one home for every state file |
| R-5 | Daemon control plane (stop, purge, sweep) out of `cli` | A,C | A-opus-3, A-fable-4, C-opus-1, C-fable-1 | agreed | M | medium | Removes `daemon->cli`, `shim->cli`; `cli` becomes a pure leaf |
| R-6 | `IndexingStatus` out of `daemon` | A,B | A-opus-2, A-fable-3, B-opus-7, B-fable-4 | agreed | S | low | Removes `embedding->daemon`; shrinks `mcp/cli->daemon` |
| R-7 | One bundle script for cargo-built plugins | D | D-opus-2, D-fable-2 | agreed | S | low | Staging/smoke fixes land once |
| R-8 | `languages` catalogue decoupled from daemon manifest and outcome storage | B | B-opus-10, B-fable-11 | agreed | S | low | Catalogue becomes a leaf; needed for R-2 to actually break the cycle |
| R-9 | Embedding settings injected; `embedding` stops reading config/env | B,C | B-opus-7, B-fable-10, C-opus-6, C-fable-6 | agreed | S-M | low-med | Removes the only B->C edge; embedding testable with plain values |
| R-10 | Front-mode switch constants into `protocol` | A,C | A-opus-9, A-fable-10, C-fable-3 (C-opus rejects) | agreed (contested) | S | very low | Removes the only `shim->mcp` edge |
| R-11 | One progress-heartbeat helper in `mcp` | C | C-opus-2 | single ✓ | S | low | Progress protocol changed once instead of three times |
| R-12 | One "node row -> embedded text" mapping | B | B-fable-12 | single ✓ | S | low | Text-form change cannot leave a stale vector by a missed SELECT |
| R-13 | One plugin list for scripts/workflows; `ci.yml` checked against test sections | D | D-opus-5, D-fable-2, D-fable-5 | agreed | S-M | low-med | Adding/dropping a plugin or section is one edit; a missed CI step fails loudly |
| R-14 | SDK uses `wire`'s control-plane types | D | D-fable-1 | single ✓ | S-M | medium | A control-message rename becomes a compile error in the SDK |
| R-15 | Split `watcher`: FS events vs index-apply pipeline (`ingest`) | A | A-opus-4, A-fable-5 | agreed | M | low-med | Removes `watcher->daemon`; watching and diff application change independently |
| R-16 | Break `storage<->graph` and `storage<->embedding` | B | B-opus-4, B-opus-8, B-fable-5, B-fable-6 | agreed (proposals differ) | M-L | medium | Storage stops depending on linking and embedding; MCP stops importing `storage::write` |
| R-17 | `mcp` talks to the daemon through a trait; `mcp` builds `QueryShapes` | A,C | A-opus-5, A-fable-7(a,c), C-opus-4, C-fable-3, C-fable-4 | agreed | M | medium | Breaks `daemon<->mcp`; MCP tests without a real registry |
| R-18 | MCP response byte budget out of `graph::pagination` | B | B-opus-6, B-fable-9 | agreed | S-M | low | Budget tuning becomes MCP-only |
| R-19 | One context struct per MCP tool call (and typed answers) | C | C-opus-3, C-fable-2 | agreed | M | low | New per-call input = one field, not a wrapper per tool |
| R-20 | One table of `G_MESH_*` knobs with shared parsers | A,C | A-opus-10, C-opus-6, C-fable-6 | agreed | S-M | low | One place to add, document and list a knob |
| R-21 | Shared contract fixtures for the Go plugin; decide the id scheme | D | D-opus-6, D-fable-4 | agreed | M | low (id migration: data) | Go drift fails a unit test at the same commit |
| R-22 | Split `storage::schema` into DDL + state families | B | B-opus-5, B-fable-7 | agreed | M | low | Lifecycle-rule changes stop colliding with DDL |
| R-23 | Move SQL out of `mcp`/`cli`/`daemon`/`watcher` into area B | B,C | B-opus-9, B-fable-8, C-opus-5 | agreed | M | low | Schema migrations become local to area B |
| R-24 | One typed `plugin.toml` schema; conformance tests use the shipped manifest | D | D-opus-7, D-fable-3 | agreed | M | medium | A manifest key change is one edit checked on both sides |
| R-25 | `daemon::run` into named startup phases; watch loop to its own file | A | A-opus-11, A-fable-9 | agreed | M | medium | Startup decisions unit-testable; startup and loop edits stop colliding |
| R-26 | Emit layer (id dedup, `Positions`) into the SDK | D | D-opus-4, D-fable-9 | agreed | M | low | Next tree-sitter plugin gets conformant emission for free |
| R-27 | Embedding eval behind a cargo feature | C | C-opus-9, C-fable-7 | agreed | M | low-med | 8.3k eval lines out of product builds and `mcp` API |
| R-28 | `plugins/sdk/src/lsp` as its own crate | D | D-fable-6 | single ✓ | M | medium | Bridge edits stop forcing a full test run |
| R-29 | Split `lsp/bridge.rs` by concern | D | D-opus-10, D-fable-7 | agreed | M | low | Server-quirk changes stop touching lifecycle/budget code |
| R-30 | `daemon::candidates` out of `daemon` | A | A-opus-2 | single ✓ | S | low | Shim stops reaching into `daemon` for detection |
| R-31 | Process role set once in `log`, not typed at 162 call sites | C | C-fable-9 | single ✓ | S-M | low-med | Correct process label for CLI-run walks; log format in one place |
| R-32 | Index-health read model out of `cli::status` | C | C-fable-8 | single ✓ | M | medium | One model of language outcomes for CLI and MCP |
| R-33 | Break up `PluginRegistry`; group the plugin host | A | A-opus-6, A-fable-7, A-fable-8 | agreed | L | med-high | Routing, supervision and reconcile change separately |
| R-34 | Core tests on the fake plugin; per-language tests in plugin crates | D | D-opus-8 | single ✓ | L | medium | A plugin-only change stops selecting every core section |
| R-35 | Narrow `lib.rs` exports behind a `testkit` | C | C-opus-10 (C-fable rejects) | single ✓ (contested) | M | low | Internal moves update one re-export file, not N tests |
| X-1 | Move the Go build out of `core/build.rs` | D | D-opus-3 (D-fable rejects) | single ✗ | S | - | - |
| X-2 | OS sandbox as its own crate | D | D-fable-8 (D-opus rejects) | single ✗ | M-L | - | - |
| X-3 | Data layer imports `QualifiedPath` from `g_mesh_wire` directly | A | A-opus-8(a) (A-fable, B-fable reject) | single ✗ | S | - | - |

## 3. Merged findings (agreed and confirmed)

### R-1. Baseline excluded dirs in `g-mesh-walk`
- **Location:** `core/src/watcher/mod.rs:34`, `plugins/sdk/src/walk.rs:50` (both `[".git", ".claude"]`),
  `core/src/project_walk.rs:31` (imports it from `watcher`), `plugins/go/ignore.go:45-53` (`hardExcludedDirs`).
- **Problem:** one rule, two Rust copies kept equal by a comment, and a third Go copy that has
  already drifted: Go adds only `.git` (verified: `ignore.go:47` `{".git": true}`, no `claude` in
  non-test Go). The constant's placement is the whole `project_walk -> watcher` edge.
- **Change:** `pub const BASELINE_EXCLUDED_DIRS` in `walk/src/lib.rs`; core and SDK import it. D-opus
  additionally has `walk()` prune it itself; A/B reports only move the constant. Pick: move first,
  have `walk()` prune it in the same change (both callers already prepend it, `project_walk.rs:170`,
  `sdk/walk.rs:208`), keep the re-export for `watcher::ignore_layers`. Add `.claude` to Go and pin
  it via R-21's fixtures (or a Go test reading the Rust constant, as `manifest_test.go` reads `plugin.toml`).
- **Order:** independent; do first.

### R-2. Plugin-manifest model out of `daemon`
- **Location:** `core/src/daemon/manifest.rs` (+ `manifest/tests.rs`).
- **Problem:** a pure schema/discovery module (only crate imports: `graph::symbol_links::LinkRules`,
  `protocol::types::CURRENT_PROTOCOL_VERSION`) that every reader reaches through `daemon`; it is the
  whole `languages -> daemon` edge and the largest share of `mcp/cli -> daemon`. Opus counts 60
  importing files, Fable 61 (same g-mesh call; trivial difference).
- **Change:** both propose a top-level `plugin_manifest` module with a one-release `pub use` in
  `daemon`. Agreed as written.
- **Order:** with R-8 (see the note there: moving it alone leaves a longer cycle). Precedes R-17,
  R-24, R-33.

### R-3. Native kinds into `g-mesh-wire`
- **Location:** `core/src/graph/{symbol_links.rs:305,311, imports.rs:97,116, containers.rs:197}`;
  consumers `protocol/conformance.rs:7-9`, `storage/language_swap.rs:57-58`; copies
  `plugins/sdk/src/graph.rs:238-241`, `plugins/go/extract.go:107-117`.
- **Problem:** a plugin-emitted vocabulary defined beside linking algorithms. It is the whole
  `protocol -> graph` edge, and (B-fable, verified) a cycle inside `graph`: `graph/queries.rs` imports
  the constants from all three pass modules while `containers.rs:192` and `symbol_links.rs:297`
  import `queries`.
- **Change:** a `native_kind` module (or `PlaceholderKind` enum with `as_str`) in `g-mesh-wire`,
  including `REEXPORT_ALL_NAME`. A-opus-8 offered moving `conformance.rs` into `cli/plugin_check`
  instead; pick the wire move (3 of 4 proposals; it fixes storage and the SDK too). `conformance`
  then imports nothing from `graph` and can stay in `protocol`.
- **Order:** independent.

### R-4. State-dir layout and identity into `paths`
- **Location:** `daemon/identity.rs` (`project_hash`, root/device records), `storage/connection.rs`
  (`projects_root`, `project_dir`, `ensure_project_dir`; `:7` imports `daemon::identity`),
  `daemon/mod.rs:51-306, 897-1036` (~25 path/pid/lock helpers), `config/mod.rs:54,251`,
  stragglers (`shim.rs` log file, `registry.rs` pid/marker names, `cli/clean.rs` sweep stamp).
- **Problem:** "where a project's state lives and what is in it" is split over four modules; this is
  the whole `storage -> daemon` edge and the reason `config` imports `storage`.
- **Change:** proposals differ only in the home (`paths`: B-opus-3, B-fable-3, C-opus-8, C-fable-5,
  A-opus-2; new `state_dir.rs`: A-fable-2; `daemon/state_dir.rs`: A-opus-7; `project_state`:
  C-opus-1). Pick `paths` (with a `paths::state_dir` submodule): it is already a fan-out-0 leaf
  documented as "where g-mesh keeps what it owns". Step 1 (S): `identity` + the three directory
  functions + a pinned-hash test (a changed hash orphans every index). Step 2 (M): the `StateDir`
  value type with file methods and the probes. Keep **both** lock polarities and name them at the
  call site (A-opus-7's `OnUnreadable::Held|Free`); they differ on purpose (verified:
  `daemon/mod.rs:980` "not held ... leaving processes alone", `cli/clean.rs:513` "counts as held").
  Re-export old paths for a release; `storage::connection::project_dir` has 243 references, mostly tests.
- **Order:** step 1 before R-5 and R-6.

### R-5. Control plane out of `cli`
- **Location:** `cli/stop.rs` (`stop`, `terminate_daemon`, `tidy_state_files`, `TERMINATION_TIMEOUT`),
  `cli/clean.rs:257-624` (`RootState`, `Purge`, `LockWait`, `purge_orphaned_state_dir`,
  `sweep_orphaned*`); callers `daemon/lifecycle.rs:1015-1022`, `shim.rs:15,152,378,458`.
- **Problem:** the daemon and the shim call up into CLI command files for unattended deletion and
  termination; this is all of `daemon -> cli` and `shim -> cli`. All four reports cite the same
  `find_callers` answers.
- **Change:** homes differ (top-level `control/`: A-opus-3; `gc::orphans` + `daemon::control`:
  A-fable-4, C-fable-1; `project_state/`: C-opus-1). Pick one top-level `control` module with
  `stop.rs`, `purge.rs`, `sweep.rs`. Reason: `gc` documents itself as never deleting
  (`gc/mod.rs:5-7` "Nothing here ever deletes anything") and `gc::warning` as informational only, so
  moving deletion there inverts its contract; putting `stop` in `daemon` keeps the shim tied to
  `daemon`. A-opus-3 also moves the shim's bootstrap/evict functions into `control`; treat that as an
  optional second step. Move code without editing logic; `clean.rs`'s tests split along the same line.
  Update the stale `gc` docs in the same change.
- **Order:** after R-4 step 1.

### R-6. `IndexingStatus` out of `daemon`
- **Location:** `daemon/indexing_status.rs`; `embedding/backfill.rs:102`.
- **Problem:** shared progress state (read by `mcp`, `cli`, written by `embedding`) living in `daemon`.
- **Change:** A reports move the module to top level; B reports keep it and give `backfill::run` a
  two-method `EmbedProgress` trait. Pick the move (removes `embedding -> daemon` **and** ~20 `mcp/cli`
  references); the trait is optional on top. Correction: A-opus-2 says the module imports only std,
  serde and `Notify`; it also calls `super::write_state_file_atomic` (`indexing_status.rs:354,492`),
  as A-fable-3 says, so it follows R-4.
- **Order:** after R-4 step 1.

### R-7. One cargo-plugin bundle script
- **Location:** `scripts/bundle-{plugin,rust-plugin,python-plugin}.sh`, `build-targets.sh:205-211`.
- **Problem:** verified: comment-stripped diff of the Python and Rust scripts is 9 hunks, every one a
  language-name substitution (both reviews say ~10).
- **Change:** `scripts/bundle-cargo-plugin.sh <language>`; keep the Go script; D-fable also moves the
  shared tail (manifest rewrite, smoke) into `scripts/lib/`. Agreed; take both. Update
  `release.yml`'s path filter and `core/tests/scripts/release_packaging_scripts.rs`.
- **Order:** before R-13.

### R-8. `languages` catalogue decoupled
- **Location:** `core/src/languages.rs:42` (`use crate::daemon::manifest::{extension_of, under_excluded_dir, DiscoveredPlugins}`),
  `LanguageOutcome` (`languages.rs:78`, used by `storage/schema.rs:6`).
- **Problem:** the "without a plugin" catalogue depends on daemon discovery and also holds a stored
  outcome type.
- **Change:** both: move the two path helpers down (B-opus: `languages` or `project_walk`; B-fable:
  `project_walk`, which owns `under_any` - pick `project_walk`) and take a small trait instead of
  `&DiscoveredPlugins`. B-opus additionally moves `LanguageOutcome` + error encoding to storage. Keep
  that part: after R-2, the edges `languages -> plugin_manifest -> graph (LinkRules) -> storage ->
  languages (LanguageOutcome)` would still form a cycle (inferred from the edge lists, not
  recomputed); moving `LanguageOutcome` (or `LinkRules`, C-opus-7's alternative) closes it.
- **Order:** with R-2.

### R-9. Embedding settings injected
- **Location:** `embedding/pipeline.rs:115-135` (`CacheSettings::from_global_config`),
  `embedding/rerank.rs:318-326` (`RerankSettings::from_environment`), `embedding/model.rs:127`.
- **Problem:** the data layer reads the global config file and env at first use; the only B->C edge.
- **Change:** agreed shape: `EmbeddingSettings { model, cache, rerank }` built by the composition
  root and passed to `EmbeddingPipeline::load` / the reranker. Keep env-over-file precedence. B-opus
  notes the lazy read means a daemon that never embeds never reads config; moving it changes only
  when a broken file is logged.
- **Order:** independent; part of the config work with R-20.

### R-10. Front-mode constants into `protocol`
- **Location:** `shim/router.rs:129`, `mcp/front.rs` (`SELECT_PROJECT`, `PROJECT_ARG`, `SWITCH_PROJECT_META`).
- **Problem:** verified that `router.rs:129` is the only `crate::mcp` reference in `shim`. C-opus calls
  the move cosmetic (two files); see contradiction K-1.
- **Change:** `protocol/front.rs`, imported by both sides. **Order:** independent; low benefit, very low cost.

### R-11. Heartbeat helper (single ✓, C-opus-2)
- **Verified:** three copies of `interval_at` / `MissedTickBehavior::Delay` / `notify_progress` at
  `mcp/mod.rs:316/320/336`, `411/415/431`, `528/532/559`.
- **Change:** `mcp/progress.rs::with_heartbeat(ctx, started, task, message)`; keep the `biased`
  select order and the rising `progress` invariant. **Order:** independent.

### R-12. One embedded-text row mapping (single ✓, B-fable-12)
- **Verified:** `EmbedInput { ... }` is built in non-test code at `embedding/pipeline.rs:862,890`,
  `embedding/backfill.rs:333,386`, `storage/language_swap.rs:418`, plus `cli/embed_eval.rs:296`,
  `cli/embed_eval/schedule.rs:292` (and `mcp/search_code.rs:1062,1078`, not checked whether test code).
- **Change:** `embedding::text::{EMBED_COLUMNS, EmbedInput::from_row}`. The stale-vector consequence
  is inferred, not observed. **Order:** before R-16's vector seam (which then removes the swap's copy).

### R-13. One plugin list; CI section guard
- **Location:** `scripts/build-targets.sh:130` (`BUNDLED_PLUGINS`), `cut-release.sh:298`
  (`CRATE_BACKED_PLUGINS`), `release.yml:585,791,822` (`@('typescript','go','rust','python')`,
  verified), `test-sections.sh:39-51` (12 sections) vs 12 `tests:` steps in `ci.yml`.
- **Problem:** the language set is restated in about 8 files; no script checks `ci.yml` against the
  section list (verified: neither `test-sections.sh` nor `test-sections-check.py` mentions `ci.yml`).
- **Change:** D-opus: derive the list from `plugins/*/plugin.toml` via `scripts/lib/plugins.sh`;
  D-fable: reuse `build-targets.sh --plugins`. Pick D-fable's (the helper already exists and three
  scripts use it), extending it with a cargo/go split. For CI, keep per-section steps (UI) and add
  the `ci.yml` check to `test-sections.sh check` (both offer it). **Order:** after R-7.

### R-14. SDK uses `wire`'s control types (single ✓, D-fable-1)
- **Verified:** no `ControlMessage`/`FileChangeResponse`/`ResolutionChangedResponse` use under
  `plugins/sdk/src` except comments (`semantic.rs:155`, `run.rs:891`); `run.rs:433` matches
  `"fileChanged"` by string and responses are `serde_json::json!` literals (`run.rs:546,885,896,915`).
- **Change:** tolerant `ControlRequest::parse` in `wire` (loose `id`/`method`, typed `params`),
  responses from the wire types. Define and test the fallback when typed decode fails (today a bad
  `filePaths` entry is skipped, not fatal). **Order:** independent; pairs with R-21.

### R-15. Split `watcher` into events and `ingest`
- **Location:** `watcher/apply.rs` (1419 lines + 3058-line tests), `watcher/staleness.rs`,
  `apply.rs:422` (`use crate::daemon::config_reindex::{select_with, Selection}`).
- **Problem:** the FS event source and the plugin round-trip/diff application share a module; the
  latter is the `watcher -> daemon` edge. Both reviews cite the same g-mesh importer list.
- **Change:** agreed: `ingest/` (Opus name) or `index_apply/` (Fable) with `apply`, `staleness`,
  a pure `convert` module for wire->storage mapping. For the upward call, A-fable left "callback vs
  move `select_with`" open; pick the move: `select_with` is pure SQL over `&Connection` plus wire
  selectors (g-mesh `find_definition daemon::config_reindex::select_with`). Move
  `staleness::mtime_millis` (used by `daemon/build_stamp.rs:89`, `cli/status.rs:59`) to `paths` or a
  small `fsutil`. Keep `Unit::WatcherApply` and log strings.
- **Order:** independent; before R-33.

### R-16. Break `storage<->graph` and `storage<->embedding`
- **Location:** `storage/index_store.rs:39-42` (embedding, graph linking), `storage/write.rs:6`
  (`graph::containers`), `storage/language_swap.rs:54-58`, `storage/write.rs:NodeRecord` (167 refs in 34 files).
- **Problem:** agreed: storage runs the indexing unit of work (write, link, embed) and owns the row
  types every reader uses.
- **Change:** the proposals differ. B-opus-4: a new `index` layer above storage that takes all linked
  operations (`index_store` + `language_swap` move, L). B-fable-5/6: (a) record types to a leaf
  `model`, (b) split `graph` into `link` (write passes) and `read`, (c) a `VectorWrite` callback and a
  text callback so storage never names `EmbeddingPipeline`. Both agree on the record-type move
  (B-opus-8 = B-fable-5a). Pick B-fable's path as the default: it breaks both cycles with M effort
  and keeps the 1.6k + 1.5k-line files in place; the vector callback keeps "vectors written inside
  the swap's single transaction". For `containers::detach/attach` (only caller: `storage::write`,
  verified with g-mesh `find_callers graph::containers::detach`, 1 file), take B-opus's move into
  storage row maintenance: it removes `write.rs -> graph` without a callback. Keep B-opus's `index`
  layer as a later option if the owner wants `storage` to be a pure leaf.
- **Order:** after R-3 (constants) and R-12; R-23 benefits from the `graph::read` module.

### R-17. `mcp` behind a host trait
- **Location:** `mcp/mod.rs:serve_connection`, `GMeshMcpServer` fields, `daemon/registry.rs:960-961`.
- **Problem:** agreed: `mcp` uses ten registry methods (both models counted the same list) plus
  `CoreActivity` and `IndexingStatus`, and the registry builds `mcp::query_shapes::QueryShapes`.
- **Change:** one trait owned by `mcp` (C-fable-4 lists the indexing and activity methods too; take
  that complete list). `GMeshMcpServer::new` builds `QueryShapes::from_manifests` itself (K-2). The
  `wait_for` part is async; C-fable did not read it, so the trait's async shape is still open. Restate
  the `mark_used`/plugin-lock order in the trait docs.
- **Order:** after R-2 and R-6.

### R-18. MCP byte budget out of `graph::pagination`
- **Change:** agreed: budget items (`MAX_RESPONSE_BYTES`, `bound_page*`, `fit_budget*`, tallies) to
  `mcp`; keyset SQL stays. `cli/plugin_check/expectations.rs:630` imports `pagination::{self,
  Direction}` and uses `MAX_PAGE_SIZE` (keyset side), so it stays on `graph`. B-fable also moves
  `Direction` to `graph/mod.rs`. **Order:** independent.

### R-19. One tool-call context
- **Location:** `mcp/find_references.rs`, `find_callers_callees.rs`, `find_definition.rs`,
  `get_dependencies.rs`, `get_file_outline.rs` handler ladders (17 `handle*` entry points by
  module-level grep; C-opus counts 18).
- **Change:** both propose a context struct (`ToolContext` / `QueryCtx`). C-fable goes further:
  typed `Answer<Page>` results and a separate `query` module so `plugin_check` stops re-parsing
  JSON (verified: `expectations.rs:1827-1838` parses `content.first()` text with `serde_json`). Take
  the context struct first (M), typed answers second. Fix `handle_callees`' `SessionHints::default()`
  (`find_callers_callees.rs:686`). **Order:** independent; eases R-27.

### R-20. One knob table
- **Problem:** many `G_MESH_*` variables, each parsed at its site (Opus: 44 constants in 20 files;
  Fable: 48 distinct names in 22 files; different counting rules, not a contradiction).
- **Change:** agreed S step: one table (name, default, kind user/tuning/test-hook, parse rule) and
  shared parsers. The two zero rules are deliberate (verified: `lifecycle.rs:128-133` zero = off,
  `plugin.rs:250-262` zero = default, documented) and must be two named rules. Injection of a
  resolved `Settings` is a later M step; keep per-call re-reads that tests flip on a live daemon
  (not checked which). Fix the stale `config/mod.rs:39` "Not wired to runtime behavior yet".
- **Order:** with R-9.

### R-21. Go contract fixtures; id scheme decision
- **Location:** `plugins/go/{ids,wire,walk,ignore}.go` vs `plugins/sdk/src/ids.rs`, `wire/src/lib.rs`, `walk/src/lib.rs`.
- **Problem:** only `generated_headers.json` is shared. Verified drift: Go ids are space-joined
  (`ids.go:31`), SDK ids NUL-joined (`ids.rs:90-106`); see side discovery 2.
- **Change:** agreed: Rust-generated golden corpora (`ids.json`, walk/gitignore cases, wire lines,
  control frames) read by Go tests. Decide the id scheme explicitly: migrate Go to NUL (reindex of Go
  projects) or document the difference and correct three comments. **Order:** after R-1.

### R-22. Split `storage::schema`
- **Change:** agreed: keep DDL + generation in `schema`; move the ~60 state accessors into
  `storage::state::{languages|bulk, retry, reextracts, semantic, seed, embedding_meta}` behind
  re-exports. B-opus's typed `SemanticState` API is a follow-up. **Order:** before R-23 and R-32.

### R-23. SQL out of surfaces and runtime
- **Problem:** agreed; counts differ by scope (B-opus 23 statements in mcp/cli/watcher/daemon;
  B-fable ~38 incl. `config_reindex`, `gc`, `registry`, `embedding`, `embed_eval`; C-opus 9 surface files).
- **Change:** named functions in `graph::read`/`graph::queries` and `storage::state`; one shared
  read-only open (C-opus); a grep test with an allow-list so the count only goes down (B-fable).
  Move queries verbatim. **Order:** after R-22 (and R-16(b) if taken).

### R-24. One `plugin.toml` schema
- **Location:** three `struct RawManifest` (`core/src/daemon/manifest.rs:856`,
  `plugins/sdk/src/manifest.rs:182`, `plugins/sdk/src/lsp/config.rs:461`).
- **Change:** agreed: serde-only schema in `wire` or a small `g-mesh-manifest` crate; lenient parse
  (all-optional, unknown keys tolerated), strictness left to core. Add D-fable's
  `PluginCheck::from_manifest` and `PluginSpec::from_manifest_str` so code and tests stop restating
  extensions/excludes. **Order:** after R-2.

### R-25. `daemon::run` phases
- **Change:** agreed: extract the pure startup decision (`StartupPlan`, A-fable) and named phases
  (A-opus); move `spawn_watch_consumer`/`watch_and_route_once`/`Rescans` to `daemon/watch_loop.rs`
  (A-fable-9b, an S part). Keep side-effect order (bind before pid file, `record_serving_owner`
  after the phase file). **Order:** after R-4.

### R-26. Emit layer into the SDK
- **Change:** agreed: dedup-by-id builder and `Positions` behind an SDK `tree-sitter` feature.
  Verified D-opus detail: Python's `render_target` copy (`python/src/extractor/emit.rs:431-437`)
  cites a private SDK renderer, but `plugins/sdk/src/lib.rs:103` exports it; delete the copy.
  Pin one `tree-sitter` version. **Order:** independent.

### R-27. Embedding eval out of the product
- **Change:** C-opus: cargo feature (or separate binary); C-fable: separate workspace member. Pick
  the feature first (K-11). **Order:** after R-19 (typed query API).

### R-28. `sdk/lsp` as its own crate (single ✓, D-fable-6)
- **Verified:** the only reference to `lsp` from SDK files outside `lsp/` is `run.rs:370`
  `crate::lsp::kill_live_servers()`; `test-select.sh:160` maps `plugins/sdk/*` to `full=1`.
- **Change:** `plugins/sdk-lsp` crate; replace the back-edge with an exit-hook registry; map its
  path in `test-select.sh` to its section + plugin sections. **Order:** after R-29 (split first, then move).

### R-29. Split `lsp/bridge.rs`
- **Change:** agreed: `questions`, `answers` (+ hover, landing), `pass`, leaving lifecycle/budgets.
  D-fable's `ServerProfile` for per-server knobs is optional. **Order:** before R-28.

### R-30. `daemon::candidates` out of `daemon` (single ✓, A-opus-2)
- **Verified:** `daemon/candidates.rs` depends on `storage::connection::project_dir` and
  `storage::schema` only (`:26-27`); used by the shim. Move to a top-level `project_detect`.
  **Order:** after R-4.

### R-31. Log role (single ✓, C-fable-9)
- **Verified:** 162 `"g-mesh daemon: "` literals; `log_line!` adds no prefix (`log.rs:19-36`);
  `daemon/bulk_index.rs:512` logs "g-mesh daemon: skipping malformed bulk-index line" and
  `bulk_index::run` is called in-process by `cli/init.rs:254` and `cli/reindex.rs:120`, so a CLI-run
  walk labels lines as the daemon's.
- **Change:** `log::set_role` once per process; byte-identical output for the daemon role
  (`core/tests/it/daemon_log_lines.rs`). **Order:** independent.

### R-32. Index-health read model (single ✓, C-fable-8)
- **Verified:** daemon and storage tests reach up into `cli::status` (`daemon/semantic.rs:786,793`,
  `storage/index_store.rs:835`, both in test modules); `mcp/mod.rs:677-679` derives coverage from
  the same `schema` accessors. Overlap of the rules themselves is inferred.
- **Change:** extract the data half of `status.rs` into a non-surface module; keep a no-I/O path for
  MCP instructions. **Order:** after R-22.

### R-33. `PluginRegistry` and the plugin host
- **Problem:** agreed: a ~1380-line impl with ~45-48 methods and several roles; plugin lifecycle
  spread over `plugin.rs`, `lifecycle.rs` (`PluginSupervisor`), `registry.rs`, `memory.rs`.
- **Change:** A-fable-8 first (S-M, file moves: `daemon/plugin_host/{process, supervisor, registry,
  memory}`), then the immutable `LanguageMap`/`LanguageRouter` extraction (both), then the reconcile
  methods to their own file (A-fable) or into `ingest` (A-opus). Pick A-fable's file split for
  reconcile first (no API change). **Order:** after R-15 and R-17; pure moves before logic.

### R-34. Core tests on the fake plugin (single ✓, D-opus-8)
- **Verified:** `scripts/test-plugin-spawns.txt` 103 rows: none 16, typescript 66, rust 62,
  python 50, go 47; `test-select.sh:160` `wire/* | plugins/sdk/*) full=1`.
- **Change:** move core-behaviour tests to `g-mesh-fake-plugin` with scripted output, language-output
  tests into plugin crates, keep `core-spawns-*` as the e2e layer. Incremental. **Order:** last; L.

### R-35. Narrow `lib.rs` (single ✓, contested; C-opus-10)
- **Verified:** 17 `pub mod` in `lib.rs`; `core/tests` names `g_mesh::daemon` 113 times,
  `storage` 110, `shim` 78, `embedding` 56.
- **Change:** `pub(crate)` modules plus a `#[doc(hidden)] pub mod testkit`. See K-7: worth it only if
  several of R-2..R-6 land in one release; the per-finding `pub use` re-exports already absorb
  most test breakage. **Order:** optional, before the big moves if taken.

## 4. Contradictions

| # | Topic | Positions | Verdict and evidence |
|---|---|---|---|
| K-1 | Front-mode constants | A-opus-9, A-fable-10, C-fable-3: move to `protocol`. C-opus: cosmetic (2 files). | Move (R-10). Both facts true; the move is S and `shim/router.rs:129` is the only `crate::mcp` use in `shim` (grep), so it removes a whole module edge. |
| K-2 | Home of `QueryShapes` | C-fable-3: into `daemon::manifest`. A-opus-5, A-fable-7, C-opus-4: `mcp` builds it from manifests. | `mcp` builds it. `mcp/query_shapes.rs:12` imports only `NonSymbolShapes, PluginManifest`; its one caller is `mcp/mod.rs:230`. After R-2 that is a downward edge, and query shaping stays an MCP concept. |
| K-3 | `IndexingStatus` | A: move module. B: `EmbedProgress` trait. A-opus-2: module needs only std/serde/Notify. | Move (R-6), trait optional. A-opus-2's dependency claim is wrong: `indexing_status.rs:354,492` call `super::write_state_file_atomic` (A-fable-3 is right), so R-6 depends on R-4. |
| K-4 | Home of the state-dir layout | `paths` (5 reports) vs `state_dir.rs` vs `daemon/state_dir.rs` vs `project_state` | `paths` (R-4): existing leaf with fan-out 0. |
| K-5 | Home of stop/purge/sweep | `control/` (A-opus) vs `gc::orphans` + `daemon::control` (A-fable, C-fable) vs `project_state/` (C-opus) | Top-level `control` (R-5). `gc/mod.rs:5-7` and `gc::warning`'s doc define `gc` as never deleting. |
| K-6 | Breaking storage cycles | B-opus-4: new `index` layer (L). B-fable-5/6: `model` + `graph::{link,read}` + callbacks (M). | B-fable's path, plus B-opus's containers-rows move (R-16). |
| K-7 | `lib.rs` narrowing | C-opus-10: `pub(crate)` + testkit. C-fable: no independence gained inside one crate. | Facts of both hold. Kept as optional R-35: it lowers the cost of moves, not coupling; C-fable is right that it adds no independence by itself. |
| K-8 | Go build in `core/build.rs` | D-opus-3: move to its own build crate. D-fable: best-effort, flags shared; fine. | Rejected (X-1). `core/build.rs:1-5` confirms the coupling but it only warns when Go is absent; the move would stop `cargo build -p g-mesh` from producing the Go binary that 47 core test modules spawn. |
| K-9 | OS sandbox crate | D-fable-8: extract. D-opus: correctly Rust-owned. | Rejected (X-2). Cycle verified (`sandbox.rs:29` / `semantic.rs:83`) but it is inside one crate; no second consumer (no `sandbox` in python/typescript `semantic.rs`). Revisit if another plugin needs confinement. |
| K-10 | `QualifiedPath` via `protocol::types` | A-opus-8(a): import from `g_mesh_wire`. A-fable, B-fable: harmless re-export. | Rejected (X-3). After R-3, `protocol` imports nothing from `graph`, so `graph/storage -> protocol` is one-way to a re-export (`storage/write.rs:7`, `qualified_path.rs:11`, `symbol_links.rs:298`). |
| K-11 | Embedding eval | C-opus-9: cargo feature or binary. C-fable-7: separate workspace member. | Feature first. No script or workflow calls `debug-embed-eval` (grep of `scripts/`, `.github/`: none), and a feature keeps `top_k_for_eval` etc. `pub(crate)` instead of widening the lib API. |
| K-12 | Watcher upward call | A-opus-4: move `select_with` down. A-fable-5: callback or move (undecided). | Move (R-15): `select_with` is pure SQL over `&Connection` (g-mesh `find_definition`). |
| K-13 | `containers::detach/attach` | B-opus-4: into storage row maintenance. B-fable-5: into `graph::link`. | Storage (R-16): single caller `storage::write` (g-mesh `find_callers`, 1 file), inside `write_diff`'s transaction. |

Count differences that are not contradictions: manifest importers 60 vs 61; SQL sites 23 / ~38 /
9 files; env names 44 consts vs 48 names; handler entry points 18 vs 17 (module-level grep). Each
pair used a different scope or pattern.

## 5. Rejected single-model findings

- **X-1 (D-opus-3) Go build out of `core/build.rs`.** Coupling is real but best-effort (warns when Go
  is absent) and the flags are already shared via `core/go_plugin_build_flags.rs`; moving it changes
  what `cargo build -p g-mesh` produces for 47 Go-spawning test modules. Low benefit for a workflow change.
- **X-2 (D-fable-8) Sandbox as its own crate.** Only rust-analyzer runs project code; no second
  consumer exists; the `sandbox <-> semantic` cycle is intra-crate and does not block independent
  updates. A product decision (confining pyright/vtsls) would revive it.
- **X-3 (A-opus-8a) Import `QualifiedPath` from `g_mesh_wire` in the data layer.** Cosmetic once R-3
  lands: the remaining edges point one way at a documented re-export.

Also not a finding: **D-opus-9 / D-fable-10 plugin-version mismatch**. Resolved; see section 8.

## 6. Suggested backlog grouping (proposal; the owner decides)

1. **Leaf moves that break the SCC** (S each, mechanical, one release): R-1, R-2 + R-8, R-3, R-4
   step 1, R-6, R-10, R-30. Expected effect: removes `project_walk<->watcher`, `languages<->daemon`,
   `graph<->protocol`, `storage->daemon`, `embedding->daemon`, `shim->mcp` (inferred; Tarjan not re-run).
2. **Control plane** (M): R-5, then R-4 step 2 (`StateDir`). Removes `cli` from the SCC.
3. **Watcher and daemon startup** (M): R-15, R-25.
4. **Data layer** (M-L): R-12, R-16, R-22, then R-23, R-18, R-32.
5. **MCP seam** (M): R-17, R-19, R-11, then R-27.
6. **Configuration** (S-M): R-9, R-20, R-31.
7. **Plugin contracts** (M): R-21 (with the id decision), R-14, R-24, R-26.
8. **Scripts and CI** (S-M): R-7, R-13.
9. **SDK structure** (M): R-29, then R-28.
10. **Large and optional** (L): R-33, R-34, R-35.

Rough order: 1 -> 2 -> (3, 4, 5 in parallel) -> 10; groups 6-9 are independent of 1-5 and can
interleave.

## 7. Opus 5.5 vs Fable 5.2

Overlap per area (a finding counts as matched when the other model's report in the same area
covers the same change; cross-area matches noted):

| Area | Opus findings | Fable findings | Opus matched in area | Fable matched in area | Opus unique (valid) | Fable unique (valid) |
|---|---|---|---|---|---|---|
| A | 11 | 10 | 10 | 10 | A-opus-10 (matched by C-fable-6 cross-area); candidates move (R-30) | none (A-fable-8 is a sub-change of A-opus-6) |
| B | 10 | 12 | 10 | 11 | none | B-fable-12 (R-12) |
| C | 10 | 9 | 6 (+2 cross-area: C-opus-5 ~ B-fable-8, C-opus-7 ~ A-fable-1) | 7 | C-opus-2 (R-11), C-opus-10 (R-35, contested) | C-fable-8 (R-32), C-fable-9 (R-31) |
| D | 10 | 10 | 7 (+1 cross-area: D-opus-1 ~ A-fable-6/B-fable-2) | 7 | D-opus-8 (R-34) | D-fable-1 (R-14), D-fable-6 (R-28) |
| Total | 41 | 41 | | | 4 valid unique findings | 5 valid unique findings |

Rejected or wrong:
- **Opus:** X-1 (D-opus-3) and X-3 (A-opus-8a) rejected as low value; one factual error
  (A-opus-2: `indexing_status` "imports only std, serde, Notify", contradicted by
  `indexing_status.rs:354,492`). C-opus's rejection of the front constants (K-1) was overruled.
- **Fable:** X-2 (D-fable-8) rejected; C-fable-3's `QueryShapes` home overruled (K-2); C-fable's
  rejection of `lib.rs` narrowing is partly upheld (K-7). No factual error found in the spot checks.
- **Both:** flagged the plugin-version mismatch as a structural defect; it was a bump omission (section 8).

Unique facts that only one model surfaced: Opus - Go walk lacks `.claude` (D-opus-1), stale Python
`render_target` copy (D-opus-4), stale Go `go.mod` comment (D-opus-6), intentional lock polarities
(A-opus-7), the two zero rules (A-opus-10). Fable - Go/SDK id-scheme divergence with computed hashes
(D-fable-4), the g-mesh `get_dependencies` miss (C-fable), the intra-`graph` cycle (B-fable-1), stale
`config` doc (C-fable-6), the log-label consequence (C-fable-9).

Depth (counts of the words in each report; a rough proxy):

| Report | "measured" | "inferred" | "not verified/checked/read" admissions |
|---|---|---|---|
| A-opus / A-fable | 61 / 37 | 6 / 7 | 0 / 9 |
| B-opus / B-fable | 27 / 11 | 0 / 6 | 0 / 5 |
| C-opus / C-fable | 29 / 40 | 2 / 8 | 0 / 8 |
| D-opus / D-fable | 12 / 33 | 6 / 9 | 0 / 10 |

Opus tags more claims as measured in A and B and gives no explicit "not verified" sections (two
soft hedges: C-opus-7 "did not re-run Tarjan", B-opus "Not decided"); its one factual error was in
an untagged dependency claim. Fable writes a "Not verified"/"Not examined" section in every report
and separates measured from inferred per bullet, and computed values (hashes) to prove a claim.
Both models used the same g-mesh answers where they overlapped (identical caller lists, counts).

Cost (from `slice_metrics`):

| slice | area | model | reread_tokens | max_context | api_turns | tool_uses | output_tokens | gmesh_calls | recursive_greps | wall_s |
|---|---|---|---|---|---|---|---|---|---|---|
| S2 | A | opus-5-5 | 4016212 | 152509 | 40 | 55 | 31197 | 22 | 9 | 329 |
| S3 | A | fable-5-2 | 2250080 | 136903 | 24 | 35 | 28149 | 11 | 20 | 333 |
| S4 | B | opus-5-5 | 3068190 | 128483 | 34 | 39 | 23700 | 6 | 24 | 265 |
| S5 | B | fable-5-2 | 2237863 | 138815 | 22 | 33 | 24364 | 10 | 13 | 305 |
| S6 | C | opus-5-5 | 4434018 | 149878 | 43 | 51 | 29455 | 10 | 37 | 332 |
| S7 | C | fable-5-2 | 1564490 | 127401 | 18 | 32 | 27769 | 12 | 26 | 309 |
| S8 | D | opus-5-5 | 5217106 | 154069 | 50 | 54 | 30365 | 4 | 5 | 347 |
| S9 | D | fable-5-2 | 5105956 | 166472 | 44 | 51 | 34817 | 6 | 13 | 3135 (cause unknown, not polling) |
| **sum** | | opus-5-5 | 16735526 | - | 167 | 199 | 114717 | 42 | 75 | 1273 |
| **sum** | | fable-5-2 | 11158389 | - | 108 | 151 | 115099 | 39 | 72 | 4082 (947 without S9) |

Opus USD (API-equivalent): S2 2.16, S4 1.70, S6 2.10, S8 2.30; total 8.26. Fable has no USD
price in the tool; its per-token price is higher than Opus's, so its 33% lower re-read volume does
not by itself mean a lower cost. Fable used 35% fewer turns for about the same output tokens; in
area D both models cost about the same in tokens.

## 8. Side discoveries (not refactorings)

1. **g-mesh defect (confirmed): `get_dependencies` misses importers of `core/src/cli/clean.rs`.**
   Reproduced: `get_dependencies(Incoming, core/src/cli/clean.rs, depth 1)` and the same anchored on
   `g_mesh::cli::clean` both return only the module itself with `truncated: false` and the hint
   "don't re-derive it with grep". Real users: `shim.rs:152` (a `use` inside a closure),
   `daemon/lifecycle.rs:1015-1022` (inline `crate::cli::clean::` paths), `cli/mod.rs:379`
   (`clean::run` via the parent module). Control: `cli/stop.rs`, imported by module-level `use` in
   `shim.rs:15` and `cli/reindex.rs:51`, returns both importers. So function-scoped `use` (and inline
   paths) produce no import edge, while the answer claims completeness. Worth its own GM task:
   either index those edges or drop the completeness claim for Rust.
2. **Go and SDK id schemes differ (confirmed).** `plugins/go/ids.go:31` hashes
   `"node %s %s %s %s"` (space-joined); `plugins/sdk/src/ids.rs:90-106` joins with NUL. For
   `(main.go, File, main.go, "")`: space form `1453099e1c11aef90ca118eb6d140fe1` (the value pinned in
   `ids_test.go`), NUL form `1c06532b9ba18d30da653a043b3ed0e1` (computed with `shasum -a 256` here).
   The comments claiming parity are false: `ids.rs:19` "plugins/go must match this", `ids_test.go:30-31`,
   `ids.go:9` "mirrors node_id/edge_id in plugins/sdk/src/ids.rs". Core computes its own container
   ids (`graph/containers.rs:237-238`) and was not seen recomputing plugin ids, so no wrong answer is
   shown; impact not traced further. Decision needed (R-21).
3. **Go plugin's excluded dirs lack `.claude` (confirmed).** `plugins/go/ignore.go:45-53`
   `hardExcludedDirs = {".git"} + manifestExcludeDirs (vendor, testdata)`; core and SDK exclude
   `.claude`. Inferred effect: the Go bulk index includes `.go` files under `.claude/worktrees/`
   that core's watcher ignores. Fix with R-1.
4. **Plugin version mismatch: resolved.** D-opus-9 / D-fable-10 saw plugin crates at 4.30.0 and
   `plugin.toml` at 4.28.0 in this worktree. It was a bump omission, fixed on `release-4.30.0` by
   `ce15c611` ("chore: bump plugin manifests to 4.30.0"; `plugins/rust/plugin.toml` there has
   `plugin_version = "4.30.0"`). The two-rule design is documented in
   `docs/architecture/plugin-modularity.md` (cited by that manifest); not a finding.
5. **Stale documentation (confirmed), cheap to fix with the related finding:**
   `gc/mod.rs:5-7` "Nothing here ever deletes anything" (the daemon and shim now purge; R-5);
   `config/mod.rs:39` "Not wired to runtime behavior yet" (`daemon/mod.rs:467` reads it; R-20);
   `plugins/python/src/extractor/emit.rs:431` "the SDK keeps its renderer private" (exported at
   `plugins/sdk/src/lib.rs:103`; R-26); `plugins/go/ignore.go:8-10` "`go.mod` ... has zero
   requirements" (`go.mod` requires `golang.org/x/tools v0.50.0`; R-21).
6. **Module-map undercount:** `log` fan-in is listed as 1 because the regex misses `log_line!`
   (C-opus and C-fable both noted it; 30 files use it per C-fable). Fix in the map's method if it is reused.
7. **CLI-run walks log as the daemon** (confirmed, R-31): `daemon/bulk_index.rs:512` reached from
   `cli/init.rs:254`, `cli/reindex.rs:120`.

## 9. Backlog tasks

Owner decisions (2026-11-14): findings "Все 35, по 10 группам (Recommended)"; side discoveries
"Отдельные fix-задачи (Recommended)". All tasks are in the backlog (no release).

| Group (section 6) | Task | Findings | Depends on |
|---|---|---|---|
| 1. Leaf moves that break the SCC | GM-705 | R-1, R-2, R-3, R-4 (step 1), R-6, R-8, R-10, R-30 | - |
| 2. Control plane | GM-706 | R-5, R-4 (step 2) | GM-705 |
| 3. Watcher and daemon startup | GM-707 | R-15, R-25 | GM-706 |
| 4. Data layer | GM-708 | R-12, R-16, R-18, R-22, R-23, R-32 | GM-706 |
| 5. MCP seam | GM-709 | R-11, R-17, R-19, R-27 | GM-706 |
| 6. Configuration | GM-710 | R-9, R-20, R-31 | - |
| 7. Plugin contracts | GM-711 | R-14, R-21, R-24, R-26 | - |
| 8. Scripts and CI | GM-712 | R-7, R-13 | - |
| 9. SDK structure | GM-713 | R-28, R-29 | - |
| 10. Large and optional | GM-714 | R-33, R-34, R-35 | GM-707, GM-708, GM-709 |

Follow-up not covered by the reviews: crate-level decomposition of core (design) is GM-718, after GM-705 and GM-706.

Side discoveries (section 8):

| Side discovery | Task |
|---|---|
| 8.1 `get_dependencies` Incoming misses closure `use` and inline `crate::` paths | GM-715 |
| 8.2 Go and SDK id schemes differ (open decision: migrate Go to NUL vs correct the docs) | GM-716 |
| 8.3 Go plugin's excluded dirs lack `.claude` (R-1 then makes drift impossible) | GM-717 |
| 8.4 Plugin version mismatch (resolved, no task) | - |
| 8.5 Stale documentation | folded into GM-706 (gc), GM-710 (config), GM-711 (python emit, go.mod) |
| 8.6 Module-map `log` fan-in undercount | no task (method note) |
| 8.7 CLI-run walks log as the daemon | GM-710 (R-31) |

Per finding: R-1 GM-705, R-2 GM-705, R-3 GM-705, R-4 GM-705 (step 1) / GM-706 (step 2),
R-5 GM-706, R-6 GM-705, R-7 GM-712, R-8 GM-705, R-9 GM-710, R-10 GM-705, R-11 GM-709,
R-12 GM-708, R-13 GM-712, R-14 GM-711, R-15 GM-707, R-16 GM-708, R-17 GM-709, R-18 GM-708,
R-19 GM-709, R-20 GM-710, R-21 GM-711, R-22 GM-708, R-23 GM-708, R-24 GM-711, R-25 GM-707,
R-26 GM-711, R-27 GM-709, R-28 GM-713, R-29 GM-713, R-30 GM-705, R-31 GM-710, R-32 GM-708,
R-33 GM-714, R-34 GM-714, R-35 GM-714. X-1..X-3 rejected: no tasks.
