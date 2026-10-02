# GM-450: Rust struct fields and inherent methods by qualified name

Status: fields implemented as proposed below, including the semantic open
sites for `x.f` (owner decision, 2026-10-01). Partial qualified-name lookup
(`Type::method`) is a separate resolver task. A field's qualifiedName is
`<module path>::T.field`, not `::T::field` as first proposed (owner
decision, 2026-10-01; see "Why `T.f`" below).

## Reproduction

Live, on g-mesh's own index (MCP `find_*`, project `g-mesh`, 2026-10-01):

| query | answer |
|---|---|
| `find_references("all_unresolved")` | `resolvedBy: semanticNeighbours`, "Nothing is named 'all_unresolved'" |
| `find_definition("IndexStore::read")` | `resolvedBy: semanticNeighbours`, "Nothing is named 'IndexStore::read'" |
| `find_definition("storage::index_store::IndexStore::read")` | resolves, `resolvedBy: qualifiedName`, `nativeKind` method |
| `find_definition("read")` | `nameAmbiguous`, 12 candidates, `IndexStore::read` first |

Fixtures (both assert **current** behaviour, labelled as such, so the suite
stays green and S2 has a baseline to flip):

- `plugins/rust/src/extractor/tests.rs`,
  `struct_fields_are_not_nodes_yet`: a two-file crate. `store.rs`
  declares `Ledger { all_unresolved, truncated_by }` and `impl Ledger { fn
  settle }`; `user.rs` builds a `Ledger` literal, calls `settle` and reads a
  field. It asserts that `store::Ledger::settle` is a `method` node, that
  `Ledger::settle` is not a qualifiedName, that no node is named after
  either field, and that `user::tally`'s only `REFERENCES` target is the
  `Ledger` placeholder.
- `plugins/rust/conformance/`: `gaps.rs` GAP 4 (`Ledger`) and
  `internals::tally` (the use from another file), and four entries at the end
  of `expect.toml`: `[[definition]] gaps::Ledger::settle` (resolves), and
  `[[refusal]]`s for `definition Ledger::settle`, `definition
  all_unresolved` and `references truncated_by`. These go through the real
  `find_definition`/`find_references` handlers, linked, after rust-analyzer's
  semantic pass.

## The gap, exactly

1. **Fields are not nodes, by a documented decision.**
   `plugins/rust/src/extractor/decls.rs:22-23` ("Struct fields and enum
   variants are *not* nodes ... nothing in the tool surface addresses one"),
   enforced by `node_kinds` (`decls.rs:131-163`, fn at 134), which has no arm for
   `field_declaration`; `Declarer::item` (`decls.rs:263-300`) never descends
   into a `struct_item`'s `field_declaration_list`.
2. **Field uses are not sites.** `bodies.rs:151-157`: a `field_expression`
   walks only its receiver ("the field name is not a symbol this index
   carries"). A struct literal's `field_initializer` and a pattern's
   `field_pattern` fall to `visit_children`, where the field name is a
   `field_identifier` that no arm handles, so nothing is emitted.
3. **Inherent methods are indexed; the miss is the spelling.** Not confirmed
   as a Rust indexing gap. `impl T { fn m }` is a `Function` node,
   `nativeKind` `method`, qualifiedName `<module path>::T::m`
   (`decls.rs:15`, `impl_block` at `decls.rs:183-210`). The resolution ladder
   (`docs/architecture/symbol-resolution-ladder.md`, rung 2) matches a
   qualifiedName **whole**. `IndexStore::read` is neither a whole
   qualifiedName (it is `storage::index_store::IndexStore::read`) nor a bare
   name, so every rung misses and the answer is semantic neighbours. This is
   a resolver question rather than a plugin one, and it matters most for Rust
   because Rust is the only plugin whose qualifiedNames carry the module path
   (Python: `C.m`, TS: `C#m`/`C.m`, Go: `T.m`, none with a module prefix).

## What the other plugins do

None of them indexes data members:

| plugin | data member | where |
|---|---|---|
| Python | class-body assignments are not nodes | `plugins/python/src/extractor/decls.rs:31-36` |
| TypeScript | class data properties and interface `property_signature`s are not nodes; only function-valued fields become `Function` nodes | `plugins/typescript/src/extract.ts:1274-1281`, `handleField` at `1929-1957` |
| Go | struct fields are not nodes; interface methods are (`interface_method`) | `plugins/go/extract.go:697-716` |

So fields in Rust alone would make Rust the one language where
`find_references(field)` answers. That is the task's acceptance, but it is
also a cross-language inconsistency the owner should accept knowingly.

## Proposal

### Fields

- **Node**: `kind` `Variable`, `nativeKind` `field`, `name` the field name,
  qualifiedName `<module path>::T.field` (`store::Ledger.all_unresolved`
  beside the inherent method `store::Ledger::settle`); the file model keys
  it by the tail `T.field`.
  `Variable` is the existing kind for named values (`const`, `static`,
  `assoc_const`); no `NodeKind` variant is added (`wire/src/lib.rs:77-83`
  stays five kinds).
- **Scope**: named fields of `struct` and `union` items. Not tuple-struct
  fields (`.0` has no name an agent would type), not enum-variant fields
  (variants themselves are not nodes; adding them is a separate decision).
- **Visibility**: the field's own `pub`/`pub(crate)`/none, through the
  existing `visibility()`; a field with no modifier is private to its module,
  as Rust says.
- **Signature**: `pub all_unresolved: bool` (the field's own text), doc
  comment as for any item.
- **Uses** (`REFERENCES` edges):
  - structural: `self.f` inside `impl T` binds to `T.f` in the container
    `self.m()` would address; `T { f: .. }` and `T { f, .. }` bind to
    `<resolved T>.f`, through the same qualifiedName placeholder path a
    cross-file `T::m` call takes;
  - semantic: `x.f` with an unknown receiver becomes an
    `OpenSiteKind::Reference` open site. The SDK's LSP bridge already asks
    `textDocument/definition` for every non-implementation open site and maps
    the answer through `SdkIndex::node_at` (`plugins/sdk/src/lsp/bridge.rs:
    155-170`), so rust-analyzer's answer lands on the new field node with no
    bridge change.

#### Why `T.f`

The first cut named a field `T::f`, the shape of an inherent method. A getter
named like its field (`inner` and `fn inner()`) then had the same
qualifiedName as the field; g-mesh's own code has 50 such pairs. The fresh
verify slice measured three consequences, each from that one shared key:

- the file model keeps the first declaration under a tail, and the struct
  usually precedes its `impl`, so `self.inner()` bound to the *field*: 34
  method calls in g-mesh moved off the method, which then showed 0 callers;
- a method body's edges are attributed by the same tail lookup, so the
  getter's body was attributed to the field (`self.inner` became a
  field-to-field self-loop);
- core's linker resolves a qualifiedName placeholder to the one candidate
  that fits; `REFERENCES` accepts any kind, so a cross-file struct-literal
  field met two candidates (field and method) and stayed unresolved.

Rust's own syntax already separates the two namespaces: an associated item
is reached by a path (`T::f`), a field only through a value (`x.f`). So a
field takes `.` and an associated item keeps `::`, and no field can share a
qualifiedName, a node id or a file-model key with a method, constant or
type alias of `T`. Method calls (`self.f()`, `T::f(..)`, `Self::f(..)`)
look up `T::f`; field reads, literal fields and pattern fields look up
`T.f`.

### Inherent methods by `Type::method`

Out of this plugin. Two options for core's resolver, to be decided with the
owner rather than in S2 by default:

- **(a) a suffix rung** after rung 3: a `::`- or `.`-separated query matches
  qualifiedNames that end with `<sep><query>` on a segment boundary; one
  match resolves, several are `nameAmbiguous`. Fixes every language and
  every partial path (`index_store::IndexStore::read` too).
- **(b) leave it**: the bare-name rung already returns the method first among
  candidates (`find_definition("read")` above), and the guidance can say
  "Rust qualifiedNames start at the module path".

(a) is a resolver change with its own ambiguity rules and belongs in its own
task; GM-450's acceptance for methods is met by documenting this finding.

## Impact

### Embedding

Fields would become embeddable: production embeds every node with a doc
comment or a signature (`core/src/embedding/backfill.rs:196-209`,
`pipeline.rs:735-743`), kind-agnostic, and every field node has a signature.
Excluding them would need a new kind-based filter that does not exist today.
**Decision proposed: embed them**, because a documented field is exactly
what a natural-language query like "flag saying every row is unresolved"
should find, and no new mechanism is needed. Owner to confirm.

Size on g-mesh itself (measured, 2026-10-01):

- index: 6,606 embeddable Rust nodes today (File 215, Function 4,892,
  Module 337, Type 614, Variable 548), 7,966 vectors in all languages;
- fields: about 1,617 named fields in 416 brace structs, 572 of them
  documented (an awk count over `git ls-files '*.rs'`, line-based, so an
  estimate, not a parse);
- so about +24% Rust embeddable nodes, +20% vectors overall.

Time, by GM-466's combined curve (`docs/results/gm-466-cost-model.md`,
`t(n) = 2.7127 + 0.84679 n + 2.4646e-4 n^2` ms per text): a field text is
short (signature about 8 tokens, plus the trimmed doc, about 10-25), so
about 16-32 tokens, 16-30 ms each, about 25-50 s extra on a cold full
embed of g-mesh. GM-466 itself found its curve cannot tell the quadratic
model from a linear one within 0.05 on ratios, so treat this as an order of
magnitude, and measure a real pass in S2 if the owner wants a number.

### Semantic pass

About 11,000 field-access sites (`x.f` not followed by `(`) against about
24,000 receiver-call sites in g-mesh (same rough grep), about 1,200 of them
`self.f`, which bind structurally. The rest become open sites, so the
rust-analyzer pass gets roughly +40% `textDocument/definition` requests.
If that is too much, S2 can ship the structural half first (`self.f`,
literals, patterns) and add the open sites separately, at the cost of
`x.f` reads being missing from `find_references` until then.

### Index schema and other languages

- No SQL schema change, no `NodeKind`/`EdgeKind` change. One new
  `nativeKind` string, `field`, part of the node id like every other.
- Node count grows (above); a re-index after the upgrade adds the nodes,
  nothing migrates.
- `find_definition("x")` for a common field name (`name`, `path`, `kind`)
  gets more `nameAmbiguous` candidates. Ranking (rung 4's inbound
  `REFERENCES`+`CALLS` count) is unchanged.
- Other plugins unchanged. Whether Python/TS/Go should follow is a separate
  decision; this note does not propose it.

## Controls for S2

- `struct_fields_are_not_nodes_yet` fails once fields are emitted;
  S2 rewrites it to assert the field nodes and edges, and reverting the
  extractor change must make the rewritten test fail.
- The two field `[[refusal]]`s in `expect.toml` fail as "an answer" once
  fields resolve; S2 replaces them with `[[definition]]`/`[[references]]`
  entries (the `references` one needs `tier = "semantic"` if it includes the
  `x.f` read).
- `[[refusal]] definition Ledger::settle` is the control for option (a), if
  that is ever taken.
