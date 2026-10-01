# GM-472: qualifiedName references through re-exports

Status: implemented (S6), on the segment model of GM-474
([ADR 0015](../adr/0015-qualified-name-segments.md)). The change is in core's
linker (`graph::symbol_links`) and applies to every language that sends
`qualifiedName` keys **with a `keyPath`**. Sections "Problem" to "Cost" are
S1's analysis; S1 split the key string, which the segment model replaces
(Option C below states the rule as implemented).

## Problem, reproduced

`use crate::protocol::types::ControlMessage;` followed by
`ControlMessage::Ping` or `ControlMessage { field: .. }` makes the Rust plugin
send a placeholder addressed by **qualifiedName** in the module the `use`
names: scope `container g_mesh::protocol::types`, key
`protocol::types::ControlMessage::Ping`. That module declares nothing. It has
`pub use g_mesh_wire::*;`, so the declaration is one hop further. The linker
walks re-exports for `name` keys only (`Resolver::resolve`,
`core/src/graph/symbol_links.rs`: "a qualifiedName names a declaration, never
a pass-through"). The edge stays unresolved.

Fixtures (S1 recorded the gap; S6 flipped them to the fixed behaviour):

- `plugins/rust/src/extractor/tests.rs`,
  `gm472_a_member_used_through_a_pub_use_is_addressed_at_the_reexporting_module`
  (real source, run through the Rust extractor). `a` declares
  `struct T { pub f }` and `impl T { fn m }`. `named` has `pub use
  crate::a::T;` and `pub use crate::a::T as Renamed;`, `glob` has `pub use
  crate::a::*;`, `outer` has `pub use crate::named::*;`, which makes a
  two-hop chain. Four `user_*` files use `T { f: 1 }` and `T::m(&t)` through
  each path. The test asserts the emitted rows: re-export nodes with a
  `name` target, and the member placeholders keyed
  `<reexporting module>::<head>.f` / `::<head>::m` in the re-exporting
  module, each with its `keyPath` (`named`, `::T`, `.f`). This output is
  correct per-file and stays as it is.
- `core/src/graph/symbol_links/tests.rs`, `gm472_*` (the same rows through
  the real linker SQL). Each test's doc comment names its control.
  - `gm472_control_a_member_addressed_at_its_own_module_links`: the same
    declarations addressed at `krate::a` link (2 edges), so a failure below
    is the re-export hop and nothing else.
  - `gm472_members_through_a_reexport_link_to_the_declaration`: named,
    alias, glob, and glob-over-named: all 8 edges link (S1: 0).
  - `gm472_a_key_without_a_key_path_is_not_split`: the same rows without
    `keyPath` stay unresolved (an old plugin keeps today's behaviour).
  - `gm472_a_late_declaration_links_through_a_reexport`,
    `gm472_a_late_named_reexport_links_the_members_behind_it`,
    `gm472_a_late_member_links_through_a_reexport_of_its_unchanged_head`:
    the `link_diff` half, one per trigger (S1: 0 links).
  - `gm472_a_glob_cycle_terminates_and_links_only_what_leaves_it`: a pure
    glob cycle links nothing; the same cycle with an exit to `krate::a`
    links through it.
  - `gm472_two_globs_offering_one_head_stay_unresolved`: two heads, only one
    with `m`: nothing links.
  - `link_all_and_link_diff_agree_on_the_same_end_state` carries the GM-472
    rows (head and field in one diff, the method as a later edit, each
    re-exporting module, four users and one without `keyPath`).
- `core/tests/reexport_member_linking.rs`: the Rust fixture plus a Python
  package (`from .mod import *` in `__init__`, `Cls.method()` from a user)
  through the real plugin binaries, wire, write path and linker.

## How re-exports are represented today

There is one wire shape for every language: a `Module` node with
`nativeKind = "reexport"`. Its `name` is what the scope **publishes**, and its
`placeholder_targets` row says what that name **is** (scope + `name` key).
`*` at both ends means a whole-module re-export. Core stores re-exports in
`nodes` plus `placeholder_targets`, with no edges and no separate import
table. A file scope's re-exports are those whose node is in that file. A
container scope's re-exports are those whose `(language, container)` is that
container.

Per-plugin callers below are from g-mesh (`find_callers` on each plugin's
`Emitter::reexport`).

| language | emitted for | target | notes |
|---|---|---|---|
| Rust | `pub use a::T;` / `pub use a::T as X;` / `pub use a::*;` (`Declarer::reexport`, `decls.rs:515,524`) | `container a`, `name T` (or `*`) | node in the re-exporting module's container. `as` keeps the real name in the key and the alias in `name`. `pub(crate) use` also emits one, and the re-export's own visibility is not checked (module doc, "Re-export chains"). A `pub use` of an **external** crate emits none (`PathTarget::ExternalCrate` returns early). `pub use g_mesh_wire as wire;` (a crate alias) is a module path, not an item, and is not a re-export node. |
| Python | `from x import *` (`import_from_statement`) and each `__all__` entry imported from elsewhere (`reexport_dunder_all`) | `container x`, `name` | named re-exports only via `__all__` |
| TypeScript | `export { a } from`, `export { a as b } from`, `export * from` | `file`, `name` | TS sends no `qualifiedName` keys at all (`extract.ts:253`) |
| Go | none (the language has no re-export; `type T = p.T` aliases are not modelled) | | Go's `qualifiedName` keys come from the semantic tier and address the **declaring** package |

Who sends `qualifiedName` keys, and so who is affected:

- **Rust**, structural: `T::m`, `T.f` and `Self::`-less paths via
  `tail_in`/`resolve_type_qualified` (`bodies.rs`). The scope is the
  container the `use` (or the path prefix) names, and the key is
  `qualified_in(container, "T::m")`.
- **Python**, structural: `Cls.method()` with `Cls` imported. The key is
  `Cls.method` in the container `Cls` was imported from (`bodies.rs:718`).
  A package `__init__` that does `from .mod import *` has the same gap.
- **SDK LSP bridge / Rust semantic / Go semantic**: `qualifiedName` in the
  **declaring** file or package. No re-export hop is needed. Unaffected.
- **TypeScript**: none. Unaffected.

## Options

**A. Plugin resolves through the re-export.** Rejected. A plugin sees one
file at a time (Constraints: per-file extraction). The Rust plugin's
`model` knows only the current file's imports and declarations. It cannot see
that `protocol::types` re-exports `ControlMessage` from `g_mesh_wire`, and it
must not read other files. This is exactly why core owns the re-export walk.

**B. Wire hint: the plugin tells core where the head ends.** Add a member
key kind, for example `{member: {head: "T", tail: "::m"}}`. Superseded by
GM-474's `keyPath`, which carries the same information for every
`qualifiedName` key without a new key kind.

**C. Core: split the segments, walk the head by name, re-key the member
(implemented).** For a `qualifiedName` placeholder that finds **nothing** in
its scope and carries a `keyPath` of at least two segments:

1. **Split the segments, never the string.** The **head** is every segment
   but the last; the name walked is the head's last name (`T` in `named`,
   `::T`, `.f`). The **member** is the last segment with its separator
   (`.f`). A key with no `keyPath` (a plugin that sends no paths, or a
   stored path that does not decode) is not split: it is looked up whole,
   as before.
2. **Walk the head by name.** Resolve `(scope, name = head)` with the
   existing breadth-first re-export walk, unchanged: named hops follow
   renames (`Renamed` -> `T`), globs pass the name through, the visited set
   ends cycles, and `MAX_REEXPORT_DEPTH` bounds length. Visibility is checked
   against the original requester. Only a hop of depth >= 1 counts. If the
   scope declares the head itself, the member is simply absent, so stop.
3. **Exactly one head, or stop.** Two visible heads, for example two globs
   offering different `T`s, leave the edge unresolved. Rust rejects such a
   use anyway, and picking the one that happens to have an `m` would be a
   guess.
4. **Re-key and look up once.** The new key is `head.qualifiedName +
   member.sep + member.name` (`a::T` + `::m`), looked up with the existing exact
   `qualifiedName` query in the head's own scope (its container, or its file
   when it has none). Visibility and the kind filter are as today, and
   "exactly one" is required. No further walking: a member is declared with
   its type, never re-exported on its own.

This does not need core to know any language's path convention. Rust's
`<module path>::T` and Python's `Cls` (no module) both work, because the new
key is built from the head's own stored `qualifiedName` and the separator
the plugin put on the member segment, so a field reference (`.f`) can never
re-key onto a same-named method (`::f`). It is also not GM-469's suffix
match: every step is an exact lookup, so "a missing edge beats a wrong one"
holds.

Known limits:

- The impl-in-another-module case. `impl T` written in a module other than
  `T`'s gives methods a different `qualifiedName` prefix. The direct
  (non-re-exported) address has the same limit, so this change does not
  make it worse.
- A head of more than one name past the scope. Only the head's **last** name
  is walked. That is right for Rust (`<module path>::T`, where the module
  path is the scope) and for Python's `Cls.method`. A Python key
  `Outer.Inner.m` through `import *` walks `Inner` alone, so a different
  class's nested `Inner` in the re-exported module could answer. No such
  case exists on g-mesh; a key that needs it would want the plugin to say
  which segment the scope publishes.

### `link_diff` (incremental)

`seeds` → `republished_addresses` → `waiting_placeholders` match
`placeholder_targets.key` by **string equality**, so a `qualifiedName`
placeholder keyed `named::T::m` is never woken by `a::T::m` appearing, or by
a `pub use` of `T` appearing in `named`. Two triggers were added (the
seventh in `link_diff`'s doc):

- `waiting_on_a_head`: for every address `republished_addresses` returns
  (the seeds themselves included), also take the `qualifiedName`-keyed
  placeholders scoped there whose decoded `keyPath` head name is that
  address's name. One scope scan per distinct scope on `idx_targets_scope`'s
  leading columns, filtered in memory. Over-inclusive, like every trigger.
- `heads_of_members`: a declaration in the diff with a `qualifiedPath` of at
  least two segments is a possible member. Its head's `qualifiedName` is
  `qualifiedPath` without its last segment, joined (`a::T`). The head is
  looked up by that exact `qualifiedName` beside the member (one
  `idx_nodes_qualifiedName` query), and the head's `(scope, name)` seeds
  join the name seeds, so the walk and the trigger above wake what waits on
  it. This covers an edit that adds a method while `T` itself is unchanged
  and so not in the diff.

A `*` re-export appearing already wakes every placeholder in its scope
(`whole_scope`), whatever the key kind. `link_all_and_link_diff_agree_on_the_same_end_state`
carries the GM-472 rows, so the two paths are held to the same answer over
102 diff orders.

### Provenance

There is no new `resolvedBy` and no new column. A linked edge is
`resolved = 1` with its original `source` (`tree-sitter`), exactly like a
`name` key linked through a barrel today. Repointing records what an edge
points at, not how it was found. The tools that read these edges
(`find_references`, `find_callers`, `find_callees`, via `graph::traversal`)
need no change: they read any edge whose `toId` is the declaration, so a
linked edge simply appears. The placeholder is kept as today, so a later
diff can re-link.

## Cost and scope on g-mesh's own index

The index was built fresh from this branch (`0344cf3`, GM-450 merged):
`G_MESH_HOME=<tmp> target/debug/g-mesh reindex` with the semantic passes on,
giving 17,716 nodes and 36,359 edges. The reindex took `real 1546.58 user
2619.91 sys 27.85` at load average 336 to 406 (`uptime`), so wall time here
says nothing. Rule C was then simulated read-only over a copy of `index.db`
by a Python re-implementation of the walk (named and glob hops, depth 8,
visited set, public/container visibility, kind filter, exactly-one rule).
The script is not committed. It is a measurement aid, and S2's tests are the
real check.

| unresolved `qualifiedName`-keyed usage edges (all Rust) | 3,360 |
|---|---|
| ... whose scope has no re-export (other causes, out of scope) | 2,870 |
| ... whose scope has at least one re-export | **490** |
| would link under rule C | **375** (233 field `REFERENCES`, 142 method `CALLS`) |
| head found, member not declared at the head | 93 (enum variants, which are not nodes, e.g. `NodeKind::Function` x5; derived `default`) |
| head ambiguous, refused | 16 (`g_mesh::ipc`: `#[cfg(unix)]`/`#[cfg(windows)]` `pub use` of `Stream`/`Listener`/`Endpoint`) |
| head declared in the scope itself | 4 |
| head not found | 2 |

By scope: `g_mesh::protocol::types` 208 (the `pub use g_mesh_wire::*` GM-450/S5
counted as 160), `g_mesh_plugin_sdk` 153, `g_mesh_plugin_sdk::lsp` 73,
`g_mesh::embedding` 34, `g_mesh::ipc` 16, and 6 elsewhere. Every linkable
head is **one** hop away. No chain on g-mesh is longer.

The 16 `cfg` cases are correct refusals: both platform modules are indexed
and nothing structural says which applies. That is a separate question
(cfg-awareness), not a re-export one.

**Lookup cost.** 490 placeholders cost 1,944 extra lookups (name-walk steps
plus one member lookup each). Under memoization they are 194 distinct
`declared` keys and 39 distinct `hops` keys, all on existing indexes
(`idx_nodes_container`, `idx_nodes_qualifiedName`). The simulation took
0.45 s in Python (`user 0.35`) on the loaded machine, where the reindex took
about 1,500 s. In the Rust linker it is a rounding error. Placeholders whose
scope has no re-export pay one keyed `hops` lookup each (2,870 here,
memoized per scope).

### Measured after S6

`g-mesh reindex` (semantic passes and embeddings on) of the tree at
`389c36b`, once with the build before this change (`3ebcf4c`) and once with
it, same plugins (`G_MESH_PLUGIN_ROOTS_OVERRIDE`), separate `G_MESH_HOME`s.
Both indexes have 20,444 nodes and 52,467 edges with identical edge ids.

| | before | after |
|---|---|---|
| `CALLS` resolved / total | 11,443 / 14,182 | 11,591 / 14,182 |
| `REFERENCES` resolved / total | 12,316 / 14,174 | 12,578 / 14,174 |
| unresolved `qualifiedName`-keyed usage edges | 3,492 | 3,082 |

**410 edges newly link**: 261 field `REFERENCES`, 148 method `CALLS`, and 1
method `REFERENCES` (`QualifiedPath::head` passed as a function value). By
scope: `g_mesh::protocol::types` 193, `g_mesh_plugin_sdk` 120,
`g_mesh_plugin_sdk::lsp` 65, `g_mesh::embedding` 32. No edge that was
resolved before is unresolved or points elsewhere after. S1's 375 was
simulated on an older, smaller tree (`0344cf3`, 17,716 nodes).
Wall time (`real 1909.93`/`855.38`, `user 3307.61`/`2254.66`, load average
64 to 168) is dominated by the semantic pass and embeddings under load and
says nothing about the linker.

## Interplay with GM-469

GM-469 (branch `feat/GM-469-qualified-name-suffix-rung`, design only) adds a
**query-time** rung in `find_definition::resolve_symbol_name` that matches a
`qualifiedName` suffix for user queries. That doc already says (its "GM-472"
section) that the two share no code and that the linker must not borrow a
suffix match. This design agrees. Step 1 above *splits* a key at its last
separator, which is the same `<sep><name>` invariant GM-469's census
established. It then resolves both parts exactly. The two meet only in
results: after both land, `find_references("ControlMessage::Ping")` resolves
its anchor via GM-469, and the uses written through `protocol::types` show
up because of GM-472. Neither blocks the other, and neither changes the
other's tests.

## Owner decisions (2026-10-01)

1. **Core rule C, on GM-474's segments.** No string is parsed; a key with
   no `keyPath` behaves as before. Python's `from .mod import *` in an
   `__init__` is covered by `core/tests/reexport_member_linking.rs`.
2. **Ambiguous head = no link**, even when only one of the heads has the
   member.
3. **`link_diff` triggers ship in this task**, so full and incremental
   reindex give the same links.
4. **External-crate `pub use`** (`pub use serde::Serialize`) and crate-alias
   re-exports (`pub use g_mesh_wire as wire`) are out of scope. They are a
   plugin path-resolution question, not a linker one.
