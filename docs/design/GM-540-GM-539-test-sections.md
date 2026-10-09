# GM-540 + GM-539: test sections, and running them by what a change touches

Status: draft for owner review (GM-540/S1; GM-539/S1 folded in).
Base: `release-4.3.0` at `07b8a12`. No code changes in this slice.

## Summary

- The suite (`cargo nextest list --workspace`: **2970** tests that run by
  default, plus 21 `#[ignore]`d ones, 2991 in total) is split into **11 named
  sections**: 6 for core and 1 for each other crate. A section is a nextest
  filterset. One script, `scripts/test-sections.sh`, holds the filtersets and
  runs, lists and checks them.
- A coverage check lists the suite once and lists each section once. It fails
  if any test is in no section or in two sections.
- CI builds once, then runs each section as its own step with its own timeout.
  It gets one JUnit file per section. GitHub shows each step's duration. A
  failing section does not skip the sections after it.
- GM-539 adds `scripts/test-select.sh`, which maps changed paths to section
  names. A small `select` job in CI uses it. When a path matches no rule, the
  answer is a full run. `release-*`, `main` and `workflow_call` always run in
  full.
- **One finding changes GM-539's premise.** Core's *unit* tests also use real
  plugins. 354 lib tests in 8 modules read `plugins/*/plugin.toml` or spawn a
  real plugin or SDK binary. `include_str!` also compiles every plugin's
  `plugin.toml` into core. So "a plugin-only change does not run core unit
  tests" is not safe as the task states it (question Q1).

## 1. Facts this note relies on

### Test counts (local `cargo nextest list --workspace --message-format json`)

Machine state: build `real 382s user 996s sys 190s`. Load average was 4.5 at
the start and **308** at the end. The GM-538 agent was using the machine at the
same time, so these build times are not a baseline. Listing does not depend on
timing.

| crate / kind | binaries | default | ignored |
|---|---|---|---|
| g-mesh lib | 1 | 1704 | 9 |
| g-mesh integration (`core/tests/*.rs`) | 71 | 292 | 8 |
| g-mesh bin (`main.rs`) | 1 | 0 | 0 |
| g-mesh-plugin-sdk (lib 161, bin `toy` 9, tests 119) | 9 | 289 | 0 |
| g-mesh-plugin-typescript (lib 16, tests 308) | 12 | 324 | 0 |
| g-mesh-plugin-rust | 4 | 168 | 2 |
| g-mesh-plugin-python | 3 | 160 | 2 |
| g-mesh-wire | 1 | 33 | 0 |
| **total** | | **2970** | **21** |

The task text says typescript has 16 tests. That is its lib only. Its 308
integration tests make it the third-largest crate.

### Time per area (CI run 37859404190, `main` at `936c5dd`, JUnit `time` sums)

These are sums of per-test times, not wall time. The suite runs tests in
parallel, and `core-it` runs at most 2 at a time (`daemon-spawning` group).

| area | linux | x86_64-darwin | aarch64-darwin | windows |
|---|---|---|---|---|
| core-it (292) | 379s | 887s | 538s | 396s (232) |
| core lib, all modules | 211s | 735s | 135s | 342s |
| sdk + plugins + wire | 100s | 288s | 125s | 132s |
| nextest run total (wall) | 287s | 727s | 369s | 340s |
| `cargo nextest run` step (build + run) | 8m29s | **24m42s** | 9m47s | 13m47s |

On x86_64-darwin the step took 24m42s against a step timeout of 25 minutes,
and about 12.5 of those minutes are the build. A separate build step removes
this risk regardless of the rest of this note.

### Why `fail-fast = false` (GM-246, commit `59c8333`)

"cargo otherwise stops at the first binary that fails and every binary after it
goes unreported. Windows has ~35 integration binaries whose result has never
been seen once." The setting moved to `.config/nextest.toml` in `01a6ee6`
(GM-253). The design keeps the same principle at the next level up: in CI, a
failing section must not hide the sections after it.

### Cross-file answers (g-mesh, project `g-mesh` = main checkout; the branch has no code changes)

| question | answer | call |
|---|---|---|
| Who imports `g_mesh_wire` directly | `plugins/sdk/src/{lsp/bridge,semantic,index,run,columns,lib,ids,diff,graph}.rs`, `core/src/protocol/types.rs`. Plugins reach wire through the SDK, and Cargo.toml lists wire for them directly too. | `get_dependencies(wire/src/lib.rs, Incoming, depth 1)`, `truncated: false` |
| Which core lib tests read real plugin manifests | `daemon/manifest/tests.rs` (21), `daemon/lifecycle/tests.rs` (7), `mcp/instructions/tests.rs` (3), `mcp/unlinked_tests.rs` (3), `mcp/member_name_collision_tests.rs` (2), `cli/status/tests.rs`, `daemon/plugin/tests.rs`, `graph/symbol_links/tests.rs`. Also 8 `core/tests/*.rs` files. | `find_references(symbol_id = daemon::manifest::read_manifest, answer: files)`, 62 refs |
| Which core lib tests spawn the SDK's `g-mesh-fake-plugin` | importers of `daemon::test_plugin`: `watcher/batch/tests.rs`, `daemon/{semantic,workspace_reindex,bulk_index}.rs`, `daemon/{registry,lifecycle}/tests.rs` | `get_dependencies(core/src/daemon/test_plugin.rs, Incoming, depth 1)`, `truncated: false` |
| Outline of `core/tests/common/mod.rs` (how integration tests build plugin roots) | `add_real_rust_plugin`, `rust_and_missing_python_plugin_root`, `typescript_manifest`, `missing_workspace_binary_plugin_root` | `get_file_outline` |

Checked with grep (string literals and non-code files, which g-mesh does not
track):

- `mcp/unlinked_tests.rs` walks "the real rust plugin" and uses `g-mesh-fake-lsp`.
- `mcp/member_name_collision_tests.rs` uses "one real plugin".
- `daemon/plugin/tests.rs` spawns the real TypeScript plugin.
- `include_str!` of `plugins/{typescript,go,rust,python}/plugin.toml` appears in
  `mcp/query_shapes.rs`, `cli/plugins.rs` and `daemon/manifest/tests.rs`.
- `include_str!("README.md")` appears in `cli/agent_instructions.rs`, and
  `include_str!("scripts/fetch-embedding-model.sh")` in `cli/model.rs`.
- `core/tests/release_packaging_scripts.rs` reads `scripts/*.sh` and
  `.github/workflows/release.yml`.
- `core/tests/cut_release_plugin_versions.rs` runs `scripts/cut-release.sh`. It
  does not assert on the text of its `cargo test` log lines.
- `cli/embed_eval.rs` reads `eval/embedding/variants.toml`.

Per-module counts of the 8 manifest-reading lib modules: `graph::symbol_links`
109, `daemon::manifest` 59, `mcp::instructions` 57, `daemon::lifecycle` 41,
`cli::status` 38, `daemon::plugin` 25, `mcp::unlinked` 18,
`mcp::member_name_collision_tests` 7. **354 in total.**

## 2. Sections (decision 1)

| # | section | filterset (nextest, always with `--workspace`) | default | +ignored |
|---|---|---|---|---|
| 1 | `core-it` | `package(g-mesh) & kind(test)` | 292 | 300 |
| 2 | `core-mcp` | `package(g-mesh) & kind(lib) & test(/^mcp::/)` | 506 | 507 |
| 3 | `core-daemon` | `package(g-mesh) & kind(lib) & test(/^daemon::/)` | 309 | 309 |
| 4 | `core-cli` | `package(g-mesh) & kind(lib) & test(/^cli::/)` | 308 | 308 |
| 5 | `core-graph` | `package(g-mesh) & kind(lib) & test(/^graph::/)` | 218 | 218 |
| 6 | `core-rest` | `package(g-mesh) & (kind(bin) \| (kind(lib) & !test(/^(mcp\|daemon\|cli\|graph)::/)))` | 363 | 371 |
| 7 | `sdk` | `package(g-mesh-plugin-sdk)` | 289 | 289 |
| 8 | `plugin-typescript` | `package(g-mesh-plugin-typescript)` | 324 | 324 |
| 9 | `plugin-rust` | `package(g-mesh-plugin-rust)` | 168 | 170 |
| 10 | `plugin-python` | `package(g-mesh-plugin-python)` | 160 | 162 |
| 11 | `wire` | `package(g-mesh-wire)` | 33 | 33 |
| | **sum** | | **2970** | **2991** |

`core-rest` holds storage 102, embedding 86 (+8 ignored), watcher 63,
protocol 36, languages 23, gc 14, shim 13, config 12, ipc 8, paths 4,
process 2, and the bin (0 tests).

The sum matches `cargo nextest list --workspace`: 2970 default, 2991 with the
ignored tests. Core alone is 1996 / 2013.

Design choices:

- **`core-rest` is a complement, not a list.** A new top-level module lands in
  it automatically, so the coverage check stays green without editing the
  script. The only way to break the partition is to edit a filterset.
- **Small modules share one section.** storage, embedding, watcher and protocol
  together take under 30s of summed test time even on x86_64-darwin, and each
  extra section adds a step with its own nextest startup. The task's
  suggestion of one section per module is question Q4.
- **`core-it` stays one section.** It is the longest one (linux ~190s wall,
  x86_64-darwin ~445s), but splitting it does not shorten it. Its 2-thread
  group is the limit. The JUnit file still reports it per binary (71
  `<testsuite>`s).
- **Always `--workspace` plus `package(...)`, never `-p`.** Every section then
  uses the one build CI does today, with the same feature unification. The
  plugin and SDK binaries that `core-it` and core lib tests spawn always exist,
  so the GM-505 trap cannot happen. A `-p` per section would give different
  feature sets and rebuild for each section. The GM-302 `-p g-mesh` run in
  `cut-release.sh` stays as it is: it exists to test that other configuration.

## 3. How sections are expressed (decision 2)

The filtersets live in **one script**, `scripts/test-sections.sh`, not in
`.config/nextest.toml` profiles. The reason is JUnit. CI needs the `ci`
profile's JUnit output for each section, and a nextest profile cannot inherit
from two parents. Profiles would therefore mean 11 local `sec-*` profiles plus
11 `ci-sec-*` profiles, with each filterset written twice.

With the script, both CI and local runs use the existing `default` and `ci`
profiles unchanged. These keep working:

- `fail-fast = false`
- `slow-timeout` 30s × 7
- the `daemon-spawning` group for `kind(test)`
- the override for `two_languages_spawn_at_the_same_time_rather_than_one_after_the_other`
- JUnit in `ci`

The `ci` profile already inherits overrides from `default` the same way.

Interface (bash; checked by the `shellcheck` job like every `*.sh`):

```
scripts/test-sections.sh list                    # section names, in run order
scripts/test-sections.sh filter <section>        # prints the filterset
scripts/test-sections.sh run [--profile P] [--keep-going] <section>...|all
scripts/test-sections.sh check                   # coverage check (section 4)
```

`run` does the following for each section:

- Runs `cargo nextest run --workspace --profile "$P" --no-tests=fail -E "$filter"`.
  An empty section is a bug, so `--no-tests=fail`.
- Times the section and prints `== <section>: PASS|FAIL in <N>s`.
- With `--profile ci`, moves `target/nextest/ci/junit.xml` to
  `target/nextest/ci/junit-<section>.xml`.
- If `$GITHUB_STEP_SUMMARY` is set, appends a timing row to it.

By default `run` stops after the first failing section. That section still
reports in full, because `fail-fast=false` applies inside it. `--keep-going`
runs every section and fails at the end. A section can be run alone by hand:

```
cargo nextest run --workspace -E "$(scripts/test-sections.sh filter core-mcp)"
```

`.config/nextest.toml` gets a comment that points to the script, and no new
settings.

## 4. Coverage check (decision 3)

`scripts/test-sections.sh check`:

1. Runs `cargo nextest list --workspace --run-ignored all --message-format json`
   once for the whole suite. Ignored tests are included so they are partitioned
   too.
2. Runs the same command once per section with `-E <filter>`, keeping the tests
   whose `filter-match.status` is `matches`. That is 12 list calls on one warm
   build, and none of them runs a test.
3. Passes the 12 JSON files to `scripts/test-sections-check.py full.json
   name=sec.json ...`. This pure checker uses only the standard library and does
   no cargo work. It computes, keyed on (binary-id, test name):
   - **missing**: tests in the full list that are in no section
   - **overlap**: tests that are in two or more sections, with the section names
   - **phantom**: tests a section matches that the full list does not have.
     This can only happen if the two lists come from different builds.
4. Prints the count for each section and the total. It exits 0 only if all
   three sets are empty. Otherwise it exits 1 and prints every offending test,
   one per line (`missing: <binary> <test>`,
   `overlap: <binary> <test> in core-mcp,core-rest`), never truncated.

The checker is split out so that GM-540/S3 can test it on fixture JSON (a clean
partition, a test in no section, a test in two sections) without a cargo build.
The repo already tests scripts this way from Rust in
`core/tests/release_packaging_scripts.rs`.

Where the check runs:

- In CI, on the linux row only, right after the build step. The partition does
  not depend on the platform: Windows' smaller list is a subset filtered by the
  same expressions.
- In `cut-release.sh`, before the sections run.
- In GM-540/S4 verify.

## 5. Gate and CI layout (decision 4)

### CI `test` job: steps in one job, not a matrix

A matrix over sections would mean 11 × 4 jobs, each doing its own cold build of
9-12 minutes. Steps share one build.

1. `Build the test binaries` (new, `id: build`, timeout 25):
   `cargo nextest run --workspace --profile ci --no-run`. This builds the same
   set the current step builds, including every plugin and SDK binary, since
   each of those packages has integration tests.
2. `Test sections cover the whole suite` (new, linux only, timeout 5):
   `scripts/test-sections.sh check`.
3. One step per section, 11 in all, each like this:

   ```yaml
   - name: "tests: core-mcp"
     if: ${{ !cancelled() && steps.build.outcome == 'success' }}   # GM-539 adds the selection test
     timeout-minutes: 10        # core-it: 20
     shell: bash
     run: scripts/test-sections.sh run --profile ci core-mcp
   ```

   `!cancelled()` instead of the default `success()` is the GM-246 rule one
   level up: a red `core-daemon` must not skip `core-it`. There is no
   fail-fast between sections in CI. Each step's duration on the run page is
   the per-section timing, and the script's summary row repeats it with test
   counts.
4. `G_MESH_DAEMON_LOG` moves from the old step's `env:` up to the job's `env:`,
   so every section step has it.
5. `Test results, by binary` loops over `target/nextest/ci/junit-*.xml`.
   `ci-junit-summary.py` accepts several files. `Keep the JUnit results`
   uploads `target/nextest/ci/junit-*.xml` under the same artifact name,
   `junit-<target>`. `scripts/ci-diff-runs.sh` already reads every `*.xml` in
   that directory (`read()` walks the directory), so it works unchanged.
6. The `g-mesh plugins check` step needs no change: the build step produced the
   `g-mesh` binary it reuses.

The job's `timeout-minutes: 45` stays. The step timeouts now bound the build
and each section separately. On x86_64-darwin the bounds are about 12.5 minutes
for the build and about 8 minutes for `core-it`, instead of 24m42s against one
25-minute bound.

### Release gate (`scripts/cut-release.sh` `main()`)

Today it runs `cargo test --workspace` and then `cargo test -p g-mesh`
(GM-302). Proposal (Q3): replace the first with
`scripts/test-sections.sh check && scripts/test-sections.sh run all`. The
second stays as it is. Per-section timings go to the gate's log, it fails fast
per section, and it now runs under the same tool and timeouts as CI. If cargo
is missing nextest, the gate stops with an install hint.

### Will the sections add up to more than one run? (acceptance criterion)

Today `core-it` (2 threads) overlaps with the unit tests on the other cores. In
sequential steps that overlap is lost. Estimate from the JUnit sums above:

| | linux | x86_64-darwin |
|---|---|---|
| core-it, wall ≈ sum/2 | ~190s | ~445s |
| other core sections | ~55s | ~185s |
| sdk, plugins, wire | ~25s | ~70s |
| startup, 11 nextest invocations | ~10-30s | ~10-30s |
| **sum of sections, estimated** | **~280-300s** | **~710-730s** |
| today, one run | 287s | 727s |

So the sum should be roughly equal to one run. This is an estimate, not a
measurement; GM-540/S5 measures it. If the sum is worse, the lever is to raise
`daemon-spawning` max-threads only for the `core-it` step, which then runs
alone. That is a separate decision, with the contention history in
`nextest.toml` behind it, and it is not part of this design.

## 6. GM-539: selection by changed paths (decision 5)

### Rule table

Every changed path is matched to the first row that fits. The selection is the
union of the rows. `full` means all 11 sections. Here *all-core* means the 6
core sections, and *Q1-core* depends on the answer to Q1.

| path class | sections | why |
|---|---|---|
| `wire/**` | full | sdk, all plugins and core depend on it |
| `plugins/sdk/**` | full | every plugin builds on it; core lib and core-it spawn its `fake-plugin`, `fake-lsp` and `toy` |
| `Cargo.toml`, `Cargo.lock`, `.cargo/**`, `.config/nextest.toml`, `rust-toolchain*` | full | build and runner config |
| `.github/workflows/**`, `scripts/test-sections.sh`, `scripts/test-sections-check.py`, `scripts/test-select.sh` | full | the machinery that decides what runs |
| `.gitattributes` | full | line endings on the Windows checkout |
| `core/**` | all-core | no crate depends on core |
| `plugins/{python,rust,typescript,go}/plugin.toml` | that plugin's section (none for go) + all-core | `include_str!` into core; read by 8 lib test modules |
| `plugins/{python,rust,typescript}/**` | that plugin's section + `core-it` + Q1-core | core-it and 354 lib tests spawn or read the plugin |
| `plugins/go/**` | `core-it` + Q1-core | not a cargo crate; the Go steps always run anyway |
| `README.md` (root) | `core-cli` | `include_str!` in `cli::agent_instructions` |
| `scripts/**` (others) | `core-it` + `core-cli` | release_packaging and cut_release tests read scripts; `cli::model` uses `include_str!` on `fetch-embedding-model.sh` |
| `eval/**` | `core-cli` | `cli::embed_eval` reads `eval/embedding/variants.toml` |
| `docs/**`, other `*.md`, `LICENSE*`, `.gitignore`, `.git-blame-ignore-revs`, `clippy.toml`, `rustfmt.toml` | none | the `lint` job, which is not selected, covers clippy and rustfmt |
| anything else | full | safe default for an unknown path |

`test-select.sh` prints one of three answers: `full`, a list of section names,
or `none`.

### Where the rule lives, and the base for the diff

One script serves both callers:

```
scripts/test-select.sh [--base <ref>] [--paths-from <file>]
```

- **Locally**, the default is `--base` = the merge-base with the release branch
  the task branched from. The verify brief passes it, for example
  `--base release-4.3.0`. The changed paths are `git diff --name-only
  <base>...HEAD`, plus uncommitted and untracked files. A verify slice runs
  `scripts/test-sections.sh run $(scripts/test-select.sh --base release-4.3.0)`.
  This replaces "-p each crate whose production code changed" plus the manual
  `cargo build --workspace`.
- **In CI**, a new `select` job (ubuntu, `fetch-depth: 0`) outputs `mode`
  (`full|partial|none`) and `sections`. The base depends on the event:
  - `pull_request`: `github.event.pull_request.base.sha`.
  - push to `experiment/*`: `github.event.before`. An all-zero sha (new branch),
    or a sha missing after a force-push, falls back to `full`.
  - push to `release-*` and `main`, `workflow_call` (release.yml's gate) and
    `workflow_dispatch`: always `full` (Q2).
- `--paths-from` exists for GM-539/S3's fixture tests.

How the `test` job uses the selection:

- It gets `needs: [matrix, select]`.
- Each section step's `if:` gains
  `contains(format(' {0} ', needs.select.outputs.sections), ' core-mcp ')`.
- When `mode` is `none` (docs only), the whole job is skipped with
  `if: needs.select.outputs.mode != 'none'`.
- A partial run writes "## Partial run (by change)" to the step summary, with
  the sections it ran and skipped. This is the GM-343 rule: a partial run must
  never be mistaken for a full one.
- The `matrix` (platforms), `lint`, `shellcheck`, `installer` and Go jobs are
  not selected.

### Backstop

The full suite still runs at least once per release, in three places. The
first already exists; the second and third are this design's choices.

- `cut-release.sh` runs `--workspace` locally before tagging. This already
  exists.
- `check_release_branch_ci_passed` requires a green `ci.yml` at the
  release-branch tip, and pushes to `release-*` always run full.
- The tag's `release.yml` calls `ci.yml` through `workflow_call`, which always
  runs full.

No nightly schedule is needed.

### Why the CI saving is small (for the measurement in GM-539/S5)

Selection never changes the build: it is always `--workspace`, because
`core-it` needs every plugin binary. `core-it` runs for every code change. So
in CI, selection saves at most:

- **Core-only change:** the sdk, plugin and wire sections, about 25s on linux
  and 70s on x86_64-darwin.
- **Plugin-only change under Q1 option B:** the other plugins plus sdk and wire.
- **Docs-only change:** the whole `test` job, 9-27 minutes per row. **This is
  the large win.**

Locally, the win is bigger. A verify slice stops paying for sections its change
cannot affect.

## 7. Edit map (decision 6)

| slice | file | what |
|---|---|---|
| GM-540/S2 | `scripts/test-sections.sh` (new) | `list`, `filter`, `run`, `check` as in sections 3-4 |
| GM-540/S2 | `scripts/test-sections-check.py` (new) | pure partition check over nextest list JSON |
| GM-540/S2 | `.config/nextest.toml` | comment only: sections live in the script; profiles unchanged |
| GM-540/S2 | `.github/workflows/ci.yml` `test` job | `G_MESH_DAEMON_LOG` moves to job `env`. Step `cargo nextest run` (lines 600-669) becomes the build step, the linux check step and 11 section steps. The old step's comment is reduced to invariants on the build step (see the ADR row). `Test results, by binary` (671-685) loops over files. `Keep the JUnit results` (687-695) uses the glob. |
| GM-540/S2 | `docs/adr/0030-test-sections.md` (new) + `docs/adr/README.md` index | the decisions themselves (filtersets in a script rather than profiles; steps rather than a matrix; `!cancelled()` between sections; `--workspace` + `package()` rather than `-p`). New comments in `ci.yml`/scripts state invariants only and link this ADR once, per the repo CLAUDE.md comment rule; the moved step's history comment is not copied onto the new steps |
| GM-540/S2 | `scripts/ci-junit-summary.py` `main()` | accept one or more JUnit paths |
| GM-540/S2 | `scripts/cut-release.sh` `main()` lines 605-622, plus usage lines 10 and 219 | `cargo test --workspace` becomes `test-sections.sh check` + `run all` (if Q3 = yes) |
| GM-540/S3 | test of `test-sections-check.py` (Rust integration test under `core/tests/`, following `release_packaging_scripts.rs`) | fixtures: clean, missing, overlap, phantom; plus one real-tree `check` |
| GM-539/S2 | `scripts/test-select.sh` (new) | the rule table; `--base`, `--paths-from` |
| GM-539/S2 | `.github/workflows/ci.yml` | new `select` job; `test.needs`; the `if:` on each section step; job `if` for `none`; partial-run summary |
| GM-539/S2 | `CLAUDE.md` (repo) testing section | how a verify slice selects sections. The `slice-task` skill's verify wording is in the owner's global config, outside the repo; the owner edits it. |
| GM-539/S3 | fixture test of `test-select.sh` | one path set per rule row, plus unknown path → `full` |

Order and parallel work:

- GM-540/S2 goes first: it fixes the section names and owns the `ci.yml` steps.
- GM-539/S2's `test-select.sh` can be written **in parallel** with GM-540/S2,
  because it only needs the section names fixed in this note. Its `ci.yml` edit
  goes on top of GM-540/S2, since it touches the same steps.
- GM-540/S3 and GM-539/S3 can run in parallel: different files, different
  agents.
- Each slice needs its own worktree (a parallel-agent rule).

## 8. Risks and trade-offs (decision 7)

- **Coverage check vs classification.** The check proves the sections partition
  the suite. It cannot prove the GM-539 rule table is right. A rule that is too
  narrow lets a test that a change breaks get skipped until a full run catches
  it. Mitigations:
  - unknown paths default to a full run;
  - wire, sdk and build config force a full run;
  - every release runs in full three times (see "Backstop").
- **Possible total-time regression.** Sequential sections lose the overlap
  between `core-it` and unit tests. The estimate says the totals are about
  equal. S5 measures it, and section 5 names the lever if it is worse.
- **nextest startup for each section.** Each `run` and each `list` executes the
  workspace's test binaries to list them. On this Mac that costs whatever
  per-exec latency the deps directory imposes (GM-538). If nextest does not
  skip binaries outside `package()`/`kind()` before listing them, 11 sections
  pay that cost 11 times. S5 records this, under `uptime`.
- **Filtersets in a script, not in nextest profiles.** Without the script,
  `cargo nextest run --profile core-mcp` would not work. The upside is one
  source of truth, and the JUnit file for each section uses the existing `ci`
  profile.
- **Q1: core lib tests that use real plugins.** If the owner keeps "a plugin
  change does not run core unit tests", 354 lib tests that read or spawn that
  plugin go unrun on plugin changes. These include `daemon::plugin::tests`
  (spawns TypeScript) and `mcp::unlinked::tests` (walks with the real Rust
  plugin).
- **A failing section no longer skips the rest in CI (`!cancelled()`).** One
  red section therefore produces up to 10 more steps of output. That is the
  intent of GM-246.
- **The gate moves from `cargo test` to nextest (Q3).** nextest runs one
  process per test. The 30s × 7 timeout kills a test that would have hung the
  gate. One more tool must be installed on the release machine.

## 9. Questions for the owner

- **Q1.** For a change to one plugin's sources, which core tests run?
  - **A (recommended): all core sections.** Simple and safe. It costs about 55s
    of wall time on linux and about 3 minutes on x86_64-darwin for core lib
    tests. It means amending GM-539's acceptance criterion.
  - **B: `core-it`, plus a filter over the 8 lib modules that use real
    plugins.** This keeps most of the saving. The list is hand-kept and goes
    stale when a new test elsewhere spawns a plugin.
  - **C: `core-it` only, as the criterion is written.** 354 relevant tests are
    skipped.
- **Q2.** Does a push to `release-*` run in full, or by selection?
  - **Full (recommended).** The gate's "green CI at the tip" keeps meaning the
    whole suite. The CI saving then applies to PRs and `experiment/*` only.
  - **Selection.** This saves more, but `cut-release.sh` must then check that
    the tip's green run was a full one.
- **Q3.** In `cut-release.sh`, does `cargo test --workspace` become sections
  under nextest?
  - **Yes (recommended).** The gate gets per-section timings and the same
    runner as CI.
  - **No.** Sections stay a CI and verify feature only.
- **Q4.** Section granularity?
  - **11 as listed (recommended).**
  - **15, with storage, embedding, watcher and protocol as separate sections,**
    as the task text suggested. This adds 4 near-empty steps.

## 10. Owner decisions (2026-10-09)

- **Q1: "Все секции core".** A change to one plugin's sources runs that
  plugin's section plus all six core sections. GM-539's acceptance criterion
  ("a plugin-only change does not run core unit tests") is amended to match.
- **Q2: "По выбору путей".** Pushes to `release-*` are selected by changed
  paths like any other push, not always full. `main`, `workflow_call` and
  `workflow_dispatch` stay full. Consequence: a green `ci.yml` at the release
  tip no longer implies the full suite, so `cut-release.sh`'s
  `check_release_branch_ci_passed` must also confirm that the tip has a green
  **full** run (the `select` job's `mode` output, or the partial-run summary
  marker), and refuse otherwise with a hint to run `ci.yml` by
  `workflow_dispatch`. This belongs to GM-539/S2, and its tests to GM-539/S3.
- **Q3: "Да, на секции".** The gate's `cargo test --workspace` becomes
  `test-sections.sh check` + `run all`; the GM-302 `-p g-mesh` run stays.
- **Q4: "11, мелкие вместе".** The 11 sections in section 2 as listed.

## 11. Results (S9, 2026-10-09)

Measured at c827cfa. Verdict: **the sum of the sections is not worse than one
run**, in CI on every platform and locally in CPU time.

### CI

Branch run 37948088042 (`workflow_dispatch`, target `all`, green) against main
run 37859404190 (936c5dd) and release-4.2.0 run 37856293674 (3ea8a7e). On main,
the single `cargo nextest run` step includes the build. The branch splits it
into `Build the test binaries` plus 11 section steps, so the comparable number
is build + sections. "Tests" is the sum of nextest's `Summary [..]` times.
All values are in seconds.

| platform | main step (build / tests) | 4.2.0 step (build / tests) | branch build + sections (build / tests) | sections steps wall | main job | 4.2.0 job | branch job |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| x86_64-linux | 509 (202 / 287) | 514 (224 / 286) | 482 (222 / 254) | 260 | 590 | 613 | 596 |
| aarch64-darwin | 587 (211 / 369) | 747 (319 / 415) | 681 (281 / 388) | 400 | 702 | 900 | 828 |
| x86_64-darwin | 1482 (728 / 727) | 1113 (513 / 583) | 918 (387 / 512) | 531 | 1762 | 1345 | 1079 |
| windows | 827 (465 / 340) | 833 (478 / 335) | 781 (453 / 310) | 328 | 1001 | 1013 | 979 |

- The overhead of 11 nextest invocations is small. The section steps' wall
  time minus the sections' summed test time is 6s on linux, 12s on
  aarch64-darwin, 20s on x86_64-darwin and 18s on windows. Each invocation
  re-checks freshness in 0.2-0.6s and lists the binaries.
- The sections' test time is lower than one run's on linux, x86_64-darwin and
  windows. On aarch64-darwin it is 388s, which lies between main's 369s and
  4.2.0's 415s. That gap is runner variance, not a section cost: core-it alone
  is 276s there, and the other ten sections add 112s. The branch's aarch64 job
  is 126s longer than main's, but 70s of that is the build step (281 vs 211)
  on a different runner.
- The branch runs 2979 tests against 2970 on main and 4.2.0; the branch adds
  tests.

### Local warm-build A/B

The machine was shared, with another agent's full verify suite running.
Load ranged from 120 to 450 during the runs, so `real` is load-bound and is
not comparable. `user+sys` is the comparable number. The warm-up was
`cargo build --workspace` + `nextest --no-run`. GM-538's
`prune-stale-objects.sh` ran on this target before each arm.

Arm A is `cargo nextest run --workspace --no-fail-fast`, not `-p g-mesh` as
briefed, because the sections run `--workspace`. `-p g-mesh` would test a
different set (core only) under a different feature unification, which means
a rebuild. Arm B is `scripts/test-sections.sh run --keep-going all`. Both arms
ran 2979 tests, all passing.

| run | user+sys (s) | real (s) | load before / after (1-min) |
| --- | ---: | ---: | --- |
| A1 | 988.9 | 1750 | 401 / 129 |
| B1 | 880.0 | 1713 | 120 / 407 |
| A2 | 748.5 | 1623 | 404 / 328 |
| B2 | 729.5 | 2037 | 311 / 148 |

In each A/B pair, B used no more CPU than A: -11% and -3%. The means are A
868.7s and B 804.7s. B2's longer `real` came mostly from core-it, which took
1152s against 805s in B1, under the verify suite's load. It is not a cost of
splitting.

## 12. GM-539/S9: CI measurement (2026-10-09)

### Method

Four throwaway branches off `8aa5a54`, each with one comment-only commit,
pushed as `experiment/GM-539-<kind>` and deleted afterwards. A push to a new
branch has an all-zero `github.event.before`, which the `select` job turns into
`full`. So each branch was first pushed at `8aa5a54` itself, then the probe
commit was pushed on top. The second push's `before` is `8aa5a54`, and its
run diffed exactly the probe commit. The first push's runs were cancelled by
the workflow's `cancel-in-progress` concurrency group.

Baseline: `main` run 37859404190 (`936c5dd`), the single `cargo nextest run`
step before sections. The probe runs overlapped one another and a
`workflow_dispatch` run on GM-540, so wall times carry runner noise: the wire
run is a full run, yet its x86_64-darwin job took 36.4m against the baseline's
29.4m.

### Results

| change (probe path) | run | `select` output | linux | win | arm64-mac | x86-mac | runner min (all jobs) |
|---|---|---|---|---|---|---|---|
| baseline, `main` | 37859404190 | (no select job) | 9.8m | 16.7m | 11.7m | 29.4m | 71.8 |
| plugin (`plugins/go/walk.go`) | 37948206534 | partial: 6 core sections | n/a | n/a | n/a | n/a | 9.5 (failed early) |
| core (`core/src/lib.rs`) | 37948206213 | partial: 6 core sections | 9.2m | 13.1m | 12.8m | 29.1m | 68.6 |
| wire (`wire/src/lib.rs`) | 37948205509 | full: 11 sections | 8.4m | 17.9m | 13.3m | 36.4m | 80.0 |
| docs (`docs/adr/0030-test-sections.md`) | 37948205182 | none: `test` job skipped | 0 | 0 | 0 | 0 | 3.7 |

The platform columns are `tests (<target>)` job wall times.

- **The selection is right in all four cases.** Plugin and core select the 6
  core sections (`plugins/go/**` has no section of its own, and Q1 puts all
  core sections on a plugin change). Wire selects `full`. Docs selects `none`,
  and the `test` job is skipped.
- **The plugin run measured nothing about time.** The probe comment broke
  `gofmt` (no blank line before a trailing top-level comment). The Go step
  failed on every platform before the build. This is the probe's fault, not
  selection's, and the run was not re-pushed. It selected the same sections
  as the core run, so the core row stands for its expected time.
- **The core run's linux failure is unrelated to the change.** `core-it`
  `plugin_check::a_bulk_index_that_dies_quotes_what_the_plugin_said_about_it`
  failed because bulk run 2 wrote nothing to stderr, an intermittent
  stderr-capture result on a comment-only change. Every later section step
  still ran, so the linux time is complete.
- **The sections a core change skips cost little.** In the full wire run, the
  `sdk`, three plugin and `wire` steps took 0.8m on linux, 1.0m on windows,
  1.1m on arm64-mac and 3.4m on x86-mac, about 6 runner minutes per run. That
  is what a core or plugin change saves. It is within the run-to-run noise
  seen here, as section 6 predicted: the build (3.4-14.9m) and `core-it`
  always run.
- **A docs-only change is the large win:** 3.7 runner minutes against 71.8,
  and no wait on x86_64-darwin.
