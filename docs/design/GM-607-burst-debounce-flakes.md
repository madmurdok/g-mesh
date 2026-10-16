# GM-607: burst-debounce tests flaky under load

Status: diagnosis (S1). No production or test change in this slice.

## Failures

| Run | Test | Panic |
|---|---|---|
| GM-606/S10 full lib suite, load ~9 | `a_burst_of_rapid_saves_to_the_same_file_costs_one_plugin_round_trip_not_one_per_save` | `tests.rs:374` "a burst still in progress must not have reached the plugin yet" |
| GM-606/S13 full lib suite, load ~120 | `two_different_files_in_the_same_burst_each_get_their_own_round_trip` | `tests.rs:428` left `["a", "b", "b"]`, right `["a", "b"]` |

Both diffs were outside `core`; each test passed 5/5 alone.

## Mechanism under test

- `Debouncer` (`core/src/watcher/debounce.rs:17-63`) timestamps a path when
  `record` is called (`Instant::now()`), not when the filesystem changed, and
  `drain_ready` fires a path once it has been quiet for `window`.
- `watch_and_route_once` (`core/src/daemon/mod.rs:679-729`) takes at most ONE
  path per call from `ProjectWatcher::next_change(DEBOUNCE_WINDOW)`, records it,
  then drains. `DEBOUNCE_WINDOW` (`mod.rs:80`) is 300 ms and is used for both the
  poll timeout and the debounce window.
- `ProjectWatcher` (`core/src/watcher/mod.rs:74-`, `next_change` at 232) uses
  notify 6.1.1 FSEvents with latency 0.0 and `NoDefer`: every FSEvents callback is
  forwarded as soon as fseventsd delivers it, one channel message per path per
  notify event. One `fs::write` (open O_CREAT|O_TRUNC, write, close) can surface
  as several events, possibly in separate callbacks.

So a path's debounce clock is "when the loop saw the event", and the loop sees
an event only when fseventsd has delivered it.

## Timing each test assumes

1. `a_burst...` (`tests.rs:353-388`): five writes, each followed by `sleep(20 ms)`
   and one `watch_and_route_once`. Assumes every write's event is *observed*
   less than 300 ms after the previous observation of the same path. If one
   delivery lags 300 ms or more, `next_change` times out (returns `None` at exactly
   300 ms) and `drain_ready` finds `a` quiet for the full window: it fires
   mid-burst, and the line 374 assertion fails. Same outcome if the test thread
   is descheduled for 300 ms or more between iterations.
2. `two_different_files...` (`tests.rs:410-432`): assumes all events of the one
   `fs::write(b)` are observed within 300 ms of each other. If fseventsd delivers
   b's create and b's data-modify (or a re-reported flag set) more than 300 ms
   apart, measured from when the loop sees them, b settles and is routed, and the
   late event starts a second window: `["a", "b", "b"]`. The exact-list
   assertion fails although each file did get its own round trip.
3. `pump_until_round_trips` (`tests.rs:321-341`) waits up to 60 s for `expected`
   trips, then settles for `3 * DEBOUNCE_WINDOW`. Under an event storm a path's
   event can fail to arrive at all (see the discovery below).

Neither failure is a counting bug in the debouncer. Both are delivery lag of
300 ms or more (fseventsd, or the scheduler) against a 300 ms window measured on the
consumer side.

## Reproduction (macOS, 8 cores)

Script: `scratchpad/gm607/run.sh` and `run2.sh`. Both tests, `cargo test -p g-mesh --lib -- --exact T1 T2`.

- **Arm 1, CPU load:** 48 `yes` burners plus other agents, load 150 → 830.
  **50/50 pass** (real 10-20 s at user ~0.33 s: each rep waits on the
  debounce/settle windows, not on CPU). CPU load alone does not reproduce it.
- **Arm 2, filesystem-event storm:** 12 shell writers creating and deleting 200
  files each in a loop under the scratchpad (fseventsd pressure, no burners),
  load 640 → 500 (left over from arm 1 and other agents). **Failed at rep 2**:
  `two_different_files...` ran 64 s (real 70.0 s, user 0.43 s: waiting) and
  panicked `left: ["b.python-src"]`: a's change **never reached the watcher in
  60 s**, while b's did.

The original two failures ran inside the full lib suite (about 1930 tests in
parallel, many creating tempdirs, watchers and files), which matches arm 2's
fseventsd pressure better than arm 1's CPU load. Confidence: **medium-high** for
"delivery lag of 300 ms or more on the consumer-side clock" as the cause of both
original failures. That reading follows the code; the original panics were not
re-observed in shape (arm 2 produced a third shape). **High** that CPU load alone
is not the trigger (50/50).

## Discovery: dropped FSEvents are ignored in production (likely)

Under pressure FSEvents drops events and sends `MustScanSubDirs` (+
`UserDropped`/`KernelDropped`) for a directory. notify 6.1.1 maps that to
`EventKind::Other` with `Flag::Rescan` (`notify-6.1.1/src/fsevent.rs:114-122`),
carrying the directory path. `ProjectWatcher`'s callback forwards only
`event.paths` and ignores the flag. When the directory is the project root,
`watch_and_route_once` drops it as "the project root itself" (`mod.rs:705`).
Result: an edit lost to an fseventsd drop is never reindexed until something
else touches the file. Arm 2's lost `a.python-src` fits this. The rescan event
itself was not captured, because a probe needs a code change and this slice
makes none. This is a **production correctness gap**, separate from the test
timing. It should be its own task (see the decisions below), not part of GM-607's fix.

## Fix options (GM-607 scope: the two tests)

**A. Test window decoupled from delivery lag (recommended for test 1).**
Build the test's `Debouncer` with a test-only window (`BURST_TEST_WINDOW`, e.g.
2 s) instead of `DEBOUNCE_WINDOW`. `watch_and_route_once` keeps polling every
300 ms, so a 300 ms poll timeout no longer equals "settled". `pump_until_round_trips` takes the
window and settles for `2 * window`.
- Benefit: tolerates delivery or scheduler stalls up to 2 s, keeps the real watcher
  and the real loop, and keeps "exactly 1".
- Risk: about 4-6 s longer per test. A stall of more than 2 s still flakes, which
  shrinks the window of exposure but does not close it.

**B. Assert what the test claims (recommended for test 2).** Compare the
*deduplicated* sorted list to `["a", "b"]`, and let the pump wait for 2 *distinct*
paths, not for 2 entries.
- Benefit: `b` routed twice (a legitimate outcome of split delivery) no longer
  fails. The test's claim ("different files are not merged") is unchanged.
- Risk: a regression that routes every path twice is not caught here. Test 1
  covers same-path duplication.
- Combine with A (a larger window) so b's split delivery usually coalesces anyway.

**C. Retry on a non-burst (precondition check).** Time each burst iteration. If
any gap reaches `window`, the burst was not a burst: rerun the scenario in a
fresh tempdir, up to 3 times.
- Benefit: exact asserts and the 300 ms window stay.
- Risk: more test code, and it hides genuine slowness behind retries.

**D. Production: timestamp at event time** (carry an `Instant` from the notify
callback through the channel). It removes the consumer-side lag from test 1's
mid-burst fire, but not fseventsd's own lag, which is the dominant term.
- Benefit: a small production gain.
- Risk: it changes `next_change`'s signature and all its callers, and does not
  fix the observed failures. **Not recommended.**

**Not fixable in tests:** a change lost to an FSEvents drop (arm 2). No window
helps. The 20x-under-load acceptance should be run under CPU load *and* with
filesystem churn kept modest. Alternatively, arm 2's lost-event failure is
accepted as the separate production task.

**Recommendation:** A + B, test-only. The rescan gap goes to a new task.

## Edit map

- `core/src/daemon/tests.rs:304-341`: `ROUND_TRIP_DEADLINE` stays.
  `pump_until_round_trips` gains `window: Duration` (settle `2 * window`) and an
  `expected` counted on distinct paths (or a separate `distinct: bool`). Callers:
  lines 379, 402, 426. The isolated-save test (391-406) passes `DEBOUNCE_WINDOW`
  and stays unchanged otherwise.
- `core/src/daemon/tests.rs:353-388` (test 1): `Debouncer::new(BURST_TEST_WINDOW)`.
  The loop and the asserts stay the same, and the comment at line 371 names the
  test window.
- `core/src/daemon/tests.rs:410-432` (test 2): `Debouncer::new(BURST_TEST_WINDOW)`,
  pump for 2 distinct paths, `requested.sort(); requested.dedup();`, same
  assert message.
- New `const BURST_TEST_WINDOW: Duration` next to `ROUND_TRIP_DEADLINE`, with a doc
  comment: why it is larger than `DEBOUNCE_WINDOW` (consumer-side clock, FSEvents
  delivery lag, GM-607).
- No change in `core/src/daemon/mod.rs` or `core/src/watcher/*`.

## Controls (must still fail after the fix)

1. **Per-save round trip:** in `Debouncer::drain_ready_at`
   (`debounce.rs:52-63`), drop the window filter (return every recorded path).
   Test 1 must fail at the line 374 assert, because the first observed write fires
   during the burst loop.
2. **Merged pair:** in `Debouncer::record_at` (`debounce.rs:34-36`), key every
   path under one entry, e.g. `self.last_seen.clear()` before the insert, so only
   the last-recorded path survives. Test 2 must fail: the deduplicated list is
   `["b"]` (a is overwritten by b before it settles, because b follows a by
   about 20 ms, which is less than `BURST_TEST_WINDOW`).

Note for control 2: with a 2 s window, a must still be pending when b is
recorded. It is, because b is written about 20 ms after a's iteration.

## Acceptance run

20x of both tests under CPU load (burners, load ≥150), plus 20x with
moderate filesystem churn (`run2.sh` with fewer writers). Record any lost-event
failure separately as the rescan task's evidence, not as a GM-607 regression.
