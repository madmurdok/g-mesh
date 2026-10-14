# GM-584: make the real rust-analyzer memory-limit tests deterministic

Status: diagnosis done (S1). Cause reproduced locally; fix proposed, awaiting
the owner's decisions below.

## Problem

Release run 38441041236 (v4.7.0, 28d1115), attempt 1, `tests (x86_64-apple-darwin)`,
section core-it, failed both limit-tripping tests in `core/tests/plugin_memory_limit.rs`:

- `a_real_rust_analyzer_over_its_memory_limit_is_suspended_and_structural_work_continues`
  at :349 ("a plugin over its memory limit must be put to sleep");
- `the_generated_mcp_instructions_reflect_a_real_suspended_rust` at :531
  ("rust was never suspended within the deadline; markers so far: []").

Attempt 2 of the same run passed, and CI run 38431504865 passed the same tree an
hour earlier on the same platform. The control test
(`with_a_memory_limit_far_above_real_usage_...`) passed in the failing attempt.

## How the tests decide

- `LOW_LIMIT_MB = 150`, justified in its doc comment by "the plugin alone is
  ~5-6MB, rust-analyzer plateaus at 535-580MB".
- Test 1 runs one `semantic_pass` and one `check_memory_limit` racing it on the
  plugin lock, so the check samples once, right after the pass returns.
- The MCP test relies on the daemon's supervise tick (30s) for 180s: about six
  samples of a settled rust-analyzer.
- `check_memory_limit_sampled_by` (core/src/daemon/lifecycle.rs) suspends only
  when two consecutive samples of `process_tree_rss_mb` exceed the limit.
- `process_tree_sample` (core/src/daemon/memory.rs) sums `sysinfo` RSS over the
  plugin pid and its descendant processes.

## Cause

The 150MB limit does not sit between "plugin alone" and "rust-analyzer". It
sits between rust-analyzer **with std** (~560MB) and rust-analyzer **without
std** (~45MB). The 535-580MB plateau is almost all sysroot (std/core/alloc)
def maps from cache priming. When rust-analyzer cannot load the sysroot, as on
the failing runner, the tree settles at ~45MB, nothing trips, and both tests
fail. Whether the sysroot loads depends on the runner, not on timing or load.

### Evidence from the failing CI log

- Test 1: semantic pass 4.47s, `pass_returned_at=4.78s`. With std loaded the
  pass takes 8-10s unloaded (below).
- No "confirming sample" and no "sampling unavailable" line, so the first
  sample answered and read <= 150MB.
- The MCP test sat through about six ticks without suspending: a settled
  footprint, not an early sample.
- Every rust-analyzer start in that job logged
  `ERROR cargo metadata failed on .../lib/rustlib/src/rust/library/Cargo.toml`.
  That is a sysroot load failure. The cause line is not in the relayed log.
  Locally the same command, with the plugin's pinned env and through the rustup
  proxy, exits 0, even with an empty `CARGO_HOME`. Why it fails on the runner
  is still unknown (see decision D3).

### Measurement (local x86_64 macOS, rust-analyzer 1.97.1)

Script: `gm584-measure.sh` in the session scratch dir. It applies a scratch
patch: an env-gated trace of each watchdog sample, plus an `#[ignore]` test that
samples `process_tree_sample` every 100ms with no limit set, through the pass
and for 15s after. N=5 per arm. Each arm runs both the footprint test and the
real test 1. Machine state was recorded per run (load average, `/usr/bin/time -p`).

| Arm | Condition | Load avg (1m) | Pass time | Plugin alone before pass | Tree at pass return | Peak | Settled (15s after) | Real test 1: watchdog sample, result |
|---|---|---|---|---|---|---|---|---|
| U | unloaded, std loads | 8-14 | 8.2-9.7s | 2-3MB | 562-568MB | 562-567MB | 562-567MB | 560-570MB, 5/5 pass |
| L | 16 x `yes` on 8 CPUs | 18-520 | 11.3-25.4s | 3-4MB | 544-563MB | 544-563MB | 544-563MB | 551-569MB, 5/5 pass |
| S | std unavailable (`cargo.sysrootSrc` = empty dir) | 5-15 | 3.5-5.2s | 3MB | 44-48MB | 56-126MB (transient) | 44-48MB | 44-46MB, **5/5 fail at :349** |

Tree members at peak in U/L: `g-mesh-plugin-rust` 7MB plus `rust-analyzer`
536-560MB. In S, the transient peak is rust-analyzer (~30MB) plus several
short-lived ~10-19MB children (cargo/rustc probes). They are gone by pass
return, so the settled tree is two processes, ~45MB.

- Arm S reproduces the CI failure exactly: the same assertion, a fast pass, a
  sample far under 150MB.
- Load does not move the footprint (L is within 25MB of U); it only stretches
  the pass. Load is not the cause.
- First attempt at arm S, with `RUST_SRC_PATH` pointing at an empty dir:
  rust-analyzer 1.97.1 ignored it (557-567MB, 5/5 pass). That attempt is a null
  arm and is not counted. `cargo.sysrootSrc` is what removed std. Unlike CI, it
  logs no error.
- GM-513 (build scripts and proc macros off) does not matter here: with std
  loaded the tree is ~560MB either way, and no proc-macro server appears in
  any arm's tree.

## Options for determinism

1. **Limit below rust-analyzer's no-std footprint**, e.g. `LOW_LIMIT_MB = 24`.
   The plugin alone is 2-7MB (3.4x margin under 24); rust-analyzer's settled
   tree is >= 44MB with or without std (1.8x margin over 24).
   - Benefit: one constant, and the test passes whether or not the runner
     loads std. It still asserts "rust-analyzer's own memory trips the limit".
   - Risk: the 1.8x margin depends on rust-analyzer's no-std footprint, which
     is measured only on x86_64 macOS. Linux and Windows no-std numbers are not
     measured (with std they are far above any of these limits).
2. **Limit from a baseline measured at run time**, e.g. the plugin alone +
   k MB, by starting the plugin once before the supervisor under test.
   - Benefit: adapts to each platform.
   - Risk: the baseline is 2-4MB and noisy, so k carries the whole margin
     and needs the same evidence as option 1. It adds a second plugin start
     and more code.
3. **Make the fixture reliably exceed 150MB**, either by making the runner
   load std or by growing the fixture.
   - Benefit: the test keeps its current meaning.
   - Risk: still environment-dependent; a runner that cannot load std fails
     again. A bigger fixture slows every run.
4. **Retry the check in test 1** until suspended or `TIMEOUT`, like the
   production tick.
   - Benefit: removes the single-sample dependence.
   - Risk: none for correctness, but it fixes nothing measured here. In every
     arm the tree had settled by pass return, and the MCP test already
     retries and still failed on CI.
5. **Precondition and diagnostics**: assert the plugin-alone baseline is under
   `LOW_LIMIT_MB / 2` before the pass, and print the sampled tree (members,
   MB) when the suspension assertion fails.
   - Benefit: the next failure names its cause.
   - Risk: a few lines.
6. **Pin the no-std case as its own test variant**: run test 1 a second time
   with `cargo.sysrootSrc` set to an empty dir in the manifest's
   initialization options.
   - Benefit: CI exercises the condition that failed on every platform, every
     run, not only when a runner happens to break.
   - Risk: requires injecting initialization options into the manifest the
     test hands the supervisor. One more ~7s real-rust-analyzer test.

## Proposed fix

Options 1 + 5, plus 6 if the owner wants the no-std case pinned (D2). Report
the CI sysroot failure as a separate product task (D3). No production code
changes.

### Edit map

- `core/tests/plugin_memory_limit.rs`
  - `LOW_LIMIT_MB`: 150 -> 24. Rewrite its doc comment with the three measured
    bounds: plugin alone 2-7MB, rust-analyzer without std 44-48MB settled, with
    std ~540-570MB. Drop the GM-290 history.
  - The fixture's doc comment ("plateau at 535-555MiB"): state that this is with
    std loaded, and that without std it is ~45MB.
  - Test 1: before the race, sample `process_tree_sample(pid)` and assert it is
    under `LOW_LIMIT_MB / 2`. On the `pid().is_none()` assertion failure,
    include a fresh `process_tree_sample` (members and MB) in the message.
  - MCP test: no logic change; it inherits the new limit.
  - (D2) A variant of test 1 with `cargo.sysrootSrc` pointing at an empty
    tempdir in the manifest's initialization options.
  - `TIMEOUT` doc comment ("9-25s for a cold pass"): still true with std;
    add 3.5-6s without std.

## Tests and controls

- Acceptance: 10 consecutive green runs of this file's tests: 5 unloaded and
  5 under `yes` load, plus 5 in the no-std condition (the D2 variant, or the
  scratch `cargo.sysrootSrc` patch from `gm584-measure.sh`). New tests that
  spawn processes run 5x in the tests slice and 5x in verify, once under load.
- Control 1 (task acceptance): disable the watchdog
  (`check_memory_limit_sampled_by` returns at once). Test 1, its no-std
  variant and the MCP test must fail.
- Control 2: restore `LOW_LIMIT_MB = 150` in the no-std condition. Test 1
  must fail. Already observed 5/5 in arm S, so this control is known to tell
  the arms apart.

## Decisions for the owner

- **D1: limit value.** Recommended: 24MB, a 1.8x margin under the measured
  no-std floor. Alternative: 16MB, wider over rust-analyzer (2.75x) but only
  2.3x over the plugin's 7MB.
- **D2: pin the no-std case (option 6)?** Recommended: yes. Without it, CI
  checks the fix only on runners that happen to fail to load std.
- **D3: CI runner's sysroot failure.** On x86_64 macOS CI, rust-analyzer could
  not load std. In production that means a rust semantic tier that answers
  without std edges. Recommended: a separate task to capture the full
  `cargo metadata` error on that runner (e.g. a CI step running the same
  command with the plugin's env).

## Side finding (not this task)

During this slice, a cold `cargo build` with the user-level `sccache`
rustc-wrapper failed with E0463 ("can't find crate for `toml_edit`/`serde`")
on artifacts that existed. The same build with `RUSTC_WRAPPER=` succeeded
(129s). sccache may be serving unreadable rmeta to other worktrees too.
