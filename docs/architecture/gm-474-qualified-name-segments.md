# GM-474: qualifiedName as plugin-emitted segments

Status: design, revised in S6 for owner review. No product code changes in
this slice.

S6 revision (owner direction, 2026-10-01): core holds **no** language-specific
rules. S1 still had two in core, a query splitter (old §3.4) and the
`<X as T>` alias rule. Both are gone. Core now stores every boundary-anchored
suffix of a declaration's path as display text in an indexed table, and a
query matches it by exact equality without being parsed (§3.3, §3.4). Syntax
knowledge, such as "a trait-impl method is also reachable as `X::m`", reaches
core only as extra paths a plugin sends (§3.1). Sections 1 and 2 (inventory and
census) are unchanged from S1.

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
only ever concatenates. Segments carry no per-segment kind. A plugin may also
send `aliasPaths`, other spellings of the same declaration (Rust: `X::m` for
`<X as T>::m`). At ingest core joins every suffix of the path that starts at a
segment boundary and has at least two segments, plus the same for each alias,
and writes them to a new table `qualified_suffixes(suffix, nodeId)`. GM-469's
rung is one exact lookup in that table; core never splits the query. Storage
also gains the nullable columns `nodes.qualifiedPath` and
`placeholder_targets.keyPath` (GM-472 reads them), and the schema goes from
version 9 to 10. The 3.18 upgrade already reindexes every project, so this
costs no extra reindex. Protocol version stays 2, because every new field is
optional. A plugin that sends no path gets today's behaviour for its symbols,
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
// WireNode, new optional fields (camelCase, like every other field)
"qualifiedName": "storage::index_store::<IndexStore as Read>::read",
"qualifiedPath": [
  {"name": "storage"},
  {"sep": "::", "name": "index_store"},
  {"sep": "::", "name": "<IndexStore as Read>"},
  {"sep": "::", "name": "read"}
],
"aliasPaths": [[
  {"name": "storage"},
  {"sep": "::", "name": "index_store"},
  {"sep": "::", "name": "IndexStore"},
  {"sep": "::", "name": "read"}
]]
// PlaceholderTarget, new optional field, present iff key is {qualifiedName}
"key": {"qualifiedName": "a::T.f"},
"keyPath": [{"name": "a"}, {"sep": "::", "name": "T"}, {"sep": ".", "name": "f"}]
```

- **Segments are `{sep, name}`, and `sep` is omitted on the first segment**
  (recommended, to confirm). The alternative was a flat alternating array.
  Objects are self-describing and schemars can document them. The cost is
  about 40 to 80 bytes per node on the wire, which is noise next to
  signatures and doc comments.
- **Invariants of `qualifiedPath` and `keyPath`**, which
  `protocol::conformance` and `g-mesh plugin check` enforce:
  - the path is non-empty;
  - no `name` is empty;
  - every `sep` after the first segment is non-empty;
  - no element contains U+001F or NUL, so storage can use U+001F as a joiner;
  - the concatenation of each `sep` and `name` equals `qualifiedName`
    (or `key`);
  - for a declaration, the last `name` equals the node's `name`. The census
    found zero declarations where `name` is not a suffix of `qualifiedName`.
- **`aliasPaths`** (new in S6) is a list of paths that name the same
  declaration in another spelling. Each alias obeys the same element rules
  and ends in the node's `name`. It need not join to `qualifiedName`, since
  that is the point of it, but it must differ from `qualifiedPath` and have at
  least two segments, because a one-segment alias is just the bare name, which
  rung 3 already finds. Aliases are only inputs to the suffix table (§3.3):
  they are not stored on the node, not printed and not hashed into ids.
  Which aliases a plugin sends is the plugin's decision. My recommendations:
  - **Rust: send the trait-impl alias.** For a method whose path contains a
    `<X as T>` segment, send the path with that segment replaced by `X`
    (references, lifetimes, `dyn` and generics stripped, which only the
    plugin can do correctly). On g-mesh this adds 362 suffix rows (§3.3).
  - **Rust: do not send `crate::` forms by default.** Measured below, a
    `crate::`-prefixed alias per declaration more than doubles the table
    (11,207 to 26,360 rows) for a spelling agents rarely type.
  - **Never `self::`/`super::`.** They are relative to where the query is
    written, and `find_definition` has no such place.
  - Python, TypeScript and Go: no aliases in this task. A module-qualified
    alias (`pkg.mod.Class.m`, Go `pkg.T.M`) is a plugin-side follow-up if
    agents turn out to write those.
- **No per-segment kind.** The census shows the separator already carries
  the distinctions that matter, and the owners are nodes with their own kind.
- **Which nodes carry a path:** declarations. Placeholders, `File` nodes,
  `external_module` nodes and core-materialized containers omit it, because
  their `qualifiedName` is a label or a path, not a symbol path. A node
  without a path gets no suffix rows.
- **SDK, so plugins get it cheaply:** a `QualifiedPath` type in `wire`
  (shared by core and the SDK) with `root(name)`, `child(sep, name)`,
  `display()`, `head()` and `last()`. `NodeSpec::with_path(kind, name, path,
  range)` sets `qualified_name = path.display()`, so the two cannot disagree,
  and the id is unchanged. `NodeSpec::alias(path)` appends an alias.
  `NodeSpec::new` stays for path-less nodes. The LSP bridge's `address_of`
  copies `node.qualified_path` into `keyPath`. The Rust plugin's
  `ModuleCtx::qualified(tail)` and Python's `scope::child_path` return a
  `QualifiedPath`. TS (`qualify`) and Go (6 string-concatenation sites) build
  the arrays next to the strings they already build.

### 3.2 Protocol version and plugins that send only a string

**Recommendation, to confirm: no version bump.** Every new field is optional
and additive. `CURRENT_PROTOCOL_VERSION` is "bumped on any breaking change",
and this change does not break anything. A v2 plugin that does not know these
fields still loads.

- **Missing path:** store NULL and write no suffix rows. **Core never derives
  segments by parsing.** That would bring back the per-language table this
  task exists to remove. The effect is today's behaviour for that plugin's
  symbols. Rungs 1 to 3 work, the GM-469 rung finds nothing in them, and
  GM-472 does not split their keys.
- **Path that breaks an invariant at ingest (recommendation, to confirm):**
  drop the path and its aliases, keep the node, and log a warning once per
  plugin and file. An alias that breaks an invariant is dropped alone. Rejecting
  the whole diff over a label would cost a file's worth of symbols over
  something conformance exists to catch. `plugin check` fails on it.
- `g-mesh plugin check` reports declarations without a path as a **warning**
  for third-party plugins. The bundled plugins' conformance tests treat it as
  an **error**.
- Rejected: bump to v3 and refuse v2. That breaks every third-party plugin
  for a feature that degrades gracefully.
- Rejected: v3 while still accepting v2. That contradicts the stated rule
  "a mismatch is a hard load failure, never best-effort compatibility".

### 3.3 Storage

#### The suffix table

```sql
-- Every spelling by which a partial path finds a declaration: each suffix of
-- its qualifiedPath that starts at segment i >= 1 and keeps >= 2 segments,
-- and each suffix of each alias path that starts at i >= 0 and keeps >= 2
-- segments, written as display text (segment names joined by the path's own
-- separators). Matched by `suffix = ?` only, never by LIKE.
--
-- Written only through storage::write::apply_diff, which replaces a node's
-- whole set on every upsert of it, and copied by storage::language_swap.
CREATE TABLE IF NOT EXISTS qualified_suffixes (
    suffix TEXT NOT NULL,
    nodeId TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY (suffix, nodeId)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_qualified_suffixes_nodeId ON qualified_suffixes(nodeId);
```

- **Keys.** The primary key `(suffix, nodeId)` is the lookup index: a
  `WITHOUT ROWID` table is its own B-tree, so `suffix = ?` is one seek
  (`SEARCH qualified_suffixes USING PRIMARY KEY (suffix=?)`). It also
  deduplicates, so a suffix shared by the path and an alias is stored once.
  `idx_qualified_suffixes_nodeId` serves the per-node delete. Without it,
  every node delete or re-upsert scans the table.
- **Why the whole primary path (i = 0) is left out:** that string is the
  node's `qualifiedName`, which rung 2 already finds through
  `idx_nodes_qualifiedName`. Including it would add 7,389 rows on g-mesh (+68%)
  and answer nothing new. An alias's whole path is included, because no other
  index holds it.
- **Derivation is joining, not parsing.** For segments `p[0..n]`, the suffix
  at `i` is `p[i].name` followed by `p[j].sep + p[j].name` for each `j > i`.
  Core never looks inside a `name` or a `sep`.
- **Foreign key and cleanup.** The `REFERENCES ... ON DELETE CASCADE` is
  documentation only: foreign keys are off on the daemon's connection
  (`storage::connection::open`), so every site that deletes a node deletes its
  suffix rows explicitly, as it already does for `declarations`. Found with
  grep for `DELETE FROM declarations` (a known literal, so grep is the right
  tool). There are seven places, in four files:

  | site | when | change |
  |---|---|---|
  | `storage/write.rs::apply_diff`, `delete_node_ids` loop | a plugin's diff deletes a node | `DELETE FROM qualified_suffixes WHERE nodeId = ?` |
  | `apply_diff`, `upsert_nodes` loop | every node upsert, full or incremental | delete the node's rows, then insert its suffixes. This is the `declarations` "replace wholesale" pattern, with `prepare_cached`. |
  | `graph/imports.rs` (`drop_placeholder_dependents`) | `imports::link_diff` drops a linked-away placeholder | add the delete. Placeholders carry no path today. The delete keeps the "an orphan is inherited by the next node with this id" invariant independent of that. |
  | `graph/containers.rs::delete_container` | an empty container is removed | add the delete, for the same reason |
  | `storage/language_swap.rs::swap_attached` | a language swap | add `qualified_suffixes` to the child-table delete loop (`for table in ["declarations", "placeholder_targets"]`) and an `INSERT ... SELECT` from `staging.qualified_suffixes` for upserted nodes |
  | `storage/language_swap.rs::plan_attached` | planning a swap | add `("qualified_suffixes", "suffix, nodeId")` to the loop that marks a node upserted when its child rows differ. Otherwise an alias change that leaves every `nodes` column equal is never swapped in. |
  | `language_swap.rs::delete_placeholders` | dropping kept placeholders | add the delete |

  All of them are in core and none is language-aware.
- **Full and incremental reindex use the same path.** Node rows reach the
  database only through `apply_diff`, or through a language swap that copies
  rows a staging database got from `apply_diff`. g-mesh `find_callers` on
  `storage::write::apply_diff` names, among its files, `workspace_reindex.rs`,
  `watcher/burst.rs`, `index_store.rs`, `language_swap.rs` and
  `graph/queries.rs` as the product callers; the rest are tests. So suffix rows are written once, in `apply_diff`, and both reindex
  modes inherit it. `symbol_links::link_diff` writes edges and placeholder
  rows only, not declaration nodes, so it needs no change for the table. It
  needs one for GM-472 (§4).

#### Columns

| change | why |
|---|---|
| `nodes.qualifiedPath TEXT` (nullable). Encoding: `name0 US sep1 US name1 ...` with US = U+001F | GM-472's "member appeared" trigger needs the last `sep` of a declaration to compute its head (§4). It is not indexed. It is also what `language_swap`'s `NODE_COLUMNS` comparison sees, so a path change re-swaps the node. |
| `placeholder_targets.keyPath TEXT` (nullable, same encoding) | GM-472: head = all but the last segment, member = the last segment with its separator |
| `qualified_suffixes` + `idx_qualified_suffixes_nodeId` | GM-469 (above) |
| `qualifiedName` and `idx_nodes_qualifiedName` unchanged | rung 2, the linker's exact lookups and GM-472's re-key all stay display-string equality |
| `CURRENT_SCHEMA_VERSION` "9" to "10" | `ALTER TABLE` is not allowed here ("no migration framework"). A wipe plus reindex is free inside 3.18. |
| `language_swap.rs` `NODE_COLUMNS`/`TARGET_COLUMNS` gain the two columns | otherwise a language swap silently drops paths. The verify slice needs a control for this. |
| new columns nullable, with no default needed | dozens of test `INSERT INTO nodes (...)` statements name their columns and stay valid |

**`idx_nodes_name` is no longer part of this task.** S1 needed it because
GM-469's candidates were `name IN (last segments)`. The suffix table replaces
that. The one remaining reader is rung 3 (`queries::find_by_name`, `WHERE
name = ?`), which scans `nodes` today; S1 measured that shape at about 7 ms
on g-mesh against 0.1 ms indexed. That is a separate performance question,
worth its own task, and nothing here depends on it.

Rejected: a segments table `(nodeId, pos, sep, name)`. Matching a suffix
through it needs a join per segment, and its rows are per segment rather than
per suffix. Rejected: a reversed-path column with a range index (S1's
`qualifiedRev`). It needs the query split into segments to build the range
key, which is exactly the parsing the owner ruled out.

#### Measured

`suffix.py` (session scratchpad, `gm474/`) ran on `.backup` copies of three
indexes in a `mktemp` directory: g-mesh (Rust-heavy, schema 9), excalidraw
(TypeScript) and requests (Python). **No plugin emits paths yet, so paths
were simulated** in the script from the stored `qualifiedName` strings with
a per-language splitter (bracket-aware, `r#` and TS `#priv` handled), and
Rust trait-impl aliases were simulated by replacing a `<X as T>` segment with
`X`. That splitter exists only in the measuring script. Only declarations
(the census's definition) get rows.

Rows and size, from `dbstat` after `VACUUM`. "table" is the `WITHOUT ROWID`
B-tree that doubles as the lookup index.

| index | nodes / decls / decls with >= 2 segments | variant | rows | table | nodeId index |
|---|---|---|---|---|---|
| g-mesh, 68.2 MB file | 19,902 / 9,600 / 7,389 | primary, i >= 1 | 10,845 | 0.84 MB | 0.84 MB |
| | | **+ trait-impl alias (recommended)** | **11,207** | **0.86 MB** | **0.86 MB** |
| | | primary incl. i = 0 (rejected) | 18,234 | 1.44 MB | 1.44 MB |
| | | + alias + `crate::` alias (not by default) | 26,360 | 2.15 MB | 2.15 MB |
| excalidraw, 28.4 MB | 13,424 / 4,501 / 842 | recommended | 8 | 4 KB | 4 KB |
| requests, 6.2 MB | 1,635 / 947 / 579 | recommended | 98 | 12 KB | 12 KB |

For scale, g-mesh's `idx_nodes_qualifiedName` is 1.16 MB and its `nodes`
table 7.03 MB, so the recommended table plus index adds 1.72 MB, about 2.5%
of the file. TypeScript paths are almost all two segments (`C#m`, `C.s`), so a
TS project gets very few rows: a two-segment partial query there is a whole
`qualifiedName`, which rung 2 answers.

Insert cost on g-mesh: writing the 11,207 rows took 98 ms, against 112 ms to
copy the 9,600 declaration rows into a two-index copy of `nodes`. That is the
same order as writing the nodes themselves, on a loaded machine.

Lookups, in microseconds per lookup over 200 queries, 10 repetitions after a
warm-up, two runs. A "hit" returns full node rows. A "miss" is a query that
finds nothing. Rung 3.5 runs only on the miss path of rungs 1 to 3, so the
miss row is the cost every unresolved query pays.

| index | today: `qualifiedName = ?` hit | suffix `= ?` JOIN nodes hit | today miss | suffix miss |
|---|---|---|---|---|
| g-mesh | 147.6 / 109.2 | 92.8 / 68.4 | 41.0 / 33.4 | 41.8 / 30.9 |
| excalidraw | 151.1 / 85.3 | 64.9 / 32.5 | 48.0 / 22.3 | 32.4 / 15.5 |
| requests | 137.3 / 70.8 | 65.0 / 32.8 | 37.0 / 17.4 | 32.3 / 16.7 |

- **Ratios: suffix/today is 0.68 to 1.02 on misses and 0.38 to 0.63 on
  hits.** Both are single index seeks. The miss pair is the like-for-like
  comparison. The hit pair is not quite: today's sample is random whole
  names, some of which return several rows (bare names such as Go `main`),
  while the suffix sample is two-segment tails. Read it as "no slower", not
  as "faster".
- Recall on g-mesh: for 200 random declarations with >= 3 segments, the
  two-segment tail found the declaration 200 times, with one match 179 times
  and several 21 times (at most 19, e.g. `tests::setup`). The three-segment
  tail found it 142 times; the other 58 were whole three-segment names, which
  rung 2 answers. requests: 68 of 68, with one match 56 times.
- **Machine state:** `uptime` load averages were 9.59 / 14.36 / 40.13 before
  the first run, 8.24 / 13.42 / 38.60 before the timing runs and
  21.91 / 16.18 / 38.87 after. `/usr/bin/time -p` for the g-mesh timing runs:
  `real 8.02 user 3.91 sys 2.95` and `real 7.99 user 4.11 sys 2.88`, so the
  process was mostly on CPU rather than waiting. The machine was busy, so
  read the ratios, not the absolute numbers.

### 3.4 Matching a user's query: no parsing

A query string is looked up as it is: `SELECT ... FROM qualified_suffixes
WHERE suffix = ?1`, joined to `nodes` for the rows. Case-sensitive, never
`LIKE` (`_` is a `LIKE` wildcard). Core does not split the query, strip
generics or prefixes, or check that it "looks like a path". A string that is
not a stored suffix misses, at the cost of one index seek.

What this means for spellings, since each case S1's splitter handled is now
either stored or a miss:

| query | result | why |
|---|---|---|
| `IndexStore::read` | match | stored suffix |
| `Store::read` vs stored `IndexStore::read` | miss | only whole segments start a suffix |
| `Variant.model_dir` vs `Variant::model_dir` | matches only the one spelled that way | the separator is part of the stored text |
| `Stream::read` for `<Stream as Read>::read` | match, if the Rust plugin sends the alias | plugin knowledge, delivered as an alias |
| `Foo::r#async` | match | stored segment `r#async` |
| `C##priv` | match only if the path has >= 3 segments; a two-segment `C##priv` is the whole `qualifiedName`, rung 2 | stored text |
| `Vec<T>::new`, `crate::a::T` | miss, unless a plugin sends such an alias | no stripping in core |
| `::read`, `a..b`, `*.scss` | miss (or rung 3 for a whole name) | nothing stored looks like that |

The separator-insensitive tier (S1's "tier B", `Fonts.registered` finding
`Fonts#registered`) is declined by the owner and not designed here. An agent
has to spell a separator the way the tool output prints it. GM-469's
guidance text should say so.

### 3.5 Language-specific rules left in core

After this design, **nothing in core parses or interprets `qualifiedName`**.
S1's splitter (`is_partial_path`, `last_segments`, `separators(language)`,
`at_boundary`, the `r#`/`#priv`/`<...>` rules) and the trait-impl alias rule
(`trait_impl_self_spelling`) are not built. Their knowledge goes to the Rust
plugin's `aliasPaths`. A sweep of `core/src` for separator literals (`"::"`,
`'#'`, `rsplit`, `" as "`, `'<'`), excluding tests, found nothing else that
touches symbol names. Three related items, none of them about
`qualifiedName`:

| item | where | language-specific? | proposal |
|---|---|---|---|
| `cli/embed_eval` (`owner_of`, `trait_impl_segment`, `symbol_name`) and `eval/embedding/*.py` | eval tooling, not shipped in the server's query path | parses `::`, `.`, `#` | read `nodes.qualifiedPath` instead once it exists; a follow-up inside GM-455's eval work, not this task |
| `mcp/find_definition.rs::is_module_specifier` (`@` prefix or contains `/`) | gates the semantic-neighbour rung | yes: the shape of an npm or Go import specifier | kept as a spelling rule: a lookup cannot express it (GM-475, below) |
| `embedding/text.rs` (Markdown headings, `<url>` links in doc comments) | embedding text | doc-comment Markdown, shared by all languages | none needed |

The SDK's `render_target` (`<file>#<key>`, `<container>::<key>`) builds
placeholder labels on the plugin side, so it is not core.

**`is_module_specifier` stays a spelling rule (GM-475).** The proposed
lookup, "the query equals a stored module key or file path", was built and
measured, and it does not preserve answers. Every stored specifier is
answered before the semantic rung: a file path is a `File` node's
`qualifiedName` (rung 1), and an import placeholder's specifier is answered
by `import_only_refusal`. Only a container key (`containers.key`) reaches the
check. What the check actually catches are specifiers that no table holds: a
TypeScript relative specifier (`./extract.js`), stored only as the file it
resolves to, and a package the project never imports (`@types/node`). A
lookup returns "not a specifier" for both, so they reach the semantic rung
and get whatever clears the floor.

Measured through the MCP shim, with embeddings, on copies of
`plugins/typescript` (401 queries: every import specifier in the sources, 150
sampled names, 40 truncated names, 50 qualified names, every file path, the
placeholder specifiers, 16 synthetic) and `plugins/go` (335 queries, same
classes plus the 4 container keys). The lookup turned 55 TypeScript
refusals into `semanticNeighbours` pages: 47 import specifiers written in
those sources, almost all relative (`./a`, `../src/extract`); 6 synthetic
(`@types/node`, `github.com/nope/pkg`, `@`); and 2 regex captures of comment
text. Go changed 1 answer, the synthetic `./extract`, because every real Go
import is stored as a placeholder or a container key. Every other answer was
byte-identical. The lookup query itself costs about 56 µs on g-mesh's own
index (2,000 runs: 0.112 s real, 0.108 s CPU), using the `indexed_files` and
`nodes.qualifiedName` indexes plus a covering scan of `containers`.

A data-driven version needs the index to hold the specifier as written.
That is a plugin change: for example, the TypeScript plugin storing each
relative import's raw specifier the way it stores external ones, so that
`import_only_refusal` answers it. Even then, packages the project never
imports would still reach the semantic rung. The rule's cost runs the other
way: a non-specifier name with `@` or `/` (`@Component`) is refused instead
of being offered neighbours.

## 4. How GM-469 and GM-472 work on top of this

**GM-469 (query-time rung 3.5).** It runs only on the miss path of rungs 1
to 3, as that task's design already says. The rung is a single query:
`qualified_suffixes.suffix = query`, joined to `nodes` and filtered to
declarations. **One match** resolves with `resolvedBy: qualifiedNameSuffix`.
**Several** return the `nameAmbiguous` candidate page, ranked by the existing
`rank_candidates`. **None** falls through to rung 4 and then rung 5, as today.
There is no partial-path gate and no tier B. A plugin that sends no paths has
no rows, so its symbols are never matched by this rung, which is today's
behaviour.

Reusable from `wip/GM-469-s7-separator-table` (9f3b471): the plumbing, that
is `find_candidates_by_ids`/`rank_candidates`,
`ResolvedBy::QualifiedNameSuffix`, the `by_qualified_name_suffix` arm, the
anchor test and the `find_definition` tests, the conformance cases (Rust
`Ledger::settle` flip, TS `#`/`.`), the ladder doc, README and
`agent_instructions`/`mcp/mod.rs` text. **All of `suffix.rs` is deleted**,
including `is_partial_path`, `last_segments`, `separators(language)`,
`at_boundary` and `trait_impl_self_spelling`. `find_by_names` is replaced by
one `find_by_qualified_suffix(conn, query)` query in `graph::queries`. The
Rust `Ledger::settle` conformance case for a trait impl moves to the Rust
plugin's alias test.

**GM-472 (index-time, `graph::symbol_links`).** Unchanged from S1, and it
does not use the suffix table. It applies to a `qualifiedName` placeholder
that finds nothing in its scope and has a `keyPath` of at least two segments.
Head = `keyPath[..n-1]`, and its last name is the name to walk. Member =
`keyPath[n-1]`, as `(sep, name)`. The steps:

1. Walk the head **by name** with the existing breadth-first re-export walk.
2. Require exactly one visible head.
3. Re-key as `head.qualifiedName + member.sep + member.name` and look it up
   with the existing exact `qualifiedName` query in the head's scope. The
   separator comes from the placeholder, so a field reference can never
   re-key onto a getter.

For `link_diff`, the "head appeared" trigger filters scope-matched
placeholders on the second-to-last name in the decoded `keyPath`. The "member
appeared" trigger takes the head's display string as the member's
`qualifiedName` minus the last `sep + name` of its stored `qualifiedPath`,
then looks it up exactly. Nothing parses a string, and the design's rules
(exactly one head, ambiguous head = no link, no new `resolvedBy`) are
unchanged. A key with no `keyPath` (a plugin without paths) is not split,
which is today's behaviour.

## 5. Migration and compatibility

- **User-visible output:** none changes. `qualifiedName` in every tool
  response is the same display string, and neither `qualifiedPath` nor an
  alias is exposed. The ids are unchanged, so edges, vectors and the embedding
  cache carry over. `TEXT_FORM_TAG` is unchanged.
- **Schema 10:** wipe plus reindex, inside the 3.18 reindex everyone already
  takes.
- **Conformance:**
  - `protocol::conformance` gains the path and alias invariants (§3.1).
  - Each bundled plugin's conformance test fails on a declaration without a
    path.
  - `plugin check` warns for third-party plugins.
  - `[[definition]]` expectations stay display-keyed.
- **Controls the verify slice builds** (each must fail with its target
  reverted, in its own worktree):
  - blank one plugin's path emission: its conformance fails;
  - remove the suffix delete from `apply_diff`'s delete loop: a test that
    deletes a node and re-adds a different node under the same id finds a
    stale suffix row;
  - remove `qualified_suffixes` from `language_swap`'s child-table loops: a
    swap test that changes only an alias loses the change;
  - remove the new columns from `NODE_COLUMNS`/`TARGET_COLUMNS`: a swap test
    loses the path;
  - stop the Rust plugin sending the trait-impl alias: `Stream::read` for
    `<Stream as Read>::read` no longer resolves.
- **ADRs:**
  - **new ADR 0015**, "qualifiedName is plugin-segmented; core never parses
    separators", covering the shape, aliases, the suffix table, the no-bump
    compatibility rule and the rejected options (separator table, query
    splitter, segment table, reversed-path index, v3 bump, separator-
    insensitive matching).
  - **update `multi-language-plugins.md`**, "Interfaces > Wire v2": the three
    optional fields and their invariants.
  - **update `symbol-resolution-ladder.md`**, which GM-469 does.
- **Plugin authoring:** the SDK doc comments on `NodeSpec::with_path` and
  `NodeSpec::alias` are the guidance. `docs/` has no separate authoring guide
  to update.

## 6. Slice plan check

The order core, then plugins, then verify still holds. Changes from S1's
check:

1. **Core grows by the suffix table and shrinks by the splitter.** The core
   slice covers:
   - the wire types, `QualifiedPath` and `aliasPaths` in `wire`;
   - `NodeSpec::with_path`, `NodeSpec::alias` and the LSP bridge `keyPath`;
   - ingest validation and suffix derivation (`watcher/apply.rs` to
     `NodeRecord`);
   - schema 10 (two columns, the table and its index), `write.rs`, and the
     seven delete/copy places in §3.3, including both `language_swap` loops;
   - `protocol::conformance` and `plugin check`;
   - accessors (`NodeRecord.qualified_path`, decode) and
     `queries::find_by_qualified_suffix`.
   The GM-469 rung and the GM-472 walk stay in their own tasks, rebased onto
   this one; GM-469 shrinks to one query arm. The core slice must keep
   `NodeSpec::new` compiling, so the plugins build unchanged until their slice.
2. **Plugins, paired by toolchain:** Rust + Python, then TS + Go, each pair in
   its own worktree. Rust also sends the trait-impl alias and owns its
   conformance case. Python, TS and Go send paths only.
3. **Verify** owns the controls in §5, a reindex of g-mesh with a census query
   showing every declaration has a valid path and the suffix-row count is in
   line with §3.3's simulation, and byte-identical tool output before and
   after on a fixed set of queries.

## Owner decisions

To confirm (recommendations, not yet confirmed by the owner):

1. **Path shape:** per-segment `{sep, name}` plus the display string
   (recommended). Names only would merge 51 field/getter and instance/static
   pairs, which GM-472 would then link wrongly.
2. **No protocol version bump:** optional fields under protocol 2, a plugin
   without paths keeps today's behaviour, and core never parses (recommended).
3. **Bad path at ingest:** drop the path (and its aliases), keep the node, log
   a warning (recommended). A bad alias is dropped alone.
4. **Suffix set:** primary-path suffixes from segment 1, alias suffixes from
   segment 0, both with at least two segments (recommended; 11,207 rows and
   1.72 MB on g-mesh). Including the whole primary path adds 68% for nothing
   rung 2 does not already answer.
5. **Rust aliases:** the trait-impl alias only; no `crate::` form by default
   (it takes g-mesh from 11,207 to 26,360 rows); never `self::`/`super::`.

Decided by the owner: no separator-insensitive tier; no language-specific
rule in core.

Separate from this task: an index on `nodes(name)` for rung 3's full scan.
`is_module_specifier` stays a spelling rule (§3.5).
