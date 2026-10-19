# GM-630: Rust enum variants as nodes

Status: design (S1), for [ADR 0038](../adr/0038-rust-enum-variants-are-nodes.md)
(Proposed). Branch `feat/GM-630-rust-enum-variant-nodes`, off release-4.12.0.
Option (d) of [GM-619](GM-619-variant-constructor-calls.md).

Tags: [code] read in this branch; [run] measured. Line numbers are 1-based.

## 0. Sources

g-mesh calls (project `g-mesh`, the main checkout's index; this branch has
changed no code):

| question | call | answer |
|---|---|---|
| who walks items into the declarer | `find_callers(Declarer::item)` | `Declarer::collect` only |
| who registers members in the file model | `find_callers(FileModel::declare_member)` | `Declarer::fields`, `positional_fields`, `declare` |
| where `E::V` addresses come from | `find_callers(Bodies::member_of)` | `receiver_call`, `call_type`, `resolve_type_qualified`, `self_member` |
| who builds the `qualifiedName` placeholder | `find_callers(Bodies::tail_in)` | `member_of`, `field_of` |
| where core's kind rule is applied | `find_callers(required_target_kind)` | `link_diff`, `link` (`core/src/graph/symbol_links.rs`) |
| how a node joins its container | `find_definition(NodeSpec::in_container)` | `plugins/sdk/src/graph.rs:382`; core writes the `DEFINES` edge (`core/src/graph/containers.rs` module doc) |
| edge kinds that exist | `find_definition(EdgeKind)` | `Defines, Imports, Calls, SupertypeOf, References, Exports`: no `CONTAINS` |
| what a Rust outline shows | `get_file_outline(plugins/rust/src/extractor/model.rs)` | flat, source order; `enum Import` has no variant rows; fields appear as `T.f` |

grep was used inside single known files (`decls.rs`, `bodies.rs`,
`bridge.rs`, `symbol_links.rs`, `find_definition.rs`,
`find_callers_callees.rs`) and for the test fixtures.

[run] counts: `scratchpad/gm630-s1/count_db.py` lists each `nativeKind =
'enum'` node of an index, scans its body for variants and classifies the
shape. ripgrep: GM-593/S9's `index-ripgrep-after.db` against a checkout of
15.2.0 (`e89fff8`, the registry's revision). g-mesh: the main checkout's
own index (`~/.g-mesh/projects/959ade85d9a343b1`, built 2026-10-18).

| corpus | enums | unit | tuple | struct | new nodes | Rust declarations today | growth |
|---|---|---|---|---|---|---|---|
| ripgrep | 70 | 139 | 96 | 18 | 253 | 4,244 | +6.0% |
| g-mesh | 194 | 418 | 136 | 84 | 638 | 14,280 | +4.5% |

Name overlap: 40 of ripgrep's 192 distinct variant names (52 of g-mesh's
445) already name another declaration.

## 1. Today [code]

- `Declarer::item` (`decls.rs:350-391`) declares an `enum_item` as a
  `Type`/`enum` and stops; `node_kinds` (`decls.rs:205-244`) has no arm for
  `enum_variant`; the module doc (`decls.rs:24-25`) states variants are not
  nodes.
- `Bodies::resolve_type_qualified` (`bodies.rs:1226-1291`) -> `member_of`
  -> `tail_in` (`bodies.rs:1323-1337`): `lookup_tail(container, "E::V")`
  misses, so it emits `Bound::There` with `TargetKey::QualifiedName
  ("mod::E::V")`. `Self::V` goes the same way through `self_member`
  (`bodies.rs:1294-1311`).
- Core links that placeholder onto a node of that `qualifiedName` in the
  container, subject to the kind rule (`symbol_links.rs:357-391`):
  structural `CALLS` only onto `Function`, `REFERENCES` onto anything.
- Struct-variant literals `E::V { .. }`: the name is a
  `scoped_type_identifier`, walked by `visit` (`bodies.rs:177-183`) into
  `path_use` -> the same `qualifiedName` placeholder, as `REFERENCES`.
  `struct_address` (`bodies.rs:1008-1047`) returns `None` for a
  type-qualified name, so no field edges.
- Patterns (`E::V(x) =>`, `E::A =>`) produce no edge to the variant
  (`scope.rs` `bind_pattern`); this does not change.
- The `enum_variant` arm of `visit` (`bodies.rs:157-159`) walks a
  variant's payload with `from` = the enum's id.

## 2. Change

Rust plugin only. No core, SDK, bridge or wire change: the placeholders
already carry the key, and the kind rule already admits each shape's edge.

### Edit map

| file:lines | function | change |
|---|---|---|
| `plugins/rust/src/extractor/decls.rs:1-32` | module doc | add rows `enum variant V of E` -> `Function`/`Variable`/`Type` by shape, `variant`, `E::V`; replace "Enum variants, and so their fields, are *not* nodes" with "a variant's fields are not nodes"; link ADR 0038 once here |
| `decls.rs:350-391` | `Declarer::item` | after `self.declare(..)` for `enum_item` (lines 385-388, beside the `fields` call for struct/union); `own` must be cloned before `declare` takes it, call `self.variants(item, module, own)` when declared |
| new, beside `fields` (`decls.rs:401`) | `Declarer::variants` | walk `body` (`enum_variant_list`) for `enum_variant` children; shape from the variant's `body` field: `ordered_field_declaration_list` -> `Function`, `field_declaration_list` -> `Type`, none -> `Variable`; `NodeSpec::with_path(kind, V, module.qualified_path(&member_tail_path-equivalent "E::V"), range(variant)).native_kind("variant").visibility(enum's own).in_container(module.key, module.parent)`; `signature`, `outer_doc_comment` as in `fields`; `emitter.declare(spec, is_public(&own))`; `model.declare_member(&module.key, "E::V", DeclRef { id, kind })`. Never `model.declare` (no bare name). |
| `decls.rs:205-209` | `node_kinds` | none: a variant is not an item, so it stays out of `node_kinds` and its doc line naming `enum_variant` stays true |
| `plugins/rust/src/extractor/bodies.rs:154-159` | `visit`, `enum_variant` arm | comment only ("An enum variant is not a node" becomes "a variant's payload is referenced from its enum"); `from` stays the enum's id |
| `docs/adr/0035-engine-calls-answers-kind-rule.md`, Consequences | ADR text | replace the tuple-variant bullet as ADR 0038 "Supersedes" says; set 0038 Accepted and add it to `docs/adr/README.md` |

No change in `resolve_type_qualified`, `tail_in`, `self_member`: once the
model holds `E::V`, same-file sites become `Bound::Here`, cross-file sites
keep the placeholder that now links.

### Functions to read before editing

- `Declarer::fields` / `positional_fields` (`decls.rs:401-500`): the
  pattern for a member node (spec, signature, doc, `declare_member`).
- `Declarer::declare` (`decls.rs:546-585`): how `qualified_path` and the
  tail are built; `member_tail_path` (`decls.rs:161`).
- `Emitter::declare` (`emit.rs:181`): id dedup across `cfg` alternatives.
- `Bodies::tail_in` (`bodies.rs:1323-1337`): the key the new node must
  equal (`qualified_in(container, "E::V")`).
- `Linker::drop_type_members` / `is_type_member`
  (`core/src/graph/symbol_links.rs:1839-1923`): why a name-keyed
  placeholder does not pick a variant over a same-named item.
- `lands_on_name` (`plugins/sdk/src/lsp/bridge.rs:2148-2170`) and
  `SdkIndex::node_at` (`plugins/sdk/src/index.rs:176`): an answer at `V`
  lands on the variant, at its name.

### Tests that change

- `extractor/tests.rs:753` `a_type_qualified_call_inside_a_path_pattern_arm_keeps_its_structural_edge`:
  `Mode::B` becomes a resolved target, no longer a `pending_symbol`;
  update the assertion, keep the "no open site" half.
- `extractor/tests.rs:142` `every_item_kind_becomes_the_node_the_design_doc_names`:
  add `("E::A", NodeKind::Variable, "variant")`.
- `extractor/tests.rs:2585` `tuple_variants_plain_tuples_and_index_literals_declare_and_link_no_positional_field`:
  filters `nativeKind == "field"`, so it holds; check it still passes.
- `plugins/rust/conformance`: the fixture has no enum. Add one to
  `crates/alpha/src/shapes.rs` (`pub enum Tone { Low, High(u8), Wide { w: u8 } }`)
  used from `crates/beta/src/main.rs`, with `[[callers]]`,
  `[[references]]` and `[[definition]]` entries in `expect.toml`.

## 3. Behaviours to pin, one control each

Six controls (rule: 6-8). Each control reverts code, never a test.

1. **A node per variant, by shape.** `enum E { U, T(u8), S { a: u8 } }`
   gives `E::U` `Variable`, `E::T` `Function`, `E::S` `Type`, all
   `nativeKind` `variant`, the enum's visibility, container the module.
   Control A: drop the `variants` call in `Declarer::item` (no node).
   Control B: map every shape to `Function` (the `U`/`S` kinds fail).
2. **Same-file construction resolves onto the variant.** `E::T(1)` and
   `Self::T(1)` inside `impl E` are resolved `CALLS` onto `E::T`; `E::U`
   as a value is a resolved `REFERENCES` onto `E::U`; `E::S { a: 1 }` a
   resolved `REFERENCES` onto `E::S`. Control C: skip
   `model.declare_member` for variants (sites fall back to placeholders).
3. **Cross-crate links structurally** (conformance): `find_callers
   (shapes::Tone::High)` lists the `crates/beta` caller,
   `find_references(shapes::Tone::Low)` its use, `find_definition` lands on
   the variant's line. Control D: give tuple variants kind `Variable` (the
   structural `CALLS` is refused by ADR 0035 D4; `[[callers]]` fails).
4. **A variant is not a bare name.** With `pub fn T() {}` beside
   `enum E { T(u8) }`, a bare `T()` stays onto the free fn; and in core, a
   name-keyed `use m::X` where `m` declares `struct X` and an enum variant
   `E::X` links the struct. Control E: register variants with
   `model.declare` (by name too); the bare call binds the variant.
5. **`find_callers(E)` lists no constructions** (D3), while
   `find_callers(E::T)` does. No control: no code is added for it; it
   fails only if someone adds a roll-up, which is that change's test.
6. **Payload references stay from the enum.** `enum E { T(Foo) }` gives
   `REFERENCES` `E -> Foo`, none from `E::T`. Control F: pass the variant's
   id as `from` in the `enum_variant` arm of `visit`.

Repeats: none of these tests involve processes, threads or timers.

## 4. Measure (verify)

ripgrep 15.2.0, reindex before/after, sqlite3:

- nodes `nativeKind = 'variant'`: 0 -> about 253 (139 `Variable`, 96
  `Function`, 18 `Type`; the scan is an estimate, cfg duplicates dedup).
- resolved `CALLS` onto a variant: 0 -> about 135; resolved `REFERENCES`
  onto a variant: 0 -> about 318 plus struct-variant literals.
- `CALLS` onto a `nativeKind = 'enum'` node: 0 before and after.
- unlinked `qualifiedName` placeholders: down by the same amount; no new
  unlinked rows.

The node-count growth of section 0 is what golden and benchmark node counts
move by; the embedding eval's ripgrep pool grows by the same 253 nodes.

## 5. Decisions for the owner

Each: today / change / example / consequence.

- **D1 node kind per shape (recommended: tuple `Function`, unit
  `Variable`, struct `Type`).** Today: no node. Change: one node per
  variant, kind by Rust's namespace. Example: `ContextMode::Limited(..)`
  links as a call onto `Limited` (`Function`); `ContextMode::Passthru`
  links as a reference onto `Passthru` (`Variable`). Consequence: every
  existing placeholder links with no core change; the alternative "all
  `Function`" is simpler but calls a unit variant a function in outlines;
  "struct variants not nodes" leaves 18 ripgrep variants without a
  definition.
- **D2 `nativeKind` (recommended: one `variant`).** Today: n/a. Change: a
  new `nativeKind`. Example: outline row `Limited  Function  variant`.
  Consequence: one string to filter on; per-shape names would repeat
  `kind`.
- **D3 roll-up (recommended: no).** Today: `find_callers(ContextMode)`
  lists no constructions. Change: none; `find_callers(Limited)` lists
  them. Consequence: consistent with GM-619 and with methods, which are
  not rolled up into their type; a user asking "who constructs `Error`"
  asks per variant.
- **D4 count growth (recommended: accept).** Today: ripgrep 4,244 Rust
  declarations, g-mesh 14,280. Change: +253 (+6.0%) and +638 (+4.5%)
  nodes, in outlines, embeddings and node counts. Consequence: 40 of
  ripgrep's 192 variant names (`Error`, `Match`, `Glob`) add a candidate to
  a bare-name `find_definition`; path queries stay exact.
