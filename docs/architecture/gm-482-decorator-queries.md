# GM-482: `find_definition("@Component")` finds the decorator `Component`

Design note (slice S1). Nothing here is implemented yet.

## 1. Problem, measured

A TypeScript decorator is a plain function, indexed under its own name
(`Component`). Agents copy the use site and ask for `@Component`. Nothing is
named that, so every structural rung misses. The semantic rung then stops
because every shipped plugin declares `starts_with = ["@"]` in
`[plugin.non_symbol_queries]` (ADR 0018). The answer is
`no symbol named '@Component' found`, although the index holds the
declaration.

Measured on core as of GM-481 (`5485fc9`; `core/src` outside tests is
identical to this branch's base `041ab7b`). The harness is GM-475's
`drive.py`, run through the real MCP shim with the model loaded, on three
fixtures and GM-475's Python corpus (`py-corpus`, requests). Scripts and raw
output are in the session scratchpad under `gm482/`.

| Corpus | Query | find_definition today | Bare remainder today |
|---|---|---|---|
| ts-deco | `@Component`, `@Injectable` | refused | `qualifiedName` (the decorator) |
| ts-deco | `@scope/pkg` | import note (`import_only_refusal`) | refused |
| ts-deco | `@decorators` (a file stem) | refused | `fileName` page |
| ts-deco | `@Component()` | refused | `semanticNeighbours`, wrong rows |
| mixed-deco (TS + Python, each declares a `Component` decorator) | `@Component` | refused | `nameAmbiguous`: TS and Python |
| py-deco | `@register`, `@retry`, `@Widget.size` | refused | `qualifiedName` |
| py-deco | `@property`, `@staticmethod`, `@dataclass` | refused | refused |
| py-deco | `@deco.register`, `@pkg.deco.register` | refused | refused (a Python qualifiedName has no module part) |
| py-corpus (13 distinct decorators) | every `@x` | refused | 0 structural hits; 9 `semanticNeighbours` pages, all wrong (`pytest.fixture` gives `nosan_server`, `classmethod` gives `Response.__init__`); 4 refused |

Two facts drive the design:

- **The rewrite pays off only on the structural rungs.** Every remainder that
  resolved did so at `qualifiedName`/`name`. Every semantic page for a bare
  remainder was wrong, in both languages.
- **The query carries no language.** In the mixed project, `@Component`
  names two decorators. A rewrite run for TypeScript alone would answer with
  the TypeScript one, even to an agent reading the Python file.

`search_code` today: `@Component` gets the `queryIsAPathOrPackage` verdict.
The rows are still returned. Neither `@Component` nor `Component` ranks the
`Component` declaration first (the top rows score 0.62 and 0.67, and both
are other declarations).

## 2. Mechanism (recommended)

A new optional manifest table. The plugin declares the prefixes that, when
stripped, can leave a name in its own language:

```toml
[plugin.symbol_query_prefixes]
# A query `@Name` that matches nothing is looked up again as `Name`, among
# this language's declarations only. `@` marks a decorator use.
strip = ["@"]
```

Core treats each prefix as an opaque literal, as ADR 0018 does for shapes.
Core holds no `@`, and no identifier rule.

### Algorithm, in `resolve_symbol_name`

1. **The original query goes first, unchanged:** exact `qualifiedName`,
   `name` (with its ambiguity page), then the qualifiedName suffix. A hit
   here returns exactly as today, so no current answer can change.
2. **Rewrite, only when all of step 1 missed.** For each discovered language
   L whose `strip` has a prefix `p` that the query starts with, take the
   remainder `r = query[p.len()..]`. Skip L when:
   - `r` is empty (`@` alone), or
   - **`r` matches L's own `non_symbol_queries`.** This is how a refusal
     orders against the rewrite. `@scope/pkg` leaves `scope/pkg`, which
     TypeScript refuses (`contains = ["/"]`), so no lookup runs and the
     query goes on to step 3, where it gets today's answer. `@@Component`
     leaves `@Component`, which is refused by `starts_with = ["@"]`, so a
     prefix is stripped only once. `@src/app.ts` leaves a path, which is
     refused, so it can never resolve to a `File` node by the back door.
   The "is the rest an identifier" test is therefore the plugin's own
   declaration applied to the remainder. Core adds no rule of its own.
3. **Look up each surviving `(L, r)`** with the same three structural
   lookups, keeping only rows whose `language` is L. Then union the rows
   over the languages. Exactly one row resolves. Two or more give the
   existing ambiguous candidate page, so a mixed project in which both
   languages opt in shows both decorators. No row means the ladder goes on
   with the **original** query.
4. **Then the original ladder continues unchanged:** file name,
   `import_only_refusal`, semantic rung. All of them see the original query,
   so `@scope/pkg` still gets its import note and `@NoSuchThing` still gets
   the terse refusal (`refused_by_all` still holds for `@`).

### Which rungs and tools

| Rung | Rewritten? | Why |
|---|---|---|
| exact `qualifiedName`, `name`, qualifiedName suffix | yes | every measured hit landed here |
| file name | no | `@decorators` would answer with a file page; a decorator is never a file |
| `import_only_refusal` | no in v1 (decision D7) | |
| semantic | no | measured: 9 of 13 Python decorator remainders and TS `Component()` give wrong neighbour pages |
| `search_code` | no (decision D4) | one query vector serves every language, so a per-language rewrite has nothing to attach to; measured: stripping does not bring the decorator to the top |

The rewrite sits in `resolve_symbol_name`, so `anchor::resolve` passes it to
`find_callers`, `find_callees`, `find_references` and `find_implementations`
(by `symbol_name`) for free. They then mean what `find_definition` means.

### Carrying it

Extend `QueryShapes`, which is already built once per daemon from the
manifests, to carry each language's `strip` list next to its
`NonSymbolShapes`, with one method:

```rust
/// (language, remainder) pairs to retry after the original query missed.
fn rewrites<'q>(&self, query: &'q str) -> Vec<(&str, &'q str)>
```

`resolve_symbol_name` reaches it as `semantic.shapes()`. No signature
changes.

The ambiguous page needs a language filter inside the SQL, not after it
(`language IN (...)` in `find_candidates_by_name` and
`find_candidates_by_qualified_suffix`). Filtering after a paged query would
break `hasMore` and the cursor.

## 3. Per-language scoping in a mixed project

- A language without the table is never rewritten for. Its declarations can
  never come back for an `@` query, and an empty map rewrites nothing.
- A rewrite for L returns only L's rows. A TypeScript declaration can
  therefore never make a Python name resolve, and the reverse holds too.
- What remains is the hazard in section 1: if only TypeScript opts in, a
  Python reader's `@Component` resolves, confidently, to the TypeScript
  decorator. The fix is for every language that has `@` decorators to opt
  in. Both then appear on one ambiguous page, which is what the bare query
  already returns today.

## 4. Python

Python's `@` is decorator syntax too, so the declaration is just as true for
Python. Measured effect:

- **requests:** 0 of 13 decorator queries change. Every remainder is a
  builtin (`property`, `staticmethod`, `classmethod`), an external
  (`pytest.mark.*`, `contextlib.contextmanager`, `overload`,
  `runtime_checkable`) or a local variable that is not indexed
  (`possible_keys`). None resolves structurally, and the semantic rung is
  not rewritten, so all 13 stay refusals.
- **py-deco:** 3 of 7 become answers (`@register`, `@retry`, `@Widget.size`).
- **mixed-deco:** opting in turns the TypeScript-only answer into the
  honest two-language page.

Python's `strip = ["@"]` passes its own remainder check: `.models` is
refused by `starts_with = ["."]`, and `pytest.mark.parametrize` is looked up
and then misses. **Recommended: Python opts in, Rust and Go do not.** Rust
attributes are `#[...]`, and Go has no decorators.

## 5. Validation (in `read_manifest`, inherited by `plugins check`)

1. An empty string in `strip` is a hard error. It would rewrite every query
   to itself.
2. Unknown keys are rejected: `#[serde(deny_unknown_fields)]` on this table
   only.
3. **Every `strip` prefix must also be in the same manifest's
   `non_symbol_queries.starts_with`.** Otherwise a query could be both "maybe
   a symbol as typed" and "a symbol once stripped", and could reach the
   semantic rung with its prefix still on. This also keeps the ordering in
   section 2 true by construction.

Compatibility: `RawPlugin` does not deny unknown fields, so an older core
ignores the new table. It then does no rewrite and does not fail. A key
added *inside* `non_symbol_queries` would make an older core reject the
manifest, because that table denies unknown fields. That is the reason for a
separate table (decision D1). `plugin.toml` is fingerprinted, so each index
is rebuilt once. `g-mesh plugins list` and plugin-check print the `strip`
list next to the shapes.

## 6. Test plan (each test has a control: revert the code, never the test)

| # | Test | Control that must make it fail |
|---|---|---|
| T1 | Manifest: table parses; an absent table gives empty `strip` | parse the field as always-empty |
| T2 | Validation: `strip = [""]` is rejected | remove the empty check |
| T3 | Validation: an unknown key is rejected | drop `deny_unknown_fields` on the table |
| T4 | Validation: a `strip` prefix missing from `starts_with` is rejected | remove rule 3 |
| T5 | `QueryShapes::rewrites`: `@Component` gives `[(typescript, Component)]`; `@`, `@@X`, `@scope/pkg` give `[]`; a language without the table gives `[]` | drop the remainder-shape check, and `@scope/pkg` fails |
| T6 | Shipped map: TypeScript and Python declare `strip = ["@"]`; Rust and Go declare nothing (read from the committed manifests, as `QueryShapes::shipped()` does) | edit nothing; this pins the manifests |
| T7 | Rung: a TS `Component` node with the TS rewrite gives `find_definition("@Component")` = that node | delete the rewrite call in `resolve_symbol_name` |
| T8 | Rung: TS and Python `Component` nodes, only TS opts in, gives the TS node with no page | remove the language filter, which gives an ambiguous page |
| T9 | Rung: both opt in gives an ambiguous page with both | (with T8) collapse the union to the first language |
| T10 | Rung: a TS `File` node `src/app.ts` gives `@src/app.ts` still refused | drop the remainder-shape check, which resolves to the file |
| T11 | Rung: an import placeholder for `@scope/pkg` still gives the import note | run the rewrite after `import_only_refusal`, or let it consult the file rung |
| T12 | Rung: `@NoSuchThing` with an embedding rung leaves `reached` empty (no model load) | rewrite at the semantic rung |
| T13 | Original first: a node whose qualifiedName is literally the query wins over a rewrite hit | run the rewrite before step 1 |
| T14 | Anchor: `find_callers(symbol_name: "@Component")` anchors on the same id as `Component` | as T7 |
| T15 | TS conformance `expect.toml`: `[[definition]] symbol = "@<decorator>"` on a decorator added to the conformance project, through `plugins check --expect` | manifest without the table |
| T16 | Measurement: rerun this harness plus GM-475's `ts-corpus`, `go-corpus` and `gmesh-corpus` with `before`, `after` and a no-`strip` control root. Expected diffs are only `@x` queries whose remainder is a declaration of an opting language. Every other diff is a finding. The control must equal `before`, and `after` must differ on ts-deco (`@Component` resolves). | the control arm itself |

## 7. Alternatives considered

- **Core retries any `@x` miss as `x` for languages that set a flag**
  (`decorator_queries = true`). Rejected: `@` would sit in core as language
  syntax, against GM-475's principle. Its answers are the same as the
  recommendation's.
- **A regex rewrite (`^@([A-Za-z_$][\w$]*)$`).** It states the identifier
  rule exactly. Rejected for now, as in ADR 0018: the remainder-shape check
  gives the same outcome on every measured query, and a literal cannot be
  subtly wrong. It is the fallback if a case appears that the check gets
  wrong.
- **Plugin-emitted alias rows** (`@Component` as an extra name or suffix).
  Rejected: suffix rows are built by core from segments (ADR 0015), so this
  needs a protocol change. A plugin cannot tell which functions are
  decorators, so every function would get an alias. The answer would also
  be labelled `qualifiedNameSuffix`.
- **Rewrite at the semantic rung.** Rejected on the measurement in section 1.
- **One global rewrite with no language filter.** Rejected: it would let
  TypeScript's declaration change Python's answers.

## 8. Risks and trade-offs

- **The TS-only answer in a mixed project** if Python does not opt in
  (section 3). Mitigated by D2.
- **A coincidental hit.** `@types` alone leaves `types`. If something is
  named `types`, it resolves. That is rare and harmless: it is an exact name
  match, and a scope with no package is not a valid specifier.
- **The answers of five tools change** for `@x` queries that resolve: from a
  refusal to an answer, never from one answer to another (step 1 runs
  first).
- **`@Component()` and `@retry(3)` stay refused** (D6).
- **An older core silently ignores the table** (it does no rewrite). That is
  preferred to rejecting the manifest.
- **Small SQL change** for the language-filtered ambiguous page.
- **A one-time re-index** through the fingerprint.

## 9. Recommendation and decisions for the owner

Recommended: section 2 as written. Add a separate
`[plugin.symbol_query_prefixes] strip = ["@"]` for TypeScript and Python.
Apply it at the structural rungs only, after the original query misses.
Check the remainder against the same language's own shapes, and filter by
language. No change to the semantic rung or to `search_code`. Record it as
ADR 0019.

| # | Decision | Recommended |
|---|---|---|
| D1 | A separate table vs a key inside `non_symbol_queries` | separate table (older cores ignore it) |
| D2 | Python opts in | yes (0 changes on requests, 3 of 7 on the fixture, and an honest mixed page) |
| D3 | How a rewritten answer is labelled | keep the rung's `resolvedBy` (`name`/`qualifiedName`) and add `queriedAs: "Component"`; a new `resolvedBy` value would read as "not exact" to clients |
| D4 | `search_code` applies the rewrite | no |
| D5 | Validation rule 3 (a `strip` prefix must be in `starts_with`) | yes |
| D6 | Strip a trailing call (`@Component()`) | no; core would need syntax for it, so it is a follow-up if wanted |
| D7 | Run `import_only_refusal` on the remainder (Angular's `@Component` is imported from `@angular/core`) | not in v1; first measure whether TS placeholders carry a named import's binding |
