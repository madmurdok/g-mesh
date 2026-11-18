# Area A (Runtime) architecture review

Model: claude-fable-5-2

Scope: `core/src/{daemon,ipc,protocol,shim,shim.rs,watcher,gc,process.rs}` at
g-mesh 4.30. Input: `module-map.md`, then code. No builds or tests were run.

Evidence labels: **measured** = seen in code, grep output or a g-mesh result
during this review; **inferred** = a conclusion drawn from those, not checked
by building or running anything. Every "what becomes cheaper" statement is
inferred. Line numbers are 1-based, from the worktree.

Summary of the shape: `daemon` is in the 13-module strongly connected component
mostly because it *houses* four dependency-free leaf modules (plugin manifest,
project identity, state-directory layout, indexing status) that everything else
needs, and because two runtime operations (purge, stop) live in `cli`. Moving
those is mechanical and removes most of the cycle edges that touch Area A
(findings 1-4). The remaining findings are about three oversized units
(`watcher::apply`, `PluginRegistry`, `daemon::run`) whose internal roles change
for different reasons.

Cycle edges touching Area A, and the finding that removes each:

| edge (map section 5) | cause (measured) | finding |
|---|---|---|
| languages -> daemon | `languages.rs:42` uses `daemon::manifest` | 1 |
| storage -> daemon | `storage/connection.rs:7,206` uses `daemon::identity` | 2 |
| embedding -> daemon | `embedding/backfill.rs:102` uses `daemon::indexing_status` | 3 |
| daemon -> cli | `daemon/lifecycle.rs:1015-1022` uses `cli::clean` | 4 |
| shim -> cli | `shim.rs:15,152` uses `cli::stop`, `cli::clean` | 4 |
| watcher -> daemon | `watcher/apply.rs:422` uses `daemon::config_reindex` | 5 |
| project_walk -> watcher | `project_walk.rs:31` uses `watcher::BASELINE_EXCLUDED_DIRS` | 6 |
| protocol -> graph | `protocol/conformance.rs:7-9` | 10 |
| daemon -> mcp (registry part) | `daemon/registry.rs:960-961` | 7 |

---

## A-fable-1. The plugin manifest model lives inside `daemon` but is a crate-wide leaf

- **Location:** `core/src/daemon/manifest.rs` (+ `manifest/tests.rs`);
  `core/src/languages.rs:42`.
- **Problem (dependency direction, cohesion):** `manifest` reads and validates
  `plugin.toml` and discovers plugins. It needs nothing from the daemon, yet
  every consumer has to depend on `crate::daemon` to reach it. That one
  placement creates the `languages <-> daemon` cycle and accounts for the
  largest share of `mcp -> daemon` and `cli -> daemon` references.
- **Proposed change:** move `daemon/manifest.rs` and `daemon/manifest/` to a
  top-level `core/src/plugin_manifest.rs` (or `plugins/manifest.rs` if finding
  8's `plugin_host` is adopted). Update `use` paths; optionally leave
  `pub use crate::plugin_manifest as manifest;` in `daemon/mod.rs` for one
  release to keep the diff small. No behaviour change.
- **Benefit:** a manifest schema change (new capability, new `[plugin.*]`
  table) stops being a "daemon" change; `languages`, `mcp` query shaping and
  `cli plugins/plugin_install/plugin_check` depend on the schema only. Removes
  `languages -> daemon`. Prerequisite for ever extracting the manifest model
  into its own crate for plugin authors.
- **Effort:** S (path sweep over ~60 files, no logic). **Risk:** low; merge
  conflicts with in-flight branches that touch the same `use` lines; doc links
  `crate::daemon::manifest::...` in comments must be swept too.
- **Evidence:**
  - measured: non-test code of `daemon/manifest.rs` references only
    `crate::graph::symbol_links` and `crate::protocol::types` (grep of
    `crate::`/`super::` paths before the first `#[cfg(test)]`).
  - measured: g-mesh `get_dependencies(core/src/daemon/manifest.rs, Incoming,
    depth 1)` returned 61 files, no truncation flag: 20 non-test files outside
    `daemon` (`languages.rs`, 9 in `mcp`, 10 in `cli` incl. `plugin_check/*`),
    the rest daemon files and tests.
  - measured: in `mcp` + `cli`, `crate::daemon::manifest::*` is the most
    frequent `crate::daemon::` path (grep count: `MemberOverrides` 13,
    `Capabilities` 12, `read_manifest` 7, `bare_manifest` 5,
    `DiscoveredPlugins` 6, ...). Test files are included in that count.

## A-fable-2. State-directory layout and project identity are trapped in `daemon/mod.rs` next to `run`

- **Location:** `core/src/daemon/mod.rs:51-306` and `:897-1036` (file-name
  constants, `*_path`/`*_path_in`, pid-file read/write, rebuild marker,
  singleton/daemon lock inspection, serving owner);
  `core/src/daemon/identity.rs`; `core/src/storage/connection.rs:7,206`;
  stragglers `shim.rs:83` (`DAEMON_LOG_FILE`), `daemon/registry.rs:117-224`
  (plugin pid files, suspension markers), `daemon/workspace_reindex.rs:101-102`
  (staging files), `cli/clean.rs:567` (`orphan-sweep.stamp`).
- **Problem (god module, dependency direction):** `daemon/mod.rs` is two
  things: the on-disk contract of `~/.g-mesh/projects/<hash>/` that the shim,
  the CLI, `gc` and `storage` all read, and the daemon process itself (`run`,
  the watch loop, the accept loop). A process that only wants "where is the pid
  file" or "what is this root's hash" depends on the module that owns the
  daemon's main loop. `storage::connection` needs `project_hash`, which gives
  `storage <-> daemon`.
- **Proposed change:** new leaf module `core/src/state_dir.rs` (or
  `daemon/state_dir.rs` re-exported, as a first step) holding: the file-name
  constants, the `*_path_in` functions, `read_pid_file*`/`write_pid_file`/
  `write_state_file_atomic`, `Rebuild` + marker functions, `DaemonLock` +
  `inspect_daemon_lock`, `acquire_singleton_lock`, `serving_owner_in`, and
  `identity` (`project_hash`, root/device records). Pull the stragglers'
  file names in as well so one file lists everything that may exist in a state
  directory. `daemon/mod.rs` keeps `run`, `stand_down`, `serve_forever`.
  `storage::connection` imports `state_dir::identity`.
- **Benefit:** a change to the daemon's start-up or loops no longer touches the
  module the shim/CLI/storage compile against, and a layout change (new state
  file) has one home, including its release in
  `lifecycle::release_state_files` and its treatment by `cli::clean`. Removes
  `storage -> daemon`. Makes finding 4 possible without a new cycle.
- **Effort:** M (many call sites: `cli -> daemon` is 27 use / 8 inline sites
  per the map, most of them for these functions - the "most" is inferred, not
  counted). **Risk:** low-medium; the ordering invariants documented around
  `write_pid_file`/`release_state_files` (pid file removed last, endpoint
  last) must travel with the functions' comments.
- **Evidence:**
  - measured: `daemon/identity.rs` has no `crate::`/`super::` reference in
    non-test code.
  - measured: g-mesh `get_dependencies(g_mesh::daemon::identity, Incoming, 1)`:
    `storage/connection.rs`, `cli/clean.rs`, `shim.rs` + 4 integration tests.
  - measured: `daemon/mod.rs` outline - lines 51-306 are constants and path/pid
    helpers, `run` starts at 308, lock inspection at 897-1036.
  - measured: `lifecycle.rs:1046-1063` `release_state_files` reaches back to
    `super::*_path_in` six times and `super::registry::discovered_pid_files`.

## A-fable-3. `IndexingStatus` is shared progress state, not a daemon internal

- **Location:** `core/src/daemon/indexing_status.rs`;
  `core/src/embedding/backfill.rs:102`.
- **Problem (dependency direction):** `IndexingStatus`/`Phase`/`ColdCause`/
  `ProgressSnapshot` are read by `mcp` (to decide whether a tool call may
  answer), by `cli status/init/reindex/deps`, and written by
  `embedding::backfill`. Living in `daemon`, it produces `embedding -> daemon`
  and part of `mcp -> daemon`.
- **Proposed change:** move to a top-level `core/src/indexing_status.rs`. Its
  only upward need, `super::write_state_file_atomic` (2 sites), comes from
  `state_dir` after finding 2 (or is passed in as the attached file writer).
- **Benefit:** the readiness/phase vocabulary can change without recompiling
  conceptually "daemon" consumers, and `embedding` no longer depends on the
  runtime. Removes `embedding -> daemon`.
- **Effort:** S. **Risk:** low.
- **Evidence:**
  - measured: non-test code of `indexing_status.rs` references only
    `crate::log_line` and `super::write_state_file_atomic` (x2).
  - measured: g-mesh `get_dependencies(g_mesh::daemon::indexing_status,
    Incoming, 1)`: 20 files, among them `embedding/backfill.rs`,
    `mcp/{mod,instructions,search_code}.rs`, `cli/{init,reindex,status,deps}.rs`.

## A-fable-4. Runtime operations (purge orphaned state, stop a daemon) are implemented in `cli`, and the runtime calls up into it

- **Location:** `core/src/cli/clean.rs:257-624` (`RootState`, `root_state`,
  `Purge`, `LockWait`, `purge_orphaned_state_dir`, `sweep_orphaned*`,
  `ORPHAN_SWEEP_INTERVAL`); `core/src/cli/stop.rs:128-320` (`stop`,
  `terminate_daemon`, `tidy_state_files`, `TERMINATION_TIMEOUT`); callers
  `core/src/daemon/lifecycle.rs:1008-1035` (`purge_state_dir`),
  `core/src/shim.rs:147-163` (`spawn_orphan_sweep`), `shim.rs:378`
  (`retire_outdated_daemon`), `shim.rs:458` (`evict_wedged_daemon`).
- **Problem (dependency direction, leaky layering):** the daemon and the shim
  are long-lived processes below the CLI surface, but both import
  `crate::cli` for behaviour they run unattended. This is the whole of
  `daemon -> cli` and `shim -> cli`, i.e. the reason `cli` is in the SCC from
  the Area A side. A change to a CLI command's file (argument handling,
  rendering) sits in the same file as the rule "when may a state directory be
  deleted", which three processes rely on.
- **Proposed change:** move the purge/sweep core into `gc` (it already owns
  "which project indexes are still in use"): `gc/orphaned.rs` with
  `root_state`, `purge_orphaned_state_dir`, `sweep_orphaned_if_due`.
  Move `stop`/`terminate_daemon`/`tidy_state_files` to a runtime module, e.g.
  `daemon/control.rs` (it uses only `daemon` paths, `process`, and
  `storage::connection::project_dir`). `cli::clean` and `cli::stop` keep
  argument parsing, `run`, `render`, and call down.
- **Benefit:** CLI commands become thin and can change freely; the
  deletion/termination rules get one owner in the runtime with their tests.
  Removes `daemon -> cli` and `shim -> cli`; after it, Area A has no edge into
  `cli` at all.
- **Effort:** M (`cli/clean.rs` is 1925 lines with a large inline test block
  that has to be split along the same line). **Risk:** medium; these functions
  encode lock-order and "delete the root record last" invariants - move, do not
  rewrite.
- **Evidence:**
  - measured: `daemon/lifecycle.rs:1015-1022` - four inline `crate::cli::clean::`
    paths (matches the map's `daemon -> cli 0/4`).
  - measured: `shim.rs:15` `use crate::cli::stop;`, `shim.rs:152`
    `use crate::cli::clean::{sweep_orphaned_if_due, Sweep, ORPHAN_SWEEP_INTERVAL};`.
  - measured: g-mesh `find_callers(cli::stop::stop)`: `cli::stop::run`,
    `cli::reindex::stop_for_rebuild`, `shim::retire_outdated_daemon`.
  - measured: g-mesh `find_callers(purge_orphaned_state_dir, files)`:
    `cli/clean.rs` 6, `daemon/lifecycle.rs` 1.
  - measured: `cli/stop.rs` non-test imports are `crate::daemon`,
    `crate::process`, `crate::storage::connection::project_dir` only.
  - not checked: whether `cli/clean.rs`'s purge functions use `CleanArgs` or
    other CLI-only types internally (their signatures do not).

## A-fable-5. `watcher::apply` is the plugin-diff application pipeline, not part of file watching

- **Location:** `core/src/watcher/apply.rs` (1419 lines, + 3058-line
  `apply/tests.rs`): `apply_file_change_in:234`, `reextract_affected:406`,
  `apply_semantic_pass_in:637`, `round_trip:992`, wire->storage conversion
  `to_storage_diff:1186`..`edge_source_tier_wire_value:1411`;
  `core/src/watcher/staleness.rs` (`ensure_fresh:155`, walk baselines);
  `core/src/daemon/plugin.rs:1229-1524` (callers).
- **Problem (cohesion, misplaced boundary, cycle):** `watcher/` contains two
  unrelated subsystems. (a) Filesystem events: `ProjectWatcher`, `debounce`,
  `burst`, `batch`, `rescan`, `ignore_layers` - no plugin or storage-write
  knowledge. (b) "Send a request to a plugin, read its diff, convert wire
  records to storage records, write, embed, link": `apply` and `staleness`.
  (b) is called by the plugin host (`daemon::plugin`), by cold indexing and by
  `cli plugin_check`, none of which involve a watcher event. (b) also calls
  back up into `daemon::config_reindex`, which is the `watcher -> daemon`
  cycle edge. Inside `apply.rs`, three roles share one file: the round-trip
  protocol, the wire->storage mapping (pure), and semantic-pass orchestration.
- **Proposed change:**
  1. Move `watcher/apply*` and `watcher/staleness.rs` out of `watcher` into a
     module named for what it does, e.g. `core/src/index_apply/`
     (`file_change.rs`, `semantic_pass.rs`, `round_trip.rs`, `staleness.rs`).
  2. Split the pure mapping (`to_storage_diff`, `to_node_record`,
     `to_edge_record`, `PathWarnings`, `checked_paths`, visibility/kind
     helpers, lines 1186-1416) into `index_apply/convert.rs` with no I/O.
  3. Cut the upward call: `reextract_affected` takes the selection as a
     parameter (`&dyn Fn(&Connection, ...) -> Result<Selection>`), supplied by
     `daemon::plugin`, or `config_reindex::select_with`/`Selection` move down
     next to it (they take a `Connection` and wire selectors; whether
     `select_with` has other daemon-only dependencies was not read).
- **Benefit:** the watcher (notify backend, debounce policy, ignore rules)
  becomes replaceable and testable without the plugin protocol; a wire-format
  change touches `convert.rs` and its tests rather than a 1400-line file with
  a 3000-line test file; `watcher` leaves the cycle with `daemon`
  (`daemon -> watcher` remains, one-way).
- **Effort:** M (moves + one signature change; the test file moves whole).
  **Risk:** medium-low; `Unit::WatcherApply` and timing-log names refer to the
  old placement and are observable in logs - keep the strings.
- **Evidence:**
  - measured: `watcher/apply.rs:26-36` imports `embedding`, `protocol::jsonrpc`,
    `protocol::types`, `storage::{file_rows,index_store,schema,write}`; none of
    `watcher::{debounce,burst,batch,rescan,ignore_layers}`.
  - measured: `watcher/apply.rs:422`
    `use crate::daemon::config_reindex::{select_with, Selection};` inside
    `reextract_affected`.
  - measured: g-mesh `get_dependencies(core/src/watcher/apply.rs, Incoming, 1)`
    (`truncated: false`): `daemon/{plugin,lifecycle,semantic,bulk_index,
    workspace_reindex}.rs`, `watcher/staleness.rs`,
    `cli/plugin_check/session.rs`, 6 integration tests. No importer is a
    watcher-event file.
  - measured: g-mesh `find_callers(apply_semantic_pass, files)`:
    `daemon/plugin.rs` 1, `cli/plugin_check/session.rs` 1, tests 13.
  - measured: `watcher/staleness.rs:1` module doc "Query-time staleness check
    with synchronous fallback reindex" (map, section 2) and its imports
    (`:77-81`): embedding, protocol, storage, `watcher::apply`.
  - inferred: that `watcher/mod.rs` itself has no dependency on `apply`/
    `staleness` beyond declaring them (its only `crate::` import seen is
    `project_walk::Remap`, line 17).

## A-fable-6. The baseline exclusion list is defined twice and in the wrong module

- **Location:** `core/src/watcher/mod.rs:34`
  (`pub const BASELINE_EXCLUDED_DIRS`), `plugins/sdk/src/walk.rs:50` (same
  constant), `core/src/project_walk.rs:31`.
- **Problem (duplication, dependency direction):** two definitions of one
  rule, kept equal by a comment ("The same pair as `BASELINE_EXCLUDED_DIRS` in
  plugins/sdk/src/walk.rs"). Core's walk imports it from `watcher`, while
  `watcher` imports `project_walk::Remap`: the `project_walk <-> watcher`
  cycle. If the two lists diverge, the watcher/index walk and the plugins'
  bulk walk disagree on which files exist (structural correctness risk).
- **Proposed change:** define the constant once in the `g-mesh-walk` crate,
  which both `core` and `plugins/sdk` already depend on; `watcher`,
  `project_walk`, `cli::status` and the SDK import it from there.
- **Benefit:** adding an always-excluded directory is one edit;
  `project_walk -> watcher` disappears.
- **Effort:** S. **Risk:** low.
- **Evidence:**
  - measured: both definitions are `[".git", ".claude"]` (grep).
  - measured: `walk/src/lib.rs` has no `EXCLUDED` constant today (grep, no
    match).
  - measured: map section 1 - `g-mesh` and `g-mesh-plugin-sdk` both list
    `g-mesh-walk (normal)`.

## A-fable-7. `PluginRegistry` is one type with five roles and a ~1380-line `impl`

- **Location:** `core/src/daemon/registry.rs:589-2029` (`struct PluginRegistry`
  and its `impl`, 48 methods).
- **Problem (god object, cohesion, missing seam):** the methods fall into
  groups that change for different reasons:
  1. *Static routing from manifests* (no process needed): `language_for:787`,
     `path_coverage:758`, `workspace_language_matches:824`,
     `watch_languages:850`, `has_manifest:885`, `entry_points:917`,
     `receiver_call_capabilities:947`, `missing_languages:954`,
     `query_shapes:960`, `semantic_pass_languages:985`.
  2. *Supervisor pool*: `get_or_spawn:1036`, `active_supervisors:1806`,
     `sleep_if_idle_all:1816`, `check_memory_limits_all:1839`,
     `sleep_all_now:1881`, failed-language bookkeeping `705-743`.
  3. *Event routing*: `route_settled_path:1099`, `announce_created:1128`,
     `file_changed:1185`, `workspace_file_changed:1241`.
  4. *Reconciliation with disk*: `gitignore_changed:1289`,
     `recheck_gitignores_since_index:1347`, `rescan_subtrees:1402`,
     `reconcile_since_index:1458` (128 lines), `retire_file:1595`,
     `reconcile_watch_files_with_seed:1623`, `owed_semantic_file_passes:1690`.
  5. *Query-time freshness*: `has_pending:1912`, `pending_summary:1927`,
     `replay_pending:1951`, `ensure_fresh:1989`.
  `mcp` needs groups 1 and 5 only but receives the whole `Arc<PluginRegistry>`;
  group 1 also forces a `daemon -> mcp` edge (`query_shapes` builds an `mcp`
  type).
- **Proposed change:** (a) extract group 1 into a `LanguageRouter` value built
  from `DiscoveredPlugins` + project root (pure, unit-testable without
  spawning), owned by the registry and handed to `mcp` by reference/`Arc`;
  `mcp` builds `QueryShapes::from_manifests(router.manifests())` itself, which
  deletes `registry.rs:960-961`. (b) move group 4 to `daemon/reconcile.rs` as
  a second `impl PluginRegistry` block or free functions over
  `(&PluginRegistry, &IndexStore)` - a file split first, no API change.
  (c) give `mcp` a narrow trait for group 5 (`Freshness`: `has_pending`,
  `pending_summary`, `replay_pending`, `ensure_fresh`) so tool handlers can be
  tested against a stub instead of a real registry with spawned plugins.
- **Benefit:** routing rules (which language owns a path) and reconcile policy
  can be changed and tested separately from process supervision; `mcp` tests
  need less daemon machinery (the map shows `mcp -> daemon` test sites 43/32,
  more than the non-test 23/1); removes one `daemon -> mcp` reference.
- **Effort:** (a) M, (b) S, (c) M. **Risk:** medium for (a)/(c) - lock order
  and "spawn at most once per language" (`SpawnReservation`) must stay inside
  the pool; (b) is low.
- **Evidence:**
  - measured: method list with line numbers from an `awk`/`grep` outline of
    `registry.rs:650-2031`; struct fields at `:592-628` (`project_root`,
    `state_dir`, `discovered`, `idle_timeout`, `memory_limit_mb`, `embedding`).
  - measured: registry methods called from `mcp` (grep `registry\.<name>(`,
    test files included): `project_root` 8, `path_coverage` 4,
    `receiver_call_capabilities` 2, `replay_pending`, `query_shapes`,
    `pending_summary`, `missing_languages`, `has_pending`, `entry_points`,
    `ensure_fresh` 1 each. Receivers named other than `registry` are not
    counted, so this is a lower bound.
  - measured: `daemon/registry.rs:960-961` returns
    `crate::mcp::query_shapes::QueryShapes`.
  - inferred: the five-way grouping (from names, signatures and doc position;
    method bodies other than `reconcile_since_index`'s size were not read).

## A-fable-8. `daemon/lifecycle.rs` holds two unrelated state machines; `PluginSupervisor` belongs with the plugin host

- **Location:** `core/src/daemon/lifecycle.rs:150-733` (`DirtyQueue`,
  `SupervisedPlugin`, `PluginSupervisor`, `AwakeHold`) versus `:736-1063`
  (`CoreActivity`, `Orphaned`, `orphan_check`, `supervise`, `Exit`,
  `end_process_group`, `purge_state_dir`, `release_state_files`).
- **Problem (cohesion, intra-module cycle):** the file's doc says "the
  daemon's two idle timers". The two halves share only `IdleTimeouts`.
  `PluginSupervisor` is the per-language plugin wrapper (dirty queue, replay,
  memory limit, sleep, suspended marker); it is what `registry` stores and
  what `plugin::PluginProcess` is wrapped by, giving
  `registry -> lifecycle -> registry` (pid/marker file names) and
  `lifecycle -> plugin -> registry` references. The second half is the core
  process's own shutdown logic.
- **Proposed change:** group the plugin host: `daemon/plugin_host/{process.rs
  (today plugin.rs), supervisor.rs (PluginSupervisor), registry.rs, memory.rs}`
  with pid/marker file names owned there (or by `state_dir`, finding 2).
  `lifecycle.rs` keeps `IdleTimeouts`, `CoreActivity`, orphan check,
  `supervise`, exit/teardown. `mcp` then imports `CoreActivity` from a small
  file rather than one that also defines plugin supervision.
- **Benefit:** plugin supervision policy (sleep, replay, memory limit) and core
  lifetime policy (idle exit, orphan detection, teardown order) are edited and
  reviewed separately; the plugin host becomes a unit with one outward face,
  the precondition for testing or replacing it independently.
- **Effort:** S-M (file moves, no logic). **Risk:** low; `lifecycle/tests.rs`
  (1086 lines) has to be split along the same line.
- **Evidence:**
  - measured: outline of `lifecycle.rs` (grep of items, lines 46-1063).
  - measured: `registry.rs:100` `use crate::daemon::lifecycle::PluginSupervisor;`
    and `lifecycle.rs` references `super::registry::discovered_pid_files`,
    `super::registry::plugin_suspended_marker_file_name`,
    `crate::daemon::plugin::PluginProcess`; `plugin.rs:374` calls
    `crate::daemon::registry::plugins_digest`.
  - measured: `mcp` + `cli` reference `crate::daemon::lifecycle::CoreActivity`
    9 times and `lifecycle::PluginSupervisor` once (grep, tests included).

## A-fable-9. `daemon::run` (294 lines) fuses the start-up decision, wiring and the watch loop

- **Location:** `core/src/daemon/mod.rs:308-604` (`run`), `:643-703`
  (`spawn_watch_consumer`), `:763-834` (`watch_and_route_once`), `:705-735`
  (`Rescans`).
- **Problem (god function, testability):** `run` does, in order: singleton
  lock, build stamp, multi-project detection, plugin discovery, worktree
  seeding, schema check, and then computes six interdependent start-up facts
  (`cold_cause`, `needs_bulk_index`, `needs_semantic_pass_retry`,
  `retry_languages`, `seeded`, initial `IndexingStatus`) inline, before
  binding, loading config/embedding, and wiring four threads. The
  start-up decision ("given this index state, what does the daemon owe before
  it is ready") is pure given a connection, but can only be exercised by
  starting a daemon. The watch consumer - the daemon's steady-state loop -
  sits in `mod.rs` beside path helpers.
- **Proposed change:** (a) extract
  `fn startup_plan(conn, &discovered) -> Result<StartupPlan>` returning a
  struct with those six facts (lines ~346-395 and 419-425 of `run`), unit
  tested against fixture databases; `ActivationCtx` then takes the plan
  instead of five loose fields. (b) move `spawn_watch_consumer`,
  `watch_and_route_once`, `Rescans`, `ConsumerStart`, `timing_path`,
  `relative_wire_path`, `DEBOUNCE_WINDOW` to `daemon/watch_loop.rs`. With
  finding 2, `mod.rs` shrinks to `run` + `serve_forever` + `stand_down`.
- **Benefit:** a new start-up state (as `Seeded` and "failed-language retry"
  were) is added in one tested function instead of threaded through `run`;
  watch-loop changes stop conflicting with start-up changes in one file.
- **Effort:** (a) M, (b) S. **Risk:** (a) medium - the order of side effects in
  `run` is deliberate (bind before pid file, `record_serving_owner` after the
  phase file is attached); only the read-only decisions may move. (b) low.
- **Evidence:**
  - measured: read of `run` (comment lines stripped); the six facts are
    computed at the lines given and five of them are passed on as separate
    `ActivationCtx` fields (`needs_walk`, `needs_semantic_pass_retry`,
    `retry_languages`, `seed_gate`, `seed_reconciled`).
  - measured: map section 7.3 lists `run` as the largest function in the
    repository (294 lines).
  - inferred: that the decision block has no side effects other than the two
    `log_line!` fallbacks and `last_used::touch` /
    `remove_stale_semantic_pending` interleaved with it (those two would stay
    in `run`).

## A-fable-10. Wire vocabulary that plugins must spell is defined in `graph`, and the front-mode contract in `mcp`

- **Location:** `core/src/protocol/conformance.rs:7-9`;
  `core/src/graph/{containers.rs:197, imports.rs:97,116,
  symbol_links.rs:305,311}` (`*_NATIVE_KIND` constants);
  `core/src/shim/router.rs:129` and `core/src/mcp/front.rs:37-51`
  (`SELECT_PROJECT`, `SWITCH_PROJECT_META`, `PROJECT_ARG`).
- **Problem (dependency direction, duplicated contract):** the reserved native
  kinds (`"container"`, `"resolved_module"`, `"external_module"`,
  `"pending_symbol"`, `"reexport"`) are part of what a plugin emits, but the
  constants live in core's `graph`, so `protocol::conformance` imports `graph`
  (`protocol <-> graph` cycle) and each plugin repeats the literals. Likewise
  the shim, a separate process whose job is byte routing, imports `mcp` for
  three protocol constants that define how it and the front daemon talk.
- **Proposed change:** define the native-kind constants in `g-mesh-wire`
  (re-exported through `protocol::types` as everything else is); `graph`,
  `protocol::conformance` and the Rust plugins/SDK use them. Move the three
  front-mode constants to `protocol` (e.g. `protocol/front.rs`); `mcp::front`
  and `shim::router` both import from there.
- **Benefit:** `protocol` becomes a true leaf over `wire` (no `graph` import);
  a renamed or added reserved kind is one edit checked by the compiler in
  every Rust plugin; the shim's dependency on `mcp` disappears, leaving it
  dependent on `ipc`, `protocol`, the state-dir layout and
  `daemon::candidates` only.
- **Effort:** S (constants) + S (front constants). **Risk:** low; the Go plugin
  keeps its own literals either way.
- **Evidence:**
  - measured: `protocol/conformance.rs:7-9` imports four `*_NATIVE_KIND`
    constants from `graph::{containers,imports,symbol_links}`.
  - measured: the literals `"resolved_module"|"pending_symbol"|"reexport"|
    "external_module"` appear in non-test sources of
    `plugins/{python,rust,typescript,sdk,go}` and in `wire/src/lib.rs` (grep,
    file list). Not inspected: in what role `wire/src/lib.rs` mentions them
    (it has no `*_NATIVE_KIND` constant definition per a second grep).
  - measured: `shim/router.rs:129`
    `use crate::mcp::front::{PROJECT_ARG, SELECT_PROJECT, SWITCH_PROJECT_META};`
    is the only `crate::mcp` reference in `shim.rs`/`shim/`.
  - measured: g-mesh `get_dependencies(g_mesh::protocol::conformance,
    Incoming, 1)`: `cli/plugin_check/checks.rs` and
    `core/tests/it/protocol_conformance.rs` only (plus itself).

---

## Suggested order

1, 3, 6, 10 are independent S-size moves. 2 before 4 (purge/stop need the
layout below them). 8 before 7 (group the plugin host, then split the
registry). 5 and 9 are independent of the rest.

After 1-6 and 10, the Area A modules' remaining non-test edges out of the area
would be (inferred from the grep edge lists, not recomputed): `daemon ->
{storage, embedding, languages, graph, project_walk, config, log, mcp}`,
`watcher -> project_walk`, `gc -> {storage, config}`, `shim -> {storage,
paths}`; inbound from B: `storage/graph -> protocol::types` only (a re-export
of `g-mesh-wire`). `daemon <-> mcp` remains and is discussed below.

## Not decided

- **`daemon <-> mcp`.** `daemon::serve_forever` calls
  `mcp::serve_connection(stream, conn, registry, core_activity, indexing,
  embedding)` (`daemon/mod.rs:874`), and `mcp` uses `PluginRegistry`,
  `CoreActivity`, `IndexingStatus`. Findings 1, 3 and 7 shrink the `mcp ->
  daemon` side to the registry facade and `CoreActivity`. Whether to invert the
  rest (a `ServeContext` trait owned by `mcp`, implemented by the daemon) is
  mainly an Area C question; I did not read `mcp/mod.rs` and cannot say what it
  would cost.
- **Finding 5, step 3:** callback versus moving `select_with` down. Depends on
  `config_reindex::select_with`'s own dependencies, which I did not read.

## Considered and rejected

- `ipc` and `process.rs`: fan-out 0, one name per operation with per-platform
  halves; already the seam one would ask for.
- `protocol` mixing three framings (`jsonrpc` Content-Length for plugins,
  `ndjson` for bulk output, `ndjson_frame` for MCP): they are separate files
  with disjoint users (measured by grep); splitting the directory would only
  rename paths.
- `protocol/types.rs` being a glob re-export of `g-mesh-wire`: deliberate and
  documented; it is why `storage/graph -> protocol` is harmless.
- `daemon::is_process_alive` (`mod.rs:293`) wrapping `process::is_alive`: a
  one-line alias, not duplication worth a ticket.
- `daemon -> cli::status` in `daemon/semantic.rs:786-793`: inside
  `#[cfg(test)]` (block starts at line 617); not a production edge.
- `registry::fixture_indexer_version` used by `gc`/`storage` tests: it is
  `#[cfg(test)]` (`registry.rs:417`); test-only coupling, cheap to live with.
- `shim/router.rs` at 2358 lines: 1135 are inline tests (map), 0 `pub` items,
  one consumer; large but self-contained. Not read in depth.
- `daemon/workspace_reindex.rs` at 2489 lines: 2002 are inline tests (map);
  the production part is ~490 lines.
- `shim::connect_or_bootstrap` called from `cli::deps` (g-mesh
  `find_callers`): `cli -> shim` is the correct direction once finding 4
  removes `shim -> cli`.
- `gc -> config`/`gc -> storage`: small, downward, and `gc` is the right home
  for more (finding 4), not less.
- State-file constants scattered widely: mostly not true - ten of them are
  already together in `daemon/mod.rs:51-83`; only the stragglers listed in
  finding 2 are outside.

## Not examined

`daemon/{candidates,front,client,seed,memory,build_stamp,bulk_index,semantic,
activation,config_reindex,workspace_reindex,path_case,test_plugin}.rs` were
looked at only through their import lists; `watcher/{burst,debounce,batch,
rescan,ignore_layers}.rs`, `ipc/*`, `process.rs`, `gc/*`, and
`protocol/{jsonrpc,ndjson,ndjson_frame,handshake}.rs` only through the module
map and import greps. Findings about their internals may exist and are not
ruled out by this report. One observed but unranked item:
`daemon/build_stamp.rs:89` imports `watcher::staleness::mtime_millis`, a small
utility that would move with finding 5.
