# GM-594: engine CALLS onto a Rust enum come from a mis-bound path pattern

Status: design (S1). Branch `fix/GM-594-engine-calls-on-enums`, off
release-4.10.0 (contains GM-593). Follows GM-569 (ADR 0035).

Tags: [code] read in this branch; [run] measured with sqlite3 on the cached
4.10.0 base indexes (`index-cache/d8fcc50/index-{ripgrep,excalidraw}.db`,
indexer `4+8e6a248ba1281a91`) and GM-569/S8's `index-gin-after.db`; sources
read from the ripgrep corpus the index was built from. No reindex.

## 0. Summary

On ripgrep, three semantic `CALLS` edges land on the enum `ContextMode`, from
its own methods `set_before`/`set_after`/`set_both`. The task framed them as
enum-variant constructors (`ContextMode::Limited(..)`) linking where ADR 0035
said such constructors stay unlinked. They are not that. They are a Rust
plugin scoping bug: a unit-variant pattern `ContextMode::Passthru =>` binds
`ContextMode` (and `Passthru`) as *locals* for the arm. Inside the arm the
call `ContextMode::Limited(..)` then counts as "type is a local", so the
plugin writes no structural edge and opens a site **at the start of the path**,
the qualifier `ContextMode`. rust-analyzer answers what *that token* is: the
enum, at its name. `lands_on_name` passes, and D3 accepts a `Type`.

So the edge answers "what is `ContextMode`", not "what is called". The same
call outside such an arm (`impl Default for ContextMode::default`, line 349)
keeps its structural placeholder `ContextMode::Limited` and links to nothing,
as ADR 0035 says.

**Recommendation: option D. Fix the root in the Rust plugin: a path pattern
(`scoped_identifier`, `generic_pattern`) binds nothing.** A path in pattern
position can never be a binding in Rust, so this is exact, not a guess.
ripgrep: `CALLS`→enum 3→0, semantic `REFERENCES`→enum 6→0 (same mechanism),
and those 9 sites get back the structural placeholders every other
`Enum::Variant` use has. No bridge or linker change.

## 1. What happens today

### The example [code][run]

`crates/core/flags/lowargs.rs` (1-based lines):

```rust
340 pub(crate) enum ContextMode {
342     Passthru,
344     Limited(ContextModeLimited),
345 }
347 impl Default for ContextMode {
348     fn default() -> ContextMode {
349         ContextMode::Limited(ContextModeLimited::default())   // no semantic edge
...
353 impl ContextMode {
359     pub(crate) fn set_before(&mut self, lines: usize) {
360         match *self {
361             ContextMode::Passthru => {
362                 *self = ContextMode::Limited(ContextModeLimited { .. })   // CALLS -> enum
```

Index rows:

| caller | edge | target |
|---|---|---|
| `ContextMode::default` | syntactic `CALLS`, unresolved | pending `…ContextMode::Limited` (348:8) |
| `ContextMode::set_before` | semantic `CALLS`, resolved | `Type/enum flags::lowargs::ContextMode` |
| `set_after`, `set_both` | same as `set_before` | same; one shared placeholder, key `qualifiedName flags::lowargs::ContextMode`, range 361:24-44 = the text `ContextMode::Limited` |

`set_before` has **no** structural `CALLS` row for `ContextMode::Limited`;
`default` has one.

### The mechanism, step by step [code]

1. `match_arm` (`plugins/rust/src/extractor/bodies.rs:521-544`) pushes a frame
   and calls `Scopes::bind_pattern` on each pattern child.
2. `bind_pattern` (`plugins/rust/src/extractor/scope.rs:127-146`) binds
   `identifier`s and recurses into everything else, skipping only a
   `tuple_struct_pattern`/`struct_pattern`'s `type` field. The pattern
   `ContextMode::Passthru` is a `scoped_identifier` (fields `path`, `name`;
   tree-sitter-rust 0.24.2 lists it among `_pattern` subtypes), which has no
   `type` field, so the recursion binds both `ContextMode` and `Passthru`.
   (`ContextMode::Limited(ContextModeLimited { ref mut before, .. })` is a
   `tuple_struct_pattern`; its type is skipped correctly.)
3. In the arm body, `call` → `path_use` → `resolve_path` →
   `resolve_type_qualified` (`bodies.rs:1225-…`): `self.scopes.binds("ContextMode")`
   is true, the branch meant for `T::new()` on a generic parameter, so it
   returns `Bound::Open`.
4. `emit` (`bodies.rs:1338-1357`) turns `Bound::Open` into `open_site` with
   no `replaces`; `open_site` (`bodies.rs:1378-1397`) puts the site's position
   at `at.start_position()`, the start of the whole path: the qualifier.
5. The bridge asks `textDocument/definition` there. rust-analyzer resolves
   the qualifier segment: the enum `ContextMode`, pointing at its name.
6. `record_answer` (`plugins/sdk/src/lsp/bridge.rs:2562-…`): the landing is a
   `Type`, so it is held to `lands_on_name` (`bridge.rs:2018-2048`), which it
   passes; the linker's D3 accepts `Type`. Edge recorded.

### GM-593 does not touch these [code]

GM-593 only re-checks a `CALLS` landing on the caller itself
(`node.id == site.from_id`). Here the caller is the method and the landing is
the enum, a different node, so it goes down the ordinary non-`Function`
path, `lands_on_name`, and passes because the engine really did point at
the enum's name.

### Why ADR 0035's Consequence still holds [code]

ADR 0035: "Rust tuple-variant constructors (`Enum::Variant(..)`) have no node
and land on the enum away from its name, so they stay unlinked." That is
about a site asked at the variant. Correctly bound variant calls are never
asked at all: `resolve_type_qualified` gives them a `qualifiedName`
placeholder (`Enum::Variant`) that links to nothing, because a variant has no
node. The 3 edges come from the one path that asks at the **qualifier**: a
type name over-bound as a local.

### The same mechanism outside CALLS [run]

Every semantic edge whose site text is `X::Y` and whose target is the node
named `X` (a "qualifier landing"), ripgrep:

| edge | caller | site | arm pattern above it |
|---|---|---|---|
| CALLS | `ContextMode::set_before/after/both` | `ContextMode::Limited` | `ContextMode::Passthru =>` |
| REFERENCES | `HiArgs::from_low_args` (hiargs.rs:131) | `SearchMode::Count` | `SearchMode::CountMatches if .. =>` |
| REFERENCES | `HiArgs::from_low_args` (hiargs.rs:156) | `ColorChoice::Never` | `ColorChoice::Auto if .. =>` |
| REFERENCES | `Match::invert`, `Match::map` (ignore/lib.rs:455) | `Match::None` | `Match::None =>` |
| REFERENCES | `<Error as Clone>::clone` (ignore/lib.rs:149) | `Error::InvalidDefinition` | `Error::InvalidDefinition =>` |
| REFERENCES | `Parser::parse_star` (globset/glob.rs:946) | `Token::RecursivePrefix` | `Token::RecursivePrefix =>` |

All 9 sit in an arm whose pattern is a unit-variant path. 0 other
qualifier landings. ripgrep has 526 `Upper::Upper(` texts and 170 arms of the
form `X::Y =>`; only these 9 sites became edges, which is the inconsistency a
user would see.

### Other corpora [run]

Non-`Function` `CALLS` targets per corpus:

| corpus | Type/enum | Type/struct | Variable/const |
|---|---|---|---|
| ripgrep (Rust) | 3 | 48 (all bare `Name(..)` sites, none a qualifier landing) | 0 |
| excalidraw (TS) | 0 | 0 | 33 |
| gin (Go) | 0 | 0 | 0 |

TS enums are not callable and Go has none; a Python `Enum` subclass is a
class (`Color(1)` landing on it is the class-constructor case ADR 0035
already accepts). The bug is Rust-plugin-only.

### What a user sees today

- `find_callers("ContextMode")`: `set_before`, `set_after`, `set_both`; not
  `default`, not any of the other `ContextMode::Limited(..)` constructions.
  Reads as "these call the enum", and is a partial list of constructions.
- `find_callers` / `find_definition` on the variant `Limited`: nothing, under
  every option (a variant has no node).
- `find_callees("set_before")`: the enum, instead of the unresolved
  `ContextMode::Limited` that `default` shows.
- Latent, 0 on ripgrep: in an arm `E::A => { E::new() }` the same path makes
  `CALLS` caller → enum `E` instead of the structural edge to `E::new`, which
  the plugin would have resolved. That is a wrong edge and a lost right one.

## 2. Options

### A. Accept enum landings (today)

Benefit: no change; 3 extra "constructor" rows on ripgrep.
Risk: the edge answers the qualifier, not the callee. It appears only in arms
after a unit-variant path pattern, so `find_callers(enum)` is an arbitrary
subset of constructions. A same-arm `E::assoc_fn()` would become a wrong
`CALLS` onto `E` and lose its correct structural edge. Also the 6 enum
`REFERENCES` stay.

### B. Refuse enum targets for engine CALLS (bridge or linker)

Benefit: 3→0 with a one-line kind check; a Rust enum is never callable, so no
correct edge is lost.
Risk: treats the symptom. The 3 sites still have no structural edge (the
mis-binding suppressed it), so `find_callees(set_before)` loses the call
entirely, unlike `default`. The 6 enum `REFERENCES` and the latent
`E::assoc_fn()` case (struct or enum qualifier via a `REFERENCES`, or a
future `Type` kind) stay. Needs a language-specific "enum" test in shared
code (`nativeKind == "enum"`), which the bridge has avoided so far.

### C. Refuse only when the caller is the enum's own method

Benefit: matches the observed 3.
Risk: the mechanism does not depend on the caller's impl: the 6 `REFERENCES`
come from `HiArgs`, `Parser`, `<Error as Clone>`. A caller-owner check is
narrower than the cause and has all of B's risks.

### Treat like a tuple-struct constructor (orchestrator's earlier suggestion)

`Logger(())` lands on `Logger` because the engine is asked at the called
name, which *is* the struct. Here the engine is asked at the qualifier; the
called thing is the variant. Making "variant construction → enum" a real
feature means linking every `Enum::Variant(..)` placeholder to its enum
(hundreds on ripgrep), not keeping 3 accidental ones. That is a new linking
rule with its own decision (does `find_callers(enum)` list constructions?);
out of scope here, listed as a follow-up.

### D. A path pattern binds nothing (Recommended)

`bind_pattern` returns without binding for `scoped_identifier` and
`generic_pattern`. In Rust a binding is always a single identifier; a path in
pattern position names a variant or a constant.

Benefit: fixes the cause. The 9 sites become `resolve_type_qualified`
`qualifiedName` placeholders like every other `Enum::Variant` use: no open
site, no question, no engine edge; `find_callees(set_before)` shows the
unresolved `ContextMode::Limited` exactly like `default`. `E::assoc_fn()` in
such an arm gets its structural edge back. No shared-code change; `Passthru`
and `None` stop being false locals for the arm body too.
Risk: `find_references("ContextMode")` etc. loses the 6 semantic
`REFERENCES` rows that happened to be right in spirit (a variant use
attributed to its enum); those were an accidental 6 of hundreds. It also
changes which sites are open sites, so the semantic pass asks 9 fewer
questions on ripgrep: verify by reindex, not by assumption. Does not cover a
bare unit variant imported with `use E::*` (`Passthru =>` is an `identifier`,
indistinguishable from a binding without resolution); that stays
over-bound, costing a missing edge, as scope.rs documents.

## 3. Decisions

- D1 (recommended: D). Option D, plugin fix; no bridge/linker change.
- D2 (recommended: follow-up task, not here). Two hardenings found on the
  way, each its own behaviour change: (i) an open site for a path call asks
  at the path's **last** segment, not its start, so `T::new()` on a generic
  parameter becomes answerable instead of always dropped; (ii) "variant
  construction lists under `find_callers(enum)`" as a deliberate rule.

## 4. Edit map (option D)

- `plugins/rust/src/extractor/scope.rs:127-146` `Scopes::bind_pattern`: add
  an arm `"scoped_identifier" | "generic_pattern" => {}` before the
  recursive `_`, with a comment: a path in pattern position names a variant
  or a constant and never binds; binding its segments would make a type
  name a local, which opens `Type::x` paths in the arm to the semantic tier
  at the qualifier (present tense, no ticket id: CLAUDE.md comment rule).
  Extend the doc comment's list of non-binding forms
  (lines 122-126) with "a path pattern (`E::A`)".
- `plugins/rust/src/extractor/scope.rs:45-60` module doc, "What it does when
  it is unsure": add that over-binding a *type* name is not only a missing
  edge: `resolve_type_qualified` opens its paths and the engine answers the
  qualifier, so non-binding forms that are exact (path patterns) must be
  skipped.
- `docs/adr/0035-engine-calls-answers-kind-rule.md`, Consequences, the
  tuple-variant bullet: append "(GM-594: the 3 that linked on ripgrep came
  from a unit-variant path pattern over-binding the enum's name, fixed in the
  Rust plugin)".
- No change in `bodies.rs`, `bridge.rs` or the linker.

## 5. Behaviours the tests must pin, with controls

One behaviour, one control.

1. **A path pattern binds nothing; its sub-patterns still do.** Unit test in
   `scope.rs` tests (beside `a_tuple_struct_pattern_binds_its_fields_and_not_its_variant`,
   line 257): bind each arm pattern of
   `fn f() { match x { E::A => {}, E::B(y) => {}, E::<u8>::C | E::D => {} } }`;
   assert `!binds("E")`, `!binds("A")`, `!binds("C")`, `!binds("D")`,
   `binds("y")`.
2. **A type-qualified call inside a unit-variant arm keeps its structural
   edge and opens no site.** Extractor test in `plugins/rust/src/extractor/tests.rs`
   (beside `a_type_qualified_path_call_is_a_qualified_name_placeholder`,
   line 723): one file with
   `pub enum Mode { A, B(u8) } impl Mode { pub fn new() -> Mode { Mode::A } pub fn set(&mut self) { match *self { Mode::A => { *self = Mode::B(1); Mode::new(); } Mode::B(_) => {} } } }`.
   Assert `set`'s `CALLS` targets contain the `qualifiedName` placeholder for
   `Mode::B` and the resolved `Mode::new`, and no open site from `set` has a
   name starting `Mode::`.

Control (covers 1 and 2, one build of the rust plugin's lib tests): remove the
new `bind_pattern` arm. Test 1 fails on `binds("E")`; test 2 fails because
both calls become open sites with no structural edge.

Pin the existing behaviour too (no control needed, already tested):
`a_tuple_struct_pattern_binds_its_fields_and_not_its_variant` and
`destructuring_patterns_bind_every_name_they_introduce` stay green.

## 6. Measure (verify slice)

Reindex ripgrep with the fix (cold; the cache's `d8fcc50` is the before).
Expected: semantic `CALLS`→`Type/enum` 3→0; semantic `REFERENCES`→`Type/enum`
6→0; qualifier landings (site `X::Y` onto node `X`) 9→0; `CALLS`→`Type/struct`
48 unchanged; syntactic placeholders for the 9 sites present (e.g.
`set_before` → pending `…ContextMode::Limited`). Any other edge-count delta is
a finding to explain. Excalidraw and gin need no rerun (no Rust).
