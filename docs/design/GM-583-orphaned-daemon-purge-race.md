# GM-583: orphaned daemon purge race, test budgets above nextest's timeout, half-read log lines

Status: diagnosed (Phase 2 done). Cause confirmed by a deterministic
reproduction of the exact symptom; the natural rate is too low to reproduce
unaided (0 of 30 at load ~750).

## Problem

GM-581 verify F2: `core/tests/orphaned_daemon.rs`
`a_daemon_whose_project_root_is_deleted_stops_itself` failed 1 of 5 runs on
macOS at load 324 during a full suite: the daemon exited cleanly and released
its pid/plugin/build-stamp/socket files (`assert_released_everything`
passed), then the final assertion found the state directory "still there".

Related findings:

- F1: test budgets above nextest's limit. `.config/nextest.toml` kills a test
  at 210 s (`period = "30s", terminate-after = 7`), but
  `common::startup_timeout()` is 240 s (`DEFAULT_STARTUP_TIMEOUT_SECS`) and
  `orphan_budget()` reuses it. A wait that is about to fail is killed by nextest
  first, so the helper's own panic (e.g. `sweep_with_a_shim` with the shim's
  stderr) is never shown. GM-565 verify hit this: `a_shim_start_sweeps...` was
  killed at 210 s while it was waiting on `startup_timeout`.
- `core/tests/daemon_core.rs` `reconciled_line` reads the log with
  `text.lines()`, which also yields an unterminated last line, so it can match
  a half-written "reconciled ..." line and fail the `ends_with` assertion.
  `a_restart_against_an_already_indexed_project_does_not_walk_it_again` also
  cannot fail fast on the regression it guards against: a restart that
  re-walks never prints the reconcile line, so the test fails only when the
  240 s budget runs out, and nextest has killed it before then.

## What happens today

Main thread at the end of `daemon::run` (core/src/daemon/mod.rs):

1. `lifecycle::supervise`: `orphan_check` sees the root gone (`ProjectRootGone`)
   -> `registry.sleep_all_now` -> `end_process_group` -> `release_state_files`
   -> returns `Exit::Orphaned`.
2. `drop(singleton)`.
3. `lifecycle::purge_state_dir` -> `cli::clean::purge_orphaned_state_dir`:
   judge -> **`try_lock` on `bootstrap.lock`** -> re-judge ->
   `delete_root_record_last`. On `WouldBlock` it returns
   `Kept("a shim is bootstrapping it")`, and that is only logged (to stderr,
   which this test sends to `/dev/null`).
4. The process exits.

From step 2 on, the directory is exactly what every shim's startup orphan
sweep (`cli::clean::sweep_orphaned`, same function, same lock) deletes: the
root is gone, its parent exists, no pid files, and the singleton is free. All test
processes share one home (`/tmp/g-mesh-test-home`, `.cargo/config.toml`
`[env]`), and in a full suite many tests start shims.

## Cause (confirmed)

**A shim's startup sweep, run by another test over the shared home, takes the
bootstrap lock of this directory between steps 2 and 3. The daemon's own purge
then hits `WouldBlock` and gives up (`Kept`) instead of waiting. The daemon
exits at once, and the test asserts while the shim is still deleting** (or
after a shim killed mid-sweep left the directory for the next sweep). The race
is in production ordering, not in the test's wait: the test asserts the
documented promise (on Unix the purge runs before the process exits), and the
daemon breaks that promise whenever someone else holds the lock.

H1 (a daemon thread writing a state file during the purge, ENOTEMPTY) is ruled
out for this failure: across 30 traced runs, no state file was written after
`release_state_files`. The last `index.phase`/`index.progress` writes land
5-10 ms *before* the release, in every run. They come from the watcher's
in-flight change, which `sleep_all_now` waits out under the plugin lock.

### Evidence

Scratch tooling, not committed: `repro.sh` and three env-gated trace patches
in the orchestrator scratch dir `gm583/`. Each process appends timestamped
lines to `$GM583_TRACE`: the purge's judge, lock and delete steps, every
`write_state_file_atomic`, and on a leftover, the entries at reap time and
whether the directory goes within 10 s.

| run | load | result |
|---|---|---|
| `plain`: 15 runs, 64 CPU burners, private home, no other shims | 1-min avg 50 -> 804 | 15/15 pass, 15 `Deleted`; singleton-drop -> lock 1-5 ms; 0 state writes after release |
| `sweeper`: 15 runs, 64 burners, plus a shim-start loop (sweep marker removed each time) over the same home | ~745-804 | 15/15 pass, 15 `Deleted`; 104 shim sweeps judged the directory (17 while the daemon still ran), none hit the 1-5 ms window |
| window widened 3 s (`GM583_PURGE_DELAY_MS`), sweeper, no burners | ~180 | 4/4: a shim claims the directory in the window every time and deletes it before the daemon wakes; the daemon's purge returns `Kept`, the test passes |
| window widened 1.5 s, and the shim holds the lock 3 s during its delete (`GM583_SWEEP_HOLD_MS`) | ~40-70 | **3/3 fail with the reported symptom**: daemon trace `bootstrap lock busy` -> `Kept("a shim is bootstrapping it")`, the daemon exits, the test reaps it, and the directory is still there with `index.db*`, `project.root`, `daemon.lock`, `daemon.serving`, `bootstrap.lock` |

Timing (`/usr/bin/time -p`): load runs real 1017 s / user 4771 s / sys 82 s
(the CPU was saturated, not waiting). Per-run test time was 25-40 s under load.
In the last row the directory did not go even 10 s later, because each
looping shim was killed (stdin EOF after 2 s) while still inside its 3 s hold.
That is the other real-world shape: a shim killed mid-sweep leaves the
directory for a later sweep, and the daemon had already given up on it.

## Fix proposal

**B (recommended): the daemon's purge waits for the bootstrap lock, bounded,
instead of only trying it.** Give `purge_orphaned_state_dir` a lock policy:
`LockWait::Try` for the shim sweep (unchanged, so it never stalls a shim
start) and `LockWait::UpTo(Duration)` for the daemon's own purge
(`lifecycle::purge_state_dir`, e.g. 10 s), polling `try_lock` every 10-20 ms.
After it gets the lock it re-judges, as today:
- a sweeper finished first: the directory is gone. The lock is taken on an
  fd opened before the delete, so the re-judge today would say
  `Kept("... not recorded")`; this is the outcome the widened-window
  rows logged. So the purge must check for a missing `state_dir`
  (`NotFound`) after it gets the lock and return `Deleted` before
  re-judging;
- a sweeper died mid-delete: the lock is released and the directory is still
  orphaned, so the daemon deletes the rest;
- a shim is really bootstrapping (only possible if the root came back): the
  re-judge sees the root and keeps the directory.
Benefit: "on Unix the purge has finished before the daemon exits" becomes true,
and the test stays strict. It also covers a shim killed mid-sweep. Risk: an
orphaned daemon's exit can be delayed by up to the bound while a lock is held
(nothing waits on that exit except tests and `stop`). And
`purge_orphaned_state_dir` gains a parameter (two callers; `find_callers`
lists `daemon::lifecycle::purge_state_dir` and `cli::clean::sweep_orphaned`).

A (alternative, test-only): replace the final `assert!` with
`common::wait_for(..., orphan_budget(), || !state_dir.exists())`. Benefit: no
production change. Risk: it would not have passed the killed-shim case
above, where nothing deletes the directory until a later shim starts. And it
stops pinning that the daemon deletes its own state.

F1 budgets:
- `core/tests/common/mod.rs`: `DEFAULT_STARTUP_TIMEOUT_SECS` 240 -> 180. Fix
  the doc: an 80 s dyld stall still fits twice. And state the invariant that
  it stays below nextest's `period * terminate-after` (210 s).
  `orphan_budget()` follows unchanged.
- `.config/nextest.toml`: next to `slow-timeout`, a comment naming the
  constants that must stay below it.
- A guard test in `core/tests` that parses `.config/nextest.toml`'s default
  `slow-timeout` and asserts the default `startup_timeout()` and
  `DEFAULT_INDEXED_TIMEOUT_SECS` are each below it.
- Alternative (not recommended): `terminate-after = 9` (270 s). It keeps the
  240 s budget, but every real hang then costs a minute more.
- Residual: a test with several sequential waits can still pass 210 s in
  total. That is out of scope here; each single wait now fails first with its
  own message.

`daemon_core.rs`:
- `reconciled_line`: match only complete lines, i.e. text up to the last `\n`:
  `let whole = &text[..text.rfind('\n').map_or(0, |i| i + 1)];`
  then `whole.lines()`.
- Fail fast on a re-walk: in the same predicate, panic as soon as the
  restarted daemon's phase file (`daemon::read_phase`) reads the walking
  phase, with the log's contents. Only a walk publishes that phase, and a
  reconcile never does.

## Edit map

| file | change |
|---|---|
| core/src/cli/clean.rs | `purge_orphaned_state_dir(state_dir, LockWait)`; bounded wait loop on `WouldBlock` for `UpTo`; `sweep_orphaned` passes `Try` |
| core/src/daemon/lifecycle.rs | `purge_state_dir` passes `LockWait::UpTo(PURGE_LOCK_WAIT)`; doc: the purge finishes, or is kept for a reason, before exit |
| core/tests/orphaned_daemon.rs | new test (below); the existing test unchanged |
| core/tests/common/mod.rs | `DEFAULT_STARTUP_TIMEOUT_SECS` 180 + doc invariant |
| .config/nextest.toml | comment tying `slow-timeout` to the budgets |
| core/tests/<guard>.rs or common | budget guard test |
| core/tests/daemon_core.rs | `reconciled_line` reads whole lines only; re-walk fails fast |

## Tests and controls

1. New, deterministic: `a_purge_that_finds_the_bootstrap_lock_held_waits_for_it`.
   The test process takes `bootstrap.lock` in the state dir (flock via
   `File::try_lock`), deletes the root, and waits until `daemon.pid` is gone
   (the orphan release). Then it holds the lock about 1 s longer, asserts the
   daemon is still running (waiting), releases the lock, reaps the daemon, and
   asserts the directory is gone.
   Control: revert B (back to `try_lock` only). The daemon exits while the
   lock is held and the test fails (deterministic, no load needed).
2. Unit test in `cli::clean`: `LockWait::Try` on a held lock returns `Kept`
   at once, and `UpTo` returns `Deleted` once the holder deletes the
   directory and releases the lock. Control: map `UpTo` to `Try`; the second
   case fails.
3. Flaky control, as data only: on old code the natural rate under load was
   0 of 30 at load ~750 (private home). The failure needs another process's
   sweep in a 1-5 ms window. With the window widened by the scratch patch
   (`trace-hold.patch`, 1.5 s window, 3 s hold), old code fails 3 of 3.
   The verify slice can rerun that patched arm against the fix and expect 0
   of 3. The patch is never committed.
4. Acceptance repro: `repro.sh 20 64 plain,sweeper` on the fixed code, 20/20
   per arm under load.
5. `reconciled_line`: a unit test over a log whose last line is a cut-off
   "reconciled ... 2 created, 2 rei" expects no match. Control: revert to
   `text.lines()`; the test fails.
6. Re-walk fast fail: delete the bulk-index completion marker from `index.db`
   before the restart. The test must fail within seconds, naming the walk.
   Control: revert the fast-fail and the same run waits out the budget.
7. Budget guard: control = `DEFAULT_STARTUP_TIMEOUT_SECS` back to 240; the
   guard fails.

## Decisions for the owner

- B (production bounded lock wait, test stays strict) vs A (test waits).
  Recommended: B.
- `PURGE_LOCK_WAIT` bound: 10 s proposed.
- Budget: lower the startup timeout to 180 s (recommended) vs raise nextest's
  `terminate-after` to 9.
