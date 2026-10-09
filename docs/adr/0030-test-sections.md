# 0030. The test suite runs as named sections, defined in one script

## Status
Accepted 2026-10-09 (GM-540, owner review). Facts, measurements, options and
the edit map:
[`GM-540-GM-539-test-sections.md`](../design/GM-540-GM-539-test-sections.md).

## Context
CI ran the whole workspace suite in one `cargo nextest run` step that also
built it. On x86_64-darwin that step took 24m42s against a 25-minute bound,
about half of it the build, and the step's duration said nothing about which
part of the suite was slow. A failing or hung area could not be told apart
from the rest without downloading the log. GM-539 also needs units it can
select by changed paths.

## Decision
We split the suite into 11 named sections, each a nextest filterset over the
whole workspace, held in `scripts/test-sections.sh`: `core-it`, `core-mcp`,
`core-daemon`, `core-cli`, `core-graph`, `core-rest`, `sdk`,
`plugin-typescript`, `plugin-rust`, `plugin-python`, `wire`.

- **Filtersets in a script, not nextest profiles.** CI needs the `ci`
  profile's JUnit output per section, and a nextest profile cannot inherit
  from two parents: profiles would mean 22 of them with every filterset
  written twice. With the script, sections run under the existing `default`
  and `ci` profiles unchanged. The cost: `cargo nextest run --profile
  core-mcp` does not exist; `-E "$(scripts/test-sections.sh filter core-mcp)"`
  does.
- **`--workspace` plus `package()`, never `-p`.** Every section uses the one
  build and the one feature unification, and the plugin and SDK binaries core
  tests spawn always exist. A `-p` per section would rebuild with different
  features. The release gate's `cargo test -p g-mesh` run stays, because it
  exists to test that other configuration.
- **`core-rest` is a complement.** It holds every core lib test outside
  `mcp`, `daemon`, `cli` and `graph`, so a new top-level module needs no edit.
  Small modules share it rather than each adding a near-empty step.
- **A partition check.** `test-sections.sh check` lists the suite once and
  each section once (ignored tests included) and
  `scripts/test-sections-check.py` fails on a test in no section, in two
  sections, or matched by a section but absent from the suite. CI's linux row
  and the release gate run it.
- **CI: steps in one job, not a matrix.** One build step, then one step per
  section with its own timeout and JUnit file. A matrix over sections would
  be 44 jobs, each with its own cold build.
- **`!cancelled()` between sections in CI.** A failing section does not skip
  the ones after it, the same rule as `fail-fast = false` inside a section.
  Locally and in the release gate, `run` stops at the first failing section
  unless given `--keep-going`.
- **The release gate runs `check` and `run all`** instead of
  `cargo test --workspace`, so it uses the same runner and per-test timeout
  as CI.

## Consequences
- Each section's duration is a step duration on the CI run page, and each
  step's timeout bounds one section rather than the build plus the suite.
- Sequential sections lose the overlap between `core-it`'s two-thread group
  and the unit tests; the estimate is that the sum stays about one run.
- Every section and every check listing starts nextest, which executes the
  workspace's test binaries to list them; that per-start cost is paid once
  per section.
- One red section produces the output of every later section too.
- The release machine needs cargo-nextest; the script stops with an install
  hint without it.

## Amendment: sections selected by changed paths (GM-539)
Accepted 2026-10-09 (owner decisions Q1 and Q2 in the design note, section 10).

- **One rule, in `scripts/test-select.sh`.** It maps changed paths to
  sections and prints `full`, `none`, or section names; `test-sections.sh run`
  accepts all three. CI's `select` job and a local verify run call the same
  script. `wire/`, the SDK, cargo and nextest config, CI workflows and the
  selection scripts select `full`, and so does any path no rule names.
- **A plugin change runs that plugin's section and all six core sections**
  (Q1), never the other plugins' sections. Core tests spawn and read every
  plugin, so narrowing core further would trade safety for little time.
- **Pushes to `release-*` are selected like any other push** (Q2). `main`, a
  `workflow_call` (the tag's release gate) and a `workflow_dispatch` always
  run full. A push whose earlier tip is unknown (new branch, force-push) runs
  full.
- **The release gate requires a full run, read from a job, not a summary.**
  ci.yml's `full-suite` job ("Full test suite ran") succeeds only when the
  `select` job said `full`, the platform matrix was not narrowed, and every
  test row passed. `cut-release.sh` looks for that job on a green ci.yml run
  of the release-branch tip and otherwise refuses, naming
  `gh workflow run ci.yml --ref release-<version>`. Job outputs and step
  summaries are not in the REST API; a job's name and conclusion are.

Consequences: a docs-only change skips the `test` job entirely; the
release-branch tip needs a full ci.yml run, which a push that touched only
selected paths does not give, so a release may need one `workflow_dispatch`.
The selection never changes the build (`--workspace` every time), so CI saves
section time, not build time.
