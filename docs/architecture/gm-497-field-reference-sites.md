# GM-497: typed Rust field reads as open sites the bridge still asks

GM-497 makes a typed named-field read `x.f` emit a `REFERENCES` edge to
`T.f` and record its open site with `replaces = Some(edge)`, the way
`x.m()` already does (`Bodies::receiver_call`). S1 stopped on a bridge
finding. This note decides how the SDK bridge tells such a site from a
GM-325 re-export hop.

## 1. The problem

`plugins/sdk/src/lsp/bridge.rs` `questions` (939-1028) routes every
`OpenSiteKind::Reference` site that has `replaces` through the hop rule
(971-982). `unsettled_hop` (1043-1060) asks only when the replaced edge
lands on a `pending_symbol` placeholder with `TargetScope::File` +
`TargetKey::Name`, the file is indexed, and it declares nothing of that name.
A typed field edge from `Bodies::edge` is one of two shapes:

- `Bound::Here`: a resolved edge onto the field node, not a placeholder.
  `unsettled_hop` returns false.
- `Bound::There`: a `pending_symbol` placeholder from `tail_in`
  (bodies.rs 1271-1283). Its scope is `Container`, its key is
  `QualifiedName("…T.f")`. `unsettled_hop` returns false.

So if the Rust plugin recorded `Reference` + `replaces`, every typed field
site would be skipped. It would never be asked and would get no R2 agreement
and no contradiction retract. Today these sites are `Reference` with
`replaces: None`, and they are asked. The change would swap a checked
semantic answer for an unchecked structural guess.

`record_answer`'s `Ask::Definition` arm (2405-2560) handles any site that
carries `replaces` correctly: R2 agreement (`lands_on_it ||
prospective == replaced`), upheld or confirmed, contradiction with retract,
and empty or ambiguous answers upholding R1. None of those paths reads
`site.kind`. The only kind checks are for `ReceiverCall` (the untyped
overload path, `untyped_call_answered`, `trim_untyped_calls`) and for
`OverloadCall`. `Answers::settle` (1487) is kind-agnostic for R1/R3. **The
only thing that has to change in the SDK is how `questions` routes the
site.**

## 2. Options

### (a) A new kind, `OpenSiteKind::ReceiverField`. Recommended.

`x.f` read through a receiver becomes a variant of its own, the
field-read twin of `ReceiverCall`. `questions` asks it unconditionally with
`Ask::Definition`, as it asks `ReceiverCall`.

- **Wire and protocol: no impact.** `OpenSite` is SDK-internal: it lives
  in `FileGraph.open_sites` inside the plugin process and is never
  serialized to core. `wire/` has no reference to open sites
  (grep), and `CURRENT_PROTOCOL_VERSION` (wire/src/lib.rs:47, = 2) is
  untouched. Go has its own `openSite` type (`plugins/go/open_sites.go`)
  and does not use the SDK enum.
- **TypeScript hop: provably unchanged.** The `Reference` arms of
  `questions`, `unsettled_hop` and the overload-dedup `retain` (989-998)
  are not touched. TypeScript never emits the new kind. The four existing hop
  tests (bridge.rs 3721-3890) keep their expectations byte for byte.
- **The gm-325 §4.3 premise still holds.** It says "`Reference` with
  `replaces` is recorded by no other plugin", and that stays true.
- **Overload-dedup `retain`:** it is not involved. It matches
  `kind == Reference`, and Rust records no `OverloadCall` anyway.
- **Cost:** a new public SDK enum variant. The only exhaustive `match` on
  `OpenSiteKind` is `questions` (grep and g-mesh, §6), so the compiler
  forces the one routing edit. Every other consumer compares with `==`
  against `ReceiverCall`, `Implementation` or `OverloadCall`. One existing
  Rust extractor test filters `kind == Reference` for an `x.f` read
  (tests.rs:1634) and needs updating.
- **Enum design rule.** The enum doc says each variant is a distinct
  question shape. `ReceiverCall` and `Reference` already share
  `definition`. The variant names *what the site is* (a member reached
  through a receiver), and that is exactly what separates it from a
  path/name `Reference` and from a hop.

### (b) A narrower filter in `questions`. Rejected.

Under (b), the hop rule applies only when the replaced edge lands on a
`pending_symbol` File+Name placeholder. Any other `Reference` + `replaces`
site gets `Ask::Definition` with the GM-489 rules.

Rejected for these reasons:

- **It changes a pinned SDK contract.** The table in
  `a_hop_is_asked_only_while_the_linker_cannot_settle_its_placeholder`
  (bridge.rs 3721-3797) asserts **0 questions** for "a container scope",
  "a qualified-name key" and "a reexport placeholder", and likely for "the
  edge is missing" too. A Rust `Bound::There` field edge *is* a
  container-scoped, qualified-name-keyed `pending_symbol` placeholder. So
  (b) cannot ask Rust field sites without flipping exactly those cases to
  1. TypeScript never produces those shapes: `keys.rs:118 file_target` is
  its only placeholder target, and it is File+Name. The real TS behaviour
  would therefore stay the same, but the SDK's documented hop semantics
  would not.
- **Implicit classification.** The question a site gets would be inferred
  from the shape of the edge it replaces, not stated by the extractor. A
  future plugin that records `Reference` + `replaces` onto a File+Name
  placeholder for a non-hop reason would silently get the hop rule.
- **More to update.** The gm-325 §4.3 premise ("recorded by no other
  plugin") becomes false and needs rewriting. The dedup `retain` would then
  also cover non-hop sites, which is harmless today but not what it was
  written for.
- **What it would save:** one enum variant plus about 3 doc lines. That
  does not outweigh the contract change.

## 3. Edit map

### SDK, `plugins/sdk`

| Where | Change |
|---|---|
| `src/graph.rs` 95-125 `enum OpenSiteKind` | Add `ReceiverField` after `ReceiverCall` (97-100). Doc: "`x.f`, a named field read through a receiver. `replaces` names the structural `REFERENCES` edge when the plugin typed the receiver; asked like `ReceiverCall`." |
| `src/graph.rs` 168-188 `OpenSite::replaces` doc | Mention typed receiver calls *and* typed field reads as the shapes that carry it, besides Go's `placeholderCall` and the TS hop. |
| `src/lsp/bridge.rs` 983 (`questions`) | `OpenSiteKind::ReceiverCall \| OpenSiteKind::ReceiverField \| OpenSiteKind::Reference => Ask::Definition(..)`. The arm at 971 stays `Reference if replaces.is_some()`. |
| `src/lsp/bridge.rs` 189-207 (struct doc, "What it asks") | One line saying a typed `ReceiverField` site follows the same retraction rules as a typed `ReceiverCall`. |
| `src/lsp/bridge.rs` 299-310 ("# Re-export hops") | Add "a `ReceiverField` site with `replaces` is not a hop". |
| `src/lib.rs:99` | No change. The variant is re-exported with the enum. |

Not changed, read for context: `record_answer` 2405-2560 (R2 at
2461-2481, contradiction at 2509-2516), `Answers::settle` 1487-1542,
`untyped_call_answered` 2586-2597, `trim_untyped_calls` 2618+,
`fold_untyped_calls` graph.rs 618-637 (`ReceiverCall` only, so an `x.f` is
never folded into `untypedCalls`, as today), `unsettled_hop`, and the
`retain` at 989-998.

### Rust plugin, `plugins/rust/src/extractor/bodies.rs`

| Where | Change |
|---|---|
| `field_access` 878-899 | After `self.visit(value, ..)`: `typed = self.receiver_type(value, module, block).filter(\|ty\| ty.wrapper == Wrapper::Plain)`; `replaces = typed.and_then(\|ty\| self.edge(self.field_of(&ty.container, &ty.name, name, module), EdgeKind::References, from, field))`; `self.open_site(from, field, name, module, OpenSiteKind::ReceiverField, EdgeKind::References, replaces)`. The `self.f` branch (888-895) and the `x.0` early return (883-886) are untouched. |
| Module doc table 13-16, Decision 7 50-51 | `x.f`, `x` typed in this file: "`T.f`, as `self.f` is, and an open site that replaces it". Any other `x`: unchanged wording, kind `ReceiverField`. |
| Pattern to mirror | `receiver_call` 560-595. Address helpers `field_of` 1267, `tail_in` 1271-1283, `edge` 1306-1317, `open_site` 1326. |

**Hop count (differs from S1's planned edit).** S1 wrote `ty.hops + 1 <=
MAX_HOPS`. `receiver_call` applies no extra hop for the final `.m`. The
final `.f` resolves no written type: it only addresses `T.f`. So the
compounding risk that `MAX_HOPS` guards does not arise. I recommend parity
with `receiver_call` (no `+ 1`), so `x.a.b.f` is typed exactly when
`x.a.b.m()` is. See must-confirm M4.

**Untyped `x.f` uses the new kind too.** It becomes `ReceiverField` with
`replaces: None`, mirroring `ReceiverCall`. Its question and answer path are
identical to today's `Reference` / `None` (`Ask::Definition`; `record_answer`
does not branch on kind here).

### Existing tests that change

- `plugins/rust/src/extractor/tests.rs` 1630-1636: the filter becomes
  `kind == OpenSiteKind::ReceiverField`. If `ledger` is typed there, also
  assert `replaces.is_some()`.
- `plugins/sdk/src/lsp/bridge.rs` 3799-3826
  (`a_reference_without_replaces_and_a_receiver_call_are_asked_as_before`):
  extend it with a `ReceiverField` + `TheEdge` row that is asked. No
  existing expectation changes.

## 4. Behaviour list (for the tests slice)

Extractor (`Bodies::field_access`):

1. `x.f` with `x` typed to a same-file struct `T` with field `f` emits
   `REFERENCES from -> T.f` (resolved, `Bound::Here`). One `ReceiverField`
   site at `f` carries `replaces = Some(that edge id)`.
2. `x.f` with `T` in another file emits `REFERENCES` onto a `pending_symbol`
   placeholder with target `(Container(T's container),
   QualifiedName("…T.f"))`. The site's `replaces` is that edge's id.
3. `x.f` with `x` untyped (unknown local, generic, `dyn`, wrapped
   `Option<T>`, more than `MAX_HOPS`) emits no `REFERENCES` edge for `f`. One
   `ReceiverField` site carries `replaces: None`.
4. In `x.a.f` (both typed), each level gets its own edge and site: `X.a`
   from the inner `field_access`, then `A.f`.
5. `x.0` emits no edge and no site. The value is still visited.
6. `self.f` inside `impl T` is unchanged: an edge to `T.f` and **no** open
   site.
7. Same-named fields on two structs: `a.f` and `b.f` with `a: A`, `b: B`
   link to `A.f` and `B.f` respectively, never across. (No wrong target.)
8. A field and a method of the same name: `x.f` links to `T.f`, never to
   `T::f`.
9. The site's `edge_kind` is `References` and its `position` is the field
   name token.

Bridge (`questions` / `record_answer`):

10. A `ReceiverField` site with `replaces` is asked (`Ask::Definition`)
    whatever its replaced edge's target shape is: a resolved node, or a
    Container+QualifiedName placeholder.
11. A `ReceiverField` site without `replaces` is asked.
12. R2, same file: when the answer lands on the replaced edge's own target,
    nothing new is recorded and the structural edge is re-sent unchanged
    (one row, `source = syntactic`). Mirror
    `a_typed_call_its_server_confirms_in_the_same_file_stays_one_structural_edge`
    (tests/lsp_bridge.rs:2333).
13. R2, cross file: an answer whose prospective id equals `replaces` agrees
    and records nothing. A later empty pass does not lose the edge. Mirror
    `a_cross_file_call_the_server_confirms_survives_a_later_empty_pass`
    (2354).
14. Contradiction: an answer landing on another field node records the
    semantic edge and retracts `replaces`. Mirror
    `a_contradicted_call_is_restored_by_a_later_empty_pass` (2380).
15. Empty or ambiguous answer: the structural edge is upheld (R1).
16. TypeScript hop unchanged: the existing four hop tests (bridge.rs
    3721-3890) pass with no edits. A `Reference` + `replaces` site onto a
    Container or QualifiedName placeholder is still **not** asked.
17. A `ReceiverField` site never enters `untypedCalls`
    (`fold_untyped_calls`) and never counts in `untyped_answered`.

Controls (verify builds these): route `ReceiverField` to `continue` in
`questions` (a plain removal would not compile, because the match is
exhaustive), and 10-15 must fail. Revert `field_access` to `replaces: None`
with no edge, and 1, 2, 4, 7 and 8 must fail. Record the field site as
`Reference` + `replaces`, and 10 must fail (this is the S1 scenario).

## 5. Must confirm (before or in the code slice)

- **M1. R2 id equality for cross-file fields.** The bridge's
  `address_of(field node)` must produce the same placeholder id as
  `tail_in`'s target: the container, `qualifiedName` `…T.f`, and
  `key_path` = `qualified_path_in(container, field_tail_path(T, f))`
  (keys.rs:134, decls.rs:151). GM-489 §1.4 measured this only for methods
  (`T::m`). If the ids differ there is no wrong target, but every agreeing
  answer becomes a contradiction: a semantic edge plus a retract, which is
  the GM-489 two-row/lost-edge window. Pin it with behaviour 13. The code
  slice states the field node's `container`, `qualified_name` and
  `qualified_path`.
- **M2. rust-analyzer `definition` on a field use** lands on the field's
  declaration name, and `node_at` maps it to the field node, not to the
  struct. Confirm in the whole-repo diff slice with a real server.
- **M3. Which `field_expression` contexts reach `field_access`.** This
  covers the assignment LHS (`x.f = v`), `x.f += 1`, `&mut x.f`, and macro
  token trees (not parsed). `self.f` already emits `REFERENCES` for writes,
  so typed writes doing the same is consistent. The AC says "reads".
  Confirm, and list the contexts in the code slice's notes.
- **M4. Hop budget.** No `+ 1` for the final field (§3, parity with
  `receiver_call`) against S1's `+ 1`. This is the owner's call if parity
  is not accepted.
- **M5. Variant name.** `ReceiverField` (parallel to `ReceiverCall`) against
  the brief's `FieldReference`. The behaviour is the same either way.
- **M6. No other exhaustive `match` on `OpenSiteKind`.** The compiler will
  confirm this (the workspace build in verify), and so does a grep in the
  plugins that do not depend on the SDK (`plugins/go` has its own type).

## 6. Facts relied on, and where they came from

- `questions` has one caller, `<LspBridge as SemanticEngine>::answer`
  (bridge.rs:2749): g-mesh `find_callers(symbol_id = lsp::bridge::questions)`.
- `unsettled_hop` has one reference, `questions` (bridge.rs:938): g-mesh
  `find_references`, from S1.
- Writers and readers of `OpenSite.replaces` outside bridge.rs: g-mesh
  `find_references(graph::OpenSite.replaces)` found these:
  - `sdk/toy/main.rs` `extract`
  - python `Bodies::overload_call`, `Bodies::walk_receiver`
  - rust `Bodies::open_site`
  - TS `Declarer::record_placeholder_use_site`,
    `record_overload_call_sites`, `add_site`
  - `sdk/tests/lsp_bridge.rs` fixtures

  The bridge's own reads (`questions`, `settle`, `settle_overloads`,
  `conclude`, `record_answer`, `untyped_call_answered`,
  `trim_untyped_calls`) and `graph.rs fold_untyped_calls` came from grep on
  those two known files. That index's bridge.rs is still semantic-pending.
- Users of `OpenSiteKind`: g-mesh `find_references(graph::OpenSiteKind)`
  returned 13 files, all in `plugins/` (SDK, python, rust, typescript, toy),
  none in core or wire. Enum variants are not indexed as symbols:
  `find_references("OpenSiteKind::Reference")` returned only semantic
  neighbours. So the per-variant uses came from grep. `Reference` is emitted
  by rust `bodies.rs` (568, 893, 898, 934, 1034, 1122, all `replaces: None`
  except via `emit`, which never sets it) and TS `sites.rs` (110 with
  `replaces`, 145 without). Python emits none.
- Every TS placeholder target is File+Name: grep `TargetKey::|TargetScope::`
  in `plugins/typescript/src` finds only `keys.rs:118-121`.
