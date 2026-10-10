# GM-550: Rust conformance flake, `callers[7]` misses `gaps::measure` under load

Status: diagnosis (S1). Branch `fix/GM-550-rust-conformance-flake` off `release-4.3.0` at 68be8b8.

## Summary

The flake does not come from GM-537, and the semantic pass does not drop the call by itself.
Under load, rust-analyzer fails to answer the whole-project pass's first round of
questions within the bridge's fixed 10 s per-question budget. The pass then reports
itself incomplete and names `gaps.rs` and `shapes.rs` as unfinished. Since GM-521
(cbbefff), core counts a whole-project pass that names its unfinished files as
*residual*. That is a success, not an error. The conformance kit throws the residual
count away (`.map(|_outcome| ())`) and judges the expectations straight away, against an
index that is missing `gaps.rs`'s semantic edges. `shapes.rs` gets those edges back
from the kit's later per-file pass. `gaps.rs` never does. So the test checks a "fully
linked index" that the code only promises after the owed files are asked again, on the
next daemon start or an edit.

## Machine state

- 8 CPUs (`hw.ncpu`). Load averages were 66-990 while the A/B ran. Other sessions'
  work caused most of that. The script added 16x `yes > /dev/null` for each run and
  killed them by pid afterwards.
- Each run took `real` 23-104 s at `user` 10.3-11.6 s and `sys` 3.8-4.3 s. Real time far
  above user time means the test process was waiting. It waits on the plugin child and
  rust-analyzer, whose CPU time is not counted in the test's own `user`.
- deps dir: 23.8k files (after), 24.6k (before). Not the exec-latency regime.

## A/B (8 runs per arm, alternating, conformance test alone)

Filter: `binary(conformance) & test(=the_linked_index_answers_the_acceptance_criteria)`,
`--no-capture`, under 16x `yes`. Script and logs: `scratchpad/gm550/ab.sh`,
`summary.txt` and `run-<arm>-<n>.log`.

| arm | commit | failures | load (1-min) range |
|---|---|---|---|
| before (pre GM-537) | 404ed48 | 0/8 | 66-966 |
| after | 68be8b8 | 0/8 | 370-990 |

Neither arm failed, so **this A/B tells the arms apart on nothing**. I also counted
"did not answer" lines per run. Those counts mean nothing as well: the kit quotes the
plugin's stderr only in the panic message of a failing run, so a run that passes logs
none of it. The counts are 0 whether or not a timeout happened. This matches GM-537/S7:
conformance-only runs passed 3/3 under load there too. Both S7 failures came from the
combined run (`test(/extractor::/) | binary(conformance)`, 137 tests, load 760-830). In
that run the extractor unit tests start at the same moment the whole-project pass
starts.

**Arms by code, not by count.** Every line on the failure path below is the same in
404ed48 and 68be8b8. `git show <rev>:core/src/cli/plugin_check/session.rs` contains the
`.map(|_outcome| ())` discard in both. `Budgets::default()` has `request: 10s,
concurrency: 8` in both. `plugins/rust/src/semantic.rs` has no `warm_up` in either.
GM-537's diff does not touch `core/src/watcher/apply.rs` or
`core/src/cli/plugin_check/session.rs`, and its only change to `bridge.rs` is one line
that has nothing to do with budgets. The flake therefore predates GM-537 and goes back
to at least cbbefff (GM-521). Before cbbefff the same timeout made the whole-project
pass an `Err`. That fails the session, the kit skips the expectations, and this test
still fails with a different message: "... was not judged and passed".

## Failing evidence (GM-537/S7, `scratchpad/gm537/s7/plugin1.log` and `plugin1-run1.log`)

Both failures show the same stderr, quoted by the kit:

```
[rust] rust-analyzer: ... WARN overly long loop turn took 128ms ...: PrimeCaches(End { cancelled: false })
[rust] the server did not answer a question about crates/alpha/src/gaps.rs within 10s
[rust] the server did not answer a question about crates/alpha/src/shapes.rs within 10s   (x6)
[rust] semantic pass: 14 file(s), 6 node(s)/11 edge(s) upserted, ... in 115.98s (incomplete)
g-mesh: rust's whole-project semantic pass was incomplete (the language server did not answer a
  question about crates/alpha/src/gaps.rs within 10s) - 2 file(s) left, the next start asks only those
[rust] semantic pass: 1 file(s), 9 node(s)/9 edge(s) upserted, 1 edge(s) retracted   <- fileChanged #6, shapes.rs only
...
FAIL  expectations.callers[7]
  - expected: {gaps.rs:gaps::measure, shapes.rs:shapes::total_dyn}
  - actual:   {shapes.rs:shapes::total_dyn}
```

(run1: the same lines, pass time 78.5 s.) Seven timeouts all at once, right after
`PrimeCaches(End)`, is the shape of a first wave. With no warm-up, the bridge sends
`concurrency = 8` questions to a server that has only just finished indexing. Seven of
those eight go past 10 s while rust-analyzer does its lazy first analysis on a loaded
machine. `shapes.rs` gets its semantic edges back from the kit's per-file pass
(`fileChanged #6 ... through the manifest's semantic_pass gate`). `gaps.rs` is never
touched again. That is exactly why `total_dyn` is present and `measure` is missing,
and why every other `shapes.rs`-based check passes.

## Cause, with the code

1. **Trigger: a fixed 10 s budget per question and no warm-up for rust-analyzer.**
   `plugins/sdk/src/lsp/bridge.rs:161,163` (`Budgets::default`: `request: 10s`,
   `concurrency: 8`, `warm_up: None`). `bridge.rs:2198-2202`: with no `warm_up`, the
   first wave is 8 wide on the 10 s budget. `bridge.rs:2366-2393`: an expired question
   is cancelled and its file goes into `failed_files`.
   `plugins/rust/src/semantic.rs:97`: `LspBridge::new(LANGUAGE, root, config)`, with no
   `.warm_up(..)`. TypeScript sets one at `plugins/typescript/src/semantic.rs:72,99`
   (`WARM_UP = 120s`, measured in GM-325) for the same kind of slow first answer.
2. **Core turns an incomplete whole-project pass that names its files into a success.**
   `core/src/watcher/apply.rs:513-524`: `record_language_semantic_residual` returns
   `Ok(SemanticPassOutcome::Residual { left })`. In production that is correct: the
   owed files are asked again on the next start (`daemon/semantic.rs`, `OwedPass`), or
   settled by an edit to them.
3. **The kit drops that outcome.** `core/src/cli/plugin_check/session.rs:1539-1542`:
   `apply_semantic_pass(..).map(|_outcome| ())` ("the check reads only success or
   error"). The session gets no failure, and `core/src/cli/plugin_check/mod.rs:420`
   (`session_ready = bulk1_complete && session.failure.is_none()`) lets the
   expectations run. Their own guard says they need "the fully linked index a completed
   session leaves behind" (`mod.rs:428`). Since GM-521, a session that finishes without
   a failure no longer means that.

So the test asserts something the code does not promise inside one session. The pass
does not silently lose the call: it reports the file as unfinished, and core records it
as owed. Nothing in the kit acts on that record.

Callers relied on (g-mesh, project `g-mesh`, release checkout):
`find_callers(apply_semantic_pass)` returned `cli::plugin_check::session::Driver::step`
(session.rs:1501), `daemon::plugin::PluginProcess::semantic_pass` (plugin.rs:1175),
`core/tests/protocol_conformance.rs` and 12 unit tests in `watcher/apply/tests.rs`
(`hasMore: false`). The kit is therefore the only place outside the daemon that turns
the outcome into a verdict. I used grep, on purpose, for single known strings: the log
messages, `warm_up` call sites, and `Budgets`.

## Fix options (no retries or sleeps)

**A. Give rust-analyzer a warm-up budget** (`plugins/rust/src/semantic.rs:97`,
`.warm_up(WARM_UP)`, the same mechanism as TypeScript).
- Benefit: removes the trigger in production as well as in the test. A user's daemon on
  a busy machine would otherwise leave files owed until the next start, which is the
  same partial index an agent queries. The first question goes out alone on a long
  budget, rust-analyzer finishes its lazy analysis once, and then the bridge widens to 8.
- Risk: the size of the budget is a guess until measured. This fixes the trigger but
  not the kit's blind spot, so if a later question times out for some other reason, the
  test flakes again with the same misleading "missing caller" message. A server that
  never answers costs one long wait, the same as with TypeScript.

**B. The kit treats a residual whole-project pass as "not linked"**
(`session.rs:1539-1542` keeps the outcome. `Residual { left > 0 }` becomes a session
failure, or a FAIL on its own check, quoting the pass's reason. `mod.rs:420` then skips
the expectations, and `the_linked_index_answers_...` fails with "was not judged").
- Benefit: the verdict is honest. A timeout reads as "the semantic pass was incomplete:
  rust-analyzer did not answer about gaps.rs within 10s", not as a phantom missing
  caller. It applies to every plugin the kit checks.
- Risk: the flake stays red under load, but correctly labelled. `g-mesh plugins check`
  becomes stricter for every plugin, and a third-party plugin on a slow CI box now fails
  `session` where it used to pass. The kit's own tests for the residual path need
  updating.

**C. The kit runs the product's own recovery: one residual pass over the owed files**
(`apply_residual_semantic_pass`, `core/src/watcher/apply.rs:349`, with
`schema::semantic_residual_files`) before it judges the expectations.
- Benefit: it judges the index the product actually reaches, the one a daemon restart
  produces, and it tests GM-521's path end to end.
- Risk: in effect a retry inside the check, which the brief rules out as a fix. It hides
  a real production gap: one daemon session really does serve the partial index until a
  restart.

**D. Change the test to skip or accept the case when the pass was incomplete.**
Rejected. It hides the gap and turns the assertion into "pass unless it didn't".

## Recommended fix

**A + B.** A removes the cause, both for users and for this test. B makes sure any
leftover case says what really happened, not a wrong expectation diff.

For A, the warm-up value should be measured rather than guessed. Run the conformance
fixture under the S7 combined load and record how long rust-analyzer takes to answer
its first question after `PrimeCaches(End)`, as GM-325 did for vtsls. The default
starting point is TypeScript's 120 s, which is small against the 15-min whole-project
floor.

**Controls for the fix slices.**
- Revert `.warm_up` and run the bridge unit test that pins it. The test must fail, the
  same pattern as `the_engine_gives_vtsls_a_two_minute_warm_up`.
- Revert the kit change and run a kit test where a toy/fake plugin returns a listed
  incomplete whole-project pass. The test must fail. The toy plugin can already answer
  `incomplete` with `unfinishedFiles`; see `plugins/sdk/tests/lsp_bridge.rs` and the
  GM-521 tests.

Load does not have to reproduce for either control.

## Edit map

| file | change | option |
|---|---|---|
| `plugins/rust/src/semantic.rs:80-97` | `const WARM_UP: Duration` with its measured rationale; `.warm_up(WARM_UP)` | A |
| `plugins/rust/src/semantic.rs` tests | pin the wiring (same as TS `the_engine_gives_vtsls_a_two_minute_warm_up`) | A |
| `core/src/cli/plugin_check/session.rs:1528-1542` | keep `SemanticPassOutcome`; `Residual { left > 0 }` -> `session.failure` (or a dedicated check) quoting the reason | B |
| `core/src/cli/plugin_check/mod.rs:420-432` | the skip message also names "the whole-project semantic pass left N file(s) unfinished" | B |
| `core/src/cli/plugin_check/` tests, `plugins/sdk/tests/toy_conformance.rs` | a listed incomplete whole-project pass now fails `session`; update any test that relied on it passing | B |
| `plugins/rust/tests/conformance.rs` module doc (lines 28-40) | the doc says an incomplete whole-project pass is a session failure; true again after B, so only reword it | B |
| docs on the kit's expectations rule (decision 1) | a residual pass is not "fully linked" | B |

## Open questions for the owner

**Q1. Should `g-mesh plugins check` fail a session whose whole-project semantic pass
left files unfinished?**

- *Today:* since GM-521, a whole-project pass that times out on some files and names
  them is recorded as residual and counts as success. The kit accepts it and judges the
  expectations against an index missing those files' semantic edges. The verdict blames
  the content ("`gaps::measure` missing from `callers[7]`"). The real problem is that
  rust-analyzer did not answer a question within 10 s.
- *Change:* the kit treats `Residual { left > 0 }` from the whole-project pass as a
  session failure that quotes the reason. The expectations are then skipped (the
  existing "needs a fully linked index" rule).
- *Example:* on a loaded CI box, a third-party plugin whose server is slow to answer
  its first questions used to get PASS on `session` and FAIL on some expectations. Now
  it gets FAIL on `session` ("the whole-project semantic pass left 2 file(s)
  unfinished: the language server did not answer a question about gaps.rs within 10s"),
  and its expectations are SKIP.
- *Consequence:* the check is stricter for every plugin, but every failure names its
  real cause.
- Options:
  1. **Fail the session on a residual pass (Recommended).** Benefit: honest verdict,
     consistent with the kit's rule that expectations need a fully linked index. Risk:
     slow servers show as red `session` failures. The warm-up (option A) covers Rust's
     case.
  2. **Keep the session passing, but add a FAIL check `semantic-pass-complete`.**
     Benefit: the other session checks stay green, and the failure is named
     separately. Risk: one more check id to keep up, and the expectations still need to
     be skipped, or the misleading diff is printed next to it.
  3. **Run one residual pass in the kit before judging (option C).** Benefit: judges
     the index a restart reaches. Risk: it acts as a retry and hides that a single
     daemon session serves a partial index.

## Owner decision (2026-10-10)

Q1: "Проваливать сессию (Recommended)": a whole-project semantic pass that leaves files
unfinished fails the `session` check with its reason; the expectations are skipped.
Fix = A (measured rust-analyzer warm-up) + B (kit fails the session on a residual pass).

## Warm-up measurement (S4, 2026-10-10)

**Verdict: the control did not pass, so this measurement cannot size the warm-up.**
At this load, rust-analyzer answers its first question after `PrimeCaches(End)` within
milliseconds, idle and loaded alike. The 10 s+ first wave from GM-537/S7 did not
reproduce. The load did reach rust-analyzer: it more than doubled the time to
`PrimeCaches(End)`. But that time falls inside the readiness budget, not the
per-question one. A second finding matters for option A (below): in 2 of 18 runs the
bridge called the server ready during a gap between two startup phases.

### Method

- Throwaway worktree `../g-mesh-wt-gm550-meas` at 99867cf, not committed. One local
  patch: `plugins/rust/src/semantic.rs:97` gets `.warm_up(Duration::from_secs(600))`.
  With that patch the first question goes out alone and cannot time out, so its full
  wait is visible. Without it, the first wave is 8 questions that get cancelled at 10 s.
  Debug build, the same profile the conformance test uses.
- `g-mesh plugins check <scratch>/rust --fixture plugins/rust/conformance/project
  --expect plugins/rust/conformance/expect.toml`, under `/usr/bin/time -p`. The scratch
  manifest is `plugins/rust/plugin.toml` with two changes: the patched plugin binary,
  and `[plugin.semantic] command` pointing at a transparent stdio proxy. The proxy logs
  one line per LSP message (direction, method, id, `$/progress` token and kind, ms since
  the server started, no payloads), the same technique as GM-325/S41. rust-analyzer
  1.97.1.
- The bridge has no per-question latency logging. It logs only timeouts and the
  warm-up line (`bridge.rs` `run_pass`), so the proxy is the clock.
- Three arms, alternating, 6 rounds: `idle`, `y16` (16x `yes > /dev/null`) and `y64`
  (64x). Load was started 3 s before each run and killed by pid after it. Script,
  proxy, analysis and logs are in `scratchpad/gm550/warm/` (`bin/run.sh`,
  `bin/ra_proxy.py`, `bin/analyse.py`, `summary.txt`, `runs/<arm>-<n>/`).
- "First wait" is the time from sending the first `textDocument/*` question to its
  answer. "Ready" is the last `rustAnalyzer/cachePriming` `end` before that question,
  in seconds from rust-analyzer's start. "Later" covers every other question of the
  instance.

### Machine state

8 CPUs. The 1-minute load average was 5.0 just before the series and 32-441 during it.
Other sessions caused most of that, which is why "idle" is relative. It was 245 at the
end. `real` was 21-107 s. `user`/`sys` were 9.8-12.2 s and 3.4-4.3 s in the 5 runs where
the plugin's child exited by itself, and about 1.1-1.4 s and 0.3 s in the 13 runs where
the plugin killed the proxy at shutdown. A killed child is never reaped, so its CPU
time is not counted. That is an artefact of the proxy, and it happens after the pass.
Either way, `real` far above `user + sys` means waiting, mostly on rust-analyzer, whose
CPU time the kit's rusage does not include.

### Per run

| run | load 1-min before / after | real s | user s | sys s | ready (s) | first question after ready | first wait | later: median / max | later >= 5 s | kit |
|---|---|---|---|---|---|---|---|---|---|---|
| idle-1 | 33 / 73 | 22.9 | 10.8 | 4.0 | 15.7 | 2.02 s | 5 ms | 7 ms / 0.70 s | 0 | PASS |
| y16-1 | 63 / 82 | 28.7 | 1.1 | 0.3 | 21.8 | 2.01 s | 4 ms | 22 ms / 0.85 s | 0 | PASS |
| y64-1 | 84 / 202 | 39.5 | 1.2 | 0.3 | 25.2 | 2.01 s | 5 ms | 26 ms / 0.99 s | 0 | PASS |
| idle-2 | 211 / 218 | 27.6 | 1.4 | 0.3 | 19.8 | 2.02 s | 56 ms | 39 ms / 1.94 s | 0 | PASS |
| y16-2 | 197 / 276 | 55.7 | 1.3 | 0.3 | 43.5 | 2.02 s | 42 ms | 49 ms / 1.27 s | 0 | PASS |
| y64-2 | 266 / 338 | 47.6 | 1.3 | 0.3 | 28.7 | 2.01 s | 5 ms | 31 ms / 0.97 s | 0 | PASS |
| idle-3 | 333 / 350 | 34.6 | 1.4 | 0.3 | 25.8 | 2.03 s | 30 ms | 44 ms / 1.36 s | 0 | PASS |
| y16-3 | 308 / 386 | 52.6 | 1.4 | 0.3 | 41.0 | 2.02 s | 35 ms | 58 ms / 1.03 s | 0 | PASS |
| y64-3 | 386 / 441 | 91.0 | 1.3 | 0.3 | 68.1 | 2.00 s | 30 ms | 47 ms / 3.28 s | 0 | PASS |
| idle-4 | 430 / 97 | 106.7 | 11.6 | 4.0 | 90.1 \* | before ready \* | 2602 ms (empty) | 5 ms / 0.92 s | 0 | PASS |
| y16-4 | 89 / 173 | 34.5 | 1.2 | 0.3 | 26.7 | 2.01 s | 5 ms | 43 ms / 1.36 s | 0 | PASS |
| y64-4 | 171 / 225 | 72.2 | 1.2 | 0.3 | 56.2 | 2.02 s | 5 ms | 21 ms / 0.94 s | 0 | PASS |
| idle-5 | 207 / 150 | 21.1 | 9.8 | 3.4 | 15.6 | 2.00 s | 15 ms | 3 ms / 0.75 s | 0 | PASS |
| y16-5 | 127 / 218 | 49.5 | 1.2 | 0.3 | 37.8 | 2.01 s | 5 ms | 47 ms / 2.59 s | 0 | PASS |
| y64-5 | 201 / 356 | 52.5 | 1.3 | 0.3 | 34.2 \* | before ready \* | 74 ms (empty) | 9 ms / 1.44 s | 0 | PASS |
| idle-6 | 327 / 220 | 27.0 | 12.2 | 4.3 | 17.7 | 2.03 s | 7 ms | 4 ms / 1.08 s | 0 | PASS |
| y16-6 | 195 / 230 | 37.4 | 1.3 | 0.3 | 27.0 | 2.01 s | 6 ms | 15 ms / 1.52 s | 0 | PASS |
| y64-6 | 247 / 277 | 76.4 | 1.2 | 0.3 | 58.0 | 2.00 s | 5 ms | 12 ms / 0.86 s | 0 | PASS |

Every run logged the warm-up line once and no `did not answer` line. No question went
unanswered. No answer after the first took 5 s or more (the slowest was 3.28 s).

\* The two runs marked \* called the server ready too early (see below). Their "ready"
column is the real `PrimeCaches(End)`, reached 46 s (idle-4) and 25 s (y64-5) after the
first question.

### Distribution

| arm | n | first wait: median / max | ready: median (range) |
|---|---|---|---|
| idle | 6 | 22.5 ms / 2602 ms (15 ms / 56 ms without idle-4) | 18.7 s (15.6-90.1) |
| y16 | 6 | 5.5 ms / 42 ms | 32.4 s (21.8-43.5) |
| y64 | 6 | 5 ms / 74 ms | 45.2 s (25.2-68.1) |
| loaded (y16 + y64) | 12 | 5 ms / 74 ms | - |

**Control.** The idle and loaded distributions of the first wait do not differ. Both are
milliseconds, and the loaded median is in fact the lower one. Under the rule for this
slice, **the first-answer measurement tells nothing about how large the warm-up has to
be under S7's load.** The arms did differ on readiness (median 18.7 s, then 32.4 s, then
45.2 s), so the load reached rust-analyzer. It slowed indexing, which the 10-min
readiness budget absorbs, and it did not slow the answer to the first question. With
16x and 64x `yes` at a 1-minute load up to 441, the S7 shape (seven 10 s timeouts right
after `PrimeCaches(End)`, load 760-830, with the 137-test extractor run starting at the
same moment) did not reproduce. That run's contention, which comes from parallel
compile and test processes and their memory and I/O, is not what `yes` produces.

### Readiness called inside a phase gap (2 of 18 runs)

In idle-4 and y64-5, rust-analyzer's first `Fetching` ended, and the next phase
(`Building CrateGraph`) began 2.47 s and 2.04 s later. Both gaps are longer than the
bridge's 2 s `settle`. The bridge called the server ready and sent its warm-up question
2.007 s after `Fetching` ended. That question was answered **empty** in 2.6 s and 74 ms.
The answer spent the warm-up (`mark_warmed_up`), and 16 questions went out at once
while rust-analyzer was still fetching, scanning roots and priming, for another 46 s
and 25 s. Here they were all answered empty within 15 ms, deferred, and asked again
after the server went quiet (ids 19+ in idle-4 go out 2.0 s after the real
`PrimeCaches(End)`). The deferral rule absorbed it, and both runs passed.

This matters for option A. If the server answers a question asked mid-indexing slowly
rather than empty, the warm-up does not protect it: the warm-up has already been spent
on the premature empty answer, and the wave is 8 wide on 10 s. The GM-310 trace in
`plugin.toml` measured the largest gap at 182 ms idle. Under load it reached 2.47 s
here. S7's stderr says the timeouts came "right after `PrimeCaches(End)`", which is the
case the warm-up does cover. It carries no per-message timestamps, though, so it cannot
rule out this path. Option B (the kit fails a residual session) covers both paths.

### Proposed `WARM_UP`

**120 s, the same as TypeScript.** The data cannot support a tighter value, and nothing
in it argues for a larger one.

- Observed: the slowest first answer was 2.6 s (an empty answer during indexing), and
  74 ms loaded. 120 s / 2.6 s gives a 46x margin over everything measured.
- The case that failed in S7 needed more than 10 s for 7 of 8 questions. 120 s is 12x
  that budget. TS's vtsls needed at most 38 s at load 103 (GM-325 §5, 32% of 120 s).
- Cost: the warm-up keeps one question in flight until the first answer. Here that is
  4-74 ms of wall time per pass (2.6 s at worst), so the price on a healthy machine is
  milliseconds. A server that never answers costs one 120 s wait, after which the
  bridge widens to 8 on 10 s.
- Against the whole-project floor: 120 s / 900 s = 13% of the 15-min floor, which leaves
  780 s for the pass itself. The fixture's whole run was 21-107 s here, and S7's pass
  took 78.5-116 s *with* seven 10 s timeouts.

Not measured: whether 120 s is enough at S7's own load. Reproducing that needs the
combined extractor and conformance run, which this slice's rules exclude.
