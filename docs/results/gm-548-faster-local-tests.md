# GM-548: faster local test runs, measured

Measure slice of GM-548 (note: `docs/architecture/gm-548-faster-local-tests.md`,
section 6; ADR: `docs/adr/0031-local-test-runs.md`). All runs on commit
`9284e6c`, one build, 2026-10-10, one background script, arms alternated.

## Setup

- Machine: 8 CPUs, macOS. Other agents were running; load moved from ~25 to
  ~580 and back to ~4 during the session (see per-run load below).
- Before the runs, `target/debug/deps` held 99,844 files.
  `scripts/prune-stale-objects.sh` deleted 60,835 stale `.rcgu.o`, leaving
  39,009 (35,676 objects still referenced by linked binaries, so a
  `cargo clean` + rebuild would land at about the same count).
- Warm-up: `cargo nextest run --workspace --no-run` and
  `cargo test --workspace --lib --no-run` (compiled 7 crates). Every measured
  run after it compiled nothing (0 `Compiling` lines in each log).
- Arms:
  - **OLD** (what a verify slice ran before):
    `scripts/test-sections.sh run --keep-going $(scripts/test-select.sh --paths-from <f>)`,
    `default` profile, one process per test, heavy tests included.
  - **NEW**: `scripts/test-local.sh --keep-going --paths-from <f>`.
- Path sets:
  - (a) `core/src/cli/status.rs`. CI selection: all 6 core sections. Local
    (`--narrow`, owner decision Q1-B): `core-it core-cli`.
  - (b) `core/src/graph/containers.rs`. Both: all 6 core sections.
- Order: a1 OLD, a2 NEW, b1 NEW, b2 OLD, a3 NEW, a4 OLD, b3 OLD, b4 NEW, full.
- Times from `/usr/bin/time -p`; CPU = user + sys. Load = `uptime` 1-minute
  average before and after the run.

## Results

Every run passed (0 failed tests).

### (a) `core/src/cli/status.rs`

| run | arm | load before -> after | real s | user s | sys s | CPU s |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| a1 | OLD | 176 -> 278 | 2015 | 364 | 202 | 566 |
| a2 | NEW | 278 -> 582 | 822 | 110 | 71 | 181 |
| a3 | NEW | 23 -> 15 | 564 | 88 | 58 | 146 |
| a4 | OLD | 15 -> 4 | 1535 | 288 | 163 | 451 |

Loaded pair (a1/a2, NEW under the higher load): real 2.5x, CPU 3.1x lower.
Quiet pair (a3/a4, OLD under the lower load): real 2.7x, CPU 3.1x lower.

### (b) `core/src/graph/containers.rs`

| run | arm | load before -> after | real s | user s | sys s | CPU s |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| b1 | NEW | 582 -> 511 | 1165 | 160 | 97 | 257 |
| b2 | OLD | 511 -> 23 | 1789 | 340 | 196 | 536 |
| b3 | OLD | 4 -> 4 | 981 | 283 | 152 | 435 |
| b4 | NEW | 4 -> 10 | 462 | 138 | 112 | 250 |

Loaded pair (b1/b2, NEW under the higher load, b2's load fell during the
run): real 1.5x, CPU 2.1x lower. Quiet pair (b3/b4, comparable load): real
2.1x, CPU 1.7x lower.

### Where NEW spends its time

| run | lib part (one process) | nextest part |
| --- | --- | --- |
| a2 | 315 tests, 13 s | 312 tests, 680 s |
| a3 | 315 tests, 22 s | 312 tests, 375 s |
| b1 | 1794 tests, 53 s | 327 tests, 988 s |
| b4 | 1794 tests, 37 s | 327 tests, 331 s |

The lib is no longer the cost: 1794 lib tests take 37-53 s in one process
(OLD's five lib sections took 478-708 s of nextest time). What remains is
`core-it` under nextest (304 tests without the heavy ones) plus the isolated
env-writing lib tests.

### Full run (`scripts/test-local.sh --full`, batch end)

| load before -> after | real s | user s | sys s | CPU s | tests |
| --- | ---: | ---: | ---: | ---: | ---: |
| 10 -> 4 | 1387 | 354 | 195 | 549 | 3189 passed, 0 failed |

Heavy-list report (54 terms, 3189 tests timed, threshold 10 s):

- 38 listed tests ran under 10 s on this quiet machine (3.5-9.7 s), among
  them most `release_packaging_scripts`, `cli_status`, `idle_lifecycle` and
  `daemon_build_staleness` entries.
- 2 unlisted tests at or over 10 s:
  `g-mesh-plugin-rust::conformance the_linked_index_answers_the_acceptance_criteria`
  (17.2 s) and `...the_plugin_passes_every_check_that_applies_to_it` (15.6 s).
- 0 listed tests missing (no renames).

The list was chosen from loaded logs; on a quiet machine most of it is under
the threshold. Per owner decision Q4 this is the batch-end finding to act on;
it does not change the local run's correctness, only how much it skips.

## Control observation

The arms differ in the number of test processes started, and the counts add
up:

- OLD, (a) and (b): 2175 nextest PASS lines, i.e. 2175 test processes
  (core-it 358 + mcp 513 + daemon 341 + cli 315 + graph 222 + rest 426).
- NEW (b): 1 libtest process (`running 1794 tests`) + 327 nextest processes.
  1794 + 327 = 2121 = 2175 - 54 heavy tests.
- NEW (a): 1 libtest process (`running 315 tests`, core-cli) + 312 nextest
  processes; the narrowed selection skips the other 1479 lib tests.
- No measured run (OLD, NEW or full) printed a `Compiling` line: the lib
  part's `cargo test` reuses the nextest build.

## Conclusion

On the same build, for a typical core change, the local run is 2.1-2.7x
faster in wall time on a quiet machine and uses 1.7-3.1x less CPU; it stays
faster when it runs under the higher load (a2, b1). The gain for an all-core
change (b) comes from one lib process and the heavy exclusion; a CLI-only
change (a) gains also from the narrowed selection. The remaining local cost
is `core-it` under nextest (~330-990 s depending on load). The batch-end full
run took 23 minutes and its heavy report says the static list needs
re-measuring (38 entries under 10 s, 2 rust conformance tests over).

Raw logs and the script stayed in the slice's scratch directory.
