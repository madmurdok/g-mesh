# GM-474: qualifiedName as plugin-emitted segments

Status: design (S1), for owner review. No product code changes in this slice.

Owner decision (2026-10-01): plugins send `qualifiedName` as a list of
segments as well as a display string in the language's own syntax, so that
core never needs per-language separators. Two tasks are waiting on this:
GM-469 (match a partial path such as `IndexStore::read` against a
declaration's trailing segments) and GM-472 (split a `qualifiedName` key into
a head and a member, so the head can be walked through re-exports). This note
records what reads and writes `qualifiedName` today, the wire and storage
shape I recommend, and how the two waiting tasks use it.

## Recommendation in one paragraph

Keep `qualifiedName` on the wire and in storage exactly as it is today. It
stays the display string, the input to node and edge ids, and what every tool
prints. Add an **optional** `qualifiedPath` to `WireNode`: an ordered list of
`{sep, name}` segments whose concatenation must equal `qualifiedName`. Add
the matching `keyPath` to `PlaceholderTarget` for `qualifiedName` keys. Each
segment carries its own **separator text**, which the plugin chooses and core
only ever compares for equality. Segments carry no per-segment kind. Storage
gains one nullable column `nodes.qualifiedPath`, one nullable column
`placeholder_targets.keyPath` and `idx_nodes_name`, and the schema goes from
version 9 to 10. The 3.18 upgrade already reindexes every project, so this
costs no extra reindex. Protocol version stays 2, because both fields are
additive. A plugin that sends no path gets today's behaviour for its symbols,
and core never parses its strings. User-visible output does not change.

## 1. Inventory: who writes and who reads `qualifiedName`

### How it was gathered

g-mesh served project `g-mesh` from the main checkout. That checkout is on
`release-3.18.0` at `8901b93`, the same commit this branch starts from.

| g-mesh call | answer |
|---|---|
| `find_references("WireNode.qualified_name")`, `find_definition("qualified_name")` | no match (`semanticNeighbours`). The served index has no field nodes: it reports 0 `nativeKind='field'` rows, so it was built before GM-450. Field-level reads were therefore found with grep (below). |
| `find_references("TargetKey::QualifiedName")` | no match (enum variants are not nodes). Grep was used instead. |
| `get_file_outline("wire/src/lib.rs")` | `TargetKey` :151, `PlaceholderTarget` :168, `WireNode` :214, `CURRENT_PROTOCOL_VERSION` :46 |
| `find_callers("graph::queries::find_by_qualified_name")` | `mcp::find_definition::resolve_symbol_name` plus one test (complete) |
| `find_callers("mcp::find_definition::resolve_symbol_name")` | `find_definition::by_name`, `mcp::anchor::resolve` (complete). All five symbol tools go through these two. |
| `find_definition("text_to_embed")` | `embedding::text::text_to_embed(doc_comment, signature)`. It does **not** take `qualifiedName`. |
| `find_callers("cli::embed_eval::context::owner_of")` | `parent_ref` (GM-455 eval only) |
| `find_callers("extractor::keys::qualified_in")` (Rust plugin) | `ModuleCtx::qualified`, `Bodies::tail_in`, `Bodies::member_of` |
| `find_callers("ids::node_id")` (SDK) | `FileGraphBuilder::add_node`, `graph::placeholder_id`, the Rust and Python `Emitter::{declare, reexport, external_module}`, the toy plugin, and tests |
| `find_callers("graph::NodeSpec::new")` | SDK-internal callers only. **This is a g-mesh gap:** the 13 cross-crate `NodeSpec::new(...)` calls in `plugins/{rust,python}/src/extractor/{decls,emit,bodies}.rs` are missing from the result, and grep is what found them. |
| `find_callers("render_target")` | ambiguous: `sdk graph::render_target` and Python's private copy |

The grep sweep (`grep -rla`) counted `qualified_name|qualifiedName` across
`*.rs`, `*.ts`, `*.go`, `*.py` and `*.sql`. It found **119 files**, most of
them tests and fixtures. In the TS plugin, 4 of the 15 files in
`plugins/typescript/src` match. One of them, `extract.ts`, contains a NUL
byte, so plain `grep` treats it as binary and reports nothing; it needs
`-a`.

### Producers (5)

| producer | how `qualifiedName` is built | separators in use |
|---|---|---|
| `plugins/sdk` (`graph.rs` `NodeSpec`, `FileGraphBuilder`, `render_target`, `ids::node_id`; `lsp/bridge.rs::address_of`) | passed through from the plugin. `render_target` builds a placeholder label `<file>#<key>` or `<container>::<key>`. The LSP bridge copies a declaration's `qualifiedName` into a `TargetKey::QualifiedName`. | as given |
| Rust (`extractor/keys.rs` `qualified`/`qualified_in`, `decls.rs` `BlockCtx::tail`, `field_tail`, `impl_block`; `bodies.rs::tail_in`) | module path within the crate, then `T::m`, `T.f` (GM-450), `<X as Tr>::m`, impl-block nodes named `<X as Tr>`, raw identifiers `r#async` | `::`, `.` |
| Python (`decls.rs` via `scope::child_path`, `bodies.rs:718` keys, `emit.rs` labels) | lexical path within the module (`Outer.Inner.m`), and a module announcement whose name is its dotted key | `.` |
| TypeScript (`extract.ts::qualify`, :811, with call sites :1649 to :2000) | `prefix + sep + name`. Instance members use `#` and static or namespace members use `.`. Ambient module names such as `*.scss` and `png-chunk-text` are used as written. A private member is `C##priv`. | `#`, `.` |
| Go (`extract.go:603-617`, `:706`, `uses.go:221,772`, `semantic.go::addressOf`/`placeholder`) | `T.M` and `I.M`. Package-level names are bare. A semantic key is spelled "character for character" to match the declaration (semantic.go:740). | `.` |

Node ids hash the display string: `(filePath, kind, qualifiedName,
nativeKind)` in `sdk/src/ids.rs`, `extract.ts:531` and `go/ids.go:31`. Edge
ids hash the key string (`emit.rs:116`, `bridge.rs:876`). If the display
string is unchanged, every id is unchanged.

### Core consumers (by role)

| role | site | reads |
|---|---|---|
| wire type | `wire/src/lib.rs` `WireNode.qualified_name`, `TargetKey::QualifiedName` | — |
| ingest | `watcher/apply.rs:502` (WireNode to NodeRecord), `:574` (target key to `keyKind`/`key`) | display |
| storage | `storage/schema.rs:183` column, `:212` `idx_nodes_qualifiedName`, `:340` `placeholder_targets.keyKind`; `storage/write.rs:324` upsert; **`storage/language_swap.rs:50` `NODE_COLUMNS` / `TARGET_COLUMNS`** (an explicit column list, so a new column that is missing from it is silently dropped on a language swap) | display |
| rung 2, whole-name lookup | `graph::queries::find_by_qualified_name` (`=`), `find_definition::resolve_symbol_name` (`exact[0].name != name` as "genuinely qualified"), `find_candidates_by_name` (`NameColumn::QualifiedName`) | display, by equality |
| linker | `graph::symbol_links` `seeds` (:666, `qualified_name != name`), `Resolver::in_{file,container}_by_qualified_name` (:1106, :1115, by equality), `waiting_placeholders` (key equality) | display, by equality. No parsing. |
| import specifiers | `queries::import_specifiers_named` (:610) | placeholders only |
| tool output | `find_definition`, `get_file_outline`, `find_references`, `find_callers`/`find_callees`, `find_implementations`, `get_dependencies` (`resolvedFrom`), `search_code` | display, printed as-is |
| guidance | `cli/agent_instructions.rs:64`, `mcp/session_hints.rs:46` | the label `qualifiedName` |
| conformance | `protocol/conformance.rs:89` (placeholder shape), `cli/plugin_check/expectations.rs` (`file:qualifiedName` rows, the re-call by candidate `qualifiedName`) | display |
| eval only, not product | `cli/embed_eval/context.rs::owner_of` (strips `::`, `.` or `#`), `trait_impl_segment`; `embed_eval/queries.rs::symbol_name` (rsplit on `:.#/`); `embed_eval/churn.rs` (rewrites qNames); `eval/embedding/*.py` (`check_queries.symbol_name` regex, and `(filePath, qualifiedName)` keys) | **the only code that parses separators today** |

**Embedding text:** `text_to_embed(doc_comment, signature)` never sees
`qualifiedName`, so `TEXT_FORM_TAG` ("structured") does not change and
GM-467's rule does not come into play. If GM-455's structural context ever
reaches production text, its `owner_of` should read the path rather than the
separators.

**Bench:** g-mesh-bench reads `qualifiedName` from tool output only (5 TS
scripts, none of which parse an index). Unchanged display means unchanged
bench.

In total there are 5 producers and about 14 distinct core consumer sites, not
counting tests. **Every product consumer compares the display string for
equality**. The only code that parses it is in the eval tools. GM-469 and
GM-472 would add the first parsing in the product, which is what this task
replaces.

## 2. Census: why segments must keep their separators

This is `census.py` (session scratchpad) run over `.backup` copies of five
indexes. It counts declarations, meaning `kind != 'File'` and not a
placeholder or container.

| index | decls | Rust `<X as T>` | name contains `::`, `.` or `#` | **distinct qNames merged by a separator-free key** |
|---|---|---|---|---|
| g-mesh (this branch, schema 9, 1,625 fields) | 9,600 | 172 | 0 | **98 (49 pairs)**, all Rust: `Variant.model_dir` / `Variant::model_dir` (field vs getter) |
| ripgrep | 3,623 | 1,063 | 1 (`r#async`) | 0 |
| excalidraw | 4,501 | 0 | 1 (`*.scss`) | **4 (2 pairs)**: `Fonts#registered` / `Fonts.registered` (instance vs static) |
| requests | 947 | 0 | 0 | 0 |
| gin | 1,688 | 0 | 0 | 0 |

No `<X as T>` segment in any index contains `::`, `.` or `#`, because the
Rust plugin reduces a path to its last segment (`decls.rs::type_name`). The
plugin's fallback for non-path types (`&T`, `dyn a::Tr`) can produce one,
though, and only the plugin knows where that segment ends.

The finding that drives the shape: a key made of segments **without**
separators merges 51 pairs of distinct declarations. A field and a getter
with the same name are the common case in Rust, and GM-450 kept them apart on
purpose. TypeScript instance and static members are the other case. GM-472's
re-key (`head + member`) would link a field reference to a getter under such a
key. So the separator stays as data, but it is the plugin's data, carried per
segment and compared only for equality. Core never decides what counts as a
separator in stored data.

## 3. Decisions

### 3.1 Wire shape

```jsonc
// WireNode, new optional field (camelCase, like every other field)
"qualifiedName": "storage::index_store::IndexStore::read",
"qualifiedPath": [
  {"name": "storage"},
  {"sep": "::", "name": "index_store"},
  {"sep": "::", "name": "IndexStore"},
  {"sep": "::", "name": "read"}
]
// PlaceholderTarget, new optional field, present iff key is {qualifiedName}
"key": {"qualifiedName": "a::T.f"},
"keyPath": [{"name": "a"}, {"sep": "::", "name": "T"}, {"sep": ".", "name": "f"}]
```

- **Segments are `{sep, name}`, and `sep` is omitted on the first segment.**
  The alternative was a flat alternating array. I chose objects because they
  are self-describing and schemars can document them. The cost is about 40 to
  80 bytes per node on the wire, which is noise next to signatures and doc
  comments.
- **Invariants**, which `protocol::conformance` and `g-mesh plugin check`
  enforce:
  - the path is non-empty;
  - no `name` is empty;
  - every `sep` after the first segment is non-empty;
  - no element contains U+001F or NUL, so storage can use U+001F as a joiner;
  - the concatenation of each `sep` and `name` equals `qualifiedName`
    (or `key`);
  - for a declaration, the last `name` equals the node's `name`. The census
    found zero declarations where `name` is not a suffix of `qualifiedName`.
- **No per-segment kind.** The census shows the separator already carries
  the distinctions that matter, and the owners are nodes with their own kind.
  A language-neutral kind enum would make core map query separators onto
  kinds language by language, which is the separator table again.
- **Which nodes carry a path:** declarations should. Placeholders, `File`
  nodes, `external_module` nodes and core-materialized containers omit it,
  because their `qualifiedName` is a label or a path, not a symbol path. Core
  treats a missing path as one segment, the display string itself.
- **SDK, so plugins get it cheaply:** a `QualifiedPath` type in `wire`
  (shared by core and the SDK, like every other wire type) with `root(name)`,
  `child(sep, name)`, `display()`, `head()` and `last()`.
  `NodeSpec::with_path(kind, name, path, range)` sets `qualified_name =
  path.display()`, so the two cannot disagree, and the id is unchanged.
  `NodeSpec::new` stays for path-less nodes. The LSP bridge's `address_of`
  copies `node.qualified_path` into `keyPath`. The Rust plugin's
  `ModuleCtx::qualified(tail)` and Python's `scope::child_path` return a
  `QualifiedPath`. Their file-model lookup keys can stay display strings.
  TS (`qualify`) and Go (6 string-concatenation sites) build the arrays next
  to the strings they already build.

### 3.2 Protocol version and plugins that send only a string

**Recommendation: no version bump. Both fields are optional and additive.**
`CURRENT_PROTOCOL_VERSION` is "bumped on any breaking change", and this change
does not break anything. A v2 plugin that does not know these fields still
loads.

- **Missing path:** store NULL, and treat it as the single segment
  `[qualifiedName]`. **Never derive segments by parsing.** That would bring
  back the per-language table this task exists to remove, and it would
  mis-split a third-party language whose separators core has never seen. The
  effect is today's behaviour for that plugin's symbols. Rungs 1 to 3 work.
  The GM-469 rung cannot match a partial path into its symbols, and GM-472
  does not split its keys, so those references stay unresolved exactly as
  they are now.
- **Path that breaks an invariant at ingest:** drop the path, keep the node,
  and log once per plugin. Rejecting the whole diff over a label would cost a
  file's worth of symbols over something conformance exists to catch.
  `plugin check` fails on it.
- `g-mesh plugin check` reports declarations without a path as a **warning**
  for third-party plugins. The bundled plugins' conformance tests treat it as
  an **error**.
- Rejected: bump to v3 and refuse v2. That breaks every third-party plugin
  for a feature that degrades gracefully.
- Rejected: v3 while still accepting v2. That contradicts the stated rule
  "a mismatch is a hard load failure, never best-effort compatibility".

### 3.3 Storage

| change | why |
|---|---|
| `nodes.qualifiedPath TEXT` (nullable). Encoding: `name0 US sep1 US name1 ...` with US = U+001F | GM-469's boundary check decodes it for a handful of candidate rows. It is not indexed, since nothing looks a node up by it. |
| `placeholder_targets.keyPath TEXT` (nullable, same encoding) | GM-472: head = all but the last segment, member = the last segment with its separator |
| `CREATE INDEX idx_nodes_name ON nodes(name)` | GM-469's candidates are `name IN (last segments)`. It also turns today's rung 3 (`find_by_name`, a full scan) into an index seek. |
| `qualifiedName` and `idx_nodes_qualifiedName` unchanged | rung 2, the linker's exact lookups and GM-472's re-key all stay display-string equality |
| `CURRENT_SCHEMA_VERSION` "9" to "10" | `ALTER TABLE` is not allowed here ("no migration framework"). A wipe plus reindex is free inside 3.18. |
| `language_swap.rs` `NODE_COLUMNS`/`TARGET_COLUMNS` gain the two columns | otherwise a language swap silently drops paths. The verify slice needs a control for this. |
| new columns nullable, with no default needed | dozens of test `INSERT INTO nodes (...)` statements name their columns and stay valid |

Rejected: a segments table `(nodeId, posFromEnd, sep, name)`. Every match
the two tasks need is anchored on the last segment, and that is already the
`name` column. A table would cost a join and roughly one row per segment
(about 4 per Rust node) for nothing.

Rejected for now: a reversed-path column with its own index (`qualifiedRev`,
prefix range = "last k segments spelled exactly"). It is as fast as
`idx_nodes_name` (see the table below), but it costs another column plus a
1.16 MB index, and it serves only this one query.

**Measured** on a copy of g-mesh's own index (19,902 nodes; `bench.py` in the
session scratchpad). Paths were simulated with a bracket-aware split. 200
two-segment tail queries, seed 474, 5 repetitions after warm-up:

| query | ms per lookup |
|---|---|
| rung 2 today, `qualifiedName = ?` | 0.019 |
| GM-469 candidate: `name IN (...)` + path check, **no** name index | 7.31 |
| same, **with `idx_nodes_name`** | **0.106** |
| reversed-path indexed range (rejected alternative) | 0.096 |
| GM-472 re-key: head display + sep + member, exact | 0.018 |

- The `name IN` approach and the reversed-path range returned identical
  match sets on all 200 queries, and the sampled declaration was found in
  172 of them. The other 28 queries were whole two-segment names, which
  rung 2 already answers.
- Size, after `VACUUM` on a 66.4 MB index (vectors dominate it):
  `qualifiedPath` adds about 1.08 MB with every row filled (fewer once
  placeholders and files are NULL), and `idx_nodes_name` adds 0.46 MB.
- Insert cost of `idx_nodes_name`: copying 19,902 rows into a 3-index
  `nodes` table took 580 to 605 ms without the index and 582 to 702 ms with
  it, best of 5 per arm. That difference is inside the noise on this
  machine, so call it "not measurable here", not zero.
- **Machine state:** `uptime` load averages were 6.6 / 17.3 / 71.4 before
  and 5.8 / 15.6 / 68.0 after. Bench run: `real 17.96, user 9.54, sys 6.57`.
  Insert run: `real 15.66, user 9.41, sys 5.12`. The machine was busy, so
  read the ratios, not the absolute numbers.

### 3.4 Splitting a user's query (core, language-neutral)

A query is a string, so core needs exactly one splitter. It is used only for
GM-469's candidate names and its tier-B comparison. Tier A never splits the
query (§4). The rules:

| case | example | rule |
|---|---|---|
| separators | `a::B.c#d` | `::`, `.`, `#`, at bracket depth 0 only |
| Rust raw identifier | `Foo::r#async` | `#` does not split when the segment text so far is exactly `r`. A TS class named `r` with a member `async` is misread, but only in tier B; tier A compares display text. |
| TS private member | `C##priv`, `C#priv` written by an agent | a `#` directly after a separator, or at the start of the query, belongs to the name: `[C, #priv]` |
| generics, trait impls | `Vec<T>::new`, `<Stream as Read>::read` | `<...>` is one depth level, and separators inside it do not split. A query segment's trailing `<...>` is dropped before tier-B comparison, because stored Rust segments carry no generic arguments. |
| trait-impl alias (matching, not splitting) | `Stream::read` vs stored `<Stream as Read>` | a stored segment of the form `<X as T>` also matches the query segment `X`, with generics stripped. It never matches `T`. This is the one syntax-aware rule left; it reuses `trait_impl_segment` and now runs on one segment instead of searching a whole string. |
| empty segment, or a leading or trailing separator | `::read`, `IndexStore::`, `a..b` | not a partial path, so the rung is skipped |
| quoted or odd names | `*.scss`, `"foo.bar"` | rung 3 matches the whole name first. A stored segment `foo.bar` matches only as a whole segment, never across its inner `.`. |
| specifier or file path | contains `/` or starts with `@` | not a symbol path (`is_module_specifier`) |
| case | — | case-sensitive, with `=` and never `LIKE` (`_` is a `LIKE` wildcard) |
| `crate::`, `self::`, `super::` prefixes | `crate::a::T` | not handled. Stripping them is Rust-specific, so such a query falls to rung 5. This is a known limit. |

## 4. How GM-469 and GM-472 work on top of this

**GM-469 (query-time rung 3.5).** The query enters only if it is a partial
path (§3.4), and only on the miss path of rungs 1 to 3, as the existing design
already specifies. The candidates are `name IN (last_segments(query))` with
`idx_nodes_name`. **Tier A, exact spelling:** a candidate matches if its
display string, read from the start of some segment `i ≥ 1`, equals the query.
Segment starts come from `qualifiedPath`, not from a separator table. So
`Store::read` does not match `IndexStore::read`, `T.f` matches only the field,
and `C.m` matches only the static member. The trait-impl alias adds the
spelling with `<X as T>` replaced by `X`. **Tier B, only if tier A is
empty and if the owner accepts it:** compare the names of the query's split
segments with the candidate's trailing segment names, ignoring separators. One
match resolves with `resolvedBy: qualifiedNameSuffix`, and several give the
`nameAmbiguous` page, both unchanged from the design. A plugin with no path
gives single-segment candidates, which never match a partial path.

**Reusable from `wip/GM-469-s7-separator-table` (9f3b471):** all the plumbing.
That is `find_by_names`, `find_candidates_by_ids`/`rank_candidates`,
`ResolvedBy::QualifiedNameSuffix`, the `by_qualified_name_suffix` arm, the
anchor test and the `find_definition` tests, the conformance cases (Rust
`Ledger::settle` flip, TS `#`/`.`), the ladder doc, README and
`agent_instructions`/`mcp/mod.rs` text. Of `suffix.rs`, `is_partial_path`
and `last_segments` stay, and they become §3.4's splitter.
`separators(language)` and `at_boundary` are deleted and replaced by the
path-boundary check. `trait_impl_self_spelling` becomes a per-segment alias
check.

**GM-472 (index-time, `graph::symbol_links`).** This applies to a
`qualifiedName` placeholder that finds nothing in its scope and has a
`keyPath` of at least two segments. Head = `keyPath[..n-1]`, and its last
name is the name to walk. Member = `keyPath[n-1]`, as `(sep, name)`. The
steps:

1. Walk the head **by name** with the existing breadth-first re-export walk.
2. Require exactly one visible head.
3. Re-key as `head.qualifiedName + member.sep + member.name` and look it up
   with the existing exact `qualifiedName` query in the head's scope. The
   separator comes from the placeholder, so a field reference can never
   re-key onto a getter.

For `link_diff`, the "head appeared" trigger filters scope-matched
placeholders on the second-to-last name in `keyPath`. The "member appeared"
trigger takes the head's display string as the member's display string minus
the last `sep + name` of its own `qualifiedPath`, then looks it up exactly.
Nothing parses a string, and the design's rules (exactly one head, ambiguous
head = no link, no new `resolvedBy`) are unchanged. A key with no `keyPath`
(a v2 plugin) is not split, which is today's behaviour.

## 5. Migration and compatibility

- **User-visible output:** none changes. `qualifiedName` in every tool
  response is the same display string, and `qualifiedPath` is not exposed.
  The ids are unchanged, so edges, vectors and the embedding cache carry
  over. `TEXT_FORM_TAG` is unchanged.
- **Schema 10:** wipe plus reindex, inside the 3.18 reindex everyone already
  takes.
- **Conformance:**
  - `protocol::conformance` gains the path invariants (§3.1).
  - Each bundled plugin's conformance test fails on a declaration without a
    path.
  - `plugin check` warns for third-party plugins.
  - `[[definition]]` expectations stay display-keyed.
  - The control: blank one plugin's path emission and its conformance must
    fail.
  - A second control: remove the new column from `language_swap.rs` and a
    swap test must lose the path.
- **ADRs:**
  - **new ADR 0015**, "qualifiedName is plugin-segmented; core never parses
    separators", covering the shape, the no-bump compatibility rule and the
    rejected options (separator table, segment table, v3 bump).
  - **update `multi-language-plugins.md`**, "Interfaces > Wire v2": the two
    optional fields and the invariants.
  - **update `symbol-resolution-ladder.md`**, which GM-469 does.
- **Plugin authoring:** the SDK doc comment on `NodeSpec::with_path` is the
  guidance. `docs/` has no separate authoring guide to update.

## 6. Slice plan check

The planned order is core (wire, SDK, storage, lookups), then plugins in
parallel (Rust + Go, Python + TS), then verify. The order is right. Three
changes:

1. **Core has no "lookups" to change.** Every product read stays
   display-equality (§1). The core slice covers:
   - the wire types and `QualifiedPath` in `wire`;
   - `NodeSpec::with_path` and the LSP bridge `keyPath`;
   - ingest validation (`watcher/apply.rs`);
   - schema 10, `write.rs` and `language_swap.rs`;
   - `protocol::conformance` and `plugin check`;
   - accessors (`NodeRecord.qualified_path`, decode).
   The GM-469 rung and the GM-472 walk stay in their own tasks, which are
   rebased onto this one. The core slice must keep `NodeSpec::new` compiling,
   so the plugins build unchanged until their slice.
2. **Pair the plugins by toolchain:** Rust + Python, then TS + Go. Rust and
   Python share the SDK `NodeSpec` idioms (`ModuleCtx::qualified`,
   `child_path`, `emit.rs` labels, `TargetKey::QualifiedName` keys). TS and
   Go each build their own wire structs from string concatenation. Each pair
   needs its own worktree.
3. **Verify** owns the controls in §5, plus a reindex of g-mesh showing that
   every declaration has a valid path (a census query) and that tool output
   is byte-identical before and after on a fixed set of queries.

## Owner decisions

1. **Wire shape:** per-segment `{sep, name}` plus the display string
   (recommended), or names only. Names only merges 51 field/getter and
   instance/static pairs, which GM-472 would then link wrongly.
2. **Compatibility:** optional fields under protocol 2, a v2 plugin keeps
   today's behaviour, and core never parses (recommended). The alternative is
   a v3 bump that refuses v2.
3. **Bad path at ingest:** drop the path and keep the node (recommended), or
   reject the diff.
4. **`idx_nodes_name`** in this task (recommended now). It is the index that
   segment matching anchors on, and it also speeds rung 3. GM-469's design
   had deferred it to a separate task.
5. **GM-469 tier B** (separator-insensitive) is still that task's decision.
   This design supports either answer.
