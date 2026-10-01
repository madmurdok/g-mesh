# GM-472: qualifiedName references through re-exports

Status: design (S1), for owner review. The change is in core's linker
(`graph::symbol_links`) and applies to every language that sends
`qualifiedName` keys. No product code changes in this slice.

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

Fixtures. Each asserts **current** behaviour and is labelled so; S2 flips
the `gm472_current_*` ones:

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
  module. This output is correct per-file and stays as it is.
- `core/src/graph/symbol_links/tests.rs`, `gm472_*` (the same rows through
  the real linker SQL):
  - `gm472_control_a_member_addressed_at_its_own_module_links`: the same
    declarations addressed at `krate::a` link (2 edges). This is the control:
    it proves the unresolved rows below fail on the re-export hop and nothing
    else.
  - `gm472_current_members_through_a_reexport_stay_unresolved`: named, alias,
    glob, and glob-over-named give 8 edges and 0 links. In the same index the
    head `T` **by name** through `outer` does link to `a::T`, so the walk
    exists. A qualifiedName key is never given it.
  - `gm472_current_a_late_declaration_does_not_link_through_a_reexport`: the
    `link_diff` half. Declarations arriving after the usage do not wake the
    placeholder.
  - `gm472_a_glob_cycle_terminates_and_links_nothing` and
    `gm472_two_globs_offering_one_head_stay_unresolved` must pass before and
    after the change.

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
key kind, for example `{member: {head: "T", tail: "::m"}}`. Exact, but it is a
protocol change across the SDK, three plugins and conformance, for
information core can already read off the key. Keep it as the fallback if B'
below ever proves ambiguous.

**C. Core: split the key, walk the head by name, re-key the member
(recommended).** For a `qualifiedName` placeholder that finds **nothing** in
its scope:

1. **Split.** Every non-bare `qualifiedName` ends in `<sep><name>`. GM-469's
   census found zero exceptions across five indexes. For a placeholder,
   `name` is the member (`f`, `m`, `method`). Strip `<sep><member>` off the
   key. The last `::`/`.` segment of what remains is the **head** (`T`).
   Splitting on `#` is avoided for the head because Rust raw identifiers
   (`r#type`) contain it, and no language that sends `qualifiedName` keys
   uses `#` as a separator.
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
   <sep><member>` (`a::T` + `::m`), looked up with the existing exact
   `qualifiedName` query in the head's own scope (its container, or its file
   when it has none). Visibility and the kind filter are as today, and
   "exactly one" is required. No further walking: a member is declared with
   its type, never re-exported on its own.

This does not need core to know any language's path convention. Rust's
`<module path>::T` and Python's `Cls` (no module) both work, because the new
key is built from the head's own stored `qualifiedName`, not recomputed. It
is also not GM-469's suffix match: every step is an exact lookup, so "a
missing edge beats a wrong one" holds.

Known limit: the impl-in-another-module case. `impl T` written in a module
other than `T`'s gives methods a different `qualifiedName` prefix. The direct
(non-re-exported) address has the same limit today, so this change does not
make it worse.

### `link_diff` (incremental)

Today `seeds` → `republished_addresses` → `waiting_placeholders` match
`placeholder_targets.key` by **string equality**. A `qualifiedName`
placeholder keyed `named::T::m` is never woken by `a::T::m` appearing, or by
a `pub use` of `T` appearing in `named`. Two additions, both keyed by
`idx_targets_scope`:

- For every republished address `(scope, n)` at depth >= 1 (and every re-export
  seed's own `(scope, published)`), also take the `qualifiedName`-keyed
  placeholders scoped at `scope` whose parsed head is `n`. Use a scope range
  scan, filtered in memory by the same split as step 1.
- An exact seed for a **member** (`a::T::m` new or changed): strip
  `<sep><name>` to get the head's qualifiedName (`a::T`). Look the head up
  by that exact qualifiedName in the member's container, which takes one
  indexed query. Then feed the head's `(scope, name)` through
  `republished_addresses` and apply the rule above, keeping only
  placeholders whose member is this node's `name`.

A `*` re-export appearing already wakes every placeholder in its scope
(`whole_scope`), whatever the key kind. `link_all_and_link_diff_agree_on_the_same_end_state`
gets a GM-472 diff added so the two paths are held to the same answer.

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

## For the owner to decide

1. **Core rule C, generic across languages (recommended) vs. wire hint B.**
   C changes Python's results too (`from .mod import *` in an `__init__`),
   which is intended but is a second language's behaviour change. The
   Python conformance must then gain a case.
2. **Ambiguous head = no link** (recommended), even when only one of the
   heads has the member.
3. **Incremental scope:** ship the `link_diff` triggers in the same task
   (recommended, otherwise an edit leaves the index different from a
   reindex), or full-pass only with a follow-up.
4. **External-crate `pub use`** (`pub use serde::Serialize`) and crate-alias
   re-exports (`pub use g_mesh_wire as wire`) are out of scope. They are a
   plugin path-resolution question, not a linker one.
