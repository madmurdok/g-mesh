# GM-485: method calls through a local whose type comes from a return value

Design note for GM-485/S1, on `release-3.20.0` at `041ab7b` (after GM-480 and
GM-481). Nothing here is implemented. Counts come from a throwaway counter over
the 270 tracked `*.rs` files and from the live index of the main checkout
(`~/.g-mesh/projects/959ade85d9a343b1/index.db`, same commit). The counter is
described under [How the counts were taken](#how-the-counts-were-taken).

## 1. What happens to `let shapes = semantic.shapes(); shapes.refused_by_all(name)` today

In short, **the call is not dropped and not mislinked. The plugin never emits
it. The structural tier records an open site, and this time the semantic tier
did not answer it.**

1. **Structural tier (plugin).** `Bodies::call` sends every
   `field_expression` callee to `Bodies::receiver_call`
   (`plugins/rust/src/extractor/bodies.rs`). That function resolves only
   `self.m()` inside an `impl`. Every other receiver gets
   `open_site(.., OpenSiteKind::ReceiverCall, EdgeKind::Calls)` and nothing
   else: no edge and no placeholder. This is the module's Decision 7, and
   `tests.rs::a_receiver_call_produces_no_edge_and_one_open_site` pins it
   even for a typed parameter (`fn run(p: P) { p.m(); }`). `Scopes` tracks
   only *names* (`HashSet<String>` per frame). It holds no types, so the
   plugin cannot tell a typed local from an untyped one.
2. **Open sites never reach core.** `plugins/sdk/src/run.rs::write_graph`
   says: "Open sites are not written - they are the semantic tier's, and core
   has no field for them." Only the plugin's LSP bridge reads them.
3. **Semantic tier (rust-analyzer).** On the live index, `find_definition.rs`
   holds 7 semantic edges. One of them is `by_semantic_neighbours →
   SemanticRung::shapes`, so the call that *produces* `shapes` was answered.
   The two calls on `shapes` were not. The daemon log shows why the run was
   fragile. After a wake-up, `find_definition.rs` was the first of 45 replayed
   files. Its per-file pass took **56.7 s** on a cold rust-analyzer and
   upserted 7 edges. `plugin.toml` documents that this server returns
   *empty* receiver-call answers for seconds after it reports progress, and
   the bridge believes the second empty answer. `language_state.rust` reads
   `semanticPassError = "not run - its plugin was asleep or
   memory-suspended"`. By contrast, `similarity.rs`'s
   `shapes.refuses(..)` (through a *parameter*) has semantic edges from an
   earlier pass. That is why `find_callers(QueryShapes::refuses)` lists only
   `similarity.rs`.
4. **What the response says.** `find_callers(QueryShapes::refuses)` returns 2
   rows, `hasMore: false`, `allUnresolved: false`, and
   `provenance: {language: "rust", semanticTier: "absent"}`. No
   `unlinkedUsages` field appears. The GM-477 probe walks unlinked
   *placeholders*, and an open site leaves none (see §4). The only signals are
   the call-level `semanticTier: absent`, which every Rust answer carries
   while the whole-project pass is incomplete, and the session instructions'
   "a method call through a variable receiver produces no edge in rust". Both
   apply to every call, so neither points at this one.

So GM-485 has two causes. The structural tier does not type receivers, by
design. The semantic tier that should cover them gave a partial answer and
left no trace. This note designs a fix for the first cause. The second is a
separate bridge question (decision D5).

**Fixture for the implement slice.** In one file, declare
`pub struct Q; impl Q { pub fn refuses(&self) -> bool {..} }`,
`pub struct A; impl A { pub fn shapes(&self) -> &Q {..} }` and
`pub fn run(a: &A) { let s = a.shapes(); s.refuses(); }`. Today `run` has no
`CALLS` edge to `Q::refuses`, and the graph holds 2 `ReceiverCall` open
sites. A second file with the same caller and `use crate::m::{A, Q}`
covers the cross-file placeholder path.

## 2. Counts on g-mesh's own sources

There are **24,478 receiver-call sites** (`x.m()`, excluding `self.m()` inside
an impl, the same set `receiver_call` turns into open sites). Of these,
**9,438 are candidates**: the method name is declared as a method somewhere
in the project, so a missing edge can hide a project caller. The other
15,040 sites call names only std or dependencies declare (`iter`, `map`,
`to_string`...), so no edge is the correct answer for them.
None of them gets a structural edge today. Of the candidates, 2,744 have
some `CALLS` edge to a same-named target from the same function, almost all
semantic (3,007 semantic Rust edges exist in total). This is an upper-bound
proxy for "linked today".

The table below covers candidate sites. The columns count the sites a
structural inference *would* link at each level (§3). Each level includes
the levels before it. "Same-file" restricts return and field types to
declarations in the caller's own file, which is what the per-file plugin can
see. "Any-file" also uses declarations in other files.

| Class (receiver's origin) | candidates | L1 | L2 same / any | L3 same / any | L4 same / any |
|---|---:|---:|---:|---:|---:|
| fn parameter with a written type (not in the task's list; the largest exact class) | 757 | 236 | 236 / 236 | 236 / 236 | 236 / 236 |
| `let` from a method return (**the GM-485 shape**) | 255 | 0 | 0 / 0 | 5 / 10 | 5 / 10 |
| `let` from `T::new()`/assoc fn | 1,532 | 0 | 144 / 307 | 144 / 307 | 144 / 307 |
| `let` from a free fn | 387 | 0 | 96 / 106 | 96 / 106 | 96 / 106 |
| `let x: T` / struct literal / `let y = &x` | 319 / 67 / 37 | 1 / 22 / 0 | same | same | same |
| struct field (`self.f.m()`, `x.f.m()`, `let y = x.f`) | 896 + 11 | 0 | 0 | 0 | 76 / 77 |
| chained (`a.b().m()`, `T::f().m()`) | 1,533 + 924 | 0 | 0 | 0 | 39 / 165 |
| closure / `for` params (untyped) | 657 + 80 | 0 | 0 | 0 | 0 |
| closure params with a written type | 38 | 7 | 7 | 7 | 7 |
| `?` / `unwrap()`/`expect()` wrappers (receiver or `let`) | 31 + 107 + 872 | 0 | 9 / 59 | 11 / 68 | 11 / 68 |
| generics / trait objects (`T: Tr`, `dyn`, `impl`) | 19 (+8 reached later) | 0 | 0 | 0 | 0 |
| other (destructuring 321, match/if-let bindings 97, `.await` 133, index 132, unbound 86, ...) | ~800 | 0 | 0 | 0 | 0 |
| **Total linked** | **9,438** | **266** | **515 / 738** | **522 / 757** | **637 / 999** |

How these break down:

- **Not linked but typed.** At L4 any-file, 1,259 sites have a receiver of a
  type the project does not declare (`ext`), where the right answer is no
  edge. 210 have a project type that has no method of that name (`nf`):
  trait default methods, trait impls named `<T as Tr>::m`, `Deref`. Another
  393 are `amb`: the type name is declared more than once in the project.
  The counter cannot pick between those declarations, but the plugin's
  `use`-based path resolution can. The rest, 6,550 sites, have a receiver
  whose type is unknown.
- **Mislinked.** The structural tier mislinks 0 sites today, because it
  emits nothing. To check the proposal, each simulated link was compared
  with rust-analyzer's edge from the same function to the same method name,
  where one exists. **The proposal contradicts rust-analyzer 0 times.** At
  L4 any-file, 919 links agree and 80 have no semantic edge to compare
  (the 2 GM-485 sites are among the 80). Of the 919, 17 agree only because
  rust-analyzer's target is the trait-impl method `<T as Tr>::m`. A
  `…::T::m` key would *miss* those, not mislink them (§3).
- **Tests.** 117 of the 637 same-file L4 links are in test files.

The gain is mostly **redundancy with the semantic tier**: 919 of the 999
links already exist when rust-analyzer has answered. The proposal is worth
doing because of the cases where rust-analyzer has not answered: it is
absent, cold, asleep, or gave a partial pass, as in GM-485. In those cases
the 637 same-file links are the difference between an empty caller list and
a correct one.

### How the counts were taken

A scratch binary (`rcount`, tree-sitter 0.25.10 and tree-sitter-rust 0.24.2,
the plugin's versions) parses every tracked `*.rs` file twice:

1. **Declarations.** For each type and method it records the
   `(Type, method) → return type` pair, with `Self` substituted. Methods come
   from inherent and trait impls, keyed by the impl's self type. It also
   records free fn return types, named struct field types, and how often each
   type name is declared.
2. **Bodies.** It keeps a scope stack modelled on `Scopes`, plus the origin
   of each binding. It classifies each receiver call by its receiver's
   syntax (identifier → origin of its binding; field; call; `?`; other). It
   then evaluates the receiver's type at L1–L4, with and without the
   same-file restriction. `?`/`unwrap`/`expect` unwrap `Option`/`Result`.
   `&`, `Box`, `Rc`, `Arc` and `Cow` are dereferenced only to look up a
   method.

`summarise.py` joins each site to the index by
`(filePath, enclosing fn name, nearest startLine)` and compares the result
with the semantic `CALLS` edges.

The counter has these limits:

- Methods and types are matched by bare type name, not by path, so `amb`
  sites are not counted as links.
- A `let` initialiser is re-evaluated in the scope of the call site, which
  matters only for shadowing.
- Calls inside macro token trees are invisible, as they are to the plugin.

The implement slice's real before/after count should be taken from the
plugin's output: `CALLS` edges whose open site has `replaces` set, grouped
by the same classes.

## 3. How far to infer

The proposal reuses the addressing that `T::f()` already has. Once a
receiver's type `T` is known, `receiver_call` calls
`resolve_type_qualified`/`member_of` exactly as for `T::m(x)`. The result is
the same-file `Bound::Here`, or a `qualifiedName` placeholder `…::T::m` in
`T`'s container. A wrong type guess therefore fails the way `T::m()` fails
today ("a wrong guess simply finds nothing", `resolve_type_qualified`). No
linker change is needed.

The open site **stays**, with `replaces: Some(<edge id>)`. rust-analyzer
still answers it, and it retracts the structural edge when it lands somewhere
else (`OpenSite::replaces`, `LspBridge` contradiction rule). When it does
not answer, the structural edge stands.

| Level | Typed from | Gain (same-file) | Wrong-link risk |
|---|---|---:|---|
| **L1** | A written type: `fn f(x: T)`, `x: &T`/`&mut T`, `let x: T`, `T { .. }` literals, typed closure params. `Self` resolves to the impl type. | 266 | **Lowest.** Inherent methods win over trait methods in Rust's lookup at the same autoref step, so when `T::m` exists it is the target. The exception is a trait implemented for `&T` itself (a blanket impl) whose method shares the name, which is rare. A generic parameter is already bound in `Scopes` and stays open. `dyn`/`impl Tr` stay open. |
| **L2** | `let x = T::f(..)` / `let x = f(..)` where `f` is declared **in this file** with a written return type (`T`, `Self`, `&T`), plus `?`/`unwrap()`/`expect()` on `Option<T>`/`Result<T, _>`. | +249 | Low. The return type is read from the declaration, never guessed by name (no "`new` returns `Self`" heuristic). A project type named `Option` or `Result`, or an alias whose first argument is not the payload, could unwrap wrongly. Unwrap only when the head resolves to nothing the project declares (std). |
| **L3** | `let x = recv.m(..)` where `recv` is typed by L1/L2 and `T::m` is declared **in this file** with a written return type. This is the GM-485 shape. | +7 | Low. It is the same rule applied one hop further. Bound the depth (2 hops). |
| **L4** | Struct fields (`x.f.m()`) using the field's written type, and chains (`a.b().m()`) using L3's rule without a `let`. | +115 | Low to medium. A field type behind a generic (`Vec<T>`, `Option<Box<T>>`) needs the same unwrapping rules, and `Deref` impls on project types are not modelled (that gives a miss, not a wrong link). |
| **X** (cross-file) | L2–L4 using return/field types declared in **other** files. | +362 (999 − 637) | The extraction contract is per file, so this needs a core feature: a two-hop key ("member `n` of the return type of `T::m`"). It also needs the incremental tracking described in §5. Deferred. |
| out of scope | Untyped closure / `for` / pattern / destructuring bindings, generics, trait objects, `impl Trait` returns, macros. | – | Stays an open site. Marked as described in §4. |

**References, Option/Result and Self.**

- Strip `&`, `&mut` and lifetimes.
- `Self` is the impl's `BlockCtx::self_type`.
- Dereference `Box<T>`, but **not** `Rc<T>`/`Arc<T>`. Their own `clone`,
  `as_ref` and `downgrade` would otherwise be keyed to `T` (decision D2).
- Unwrap `Option`/`Result` only through an explicit `?`, `unwrap()` or
  `expect()`. Never unwrap through `map`, `and_then` or `if let Some(x)`.
- Trait-impl methods are named `<T as Tr>::m` by this plugin, so the key
  `…::T::m` misses them. This happened at 17 measured sites. It is not a
  wrong link, and the open site stays for rust-analyzer (decision D3).

## 4. Interaction with GM-477's `unlinkedUsages`

- **Today's miss is not marked.** `unlinked::probe` looks for
  `pending_symbol` placeholders whose name (and second-to-last segment)
  match the anchor. A receiver call leaves an open site, never a
  placeholder, and open sites never reach core (§1). The page is therefore
  indistinguishable from a complete one, apart from the call-level
  `semanticTier: absent`.
- **The proposal's own misses are marked for free.** A typed call that does
  not link leaves a placeholder keyed `…::QueryShapes::refuses`. This
  happens when the impl is in another module than the type, or for a trait
  method. The probe already discloses that placeholder on the
  `QueryShapes::refuses` page as a "may" usage. One caveat must be checked
  in the implement slice: the probe's second-to-last segment for an anchor
  `<T as Tr>::m` may not equal `T`.
- **Out-of-scope classes need a new marker.** They produce no placeholder,
  so GM-477 cannot see them. The options:
  - **M1, receiver-call side table (recommended).** The plugin sends, per
    file, the list of `(fromId, name)` pairs for receiver calls it could not
    type. That is a new `FileGraph` field, written to the NDJSON and stored
    in a new table. The finished design should replace that table in step
    with the file's other rows. `find_callers`/`find_references` on a
    *method* anchor then add
    `untypedReceiverCalls: {count, files, hint}`, counting rows whose name
    equals the anchor's bare name and whose `fromId` has no edge to the
    anchor yet. This is a "may", exactly like `unlinkedUsages`, and it
    would have named `find_definition.rs` for GM-485. Size: 16,512 rows of
    `(fromId, name)` on g-mesh, against 47,028 Rust edges, with no nodes and
    no edges added. Noise: common names (`get` 725, `push` 485, `len` 345
    candidate sites) produce long tallies. The `files` cap (20) and the
    "has no edge from this fn yet" filter bound the noise. After a complete
    semantic pass, most rows are filtered out.
  - **M2, placeholder per untyped call.** A name-only `pending_symbol` per
    `(file, name)` would add 7,429 nodes (+40 % of Rust nodes) and 16,512
    edges (+35 %) on g-mesh, and it would put fake unresolved edges into
    `find_callees`. Rejected.
  - **M3, wording only.** Keep `semanticTier: absent` and make the
    instructions/provenance hint name the classes ("calls through untyped
    locals, fields, chains and closure params may be missing"). This is
    cheapest and changes no answer. It does not tell an agent *which* page
    is incomplete, and that was GM-485's complaint.

## 5. Interactions

- **GM-470 (member/free-fn tie-break).** That rule applies to `name`-keyed
  placeholders, where core keeps the free fn because a module-scoped name
  never denotes a member. Typed receiver calls use `qualifiedName` keys
  (`…::T::m`), which are exact and never reach a free fn, so the tie-break
  never sees them. No change.
- **GM-480 (associated items out of the bare-name table).** This helps L2:
  `let x = f()` looks up `f` by bare name, and since GM-480 that lookup can
  no longer return a same-named method. L3 and L4 look up `T::m` through
  `lookup_tail` (`by_tail`), which `declare_member` still fills. The model
  needs one addition: each `DeclRef` must carry its written return type, or
  a field's written type. That data is recorded by `Declarer::declare`,
  `Declarer::mod_item` and `Declarer::fields`, the callers of
  `FileModel::declare`/`declare_member` (found with g-mesh).
- **Incremental reindex.** Same-file inference (L1–L4) depends only on the
  file being extracted, plus the type's container, which `use` resolution
  already supplies. When a return type changes in *another* file, the
  emitted keys do not change, and the existing placeholder relinking covers
  renames and moves of `T::m`. Cross-file inference (X) would make file A's
  edges depend on file B's signatures. Core tracks no such dependency, so a
  change to B would leave A's edges stale until A is reindexed. GM-476's
  stale-node work is the precedent. This is the main reason to defer X.
- **Index size.** At most one edge, plus at most one placeholder per
  `(file, target)`, per linked site. That is ≤ 637 edges (+1.4 % of Rust
  edges) and fewer placeholders, because placeholders are deduplicated per
  file. M1 adds 16.5k small rows.
- **Semantic tier.** Structural and semantic edges may both land on the
  same target. The bridge retracts `replaces` only when "the answer lands
  somewhere else". The implement slice must confirm that a confirming
  answer produces a single caller row and not a duplicate. Today the
  comparison is by edge id.
- **Capabilities and instructions.** `receiver_calls_structural` stays
  `"unresolved"`, because most receiver calls are still open. The
  instruction "a method call through a variable receiver produces no edge
  in rust" (`core/src/mcp/instructions.rs`) becomes false and needs
  rewording to "may produce no edge".

**Files a change touches.** These come from g-mesh calls, except where noted:

- `plugins/rust/src/extractor/bodies.rs`: `receiver_call`, which is called
  only by `Bodies::call`. Also `let_declaration`, `closure`,
  `for_expression`, `bind_let_condition` and `match_arm`, the callers of
  `Scopes::bind_pattern`, plus `member_of`/`tail_in`.
- `plugins/rust/src/extractor/scope.rs`: frames go from a set of names to a
  map from name to an optional type address. `Scopes` is referenced from
  `bodies.rs`, `scope.rs` and `mod.rs`.
- `plugins/rust/src/extractor/model.rs` and `decls.rs`: return and field
  types on `DeclRef`.
- `plugins/rust/src/extractor/tests.rs`:
  `a_receiver_call_produces_no_edge_and_one_open_site` changes from "no
  edge" to "an edge plus an open site with `replaces`".
- The `plugins/rust/conformance` fixture and its expectations, and
  `core/src/mcp/instructions.rs` (found by grep).
- For M1: `plugins/sdk/src/graph.rs` (the `OpenSite`/`FileGraph` field),
  `plugins/sdk/src/run.rs::write_graph`, core storage schema/write, and
  `core/src/mcp/unlinked.rs` or a sibling. Its callers are
  `find_callers_callees::handle_callers_in` and
  `find_references::handle_in`, by g-mesh.

## 6. Alternatives, risks, recommendation

| Option | Benefit | Risk / cost |
|---|---|---|
| **A. Do nothing structurally; fix the bridge** (D5) | No new edges and no wrong-link surface. | Leaves every machine without a warm rust-analyzer at 0 receiver edges. GM-485 recurs whenever a pass is partial. |
| **B. L1 only** | 266 links, near-zero risk, small diff. | Misses the GM-485 shape itself, which is L3. |
| **C. L1–L3 same-file (recommended), plus L4 fields/chains if cheap** | 522–637 links including GM-485, no core change, no cross-file dependency, every miss a placeholder that GM-477 already discloses. | Reverses Decision 7 and its test. Duplicate structural and semantic rows must be ruled out. A trait-impl method (`<T as Tr>::m`) is still missed. |
| **D. Cross-file (X)** | 999 links. | A core two-hop key, plus signature-dependency tracking for incremental reindex. A larger and riskier task. |
| **E. Marker only (M1), no inference** | Every page with a possible receiver-call miss says so. | Adds no edges. Noisy for common names. |

**Recommendation.** Do C (L1–L3 same-file, with `Box` deref and explicit
`?`/`unwrap`/`expect` unwrapping), and add M1 as its own task, because it is
what makes the remaining 89 % of candidate sites honest. Defer D. File D5 as
a bridge task: a cold first pass answered `SemanticRung::shapes` but left
`shapes.refused_by_all` empty.

Acceptance mapping for the implement slice:

- The fixture from §1, with a control: revert the `receiver_call` change,
  and the `Q::refuses` caller must disappear.
- Zero contradictions with rust-analyzer on g-mesh, using the agreement
  check above.
- A before/after count of linked receiver calls by class.
- Classes left out of scope get M1 (or M3, if the owner declines M1).

### Decisions for the owner

- **D1.** Reverse Decision 7 for typed receivers: emit a structural edge
  plus an open site with `replaces`? This is the premise of everything else.
- **D2.** Which wrappers to dereference: `&`/`&mut`/`Box` only (proposed),
  or also `Rc`/`Arc` (+ a few links, and a risk for `clone`/`as_ref`)?
- **D3.** Trait-impl methods. Accept the miss (proposed). The alternative
  is a core linker rung that lets a `…::T::m` qualifiedName key also match
  a unique `<T as _>::m`, which is a linker change that GM-470/GM-474
  reviewers would need to see.
- **D4.** The marker for out-of-scope classes: M1 side table (proposed),
  M3 wording only, or none.
- **D5.** Open a separate task for the bridge's cold-start partial answers,
  which are the immediate cause of this specific miss?
- **D6.** Include L4 (fields and chains, +115 same-file) in the same task, or
  split it out?

## Evidence that g-mesh itself missed something during this work

- `find_callers(QueryShapes::refuses)` and `find_callers(refused_by_all)`
  answered `similarity.rs` only (the subject of this task), with
  `provenance.semanticTier: "absent"` and no `unlinkedUsages`.
- `find_references("OpenSiteKind::ReceiverCall")` found nothing named that
  and fell back to semantic neighbours. Enum variants are not indexed
  symbols, so "what references the receiver-call kind" had to be answered
  through `find_references(OpenSiteKind)` (14 rows) and grep.
- `find_callers(FileModel::declare)` and `find_references(Scopes)` were
  ambiguous between the Rust and Python plugins. Re-querying by `symbol_id`
  resolved both, as the response's `explanation` instructed.
