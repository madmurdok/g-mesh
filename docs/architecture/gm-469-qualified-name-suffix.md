# GM-469: a qualifiedName-suffix rung in the resolution ladder

Status: design (S4), for owner review. `resolvedBy` is a public field, so the
new value and the guidance line that lists it are the owner's call.

## Problem, reproduced

Live on g-mesh's own index (MCP, project `g-mesh`, 2026-10-01):
`find_definition("IndexStore::read")` answers `resolvedBy: semanticNeighbours`
with `storage::index_store` (a module), a test helper and an unrelated
function. The full path `storage::index_store::IndexStore::read` resolves,
and bare `read` is `nameAmbiguous` over 12 declarations. A partial path is
neither a whole qualifiedName (rung 2) nor a declaration's name (rung 3), so
every structural rung misses. The Rust conformance fixture already records
this as a refusal waiting for this rung: `plugins/rust/conformance/expect.toml`,
`[[refusal]] definition "Ledger::settle"`.

## Who resolves a user-supplied symbol (g-mesh calls)

| call | answer |
|---|---|
| `find_callers("mcp::find_definition::resolve_symbol_name")` | `find_definition::by_name`, `mcp::anchor::resolve` (complete, `hasMore: false`) |
| `find_callers("mcp::anchor::resolve")` | `find_references::handle`, `find_callers_callees::handle_callers` / `handle_callees`, `find_implementations::handle` / `dispatch`, plus 3 tests in `anchor.rs` |
| `find_definition("find_by_qualified_name")` | `graph::queries::find_by_qualified_name`: `qualifiedName = ?1 AND declaration_only` |

So `find_references` uses the same ladder as `find_definition`, through
`anchor::resolve`. One rung added in `resolve_symbol_name` reaches all five
symbol tools. No other user-facing path resolves a name: `get_dependencies`
takes paths, `search_code` is semantic. The other SQL that matches
`qualifiedName` (grep, since these are SQL strings, not symbols) is in
`graph::symbol_links` (index-time linking, :1107, :1115),
`queries::import_specifiers_named` (placeholders only) and `storage::write`'s
upsert. None of them resolves a query, and none of them should get this rung
(see GM-472 below).

## What the plugins actually emit

This is a census, not a guess. For every declaration (`declaration_only`,
not `File`), the text before `name` at the end of `qualifiedName`:

| index (schema/indexer) | language | bare | `::` | `.` | `#` |
|---|---|---|---|---|---|
| g-mesh, GM-450 worktree (9/2) | rust | 977 | 5,463 | 1,625 (fields) | 0 |
| | python | 496 | | 106 | |
| | go | 316 | | 88 | |
| | typescript | 422 | | | 107 |
| ripgrep (8/2) | rust | 213 | 3,410 | | |
| requests (8/2) | python | 369 | | 584 | |
| gin (8/2) | go | 1,271 | | 461 | |
| excalidraw (7/1) | typescript | 3,544 | | 258 | 583 |

Every non-bare qualifiedName in all five indexes ends in `<sep><name>`
with `sep` from that language's set. There were zero `NOT-SUFFIX` rows.
Other findings that shape the rule:

- **TypeScript uses both `#` and `.`.** Instance members are `Foo#bar`.
  Static members and namespace or module members use `.`, for example
  `ShapeCache.get` and `image-blob-reduce.ImageBlobReduce#toBlob`. Module
  segments can contain `-`.
- **Rust has `#` inside a name.** Raw identifiers like ripgrep's `r#async`
  mean `#` must not split a Rust qualifiedName. That is why separators are
  chosen per candidate's language, not once globally. The declarations whose
  own `name` contains `::`, `.` or `#` are `r#async` (ripgrep) and `*.scss`
  (excalidraw ambient module), one each.
- **Rust trait-impl members are `<X as T>::m`.** That is 1,050 of ripgrep's
  3,623 Rust declarations and 164 of g-mesh's 8,065. With an exact-spelling
  suffix rule, `Stream::read` would never reach
  `ipc::unix::<Stream as Read>::read`. `embed_eval::context::trait_impl_segment`
  already parses this segment (depth-aware ` as ` split).
- **Go's package-level declarations are bare** (`New`, not `gin.New`), so
  `gin.New` has nothing to suffix-match. Matching a package prefix is a
  different rule and is out of scope.

## Recommendation

### Position: rung 3.5, after bare name, before file name (rung 4)

```
1 id · 2/2' whole qualifiedName · 3/3' bare name · 3.5/3.5' qualifiedName suffix · 4 file name · import-only refusal · 5 semantic
```

In code, it is the `None =>` arm of `resolve_symbol_name`, before
`by_file_name`. It runs only when `exact` and `matches` are both empty, so:

- It cannot change any answer rungs 1 to 3 give today. A query that already
  resolves is never reached by the new rung, and the happy path pays nothing.
- Rungs 2 and 3 are effectively disjoint from it. Rung 3 matches a name, and
  only the two names above contain a separator. So "before" or "after"
  rung 3 is a matter of cost, and "after" is free.
- It comes before rung 4, because a declaration whose path ends in the query
  is stronger evidence than a file stem. `File` nodes are excluded from its
  candidates, since paths belong to rung 4. The Python `os.path` and Go
  `strings` import-only refusals in conformance stay as they are, because
  placeholders are already excluded by `declaration_only`.

### Rule

- **Gate:** the query contains at least one separator (`::`, `.`, `#`).
  Every segment must be non-empty, so `::read` and `IndexStore::` do not
  enter. A single-segment query is rung 3's job and never enters this rung.
- **Candidates:** declarations whose `name` is one of the query's possible
  last segments: the text after the last `::`, after the last `.`, and after
  the last `#`. That is at most three strings, `name IN (?,?,?)`. Taking all
  three keeps `Foo::r#async` and `*.scss` correct without knowing the
  language yet.
- **Segment-boundary match (tier A, exact spelling):** `qualifiedName`
  ends with the query, is not equal to it, and the text before that suffix
  ends with one of the candidate's language separators. Rust uses
  `::` and `.`. TypeScript and JavaScript use `#` and `.`. Python and Go use
  `.`. An unknown language falls back to all three. `Store::read` does not
  match `IndexStore::read`, because the character before the suffix is `x`.
- **Trait-impl segment:** a candidate's `<X as T>` segment also matches a
  query segment `X`, compared without generic arguments. So `Stream::read`
  matches both `ipc::{unix,windows}::<Stream as Read>::read`, and that is
  ambiguous. Matching on `T` would equate every impl with the trait's own
  method, so `T` is not matched. Written out in full, `<X as T>::m` is
  matched literally.
- **Owner decision, tier B (separator-insensitive):** only if tier A finds
  nothing, compare segment sequences while ignoring which separator joins
  them. Then `Extractor.declareSymbol` (how agents write TypeScript) reaches
  `Extractor#declareSymbol`, and `IndexStore::conn` reaches the field
  `IndexStore.conn`. Benefit: the commonest mis-spelling resolves.
  Risk: Rust keeps getter `T::f` and field `T.f` apart on purpose
  (GM-450), and tier B only ever runs when the exact separator matched
  nothing, so it never merges them. But it does turn a mistaken separator
  into an answer rather than a refusal. I recommend shipping tier B with
  the same label. It uses the same candidate rows and only a different
  comparator.

### Answer and labels

- **One match:** resolution, with `resolvedBy: "qualifiedNameSuffix"`. This
  is a new `ResolvedBy::QualifiedNameSuffix` (serde camelCase). The answer
  carries the declaration's full `qualifiedName`, so the caller sees what
  was matched.
- **Several matches:** today's ranked candidate page, with
  `resolvedBy: "nameAmbiguous"`. It is the same shape and re-query-by-id
  contract as rungs 2′ and 3′, and the existing `AMBIGUOUS` explanation.
  The ordering is the same as 3′ and 4: inbound `REFERENCES`+`CALLS` count
  descending, then `id ASC` (`pagination::paginate_by_score`).
  `find_candidates_by_name` needs an id-set variant (`WHERE n.id IN (…)`),
  because the `NameColumn` equality cannot express a suffix.
- **Guidance:** `cli/agent_instructions.rs:64` lists
  `id`/`qualifiedName`/`name` as the exact `resolvedBy` values, and anything
  else tells an agent to distrust the answer. If `qualifiedNameSuffix` is
  meant as a resolution, it has to be added to that list (one word).
  Otherwise agents will re-check it with grep, which is exactly the cost the
  ladder exists to remove. The alternative is to reuse
  `"qualifiedName"`, which needs no guidance change but means a caller cannot
  tell "you typed the whole path" from "you typed a tail of it". I
  recommend the new value plus the one-word guidance change. **Owner
  decides.**
- The `symbol_name` parameter docs in `mcp/mod.rs:803,823` ("Qualified name
  resolved first, then bare") gain "then a qualified-name tail".

### Measured ambiguity

Two-segment tail queries (`Owner<sep>name`) were built from 200 random
non-bare declarations per index, seed 469:

| index | equal to the whole qualifiedName (rung 2 already) | 1 match | 2 | 3 | 5+ |
|---|---|---|---|---|---|
| g-mesh | 40 | 149 | 7 | 1 | 4 |
| ripgrep | 44 | 148 | 4 | 4 | 1 |
| excalidraw | 200 | 0 | 0 | 0 | 0 |

Of the queries rung 2 does not already answer, 93% (g-mesh) and 94%
(ripgrep) resolve to one declaration. Excalidraw's TypeScript qualifiedNames
are almost all two segments, so a two-segment tail is the whole name and
the rung adds nothing there. That is expected, not a failure.

## SQL and index support: no schema change needed

`nodes` has indexes on `filePath`, `qualifiedName` and
`(language, container)`, but **not on `name`**. `EXPLAIN QUERY PLAN` gives
`SCAN nodes` for today's rung 3 (`name = ?`) and for a suffix predicate
alike. Three candidate queries were timed with Python `sqlite3` on `.backup`
copies, over 201 queries × 5 reps after warm-up. The copies are in the
session scratchpad, and the script is reproduced in the S4 result.

| ms/lookup | g-mesh (19,902 nodes, 68 MB) | ripgrep (6,585) | excalidraw (13,414) |
|---|---|---|---|
| V0 `name = ?` (today's rung 3) | 10.2 | 1.29 | 2.36 |
| V1 `substr(qualifiedName, -len) = ?` scan | 28.3 | 3.17 | 5.08 |
| **V2 `name IN (last segs)` + check in Rust** | **5.4** | **1.80** | **3.42** |
| V0 with `idx_nodes_name` | 0.028 | 0.038 | 0.025 |
| V2 with `idx_nodes_name` | 0.25 | 0.21 | 0.09 |

V1 and V2 returned identical match sets on all 603 queries. **Machine
state:** `uptime` load averages were 484 / 350 / 257 across the runs. The
g-mesh no-index run took `real 49.18, user 19.36, sys 1.25`, so most of its
wall time was waiting for CPU. Absolute milliseconds are therefore inflated,
and only the ratios are meaningful.

- **V2 is the recommendation.** It costs about one rung-3 scan, the
  comparator has to live in Rust anyway for per-language separators and
  `<X as T>`, and it avoids `LIKE`. `LIKE` is case-insensitive, and `_`, which
  is in every Rust identifier, is a wildcard to it. The cost is paid only on
  the miss path of rungs 1 to 3, for a query that today goes on to
  the semantic rung (43 to 48 ms warm, 2.4 s cold, per the ladder doc).
- **No reverse-string index or tail column.** The query's last segment *is*
  the candidate's `name`, so the `name` column already is the tail column.
- **Optional, separate from this task:**
  `CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name)` cuts rung 3
  itself by 50 to 350× and V2 to about 0.1 to 0.25 ms. It needs no version
  bump: `schema::ensure_current` runs `apply`'s DDL on an index whose
  `schema_version` already matches, so an existing index gains it in place.
  The cost is one more B-tree on every upsert, not measured here. I suggest
  it as its own task, with an indexing-time measurement, rather than
  folded in here.

## GM-472 (re-exports for qualifiedName refs): independent

GM-472 changes index-time linking in `graph::symbol_links`. This rung is
query-time, in `find_definition::resolve_symbol_name`. They share no code,
and neither blocks the other. Two notes:

- **The linker must not borrow this rung.** A suffix match there would
  create edges from resemblance, which breaks "a missing edge beats a wrong
  one". It stays exact.
- **They meet only in what the user sees.** After GM-469,
  `find_references("T::m")` resolves its anchor wherever `T` lives. Whether
  the uses written through `pub use` re-exports appear in the result is
  still GM-472's job. A query spelled through a re-export path
  (`wire::T::f`) suffix-matches only if `T`'s real module path ends that
  way. Resolving re-exported spellings at query time is not part of this
  design.

## Implementation notes for the implement slice

- **Fixture flips:** the Rust conformance `[[refusal]] "Ledger::settle"`
  becomes a `[[definition]]` with `resolvedBy` `qualifiedNameSuffix`. Add a
  boundary case (`Store::read` refused while `IndexStore::read` exists),
  a trait-impl case, a several-match case and a raw-identifier case, and add
  `#`/`.` cases to the TypeScript, Python and Go conformance.
- **Controls:** remove the rung's arm. `Ledger::settle` must go back to the
  refusal and every new case must fail.
- **On g-mesh's own code:** `find_definition` / `find_references` resolve
  `IndexStore::read` (one match). `ChunkedReader::read` is `nameAmbiguous`
  (impls in two test modules).
- **Ladder doc:** add rows 3.5 and 3.5′ to the table and a paragraph. Fix
  the drift noticed while reading: the table says rung 5 is `semantic`,
  but the code serializes `semanticNeighbours`.

## Owner decisions

1. `resolvedBy` value: new `qualifiedNameSuffix` plus adding it to the
   guidance's exact list (recommended), or reuse `qualifiedName`.
2. Tier B (separator-insensitive fallback): ship it with the same label
   (recommended), or exact separators only.
3. `idx_nodes_name`: a separate task (recommended), folded in here, or not
   at all.
