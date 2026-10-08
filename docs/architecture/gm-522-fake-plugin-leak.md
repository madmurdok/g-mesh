# GM-522: test runs leaking `g-mesh-fake-plugin` and daemon processes

Diagnosis note (GM-522/S1). Branch `fix/GM-522-fake-plugin-leak`, off
`release-4.1.0` at `f15eda7`. No production code or tests changed here.

## 1. Report

After earlier integration work, 72 processes were alive: `g-mesh-fake-plugin
--language python|go|ruby|alpha|beta|rust --dir /var/folders/...` (debug build)
and one "g-mesh daemon".

## 2. Measurements

All counts are scoped to processes whose executable lives under this worktree's
`target/`. Each count was taken once, 30 s after the run ended. Load averages
were 57-413 throughout, because other agents' suites were running on the same machine.

| # | Run | Before | After (fake / daemon) |
|---|-----|--------|------------------------|
| 1 | `cargo nextest run -p g-mesh` (1867 passed, 17 skipped; real 1825 s, user 1298 s, sys 323 s) | 0 / 0 | **0 / 0** |
| A | `cargo test -p g-mesh --lib daemon::` (libtest, one process, 293 passed) | 0 / 0 | 0 / 0 |
| A' | same, sampled every 0.5 s | - | at most **7** fake plugins alive at once, so they do not pile up |
| B1 | A, test binary `kill -9`'d while 5 plugins were alive | 0 / 0 | 0 / 0 |
| C | `cargo nextest run -p g-mesh --lib daemon::`, nextest sent INT / TERM / KILL while 3-4 plugins were alive | 0 / 0 | 0 / 0 each |
| G | same, nextest sent `TSTP`, then `KILL` | 0 / 0 | 0 / 0 |
| **D** | `cargo test --test semantic_pass_trigger`, test binary `kill -9`'d while its daemons were up | 0 / 0 | **0 / 2**: `g-mesh daemon --project-root /var/folders/.../.tmpXXXX`, ppid 1 |
| **E** | A, test binary `SIGSTOP`'d, then its `cargo` parent `kill -9`'d | 0 / 0 | **3 / 0** fake plugins (`--language scripted\|held --dir /var/folders/...`) whose ppid was the stopped `deps/g_mesh-<hash> daemon::` |
| E' | E's stopped core `kill -9`'d | - | 0 within 3 s: the plugins exit by themselves |
| **F** | `cargo nextest run -p g-mesh --test shim_bootstrap --test lazy_activation`, nextest sent INT while a daemon was up | 0 / 0 | **0 / 2**: daemons with ppid 1 and **pgid = their own pid** |

The leaked daemons from D and F answered `SIGTERM` (D's daemon was gone within 3 s).
Every process listed above was then `kill -9`'d; the final count is 0.

**A clean run leaks nothing.** The suite as it stands meets the first AC. The
leaks need an interrupted or stalled run.

## 3. Cause

### 3.1 Fake plugins: they outlive their core only while the core is still alive

`plugins/sdk/fake/main.rs`, every exit path:

- `serve` (L208-222) calls `process::exit(0)` at stdin EOF. Both the
  control-plane personas (stub and fixture) run it, and the fixture runs it on
  a reader thread from the start (L250-257). So a `gated` plugin whose
  `handshake.allow` never appears still exits at EOF: `wait_for` runs on the
  main thread, and `process::exit` from the reader thread ends the process.
  Gates that hold `semanticPass` answers sit inside the same serving loop.
- `exitWithoutHandshakeAfterMs` (L240-243) sleeps without reading stdin.
  Bounded: its only caller, `never_handshake` (from
  `registry::tests::a_failed_spawn_memoizes_nothing_and_answers_everyone_waiting_on_it`),
  passes 200 ms.
- `fixture_bulk` / stub bulk write a few lines and return; a dead reader turns
  the write into EPIPE.
- `memoryHungry` and `stalling` change only what the plugin answers; it still
  serves stdin.
- `spam_stderr` (stub) is a side thread; `serve` still ends the process.

Core ends a plugin by closing its stdin, then waiting a grace period, then `kill`
(`PluginState::end`, core/src/daemon/plugin.rs L665-690). Dropping a
`PluginProcess` also closes stdin. When the core process dies, the kernel
closes the write end, so a plugin keeps running only while **some live process
holds its stdin's write end**. Run B1 shows this (SIGKILL'd core gives 0
survivors), and so does E' (killing the stopped core frees the plugins in under 3 s).

Who could hold that write end:

1. **The core itself, still alive but not finishing**: run E. `--dir` fixtures
   come only from lib unit tests (`daemon::test_plugin` is
   `#[cfg(test)] pub(crate)`, core/src/daemon/mod.rs L16-18), so their core is
   the lib test binary `target/debug/deps/g_mesh-<hash>`. **`pgrep -fl g-mesh`
   does not match that name (underscore)**, and its command line for a
   `daemon::` filter is `.../g_mesh-<hash> daemon::`, which reads as "a g-mesh
   daemon". The reported shape fits one such binary that was stalled (hung or
   stopped) after its runner went away, holding the plugins of every test
   thread that was stuck in it. libtest has no per-test timeout, so a `cargo test`
   run that hangs (and not nextest, whose 210 s timeout kills the test's whole
   process group) stays up indefinitely. I did not catch a natural hang: the
   suite ran green twice under load 100-400.
2. **A sibling that inherited the pipe.** GM-397's `process::spawn_serialized`
   already closes this for plugin-vs-plugin. Unserialized spawns that remain in lib
   tests (`cli::stop` `Victim`: `sh -c "trap '' TERM; sleep 30"`,
   `daemon::memory` `sleep 5`, `embedding::cache` child test binaries,
   `manifest::tests` Node plugin) can still pick up a concurrent plugin's
   pipe under libtest, but every one of them ends within about 30 s. The
   `Victim` drop kills only `sh`, so its `sleep 30` can outlive it. These are
   bounded and cannot explain a lasting leak.
3. **A leaked daemon.** It holds its own plugins: the stub persona with no `--dir`, or
   real plugins. This covers the daemon part of the report, not the `--dir` part.

### 3.2 Daemons: a test process that dies without unwinding leaves its daemon running

Integration tests stop their daemons in teardown, which is either explicit
(`common::kill_pid_file`) or a `Drop` (the `semantic_pass_trigger::Harness` drop
removes only the state dir). A test process that is SIGKILL'd, or interrupted by
nextest, runs no teardown, and its `TempDir` is never deleted. The daemon's own
exits then do not fire:

- `orphan_check` (core/src/daemon/lifecycle.rs L707-716) fires only when the
  project root or the executable is gone. The tempdir survives, and so does `target/`.
- The core idle exit defaults to 24 h (`DEFAULT_CORE_IDLE`, L48).

Two escape routes, each reproduced:

- **D, direct spawn** (`Command::new(BIN).arg("daemon")`, 53 sites across
  53 files in core/tests). The daemon shares the test's process group. A
  nextest timeout or ctrl-c signals that group and kills it. A lone `kill -9`
  of the test binary does not, and that is what a harness killing a stuck
  command, or OOM, does.
- **F, shim bootstrap** (`shim::spawn_detached_daemon`, core/src/shim.rs
  L462-485, `process::detach` = `process_group(0)`). In production the daemon
  is meant to escape its spawner's group, so **nextest's own interrupt or
  timeout cannot reach it**. Every shim-bootstrapping test (`shim_bootstrap`,
  `lazy_activation`, `mcp_e2e`, ...) leaks its daemon on ctrl-c or timeout.

### 3.3 Which tests leaked

On a clean run, none. On an interrupted run, every integration test that
has a daemon up at the moment of the interrupt leaks it (D, F). A lib test
leaks its fixture plugins only while its own process lives on (E).
The g-mesh index's spawners of fixture plugins (all lib tests):
`find_callers install_inner` lists `install`, `install_with_workspace`,
`install_with_workspace_semantic_pass_capable`,
`install_semantic_pass_capable`, `install_gated`, `install_stalling`,
`install_memory_hungry` and `install_incomplete_once`. `find_references install`
(id `48cc…`) lists 18 tests and helpers in `daemon::{bulk_index, lifecycle::tests,
registry::tests, semantic, workspace_reindex}` and `mcp::instructions::tests`.
`install_gated` is called from `registry::tests::registry_over_inner`, and
`install_stalling` from `lifecycle::tests::a_timed_out_file_change...` and
`semantic::tests::install_language`.

## 4. Fix options

**Fake plugin: no change.** Every path ends at stdin EOF or after a bounded
delay. Adding a parent-pid watchdog would only hide a live, stalled core, and that
core is the finding.

For the daemon:

| Option | What | Covers | Risk |
|--------|------|--------|------|
| **A (recommended)** | A test-only **lifeline pid**. When `G_MESH_LIFELINE_PID` is set, `orphan_check` also returns `Orphaned::LifelineGone(pid)` once `process::is_alive(pid)` is false. Tests set it to `std::process::id()` on the `Command` that starts `g-mesh` (either a daemon or a shim). The shim's spawn inherits the env, so a detached daemon carries it too. | D, F, nextest timeout, ctrl-c, SIGKILL, OOM: the daemon exits within one tick (at most 30 s, `MAX_TICK`) and tears down its plugins as an idle exit does. | Touches the 53 integration-test spawn sites (a mechanical `.env(...)` edit, or a `common::gmesh_command()` helper). Pid reuse could keep a daemon alive longer, never shorter. Same test-only env pattern as `G_MESH_CORE_IDLE_MS`, so there is no production behaviour change while the variable is unset. |
| B | Tests set `G_MESH_CORE_IDLE_MS` to a short value | Bounds the leak to the idle time | Tests that sit idle between requests would be killed. The same 53 sites need the env. Only bounds the leak, does not prevent it. |
| C | Automatic: the daemon exits when its process-group leader dies (pgid != own pid) | D, plus nextest-timeout cases, with no test edits | Changes production behaviour (a `g-mesh daemon` run from a script dies with the script). Misses F, whose pgid is its own pid. |
| D | No code change: a reaper script that kills processes under `<worktree>/target/debug/` with a ppid of 1, plus a RULES line telling agents to use nextest and never bare `cargo test` | Cleanup after the fact | Does not meet "a clean full run leaves none" in a stronger sense than today. Agents forget to run it. |

Recommendation: **A**. For the lib-test half (E), make no code change: nextest's
per-test timeout already covers it. Document in the testing notes that `cargo test`
(libtest) has no per-test timeout, and that its test binary is named
`g_mesh-<hash>`, so a leak hunt has to `pgrep -f g_mesh-` too.

## 5. Edit map (option A)

Change:

- `core/src/daemon/lifecycle.rs`
  - L52-56 (env consts): add `pub const LIFELINE_PID_ENV: &str = "G_MESH_LIFELINE_PID";`.
  - `enum Orphaned` L681-687 plus `Display` L689-700: add a `LifelineGone(u32)` variant
    and its message.
  - `orphan_check` L707-716: add a `lifeline: Option<u32>` parameter, judged after root
    and exe. `None`, or a value that does not parse, is never evidence.
  - `supervise` L750-790: read the env once before the loop, pass it to `orphan_check`.
- `core/src/daemon/lifecycle/tests.rs` L361-423: existing `orphan_check` calls
  gain the extra argument (`None`).
- `core/tests/common/mod.rs`: add a helper (for example `gmesh_command()`, or
  `with_lifeline(&mut Command)`) that sets `LIFELINE_PID_ENV`.
- The 53 `env!("CARGO_BIN_EXE_g-mesh")` spawn sites in `core/tests/*.rs`
  (`grep -rn 'CARGO_BIN_EXE_g-mesh"' core/tests`). Each one gets the env
  through the helper. Apply by role: sites that start a daemon or a shim need it;
  one-shot CLI calls (`status`, `stop`, `init`, ...) do not, though they are
  harmless with it.

Read for context: `process::is_alive` (core/src/process.rs ~L44),
`process::detach` (L173), `shim::spawn_detached_daemon` (shim.rs L462-485),
`IdleTimeouts::from_config` / `tick` (lifecycle.rs L79-104),
`daemon::run`'s call into `supervise` (daemon/mod.rs L423).

## 6. Control

- Revert the `LifelineGone` branch in `orphan_check` (code only).
- Run D: `cargo test -p g-mesh --test semantic_pass_trigger`, and `kill -9`
  the `deps/semantic_pass_trigger-*` binary once a `g-mesh daemon` is up.
- Run F: `cargo nextest run -p g-mesh --test shim_bootstrap --test lazy_activation`,
  and send SIGINT to `cargo-nextest` once a daemon is up.
- Count after **65 s**, which is 30 s tick plus margin. Expected count: **with the fix 0 daemons,
  with the branch reverted 2 per run.**
- Scope every count to `<worktree>/target` and kill all leftovers afterwards.
  `scratchpad/gm522/interrupt.sh` and `count.sh` are the scripts used here.

## 7. Behaviours for the tests slice

1. `orphan_check` with a lifeline pid that is alive, while root and exe are present, returns `None`.
2. `orphan_check` with a lifeline pid that has exited (spawn `sleep 0`, then reap it)
   returns `Some(Orphaned::LifelineGone(pid))`.
3. `orphan_check` with `lifeline = None` behaves exactly as today. The existing tests keep their expectations.
4. A gone project root is still reported first, ahead of a gone lifeline.
5. An env value that does not parse is treated as "no lifeline" and never ends the daemon.
6. Integration: a daemon started with `G_MESH_LIFELINE_PID` set to a short-lived
   helper's pid exits by itself (its pid file and socket are released) within
   the tick, once the helper is reaped. Use `G_MESH_CORE_IDLE_MS` to shorten
   the tick, since `tick` = min(timeouts)/4, clamped to 50 ms..30 s. This test
   spawns processes and runs on timers, so it runs 5 times.

## 8. Must confirm

- **The original 72.** Were they children (`ppid`) of a live
  `deps/g_mesh-<hash>` lib test binary, and was the "g-mesh daemon" actually that
  binary (`... g_mesh-<hash> daemon::`)? This is the only mechanism that
  reproduced `--dir` survivors (E). If their ppid was 1 and no core was alive,
  this note's model is wrong and needs another pass.
- Whether the earlier integration work ran `cargo test` (libtest, no per-test
  timeout), or killed runs with a lone SIGKILL. Both are shapes that leak.
- Option A's acceptance of 53 test-site edits, versus a narrower subset (only
  the daemon or shim starters).
- On Windows, `process::is_alive` already handles a pid, so option A ports. Confirm in CI.
