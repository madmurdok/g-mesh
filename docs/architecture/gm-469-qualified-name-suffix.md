# GM-469: a qualifiedName-suffix rung in the resolution ladder

Status: accepted 2026-10-01, implemented on GM-474's `qualified_suffixes`
table (ADR 0015) rather than the separator rules this note first proposed.
Owner decisions: (1) the new `qualifiedNameSuffix` value plus one word in the
guidance line; (2) the rung is an exact-equality lookup of the query in that
table, so core does no parsing and has no language rules; (3) no
separator-insensitive fallback; (4) no `idx_nodes_name` here. The census and
measurements below are from the design as reviewed; the rule, the SQL and
the implementation notes describe what was built.

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
  is stronger evidence than a file stem. `File` nodes carry no qualified
  path, so they have no suffix rows and stay rung 4's. The Python `os.path` and Go
  `strings` import-only refusals in conformance stay as they are, because
  placeholders are already excluded by `declaration_only`.

### Rule

The query is looked up as given, by exact equality, in `qualified_suffixes`
(`graph::queries::find_by_qualified_suffix`). Core never splits it, knows no
separator and applies no per-language rule: what counts as a partial path is
decided at index time, from the segments each plugin sends with a declaration
([`gm-474-qualified-name-segments.md`](gm-474-qualified-name-segments.md)).
The table holds, per declaration:

- the suffixes of its primary path of two or more segments, starting at
  segment 1 (the whole path is rung 2's, so it is not stored);
- the suffixes of each alias the plugin sends, starting at segment 0.

What follows from that:

- **Segment boundaries come for free.** Only whole segments are stored, so
  `Store::read` does not match `storage::index_store::IndexStore::read`, while
  `IndexStore::read` and `index_store::IndexStore::read` do.
- **Exact spelling.** A Rust field is `module::T.f` and its getter
  `module::T::f`: `T.f` reaches only the field, `T::f` only the getter, and a
  TypeScript `Foo.bar` never reaches `Foo#bar`. There is no
  separator-insensitive fallback.
- **Trait-impl members** `module::<X as T>::m` are found through the Rust
  plugin's alias `module::X::m`, so `X::m` matches; `T::m` is not an alias
  and never equates an impl with the trait's own method. `<X as T>::m`, a
  primary-path suffix, matches as written.
- **Equality, never `LIKE`:** `_`, in most identifiers, is a `LIKE` wildcard.
- Placeholders and other non-declarations are excluded by `declaration_only`,
  as on every other rung.

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
  The page is ranked by the shared `rank_candidates`, filtered on
  `n.id IN (SELECT nodeId FROM qualified_suffixes WHERE suffix = ?1)`.
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

## SQL and index support

The lookup is one equality on `qualified_suffixes.suffix`, the table's
primary-key prefix, joined to `nodes` by id: an index seek, not the `name`
scan the separator design measured below. It runs only on the miss path of
rungs 1 to 3. The schema change (schema 10) is GM-474's, not this rung's.

The separator design's timings, kept for the record (`nodes` has no index on
`name`; Python `sqlite3` on `.backup` copies, 201 queries × 5 reps after
warm-up):

| ms/lookup | g-mesh (19,902 nodes, 68 MB) | ripgrep (6,585) | excalidraw (13,414) |
|---|---|---|---|
| V0 `name = ?` (today's rung 3) | 10.2 | 1.29 | 2.36 |
| V1 `substr(qualifiedName, -len) = ?` scan | 28.3 | 3.17 | 5.08 |
| V2 `name IN (last segs)` + check in Rust | 5.4 | 1.80 | 3.42 |

**Machine state:** `uptime` load averages were 484 / 350 / 257 across those
runs, `real 49.18, user 19.36, sys 1.25` for the g-mesh run, so only ratios
are meaningful. An `idx_nodes_name` index (rung 3 itself 50 to 350× faster)
remains a separate suggestion.

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

## Implementation notes

- **Code:** `find_definition::by_qualified_name_suffix`, the `None =>` arm
  of `resolve_symbol_name` before `by_file_name`;
  `ResolvedBy::QualifiedNameSuffix`; `rank_candidates` shared with
  `find_candidates_by_name`.
- **Fixtures:** the Rust conformance `Ledger::settle` refusal is now a
  definition, with `edger::settle` refused and the trait-impl member
  `Square::area` resolving; TypeScript refuses `Greetable.greet` (the member
  is `Greetable#greet`). Unit tests in `find_definition/tests.rs` cover each
  property above, each with its control.
- **Ladder doc:** rows 3.5 and 3.5′ and a paragraph; rung 5's label corrected
  to `semanticNeighbours`.

## Owner decisions

1. `resolvedBy` value: new `qualifiedNameSuffix`, added to the guidance's
   exact list. Decided.
2. Separator-insensitive fallback: not built. Decided.
3. Partial paths come from plugin segments (GM-474, ADR 0015), not from
   separator rules in core. Decided.
