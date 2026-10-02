# GM-477: `find_callers(EmbeddingPipeline::load)` answers "complete" with no rows

Status: diagnosis (S1), then option (b) implemented (S2, see
"Implementation" below). The brief asked for
`docs/design/GM-477-...`; `docs/design/` does not exist and task notes live in
`docs/architecture/gm-NNN-<slug>.md`, so this note follows that convention.

## Verdict in one paragraph

The miss GM-464/S3 saw is a **stale serving binary**, not a 3.18 bug. The
daemon serving g-mesh's own index runs `g-mesh/target/release/g-mesh`, built
Sep 29 (`--version` prints `3.17.0`), with a rust plugin built the same day.
That predates GM-472 (re-export links, merged in `9573f48`, the very commit
GM-464/S3 cited). With the 3.18/3.19 code, the three callers link. What stays
true after a rebuild is the **class** of the bug: when a call edge points at a
placeholder that never linked, `find_callers` on the real declaration cannot
see it, and the answer still comes back `hasMore: false` with no marker. On
3.19 code, about 580 `T::f(..)` calls on g-mesh's own sources are still in that
state, nearly all of them in `mod tests { use super::*; }`.

## Repro on g-mesh's own index (served, 3.17 binary)

- `find_callers(symbol_name: "EmbeddingPipeline::load")`: `resolvedBy:
  semanticNeighbours`, "Nothing is named ..." (the 3.18 suffix rung is absent
  from the 3.17 binary, so the bare form does not even anchor).
- `find_definition("embedding::pipeline::EmbeddingPipeline::load")`: id
  `ba00ea1f…`, `core/src/embedding/pipeline.rs:202`.
- `find_callers(symbol_id: ba00ea1f…)` and `find_references(ba00ea1f…)`:
  `results: []`, `hasMore: false`, `allUnresolved: false`. **Reproduced.**
- `find_callees("cli::reindex::reindex")` and `find_callees("daemon::run")`:
  each lists the call, but to a placeholder:
  `g_mesh::embedding::embedding::EmbeddingPipeline::load`, `kind: Module`,
  `resolved: false`. So the edge is **neither missing nor pointing at another
  `load`**: it points at a pending placeholder addressed by qualifiedName in
  the re-exporting module `g_mesh::embedding` (`pub use
  pipeline::{.., EmbeddingPipeline}`, `core/src/embedding/mod.rs:27`). The
  doubled segment is the normal placeholder name (`<container>::<key>`), not a
  bug.
- Read-only SQL on the live DB (`~/.g-mesh/projects/959ade85d9a343b1/index.db`):
  `placeholder_targets` has **no `keyPath` column**, so the index was written
  before GM-474/GM-472. All 5 `…EmbeddingPipeline::load` CALLS edges have
  `resolved = 0`. The `g-mesh-wt-gm469` index (keyPath present, pre-GM-472
  linker) shows the same 5 unresolved edges.

## Repro on a fixture, current code (worktree, 3.19.0 = 3.18 code)

The throwaway test (kept in the scratchpad, not on the branch) copied
`core/tests/reexport_member_linking.rs` and ran the real rust plugin, bulk walk
and linker. Four modules re-export `P` from `inner` as `pub use inner::{Q, P}`,
`pub use inner::P`, `pub use self::inner::P` and `pub use crate::m::inner::P`.
Each user calls `P::load` three ways: top-level `use crate::m::P`, the inline
path `crate::m::P::load(..)`, and a fn-local `use`.

| call form | 3.19 code | 3.19 code, `through_head` disabled (control) |
|---|---|---|
| top-level `use` + `P::load` (4 re-export shapes) | links (4/4) | unresolved, `krate::m::m::P::load` (0/4) |
| inline `crate::m::P::load` (4 shapes) | links (4/4) | unresolved (0/4) |
| `use crate::m::inner::P` (no re-export) | links | links |
| fn-local `use crate::m::{Q, P as P2}` | unresolved (separate gap) | unresolved |

The control was made by replacing the `through_head` arm in
`Resolver::resolve` (`core/src/graph/symbol_links.rs:1300`) with
`Ok(Vec::new())`, then reverted. It reproduces the live index's exact shape,
which names the step: the **linker**, through the GM-472 head walk in
`Resolver::resolve` → `Resolver::through_head` (`symbol_links.rs:1293-1302`,
`1360`). Extraction (`plugins/rust/src/extractor/bodies.rs`) is correct
(GM-472 doc: per-file output unchanged), and the query
(`core/src/mcp/find_callers_callees.rs`, not `find_callers.rs`) only reads
edges whose `toId` is the anchor, which is correct for what it was given.

The same throwaway test walked a copy of the worktree's own `*.rs` and
`Cargo.toml` files with 3.19 code:

- `cli::init::init`, `cli::reindex::reindex` and `daemon::run` →
  `embedding::pipeline::EmbeddingPipeline::load`, `method`, `resolved = 1`.
  **Acceptance criterion 2 holds on current code once the served binary is
  rebuilt.**
- Still silent: the two callers in `mcp::search_code::tests` (the key is
  `mcp::search_code::tests::EmbeddingPipeline::load`, through `use super::*`).
  `pipeline.rs:918` (inside `assert_eq!`) produces no edge at all, because
  macro arguments are not parsed: that is a separate, known gap.

Hypotheses:
- Call through an import/re-export: **ruled in** for 3.17, fixed by GM-472.
- Edge resolved to a different node: **ruled out**. It resolves to no node
  and stays on its own placeholder.
- Anchor resolved to the wrong `load`: **ruled out**. The id anchor is
  `pipeline.rs:202`. `rerank.rs:151` and `model.rs:293` are different
  `load`s and get no edges from these sites.

## Counts on g-mesh's own sources

Method: unresolved CALLS edges whose placeholder has a `qualifiedName` key.
A call counts as "linkable" when the key's last two segments are `T::f` with
`T` capitalised and the project declares a Function whose qualifiedName ends
in that same `T::f`. This is a name heuristic, so the counts are rough. Rust
files only. "3.17" is the live DB; "3.19" is the scratch walk over the same
files.

| | 3.17 (live) | 3.19 code |
|---|---|---|
| CALLS resolved / unresolved (all languages, live) | 11692 / 2815 | 7279 / 2346 (rust only) |
| unresolved, qualifiedName key | 2345 | 2167 |
| `T::f`, no project decl (std/external, mostly correct to leave) | 1589 | 1584 |
| `T::f`, exactly one project decl: non-test / test | **72** / 596 | **13** / 499 |
| `T::f`, several project decls: non-test / test | 6 / 65 | 6 / 65 |

The ~583 linkable-looking misses that remain on 3.19, by cause:
- **556 in `mod tests { use super::*; }`.** The glob pulls in the parent's
  *private* `use` items. The plugin emits re-export nodes only for `pub use`
  (`Declarer::use_declaration`/`use_leaf`, `decls.rs:495-540`), so the linker
  has no path from `…::tests` to the parent's imports.
- **9 cfg twins**: `#[cfg(unix)] pub use unix::{Listener, Stream, Endpoint}`
  and `#[cfg(windows)] pub use windows::{..}` (`core/src/ipc/mod.rs:48,53`).
  Two heads offer the name, so the linker refuses by design
  (`gm472_two_globs_offering_one_head_stay_unresolved`). This covers
  `daemon::run → ipc::Listener::bind` and `shim::* → ipc::Stream::connect`.
- **9 through a crate alias**: `pub use g_mesh_wire as wire`
  (`plugins/sdk/src/lib.rs:109`). `wire::QualifiedPath::root` from the
  plugins crosses workspace crates.
- 8 in `core/tests/*` and 1 via `use crate::embedding::cache::{self, ..}`.

## Options

### (a) Link the calls

Teach the plugin and linker to follow `use super::*` into the parent's private
imports. One way: emit an import-scoped re-export for private `use` items
that only child modules may walk, and have the walk honour that restriction.
- Cost: plugin `decls.rs` (use emission) plus `symbol_links` (`walk` and
  `through_head` gain a visibility rule for private imports), plus `link_diff`
  triggers. Roughly GM-472's size: 1-2 days with tests.
- Gain: about 556 test-side edges. Almost no non-test edges (13 left), and it
  does nothing for cfg twins or the crate alias.
- Risk: the linker today does not check a re-export's own visibility
  (`symbol_links` module doc). Emitting private imports as re-exports
  without that check would let sibling modules link through them: false
  *resolved* edges, which is worse than the current miss. Index size grows
  (one node per private `use` leaf).

### (b) Mark the answer as not exact

In `handle_callers_in` (`core/src/mcp/find_callers_callees.rs:349`), and the
same for `find_references`, also count unresolved CALLS/REFERENCES edges to
`pending_symbol` placeholders whose `name` equals the anchor's name and, for a
`T::f` key, whose penultimate key segment equals the anchor's parent type
name. Report a count and files, like the existing `excludedReferences`
(`find_callers_callees.rs:281`), with a hint that these may be callers g-mesh
could not link. Update the server guidance that today says a
`hasMore: false` page for qualified-type calls "is exhaustive".
- Cost: one SQL probe per call plus the response field, hint text,
  guidance wording and tests. About half a day. Placeholder nodes carry the
  bare `name` (`load`), but `nodes.name` has no index (only
  `filePath`/`qualifiedName`/`container`), so either add one or accept a scan
  over the pending nodes.
- Gain: covers every unlinked class at once: `use super::*`, cfg twins,
  crate aliases, fn-local `use`, and an index left by an older plugin. An
  empty page stops looking complete when it is not.
- Risk: false positives from an unrelated `T::f` with the same names (the
  `T` match narrows it). Agents may grep more often, and a noisy marker
  teaches agents to ignore it. It does not add the missing rows.

## Recommendation

1. **Ops, now:** rebuild `g-mesh/target/release` (core + plugins) and restart
   the serving daemon so the index is rebuilt with keyPath. That alone makes
   `find_callers(EmbeddingPipeline::load)` list `daemon::run`,
   `cli::reindex::reindex` and `cli::init::init` (verified on a scratch walk).
2. **GM-477's fix: (b).** It is cheap and general, it covers the cfg-twin and
   alias classes that (a) cannot, and it addresses the actual harm: a
   *false complete* answer. Put (a) in a separate backlog task, scoped to test
   modules, if test-caller coverage matters.
3. Regression test for (b), which must fail when the fix is reverted: a
   fixture where `mod tests { use super::*; }` calls `P::load`, plus a
   cfg-twin re-export. Assert that `find_callers(P::load)` carries the new
   marker with count ≥ 1. Control: the same fixture with the call moved to a
   top-level `use`, where the marker must be absent and the row present.

The owner decides between (a) and (b).

## Implementation (option b)

The owner chose (b). Linking through `use super::*` (option a) is a separate
backlog task.

- `core/src/mcp/unlinked.rs`: `probe` finds usage edges still on
  `pending_symbol` placeholders of the anchor's language whose bare `name` is
  the anchor's. A key of two or more segments counts only when its
  second-to-last segment equals the anchor's; a one-segment key (or one with
  no `keyPath`) counts only when the anchor is not a member of a `Type`.
  Segments come from `keyPath`/`qualifiedPath`; no display string is split.
- `find_callers` (CALLS) and `find_references` (CALLS, REFERENCES,
  SUPERTYPE_OF) carry the result as `unlinkedUsages: {count, files,
  filesTruncated?, hint}`, shaped like `excludedReferences`. The field is
  absent when there is no candidate. `count` is uncapped; `files` is capped at
  20. The page bound reserves the field's bytes only when it is present.
- Guidance P4 now reads "bare function calls and this/super/qualified-type
  calls have no such gap, and for those `hasMore: false` without
  `unlinkedUsages` is exhaustive". To stay under the 1,900-byte ceiling, the
  wait sentence in P4 drops "before answering" (worst case 1,890 bytes).
  `select_project` serves the same rendering.
- The probe does not filter on `edges.resolved`. The linker sets
  `resolved = 1` only while repointing an edge onto a declaration
  (`graph/symbol_links.rs:1146`), but ingest stores a plugin's own bit as sent
  (`storage/write.rs:481-498`; the "never `resolved: true` onto a
  placeholder" rule is checked only by `g-mesh plugins check`,
  `cli/plugin_check/checks.rs:395`). An edge still on a placeholder is
  unlinked whatever its bit says.
- Index: `idx_nodes_pending_name ON nodes(name) WHERE nativeKind =
  'pending_symbol'`, created by `schema::apply` on any current-version index,
  so no version bump or reindex. Without it the probe scans every node of the
  language on every call.

Measured (Rust files of this worktree, scratch walk, 14,809 nodes, 8,474
non-placeholder anchors, release build):

| | p50 | p99 | max |
|---|---|---|---|
| probe, no index (load average 200-350) | 6.2 ms | 12.1 ms | 173 ms |
| probe, partial index (load average 10-96) | 0.04 ms | 7.7 ms | 12.7 ms |

On a snapshot of the live index (17,137 nodes), the bare candidate query for
a name with no placeholders took 6-7 ms as a scan and under 0.1 ms with the
index.

Anchors carrying the marker on that walk: 0 before (the field did not exist),
105 on `find_callers` and 151 on `find_references` after.
`EmbeddingPipeline::load` (2, the `search_code` tests) and both
`Listener::bind` twins (6 each) are among them. 27 of the 105 come from
one-segment keys in `core/tests/*` crates (`use common::*`), and 15 of those
are the 15 same-named `wait_for` helpers, each flagged by the same calls.

## g-mesh calls used (served index, 3.17 binary)

- `select_project("g-mesh")`.
- `find_callers(symbol_name: "EmbeddingPipeline::load")`: semanticNeighbours,
  no anchor.
- `find_definition("EmbeddingPipeline::load")`: same; `find_definition(
  "embedding::pipeline::EmbeddingPipeline::load")` and by position
  `pipeline.rs` 201:11: id `ba00ea1f…`.
- `find_callers(symbol_id)` / `find_references(symbol_id)`: `[]`, complete.
  This is the bug.
- `find_callees("cli::reindex::run")`: only `reindex`.
  `find_callees("cli::reindex::reindex")` and `find_callees("daemon::run")`:
  the load call goes to an unresolved placeholder (as do
  `ipc::Listener::bind` and `candidates::Limits::default`).
- `get_dependencies(core/src/cli/init.rs, Outgoing, 1)`: imports
  `g_mesh::embedding` (the re-exporting module), not
  `g_mesh::embedding::pipeline`.
- `find_callers("graph::symbol_links::Resolver::resolve")`: only
  `graph::symbol_links::link`. `find_callers("mcp::find_callers_callees::handle_callers_in")`:
  `handle_callers`, `GMeshMcpServer::find_callers`. These are the entry points
  (a) and (b) would touch.
- grep was used for the ground-truth call sites (`EmbeddingPipeline::load(`:
  `daemon/mod.rs:338`, `cli/reindex.rs:109`, `cli/init.rs:210,239`, plus 3 in
  tests), because g-mesh's answer was the thing under test.
