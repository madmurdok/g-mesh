# GM-401 first-answer idle gap: measurement and attribution

Slice S1 (measure) of GM-401. It explains the gap GM-395's M2 found
(`docs/results/gm-395-lazy-indexing.md`, "## M2. Real timings"). There the
daemon reported `waited_ms=22721`, but the client got its first
`get_file_outline` answer only at 93.2s, and the daemon sat at 0.0% CPU
from t=40s to t=122s. No code under `core/` or `plugins/` was changed to
produce these numbers.

## Summary

- **The gap reproduces**: 101.3s in run 1 and 75.7s in run 2, against
  22.5s and 24.1s of daemon-reported `waited_ms`.
- **Stage.** The gap sits between `prepare: done` and the reply. That is,
  inside `get_file_outline`'s `ensure_file_fresh`, after the indexing wait
  has ended. Stack samples of the daemon put the handler thread in
  `ensure_file_fresh → PluginRegistry::ensure_fresh → staleness::ensure_fresh
  → apply_file_change → round_trip` for the whole gap. The reply lands
  within 10ms of the rust plugin's
  `[rust] semantic pass: 1 file(s) ... in 66.5s / 38.7s` line.
- **Cause (g-mesh's own).**
  - The bulk walk writes no `indexed_files` baseline. After a fresh walk the
    table is empty, and run 1's index held exactly one row afterwards: the
    queried file.
  - So the first query on *any* file reads as "never indexed". The query
    then does a full synchronous reindex of that file before answering:
    1. a structural `fileChanged` round trip. It also embeds the file's
       nodes, which means loading the embedding model on first use. For
       `core/src/mcp/mod.rs` this took ~10-12s at ~390% daemon CPU.
    2. a per-file `semanticPass` round trip. For a Rust file this cold-starts
       rust-analyzer: `cargo metadata`, build scripts, proc macros, then
       readiness. That took 38.7-66.5s, with the CPU in rust-analyzer and
       its children and the daemon at 0%.
  - No progress heartbeats are sent during this phase. They stop at
    `wait over`, which leaves a silent stretch of 51-77s.
- **What the M2 numbers meant.** M2's "~380% at t=30-40s (the walk)" was
  not the walk. The walk's CPU is spent in the plugin processes. That burst
  was the embedding step of this reindex. M2's 0% stretch was the daemon
  waiting on rust-analyzer. M2's own log carries the same signature:
  `[rust] semantic pass: 1 file(s), 0 node(s)/0 edge(s) upserted ... in
  58.669087074s`.
- **Hypotheses:**
  - **(c): confirmed.** It is work after the wait, in child processes.
  - **(b): refuted.** Everything before `prepare` took 0.15-1.7s.
  - **(a): refuted as the cause.** g-mesh's plugin binaries were not
    freshly built at all. Gatekeeper did add a one-time ~26s to run 1,
    through things rust-analyzer's `cargo check` touched (see below).
  - **(d): a magnitude factor, not the cause.** The gap is 51s at load 4-6.

## Environment

- g-mesh commit `0d122a7` ("chore: bump the workspace to 3.12.0"), branch
  `fix/GM-401-first-answer-idle-gap`.
- Build: `cargo build --release --workspace`. It started at
  2026-09-24T13:13:42+0300 and finished at 13:14:53
  (real 70.65, user 292.51, sys 9.62). It rebuilt the workspace crates only,
  because of the version bump. `target/release/g-mesh` has a new sha256,
  `863952f4…`, and an mtime of 13:14:53.
- `plugins/typescript/dist` was already present.
- `target/release/g-mesh` was not executed between the build and run 1.
  One unrelated binary was: `g-mesh-fake-lsp` ran at 13:15:23 to check that
  `sample(1)` works. The daemon never runs it.
- **Which plugin binaries the daemon actually runs.** The checked-in
  manifests point at `plugins/rust/plugin.toml` → `../../target/debug/g-mesh-plugin-rust`
  and `plugins/python/plugin.toml` → `../../target/debug/g-mesh-plugin-python`.
  Both were built at 04:40:38. The Go plugin is
  `plugins/go/g-mesh-plugin-go`, built 2026-09-23 23:05. TypeScript runs
  under `node`. All of them had been executed before (M2 ran at 05:20). So
  the release build refreshed none of them.
- Machine: a loaded dev machine (7 users, an unrelated long-running
  rust-analyzer pid 28154, SentinelOne `sentineld` active).
  `uptime` per run:

  | run | start | load before (1/5/15m) | load after |
  |---|---|---|---|
  | 1 | 13:16:07 | 12.47 12.85 8.52 | 5.48 10.24 8.00 |
  | 2 | 13:20:26 | 6.29 8.23 7.51 | 5.68 7.60 7.32 |
  | 3 | 13:22:54 | 4.18 6.72 7.01 | 6.57 7.13 7.15 |
  | 4 | 13:28:07 | 9.74 8.20 7.55 | 9.74 8.20 7.55 |

## Method

- **Setup.** The project was the whole g-mesh repo. Each run had a fresh
  `G_MESH_HOME=/tmp/gm401-N/home`, except run 4, which reused a copy of
  run 2's. The runs used the release binary with `G_MESH_TRACE_CALLS=1`.
- **Embedding backfill.** It was held with `G_MESH_EMBED_PASS_HOLD_FILE`.
  That pass runs on the activation thread only after the semantic pass
  (`daemon/activation.rs`), so holding it changes nothing before the first
  answer.
- **Scripts.** All are in the session scratchpad, `gm401/`:
  - `client.py`: a raw MCP client. It spawns `g-mesh mcp-shim` and writes
    wall-clock timestamps for the `initialize` send and reply, each
    `tools/call` send and reply, every progress notification and the shim's
    stderr. Its calls carry a `progressToken`.
  - Every ~9s while a call was pending, the client also ran
    `sample <daemon pid> 1` for thread stacks.
  - `tailer.py`: the daemon log has no timestamps of its own, so this
    follows `G_MESH_DAEMON_LOG` every 20ms and prefixes each line with the
    time it was first seen.
  - `sampler.py`: every 1s it runs `ps -axo pid,ppid,%cpu,time,command`,
    finds the daemon (pid from `daemon.serving`), and writes the daemon and
    **all its descendants**.
- Each run was wrapped in `/usr/bin/time -p`. That covers the client and
  shim only; the detached daemon is not a child.
- **Gatekeeper.** Scans were counted afterwards with `/usr/bin/log show
  --predicate 'process == "syspolicyd"'`.
- Raw logs are in `/tmp/gm401-{1,2,3,4}/`: `events.log`, `daemon.ts.log`,
  `cpu.log`, `stack-*.txt`, `uptime.*` and `time.txt`.

## Runs and per-stage timelines

Times are seconds from the client spawning the shim.

### Run 1: first execution after the fresh build (`core/src/mcp/mod.rs`)

`/usr/bin/time -p`: real 102.79, user 2.18, sys 2.18.

| t (s) | event | evidence |
|---|---|---|
| 0.008 | `initialize` sent | client |
| 1.270 | shim: "nothing is serving - starting a daemon" | shim stderr |
| 1.660 | `initialize` reply | client |
| 1.661 | `tools/call get_file_outline` sent; `prepare: entered` | client, daemon log |
| 17.98 / 18.34 / 23.03 / 23.86 | go / python / rust / ts bulk index complete | daemon log |
| 24.18 | `wait over ... waited_ms=22493 progress_sent=4`; `prepare: done` | daemon log (last heartbeat at 21.66) |
| 25-35 | daemon ~390% CPU (1.7 → 34.6 CPU-s): `apply_file_change` embedding mod.rs's nodes, incl. `EmbeddingModel::load` | cpu.log, stack at 25.6 |
| 34.80 | `[rust] semantic tier: rust-analyzer` (RA spawned for the per-file pass) | daemon log |
| 37-55 | RA runs `cargo metadata` and `cargo check --workspace`. That re-ran core's `build-script-build`, which ran `npm run build` → `tsc` (210% CPU); 29 files in `plugins/typescript/dist` were rewritten | cpu.log tree, file mtimes |
| 60-89 | RA and proc-macro-srv at **0% CPU**, RA CPU time flat at 12.50s | cpu.log |
| 52-89 | syspolicyd logs ~25 serial `GK evaluateScanResult ... MacOS error: -67062` (unsigned code), one every ~1.5-2s; the last one is at 13:17:36 = +89 | `log show` |
| 89-99 | RA at 400% CPU (analysis) | cpu.log |
| 101.25 | `[rust] semantic pass: 1 file(s) ... in 66.519856883s` | daemon log |
| **101.26** | **reply** | client |

Stacks at 35-101s (e.g. `stack-0054.2.txt`):
- the tokio blocking thread is in `ensure_file_fresh → ... →
  apply_file_change → round_trip → read_message_with_timeout`;
- the `g-mesh-activation` thread is in `semantic::run_with_registry →
  semantic_pass → round_trip`, driving the go and python passes.

### Run 2: the same binaries, all executed once already (`core/src/mcp/mod.rs`)

`/usr/bin/time -p`: real 75.80, user 1.71, sys 1.72.

| t (s) | event |
|---|---|
| 0.276 | shim: "nothing is serving" |
| 0.620 | `initialize` reply; `get_file_outline` sent |
| 0.639 | `prepare: entered` |
| 24.31 | last bulk index complete (ts) |
| 24.69 | `wait over ... waited_ms=24056 progress_sent=4`; `prepare: done` |
| 25-37 | daemon 330-397% CPU (2.1 → 39.6 CPU-s): embedding in `apply_file_change` |
| 36.98 | `[rust] semantic tier: rust-analyzer` |
| 38-42 | RA `cargo metadata` only, ~0.4 CPU-s; no build script rerun and no tsc |
| 41-65 | RA at ~40% CPU (one core, workspace load); pyright at 20-180% alongside |
| 66-71 | RA at 320-450% |
| 75.65 | `[rust] semantic pass: 1 file(s) ... in 38.679746634s` |
| **75.66** | **reply** |

Gatekeeper `evaluateScanResult` lines in the run-2 window: **0**.

### Run 3: control on the post-wait path (`README.md`, then `plugins/go/walk.go`, then `core/src/lib.rs`)

`/usr/bin/time -p`: real 46.35, user 0.83, sys 0.91.

| call | sent | `prepare: done` | reply | note |
|---|---|---|---|---|
| `README.md` (no language) | 0.146 | 24.604 | **24.602** | `ensure_fresh` returns at once when no language owns the file; the reply comes 0.0s after `wait over` (answer: "no file found", as expected) |
| `plugins/go/walk.go` | 25.452 | 25.458 | **44.994** (19.5s) | stacks at 30.5 and 39.8 show `ensure_file_fresh` in `psynch_mutexwait` on the go plugin, which activation's whole-project go semantic pass holds until 42.7; then `file changed` at 44.30 and a 0.447s 1-file go semantic pass. The daemon sat at 0% CPU from 25 to 42 |
| `core/src/lib.rs` | 44.994 | 44.999 | **46.227** (1.2s) | reindexed but did not wait on RA; RA was spawned only at 46.22, after the reply (lib.rs is `mod` lines only) |

Gatekeeper lines in the window: **0**.

### Run 4: the same file with its `indexed_files` baseline present (`core/src/mcp/mod.rs`)

This run used a copy of run 2's `G_MESH_HOME`. Run 2's only `indexed_files`
row is `core/src/mcp/mod.rs`, written by run 2's first-touch reindex.
`/usr/bin/time -p`: real 0.83.

| t (s) | event |
|---|---|
| 0.688 | `get_file_outline` sent |
| 0.700 | `prepare: done` (`waited_ms=0`) |
| **0.711** | **reply**, 23ms after sending; rust-analyzer was not running yet (spawned at 21.8 for the owed semantic retry) |

This is the control for the attribution:
- **Same file, cold rust-analyzer, baseline present:** 23ms.
- **Baseline absent** (runs 1 and 2): 51-77s after `prepare: done`.

## Attribution

**Stage.** The whole gap falls between `prepare: done` (24.2 / 24.7s) and
the reply (101.3 / 75.7s). It is `get_file_outline`'s `ensure_file_fresh`
step (`core/src/mcp/mod.rs`,
`self.ensure_file_fresh(&params.0.file_path).await`). The trace does not
cover this step; `waited_ms` stops at `wait over`.

**Cause.**
1. **The first touch of a file after a fresh walk counts as stale.** The
   bulk walk records no `indexed_files` baselines. `cli/status.rs` already
   notes "A bulk-indexed file with no `indexed_files` row". So
   `staleness::decide` finds no prior record and returns `NeedsReindex`.
   Evidence: run 1's index held 1 `indexed_files` row after the run, the
   queried file.
2. **The reindex is synchronous and does all the work before answering.**
   `watcher::apply::apply_file_change` does two things before
   `ensure_fresh` returns:
   - the `fileChanged` round trip. This includes embedding the file's
     nodes, so on first use it also loads the ONNX model.
   - a `semanticPass` round trip for that one file.

   The SDK's per-file budget for that pass is `single_file = 90s`
   (`plugins/sdk/src/lsp/bridge.rs`), and its readiness wait counts
   against that budget.
3. **For a Rust file, the semantic pass cold-starts rust-analyzer.** That
   means spawning it, `cargo metadata`, and a `cargo check` of build scripts
   when they are stale, followed by proc-macro loading and indexing. It
   costs 38.7s warm (run 2) and 66.5s right after a workspace change
   (run 1). All of this CPU is in rust-analyzer's process tree. The daemon
   sits at 0%, which is exactly what M2's daemon-only sampler recorded.
4. **A second contention of the same kind.** If the queried language's
   plugin is busy in activation's whole-project semantic pass, the
   first-touch reindex also waits on that plugin's mutex. Run 3's Go call
   waited 17s this way.
5. **The client hears nothing during this phase.** Progress heartbeats come
   only from `wait_for_index`, so none are sent from `wait over` to the
   reply. In runs 1 and 2, `progress_sent=4` and then silence for 77s and
   51s.

**Why run 1 was 26s slower than run 2.** Hypothesis (a) applies here, but
not to g-mesh's own binaries:
- **The one fresh g-mesh binary.** `target/release/g-mesh` cost ~1s on first
  execution. Gatekeeper `performScan` ran from 13:16:07.29 to 08.28 and
  again from 08.95 to 09.48. As a result the shim printed "nothing is
  serving" at 1.27s (0.28s and 0.03s in later runs), and the `initialize`
  reply came at 1.66s (0.62s and 0.15s).
- **rust-analyzer's `cargo check` re-ran core's build script.** The 3.12.0
  bump made the debug build state stale. The script produced a fresh
  `build-script-build` and ran `npm run build` → `tsc`, costing ~15s at
  37-55s.
- **Serial Gatekeeper scans while rust-analyzer waited.** Over 60-89s
  rust-analyzer and its proc-macro server sat at 0% CPU while syspolicyd
  logged ~25 `-67062` (unsigned code) evaluations. These were serial, one
  every 1.5-2s, and stopped at +89, exactly when rust-analyzer resumed.
  Runs 2 and 3 logged none.
- **The scanned files are not identified.** syspolicyd logs only path
  hashes. Short-lived `cargo`/`rustup` processes spawned by rust-analyzer
  appear in the system log at 13:16:58-59 and 13:17:08-09.
- These are one-time costs, charged on the first rust-analyzer start after
  a workspace change. The machine was also at load 12.5 then, against 6.3.
  Without them the gap is still 51s (run 2).

**Hypotheses:**
- **(a) First-execution cost of freshly built plugin binaries: refuted as
  the cause.** The plugin binaries the daemon runs were not rebuilt. The gap
  reproduced in run 2 with every binary already executed. What (a)
  explains is run 1's extra ~1s at startup and, most likely, part of its
  extra 26s, through things rust-analyzer's `cargo check` produced or
  loaded.
- **(b) Time before the request reaches `prepare`: refuted.**
  `prepare: entered` came 0.02-1.66s after the client started, and within
  20ms of the `tools/call` send.
- **(c) Work after the wait, in child processes: confirmed.** Specifically:
  - the first-touch `ensure_file_fresh` reindex, with the embedding done
    in-daemon;
  - a synchronous per-file semantic pass that waits on a cold
    rust-analyzer.

  It is not the whole-project semantic pass. That pass ran on the
  activation thread in parallel and did not block the answer, except
  through the plugin mutex (run 3).
- **(d) Machine load: contributes, does not cause.** Run 2 at load ~6
  still gapped 51s. Run 1 at load ~12 was the slowest.

## Is the cause g-mesh's own?

**Yes.** The machine only changes the size of the gap. Two g-mesh
decisions create it:
- the bulk walk leaves no staleness baselines, so the first query per file
  pays a reindex;
- query-time reindexing runs the embedding and, above all, the per-file
  semantic pass synchronously. It is bounded only by the 90s
  `single_file` budget, and it sends no heartbeats.

This bites on the first call to every file after a fresh walk, not only
the first call of a session. It explains M2's 93s, and it can push a first
answer past Claude Code's 2-minute auto-background.

Directions for S2. These are not decided here.
1. **Write `indexed_files` baselines during the bulk walk.** With a
   baseline, the answer takes 23ms (run 4).
2. **Take the semantic pass off the query path.** Let `ensure_fresh`
   answer once the structural part is done, and run the semantic upgrade
   in the background.
3. **Send heartbeats while `ensure_fresh` runs.**

## Cleanup

Each run stopped its own daemon with `G_MESH_HOME=/tmp/gm401-N/home
target/release/g-mesh stop`, run in the project. Every `stop.txt` reports
the daemon terminated and its plugins exited or terminated. The final
check follows. The only matches are the check's own shell and `ugrep`.
rust-analyzer 28154 and its proc-macro-srv 28187 pre-date these runs
(started Sep 23 14:48) and are not g-mesh's.

```
$ ps -axo pid,command | grep -E 'g-mesh (daemon|mcp-shim)|bulk-index'
47122 /bin/zsh -c source .../shell-snapshots/snapshot-zsh-...sh ... eval 'ps -axo pid,command | grep -E ...'
47125 ugrep -G ... -E g-mesh (daemon|mcp-shim)|bulk-index
```

## After the fix (GM-401 S3)

Slice S3 (verify) of GM-401. Independent re-measurement of commit `74b8a04`
("fix: answer the first query after a walk without reindexing the file
(GM-401 S2)"), in a fresh worktree (`wt-gm401-ctl`) checked out detached at
that commit, never the branch's original working tree.

### Controls (each reverted with `git checkout -- .` before the next; worktree
confirmed clean via `git status --short` between every one)

| # | Reverted | Expectation | Result |
|---|---|---|---|
| 1 | `walk_baseline_for`'s `mtime >= cutoff` check dropped | `walk_baselines_vouch_only_for_files_untouched_since_the_walk_started` fails | **Failed as predicted**: `WalkBaselines { recorded: 3, skipped: 2 }` vs expected `{ recorded: 1, skipped: 4 }` (`fresh.rs`/`edge.rs` wrongly baselined) |
| 2 | `record_walk_baselines` call removed from `bulk_index::run_with_progress` | `the_first_query_after_a_walk_takes_the_fast_path` fails, empty table | **Failed as predicted**: `project.baselines()` returned `[]` instead of `["src/index.ts", "src/other.ts"]` |
| 3 | `staleness::decide` returns `AlreadyFresh` whenever a prior record exists | `a_file_edited_after_the_walk_is_still_reindexed` fails | **Failed as predicted**: outline came back `["size"]`, missing the post-walk edit (`"grown"` never seen) |
| 4 | Progress-ticker `select!` branch deleted from `ensure_file_fresh` | `a_slow_query_time_reindex_sends_a_heartbeat_then_the_full_answer` fails | **Failed as predicted**: "expected at least 3 progress notifications ... got 0: []" |

All four controls demonstrate the tests actually exercise the fix, not just
pass incidentally.

### fmt / clippy / full suite (on the untouched tip, worktree clean before and after)

- `cargo fmt --all -- --check`: clean, no diff.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean, exit 0, zero
  warnings.
- One full `cargo test -p g-mesh --no-fail-fast`: **931 passed, 0 failed, 7
  ignored** (plus the doctests/integration binaries: every `test result: ok`
  line across the run reports `0 failed`). The log was read in full and
  grepped for `FAILED|panicked|test result`; the only lines matching
  `failed`/`FAILED` as a keyword are test *names* describing failure-handling
  behavior (e.g. `a_failed_walk_is_a_tool_error_and_is_retried ... ok`), not
  actual failures. This full run had not been executed after the S2
  implementer's last assertion change; it now has, and it's green.

### Re-measurement

Built `cargo build --release --workspace` in the worktree (debug plugin
binaries and the TypeScript/Go plugins were already built during setup).
Reused the S1 harness (`gm401/run.sh`, `client.py`, `sampler.py`,
`tailer.py`), copied to `run-ctl.sh`/`client-ctl.py` pointing at
`wt-gm401-ctl`'s own `target/release/g-mesh` and using it as both the spawn
binary and the project root, otherwise unchanged. One cold run, fresh
`G_MESH_HOME=/tmp/gm401-ctl-1/home`, `get_file_outline` of
`core/src/daemon/mod.rs` (as specified for this slice; S1 run 2 used
`core/src/mcp/mod.rs`).

| | S1 (before, runs 1-2) | S3 (after, run 1) |
|---|---|---|
| `waited_ms` (walk) | 22,493 / 24,056 | 12,869 |
| client-observed first answer | 101.26s / 75.66s | **14.916s** |
| gap after `wait over` | 77.02s / 51.02s | **0.022s** (14.938 - 14.916 sent-vs-log clock skew; `ensure_fresh` itself took 6ms) |
| `ensure_fresh` outcome | (not traced; a synchronous reindex ran) | `outcome=AlreadyFresh elapsed_ms=6 progress_sent=0` |

Full trace (`daemon.ts.log`, times are wall-clock via `tailer.py`):

```
prepare: entered tool=get_file_outline request=2 progressToken=present
prepare: wait over tool=get_file_outline request=2 outcome=satisfied waited_ms=12869 progress_sent=2
prepare: past the indexing wait tool=get_file_outline request=2
prepare: done tool=get_file_outline request=2
ensure_fresh: tool=get_file_outline request=2 file=core/src/daemon/mod.rs outcome=AlreadyFresh elapsed_ms=6 progress_sent=0
```

Client's own timeline (`events.log`, seconds since the shim was spawned):
`tools/call` sent at +2.026s, two walk-progress notifications at +7.03s and
+12.04s, reply at **+14.916s**. `/usr/bin/time -p`: real 15.07, user 0.52,
sys 0.49 - real far exceeds user+sys, confirming the client process was
waiting on the daemon's walk, not CPU-bound itself.

The expectation set for this slice ("close to the walk's ~24s") is met and
exceeded: the walk itself was faster here (12.9s vs S1's 22.5-24.1s,
plausibly machine-load-dependent - `uptime` before the run read load
averages `53.00 166.17 119.31`, an extremely loaded machine, against S1's
4-12), and the post-wait gap that GM-401 targeted is gone: 6ms of
`ensure_fresh` instead of 51-77s of synchronous reindex + cold
rust-analyzer semantic pass. The old per-file semantic-pass cost (a Rust
file's rust-analyzer cold start) simply never runs, because the walk's own
baseline is trusted.

`uptime`:

| | before | after |
|---|---|---|
| S3 run 1 | 2026-09-24T14:27:06+0300, load 53.00 166.17 119.31 | 2026-09-24T14:27:21+0300, load 42.95 158.41 117.35 |

### Cleanup

`run-ctl.sh` stopped the daemon itself (`G_MESH_HOME=/tmp/gm401-ctl-1/home
target/release/g-mesh stop`, run in the worktree):

```
g-mesh: stopped the daemon for /Users/Valentin_Taiurskii/Projects/ClaudeProjects/wt-gm401-ctl
  daemon core: pid 95397 (terminated)
  plugin (go): pid 95476 (exited with its core)
  plugin (python): pid 95550 (exited with its core)
  plugin (rust): pid 95699 (terminated)
```

A follow-up check found nothing left over:

```
$ ps -axo pid,command | grep -E 'g-mesh (daemon|mcp-shim)|bulk-index|wt-gm401-ctl' | grep -v grep
(no output)
```
