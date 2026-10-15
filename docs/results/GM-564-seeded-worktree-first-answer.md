# GM-564: seeded worktree index, time to first answer

Measure slice S14 of `docs/design/GM-564-seed-worktree-index.md` ("Measure protocol"). Binary: `cargo build --release --workspace` of branch `feat/GM-564-seed-worktree-index` (tip 23bfd63), with its bundled plugins.

## Result (medians of 3, interleaved)

| Arm | initialize -> first `find_definition daemon::run` | daemon start -> first answer | Ready | seed copy | reconcile |
|---|---|---|---|---|---|
| cold (`G_MESH_SEED=0`) | 15.3 s | 15.7 s | 164.6 s | - | - |
| seeded (default) | 1.15 s | 1.95 s | 2.1 s | 564-718 ms | 567 files, 203-306 ms, 0 deleted / 0 created / 0 reindexed |
| seeded-diverged (`dfc9cac~50`) | 25.6 s | 26.4 s | 25.3 s | 501-638 ms | 595 files, 19.4-79.4 s, 470 deleted / 28 created / 121 reindexed |

Seeded is about 13x faster to the first answer than cold and 80x faster to Ready. The diverged arm is slower to the first answer than cold's 15 s: `dfc9cac~50` differs from the seed source in 968 files (281k lines added), so the reconcile reparses 121 files and deletes 470, and it holds the tool answers until it ends. It is still 6x faster to Ready than cold (no embedding backfill, no whole-project semantic pass).

## Per run

`init->ans` = MCP `initialize` sent to the first answer; `start->ans` = from launching the daemon (includes the seed copy and plugin start). `Structural` is the phase file's first `structural`; the seeded arms go `walking` -> `embedding` inside one 10 ms poll (the seed gate moves the phase once the reconcile is done), so their Structural equals the first answer within a poll. Load = 1-minute load average from `uptime` before the run (8 cores). `time -p` = real/user/sys of the daemon process.

| Run | load1 | init->ans s | start->ans s | Structural s | Ready s | seed copy ms | reconcile | index.db | target/ created | time -p real/user/sys | seed_origin row |
|---|---|---|---|---|---|---|---|---|---|---|---|
| cold-1 | 7.7 | 20.68 | 21.33 | 21.34 | 193.9 | - | - | 118.7 MB | no | 194.21/14.18/9.41 | no |
| cold-2 | 7.3 | 14.61 | 15.03 | 15.03 | 164.6 | - | - | 118.6 MB | no | 164.98/12.02/8.67 | no |
| cold-3 | 7.6 | 15.31 | 15.71 | 15.73 | 159.3 | - | - | 118.7 MB | no | 159.73/11.52/8.16 | no |
| seeded-1 | 7.3 | 0.96 | 1.88 | ~1.89 | 2.1 | 676 | 567 f / 250 ms: -0 +0 ~0 | 119.0 MB | no | 2.15/0.52/0.81 | yes |
| seeded-2 | 7.2 | 1.37 | 2.41 | ~2.41 | 2.6 | 718 | 567 f / 306 ms: -0 +0 ~0 | 119.0 MB | no | 2.75/0.63/0.92 | yes |
| seeded-3 | 8.0 | 1.15 | 1.95 | ~1.96 | 2.1 | 564 | 567 f / 203 ms: -0 +0 ~0 | 119.0 MB | no | 2.22/0.51/0.73 | yes |
| diverged-1 | 6.9 | 79.73 | 80.47 | ~80.4 | 80.5 | 587 | 595 f / 79409 ms: -470 +28 ~121 | 119.0 MB | no | 80.91/233.31/10.89 | yes |
| diverged-2 | 6.9 | 25.59 | 26.38 | ~25.3 | 25.3 | 638 | 595 f / 24150 ms: -470 +28 ~121 | 119.0 MB | no | 26.82/6.69/10.4 | yes |
| diverged-3 | 8.0 | 20.33 | 20.96 | ~20.3 | 20.3 | 501 | 595 f / 19362 ms: -470 +28 ~121 | 119.0 MB | no | 21.35/5.52/8.26 | yes |

Reading notes:

- diverged-1 (80 s, user 233 s) is an outlier against 25 s and 20 s; the reconcile line says the same work (595/470/28/121). Its user time shows a CPU-bound reparse; the cause was not isolated (first run after the three-arm cold/seeded warm-up, machine load comparable). The median is used; the range is 20-80 s.
- `time -p` covers the daemon process and the children it reaped; a plugin or rust-analyzer still alive when the daemon is stopped is not in user/sys, so cold's 12-14 s user understates the machine cost (the separate full index of the source took user 2801 s, real 951 s, with a cold embedding cache).
- `target/` was never created in any arm, including cold: no rust-analyzer build ran in the worktree within these runs, so "build avoided" is not observable here (the arms do not differ on it).
- Embedding cache: all arms share the fresh `G_MESH_HOME`'s machine-wide embedding cache, warmed by indexing the source clone, so cold's Ready (160-194 s) is a warm-cache cold and understates a first-ever cold index.
- seeded index.db is 0.3% larger than cold's (119.0 vs 118.7 MB): the copy is the complete source index, plus the `seed_origin` table.

## Sanity checks

- A/B: the `index seeded from <source>/ in N ms` and `the index is a seed - reconciling` log lines and a `seed_origin` row (sourceRoot = the clone, `reconciledAt` set) exist in all 6 seeded runs and in none of the 3 cold runs (its `seed_origin` table is empty).
- Correctness: `find_definition` for `daemon::run` (`core/src/daemon/mod.rs` 293-534), `stored_generation` (`core/src/storage/schema.rs` 764-777), `project_hash` (`core/src/daemon/identity.rs` 9-17) and `write_ndjson_frame` (`core/src/protocol/ndjson_frame.rs` 29-37): the full response of every seeded run equals cold's (6 of 6 identical to cold-1; cold runs also identical to each other).
- Diverged runs are at an older tree, so only content that did not change can be compared: `write_ndjson_frame` gave the same file and lines as cold (the file is unchanged between the revisions); `project_hash` moved by one line (8-16), `stored_generation` and `daemon::run` do not exist at `dfc9cac~50` (the answer is a semantic-neighbour fallback), consistent with the older tree.

## Machine state and method

- MacBook, Intel i7-1068NG7 (8 logical cores), 32 GB; macOS 25.6. The machine was not quiet: load average 7-8 before every run (other sessions and `opswat-gears-od`, `claude`, Chrome), below the 2x cores = 16 abort line; a run was refused at load 26 right after the binary build and waited for load < 8. Results carry that noise; 3 interleaved runs per arm (cold, seeded, diverged, repeat).
- Isolation: `G_MESH_HOME=/tmp/g14` (a symlink to a fresh scratchpad directory: the daemon socket path limit of 103 bytes rejects the long scratchpad path). A `git clone` of the g-mesh repo at `dfc9cac` (release 4.9.0) served as the "main" checkout; it was indexed to `ready` with the branch binary (cold, `G_MESH_SEED=0`: 951 s), then each measured worktree was `git worktree add --detach` from that clone and removed afterwards (`git worktree remove --force`); no daemon or process referencing a measured worktree remained after any run. `npm ci` was not run in any arm.
- Driver: `/private/tmp/.../scratchpad/s14/run.py`, a small Python script. It starts `/usr/bin/time -p g-mesh daemon --project-root <wt>` (the shim's own bootstrap spawns the same command), polls `index.phase` every 10 ms for phase times, then speaks newline-delimited JSON-RPC over the real `g-mesh mcp-shim` stdio (`initialize`, `notifications/initialized`, `tools/call find_definition` with `symbol_name: "daemon::run"`, `include_source: false`), waits for `ready`, runs the three spot-check `find_definition` calls, reads `index.db` size and `seed_origin`, then `g-mesh stop`.
