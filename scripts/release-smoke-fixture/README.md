# Release smoke fixture

Used by `scripts/release-smoke.sh` (called from the `build` job in
`.github/workflows/release.yml`) to run the freshly staged artifact for real:
`g-mesh reindex` against this directory, on the artifact's own runner.

One file per bundled plugin (go, python, rust, typescript), each shaped like
`plugins/typescript/conformance/project/src/math.ts` - two free functions in
one file, the second calling the first - deliberately the simplest case that
still produces a real node and a real same-file `CALLS` edge, needing no
cross-file or cross-package resolution and no semantic pass.

## Why one file per language, not one

`daemon::bulk_index::run` spawns every plugin the running binary discovers,
unconditionally, and fails the whole walk if any one of them fails to spawn
or exits non-zero (`core/src/daemon/bulk_index.rs`, `run`/`walk_one_language`)
- so a fixture containing only one language would *already* prove all four
plugins spawn and exit cleanly; that part of "run every plugin for real" is
free regardless of what this fixture contains.

What it does not cover: a plugin that spawns fine, is handed zero files of
its own language (because none are in the fixture), and exits 0 having
correctly done nothing. That is indistinguishable, from the outside, between
"nothing to do" and "broken but silent" - the very case `g-mesh reindex`'s
aggregate `nodes`/`edges` count cannot separate out either (see
`scripts/release-smoke.sh`'s own header for that limitation). Giving every
language one real file it must actually parse is what turns "the process
exited 0" into "the process extracted a symbol", for all four, not just
whichever language happens to be present.

## What this deliberately does not prove

`g-mesh reindex`'s own output has no per-language breakdown (`BulkIndexSummary`
is one aggregate struct - see `core/src/daemon/bulk_index.rs`), so
`scripts/release-smoke.sh` can assert "the project's total nodes/edges are
non-zero" but not "each of the four languages individually contributed at
least one node". A single broken extractor that silently emits nothing for
its own file, while the other three still produce a non-zero total, would not
fail this check. Catching that would need either a per-language count from
`g-mesh` itself (it has none today) or querying `index.db` directly, which
needs `sqlite3` on all three runner OSes - not confirmed available on the
Windows image, so not attempted here. Flagged rather than silently accepted.
