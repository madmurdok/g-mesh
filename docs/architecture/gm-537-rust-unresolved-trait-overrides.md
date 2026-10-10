# GM-537: overrides for a Rust trait the structural tier cannot resolve

Status: design (GM-537/S1), for owner review. No production code. Base:
`release-4.3.0` at `404ed48`. Context: `gm-502-override-callers-field.md`
D3/D4, `plugins/rust/README.md` "What it does not see".

## 1. Problem, for someone with no context

`find_callers` on a Rust trait-impl method (`<Megaphone as Loud>::speak`)
carries an `overrides` field naming the trait method (`shapes::Loud::speak`),
so a session knows that calls through `dyn Loud` / `T: Loud` sit on the trait
method's page instead. For Rust the field is `declared` (GM-502 D3): core
reads the method's outgoing `SUPERTYPE_OF` edge to a `Function`, and only the
Rust plugin's structural extractor emits that edge
(`bodies.rs` `member_supertypes`), and only when the impl's trait clause
resolves. GM-502's verify found a case where it does not, and the field is
missing even after rust-analyzer's pass.

## 2. Confirmed cause (fixture run)

Scratch copy of `plugins/rust/conformance/project` with five extra modules in
`crates/beta/src/cases/`, each `impl <trait> for <its own struct>` with one
`fn speak`, plus the existing `Megaphone`:

| Case | How `Loud` reaches the impl |
|---|---|
| `Megaphone` (`main.rs`) | `use alpha::prelude::*;` (glob of a module that re-exports `Loud`) |
| `GlobDirect` | `use alpha::shapes::*;` (glob of the declaring module) |
| `NamedReexport` | `use alpha::prelude::Loud;` (named import through the re-export) |
| `NamedDirect` | `use alpha::shapes::Loud;` |
| `ReexportPath` | `impl alpha::prelude::Loud for …` |
| `DirectPath` | `impl alpha::shapes::Loud for …` |

Run with `g-mesh plugins check` and two `[[implementations]]` entries
(`shapes::Loud`, type level; `shapes::Loud::speak`, method level, i.e. the
edge `overrides` reads). Structural = a copy of `plugin.toml` with
`semantic_pass/sweep/prepare = false`; semantic = the shipped manifest with
rust-analyzer 1.97.1.

| Case | structural: type edge | structural: method edge | after rust-analyzer: type | after rust-analyzer: method |
|---|---|---|---|---|
| Megaphone (glob of re-export) | no | **no** | yes | **no** |
| GlobDirect (glob of declaring module) | no | **no** | yes | **no** |
| NamedReexport | yes | yes | yes | yes |
| NamedDirect | yes | yes | yes | yes |
| ReexportPath | yes | yes | yes | yes |
| DirectPath | yes | yes | yes | yes |

So the cause is narrower and different from the D4 wording: **a re-export is
not the problem, a glob is.** Any trait whose only route into the impl's
module is a glob import (`use x::*`, including `use super::*`) gets neither
edge structurally; rust-analyzer's implementation sweep then adds the
type-level edge (`Megaphone -> Loud`) but nothing emits the method-level one.
A named import through a `pub use` works because core's linker walks the
head of a `qualifiedName` key through re-exports
(`symbol_links::Resolver::through_head`).

Code path (verified): `impl_item` (`bodies.rs:318-367`) ->
`resolve_supertype` (`:1548-1561`) -> `resolve_path` -> `resolve_bare`
(`:1088-1158`): `Loud` is not declared in the module and not imported by
item, so the `None` arm with `want == Type` returns `Bound::Nothing`
(deliberately: `Vec`/`String` must not become questions). `supertype_edge`
(`:1433`) then emits nothing (`emit` drops `Nothing`), and
`member_supertypes` (`:1574-1604`) returns early unless the bound is
`Here`/`There`.

Timing (machine under load): fixture runs `real 3.41 / user 0.81 / sys 0.13`
(structural) and `real 64.79 / user 14.41 / sys 5.27` (semantic), load
average 590-720 throughout.

## 3. Options

### A. Structural: a bare trait name under a glob is a `name` key into the module (recommended)

In `resolve_supertype` only (trait clauses and supertrait bounds), when the
path is one segment, resolves to `Bound::Nothing`, is not a type parameter,
is not imported by item, and the module has a glob `use`
(`FileModel::has_glob`), return `Bound::There` with a `name` key `Loud` in
the impl's own module. Nothing else changes: `supertype_edge` emits the
type-level placeholder edge and `member_supertypes` addresses `Loud::speak`
as a `qualifiedName` key in the same module (the existing
`resolve_type_qualified` -> `tail_in` path, with `key_path`).

Core already resolves both: the module's glob is a private `*` re-export row
(`decls.rs` `use_leaf`, `LeafKind::Glob`), `Resolver::walk` follows it for a
`name` key, and `through_head` walks the head `Loud` of `Loud::speak` the
same way and looks up the member in the declaring container.

**Prototyped** in a throwaway worktree (11-line patch in `resolve_supertype`,
reverted; worktree removed): structural-only, `Megaphone` and `GlobDirect`
gain both edges, so all six cases match on both entries; the semantic run's
sets are unchanged (no extra rows); the shipped conformance run passes
(46 passed, 0 failed).

- Benefit: fixes it at the source, from the first index, without
  rust-analyzer; exact (`<Megaphone as Loud>::speak` names `Loud::speak`
  only); no core, wire or bridge change; reuses the ambiguity rule core
  already has: two globs that both offer `Loud` give two candidates, and
  `walk`/`through_head` refuse ("a missing edge beats a wrong one"), which is
  the README's "two globs make the placeholder ambiguous" concern already
  handled; a named import shadows a glob (`named_shadows_glob = true`).
- Risk: a trait from outside the project under a glob (`impl Display for X`
  in a `mod tests { use super::*; }`) now costs one placeholder node and one
  unlinked edge per clause, plus one member placeholder per method, where it
  cost nothing before. Bounded to trait clauses, not every bare type, so far
  below GM-314's 2,052 excluded names; the implement slice measures it on
  g-mesh's own tree. It also removes the structural witness that only the
  semantic tier finds `Megaphone` (Q2).
- `CURRENT_INDEXER_VERSION` bump: the extractor emits new edges.

### B. Semantic: rust-analyzer sweep over trait methods

Add the trait-method `nativeKind` to `implementation_kinds`; the bridge asks
`textDocument/implementation` on each trait method and records
`SUPERTYPE_OF` from each implementing `fn` to it. `record_implementor`
(`plugins/sdk/src/lsp/bridge.rs:2694`) refuses non-`Type` implementors today,
so it needs a "Function anchor accepts Function implementor" rule.

- Benefit: language-server truth; also covers macro-generated or otherwise
  unparseable trait paths.
- Risk: one LSP request per trait method per whole-project pass (on tokio,
  thousands, at load-dependent latency); only after the pass, never without
  rust-analyzer; a bridge change shared by every plugin. GM-498: a per-file
  pass asks only nodes of changed files, so adding `impl Loud for New` in
  beta does not re-ask `Loud::speak` (in alpha) until the next whole pass,
  the same staleness the type-level edge has today. GM-495: the pass's
  re-sent nodes take only `untypedCalls`; the edges are new rows, so no
  conflict, but the structural and semantic copies of the same method edge
  (for the four cases that already work) would need GM-531-style agreement,
  or they duplicate.

### C. Core derivation: method edge from a later type-level edge

At read time (`overrides.rs` `declared`) or link time (`symbol_links`), when
the anchor has no method edge, take the impl's self type, follow its
resolved `SUPERTYPE_OF` edges to a trait whose name equals the `as Tr` part
of the anchor's `<T as Tr>` segment, and report that trait's member of the
anchor's name (GM-502 D2's `member_of`).

- Benefit: no plugin change; closes the gap after rust-analyzer's pass for
  any trait clause the plugin cannot resolve, glob or not.
- Risk: only after the semantic pass; parses Rust syntax (`<T as Tr>`) in
  core; misses an aliased trait (`use … Loud as L`) and an impl in a module
  other than the self type's (the owner lookup is by container); two traits
  of one name implemented by one type are ambiguous and must be refused.
  `find_implementations`/`find_references` on `Loud::speak` stay without the
  impl method, since no edge exists.

### Recommendation

**A.** It is the only option that works from the first index and on a
machine without rust-analyzer, it is exact, and the prototype shows core
already does the hard part. B and C stay available for whatever A leaves
(a trait reached through a macro, an alias chain core cannot walk).

The `overrides` field for the glob cases then comes **structurally**.

## 4. Edit map

Change:

| Function / file | Lines | Change |
|---|---|---|
| `plugins/rust/src/extractor/bodies.rs` `Bodies::resolve_supertype` | 1548-1561 | the glob fallback above (census: record a new reason, e.g. `SupertypeViaGlob`, instead of `BareUnknownType` in `Ctx::Supertype`) |
| `bodies.rs` `member_supertypes` doc | 1563-1573 | say a glob-reached trait is addressed through the module |
| `plugins/rust/src/extractor/tests.rs` | near 1491 (`a_trait_impl_method_is_a_supertype_edge_to_the_trait_method`) | unit tests (section 5) |
| `plugins/rust/conformance/project/crates/beta/src/main.rs` | 50-60 (`Megaphone` doc) | no longer "invisible to every amount of parsing" |
| `plugins/rust/conformance/project/crates/alpha/src/prelude.rs` | 4-10 | same |
| `plugins/rust/conformance/expect.toml` | 31-33, 410-421; new entry | `shapes::Loud` entry no longer semantic-only (Q2); add `[[implementations]] symbol = "shapes::Loud::speak"` with `<Circle as Loud>::speak` and `<Megaphone as Loud>::speak` |
| `plugins/rust/tests/conformance.rs` | 430-458 (`the_semantic_tier_is_what_closes_the_receiver_call_gap`) | drop or replace the `Megaphone` missing-row assertion and the semantic-entry count (Q2) |
| `plugins/rust/README.md` | 139-145, 189-212 | remove the `overrides` limitation; rewrite the glob bullet (trait clauses resolve through a glob; other bare types still do not) |
| `docs/architecture/gm-502-override-callers-field.md` | D4 "Rust limitation" (≈143-150) | remove, or point here |
| core `CURRENT_INDEXER_VERSION` | - | bump |

Read for context (not changed): `bodies.rs` `impl_item` 318-367,
`supertype_edge` 1433-1503, `resolve_bare` 1088-1158,
`resolve_type_qualified` 1225-1290, `tail_in` 1322-1333; `model.rs`
`has_glob` 203; `decls.rs` `use_leaf` 655-745 (the private `*` row);
`core/src/graph/symbol_links.rs` `Resolver::walk_capped` 1511-1580,
`binding_hops`, `through_head` 1695-1727; `core/src/mcp/overrides.rs`
`declared` 230.

Callers and references relied on (g-mesh on `g-mesh-wt-gm537`):
- `find_callers(resolve_supertype)` -> name ambiguous (Rust and TS
  plugins); re-asked by id `96fd58b8…`: `Bodies::impl_item`,
  `Bodies::supertype_to`, both `bodies.rs`, complete. So the change affects
  impl trait clauses and supertrait bounds only.
- `find_callers(Bodies::member_supertypes)`: `impl_item` only.
- `find_callers(FileModel::has_glob)`: `Declarer::use_leaf` only (the new
  call is the second user).
- `find_callers(through_head)`: `Resolver::resolve` only.
- Megaphone's witnesses (`grep`, non-code and one known name):
  `tests/conformance.rs:454`, `expect.toml`, `beta/src/main.rs`,
  `alpha/src/prelude.rs`, `bridge.rs:239` and
  `multi-language-plugins.md:2218/2234` (historical GM-290 records, left
  as they are).

## 5. Behaviour list for tests (one control each)

1. `impl Tr for T` where `Tr` reaches the module only through one glob of a
   module declaring it: type edge `T -> Tr` and member edge
   `<T as Tr>::m -> Tr::m`, structurally (unit test asserting both
   placeholders' addresses; conformance entry linked). Control: revert the
   `resolve_supertype` fallback; both disappear.
2. Same through a glob of a module that only re-exports `Tr`
   (`Megaphone`): linked to `shapes::Loud` / `shapes::Loud::speak`.
   Same control.
3. `find_callers` on `<Megaphone as Loud>::speak` carries `overrides =
   [shapes::Loud::speak]` with no semantic pass (core-level test over the
   linked fixture, or the MCP probe the GM-502 verify used). Same control.
4. Two globs both offering `Tr`: no edge (core refuses the ambiguity), not
   a wrong one. Control: none needed beyond 1 (this pins the refusal).
5. A named import shadows the glob: `use a::Tr; use b::*;` (both have
   `Tr`) links to `a::Tr`. Unchanged path, regression only.
6. No glob in the module: a bare unknown trait still emits nothing (no
   placeholder for `impl Display for X`). Pins the scoping.
7. A generic parameter or a type parameter bound named like a trait is not
   addressed (`scopes.binds`).

## 6. Must confirm (implement slice)

- Census count of trait clauses resolved through a glob on g-mesh's own tree
  (new placeholders added), for the Risk line.
- `find_implementations(shapes::Loud)` and `find_references(shapes::Loud::speak)`
  show one row per impl after the semantic pass (the structural placeholder
  edge and the sweep's semantic edge for `Megaphone` both land on `Loud`;
  the probe showed no extra set members, rows not checked).

## 7. Open questions for the owner

### Q1. Where should the missing `overrides` come from?

Today: `<Megaphone as Loud>::speak` (trait reached through `use x::*`) has no
`overrides`, structurally or after rust-analyzer, because no tier emits the
method-level edge. Change: one of the sources in section 3. Example:
`find_callers("<Megaphone as Loud>::speak")` would show
`overrides: [{qualifiedName: "shapes::Loud::speak", …}]`. Consequence: which
machines and when the field appears, and how much work a pass costs.

- **A. Structural glob fallback for trait clauses (Recommended).** Benefit:
  from the first index, no rust-analyzer needed, exact, prototyped and core
  untouched. Risk: one unlinked placeholder per external trait under a glob
  (`impl Display` in test modules); Q2's witness changes.
- **B. rust-analyzer sweep over trait methods.** Benefit: server truth,
  covers macro-made traits. Risk: thousands of extra LSP requests per pass,
  only after the pass, bridge change for every plugin, structural/semantic
  duplicate edges to reconcile.
- **C. Core derivation from the type-level edge.** Benefit: no plugin
  change. Risk: only after the pass; core parses `<T as Tr>`; misses
  aliases; `find_implementations` on the trait method still lacks the impl.

### Q2. What becomes the proof that the semantic tier finds a cross-crate impl?

Today: `tests/conformance.rs`'s structural run asserts that
`crates/beta/src/main.rs:Megaphone` is *missing* from
`find_implementations(shapes::Loud)`, which is GM-290's evidence that only
rust-analyzer finds it. Change: with A, the structural tier finds
`Megaphone` itself, so that assertion fails. Example: the `[[implementations]]
symbol = "shapes::Loud"` entry stays `tier = "semantic"` but passes
structurally, and the "every semantic entry must fail" count drops by one.
Consequence: GM-290's acceptance case either moves to a new witness or is
retired.

- **Untag the `shapes::Loud` entry and keep `shapes::total` as the only
  semantic witness (Recommended).** Benefit: smallest change; the receiver-call
  witness still proves the tier runs. Risk: no fixture case where only the
  sweep finds a cross-crate *impl*.
- **Add a new sweep-only witness** (a trait reached by a shape A does not
  cover, e.g. through a `macro_rules!`-generated `impl`). Benefit: keeps
  GM-290's claim measured. Risk: needs a shape rust-analyzer answers and the
  parser cannot, to be found and verified in the fixture.

## 8. Owner decisions (2026-10-09)

- Q1: "A: структурно (Recommended)". The glob cases get `overrides` structurally (option A).
- Q2: "Новый пример". GM-290's claim stays measured: the tests slice finds and adds a new
  sweep-only witness (an impl only rust-analyzer finds, e.g. one generated by `macro_rules!`),
  tagged `tier = "semantic"`, replacing `shapes::Loud`/`Megaphone` as the cross-crate impl witness.
