# GM-488: typed Rust receivers through struct fields and call chains (L4)

Design note for GM-488/S1, on `release-3.21.0` at `ee6129e`. Nothing here is
implemented. It extends L1–L3 from
[GM-485](gm-485-local-receiver-types.md) §3 (level L4: "struct fields
(`x.f.m()`) using the field's written type, and chains (`a.b().m()`) using
L3's rule without a `let`", +115 same-file links measured: 76 fields, 39
chains). Decision D6 of that note split L4 into this task.

**Summary.** A receiver *expression*, not only a local, gets a type:
`receiver_type` learns `x.f`, `self.f`, `x.0` and `call(..)` / `call(..)?` /
`call(..).unwrap()`. A field's type comes from a new per-file side table of
written field types, filled where fields are declared, the same way L3's
return types are kept. **Plugin-only: no change to core, the wire format,
the storage schema, the SDK or the bridge.**

## 1. Where L1–L3 live today

All in `plugins/rust/src/extractor/` (1-based lines):

- `bodies.rs::receiver_call` (559–581) asks `receiver_type(value)`, keeps a
  `Wrapper::Plain` type, addresses the call with `member_of` (`T::m` in
  `T`'s container: `Bound::Here` same-file, else a `qualifiedName`
  placeholder), and emits the open site with `replaces: Some(edge id)`.
- `receiver_type` (682–689) handles only `identifier` (a typed local from
  `Scopes::type_of`), `&e` and `(e)`. Every other receiver, including
  `self.f`, `x.f` and any call, is `None`, so the site is untyped
  (`replaces: None`) and folds into the caller's `untypedCalls` (GM-486).
- `value_type` (693–704) → `peel_unwrap` (708–727) → `expression_type`
  (729–752) → `call_type` (757–808) → `returned` (812–816) type a `let`
  initializer. `call_type` already types `f()`, `a::f()`, `T::f()`,
  `Self::f()`, `self.m()` and `x.m()` with `x` typed, from a same-file
  declaration's written return type, counting hops against
  `typing::MAX_HOPS = 2`.
- `resolve_written` (628–671) reduces a written type: strips `&`/lifetimes
  (in `WrittenType::parse`), looks through `Box`, records `Option`/`Result`
  as a `Wrapper` (only when no project type of that name is in scope),
  refuses generics, `dyn`/`impl`, tuples and anything not declared or
  imported by name.
- Return types are **not on `DeclRef`**: `Declarer::declare` (decls.rs
  450–485) calls `Declarer::returns` (491–506) and stores the result in
  `FileModel::returns`, a side table keyed by node id (`set_returns`,
  model.rs 137–147; `returns` 150–152). The `Returns` value is
  `{ module: ModuleCtx, ty: WrittenType }`, i.e. "a written type plus the
  module its names resolve in".

Callers, from g-mesh: `find_callers(Bodies::receiver_type)` → `receiver_call`,
`call_type`, `expression_type` (and itself); `find_callers(Bodies::value_type)`
→ `let_declaration` only; `find_callers(Declarer::fields)` → `Declarer::item`
only; `find_callers(generic_names)` → `Declarer::returns` only.
`find_callers(FileModel::set_returns)` returned 0 rows, because the call is
`self.model.set_returns(..)` (a field receiver, exactly this task's gap);
grep found the one call at decls.rs:474.

## 2. Field written types reach the model

**What exists.** `Declarer::fields` (decls.rs 391–419), called from
`Declarer::item` (decls.rs 380–382) for `struct_item`/`union_item`, emits a
`Variable`/`field` node per *named* field and records it with
`declare_member(container, "T.f", DeclRef { id, kind })`. It returns early
for a tuple struct (`ordered_field_declaration_list`), which declares
nothing. The field's type is not kept anywhere.

**What to add.** A second side table on `FileModel`, next to `returns`:

```rust
/// Written types of this file's struct fields, by `(container, "T.f")` or
/// `(container, "T.0")`. `None` when cfg alternatives disagree.
field_types: HashMap<(String, String), Option<Returns>>,
pub(crate) fn set_field_type(&mut self, container: &str, tail: &str, ty: Returns);
pub(crate) fn field_type(&self, container: &str, tail: &str) -> Option<&Returns>;
```

- **Keyed by `(container, tail)`, not by node id**, because positional fields
  have no node. `set_field_type` has `set_returns`' conflict rule: a second,
  different type for the same key makes it `None`.
- **`Returns` is reused** as "written type + module" (a doc line says so; a
  rename is optional churn the implementer may skip).
- **`DeclRef` does not change.** The task text says "field types on
  DeclRef"; the L3 precedent put return types in a side table instead, and
  fields follow it.
- `Declarer::fields` fills it:
  - named field: `field_declaration`'s `type` child, key `field_tail(T, f)`;
  - tuple struct: the `ordered_field_declaration_list`'s type children in
    order (skip `visibility_modifier`/`attribute_item`), key `T.0`, `T.1`…;
    still no node and no `declare_member` for them;
  - `WrittenType::parse`, then `substitute_self(Some(T))` (a field may say
    `Box<Self>`), then refuse it when it `mentions` a generic parameter of the
    struct (`generic_names` gains `struct_item`/`union_item`, today it walks
    only `fn`/`impl`/`trait`). Stored with the struct's own `ModuleCtx`,
    where its names resolve.

**Storage and wire.** `FileModel` is built and dropped inside one
`extract` call. Nothing new is serialized: the output is still edges, nodes
and open sites of existing shapes. **Core, the wire format
(`WireNode`/`WireEdge`/`OpenSite`) and the schema do not change**, and
neither does the SDK. A rebuilt plugin changes its fingerprint, so the
language's staging reindex rewrites Rust edges (GM-489 §3.2 item 5).

## 3. Typing a receiver expression

`receiver_type(value)` becomes `receiver_type(value, module, block)`:

| `value.kind()` | Type |
|---|---|
| `identifier` | as today (`Scopes::type_of`) |
| `reference_expression`, `parenthesized_expression` | as today, recursing |
| `field_expression` | **new**: `field_type(value, module, block)` |
| `call_expression`, `try_expression` | **new**: `value_type(value, module, block)`, i.e. `peel_unwrap` + `call_type` |
| anything else (`self` alone, index, `.await`, macro, unary `*`) | `None` |

`field_type(node)`:

1. The owner: when `node.value` is `self` and `block` is an impl block
   (`family != TraitDecl`) whose `self_type` is a plain identifier, the owner
   is `type_address(self_type, module)` with hops 0. Otherwise the owner is
   `receiver_type(node.value)` and must be `Wrapper::Plain`.
2. `hops = owner.hops + 1`; refuse when `hops > MAX_HOPS`.
3. The field name: `field_identifier` text, or the `integer_literal` index.
4. `model.field_type(owner.container, field_tail(owner.name, name))`. Only
   this file's structs are in the table, so a type declared in another file
   finds nothing: same-file, as L1–L3.
5. `resolve_written(ty, ty.module, &|_| false)` (the generic check already
   ran at declaration). The `Wrapper` it returns is kept, so `o.opt.m()` on
   an `Option<T>` field stays untyped and `o.opt.unwrap().m()` /
   `o.opt?.m()` is typed through `peel_unwrap`.
6. `LocalType { origin: Origin::Field, hops, .. }`.

Consequences, all through existing code:

- **`self.f.m()`, `x.f.m()`, `x.0.m()`, `self.0.m()`**: `receiver_call` →
  `receiver_type` → `field_type`.
- **`a.b().m()`, `T::new().m()`, `f().m()`, `a.b()?.m()`,
  `a.b().unwrap().m()`**: `receiver_type` → `value_type` → `call_type`. This
  is L3's rule without the `let`; the method `b` must be a same-file
  `Bound::Here` with a written return type, exactly as for `let x = a.b()`.
- **Mixed nesting**: `self.f.b().m()`, `a.b().f.m()`, `x.f.g.m()` compose
  because `call_type`'s `field_expression` branch also calls the new
  `receiver_type`.
- **`let y = x.f; y.m()`** (and `&self.f`): `expression_type` gains a
  `field_expression` arm that calls `field_type`.
- **Depth.** Fields and method returns share one hop counter and
  `MAX_HOPS` stays 2 (`self.a.b.m()` and `a.b().c().m()` link;
  `self.a.b.c.m()` does not). The recursion itself is bounded by the syntax
  tree. See D1.
- **`receiver_call`'s own `self.m()` branch is unchanged**, and `self` alone
  is not given a type outside a field owner, so no L1–L3 site changes.
- **No double emission.** `receiver_call` still visits the receiver first;
  the inner call `a.b()` gets its edge there once. `call_type` only re-reads
  which declaration it lands on.
- **Box/&/Option/Result** follow L3 exactly: `&`, `&mut`, lifetimes and
  `Box<T>` are looked through; `Option`/`Result` come off only through one
  explicit `?`/`unwrap()`/`expect()`; `Rc`/`Arc` are not dereferenced
  (GM-485 D2). Deref impls are not modelled.

## 4. Bridge (GM-489) and the untyped-call marker (GM-486)

An L4 site changes from `replaces: None` to `replaces: Some(X)` and gains the
edge `X`. Nothing in the SDK changes; what each rule does with it:

- **`Answers::settle` (bridge.rs:1115), R1.** For a finished file it re-sends
  `X` unless every answer contradicted it. Same as for an L1–L3 site.
- **`record_answer` (bridge.rs:1580), R2/R3.** An answer on `X`'s target
  agrees and adds nothing (one caller row, `source = syntactic`); a
  same-target semantic edge from the same caller is covered and dropped. An
  answer elsewhere records the semantic edge and retracts `X`.
- **`untyped_call_answered` (bridge.rs:1713)** counts only `replaces: None`
  sites, so L4 sites stop counting toward the GM-486 trim. They have their
  own edge instead.
- **`FileGraphBuilder::finish` (sdk graph.rs:580–592)** folds only
  `replaces.is_none()` receiver calls into `untypedCalls`. L4 sites leave it
  at extraction time, without waiting for a semantic pass. Pages that showed
  them as a "may" row now show a real caller row.
- **What gets less visible.** When `X` is a placeholder that never links (the
  method is a trait-impl `<T as Tr>::m`, or an inherent impl lives in another
  module than `T`, or it comes through `Deref`), the call is no longer in
  `untypedCalls`. GM-477's `unlinkedUsages` discloses that placeholder on a
  `T::m` page, with GM-485 §4's caveat for `<T as Tr>::m` anchors. This is the
  L1–L3 behaviour, accepted then (GM-485 D3). S4 counts how many L4 sites end
  up there.
- `core/src/mcp/instructions.rs` already says "may produce no edge"
  (lines 144, 170): no wording change.

## 5. Wrong-target risks

| Risk | Why it does not mislink |
|---|---|
| Same-named fields on different structs | The key is `T.f` in `T`'s container, never a bare `f`. Test F7. |
| A field and a method of one name | Fields are in their own table, keyed `T.f`; methods are `T::m`. A call `(x.f)()` is not a field_expression callee and is untouched. |
| Shadowing | Locals come from `Scopes`, unchanged. `self` cannot be shadowed. The field type resolves in the struct's module, not the call site's, so a use-site `use` or generic of the same name cannot capture it. |
| Generic structs | A field mentioning a struct generic has no recorded type (`G<T> { t: T, o: Option<T> }`). Test N3 declares a real `struct T` beside it, so dropping the refusal would mislink. |
| Generic call-site parameters, `dyn`, `impl` | Unchanged L1 behaviour: they never type a local, so a field owner reached through them is untyped. |
| Trait vs inherent | Inherent `T::m` wins in Rust's lookup; a trait-impl method is `<T as Tr>::m`, so `T::m` misses it (placeholder, not a wrong link). |
| `Deref` on project types | `x.f` where `x`'s type has no field `f` finds nothing in the table: a miss. A wrapper that has the field itself is what Rust picks too. |
| `Option`/`Result` fields, a project type called `Option` | `resolve_written` keeps the wrapper (receiver must be `Plain`); a project `Option` is not unwrapped. |
| cfg alternatives of one struct with different field types | `set_field_type` conflict → `None`. |
| Trait default method bodies | `self` has no type in a trait declaration (`family == TraitDecl`). |
| Chains through std/derive methods (`x.clone().m()`) | `call_type` needs a same-file `Bound::Here` with a written return type. |

**Stays unresolved by design:** fields of types declared in another file and
chains through methods declared in another file (level X, needs core);
`Rc`/`Arc`/`Vec`/`HashMap` fields and anything indexed; `Deref`; `.await`;
`*x`; closure/`for`/pattern bindings; more than `MAX_HOPS` hops; tuple
values that are not tuple structs (`(A, B).0`); field *references* (see D2).

## 6. Edit map

Change (1-based lines at `ee6129e`):

- `plugins/rust/src/extractor/model.rs`: `FileModel` (76–86) gains
  `field_types`; add `set_field_type`/`field_type` next to `set_returns`
  (137–147) and `returns` (150–152); doc on `Returns` (52–55).
- `plugins/rust/src/extractor/decls.rs`: `Declarer::fields` (391–419)
  records named and positional field types; read `Declarer::returns`
  (491–506) for the parse/substitute/generic pattern.
- `plugins/rust/src/extractor/typing.rs`: `generic_names` (195–217) adds
  `struct_item`/`union_item`; `Origin` (155–163) adds `Field`; module doc
  (1–28) lists fields and chains; `MAX_HOPS` doc (34–37) says fields count.
- `plugins/rust/src/extractor/bodies.rs`: `receiver_call` (559–581, new
  signature and census shape), `receiver_type` (682–689, new arms and
  signature), `call_type` (757–808, passes `module`/`block` to
  `receiver_type`), `expression_type` (729–752, `field_expression` arm), new
  `field_type` beside `returned` (812–816).
- `plugins/rust/src/census.rs`: `typed_receiver` (165–169) and the
  `typed_receivers` key (94) gain the receiver shape (`Local`, `Field`,
  `Chain`); the printout (259–261) shows it. Add a refused-by-`MAX_HOPS`
  counter, and a TSV dump of every typed site (file, caller, method, shape,
  origin, target, `Here`/placeholder) to `GM314_OUT` for S4.
- `plugins/rust/src/extractor/tests.rs`: new tests (§7).

Read for context only: `bodies.rs` `value_type` (693–704), `peel_unwrap`
(708–727), `resolve_written` (628–671), `field_access` (821–839),
`type_address` (937–942), `self_member` (1182–1196), `member_of`/`field_of`/
`tail_in` (1201–1223), `edge`/`open_site` (1250–1287); `decls.rs::item`
(344–385); `scope.rs` `bind_typed`/`type_of` (104–119); sdk `graph.rs::finish`
(580–592); `bridge.rs` `settle` (1115), `record_answer` (1580),
`untyped_call_answered` (1713).

No change: core, `wire`, storage schema, SDK, bridge, the conformance
`expect.toml` (its only field/chain receiver calls are
`self.truncated_by.is_none()` on an `Option` field and `(**self).accept()`,
both still untyped; the implementer confirms by running conformance).

## 7. Test plan

Each test names its control: the code revert (never a test edit) that makes
it fail. Positive tests assert the edge target and that the site's
`replaces` is `Some`; negative tests assert no edge to the decoy and
`replaces: None` (or no `T::m` target).

| # | Fixture (one `src/lib.rs` unless noted) | Expect | Control |
|---|---|---|---|
| F1 | `struct Inner; impl Inner { fn m(&self) }`, `struct Outer { inner: Inner, boxed: Box<Inner>, r: &'a Inner }`; `fn run(o: &Outer) { o.inner.m(); o.boxed.m(); o.r.m(); }`; `impl Outer { fn go(&self) { self.inner.m(); } }` | `run`, `go` → `Inner::m`; sites `replaces: Some` | Drop the `field_expression` arm of `receiver_type`; separately, skip `set_field_type` in `Declarer::fields` |
| F2 | `struct W(Inner); impl W { fn go(&self) { self.0.m() } }`, `fn f(w: W) { w.0.m() }` | → `Inner::m` | Drop the positional branch in `Declarer::fields` |
| F3 | `impl A { fn new() -> A; fn b(&self) -> B; fn maybe(&self) -> Option<B> }`, `impl B { fn m(&self) }`, `fn make() -> B`; `fn run(a: A) -> Option<()> { a.b().m(); A::new().b().m(); make().m(); a.maybe()?.m(); a.maybe().unwrap().m(); None }` | all `m` → `B::m` | Drop the `call_expression`/`try_expression` arms of `receiver_type` |
| F4 | GM-486 shape: F3's `run` has no `m` in `untyped_calls` | `untyped_calls` lacks `m` | Same revert as F3 (`m` reappears) |
| F5 | Nesting: `self.f.g.m()` and `a.b().c().m()` link; `self.f.g.h.m()` and `a.b().c().d().m()` do not (one untyped site each) | 2 hops linked, 3 not | Set `MAX_HOPS = 3`: the 3-hop calls link (proves the cap stops them) |
| F6 | `let y = x.inner; y.m(); let z = &self.inner; z.m();` | → `Inner::m` | Drop the `field_expression` arm of `expression_type` |
| F7 | `struct X { inner: P }`, `struct Y { inner: Q }`, both `P` and `Q` with `fn m`; `fn run(x: X, y: Y) { x.inner.m(); y.inner.m(); }` | `P::m` and `Q::m`, nothing else | Key the table by bare field name: one mislinks |
| N1 | `struct O { opt: Option<Inner>, rc: Rc<Inner>, v: Vec<Inner> }`; `o.opt.m(); o.rc.m(); o.v.m();` | no `Inner::m` | Remove the `Wrapper::Plain` filter on the owner/receiver: `o.opt.m()` links |
| N2 | `src/a.rs` declares `Outer`/`Inner`; `src/b.rs` does `use crate::a::Outer; fn run(o: Outer) { o.inner.m() }` | `m` site untyped (only `Outer` is cross-file typed for `o`) | none needed: it pins the same-file rule; a lookup that ignored container would link it |
| N3 | `struct T; impl T { fn m(&self) }`, `struct G<T> { t: T, o: Option<T> }`; `fn run(g: G<u8>) { g.t.m(); }` | no `T::m` | Drop the generic refusal in `Declarer::fields`: links to `T::m` |
| N4 | trait `Tr { fn d(&self) { self.f.m(); } }` with a same-file `struct` holding `f` | untyped | Let `field_type` accept `TraitDecl` blocks |
| N5 | `#[cfg(a)] struct S { f: P }`, `#[cfg(not(a))] struct S { f: Q }`; `s.f.m()` | untyped | Make `set_field_type` keep the first type |
| N6 | `x.inner.clone().m()` (`Inner: Clone` by derive) | `m` untyped | none: `call_type` already requires `Bound::Here` |
| N7 | Trait-impl method through a field: `impl Tr for Inner { fn t(&self) }`; `o.inner.t()` | placeholder `Inner::t`, never `<Inner as Tr>::t` | (as `a_typed_receiver_finds_the_inherent_method_and_misses_a_trait_impl_method`) |

Existing tests that must keep passing unchanged:
`an_untyped_receiver_call_produces_no_edge_and_an_open_site_that_replaces_nothing`,
`untyped_receiver_calls_reach_the_enclosing_fn_and_typed_ones_do_not`
(`ps.first().unwrap().m()` stays untyped: `Vec`),
`wrappers_shadowing_generics_and_long_chains_leave_a_local_untyped`,
`struct_fields_are_nodes_and_their_uses_are_references`,
`a_getter_named_like_its_field_keeps_its_calls_and_the_field_keeps_its_references`,
and the plugin conformance suite.

## 8. Measurement plan (S4)

Same corpus discipline as GM-486 S17: `git archive` of the base commit
(`ee6129e`) into a scratch directory, the same corpus for every arm; record
`uptime` and `/usr/bin/time -p` with any timing.

1. **Plugin census, before/after (no daemon).** Run
   `GM314_CORPUS=<corpus> GM314_OUT=<dir> cargo test -p g-mesh-plugin-rust
   --lib census::run::open_site_census -- --ignored --nocapture` on
   `ee6129e` and on the S2 commit. Report CENSUS-TYPED by
   `(shape, origin, unwrapped, same_file)`, the refused-by-`MAX_HOPS` count,
   and the before/after totals. Expected order: about +115 `Here` sites
   (GM-485 §2: 76 fields, 39 chains), plus placeholder-addressed sites.
2. **Whole-repo edge diff.** Dump every `CALLS` edge the extractor emits
   (file, caller qualifiedName, target qualifiedName or placeholder key, the
   site's `replaces`) for both arms and diff:
   - **removed or changed: must be 0.** L4 only adds types where the old code
     returned `None`; any removal is a bug to report, not classify.
   - **added**: classify each against rust-analyzer. Oracle: the semantic
     `CALLS` edges of an index built by the *before* build with a completed
     semantic pass (check `semanticPassAt` set, no `semanticPassError`), so
     L4 sites were still untyped and got semantic edges. Edges carry no
     range, so join as GM-485 §2 did: same caller node, target with the same
     bare name. Classes: **agree** (same target), **contradict** (a semantic
     edge to a different same-named target and none to ours), **no oracle**.
     Every contradiction is read at its site and reported individually; the
     criterion is 0 that are real mislinks.
   - **typed but unlinked**: added placeholder edges that link to nothing
     after `link_all` (from a structural-only reindex of the after build,
     fresh `G_MESH_HOME`, `semantic_pass = false`). These left `untypedCalls`
     without gaining a caller row (§4).
3. **Marker effect.** `untyped_calls` row count before/after from the same
   structural-only reindexes (expected to drop by about the number of L4
   callers whose every untyped site of a name became typed).

Kill leaked daemons (`kill -9`) before each reindex arm.

## 9. Acceptance criteria

- *Field and chain fixtures are linked*: F1–F7, with controls.
- *No resolved edge moves to a wrong target (whole-repo diff classified)*:
  §8.2, removed/changed must be 0, added classified against rust-analyzer.
- *Before/after counts*: §8.1 and §8.2.
- *Tests with controls*: §7.

Within the criteria. No core/wire/schema change, so no other plugin or
reviewer is pulled in.

## 10. Decisions for the owner

- **D1. Hop budget.** Fields count as hops against `MAX_HOPS = 2`
  (proposed: one rule, conservative; S4 reports how many sites the cap
  refuses), or field hops are free (a field type is written, so its risk
  does not compound).
- **D2. Typed field references.** With `x`'s type known, `x.f` *read as a
  value* could also become a `REFERENCES` edge to `T.f` (today only
  `self.f` does, `field_access`). Proposed: not in GM-488 (its criteria are
  about `CALLS`); open a follow-up task if wanted.
- **D3. Generic structs.** Refuse a field type that mentions any struct
  generic (proposed, the same rule as `Declarer::returns`), or allow it when
  the generic is only an argument (`inner: Inner<T>` would link to
  `Inner::m`).

## g-mesh calls relied on

- `get_file_outline` on `typing.rs`, `model.rs`, `bodies.rs`, `decls.rs`
  (line ranges above).
- `find_callers(Bodies::receiver_type)` → `receiver_call`, `call_type`,
  `expression_type`, itself.
- `find_callers(Bodies::value_type)` → `let_declaration`.
- `find_callers(Declarer::fields)` → `Declarer::item`.
- `find_callers(generic_names)` → `Declarer::returns`.
- `find_callers(FileModel::set_returns)` → 0 rows (field-receiver call, the
  known gap); grep found `decls.rs:474`. Bridge, SDK fold, census, schema
  and instruction lines were located with grep in known files.

## Measured (S8)

Arms: before = `ee6129e` (release-3.21.0 base), after = `cb66ad9`
(`5d2ff21` + tests). Corpus: `git archive ee6129e`, 274 `.rs` files, the
same for every arm. Extractor edges were dumped by a scratch binary linking
each arm's `g-mesh-plugin-rust` (every `CALLS` edge: file, caller
qualifiedName, target qualifiedName or rendered placeholder, the open sites
whose `replaces` names it). Reindexes ran with fresh `G_MESH_HOME`s under
`/tmp`, rust plugin only; no leaked daemons were found or killed.

### Census (no daemon)

| shape | origin | same_file | before | after | delta |
|---|---|---|---:|---:|---:|
| Field | Field | false | 0 | 243 | +243 |
| Field | Field | true | 0 | 21 | +21 |
| Chain | FreeFnReturn | false | 0 | 27 | +27 |
| Chain | FreeFnReturn (unwrapped) | true | 0 | 1 | +1 |
| Chain | AssocFnReturn | true / false | 0 | 8 / 1 | +9 |
| Chain | MethodReturn | true / false | 0 | 4 / 3 | +7 |
| Chain | MethodReturn (unwrapped) | true | 0 | 1 | +1 |
| Local | Field (`let x = self.f`) | true / false | 0 | 4 / 2 | +6 |
| Local | all other origins | — | 1467 | 1467 | 0 |
| **total** | | | **1467** | **1782** | **+315** |

`Here` (same-file) sites 1159 → 1198 (+39); placeholder-addressed 308 → 584
(+276). Refused by `MAX_HOPS`: 88 (the before build has no counter). Open
sites (42519) and bridge questions (42528) are unchanged. The design's "+115
`Here`" was GM-485's count of sites that *link*, not of same-file sites: most
field types are declared in another file, so L4 lands them on placeholders.

### Extractor `CALLS` diff

Rows 11980 → 12272: **removed 0 edges**, added 295 (35 `Here`, 260
placeholder; every added edge carries a replacing receiver site). Three rows
differ in the replacing-site list only — same file, caller, target and
`resolved`; the edge now has two replacing sites where it had one, because a
newly typed site calls the same target from the same caller as an
already-typed one:

- `core/src/shim/router.rs` `only_sub_project_tool_answers_are_stamped` →
  `Fake::answer`: `session.front.answer(7)` (new, field) joins `a.answer(9)`.
- same caller → `Fake::reply`: `session.front.reply(..)` joins `a.reply(..)`.
- `core/tests/incremental_matches_full_reindex.rs` `assert_matches_full_reindex`
  → `IndexStore::into_inner`: `project.walk().into_inner()` (new, chain)
  joins `store.into_inner()`.

No edge was removed or retargeted; whether a shared `replaces` id is right
when both sites answer the same target is the bridge's existing rule (§4).

Oracle: `B_sem`, the before build with a completed semantic pass
(`semanticPassAt = 2026-10-04 08:31:00`, `semanticPassError` NULL; 274 files,
6511 edges upserted, 58 retracted). Joined by caller (file + qualifiedName,
all 295 found) and target bare name, semantic edges only:

| class | Here | placeholder | total |
|---|---:|---:|---:|
| agree | 33 | 253 | 286 |
| contradict | 0 | 0 | **0** |
| no oracle | 2 | 7 | 9 |

Typed but unlinked (structural-only reindex of the after build): 6 of 260
added placeholder edges link to nothing — `SessionHints::clone` ×3 and
`RelPath::clone` (derived `Clone`, no declared method),
`FileContainers::get` (a type alias of `BTreeMap`), `GMeshMcpServer::serve`
(a trait method from an external crate). None is a wrong edge.

### `untyped_calls` (structural-only reindexes)

before 16469 rows / 4297 callers → after 16189 / 4274 (−280 rows, −23
callers).

### Machine

Load was high (a concurrent full test suite): load averages 29.66 / 94.01 /
102.49 at start, 6.85 / 10.75 / 29.16 at the end. Reindex `time -p`:
A_struct real 397.3 user 1494.5 sys 10.7; B_struct real 407.4 user 1535.1
sys 10.4; B_sem real 544.1 user 1498.8 sys 11.5 (semantic pass 146.4s).
Counts do not depend on load; timings are not comparable to an idle run.
