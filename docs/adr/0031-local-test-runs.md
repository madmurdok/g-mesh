# 0031. Local test runs: lib tests in one process, heavy tests at batch end

## Status
Accepted 2026-10-10 (GM-548, owner decisions Q1-Q4). Measurements, the
global-state audit and the options:
[`gm-548-faster-local-tests.md`](../architecture/gm-548-faster-local-tests.md).

## Context
A verify slice ran `scripts/test-sections.sh run $(scripts/test-select.sh)`
(ADR 0030), the same per-process nextest run CI uses. Locally that cost two
things CI does not feel as much:

- **One process per lib test.** The g-mesh lib has ~1765 tests; on a loaded
  developer machine each process start costs seconds. The whole lib ran in
  733s `real` per process against 146s in one libtest process (238s against
  207s CPU).
- **A few slow tests.** 56 tests took 10s or more in both of two full-suite
  logs (GM-509/S24 and GM-523). They are 26-37% of the summed suite time and
  about half of `core-it`'s wall time, since `core-it` runs at most 2 tests at
  a time.

CI must keep running everything exactly as before.

## Decision
- **One local entry point, `scripts/test-local.sh`.** A verify slice runs
  `scripts/test-local.sh --base <release-branch>`; the batch-end slice runs
  `scripts/test-local.sh --full`. CI does not call it.
- **The selected g-mesh lib tests run in one libtest process**
  (`cargo test --workspace --lib -- --exact <names>`). The names come from
  `cargo nextest list --profile local`, so section membership, the heavy
  exclusion and the isolation list each have one definition. `--workspace`
  reuses the `cargo nextest run --workspace --no-run` build. The run has a
  total budget (600s, `G_MESH_LIB_BUDGET_SECS`), because libtest has no
  per-test timeout; `--per-process` sends the lib back to nextest to find a
  hung test. Everything else runs in one `cargo nextest run --profile local`
  whose filter is the selected sections minus the lib part, so no test runs
  in both parts or in neither.
- **Lib tests that write process-wide state stay on nextest** (Q2). The
  `ISOLATED` filterset in `test-local.sh` names the tests that set env vars
  other modules read, plus one that asserts on spawn timing. Each is meant to
  take its override as an argument instead (the `cli::model::sources_from`
  pattern), after which it leaves the list.
- **Heavy tests are skipped by a `local` nextest profile only** (Q4). Its
  `default-filter` excludes one exact `(binary_id, name)` term per heavy test.
  `default` and `ci` are untouched, so CI, the release gate and
  `test-sections.sh check` run and count them as before. The list is static
  and reviewable in git. `--full` runs `test-sections.sh run --profile ci
  --keep-going all` (every section, every heavy test, JUnit written) and then
  `scripts/test-heavy-report.py`, which reports listed tests now under 10s,
  unlisted tests at 10s or more, and listed tests the run did not contain
  (renamed or deleted). The report is a finding for the batch, not a failure.
- **The two random-sequence tests run reduced locally, not skipped** (Q3).
  `G_MESH_CONTAINERS_SEEDS=<n>` takes the first `n` built-in seeds of
  `graph::containers::tests`; `G_MESH_CONTAINERS_SEED` (one seed, replay)
  still wins. `test-local.sh` sets `n=2`; `--full` and CI leave it unset, so
  they run every seed.
- **Leaf core modules select narrower sections, locally only** (Q1). With
  `scripts/test-select.sh --narrow`, which only `test-local.sh` passes, a path
  under `core/src/cli/` selects `core-cli` and `core-it`, one under
  `core/src/mcp/` selects `core-mcp` and `core-it`; every other `core/` path
  still selects all core sections. Without the flag (CI's `select` job) the
  `core/**` row of ADR 0030 is unchanged (owner: "Только локально, CI как был").

Rejected: `#[ignore]` on heavy tests (CI does not pass `--run-ignored`, so it
would lose coverage); an env gate inside each heavy test (56 edits, and a test
that forgets it runs anyway); a nextest in-process mode (nextest has none);
`cargo test -p g-mesh --lib` (rebuilds against the workspace feature set, and
needs a second copy of the heavy list); a heavy list generated from the last
run's JUnit (not reviewable, depends on another run's file).

## Consequences
- A local run skips the heavy tests and 6 of 8 (sequence) and 2 of 4 (batch)
  containers seeds. A defect only they catch shows up at batch end, not in
  verify.
- One hung lib test stops the whole lib part until the budget kills it, and a
  test that aborts the process hides the rest of the lib's results.
- A new lib test that writes the process environment races its readers in
  the one-process run until it is added to `ISOLATED`.
- The heavy list drifts between batches; the `--full` report is how it is
  kept current.
- A `cli`/`mcp` change that breaks another module's lib tests is not caught
  by the local verify run; CI (which does not narrow), `--full` at batch end
  and the release gate's full run catch it.
