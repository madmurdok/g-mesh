# GM-720: watcher:: lib tests fail under heavy machine load (diagnosis, S1)

Branch `fix/GM-720-watcher-tests-under-load` (off release-4.32.0), worktree
`g-mesh-wt-gm720`. Read-only diagnosis; throwaway patches were applied by the
harness scripts and reverted (`git checkout -- core/src/watcher/mod.rs`,
`git status --short` empty afterwards).

## 1. Summary

- Only the **live FSEvents tests in `core/src/watcher/mod.rs` (`mod tests`)**
  fail. None of `watcher::apply::*`, `watcher::staleness::*`,
  `watcher::batch::*`, `watcher::rescan::*`, `watcher::debounce::*`,
  `watcher::burst::*` failed in any run (those use in-process stub plugins
  and injected clocks).
- The trigger is **not CPU load** but a **flood of filesystem events on the
  machine** (other agents' `cargo build`s writing thousands of files).
  fseventsd is one system-wide service; while it is backlogged, a freshly
  created FSEvents stream receives its events late (moderate backlog) or not
  at all within 120 s (heavy backlog).
- Two failure modes, one root cause (tests depend on system-wide FSEvents
  latency):
  1. **Late startup noise** (what GM-716/S7 hit, load 250-440): events for
     the test's own setup writes (root dir, `.gitignore`, `src`, setup
     symlinks) arrive after `drain_startup_noise`'s 300 ms quiet window and
     are read as the change under test, or fall into a 500 ms "no event"
     window.
  2. **No delivery** (heavy flood): `next_change` returns nothing for 120 s:
     no path, no `Rescan`. No timeout value fixes this.

## 2. Reproduction

Machine: 8 CPUs, already loaded by other work (load 25-200 at start).
`target/debug/deps`: 1221 files (fresh worktree target, no exec stall).
Cold build of the lib test binary: real 342.31, user 351.07, sys 66.24.

Harness scripts (scratchpad `repro/run.sh`, `run2.sh`, `run3.sh`, `run4.sh`)
ran `watcher::` lib tests repeatedly with load generators started and killed
by PID.

| Phase | Runner | Load generator | Load avg (1 min) | Runs | Failing runs | real / user / sys (typical) |
|---|---|---|---|---|---|---|
| idle | nextest | none | 25-35 | 5 | 0 | 4.6 / 4.8 / 3.4 |
| cpu | nextest | `yes` x96 | 350-590 | 5 | 0 | 24-36 / 5.2-6.2 / 3.6-4.2 |
| cpu + light churn | nextest | `yes` x96 + 4 bash file loops | 610-700 | 5 | 0 | 28-36 / 5.2-6.2 / 3.7-4.3 |
| A idle | cargo test | none | 150-196 | 5 | 0 | 3.8-5.1 / 2.3-2.7 / 3.5-4.0 |
| B cpu | cargo test | `yes` x96 | 320-440 | 5 | 0 | 12-15 / 1.4-1.9 / 1.3-1.9 |
| **D churn** | cargo test | 16 python create/unlink loops | 190-410 | 5 | **5 (7-12 tests each)** | 30-34 / 2.0-2.7 / 2.1-2.6 |
| **E churn + cpu** | cargo test | churn x16 + `yes` x64 | 510-750 | 5 | **5 (8-11 tests each)** | 37-46 / 1.7-1.9 / 1.6-1.7 |
| F churn + cpu, barrier patch | cargo test | churn x16 + `yes` x64 | 390-650 | 10 | 4 (runs 7-10, 6-11 tests) | 19-56 / 1.6-1.8 / 1.5-1.7 |

`real` far above `user + sys` everywhere under churn means the test process is
**waiting** (for FSEvents), not computing. CPU load alone (B: load 440) never
failed, nor did nextest (process per test) under CPU load.

**Instrumented run** (throwaway: `EVENT_TIMEOUT` and `LINK_EVENT_TIMEOUT`
raised to 120 s, `changes_until` logs elapsed time, count and any
`Change::Rescan`):

| Run | `changes_until` OK | OK latency | TIMEOUT at 120 s | Rescans | Events seen before timeout |
|---|---|---|---|---|---|
| idle | 13 | max 0.093 s, median 0.040 s | 0 | 0 | - |
| churn 1 (real 249.63, user 2.04) | 2 | 0.55, 0.71 s | 7 | 0 | 0 |
| churn 2 (real 252.34, user 2.05) | 3 | 0.33-0.82 s | 8 | 0 | 0 |

Bimodal: a stream either delivers within 1 s or delivers **nothing at all**
for 120 s, not even setup noise. FSEvents raised no dropped-event signal
(`MustScanSubDirs`/`UserDropped`/`KernelDropped`) either. So the second
failure mode is not a too-short timeout.

GM-716/S7's own logs (`b7eb225e…/scratchpad/gm716s7/watcher{1,2,3}.log`,
`suite.log`; `cargo test`, load 256-444) show failure mode 1 only. For
example, `write_to_tracked_file_produces_an_event`: `left: Some(<root>)`,
`right: Some(<root>/tracked.txt)`. In the link tests the
`only the winner's spelling surfaces` lists start with `<root>`,
`<root>/.gitignore`, `<root>/src`, `<root>/src/main.ts`, `<root>/src/dup` or
`<root>/src/b.ts`: setup writes made before `ProjectWatcher::new`.

## 3. Failing tests and cause per test

All in `core/src/watcher/mod.rs`, `mod tests` (line numbers on
release-4.32.0 `ef638652`). GM-707 moves `apply.rs`/`staleness.rs`, not these,
but may move `mod.rs`'s tests with the events half; names are stable.

| # | Test (fn) | Line | Seen failing in | Mode 1 (late noise) assertion | Mode 2 (no delivery) assertion |
|---|---|---|---|---|---|
| 1 | `write_to_tracked_file_produces_an_event` | 357 | S7, D, E, F | first change == root (366, `assert_eq`) | `None` (366) |
| 2 | `write_to_gitignored_path_produces_no_event` | 370 | S7, D, E, F | `NO_EVENT_TIMEOUT` window not empty (379) | `EVENT_TIMEOUT` `is_some` (387) |
| 3 | `write_under_dot_git_produces_no_event` | 391 | S7, D, E, F | same (399) | same (405) |
| 4 | `write_under_dot_claude_produces_no_event` | 409 | S7, D, E, F | same (417) | same (423) |
| 5 | `a_save_under_a_directory_a_nested_gitignore_ignores_produces_no_event` | 520 | D, E, F | - | `changes_until` `expect` (534) |
| 6 | `reloading_after_a_gitignore_edit_applies_the_new_rules` | 577 | D, E, F | - | `changes_until` (600) |
| 7 | `a_gitignore_that_ignores_itself_still_delivers_its_own_event` | 614 | D, E, F | - | `changes_until` (626) |
| 8 | `links::edits_creates_and_deletes_under_an_alias_only_target_surface_under_the_link` | 738 | D, E, F | - | `surfaces` 20 s (725) |
| 9 | `links::an_edit_under_a_duplicated_target_surfaces_under_the_winning_link_only` | 768 | S7, D, E, F | `src/dup` setup event in `seen` (779) | `surfaces` (725) |
| 10 | `links::a_nested_gitignore_in_an_alias_only_target_surfaces_under_the_link_and_applies` | 827 | D, E, F | - | `surfaces` (725) |
| 11 | `links::un_ignoring_the_target_moves_its_events_to_the_plain_spelling` | 887 | S7, D, E, F | setup noise in `seen` (905) | `surfaces` (725) |
| 12 | `links::edits_deletes_and_creates_of_a_file_links_target_surface_as_the_link` | 921 | D, E, F | - | `surfaces` (725) |
| 13 | `links::refused_and_duplicate_file_links_are_placed_as_the_walk_lists_them` | 973 | S7, D, E, F | `src/b.ts` setup event in `seen` (1002) | `surfaces` (725) |

The other live-FSEvents tests never failed: `creating_or_removing_a_link_reloads_and_names_it`
(852) asserts no events; `inotify_watch_limit_error_falls_back_to_polling_the_affected_subtree`
(435) polls with `PollWatcher` (no FSEvents); `next_change_returns_a_rescan_as_a_rescan_of_its_directory`
(1067) uses `inject_rescan`.

Timing constants involved (mod.rs): `EVENT_TIMEOUT` 5 s (339),
`NO_EVENT_TIMEOUT` 500 ms (340), `DRAIN_TIMEOUT` 300 ms (341),
`drain_startup_noise` (352), `LINK_EVENT_TIMEOUT` 20 s (706). Raising any of
them does not fix mode 2 (no delivery in 120 s), and a quiet-period drain
cannot bound mode 1 (the noise can arrive arbitrarily late).

## 4. Fix proposals

### P1 (cause: mode 1, late noise): event-order barrier instead of a quiet window

Change `drain_startup_noise` (mod.rs:352) to write a sentinel file at the
root after `ProjectWatcher::new` and consume changes until that sentinel
surfaces. FSEvents (and inotify) deliver in event order, so every setup event
is behind it. Then rewrite the negative tests (2-4) as "write ignored, write
tracked, `changes_until(tracked)`; nothing under the ignored dir in `seen`",
the same shape #5 already has. Test 1 becomes `changes_until(tracked).is_some()`
instead of "first change == tracked". Prototype: `repro/barrier.patch`
(+24/-25 in mod.rs).

- Evidence: under churn + CPU, unpatched failed 10/10 runs; patched passed
  runs 1-6 and failed 7-10 (all in the barrier's own 20 s wait, mod.rs:360:68
  in the patched file, i.e. mode 2). It removes mode 1, which is the only mode
  S7 saw.
- Risk: the sentinel must not be ignored by any test's layers (assert it in
  the helper; the prototype did). Tests 2-4 stop proving "nothing at all
  surfaced" and prove "the ignored write never surfaced before a later
  tracked one". That is equivalent given in-order delivery, which the
  barrier already relies on. A late duplicate of the sentinel can still
  appear in a `seen` list, so assertions must filter by the paths they mean
  (they already do, except test 1's `assert_eq`, which P1 changes).

### P2 (cause: mode 2, no delivery; also removes most of mode 1): stop routing filter/remap logic through the live OS service

Tests 2-13 pin `next_change`'s filtering and remapping (`IgnoreLayers`,
`LinkTable::to_indexed`, reload), not FSEvents. Add an
`inject_path(&self, path)` test seam next to `inject_rescan` (mod.rs:276,
sending `WatchSignal::Path` on `test_events`). Feed those tests the paths
FSEvents reports (canonical real-target spelling: `gen/a.ts`, not
`src/api/a.ts`, matching each test's control note) and assert on what
`next_change` returns. These become deterministic and need no timeouts.

Keep a **small live set** (proposal: #1 `write_to_tracked_file_produces_an_event`,
#2 `write_to_gitignored_path_produces_no_event`, #8 alias-only link edit) on
real FSEvents with the P1 barrier. That covers "FSEvents reports the real
target spelling" and "the stream works at all".

- Risk: the injected spelling becomes an assumption. If macOS changed which
  spelling it reports, the injected tests would keep passing and only the live
  #8 would catch it, so #8 must stay live. Controls listed in each test's doc
  comment need re-checking against the injected form (the `next_change` remap
  controls of tests 8 and 10 still discriminate: they act on the path
  `next_change` receives). Edit size: ~11 tests, mostly replacing
  `write(...)`+`surfaces(...)` pairs with `write(...)`+`inject_path(...)`.

### P3 (cause: mode 2 for the live set): run the remaining live tests only where FSEvents is not flooded

Mode 2 cannot be fixed by the test. A machine whose fseventsd delivered
nothing for 120 s cannot pass a live event test, and skipping on a starved
probe would also skip a genuinely broken watcher. Proposal: add the 3 live
tests to `.config/nextest.toml`'s `[profile.local]` heavy list (the
"run on CI, not in test-local" mechanism the file already has). Also exclude
them from test-local's `cargo test --lib` name list, which is derived from
the nextest listing (scripts/test-local.sh ~line 240), so check that the
local profile filter applies to that listing. CI runners (macOS and Linux)
keep running them.

- Risk: a watcher regression on macOS is caught on CI, not locally, one
  push later. `scripts/test-sections.sh check` / `the_local_profile_skips_exactly_its_heavy_list`
  must be updated with the list.
- Alternative (not recommended): skip when an independent raw `notify` probe
  in another tempdir also sees nothing in N s. That keeps local coverage but
  adds a skip path that can hide real failures, and costs N s per test when
  starved.

### Recommendation

P1 + P2 + P3 together: P1 fixes S7's actual failures, P2 makes 10 of 13
tests deterministic, P3 keeps the 3 live tests off flooded dev machines.
Acceptance check for S2: 5 runs of `cargo test -p g-mesh --lib watcher::`
under `repro/churn.py` x16 (+ `yes` x64) green. That generator is the one that
discriminates: 10/10 unpatched runs failed. CPU-only load is **not** a valid
control (0/15 failed).

## 5. Discovery (outside this task)

Mode 2 is also a production question: a daemon whose FSEvents stream is
starved during a heavy build receives no change events and no `Rescan`
signal for minutes. The query-time staleness check (`watcher/staleness.rs`)
is the safety net for edits missed this way. Its coverage of this case was
not traced here. Worth a separate look if users report stale answers during
large builds.

## 6. Artifacts

Scratchpad
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/57f9eeff-ce48-4fef-a935-e2d113711786/scratchpad/repro/`:
`run.sh`/`out/` (nextest idle/cpu/light churn), `run2.sh`/`out2/` (cargo test
idle/cpu/barrier), `run3.sh`/`out3/` (churn, churn+cpu, barrier), `run4.sh`/`out4/`
(instrumented 120 s), `churn.py`, `barrier.patch`.

## 7. Owner decisions (2026-11-16)

- Fix: "P1 + P2 + P3 (Recommended)".
- Production starvation (section 5): "Задача в бэклог (Recommended)".
