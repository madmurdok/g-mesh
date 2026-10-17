# GM-623: merge core's integration-test binaries

Status: design (S1). Follows GM-612 (`docs/design/GM-612-x86-macos-ci-timeout.md`,
option C), which raised the x86_64 macOS limits as the short fix and named
this as the fix for growth.

## Today

- `core/tests/` holds 87 top-level `*.rs` files (at a029b5b). Cargo
  auto-discovers each as its own integration-test crate, so the build compiles
  87 crates and **links 87 executables**, each against the `g_mesh` lib and
  its native dependencies (ONNX Runtime bindings, bundled SQLite, Oniguruma).
  Locally each is ~24-28 MB (`target/debug/deps/cli_init-*` 23.6 MB,
  `plugin_check-*` 27.8 MB); 68 test executables over 50 MB-class sum to
  4.5 GB in the main checkout's `target/`.
- Shared helpers: `common/mod.rs` (507 lines, `#![allow(dead_code)]`,
  declared by 61 files as `mod common;`), `typescript_registry/mod.rs`
  (132 lines, declared by 9 unix-only files), `log_line/shared.rs`
  (`include!`d by `log_line_atomic.rs` **and** by
  `plugins/sdk/tests/log_line_atomic.rs` via
  `include!("../../../core/tests/log_line/shared.rs")`). Data in
  `fixtures/`, read through `env!("CARGO_MANIFEST_DIR")` + `tests/fixtures/...`.
- 29k lines of test code in total; 6 files (3.4k lines) test repo scripts
  rather than the product (see grouping).
- No `[[test]]` entries, no `harness = false`, no `autotests` setting in
  `core/Cargo.toml`.

CI cost (baseline, fetched for this note with `gh api .../runs/<id>/jobs` and
`gh run view --log`; "test bins" = start of the last workspace crate's
`Compiling` to the build step's `Finished`, GM-612's method):

| run | target | job | `Build the test binaries` | test bins | `tests: core-it` |
|---|---|---|---|---|---|
| 38973862380 main 4.10.0 | x86_64 macOS | 42.4 | 14.6 | 9:25 | 13.6 |
| 38955950507 release-4.10.0 | x86_64 macOS | 45.6 | 16.4 | - | 13.2 |
| 38973862380 main 4.10.0 | Linux | 14.3 | 4.4 | 3:10 | 4.6 |
| 38955950507 release-4.10.0 | Linux | 12.5 | 4.0 | - | 3.6 |
| 38803570019 (1) main 4.9.0 (GM-612) | x86_64 macOS | 45.8 | 17.9 | 11:29 | 15.4 |

4.10.0 ran 45.6 min on x86 macOS: past the old 45-minute limit again, three
days after GM-612. The 75-minute limit holds, but growth continues.

## Grouping

**Recommended: two binaries** (decision D1):

| binary (nextest id) | dir | what goes in | files today |
|---|---|---|---|
| `g-mesh::it` | `core/tests/it/` | everything that exercises the product: the lib, the `g-mesh` binary, daemons, shims, plugins | 81 |
| `g-mesh::scripts` | `core/tests/scripts/` | tests whose subject is a file under `scripts/` (run it, parse its output) | 6: `test_sections_scripts`, `test_select_scripts`, `test_local_script`, `prune_stale_objects_script`, `release_packaging_scripts`, `cut_release_plugin_versions` (+ any new one from GM-627/GM-628) |

Placement rule (written into each `main.rs`'s module doc and into the repo
`CLAUDE.md`): *a test whose subject is a `scripts/` file goes to
`scripts/`; every other core integration test goes to `it/`*.
`nextest_wait_budgets` reads `.config/nextest.toml` but checks
`common`'s constants, so it goes to `it/` (it needs `common`).

Why two and not one:

- Link saving is the same order: 87 links become 2 (and the `scripts`
  binary references little of `g_mesh` - `release_packaging_scripts` uses
  `daemon::manifest::read_manifest` and `languages::CATALOGUE` only - so its
  link is small).
- Release and tooling tasks (GM-627, GM-628 in this batch; most release
  batches) edit only script tests. In one binary every such edit recompiles
  the 29k-line crate; in `scripts` it recompiles 3.4k lines.
- Nothing ties the two: `scripts` uses no `common`, no `typescript_registry`.

Why not more groups (by area, ~5): each extra group is one more full link
(~25 MB, the cost being removed). The one case for more is if the single
`it` crate's compile becomes the build's critical path on a 3-4 core runner
(see Risks R4); then splitting `it` is `git mv` + one `mod` line per file,
decided by the measurement, not now.

Layout after the change:

```
core/tests/
  it/main.rs            mod common; #[cfg(unix)] mod typescript_registry; mod layout; mod <file>; ...
  it/common/mod.rs      (moved from tests/common/)
  it/typescript_registry/mod.rs   (moved from tests/typescript_registry/)
  it/layout.rs          the stray-file guard (below)
  it/<81 files>.rs
  scripts/main.rs       mod <file>; ...
  scripts/<6 files>.rs
  fixtures/             unchanged (paths are CARGO_MANIFEST_DIR-relative)
  log_line/shared.rs    unchanged (the SDK includes it by path)
```

## Stray-file guard (robust to other tasks adding `core/tests` files)

Cargo makes two silent failure modes possible after the merge:

1. A new `core/tests/foo.rs` silently becomes a new binary again (works, but
   undoes the task one file at a time).
2. A new `core/tests/it/foo.rs` **without** `mod foo;` in `main.rs` is
   silently never compiled: its tests never run.

`it/layout.rs` gets one test, `core_tests_have_exactly_the_two_binaries`:

- `core/tests/` top level contains no `*.rs`, and its directories are exactly
  `{it, scripts, fixtures, log_line}`; any other entry fails, by name, with
  the placement rule in the message.
- For `it/` and `scripts/`: the set of `*.rs` files (excluding `main.rs`)
  plus subdirectories with a `mod.rs` equals the set of `mod <name>;`
  declarations in that `main.rs` (parsed with a line regex, `#[cfg]`/`#[path]`
  attributes allowed above them). Missing or extra fails by name.
- No `*/main.rs` under `core/tests/` other than `it/` and `scripts/`.

Auto-discovery stays on (`autotests` not set): turning it off would make
mode 1 silent-and-not-run instead of loud. The guard is plain file reading,
no build, so it runs in milliseconds in `core-it`.

For this batch: GM-623 runs last, so the implement slice enumerates
`core/tests/*.rs` and `.config/nextest.toml` terms **at its own start**,
never from the counts in this note (87/81/6 and 33 core heavy-list terms are as
of a029b5b).

## Edit map

Line numbers omitted on purpose: GM-626/627/628 land first and move them.

### `core/tests/` (moves)

- `git mv` each top-level `*.rs` to `it/` or `scripts/` per the rule;
  `git mv common it/common`, `git mv typescript_registry it/typescript_registry`.
  Commit the moves alone first (`refactor: move core integration tests into
  two binaries`), edits second, so `git log --follow` and rename detection
  keep history.
- In each moved file, by role:
  - `mod common;` (61 files) -> `use crate::common;`. Existing
    `use common::X;` lines then resolve through the import (2021 uniform
    paths); if any does not, `use crate::common::X;`.
  - `mod typescript_registry;` (9 files) -> `use crate::typescript_registry;`.
  - `log_line_atomic.rs`: `include!("log_line/shared.rs")` ->
    `include!("../log_line/shared.rs")` (relative to the including file).
  - File-level `#![cfg(unix)]` (25 files) and
    `plugin_memory_limit.rs`'s `#![allow(clippy::disallowed_macros)]` stay:
    inner attributes on an out-of-line module apply to that module. They
    must stay before the first item (they already are).
  - Every `crate::` occurrence found in today's files is inside a string
    literal (fixture Rust source), not a path: no change.
- `it/main.rs`: crate doc with the placement rule; `mod common;`;
  `#[cfg(unix)] mod typescript_registry;` (all 9 users are unix-only, so
  on Windows it would be dead code; today those binaries are simply empty
  there); `mod layout;`; one `mod <file>;` per file, sorted.
- `scripts/main.rs`: same, no shared modules.
- `it/shim_bootstrap.rs`,
  `every_shim_spawn_in_this_suite_decides_which_project_it_serves`: it
  `read_dir`s `CARGO_MANIFEST_DIR/tests` non-recursively for `*.rs`. After
  the move it finds no files, and its `spawns >= 20` floor fails (loudly, not
  silently). Change: walk `tests/` recursively (it/, it/common/, scripts/).
- `scripts/test_sections_scripts.rs`: doc comment
  `cargo nextest run -p g-mesh --test test_sections_scripts --run-ignored only`
  -> `--test scripts -E 'test(/^test_sections_scripts::/)' --run-ignored only`.
- Fixture JSON in `test_local_script.rs` / `test_sections_scripts.rs` naming
  `g-mesh::cli_init` is synthetic input to the scripts' logic, not a lookup
  of a real binary: no change needed.

### `.config/nextest.toml`

- `[test-groups] daemon-spawning` and the `kind(test)` override: **no
  change**. `g-mesh::it` and `g-mesh::scripts` are still `kind(test)`, so every
  test keeps the same group and `max-threads = 2`. Comment "`kind(test)` is
  exactly the `core/tests/*.rs` targets" -> "the core and plugin
  integration-test binaries".
- slow-timeout override
  `test(=the_real_sections_partition_the_real_suite) | test(=the_local_profile_skips_exactly_its_heavy_list)`:
  nextest's test name in a merged binary is `<module>::<fn>`, so `=` no
  longer matches and the override would silently stop applying. ->
  `test(=test_sections_scripts::the_real_sections_partition_the_real_suite) | test(=test_sections_scripts::the_local_profile_skips_exactly_its_heavy_list)`
  (both live in `test_sections_scripts.rs`; re-check any term GM-626 adds).
- `[profile.local]` heavy list: every term
  `binary_id(g-mesh::<file>) & test(=<name>)` for a core file becomes
  `binary_id(g-mesh::it) & test(=<file>::<name>)` (or `g-mesh::scripts`
  for the script files, e.g. `release_packaging_scripts`). Unchanged:
  `binary_id(g-mesh)` (the lib) and `binary_id(g-mesh-plugin-rust::conformance)`.
  Mechanical transform; `the_local_profile_skips_exactly_its_heavy_list`
  already fails on a term that names no test.
- Comment "Every single wait budget in `core/tests/common/mod.rs`" -> `it/common/mod.rs`.

### `scripts/`

| file | role of the binary id / file name there | change |
|---|---|---|
| `test-sections.sh` | `core-it` = `package(g-mesh) & kind(test)`; lib sections `kind(lib)` | none (verify `check` partitions) |
| `test-sections-check.py` | keys tests by (binary-id, name), generic | none |
| `test-local.sh` | lib part keyed on `binary-id == package`; counts `binary_id(` terms | none (verify `--dry-run`) |
| `test-heavy-report.py` | regex `binary_id\(B\) & test\(=N\)` vs JUnit `classname`/`name` | none: JUnit classname is `g-mesh::it`, name `<file>::<fn>`, the terms match after the transform above; verify on one `--full` JUnit |
| `ci-junit-summary.py` | failure table **per binary** (GM-337: "25 in one binary is one cause") | D2: key `g-mesh::it` / `g-mesh::scripts` rows by `<binary>::<first module segment>`, so the table keeps one row per former file |
| `ci-diff-runs.sh` | test identity `"<binary> <name>"` | none in code; a diff across the merge boundary shows every core test as removed+added once. Note it in the release notes of the verify slice; do not add a mapping for a one-time event |

### CI (`.github/workflows/ci.yml`, `release.yml`)

No job or step selects by binary id or test file: sections come from
`test-sections.sh`, the build is `cargo nextest run --workspace --profile ci
--no-run`. Changes: comments naming `core/tests/plugin_check.rs` -> `core/tests/it/plugin_check.rs`.
The x86 row's `timeout`/`build_timeout`/`it_timeout` (75/30/25) stay until
measured (lowering them is a follow-up decision, not this task).

### Path references in comments

`core/tests/<file>.rs` appears in code comments in 10 files under `core/src/`,
`plugins/sdk/fake/*.rs`, `plugins/sdk/Cargo.toml`,
`plugins/{typescript,rust}/conformance/expect.toml`, `.config/nextest.toml`,
`ci.yml`. Update those to `core/tests/it/<file>.rs` (each match checked by
role: a path to a moved file, not `plugins/*/tests/...`). `docs/design/*`
and `docs/adr/*` are dated records: leave them.

### Docs

- `CLAUDE.md` (repo): one rule under testing: where a new core integration
  test goes, that it needs a `mod` line, and that the layout guard enforces it;
  how to run one file's tests: `cargo nextest run -p g-mesh --test it -E 'test(/^<file>::/)'`.
- `docs/adr/0030-test-sections.md`: one sentence that `core-it` is now two
  binaries (it still selects by `kind(test)`).

## Risks

| # | risk | assessment | mitigation |
|---|---|---|---|
| R1 | Shared process-global state across merged files | Under nextest (CI and `test-local.sh` part 2) every test is still its own process: nothing changes. Under plain `cargo test`, tests of different files now share one process. grep of `core/tests`: no `set_var`/`remove_var`/`set_current_dir`/signal handlers; 5 statics, all function- or file-scoped (`cli_clean_sweeping` and `worktree_daemon_lifecycle` `NEXT` counters, `module_tree_refresh_rust` `STEP`, `serving_while_indexing` `CACHED`, `cli_status` `GLOBAL_CONFIG_LOCK`). `GLOBAL_CONFIG_LOCK` guards the real `~/.g-mesh/config.toml` against its own file's threads only; under `cargo test` its tests could now overlap other files' tests in one run, which nextest already allows across binaries today | none needed; `cargo test` is not a supported runner for core-it |
| R2 | Test-group concurrency changes | none: same `kind(test)` override, same group, same width | verify: `cargo nextest show-config test-groups` lists every core-it test in `daemon-spawning` |
| R3 | Per-binary setup cost | no ctor/static init per binary; each test process execs a ~30 MB binary instead of ~25 MB: page-in, negligible. nextest's listing runs `<bin> --list` once per binary: 87 -> 2 execs (each one costly on a host with a huge `deps/`, see the exec-latency note) | none |
| R4 | One big crate's compile becomes the build's critical path | 87 small crates compiled in parallel become one 25k-line rustc (front end serial; codegen split across CGUs). On a 3-4 vCPU runner it overlaps the `cfg(test)` lib and 30 plugin test bins; unmeasured | the A/B reports the `it` unit's time (`--timings`); if it is the tail, split `it` (git mv + mod line) |
| R5 | Local incremental rebuild cost | Edit in `core/src` (the common case): before 87 rustc + 87 links, after 2 + 2: large win. Edit in one `it/` test file: before 1 small crate + 1 link, after an incremental recompile of the 25k-line crate + 1 link of the same size: a loss of seconds per edit-run (incremental reuses unchanged modules' CGUs). Edit in a script test: `scripts` is 3.4k lines, about as before. Not measured now: machine load 190-247 (`uptime` 13:46) makes any timing meaningless | measured in S2 on a quiet machine (Measurement plan, local) |
| R6 | Lost diagnosability: JUnit has one `<testsuite>` for all of core-it | real: `ci-junit-summary.py` would print one row | D2 |
| R7 | Silent non-running tests (`it/foo.rs` without `mod foo;`) or silently dead filters (`test(=old_name)`) | real; the most dangerous failure mode of this change | layout guard; every `test(=...)`/`binary_id(...)` term checked to match >= 1 test (AC4) |
| R8 | Collisions with in-flight work | every task touching `core/tests/*.rs` conflicts with the move | runs last in the batch, alone; moves in their own commit |
| R9 | Windows-only dead-code warnings | `typescript_registry` and `log_line/shared.rs` users are all unix-only | `#[cfg(unix)]` on the `mod` (and the include stays inside the `#![cfg(unix)]` file) |
| R10 | Owner's own workflow text | the standing rules for controls say "build and run only its test targets (`--test <file>`)". After the merge `--test it` builds the whole `it` crate; a control reverting test code recompiles 25k lines, one reverting production code is cheaper than before (one relink instead of one per affected binary) | suggested wording: `--test it -E 'test(/^<file>::/)'`; owner edits their rules |

## Measurement plan

**Baseline** (already fetched, table above): main 4.10.0 run 38973862380 and
release-4.10.0 38955950507; GM-612's 4.9.0 runs. Single-run comparison on
x86 macOS is meaningless (GM-612: 2.6x runner variance), so:

**A/B on CI (needs a push, D3).** Push the branch; `workflow_dispatch` ci.yml
3 times on the branch tip and 3 times on its merge-base, interleaved, in the
same hour. Per run and target, from `gh api .../jobs` and `gh run view --log`:
deps phase (step start -> `Compiling g-mesh v`), test-bins phase (last
workspace crate -> `Finished`), build step, `core-it` step, job total.

- Normalizer: the deps phase is untouched by this change, so
  `test-bins / deps` per run cancels host speed. Compare medians of that ratio
  per arm, plus raw minutes.
- Control that tells the arms apart (cheap, must differ): the number of
  `Running`/listed test binaries in the `core-it` step (`cargo nextest list`
  JSON: 87 core binaries vs 2). If the arms do not differ there, the run did
  not build what it claims and the timing says nothing.
- Correctness: the core-it test count per target equals the base arm's
  (408-ish on Linux; computed, not assumed).

**Local proxy (S2/verify, quiet machine only: `uptime` load below core
count, recorded with `/usr/bin/time -p` user/sys/real).** In the task
worktree and a base worktree with APFS-cloned targets:
`cargo build -p g-mesh --tests --timings` for (a) after `cargo clean -p
g-mesh`, (b) after touching `core/src/lib.rs`, (c) after touching one `it/`
test file, (d) after touching one `scripts/` test file. Report the
`--timings` per-unit times of the test units and the wall time. (c) is the
cost R5 names; (b) the gain.

## Proposed acceptance criteria

1. **Two binaries.** `core/tests/` has no top-level `*.rs`;
   `cargo nextest list -p g-mesh --message-format json` shows exactly the
   binary ids `g-mesh`, `g-mesh::bin/g-mesh`, `g-mesh::it`, `g-mesh::scripts`.
2. **Same tests.** The set of (file, test) from the base commit's listing,
   mapped `g-mesh::<file> <name>` -> `<file>::<name>`, equals the branch's
   set of `it`/`scripts` test names, ignored ones included
   (`--run-ignored all`), on macOS locally and, by count, on all four CI rows.
3. **Guard bites.** The layout test fails, naming the file, on (control a)
   a stray `core/tests/stray.rs`, (control b) an `it/orphan.rs` with no
   `mod` line; and passes on the real tree.
4. **No dead filter.** Every `binary_id(...)`/`test(=...)` term in
   `.config/nextest.toml` matches at least one listed test (one-off check in
   verify, plus the existing heavy-list test); the two ignored listing tests
   still get the 60s x 20 slow-timeout (`cargo nextest show-config` or a
   listing under the profile).
5. **Partition holds.** `scripts/test-sections.sh check` passes;
   `scripts/test-local.sh --dry-run` selects the same tests as on base
   (modulo the renaming in 2).
6. **Shim scan still guards.** `every_shim_spawn_in_this_suite_decides_which_project_it_serves`
   finds at least as many spawns as on base (control c: revert the
   recursive walk -> it fails on the `>= 20` floor).
7. **Green everywhere.** Full CI green on all four rows; clippy `-D warnings`
   `--all-targets` clean; no new warnings on the Windows row.
8. **Faster build, measured** (threshold D4): median `test-bins / deps`
   ratio on x86 macOS and Linux falls by >= 25% against the base arm in the
   A/B; local (b) faster, local (c) recorded.
9. **Failures stay attributable** (if D2 is taken): `ci-junit-summary.py` on
   a JUnit with failures in two `it` modules prints two rows.

## Decisions for the owner

**D1. How many binaries.**
Today: 87 test files, 87 binaries, 87 links per build.
Change: (recommended) two binaries, `it` (product, 81 files) and `scripts`
(script tests, 6 files); alternatives: one binary for all; or ~5 by area.
Example: GM-628 edits `test_local_script.rs`. With two binaries it
recompiles 3.4k lines; with one, 29k lines, before it can run.
Consequence: two = one extra small link per build, one extra placement rule
(enforced by the guard); one = simplest rule, slower edit loop for every
release/tooling task; five = five big links, closer to today's cost.

**D2. Failure table after the merge.**
Today: CI's step summary groups failures per test binary, which is per file
(the GM-337 diagnosis relied on it).
Change: (recommended) `ci-junit-summary.py` splits `g-mesh::it` rows by the
first segment of the test name (`cli_status::...` -> row `g-mesh::it::cli_status`);
alternative: leave it, one row "g-mesh::it: 25 of 400 failed".
Example: 25 failures all in `plugin_memory_limit` vs 25 across 12 files read
differently only with the split.
Consequence: split = ~10 lines of Python plus a test; leave = the table stops
telling one cause from many.

**D3. How to measure the CI gain.**
Today: nothing measured; GM-612's "5-9 min" is an estimate.
Change: (recommended) push the task branch and dispatch 3+3 interleaved runs
(branch vs base); alternatives: local `--timings` proxy only; or measure on
the release-4.11.0 run after merge.
Example: one release run at 34 min after vs 42 before could be the runner
alone (24-46 min for identical code on 4.9.0).
Consequence: A/B = 6 runs of ~45 min on four runners (public repo: no cost,
queue time); proxy only = no CI number; after-merge = one noisy sample.

**D4. Pass threshold for the build-time gain.**
Today: no criterion.
Change: (recommended) >= 25% drop in the normalized test-bins phase on x86
macOS and Linux; alternatives: "not slower" (the merge is kept for disk and
local-rebuild gains anyway); or a fixed minutes target.
Example: test bins 9:25 -> 7:00 on x86 is 26%: passes; 9:25 -> 8:30 fails
and the result is reported with R4 (is `it` the tail?) before merge.
Consequence: 25% may fail if the `cfg(test)` lib is the real tail; "not
slower" cannot fail on time.

**D5. Plugin integration tests.**
Today: 30 more binaries under `plugins/*/tests/`, same link cost per binary.
Change: (recommended) out of scope, a follow-up task once this one is
measured; alternative: include them now.
Consequence: follow-up = smaller diff, learns from this measurement first;
now = one more layout per crate and four more crates' filters in one batch.

## Search notes

- g-mesh (main checkout project `g-mesh`): `get_dependencies` Incoming on
  `core/tests/common/mod.rs` returned an `orphan:` module with no importers,
  and `find_references DEFAULT_STARTUP_TIMEOUT_SECS` linked 0 uses and
  listed `nextest_wait_budgets.rs` as an unlinked usage: g-mesh does not link
  `mod common;` across the per-file test crates (each file is its own crate
  root). The `mod`/`include!`/static/env inventories above are from grep.
- Non-code (`.toml`, `.sh`, `.py`, `.yml`) by grep.
