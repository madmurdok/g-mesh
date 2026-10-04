# GM-491: link_diff reopens a link a new provider changes

Status: design (GM-491/S1). Base: `release-3.21.0` at `feab93a`.

## The bug

Fixture: `gm490_python_diffs(true)` in `core/src/graph/symbol_links/tests.rs`.
`pkg/__init__.py` does `from .a import f` and then `from .b import *`. `pkg.a`
and `pkg.b` both declare `f`. `user.py` does `from pkg import f` and calls it.
There is one diff per file. Under the bundled rules Python does not declare
`named_shadows_glob`. So `link_all` follows both rows of `pkg` at one depth,
finds two `f`s, and leaves the call on its placeholder, unresolved.

In the order `__init__`, `a`, `user`, `b`, `link_diff` links the call to
`pkg.a.f` when `user` arrives, because `pkg.a.f` is then the only `f`. When `b`
arrives the edge stays on `pkg.a.f`.

### What the throwaway probe showed

The probe ran against this base and was not committed.

- The wake-up triggers do fire. When `b` arrives, `seeds` gives
  `(pkg.b, f)` and `republished_addresses` walks it up through
  `__init__`'s `*` row to `(Container pkg, f)`. Then `waiting_placeholders`
  returns `{"pending:user.py:pkg:name:f"}`. The placeholder is in `link`'s
  pending set.
- `link` skips the placeholder anyway (`symbol_links.rs:1203-1209`). It looks
  for edges whose `toId` is the placeholder, finds none because the call edge
  was repointed to `pkg.a.f`, and hits `continue; // already linked`. After a
  repoint the edges table has no record of which placeholder an edge came
  from: `UPDATE edges SET toId = ?, resolved = 1` overwrites the only link
  back. So the linker cannot reopen the edge even when it is told to.
- The task title says the triggers miss the new provider. That is wrong. The
  triggers are fine. The gap is the missing provenance and the
  `edge_kinds.is_empty()` skip.
- The four files have 24 arrival orders, and `link_diff` disagrees with
  `link_all` in 12 of them. Six end on `pkg.a.f`: every order where `a`,
  `__init__` and `user` are all in before `b`. Six end on `pkg.b.f`: the mirror
  orders, where `a` comes last. `link_all` leaves the call unresolved in every
  order. The
  acceptance criterion says "every order", so it covers the `pkg.b.f` orders
  too. In those orders the new provider is the *named* row's declaration, not
  a star import. A fix that only reacts to globs would leave six orders
  failing.

## How link_diff works today

`link_diff` (`symbol_links.rs:618-714`) builds a superset of placeholder ids
from seven triggers: the diff's own placeholders, its declarations and
re-exports (`seeds`), and the walk back up re-export chains
(`republished_addresses`, `waiting_on_a_head`, `waiting_placeholders`). It also
adds new usage edges' targets and requesters below new containers. It narrows
that set to pending symbols and hands it to `link`.

`link` (`:1172-1255`) moves only edges whose `toId` is still the placeholder.
The doc comment on `link_diff` states this limit: "any change that makes an
already-linked edge's answer worse (a second candidate appearing, a shallower
declaration shadowing the linked one, a visibility narrowing)" is not covered.
GM-491 is exactly the "second candidate appearing" case.

### How `named_shadows_glob` flows

This comes from `c0c8f6e` (GM-490):

1. A plugin manifest declares `[plugin.reexports] named_shadows_glob`.
2. `daemon::manifest::link_rules` (`manifest.rs:266`) turns it into
   `LinkRules::with_named_shadows_glob`. That function has exactly one
   reference, from `link_rules`, found with `find_references` on
   `LinkRules::with_named_shadows_glob`.
3. `Resolver::hops` tags each re-export row with its own language's flag.
4. `Resolver::walk` drops glob hops at a depth where a flagged named hop
   exists.

Rust and TypeScript set the flag; Python and Go do not.

The fix does not touch this flow. Reopening re-runs `Resolver::resolve`, which
applies each language's rule exactly as `link_all` does.

## Decision

**Record which placeholder each linked edge came from, and let `link` reopen
every placeholder it is handed. A reopened placeholder ends exactly where
`link_all` would leave it.**

### 1. Provenance column

`edges.linkedFrom TEXT NULL` holds the placeholder id an edge was linked from.
It is NULL for an edge that was never linked.

- There is a partial index on it:
  `CREATE INDEX idx_edges_linkedFrom ON edges(linkedFrom) WHERE linkedFrom IS NOT NULL`.
- The column has no foreign key. A placeholder and its usage edges belong to
  the same importer file and leave together. A stale value on a surviving
  edge does no harm: no pending set can contain the deleted placeholder's id,
  so nothing ever looks it up.
- `CURRENT_SCHEMA_VERSION` goes to `"12"`. Every schema change in this codebase
  takes the wipe-and-reindex bump, and `ensure_current` already orders it so
  the new index DDL never runs against an old table (GM-357).

### 2. Write sites

- `link` repoint: `SET toId = ?target, resolved = 1, linkedFrom = ?placeholder`.
- `apply_diff`'s edge upsert (`write.rs:502-525`): `ON CONFLICT ... SET
  linkedFrom = NULL`. A re-sent edge describes what the plugin says now. If the
  plugin re-sends an edge already resolved to a declaration, for example after
  a semantic upgrade, a later reopen of its old placeholder must not move it.
- `language_swap::EDGE_COLUMNS` (`language_swap.rs:58`) gains `linkedFrom`.
  Without it a swap would drop the provenance, and the `EXCEPT` comparison
  would miss a change to it.
- The other edge writers need no change. `containers.rs:393` and
  `graph/imports.rs:343` write non-usage edges or `IMPORTS` edges, which this
  linker never touches, and the column defaults to NULL.

### 3. `link` reopens

For each placeholder in the pending set:

1. Collect two sets: the edge kinds still on the placeholder (as today), and
   the kinds of edges with `linkedFrom = placeholder`.
2. If both sets are empty, skip the placeholder. It has no usages.
3. Otherwise call `resolve` once and, for each kind, choose the target the way
   the code does today (`fitting`, then `sole_non_member`).
   - **One target:**
     `UPDATE edges SET toId = ?t, resolved = 1, linkedFrom = ?p WHERE (toId = ?p OR linkedFrom = ?p) AND kind = ?k AND toId != ?t`.
     This covers both a first link and a move to a different target. The
     `toId != ?t` filter is what keeps `link` idempotent: an unchanged answer
     writes nothing and counts nothing.
   - **No target** (nothing found, nothing of the right kind, or several
     equally good candidates):
     `UPDATE edges SET toId = ?p, resolved = 0, linkedFrom = NULL WHERE linkedFrom = ?p AND kind = ?k`.
     This is unlinking: the edge goes back on its placeholder, which is where
     `link_all` leaves it. Edges still on the placeholder stay as they are.

`LinkSummary.linked_edges` keeps its meaning: edges moved onto a target,
including moves from one target to another. Unlinks go to one `debug!` line
per pass and are not counted. Adding a field would change 58 struct literals
in tests for a number nothing reads.

### Why reopen everything rather than only star-import providers

- **Only globs:** misses the six `pkg.b.f` orders, where the late arrival
  comes in through the named row. Those orders measurably fail.
- **Gate by language** (reopen only where `!named_shadows_glob`): Rust and
  TypeScript would keep their existing `link_diff` disagreements. One example
  is a TS `export { mutate } from "./a"` arriving after a call was linked
  through `export *` to `b.ts`: `link_all` picks `a.ts` and `link_diff` stays
  on `b.ts`. Gating saves no mechanism, because provenance is needed either
  way.
- **Does this change other languages' answers?** `link_all`'s answers do not
  change in any language: on a fresh store every usage edge is still on its
  placeholder, so the new path never fires. `link_diff` changes only where it
  currently disagrees with `link_all`, and it moves to `link_all`'s answer.
  Where the two already agree, re-resolving gives the same target and the
  `toId != ?t` filter writes nothing.
- **Python's own semantics** say the later import binds, so `pkg.b.f`.
  GM-490 chose "no winner" for Python, and that is what `link_all` gives. This
  fix makes the six orders that currently land on `pkg.b.f` end unresolved, to
  match. Making the later import win would need statement order on the rows.
  That is a follow-up, not GM-491.

### Still not covered

Triggers that never fire stay uncovered:

- A declaration deleted outright. Edges into it are deleted with it, as today.
- A visibility narrowing that publishes no scope. `seeds` emits nothing for
  it, so no placeholder wakes.

The `link_diff` doc's "Not covered" paragraph shrinks to these two, plus this
general rule: anything that does not seed a trigger is not reopened.
`link_all`, which now re-resolves linked placeholders too, heals all of these
on its next run.

## Cost

### Which links a new provider may reopen

Exactly the woken placeholders that already have linked edges. Those are the
placeholders the existing triggers return: any placeholder addressed at
`(scope, name)` where the diff seeds that address directly or through
re-export walk-back. A `*` seed wakes every placeholder addressed at the
re-exporting scope and at every scope that republishes it. A Python
`__init__.py` with star imports wakes every importer of the package. Before the
fix each of these cost one indexed query and a `continue`. After the fix each
costs one more indexed query on `linkedFrom` plus one `resolve`.

### Measured on a copy of the g-mesh index

The copy was taken with `.backup` from `~/.g-mesh/projects/959ade85d9a343b1`:
9,457 placeholders, 7,239 of them fully linked. The run used a release build
and a throwaway `#[ignore]` test. Machine load average was 4.32. Test time was
1.78 s; the `real 215 s` / `user 464 s` of the whole command was the release
compile.

| what | time per placeholder |
|---|---|
| `resolve` with the pass's shared `Resolver` (cache warm-up pass / steady state) | 26 µs / 15 µs |
| `resolve` with a fresh `Resolver` each time (statement preparation dominates; paid once per `link` call, not per placeholder) | 233 µs |

### Woken already-linked placeholders per file

These counts assume a file re-send seeds every declaration in it. They count
direct addresses only, over the same copy:

- 268 of 403 files have any.
- Median 9, p90 54, p99 253.
- Maximum 444 (`core/src/storage/write.rs`), then 347 (`wire/src/lib.rs`).

Worst case per edit is therefore about 444 × 26 µs ≈ 12 ms of extra linking.
The typical case is about 0.25 ms. Re-export walk-back adds some placeholders
on top of these counts. All of this is small next to a plugin round trip.

### `link_all`

`link_all` now re-resolves linked placeholders as well. On a fresh store this
costs nothing extra, because every edge is still on its placeholder. On a
store that already has links (`workspace_reindex::rebuild` over a live index,
`plugin_check`) it adds about 9.5k × 26 µs ≈ 250 ms on g-mesh. That run was
measured: 250 ms for the first full pass.

### Other costs

- **Storage:** one nullable TEXT per linked usage edge. g-mesh has about 26k
  resolved linkable edges and placeholder ids average 32 bytes, so about
  0.9 MB plus the partial index, under 2 MB on an 80 MB file.
- **One-time:** the schema bump wipes and reindexes every project on upgrade.
- **Python-heavy corpus:** none is indexed locally. The largest Python share
  is 1,117 nodes, inside g-mesh itself, so this was not measured. Per
  placeholder the cost is the same; the count scales with the number of
  importers of a package whose `__init__` changes.

## Edit map

Line ranges are 1-based, at `feab93a`.

| file:lines | change |
|---|---|
| `core/src/storage/schema.rs:72-75` | bump to `"12"`, with a paragraph citing GM-491 |
| `core/src/storage/schema.rs:283-295` | `linkedFrom TEXT` column on `edges`, its DDL comment, `idx_edges_linkedFrom` partial index |
| `core/src/storage/write.rs:502-525` | upsert writes `linkedFrom = NULL` on insert and on conflict |
| `core/src/storage/language_swap.rs:58` | add `linkedFrom` to `EDGE_COLUMNS` |
| `core/src/graph/symbol_links.rs:1172-1255` (`link`) | linked-kinds query, reopen and unlink branches, `toId != ?t` filter, doc comment (idempotence, provenance) |
| `core/src/graph/symbol_links.rs:618-674` (`link_diff` doc) | "Not covered" paragraph narrows. The equivalence claim becomes "agree whenever every change seeds a trigger". |
| `core/src/graph/symbol_links.rs` module doc (lines 1-275) | one sentence where linking is described as moving edges off a placeholder: linked edges keep `linkedFrom` and a woken placeholder is re-decided |
| `core/src/graph/symbol_links/tests.rs:2765-2826` | tighten the GM-490 Python test to "unresolved in every order"; delete its "Not covered" paragraph |

Read for context only: `Resolver::resolve`/`walk`/`hops` (`:1390-1730`),
`waiting_placeholders` and `republished_addresses` (`:877-1043`), `seeds`
(`:723-760`), `schema::ensure_current` (`schema.rs:583-630`).

### Callers and references this note relies on (g-mesh, project `g-mesh`)

- `find_callers graph::symbol_links::link_diff` returned
  `storage::index_store::apply_and_link` plus tests.
- `find_callers storage::index_store::apply_and_link` returned
  `Writer::apply_diff_linked` and `Writer::apply_file_diff_linked`.
- `find_callers Writer::apply_file_diff_linked` returned 0 rows. This is the
  known Rust receiver-method gap. A grep found the real call sites:
  `watcher/apply.rs:433` (`apply_file_diff_linked`) and `:435`
  (`apply_diff_linked`). So the watcher's per-file path is the hot path for
  the reopen cost.
- `find_callers graph::symbol_links::link_all` returned
  `IndexStore::link_all` plus tests.
- `find_callers IndexStore::link_all` returned `daemon::bulk_index::run_with_progress`,
  `daemon::workspace_reindex::rebuild` and `cli::plugin_check::session::ingest_and_link`.
- `find_references LinkRules::with_named_shadows_glob` returned
  `daemon::manifest::link_rules` only.
- Edge writers were found by grep on the SQL strings, which g-mesh does not
  index: `UPDATE|INSERT INTO|DELETE FROM edges` under `core/src`.

## Test plan

Every control reverts code, never the test.

1. **`gm491_link_diff_agrees_with_link_all_in_every_arrival_order`**. For all
   24 permutations of the four `gm490_python_diffs(true)` diffs, apply each
   diff and run `link_diff`. After *every* step, compare `usage_edges` with
   `link_all` run on a fresh store holding the same prefix of files.
   *Control:* put `if edge_kinds.is_empty() { continue; }` back in `link`.
   The 12 orders listed above fail; this was measured on the base.
2. **`gm491_a_late_star_import_provider_unlinks_the_named_answer`**. The
   reported order (`__init__`, `a`, `user`, `b`): linked to `pkg.a.f` after
   `user`, then back on the placeholder with `resolved = 0` after `b`.
   *Control:* same as test 1.
3. **`gm491_a_late_named_reexport_moves_a_typescript_link`**. Start with a
   barrel that has only `export * from "./b"` and a call linked to
   `b.ts:mutate`. Then `export { mutate } from "./a"` arrives, and under the
   TS rule the call moves to `a.ts:mutate`, as `link_all` gives. This shows
   that a move to a different single target works and that per-language rules
   hold. *Control:* same revert; the call stays on `b.ts`.
4. **`gm491_a_late_external_named_use_unlinks_a_rust_glob_answer`**.
   Reuse the GM-479 shape: a call linked through a glob, then a named `use` of
   an external crate arrives. That shadows the glob and resolves to nothing, so
   the edge goes back on its placeholder. *Control:* in the no-target branch,
   `continue` when `candidates.is_empty()`; the edge stays linked.
5. **`gm491_a_resent_resolved_edge_is_not_reopened`**. Link an edge, then
   re-send it from the importer already resolved to a declaration (a semantic
   upgrade, `resolved = true`). Wake the placeholder with an ambiguous second
   provider and assert the edge is untouched and its `linkedFrom` is NULL.
   *Control:* remove `linkedFrom = NULL` from the upsert's `ON CONFLICT`; the
   reopen moves the semantic edge back onto the placeholder.
6. **Idempotence.** The existing `linking_twice_changes_nothing_the_second_time`
   plus a `link_diff` variant: run `link_all` twice over a linked store; the
   second run reports `linked_edges: 0`. *Control:* drop `AND toId != ?t`; the
   second run counts the edges again.
7. **Language swap keeps provenance.** Extend the existing swap test: after a
   swap a linked edge still has `linkedFrom`, and a following reopen works.
   *Control:* remove `linkedFrom` from `EDGE_COLUMNS`.
8. **Must stay green:** `link_all_and_link_diff_agree_on_the_same_end_state`,
   `gm479_link_all_and_link_diff_agree_whatever_order_the_files_arrive_in`,
   all `gm490_*` tests (the Python one is tightened as listed in the edit map),
   and the schema-version tests.

## Acceptance criteria

- *link_diff and link_all agree in every file arrival order, tested with a
  control:* tests 1 and 2. Test 1 checks every prefix of all 24 orders.
  Without the fix it fails in 12 orders.
- *The fix is documented, including which resolved links a new provider may
  reopen and what that costs:* this note, plus the `link_diff`/`link` doc
  comments and the schema paragraph.

The fix stays within the criteria, with two side effects:

- **Schema bump.** The one-time reindex is the price of provenance. A side
  table with `CREATE TABLE IF NOT EXISTS` would avoid the bump, but links made
  before the upgrade could then never be reopened. The codebase convention is
  the bump.
- **Python answer change.** In six arrival orders the end state changes from
  Python-correct `pkg.b.f` to unresolved. The criterion requires this, because
  `link_all` is the reference. Making Python's later import win is a separate
  task.
