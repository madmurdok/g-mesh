# GM-710: embedding settings injected, one `G_MESH_*` knob table, log role set once (R-9, R-20, R-31)

Source findings: `docs/review/4.30/README.md` R-9, R-20, R-31. Read against
`release-4.33.0` (= `main` + version bump). Line numbers are that tree at the time of
writing; each code slice re-reads by symbol, not by line. Several review line numbers have
drifted; the current ones are given below, with the drift noted where it matters.

## 0. Where we start

### 0.1 `G_MESH_*` names

Counted with `grep -rhoE '"G_MESH_[A-Z0-9_]+"' --include='*.rs'` (quoted string literals;
identifiers such as `G_MESH_BIN_ENV` and the manifest placeholder `${G_MESH_BIN_DIR}` are
not env reads and are not counted).

- **67** distinct names in the repo. **42** in non-test `core/src` code; 6 more appear only
  in `core/src` test code (`G_MESH_CONTAINERS_SEED`, `G_MESH_CONTAINERS_SEEDS`,
  `G_MESH_TEST_CACHE_WRITER`, `G_MESH_TEST_FP32_MODEL_DIR`, `G_MESH_TEST_THREADED_CHILD`,
  `G_MESH_WRITE_SEARCH_GOLDEN`); the rest belong to the plugin SDK
  (`G_MESH_BIN`, `G_MESH_PLUGIN_HOLD_DIR`, `G_MESH_FAKE_PLUGIN_*`) or to test binaries
  (`G_MESH_BLESS`, `G_MESH_RS_GM*_CHILD`, `G_MESH_VERSION`, ...).
- Of the 42 core names, **39 are read by core** and **3 are only set by core for a child
  plugin** (`G_MESH_BULK_STDIN_LIFELINE`, `G_MESH_PLUGIN_MANIFEST`,
  `G_MESH_PLUGIN_CHECK_MARKER_DIR`; the SDK reads them).
- The 39 reads are **31 raw `std::env::var`/`var_os` call sites in 20 files**
  (`InferenceOptions::from_env` reads 7 names through one closure, `mcp::env_millis` 3,
  `plugin_install::download_base` 2). Plus 2 non-`G_MESH` production reads:
  `CLAUDE_PROJECT_DIR` (`shim.rs:181,195`).
- Kinds (from each constant's doc comment): **user 10, tuning 12, test-hook 17**; child
  contract 3. The full table is section 2.2.

Files with a production read: `plugin_manifest.rs`, `log.rs`, `paths.rs`, `shim.rs`,
`embedding/{backfill,rerank,pipeline,model}.rs`, `ingest/apply.rs`, `mcp/mod.rs`,
`cli/{model,plugin_install}.rs`, `storage/index_store.rs`,
`daemon/{bulk_index,front,lifecycle,seed,plugin}.rs`.

Parse rules found (more than the review's two):

| Rule | Where | Zero | Bad value | Empty |
|---|---|---|---|---|
| millis, zero = off | `lifecycle::parse_timeout` (fn at 126-140): `PLUGIN_IDLE`, `CORE_IDLE`, `FRONT_IDLE` | `None` (timer off) | default + one `g-mesh daemon: ignoring ...` line | bad |
| millis, zero = default | `plugin::parse_round_trip_timeout` (249-265): 3 round-trip timeouts | default | default + the same line text | bad |
| millis, lenient | `mcp::env_millis` (164-170), `shim::bootstrap_timeout` (512-517) | `Duration::ZERO` (consumer decides: no progress / no cap / no wait) | default, **silent** | bad |
| u64/u32, lenient | `bulk_index` delay (430), `lifecycle::lifeline_pid_from` (835-837) | 0 | `None`, silent | bad |
| flag | `TRACE_CALLS`, `EDIT_TIMING`, `NO_ABSENT_COUNT`, `EMBED_PASS_PANIC` | - | - | unset |
| off switch | `RERANK`, `EMBEDDING_CACHE`: `eq_ignore_ascii_case("off")` | - | - | on |
| path, empty = unset | `HOME`, `DAEMON_LOG`, `RERANK_MODEL_DIR`, 5 hold files, `MODEL_BASE_URL` | - | - | unset |
| path, empty = set | `MODEL_DIR` (`model.rs:1076`, no filter); `PLUGIN_ROOTS_OVERRIDE` (both sites) | - | - | **`PathBuf("")`** |
| tri-state | `seed::seed_enabled_from`: `0` off, `1` on, else default | off | default | default |
| string, empty = unset | `DOWNLOAD_BASE`, `REPO` | - | - | unset |
| custom | `InferenceOptions::from_env_with` (`model.rs:407-...`): 7 `EMBED_*` | per field | default + log | unset |

`PLUGIN_ROOTS_OVERRIDE` is read twice with different APIs: `plugin_manifest::default_roots`
(60) via `env::var` (non-UTF-8 reads as unset), `plugin_install::install_root` (104) via
`var_os`.

### 0.2 Which per-call re-reads tests flip on a live daemon

**None flip an env var.** `grep set_var|remove_var` finds no call in `core/src` or
`core/tests` (only doc comments saying tests avoid it); a spawned daemon's environment is
fixed at spawn. What tests flip on a live daemon is the **file a hold knob names**; the
re-read that matters is the file's existence, polled inside the hook, not the env read:

| Hook (knob) | Polled in | Tests that flip it |
|---|---|---|
| `G_MESH_BULK_INDEX_HOLD_FILE` | `bulk_index::hold_the_walk_open_for_tests` (412) | `index_wait_progress`, `plugin_bridge`, `multi_project_front`, `cli_deps` |
| `G_MESH_BULK_INDEX_HOLD_LOCK_FILE` | `index_store::hold_the_lock_open_for_tests` (190) | `handshake_independent_of_indexing` |
| `G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE` | `apply::hold_compute_open_for_tests` (1077) | `first_query_after_walk`, `superseded_collapsed_call`, `incremental_embed_outside_lock`, `replay_progress` |
| `G_MESH_EMBED_PASS_HOLD_FILE` | `backfill::hold_before_first_batch_for_tests` (354) | `structural_does_not_wait_for_embeddings` |
| `G_MESH_SEED_HOLD_RECONCILE_FILE` | `seed::hold_the_seeded_reconcile_for_tests` (73) | `worktree_seed` |
| `G_MESH_PLUGIN_HOLD_DIR` (SDK) | SDK `hold.rs` | `plugins_die_with_daemon` |

(all under `core/tests/it/`). `mcp::env_millis` documents "read per call, so a test can
change it without starting a new daemon", but no test does. Rule for the knob table:
**readers do not cache**; sites that cache today keep their own caching
(`log::timing_enabled`'s `OnceLock`, `IdleTimeouts` and `RoundTripTimeouts` at
construction). That keeps every observable timing as it is.

### 0.3 `"g-mesh daemon: "` literals

- **171** occurrences in `.rs` files: **160** in `core/src` code (the review's 162 has
  drifted), 3 in a doc comment (`mcp/mod.rs:102-104`, example output), 8 in `core/tests`
  (assertions on a spawned daemon's log: `daemon_log_lines`, `edit_timing_lines`,
  `one_plugin_binary_missing` x2, `replay_progress`, `daemon_core` x3).
- The 160 code sites, in **25 files**: `daemon/registry.rs` 28, `daemon/lifecycle.rs` 17,
  `daemon/workspace_reindex.rs` 14, `daemon/activation.rs` 13, `daemon/config_reindex.rs` 11,
  `mcp/mod.rs` 8, `daemon/plugin.rs` 8, `daemon/seed.rs` 8, `embedding/backfill.rs` 9,
  `embedding/pipeline.rs` 9, `daemon/startup.rs` 7, `paths/state_dir.rs` 5,
  `daemon/semantic.rs` 4, `embedding/rerank.rs` 4, `daemon/bulk_index.rs` 3,
  `daemon/front.rs` 2, `daemon/mod.rs` 2, and 1 each in `daemon/watch_loop.rs`,
  `indexing_status.rs`, `ingest/apply.rs`, `ingest/convert.rs`, `mcp/front.rs`,
  `mcp/progress.rs`, `plugin_manifest.rs`, `storage/index_store.rs`.
- Shapes: **152** are the first argument of `crate::log_line!(` (43 on the same line, 109
  on the next line; every one is spelled `crate::log_line!`), **7** are `format!` strings
  logged later (`registry.rs:1603,1608` in `unroutable_notice`, logged at 1031;
  `ingest/convert.rs:40` in `PathWarnings::warn`; `rerank.rs:536,557,580,589` passed to the
  `Reranker`'s `log` closure), **1** is a `panic!` (`backfill.rs:374`, the test knob).
- No production code parses the prefix (`grep 'starts_with("g-mesh'` in `core/src` finds
  only an unrelated test). `log_line!` adds no prefix (`log.rs:19-35`), and the SDK carries
  an identical copy pinned by the shared `core/tests/log_line/shared.rs`.
- CLI processes that log `g-mesh daemon:` today: `bulk_index::run` (callers
  `cli::init::init` at `init.rs:256`, `cli::reindex::reindex` at `reindex.rs:122`; the
  review's `bulk_index.rs:512` is now 490), plus everything those walks reach (embedding
  pipeline/backfill, `ingest`, `index_store`, `paths/state_dir`), and
  `RoundTripTimeouts::from_env`'s bad-value line under `g-mesh plugins check`
  (`plugin_check/mod.rs:156`).
- The role decision has one daemon entry: `cli::dispatch`'s `Command::Daemon` arm
  (`cli/mod.rs:384`) is the only caller of `daemon::run` (g-mesh `find_callers`), and
  `daemon::run` covers the front (`front::run`) too.

### 0.4 Embedding settings

- `embedding -> config` is the only B->C edge (`module-cycles.py --edges`:
  `embedding -> config 1/3`): `pipeline.rs:65` (`use crate::config::EmbeddingConfig`),
  `pipeline.rs:120` (`read_global_config` in `CacheSettings::from_global_config`),
  `rerank.rs:321` (`read_global_config` in `RerankSettings::from_environment`),
  `model.rs:127` (`EmbeddingConfig::default().model` in `embedding_version`).
- Env reads for settings inside `embedding`: `pipeline.rs:117` (`G_MESH_EMBEDDING_CACHE`),
  `rerank.rs:320` (`G_MESH_RERANK`), `rerank.rs:96` (`G_MESH_RERANK_MODEL_DIR`),
  `model.rs:1076` (`G_MESH_MODEL_DIR`), `model.rs:402` (`InferenceOptions::from_env`, 7
  `G_MESH_EMBED_*`). Test hooks (not settings): `backfill.rs:355,373`.
- When they are read today: the cache settings on the first `compute` with something to
  embed (`open_if_unopened`, `pipeline.rs:613-631`, `CacheSlot::Unopened(None)`); the rerank
  on the first `search_code` that would rerank (`Reranker`'s `settings` closure); the
  `EMBED_*` options when the model loads (inside the loader closure); `G_MESH_MODEL_DIR` on
  every `EmbeddingPipeline::model_dir` call (308-313).
- `EmbeddingPipeline::load` production callers (g-mesh `find_callers`, complete):
  `daemon::startup::start_services` (`startup.rs:345`), `cli::init::init` (**two** sites,
  `init.rs:218` and `:249`), `cli::reindex::reindex` (`reindex.rs:118`). Test caller:
  `mcp::search_code::tests::a_query_related_to_a_docstring_...` (`search_code.rs:1011`).
  `load_with_cache`: `core/tests/it/embedding_generation_pipeline.rs` (`load_real_pipeline`
  150, `cached_vectors_are_bit_identical_...` 389). `load_at`: `search_code.rs:764` (test).
- Reranker settings constructors: `RerankSettings::from_environment` (rerank.rs:318-326),
  called by `RerankModelState::from_environment` (351-354; caller `cli::status` at
  `status.rs:264`) and installed as the closure in `Reranker::from_environment` (446-456;
  only caller `EmbeddingPipeline::build`, pipeline.rs:239-262).
  `default_rerank_model_dir` callers: `RerankSettings::from_environment`,
  `cli::model::run` (243, 247), tests `rerank::tests::load_real`, `ce_text` test.
- `default_model_dir` callers: `resolve_model_dir` (only caller `cli::model::model_dir`,
  365), `EmbeddingPipeline::model_dir`, tests in `model.rs`, `search_code.rs:966`,
  `core/tests/it/embedding_generation_pipeline.rs::model_dir`,
  `core/tests/it/embedding_peak_memory.rs`. `InferenceOptions::from_env` callers:
  `EmbeddingPipeline::build`, `cli::embed_eval::run_variant` (815), `cli::embed_eval::probe::run` (69).
- `config/mod.rs:39-45` says "Not wired to runtime behavior yet". It is wired: the project
  config is read by `daemon::startup::start_services` (`startup.rs:337-340`; the review's
  `daemon/mod.rs:467` predates the startup split) into `IdleTimeouts`, the plugin memory
  limit and `EmbeddingPipeline::load`; the global config by `cli::clean`, `gc::warning`
  and (today) `embedding`. Two more stale docs: `DaemonConfig` ("Not wired to the real
  daemon lifecycle yet", ~106-108) and `EmbeddingConfig` ("not yet shipped", ~122-123).

## 1. Decisions

Recommendation first in each.

### D1. Embedding settings are resolved eagerly by the composition root

- **Today:** the cache settings are read at the first embed, the rerank settings at the first
  rerank, each reading `config.toml` itself; a daemon that never embeds never reads it.
- **Change:** `config::embedding_settings(&EmbeddingConfig) -> EmbeddingSettings` runs where
  `EmbeddingPipeline::load` is called today (4 production sites) and reads env and the global
  file there. `embedding` gets plain values.
- **Example:** a broken `~/.g-mesh/config.toml` and a daemon that only answers
  `find_callers`: today nothing is logged; after, `g-mesh: failed to read the global config
  (...) - using the embedding cache's defaults` is in `daemon.log` right after start.
- **Consequence:** the review's one allowed change. One small file read per daemon/CLI
  start (sub-millisecond; not a startup-budget risk like the model load the startup comment
  warns about). A config edit still needs a restart, as today.
- **Alternative B:** inject lazy closures built by the root (`Box<dyn Fn() -> ...>`).
  Timing byte-identical, but a second closure type per setting, and `embedding` still cannot
  be tested with plain values. Not recommended.

### D2. Read the global file at most once, keep both existing failure lines

- **Today:** each of cache and rerank reads the file; a broken file gives two different lines
  (`... - using the embedding cache's defaults`, `... - the rerank keeps its default (on)`),
  and a part switched off by env never reads the file nor logs.
- **Change:** `embedding_settings` reads the file only if either env switch leaves its part
  on, once, and logs each existing line only for a part that needed the file. Texts unchanged.
- **Consequence:** the only difference from today is the moment of logging (D1). No test
  asserts these lines (grep).
- **Alternative:** one merged line. Changes text for no gain.

### D3. Where resolution lives: `core/src/config/embedding.rs`

- **Change:** new submodule of `config` holding `embedding_settings`, `model_dir(name)`,
  `resolve_model_dir(explicit, name)`, `rerank_model_dir()`, `inference_options()`. These are
  today's env-reading helpers moved out of `embedding` with the same names; `embedding` keeps
  pure versions that take the override as an argument.
- **Consequence:** new edge `config -> embedding` (C->B, downward); `embedding -> config` is
  gone (`module-cycles.py --absent 'embedding->config'` passes; the graph stays acyclic).
- **Alternative:** a function in `daemon::startup` plus copies in `cli::init`/`reindex`:
  three copies of precedence rules. Not recommended.

### D4. The default model name moves into `embedding`

- **Today:** `embedding_version` (`model.rs:127`) and `build` (`pipeline.rs:240`) compare with
  `config::EmbeddingConfig::default().model`.
- **Change:** `pub const DEFAULT_MODEL_NAME: &str = "jina-embeddings-v2-base-code"` in
  `embedding::model`; `EmbeddingConfig::default` uses it.
- **Consequence:** one owner of the string; the stored `embeddingVersion` is unchanged.

### D5. Knob table: a new leaf module `core/src/knobs.rs`

- **Today:** each constant sits next to its consumer; 31 sites call `std::env` directly.
- **Change:** `knobs.rs` holds `pub const <NAME>: Knob = Knob { name, kind, doc }` per row of
  section 2.2, `pub const ALL: &[Knob]`, and the readers. It depends only on `std` and
  `crate::log_role!` (for bad-value lines). Existing `pub const *_ENV` names stay as aliases
  (`pub const CORE_IDLE_ENV: &str = crate::knobs::CORE_IDLE.name;`), so the 174
  `<path>::*_ENV` references in `core/tests` and `plugins`
  (`g_mesh::embedding::model::MODEL_DIR_ENV` alone 39) do not move.
- **Why not `config/knobs.rs`:** `embedding/backfill.rs`'s two test hooks would import
  `config` and recreate the edge R-9 removes. **Why not `paths`:** unrelated concern.
- **Consequence:** one place lists every knob with its kind; `embedding -> knobs` is a leaf
  edge.

### D6. Zero rules: two named rules plus a third, lenient reader

- **Today:** the two deliberate rules (D-review), and a third undocumented one: `env_millis`
  and `bootstrap_timeout` pass `0` through as `Duration::ZERO` and ignore bad values
  silently.
- **Change:** `knobs::millis(&K, ZeroIs::Off, default) -> Option<Duration>` (idle timers),
  `knobs::millis(&K, ZeroIs::Default, default) -> Option<Duration>` always `Some` (round
  trip; site unwraps), both logging the existing bad-value line; and
  `knobs::millis_lenient(&K, default) -> Duration` (zero is zero, silent) for the 4 lenient
  sites. Exact signature is S3's choice as long as the names `ZeroIs::Off` and
  `ZeroIs::Default` exist.
- **Example:** `G_MESH_CORE_IDLE_MS=0` -> `None`; `G_MESH_FILE_CHANGED_TIMEOUT_MS=0` -> 30 s;
  `G_MESH_INDEX_WAIT_CAP_MS=0` -> `ZERO` (cap removed by the consumer);
  `G_MESH_INDEX_WAIT_CAP_MS=abc` -> 25 min, no log line.
- **Alternative:** fold the lenient sites into `ZeroIs::Off` with logging: adds log lines and
  reshapes 4 consumers. Out of scope.

### D7. Empty values keep each knob's current rule

- **Today:** `G_MESH_MODEL_DIR=""` resolves to `PathBuf("")`, `G_MESH_RERANK_MODEL_DIR=""`
  to the default; `PLUGIN_ROOTS_OVERRIDE` is UTF-8-only at one site.
- **Change:** none in behaviour. The table records the rule per row; readers
  `os_raw`/`string_raw` (empty counts) and `os_non_empty`/`string_non_empty` (empty = unset)
  both exist. Both `PLUGIN_ROOTS_OVERRIDE` sites switch to one reader (`os_raw`); the
  manifest site's non-UTF-8 behaviour changes from "unset" to "that path", which no test or
  user can hit on macOS/Linux with a valid path. If the owner prefers zero change, keep
  `string_raw` there.
- **Follow-up (not here):** normalise to "empty = unset" everywhere.

### D8. Enforcement: a unit test in `knobs.rs`

- **Change:** `every_env_read_goes_through_the_knob_table` scans `core/src/**/*.rs` (non-test
  files; skips brace-balanced `#[cfg(test)]` items the way `module-cycles.py` does, or simply
  skips everything after the file's `#[cfg(test)] mod`) for `env::var(` / `env::var_os(` /
  `var_os(` outside `knobs.rs`, against an allow-list: `shim.rs` `CLAUDE_PROJECT_DIR` (2).
  It also asserts every `ALL` name is unique and starts with `G_MESH_`.
- **Alternative:** a grep in `scripts/check.sh` (not run by `cargo test`; drifts unseen), or
  review only. Not recommended.

### D9. Log role: `log::set_role` once, new macro `log_role!`, `log_line!` unchanged

- **Today:** 160 sites spell `"g-mesh daemon: "`; the same code run by the CLI prints it too.
- **Change:** in `log.rs`: `pub enum Role { Daemon, Cli }`, `static ROLE: OnceLock<Role>`,
  `pub fn set_role(Role)` (first call wins; later calls ignored), `pub fn role() -> Role`
  (unset = `Cli`), `pub const fn prefix_for(Role) -> &'static str`, `pub fn role_prefix()`,
  and `#[macro_export] macro_rules! log_role` writing
  `write_line(format_args!("{}{}", role_prefix(), format_args!($($arg)*)))`: one `write(2)`,
  same bytes as today for the daemon.
- **Why not prefix inside `log_line!`:** 140 `"g-mesh: "` sites and the timing lines would get
  a double prefix, and core's copy would diverge from the SDK's, which one test file pins for
  both.
- **Consequence:** daemon lines byte-identical; shared code needs no role parameter.

### D10. The daemon role is set in `cli::dispatch`, not in `daemon::run`

- **Change:** `Command::Daemon { project_root } => { crate::log::set_role(Role::Daemon);
  daemon::run(&project_root) }` (`cli/mod.rs:384`).
- **Why:** the role is a property of the process, decided at the process entry; `daemon::run`
  stays callable in-process by a future test without fixing that test binary's role.
  Covers `stand_down`, `startup::claim` and the front, which all run inside `daemon::run`.
- **Alternative:** first line of `daemon::run`. Same output today.

### D11. CLI lines use `g-mesh: `

- **Change:** `prefix_for(Role::Cli) == "g-mesh: "`.
- **Example:** `g-mesh reindex` with a malformed bulk line printed
  `g-mesh daemon: skipping malformed bulk-index line: ...`, now
  `g-mesh: skipping malformed bulk-index line: ...`, the same prefix as `main.rs`'s error line
  and the existing CLI-side lines (`g-mesh: failed to read the global config ...`).
- **Alternatives:** `g-mesh <subcommand>: ` (needs the role to carry the command name;
  different from every existing CLI line); `g-mesh cli: ` (a new spelling nobody reads).
- **Consequence:** a CLI process's stderr has no `g-mesh daemon:` line; the shim and
  `plugins check` also print `g-mesh: ` for shared code.

### D12. Tests never call `set_role`

- **Today/after:** test binaries (unit and in-process integration tests that run daemon
  code, e.g. walks through `bulk_index::run`) leave the role unset, so shared code prints
  `g-mesh: ` there. No in-process assertion depends on the prefix (checked: `PathWarnings`
  tests use `contains`, `unroutable_notice` tests `is_some`, rerank tests do not read the
  prefix). Daemon bytes are pinned on spawned binaries (section 3). The prefix rule itself
  is tested through the pure `prefix_for`, not by mutating the global.
- **Alternative:** a thread-local or resettable role for tests: makes "once per process"
  untrue and buys nothing.

### Not changed (follow-ups for the owner's backlog)

- Shared-code lines spelled `"g-mesh: "` that the daemon also prints (e.g. `pipeline.rs`,
  `model.rs` `ignoring {name}=...`) stay as they are: converting them would change daemon
  bytes.
- `mcp/mod.rs:102-104` doc example keeps `g-mesh daemon:` (it shows daemon output).
- Injecting a resolved `Settings` instead of readers (review's later M step).
- Plugin SDK knobs: the SDK does not depend on core; its names are not in core's table
  except the 3 core sets (kind `Child`).

## 2. Edit maps

### 2.1 R-9 (S2, opus)

Change:

| File | Item (current lines) | Change |
|---|---|---|
| `embedding/model.rs` | `embedding_version` (126-132) | compare with new `DEFAULT_MODEL_NAME` |
| | `MODEL_DIR_ENV` (219) | keep the const (tests import it); no read here |
| | `InferenceOptions::from_env` (400-403) | delete; keep `from_env_with` |
| | `default_model_dir` (1075-1081), `resolve_model_dir` (1092-1097) | become pure `default_model_dir_from(override: Option<&Path>, name)`; env-reading versions move to `config/embedding.rs` (same names) |
| `embedding/pipeline.rs` | `use crate::config::EmbeddingConfig` (65) | remove |
| | `CacheSettings::from_global_config` (113-136), `env_disables_cache` (138-140) | move to `config/embedding.rs` |
| | `CacheSlot::Unopened(Option<..>)` (144-152), `open_if_unopened` (613-631) | `Unopened(CacheSettings)`; drop the `or_else(from_global_config)` |
| | struct field `config: EmbeddingConfig` (~190) | `model: ModelSettings` |
| | `load` (216-219), `load_with_cache` (222-228), `load_at` (235-238), `build` (239-262), `disabled` (270-275), `with_loader` (280-296) | `load(settings: EmbeddingSettings)`; delete `load_with_cache` (callers set `settings.cache`); `build` takes `settings.model.inference` into the loader and `Reranker::new(settings.rerank)` |
| | `model_dir` (308-313) | explicit dir, else `default_model_dir_from(settings.model.dir_override, name)` |
| | new | `pub struct EmbeddingSettings { model: ModelSettings, cache: Option<CacheSettings>, rerank: RerankSettings }`, `pub struct ModelSettings { name: String, dir_override: Option<PathBuf>, inference: InferenceOptions }` (or a new `embedding/settings.rs`) |
| `embedding/rerank.rs` | `default_rerank_model_dir` (95-101) | pure `rerank_model_dir_from(override)`; env version to config |
| | `RerankSettings` (310-327), `env_disables_rerank` (329-331) | `pub` struct; `from_environment` moves to config |
| | `RerankModelState::from_environment` (351-354) | delete; `of` becomes `pub` |
| | `Reranker::from_environment` (446-456) | `Reranker::new(settings: RerankSettings)`; the `Fn` settings source can hold `Mutex<Option<RerankSettings>>` taken once (S2's choice) |
| `embedding/mod.rs` | re-exports (27, 29) | export the pure fns, `EmbeddingSettings`, `ModelSettings`, `RerankSettings`, `DEFAULT_MODEL_NAME` |
| `config/mod.rs` | `EmbeddingConfig::default` (~131-136) | use `embedding::model::DEFAULT_MODEL_NAME`; add `pub mod embedding;` |
| `config/embedding.rs` (new) | | `embedding_settings`, `model_dir`, `resolve_model_dir`, `rerank_model_dir`, `inference_options`, cache/rerank resolution per D2 |
| `daemon/startup.rs` | `start_services` (326-...; load at 345) | `EmbeddingPipeline::load(config::embedding::embedding_settings(&project_config.embedding))` |
| `cli/init.rs` | `init` (load at 218, 249) | same |
| `cli/reindex.rs` | `reindex` (load at 118) | same |
| `cli/status.rs` | 264 | `RerankModelState::of(config::embedding::rerank_settings())` |
| `cli/model.rs` | `run` (243, 247), `model_dir` (364-366) | config versions |
| `cli/embed_eval.rs` 815, `cli/embed_eval/probe.rs` 69 | | `config::embedding::inference_options()` |
| tests | `search_code.rs` 764, 966, 1011; `model.rs` tests 1105-1152; `rerank.rs` `load_real` 1034; `ce_text.rs` 365-369; `core/tests/it/embedding_generation_pipeline.rs` 137-160, 389; `core/tests/it/embedding_peak_memory.rs` | build `EmbeddingSettings` / call config versions |

Read for context only: `pipeline.rs` module doc "Where the model lives", `open_cache`
(634-...), `reopen` (815); `rerank.rs` `Reranker::outcome` (525-565); `cache::default_path`.

Keep raw env reads in `config/embedding.rs` for now (S3 converts them to knob readers).
Acceptance for S2: `scripts/module-cycles.py --absent 'embedding->config'` exits 0; `grep -n
'std::env' core/src/embedding/*.rs` shows only the two `backfill.rs` hooks and test code.

### 2.2 R-20 (S3, sonnet): the table and the site edits

`knobs.rs` row per knob; "reader" is what the site calls after the change. Line = current
const definition; read site in brackets.

| Knob | Kind | Const (file:line) | Read site | Reader |
|---|---|---|---|---|
| `G_MESH_HOME` | user | `paths.rs:22` | `g_mesh_home` (38) | `os_non_empty` (keep `home_from`) |
| `G_MESH_DAEMON_LOG` | user | `shim.rs:80` | `daemon_stderr` (601) | `os_non_empty` |
| `G_MESH_MODEL_DIR` | user | `embedding/model.rs:219` | `config::embedding::model_dir` (after S2) | `os_raw` (empty = set) |
| `G_MESH_RERANK_MODEL_DIR` | user | `embedding/rerank.rs:54` | `config::embedding::rerank_model_dir` | `os_non_empty` |
| `G_MESH_RERANK` | user | `embedding/rerank.rs:57` | `config::embedding` | `off_switch` |
| `G_MESH_EMBEDDING_CACHE` | user | `embedding/cache.rs:39` | `config::embedding` | `off_switch` |
| `G_MESH_SEED` | user | `daemon/seed.rs:40` | `seed_enabled` (55) | `string_raw` + existing `seed_enabled_from` |
| `G_MESH_MODEL_BASE_URL` | user | `cli/model.rs:132` | `sources` (136) | `os_non_empty` |
| `G_MESH_DOWNLOAD_BASE` | user | `cli/plugin_install.rs:68` | `download_base` (118) | `string_non_empty` |
| `G_MESH_REPO` | user | `cli/plugin_install.rs:71` | `download_base` (118) | `string_non_empty` |
| `G_MESH_EMBED_INTRA_THREADS`, `_INTER_THREADS`, `_OPT_LEVEL`, `_SPIN`, `_BATCH`, `_TOKEN_BUDGET`, `_BUCKET` (7) | tuning | `embedding/model.rs:265-271` | `config::embedding::inference_options` | `InferenceOptions::from_env_with(knobs::lookup)`; custom rule stays in `from_env_with` |
| `G_MESH_EDIT_TIMING` | tuning | `log.rs:40` | `timing_enabled` (49) | `flag` (keep the `OnceLock`) |
| `G_MESH_TRACE_CALLS` | tuning | `mcp/mod.rs:113` | `trace_call` (107) | `flag` |
| `G_MESH_PROGRESS_INTERVAL_MS` | tuning | `mcp/mod.rs:120` | `env_millis` callers (`progress.rs:42`) | `millis_lenient` |
| `G_MESH_INDEX_WAIT_CAP_MS` | tuning | `mcp/mod.rs:129` | `mcp/mod.rs:457` | `millis_lenient` |
| `G_MESH_SEARCH_EMBEDDING_WAIT_MS` | tuning | `mcp/mod.rs:142` | `mcp/mod.rs:549` | `millis_lenient` |
| `G_MESH_PLUGIN_ROOTS_OVERRIDE` | test-hook | `plugin_manifest.rs:54` | `default_roots` (60), `plugin_install::install_root` (104) | `os_raw` (D7) |
| `G_MESH_BOOTSTRAP_TIMEOUT_MS` | test-hook | `shim.rs:48` | `bootstrap_timeout` (512-517) | `millis_lenient` |
| `G_MESH_PLUGIN_IDLE_MS` | test-hook | `daemon/lifecycle.rs:54` | `IdleTimeouts::from_config` (93-99) | `millis(ZeroIs::Off)` |
| `G_MESH_CORE_IDLE_MS` | test-hook | `daemon/lifecycle.rs:57` | same | `millis(ZeroIs::Off)` |
| `G_MESH_FRONT_IDLE_MS` | test-hook | `daemon/front.rs:47` | `front_core_idle` (49-51) | `millis(ZeroIs::Off)` |
| `G_MESH_FILE_CHANGED_TIMEOUT_MS` | test-hook | `daemon/plugin.rs:193` | `RoundTripTimeouts::from_env` (215-233) | `millis(ZeroIs::Default)` |
| `G_MESH_SEMANTIC_PASS_FILE_TIMEOUT_MS` | test-hook | `daemon/plugin.rs:195` | same | `millis(ZeroIs::Default)` |
| `G_MESH_SEMANTIC_PASS_PROJECT_TIMEOUT_MS` | test-hook | `daemon/plugin.rs:200` | same | `millis(ZeroIs::Default)` |
| `G_MESH_LIFELINE_PID` | test-hook | `daemon/lifecycle.rs:66` | `lifeline_pid` (830-832) | `parsed::<u32>` |
| `G_MESH_BULK_INDEX_DELAY_MS` | test-hook | `daemon/bulk_index.rs:60` | `hold_the_walk_open_for_tests` (430) | `parsed::<u64>` |
| `G_MESH_BULK_INDEX_HOLD_FILE` | test-hook | `daemon/bulk_index.rs:72` | same (415) | `os_non_empty` |
| `G_MESH_BULK_INDEX_NO_ABSENT_COUNT` | test-hook | `daemon/bulk_index.rs:66` | `absent_count_switched_off` (406-408) | `flag` |
| `G_MESH_BULK_INDEX_HOLD_LOCK_FILE` | test-hook | `storage/index_store.rs:187` | `hold_the_lock_open_for_tests` (191) | `os_non_empty` |
| `G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE` | test-hook | `ingest/apply.rs:1095` | `hold_compute_open_for_tests` (1078) | `os_non_empty` |
| `G_MESH_EMBED_PASS_HOLD_FILE` | test-hook | `embedding/backfill.rs:123` | `hold_before_first_batch_for_tests` (355) | `os_non_empty` |
| `G_MESH_EMBED_PASS_PANIC` | test-hook | `embedding/backfill.rs:136` | `panic_before_first_batch_for_tests` (373) | `flag` |
| `G_MESH_SEED_HOLD_RECONCILE_FILE` | test-hook | `daemon/seed.rs:88` | `hold_the_seeded_reconcile_for_tests` (74) | `os_non_empty` |
| `G_MESH_BULK_STDIN_LIFELINE` | child | `daemon/bulk_index.rs:44` | set at 346, `plugin_check/session.rs:391` | none (set only) |
| `G_MESH_PLUGIN_MANIFEST` | child | `plugin_manifest.rs:50` | set at `daemon/plugin.rs:621`, `session.rs:384,1586` | none |
| `G_MESH_PLUGIN_CHECK_MARKER_DIR` | child | `cli/plugin_check/session.rs:102` | set at `session.rs:168` | none |

Counts to check against: 10 user + 12 tuning + 17 test-hook + 3 child = 42.

Site edits:
1. New `core/src/knobs.rs` (+ `pub mod knobs;` in `lib.rs`): `Knob`, `Kind {User, Tuning,
   TestHook, Child}`, `ZeroIs {Off, Default}`, the 42 consts, `ALL`, readers `flag`,
   `off_switch`, `os_raw`, `os_non_empty`, `string_raw`, `string_non_empty`, `parsed<T>`,
   `millis`, `millis_lenient`, `lookup`, and the D8 tests.
2. Every `pub const X_ENV: &str = "G_MESH_..."` in the table becomes
   `= crate::knobs::X.name` (same visibility, same name).
3. Replace each read site per the table. `lifecycle::parse_timeout` (126-140) and
   `plugin::parse_round_trip_timeout` (249-265) move into `knobs::millis` (both bad-value
   lines are the same text). Callers that pass raw strings in tests
   (`IdleTimeouts::from_config_and_overrides`, `seed_enabled_from`, `lifeline_pid_from`,
   `timing_enabled_from`, `home_from`) keep their raw-value seam: the reader is called in the
   thin wrapper only. Keep `parse_timeout`'s tests passing by pointing them at the knobs
   function or keeping a `pub(crate)` shim.
4. `mcp::env_millis` (164-170) is deleted; its 3 callers and `progress_tests.rs:19,92` use
   `knobs::millis_lenient`.
5. `config/mod.rs`: rewrite the "Not wired to runtime behavior yet" section (39-45) to say
   which consumer reads each section (section 0.4); fix the `DaemonConfig` and
   `EmbeddingConfig` doc sentences.

Stop rule for the sonnet slice: a site whose current rule is not exactly the row's reader
(empty handling, trimming, case, logging) is reported, not adapted.

### 2.3 R-31 (S3, sonnet)

1. `log.rs` after `write_line` (24-35): D9 items, module doc gains one paragraph.
2. `cli/mod.rs:384`: D10.
3. The 152 `crate::log_line!(` sites: one Python pass over `core/src/**/*.rs` excluding
   `log.rs`: where `"g-mesh daemon: ` is the first token after `crate::log_line!(` (same line
   or the next non-blank line), replace `crate::log_line!(` with `crate::log_role!(` and drop
   the `g-mesh daemon: ` from the literal. Expected: 152 replacements; abort if different.
4. The 8 other sites by hand: `registry.rs:1603,1608` and `convert.rs:40` and
   `rerank.rs:536,557,580,589`: `format!("{}...", crate::log::role_prefix(), ...)` with the
   literal's prefix removed (the log call that prints them stays `log_line!("{...}")`);
   `backfill.rs:374`: `panic!("{}G_MESH_EMBED_PASS_PANIC ...", crate::log::role_prefix())`.
5. Check: `grep -rn '"g-mesh daemon: ' core/src` returns nothing; `log.rs` holds the prefix
   once, in `prefix_for`.

Do R-31 first in S3 (pure text), then R-20: R-20's bad-value lines are then written with
`log_role!` directly.

## 3. What the tests slice must pin

| # | Behaviour | How |
|---|---|---|
| T1 | `embedding_settings` precedence: env `off` beats file `enabled = true`, for cache and rerank; file `enabled = false` turns each off; a broken file gives defaults and the existing line | unit test on a pure inner `resolve(lookup, global: Result<GlobalConfig>)` (no `set_var`) |
| T2 | A broken global config is logged at daemon start, before any embed (D1, the allowed change) | integration: daemon with a broken `$G_MESH_HOME/config.toml`, read `daemon.log` after `serving`, no `search_code` call |
| T3 | `EmbeddingPipeline::load` honours `settings.cache = None` (no cache file created) and `settings.model.inference` reaches the loader | unit, `with_loader`-style |
| T4 | `embedding -> config` absent | `module-cycles.py --absent 'embedding->config'` in a `core/tests/scripts` test or CI step |
| T5 | `ZeroIs::Off`: `0` -> `None`; `ZeroIs::Default`: `0` -> default; bad -> default and one line naming the knob; lenient: `0` -> `ZERO`, bad -> default, no line | table-driven unit test in `knobs.rs` |
| T6 | Table integrity: names unique, `G_MESH_` prefix, each `*_ENV` alias equals its row | unit |
| T7 | No env read outside `knobs.rs` except the allow-list | D8 test |
| T8 | Daemon bytes unchanged | existing `daemon_log_lines`, `edit_timing_lines`, `one_plugin_binary_missing`, `replay_progress`, `daemon_core` (they assert `g-mesh daemon: ` on a spawned daemon) |
| T9 | A CLI-run walk logs the CLI role | integration: `g-mesh reindex` with `G_MESH_BULK_INDEX_DELAY_MS=1`; stderr has `g-mesh: holding the finished bulk walk open for 1ms` and no `g-mesh daemon:` |
| T10 | `prefix_for(Daemon) == "g-mesh daemon: "`, `prefix_for(Cli) == "g-mesh: "`, unset role is `Cli` | unit on the pure fn |

Candidate controls (revert = code change, never the test):

| C | Revert | Must fail |
|---|---|---|
| C1 | `config::embedding` ignores `G_MESH_EMBEDDING_CACHE` | T1 |
| C2 | rerank `enabled` ignores the file (`true`) | T1 |
| C3 | cache settings read lazily again (`load` stores `None`, pipeline reads at first embed) | T2 |
| C4 | `PLUGIN_IDLE` site uses `ZeroIs::Default` | T5 (and `lifecycle` tests) |
| C5 | `knobs::millis(ZeroIs::Default)` returns `None` on `0` | T5 |
| C6 | plant `std::env::var("G_MESH_SEED")` back in `seed_enabled` | T7 |
| C7 | drop `set_role(Role::Daemon)` from `cli/mod.rs:384` | T8 (cheapest: `edit_timing_lines` or `daemon_core`) |
| C8 | `prefix_for(Cli)` returns `"g-mesh daemon: "` | T9, T10 |

C1/C2/C5/C6/C8's unit halves share one `--lib` build; C3, C7, C8's integration half need the
`it` binary.

## 4. Proposed slicing

Keep the draft order; R-9 first because S3 converts the reads S2 moves into `config`.

- **S2** R-9, opus code (section 2.1). Touches 4 embedding files, 2 config files, 6 CLI/daemon
  call sites, ~6 test files.
- **S3** R-31 then R-20, sonnet code, two commits (sections 2.3, 2.2). Mechanical: R-31 is a
  counted script plus 8 hand edits; R-20 is a 42-row table with a reader per row and a stop
  rule for any mismatch. If the owner wants less risk on one sonnet brief, split R-20 into its
  own sonnet slice after R-31 (adds one slice, removes nothing).
- **S4** opus review of S2+S3.
- **S5** tests: T1-T10 and the controls above.
- **S6** verify.

## 5. Owner review (2026-11-17)

- D1: "При старте (Recommended)".
- D11: "g-mesh: (Recommended)".
- D7: "Как в ноте (Recommended)".
- Slicing and D2-D6, D8-D10, D12 as recommended: "R-20 отдельным слайсом (Recommended)" —
  S2 R-9 (opus), S3 R-31 (sonnet), S4 R-20 (sonnet), then review, tests, verify.
