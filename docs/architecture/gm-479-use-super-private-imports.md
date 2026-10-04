# GM-479: link `T::f` calls reached through `use super::*`

Status: design (S1). Nothing is implemented. The owner reviews this note
before the implement slice starts.

## The gap

`mod tests { use super::*; fn t() { P::load(1) } }` with `use crate::m::P;`
in the parent. The plugin finds no `P` in `tests`, so it addresses the call
by `qualifiedName` at its own module (`resolve_type_qualified`'s
"declared here, or reached through a glob import" branch,
`plugins/rust/src/extractor/bodies.rs`): scope `…::tests`, key
`…::tests::P::load`, `keyPath` ending in `P`, `load`. The linker then fails at
the first step. `Resolver::through_head` walks `tests` by name `P`, and
`tests` has no hops at all, because the plugin emits re-export rows only for
`pub use` (`Declarer::use_declaration`, `republishes`). A private glob
(`use super::*`) leaves only a file-level `IMPORTS` edge, and so does a
private named `use`. That second omission matters too: even with a glob hop
into the parent, the parent's private `use crate::m::P` is not a row the walk
can follow.

Two row kinds are missing, not one:

1. the child's **private glob** `use super::*`, which is a hop from `tests` to
   the parent;
2. the parent's **private named imports**, which are hops from the parent to
   where `P` really lives.

If the parent declares `P` itself, only (1) is needed.

## Counts on g-mesh (live index, 3.19 binary = release-3.20.0 code)

Snapshot of `~/.g-mesh/projects/959ade85d9a343b1/index.db` (21,579 nodes,
18,264 of them Rust; Rust `CALLS` 12,870, of which 10,370 are resolved). Input:
unresolved Rust `CALLS` edges on a `qualifiedName` placeholder addressed at
the requester's own module (2,114 edges). A module "has `use super::*`" when
its source contains that line; I matched it by file plus module name. A
"predicted link" needs exactly one `Type` head (found in the parent, or one
re-export hop further), and one `Function` whose qualifiedName is
`head::f`. The scripts are in the scratchpad (`gm479/classify*.py`). This is a
heuristic. It is not a linker run.

| class | edges | after GM-479 |
|---|---|---|
| `use super::*`, `T` **declared** (or `pub use`d) in the parent | 162 (119 inline / 43 file-backed `tests.rs`) | link: needs glob row only |
| `use super::*`, `T` **privately imported** by the parent | 424 (307 inline / 117 file-backed) | link: needs glob + private-import rows |
| `use super::*`, parent-side but no single head/member (cfg twins `ipc::Endpoint`, crate alias `wire::QualifiedPath`) | 56 | stay unresolved (out of scope) |
| `use super::*`, `T` not from the parent (`Vec`, `Path`, `HashMap`, …) | 774 (22 look linkable) | stay unresolved |
| `use super::*`, no project `T::f` (derive `Default`, enum variants) | 249 | stay unresolved |
| no `use super::*` in the requester | 447 (5 look linkable) | unchanged |
| keyPath shorter than two segments | 2 | unchanged |

**Expected gain: about 586 `CALLS` edges** (162 + 424). GM-477 estimated
about 556. Almost all of them are test-to-code edges. Placeholders addressed
at an *ancestor* (`super::T::f`, `use super::T` where the parent only imports
`T`) add 1 more. `REFERENCES` edges (variants, fields) gain about 1, since
type names through a glob emit no placeholder at all (`Bound::Nothing`).

## Design (recommended: option A)

### Plugin (`plugins/rust/src/extractor/decls.rs`, `emit.rs`)

`Declarer::use_leaf` already resolves every leaf's container. For a leaf
**without** a visibility modifier:

- **Glob** (`use super::*`, `use crate::x::*`): always emit a `reexport` row
  `*` → `(container, "*")`, with `visibility: container(<this module>)`. It is
  emitted even when the module has no children, because the module itself is
  the requester.
- **Named** (`use a::T [as U]`): emit a `reexport` row `U` → `(a, T)` with
  `visibility: container(<this module>)`, but **only when this module
  declares at least one child module** (`mod x;` or `mod x {}`). Only
  descendants can use such a row. The module itself never does, because the
  plugin already addresses its own uses of an imported name at the import's
  target (`lookup_import`). A leaf module has no descendants, so its rows
  would be dead weight. Child modules are always declared in the parent's own
  source, so this is known per file. However, `mod tests` usually sits *after*
  the `use` lines, so the decision is deferred: collect the private named
  leaves per module during the declaration pass and emit them once the pass
  has seen the whole file.
- `pub use` stays exactly as today (`visibility: file`, unchecked).
  `pub(crate)`/`pub(super)`/`pub(in …) use` also stay unchanged (see
  "Decisions").

`Emitter::reexport` gains a `visibility` argument. Its node id must also
include the publishing module. Today it is `node_id(file, Module,
"<target> as <published>", reexport)`, so two modules in one file that import
the same thing (two inline `mod`s each `use crate::x::T`) collapse into one
row carrying the first module's container. That bug is latent for `pub use`
and would become common with private rows. Fixing it changes every existing
re-export's id once, on the next extraction of its file. The rows are
self-contained, so nothing else points at them.

### Linker (`core/src/graph/symbol_links.rs`)

The descendant rule is the existing `container(c)` visibility rule, applied to
the **hop** as well as to the final declaration. A private `use` in `P` is
`container(P)`, which is visible iff the requester's `fromContainer` is `P`
or has `P` on its parent chain. A sibling (`crate::q`, or `P`'s parent) does
not have `P` on its chain and is refused. This is Rust's own rule for private
imports (E0603), and it needs no new concept.

- `Resolver::hops`: the `REEXPORT` select also reads `n.visibility,
  n.visibilityContainer`. `Hop` becomes a small struct with `restricted_to:
  Option<String>`, which is `Some(c)` only for `visibility = 'container'`.
  The cache stays keyed per `(scope, name)` and requester-independent.
- `Resolver::walk`: when it expands the frontier, it skips a hop whose
  `restricted_to` the requester cannot see. It checks this *before* inserting
  the hop into `visited`, and it uses the same memoized parent chain as
  `visible` (factor `visible`'s `container` branch into `fn sees(&mut self,
  requester, language, container) -> bool`). A row in another language is
  never visible, as in `visible`.
- `through_head` and `resolve` need no change: both reach hops only through
  `walk`, so the rule covers name keys, `qualifiedName` heads and chains of
  any depth (`tests` → glob → `P` → private import → `C`, depth 2).
- Every re-export row in the index today is `visibility: file` (Rust 69,
  Python 3, TS 2 on the live index). Only `container` rows are checked, so
  the change is a no-op for every existing row and for TS's
  `exported: false` barrels, which the module doc's "Re-export chains"
  paragraph protects.
- The module doc's sentence "The re-export statement's own visibility is not
  checked" becomes: "checked only when the row says `container(..)`; `file`
  and `public` rows are walkable by anyone."

The check is against the **original requester**, as for declarations. For a
chain `tests` → `P` (glob) → `P`'s private glob of `Q` → `Q`'s private
import, Rust checks visibility from `P`, and we check it from `tests`, which
is a descendant of `P`. Visible from `P` implies visible from `tests`, so the
check is never looser than Rust's, except for rows private to `tests` or below
it. A glob in `P` cannot reach those.

### Incremental re-link (`link_diff`)

No new trigger is needed. The existing ones cover every way an answer
*improves*:

- A new private row is a re-export in the diff, so `seeds` makes it a named
  seed at its container. `republished_addresses` walks back up through any
  glob row targeting `P` (`tests`' `*`) to `(tests, T)`. `waiting_on_a_head`
  then wakes `tests`' `qualifiedName` placeholders with head `T`, and
  `waiting_placeholders` wakes its name keys.
- A new glob row seeds `(tests, *)`, and that retries every placeholder scoped
  at `tests`.
- `T` (or its member) appearing in `C` is a seed `(C, T)`.
  `republished_addresses` follows `P`'s private row to `(P, T)` and the glob
  to `(tests, T)`. The walk is visibility-blind, so it over-includes.
  `link` filters.
- A new container (`requesters_below_new_containers`) is unchanged.

Not covered, by the module's standing rule ("any change that makes an
already-linked edge's answer worse"): removing the parent's `use`, or turning
`pub use` into `use`. An inline `mod tests` lives in the parent's file, so it
is re-extracted with it and comes back as fresh placeholders. A file-backed
`tests.rs` keeps its linked edge until that file is re-extracted. This is the
same staleness `pub use` removal has today.

### Index size

Counted from sources (`gm479/uses2.py`, regex-level and approximate): private
`use` leaves into this workspace's crates are about 1,630 named and 133 globs
(132 of them `use super::*`). Of the named ones, about 980 sit in a module
with child modules.

- With the child-module filter: **about 1,110 new nodes plus 1,110
  `placeholder_targets` rows**, about 6% of the Rust nodes (5% of all nodes).
- Without the filter (every private `use`): about 1,760 (10%).

Re-export rows are not container members (`containers::membership` excludes
placeholders), so `memberCount` and `requesters_below_new_containers` are not
affected. Lookups stay keyed (`idx_nodes_container`). A failed name lookup in
`P` from *any* requester now also tries `P`'s private hops, and those are
refused cheaply from the memoized chain.

## Interactions

- **GM-477 `unlinkedUsages`** (`core/src/mcp/unlinked.rs`): no code change.
  The probe counts edges still on placeholders. Edges this links move off
  them, so the marker disappears for those anchors (for example
  `EmbeddingPipeline::load` gains its two `search_code::tests` rows and loses
  its marker). Edges the new rule refuses (siblings, cfg twins, aliases) keep
  it. **One existing test breaks by design**:
  `core/src/mcp/unlinked_tests.rs`'s `UNLINKED_USER` uses exactly
  `use crate::m::P; mod tests { use super::*; … P::load(1) }`, and
  `calls_the_linker_left_on_placeholders_mark_the_callers_page_not_exact`
  asserts that call is *not* linked. The implement slice moves that case to a
  still-unlinked shape: a sibling `mod other { use crate::user::*; … }`. That
  sibling is also GM-479's negative case.
- **GM-470 `sole_non_member`**: unchanged and still correct. It judges
  candidates (is this a type member?), not the hop that reached them, so it
  applies at whatever depth a name key now resolves. `through_head` still
  takes exactly one head without a tie-break, so a `tests` with two globs that
  both offer `T` stays unresolved (a missing edge, never a wrong one).
- **GM-472 head walk**: this is what makes it pay off for test modules. The
  depth-0 rule ("a scope that declares the head itself lacks the member")
  still holds, because heads reached through the glob are at depth ≥ 1.

## Alternatives

**A. Private rows plus hop visibility (recommended).**
- Benefit: about 586 edges, including file-backed `tests.rs` (160). The
  descendant rule is Rust's own and reuses `container(c)`. It is a no-op for
  existing rows.
- Risks: about 6% more Rust nodes. Every re-export id changes once. A plugin
  that later sends `container` re-exports for a non-Rust language gets the
  check too, which is intended but needs stating. Roughly GM-472's size: 1-2
  days with tests.

**B. Plugin-only, same file.** When a module has `use super::*` and its parent
is in the same file, the plugin consults the parent's `FileModel` (its
declarations, then its imports) and addresses the call at the parent's import
target, or emits a `Here` edge for a parent-declared type.
- Benefit: no core change, no index growth, no sibling risk (the linker never
  sees a private hop).
- Risks: it covers inline modules only, 426 of 586 (73%). Per-file extraction
  rules out the file-backed 27%, and GM-477's marker stays on those. It
  duplicates Rust glob resolution in the plugin (shadowing by own
  declarations and explicit imports, ambiguity between two globs, where the
  plugin would have to refuse whenever the module has any other glob). It is
  cheaper (about half a day), and it could be a first step if A's index
  growth is unwelcome.

**C. Linker-only, derived from existing placeholders.** Treat `P`'s
`pending_symbol` name-key placeholders (`fromContainer = P`) as `P`'s
imports, with no plugin change. **Rejected.** The index cannot tell a `use`
leaf from a path use `a::T` (which binds nothing in `P`). It also stores the
original name, not the alias, so `use a::T as U` would let a child's `T`
link to `a::T`: a false *resolved* edge. And nothing in the index records
that `tests` has `use super::*`.

**D. Do nothing.** GM-477's marker already makes these answers honest. Cost
zero. 586 test callers stay missing.

## Decisions for the owner

1. A (recommended) or B.
2. Whether to emit private named rows only in modules with child modules
   (recommended, about 1,110 rows) or for every private `use` (about 1,760,
   simpler code).
3. Whether `pub(crate)`/`pub(super)`/`pub(in …) use` should also carry their
   `container(..)` visibility now. Recommendation: not in GM-479. It is
   correct Rust and nearly free in code, but it changes rows that link today
   (a cross-crate requester through a `pub(crate) use` would be refused), so
   it needs its own before/after.

## Tests the implement slice owes (each with a control)

Core fixture tests run the real Rust plugin and linker
(`core/src/mcp/unlinked_tests.rs` `Fixture` / `core/tests/reexport_member_linking.rs` style):

1. `use crate::m::P;` + `mod tests { use super::*; … P::load(1) }`:
   `find_callers(m::inner::P::load)` lists `user::tests::loads` and has no
   `unlinkedUsages`. Variants: a parent-declared `P`; a file-backed `mod
   tests;` + `user/tests.rs`; an aliased `use crate::m::P as Q` with `Q::load`.
   Control: with the plugin not emitting the private rows (in a worktree),
   the test fails.
2. Sibling: `src/other.rs` with `use crate::user::*;` (and the inline
   `crate::user::P::load(..)`) calling `P::load`: unresolved, marker present.
   Control: with `walk`'s hop check removed, this test fails (it links).
3. Plugin: the private glob and the parent's private named rows carry
   `visibility: container(<module>)`. A leaf module's private named `use`
   emits no row. Two inline modules importing the same item get two rows.
4. `link_diff`: the parent's `use` arriving in a later diff than the
   file-backed `tests.rs` links the call, and agrees with `link_all`
   (`link_all_and_link_diff_agree_on_the_same_end_state` style).
5. Measurement on g-mesh: Rust `CALLS` resolved before/after (expect about
   +586 over 10,370), node count before/after (expect about +1,110), and the
   number of anchors carrying `unlinkedUsages` on `find_callers` (105 at
   GM-477).

## g-mesh calls used (served index, 3.19 binary)

- `select_project("g-mesh")`.
- `find_callers(Resolver::walk)`: `Resolver::resolve`, `Resolver::through_head`.
  `find_callers(Resolver::through_head)`: `Resolver::resolve` only.
  `find_callers(Resolver::hops)`: `Resolver::walk` only. All complete, so the
  hop check in `walk` covers every path.
- `find_references(REEXPORT_NATIVE_KIND)`: `symbol_links::is_reexport`,
  `protocol::conformance::PLACEHOLDER_NATIVE_KINDS`,
  `graph::queries::NON_DECLARATION_NATIVE_KINDS`, `mcp/unlinked.rs`,
  `graph/containers/tests.rs`. Uses inside `symbol_links`' SQL `format!`/`params!`
  are macro arguments, so they are not edges. They were read directly.
- `find_references(NON_DECLARATION_NATIVE_KINDS)` and
  `find_callers(declaration_only)`: `graph::queries` lookups,
  `containers::membership`, `Resolver::new`, `heads_of_members`. Re-export
  rows stay out of all of them.
- `find_references(PLACEHOLDER_NATIVE_KINDS)`: `conformance::placeholder_target_violation`,
  `plugin_check::checks::is_placeholder`. These are target-required checks
  only, with no visibility rule.
- `find_callers(extractor::emit::Emitter::reexport)`: ambiguous (Python and
  Rust). By id for Rust: `Declarer::reexport`, whose only caller is
  `Declarer::use_leaf` (`find_callers`), whose only caller is
  `Declarer::use_declaration`.
- grep was used for one-file excerpts, the `use super::*` fixture in
  `unlinked_tests.rs` and the plugin tests that assert re-export rows
  (`extractor/tests.rs:372`, `:950`).
