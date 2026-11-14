# Area C (Surfaces) architecture review

Model: claude-fable-5-2

Scope: `core/src/{mcp,cli,config,log.rs,main.rs,lib.rs}` and their boundaries. Blind review; inputs were `module-map.md` and the code. No builds or tests were run.

Evidence labels: **measured** = seen in code, grep output or a g-mesh result during this review; **inferred** = a conclusion drawn from measured facts, not itself observed. g-mesh calls ran against project `g-mesh` (main checkout); grep ran in the worktree. Line numbers are 1-based, from the worktree.

Corrections to the input map (measured):

- `log` fan-in is listed as 1 module / 1 site. The regex only counts `crate::log::` paths; the module is reached through the `log_line!` macro. `grep -rnE "crate::log\b|log_line!"` finds uses in 30 files outside `log.rs` (daemon, embedding, watcher, mcp, storage, graph, shim, cli, main).
- `get_dependencies(Incoming, core/src/cli/clean.rs, depth 1)` returned only the module itself, although `shim.rs:152` (function-scoped `use`) and `daemon/lifecycle.rs:1015-1022` (inline paths) refer to it. The answer said `truncated: false`. For inbound edges to `cli` I therefore used grep.

Findings are ranked by benefit/effort, highest first.

---

## C-fable-1. Runtime calls into `cli` for process-control and orphan-purge logic

**Location**
- `core/src/cli/clean.rs`: `purge_orphaned_state_dir`, `Purge`, `LockWait`, `sweep_orphaned`, `sweep_orphaned_if_due`, `Sweep`, `ORPHAN_SWEEP_INTERVAL`, `root_state`
- `core/src/cli/stop.rs`: `stop`, `terminate_daemon`, `TERMINATION_TIMEOUT`
- Callers: `core/src/daemon/lifecycle.rs:purge_state_dir`, `core/src/shim.rs` (startup sweep, `retire_outdated_daemon`)

**Problem (dependency direction, cohesion).** `cli` is the outermost surface, but the daemon and the shim depend on it for domain operations: deleting an orphaned state directory under its locks, and stopping a daemon with escalation. These are lifecycle operations that happen to have a subcommand; they are not command-line code. The result is the `cli <-> daemon` and `cli <-> shim` cycles in map section 5.

**Proposed change.**
- Move the purge/sweep half of `clean.rs` (roughly lines 257-624: `RootState`, `root_state`, `Purge`, `LockWait`, `purge_orphaned_state_dir`, `delete_root_record_last`, `singleton_lock_is_held`, `sweep_orphaned*`, the marker constants) to `gc::orphans`. `gc` already owns `last_used` and the stale-projects warning.
- Move `stop`, `terminate`, `terminate_daemon`, the wait helpers and `tidy_state_files` from `cli/stop.rs` to a `daemon::control` module (or `process`, for the pid/group part).
- `cli::clean` and `cli::stop` keep argument handling, `run` and `render`. `daemon::lifecycle`, `shim` and `cli::reindex::stop_for_rebuild` call the new homes.

**Benefit.** The CLI can change (flags, output, new commands) with no rebuild-relevant edge into daemon or shim. Inferred from the map: `cli` has non-test fan-in from exactly two modules (daemon 0/4, shim 2/0); once those six sites are gone, `cli` leaves the 13-module strongly connected component and becomes a pure leaf consumer.

**Effort** S-M. **Risk**: low; pure moves. The purge tests (`clean.rs` lines 1766-1912) move with the code. `ORPHAN_SWEEP_MARKER` and lock-order comments must move intact.

**Evidence**
- measured: `find_callers(purge_orphaned_state_dir)` → `cli::clean::sweep_orphaned`, five `cli::clean::tests::*`, and `daemon::lifecycle::purge_state_dir`; `hasMore: false`.
- measured: `find_callers(cli::stop::stop)` → `cli::stop::run`, `cli::reindex::stop_for_rebuild`, `shim::retire_outdated_daemon`; `hasMore: false`.
- measured (grep): `shim.rs:15 use crate::cli::stop;`, `shim.rs:152 use crate::cli::clean::{sweep_orphaned_if_due, Sweep, ORPHAN_SWEEP_INTERVAL};`, `shim.rs:458 stop::terminate_daemon(pid, stop::TERMINATION_TIMEOUT)`, `daemon/lifecycle.rs:1015-1022` (four `crate::cli::clean::` paths).
- measured (grep): the other textual `crate::cli` hits outside `cli/` are doc comments or string literals (`embedding/model.rs:48,1088`, `embedding/text.rs:615,621`) or sit after a `#[cfg(test)]` marker (`storage/index_store.rs:835` after line 608; `daemon/semantic.rs:786` after line 617). That those two are inside the test module is inferred from position, and agrees with the map's non-test count of daemon->cli 0/4.

---

## C-fable-2. Query logic is welded to the MCP transport type

**Location**
- `core/src/mcp/find_references.rs`: `handle`, `handle_in`, `handle_in_covered`
- `core/src/mcp/find_callers_callees.rs`: `handle_callers{,_in,_in_covered}`, `handle_callees{,_in,_in_covered}`
- `core/src/mcp/find_implementations.rs`: `dispatch{,_in,_in_covered}`; `find_definition.rs`: `handle`, `handle_in`; `get_dependencies.rs`: `handle`, `handle_covered`
- `core/src/mcp/mod.rs`: the eight `#[tool]` methods (lines 729-955)
- Consumers outside `mcp`: `core/src/cli/plugin_check/expectations.rs`, `core/src/cli/embed_eval.rs`

**Problem (missing seam, leaky abstraction, duplication).** Every query function returns `Result<CallToolResult, ErrorData>`, the `rmcp` wire type, and takes a growing positional list (`store, semantic, capabilities, hints, uncovered, params`). Each new cross-cutting input was added as another wrapper, giving two or three entry points per tool. Two non-MCP consumers pay for it:
- the conformance kit calls the handlers and then parses the JSON text back out of the `CallToolResult`;
- the embedding eval reaches in through two special re-exports.

The router glue repeats the same seven-line shape per tool (prepare, clone four fields, `filter_coverage`, `resolve_lazily_off_worker`, call).

**Proposed change.**
- Introduce `QueryCtx<'a> { store, semantic, capabilities, hints, uncovered, project_root }` and make each tool one function `fn answer(ctx: &QueryCtx, params) -> Result<Answer<Page>, QueryError>` where `Page: Serialize` (the response structs already exist: `ReferencePage` etc.) and `Answer` is `Page | Refusal(String)`.
- Put these in a sibling module (`core/src/query/`, or `mcp::query` as a first step). `mcp` keeps `GMeshMcpServer`, `prepare`, waits, progress, instructions, and one generic adapter that turns `Answer<T>` into `CallToolResult` (today `tool_result.rs`).
- `cli::plugin_check` and `cli::embed_eval` depend on the query module, on typed values, not on `mcp`.
- Collapse the three symbol-tool router methods onto one private helper taking the tool name and a function pointer.

**Benefit.** An `rmcp` upgrade or a second transport touches the adapter only. A new per-call input (the next `uncovered`) is one struct field instead of a new wrapper per tool plus every call site. The conformance kit asserts on typed pages instead of re-parsed text, so a response-field rename is a compile error there.

**Effort** M. **Risk**: response bytes must stay identical (there are byte-ceiling tests: `response_bound_tests.rs`, `tools_list_tests.rs`); the `#[tool]` attribute needs the methods to stay on the server type, so only their bodies shrink. Many inline tests call `handle*` directly and need mechanical updates.

**Evidence**
- measured: `find_references.rs:223-345` shows `handle` → `handle_in` → `handle_in_covered`, the first two being pure forwarding; grep of `pub(crate) fn|pub(super) fn` lists the same ladder in the other tools (line numbers 446/549/562, 679/753/766, 570/584/598, 786/802).
- measured: `mcp/mod.rs:762-840`: `find_references`, `find_callers`, `find_callees` bodies differ only in the tool-name string and the callee.
- measured: `cli/plugin_check/expectations.rs:631-636` imports `find_callers_callees, find_definition, find_implementations, find_references, get_dependencies` and the param types from `crate::mcp`; `expectations.rs:1827-1839` (`tool_outcome`) takes `Result<CallToolResult, ErrorData>`, reads `content.first()` as text and runs `serde_json::from_str` on it.
- measured: `mcp/mod.rs:81-82` re-exports `search_code::top_k_for_eval` and `similarity::floor as shipped_similarity_floor` for `cli/embed_eval.rs:1430,1757`.
- measured: the responses are already typed (`json!` appears 9 times in the non-test handler files against 48 `Serialize` mentions), so the typed return is a signature change, not a rewrite.

---

## C-fable-3. `mcp` owns types and constants that its dependencies need

**Location**
- `core/src/mcp/query_shapes.rs:QueryShapes` built by `core/src/daemon/registry.rs:PluginRegistry::query_shapes` (line 960)
- `core/src/mcp/front.rs`: `SELECT_PROJECT`, `PROJECT_ARG`, `SWITCH_PROJECT_META` used by `core/src/shim/router.rs:129`

**Problem (dependency direction).** `daemon::registry` returns an `mcp` type, and `shim::router` imports routing-contract constants from `mcp::front`. Both make a lower layer depend on the surface. `QueryShapes` is derived purely from plugin manifests (`from_manifests(self.discovered.manifests.values())`); the three constants are the shim-daemon project-switch contract, not MCP server internals.

**Proposed change.**
- Move `QueryShapes` next to its source data, `daemon::manifest` (its module doc already names `daemon::manifest::NonSymbolShapes`). `mcp` imports it from there.
- Move the three constants to a small contract module both sides already depend on (`protocol`, or a new `ipc::front_contract`). `mcp::front` and `shim::router` import from it.

**Benefit.** `shim` no longer depends on `mcp` at all (map: shim->mcp 1/0 is this one `use`). `daemon -> mcp` shrinks to the two `serve_connection` calls in the accept loops, which is a composition edge rather than a type dependency. This is the precondition for C-fable-4.

**Effort** S. **Risk**: negligible; `QueryShapes` has 45 references in 19 files, all `use` path updates.

**Evidence**
- measured: `daemon/registry.rs:960-961` (`pub(crate) fn query_shapes(&self) -> crate::mcp::query_shapes::QueryShapes`).
- measured: `find_references(QueryShapes, answer: files)` → total 45; outside `mcp/`: `cli/plugin_check/expectations.rs` (2), `cli/plugin_check/mod.rs` (1), `daemon/registry.rs` (1).
- measured (grep): `shim/router.rs:129 use crate::mcp::front::{PROJECT_ARG, SELECT_PROJECT, SWITCH_PROJECT_META};`, used at lines 437, 442, 898, 934, 1195.
- measured (grep): non-test `crate::mcp` uses in `daemon` are `daemon/mod.rs:874` (`mcp::serve_connection`), `daemon/front.rs:90,114,133` (`mcp::front::Front`, `serve_connection`) and the registry lines above.

---

## C-fable-4. `GMeshMcpServer` is built on concrete daemon internals

**Location**: `core/src/mcp/mod.rs`: `GMeshMcpServer` (fields `registry: Arc<PluginRegistry>`, `core_activity: Arc<CoreActivity>`, `indexing: IndexingStatus`), `prepare`, `replay_queued_changes`, `ensure_file_fresh`, `wait_for_index`, `wait_until`, `wait_for_embeddings`, `instructions`.

**Problem (missing trait, testability, coupling).** The server needs a narrow set of services from its host, but takes three concrete daemon types to get them. This is the bulk of the `mcp -> daemon` edge (23 `use` sites non-test) and the other half of the `daemon <-> mcp` cycle. Any refactor of `PluginRegistry` (2037 lines, map 7.2) or of the indexing state machine is a change `mcp` must compile against.

**Proposed change.** Define in `mcp` (or beside the query module) one trait the daemon implements, covering what is used today:
- from the registry: `project_root`, `path_coverage`, `receiver_call_capabilities`, `missing_languages`, `has_pending`, `pending_summary`, `replay_pending`, `ensure_fresh`, `entry_points`;
- from indexing: `request_activation`, `wait_for`, `phase`, `cold_cause`, `progress_message`, `progress_detail`, `embed_progress`, `embedding_wait_spent`, `mark_embedding_wait_spent`;
- from core activity: `request`.

`GMeshMcpServer` holds `Arc<dyn ProjectHost>` (or is generic over it). Value types the trait mentions (`PathCoverage`, `Capabilities`, `Need`, `Phase`, `WaitOutcome`) either move to a neutral module or stay in `daemon` as plain data.

**Benefit.** Lifecycle, registry and indexing-state changes stop at the trait impl. Server tests can run against a fake host instead of constructing a real `PluginRegistry`.

**Effort** M-L. **Risk**: `prepare` documents a strict order and a lock-order constraint (`mark_used` must not nest inside the plugin lock); a trait must not hide that. `wait_for` is async and cancellable; the trait needs `async` methods or boxed futures. I did not read the wait functions' bodies, so the exact shape of the async part is undecided.

**Evidence**
- measured: `mcp/mod.rs:26-35` imports `daemon::indexing_status::{IndexingStatus, Need, Phase, WaitOutcome}`, `daemon::lifecycle::CoreActivity`, `daemon::manifest::Capabilities`, `daemon::registry::{PathCoverage, PluginRegistry}`.
- measured (grep `registry\.[a-z_]+\(` over `mcp/`): `ensure_fresh` 1, `entry_points` 1, `has_pending` 1, `missing_languages` 1, `path_coverage` 4, `pending_summary` 1, `project_root` 8, `query_shapes` 1, `receiver_call_capabilities` 2, `replay_pending` 1. Ten methods in total.
- measured (grep over `mcp/mod.rs`): the nine `indexing.*` methods and `core_activity.request` listed above.
- measured: `PluginRegistry::new(` appears 7 times in 7 `mcp` files (six test files and `session_hints.rs`). That those tests would be simpler with a fake is inferred.
- measured (map section 3): mcp->daemon test sites are 43 `use` / 32 inline.

---

## C-fable-5. The per-project state-directory layout has no owner

**Location**
- `core/src/paths.rs`: `g_mesh_home`
- `core/src/storage/connection.rs`: `projects_root`, `project_dir`, `ensure_project_dir`
- `core/src/daemon/identity.rs`: `project_hash`, `ROOT_FILE`, `DEVICE_FILE`
- `core/src/daemon/mod.rs` lines 92-300: about twenty `*_path`, `*_path_in`, `read_*_in`, pid-file and lock-path helpers, in the same file as `daemon::run`
- `core/src/config/mod.rs`: `project_config_path` (calls `project_dir`)
- Readers in area C: `cli/clean.rs`, `cli/status.rs`, `cli/stop.rs`, `cli/init.rs`, `cli/reindex.rs`, `cli/plugins.rs`

**Problem (cohesion, dependency direction).** "Where a project's state lives and what files are in it" is spread over four modules. Consequences inside area C: `config` imports `storage` only to find a directory; five CLI commands assemble a view of a daemon by calling free functions in `daemon/mod.rs` and `daemon::identity`. `storage::connection` in turn imports `daemon::identity::project_hash`, which is the `storage -> daemon` edge of the map's cycle list.

**Proposed change.** One module (extend `paths`, which has no dependencies) with a `StateDir` value: `StateDir::for_project(root)`, `StateDir::at(dir)`, and methods `db()`, `config()`, `pid()`, `plugin_pid()`, `socket()`, `phase()`, `progress()`, `build_stamp()`, `daemon_lock()`, `bootstrap_lock()`, `rebuild_marker()`, `root_record()`. `project_hash` moves there. Readers (`read_pid_file`, `read_phase_in`, `read_progress_in`, `read_project_root`) either become methods or stay where they are but take `&StateDir`. Keep `storage::connection::project_dir` as a re-export for one release so the 243 references (mostly `core/tests/it`) do not all change at once.

**Benefit.** Adding or renaming a state file is one edit. `config -> storage`, `shim -> storage` and the `storage -> daemon` `use` disappear, and CLI commands depend on a layout type rather than on `daemon/mod.rs`. `config` becomes a leaf (paths only).

**Effort** M (mechanical, wide). **Risk**: low; the hash and file names must not change, or existing indexes are orphaned. Windows named-pipe endpoints (`endpoint_in` returns `Option`) need care.

**Evidence**
- measured: `storage/connection.rs:7 use crate::daemon::identity::{self, project_hash};`, `config/mod.rs:54 use crate::storage::connection::project_dir;` and `config/mod.rs:251 crate::storage::connection::ensure_project_dir(root)?`.
- measured: `find_references(storage::connection::project_dir, answer: files)` → total 243; in `core/src`: `daemon/mod.rs` 10, `daemon/workspace_reindex.rs` 8, `cli/deps.rs` 3, `cli/reindex.rs` 3, `cli/stop.rs` 3, `config/mod.rs` 3, `daemon/candidates.rs` 3, `cli/status.rs` 2, `shim.rs` 2, `cli/init.rs` 1, plus tests.
- measured (grep of `daemon::…` in `cli/clean.rs`): `identity::read_root_device` 7, `identity::read_project_root` 7, `identity::DEVICE_FILE` 6, `daemon_lock_path_in` 3, `bootstrap_lock_path_in` 2, `pid_path_in`, `read_pid_file_result`, `is_process_alive`, `registry::discovered_pid_files`.
- measured: `daemon/mod.rs` outline, lines 92-300, lists the path helpers; `daemon::run` starts at line 308.

---

## C-fable-6. Runtime settings are read where they are used; `config` covers a small part

**Location**
- `core/src/config/mod.rs` (module doc, `read_global_config`, `read_project_config`)
- `core/src/embedding/rerank.rs:RerankSettings::from_environment`, `core/src/embedding/pipeline.rs:CacheSettings::from_global_config`
- `core/src/mcp/mod.rs`: `env_millis`, `TRACE_CALLS_ENV`, `PROGRESS_INTERVAL_ENV`, `INDEX_WAIT_CAP_ENV`, `SEARCH_EMBEDDING_WAIT_ENV`
- Env-name constants in 22 files across daemon, embedding, mcp, cli, shim, storage, watcher, graph, log, paths

**Problem (config sprawl, testability).** There are three ways a setting reaches code, with no single place that lists them:
1. the project TOML, read once in `daemon::run` and passed down;
2. the global TOML, read from disk by the consumer itself, deep inside `embedding`;
3. 48 distinct `G_MESH_*` environment variables, each declared and parsed by its consumer (40 `env::var` call sites in 10 top-level modules), mixing user knobs, tuning knobs and test hooks.

`mcp` documents that its env values are "read per call, so a test can change it", then adds `search_embedding_wait: Option<Duration>` with a `#[cfg(test)]` setter because writing the variable affects "every other thread in the process". That is the cost of ambient configuration showing up as a structural workaround. The `config` module doc still says "Not wired to runtime behavior yet", which is no longer true.

**Proposed change**, in two steps:
- S step: a `config::env` table: one `const` per variable with kind (user / tuning / test-hook), default and parser, plus the shared parsers (`env_millis` and equivalents). Consumers keep reading at their sites but through the table. Fix the stale module doc.
- M step: resolve a `Settings` value once at process start (daemon, shim, CLI command) from TOML + env, and inject it: `EmbeddingPipeline`, the rerank state and `GMeshMcpServer` take their section. `embedding` stops calling `config::read_global_config`, which removes the B->C edge `embedding -> config` for file reads.

**Benefit.** Adding or changing a knob is one place; the README's variable list can be generated or checked against the table; tests set a struct field instead of process-global state.

**Effort** S for the table, M-L for injection. **Risk**: some variables are deliberately re-read per call (documented in `mcp/mod.rs:150-152`); injection must keep that for the ones integration tests flip on a live daemon. I did not check which integration tests rely on per-call re-reads.

**Evidence**
- measured (grep `"G_MESH_[A-Z0-9_]+"` over `core/src`): 48 distinct names; `const … = "G_MESH_` definitions in 22 files (embedding/model.rs 8, daemon/bulk_index.rs 4, mcp/mod.rs 4, daemon/lifecycle.rs 3, daemon/plugin.rs 3, …).
- measured (grep `env::var(_os)?\(`): daemon 14, embedding 11, shim.rs 4, mcp 3, cli 3, paths.rs 2, log.rs 1, watcher 1, graph 1, storage 1.
- measured: `find_callers(read_global_config)` → `cli::clean::run`, `cli::config_wizard::run`, `embedding::pipeline::CacheSettings::from_global_config`, `embedding::rerank::RerankSettings::from_environment`, `gc::warning::stale_projects_warning`; `hasMore: false`.
- measured: `find_callers(read_project_config)` → includes `daemon::run`; so the "not wired to runtime" paragraph (`config/mod.rs`, lines 39-45) is stale.
- measured: `mcp/mod.rs:216-218` and `248-252` (the `search_embedding_wait` override and its comment).
- inferred: that no central list exists. I found the names only at their definition sites and in README/design docs; I did not read the README section to see how complete it is.

---

## C-fable-7. The embedding eval (8,270 lines) ships inside the core CLI

**Location**: `core/src/cli/embed_eval.rs` and `core/src/cli/embed_eval/` (14 files); `core/src/cli/mod.rs` `Command::DebugEmbedEval`; `core/Cargo.toml` `[features]` (CoreML switch for the eval).

**Problem (cohesion, god module by volume).** A hidden research instrument is 29% of `cli`'s lines (8,270 of 28,139) and 57% of its `pub` items (160 of 281). It has its own cargo feature in the product crate, and it forces two special-purpose re-exports out of `mcp`. Its real dependencies on the product are narrow.

**Proposed change.** Move it to its own workspace member (`eval/`, binary `g-mesh-embed-eval`, or a library the main binary links only under a feature). It depends on the `g_mesh` lib for `embedding::{model, text, backfill}`, `storage::vectors`, `languages::CATALOGUE` and the ranking entry points (after C-fable-2, from the query module). The CoreML feature moves with it.

**Benefit.** Eval work (corpora, metrics, schedules) no longer rebuilds or risks the shipping binary, and core's feature list returns to product concerns. Changes to `embedding` internals show up as a small explicit API used by the eval rather than as `pub(crate)` reach-ins.

**Effort** M. **Risk**: `g-mesh debug-embed-eval` is referenced by docs and possibly scripts (not checked); a separate binary changes how it is invoked. Items it uses that are `pub(crate)` today (`top_k_for_eval`, `shipped_similarity_floor`) must become `pub`, widening the lib's API.

**Evidence**
- measured (map section 2): `embed_eval` 14 files, 8,270 lines, 160 pub; `cli` total 28,139 lines, 281 pub.
- measured (grep of `crate::…` in `cli/embed_eval*`): `embedding::model` 7, `embedding::text` 6, `mcp::shipped_similarity_floor` 3, `storage::vectors` 1, `mcp::top_k_for_eval` 1, `languages::CATALOGUE` 1, `embedding::backfill` 1.
- measured: `core/Cargo.toml` lines 15-18: the feature comment names `debug-embed-eval run --ep coreml`.
- measured (grep): outside its own files `embed_eval` is referenced only from `cli/mod.rs:37,241,384` and three doc comments.

---

## C-fable-8. Index-health read model lives in `cli::status`, with a second derivation in `mcp`

**Location**: `core/src/cli/status.rs`: `index_status`, `semantic_pass_state`, `semantic_pass_lines`, `language_section`, `language_retries`; `core/src/mcp/mod.rs:GMeshMcpServer::instructions` with `mcp/instructions.rs:Coverage::from_outcomes`.

**Problem (cohesion, duplication risk).** "Which languages are indexed, which failed, which are owed a semantic pass" is computed from `storage::schema` rows in two surfaces. `cli::status` holds the fuller model and is reused by daemon and storage tests, which reach up into the CLI for it. `mcp` builds its own coverage from three of the same schema queries.

**Proposed change.** Extract the data half of `status.rs` (the `IndexStatus`, `LanguageSection`, `Coverage` structs and the functions that fill them from a connection) into a non-surface module, for example `storage::health` or `languages::coverage`. `cli::status` keeps `collect`'s process probing and `render`; `mcp::instructions` builds its sentence from the same struct.

**Benefit.** A new language outcome or retry state is modelled once and both surfaces show it; tests in `daemon` and `storage` stop importing `cli`.

**Effort** M. **Risk**: the two surfaces may differ on purpose (the instructions path avoids taking the store during a walk, `mcp/mod.rs:644-654`); the shared model must keep a no-I/O path.

**Evidence**
- measured (grep of `crate::` paths in `cli/status.rs`): `storage::schema::{language_outcomes, language_retries, owed_semantic_pass_languages, semantic_pass_failures, semantic_pass_completed, semantic_leftovers, semantic_pending, semantic_stale_files, pending_reindexes, bulk_index_completed, language_semantic_pass_done}`.
- measured: `mcp/mod.rs:677-683` calls `storage::schema::present_languages_with_semantic_state`, `language_outcomes`, `language_retries`.
- measured: `find_references(cli::status::index_status, answer: files)` → `cli/status/tests.rs` 2, `cli/status.rs` 1, `storage/index_store.rs` 1; grep shows `daemon/semantic.rs:786,793` calling `cli::status::semantic_pass_state` / `semantic_pass_lines`.
- inferred: that the two derivations overlap in logic. I compared the queries they call, not the rules they apply.

---

## C-fable-9. Log lines name the process by hand at 162 sites

**Location**: `core/src/log.rs:log_line!`; call sites across daemon, embedding, watcher, storage, graph, mcp.

**Problem (duplication; a correctness issue that follows from the structure).** `log_line!` adds nothing, so each call site types its own `"g-mesh daemon: "` prefix. Library modules (`embedding`, `storage`, `graph`, `daemon::bulk_index`) hard-code "daemon", but `g-mesh init` and `g-mesh reindex` run the bulk walk in the CLI process. Inferred: lines from those runs are labelled as coming from the daemon.

**Proposed change.** `log::set_role("daemon" | "mcp-shim" | "init" | …)` once at process start (in `daemon::run`, `shim::run`, `cli::dispatch`), and have `write_line` prepend `g-mesh <role>: `. Strip the literal prefixes from call sites.

**Benefit.** A log-format change (timestamps, project id, levels) is one function. Shared code reports the process it actually runs in.

**Effort** S-M (mechanical; 162 literals). **Risk**: tests and docs match on the literal text (`core/tests/it/daemon_log_lines.rs` exists; `mcp/mod.rs:90-93` documents exact lines), so output must stay byte-identical for the daemon role. The plugin SDK carries its own copy of the macro and is unaffected.

**Evidence**
- measured (grep): `"g-mesh daemon: "` 162 occurrences in `core/src`, `"g-mesh mcp-shim: "` 17, `"g-mesh timing: "` 4, `"g-mesh deps: "` 1.
- measured: `log.rs:19-36`: the macro forwards to `write_line`, which only appends `\n`.
- measured: `cli/init.rs:254` and `cli/reindex.rs:120` call `bulk_index::run` in-process; `log_line!` is used in `daemon/bulk_index.rs` (6), `embedding/pipeline.rs` (14), `embedding/backfill.rs` (8).
- inferred: that those specific lines carry the `daemon` prefix when reached from `init`/`reindex`. I counted the prefix globally and did not read each of those call sites.

---

## Considered and rejected

- `main.rs` / `lib.rs`: thin by design; nothing to change.
- `cli/mod.rs` central clap enum plus a 17-arm `dispatch`: one file to touch per new command is an acceptable cost for a single declared surface with parse tests.
- `mcp::front` as a child of `mcp`: justified; it reuses the generated `tool_router()` so schemas stay byte-identical to a normal daemon's.
- `log_line!` duplicated in the plugin SDK: deliberate, the SDK does not depend on core; moving it into `wire` would put I/O into a protocol crate.
- Ad hoc JSON in tool responses: not the case; responses are `Serialize` structs (9 `json!` uses in the handler files).
- Tool names duplicated as string literals outside `mcp`: the hits in `shim/router.rs` and `daemon/client.rs` are test or probe payloads; only `plugin_check/expectations.rs:1575-1578` maps them in product code.
- `ExitStatus` and the exit-code constants in `cli/mod.rs`: small and local (`find_references(cli::ExitStatus)` → `cli/deps.rs`, `cli/mod.rs`).
- `config` as plain serde structs with defaults: sound; its own fan-out is two paths (addressed by C-fable-5).
- `plugin_check` (7,486 lines) as a separate crate, like the eval: it is a user-facing command and needs the query handlers; revisit only after C-fable-2.
- Seventeen `pub mod` in `lib.rs`: wide, but `core/tests/it` depends on it and narrowing buys no independence while core is one crate.
- Large inline test blocks in `mcp` handler files: size, not structure.

## Not verified

- No build was run; every "move" is judged from signatures and call lists, not compiled.
- Bodies of `wait_for_index`, `wait_until`, `wait_for_embeddings`, `ensure_file_fresh` were not read (affects the trait shape in C-fable-4).
- Whether scripts or CI invoke `debug-embed-eval` (C-fable-7) and which integration tests depend on per-call env re-reads (C-fable-6) were not checked.
- `cli/plugin_check`, `cli/model.rs`, `cli/plugins.rs`, `cli/plugin_install.rs`, `cli/deps.rs` and `cli/agent_instructions.rs` were looked at only through their imports, not reviewed internally.
