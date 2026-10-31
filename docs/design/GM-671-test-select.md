# GM-671: narrow `test-select` for plugin-only diffs; seed task worktrees' target

Status: design (GM-671/S2). Before-numbers: GM-671/S1. Nothing here changes CI
or `scripts/test-local.sh --full`.

## 1. Today

`scripts/test-local.sh --base <ref>` runs what `scripts/test-select.sh --narrow`
prints. `classify()` (test-select.sh:140-163), first matching row wins:

| path | selection |
|---|---|
| `wire/*`, `plugins/sdk/*`, cargo/nextest config, selection machinery | `full` |
| `core/src/cli/*`, `core/src/mcp/*` (narrow only) | that lib section + `core-it` |
| `core/*` | all 6 core sections |
| `plugins/{python,rust,typescript}/*` (:152-155) | `plugin-<p>` + all 6 core sections |
| `plugins/go/*` (:156-159) | go tests + all 6 core sections |

S1 measured, release-4.21.0 base, load 30-130 (shared machine):

| diff | selected | tests | real | user | sys |
|---|---|---:|---:|---:|---:|
| A: comment in `plugins/typescript/src/project/facts.rs` | plugin-typescript + 6 core | 2880 (lib 2078, nextest 802) | 955.6 | 524.7 | 250.0 |
| B: comment in `core/src/config/mod.rs` | 6 core | 2501 (lib 2064, nextest 437) | 663.8 | 310.8 | 210.4 |

What diff A pays for, from S1's per-test durations (`nextest.log`, summed test
seconds, not wall time):

| group | tests | summed s | does a TS-plugin change reach it? |
|---|---:|---:|---|
| core lib (one process) | 2064 | 220 s **real** | only the few lib tests that spawn the real plugin (section 2.2) |
| `g-mesh::scripts` binary (shell-script tests) | 131 | 242 | no: they run scripts in scratch repos; none spawns a plugin |
| `g-mesh::it`, modules with no TS marker | 51 | 73 | no (marker heuristic, 2.2) |
| `g-mesh::it`, modules with a TS marker | 255 | 957 | yes: they spawn the daemon on `.ts` fixtures |
| plugin-typescript | 365 + 14 lib | 123 | yes |

So 77% of A's tests, and roughly 40% of its wall time, test code the diff cannot
reach. B is different: a core change can break any integration test, and its
lib part is only 84 s of 664 s (2.4).

## 2. How the core tests reach a plugin (structural findings)

### 2.1 The spawn sites

Every real plugin process is started by one of three `Command::new(&manifest.command)`
sites in core: `daemon::plugin::PluginProcess::spawn` (core/src/daemon/plugin.rs:606/749),
the bulk walk (core/src/daemon/bulk_index.rs:356) and `g-mesh plugin check`
(core/src/cli/plugin_check/session.rs:373, :1633). None calls `env_clear`/`env_remove`
(grep over core/src and plugins/sdk/src, production files): a plugin inherits the
environment of the daemon, which inherits the test's.

Which plugin a daemon spawns is decided at run time, not by the call graph:
`daemon::manifest::default_roots` (manifest.rs:57) scans `G_MESH_PLUGIN_ROOTS_OVERRIDE`
or `bundled_roots()` = `CARGO_MANIFEST_DIR/../plugins`, i.e. the checkout's real
plugins, and the walk spawns every plugin whose extensions occur in the fixture.
So "this test spawns the TS plugin" is a property of its fixture, which no
call-graph query can answer exactly. g-mesh calls behind this:

- `find_callers PluginProcess::spawn` -> production: `PluginSupervisor::{start, replay_pending, with_awake_exclusive_access, ensure_fresh_with}` (daemon/lifecycle.rs), `daemon::semantic::run_once`; tests: 3 in `daemon::plugin::tests`, `Project::spawn` (it/incremental_matches_full_reindex.rs), `a_killed_plugin_process_is_transparently_relaunched…` (it/plugin_crash_recovery.rs), `spawn_plugin` (it/repeated_edits_through_a_warm_plugin.rs); `untypedReceiverCalls` 1 in daemon/memory.rs.
- `find_callers default_roots` -> `daemon::run`, `discovered_fingerprint`, `cli::{deps::plugin_extensions, init::init, plugin_install::warn_if_shadowed, reindex::reindex, status::collect}`, `Project::backdate_last_used` (it/cli_status.rs). Every daemon a test starts therefore uses the real plugins unless the test overrides the roots.
- `find_references PLUGIN_ROOTS_OVERRIDE_ENV` -> 11 files override it (cli_deps, daemon_log_lines, gitignore_reevaluation, offline_changes_reconcile, plugin_build_staleness, plugin_install, semantic_pass_trigger, symlink_table_routing, worktree_seed, cli/plugin_install.rs, manifest.rs); the rest of `core/tests/it` runs against the real plugins.
- `find_references typescript_manifest` -> two declarations: `daemon::plugin::tests::typescript_manifest` (callers: the 3 lib tests above) and `common::typescript_manifest` (core/tests/it/common/mod.rs:502). The latter returns **no resolved callers** but `unlinkedUsages` in overload_declaration_storage, embedding_generation_pipeline, incremental_matches_full_reindex, plugin_crash_recovery, repeated_edits_through_a_warm_plugin (calls through `common::`; a g-mesh resolution gap, recorded here, used via the `files` list).

### 2.2 Who spawns what, by marker

A marker = a string literal ending in one of the plugin's extensions, `plugins/<p>`,
`g-mesh-plugin-<p>`, or the plugin's config files (`tsconfig`, `package.json` for
TS). Over the 84 modules of `g-mesh::it`:

| plugin | modules with a marker | tests in them |
|---|---:|---:|
| typescript | 64 | 255 run locally (282 `#[test]`) |
| rust | 21 | - |
| python | 14 | - |
| go | 9 | - |
| none | 8 (mcp_e2e, state_isolation, shim_bootstrap, …) | 51 |

TS is the default fixture language, so a module-level map keeps most of `it` for
a TS diff; the saving is the core lib and the scripts binary. Core lib tests that
reach a real plugin also exist: `daemon::plugin::tests` (3, via
`typescript_manifest`), `daemon::registry::tests::two_languages_spawn_at_the_same_time…`
(the one `ISOLATED` test, test-local.sh:45), and candidates in
`daemon::{tests, activation::tests, config_reindex::tests}`, `cli::status::tests`,
`mcp::{unlinked_tests, member_name_collision_tests}` (they name `bundled_roots`/
`../plugins`/`discover(`; whether they spawn is a run-time fact).

Marker caveats: a module can spawn TS through a fixture directory under
`core/tests/fixtures/` without a `.ts` literal in its source, and a marker can be
a string the test never indexes. Markers are a floor, not the truth.

### 2.3 What else a plugin change reaches

- `plugins/<p>/plugin.toml` is read by core lib tests (`include_str!` in
  daemon/manifest/tests.rs:896; `read_manifest` in several modules) and by
  `g-mesh::scripts` (release_packaging_scripts). `plugins/<p>/Cargo.toml` is read
  by cut_release_plugin_versions. Both must keep today's wide selection.
- `plugins/sdk`: core does not depend on it (core/Cargo.toml), but every Rust
  plugin and the toy/fake plugins core tests spawn do. Stays `full`.
- `wire/`: core lib depends on it. Stays `full`.

### 2.4 Core-only diffs (B)

B's lib part is 84 s; the nextest part (core-it, 437 tests) is 560 s. Narrowing
core lib further saves at most ~84 s. The one cheap, safe cut is the
`g-mesh::scripts` binary (131 tests, ~200 summed s, about 15% of B's wall time):
only release_packaging_scripts uses core (`read_manifest`, `languages::CATALOGUE`).
Doing it means splitting `core-it` into two sections (a CI partition change). Not
recommended inside GM-671 (decision D4).

## 3. Options for the plugin rows

**O1. Checked-in per-module map + static guard.** A data file lists, per
`g-mesh::it` module and per core lib test module, the plugins it spawns. A new
non-partition selector `core-spawns-<p>` resolves to a filterset built from it. A
layout test (plain file reading, like `layout.rs`) fails when a module has no row,
or when a module with a `<p>` marker has a row without `<p>`.
Benefit: no production code; selection is readable; a new test file cannot be
left out silently (it needs a row). Risk: the marker floor misses spawns through
fixture dirs or helpers; a new test inside an existing module inherits the row,
which is right only if the row is right.

**O2. Run-time observed map.** A script replaces each built plugin binary in
`target/debug` with a wrapper that appends `$NEXTEST_BINARY_ID $NEXTEST_TEST_NAME <p>`
(nextest 0.9.146 exports both; `strings $(which cargo-nextest)`) to a log and
`exec`s the real binary, runs the core sections, restores the binaries, and
writes the map. Benefit: exact (it sees fixture-dir and helper spawns, and lib
tests); no production code. Risk: needs a ~10-15 min core run to regenerate; a
heavy (CI-only) test is observed only with `--profile ci`; depends on env
inheritance through shim -> daemon -> plugin (no `env_clear` today, 2.1).

**O3. Coarse rule, no map.** TS diff -> `plugin-typescript` + `binary_id(g-mesh::it)`
+ `core-daemon`. Benefit: tiny change, nothing to maintain. Risk: keeps all 339
`it` tests and 398 daemon lib tests; misses lib spawners outside `daemon::`
(`cli::status::tests`, `mcp::…`) unless widened again.

**O4. Per-test map (O2 at test granularity).** Benefit: the tightest selection.
Risk: the map churns with every new test, and the guard can no longer be static
(a new test in a mapped module is invisible to it). Gain over O1 is small: a
module that writes a `.ts` fixture mostly does so in every test.

## 4. Recommendation

**O1 + O2: a checked-in per-module map, generated and validated once by O2's
observer, kept correct afterwards by O1's static guard.**

1. `scripts/test-plugin-spawns.txt` (new), one row per module:
   ```
   # <binary>:<module path>   <plugins it spawns, or none>
   it:absent_language_field          python rust typescript
   it:mcp_e2e                        none
   lib:daemon::plugin::tests         typescript
   lib:daemon::registry::tests       go python
   ```
   `it:` rows cover every module declared in `core/tests/it/main.rs` (except
   `common`); `lib:` rows name only core lib test modules that spawn a plugin.
2. `scripts/test-sections.sh filter core-spawns-<p>` (p in typescript, rust,
   python, go) builds
   `package(g-mesh) & ((binary_id(g-mesh::it) & test(/^(m1|m2|…)::/)) | (kind(lib) & test(/^(daemon::plugin::tests|…)::/)))`
   from the map. These names are **not** in `SECTIONS`/`list`, so `check` and the
   partition are untouched; `run` and `filter` accept them.
3. `test-select.sh --narrow`, plugin rows:
   - `plugins/<p>/plugin.toml`, `plugins/<p>/Cargo.toml` -> today's row
     (`plugin-<p>` + all core sections; for go, go tests + all core).
   - any other `plugins/{typescript,rust,python}/*` -> `plugin-<p> core-spawns-<p>`.
   - any other `plugins/go/*` -> go tests + `core-spawns-go`.
   - Without `--narrow` (CI): unchanged.
   - `scripts/test-plugin-spawns.txt` joins the `full` row (selection machinery).
4. `scripts/test-plugin-spawns.sh observe [--profile ci]` (new): O2's observer.
   Used in the implement slice to generate the first map, and by hand when the
   guard or a reviewer doubts a row. Its output is diffed against the map;
   observed spawns missing from the map are an error, map rows with no observed
   spawn are a warning (over-inclusion is safe).
5. Guard `core/tests/it/plugin_spawn_map.rs` (new module, file reading only, ms):
   (a) every `it` module has exactly one row and every row names an existing
   module; (b) a module whose source, or a `core/tests/fixtures/<dir>` it names,
   holds a `<p>` marker lists `<p>`; extensions are read from
   `plugins/<p>/plugin.toml`, not hard-coded; (c) a core lib test file that names
   `plugins/<p>`, `typescript_manifest`, `bundled_roots` or `PluginProcess::spawn`
   has a `lib:` row or an explicit `lib:<module> none`. It runs in `core-it`, so in
   CI and in every narrowed run.
6. `plugins/sdk/*`, `wire/*`: stay `full` (2.3). Core sub-areas: not in this task
   (2.4, D4).

Why not O3: it keeps ~740 tests that cannot fail from a TS change and still
misses lib spawners. Why not O4: churn and a run-time-only guard for a small gain.

### Expected after-numbers (to be measured by the measure slice, same method as S1)

| diff | selected | tests | real (S1 load) | change |
|---|---|---:|---:|---|
| A (TS src) | plugin-typescript core-spawns-typescript | ~650 (lib 14; nextest ~365 + ~255-280 it + lib spawners) | ~550-620 s (build/prune ~22 s, lib ~10-15 s, nextest ~1080 summed s at S1's ~2x parallelism) | ~-35-40% real, ~-77% tests; user+sys should fall more than real (the 2064 lib tests are gone) |
| B (core) | unchanged | 2501 | ~664 s | none (D4) |
| A' (`plugins/typescript/plugin.toml`) | unchanged (wide) | 2880 | ~956 s | none |

The real-time figure is the least certain: it-ts tests dominate and depend on load.
Compare arms by test count and user+sys, with `uptime` before and after.

## 5. Target seeding for new task worktrees

Today: no script creates task worktrees. The orchestrator runs `git worktree add`
by hand, then `npm ci` in `plugins/typescript` and `plugins/python`, then a cold
`cargo build --workspace` (S1: ~279 s total, 265 s of it the build, at load ~157).
The only `cp -c -R` seeding in use is for control worktrees (slice-task skill).

Proposal: `scripts/new-task-worktree.sh <dir-suffix> <branch> [--base <ref>] [--no-seed]`:

1. `git worktree add ../g-mesh-wt-<suffix> -b <branch> <base>`; refuse an
   existing directory (names collide: wt-340 vs wt-gm340).
2. `npm ci` in `plugins/typescript` and `plugins/python` of the new worktree.
3. Seed, unless `--no-seed`: if `pgrep -fl -- "--out-dir <main>/target"` prints
   anything, skip seeding with a message (never wait, never kill). Else
   `cp -c -R <main>/target/debug <wt>/target/debug` (only `debug`: the main
   target is 76 GB with `release`, `nextest`, `flycheck0`, `test-local`; not
   cloned). APFS clone: no space until written.
4. `CARGO_TARGET_DIR=<wt>/target scripts/prune-stale-objects.sh` on the clone
   (the main checkout's deps holds ~96k files; never pruned from here).
5. Print the path and the next command (`cargo build --workspace`); no build.

Benefit: a warm start for every task worktree and one place for the setup steps.
Risk: cargo may still rebuild workspace crates if any fingerprint depends on the
absolute path (third-party crates should be reused regardless); a clone of a
target that another build is writing is caught only by the `pgrep` check;
clone time for ~100k files is unmeasured. Must-confirm M3.

## 6. Missed-test control (acceptance) and the other controls

One control per behaviour, run by the verify slice in a throwaway worktree.

- **C1, missed-test control (selection keeps core spawners).** Break: in
  `plugins/typescript/src/main.rs`, before `run(...)`, exit 70 when
  `NEXTEST_BINARY_ID` is `g-mesh` or starts with `g-mesh::` (the plugin was
  launched, through a daemon, by a core test binary). Plugin tests have ids
  `g-mesh-plugin-typescript…` and do not trip it. Three runs:
  (1) `test-local.sh --base <base>` with the new rules -> FAIL, failures named in
  `core-spawns-typescript` tests (it and lib); (2) `test-sections.sh run plugin-typescript` -> PASS
  (only core spawners see the break); (3) the old narrowed selection minus core
  (`plugin-typescript` alone) is what a selection that dropped the spawners
  would run, so (2) passing is the visible miss. The break is deliberately
  synthetic: it makes every core spawn fail and no plugin test, so it measures
  exactly "the narrowed run contains the core tests that spawn the TS plugin".
  Optional realism check: a real behaviour break that `plugin-typescript` misses
  and a core spawner catches; if the verify slice cannot find one in 2 tries,
  it reports that instead.
- **C2, map guard.** Add `core/tests/it/gm671_probe.rs` (declared in main.rs)
  writing a `.ts` fixture: (a) no map row -> `plugin_spawn_map` fails naming
  the module; (b) row `none` -> fails naming `typescript`.
- **C3, selection rules.** New tests in core/tests/scripts/test_select_scripts.rs:
  TS src path with `--narrow` -> `plugin-typescript core-spawns-typescript`;
  `plugins/typescript/plugin.toml` -> today's wide answer; same paths without
  `--narrow` -> today's answer (the existing
  `a_plugin_change_runs_that_plugins_section_and_every_core_section_but_no_other_plugin`
  stays as is). Control: revert the classify edit, the first new test fails.
- **C4, filter from the map.** `test-sections.sh filter core-spawns-typescript`
  names exactly the map's typescript rows; `test-sections.sh check` still passes
  (partition untouched). Control: drop one row, the filter test fails.
- **C5, observer.** `observe` on the real tree reports `plugin_crash_recovery`
  and `daemon::plugin::tests` for typescript. Control: run it with the wrapper
  disabled (log empty) -> it must error "observed nothing", not write an empty map.
- **C6, seeding script.** Busy-writer branch (PATH-stubbed `pgrep` that prints a
  line) -> no `target/` created, exit 0 with the skip message. Control: stub
  prints nothing -> `target/debug` exists.
- **`--full` unchanged:** `test-local.sh --full --dry-run` output identical
  before/after (diff of the two dry runs).

## 7. Edit map

| file | where | change |
|---|---|---|
| `scripts/test-plugin-spawns.txt` | new | the map (section 4.1), generated by `observe`, then reviewed |
| `scripts/test-plugin-spawns.sh` | new | `observe [--profile ci]` (wrapper install/restore with a trap, run core sections, write/diff map) |
| `scripts/test-sections.sh` | header invariants (:11-20); `section_filter` (:41-55) | `core-spawns-<p>` cases via a `spawns_filter <p>` helper reading the map; usage text; `list` unchanged |
| `scripts/test-select.sh` | header invariants (:22-30); `full` row (:144); plugin rows (:152-159); output loop (:187-192) | rules of 4.3; print `core-spawns-*` after the `SECTIONS`-ordered names |
| `scripts/test-local.sh` | none expected | `core-spawns-*` are not in `LIB_SECTIONS` (:49), so their lib tests run in the nextest part; the dry-run listing already prints them |
| `core/tests/it/plugin_spawn_map.rs` + `mod plugin_spawn_map;` in `core/tests/it/main.rs` | new | guard of 4.5 |
| `core/tests/scripts/test_select_scripts.rs` | after `a_plugin_change_runs_…` (:169) | C3 tests |
| `core/tests/scripts/test_sections_scripts.rs` | new test | C4 |
| `scripts/new-task-worktree.sh` + `core/tests/scripts/new_task_worktree_script.rs` | new | section 5, C6 |
| `docs/adr/0031-local-test-runs.md` | the narrowing paragraph | plugin rows and the map; the guard |
| `plugins/typescript/src/main.rs` | control only, never committed | C1 break |

## 8. Behaviours

- B1 A diff only under `plugins/<p>/` (not `plugin.toml`/`Cargo.toml`), narrowed,
  runs `plugin-<p>` plus exactly the core tests the map says spawn `<p>`.
- B2 A `plugin.toml`/`Cargo.toml` change, any non-narrowed call, `--full`, sdk and
  wire changes select what they select today.
- B3 Every `it` module has a map row; a marker without its plugin in the row fails
  `core-it`.
- B4 `observe` reproduces the map or names the difference.
- B5 `new-task-worktree.sh` never clones a target another build writes into, and
  never touches the main checkout's files.

## 9. Must-confirm (implement slice, before relying on it)

- M1 The environment reaches the plugin: one test (plugin_crash_recovery, and a
  daemon-spawning one such as idle_lifecycle) shows up in the `observe` log.
  If `NEXTEST_*` does not survive shim -> daemon, C1 and O2 need another
  discriminator (stop and report).
- M2 Lib spawners: the observed `lib:` rows; if they are many (>~100 tests), move
  `core-spawns-*`'s lib part into test-local's lib process (`LIB_SECTIONS`) instead
  of nextest.
- M3 Seeding: time `cp -c -R` of `target/debug` and the following
  `cargo build --workspace` (`/usr/bin/time -p`, `uptime`) against S1's cold 265 s;
  if workspace crates rebuild fully, say so (the saving is then third-party only).
- M4 Wrapper side effects during `observe`: plugin_build_staleness and the
  fingerprint tests hash/stat plugin binaries and may fail under the wrapper;
  `observe` ignores test failures, it only records spawns.

## 10. Owner decisions

- D1 Map granularity: per module (recommended) vs per test (O4).
- D2 Map source: generated by `observe` once + static guard (recommended) vs
  static markers only (O1 without O2: no run, less exact).
- D3 `new-task-worktree.sh` lives in the repo's `scripts/` (recommended: it knows
  the layout and gets a test) vs `~/.claude/hooks/` (no repo churn, untested).
- D4 Core sub-areas (`core-it` split to cut the scripts binary, ~15% of a core
  diff): a follow-up task (recommended) vs inside GM-671 (a CI partition change).
