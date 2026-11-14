# Area A (Runtime) architecture review: A-opus

Model: claude-opus-5-5

Scope: `core/src/{daemon,ipc,protocol,shim,shim.rs,watcher,gc,process.rs}` and their boundaries.
Goal: refactors that let weakly coupled parts of g-mesh change independently.
Input: `docs/review/4.30/module-map.md` (sections 2-5, 7), then code via g-mesh (project `g-mesh`) and grep.

Evidence tags: **[measured]** = seen in code or tool output; **[inferred]** = a judgement drawn from it.

Summary of the dependency picture (from module-map section 5): every Runtime module except `ipc` and
`process` is in the one 13-module strongly connected component. Most of the back-edges that put it there are
small and come from a few misplaced files, not from the runtime's real design. Findings 1, 2, 3, 4 and 8
remove these edges: `storage->daemon`, `languages->daemon`, `embedding->daemon`, `daemon->cli`,
`shim->cli`, `watcher->daemon`, `project_walk->watcher`, `protocol->graph`.

---

## A-opus-1. Move the plugin manifest model out of `daemon/` into its own top-level module

- **Location:** `core/src/daemon/manifest.rs` (whole module: `PluginManifest`, `DiscoveredPlugins`,
  `Capabilities`, `MemberOverrides`, `NonSymbolShapes`, `ReceiverCallResolution`, `discover`,
  `read_manifest`, `link_rules`, `extension_of`, `under_excluded_dir`, ...).
- **Problem (dependency direction, cohesion):** `manifest` is a pure data/schema module. Its only crate
  imports are `graph::symbol_links::LinkRules` and `protocol::types::CURRENT_PROTOCOL_VERSION` (manifest.rs:20-21)
  plus `log_line!` [measured]. Nothing in it depends on the daemon runtime. But because it lives under
  `daemon/`, every reader of a manifest depends on `daemon`: `languages.rs:42`
  (`use crate::daemon::manifest::{extension_of, under_excluded_dir, DiscoveredPlugins}`) is the single
  production edge behind the `daemon <-> languages` cycle [measured]. `mcp` uses
  `daemon::manifest::*` in about 40 import sites; `MemberOverrides` alone appears 12 times [measured, grep over `mcp/`].
  So a Data-area module (`languages`) and a Surface module (`mcp`) depend on the Runtime area for
  what is really a shared vocabulary.
- **Proposed change:** move `daemon/manifest.rs` and `daemon/manifest/tests.rs` to `core/src/plugin_manifest/`
  (or `manifest/`) as a top-level module. Leave `pub use crate::plugin_manifest as manifest;` in
  `daemon/mod.rs` for one release so call sites can migrate gradually. Callers change only their `use` paths.
  `daemon::plugin::fingerprint(&PluginManifest)` and `registry::indexer_version(&DiscoveredPlugins)`
  stay in daemon and import from the new module.
- **Benefit:** a manifest schema change (a new `[plugin.*]` table, a new capability) no longer touches
  `daemon`'s module graph. `languages` stops depending on Runtime, which removes one 2-cycle, and `mcp`
  drops most of its daemon imports. The daemon runtime can be restructured (A-opus-6) without touching
  60 files.
- **Effort:** S (a mechanical move with a re-export shim). **Risk:** low. Doc links
  (`[...](crate::daemon::manifest::...)`) and the test fixtures that use `daemon::manifest::discover`
  need updating. `cargo doc` with `-D warnings` will find the broken intra-doc links.
- **Evidence:** `get_dependencies(g_mesh::daemon::manifest, Incoming, depth 1)`: 60 importing files,
  not truncated. They include `languages.rs`, 14 `mcp/*` files, 8 `cli/*` files and 11 daemon files [measured].
  `grep "^use \|crate::" daemon/manifest.rs`: only the graph and protocol imports [measured].

## A-opus-2. Move the other daemon-free leaf modules out of `daemon/` (identity, indexing_status, candidates, plus two misplaced constants)

- **Location:** `daemon/identity.rs` (`project_hash`, `read_project_root`, ...),
  `daemon/indexing_status.rs` (`IndexingStatus`, `Phase`, `Need`), `daemon/candidates.rs`
  (`detect`, `Detection`, `relative_to_root`), `watcher::BASELINE_EXCLUDED_DIRS` (watcher/mod.rs:34),
  `watcher::staleness::mtime_millis` (staleness.rs:425).
- **Problem (dependency direction, cycles):** each of these is a leaf with no dependency on the daemon
  runtime, yet it sits inside `daemon`/`watcher`. That placement creates whole cycles by itself:
  - `identity.rs` imports only `std`, `anyhow`, `sha2` [measured]. Its use in `storage/connection.rs:7,206`
    is the only production `storage -> daemon` edge (module-map section 3: `storage->daemon 1/0`), so it
    closes the `daemon <-> storage` cycle [measured].
  - `indexing_status.rs` imports only `std`, `serde`, `tokio::sync::Notify` [measured]. Its use in
    `embedding/backfill.rs:102` is the only `embedding -> daemon` edge (`1/0`), so it closes
    `daemon <-> embedding` [measured]. `mcp/mod.rs` also uses it.
  - `candidates.rs` depends only on `storage` [measured]. It is used by `shim.rs:18`,
    `shim/router.rs:128` and `mcp`, which drags the shim process's code into the daemon module.
  - `project_walk.rs:31` imports `watcher::BASELINE_EXCLUDED_DIRS`, which is the only
    `project_walk -> watcher` edge (`1/0`) and closes that cycle [measured]. The walk owns exclusion
    policy; the watcher should import it from the walk, not the other way round.
  - `mtime_millis` is a filesystem helper. `daemon/build_stamp.rs:89` and `cli/status.rs:59` import it
    from `watcher::staleness` [measured].
- **Proposed change:** move `identity` to `paths` (it is "which state dir belongs to this root").
  Move `indexing_status` to a top-level `indexing_status` module (or `storage::status`, but it holds no
  SQL). Move `candidates` to a top-level `project_detect` module. Move `BASELINE_EXCLUDED_DIRS` into
  `project_walk` and have `watcher` import it. Move `mtime_millis` to `paths` or a small `fsutil`.
  Keep `pub use` re-exports at the old paths for one release.
- **Benefit:** three 2-cycles disappear (`storage<->daemon`, `embedding<->daemon`,
  `project_walk<->watcher`), and the shim no longer reaches into `daemon` for detection. The Data area
  then depends on Runtime only through `protocol::types` (see A-opus-8). After that it could
  be split into its own crate if wanted.
- **Effort:** S. **Risk:** low. These are pure moves. `IndexingStatus` is `pub` and used by 15 files, so the
  re-export matters.
- **Evidence:** `find_references(daemon::identity::project_hash, files)`: `storage/connection.rs`,
  `cli/clean.rs` and tests [measured]. `find_references(daemon::indexing_status::IndexingStatus, files)`:
  15 files including `embedding/backfill.rs` and `mcp/mod.rs` [measured]. Grep of each file's `use crate::`
  lines (above) [measured].

## A-opus-3. Give the daemon control plane (stop, evict, purge, sweep, bootstrap) one home below `cli` and `shim`

- **Location:** `cli/stop.rs:stop`, `cli/stop.rs:terminate_daemon`, `cli/clean.rs:purge_orphaned_state_dir`,
  `cli/clean.rs:sweep_orphaned_if_due`, `cli/clean.rs:{Purge, LockWait, Sweep, RootState, daemon_is_running,
  singleton_lock_is_held}`, `shim.rs:{connect_or_bootstrap, acquire_bootstrap_lock, retire_outdated_daemon,
  evict_wedged_daemon}`, `daemon/lifecycle.rs:purge_state_dir`.
- **Problem (dependency direction, leaky layering):** the operations that change a daemon's state
  (stop it, evict a wedged one, delete an orphaned state dir, bootstrap one) are implemented in
  the CLI command modules, and lower layers call up into them:
  - `daemon::lifecycle::purge_state_dir` calls `crate::cli::clean::purge_orphaned_state_dir`
    (lifecycle.rs:1015-1022), which is the `daemon -> cli` edge [measured].
  - `shim::retire_outdated_daemon` calls `cli::stop::stop`, `shim::evict_wedged_daemon` calls
    `cli::stop::terminate_daemon`, and `shim::spawn_orphan_sweep` calls
    `cli::clean::sweep_orphaned_if_due` (shim.rs:15,152,378,458) [measured].
  - In the other direction, `cli::deps::readiness_loop` calls `shim::connect_or_bootstrap`, and
    `cli::reindex::stop_for_rebuild` calls `cli::stop::stop` [measured].
  So `cli <-> shim` and `cli <-> daemon` are cycles (module-map section 5), and the CLI's
  user-facing modules (`clean.rs` is mostly argument scoping and rendering) are also the
  safety-critical deletion policy that a running daemon executes. `clean.rs`'s own doc says the
  purge is "Shared by the daemon's own purge ... and the shim's startup sweep ... so the two can never
  disagree with `clean orphaned`" [measured]. The sharing is right; the location is not.
- **Proposed change:** create `core/src/control/` (top level, Runtime area), or `daemon/control/`, with:
  `stop.rs` (`stop`, `terminate_daemon`, `TERMINATION_TIMEOUT`), `purge.rs`
  (`purge_orphaned_state_dir`, `LockWait`, `Purge`, `RootState`, `root_state`, `daemon_is_running`),
  `sweep.rs` (`sweep_orphaned_if_due`, `ORPHAN_SWEEP_INTERVAL`), and `bootstrap.rs`
  (`connect_or_bootstrap`, `acquire_bootstrap_lock`, `spawn_detached_daemon`, `wait_until_listening`,
  `incumbent`/`retire_outdated_daemon`/`evict_wedged_daemon`). Then `cli::stop`, `cli::clean`,
  `cli::reindex` and `cli::deps` only parse arguments, call `control`, and render. `shim.rs` keeps
  only process setup and `router::serve`, and `daemon::lifecycle` calls `control::purge`.
- **Benefit:** CLI wording and argument changes no longer recompile or risk the daemon's exit path.
  The deletion and eviction policy can be reviewed and tested as one unit. The shim's dependency set
  shrinks to `control`, `ipc`, `protocol` and `project_detect`, which would let the shim become a
  separate small binary or crate later. It also removes two cycles.
- **Effort:** M (about 600 lines move; `clean.rs`'s tests split into the policy tests that move and
  the rendering tests that stay). **Risk:** medium. The purge's lock polarity and
  `LockWait` semantics are subtle (GM-524), so the move must not edit logic. Existing integration
  tests (`cli_clean_sweeping`, `orphaned_daemon`, `worktree_daemon_lifecycle`) are the guard.
- **Evidence:** `find_callers(cli::clean::purge_orphaned_state_dir)`: `cli::clean::sweep_orphaned`,
  `daemon::lifecycle::purge_state_dir` and 5 tests. `find_callers(cli::clean::sweep_orphaned_if_due)`:
  `shim::spawn_orphan_sweep`. `find_callers(cli::stop::stop)`: `cli::stop::run`,
  `cli::reindex::stop_for_rebuild`, `shim::retire_outdated_daemon`. `find_callers(cli::stop::terminate_daemon)`:
  `cli::stop::stop`, `shim::evict_wedged_daemon`. `find_callers(shim::connect_or_bootstrap)`: `shim::run`,
  `cli::deps::readiness_loop`. All complete, no `hasMore` [measured].

## A-opus-4. Split `watcher` into a filesystem event source and an index-update pipeline

- **Location:** `watcher/apply.rs` (`apply_file_change`, `apply_file_change_in`, `reextract_affected`,
  `apply_semantic_pass*`, `to_node_record`, `to_edge_record`, `round_trip`), `watcher/staleness.rs`
  (`ensure_fresh`, `is_stale`, `record_walk_baselines`), and `daemon/config_reindex.rs:select_with`.
- **Problem (cohesion, misleading boundary, cycle):** `watcher/` holds two unrelated things:
  - an FS event source: `ProjectWatcher`, `forward_event`, `debounce`, `burst`, `batch`, `rescan`,
    `ignore_layers`. These depend on `notify` and `project_walk` [measured, watcher/mod.rs imports];
  - the plugin round-trip and diff application. `apply.rs` imports `embedding`, `protocol::jsonrpc`,
    `protocol::types`, `storage::{file_rows, index_store, schema, write}` (apply.rs:26-36), and
    `staleness.rs` imports `embedding`, `storage`, `watcher::apply` [measured].
  `apply.rs` is 4661 lines with its tests, and `apply_semantic_pass_in` (183 lines) and `apply_file_change_in`
  (148 lines) are among the largest functions in core (module-map section 7.3) [measured]. All of its
  non-test importers are in `daemon` (`semantic`, `workspace_reindex`, `lifecycle`, `bulk_index`,
  `plugin`) or `cli::plugin_check`. Nothing in the FS half uses it [measured]. The pipeline half
  also calls back into the daemon: `apply.rs:422` `use crate::daemon::config_reindex::{select_with, Selection}`
  is the only `watcher -> daemon` edge (`1/0`), and it closes the `daemon <-> watcher` cycle [measured].
- **Proposed change:** create `core/src/ingest/` (Runtime area) holding `apply` (renamed from
  `watcher::apply`), `staleness`, and the pure selection query `config_reindex::select_with` /
  `Selection` (the DB part only; the orchestration stays in `daemon::config_reindex`). `watcher/` keeps
  the FS half only. `daemon` imports from both. Move the wire-to-storage conversions
  (`to_storage_diff`, `to_node_record`, `to_edge_record`, `edge_kind_wire_value`) into
  `ingest::convert` so `bulk_index` and `apply` visibly share one mapping.
- **Benefit:** a change to watching (notify backend, debounce policy, Windows polling) and a change to
  how a plugin's diff is applied become independent. They are tested separately and the cycle goes away.
  The large `apply/tests.rs` (3058 lines) stops being "watcher tests". A future
  out-of-process or async ingest has one module to replace.
- **Effort:** M (moves plus the `select_with` extraction). **Risk:** low-medium. `pub(crate)` items
  (`linked_edges`, `sweep_semantic_edges`, `PathWarnings`) are used across daemon files, so visibility
  must be kept. `scripts/test-plugin-spawns.txt` may key on test paths (module-map 6.4), so check it.
- **Evidence:** `get_dependencies(g_mesh::watcher::apply, Incoming, 1)`: daemon/{semantic, workspace_reindex,
  lifecycle, bulk_index, plugin}.rs, watcher/staleness.rs, cli/plugin_check/session.rs and tests, not truncated.
  `get_dependencies(g_mesh::watcher::staleness, Incoming, 1)`: daemon/{workspace_reindex, build_stamp,
  lifecycle, bulk_index, plugin, registry}.rs, cli/status.rs, watcher/batch.rs [measured].

## A-opus-5. Give `mcp` a port onto the daemon instead of the concrete `PluginRegistry`

- **Location:** `mcp::serve_connection` (mcp/mod.rs:186), `GMeshMcpServer::new`,
  `daemon::registry::PluginRegistry::query_shapes` (registry.rs:960), `daemon::lifecycle::CoreActivity`.
- **Problem (missing seam, dependency direction):** `serve_connection` takes
  `Arc<PluginRegistry>` and `Arc<CoreActivity>` as concrete types [measured, signature]. Production `mcp`
  code calls 10 distinct registry methods: `project_root` (8 calls), `path_coverage` (4),
  `receiver_call_capabilities` (2), and once each `ensure_fresh`, `entry_points`, `has_pending`,
  `missing_languages`, `pending_summary`, `query_shapes`, `replay_pending` [measured, grep over
  non-test `mcp/*.rs`]. `PluginRegistry` has about 45 methods [measured]. The registry also points back the
  other way: `registry.rs:960-961` builds a `crate::mcp::query_shapes::QueryShapes`. That is a
  daemon type constructing a surface type, one half of the `daemon <-> mcp` production edges [measured].
  The `mcp` unit tests have to build a real `PluginRegistry` (`search_code_wait_tests`,
  `semantic_rung_worker_tests`, `instructions/tests` reference it) [measured, find_references].
- **Proposed change:** define in `mcp` a trait (for example `mcp::backend::ProjectBackend`) with the 10
  operations `mcp` uses, plus `touch()`/`guard()` for the `CoreActivity` connection accounting.
  `daemon` implements it for `PluginRegistry` (or a thin adapter struct that holds registry and
  activity). `serve_connection` takes `Arc<dyn ProjectBackend>`. `query_shapes` moves into `mcp`,
  computed from `DiscoveredPlugins` (A-opus-1) handed over by the backend, which removes
  `registry.rs`'s import of `mcp`.
- **Benefit:** the tool surface and the plugin runtime can change independently: a registry
  refactor (A-opus-6) does not ripple into 39 `mcp` files, and `mcp` tests can use a fake backend
  instead of spawning plugin infrastructure. Cost per tool test drops [inferred].
- **Effort:** M. **Risk:** medium. `ensure_fresh` and `replay_pending` take a store and block. The trait
  must stay synchronous ("Async stops here" per the mcp module doc) and object-safe. Wrapping in
  `dyn` costs one virtual call per tool call (negligible) [inferred].
- **Evidence:** `find_definition(mcp::serve_connection)` signature [measured].
  `find_references(daemon::registry::PluginRegistry, files)`: 14 files, 4 of them non-test `mcp` [measured].
  `find_references(daemon::lifecycle::CoreActivity, files)`: `mcp/mod.rs` (4), `mcp/front.rs` (3) [measured].

## A-opus-6. Break up `PluginRegistry` and gather plugin supervision in one submodule

- **Location:** `daemon/registry.rs:PluginRegistry` (impl spans lines 650-2031),
  `daemon/lifecycle.rs:PluginSupervisor` (lines 177-735), `daemon/plugin.rs:PluginProcess`,
  `daemon/memory.rs`, `daemon/registry.rs:{discovered_pid_files, discovered_suspended_markers,
  SupervisorSlot, SpawnReservation}`.
- **Problem (god object, cohesion):** `PluginRegistry`'s single impl block is about 1380 lines and about 45 methods
  [measured]. It mixes three responsibilities:
  1. immutable language routing derived from `DiscoveredPlugins`: `language_for`,
     `workspace_language_matches`, `watch_languages`, `has_manifest`, `entry_points`,
     `receiver_call_capabilities`, `semantic_pass_languages`, `query_shapes`, `path_coverage`;
  2. process pool management: `get_or_spawn`, `active_supervisors`, `sleep_if_idle_all`,
     `check_memory_limits_all`, `sleep_all_now`, `is_semantic_suspended`, failed-language bookkeeping;
  3. index-update operations: `file_changed`, `route_settled_path`, `announce_created`,
     `workspace_file_changed`, `gitignore_changed`, `recheck_gitignores_since_index`, `rescan_subtrees`,
     `reconcile_since_index` (128 lines), `replay_pending`, `ensure_fresh`.
  The plugin-process lifecycle is spread over four files: `PluginProcess` in `plugin.rs`,
  `PluginSupervisor` with its idle and dirty-queue logic in `lifecycle.rs` (a module whose doc is "the daemon's
  two idle timers"), slots and spawn reservations in `registry.rs`, and memory sampling in
  `memory.rs`. `PluginSupervisor` is referenced from `registry.rs` (10), `workspace_reindex.rs` (6),
  `config_reindex.rs` (2) [measured].
- **Proposed change:** create `daemon/plugins/` with `process.rs` (today's `plugin.rs`),
  `supervisor.rs` (`PluginSupervisor`, `DirtyQueue`, `AwakeHold` moved out of `lifecycle.rs`),
  `pool.rs` (slots, reservations, `get_or_spawn`, sleep/memory sweeps, pid/suspension-marker files),
  and `memory.rs`. Extract `LanguageMap` (responsibility 1) as an immutable value built once from
  `DiscoveredPlugins` and the project root. Move responsibility 3 into the `ingest` module from
  A-opus-4 as free functions over `(&LanguageMap, &PluginPool, &IndexStore)`. `lifecycle.rs` keeps
  `CoreActivity`, `orphan_check`, `supervise`, `Exit`, `purge_state_dir`.
- **Benefit:** routing rules (a manifest-driven concern, often changed with new languages) and
  process supervision (timeouts, memory caps, sleep/wake) change in different files with different
  tests. `LanguageMap` is what `mcp` actually needs (A-opus-5), and it carries no locks.
- **Effort:** L. **Risk:** medium-high. Supervisor slot locking (`SpawnReservation` drop semantics)
  and the sleep/replay ordering are concurrency-sensitive. Do it after A-opus-4/5, as pure moves first,
  then the `LanguageMap` extraction.
- **Evidence:** awk over `registry.rs` lines 650-2031 for `fn` declarations (about 45) and the struct fields
  (registry.rs:589-650) [measured]. `find_references(daemon::lifecycle::PluginSupervisor, files)` [measured].

## A-opus-7. Encapsulate the state-directory layout and liveness probes in one `StateDir` type

- **Location:** `daemon/mod.rs` lines 92-300 and 906-1036 (`endpoint`, `socket_path`, `pid_path(_in)`,
  `plugin_pid_path(_in)`, `phase_path_in`, `progress_path_in`, `build_stamp_path(_in)`,
  `lock_path(_under)`, `bootstrap_lock_path_in`, `daemon_lock_path(_in)`, `rebuild_marker_path_in`,
  `read_pid_file(_result)`, `write_pid_file`, `is_process_alive`, `inspect_daemon_lock`,
  `daemon_lock_is_held`, `serving_owner_in`); `cli/clean.rs:{singleton_lock_is_held, daemon_is_running}`;
  `daemon/registry.rs:{discovered_pid_files, plugin_pid_file_name, discovered_suspended_markers}`.
- **Problem (duplication, leaky abstraction):** the on-disk contract of a project's state dir is a set of
  about 25 free path functions in `daemon/mod.rs`, mostly in `(root)` and `(state_dir)` pairs, and
  every consumer recomposes them [measured]. The same probe exists twice with opposite failure
  policy: `daemon::daemon_lock_is_held` reads an unopenable lock file as **not held** ("eviction must fail
  towards leaving processes alone"), while `cli::clean::singleton_lock_is_held` reads it as **held**
  unless it is `NotFound` ("a lock that cannot be checked counts as held") [measured, both
  docs]. Both policies are right for their caller, but each module has to know the lock file's name
  and try-lock idiom. "Is anything running for this state dir" is put together by hand in
  `cli::clean::daemon_is_running` from `daemon::pid_path_in`, `registry::discovered_pid_files`,
  `daemon::read_pid_file_result` and `daemon::is_process_alive` [measured]. `daemon::is_process_alive` is a
  one-line wrapper over `process::is_alive` (mod.rs:294) [measured]. `daemon::endpoint` has 35
  references and `daemon::read_pid_file` 27, most of them in `core/tests/it/*` [measured].
- **Proposed change:** create `daemon/state_dir.rs` (or `control::state_dir` with A-opus-3) with
  `struct StateDir(PathBuf)` built from `root` or from a path. Give it methods for each file (`pid()`,
  `plugin_pid(lang)`, `phase()`, `progress()`, `build_stamp()`, `bootstrap_lock()`, `daemon_lock()`,
  `rebuild_marker()`, `endpoint()`) and for the probes: `lock_state(OnUnreadable::Held|Free)`,
  `running_pids()`, `serving_owner()`. Keep the free functions as thin deprecated forwards until
  tests migrate. Drop `daemon::is_process_alive` in favour of `process::is_alive`.
- **Benefit:** adding or renaming a state file, or changing a probe, becomes a one-file change, and
  both lock polarities are named at the call site instead of being implied by which module you
  call. The integration tests get one helper type instead of about 20 functions [inferred].
- **Effort:** M. **Risk:** low-medium. The behaviour must stay identical; the two polarities need to
  stay distinct and tested.
- **Evidence:** `find_references(daemon::pid_path_in | read_pid_file | endpoint | is_process_alive, files)`:
  14 / 27 / 35 / 23 references, across cli, daemon and about 25 integration-test files [measured].
  Read `daemon/mod.rs:959-1002` and `cli/clean.rs:510-521, 707-722` [measured].

## A-opus-8. Make `protocol` framing-only: drop the conformance validator's graph dependency and the re-export used by the data layer

- **Location:** `protocol/conformance.rs` (imports at lines 7-9), `protocol/types.rs`
  (`pub use g_mesh_wire::*`), the users in `storage/qualified_path.rs:11`, `storage/write.rs:7`,
  `graph/symbol_links.rs:298`.
- **Problem (dependency direction, cohesion):** `protocol` contains three things: framing
  (`jsonrpc`, `ndjson`, `ndjson_frame`, `handshake`), a re-export of the `g-mesh-wire` crate (`types`),
  and a semantic validator (`conformance`). `conformance` imports `graph::containers`,
  `graph::imports` and `graph::symbol_links` constants [measured], which is the `protocol -> graph` edge
  (`3/0`). `graph` and `storage` import `crate::protocol::types::QualifiedPath`, which is the
  `graph -> protocol` / `storage -> protocol` edges [measured]. Together they form the
  `graph <-> protocol` cycle, and the Data area depends on the Runtime area for a type that really comes from an
  external crate. `conformance`'s only production user is `cli/plugin_check/checks.rs:149`
  [measured, `get_dependencies` + grep].
- **Proposed change:** (a) data-layer modules (`graph`, `storage`) `use g_mesh_wire::QualifiedPath`
  directly, and `protocol::types` stays as a convenience for runtime code. (b) Move
  `protocol/conformance.rs` to `cli/plugin_check/conformance.rs`. Alternatively, move the reserved
  native-kind constants it needs into `g-mesh-wire` (they are part of the plugin contract: kinds a plugin must
  not emit), so the validator is pure wire logic.
- **Benefit:** framing changes and graph schema changes become independent, the
  `graph <-> protocol` cycle goes away, and the data layer no longer depends on Runtime for wire types.
- **Effort:** S. **Risk:** low (moves and import rewrites; the `protocol_conformance` integration test
  path changes).
- **Evidence:** `get_dependencies(g_mesh::protocol::conformance, Incoming, 1)`: only
  `cli/plugin_check/checks.rs` and `tests/it/protocol_conformance.rs` [measured].
  `grep crate:: protocol/*.rs`: only the conformance->graph imports leave `protocol` [measured].

## A-opus-9. Move the front-mode wire vocabulary out of `mcp::front` into a shared contract module

- **Location:** `shim/router.rs:129` `use crate::mcp::front::{PROJECT_ARG, SELECT_PROJECT, SWITCH_PROJECT_META}`.
- **Problem (dependency direction):** the shim router is a byte-level proxy in a separate process,
  and it is otherwise well isolated: it reaches daemons only through the injected `Daemons` closures
  (router.rs:142-157) [measured]. Its one reach into the daemon-side tool server is for three string
  constants that define the protocol between the front daemon and the router. This is the `shim -> mcp` edge
  (`1/0`) [measured]. Any edit in `mcp::front` (tool schemas, instructions) is, at module level, an edit
  the shim depends on.
- **Proposed change:** move the three constants (and the shape of `_meta["g-mesh/switchProject"]`, if it
  is parsed on both sides) to `protocol::front` (or `protocol::switch`). `mcp::front` and `shim::router`
  both import from there.
- **Benefit:** the router and the front's tool surface can change independently. The contract between
  them is named and in one place.
- **Effort:** S. **Risk:** very low.
- **Evidence:** grep for `crate::mcp` in `shim/` [measured]. Read `router.rs:120-160` [measured].

## A-opus-10. Gather runtime knobs (env-driven timeouts, paths, test hooks) into one typed module

- **Location:** `daemon/lifecycle.rs:{PLUGIN_IDLE_ENV, CORE_IDLE_ENV, LIFELINE_PID_ENV, parse_timeout}`,
  `daemon/plugin.rs:{FILE_CHANGED_TIMEOUT_ENV, SEMANTIC_PASS_*_ENV, parse_round_trip_timeout}`,
  `daemon/front.rs:52 (FRONT_IDLE_ENV)`, `daemon/seed.rs:{SEED_ENV, HOLD_RECONCILE_FILE_ENV}`,
  `daemon/bulk_index.rs:{ABSENT_COUNT_OFF_ENV, WALK_HOLD_FILE_ENV, WALK_DELAY_ENV}`,
  `daemon/manifest.rs:59 (PLUGIN_ROOTS_OVERRIDE_ENV)`, `daemon/memory.rs:446`,
  `shim.rs:{PROJECT_DIR_ENV, BOOTSTRAP_TIMEOUT_ENV, DAEMON_LOG_ENV}`, `watcher/apply.rs:1146 (HOLD_COMPUTE_FILE_ENV)`.
- **Problem (config sprawl, duplication):** 19 `std::env::var*` read sites across 9 Runtime files
  [measured, grep excluding tests]. They mix production knobs (idle timers, round-trip timeouts) with
  test-only hooks (`*_HOLD_*_FILE`, `WALK_DELAY`), and the hooks run inside hot production functions
  (`apply.rs:hold_compute_open_for_tests`). There are two near-identical millisecond parsers with
  different meanings for `0`: `lifecycle::parse_timeout` maps 0 to "off" (`None`), and
  `plugin::parse_round_trip_timeout` maps 0 to the default. Each has its own copy of the log line [measured].
  `config` (`config::read_project_config`) handles file-based settings only, so there is no single
  place that lists what can be tuned [inferred].
- **Proposed change:** add `core/src/runtime_env.rs` with one `Knob` table (name, default, parse
  rule: `ZeroMeansOff | ZeroMeansDefault`) and a typed `RuntimeKnobs` read once in `daemon::run` and
  `shim::run` and passed down. Put the test hooks in `runtime_env::test_hooks` with one
  `hold_while_file_exists(env)` helper used by `apply`, `seed` and `bulk_index`.
- **Benefit:** adding or retiring a knob is one table row, `g-mesh status` can list effective values,
  and the zero semantics are explicit. Test-hook plumbing stops being copied into each subsystem.
- **Effort:** S-M. **Risk:** low. Integration tests set these variables on child processes, so names
  must not change.
- **Evidence:** the grep listed above. Read `lifecycle.rs:125-145` and `plugin.rs:255-275` [measured].

## A-opus-11. Thin `daemon::run` into named startup phases

- **Location:** `daemon/mod.rs:run` (lines 308-605, 294 lines; the largest function in core per module-map 7.3).
- **Problem (god function, testability):** `run` is the composition root. Composition roots are
  legitimately wide, but this one also contains the logic itself. `find_callees(daemon::run)` returns about 60
  callees across `daemon::{activation, build_stamp, candidates, front, indexing_status, lifecycle,
  manifest, registry, seed, workspace_reindex}`, `storage::{connection, index_store, schema}` (7 schema
  functions), `embedding`, `gc`, `ipc`, `watcher`, `config` [measured]. Inline in it are: singleton/stand-down
  (315-317), front-mode branching (339), leftover cleanup (`clear_stale_rebuild_marker`,
  `workspace_reindex::remove_stale_*`, `seed::remove_stale_seed_tmp`,
  `registry::clear_stale_suspension_markers`: 322, 350, 378, 434), schema reset and cold-cause
  classification (361-378), bind (431-437), service construction (473-482), the accept thread (508),
  the seed gate and watcher (530-538), activation (561), supervise and purge (583-599) [measured, grep of line numbers].
- **Proposed change:** split into `fn recover_leftovers(&StateDir)`, `fn open_index(...) -> (IndexStore,
  ColdCause)`, `fn build_services(...) -> Services { registry, embedding, indexing }`,
  `fn serve(Services, Listener)`, and `fn wait_for_exit(...) -> Exit`. `run` then reads as five lines.
  `recover_leftovers` and `open_index` become unit-testable without a socket.
- **Benefit:** adding a startup step (a new leftover file, a new migration check) has an obvious home,
  and ordering constraints get names. Reviews of startup changes stop needing the whole 300-line context [inferred].
- **Effort:** M. **Risk:** medium. Startup ordering is load-bearing (for example "Read before
  `ensure_current`" at 361). The extraction must keep the order and the comments.
- **Evidence:** `find_callees(daemon::run, limit 100)`, not truncated. Grep of the call lines inside 308-606 [measured].

---

## Considered and rejected

- `ipc` (unix/windows behind one `Endpoint`/`Stream`/`Listener` name): already a clean per-platform seam with zero fan-out into core.
- `process.rs`: one module that names POSIX signal operations once per platform. It is the right seam; its only wart (the `daemon::is_process_alive` wrapper) is folded into A-opus-7.
- `shim::router`'s coupling to daemons: reached only through injected `Daemons { connect, rebuilding, is_folder }` closures, which is a good seam. Only the constants edge (A-opus-9) remains.
- `daemon -> mcp` (`serve_forever` calls `mcp::serve_connection`, `front.rs` calls `mcp::front`): the composition root hosting the server is the correct direction. The problem is the reverse edge (A-opus-5).
- `daemon/semantic.rs:786` calling `cli::status`: inside `#[cfg(test)]`, so not a production dependency.
- `gc/` (`last_used`, `warning`): small, depends only on `config` and `storage`. Its `daemon` references are test-fixture only.
- `config_reindex` vs `workspace_reindex` as two modules: they serve distinct manifest capabilities (`resolution_delta` vs whole-language reindex). Merging would reduce cohesion. Only `select_with` should move (A-opus-4).
- `daemon -> storage` (29 use sites): the runtime consumes the data layer, which is the right direction. Only the reverse `identity` edge was wrong (A-opus-2).
- `protocol::types` as a blanket `pub use g_mesh_wire::*`: fine as a runtime convenience. The issue is only data-layer modules routing through it (A-opus-8).

## g-mesh calls used

select_project(g-mesh); get_file_outline(cli/clean.rs); find_references(purge_orphaned_state_dir → semantic-neighbour fallback, then by id);
find_callers ×6 (purge_orphaned_state_dir, sweep_orphaned_if_due, cli::stop::stop, cli::stop::terminate_daemon, shim::connect_or_bootstrap);
find_references(files) ×9 (pid_path_in, read_pid_file, endpoint, is_process_alive, project_hash, IndexingStatus, PluginRegistry, PluginSupervisor, CoreActivity);
get_dependencies(Incoming) ×4 (daemon::manifest, watcher::apply, watcher::staleness, protocol::conformance); find_callees(daemon::run); find_definition(mcp::serve_connection).
One call fell back: find_references on the path-qualified `crate::cli::clean::purge_orphaned_state_dir` returned `resolvedBy: semanticNeighbours`. It was re-queried by id and resolved. No call failed or was slow.
