# GM-619: should `Enum::Variant(..)` produce `CALLS` onto its enum?

Status: design (S1). Branch `docs/GM-619-variant-constructor-calls`, off
release-4.11.0. Follows ADR 0035 and GM-594 (its D2 (ii)).

Tags: [code] read in this branch; [run] measured with sqlite3 on the GM-593/S9
ripgrep index (`gm593-s9/index-ripgrep-after.db`, built before GM-594's
pattern fix; the counts below come from structural placeholders, which that
fix does not change, except the 3 accidental enum landings). No reindex.

g-mesh calls used (project `g-mesh-wt-gm619`): `find_definition`
(`resolve_type_qualified`, `member_of`, `self_member`, `lookup_name`,
`lookup_tail`), `find_callers(resolve_type_qualified)` (one caller:
`resolve_path`), `find_references(DeclRef, rust id)` (model.rs `declare`,
`declare_member`, `lookup_*`; decls.rs `fields`, `positional_fields`,
`declare`, `mod_item`). grep only inside `bridge.rs` and `bodies.rs`.

## 0. Summary

**Recommendation: (a) no. Keep `Enum::Variant(..)` unlinked and state the
reason in ADR 0035: a `CALLS` edge lands on the declaration of the thing
called, and a variant is not its enum.** A tuple struct links because the
struct *is* its constructor; a variant is a member of the enum, and putting
its call on the enum is the encloser landing ADR 0035 D1 removed everywhere
else. If construction sites are wanted, the honest route is (d), variants as
nodes of their own, a separate task with its own decision (sketched in 2.d,
not recommended for 4.11.0).

## 1. What happens today

### The example [code]

```rust
// crates/core/flags/lowargs.rs
pub(crate) enum ContextMode { Passthru, Limited(ContextModeLimited) }
impl Default for ContextMode {
    fn default() -> ContextMode {
        ContextMode::Limited(ContextModeLimited::default())   // site
    }
}
```

The Rust extractor (`Bodies::call` -> `path_use` -> `resolve_path`) sees a
two-segment path with an upper-case qualifier and calls
`resolve_type_qualified` (`bodies.rs:1225-1282`). That calls `member_of` ->
`tail_in` (`bodies.rs:1312-1334`): `lookup_tail("ContextMode::Limited")`
finds nothing (a variant has no declaration in the file model), so it emits
`Bound::There` with a placeholder keyed by `qualifiedName`
`flags::lowargs::ContextMode::Limited`. No node has that name, so core never
links it. No open site is written, so the semantic tier is never asked.

Index rows for `default` [run]: `CALLS -> Limited` (pending_symbol,
`...ContextMode::Limited`, unlinked) and `REFERENCES -> ContextMode` (from the
return type, not from the construction).

### How a tuple struct links, for comparison [code][run]

`Logger(())` is a bare call: `resolve_bare(.., want = Function)`.
`lookup_name` filters by kind, so a same-file struct `Logger` does not fit a
`Function`; it is not imported either, so the result is `Bound::Open`. The
open site is asked at `Logger`; rust-analyzer answers the struct at its name;
`record_answer`'s check (`lands_on_name`, `bridge.rs:2082`) passes and ADR
0035 D3 admits a `Type`.

ripgrep: 48 semantic `CALLS` onto a struct, **all 48 same-file**. An imported
struct (`use a::Logger; Logger(())`) gets a name-keyed placeholder that the
structural kind rule (`CALLS` only onto `Function`, D4) refuses, and no hop is
asked for it in Rust: 1 such site on ripgrep. So struct constructors are
"linked" mostly because ripgrep constructs its tuple structs in the declaring
file.

### What would land if the variant were asked [code]

rust-analyzer's definition of `Limited` is the variant's name inside the
enum body. `node_at` returns the tightest node containing it, the enum;
`lands_on_name` compares with the enum's *own* name (`enum ContextMode`) and
fails; the `CALLS` answer is dropped, the structural edge stands. A variant
named like its enum (`enum Foo { Foo(..) }`) also fails: the first whole-word
`Foo` is the enum's.

### Size on ripgrep [run]

`qualifiedName`-keyed placeholders whose key minus its last segment is a
node in the same container:

| edge | owner kind | sites | upper-case member | distinct owners |
|---|---|---|---|---|
| CALLS | enum | 142 | 135 (tuple variants) | 40 |
| CALLS | struct | 77 | 0 | 38 |
| CALLS | type_alias | 34 | 0 | 3 |
| CALLS | no owner node | 174 | 13 (`use super::*` in tests, `Cow::Borrowed`) | - |
| REFERENCES | enum | 318 | 318 (unit variants, `E::V` as a value) | 50 |

The 7 lower-case enum members are associated functions (`ContextMode::default`,
`Error::from`), not variants. Of the 142 enum `CALLS` sites, 90 are in the
enum's own file and 52 in another file. Top enums: `Error` 17 sites / 15
callers, `Match` 10 / 6, `ParseResult` 7 / 3, `lowargs::Mode` 7 / 7.

### What a user sees today

- `find_callers("ContextMode")`: nothing from constructions (after GM-594;
  before it, the 3 accidental `set_before/after/both` rows).
- `find_callees("default")`: an unresolved `Limited` row.
- `find_definition("Limited")` / `find_callers("Limited")`: nothing, the
  variant has no node.
- "Where is `Error::Io` constructed?" is answerable only by text search.

## 2. Options

Each option is described as: today / change / example / consequence.

### (a) No: keep ADR 0035's consequence, state the rule (Recommended)

- Today: as above.
- Change: none in code. ADR 0035's bullet gets the reason (a `CALLS` lands on
  the declaration of the thing called; a variant is not its enum) and the
  route if constructions are ever wanted (variant nodes, option d).
- Example: `find_callers("Error")` lists no constructions;
  `find_callees("default")` keeps the unresolved `Limited` row, which names
  what is called.
- Consequence: constructions stay invisible to the graph (135 sites / 40
  enums on ripgrep, plus 318 unit-variant uses). No wrong rows, no partial
  lists. Python `Foo()`, TS `new Foo()` and Rust `Logger(())` still land on
  their class/struct, which is consistent under the same rule: there the
  type is the constructor's declaration.
- Before / after GM-618: correctly bound `E::V` sites are never open, so
  GM-618 does not touch them. The few that are open (a prefix the extractor
  cannot resolve, `x::y::E::V`) are asked at the path's start today
  (rust-analyzer answers a module, or, before GM-594, the enum at its name);
  after GM-618 they are asked at `V`, land on the enum away from its name and
  are dropped. GM-618 makes (a) uniform: no variant call ever links.
- GM-614: independent (nothing new lands).

### (b) Yes, in the Rust extractor: resolve `E::V` to the enum

- Today: `tail_in` emits a `qualifiedName` placeholder for `E::V`.
- Change: in `resolve_type_qualified`, when `T` is an enum declared in this
  file (needs `DeclRef` to carry the native kind), the member is upper-case
  and `lookup_tail("E::V")` found nothing, emit `Bound::Here(enum id)` for a
  `CALLS`. A cross-file enum would need a placeholder keyed by `E`'s
  `qualifiedName` and a core change: D4 keeps structural `CALLS` on
  `Function` only, and core cannot tell "enum" from "struct" without a
  language-specific `nativeKind` check or a new wire flag.
- Example: `find_callers("ContextMode")` lists `default`, `set_before`,
  `set_after`, `set_both`; `find_callees("default")` shows `ContextMode`
  instead of `Limited`.
- Consequence: same-file only, 90 of 142 sites (63%); `find_callers(Error)`
  would silently miss the constructions in other files (52 on ripgrep), the
  partial-list problem GM-594 removed. With the core change, all 142, at the
  price of a structural kind-rule exception keyed on a Rust notion. Either
  way `find_callees` loses the variant's name (the row says `ContextMode`,
  not `Limited`), and the edge says "calls the enum", which no Rust code
  can do. Unit variants (`E::V`, REFERENCES) and struct variants
  (`E::V { .. }`) stay as they are, so the list is "tuple-variant
  constructions" only.
- Before / after GM-618: independent for resolvable sites (no open site is
  involved); unresolvable-prefix sites behave as in (a).
- GM-614: independent (structural edges, no bridge landing).

### (c) Yes, in the bridge: accept a `CALLS` landing inside an enum

- Today: `E::V` sites are not asked; an answer at a variant would be dropped
  by `lands_on_name`.
- Change: two parts. The extractor writes an open site (with `replaces` =
  the `E::V` placeholder edge) for every type-qualified call with an
  upper-case member, positioned at `V`; `questions` must ask it although
  `unsettled_hop` only handles name keys; `record_answer` accepts a `CALLS`
  location strictly inside a node whose `nativeKind` is `"enum"`.
- Example: as (b), all 142 sites, cross-file included.
- Consequence: the first language-specific (`nativeKind == "enum"`) rule in
  shared bridge code, which GM-594 avoided; a deliberate exception to D1's
  encloser rule; +142 questions per ripgrep pass against `max_sites`, and
  rust-analyzer time. It answers only while a semantic pass ran; without one
  the edge is absent, so the list varies with the tier's state.
- Before / after GM-618: depends on GM-618 for the position (the open site
  must sit at `V`; before GM-618 the extractor would ask at `E` and get the
  enum at its name, the GM-594 bug made a feature).
- GM-614: **dependent and conflicting**. GM-614 tightens the enclosing-node
  rule for `REFERENCES`/`CALLS`; (c) is a carve-out of that same rule and
  would have to be written after it, against its final shape.

### (d) Variants as nodes (follow-up, not this task)

- Change: the Rust declarer emits a node per variant
  (`qualifiedName` `mod::E::V`, kind `Function` for a tuple variant, rustc's
  own view of it, `Variable` for a unit variant; struct variants as `Type`
  or not at all). The existing `E::V` placeholders, which already carry
  exactly that key, then link structurally in every file with no linker or
  bridge change; rust-analyzer answers would land at the variant's own name.
- Consequence: `find_definition`/`find_callers`/`find_references` on
  `Limited` work; `find_callers(enum)` still lists no constructions (they
  belong to the variant), unless a later rule rolls members up. Cost: +N
  nodes per enum in every outline, golden and benchmark count, a new
  `nativeKind`, and a decision on node kinds. Its own task and ADR.

## 3. Decision

- **D1 (recommended: (a)).** No `CALLS` from `Enum::Variant(..)` onto the
  enum. Rule: an engine or structural `CALLS` lands on the declaration of
  what is called; a type is that declaration only when it is its own
  constructor (tuple struct, class). If the owner wants construction sites,
  open (d) as a follow-up task; (b) and (c) are rejected for the reasons in 2.

If the owner picks (b) instead, the edit map and controls are in section 5;
(c) is not mapped because of its GM-614 dependency.

## 4. ADR 0035 diff (for option a)

In `docs/adr/0035-engine-calls-answers-kind-rule.md`, Consequences, replace
the bullet

```
- Rust tuple-variant constructors (`Enum::Variant(..)`) have no node and land
  on the enum away from its name, so they stay unlinked. (GM-594: the 3 that
  linked on ripgrep came from a unit-variant path pattern over-binding the
  enum's name, fixed in the Rust plugin.)
```

with

```
- Rust tuple-variant constructors (`Enum::Variant(..)`) stay unlinked, by
  decision (GM-619): a `CALLS` edge lands on the declaration of what is
  called, and a variant is a member of its enum, not the enum. A tuple
  struct or a class links because the type is its own constructor. The
  extractor keys the call by the variant's `qualifiedName`, which no node
  has; a semantic answer at the variant lands on the enum away from its name
  and is dropped (D1). On ripgrep that is 142 `CALLS` sites onto 40 enums.
  Construction sites would come from variant nodes of their own, not from a
  landing rule on the enum. (GM-594: the 3 that linked on ripgrep came from a
  unit-variant path pattern over-binding the enum's name, fixed in the Rust
  plugin.)
```

No change to Decision D1-D4.

## 5. Edit map and controls, only if the owner picks (b)

Same-file scope only (no core change); cross-file needs a separate decision
on D4.

- `plugins/rust/src/extractor/model.rs:40-46` `DeclRef`: add
  `native_kind: &'static str` (or `is_enum: bool`); set it in
  `FileModel::declare` / `declare_member` (model.rs:99-118) and the four
  decls.rs builders (`Declarer::fields` 401, `positional_fields` 446,
  `declare` 546, `mod_item` 607).
- `plugins/rust/src/extractor/bodies.rs:1225-1282`
  `Bodies::resolve_type_qualified`: after the container/type is known and
  before `member_of`, if `looks_like_type(member)`, the member is not all
  upper-case (an associated const), `lookup_tail(container, "T::member")` is
  `None`, and `lookup_name(container, T, Some(Type))` is an enum, return
  `Bound::Here(enum id)`. Needs the edge kind passed down (only `CALLS`;
  `REFERENCES` to a unit variant keeps its placeholder unless decided too):
  `resolve_path` (bodies.rs:1176-1219) and its callers gain a parameter.
- ADR 0035: replace the bullet with "same-file tuple-variant constructions
  produce `CALLS` onto the enum; cross-file ones stay unlinked".

Behaviours to pin, one control each:

1. Same-file `E::V(..)` -> resolved `CALLS` onto `E`. Control: drop the new
   branch in `resolve_type_qualified`; the test fails.
2. `E::new(..)` on the same enum keeps its `E::new` target. Control: remove
   the `lookup_tail` precondition; the test fails.
3. `E::MAX` / `E::CONST(..)` (all upper-case member) keeps its placeholder.
   Control: remove the all-upper-case check.
4. A struct `S::Assoc(..)` with an upper-case member keeps its placeholder.
   Control: replace the enum check with "is a `Type`".
5. Cross-file `use a::E; E::V(..)` stays a `qualifiedName` placeholder.
   Control: none needed beyond 1 unless the cross-file variant is chosen.

Measure (verify): ripgrep `CALLS` onto enums 0 -> about 90, no
`CALLS` onto an enum from a lower-case member.
