# GM-605: orphaned daemon's state dir survives its purge under load

Status: design note (GM-605/S1), for owner review.

## Symptom

`a_daemon_whose_project_root_is_deleted_stops_itself` (`core/tests/orphaned_daemon.rs`)
fails about 1 run in 50 under heavy load with "... is still there". GM-583 fixed two
lock races in the purge (bootstrap-lock try, singleton-lock probe) and added the
on-failure diagnostic; the residual failure had no evidence until now.

## Evidence

Repro: the whole `--test orphaned_daemon` binary (all 6 tests, as in GM-583/S11, so
the other tests' shim sweeps run against the shared test home too), looped under 16
`yes` burners, the load method of GM-583/S11. Script and logs:
`scratchpad/gm605/run.sh`, `summary.txt`, `fail-4.log` (GM-605/S1 session).

- Dry run, no load: 6/6 passed (real 18.25 s, user 12.04 s, sys 2.58 s).
- Under load: reps 1-3 passed; rep 4 failed at load averages 324 / 135 / 61
  (real 27.02 s, user 12.79 s, sys 3.25 s).

The diagnostic from the failed rep:

```
... /tmp/g-mesh-test-home/projects/4fce19e965d34cf2 is still there
--- last 60 lines of daemon.log ---
(absent)
--- /tmp/g-mesh-test-home/projects/4fce19e965d34cf2 ---
index.phase  5 bytes
```

Read together:

- `daemon.log`, `project.root` and `project.dev` are gone, so the purge was not
  `Kept`. It got past both judgements and the bootstrap lock, and ran
  `delete_root_record_last` to the end: it deletes every other entry, then the root
  record, then `remove_dir_all(state_dir)`.
- The only entry left is `index.phase`, 5 bytes: the word `ready`
  (`Phase::word`, `core/src/daemon/indexing_status.rs:55-63`). Only
  `IndexingStatus::set_phase(Phase::Ready)` writes that, and the only caller that
  runs at this point is the activation thread's last line
  (`core/src/daemon/activation.rs:241`, `activate`).
- So the final `remove_dir_all` failed with `ENOTEMPTY` because `index.phase` was
  renamed into the directory after the delete had listed it. The purge returned
  `Err`, and the "could not delete" line went to the already-unlinked `daemon.log`,
  which is why the log is absent rather than showing a reason.

This confirms the hypothesis left open by GM-583/S11 (a still-running daemon thread
recreates a file during the delete) and names the writer.

## Cause

The orphan teardown stops the plugins and the main thread, but not the activation
thread, which keeps writing the phase and progress files into the state directory.

Timeline (`core/src/daemon/mod.rs:294-575` `daemon::run`,
`core/src/daemon/lifecycle.rs:869-949` `supervise`):

1. The test waits for `bulkIndexedAt` and the plugin's pid file, then deletes the
   root. At that moment the activation thread is usually inside the walk's
   semantic pass, which holds the TypeScript supervisor's lock for the whole pass
   (`PluginSupervisor::semantic_pass`, `lifecycle.rs:409-422`).
2. `supervise` sees the orphan and calls `registry.sleep_all_now`, which blocks on
   that lock until the pass returns. From then on both threads run at the same
   time.
3. Main thread: `put_to_sleep` (plugin shutdown), `end_process_group` (a no-op
   under the test harness), `release_state_files`, return, `drop(singleton)`,
   `purge_state_dir`, then `purge_orphaned_state_dir`, then `delete_root_record_last`.
4. Activation thread, at the same time (`activation.rs:147-242` `activate`): spawns
   the watch consumer, `resume_pending`, `resume_owed_reextracts`,
   `set_phase(Embedding)`, the backfill (fast with no model), then
   `set_phase(Ready)`. Each `set_phase` writes `index.phase` with
   `write_state_file_atomic` (temp file, then rename; `mod.rs:266-276`), then
   `index.progress` (`publish_progress(true)`).
5. When a write lands after `release_state_files` and before the purge, the purge
   deletes it. When it lands after the purge has finished, the write fails
   (`fs::write` does not create the parent; there is no `create_dir_all` on any
   runtime path into the state dir). Only a rename that lands inside the
   `remove_dir_all` window survives. That window is a few milliseconds, which is
   why it takes load around 150-300 to hit.

Production impact, not just a flaky test: the root record is already deleted when
the final `remove_dir_all` fails. The leftover directory (only `index.phase`, maybe
`index.progress`) reads as `RootState::Legacy` (no `project.root`). Neither the
shim's sweep nor `clean orphaned` ever deletes that, so every hit leaks a small
directory for good. That defeats the guarantee `delete_root_record_last` documents
(`core/src/cli/clean.rs:474-480`).

Other writers that could do the same, by the same mechanism (none observed): a
plugin woken after `sleep_all_now` by a live session's `ensure_fresh`
(`lifecycle.rs:495-517`, writes the plugin pid file) or by `with_awake_exclusive_access`
(`lifecycle.rs:468-478`), and the memory-limit suspended marker
(`lifecycle.rs:672-690`). The test opens no session after indexing, so these are
not this failure.

## Fix options

### A. Detach the phase and progress files before the state files are released (recommended)

Add `IndexingStatus::detach_state_files()`, which sets `phase_file` to `None` and
`progress_file.path` to `None` under their mutexes. Call it on the orphan and idle
exits of `supervise`, right before `release_state_files`. `supervise` takes an
`Option<&IndexingStatus>`: `daemon::run` passes its clone and `front::run` passes
`None`.

- Benefit: deterministic. Both writers hold their mutex across the whole write
  (`publish_phase_file` at `indexing_status.rs:337-342`, `publish_progress` at
  `:459-470`), so once detach returns no write is in flight and none can start. It
  also closes the same race on the idle exit, where a phase file written after
  `release_state_files` would survive into the next start ("never transiently
  stale", D13). A small change, unit-testable without a daemon.
- Risk: it closes only the writer we saw. A future thread that writes into the
  state dir after teardown reopens the bug. Also a signature change to `supervise`:
  two callers, `daemon::run` and `daemon::front::run` (g-mesh `find_callers
  supervise`).

### B. Purge retries the delete when the directory refills (UpTo only)

In `purge_orphaned_state_dir` under `LockWait::UpTo`, when the final
`remove_dir_all` fails with `ENOTEMPTY` (or `delete_root_record_last` fails at
all), re-list and delete again, a few attempts `LOCK_POLL_INTERVAL` apart, within
the existing deadline.

- Benefit: covers every writer, known or not, and removes the `Legacy` leak.
- Risk: masks the writer instead of stopping it. If something keeps writing, the
  retries race it until the deadline, adding up to 10 s to the exit. A woken
  plugin's pid file would be deleted while that plugin runs (it dies with its
  pipes when the daemon exits). `LockWait::Try` (the sweep) must keep the
  single-shot delete: a hand-started daemon takes no bootstrap lock, so a
  retrying sweeper could keep deleting a fresh daemon's files.

### C. Quiesce every daemon thread before the purge

A shutdown flag that the activation thread, the accept loop and the watch consumer
check, joined before `purge_state_dir`.

- Benefit: the "nothing of this process touches the state dir any more" invariant,
  for real.
- Risk: the largest change. The backfill and semantic passes can run for minutes
  between checks, so a join either hangs the exit or needs timeouts and
  cancellation plumbing through code paths this task does not own.

### Recommendation

A. It is the cause, it is deterministic and it is small. B is a reasonable second
layer against the `Legacy` leak. It is an owner decision because it changes purge
semantics and adds exit latency in the worst case.

## Edit map (option A)

Change:

- `core/src/daemon/indexing_status.rs`: new `pub(crate) fn detach_state_files(&self)`
  next to `attach_phase_file` / `attach_progress_file_every` (`:405-426`). It locks
  `phase_file` and sets `None`, then locks `progress_file` and sets `path = None`.
  Update the `phase_file` field doc (`:167-171`).
- `core/src/daemon/lifecycle.rs:869-949` `supervise`: new parameter
  `indexing: Option<&crate::daemon::indexing_status::IndexingStatus>` (or a
  `&dyn Fn()` teardown hook, if the module boundary argues for it). Call
  `detach_state_files()` before both `release_state_files(state_dir)` calls (orphan
  branch `:904`, idle branch `:919`). Update the doc comment.
- `core/src/daemon/mod.rs:294-575` `daemon::run`: keep an `indexing.clone()`
  before it moves into `ActivationCtx` (`activation::spawn` at `:543`) and pass `Some(&it)` to
  `supervise` (`:565-566`).
- `core/src/daemon/front.rs:101-102` `front::run`: pass `None`.

Read only:

- `core/src/daemon/activation.rs:147-242` `activate` (the writer, `set_phase` at
  `:222` and `:241`).
- `core/src/daemon/indexing_status.rs:316-342` `set_phase`, `publish_phase_file`;
  `:455-470` `publish_progress` (the guard is held across the write: that is the
  barrier A relies on).
- `core/src/daemon/mod.rs:261-276` `write_pid_file`, `write_state_file_atomic`.
- `core/src/cli/clean.rs:398-479` `purge_orphaned_state_dir`, `:481-507`
  `delete_root_record_last` (unchanged under A; the site for B).
- `core/src/daemon/lifecycle.rs:1029-1043` `release_state_files`.

## Tests and control

- Unit test (`indexing_status.rs` tests): attach a phase file and a progress file
  (interval zero) in a temp dir, call `detach_state_files`, delete both files, call
  `set_phase(Phase::Ready)` and bump a counter. Neither file comes back.
  **Control:** make `detach_state_files` a no-op, and the test fails with
  `index.phase` present. Deterministic: no threads.
- Barrier test (optional, deterministic): hold `phase_file`'s lock from a test
  thread (a test-only accessor, or hold it through a `set_phase` whose write is
  slowed by pointing the path at a FIFO), call `detach_state_files` from another
  thread, and assert it returns only after the holder releases it. Probably not
  worth the test-only surface; the mutex makes this true by construction.
- End to end: not deterministically reproducible without a test-only delay in
  `activate`, because the window is the rename landing inside `remove_dir_all`.
  Verify by rerunning this note's repro (whole `orphaned_daemon` binary, 16 `yes`
  burners) 50x green. It failed at rep 4 here, and about 1/50 in GM-583/S11. With
  A reverted, the repro is the probabilistic control (expected to fail within ~50
  reps at load at least 150).
- Under B (if chosen): unit test in `cli/clean.rs`, with a thread that recreates
  a file in the state dir until the purge returns. `UpTo` must return `Deleted`;
  control: drop the retry, and it returns `Err` (probabilistic, so prefer a
  hook-driven deterministic variant).
