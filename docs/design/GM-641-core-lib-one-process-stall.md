# GM-641: the core lib's one-process run stalls on one FIFO test

Diagnosis slice S1. No production or test code changes here.

## 1. The stalled test (one, not two)

The batch-end `scripts/test-local.sh --full` lib part (`target/test-local/lib.log`,
binary `g_mesh-7f8da8425161b3ed`) printed `running 1959 tests` and was killed at
`LIB_BUDGET_SECS=600`. Listing the lib tests exactly as `test-local.sh --full`
builds `lib_filter` (profile `ci`, plus the `local` profile's default-filter,
minus `ISOLATED`) gives 1959 g-mesh names; diffing them against the finished
lines of `lib.log` leaves **one** test:

- `embedding::pipeline::tests::the_model_is_hashed_without_holding_the_cache_mutex`

The "1957 ok" count missed one finished test whose `... ok` was split across
lines by interleaved stderr (`watcher::apply::tests::a_file_gone_from_disk_loses_every_row_it_owns_though_the_plugin_named_none`,
lines 2635-2636). The other test with a 60 s notice,
`graph::containers::tests::membership_invariants_hold_after_every_diff_of_a_random_sequence`,
finished (`... ok` is the log's last line): slow under every seed, not stalled.

## 2. Cause

The test (`core/src/embedding/pipeline.rs`, `the_model_is_hashed_without_holding_the_cache_mutex`)
replaces `model.onnx` with a FIFO. Inside `std::thread::scope` a `computing`
thread runs `pipeline.compute`, which reaches `EmbeddingCache::open` and then
`cache::hash_file`, whose `File::open` on the FIFO blocks until a writer opens
it. The test thread polls a non-blocking write open (`ENXIO` while no reader)
with a **30 s deadline** and `assert!`s on it.

If the computing thread has not reached `open()` within 30 s, the assert
panics. The panic unwinds to `thread::scope`, which joins every scoped thread
before re-raising. The computing thread then reaches `open(O_RDONLY)` on a FIFO
that no writer will ever open, and blocks forever. **A load-dependent test
failure becomes a permanent hang**, and libtest has no per-test timeout, so the
whole lib part sits until the 600 s budget kills it. Any other panic in the test
thread before the writer opens (the `panic!("opening the FIFO's write end")`
arm) ends the same way.

What delayed the computing thread past 30 s in the batch run (load 14-20 on 8
cores; it reaches the FIFO after a SQLite create with WAL and an immediate
transaction) is not proven; it does not matter to the fix, which removes the
hang whatever the delay.

### Evidence

| Run | Result |
|---|---|
| Test alone, 10x (`/usr/bin/time -p`) | 10/10 pass, real 0.38-0.62 s, user 0.01-0.02 |
| Full one-process run, same 1959 names, `G_MESH_CONTAINERS_SEEDS` unset, load 21-79 at start/end (`uptime`) | finished in 77 s (real 77.36, user 119.53, sys 93.78); the FIFO test passed. Not near 600 s: the stall is not "the whole run is slow" |
| Test alone under lldb, computing thread held 35 s at `hash_file` entry (whole process stopped, wall clock running), 3x | 2 pass in 35.1 s (reader won the race after resume); **1 hangs**: stderr `panicked at core/src/embedding/pipeline.rs:1505:25: the pipeline never opened the model's weights for reading`, still running at the 150 s budget |

`sample` of the hung process: the test thread in `std::thread::scoped::scope`
(`scoped.rs:163`, called from `pipeline.rs:1493`); the computing thread in
`__open` <- `OpenOptions::_open` <- `cache::hash_file` (`cache.rs:475`) <-
`pipeline.rs:1496`. Exactly the predicted state.

Ruled out: a leaked FIFO write end in a child process (the writer is opened by
`std::fs::OpenOptions`, `O_CLOEXEC` atomically; in the hang no writer exists at
all); process-wide state shared with other tests (the test touches only its own
tempdir, its own pipeline's mutex, and stderr via `log::write_line`, which takes
the stderr lock for one `write(2)`).

## 3. Reproduction

```
cd core
lldb --batch -o "command script import delay.py" \
  -o "breakpoint set -F g_mesh::embedding::cache::hash_file -N hold" \
  -o "breakpoint command add -F delay.hold hold" -o run -- \
  ../target/debug/deps/g_mesh-<hash> --exact \
  embedding::pipeline::tests::the_model_is_hashed_without_holding_the_cache_mutex --nocapture
```

`delay.py`'s `hold` sleeps 35 s on its first hit and returns `False`
(auto-continue). Racy (1 of 3 hung): after the resume either thread may run
first. Section 6 makes it deterministic.

## 4. Fix options

**A (recommended): replace the deadline with "until the reader opens or the
computing thread has finished".** The poll loop checks
`computing.is_finished()` instead of `Instant::now() < deadline`; if the
computing thread finished without ever opening the FIFO, the test fails with
the same message. A pipeline that never opens the weights still fails (it
returns, the thread finishes); a slow one is waited out.
- Benefit: no load-dependent failure and no hang; one small edit in one test.
- Risk: a pipeline bug that blocks *elsewhere* before the FIFO (not on the FIFO)
  hangs instead of failing at 30 s; nextest's slow-timeout bounds it, the
  one-process lib run only by its 600 s budget.

**B: keep the 30 s deadline, but unblock the reader before failing.** On the
deadline (and in a drop guard for any other panic), keep trying the
non-blocking write open until it succeeds or `computing.is_finished()`, close
it so the reader sees EOF, then panic.
- Benefit: keeps fail-at-30 s.
- Risk: at load the test still *fails* falsely (a red batch run instead of a
  stall), and the guard is more code to get right than the bug it replaces.

**C: put the test in `ISOLATED` in `scripts/test-local.sh`** (nextest process
per test, slow-timeout).
- Benefit: no test edit.
- Risk: hides the hang only from the one-process run; the false failure under
  load and the scope-join hang remain; `ISOLATED`'s comment says it holds one
  spawn-timing test for a load reason, this would widen it.

### Decision for the owner

Today the test fails if the pipeline has not opened the fake weights within
30 s, and that failure hangs the whole lib run until the 600 s kill. A drops the
30 s limit: the test waits as long as the pipeline is still working, and fails
only if the pipeline finished without reading the weights. Example: at load 20
the pipeline takes 40 s to open its SQLite cache; with A the test passes after
40 s, with B it fails at 30 s and the run finishes red but on time. Consequence
of A: a future bug that hangs the pipeline before the weights is caught by the
runner's timeout, not by the test's own message. Recommended: A.

## 5. Edit map (option A)

- `core/src/embedding/pipeline.rs`, test
  `the_model_is_hashed_without_holding_the_cache_mutex`: drop `deadline`; in the
  `ENXIO` arm replace the `assert!(Instant::now() < deadline, ...)` with
  `assert!(!computing.is_finished(), "the pipeline never opened the model's weights for reading")`
  (then the 10 ms sleep as today). Update the test's doc comment: why there is
  no deadline (a panic inside `thread::scope` would join a reader blocked on
  the FIFO forever). Nothing else changes; no production code.

## 6. Control

Hold the computing thread, not the process: in the lldb callback, take
`frame.GetThread()`, `Suspend()` it, continue the process, and from a Python
timer after 35 s stop the process, `Resume()` the thread, continue. The test
thread then runs past 30 s while the reader cannot reach the FIFO.
- Unfixed: the deadline panics and the run hangs (killed at a 150 s budget),
  3 of 3.
- Fixed (A): passes, 3 of 3, in about 35 s.
The verify slice dry-runs this script on the unfixed binary first (it must
hang); the racy section-3 form is not a valid control (it passed 2 of 3
unfixed).

## 7. Side finding (not this task)

The reproduction's full run failed one other test once:
`watcher::tests::inotify_watch_limit_error_falls_back_to_polling_the_affected_subtree`
(`core/src/watcher/mod.rs:455`, event path was the subtree directory, not
`huge-subtree/file.txt`). It passed in the batch run's `lib.log`. Looks like a
macOS event-coalescing flake; worth its own task.

## Appendix: what the test shares with other tests

- g-mesh `find_definition log_line` / `find_definition log::write_line`
  (project g-mesh-wt-gm641): `core/src/log.rs`, one `write(2)` under the stderr
  lock; not held across a blocking call.
- `core/src/process.rs` `spawn_serialized` (read by symbol): the only
  process-wide spawn lock; this test's `mkfifo` spawn does not take it and its
  FIFO fd is never opened before that spawn finishes.
- grep (non-code-graph): `mkfifo` appears only in this test; no `fork()` in
  `core/src`.
