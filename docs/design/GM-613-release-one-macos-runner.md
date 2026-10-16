# GM-613: release.yml - macOS builds off the Intel runner, tests skipped on a CI-green commit

Design slice S1. No workflow or script is changed by this note. Line numbers
are at `a6f8fc6` (release-4.10.0).

## Today

`release.yml` runs `tests` (a `uses: ./.github/workflows/ci.yml` call, l.207-208),
then `build` (`needs: tests`, l.210-213) as a four-row native matrix
(l.224-268), then `publish` on a tag (l.601-614) or `verify` (dry run) on any
other event (l.570-573). No cache, by design.

Measured on the last three tag runs (`gh api .../runs/<id>/jobs`, minutes):

| run | tag | tests phase (x86 macOS tests) | build phase (x86 mac / arm mac / Linux / Windows) | wall |
|---|---|---|---|---|
| 38276736547 | v4.6.0 | 40.8 (40.6) | 20.4 / 5.1 / 4.3 / 9.7 | 61.8 |
| 38441041236 | v4.7.0 | 60.4 (36.2, queued 24 min for an Intel runner) | 18.4 / 5.3 / 3.4 / 5.6 | 79.6 |
| 38815370882 | v4.9.0 | 42.2 (41.9) | 13.7 / 4.4 / 4.4 / 9.2 | 56.6 |

Both phases are gated by x86_64 macOS on `macos-15-intel`: its tests are
36-42 min where every other platform is 11-19, and its release build is
13.7-20.4 min where aarch64 macOS is 4.4-5.3.

The tests are almost always a repeat. A tag is cut by `cut-release.sh` on
`merge: release-X into main`, after GM-391 (`check_release_branch_ci_passed`,
cut-release.sh l.474-526) has proven the release-side parent has a green
**full** ci.yml run (`Full test suite ran` job succeeded). In all three
releases above the tag commit's tree is byte-identical to that parent's tree:

| tag | tag commit | tree | release-side parent (tree) | parent's green full ci.yml run | ci.yml on the tag SHA itself (push to main) |
|---|---|---|---|---|---|
| v4.6.0 | c7c66d6 | 756ea87 | 029b95f (756ea87) | 38270920886, 16:03Z | green at **16:35Z - 15 min after the tag (16:20Z)** |
| v4.7.0 | 28d1115 | 67316e6 | db255a9 (67316e6) | 38427872975, 16:33Z | green 17:39Z, tag 17:44Z |
| v4.9.0 | d34a10f | f8bc1ab | a0c3cbd (f8bc1ab) | 38792039406, 19:47Z | green 22:01Z (attempt 2), tag 22:11Z |

`release.yml` already has a dry-run path: `workflow_dispatch` (and a PR
touching the pipeline) builds all four targets, runs every smoke test and the
`verify` job (`prepare-release-assets.sh dist` + SHA256SUMS in the run
summary); `publish` is bound to `refs/tags/v*` only (l.614). S3 needs nothing
new to rehearse without publishing.

Rosetta on GitHub's arm64 macOS images: the `macos-15` arm64 image README does
not list it, but the image template does install it -
`images/macos/templates/macOS-15.arm64.anka.pkr.hcl` runs
`scripts/build/install-rosetta.sh`, and the image's own test suite
(`images/macos/scripts/tests/Rosetta.Tests.ps1`) asserts `oahd` is running on
every arm64 image (found with `gh search code rosetta --repo
actions/runner-images`). The design still probes for it at run time (below)
and fails rather than skips if it is missing.

## Change

### 1. x86_64 macOS is built on the arm64 runner and smoke-tested under Rosetta

- The `x86_64-apple-darwin` row moves from `macos-15-intel` to `macos-15`
  (shape: decision D1). `cargo build --target x86_64-apple-darwin` on an arm64
  host works with Apple clang (both SDK slices ship with Xcode); the header
  (l.132-142) measured the reverse direction, x86_64 -> aarch64, so this
  direction is first proven by the S3 rehearsal.
- The five "native only" gates - `[ "$target" = "$host" ]` in
  `build-targets.sh` (l.327) and in `bundle-plugin.sh` (l.168),
  `bundle-rust-plugin.sh` (l.197), `bundle-python-plugin.sh` (l.185),
  `bundle-go-plugin.sh` (l.195) - become one helper,
  `can_run_target <target> <host>`: true when equal, or when host is
  `aarch64-apple-darwin`, target is `x86_64-apple-darwin` and
  `arch -x86_64 /usr/bin/true` succeeds. A shared `scripts/lib/runnable.sh`
  sourced by all five (each already duplicates `host_triple`). With that, every
  existing check runs on the cross-built binary unchanged: `--version` matches
  the archive version, `plugins list` finds all four plugins, each plugin's
  handshake, and `release-smoke.sh`'s two `reindex` legs (bundled plugins and
  plugin assets) - more than `--version`, and all of it executes the x86_64
  code (Rosetta translates the binary and every x86_64 plugin it spawns).
- A smoke check that is skipped must not look like a pass: release.yml sets
  `G_MESH_REQUIRE_SMOKE=1` on the build job, and with it `build-targets.sh`
  and the bundle scripts `die` instead of logging "smoke test skipped". Local
  cross-builds (no env) keep today's skip.
- Rosetta would also run an *arm64* binary that was mislabelled x86_64, so the
  arch is asserted, not assumed: after staging, `lipo -archs <bin>` must print
  exactly `x86_64` (`arm64` for aarch64), for core and the three cargo-built
  plugin binaries (Go's `GOARCH` is already explicit). On macOS targets only,
  in the same helper file (`assert_macho_arch <bin> <target>`).

Where x86_64 macOS coverage lives afterwards (GM-612 constraint):

| what | where | runner |
|---|---|---|
| full test suite, x86_64 | ci.yml `test` row `x86_64-apple-darwin` - every push to `release-*`/`main`, and the green full run the tag gate (2) requires | `macos-15-intel`, with GM-612's per-row limits (they live in ci.yml's matrix, so release.yml's `uses:` call inherits them when it does run tests) |
| full test suite, x86_64, at release time | release.yml `tests` (the same ci.yml call) whenever the gate finds no green full run | `macos-15-intel`, GM-612 limits |
| the shipped x86_64 binary: arch, `--version`, plugin discovery, plugin handshakes, two-leg `reindex` of the fixture | release.yml `build`, x86_64 row | `macos-15` arm64 under Rosetta |

No x86 test moves off the Intel runner; only the *build* does.

### 2. The tests job is skipped when ci.yml already passed a full run on that code

A new first job, `ci-gate` (ubuntu-22.04, `timeout-minutes: 5`,
`permissions: {contents: read, actions: read}`), runs a new
`scripts/ci-green-for-commit.sh <owner/repo> <sha>` with `GH_TOKEN:
${{ github.token }}` and outputs `green=true|false` plus the run id it relied
on (written to the step summary). The script reuses GM-391's query verbatim:
`actions/workflows/ci.yml/runs?head_sha=<sha>` filtered to
`conclusion == "success"`, then for each run the jobs list must contain `Full
test suite ran` with `success` (a partial run - sections selected by changed
paths, or a single-platform dispatch - does not count). Candidates: the commit
itself, plus its parents whose tree equals its tree (D2). Any API error ->
`green=false` (fail toward running the tests, never toward skipping them).

Wiring:

```yaml
tests:
  needs: ci-gate
  if: needs.ci-gate.outputs.green != 'true'
  uses: ./.github/workflows/ci.yml
build:
  needs: [ci-gate, tests]
  if: ${{ !cancelled() && needs.ci-gate.result == 'success' && (needs.tests.result == 'success' || needs.tests.result == 'skipped') }}
```

`publish`/`verify` keep `needs: build` with no `always()`: a failed `tests`
fails the `build` condition, `build` is skipped, and both stay skipped - the
"all four or nothing" rule (l.602-608) holds. `!cancelled()` rather than
`always()` so a cancelled release does not start building.

The gate runs on every event (D4), so a `workflow_dispatch` rehearses exactly
the logic a tag uses.

## Decisions for the owner

### D1. One macOS runner building both targets, or two arm64 rows in parallel

- **Today:** two rows, x86_64 on `macos-15-intel` (13.7-20.4 min), aarch64 on
  `macos-15` (4.4-5.3 min).
- **Change:** (a) as agreed, one `macos-15` row builds aarch64 then x86_64
  sequentially; or (b) keep two rows, both on `macos-15`, the x86_64 one
  cross-building.
- **Example:** (a) ~5 min arm + ~4-5 min x86 (build-script/proc-macro host
  artifacts are reused) + ~1 min Rosetta smoke = ~10-11 min; (b) ~5-6 min each,
  in parallel. Windows takes 5.6-9.7 min, so the build phase is ~10-11 min (a)
  vs ~6-10 min (b): 0-4 min apart.
- **Consequence:** (a) changes the matrix shape: every `matrix.target` use
  (l.211, 315-317, 330-335, 359-360, 557) becomes a loop over a `targets`
  list, two artifacts are uploaded from one job (or one `g-mesh-macos`
  artifact - `merge-multiple` in verify/publish accepts either), and one
  failing target hides the other's result. (b) is the runner line plus the
  script helper; artifact names, meta files and the job-per-target view stay
  as they are; it uses two macOS slots (public repo: no cost; macOS
  concurrency is the only limit).
- **Recommendation: (b)**, two arm64 rows. It gets the whole point of the
  agreed change (off the Intel runner) with the smallest edit and the same or
  better wall time. If the owner wants one runner literally, (a) is designed
  above and costs ~0-4 min more.

### D2. What "already tested" means for the tag's commit

- **Today:** nothing; tests always run (41-60 min).
- **Change:** (a) exact SHA only; or (b) exact SHA, or a parent with the
  identical tree (the release-side parent cut-release just verified).
- **Example:** v4.6.0 was tagged at 16:20Z; the main push run on c7c66d6 went
  green at 16:35Z. Under (a) that tag runs the full tests (~41 min); under (b)
  parent 029b95f, same tree 756ea87, green full run 38270920886 -> skipped.
  v4.7.0 and v4.9.0 skip under either.
- **Consequence:** (a) depends on whether main's run finished before the tag
  was pushed: 1 of the last 3 releases would still pay the 41 min. (b) skips
  whenever the *code* was tested; a ci.yml run's behaviour depends only on the
  tree plus event/branch, and the `Full test suite ran` requirement removes
  the event/branch dependence (that is what makes a run "full"). Risk of (b):
  a mistaken tree comparison would skip on untested code - covered by control
  C3 below.
- **Recommendation: (b).** Same safety argument as GM-391, which already
  trusts the release-side parent's run for the merge into `main`.

### D3. Is Rosetta enough for the shipped x86_64 binary, or also an Intel smoke job

- **Today:** the x86_64 artifact is built and smoke-run on real Intel.
- **Change:** (a) Rosetta smoke + `lipo` arch assertion only; or (b) also a
  `smoke-intel` job on `macos-15-intel` that downloads the x86_64 artifact and
  runs `release-smoke.sh` before `publish`.
- **Example:** (b) is ~3 min of work but waits for an Intel runner - 4.7.0
  waited 24 min for one.
- **Consequence:** under (a) the shipped binary is never executed on Intel
  hardware: a fault Rosetta masks (CPU-feature dispatch in ONNX Runtime;
  Rosetta on macOS 15 implements AVX2, real old Intels may not) would not be
  caught; `search_code`'s embedding is not in any smoke today anyway, and the
  same code's x86 *tests* still run natively on Intel in ci.yml. (b) restores
  real-hardware execution but puts the Intel queue back on the release's
  critical path, which this task exists to remove.
- **Recommendation: (a).** Revisit if an x86-only report comes in; the Intel
  runner disappears in autumn 2027 regardless (release.yml l.223-227).

### D4. Does the gate apply only to tags, or to every event

- **Today:** `tests` runs on tag, dispatch and PR alike.
- **Change:** (a) every event; or (b) tags only.
- **Example:** a dispatch on `release-4.10.0` after its push run went green
  full: (a) skips tests and builds in ~11 min; (b) re-runs ~41 min of tests.
- **Consequence:** (a) makes a dispatch the exact rehearsal of the tag path
  and makes repeat dispatches cheap; a dispatch on an untested SHA still runs
  everything. (b) keeps dispatches as the "test it all" button, but then the
  skip path can only be exercised by pushing a real tag, which publishes.
- **Recommendation: (a).** A dispatch that wants the tests anyway can run
  ci.yml directly.

## Risks

- **arm64 -> x86_64 cross-build is unmeasured.** The header measured only
  x86_64 -> aarch64. `ort-sys` downloads the per-target prebuilt, the C deps
  go through `cc` with `-arch x86_64`; the S3 rehearsal is the proof. If it
  fails, fall back to the Intel row for the build only (the gate, item 2,
  still saves the 41 min).
- **Rosetta removed from the image.** The probe makes the build fail with
  `G_MESH_REQUIRE_SMOKE=1`, never silently skip; fallback in the step:
  `softwareupdate --install-rosetta --agree-to-license`.
- **Gate false positive** (skips on untested code): every API failure means
  run; candidates restricted to the commit and same-tree parents; full-suite
  job required. Controls C1-C3.
- **Gate needs `actions: read`** at job level; the workflow floor stays
  `contents: read`, the build jobs never get it.
- **ci.yml renames `Full test suite ran`** -> the gate never finds green and
  always runs tests (safe direction). The script keeps the same constant as
  cut-release.sh l.473 with a "must stay equal" comment on both.
- **GM-612 limits not merged yet** when the gate does run tests: release runs
  would hit the 45-min x86 timeout as 4.9.0's main run did. GM-612 should
  land first or in the same release.

## Edit map

`.github/workflows/release.yml`
- l.5-12 status note, l.109-142 "WHY A MATRIX ON NATIVE RUNNERS": rewrite the
  macOS paragraph - the escape hatch is now used, and how the cross-built
  binary is executed (Rosetta) and its arch asserted.
- l.151-157 dispatch comment: dispatch now also exercises the gate.
- l.198-208: add `ci-gate` job; `tests` gets `needs: ci-gate` + `if:`;
  rewrite the "This costs the release a full test run" comment.
- l.210-213: `build` `needs: [ci-gate, tests]` + the `if:` above; add
  `env: G_MESH_REQUIRE_SMOKE: "1"`.
- l.219-231: x86_64 row `runner: macos-15`; rewrite the Intel comment (x86
  *tests* stay on Intel in ci.yml). Under D1(a) instead: one row with
  `targets:`, and l.211, 315-317, 324-335, 357-360, 552-563 loop over it.
- after l.317: step "Rosetta available" (`if: matrix.target ==
  'x86_64-apple-darwin'`): `arch -x86_64 /usr/bin/true ||
  softwareupdate --install-rosetta --agree-to-license`.
- l.336-350: smoke comment ("native on its own runner" -> native or Rosetta).
- l.570-573, l.601-614: no logic change; comments that say "every leg builds
  natively".

`scripts/lib/runnable.sh` (new): `can_run_target`, `assert_macho_arch`,
`smoke_skip_or_die` (honours `G_MESH_REQUIRE_SMOKE`).

`scripts/build-targets.sh`: l.87-103 header; l.142-144 keep `host_triple`;
l.324-371 gate -> `can_run_target`, add `assert_macho_arch` on the staged
binary for macOS targets, skip branch -> `smoke_skip_or_die`.

`scripts/bundle-plugin.sh` l.164-178, `bundle-rust-plugin.sh` l.193-207,
`bundle-python-plugin.sh` l.181-195, `bundle-go-plugin.sh` l.193-205: same
three replacements (Go: no `lipo` needed beyond the shared call, harmless).

`scripts/ci-green-for-commit.sh` (new): the gate query (shellcheck-clean;
ci.yml's `shellcheck` job covers `scripts/`).

`.github/workflows/release.yml` `pull_request.paths` (l.170-185): add
`scripts/lib/runnable.sh` and `scripts/ci-green-for-commit.sh`.

Not touched: `ci.yml` (GM-612 owns its x86 row), `prepare-release-assets.sh`,
`cut-release.sh` (only the shared constant comment).

## Test and rehearsal plan

Local (S2, no CI): `shellcheck` on the new/changed scripts;
`ci-green-for-commit.sh` against history, read-only `gh api`:

| case | sha | expect |
|---|---|---|
| exact green full | 28d1115 (v4.7.0) | green, run 38436504903 |
| same-tree parent | covered in CI rehearsal step 3 (needs a commit with no run of its own) | - |
| untested | any task-branch commit never pushed | not green |
| partial only | a release-branch push whose only run was partial (find with `gh api`; e.g. a `select` mode `partial` run) | not green |
| API failure | `GH_TOKEN=bad` | not green, exit 0 |

Controls (revert the code, not the test; each must flip its row):
- **C1** remove the `Full test suite ran` filter -> the partial-only SHA
  reports green.
- **C2** make the API-error path return green -> the bad-token row reports
  green.
- **C3** compare parent *SHA* instead of tree (or drop the tree check) -> an
  untested commit whose parent is green reports green (construct: a pushed
  commit with a real change on top of a green SHA).
- **C4** `can_run_target` returning false for x86-on-arm with
  `G_MESH_REQUIRE_SMOKE=1` -> the x86 row fails (rehearsal).
- **C5** `assert_macho_arch` against the staged aarch64 binary with target
  x86_64 -> fails (a one-off step in a throwaway dispatch, or locally on
  any non-matching Mach-O).

CI rehearsal (S3, `workflow_dispatch` only, nothing published - `publish`
needs `refs/tags/v*`):
The task branch (`ci/GM-613-...`) does not trigger ci.yml on push (ci.yml
pushes: `main`, `release-*`, `experiment/*`), so its commits are untested by
construction.

1. Push the task branch; dispatch release.yml on its tip (no ci.yml run) ->
   gate `green=false`, `tests` runs, all four builds, `verify` passes. This is
   the "tag on an untested commit runs tests" acceptance, through the same
   gate a tag uses (D4 a).
2. `gh workflow run ci.yml --ref <branch>` (dispatch = full), wait green, then
   dispatch release.yml on the same SHA -> `tests` skipped, `build` runs,
   `verify` passes.
3. Push `git commit --allow-empty` on top (same tree, no run of its own) ->
   dispatch -> skipped via the parent (D2 b). Then push a commit with a real
   change -> dispatch -> tests run (C3 in CI: the gate must not borrow a
   parent's run for a different tree).
4. In run 2: x86_64 row log shows `lipo` = `x86_64`, `--version` matching,
   plugin handshakes and both `reindex` legs PASS, no "smoke test skipped".
5. Assets: `verify`'s SHA256SUMS lists 41 files; its name list, with the
   version replaced, equals `gh release view v4.9.0 --json assets` names
   (41) - same layout. Checksum values differ by construction.

Measure (later slice): wall time of runs 1 and 2 vs the table under *Today*;
record runner queue time separately (4.7.0's 24 min was queue, not work).

Expected: tests skipped ~11-12 min wall (gate <1 + build ~6-11 + publish
0.5) vs 56.6-79.6; tests run ~50-70 (x86 Intel tests unchanged, x86 build
13.7-20.4 -> ~6-11).
