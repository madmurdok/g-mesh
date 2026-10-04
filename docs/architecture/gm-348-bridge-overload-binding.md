# GM-348: overload binding through `LspBridge`

Status: design (S1). Decision: **the bridge can carry it**, with two
resolution strategies, because the two servers measured answer the question
in two different ways. TypeScript's server answers it with a position; pyright
answers it only in hover text.

## 1. What the servers actually return

The probe was a stdio LSP client: `initialize` with `definition.linkSupport`,
then `didOpen`, then the request at the callee token. It ran against
pyright 1.1.414, the version `plugins/python/package.json` pins, and against
typescript-language-server 6.0.1 on TypeScript 5.9.3. Both were installed
under the scratchpad, never under the repo. TypeScript 7.0.2, npm's current
`latest`, answered `null` to everything through tsls 6.0.1, so 5.9.3 is the
measured one.

Python fixture (`m.py`, zero-based lines):

```python
3  def f(x: int) -> int: ...      # @overload, ordinal 0
5  def f(x: str) -> str: ...      # @overload, ordinal 1
6  def f(x: Union[int, str]) ...  # implementation, ordinal 2
11/13/14  C.m: two @overload methods + implementation
17 a = f(1)   18 b = f("s")   19 c = C().m("s")   20 d = f
```

### pyright

| asked at | `definition` (also `declaration`, `typeDefinition`) | `hover` |
|---|---|---|
| `f(1)` 17:4 | `[3:4, 5:4, 6:4]` (all three, in source order) | `(function) def f(x: int) -> int` |
| `f("s")` 18:4 | `[3:4, 5:4, 6:4]` (identical) | `(function) def f(x: str) -> str` |
| `C().m("s")` 19:8 | `[11:8, 13:8, 14:8]` | `(method) def m(x: str) -> str` |
| bare `f` 20:4 | `[3:4, 5:4, 6:4]` | `(function)\ndef f(x: int) -> int: ...\ndef f(x: str) -> str: ...` |
| `f("s")` in another file (`use.py` 1:4) | `[6:4, 3:4, 5:4]`: implementation **first** | `(function) def f(x: str) -> str` |

Raw shape (trimmed): `[{"uri":"…/m.py","range":{"start":{"line":3,"character":4},"end":{"line":3,"character":5}}}, …]`.
These are plain `Location`s, even with `linkSupport`.

- `textDocument/definition` **does not** say which overload was bound. The answer is
  the whole set, implementation included, and its order is not stable across
  files. GM-299 found the same with pyright and `implementation`: it ignores
  what the client asks.
- `signatureHelp` inside the parens returns both signatures with
  `activeSignature: 0` for `f(1)`, `f("s")` and `C().m("s")` alike. That held
  with full `signatureHelp` client capabilities and with the cursor at the
  closing paren. It does not track the binding either.
- **`hover` at the callee is the only channel that carries the binding.**
  Hover at each overload's *own name* renders the same way, through the same
  printer (`g.py` probe):

  | hover at | text |
  |---|---|
  | decl `def g(x: int, y: int = 0)` | `(function) def g(\n    x: int,\n    y: int = 0\n) -> int` |
  | call `g(1)` | `(function) def g(\n    x: int,\n    y: int = 0\n) -> int` (exact match) |
  | decl `def g(x: Sequence[T])` | `(function) def g(x: Sequence[T@g]) -> T@g` |
  | call `g([1, 2])` | `(function) def g(x: Sequence[T@g]) -> T@g` (exact; generics are **not** specialised) |
  | decl method `m(self, x: str)` | `(method) def m(\n    self: Self@C,\n    x: str\n) -> str` |
  | call `C().m("s")` | `(method) def m(x: str) -> str`: bound, so `self` is dropped and the text is on one line |
  | implementation `def g(x, y=0)` | `(function) def g(\n    x: Unknown,\n    y: int = 0\n) -> Unknown` |

  Comparing against the declaration's *own hover* avoids the type-printer
  mismatch a comparison against the extractor's source `signature` would hit
  (`Union[int, str]` vs `int | str`, `T` vs `T@g`). One normalisation is still
  needed for methods: collapse whitespace, then let a call-site parameter list
  match a declaration list with its first parameter removed.

### typescript-language-server (tsserver 5.9.3)

Fixture `m.ts`: `f(x: number)` at line 0, `f(x: string)` at line 1, the
implementation at line 2. Class `C.m` is declared the same way at lines 5-7.

| asked at | `definition` | `signatureHelp` (inside parens) |
|---|---|---|
| `f(1)` 10:10 | **one** `LocationLink`, `targetSelectionRange` 0:16 (ordinal 0) | `activeSignature: 0` |
| `f("s")` 11:10 | **one** `LocationLink`, `targetSelectionRange` 1:16 (ordinal 1) | `activeSignature: 1` |
| `new C().m("s")` 12:18 | **one**, 6:2 (ordinal 1) | |
| bare `f` 13:10 | **three** links: 0:16, 1:16, 2:16 | |

Raw: `[{"originSelectionRange":{…10:10-10:11},"targetRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":37}},"targetUri":"…/m.ts","targetSelectionRange":{"start":{"line":0,"character":16},"end":{"line":0,"character":17}}}]`.
`textDocument/declaration` is `Unhandled method`. This is the same answer
`plugins/typescript/src/semanticPass.ts` measured against raw tsserver, so the
ordinal comes from containment, exactly as `declarationBindingAt` reads it
today. The server never returns the implementation for a call.

## 2. Decision

**In the bridge, not per plugin.** Only the bridge holds the LSP client, and
both strategies are generic: one is position containment, the other a
comparison of hover text against hover text. The language-specific part is a
manifest key.

**Site kind: `OpenSiteKind::OverloadCall`.** It refines a structural `CALLS`
edge; the existing kinds replace or contradict one. A plugin records it for a
call whose structural edge's target *may* be overloaded:

- `Bound::Here` onto a node that carries `declarations`: always;
- `Bound::There` (a placeholder): always. The plugin cannot know, so the bridge
  filters. Before asking, it drops a site unless its `replaces` edge's target is
  a node with `declarations`, or a placeholder whose target key names a node
  with `declarations` somewhere in the `SdkIndex`. The filter is built once
  per pass from the index. This is the analogue of the TS plugin's second
  filter. A dropped site is not counted `unanswerable`.

`replaces` is **required** for this kind and names the collapsed structural
edge. `edge_kind` is `Calls`.

**How the ordinal is chosen** (new `Ask::Overload`, first hop `definition`):

1. Map every returned location to `(file, node, ordinal)` by **containment in
   the target node's `declarations` ranges**, tightest match wins. This is
   `semanticPass.ts`'s `declarationBindingAt`. A node's own range is not enough
   here: Python's node range is the first `def`, and TS's is the
   implementation. Locations that land on different nodes leave the call
   unbound. That is the existing `agree` rule.
2. Exactly one ordinal, and it is not an implementation → bound. This is the
   tsls path.
3. Several ordinals of one node, and the manifest sets
   `overload_disambiguation = "hover"` → second hop: `hover` at the site and
   at the name of each **bodiless** candidate declaration. The declaration
   hovers are cached per `(node, ordinal)` for the pass. Normalise by
   collapsing whitespace. A candidate matches when its text is equal, or equal
   once its first parameter is dropped (a bound receiver). Exactly one match
   binds; zero or several leave the call unbound. This is the pyright path.
4. Never bind a `has_body` declaration of a set that has bodiless ones. Neither
   language lets a call bind its implementation, and a server pointing there
   is answering "the function", not "the overload".

**What an answer becomes.** A bound site records a `CALLS` edge onto the
usual pending-symbol placeholder addressed at the node, with
`to_declaration = Some(ordinal)`. `symbol_links`' repoint is an `UPDATE … SET
toId`, so the column survives linking (`core/src/graph/symbol_links.rs` ~1224).
The structural edge is handled **all or nothing per replaced edge**, as in
`semanticPass.ts`: it goes into `retract` only when *every* `OverloadCall` site
in that finished file naming it bound an ordinal. Otherwise it is re-sent (R1)
and that edge's bound edges are dropped from the diff and from `by_file`. Bound
edges are booked in `by_file`, so `retract_stale` withdraws a binding whose
call was deleted. Two settle rules change. R2 ("agreement adds nothing") must
not fire for `OverloadCall`, because the answer always lands on the structural
target. R3 coverage must never drop an edge whose `to_declaration` is `Some`,
because an unbound edge does not cover a bound one.

**Ids.** `answer_ids` and `Answers::record` take `Option<u32>` and pass it to
`ids::edge_id`. That function already hashes `Some(ordinal)` as a fourth field
and `None` as nothing, so **`ids.rs` is unchanged**, and so is the wire
(`WireEdge.to_declaration` exists). Two calls of two overloads from one caller
are two edges onto one placeholder with distinct ids. Go only hashes the
field and never fills it, so Go is untouched. `Recorded` gains `to_declaration`,
and `finish` writes it in place of the hard-coded `None`.

**Receiver calls.** A `ReceiverCall` answer whose target node has
`declarations` goes through steps 1-4 the same way, at no extra cost unless
the target really is overloaded. In scope (Q2).

## 3. Python extractor: `declarations` for `@overload` sets

**Today: none.** No caller of `NodeSpec::declarations` exists anywhere
(g-mesh, below). `Emitter::declare` (`emit.rs` 196) keeps the **first**
same-id definition and drops the rest. An `@overload` set is one node whose
range and `signature` are the first stub's, with no declaration list, so the
bridge has nothing to take the ordinal from.

The change is in S3. In `Declarer::declare` (`decls.rs` 275), every function definition
also yields a `WireDeclaration` candidate: its range (outer, decorators
included), `signature(outer)`, `has_body` = not decorated with `overload`, and
whether it is decorated with `overload` at all. The decorator test is `overload`,
`typing.overload`, or any attribute whose last segment is `overload`.
`Emitter` collects candidates per node id. At `finish`, a node whose
candidates include at least one `@overload` gets the list, ordinals assigned in
source order. A set with no `@overload` is a conditional definition or a
rebinding, gets no list, and behaves as today. The node row keeps the
first-wins rule (Q3). The SDK adds `FileGraphBuilder::set_declarations(id,
Vec<WireDeclaration>)`, because the list is only known after the last
redefinition, and the node is already pushed by then.

`Bodies::emit` (`bodies.rs` 781) then records an `OverloadCall` site beside
the edge it writes: for `Bound::Here` when `model` says the target has a list,
and for every call-shaped `Bound::There`. `.pyi` stubs contribute no
declarations (`mod.rs` §5), so stub-only overloads (much of typeshed) stay out
of reach.

## 4. Risks, trade-offs, scope

- **Hover is prose.** The pyright path depends on the printer rendering a
  declaration and a call the same way. Measured: it does for plain, defaulted
  and generic signatures, and drops `self` for bound methods. A pyright bump can
  break it. It fails **closed** (no match → unbound → structural edge stands),
  and the pinned pyright plus a real-server test turns a drift into a red test
  rather than a wrong edge.
- **Cost.** One `definition` per surviving site (the filter keeps only targets
  that are really overloaded), plus for pyright `1 + k` hovers per site, with the
  `k` declaration hovers cached per pass. Overload sets are rare. A
  project-wide `typing` overload user (pandas-style) is the worst case and is
  bounded by `Budgets::max_sites`.
- **Multi-file sets.** TypeScript can merge declarations across files.
  Containment maps each location to whichever file's node covers it, and
  locations that land on different nodes stay unbound. That is correct but
  incomplete.
- **Out of scope.** Rust and Go have no overloading and emit no
  `OverloadCall`. `.pyi`-only overloads. Exposing `toDeclaration` through MCP
  (`overload_call_resolution.rs` documents that it is not wired). Porting
  the TS plugin's own pass (GM-323). Binding of `@overload` on `__init__`
  through `C(...)`: that call is a `REFERENCES` to a class, not a `CALLS` edge.
- **Core tests.** `overload_call_binding.rs` and `overload_call_resolution.rs`
  drive the Node TS plugin and never touch the bridge. Nothing here edits
  them, and they keep passing. They become bridge tests only after GM-323.

ADR proposed: **0024 "A semantic tier refines an edge by binding a
declaration, all or nothing per edge"**, covering refine vs contradict and
when hover text is acceptable evidence. Not written yet.

## 5. Edit map

**S2: SDK bridge, graph, config** (`opus`):

| change | where |
|---|---|
| `OpenSiteKind::OverloadCall` + doc (refines, `replaces` required) | `plugins/sdk/src/graph.rs` 95-108 |
| `FileGraphBuilder::set_declarations` | `graph.rs`, beside `add_node` 449 |
| `overload_disambiguation: OverloadDisambiguation { None, Hover }` + manifest read + parse test | `plugins/sdk/src/lsp/config.rs` 198-260, 303-340, 425 |
| `Ask::Overload(site)`, `Ask::OverloadHover{…}`; `Question::method` | `bridge.rs` 702-736 |
| `questions`: route `OverloadCall`, apply the overloaded-target filter | `bridge.rs` 780-840 |
| `declaration_at(index, roots, encoding, loc) -> (RelPath, &WireNode, u32)` (tightest) | new, beside `node_at` 1330 |
| `record_answer`: `Ask::Overload` steps 1-3, return the hover follow-ups; `Ask::OverloadHover` joins via an `Answers` accumulator; ReceiverCall branch applies steps 1-4 | `bridge.rs` 1580-1712 |
| `Answers`: `to_declaration` on `Recorded`, `overload_bound: BTreeMap<replaced, Vec<id>>`, `overload_unbound: BTreeSet<replaced>`, hover cache | `bridge.rs` ~866-910 |
| `answer_ids` / `Answers::record` take `Option<u32>` | `bridge.rs` 929-937, 1052-1113 |
| `settle`: all-or-nothing, R2/R3 exceptions | `bridge.rs` 1115-1162 |
| `finish`: write `edge.to_declaration` | `bridge.rs` 1164-1231 (line 1181) |
| `LspBridge` doc: new "# Overload binding" section | `bridge.rs` ~159-178, 336 |

Read only: `ids.rs` `edge_id` 149-161, `index.rs` `node_at` 156,
`wire/src/lib.rs` `WireDeclaration` 348, `semanticPass.ts` 108-190 and
`declarationBindingAt` ~1000-1037.

**S3: Python records the site** (`opus`): `decls.rs` `declare` 275-293 and
`function` 237-255 (candidates, decorator test, using `syntax.rs` `signature`
280); `emit.rs` `declare` 196-205 and `finish` 313-316 (collect, attach);
`model.rs` `DeclRef` 53 + `declare` 118 (an `overloaded` flag);
`bodies.rs` `emit` 781-805 (site); `plugin.toml` `[plugin.semantic]` 89-152
(`overload_disambiguation = "hover"`); fixture `conformance/project/pkg/overloads.py`
plus a caller module; `conformance/expect.toml` entry (§6); the
`expect.toml` header's entry counts and `tests/conformance.rs`
`count_expectations` 161 constants.

**S4: tests** (`opus`): `plugins/sdk/fake-lsp/main.rs` 61/348 (scriptable
`hover`); `plugins/sdk/tests/lsp_bridge.rs` (scripted cases); a real-pyright
test in `plugins/python/tests/` reading the bridge's diff; extractor
unit tests in `plugins/python/src/extractor/tests.rs`.

## 6. Behaviours the tests slice must pin

| # | behaviour | control (revert in a worktree; the test must fail) |
|---|---|---|
| B1 | real pyright: `f(1)`, `f("s")`, `C().m("s")` bind ordinals 0, 1, 1 | make step 3 return no ordinal → unbound |
| B2 | conformance: `[[callers]]` `tier="semantic"` on the overload set, one caller calling both overloads, `files` tally `refs = 2` (1 when collapsed) | `overload_disambiguation` absent from the manifest → `refs = 1`; also fails in arm 3 (3.4.0 manifest) |
| B3 | scripted single-location answer binds by containment | `declaration_at` returns the node range only → no ordinal |
| B4 | all-or-nothing: one bound + one unbound site on one edge → structural edge re-sent, no bound edges | retract on any bound site → test sees the edge deleted |
| B5 | a fully bound edge is retracted and its bound edges survive settle (R3) | remove the R3 `to_declaration` exception → bound edges dropped |
| B6 | never bind a `has_body` declaration of an overload set | drop the rule → ordinal 2 recorded |
| B7 | two overloads from one caller → two edges, distinct ids, one placeholder | pass `None` in `answer_ids` → one edge |
| B8 | hover candidates matched with the first parameter dropped (method) | exact-only matching → method unbound |
| B9 | extractor: `@overload` set gets `declarations` with ordinals in source order and `has_body` only on the impl; conditional `def` gets none | drop the `@overload` gate → conditional def gets a list |
| B10 | filter: a call to a non-overloaded cross-file function asks the server nothing | remove the filter → the fake server's request log shows the request |
| B11 | a deleted bound call is retracted on the next pass (`retract_stale`) | don't book bound edges in `by_file` → edge lingers |

## 7. Open questions for the owner

1. **Hover as evidence for pyright.** It is the only channel pyright offers.
   The alternative is losing Python binding, which leaves only the GM-323
   inventory note. *Recommend: accept hover, gated by the manifest key, failing
   closed.*
2. **Bind overloaded receiver calls (`obj.m(...)`) too**, not only
   bare/qualified calls with a structural edge? *Recommend: yes. It is the same
   code path, and Python's method overloads are mostly reached this way.*
3. **Python node row for an overload set.** Keep first-wins (range and
   signature of the first `@overload`), or take the implementation's range as
   TS does? *Recommend: keep first-wins. The signature already follows TS's
   "first call signature" rule, and moving the range changes `find_definition`
   for a cosmetic gain.*
4. **A real-server test of the containment path (tsls)** now, by adding
   `typescript-language-server` to `scripts/test-deps.sh`, or deferred to
   GM-323? *Recommend: defer. Pyright proves the bridge end to end, the
   scripted B3 pins containment, and the TS port must add tsls anyway.*
5. **Write ADR 0024** (refine vs contradict, hover as evidence) in S2?
   *Recommend: yes, as part of S2.*

## Appendix: g-mesh calls behind the cross-file claims

- `find_references(symbol_id = graph::EdgeSpec.to_declaration)`:
  `FileGraphBuilder::add_edge`, `structural_edge`, `Answers::finish`
  (bridge), toy `answer`, `diff` test. Complete.
- `find_references(WireEdge.to_declaration)`: `watcher::apply::to_edge_record`,
  `add_edge`, core test helpers. `find_references(EdgeRecord.to_declaration)`:
  `queries::map_edge_row`, `pagination::paginate_edges`,
  `traversal::run_walk`, `write.rs`. Complete.
- `find_references(OpenSiteKind)`: graph.rs `OpenSite`, rust
  `bodies.rs` (`emit`, `open_site`), python `bodies.rs`, `census.rs`,
  `tests.rs`, sdk `bridge.rs`, `lib.rs`, toy, `tests/lsp_bridge.rs`, rust
  tests. There is no exhaustive `match`, so a new variant breaks no build.
  Every consumer compares with `==`.
- `find_references(graph::NodeSpec::declarations)`: **no callers**.
  `NodeSpec.declarations` (field) is touched only by `new`, `declarations`,
  `add_node`.
- The bridge's edge construction (`Answers::finish` → `add_edge`) was found
  through the `EdgeSpec.to_declaration` references above. grep was used for
  non-code and for single known symbols: SQL `toDeclaration`, `fake-lsp`
  methods, `plugin.toml`.
