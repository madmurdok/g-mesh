# GM-548: faster local test runs

Design note for GM-548/S1. It covers three owner decisions: lib unit tests in
one process, a heavy group run at batch end, and selection by changed paths.
CI's coverage does not change: every edit below either lives in a profile or
script that CI never calls, or keeps today's behaviour when nothing opts in.

## 1. Measurements

### 1a. g-mesh lib: one process against one process per test

Same build (`-p g-mesh`, profile `test`), worktree `g-mesh-wt-gm548` at
`404ed48` (release-4.3.0). Both commands ran back to back in one background
script. The machine has 8 CPUs and was shared with other agents.

| run | command | tests | result | real | user | sys | load (1/5/15 min) at start |
|---|---|---:|---|---:|---:|---:|---|
| per process | `cargo nextest run -p g-mesh --lib --no-fail-fast` | 1781 (+9 ignored) | all passed | 733.4s | 187.7s | 50.5s | 938 / 949 / 893 |
| one process | `cargo test -p g-mesh --lib --no-fail-fast` | 1781 (+9 ignored) | all passed | 146.2s | 157.1s | 49.7s | 549 / 552 / 675 |

- **One process is 5.0x faster in `real`, and uses 13% less CPU** (206.8s vs
  238.2s user+sys). The load differed (938 vs 549), so the `real` ratio is
  overstated. The CPU numbers do not depend on load, and they still show the
  per-process run paying ~31s for 1781 process starts. The rest of its extra
  `real` is waiting for the run queue. `user` is about 1/4 of `real` in the
  per-process run, so most of the time was spent waiting, not working.
- **No test failed in one process.** The release gate already runs
  `cargo test -p g-mesh` (`scripts/cut-release.sh`, GM-302), so the lib has
  been passing in one process at every release. The global-state tests in
  section 2 are therefore *race risks*, not failures seen today.
- The one-process run's `real` is bounded by its slowest test. nextest's
  per-test times for the same tests, slowest first:
  `mcp::response_bound_tests::*` (6 tests: 124, 119, 117, 113, 58, 51s),
  `graph::containers` random sequence 44s, `graph::symbol_links`
  `link_all_and_link_diff_agree_on_the_same_end_state` 37s,
  `graph::containers::bulk_batch_boundaries_do_not_change_the_result` 24s. The
  slowest test that is not heavy took 17s. With the heavy group excluded, the
  one-process run is expected to be bounded by CPU (~200s user+sys over 8
  CPUs), not by a single test. The measure slice confirms this.
- Build cost, for the record: `cargo build --workspace` was real 417.6s,
  user 165.1s. `cargo test -p g-mesh --lib --no-run` afterwards was real
  745.1s, user 175.0s: a **rebuild**, because `-p g-mesh` unifies features
  differently from `--workspace` (as GM-540 §11 already noted). The local entry
  point therefore never uses `-p`. See section 3.4.

### 1b. Heavy set (>= 10s in both suite logs)

Sources: `gm509-s24-logs/suite.log` (2372 tests, summed test time 4844s,
median 1.22s) and `gm523/logs/suite.log` (2102 tests, sum 7468s, median
1.40s). Both are full `nextest -p g-mesh` runs under agent load. The heavy set
is every test at >= 10s in **both** runs. 75 more tests reach 10s in one run
only (load noise: most of them take 3-9s in the other run), and they are left
out.

| threshold | tests | share of summed time (S24 / GM-523) |
|---|---:|---|
| >= 10s in both | **56** (10 lib, 46 integration) | **26.4% / 36.6%** |
| >= 30s in both | 11 | 10.9% / 14.8% |

Split by kind, for the >= 10s set:

| | S24 | GM-523 |
|---|---|---|
| integration tests (`core/tests`): heavy share of their summed time | 51% | 49% |
| lib: heavy share of its summed time | 11% | 24% |

The integration share matters most locally. `core-it` runs at most 2 tests at
a time (`daemon-spawning` group), so its wall time is about its summed time
divided by 2. **Excluding the heavy set roughly halves `core-it`'s wall
time.**

The 56 tests, with their times in S24 and GM-523 (seconds):

```
lib (binary g-mesh)
  36.7  158.4  graph::containers::tests::membership_invariants_hold_after_every_diff_of_a_random_sequence   [random: knob, see 3.3]
  14.0   48.7  graph::containers::tests::bulk_batch_boundaries_do_not_change_the_result                      [random: knob, see 3.3]
  55.2  122.2  mcp::response_bound_tests::every_single_hop_implementation_page_fits_the_ceiling
  52.9  116.6  mcp::response_bound_tests::every_caller_page_fits_the_ceiling
  51.9  114.3  mcp::response_bound_tests::every_reference_page_fits_the_ceiling
  49.0  114.1  mcp::response_bound_tests::every_callee_page_fits_the_ceiling
  22.5   55.5  mcp::response_bound_tests::paging_through_byte_cut_pages_returns_every_row_once
  19.7   50.2  mcp::response_bound_tests::every_files_answer_fits_the_ceiling
  17.3   83.4  graph::symbol_links::tests::link_all_and_link_diff_agree_on_the_same_end_state
  11.4   11.2  daemon::semantic::tests::an_interrupted_pass_for_one_language_is_retried_without_rerunning_the_other
integration (binary g-mesh::<name>)
  82.4  123.9  serving_while_indexing a_walk_that_outlasts_the_bootstrap_timeout_is_waited_out_rather_than_losing_the_client
  13.6   56.2  serving_while_indexing a_restart_against_an_already_walked_project_never_reports_itself_as_still_indexing
  10.6   43.4  serving_while_indexing a_first_call_is_answered_normally_when_nothing_holds_the_walk_open
  59.7  102.5  daemon_build_staleness status_reports_a_daemon_that_an_upgrade_has_left_behind
  10.9   26.3  daemon_build_staleness shims_racing_to_replace_one_outdated_daemon_produce_exactly_one_replacement
  10.2   18.4  daemon_build_staleness a_daemon_started_from_an_older_build_is_replaced_before_it_answers_again
  28.6   92.8  state_isolation a_spawned_g_mesh_resolves_the_same_isolated_state_root
  35.4   83.7  cli_clean clean_orphaned_without_force_reports_and_deletes_nothing
  35.8   76.0  cli_clean clean_and_clean_orphaned_handle_a_front_state_dir
  26.3   86.2  cli_init init_on_a_fresh_project_creates_state_and_a_subsequent_status_shows_it_indexed
  18.3   67.7  default_export_linking usages_of_a_renamed_default_import_reach_the_class_it_really_names
  35.4   48.4  plugin_memory_limit the_generated_mcp_instructions_reflect_a_real_suspended_rust
  35.4   46.6  plugin_memory_limit a_real_rust_analyzer_over_its_memory_limit_is_suspended_and_structural_work_continues
  24.0   36.8  plugin_memory_limit with_a_memory_limit_far_above_real_usage_a_real_rust_analyzer_is_left_alone
  20.1   62.6  cli_status status_in_a_front_served_folder_prints_the_front_line_and_no_coverage
  25.9   54.6  cli_status status_reports_the_daemon_plugin_coverage_and_syntax_errors_of_a_live_project
  19.5   55.8  cli_status status_reports_a_dead_daemon_and_the_files_its_index_never_saw
  17.2   49.6  cli_status status_does_not_show_a_dead_daemons_leftover_phase_and_progress_as_live
  21.0   42.8  cli_status status_warns_about_a_project_idle_past_the_threshold
  14.3   48.0  cli_status status_on_a_project_that_was_never_indexed_reports_an_empty_state
  29.2   41.0  plugin_build_staleness a_plugin_rebuilt_under_a_running_daemon_costs_the_project_a_re_walk
  25.1   36.9  plugin_build_staleness a_rebuild_of_any_discovered_plugin_retires_the_daemon_not_only_typescripts
  13.0   56.9  replay_progress a_held_replay_with_a_token_gets_a_heartbeat_then_the_full_answer
  12.8   10.8  replay_progress a_held_replay_without_a_token_gets_no_notifications_and_the_full_answer
  14.3   50.3  ambiguous_reexport_linking find_callers_resolves_through_an_ambiguous_export_star_barrel
  21.2   28.5  release_packaging_scripts prepare_release_assets_blesses_a_complete_release_with_plugin_assets
  19.4   29.5  release_packaging_scripts prepare_release_assets_refuses_a_tampered_plugin_asset_on_windows
  20.0   21.4  release_packaging_scripts prepare_release_assets_refuses_a_plugin_asset_not_identical_to_the_main_archives_copy
  19.1   20.0  release_packaging_scripts prepare_release_assets_refuses_a_plugin_asset_packed_under_plugins
  15.9   21.6  release_packaging_scripts prepare_release_assets_refuses_a_plugin_asset_declaring_another_language
  15.5   20.0  release_packaging_scripts prepare_release_assets_refuses_a_plugin_asset_with_an_extra_file
  15.5   19.8  release_packaging_scripts prepare_release_assets_refuses_a_plugin_asset_whose_command_names_no_binary
  14.5   34.7  namespace_import_after_init a_project_prepared_with_init_answers_find_callers_through_a_namespace_import
  13.0   33.0  namespace_import_resolution find_callers_reaches_a_caller_that_went_through_a_namespace_import
  13.0   31.9  overload_call_resolution an_overloaded_function_resolves_correctly_through_the_real_mcp_tools
  20.5   20.7  plugin_check a_bulk_index_that_never_finishes_fails_session_instead_of_hanging
  10.2   16.6  plugin_check the_typescript_plugin_satisfies_its_own_expectations_file
  17.6   21.2  incremental_embed_outside_lock a_tool_call_against_an_unrelated_file_is_not_blocked_by_a_reparses_lock_free_embedding_window
  10.8   21.4  idle_lifecycle the_plugin_sleeps_alone_and_a_request_replays_only_what_it_missed
  14.2   16.7  idle_lifecycle the_core_exits_on_its_own_longer_timeout_with_nothing_attached
  11.5   16.5  plugin_bridge an_ambiguous_reexport_is_resolved_by_the_plugin_semantic_pass
  12.4   15.2  orphaned_daemon a_daemon_whose_project_root_still_exists_is_left_alone
  10.0   16.2  daemon_lifeline a_daemon_whose_lifeline_is_running_or_unreadable_is_left_alone
  12.2   12.5  go_plugin_fingerprint the_go_plugin_fingerprint_ignores_the_checkouts_git_state
  10.2   12.7  last_used a_daemon_start_and_every_handled_request_advance_last_used_on_disk
  11.5   11.2  structural_does_not_wait_for_embeddings search_code_waits_for_the_embedding_backfill_pass_but_not_forever
```

The parser and the full output (including the 75 tests that reach 10s in one
run only) are in the S1 scratchpad (`gm548/heavy.py`, `gm548/heavy.txt`).

## 2. Lib tests that touch process-global state

**Method.** g-mesh, project `g-mesh` (the main checkout; this branch has no
code changes):
- `find_references(symbol_name)` on each env-var constant a test writes:
  `MODEL_DIR_ENV`, `PLUGIN_ROOTS_OVERRIDE_ENV`, `FILE_CHANGED_TIMEOUT_ENV`,
  `SEMANTIC_PASS_PROJECT_TIMEOUT_ENV`, `SEARCH_EMBEDDING_WAIT_ENV`,
  `paths::HOME_ENV`. All results were complete (`hasMore: false`), with
  `provenance: rust semanticTier absent`. That gap only affects method calls
  through a receiver, and these are constants.
- `find_definition` on the containers random-sequence test (section 3.3).
- `find_references("set_var")` returned `no symbol named 'set_var' found`.
  g-mesh does not index `std`, so the writers were found by grep over
  `core/src`: `env::set_var|env::remove_var|set_current_dir`, statics
  (`OnceLock|LazyLock|Mutex|Atomic*|RwLock`), fixed ports
  (`127.0.0.1:<n>|localhost:<n>`), `set_global_default`/subscriber `init()`,
  `set_hook`, `setrlimit`, `umask`, signal handlers.

**Not found in the lib:** `set_current_dir`, fixed ports, global tracing
subscribers, panic hooks, rlimit, umask or signal changes.

**Statics, all safe in one process:** `symbol_links::tests::RULES`,
`query_shapes::SHIPPED`, `find_definition`'s `DISABLED`,
`daemon::plugin::FINGERPRINT` are write-once caches with the same value for
every test. `ipc::NEXT` is a counter for unique names, built for concurrency.
`process::SPAWN_LOCK` is production's own spawn lock, which exists because
threads in one process spawn children.

**The env-var writers.** Readers run on any thread in the process, so an env
write races every concurrent reader in *other* modules. The module-local
`ENV_LOCK`s only serialize writers within their own module.

| # | test(s) (lib) | variable | guard | who reads it (g-mesh `find_references`) | what a racing reader sees |
|---|---|---|---|---|---|
| 1 | `shim::tests::a_bootstrap_lock_alone_is_enough_to_record_the_project_root` | `G_MESH_HOME` | none (restores it afterwards) | `paths::g_mesh_home` via `HOME_ENV` (`paths.rs`, `cli/plugin_check/session.rs`): every state-path resolution in any module | another test's home is a temp dir that this test then deletes. **Highest risk.** |
| 2 | `mcp::search_code::tests::handle_reports_a_tool_error_when_the_configured_model_is_unavailable` | `MODEL_DIR_ENV` | none | `embedding::model::default_model_dir` | another test that loads the embedding pipeline sees "no model" |
| 3 | `mcp::search_code_wait_tests::*` (4 tests, `fixture()`) | `SEARCH_EMBEDDING_WAIT_ENV` | none, never removed | `mcp/mod.rs` only | every later mcp test in the process waits `WAIT_MS` instead of the default |
| 4 | `daemon::lifecycle::tests::a_timed_out_file_change_relaunches_the_plugin_and_replays_the_dirty_file_without_blocking_another_language` | `FILE_CHANGED_TIMEOUT_ENV` = 150ms | module `ENV_LOCK` | `RoundTripTimeouts::from_env`: every plugin spawn, in any module | a concurrently spawned plugin gets a 150ms file-changed timeout and fails |
| 5 | `daemon::lifecycle::tests::{a_default_config_resolves_to_the_documented_defaults, a_configured_plugin_idle_timeout_overrides_the_default, a_configured_core_idle_timeout_overrides_the_default, the_env_override_still_wins_over_a_configured_value, only_a_well_formed_pid_in_the_environment_is_a_lifeline}` | `PLUGIN_IDLE_ENV`, `CORE_IDLE_ENV`, `LIFELINE_PID_ENV` | module `ENV_LOCK` | daemon idle/lifeline config | a 250ms plugin idle timeout elsewhere |
| 6 | `daemon::semantic::tests::an_interrupted_pass_for_one_language_is_retried_without_rerunning_the_other` | `SEMANTIC_PASS_PROJECT_TIMEOUT_ENV` = 150 | own `ENV_LOCK` | `RoundTripTimeouts::from_env` | as #4 |
| 7 | `daemon::manifest::tests::{default_roots_bundled_entry_resolves_to_the_sibling_plugins_directory, the_override_env_var_replaces_the_entire_default_roots_list}` | `PLUGIN_ROOTS_OVERRIDE_ENV` | module `ENV_LOCK` | `daemon::manifest::default_roots`, `cli::plugin_install::install_root` | plugin discovery elsewhere finds the override dir |

That is **15 writer tests**. Add one test that is timing-sensitive rather than
stateful: `two_languages_spawn_at_the_same_time_rather_than_one_after_the_other`,
already in the `daemon-spawning` group because load breaks it. **16 tests**
stay on nextest's per-process runner. `cli::model` already shows the
recommended fix: it injects the override instead of setting it
(`sources_from`).

## 3. Decisions

### 3.1 How lib tests run in one process

**Chosen:** run the g-mesh lib's selected tests with libtest, in one process:
`cargo test --workspace --lib -- --exact <names>`. The names come from
`cargo nextest list --workspace --profile local -E <filter>`. Every other
test, including the 16 isolated lib tests, runs under nextest as today.

- `--workspace` (not `-p g-mesh`) reuses the artifacts of
  `cargo nextest run --workspace --no-run`, which every section already
  builds. `-p` caused a rebuild (section 1a). The other crates' lib binaries
  start, match none of the names, and exit: 6 extra process starts.
- The name list comes from nextest, so section membership (`^mcp::` regexes),
  the heavy exclusion (the `local` profile's `default-filter`) and the
  isolation list all have exactly one definition. libtest's own filters are
  substring matches (`mcp::` would also match a future `cli::mcp::x`), and
  `--skip` would duplicate the heavy list.
- Benefit: removes ~1765 process starts from every core lib run (5x `real`
  and -13% CPU measured on the whole lib).
- Risk: one hung test blocks the whole lib run. libtest has no per-test
  timeout, while nextest terminates at 210s. Mitigation: the script wraps the
  libtest run in a total budget (`timeout`-style, 600s) and prints "re-run the
  lib under nextest to find the hung test: `scripts/test-local.sh
  --per-process`". Risk: a test that aborts the process (`abort`, `exit`)
  hides the rest of the lib's results. None was seen; the same mitigation
  applies.
- Risk: the argument list is ~1700 names (~140 KB). That is fine for macOS
  `ARG_MAX` (1 MB) and Linux. CI never runs this path.
- Rejected: a nextest setting. nextest has no in-process mode; it always
  starts one process per test. Rejected: `cargo test -p g-mesh --lib`. It
  rebuilds, and it cannot exclude the heavy and isolated sets without a
  second list.

**Global-state tests: isolated, not fixed, in this task** (see Q2). The 16
names live in one filterset variable in `scripts/test-local.sh`, which
removes them from the libtest run and sends them to nextest. Benefit: no
production or test code changes, and the writers run alone in their own
process, so their readers elsewhere are safe. Risk: a *new* env-writing lib
test races silently until someone adds it to the list. The release gate's
`cargo test -p g-mesh` already carries that same risk today.

### 3.2 How heavy tests are marked, and who runs them

**Chosen:** a new nextest profile `local` in `.config/nextest.toml`:

```toml
[profile.local]
inherits = "default"
# Tests skipped locally only. `default` and `ci` must keep running them
# (docs/adr/0031-local-test-runs.md).
default-filter = 'not (<heavy filterset>)'
```

The heavy filterset is one `(binary_id(<id>) & test(=<name>))` term per
test, so that a same-named test in another binary is never caught.

- `default` and `ci` are unchanged, so CI (`--profile ci`), the release gate
  (`test-sections.sh run all`) and `test-sections.sh check` (which lists under
  `default`) run and count the heavy tests exactly as today. **CI coverage is
  identical.**
- Verified with nextest 0.9.146 (CI pins 0.9.143; `default-filter` dates from
  0.9.7x): `default-filter` intersects with `-E`. With a temporary config whose
  `local` default-filter was `not test(/^mcp::response_bound_tests::/)`,
  `cargo nextest list -p g-mesh --lib -E 'test(/^mcp::/)'` listed 506 tests
  under `default`, 489 under `local` and 506 under
  `local --ignore-default-filter`.
- Batch end: `scripts/test-local.sh --full` runs
  `scripts/test-sections.sh run --keep-going all` under `default`. That is the
  release gate's run: heavy included, per process, every section.
- Benefit: one place, it cannot leak into CI, and `nextest list --profile
  local` shows exactly what is skipped. Risk: the list goes stale. A test that
  becomes heavy is not in it, and a renamed heavy test silently matches
  nothing. Mitigation: behaviour test B5 (every heavy term matches exactly one
  test) and Q4.
- Rejected: `#[ignore]`. CI does not pass `--run-ignored`, so it would drop
  coverage. An env gate inside each test: 56 code edits, and a test that
  forgets the gate runs anyway.

### 3.3 Random-case knob

Both random tests use `graph::containers::tests::seeds()`, which today
supports `G_MESH_CONTAINERS_SEED=<one seed>` (replay). **Chosen:** add
`G_MESH_CONTAINERS_SEEDS=<n>`, which takes the first `n` of each test's
built-in seeds. The replay variable still wins. Unset means all seeds, so CI
and every run outside the script stay full. `scripts/test-local.sh` exports
`n=2` (2 of 8 sequence seeds, 2 of 4 batch seeds), which is about 1/4 and 1/2
of their time. It also **removes these two tests from the heavy list**, so a
graph change runs them locally, reduced, in the libtest process.

- Not `PROPTEST_CASES`: `proptest` is not a dependency (the file says so).
- Benefit: a `graph` change still exercises the containers invariants locally,
  and a failure prints its seed for replay. Risk: a bug that only seeds 3-8
  find shows up at batch end instead of in verify. See Q3.

### 3.4 Local entry point

**`scripts/test-local.sh`** (bash, `set -euo pipefail`):

```
scripts/test-local.sh [--base <ref> | --paths-from <file>] [--per-process] [--keep-going]
scripts/test-local.sh --full          # batch end: every section, default profile
```

Flow:

1. `sel=$(scripts/test-select.sh [--base <ref> | --paths-from <file>])`. With
   no `--base`, test-select's nearest-`release-*` default applies. `none`:
   print `== no sections selected` and exit 0. `full`: all 11 sections.
2. `scripts/prune-stale-objects.sh`, then
   `cargo nextest run --workspace --no-run` (one build, shared by both runners).
3. **Lib part** (only if a core lib section is selected:
   `core-mcp|core-daemon|core-cli|core-graph|core-rest`):
   `L = package(g-mesh) & kind(lib) & (<union of the selected sections' filters>) & not (<ISOLATED>)`.
   Names: `cargo nextest list --workspace --profile local -E "$L" --message-format json`.
   Run: `G_MESH_CONTAINERS_SEEDS=2 cargo test --workspace --lib -- --exact <names>`.
   With `--per-process`, skip this step and leave these tests to step 4.
4. **Everything else**, one nextest invocation:
   `G_MESH_CONTAINERS_SEEDS=2 cargo nextest run --workspace --profile local --no-tests=pass -E "(<union of selected sections>) & not (<L>)"`.
   That is `core-it`, `kind(bin)`, the 16 isolated lib tests, and the
   sdk/plugin/wire sections when selected.
5. Print one summary line per part (tests, failures, seconds) and
   `== skipped locally: <n> heavy tests (run: scripts/test-local.sh --full)`.
   Exit non-zero if any part failed. With `--keep-going`, step 4 runs even
   after step 3 fails.

A **verify brief** says:

```
scripts/test-local.sh --base release-4.3.0
```

The **batch-end slice** says `scripts/test-local.sh --full`. Its failures
become tasks in the same batch.

- Union instead of per-section invocations: locally the per-section JUnit
  split is not needed, and one invocation saves 10 listings plus 10 freshness
  checks. CI keeps per-section steps.
- Benefit: one command per situation, the same selection rule as CI, and
  nothing new for CI to trust. Risk: two runners mean two outputs to read.
  The summary lines are the contract.

## 4. Edit map

| file | change |
|---|---|
| `.config/nextest.toml` | new `[profile.local]` (inherits `default`, `default-filter = 'not (<heavy>)'`), 54 terms after removing the 2 random tests. The comment states the invariant only (`default`/`ci` must keep running these); the source logs and threshold go to the ADR, per CLAUDE.md's comment rule. `default`/`ci` untouched. |
| `scripts/test-local.sh` (new) | the entry point of 3.4, with `ISOLATED` (16 terms, section 2) and `CORE_LIB_SECTIONS`. Uses `test-sections.sh filter` for section filters, never its own copies. |
| `scripts/test-select.sh` `classify()` | add `scripts/test-local.sh` to the "machinery → full" row. Finer `core/**` rows only if Q1 says so. |
| `core/src/graph/containers/tests.rs` `seeds()` | read `G_MESH_CONTAINERS_SEEDS=<n>` (truncate the default list; a malformed or 0 value panics naming the variable); `G_MESH_CONTAINERS_SEED` keeps priority. Doc comment on both. |
| `core/tests/test_local_script.rs` (new; `recording_cargo` harness as in `test_sections_scripts.rs`) | behaviours B1-B4, B6 |
| `core/tests/test_sections_scripts.rs` | B5 (heavy terms each match exactly one test), ignored + real like `the_real_sections_partition_the_real_suite` |
| `CLAUDE.md` "Testing" | which command when: verify `test-local.sh --base <release>`, batch end `test-local.sh --full`, CI unchanged |
| `docs/adr/0031-local-test-runs.md` (new, indexed in `docs/adr/README.md`) | the decisions in section 3 |

CI workflows, `scripts/test-sections.sh` and `scripts/cut-release.sh` do not
change.

## 5. Behaviours for tests

- **B1** A `none` selection prints `== no sections selected` and never calls
  cargo.
- **B2** A selection without core lib sections (`plugin-rust` alone, by
  `--paths-from`) makes no `cargo test` call. It calls `nextest run --profile
  local` once.
- **B3** A core selection calls `cargo test --workspace --lib -- --exact`
  with the names `nextest list` returned, minus none, and `nextest run
  --profile local` with `not (<L>)`. The recorded `-E` of the second call
  excludes exactly the names given to the first (no test in both, none in
  neither).
- **B4** `--full` calls `test-sections.sh run --keep-going all` with no
  `--profile local` and no `G_MESH_CONTAINERS_SEEDS`.
- **B5** (real listing) `nextest list --profile default` minus
  `--profile local` is exactly the heavy terms, one test each, and both
  random tests are absent from that difference.
- **B6** A failing first part (recording cargo exits 101 on `cargo test`)
  makes the script exit non-zero; with `--keep-going` the nextest part is
  still called.
- **B7** (lib) `seeds()`: unset gives the full list; `SEEDS=2` gives the first
  2; `SEED=0x5` wins over `SEEDS`; `SEEDS=0` or `SEEDS=x` panics naming the
  variable. Testing this needs `seeds()` to take the env values as arguments
  (`seeds_from(default, seed, count)`), the `cli::model::sources_from`
  pattern, so the test adds no new env writer.
- **Controls:** revert `default-filter` (B5 fails); make `seeds_from` ignore
  `count` (B7 fails); drop the `not (<L>)` in step 4 (B3 fails).

## 6. Expected effect (for the measure slice)

- Core lib: ~1765 fewer process starts. Measured on the whole lib, 733s to
  146s `real` (load-confounded) and 238s to 207s CPU.
- Heavy exclusion: 26-37% of summed suite time, and about half of `core-it`'s
  wall time.
- The measure slice repeats 1a under `scripts/test-local.sh` for a core-only
  change against `test-sections.sh run $(test-select.sh)`, with `uptime` and
  `/usr/bin/time -p` per arm, and checks that step 3 does not compile
  anything (no `Compiling` line).

## Open questions for the owner

**Q1. How narrow should a `core/` change be?**
Today any path under `core/` selects all 6 core sections (`core-it` plus the
5 lib sections). For example, editing only `core/src/cli/status.rs` runs
`mcp`, `graph` and `daemon` lib tests and all of `core-it`. After this task,
the lib part is one cheap process, so the remaining cost is `core-it` (292
tests, at most 2 at a time).
- **A (Recommended): keep "all core".** Benefit: no dependency analysis; a
  `graph` change that breaks `mcp` is always caught. Risk: `core-it` runs on
  every core change, though without its heavy half.
- B: leaf modules narrow. `core/src/cli/**` selects `core-cli` + `core-it`,
  `core/src/mcp/**` selects `core-mcp` + `core-it`, anything else under
  `core/` selects all core. Benefit: a CLI-only change skips ~1400 other lib
  tests. Risk: `cli`/`mcp` code called from other modules' tests would be
  missed until batch end. Small saving, since the lib is cheap now.
- C: per-module mapping from g-mesh `get_dependencies` (incoming,
  transitive). Benefit: the most precise. Risk: a new tool in the selection
  path, and its errors become missed tests.

**Q2. Global-state lib tests: isolate now or fix now?**
Today 15 lib tests write process-wide env vars (section 2). They are safe
under nextest (one process each) and race in one process (the release gate
already runs them that way). For example, the `shim` test points `G_MESH_HOME`
at a temp dir and deletes it while another thread resolves state paths.
- **A (Recommended): isolate in this task, fix in a follow-up task in this
  batch.** The 16 names run under nextest, and the follow-up converts each
  writer to injected overrides (`sources_from` pattern) and empties the list.
  Benefit: this task stays small. Risk: the list must be maintained until
  then.
- B: fix all 15 writers in this task. Benefit: no list, and the release gate's
  race goes away too. Risk: 7 modules of test and production signature
  changes added to a tooling task, so a larger review.
- C: isolate only, no follow-up. Benefit: least work. Risk: the release-gate
  race and the list stay forever.

**Q3. Random-sequence tests locally: reduced or excluded?**
Today `graph::containers` random-sequence (37-158s) and bulk-batch (14-49s)
run in full everywhere. The owner asked for fewer cases locally.
- **A (Recommended): run reduced locally (2 seeds each, via
  `G_MESH_CONTAINERS_SEEDS`), full at batch end and in CI.** Benefit: a graph
  change is still checked against the invariants in verify, in ~10-40s inside
  the lib process. Risk: a bug only seeds 3-8 hit is found at batch end.
- B: exclude them locally with the heavy group, full at batch end and in CI.
  Benefit: the fastest verify. Risk: no random coverage until batch end.
- C: reduced locally, and CI also reduced except on `main`/release. Benefit:
  faster CI. Risk: changes CI coverage, which the task rules out.

**Q4. Keeping the heavy list current**
The heavy list is a static set of 54 exact names chosen from two logs. Tests
get faster or slower, and renamed tests silently drop out (B5 catches
renames).
- **A (Recommended): static list, re-measured at each batch-end run.** The
  batch-end slice compares JUnit times against the list and reports tests
  that crossed 10s either way as a finding. Benefit: explicit and reviewable.
  Risk: drift between batches.
- B: whole binaries, for example `binary_id(g-mesh::cli_status)` and
  `release_packaging_scripts`. Benefit: fewer terms, and new slow tests in
  those binaries are covered. Risk: also excludes the fast tests in those
  binaries (`cli_status` has fast ones too), which loses local coverage.
- C: generate the list from the last full run's JUnit. Benefit: always
  current. Risk: the local selection depends on a file from another run, and
  is not reviewable in git.

## Owner decisions (2026-10-10)

- Q1: "Узить листовые модули" (option B): `core/src/cli/**` selects `core-cli` + `core-it`,
  `core/src/mcp/**` selects `core-mcp` + `core-it`, anything else under `core/` selects all core.
- Q2: "Изолировать, починить задачей в батче (Recommended)": the env-writing lib tests stay on
  nextest in this task; a separate 4.3.0 task converts each writer to injected overrides and
  empties the list.
- Q3: "Уменьшенно локально (Recommended)": `G_MESH_CONTAINERS_SEEDS=2` locally, full at batch
  end and in CI.
- Q4: "Статический, сверка в конце батча (Recommended)": static list; the batch-end run reports
  tests that crossed 10 s either way.
