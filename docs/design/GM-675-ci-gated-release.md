# GM-675: cut-release gates the tag on green GitHub CI, not a local test run

Status: design, for owner review. Branch `fix/GM-675-ci-gated-release` at
release-4.21.0 `6ac4c02`.

Owner decision 2026-10-31: the batch-end full suite and the release gate run
on GitHub CI of the release branch tip, not locally. Per-task verify stays
local (`scripts/test-local.sh --base`). Agents may push `release-*` branches
(never `main`, never force). Merging into `main` stays the owner's word.

## Today

The release ritual runs the whole suite three times on the same code:

1. **Batch end, locally:** `scripts/test-local.sh --full` (every section,
   heavy tests included, plus the heavy-list report). Its failures become
   batch tasks.
2. **`scripts/cut-release.sh` on `main`, locally** (`main()` l.826-886,
   after every cheap check and before `git tag`):
   `test-sections.sh check`, `test-sections.sh run all` (default profile),
   `cargo test --doc --workspace`, then `cargo test -p g-mesh` three ways
   (`--lib --bins`, `--doc`, `--test it --test scripts -- --test-threads=2`).
   For 4.20.0 that was **5719 s** for `run all` plus **994 s** for the doctests
   and `-p g-mesh`: about 112 minutes before the tag could exist.
3. **GitHub CI** on `release-*` pushes and dispatches (ci.yml, four
   platforms, **27-35 min** wall time; the x86_64 macOS row may take up to
   its 75 min job timeout).

cut-release already checks CI before step 2 (GM-391,
`check_release_branch_ci_passed`, l.656-725, called at l.800):

- `main`'s HEAD subject is `merge: release-<v> into main` and it has a
  second parent (`tip = main^2`);
- `origin/release-<v>` is exactly `tip` (no stale head);
- some ci.yml run with `head_sha=tip` has conclusion `success` **and** its job
  "Full test suite ran" succeeded (a push after the branch's first push is
  path-selected, so it is partial; the full run comes from
  `gh workflow run ci.yml --ref release-<v>`).

What it does not do: it does not wait for a running run (a run in progress
reads as "no successful run" and dies), it does not say why a run failed
(failed, cancelled, re-run), it does not filter the API answer by
`head_sha` (ci-green-for-commit.sh does), and it does not check that the
tagged commit's tree is the tested tree. It also runs early, before the
version checks, so a wait there would delay cheap failures.

`release.yml`'s `ci-gate` (GM-613, `scripts/ci-green-for-commit.sh`) asks
the same question of the tag commit: a green full ci.yml run on the commit or
on a parent with the **same tree** (owner decision D2: same code, not same
SHA). If it finds none, release.yml calls ci.yml again before building. So a
gate error in cut-release costs a re-run, never an untested release.

## Which SHA CI must be green on

The tag lands on `main`'s merge commit `M = merge: release-<v> into main`.
CI on `release-<v>` ran on `T = M^2`.

**Recommended (A): `T`, plus `tree(M) == tree(T)`.** When `main` has no
commit the release branch lacks (the normal case: every change reaches
`main` through a release merge), the `--no-ff` merge's tree is `T`'s tree
byte for byte, so a green full run on `T` tested exactly the tagged code.
The tree check is new and turns "main moved" into a refusal instead of an
assumption. It is the rule release.yml's `ci-gate` already applies (D2), so
both gates agree on what "this code passed CI" means, and the tag push then
skips the tests (ci-gate finds the same run).

Alternative (B): require a green full run on `M` itself. A push to `main`
is always full, so the merge push starts one. Benefit: literally "the exact
commit being tagged". Risk: a second 27-75 min wait after every merge, and a
red result appears only after `main` already carries the code. It adds no
coverage over (A) when the trees are equal, and (A) refuses when they are not.

## Gate design

`check_release_branch_ci_passed` splits in two:

- **`check_release_merge_shape <v> [merge_rev]`** (cheap, stays at l.800):
  subject, second parent, `origin/release-<v> == T`, and new
  `git rev-parse M^{tree} == T^{tree}` else die "main has commits that are not
  on release-<v> - merge main into release-<v>, push, let CI pass, merge
  again".
- **`wait_for_green_full_ci <v> [merge_rev]`** (new, replaces the local test
  block at l.826-886, so every cheap check fails first and the wait is the
  last step before `git tag`).

### Query (one poll)

```
gh api "repos/$slug/actions/workflows/ci.yml/runs?head_sha=$T&per_page=100" \
  --jq '.workflow_runs[] | select(.head_sha == "<T>")
        | "\(.id) \(.status) \(.conclusion // "none") \(.run_attempt) \(.event)"'
```

and for each run with `status == completed` and `conclusion == success`:

```
gh api "repos/$slug/actions/runs/$id/jobs?filter=latest&per_page=100" \
  --jq '[.jobs[] | select(.name == "Full test suite ran" and .conclusion == "success")] | length'
```

Same call shape as today (`gh api <endpoint> --jq <filter>`, no
`--paginate`), so the existing fake `gh` keeps working. `filter=latest`
makes the jobs those of the run's latest attempt (the API default, written
out so a re-run is read correctly).

### Classification, in this order

| State | Rule | Action |
|---|---|---|
| green | some run `completed/success` whose full-suite job succeeded | log run id, continue to tag |
| running | otherwise, some run with `status != completed` (queued, in_progress, waiting, pending, requested) | sleep, poll again |
| partial | otherwise, some run `completed/success` without a successful full-suite job | die, hint `gh workflow run ci.yml --ref release-<v>` (today's message) |
| failed | otherwise, at least one completed run (failure, cancelled, timed_out, startup_failure, action_required, ...) | die naming each run id, conclusion, attempt and `https://github.com/<slug>/actions/runs/<id>`; cancelled adds "a newer push to release-<v> cancels a running run (ci.yml concurrency)" |
| missing | no run for `T` | die, hint push + dispatch (today's message) |

Waiting: poll every 60 s (`CUT_RELEASE_CI_POLL_SECS`, a test seam), up to
`--ci-timeout <minutes>` (default 90: the x86_64 macOS row's 75 min job
timeout plus queueing). Each poll logs one line (`waiting for ci.yml run
<id> (in_progress, attempt 1), 12m of 90m`) so the wait never looks hung.
`--ci-timeout 0` means do not wait (a running run dies at once with its
URL). On timeout: die naming the running run; re-running cut-release resumes
the wait, nothing was tagged.

API errors: a failed query is retried; three consecutive failures die
("could not query ci.yml runs ... is 'gh auth status' logged in?"). An error
never counts as green. `gh` missing dies before the first query (today).

### Failure modes

- **Re-run.** GitHub re-runs keep the run id and bump `run_attempt`; the runs
  API reports the latest attempt's status. A failed run someone re-runs reads
  `in_progress`, the gate waits, and passes when the attempt turns green.
  A red run whose re-run is not started yet dies as failed, naming the
  attempt; re-run it and run cut-release again.
- **Multiple runs for one SHA.** Typical: a partial push run plus a full
  dispatch run. Any green full run wins, whatever the others say (today's
  policy, GM-391: a red run followed by a green one on the same SHA is a
  flake someone re-ran). A running run is waited for only when no green full
  run exists yet.
- **A skipped job.** "Full test suite ran" is skipped when the run was
  partial (`select` said partial/none), the matrix was narrowed (single-target
  dispatch), or a test row failed (`needs: test`). Skipped is not success, so
  such a run is partial (if green) or failed. Other jobs (lint, shellcheck,
  installer, Go-without-toolchain) count through the run conclusion.
- **A running run that will end partial.** A push run's mode is only known
  when it finishes, so the gate waits for it and then dies as partial. In the
  intended flow the full dispatch already ran at batch end, so this happens
  only when someone pushed again after it (and then `origin/release-<v>` is
  no longer `T`, which dies earlier).
- **Stale head.** `origin/release-<v> != T` dies before any query (today).
  Runs whose `head_sha` is not `T` are filtered out in the jq (new; the fake
  answers every endpoint, so a test can prove it).
- **Cancelled by concurrency.** ci.yml's `concurrency: ci-${{ github.ref }}`
  with `cancel-in-progress` means a dispatch cancels the branch's running push
  run and vice versa. The cancelled run is ignored when a green full one
  exists; alone it dies as failed with the concurrency note.

### Local steps that stay

`scripts/test-sections.sh check` still runs before the wait (acceptance).
It runs `cargo nextest list`, which builds the test binaries (a few minutes
incremental), and needs no plugin `node_modules`, so `plugin_bins_on_path`
is dropped from cut-release (it existed for `run all` and `cargo test`,
GM-628). `check_core_feature_isolation` (no build) stays.

### Flags

- `--skip-tests` is removed. Passing it dies: "--skip-tests is gone (GM-675):
  cut-release runs no test suite; CI on release-<v> is the gate". It existed
  to avoid paying for the local suite twice; there is no local suite now.
- `--ci-timeout <minutes>` is new (default 90, 0 = no wait).
- No escape hatch around the CI gate: a release needs GitHub anyway
  (release.yml builds and publishes there).

## What the old local run covered that CI does not

| Old local coverage | In CI today? | Proposal |
|---|---|---|
| `test-sections.sh run all` (every section, default profile, heavy tests included) | yes: every section per platform under `--profile ci` (`inherits = "default"`), four platforms instead of one | nothing |
| `test-sections.sh check` | yes, Linux row | still also run by cut-release |
| `cargo test --doc --workspace` | **no**: nextest does not run doctests, clippy does not either | add a step to the Linux test row, full runs only |
| `cargo test -p g-mesh` (package scope that ships, GM-302; also the only run of core's lib in **one** libtest process, which gm-548 relied on) | **no** | add steps to the Linux test row, full runs only: `--lib --bins`, `--doc`, `--test it --test scripts -- --test-threads=2` |
| Heavy-list report (`test-local --full`, `test-heavy-report.py`) | no report, but ci.yml uploads `junit-<target>` artifacts the report can read | batch end runs the report on the aarch64-apple-darwin JUnit artifact; additions only, see must-confirm Q3 |
| Lib tests in one libtest process per crate (`test-local --full`) | no (nextest: one process per test) | covered by the CI `cargo test -p g-mesh --lib` step for core; the plugin crates' one-process run stays in per-task verify (`test-local --base`) |
| Go plugin `go test` | yes, plus `go vet`/`gofmt` | nothing |

**Placement:** steps inside the `test` job's `x86_64-unknown-linux-gnu` row,
after the sections and before "Test results, by binary", gated
`if: ${{ !cancelled() && steps.build.outcome == 'success' && matrix.target == 'x86_64-unknown-linux-gnu' && needs.select.outputs.mode == 'full' }}`.
They reuse the row's toolchain, rust-analyzer, pyright, vtsls and Node
installs (core's `it` tests spawn real plugins), so no setup is duplicated;
a separate job would copy ~200 lines of install steps. Because they are in
`test`, "Full test suite ran" (`needs: test`) succeeds only when they pass,
so both gates (cut-release, release.yml `ci-gate`) inherit them with no
change to either query. Partial runs skip them: section selection is
unchanged. Step names must not start with `tests: ` (GM-671's K5 guard reads
those as sections): `doctests (workspace)` and
`shipped scope: cargo test -p g-mesh`, with `timeout-minutes: 25`.

Cost: the Linux row is 10-19 min today; `-p g-mesh` rebuilds core under the
package-scope feature set and runs `it`/`scripts` at 2 threads. The x86_64
macOS row (24-50 min) stays the long pole, so wall time should not grow;
the implement slice's first full dispatch measures it (the Linux row's
time and each new step's time from the run's job API, next to the row's
existing "Runner state" step output).

## Batch-end flow (text for CLAUDE.md Testing and ADR 0031)

CLAUDE.md "Testing", replacing the batch-end and release-gate lines:

> - **Batch end:** push the release branch and run the full CI on its tip;
>   its failures become tasks in the batch.
>
>   ```
>   git push origin release-<v>
>   gh workflow run ci.yml --ref release-<v>   # a push after the first is partial; a dispatch is full
>   gh run watch <run id> --exit-status       # one background wait, no polling
>   ```
>
>   Every section on four platforms, heavy tests included, plus the
>   workspace doctests and `cargo test -p g-mesh` on Linux. The heavy-list
>   report reads that run's JUnit: `gh run download <run id> -n
>   junit-aarch64-apple-darwin -D <dir>`, then
>   `scripts/test-heavy-report.py .config/nextest.toml <dir>/*.xml`.
>   `scripts/test-local.sh --full` stays available to run by hand.
> - **Release gate:** `scripts/cut-release.sh` runs no suite. It tags only
>   when ci.yml passed a full run on the release branch tip that `main`
>   merged (same tree), waiting for a run still in progress.

ADR 0031: an "Amendment (GM-675, 2026-10-31)" section under Decision:
the batch-end slice no longer runs `test-local.sh --full`; it pushes
`release-*` and dispatches ci.yml, and the release gate is that run; why
(4.20.0: 5719 s + 994 s locally against 27-35 min on runners that are not
the loaded developer machine); and Consequences: the heavy list is now
grown from CI timings (shorter than local ones, so fewer additions), and a
test that fails only on the loaded local machine is no longer caught at
batch end (it surfaces in some later per-task verify, or not at all). Line 54
and l.99-100 ("the release gate's full run") point at CI.

## Edit map

| File | Where | Change |
|---|---|---|
| `scripts/cut-release.sh` | header l.9-11, 93-97, 141-145, 196-201 | usage, "takes 10+ minutes" text, the GM-302 paragraph's "`cargo test -p g-mesh` further down" -> "ci.yml's Linux row", refusal list (CI gate states, tree check) |
| `scripts/cut-release.sh` | l.209-210 | drop the `plugin-bins.sh` source (no caller left) |
| `scripts/cut-release.sh` | `usage()` l.221-228 | `--ci-timeout`, no `--skip-tests` |
| `scripts/cut-release.sh` | `check_release_branch_ci_passed` l.656-725 | split into `check_release_merge_shape` (+ tree check) and `wait_for_green_full_ci` (poll loop, classification, `head_sha` filter, `filter=latest`, retry, timeout); keep `CI_FULL_SUITE_JOB` |
| `scripts/cut-release.sh` | `main()` l.727-760 | flags: `--ci-timeout N`; `--skip-tests` dies with the GM-675 message |
| `scripts/cut-release.sh` | `main()` l.798-800, 826-886 | l.800 calls `check_release_merge_shape`; the test block becomes `test-sections.sh check` then `wait_for_green_full_ci`; delete `run all`, doctests, `cargo test -p g-mesh` |
| `.github/workflows/ci.yml` | `test` job, after "tests: wire" (~l.842) | two Linux-only, full-only steps (doctests; `cargo test -p g-mesh` x3); comment at l.200-203 names them |
| `scripts/ci-green-for-commit.sh` | comments l.8-11, 22-23, 46 | new function name; "a full run includes the doctests and shipped scope" |
| `scripts/lib/plugin-bins.sh` | l.6 comment | drop cut-release as a user |
| `core/tests/scripts/test_select_scripts.rs` | `Release` harness l.475-570 and tests to ~l.650 | fixture rows gain `head_sha`, `status`, `run_attempt`; fake `gh` serves `runs-<n>.json` per call (counter file, last one repeats); call `wait_for_green_full_ci` with `CUT_RELEASE_CI_POLL_SECS=0`; new tests below |
| `core/tests/scripts/test_local_script.rs` | `run_cut_release` l.807-850 and the 3 tests l.852-915 | the record now holds `check` only; replace with: check runs and no `run`/`cargo test` runs; the CI gate runs after `check` and before `tag -a`; `--skip-tests` dies; a gate failure leaves no tag |
| `core/tests/scripts/` (file-reading, in `test_select_scripts.rs` next to the job-name test l.637) | new test | ci.yml's `test` job has the two steps, Linux-only, `mode == 'full'`, not named `tests: ` |
| `CLAUDE.md` | Testing l.61-86 | text above |
| `docs/adr/0031-local-test-runs.md` | Decision l.24-27, 54; Consequences l.99-100; new Amendment | as above |
| `docs/adr/0030-test-sections.md` | l.35-36, 44, 50-54, 83-88 | the `-p g-mesh` run and `check`/`run all` move: "CI's Linux row (full runs)" and "cut-release runs `check` and waits for CI" |
| `README.md` | l.418-427, 1136 | step 3: waits for/requires a green full ci.yml run, runs `check`, no `cargo test --workspace`; l.1136 "ci.yml's Linux row runs core this way" |
| comments | `scripts/test-sections.sh` l.13, `.config/nextest.toml` l.63, `core/src/graph/containers/tests.rs` l.636, `core/tests/scripts/test_sections_scripts.rs` l.289, `core/tests/it/common/mod.rs` l.57 | "the release gate" -> "CI's full run" where they mean the local run |

GM-671 lands first and edits ci.yml's `test` steps (new `tests: core-scripts`
step, K5 guard) and `test_sections_scripts.rs`: rebase onto it before the
ci.yml edit; the new steps go after the last `tests: ` step.

## Behaviours

- B1 A green full run on `T`, `tree(M) == tree(T)`: tags.
- B2 Only a running run: waits, tags when it turns green full.
- B3 Completed `failure`: dies before tagging, naming run id, URL, attempt.
- B4 Completed `cancelled` (alone): dies, with the concurrency note.
- B5 Any other non-success conclusion (`timed_out`, `startup_failure`): dies.
- B6 No run for `T`: dies with the push/dispatch hint.
- B7 `origin/release-<v> != T`: dies before any query; a green run whose
  `head_sha` is another commit does not count.
- B8 `tree(M) != tree(T)`: dies.
- B9 Only green partial runs: dies with the dispatch hint.
- B10 Still running at `--ci-timeout`: dies naming the run, no tag.
- B11 A failed run re-run: reads running, waits, tags on green.
- B12 Green full plus failed/cancelled/running runs on `T`: tags.
- B13 Three consecutive API failures: dies; one failure then success: goes on.
- B14 `test-sections.sh check` runs before the wait; its failure dies.
- B15 No `test-sections.sh run`, no `cargo test` in cut-release.
- B16 `--skip-tests` dies with the GM-675 message.
- B17 Every cheap check (versions, plugin versions, feature isolation) runs
  before the wait: a version drift dies without querying runs.
- B18 ci.yml: the doctest and `-p g-mesh` steps run on the Linux row of a
  full run only.

## Controls (fake `gh` on PATH)

Seven, all in `core/tests/scripts` (one test binary, one build per group):

- K1 Treat `status != completed` as missing (no wait loop) -> B2 fails.
- K2 Refuse only `conclusion == failure` (cancelled falls through to tag)
  -> B4 fails.
- K3 Drop `select(.head_sha == "<T>")` from the jq -> B7's foreign-SHA
  fixture fails.
- K4 Delete the tree check -> B8 fails.
- K5 On timeout `break` instead of `die` -> B10 fails.
- K6 Delete the `test-sections.sh check` call -> B14 fails.
- K7 Delete the `mode == 'full'` condition from the ci.yml `-p g-mesh` step
  -> B18 fails.

## Must-confirm

- Q1 Which SHA: (A, recommended) `T = M^2` plus `tree(M) == tree(T)`, the
  release.yml D2 rule; or (B) a green full run on the merge commit `M`
  itself (a second CI wait after every merge).
- Q2 The gaps: (recommended) doctests and `cargo test -p g-mesh` as Linux-row
  steps on full runs; or a separate job (duplicated setup); or drop the
  `-p g-mesh --test it --test scripts` run and keep only `--lib --bins` and
  doctests (cheaper; loses GM-302's backstop for the integration tests).
- Q3 Heavy-list report: (recommended) from CI's aarch64-apple-darwin JUnit,
  additions only; a removal still needs two local `test-local --full` runs
  under 10s; or keep the report a manual `test-local --full` job.
- Q4 Who dispatches the full run: (recommended) the batch-end step
  (`gh workflow run ci.yml --ref release-<v>`), cut-release only reads and
  waits; or cut-release dispatches when it finds no full run (no step to
  forget; cut-release then starts CI after the merge, a 30-75 min wait).
- Q5 `--skip-tests`: (recommended) removed, dies with a message; or kept as a
  no-op with a warning (old commands keep working, the flag lies).
- Q6 Wait budget: (recommended) default 90 min, `--ci-timeout N`, 0 = no
  wait.
