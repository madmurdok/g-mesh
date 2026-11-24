# GM-718: target crate decomposition of `core`

Status: design (S1), for owner review. Base: `main` at `f8925f37` (release 4.35.0). Docs only.

Scope: which top-level modules of `core/src` should become their own workspace crates, in what
order, what that buys builds and CI, and what it costs. The GM-704 review
(`docs/review/4.30/README.md`) covered module coupling inside `core`; its only crate-level items
were R-28 (`plugins/sdk-lsp`, accepted, done in GM-713) and X-2 (a crate for the Rust plugin's OS
sandbox, rejected). X-2 is about `plugins/rust`, not `core`, and nothing here revives it: it still
has no second consumer.

Scratch scripts behind the numbers (not part of the repo): `an.py` (layers, lines, external
crates), `crates.py` (module edges projected onto the proposed crates), `pubcrate.py` (visibility
estimate), `tests.py` (test counts). They reuse `scripts/module-cycles.py`'s parser
(`collect`, `strip_cfg_test`, `is_test_file`), so they use the same edge definition as the review's
module map, sections 3-5.

## 0. Corrections to the task's starting figures

- **Size.** `core/src` is 135,941 lines in 218 files
  (`find core/src -name '*.rs' -print0 | xargs -0 cat | wc -l`): 97,816 in non-test files (inline
  `#[cfg(test)]` blocks included) and 38,125 in test files (`crates.py`). `core/tests` is 37,873
  more (`find core/tests -name "*.rs" | xargs cat | wc -l`).
- **Test binaries.** `core` no longer has 87. Since GM-623 it has 2 integration binaries
  (`it`, 97 files, 394 test fns; `scripts`, 13 files, 220 test fns) plus the lib and bin unit-test
  binaries (`cargo metadata --offline --no-deps`: `g-mesh {lib 1, bin 1, test 2}`; `tests.py`).
- **The module SCC is gone.** GM-705, GM-706, GM-707, GM-708, GM-709 and GM-719 are merged on
  `main` (`git log --oneline main | grep -E "GM-70[5-9]|GM-719"`). GM-719 pins it:
  `core/tests/scripts/module_cycles_script.rs:80` runs `module-cycles.py --max-scc 1`.

## 1. Layer graph after GM-705/706

### 1.1 Method

`python3 scripts/module-cycles.py --edges` (non-test code, the module-map edge definition) and
`python3 scripts/module-cycles.py --include-tests --edges`. Layer = longest path to a sink in the
non-test graph (`an.py`), so every edge points to a strictly lower layer. Lines: `wc -l`
equivalent over non-test files / test files of the module. "Ext" = external crates named in the
module's non-test code (file count).

### 1.2 Tarjan

- **Non-test code: no SCC.** `module-cycles.py --edges` ends with `SCC: none (module graph is
  acyclic)`. The 13-module component of the 4.30 map is gone.
- **With tests: one 16-module SCC** (`module-cycles.py --include-tests --edges`): config, control,
  daemon, embedding, gc, graph, index_health, ingest, languages, link, mcp, plugin_manifest,
  project_detect, storage, test_plugin, watcher. It is made only of test-only edges (27 of them,
  `an.py`), 18 of which point upward in the layering. Inside one crate this is harmless; across a
  crate boundary each upward test edge is a compile error, so section 3 lists them as blockers.

### 1.3 Layers (non-test edges)

| layer | module | lines non-test / test files | lib test fns | fan-out | fan-in | depends on | ext |
|---|---|---|---|---|---|---|---|
| 0 | fsutil | 19 / 0 | 0 | 0 | 3 | - | - |
| 0 | knobs | 457 / 0 | 4 | 0 | 11 | - | - |
| 0 | process | 560 / 0 | 3 | 0 | 4 | - | libc, sysinfo, windows-sys |
| 0 | project_walk | 668 / 0 | 10 | 0 | 6 | - | g-mesh-walk, ignore |
| 0 | protocol | 1302 / 0 | 38 | 0 | 10 | - | g-mesh-wire |
| 1 | log | 232 / 0 | 7 | 1 | 4 | knobs | - |
| 1 | model | 469 / 0 | 3 | 1 | 8 | protocol | g-mesh-wire, sha2 |
| 1 | paths | 990 / 0 | 18 | 1 | 11 | knobs | dirs, sha2 |
| 2 | graph | 5311 / 0 | 75 | 2 | 6 | model, protocol | rusqlite, flate2, base64, schemars |
| 2 | indexing_status | 1028 / 0 | 22 | 1 | 4 | paths | tokio |
| 2 | ipc | 878 / 0 | 8 | 1 | 4 | paths | tokio, libc, windows-sys |
| 3 | control | 1453 / 187 | 25 | 3 | 3 | ipc, paths, process | - |
| 3 | link | 3299 / 4722 | 146 | 3 | 2 | graph, model, protocol | rusqlite |
| 4 | storage | 8417 / 2834 | 187 | 4 | 11 | knobs, link, model, paths | rusqlite, sqlite-vec |
| 5 | embedding | 8590 / 0 | 150 | 6 | 5 | indexing_status, knobs, log, model, paths, storage | ort, tokenizers, rusqlite |
| 5 | languages | 238 / 420 | 22 | 2 | 4 | project_walk, storage | - |
| 5 | project_detect | 977 / 0 | 26 | 2 | 4 | paths, storage | rusqlite |
| 6 | config | 748 / 0 | 20 | 3 | 3 | embedding, knobs, paths | toml |
| 6 | ingest | 2602 / 3245 | 70 | 8 | 3 | embedding, fsutil, graph, knobs, log, model, protocol, storage | rusqlite |
| 6 | plugin_manifest | 882 / 1379 | 64 | 5 | 5 | knobs, languages, link, project_walk, protocol | g-mesh-manifest, globset |
| 7 | gc | 372 / 0 | 9 | 3 | 3 | config, paths, storage | - |
| 7 | index_health | 428 / 0 | 0 | 5 | 2 | fsutil, graph, plugin_manifest, project_walk, storage | rusqlite |
| 7 | test_plugin | 723 / 0 | 0 | 3 | 0 | plugin_manifest, protocol, storage | rusqlite (cfg(test) module) |
| 7 | watcher | 2088 / 106 | 50 | 4 | 1 | ingest, model, project_walk, storage | notify, g-mesh-walk |
| 8 | mcp | 15594 / 14676 | 612 | 13 | 2 | embedding, gc, graph, index_health, indexing_status, ipc, knobs, languages, model, plugin_manifest, project_detect, protocol, storage | rmcp, rusqlite, tokio |
| 9 | daemon | 13109 / 6995 | 323 | 22 | 2 | every lower module except index_health, link, test_plugin | rusqlite, tokio, sysinfo |
| 10 | shim | 3087 / 0 | 25 | 8 | 1 | control, daemon, ipc, knobs, paths, process, project_detect, protocol | - |
| 11 | cli | 23254 / 3561 | 420 | 21 | 0 | every lower module except fsutil, ipc, link, model, test_plugin, watcher | clap, ureq, tar, tokenizers, rmcp |

Lib test fns: `#[test]`/`#[tokio::test]` count per module (`tests.py`), 2,337 in all.

### 1.4 What the graph says

- `mcp` no longer reaches `daemon` (GM-709's `McpHost`); `daemon -> mcp` is the only direction
  (g-mesh `get_dependencies` Incoming on `g_mesh::mcp`: outside `mcp` only `daemon/mod.rs`,
  `daemon/front.rs`, `daemon/mcp_host.rs`, `daemon/registry.rs`, `cli/plugin_check/expectations.rs`
  and 4 `core/tests/it` files).
- `storage` sits on `knobs, link, model, paths` only; GM-708 removed `storage -> graph/embedding`
  from non-test code (they survive only in tests, 1.2).
- The heavy native dependencies are concentrated: `ort`/`tokenizers` in `embedding` (plus
  `cli`), `rmcp` in `mcp` (plus one `cli` file), `rusqlite` from layer 2 up, `notify` in `watcher`.
- Churn (non-merge commits touching the module since 2026-05-24, of 690 touching `core/src`;
  `git log --no-merges --since=2026-05-24 --format=%h -- <path> | wc -l`): daemon 290, mcp 245,
  cli 241, graph 123, storage 120, watcher 93, embedding 74, protocol 35, everything else under 20.
  Undercounts modules created by recent moves (no `--follow`). The three hottest modules are the
  three top layers, which is the shape where crate boundaries below them pay off most.

## 2. Proposed crates

Directory layout: new packages as siblings of `core/` (`core-base/`, `core-index/`, `core-embed/`,
`core-mcp/`), like `wire/`, `walk/`, `manifest/`. `core/` stays the `g-mesh` package, so its
`build.rs` (Go plugin build), `core/tests/{it,scripts}`, `CARGO_BIN_EXE_g-mesh` and every
`core/src/{cli,daemon,shim}` path rule in `scripts/` keep working.

Line totals per crate: `crates.py` (non-test files incl. inline tests / test files).

### 2.1 `g-mesh-base` - recommended (enabler)

- **Modules:** knobs, fsutil, paths, log, process, ipc, control, indexing_status, protocol,
  project_walk, and `config` without `config/embedding.rs`. 8,047 / 187 lines; 155 lib test fns.
- **Public API:** the `G_MESH_*` knob table and readers; `paths` (state-dir layout, `StateDir`,
  atomic state-file writes); `process` (alive/stop/spawn-detached); `ipc::Endpoint`; `control`
  (stop/purge/sweep); `IndexingStatus`; `protocol` (ndjson/jsonrpc framing, handshake,
  conformance checks, re-exported wire types); `project_walk`; `config` (TOML read/write).
- **External deps:** g-mesh-wire, g-mesh-walk, tokio, libc/windows-sys, sysinfo, dirs, sha2, toml,
  ignore. No SQLite, ONNX or rmcp.
- **Benefit:** every other crate needs these modules (`index -> base` 25 sites in 14 files,
  `crates.py`), so it is the precondition for 2.2-2.4. It is also the dependency set of `shim`
  minus `daemon`/`project_detect`.
- **Risk:** low alone, but it buys almost nothing alone: a change here still rebuilds and retests
  everything. 34 `pub(crate)` items become `pub` (2.6). `config` has one production edge up into
  `embedding` (`config/mod.rs:133`, `crate::embedding::DEFAULT_MODEL_NAME`) that must be cut first.

### 2.2 `g-mesh-index` - recommended

- **Modules:** model, graph, link, storage, project_detect, languages, plugin_manifest,
  index_health, gc, and `test_plugin` (behind a `testkit` feature). 21,116 / 9,355 lines; 532 lib
  test fns. Plus `indexer_version`/`plugins_digest`/`CURRENT_INDEXER_VERSION` moved down from
  `daemon::registry` (3.2, B2).
- **Public API:** `model` row types; `storage` (`IndexStore`, connection, schema, writes,
  vectors, language swap); `graph` read queries; `link` passes; `plugin_manifest` (read, discover,
  `DiscoveredPlugins`); `languages::CATALOGUE`; `index_health::index_status`; `gc::last_used`;
  `project_detect`.
- **External deps:** base + rusqlite (bundled SQLite), sqlite-vec, g-mesh-manifest, globset,
  flate2, base64, schemars.
- **Benefit:** the data layer is the second-hottest area (graph 123, storage 120 commits) and the
  one whose internal cycles GM-705/708 broke by hand; a crate makes the compiler keep `storage`
  from reaching `daemon`, `embedding` or `ingest` again. A change in `daemon`, `mcp`, `cli`,
  `shim`, `ingest` or `watcher` no longer recompiles 21k lines of data layer or reruns its 532
  lib tests. `plugin_manifest`, `languages` and `index_health` go here (not higher) because
  `link`, `languages` and `storage` tests already use them and their production deps all fit.
- **Risk:** the most test-only back edges (B3: 7 files reach `daemon`/`ingest`, 2 reach
  `embedding`, 1 reaches `mcp`); 49 `pub(crate)` items named from other crates (upper bound, 2.6);
  the lib-test heavy list and spawn map name `g-mesh` binary ids for `link::`, `storage::`,
  `plugin_manifest::`, `languages::` tests.

### 2.3 `g-mesh-embed` - recommended

- **Modules:** embedding, and `config/embedding.rs` (settings resolved from config). g-mesh
  `get_dependencies` Incoming on `g_mesh::config::embedding` lists only `cli/model.rs`; its helpers
  (`embedding_settings`, `rerank_settings`, `inference_options`) are also called by path from
  `cli/{embed_eval,init,reindex,status}.rs`, `cli/embed_eval/probe.rs`, `daemon/startup.rs` and
  `mcp/search_code.rs` (grep), all above `embed`. 8,878 / 0 lines; 150 lib test fns.
- **Public API:** `EmbeddingPipeline`, `Embedder`, model constants and download metadata,
  `Reranker`/`Scorer`, `text` (embed tiers, noise rule), cache, backfill; features `embed-eval`
  and `ep-coreml` move here (`cfg(feature)` sites: embedding 3, mcp 3, cli 7 -
  `grep -rn 'feature *= *"' core/src`).
- **External deps:** base + index + ort (ONNX Runtime), tokenizers, sysinfo.
- **Benefit:** isolates the heaviest native dependency and the model features in one package.
  `ort`'s build script and the tokenizer stack no longer sit in the crate whose incremental
  rebuild every `daemon`/`mcp`/`cli` edit triggers. No upward test edges (1.2).
- **Risk:** `embedding::pipeline::test_support` (fake pipeline, fake vectors) is used by tests in
  `ingest`, `mcp` and `daemon` (`grep -rlE "pipeline::test_support" core/src`: 6 files outside
  `embedding`), so it needs the `testkit` feature. `embedding/cache.rs:612-638` `include_str!`s
  `core/Cargo.toml` and `Cargo.lock` (ort pin check) and `mcp/search_code_rerank_tests.rs:691`
  includes `../embedding/testdata/rerank_parity.json`; both paths change. 40 `pub(crate)` items
  named from other crates (upper bound).

### 2.4 `g-mesh-mcp` - optional

- **Modules:** mcp. 15,594 / 14,676 lines; 612 lib test fns (the largest section).
- **Public API:** `McpHost` trait, `Activity`, `PathCoverage`, the rmcp server front, `QueryCtx`,
  `QueryShapes`, tool answers/renderers used by `cli/plugin_check` (23 `pub(crate)` names, upper
  bound).
- **External deps:** base + index + embed + rmcp.
- **Benefit:** the boundary already exists as a trait (GM-709), so this is the cheapest of the
  upper cuts. A `daemon`/`cli` edit stops recompiling 15.6k lines and rerunning 612 tests; an
  `mcp` edit stops recompiling `daemon`+`cli`'s dependencies below it.
- **Risk:** three test files reach `daemon` (`mcp/instructions/tests.rs`,
  `mcp/member_name_collision_tests.rs`, `mcp/unlinked_tests.rs`) and they spawn real plugins
  (`scripts/test-plugin-spawns.txt` `lib:mcp::*` rows); they must move to `core` or onto
  `McpHost` fakes. `env!("CARGO_PKG_VERSION")` at `mcp/front.rs:279` and `mcp/mod.rs:824` would
  report `g-mesh-mcp`'s version, so every core crate needs `version.workspace = true` (or the
  version passed in). Six heavy-list rows are `binary_id(g-mesh) & test(=mcp::...)`. Optional
  because its gain overlaps with today's `--narrow` rule for `core/src/mcp/*`, which already
  selects only `core-mcp core-it`.

### 2.5 What stays in `g-mesh` (binary crate, `core/`)

daemon, shim, cli, ingest, watcher, `main.rs`, `lib.rs`, `build.rs`, `core/tests/{it,scripts}`.
44,181 / 13,907 lines; lib test fns: daemon 323, cli 420, ingest 70, watcher 50, shim 25.
`lib.rs` becomes a facade: `pub use g_mesh_index::storage;` etc., so the 97 `core/tests/it` files
keep their `g_mesh::storage` (78 files), `g_mesh::paths` (64), `g_mesh::daemon` (58) paths
(`tests.py`).

### 2.6 Considered and not proposed

| candidate | mark | benefit | risk / why not |
|---|---|---|---|
| `g-mesh-ingest` (ingest + watcher, 4,690 lines) | optional, later | a `daemon`/`cli`/`shim` edit stops recompiling them | `daemon` imports them, so a `watcher` edit (93 commits) still rebuilds `daemon` and `cli`; small gain for a fifth crate |
| `g-mesh-daemon` (daemon alone) | rejected for now | a `cli` edit (241 commits) stops recompiling `daemon` | 22 module deps, 58 `core/tests/it` files name it, 174 `pub(crate)` items in the bin crate; revisit after the measurement in 3.1 |
| one crate per module (~28) | rejected | maximal enforcement | manifest/feature bookkeeping per crate, linking and `pub` surface for a 19-line `fsutil` |
| `protocol` into `g-mesh-wire` | rejected | one protocol crate | `protocol` holds host-side checks (conformance, `front`), not shared types; the SDK would compile them |
| sandbox crate (X-2) | rejected (unchanged) | - | `plugins/rust`, not `core`; no second consumer |

Resulting crate DAG (non-test, `crates.py` after B1): `index -> base`, `embed -> {base, index}`,
`mcp -> {base, index, embed}`, `g-mesh -> all`. The only upward non-test edge today is
`base -> embed` (`config/mod.rs`), which is B1.

`pub(crate)` items that files of another proposed crate name (`pubcrate.py`, textual: the item's
module path plus its bare name in the same file, so common names like `new`/`open` over-count):
base 34 of 42, index 49 of 105, embed 40 of 77, mcp 23 of 122. Upper bound ~146 items to widen
to `pub` (or to re-export from a narrower `pub` facade). Existing `pub` items: base 319, index 319,
embed 102, mcp 54.

## 3. Extraction order

### 3.1 Step 0: baseline measurement (no code change)

Not measured in this design: building `g-mesh` here would compete with other agents' `target/`
use. Measure before step 2, with `uptime` and `/usr/bin/time -p` recorded: `cargo build -p
g-mesh --lib --timings` cold, then incremental after a touch of `core/src/cli/mod.rs`,
`core/src/mcp/mod.rs` and `core/src/storage/mod.rs`; per-section nextest wall time from the last
full CI run's JUnit. The same three touches after each step give the gain. If the incremental
rebuild after a `cli` touch is not meaningfully shorter after step 3, steps 4-5 are not worth it.

### 3.2 Step 1: prepare inside one crate

All reversible, no new packages. Each blocker with its evidence:

- **B1** cut `config -> embedding` in production: `config/mod.rs:133` uses
  `crate::embedding::DEFAULT_MODEL_NAME` (move the default name to `config` or inject it);
  `config/embedding.rs` (288 lines) moves next to `embedding` (`wc -l core/src/config/embedding.rs`).
- **B2** move `indexer_version`, `plugins_digest`, `CURRENT_INDEXER_VERSION` and
  `fixture_indexer_version` from `daemon::registry` into the index layer. g-mesh `find_callers
  fixture_indexer_version`: 9 files, of which 5 are below `daemon` (`storage/index_store.rs`,
  `storage/state/last_used.rs`, `project_detect.rs`, `gc/last_used.rs`, `gc/warning.rs`) and one in
  base (`control/fixture_tests.rs`). `indexer_version` takes only `&DiscoveredPlugins`
  (g-mesh `find_definition`), so it fits next to `plugin_manifest`.
- **B3** resolve the upward test edges across the proposed boundaries (`crates.py`, tests
  included):
  - base -> index/bin: `control/fixture_tests.rs` (fixture version, storage).
  - index -> embed: `storage/language_swap.rs:704,1470,1515` (`TextRule`, `EMBEDDING_DIM`; g-mesh
    `find_references TextRule`: 8 refs in that file), `link/symbol_links/tests.rs:3573`.
  - index -> mcp: `graph/pagination.rs:785` (`mcp::response_bound`).
  - index -> bin: `storage/containers/tests.rs:598` (`ingest::convert::to_node_record`),
    `plugin_manifest/tests.rs:42` (`daemon::plugin::missing_workspace_binary_hint`), and the B2
    fixture callers.
  - mcp -> bin: `mcp/instructions/tests.rs`, `mcp/member_name_collision_tests.rs`,
    `mcp/unlinked_tests.rs` (only for step 5).
  Each is either moved up to the crate that owns the higher module, or rewritten on a lower
  fixture.
- **B4** the cross-crate test helpers: 35 `#[cfg(test)] pub`/`pub(crate)` items
  (`grep -rn -A1 "#\[cfg(test)\]" core/src | grep -E "\.rs-[0-9]+-\s*pub"`), of which
  `test_plugin` (used by 9 files in `daemon`/`watcher`, g-mesh `get_dependencies` Incoming on
  `g_mesh::test_plugin`) and `embedding::pipeline::test_support` cross a proposed boundary.
  `cfg(test)` is not visible to another crate's tests; they go behind a `testkit` feature.
- **B5** a guard that pins the target crate DAG before any package exists: extend
  `module-cycles.py` with a module-to-crate map (as in `crates.py`) and an `--layers` check that
  fails on an upward crate edge, test code included, and pin it in `core/tests/scripts` next to
  GM-719's `--max-scc 1`. After the split the compiler takes over and the check is retired.

GM-708 (storage<->graph/embedding) and GM-709 (mcp host trait) are **done** (merged into
`release-4.32.0`, both in `main`), so they no longer block anything; the remaining storage edges
to graph/embedding are the test ones in B3.

### 3.3 Steps 2-5

| step | extract | blockers | also changes |
|---|---|---|---|
| 2 | `g-mesh-base` | B1 | workspace members; `test-select.sh` rule for `core-base/*` (all core sections); `version.workspace = true` |
| 3 | `g-mesh-index` | B2, B3 (index rows), B4 (`test_plugin`, fixture) | `.config/nextest.toml` heavy rows `binary_id(g-mesh) & test(=link::...)`; `scripts/test-plugin-spawns.txt` `lib:` rows for `link::`, `languages::`, `plugin_manifest::`, `storage::` (13 `lib:` rows in all, `grep -c "^lib" scripts/test-plugin-spawns.txt`); SCRIPTS_CLOSURE paths in `test-select.sh` (`core/src/plugin_manifest*`, `languages*`, `link/symbol_links*`, `storage/language_outcome.rs`, `protocol/*`) |
| 4 | `g-mesh-embed` | B1 (`config/embedding.rs`), B4 (`pipeline::test_support`) | features `embed-eval`/`ep-coreml` forwarded from `g-mesh`; `include_str!` paths in `embedding/cache.rs` and `mcp/search_code_rerank_tests.rs` |
| 5 (optional) | `g-mesh-mcp` | B3 (mcp rows), `CARGO_PKG_VERSION` | six heavy rows `test(=mcp::response_bound_tests::...)`; `lib:mcp::*` spawn rows; `query_shapes.rs:82-85` test `include_str!` of plugin manifests |

Each step is one task and one release-sized diff: move files with `git mv`, add the facade
re-export in `lib.rs`, fix visibility, update the scripts named above. `test-sections.sh check`
fails on a package that no section selects, so a forgotten section is caught.

## 4. Expected build/CI gain

### 4.1 Sections today and after

Today (`scripts/test-sections.sh` `section_filter`, `tests.py` counts):

| section | selects | lib test fns |
|---|---|---|
| core-it | `g-mesh::it` (97 files, 69 name `CARGO_BIN_EXE`/bin helpers) | 394 (integration) |
| core-scripts | `g-mesh::scripts` | 220 (integration) |
| core-mcp | `mcp::` | 612 |
| core-daemon | `daemon::` | 323 |
| core-cli | `cli::` | 420 |
| core-graph | `graph::`, `link::` | 221 |
| core-rest | the rest + bin | 761 |

Proposed (package-based filters): `core-base` 155, `core-index` 532, `core-embed` 150,
`core-mcp` 612 (now `package(g-mesh-mcp)`), `core-daemon` 323, `core-cli` 420, `core-rest` 145
(ingest 70, watcher 50, shim 25), `core-it`, `core-scripts`. Sum 2,337, the same tests.

### 4.2 What a change selects

| change under | today, `--narrow` (`test-select.sh:177-189`) | after steps 2-5 | lib tests no longer run |
|---|---|---|---|
| base modules | every core section but core-scripts | every core section but core-scripts | 0 |
| index modules | same | index, embed, mcp, daemon, cli, rest, it | 155 |
| embedding | same | embed, mcp, daemon, cli, rest, it | 687 |
| mcp | core-mcp, core-it | mcp, daemon, cli, rest, it | (today's rule is narrower, but hand-maintained) |
| daemon, ingest, watcher, shim | every core section but core-scripts | daemon, cli, rest, it | 1,449 |
| cli | core-cli, core-rest, core-it | daemon, cli, rest, it (`cli` and `daemon` share a crate) | 1,449, but the hand rule today is narrower still |

Two facts bound the gain:

- `core-it` runs for every core change before and after: it spawns the `g-mesh` binary, which
  links every crate. It is also the slow section (the `daemon-spawning` test group, 2 threads,
  `.config/nextest.toml`). So the per-change CI time saved is lib-test time, which is small per
  test, plus build time.
- CI calls `test-select.sh` without `--narrow` and keeps every core section for any core change
  (`test-select.sh:10-11`). The real CI gain is that package-level selection becomes **sound by
  construction** (cargo's dependency graph), so CI can adopt it, while today's narrow rules are
  hand-kept claims (SCRIPTS_CLOSURE, "`cli/` also selects core-rest because a storage test calls
  `cli::status`") that the owner chose not to trust in CI.

The build gain is likely the larger one and is unmeasured (3.1): an edit in the three hottest
modules (daemon 290, mcp 245, cli 241 of the 690 `core/src` commits, 1.4) recompiles the 44k-line
binary crate (60k with `mcp`) instead of a 98k-line crate, and rustc's front end runs per crate, so base/index/embed also compile in
parallel on a cold build.

### 4.3 Sections that stop depending on crates

- `core-base`: depends on no other core crate.
- `core-index`: stops depending on embed, mcp and the binary crate (ort, rmcp, notify, clap no
  longer in its build).
- `core-embed`: stops depending on mcp and the binary crate.
- `core-mcp`: stops depending on the binary crate (daemon, cli, ingest, watcher).
- `sdk`, `sdk-lsp`, `plugin-*`, `wire`: unchanged; they never depended on `core`.

## 5. Costs and risks

- **pub API surface.** About 146 `pub(crate)` items (upper bound, 2.6) become `pub`, and nothing
  in the workspace is published, so `pub` stops meaning "API" for these crates. Mitigation: keep
  the moved modules' internals `pub(crate)` where no other crate names them, and add no new
  `pub use` chains beyond the `lib.rs` facade. Dead-code lint coverage shrinks: rustc does not
  warn about an unused `pub` item.
- **Feature flags.** `embed-eval` and `ep-coreml` (with `ort/coreml`, optional
  `tracing-subscriber`) become features of `g-mesh-embed`, forwarded by `g-mesh-mcp` and
  `g-mesh`. Each section runs with `--workspace` (ADR 0030), so features unify; a `-p` build of
  one crate could compile a different feature set than CI does. A new `testkit` feature on
  `g-mesh-index` and `g-mesh-embed` is enabled through dev-dependencies; with resolver 2 it is
  unified into every target built in a test run, so the `g-mesh` binary that `core-it` spawns
  contains the testkit code (not the release build).
- **Compile time of the dependency chain.** The chain base -> index -> embed -> mcp -> g-mesh is
  serial at the front end: a cold build gains parallelism only between independent crates, and
  here they are a chain. A change in `base` now rebuilds four more crates' metadata. Codegen
  units still parallelise inside each crate. Net effect unknown until step 0.
- **`lib.rs` facade and R-35.** The facade (`pub use g_mesh_index::storage;` ...) keeps
  `core/tests` and `cli` paths unchanged, which is cheap but keeps `g_mesh::*` as wide as today.
  R-35 (narrow `lib.rs` with a `#[doc(hidden)] pub mod testkit`) becomes simpler after the split:
  the testkit naturally lives in the lower crates' `testkit` features, and `core/tests` can
  import `g_mesh_index::...` directly. The review marked R-35 optional and contested (K-7); this
  design does not require it.
- **Aliases in `daemon`.** `daemon/mod.rs` re-exports `project_detect as candidates`,
  `plugin_manifest as manifest` and `indexing_status`; `core/tests` uses them 17 times
  (`grep -rhn "g_mesh::daemon::\(manifest\|candidates\|indexing_status\)" core/tests | wc -l`).
  They keep working through the facade; removing them is separate.
- **Scripts that know module paths.** `test-select.sh` path rules and SCRIPTS_CLOSURE,
  `test-plugin-spawns.txt` `lib:` rows, the heavy list in `.config/nextest.toml` (12 rows
  `binary_id(g-mesh)`; `grep -c "binary_id(g-mesh)" .config/nextest.toml`), and
  `core/tests/it/module_boundaries.rs` / `module_cycles_script.rs` (which read `core/src`). All
  need edits per step; `test-sections.sh check` catches a section gap but not a stale heavy row.
- **Version.** `env!("CARGO_PKG_VERSION")` in a moved crate reports that crate's version (today
  in `mcp`, `daemon/client.rs`, `cli`); `version.workspace = true` for all core crates avoids it.
- **Release packaging.** None expected: the shipped artifact is still the `g-mesh` binary and
  the plugin binaries; `build.rs` stays in `core/`.
- **Review cost.** Each extraction is a large `git mv` diff; `git log --follow` and `blame`
  survive renames, but churn statistics by path (as in 1.4) reset.
- **An ADR** records the crate boundaries and the rule "a lower crate never names a higher one,
  tests included" when step 2 lands.

## Owner decisions

1. **Scope.** (a) base + index + embed now, mcp later after the step-0 numbers (recommended);
   (b) all four including mcp in one batch; (c) only step 0 and step 1 (measurement and in-crate
   preparation), decide crates after.
2. **Gate on measurement.** Run step 0 and require a measured incremental-build gain after step 3
   before steps 4-5 (recommended), or proceed on the structural argument alone.
3. **Test helpers across crates.** A `testkit` cargo feature on the lower crates (recommended:
   keeps tests where they are) versus moving every cross-crate test up into `g-mesh` (no feature,
   but tests move away from the code they cover).
4. **`lib.rs` facade.** Keep `g_mesh::<module>` re-exports so `core/tests` is untouched
   (recommended), or switch `core/tests` to the new crate paths during each step (R-35 direction,
   larger diffs).

### Decisions (owner, 2026-11-24)

1. Scope: "Все четыре сразу". All four crates (base, index, embed, mcp) are in scope.
2. Gate: "Гейт по замеру (Recommended)". Step 0 runs first; steps 4-5 start only after a measured
   incremental-build gain after step 3.
3. Test helpers: "Фича testkit (Recommended)".
4. `lib.rs`: "Фасад с ре-экспортами (Recommended)". `core/tests` keeps `g_mesh::<module>` paths.
