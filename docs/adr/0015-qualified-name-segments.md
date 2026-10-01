# 0015. qualifiedName is plugin-segmented; core never parses separators

## Status
Accepted (2026-10-01)

## Context

`qualifiedName` is a display string in each language's own syntax: Rust
`a::T::m`, `<X as Tr>::m` and `T.f`, Python `Outer.Inner.m`, TypeScript `C#m`
and `C.s`, Go `T.M`. Every product consumer compares it whole, for equality.
Two features need to look inside it: matching a partial path such as
`IndexStore::read` against a declaration's trailing segments, and splitting a
placeholder's key into a head and a member to walk the head through
re-exports. Splitting the string in core would need a per-language table of
separators, brackets, raw identifiers and private-name prefixes, kept in step
with every plugin. The inventory of producers and consumers, the separator
census and the measurements are in
[`gm-474-qualified-name-segments.md`](../architecture/gm-474-qualified-name-segments.md).

## Decision

**The plugin sends the segments; core only joins them.**

- `qualifiedName` stays the display string, the input to node and edge ids,
  and what every tool prints. Nothing user-visible changes.
- `WireNode.qualifiedPath` (optional) is `[{sep, name}]`, `sep` omitted on
  the first segment. It must join back to `qualifiedName` and end in `name`.
  `PlaceholderTarget.keyPath` is the same for a `qualifiedName` key. Each
  segment keeps its own separator, so `T.f` (a field) and `T::f` (a method)
  stay distinct.
- `WireNode.aliasPaths` (optional) are other spellings of the same
  declaration, such as a Rust trait-impl method without its `<X as T>`
  segment. Which aliases exist is the plugin's knowledge; core holds no rule
  about any language's syntax.
- **No protocol bump.** All three fields are optional. A plugin that sends no
  paths gets today's behaviour: a NULL path, no suffix rows, keys never
  split.
- **Bad path at ingest:** drop it (a bad `qualifiedPath` takes its aliases;
  a bad alias goes alone), keep the node, warn once per plugin and file.
  Conformance and `g-mesh plugin check` fail on it.
- **Storage (schema 10):** `nodes.qualifiedPath` and
  `placeholder_targets.keyPath`, encoded as name, then each later separator
  and name, joined by U+001F; and `qualified_suffixes(suffix, nodeId)`,
  `PRIMARY KEY (suffix, nodeId) WITHOUT ROWID` with an index on `nodeId`.
  Its rows are the suffixes of `qualifiedPath` starting at segment 1 and of
  each alias starting at segment 0, each of at least two segments, as display
  text. The whole primary path is left out because it is `qualifiedName`,
  which `idx_nodes_qualifiedName` already finds.
- **Lookup** is `suffix = ?` with the query exactly as given: never split,
  normalized or matched with `LIKE`. `graph::queries::find_by_qualified_suffix`
  is that lookup.
- `storage::write::apply_diff` replaces a node's suffix rows on every upsert;
  every site that deletes a node (`apply_diff`, `graph::imports`,
  `graph::containers::delete_container`, `storage::language_swap`) deletes
  its rows explicitly, because foreign keys are off on the daemon's
  connection. `language_swap` compares and copies the table like
  `declarations`, and the two new columns are in its column lists.

## Rejected

- **A per-language separator table or query splitter in core.** It is the
  per-language knowledge this decision moves to plugins, and it has to track
  every plugin's spelling rules.
- **Segments without separators.** Names alone merge 51 field/getter and
  instance/static pairs on g-mesh's own index.
- **A segments table `(nodeId, pos, sep, name)`.** Matching a suffix needs a
  join per segment.
- **A reversed-path column with a range index.** Building the range key needs
  the query split into segments, which is the parsing ruled out above.
- **Including the whole primary path as a suffix.** +68% rows on g-mesh for
  nothing rung 2 does not already answer.
- **Protocol v3.** Refusing v2 breaks every third-party plugin for a feature
  that degrades gracefully; accepting both contradicts "a version mismatch is
  a hard load failure".
- **Separator-insensitive matching** (`Fonts.registered` finding
  `Fonts#registered`). Declined: a query spells the separator the way tool
  output prints it.

## Consequences

- Partial-path lookup and head/member splitting work only for plugins that
  send paths; a plugin without them keeps today's behaviour.
- On g-mesh the recommended rows (primary suffixes plus the Rust trait-impl
  alias) are about 11,200, about 1.7 MB with the index, roughly 2.5% of the
  index file.
- Schema 10 wipes and reindexes an existing index once.
