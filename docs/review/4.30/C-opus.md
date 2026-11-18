# Area C (Surfaces) architecture review - independent

Model: claude-opus-5-5 (Opus 5.5)

Scope: `core/src/{mcp,cli,config,log.rs,main.rs,lib.rs}` and their boundaries.
Goal: refactors that let weakly coupled parts change independently.
Input: `docs/review/4.30/module-map.md` (sections 2-5), then the code via g-mesh
(project `g-mesh`) and grep. "measured" = seen in code or tool output;
"inferred" = my reading of consequences.

Findings are ranked by benefit/effort, highest first.

---

## C-opus-1. Project-state lifecycle lives in `cli`, but the runtime calls it

- **Location:** `cli/stop.rs:stop`, `cli/stop.rs:terminate_daemon`,
  `cli/stop.rs:TERMINATION_TIMEOUT`; `cli/clean.rs:purge_orphaned_state_dir`,
  `cli/clean.rs:LockWait`, `cli/clean.rs:Purge`,
  `cli/clean.rs:sweep_orphaned_if_due`, `cli/clean.rs:ORPHAN_SWEEP_INTERVAL`,
  `cli/clean.rs:root_state`; callers `daemon/lifecycle.rs:1015-1027`,
  `shim.rs:15,152,378,458`.
- **Problem:** dependency direction. The runtime (area A) calls into the
  command-line surface for two runtime jobs: stopping a daemon, and purging or
  sweeping orphaned state directories. That is the only reason for the
  non-test `daemon -> cli` edge (0/4) and for the `cli <-> shim` cycle that holds
  even with `use` statements alone (module-map section 5). It also breaks
  cohesion. `cli/clean.rs` (1925 lines) mixes argument parsing and rendering
  (`run`, `render`, `Target`) with lock-guarded filesystem deletion.
  `gc/mod.rs:1-7` still says "Nothing here ever deletes anything ... deletion
  is always an explicit `g-mesh clean`". The daemon and the shim now delete
  automatically, so that statement is false and the decision has no owner.
- **Proposed change:** add a runtime module, for example
  `core/src/project_state/` (or `daemon::state_dir`), that owns:
  - from `cli/stop.rs`: `stop`, `terminate_daemon`, `terminate`,
    `wait_until_*`, `tidy_state_files`, `Stopped`, `Outcome`;
  - from `cli/clean.rs`: `purge_orphaned_state_dir`, `delete_root_record_last`,
    `singleton_lock_is_held`, `sweep_orphaned[_if_due]`, `Sweep`, `Purge`,
    `LockWait`, `root_state`/`RootState`, `candidates`/`Candidate`,
    `daemon_is_running`;
  - the state-dir layout helpers from C-opus-8.

  `cli::stop` and `cli::clean` keep only `run`, the argument types and
  `render`. `shim.rs` and `daemon/lifecycle.rs` import from the new module.
  `cli::reindex::stop_for_rebuild` calls the new `stop`. Update the `gc`
  module doc, or fold `gc` into the new module.
- **Benefit:** changes to the CLI's flags, wording or exit codes no longer touch
  code that the daemon and shim run unattended. Changes to purge or lock
  semantics are reviewed in one runtime module. The `cli <-> shim` cycle and
  the `daemon -> cli` edge go away, so `cli` becomes a pure leaf consumer
  (fan-in only from `main`).
- **Effort:** S-M. Mostly moving code; the inline tests move with it.
  **Risk:** low. Integration tests that import `g_mesh::cli::clean` or
  `g_mesh::cli::stop` paths need updating (see C-opus-10).
- **Evidence:**
  - measured: `find_callers(cli::stop::stop)` returns `cli::stop::run`,
    `cli::reindex::stop_for_rebuild` and `shim::retire_outdated_daemon`.
  - measured: `find_callers(terminate_daemon)` returns `cli::stop::stop` and
    `shim::evict_wedged_daemon`.
  - measured: grep finds `daemon/lifecycle.rs:1015`
    `crate::cli::clean::LockWait::UpTo(PURGE_LOCK_WAIT)`, `:1016`
    `crate::cli::clean::purge_orphaned_state_dir`, and `shim.rs:152`
    `use crate::cli::clean::{sweep_orphaned_if_due, Sweep, ORPHAN_SWEEP_INTERVAL}`.
  - measured: the only other `daemon -> cli` sites are
    `daemon/semantic.rs:786,793`, which sit inside `#[cfg(test)]` (line 617).

## C-opus-2. Heartbeat loop copied three times in `mcp/mod.rs`

- **Location:** `mcp/mod.rs:GMeshMcpServer::replay_queued_changes` (ticker at
  :316), `::ensure_file_fresh` (:411), `::wait_until` (:528).
- **Problem:** duplication. Each copy rebuilds the same pieces:
  - `env_millis(PROGRESS_INTERVAL_ENV, ...)`;
  - the progress-token filter;
  - `interval_at` with `MissedTickBehavior::Delay`;
  - a `select!` over the task and the ticker;
  - `ProgressNotificationParam` measured from `call_started`;
  - the same log line when a send fails.

  The invariant that `progress` keeps rising across both heartbeats of one call
  (doc of `ensure_file_fresh`) holds only because all three copies agree.
- **Proposed change:** add one helper in a new `mcp/progress.rs`:
  `async fn with_heartbeat<T>(ctx, call_started, task: impl Future<Output = T>, message: impl Fn(Duration) -> String) -> (T, u32)`.
  It returns the task's output and the number of notifications sent. The three
  methods keep only their own task and message text.
- **Benefit:** a change to the progress protocol (interval, message shape, a
  new rmcp API, cancellation) is made once. A fourth waiting step costs one call
  instead of about 40 lines.
- **Effort:** S. **Risk:** low. The existing `search_code_wait_tests` and the
  progress tests cover the behaviour; watch the `biased` ordering of `select!`.
- **Evidence:** measured: grep for `interval_at|MissedTickBehavior|notify_progress`
  in `mcp/` (non-test) finds exactly three triples, at `mod.rs:316/320/336`,
  `411/415/431` and `528/532/559`. I read the bodies at 297-459.

## C-opus-3. Handler entry points multiply with every new per-call input

- **Location:** `mcp/find_references.rs:handle`, `handle_in`,
  `handle_in_covered`; `mcp/find_callers_callees.rs:handle_callers[_in[_covered]]`,
  `handle_callees[_in[_covered]]`; `mcp/get_file_outline.rs:handle[_covered]`;
  `mcp/find_definition.rs` (1215, 1230); `mcp/get_dependencies.rs` (786, 802);
  `mcp/search_code.rs` (293, 319); `mcp/find_implementations.rs:166`.
  Outside the module: `cli/plugin_check/expectations.rs:633`.
- **Problem:** there is no context type, so each handler takes 5-6 positional
  context parameters (`store, semantic|embedding+shapes, capabilities, hints,
  uncovered, params`). Each time a new per-call input was added, a new wrapper
  layer was added rather than a field (`_in` for the semantic rung, `_covered`
  for `uncovered`). The result is 18 `pub(crate)`/`pub(super)` handler entry
  points for 9 tools. The conformance kit (`plugin_check`) and the tests call
  these variants directly, so each new layer is also a signature change for
  them. `handle_callees` builds a fresh `SessionHints::default()` while
  `handle_callers` takes one: an inconsistency of exactly the kind this
  layering produces.
- **Proposed change:** add `mcp::ToolContext<'a> { store, semantic: &SemanticRung, capabilities, hints, uncovered: &[(String, PathCoverage)] }`
  with a builder for "no coverage, default hints" (what tests and
  `plugin_check` use). Give each tool one entry point,
  `handle(ctx: &ToolContext, params)`, plus at most one lazy-semantic wrapper.
  `GMeshMcpServer` builds the context once per call, in `prepare`'s caller.
- **Benefit:** a new cross-cutting input (a new disclosure field, a deadline, a
  per-session flag) becomes one struct field instead of N new wrappers, and
  `plugin_check` and the tests stop breaking each time.
- **Effort:** M (mechanical, many call sites in tests). **Risk:** low;
  behaviour is unchanged if the defaults match what each `_in` wrapper passes
  today (`&[]` for `uncovered`).
- **Evidence:**
  - measured: grep `^pub\((crate|super)\) (async )?fn handle` in `mcp/` lists
    18 definitions.
  - measured: signatures at `find_references.rs:223/318/331` and
    `find_callers_callees.rs:446/549/562/679/753/766`.
  - measured: `find_callers_callees.rs:686`
    `let hints = SessionHints::default();`.
  - measured: `cli/plugin_check/expectations.rs:633-636` imports
    `find_callers_callees, find_definition, find_implementations, find_references, get_dependencies`
    from `crate::mcp`.

## C-opus-4. The MCP server is wired to the concrete `PluginRegistry`; no runtime seam

- **Location:** `mcp/mod.rs:GMeshMcpServer` (fields `registry: Arc<PluginRegistry>`,
  `core_activity`, `indexing`), `mcp/mod.rs:serve_connection`,
  `daemon/mod.rs:874`, `daemon/registry.rs:960` (`PluginRegistry::query_shapes`).
- **Problem:** missing seam, and a reversed dependency.
  - `mcp` uses about ten methods of the registry. Because of that, it imports
    `daemon::registry::{PluginRegistry, PathCoverage}`, `daemon::lifecycle::CoreActivity`
    and `daemon::indexing_status::*`.
  - Every mcp handler test must build a real registry, which is why the
    test-only `mcp -> daemon` edge is 43/32 sites, against 23/1 in production
    (module-map section 3).
  - In the other direction, `daemon::registry` constructs an
    `mcp::query_shapes::QueryShapes` (an MCP concept), and its only caller is
    `GMeshMcpServer::new`.
  - Together these make `daemon <-> mcp` a `use`-level cycle (module-map
    section 5).
- **Proposed change:**
  1. Define a trait `mcp::QueryRuntime` that contains only what the surface
     needs:
     - `project_root`, `has_pending`, `pending_summary`, `replay_pending(&IndexStore)`;
     - `ensure_fresh(&IndexStore, &str)`, `path_coverage`,
       `receiver_call_capabilities`, `entry_points`, `missing_languages`;
     - `manifests()`, for building query shapes.

     `PluginRegistry` implements it, and `GMeshMcpServer` holds
     `Arc<dyn QueryRuntime>`.
  2. Move `QueryShapes::from_manifests(...)` into `GMeshMcpServer::new` and
     delete `PluginRegistry::query_shapes`, which removes `daemon -> mcp` at
     `registry.rs:960`.
  3. Optionally, have the composition root (`cli` dispatch for
     `Command::Daemon`) pass the daemon a connection-handler closure, so
     `daemon/mod.rs:874` and `daemon/front.rs` stop naming `mcp` at all.
- **Benefit:** the registry's internals (plugin lifecycle, queueing, suspension)
  can change without touching `mcp`. MCP handler tests can use a fake runtime
  instead of spawning plugin machinery. The `daemon <-> mcp` cycle is broken.
- **Effort:** M. **Risk:** medium. `prepare` relies on a lock-order invariant:
  `mark_used` must not be nested inside the plugin lock that the replay holds
  (doc at `mcp/mod.rs:252-262`). The trait docs must restate it. Dynamic
  dispatch cost is negligible: about 10 calls per request.
- **Evidence:**
  - measured: counting `registry.<method>(` across `mcp/*.rs` gives
    `ensure_fresh, entry_points, has_pending, missing_languages, path_coverage x4, pending_summary, project_root x8, query_shapes, receiver_call_capabilities x2, replay_pending`.
  - measured: `find_callers(PluginRegistry::query_shapes)` has exactly one
    caller, `mcp::GMeshMcpServer::new`.
  - measured: grep finds `daemon/mod.rs:874` `mcp::serve_connection(stream, conn, registry, core_activity, indexing, embedding)`.

## C-opus-5. Surfaces write SQL against the storage schema

- **Location:**
  - `cli/status.rs`: `language_section`, `language_retries`,
    `indexed_file_paths`, `recorded_baselines`, `syntax_error_files`.
  - `cli/deps.rs` (around :453), `cli/plugin_check/session.rs` (around :573),
    `cli/embed_eval.rs` (around :343).
  - `mcp/overrides.rs` (:113), `mcp/unlinked.rs:candidate_sql`,
    `mcp/untyped.rs:candidate_sql`, `mcp/find_definition.rs:rank_candidates`,
    `mcp/search_code.rs:stored_vectors`.
- **Problem:** leaky abstraction. `graph::queries` and `storage::schema` exist
  as the query layer, but surface code also embeds raw SQL that names tables and
  columns: `nodes`, `edges`, `indexed_files`, `placeholder_targets`,
  `untyped_calls`, `vectors`, `meta`, `sqlite_master`.
  - `cli/status.rs` repeats schema-version compatibility logic. It probes
    `sqlite_master` for `language_outcome` and `language_retry`, then reads
    `meta.schema_version`.
  - It also has its own `open_index`, whose doc says it copies what
    `gc::last_used` does.

  As a result, a schema change or migration must be checked across 9
  surface files as well as storage.
- **Proposed change:**
  - Add a read-only storage API, for example `storage::report`, with
    `language_outcomes_if_present`, `language_retries_if_present`,
    `indexed_file_paths`, `recorded_baselines`, `syntax_error_files`,
    `stored_vector_count`, and one shared `open_existing_for_read` that
    `gc::last_used` also uses.
  - Move the mcp disclosure queries (`unlinked`, `untyped`, `overrides`
    parent lookup, `rank_candidates`) into `graph::queries`, next to
    `declaration_only`, which they already import.
  - Surfaces then deal only in typed rows.
- **Benefit:** schema migrations (column renames, new tables, a different
  vectors layout) become changes local to storage/graph. CLI and MCP output
  changes no longer require SQL review.
- **Effort:** M. **Risk:** low to medium. The SQL moves verbatim; the risk is
  paging and ordering assumptions in `rank_candidates`, which the existing
  `find_definition` tests cover.
- **Evidence:**
  - measured: grep for SQL string literals (`"SELECT|INSERT|UPDATE|DELETE `)
    finds them before each file's first `#[cfg(test)]` in:
    - `cli/status.rs` (first :488, test module :1376);
    - `cli/deps.rs` (:453 / :685);
    - `cli/embed_eval.rs` (:343 / :1809);
    - `cli/plugin_check/session.rs` (:573 / :1936);
    - `mcp/overrides.rs` (:115 / :244);
    - `mcp/unlinked.rs` (:187 / :222);
    - `mcp/untyped.rs` (:85 / :114);
    - `mcp/find_definition.rs` (:418 / :565);
    - `mcp/search_code.rs:225`.
  - measured: I read the excerpts at `cli/status.rs:480-530` and `:870-905`.

## C-opus-6. `config` covers TOML only; settings are read ad hoc deep in the data layer

- **Location:** `config/mod.rs:read_global_config`;
  `embedding/pipeline.rs:CacheSettings::from_global_config` (:119);
  `embedding/rerank.rs:RerankSettings::from_environment` (:321);
  `mcp/mod.rs:env_millis`, `*_ENV` constants (:102-137);
  `mcp/mod.rs:GMeshMcpServer::with_search_embedding_wait`.
- **Problem:** config sprawl and dependency direction.
  - The data layer reads the user's global config from disk at the point of use:
    `embedding -> config` (1/3) is the only edge from area B to area C
    (module-map section 4).
  - Outside the TOML file, behaviour is set by 44 `G_MESH_*` env-var constants
    spread over 20 files. Each has its own parser (`env_millis`,
    `env_disables_cache`, `env_disables_rerank`, ...) and is read per call
    rather than once.
  - Because the env is process-global, tests need special hooks. For example,
    `with_search_embedding_wait` exists "so a test can shorten it without
    writing `SEARCH_EMBEDDING_WAIT_ENV` for every other thread".
- **Proposed change:**
  - Add `config::Settings`: the TOML values plus env overrides, resolved once
    at process start by the composition root (cli dispatch / `daemon::run`).
    It holds typed values (`cache: Option<CacheSettings>`,
    `rerank: RerankSettings`, `mcp: McpTimings { progress_interval, index_wait_cap, search_embedding_wait }`).
  - Pass the needed slice down through constructors. `CacheSettings::new` and
    `RerankSettings { enabled, model_dir }` already exist as value types, so
    `embedding` only loses its `from_*` constructors.
  - The `G_MESH_*` names become a table in `config`, which also documents them.
  - Do this incrementally. Diagnostics-only envs (`TRACE_CALLS`, timing) can
    stay where they are.
- **Benefit:** `embedding` stops depending on `config`, which removes the only
  B -> C edge. Tests set values by construction instead of the process env. A
  new knob has one place to be declared and documented.
- **Effort:** M (incremental per knob). **Risk:** low to medium. Some knobs
  today take effect without a restart, because they are read per call; making
  them resolve once changes that. Check which ones tests or users rely on
  changing live.
- **Evidence:**
  - measured: grep for `const .*ENV.*: &str = "G_MESH` (non-test) finds 44 in
    20 files, the largest being `embedding/model.rs` (8), `mcp/mod.rs` (4) and
    `daemon/bulk_index.rs` (4).
  - measured: grep finds `read_global_config()` at `embedding/pipeline.rs:119`
    and `embedding/rerank.rs:321`.
  - measured: the doc at `mcp/mod.rs:244-246`.

## C-opus-7. The plugin-manifest model lives under `daemon`, so every surface depends on `daemon`

- **Location:** `daemon/manifest.rs` (921 lines; `Capabilities`,
  `MemberOverrides`, `ReceiverCallResolution`, `NonSymbolShapes`,
  `PluginManifest`, `DiscoveredPlugins`, `read_manifest`, `discover`).
- **Problem:** dependency direction and cohesion. The manifest is a data model
  for what a plugin declares. Its only crate dependencies are
  `graph::symbol_links::LinkRules` and `protocol::types::CURRENT_PROTOCOL_VERSION`.
  It is imported from outside `daemon` by:
  - 11 non-test files in `mcp`;
  - `cli/plugins.rs`, `cli/plugin_install.rs`, `cli/status.rs` and 3 files of
    `cli/plugin_check`;
  - `languages.rs`, `graph/queries.rs`, `graph/containers.rs`,
    `storage/schema.rs` and `watcher/apply.rs`.

  These users need a type, not the daemon, yet each of these edges points into
  `daemon`. That feeds the 13-module strongly connected component (module-map
  section 5).
- **Proposed change:** move `daemon/manifest.rs` to a top-level `manifest`
  module, or `languages::manifest`, and keep a temporary
  `pub use crate::manifest as manifest;` in `daemon` for the transition.
  `LinkRules` would then be the one remaining upward edge (`manifest -> graph`).
  Either move `LinkRules` (a declarative rules type) into `manifest` as well,
  or accept that edge.
- **Benefit:** the surfaces' type-level dependency on `daemon` shrinks to the
  runtime seam of C-opus-4. Changes to manifest schema (a new capability field)
  stop looking like daemon changes, and daemon refactors stop touching mcp and
  cli imports.
- **Effort:** S-M (mechanical path change, about 50 files including tests).
  **Risk:** low. Path churn only; integration tests use
  `g_mesh::daemon::manifest` (11 sites, see C-opus-10).
- **Evidence:**
  - measured: `grep -l daemon::manifest` outside `daemon/` returns the files
    listed above.
  - measured: `daemon/manifest.rs:20-21` are its only `use crate::` lines.
  - measured: per-file `crate::daemon::` imports in `mcp/` (non-test) are
    `daemon::manifest` in 9 files, `daemon::registry::PathCoverage` in 6, and
    `candidates`/`indexing_status`/`lifecycle` in 4.
  - inferred: how much this shrinks the strongly connected component; I did
    not re-run Tarjan's algorithm.

## C-opus-8. `config` depends on `storage` for the state-directory layout

- **Location:** `config/mod.rs:54` (`use crate::storage::connection::project_dir`),
  `config/mod.rs:write_project_config` (:251, `storage::connection::ensure_project_dir`);
  `storage/connection.rs:project_dir`, `ensure_project_dir`.
- **Problem:** dependency direction. The rule "where a project's state lives
  under `~/.g-mesh/projects/<hash>/`, and the root record written on first
  touch" sits in `storage::connection`. As a result, `config` (a leaf by
  nature) imports `storage`, and so do `cli/stop.rs`, `cli/status.rs`,
  `cli/clean.rs` and `gc`, only for paths.
- **Proposed change:** move `project_dir[_under]`, `projects_root_under`,
  `project_hash` and `ensure_project_dir[_under]` (with
  `identity::record_project_root`) into `paths`, or into the
  `project_state` module of C-opus-1. `storage::connection` keeps opening
  `index.db` at `paths::project_dir(root)/index.db`. `config` then depends
  only on `paths`.
- **Benefit:** `config` and the state-directory tooling can change and be tested
  without the storage module. A storage refactor (connection pooling, a schema
  split) no longer touches the files that only need a path.
- **Effort:** S. **Risk:** low. The hash function must stay byte-identical, or
  existing indexes become orphaned.
- **Evidence:** measured: `config/mod.rs:54,224-226,250-253`;
  `storage/connection.rs:27-72` (shown by grep).

## C-opus-9. The embedding-eval instrument ships inside the product surface

- **Location:** `cli/embed_eval.rs` + `cli/embed_eval/` (14 files, 8270 lines);
  `mcp/mod.rs:81-82` (`pub(crate) use search_code::top_k_for_eval; pub(crate) use similarity::floor as shipped_similarity_floor;`).
- **Problem:** cohesion of the surface. `debug-embed-eval` is hidden and,
  according to its own doc, is "an instrument for choosing the embedding model,
  not a user-facing command". Yet it is the largest child of `cli` (29% of its
  lines, 160 `pub` items per module-map section 2). `mcp` carries re-exports
  that exist only for it, so a rename inside `mcp::search_code` or
  `mcp::similarity` has to account for an eval tool.
- **Proposed change:** gate `cli::embed_eval` and the two `mcp` re-exports
  behind a cargo feature (`eval`, off by default), or move them into a separate
  binary target (`core/src/bin/g-mesh-eval.rs`) that uses a narrow crate API.
  Add a CI job that builds with `--features eval`.
- **Benefit:** product builds and reviews no longer carry 8k lines of eval code.
  The `mcp` search internals can change without keeping eval-only re-exports
  stable, and eval can change without touching the shipping binary.
- **Effort:** M. **Risk:** low to medium. Scripts that call
  `g-mesh debug-embed-eval` on a release binary would need the feature build;
  check `scripts/` before doing this.
- **Evidence:**
  - measured: module-map section 2 (`embed_eval` dir, 14 files, 8270 lines,
    160 pub).
  - measured: grep finds `cli/embed_eval.rs:1430,1757,2838,2842` calling
    `crate::mcp::shipped_similarity_floor` and `crate::mcp::top_k_for_eval`.
  - measured: `mcp/mod.rs:81-82`.

## C-opus-10. `lib.rs` exports every module, so integration tests pin internal paths

- **Location:** `core/src/lib.rs` (17 `pub mod`); `core/tests/**`.
- **Problem:** there is no API boundary. The library crate exists so the binary
  and the integration tests can link it, but every module is `pub`. As a
  result, core's integration tests address internals directly:
  - `g_mesh::daemon` (113 occurrences);
  - `g_mesh::storage` (110);
  - `g_mesh::shim` (78);
  - `g_mesh::embedding` (56);
  - `g_mesh::daemon::manifest` and `g_mesh::daemon::bulk_index::WALK_HOLD_FILE_ENV`.

  Every boundary move proposed above (C-opus-1, -4, -7, -8) therefore also
  breaks integration-test imports. The compiler also cannot report items that
  are `pub` but unused outside the crate.
- **Proposed change:** make the modules `pub(crate)` except `cli`, which `main`
  needs. Add a `#[doc(hidden)] pub mod testkit` that re-exports the items the
  integration tests use. Moves inside the crate then update only `testkit`'s
  re-exports.
- **Benefit:** internal reorganisations become cheap, with one re-export file
  to update instead of N test files. Dead `pub` items become visible to
  `dead_code`.
- **Effort:** M (first pass: enumerate what `core/tests` uses).
  **Risk:** low. `plugins/sdk/src/log.rs` mentions `g_mesh::`; check whether
  that is a doc reference or a real use. Doctests on now-private items would
  need moving.
- **Evidence:**
  - measured: `core/src/lib.rs` contents.
  - measured: `grep -rhoE "g_mesh::[a-z_]+" core/tests | sort | uniq -c` gives
    the counts above.
  - measured: `grep -rln g_mesh:: plugins walk wire` returns only
    `plugins/sdk/src/log.rs`.

---

## Suggested order

1. Do C-opus-10 first, so the later moves stop breaking tests.
2. Then C-opus-1, C-opus-8 and C-opus-7, which are moves that break cycles.
3. C-opus-2 and C-opus-3 are local and independent; do them any time.
4. C-opus-4 builds on C-opus-7.
5. C-opus-5, C-opus-6 and C-opus-9 can be done incrementally.

## Considered and rejected

- **`log.rs` being in Surfaces.** It is a dependency-free leaf used through
  `$crate::log_line!` from nearly every module. The module map's fan-in of 1
  undercounts it because macro paths are not counted. This is a labelling
  question, not coupling.
- **`daemon/semantic.rs -> cli::status`.** Test-only (inside `#[cfg(test)]` at
  :617), not a production edge.
- **`cli` fan-out of 14 modules.** This is expected of the dispatch and
  composition surface. The problem is the edges back into `cli` (C-opus-1), not
  the edges out of it.
- **`cli::reindex` calling `shim::acquire_bootstrap_lock`.** The surface calls
  the runtime, which is the correct direction.
- **`plugin_check` calling handler modules directly, bypassing `GMeshMcpServer`.**
  This is deliberate: the kit exercises pure handler logic without the session
  machinery. C-opus-3 reduces the cost; removing the bypass would be wrong.
- **`shim/router.rs` importing `mcp::front::{PROJECT_ARG, SELECT_PROJECT, SWITCH_PROJECT_META}`.**
  These are three wire constants and one consuming file
  (`find_references(SELECT_PROJECT)` returns 2 files). Moving them to `protocol`
  is cosmetic.
- **Exit-code handling (`cli::ExitStatus`, `exit_code`).** One consistent
  mechanism: errors are downcast once in `main`. No finding.
- **`main.rs` size (11 lines).** Already the thin entry point it claims to be.
