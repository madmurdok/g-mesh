# g-mesh project instructions

## Code comment rule

A code comment keeps only two things:

1. **What the code does**, when that isn't obvious from the code itself.
2. **An invariant a change must not break.** Examples:
   - lock order: plugin lock before connection lock.
   - foreign keys are OFF in production, so a write for a deleted node
     leaves an orphan row (don't "fix" this locally — it's relied on
     elsewhere).
   - the `indexed_files` baseline is written only after a successful
     reindex.

Everything else goes somewhere other than the comment, never into it:

- **Change history** ("GM-394 did X, GM-396 changed it because...") goes to
  commit messages and `git blame`, not to a comment. A reader wants "why is
  this true", not "who touched this and when" — that's what history is for.
- **Decisions** (why this approach over another, what was considered and
  rejected) go to an ADR (`docs/adr/`), not to a comment. Link to the ADR
  at module level, or at the one place in the code where the decision is
  actually embodied — never per function. A decision that shaped ten
  functions gets one link, not ten.

**A comment that mixes these is split, not kept or dropped whole.** Keep
the sentence that states the present invariant, rewritten in the present
tense with no ticket id ("Unchanged by GM-294: X is checked before Y"
becomes "X is checked before Y"). Drop the history. If the "because" is a
design choice (why this and not an alternative), it goes to an ADR; if it
only says what breaks when the invariant is violated, it stays as part of
the invariant.

This rule **overrides "match the surrounding comment density."** A file
full of history comments is not a style to match; it's exactly what this
rule removes. Don't add a new history comment because the ones around it
are history comments.

**Cleanup is file by file, inside a refactor ticket** — never a bulk pass
across the codebase. A ticket that touches a file for another reason is a
reasonable place to also clean that file's comments; a ticket whose only
purpose is comment cleanup should still land as one file (or a small,
related group) at a time, not as a single sweeping diff.

## Reading large files

Read excerpts, not whole large files: `grep -n` to find the lines that
matter, then read with an offset/limit (or `sed -n`) around them. Do not
read a whole large file end to end when a targeted read answers the
question — a full read early gets carried through every later turn.

## Architecture decisions

See [`docs/adr/README.md`](docs/adr/README.md) for the ADR index —
existing design decisions (indexed from `docs/architecture/`) and new ones
going forward.

## Testing

The suite runs as named sections (`scripts/test-sections.sh`,
[ADR 0030](docs/adr/0030-test-sections.md)). Which command when
([ADR 0031](docs/adr/0031-local-test-runs.md)):

- **Verify slice:** the sections its change can affect, diffed against the
  release branch the task branched from. Lib tests run in one process; the
  heavy tests in `.config/nextest.toml`'s `local` profile are skipped.

  ```
  scripts/test-local.sh --base <release-branch>
  ```

- **Batch end:** push the release branch and run the full CI on its tip
  (GM-675); its failures become tasks in the batch.

  ```
  git push origin release-<v>
  gh workflow run ci.yml --ref release-<v>   # a push after the first is partial; a dispatch is full
  gh run watch <run id> --exit-status        # one background wait, no polling
  ```

  Every section on four platforms, heavy tests included, plus the workspace
  doctests and `cargo test -p g-mesh` on Linux. The heavy-list report reads
  that run's JUnit: `gh run download <run id> -n junit-aarch64-apple-darwin
  -D <dir>`, then `scripts/test-heavy-report.py .config/nextest.toml
  <dir>/*.xml`. `scripts/test-local.sh --full` stays available to run by hand.

- **Release gate:** `scripts/cut-release.sh` runs no suite. It tags only when
  ci.yml passed a full run on the release branch tip that `main` merged (same
  tree), waiting for a run still in progress (`--ci-timeout`, default 90 min).

- **CI** runs `scripts/test-sections.sh run` per section under `--profile ci`.

`test-select.sh` prints `full`, `none` or section names (with `--narrow`,
also `core-spawns-<plugin>`: the core tests that start that plugin, from
`scripts/test-plugin-spawns.txt`), and counts uncommitted and untracked
files too. Any path it has no rule for selects
`full`. A full CI run (batch end, release gate) always runs every section.

Core's integration tests are two binaries (GM-623): `core/tests/scripts/`
for a test whose subject is a file under `scripts/`, `core/tests/it/` for
every other one. They run as sections `core-scripts` and `core-it`. A new
file there needs a `mod <file>;` line in that directory's `main.rs`; `core/tests/it/layout.rs` fails on a missing one and
on any new top-level `core/tests/*.rs`. Run one file's tests with
`cargo nextest run -p g-mesh --test it -E 'test(/^<file>::/)'` (`--test
scripts` for a script test); nextest names them `<file>::<test>`.
