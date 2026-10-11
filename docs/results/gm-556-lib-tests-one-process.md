# GM-556/S1: lib tests in one `cargo test` process

Investigation for GM-556 (`scripts/test-local.sh` runs lib tests in one libtest
process instead of nextest's process per test). No script or production change here.

## One-process runs (`cargo test -p <crate> --lib`, 3 runs each, default threads)

Machine: GM-546 building concurrently; load averages 450-640 (1-min) for the whole
run. Times are context, not a speed claim. `G_MESH_CONTAINERS_SEEDS` unset (every seed).

| crate | tests (ignored) | failed, 3 runs | libtest s (runs 1-3) | last run real / user / sys |
| --- | --- | --- | --- | --- |
| g-mesh | 1861 (9) | 0 / 0 / 0 | 208 / 168 / 175 | 178.5 / 208.8 / 64.7 |
| g-mesh-plugin-python | 152 (2) | 0 / 0 / 0 | 8.0 / 2.7 / 3.1 | 4.6 / 2.1 / 1.3 |
| g-mesh-plugin-rust | 182 (2) | 0 / 0 / 0 | 0.7 / 0.7 / 0.7 | 2.6 / 0.5 / 1.1 |
| g-mesh-plugin-sdk | 176 (0) | 0 / 0 / 0 | 3.0 / 3.0 / 3.0 | 4.9 / 0.5 / 1.3 |
| g-mesh-plugin-typescript | 14 (0) | 0 / 0 / 0 | 0.01 each | 1.9 / 0.2 / 0.2 |
| g-mesh-wire | 42 (0) | 0 / 0 / 0 | 0.01 each | 1.3 / 0.2 / 0.1 |
| g-mesh-walk | 0 | - | - | - |

- 21 runs, 0 failures. The g-mesh runs include everything a `--full` would: the 8 heavy
  lib tests and the `ISOLATED` spawn-timing test, so even that one passed 3/3 here.
- g-mesh run 1's `real` was 544.8s: `cargo test -p g-mesh` rebuilt the lib under `-p`
  feature unification first (libtest itself 208s). Runs 2-3 were pre-built.
- g-mesh wall time is the tail: 7 tests ran past 60s in every run (libtest's warning):
  the two `graph::containers` seed tests, `link_all_and_link_diff_agree...` and four
  `mcp::response_bound_tests`. The one-process run cannot finish sooner than they do.
- Ignored tests need model weights or a census corpus; same under nextest.

## Process-global hazards in lib test code (all crates' `src/`)

Searched with grep for `set_var`/`remove_var`/`set_current_dir`/`process::exit`/signal
handlers/`static`/`OnceLock`/`LazyLock`/`thread_local!`/fixed ports/`temp_dir().join`/
`current_exe`; g-mesh `find_callers` for `kill_live_servers` and `discovered_fingerprint`.

| hit | verdict |
| --- | --- |
| `set_var`, `remove_var`, `set_current_dir` | none in any crate (GM-549 removed them) |
| `process::exit` (core `main.rs`, sdk `run.rs` x3) | safe: only `run()`, the bulk lifeline and the control stream; no lib test reaches them |
| sdk `LIVE_SERVERS` / `kill_live_servers` | safe: only caller besides `run::read_control_stream` is `plugins/sdk/tests/lsp_prepare_reaped.rs`, an integration binary (`find_callers`) |
| core `daemon::plugin` `FINGERPRINT` (`OnceLock`) | safe: only caller `build_stamp::of_running_process` (`find_callers`); read-only, no test expects a second value |
| core `process::SPAWN_LOCK` | safe but one-process-specific: serialises every plugin spawn of every lib test in the process (per test process under nextest). It is the load that breaks `ISOLATED` |
| `thread_local!` (index_store `HELD`, plugin parsers, census) | safe: per thread |
| statics `NEXT` counters (ipc, facts, extractor tests) | safe: make names unique within a process |
| temp dirs named `<prefix>-<pid>-<tag>` (sdk manifest, walk, resolve, config, run, resolution_tests; python semantic) | safe today: under nextest the pid alone separated tests, in one process only the tag does. Every literal tag checked: unique per helper, and prefixes differ across helpers |
| temp dirs named `<prefix>-<pid>` with no tag (`g-mesh-expectations-test`, `g-mesh-py-local`, `g-mesh-py-venv`) | safe today: one test uses each prefix |
| `find_definition/tests.rs` `g-mesh-tests-with-no-sources` | safe: never created, only looked up |
| `TcpListener::bind("127.0.0.1:0")` (cli model) | safe: ephemeral port |
| env reads (`G_MESH_*`, no `NEXTEST_*`) | safe: nothing writes the test process env; children get theirs via `Command::env` |
| `embedding::cache` busy-timeout test (`waited < 1500ms`), self re-exec via `current_exe` | load-sensitive, not isolation-dependent; same exposure under nextest's width. Passed 3/3 |

Isolation-dependent tests: one, already pinned: `ISOLATED`
(`daemon::registry::tests::two_languages_spawn_at_the_same_time_rather_than_one_after_the_other`).
Proposal: keep it on nextest in `daemon-spawning`, in both modes. Risky (future):
the pid-keyed temp-dir pattern. A new test reusing a tag would collide only in one
process. Fix options: switch these helpers to `tempfile::tempdir()`, or leave and note.

## Today

- `--base`/default (part 1) already runs the selected g-mesh lib tests in one process:
  names from `cargo nextest list --profile local -E "$lib_filter"` (so the heavy
  default-filter is applied by nextest), then `cargo test --workspace --lib -- --exact
  <names>` under `run_with_budget` (600s, exit 124 on timeout). `ISOLATED`, the g-mesh
  bin and the plugin/sdk/wire libs go to the part-2 nextest run (`--profile local`).
  Exit: `failed=1` if either part fails; part 2 skipped after a lib failure unless
  `--keep-going`.
- `--full` runs `scripts/test-sections.sh run --profile ci --keep-going all`: 11 nextest
  invocations, one JUnit per section, then `test-heavy-report.py` on those JUnits.
  `ci` has no default-filter: `--full` runs the heavy tests on purpose, to time them.
  Exit: test-sections' status (1 on any failed section); the report never fails it.

## Sketch for the implement slice

1. `--full` reuses the part-1/part-2 path with selection `full`, profile `ci` for both the
   listing and the nextest run, `G_MESH_CONTAINERS_SEEDS` unset, keep-going forced on.
   Part 1 then runs every g-mesh lib test except `ISOLATED` (and see Q1 for the 8 heavy).
   Part 2 is one `cargo nextest run --profile ci -E "(all) & not (lib_filter)"`, giving
   one `junit.xml` that `test-heavy-report.py` reads as today.
2. Filter mapping: no `--skip` needed. The positive `--exact <names>` list from
   `nextest list` already carries the `local` default-filter (base) or none (`ci`), so
   the heavy list stays one source. `-- --exact --skip <name>` per term would duplicate
   `.config/nextest.toml` in the script.
3. Plugin libs (sdk, plugin-python/-rust/-typescript, wire: 566 tests, ~10s in one
   process each run) join part 1 by widening `lib_filter` to `kind(lib)` over their
   packages and listing names per binary (`listed` takes one binary today).
4. Exit codes unchanged: libtest 101 -> `failed=1`; budget kill 124; INT 130; script
   exits 1 on any failure, 2 on usage. `--full` keeps `--keep-going`'s "run everything".
5. Budget: `--full` part 1 took 168-208s here at load ~500; 600s default holds, with
   a margin of about 3x over the 60s+ tail tests.
6. Heavy report: libtest (stable) prints no per-test time and no JUnit, so lib tests
   in part 1 vanish from the report. Consequences: heavy lib tests run in part 1 appear
   as "listed, not in this run"; a lib test crossing 10s is never flagged.

## Open questions for the owner

Q1. Where do the 8 heavy lib tests run in `--full`?
Today each is timed by nextest and checked by the heavy report. Change: in the one
process they lose their time. Example: `every_caller_page_fits_the_ceiling` ran >60s
here; the report would list it as "not in this run". Options: keep the 8 on nextest
in part 2 (timing kept, ~8 process starts), or run them in part 1 and teach the report
to skip lib terms. Recommend part 2.

Q2. Is losing per-test lib timing acceptable for new heavy lib tests?
Today a lib test crossing 10s shows up as "not listed, now over 10s". Change: in one
process it never does. Example: a new lib test taking 40s just stretches part 1's wall
time silently. Options: accept (part 1's total time is the signal); or
`RUSTC_BOOTSTRAP=1 ... -Z unstable-options --format json --report-time` parsed into a
JUnit for the report (exact names and times, but an unstable flag, and times are
inflated by sharing the process with ~1850 tests).

Q3. Do the plugin/sdk/wire libs (566 tests) move into part 1 too?
Today they run under nextest in both modes. Change: one libtest process per crate.
Example: plugin-python's 152 tests took 3-8s in one process. Consequence: the
`listed`/name-passing code must handle several binaries; gain is small next to g-mesh.

Q4. Pid-keyed temp dirs: convert to `tempfile::tempdir()` now, or leave?
Today 11 helpers name dirs `<prefix>-<pid>[-<tag>]` (8 tagged, 3 single-use); none collide.
Change: in one process a future duplicate tag collides where nextest hid it. Fix is
mechanical (11 helpers) but touches tests outside this task's scope.

## Measurement (GM-556/S10): the lib share of `--full`, before and after

Machine: macOS, otherwise idle except for an endpoint scanner; 2 runs per arm, alternated B, A, B, A,
each under `/usr/bin/time -p`. Every run waited (up to 10 min) for the 1-min load to fall below 20
first. Both trees were built and warm beforehand (main: `cargo clean`, deps 36,865 -> 15,354).

- **B** (release-4.5.0 `d969781`): `cargo nextest run --workspace --profile ci -E 'kind(lib)'`, the
  lib tests of the lib sections, 2431 tests.
- **A1** (`134a60a`): part 1 of `test-local.sh --full`, stopped after part 1 (in-memory patch, as in
  S9): prune, `nextest --no-run`, list, then one `cargo test --lib` per crate, 2422 tests.
- **A9**: the 8 heavy lib tests and ISOLATED on nextest (`ci`), which `--full` runs in part 2.

| arm | run | real s | user s | sys s | load 1/5/15 before | passed |
|---|---|---:|---:|---:|---|---:|
| B | 1 | 426.09 | 203.16 | 62.32 | 18.04 185.42 231.93 | 2431 |
| B | 2 | 438.19 | 191.60 | 59.60 | 17.56 55.87 110.80 | 2431 |
| A1 | 1 | 165.46 | 99.11 | 67.56 | 19.10 73.71 153.91 | 2422 (lib 94 s) |
| A1 | 2 | 132.70 | 88.51 | 78.03 | 17.19 46.36 82.96 | 2422 (lib 61 s) |
| A9 | 1 | 44.49 | 79.07 | 2.66 | 19.13 65.80 128.62 | 9 |
| A9 | 2 | 28.29 | 73.20 | 2.27 | 18.57 39.30 73.56 | 9 |

Median real: B 432 s; A1 149 s (2.9x less); A1 + A9 186 s (2.3x less), for the same 2431
tests. B's real is about 170 s above user + sys: nextest's 2431 process starts wait, not
compute. About 70 s of A1 is the prune, `--no-run` and list steps, not the tests.
