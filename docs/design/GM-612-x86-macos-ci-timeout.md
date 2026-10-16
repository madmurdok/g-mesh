# GM-612: `tests (x86_64-apple-darwin)` outgrew its 45-minute job timeout

Status: diagnosis (S1).

## What happened

Main CI run 38803570019 (d34a10f, the 4.9.0 merge), attempt 1: the job
`tests (x86_64-apple-darwin)` was cancelled at 45.8 min by the job's
`timeout-minutes: 45` (`.github/workflows/ci.yml`, `test` job). Every section
through `plugin-rust` passed, `plugin-python` was cut 0.5 min in, `wire` and
`g-mesh plugins check` never ran. Attempt 2 of the same run, same commit,
passed in **24.1 min**. The same code took 34.5 min on release run
38792039406 and 41.9 min on the v4.9.0 tag run 38815370882 (release.yml calls
ci.yml through `workflow_call`, so the tag run is the same job).

## Timing data

Source: `gh api repos/madmurdok/g-mesh/actions/runs/<id>/jobs` (step start
and end times) for 14 CI/release runs since 4.2.0, and the job logs of 4 runs
(`Compiling`/`Finished` timestamps, nextest `Summary` lines and per-test
`PASS [ Ns]` durations). Minutes unless marked.

### Job total, every platform, 4.2.0 .. 4.9.0

| run (attempt) | what | x86_64 macOS | aarch64 macOS | Linux | Windows |
|---|---|---|---|---|---|
| 37859404190 | main 4.2.0 | 29.4 | 11.7 | 9.8 | 16.7 |
| 38056429607 | release-4.3.0 | 28.4 | 12.8 | 11.6 | 18.5 |
| 38062729696 | main 4.3.0 | 24.6 | 13.0 | 11.8 | 17.4 |
| 38167126252 | release-4.5.0 | 35.7 | 11.2 | 9.0 | 14.6 |
| 38170811837 | main 4.5.0 | 24.6 | 14.3 | 11.3 | 14.9 |
| 38270920886 | release-4.6.0 | 31.6 | 16.2 | 10.5 | 13.0 |
| 38275178235 | main 4.6.0 | 27.9 | 17.7 | 8.7 | 17.6 |
| 38427872975 | release-4.7.0 | 25.4 | 15.1 | 10.5 | 17.2 |
| 38431504865 | release-4.7.0 (dispatch) | 33.2 | 16.4 | 11.9 | 18.2 |
| 38436504903 | main 4.7.0 | 28.6 | 14.9 | 10.8 | 19.0 |
| 38758533967 | release-4.9.0 (failed tests) | 42.5 | 16.3 | 13.5 | 18.0 |
| 38792039406 | release-4.9.0 | 34.5 | 15.5 | 12.1 | 18.6 |
| 38803570019 (1) | main 4.9.0 | **45.8 cancelled** | 18.8 | 14.4 | 19.7 |
| 38803570019 (2) | main 4.9.0, re-run | 24.1 | - | - | - |
| 38815370882 | tag v4.9.0 | 41.9 | 18.1 | 14.1 | 19.3 |

x86_64 macOS ranges 24.1-45.8 (median 29.4); every other platform stays
under 20. The suite itself grew about 50% over these ten days on every
platform (aarch64 11.7 -> 18.8, Linux 9.8 -> 14.4).

### Job phases, four runs with logs

`pre` = job start to the build step; `post` = after the last section
(results, `plugins check`, diagnostics).

| run | target | pre | build | sections | post | total |
|---|---|---|---|---|---|---|
| 38803570019 (1) | x86_64 macOS | 2.6 | **18.4** | **24.4** (cut) | 0.3 | 45.8 |
| 38803570019 (2) | x86_64 macOS | 1.3 | 7.1 | 14.2 | 1.4 | 24.1 |
| 38815370882 | x86_64 macOS | 1.7 | 16.2 | 20.8 | 3.2 | 41.9 |
| 38792039406 | x86_64 macOS | 2.6 | 13.7 | 15.9 | 2.4 | 34.5 |
| 38427872975 | x86_64 macOS | 1.6 | 11.6 | 10.2 | 2.0 | 25.4 |
| 38803570019 | aarch64 macOS | 1.1 | 5.5 | 11.0 | 1.2 | 18.8 |
| 38803570019 | Linux | 0.7 | 4.3 | 8.3 | 0.9 | 14.4 |

The cancelled attempt, run to the end, would have been about **50 min**:
45.8 + the rest of `plugin-python` (~0.5) + `wire` (~0.5) + `post` at that
runner's speed (~3, as on 38815370882).

### Build step, by phase (from `Compiling` timestamps)

deps = step start to `Compiling g-mesh v` (third-party crates and the small
workspace crates); crates = the workspace libraries and plugin binaries;
test bins = from the last crate to `Finished` (the `cfg(test)` build of the
core lib plus 87 `core/tests/*.rs` and 30 plugin integration-test binaries,
each linking the whole core crate, ONNX Runtime, SQLite, Oniguruma).

| run | target | deps | crates | test bins | build |
|---|---|---|---|---|---|
| 38803570019 (1) | x86_64 macOS | 4:45 | 1:38 | **11:29** | 17:52 |
| 38803570019 (2) | x86_64 macOS | 1:44 | 0:38 | 4:31 | 6:53 |
| 38815370882 | x86_64 macOS | 4:11 | 1:27 | 9:58 | 15:36 |
| 38792039406 | x86_64 macOS | 3:30 | 1:15 | 8:37 | 13:22 |
| 38427872975 | x86_64 macOS | 2:58 | 1:01 | 7:20 | 11:19 |
| 38803570019 | aarch64 macOS | 1:06 | 0:21 | 4:02 | 5:29 |
| 38803570019 | Linux | 0:53 | 0:17 | 3:06 | 4:16 |

### Sections, per step (minutes)

| section | x86 70019 (1) | x86 70019 (2) | x86 70882 | x86 39406 | arm 70019 | Linux 70019 |
|---|---|---|---|---|---|---|
| core-it | **15.4** | 8.2 | 12.0 | 12.0 | 7.0 | 5.1 |
| core-mcp | 1.4 | 0.9 | 1.2 | 1.4 | 0.4 | 0.4 |
| core-daemon | 0.8 | 0.5 | 0.6 | 0.7 | 0.4 | 0.3 |
| core-cli | 0.6 | 0.4 | 0.7 | 0.6 | 0.2 | 0.2 |
| core-graph | 0.7 | 0.5 | 0.6 | 0.7 | 0.2 | 0.2 |
| core-rest | 0.7 | 0.5 | 0.8 | 0.7 | 0.2 | 0.2 |
| sdk | 1.2 | 1.0 | 1.3 | 1.2 | 0.8 | 0.6 |
| plugin-typescript | 1.4 | 1.0 | 1.4 | 1.4 | 0.8 | 0.7 |
| plugin-rust | 1.9 | 1.1 | 1.7 | 1.9 | 0.9 | 0.8 |
| plugin-python | 0.5 (cut) | 0.3 | 0.5 | 0.5 | 0.1 | 0.1 |
| wire | - | 0.1 | 0.1 | 0.1 | 0.1 | 0.1 |

`core-it`, sum of per-test durations against wall time (nextest `Summary`):

| run | target | tests | sum of tests | wall | slowest test |
|---|---|---|---|---|---|
| 38803570019 (1) | x86_64 macOS | 408 | 1786 s | 896 s | 107 s |
| 38803570019 (2) | x86_64 macOS | 408 | 952 s | 480 s | 61 s |
| 38803570019 | aarch64 macOS | 408 | 816 s | 412 s | 51 s |
| 38803570019 | Linux | 407 | 603 s | ~306 s | 39 s |

Wall is half the sum on every platform: `kind(test)` runs in the
`daemon-spawning` test group with `max-threads = 2` (`.config/nextest.toml`),
so `core-it` is serial-ish by design and scales directly with per-test speed.
The slowest test is the same everywhere
(`serving_while_indexing::a_walk_that_outlasts_the_bootstrap_timeout_is_waited_out_rather_than_losing_the_client`),
no x86-only outlier.

## Cause

1. **Runner variance on `macos-15-intel`, about 2.6x.** The two attempts of
   38803570019 ran the same commit on the same image (provisioner 20260819.586,
   westus) and every phase slowed by the same factor: deps 2.7x, test bins
   2.5x, `core-it` 1.9x (its sum of test times 1.9x), the lib-test sections
   1.5x. A uniform slowdown across compile, link and test is CPU throughput of
   the VM, not a slow test or a code path. The logs carry no CPU model, core
   count or load, so which hosts are slow cannot be named from them.
2. **The suite grew about 50% in ten days**, on every platform (more
   integration tests: `core-it` 386 -> 408 tests; `plugin-typescript` and
   `plugin-rust` +0.7 min each from 4.7.0's `semantic_runs_no_project_code`
   and `indexing_writes_nothing_and_stays_offline`). On a fast Intel runner
   this lands at 24-29 min; on a slow one at 42-50, past 45.
3. **No cache.** The job compiles everything cold every time; the build is
   38-40% of the job on x86_64 macOS (7-18 min), and 11.5 min of the slow
   build is the 117 integration-test binaries plus the core `cfg(test)` lib.
4. **GM-603 is not the cause.** No 4.9.0 log shows rust-analyzer installing
   rust-src (only the `rust-analyzer` component download at setup); the
   sections that run rust-analyzer (`plugin-rust`, the `plugin_memory_limit`
   tests in `core-it`) slowed by the same factor as everything else. Its
   mid-suite rust-src install, where it happened, is bounded by
   `plugin-rust`'s 1.9 min at most.

## Options

Acceptance: the job's worst observed time is <= 70% of its timeout. Worst
observed full run, x86_64 macOS: about 50 min (projected from the cancelled
attempt); worst completed: 41.9.

| | option | expected worst x86 job | timeout | headroom | benefit | risk |
|---|---|---|---|---|---|---|
| A | **Per-row timeout: 75 for x86_64 macOS** (and its build/`core-it` step timeouts) | 50 | 75 | **33%** (typical 24-35 min: 53-68%) | One-file change; no test or cache semantics change; Linux, arm and Windows keep 45 | A real hang reports 30 min later on that row (sections still have their own step timeouts and nextest's 210 s kill, so a hang inside a section is still bounded at 10-25 min); does not stop growth: at +50% per ten days it buys roughly two weeks before the next review |
| B | Cargo cache (`Swatinem/rust-cache`, deps only) | ~46 (saves deps 1.7-4.8, minus ~1 restore) | 45 | none | Helps every platform a little | Does not meet 30% alone; cache scoping by branch (main reads main's; a tag run reads the default branch's); 10 GB repo cache shared by 4 targets + Go |
| C | Merge the 87 `core/tests/*.rs` into one `core/tests/it/` binary | build -5..-9 (estimate; link phase is 11.5 of 18.4 on the slow runner, not measured per binary) | 45 | ~10-25% alone (unmeasured) | Faster on every platform and locally; the real fix for growth in the build | Large refactor: 33 `binary_id(g-mesh::...)` filters in `.config/nextest.toml`, the heavy-list scripts and their tests; collides with every in-flight test change; the per-binary saving is an estimate, never measured |
| D | Split x86_64 macOS into two jobs (`core-it` / the rest), each cold-building | max(2.6+18.4+15.4+0.5, 2.6+18.4+10+3) = 37 | 45 | 18% | Halves the test half | Build runs twice; two Intel runners per push (public repo: no cost, but macOS concurrency); still under 30% without raising the bound |
| E | `daemon-spawning` `max-threads` 2 -> 4 on x86_64 macOS | `core-it` 15.4 -> ~8 | 45 | ~6-20% | Big cut in the biggest section | The group exists because full width failed 23 tests from contention (`.config/nextest.toml` comment); widening it on the slowest runner is the exact condition that produced those failures |
| F | Test x86_64 macOS elsewhere (only on release, or under Rosetta on the arm runner) | - | - | - | Removes the slow runner | Breaks "a target that ships is a target whose tests ran" (ci.yml `test` job comment); Rosetta mixes arm64 node/RA/pyright with x86 g-mesh, so it tests a different system; overlaps GM-613's runner change |

## Recommendation

**A**, plus a runner-state step so the next variance question has data:

- x86_64 macOS job timeout 45 -> **75**; its `Build the test binaries` step
  25 -> **30** (worst 18.4: 39% headroom) and its `tests: core-it` step
  20 -> **25** (worst 15.4: 38%). Other rows unchanged.
- A step printing the runner's CPU model, core count, memory and `uptime` at
  job start, and `uptime` again at the end, so a slow run is attributable to
  the host from its own log.

Expected: worst full x86 run ~50 min, **33% headroom** under 75; typical
24-35 min, 53-68%.

C is the fix for growth and should be its own task after this release (it
touches the test layout, which every other task in a batch also touches);
B is worth trying with it, measured as an A/B on dispatched x86 runs.

## Edit map (`.github/workflows/ci.yml`, line numbers at 597bc32)

1. `matrix` job, step `select`, the `all='[...]'` rows (l.86-92): add
   per-row fields to the `x86_64-apple-darwin` row only, e.g.
   `"timeout":75,"build_timeout":30,"it_timeout":25`. Leave the other rows
   without them, so they fall to the defaults below.
2. `test` job, `timeout-minutes: 45` (l.290) ->
   `timeout-minutes: ${{ matrix.timeout || 45 }}`; rewrite the comment above
   it (l.281-289): "a healthy run finishes in about five minutes" is stale
   (10-19 min; 24-50 on x86_64 macOS). The comment states the bound and
   that it is per row (no ticket history, per CLAUDE.md); the evidence stays
   in this document.
3. Step `Build the test binaries` (l.737-742): `timeout-minutes: 25` ->
   `${{ matrix.build_timeout || 25 }}`.
4. Step `tests: core-it` (l.750-754): `timeout-minutes: 20` ->
   `${{ matrix.it_timeout || 20 }}`.
5. New step right after `actions/checkout` (l.308), `if: runner.os != 'Windows'`,
   `shell: bash`: `uptime`; on macOS `sysctl -n machdep.cpu.brand_string
   hw.ncpu hw.memsize`, on Linux `nproc; grep -m1 'model name' /proc/cpuinfo;
   free -g`. And one `uptime` line at the top of the existing `always()` step
   `What the daemons were doing` (l.882-893) for the end-of-job load.

Both `timeout-minutes` keys accept the `matrix` context (job level: github,
needs, strategy, vars, matrix, inputs; step level adds job, runner, env,
steps). The matrix rows are numbers in the JSON, so `${{ matrix.timeout }}` is
a number; `|| 45` covers a row without the field.

No other file reads these timeouts (grep of `scripts/`, `core/tests/`,
`.github/` for `timeout-minutes` and `macos-15-intel`: only ci.yml and
release.yml's own build matrix).

## Constraints for GM-613

- Release runs the tests through `uses: ./.github/workflows/ci.yml`, so it
  inherits the per-row timeouts. If GM-613 changes how ci.yml is called or
  narrows its matrix, the `timeout`, `build_timeout` and `it_timeout` fields
  stay on the `x86_64-apple-darwin` row of ci.yml's `matrix` job.
- GM-613's "one macOS runner for both architectures" is about release.yml's
  build rows. ci.yml's x86_64 macOS **test** row must stay on
  `macos-15-intel`; moving those tests to the arm runner (Rosetta or
  cross-built) invalidates every number here and needs its own measurement.
- Skipping tests on a tag whose commit CI passed removes the 41.9-min tag
  run, but not the main/release-branch runs; this fix is still needed.

## How verify confirms the 30% headroom

1. Verify needs at least one complete x86_64 macOS run with the change (a
   push to the release branch, or `workflow_dispatch` with
   `target=x86_64-apple-darwin`, which the owner triggers). Read, from
   `gh api repos/madmurdok/g-mesh/actions/runs/<id>/jobs`:
   - the `tests (x86_64-apple-darwin)` job's `completed_at - started_at`
     (pass: <= 52.5 min, i.e. 70% of 75; expected 24-50);
   - `Build the test binaries` <= 21 min (70% of 30) and `tests: core-it`
     <= 17.5 min (70% of 25);
   - the new runner-state step's output (CPU model, cores, load) in the log.
2. Check the timeouts took effect, not just that the job passed: the
   `matrix` job's `testing: [...]` log line must show the three fields on the
   x86_64 macOS row only, and the workflow file at the run's commit
   (`gh api repos/madmurdok/g-mesh/contents/.github/workflows/ci.yml?ref=<sha>`)
   must carry the three expressions. An observation that tells the arms
   apart only exists on a slow host (an x86 job passing 45 min uncancelled);
   if the run lands on a fast host, say so rather than claim it.
3. The headroom claim rests on the projected 50-min worst run. Verify reports
   the new run's total next to that worst case and the 24.1-min fast case,
   and the runner-state line, so a later run that exceeds 52.5 min is
   recognisable as a slower host or as suite growth.
