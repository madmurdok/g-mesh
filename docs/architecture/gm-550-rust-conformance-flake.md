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
