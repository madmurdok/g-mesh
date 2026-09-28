# GM-429 before/after: early RA start (1a) and the TS build stamp (3)

Measured 2026-09-27, 19:48-20:11. This measures the two changes GM-429 made,
following [gm-429-speedup-proposal.md](gm-429-speedup-proposal.md) sections 1
and 3. The method comes from [gm-429-semantic-pass-time.md](gm-429-semantic-pass-time.md).

## Summary

| scenario | metric | before (median, range) | after (median, range) | saving |
|---|---|---|---|---|
| cold start | activation -> rust pass applied | 66.2 (64.3-68.8) | 63.3 (62.9-63.8) | **2.9s** (1.4-5.5 per interleaved pair) |
| cold start | walk done -> all four passes done | 64.0 (61.5-66.2) | 60.6 (60.5-61.6) | **3.4s** |
| version bump | bump -> rust pass applied | 42.4 (28.8-44.9) | 29.4 (29.4-32.3) | **13.0s** median, but see below: ~3-4s is attributable to change 3 |
| version bump | RA build-script phase | 6.2 (5.2-7.2) | 3.3 (3.2-3.3) | **2.9s** |
| version bump | `g-mesh` build-script run (cargo `--timings`) | 4.03 (3.74-4.18) | 0.97 (0.91-0.99) | **3.1s** |

- **1a works as designed, but saves less than the proposal expected.** The
  after arm starts rust-analyzer 6.0-8.5s *before* core asks for the rust
  pass. The before arm starts it 1.3-2.1s *after* the request. The proposal
  estimated ~10-16s. The saving is 2.9s because the head start is only 6-8.5s
  here: the walk plus the go and python passes took 10.5-14.4s, against
  15.7-16.8s in S1. Also, RA is still not ready when the request arrives. In the
  after arm `wait_ready` still took 9.7-13.4s after the request, against
  16.8-17.6s in the before arm. So the pass waits 4-7s less. Answering then took
  about 1.5s longer (35.6-37.2s against 34.3-35.9s), probably because RA's cache
  priming now overlaps the answering.
- **3 cuts the build-script step to about 1s rather than below 0.5s.** What is
  left is the build script's other work: the Go plugin build and the input
  digest. The before arm's `g-mesh` build-script ran in 3.7-4.2s, not S1's
  6.86s (the machine was quieter). RA's build-script phase shrinks by 2.9s,
  and the pass's `wait_ready` finishes 0.5-4s earlier.
- **The 13s median bump saving is mostly not change 3.** Rust-analyzer's
  answering took 30.2s and 30.9s in two of the three before-arm bumps. In the
  third (r3-base) it took 18.4s, which is the after arm's range (19.4-22.5s).
  r3-base's end-to-end time, 28.8s, falls inside the after arm's range.
  Answering happens after RA's build scripts and proc-macro load are done in
  every run (timeline below), so the build-script change does not plainly
  explain the 11s. With n=3 this is bimodal answering variance. It could also
  be an indirect effect of `npm run build` rewriting
  `plugins/typescript/dist` during the pass. These runs cannot tell the two
  apart. Credit change 3 with ~3-4s per bump. That is what the build-script
  phase and `wait_ready` show.
- **Edges are identical** between arms in every run of both scenarios (below).

## Method

- Code:
  - Before arm, binaries: `release-3.15.0` at `a83e3d9`.
  - After arm, binaries: `perf/GM-429-semantic-pass-time` at `66f8293`.

  Each arm had a throwaway build worktree:
  `cargo build --workspace --release`, `npm ci && npm run build` in
  `plugins/typescript`, and `npm ci` in `plugins/python`. The two builds finished
  before any measurement started, and nothing was built during it.
- Instrumentation, identical in both arms and in the build worktrees only:
  - S1's `GM429` bridge and client lines (`plugins/sdk/src/lsp/{bridge,client,mod}.rs`):
    server spawn, pass start, `wait_ready`, `run_pass` request counts and
    wall time.
  - Two core lines in `apply_semantic_pass_in` (`core/src/watcher/apply.rs`):
    `sem-req` just before the `semanticPass` request is sent, and `sem-applied`
    after its diff is committed. Both carry the language and `incomplete`.
  - rust-analyzer's own log (`RA_LOG_FILE`, S1's `RA_LOG` filter): its start
    line, and "Running build scripts" to "set build scripts to workspaces".
- Indexed project, one worktree per arm. Change 3 lives in the indexed
  project's own `core/build.rs`, so each arm needs its own build.rs. The edge
  sets can only be compared if the rest of the source is identical. So both
  project worktrees hold the `66f8293` tree. The before arm's has the build-stamp
  commit `e0f2b4c` reverted, uncommitted. That means `core/build.rs` and
  `core/Cargo.toml` identical to `a83e3d9`, and no `core/ts_build_stamp.rs` or
  `core/tests/ts_build_stamp.rs`. Each project worktree has `npm ci` in
  `plugins/typescript`, so `core/build.rs` does a developer's real
  `npm run build`, or skips it in the after arm. Each project's `target/` was
  kept across runs, as a developer's is. Before either arm's measured runs, a
  discarded warm-up run filled it from empty.
- Isolated `G_MESH_HOME`: `models` linked, and the owner's embedding cache copied
  with `.backup`.
- One run, as one script, for each run in the order w-gm429 (dry run),
  w-base, r1-base, r1-gm429, r2-base, r2-gm429, r3-base, r3-gm429:
  1. Wait until load1 < 8.
  2. Delete the index and start `g-mesh daemon` under `/usr/bin/time -p`. Activate
     it with one MCP call through `mcp-shim`.
  3. **Scenario 1 (cold start):** wait for all four "semantic pass over the
     freshly built index" lines, then dump the edges.
  4. Wait 20s.
  5. **Scenario 2 (version bump on the warm daemon):** set `core/Cargo.toml`'s
     version to a patch number that arm's `target/` had never built (3.15.1001
     upward, one per bump), then wait for the rust `sem-applied` line.
  6. Run `g-mesh stop` and SIGKILL the daemon's remaining process tree, then dump
     the edges.
  7. **Scenario 2 control:** bump to another fresh version and rerun RA's exact
     build-script command (`cargo check ... --compile-time-deps --all-targets`,
     with the nightly channel override RA sets) with `--timings`. Read the
     `g-mesh` "build-script (run)" unit's duration.
- The cold-start metric "walk done" is core's "initial index built" line.

## Machine state

- MacBook, Intel i7-1068NG7, 4 physical / 8 logical CPUs, 32 GB, macOS.
  rust-analyzer 1.97.1 (8bab26f4 2026-07-14).
- `uptime` before the set: load 12.1 / 26.7 / 19.6. The two release builds had
  just finished, and w-gm429 started at load 7.9 / 32.6 / 31.4. The 1-minute load
  at each measured run's start was 4.1-6.1. Other than these runs, nothing was
  building or measuring. The owner's other daemons were left alone.
- `/usr/bin/time -p` wraps the daemon, and its user/sys excludes
  rust-analyzer, as S1 explains. Daemon life (cold + bump), real/user/sys: before
  137.6/7.5/2.4, 142.5/7.7/2.5, 121.9/7.4/2.4; after 122.5/7.1/2.6,
  122.1/7.1/2.5, 123.3/7.0/2.4.
- After the set, no process of the arms was alive and no rust-analyzer carried
  this run's `RA_LOG_FILE`. No run needed a SIGKILL of leftovers.

## Scenario 1: cold start

Offsets are in seconds from activation unless noted. "RA spawn vs req" is the
bridge's server spawn minus core's rust `semanticPass` request. A negative
value means RA was started before the request.

| run | arm | load1 | act -> rust req | RA spawn vs act | RA spawn vs req | act -> rust applied | walk done -> all 4 | `wait_ready` after req | answering | requests (deferred) | complete |
|---|---|---|---|---|---|---|---|---|---|---|---|
| w-gm429 | after (warm-up, cold `target/`) | 7.93 | 20.8 | +8.5 | -12.3 | 158.6 | 154.4 | - | - | 29,459 (0) | yes |
| w-base | before (warm-up, cold `target/`) | 7.75 | 18.9 | +21.1 | +2.1 | 168.6 | 163.9 | - | - | 29,347 (0) | yes |
| r1-base | before | 5.83 | 10.5 | +11.8 | +1.3 | 66.2 | 64.0 | 17.6 | 35.9 | 29,347 (0) | yes |
| r1-gm429 | after | 4.52 | 12.0 | +6.0 | -6.0 | 63.8 | 61.6 | 12.9 | 37.2 | 29,459 (0) | yes |
| r2-base | before | 4.28 | 13.5 | +14.8 | +1.4 | 68.8 | 66.2 | 17.4 | 35.9 | 29,347 (0) | yes |
| r2-gm429 | after | 6.12 | 12.5 | +6.1 | -6.4 | 63.3 | 60.6 | 13.4 | 35.6 | 29,459 (0) | yes |
| r3-base | before | 5.09 | 11.1 | +12.4 | +1.3 | 64.3 | 61.5 | 16.8 | 34.3 | 29,347 (0) | yes |
| r3-gm429 | after | 4.09 | 14.4 | +6.0 | -8.5 | 62.9 | 60.5 | 9.7 | 37.0 | 29,459 (0) | yes |

The arms' `RA spawn vs req` never overlaps, so the control separates the arms.
Before: +1.3 to +2.1s, the spawn inside the pass. The S5 verifier saw +4.9s.
After: -6.0 to -8.5s, right after the walk (-12.3s in the cold-target warm-up).
RA's own log start line agrees within 0.3s in every run.

## Scenario 2: version bump on the warm daemon

Offsets are in seconds from the bump. The build-script phase is given as
"duration @ start offset". The before and after arms asked 29,347 and 29,459
requests, with 0 deferred, and every pass was complete. RA did not restart in
any run.

| run | arm | load1 | bump -> rust req | RA build scripts | RA proc macros load | `wait_ready` done | answering | bump -> rust applied | cargo `g-mesh` build-script run | cargo real |
|---|---|---|---|---|---|---|---|---|---|---|
| w-gm429 | after (warm-up) | 15.24 | 4.4 | 4.1 @ +1.5 | - | - | - | 33.9 | 1.03 | 2.48 |
| w-base | before (warm-up) | 6.97 | 4.1 | 7.0 @ +0.9 | - | - | - | 45.0 | 3.92 | 5.06 |
| r1-base | before | 4.94 | 3.7 | 6.2 @ +0.8 | +7.1 | +11.9 | 30.2 | 42.4 | 4.18 | 5.86 |
| r1-gm429 | after | 5.05 | 3.4 | 3.3 @ +1.5 | +4.9 | +9.7 | 19.4 | 29.4 | 0.91 | 2.51 |
| r2-base | before | 8.70 | 4.1 | 7.2 @ +1.6 | +8.9 | +13.7 | 30.9 | 44.9 | 4.03 | 5.20 |
| r2-gm429 | after | 4.96 | 3.4 | 3.2 @ +1.4 | +4.7 | +9.4 | 19.8 | 29.4 | 0.99 | 2.28 |
| r3-base | before | 3.67 | 3.6 | 5.2 @ +0.8 | +6.0 | +10.1 | 18.4 | 28.8 | 3.74 | 4.73 |
| r3-gm429 | after | 4.17 | 3.5 | 3.3 @ +1.4 | +4.8 | +9.6 | 22.5 | 32.3 | 0.97 | 3.04 |

The `g-mesh` build-script run is the control that separates the arms: before
3.74-4.18s, after 0.91-0.99s. The w-gm429 dry run's 1.03s was read by hand
from its `--timings` file, because the first parser version missed the unit
name. The after arm stays above the expected < 0.5s. What remains is the
build script's other work: the Go plugin build (S1: `go` 0.3s CPU) and the
digest of the TS inputs.

## Edge-set control

After every cold start and every bump, the script dumped the rows
`source='semantic' AND engine='rust-analyzer'`. Each row was joined to its
endpoints' file path, qualified name and start line, and the dump was sorted. It
excluded edges with an endpoint in a file commit `e0f2b4c` touches: the before
arm's source lacks those. Every run's dump in both scenarios is identical to
the reference run w-gm429's, in both the path form and the edge ids. The totals
are 2,712 before and 2,719 after. The difference is exactly the 7 edges from
`core/build.rs` / `core/ts_build_stamp.rs`, which exist only in the after arm's
source. The request counts, 29,347 against 29,459, differ by the questions those files add (not checked site by site).
No arm is disqualified.
