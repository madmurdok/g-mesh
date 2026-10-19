# 0038. Rust enum variants are nodes, of the kind each shape is in Rust's value namespace

## Status
Accepted 2026-10-18 (GM-630, owner decisions D1-D4 as proposed). Design note,
edit map and the behaviours tests pin:
[`GM-630-rust-enum-variant-nodes.md`](../design/GM-630-rust-enum-variant-nodes.md).
Follows [ADR 0035](0035-engine-calls-answers-kind-rule.md) and GM-619's
option (d) ([`GM-619-variant-constructor-calls.md`](../design/GM-619-variant-constructor-calls.md)).

## Context
The Rust declarer emits no node for an enum variant. The extractor already
addresses every type-qualified path `E::V` by the `qualifiedName`
`mod::E::V` (`Bodies::tail_in`), so each construction or use of a variant is
a placeholder that nothing links: on ripgrep 15.2.0, 135 `CALLS` sites
(tuple-variant constructions, 52 of them cross-file) onto 40 enums and 318
`REFERENCES` sites (unit variants used as values) onto 50 enums.
`find_definition`, `find_callers` and `find_references` on a variant answer
nothing, and a semantic answer at a variant lands on the enum away from its
name and is dropped (ADR 0035 D1). GM-619 decided that such a call must not
land on the enum (a variant is a member of its enum, not the enum), and named
variant nodes as the route to construction sites.

## Decision
- **D1: one node per variant, its kind by shape.** Rust puts a tuple variant
  in the value namespace as a constructor function, a unit variant as a
  constant, and a struct variant in the type namespace only; the node kind
  follows that:

  | variant | `kind` | why |
  |---|---|---|
  | tuple `V(..)` | `Function` | `E::V(..)` is a call; structural `CALLS` land only on a `Function` (ADR 0035 D4) |
  | unit `V` | `Variable` | `E::V` is a value, like a `const`; `REFERENCES` link onto any kind |
  | struct `V { .. }` | `Type` | `E::V { .. }` is written in type position, like a struct literal; `REFERENCES` link onto any kind |

- **D2: `nativeKind` is `variant` for all three shapes.** The shape is in
  `kind` already; one name keeps the table and every `nativeKind` filter
  short, as `field` covers named and positional fields.
- **D3: `find_callers(E)` does not roll up its variants' callers.** A
  `CALLS` edge names the declaration of what is called (GM-619); the callers
  of `E::V` are the variant's, asked by its name. Methods (`E::new`) are not
  rolled up into their type either.
- **D4: addressing and placement, as for every other type member.**
  - `name` `V`; `qualifiedName` `mod::E::V` (the `qualifiedPath` the
    extractor's `E::V` placeholders already carry), so existing placeholders
    link structurally with no linker or bridge change.
  - Container: the enum's module (`in_container(module.key, ..)`), so core
    writes one `DEFINES` edge from the module container, as for fields and
    methods. There is no enum-to-variant edge; the graph has no `CONTAINS`
    kind, and the parent relation is the `qualifiedName` prefix that
    `is_type_member` already reads.
  - Visibility and `exported`: the enum's own (a variant is as visible as
    its enum).
  - Range: the `enum_variant` node; signature: its source text collapsed
    (`Limited(ContextModeLimited)`); doc comment: its `///` run.
  - File model: registered by its tail `E::V` only (`declare_member`), never
    by its bare name, so a bare `V` in the module cannot resolve to it.
  - Variant fields stay non-nodes, and the payload's types are still
    referenced from the enum, not from the variant.

## Consequences
- `find_definition("Limited")` / `"ContextMode::Limited"`,
  `find_callers` and `find_references` on a variant answer, cross-file
  included. `find_callees(default)` shows a resolved `Limited` row.
- `find_callers(ContextMode)` still lists no constructions (D3).
- Counts grow, measured by a scan of the indexed enums' bodies:
  ripgrep 15.2.0 +253 nodes (139 unit, 96 tuple, 18 struct) on 4,244
  declarations (+6.0%); g-mesh itself +638 (418 unit, 136 tuple, 84
  struct) on 14,280 Rust declarations (+4.5%). Outlines, the golden and
  benchmark node counts, and the embedding pool grow by as much.
- 40 of ripgrep's 192 distinct variant names already name another
  declaration (`Error`, `Match`, `Glob`, `Context`); a bare-name
  `find_definition` for them gets one more candidate. A path query
  (`E::V`) stays exact, and the linker's `drop_type_members` keeps a
  name-keyed placeholder on the non-member.
- A semantic answer at a variant's name now lands on the variant (its
  tightest node) at its own name; a tuple variant is a `Function`, so
  ADR 0035 D1's check does not apply to it.
- `cfg` alternatives of one `E::V` (in one enum, or in `cfg` copies of the
  enum) are one node, the first declared, whatever the later ones' shapes.
- Unchanged gaps: a variant reached through `use E::*` or `use m::E::V`
  (no container `m::E`), and `E::V` in a `use super::*` test module, stay
  unlinked, as type members do today.

## Supersedes
ADR 0035's Consequences bullet "Rust tuple-variant constructors
(`Enum::Variant(..)`) stay unlinked, by decision (GM-619) ..." is replaced
by: "Rust tuple-variant constructors link to the variant's own node (ADR
0038), never to the enum." ADR 0035 D1-D4 are unchanged.

## Alternatives rejected
- **Every variant a `Function`.** Uniform, and links everything, but an
  outline calls `Passthru` a function and `find_callers` on it is
  meaningless.
- **Every variant a `Variable`, or a `Type`.** Tuple constructions are
  structural `CALLS`, which ADR 0035 D4 lands only on a `Function`: the 135
  sites would stay unlinked unless D4 got a Rust-specific exception.
- **No node for struct variants.** 18 variants on ripgrep would keep
  unlinked `REFERENCES` and no definition, a hole in an otherwise uniform
  rule, to save 18 nodes.
- **`nativeKind` per shape (`tuple_variant`, `unit_variant`,
  `struct_variant`).** Repeats what `kind` says.
- **Roll variant callers up into `find_callers(E)`.** The encloser landing
  ADR 0035 D1 removed, reintroduced at query time.
- **GM-619 (b)/(c):** land `E::V(..)` on the enum in the extractor or the
  bridge; rejected there.
