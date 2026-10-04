# GM-486: mark caller/reference pages that may miss untyped receiver calls (M1)

Status: design, for owner review. Base: `release-3.20.0` at 5aa6db7 (GM-485,
GM-479 and GM-482 merged). Proposal origin: §4 of
[gm-485-local-receiver-types.md](gm-485-local-receiver-types.md).

## 0. The gap in one paragraph

After GM-485 the Rust plugin types a receiver when the same file says what
its type is (L1–L3). Then it emits a `CALLS` edge plus an open site with
`replaces`. Every other receiver call (closure, `for` or pattern bindings,
generics, `dyn`, fields, chains) is an open site with `replaces: None` and
nothing else. Open sites never reach core (`run::write_graph` doc: "core has
no field for them"), so `unlinked::probe` (GM-477) cannot see them. A
`find_callers` page on a method then reads as complete when it is not. M1
sends core the `(caller, method name)` pairs of those calls and discloses,
per anchor, the ones that may be calls of it.

## 1. Wire format

**Recommendation: a node field, not a new record kind.** Add this to
`WireNode` (`wire/src/lib.rs`):

```rust
/// Bare names of methods this node calls through a receiver whose type the
/// structural tier did not know, sorted and deduplicated. Write-side only.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub untyped_calls: Vec<String>,
```

The JSON key is `untypedCalls`. On the plugin side, `FileGraph` itself gets
no new field. The data is already in `FileGraph::open_sites`.
`FileGraphBuilder::finish` folds every `OpenSite` with
`kind == ReceiverCall && replaces.is_none()` into the `untyped_calls` of the
node whose id is the site's `from_id`, then sorts and deduplicates. The fold
is opt-in per extractor (`FileGraphBuilder::record_untyped_receiver_calls()`,
called by the Rust `Emitter::new`), because Python is an open question (D2).

Why the node field and not a separate NDJSON line or `FileChangeDiff` list:

- **`write_graph` does not change.** It serializes `WireNode` as it is.
  `BulkItem::parse` (`core/src/protocol/ndjson.rs`) tells lines apart by
  their required fields, with no discriminator. A third line kind would need
  one, and an older core would reject the line as malformed.
- **The incremental path comes for free.** `diff_file` compares nodes with
  `PartialEq`. When a function's set of untyped calls changes, the function
  becomes a delete plus upsert, which the diff already does whenever any
  node field changes. A separate list would need its own
  replace-on-`fileChanged` rule and its own `complete`/`Gone` handling.
- **Precedent.** `alias_paths` (ADR 0015) has exactly this shape: an
  optional `Vec` on `WireNode` that feeds only a child table.

**Compatibility.** The field is additive and optional on both sides.
`WireNode` has no `deny_unknown_fields`, so an older core ignores it, and a
plugin that does not send it (TypeScript, Go, an older Rust build) stores no
rows. `CURRENT_PROTOCOL_VERSION` stays `2`: it is "bumped on any breaking
change", and an absent field is not one (`qualified_path` and `alias_paths`
were added inside v2 the same way). The TypeScript, Go and Python plugins
**do not have to send it**. For them, no marker appears, which is today's
behavior. Instructions wording (§4) must therefore not promise the marker
for every language.

**Cost of the delete-plus-upsert.** An edit that changes only a receiver
call's name, without moving any range, now re-upserts the enclosing
function. `apply_diff`'s delete drops the node's vector, and
`EmbeddingPipeline::apply` re-embeds it. That is one node per such edit,
which is acceptable. The language-swap path re-embeds only on a text change
(`plan_text_changed`), so it is unaffected.

## 2. Core storage

**Table** (`storage/schema.rs` DDL, next to `qualified_suffixes`):

```sql
CREATE TABLE IF NOT EXISTS untyped_calls (
    name   TEXT NOT NULL,
    nodeId TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY (name, nodeId)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_untyped_calls_nodeId ON untyped_calls(nodeId);
```

The primary key serves the query (`name = ?`). The second index serves the
per-node deletes. Size on g-mesh is about 16.5k rows, as `(fromId, name)`
pairs.

**Schema version.** Bump `CURRENT_SCHEMA_VERSION` from "10" to "11". That
means the usual wipe and reindex, with no migration framework, the same as
"10" did for `qualified_suffixes`. A `CREATE TABLE IF NOT EXISTS` without a
bump would leave an existing index with an empty table. The marker would
then be silently missing until each file was reparsed, which is the failure
this task exists to remove. (A new plugin build changes `indexer_version`
too, but the schema bump states the reason.)

**Replacement in step with the file's rows.** The table is a per-node child
table, so it hooks into the one path every node write goes through,
`storage::write::apply_diff`. Bulk (`daemon::bulk_index::ingest_in` →
`commit`), `fileChanged` (`watcher::apply::to_storage_diff` →
`IndexStore::apply_file_diff_linked`, after `file_rows::widen`), semantic
answers and staging writes all reach it.

- **Delete loop:** `DELETE FROM untyped_calls WHERE nodeId = ?1`, next to
  the `declarations`, `placeholder_targets` and `qualified_suffixes`
  deletes. Foreign keys are off on the daemon connection, so the delete must
  be explicit.
- **Upsert:** clear the node's rows, then insert `node.untyped_calls`. This
  copies the `qualified_suffixes` block. A node re-sent with an empty list
  loses its rows.
- **File gone or `complete`:** `file_rows::widen` turns both into node
  deletes, so the delete loop covers them. Nothing in `file_rows` changes.
- **Semantic-answer upserts:** these are placeholders only, so they carry
  an empty list. The only effect is clearing rows that placeholders never
  have.
- **Language swap** (`storage/language_swap.rs`): the module doc says
  "a table missing from either keeps stale rows". Add `untyped_calls` with
  `UNTYPED_COLUMNS = "name, nodeId"` to the three places that list the
  child tables:
  - the `plan_attached` change detection, so a changed set upserts the node;
  - the `swap_attached` delete loop and a matching `INSERT … SELECT FROM
    staging.untyped_calls`;
  - the `delete_placeholders` delete list.
- **Other node deletes:** `graph::containers::delete_container` and
  `graph::imports::link` delete nodes that never carry rows. Add the delete
  anyway, so that the invariant "every site that deletes a node deletes its
  child rows" (schema comment) stays true by inspection.
- **`schema::wipe`:** add `DROP TABLE IF EXISTS untyped_calls`, before
  `nodes`.

`NodeRecord` gets `untyped_calls: Vec<String>`, write-side only like
`alias_paths`. A read leaves it empty. `to_node_record` copies it from the
wire.

**Conformance.** `protocol::conformance::node_shape_violations` should
reject `untypedCalls` on a node whose kind is not `File` or `Function`, and
empty names.

## 3. The query

New module `core/src/mcp/untyped.rs`, a sibling of `unlinked.rs`:
`pub(crate) fn probe(conn, capabilities, anchor, edge_kinds, file_paths) ->
Option<UntypedReceiverCalls>`.

**Which anchors count as methods.** `kind == Function` and either:

- (a) `nativeKind` is one of `method`, `trait_method` or `trait_impl_method`
  (the Rust plugin's member kinds, kept in a named core const), or
- (b) `unlinked::is_type_member` is true, as the language-neutral fallback
  for a plugin with other native kinds.

(a) is needed because `is_type_member` joins the parent segments and looks
for a `Type` node. For `<T as Tr>::m` that parent is `<T as Tr>`, which is
no node, so (b) alone would drop trait-impl methods, which are the most
common untyped targets. A free function never qualifies: `x.f()` cannot
reach a free `f` in Rust, so its rows would be pure noise.

**When the marker applies at all.** There is no language-level gate (owner
decision D3, below). A completed semantic pass can still leave sites
unanswered (GM-485's own miss came from a cold pass recorded as done; see
GM-487), so a gate on `language_semantic_pass_done` would hide the marker in
exactly the case it exists for. Instead each row is dropped once the
semantic tier has answered *that* call: the caller already has a semantic
edge to some node whose bare name is the row's name, wherever it landed.
After a complete pass the marker therefore disappears on its own; after a
partial one it keeps the calls that are still open. The method-anchor and
language checks below still apply.

**Rows counted.**

```sql
SELECT f.filePath, COUNT(*)
FROM untyped_calls u JOIN nodes f ON f.id = u.nodeId
WHERE u.name = ?name AND f.language = ?lang
  AND [f.filePath IN (?scope…)]
  AND NOT EXISTS (SELECT 1 FROM edges e
                  WHERE e.fromId = u.nodeId AND e.toId = ?anchorId
                    AND e.kind IN (?kinds…))
  AND NOT EXISTS (SELECT 1 FROM edges e JOIN nodes t ON t.id = e.toId
                  WHERE e.fromId = u.nodeId AND e.source = 'semantic'
                    AND t.name = u.name)   -- answered by the semantic tier
GROUP BY f.filePath
```

- "No edge to the anchor yet" counts edges of **any** `source`, syntactic or
  semantic, so a caller already on the page is never counted twice.
  `edge_kinds` is `["CALLS"]` for callers and `USAGE_EDGE_KINDS` for
  references.
- `count` is the number of calling **functions** (distinct `(nodeId,
  name)`), not call sites. The field doc says so.
- `files` is sorted by refs, then path, and capped at 20 (shared with
  `MAX_UNLINKED_FILE_TALLY`), with `filesTruncated` when the cap cuts it.
- **Cost:** one PK range scan on `name`, plus one `nodes` PK lookup and one
  `idx_edges_fromId` probe per row. The worst name on g-mesh is `get`, with
  725 candidate sites, so at most about 700 rows, well under a millisecond
  of SQLite work. It runs only on method anchors with an open gap. Errors
  are swallowed to `None`, as in `unlinked::probe`.

## 4. Response and wording

The field is `untypedReceiverCalls`, a sibling of `unlinkedUsages` on
`CallerPage` and `ReferencePage`. It has the same shape, `{count, files,
filesTruncated?, hint}`, and is absent when the count is 0. Generalize
`UnlinkedUsages` into a shared tally struct with a `hint` per use, rather
than copying it.

Hint (static):

> Method calls named like this symbol, made through a receiver whose type
> g-mesh did not infer (a closure or loop variable, a generic, `dyn`, a
> field or a call chain), from functions with no edge to this symbol yet.
> Some may call this symbol, so this page is not exact: check `files` before
> treating it as complete.

Handler changes, identical in `handle_callers_in` and `find_references::handle_in`:

- probe after `unlinked::probe`;
- add `wire_len` to the page reserve passed to
  `bound_page_reserving_two_tallies`;
- chain its `files` into `touched` (provenance pending files);
- set the field.

Instructions (`mcp/instructions.rs`): `P4_GENERIC` and `p4_named` say "for
those `hasMore: false` without `unlinkedUsages` is exhaustive" about bare
calls. Add one clause about methods: "a method page that may miss such calls
carries `untypedReceiverCalls` where the language reports them". Recheck
`INSTRUCTIONS_BYTE_CEILING` (1900) and the instruction snapshot tests.

Provenance (`mcp/provenance.rs` module doc, the "41 unresolved call sites"
paragraph): add a note. M1 does not break that refusal. The count is not
project-wide. It is restricted to the anchor's bare name and to callers with
no edge to it, and it is worded as "may". The provenance block itself is
unchanged.

## 5. Interactions

- **GM-477 `unlinkedUsages`: two markers, not one.**
  - `unlinkedUsages` means "a target was named and did not link". Those are
    placeholders, keyed by name and type segment.
  - `untypedReceiverCalls` means "no target is known". It is keyed by name
    only and is noisier.
  - The two never count the same call. A GM-485 typed site has `replaces:
    Some`, so it is excluded from the table, and its miss, if any, is a
    placeholder that GM-477 already discloses. Merging the two would blur a
    precise signal with a noisy one.
  - They do share the struct, the cap and the page reserve.
- **Semantic tier.** It is gated as in §3. During `Pending`, rows already
  answered onto the anchor are filtered by the edge test.
- **GM-485.** Its typed calls stay out of the table by construction. This
  includes the trait-impl miss (D3), which becomes a placeholder.
- **GM-479.** It reduces unlinked placeholders and does not touch this
  table.
- **GM-470/GM-480.** No interaction: the query never resolves names. It
  compares bare strings.

## 6. Alternatives, risks, decisions

| Option | Benefit | Risk / cost |
|---|---|---|
| **Node field + child table (recommended)** | Rides every existing node-row path: bulk, diff, widen, swap. No protocol bump. | Re-embeds a function when only its untyped call names change. Five delete sites to keep in step. |
| New NDJSON line + `FileChangeDiff.untypedCalls` per file | Node content unchanged. | Needs a discriminator in `BulkItem`, a per-file replace rule on `fileChanged`, `complete` and `Gone`, and a swap rule. More code and more ways to leave stale rows. |
| M2 placeholders, M3 wording only | See GM-485 §4. | Rejected there. |

**Risks.**

- **Noise for common names** (`get`, `push`, `len`, `iter`, `new` on
  builders). The files cap bounds the bytes, not the false positives. The
  later measure slice should report the distribution of `count` over g-mesh
  method anchors before the hint wording is final.
- **Byte cost:** at most about 1.1 KB on a page that carries it.
- **A stale gate:** if a semantic pass is wrongly recorded as done (the
  GM-485 D5 cold-start case), the marker is suppressed. The fix belongs in
  the bridge, not here.

**Decisions for the owner.**

- **D1.** Wire shape: node field `untypedCalls` (recommended) or a separate
  record kind?
- **D2.** Opt-in per extractor, Rust only for now (recommended), or the SDK
  fold on for Python too? In Python every attribute call is untyped, so the
  marker would appear on nearly every method page until pyright runs.
- **D3.** Gate the marker off once the language's semantic pass is done
  (recommended), or always show it and rely on the edge filter?
- **D4.** Two sibling fields (recommended) or one merged `mayMiss` field?
- **D5.** `count` means calling functions (recommended) or call sites? Call
  sites would need a site count per row, which the table does not keep.

## 7. Edit map

Line numbers are 1-based at 5aa6db7.

**Wire and SDK**

| File | Function / item | Lines | Change |
|---|---|---|---|
| `wire/src/lib.rs` | `struct WireNode` | 373–434 | add `untyped_calls` after `alias_paths` (432–433); round-trip test near 862 (`an_ordinary_node_carries_no_declarations_key_at_all` pattern: absent key when empty) |
| `plugins/sdk/src/graph.rs` | `struct FileGraphBuilder` | 392–397 | add `record_untyped: bool` |
| | `FileGraphBuilder::new` | 403–410 | init `false` |
| | new `FileGraphBuilder::record_untyped_receiver_calls` | after 410 | setter |
| | `FileGraphBuilder::finish` | 556–558 | fold untyped `ReceiverCall` sites into `node.untyped_calls` (sorted, dedup) when enabled |
| | `struct FileGraph` doc | 41–55 | note `open_sites` feed `untypedCalls` |
| `plugins/sdk/src/run.rs` | `write_graph` doc | 213–214 | doc only: untyped receiver calls now reach core on their node |
| `plugins/rust/src/extractor/emit.rs` | `Emitter::new` | 146–, builder at 154 | call `record_untyped_receiver_calls()` on the builder |
| `plugins/rust/src/extractor/tests.rs` | `a_receiver_call_produces_no_edge_and_one_open_site` and peers | — | assert `untypedCalls` on the enclosing fn for an untyped site and its absence for a typed (`replaces`) one |

**Core ingest and storage**

| File | Function / item | Lines | Change |
|---|---|---|---|
| `core/src/watcher/apply.rs` | `to_node_record` | 581–640 | copy `untyped_calls` |
| `core/src/storage/write.rs` | `struct NodeRecord` | 51–135 | add field (write-side only, doc like `alias_paths` 129–134) |
| | `NodeRecord::new` | 142–175 | init empty |
| | `apply_diff` delete loop | 329–336 | add the `untyped_calls` delete |
| | `apply_diff` upsert | after 435 | clear and insert, copying 424–435 |
| | tests | 1091–1150 | mirror the three `qualified_suffixes` tests (write, replace, delete) |
| `core/src/storage/schema.rs` | `CURRENT_SCHEMA_VERSION` + doc | 69–71 | "11" |
| | DDL | after 389 | table + index (§2) |
| | `wipe` | 1211–1230 | drop the table |
| `core/src/storage/language_swap.rs` | module doc | 7–16 | list the table |
| | consts | 50–57 | `UNTYPED_COLUMNS` |
| | `plan_attached` | 208–229 | add to the child-table loop |
| | `swap_attached` | 436–443 and 484–490 | add to the delete loop; add the `INSERT … FROM staging.untyped_calls` |
| | `delete_placeholders` | 576–583 | add the delete |
| `core/src/graph/containers.rs` | `delete_container` | 535–550 | add the delete (invariant) |
| `core/src/graph/imports.rs` | `link` | 363–368 | add to `drop_placeholder_dependents` (invariant) |
| `core/src/daemon/workspace_reindex.rs` | test `a_symbol_the_new_walk_drops_is_gone_with_every_row_it_owned` | 1172–1260 | add the table to its lists (1223, 1256) |
| `core/src/protocol/conformance.rs` | `node_shape_violations` | 72–79 | kind and empty-name rule |

**Core query and response**

| File | Function / item | Lines | Change |
|---|---|---|---|
| `core/src/mcp/unlinked.rs` | `UnlinkedUsages` | 46–79 | generalize into a shared tally (per-use hint); make `is_type_member` (185–201) `pub(super)` |
| `core/src/mcp/untyped.rs` (new) | `probe`, `candidate_sql`, `is_method` | — | §3; tests in `untyped_tests.rs` modelled on `unlinked_tests.rs` |
| `core/src/mcp/mod.rs` | module list | — | `mod untyped;` |
| `core/src/mcp/find_callers_callees.rs` | `struct CallerPage` | 173–215 | field after `unlinked_usages` (207) |
| | `handle_callers_in` | 356–444 | probe at 399, reserve at 404, `touched` at 416–422, set at 441 |
| `core/src/mcp/find_references.rs` | `struct ReferencePage` | 81–116 | field after 108 |
| | `handle_in` | 186–255 | probe at 210, reserve at 218, `touched` at 229–234, set at 252 |
| `core/src/mcp/instructions.rs` | `P4_GENERIC`, `p4_named` | 139–144, 162–171 | one clause (§4); check `INSTRUCTIONS_BYTE_CEILING` (22) |
| `core/src/mcp/provenance.rs` | module doc | 36–51 | note (§4) |

**Read for context only:**

- `instructions::has_open_receiver_gap` (89–94): the gate predicate to
  reuse; move it somewhere both modules can call.
- `daemon::manifest::Capabilities` (153–157).
- `provenance::resolve` (291–307).
- `storage::file_rows::widen` (45–96): no change.
- `diff::diff_file` (`plugins/sdk/src/diff.rs` 64–107): no change.
- `daemon::bulk_index::ingest_in` (348–397) and `commit` (402–412).
- `IndexStore::apply_file_diff_linked` (`storage/index_store.rs` 369–391).
- `Bodies::receiver_call` (`plugins/rust/src/extractor/bodies.rs` 557–579)
  and `Bodies::open_site` (1265–1285): the source of the sites.

**Controls for the implement slice.**

- With the fold disabled, the Rust fixture's untyped site must produce no
  row, and the core marker test must fail.
- With the `apply_diff` delete removed, the re-upsert and delete tests must
  fail.
- With the `language_swap` entries removed, a swap test must keep a stale
  row.
- With the "answered by the semantic tier" filter removed, a row whose call
  has a semantic edge to another same-named target must still be counted.

## Cross-file answers and where they came from

- **Who calls `write_graph`:** `run::bulk_index` and one test
  (`find_callers write_graph`). `fileChanged` does not use it; it goes
  through `diff_file`.
- **Who consumes `FileGraph` in core:** nobody (`find_references
  graph::FileGraph`). Every reference is in `plugins/`, and core sees only
  `WireNode`/`WireEdge` lines (`BulkItem::parse`) and `FileChangeDiff`.
- **Where a file's rows are replaced or deleted:** `apply_diff` is the
  writer (`find_callers storage::write::apply_diff`, plus grep for its
  non-test call sites). `file_rows::widen` turns file scope into node
  deletes; its only caller is `IndexStore::apply_file_diff_linked`
  (`find_callers storage::file_rows::widen`). `language_swap` copies child
  tables, and `to_node_record` is reached from `bulk_index::ingest_in` and
  `to_storage_diff` (`find_callers` by id). The child-table delete sites
  were enumerated by grep on `qualified_suffixes`.
- **Who calls `unlinked::probe`:** `find_references::handle_in` and
  `find_callers_callees::handle_callers_in` (`find_callers` by id).
- **What the two handlers call:** `anchor::resolve`, `list_calls` /
  `list_references`, `provenance::resolve`, `unlinked::probe`,
  `bound_page_reserving_two_tallies` (callers only), `tally_edge_files`,
  `tally_is_worth_sending`, `excluded_references` (callers only) and
  `session_hints::join` (`find_callees` on each).

## Owner decisions (approved)

Approved by the owner, verbatim: "да, хорошо".

- **D1.** The node field `untypedCalls` plus a child table, not a separate record kind.
- **D2.** Rust only for now; the SDK fold is opt-in per extractor.
- **D3.** No gate on a completed semantic pass. A row is dropped when the
  semantic tier has answered that call (a semantic edge from the caller to
  any node with the row's name), so a partial pass keeps the marker honest.
- **D4.** Two sibling fields, `unlinkedUsages` and `untypedReceiverCalls`.
- **D5.** `count` is the number of calling functions, not call sites.

## Measured noise

GM-486 S13. Corpus: `git archive 7c73009` of g-mesh itself (273 `.rs` files),
indexed by a release build of this branch into a scratch `G_MESH_HOME`, with
only the Rust plugin discovered. Two fresh `reindex` runs: **A** with the
manifest's `semantic_pass = false`, **B** with it `true` (rust-analyzer
1.97.1). B's pass completed: `language_state.semanticPassAt` set, no
`semanticPassError`, 273 files, 6,217 semantic edges. The structural walk is the
same in both: `untyped_calls` holds the identical 16,029 rows (706 distinct
names). Anchors are every Rust `Function` with `nativeKind` in
`method`/`trait_method`/`trait_impl_method` (1,389). Counts come from SQL
mirroring `untyped::candidate_sql` (edge kind `CALLS`, no scope) on every
anchor. That SQL matched `find_callers` on 5/5 anchors per phase (top 2 by
count, 2 seeded-random marked, 1 seeded-random unmarked): `count`, `files` and
byte size. `count` percentiles are over marked anchors only.

| | A: structural only | B: after semantic pass |
|---|---|---|
| method anchors | 1,389 | 1,389 |
| pages with `untypedReceiverCalls` | 589 (42.4%) | 195 (14.0%) |
| `count` p50 / p90 / max | 4 / 48 / 708 | 12 / 214 / 702 |
| `untyped_calls` rows | 16,029 | 16,029 |
| largest field (bytes) | 1,525 (`call_tool`, 28 files, truncated) | 1,525 (same) |
| field bytes p50 / p90 | 505 / 1,369 | 736 / 1,418 |

Top 15 names by `count`, with the number of marked anchors carrying the name:

- **A:** expect 708 (2), collect 624 (3), path 431 (6), get 355 (3), push 277 (3),
  as_str 241 (1), len 218 (3), is_empty 211 (6), filter 204 (1), lock 145 (2),
  insert 133 (2), prepare 99 (5), contains 95 (1), extend 95 (1), kind 79 (1).
- **B:** expect 702, collect 622, path 418, get 343, push 254, as_str 221,
  len 214, is_empty 209, filter 204, insert 109, lock 99, contains 95,
  extend 95, prepare 91, find 78.

The semantic pass removes about two thirds of the marked pages. These are
mostly the long tail of project-only names. What stays is a set of
project methods that share a name with a std method (`expect`, `collect`,
`get`, `push`, `len`, `is_empty`). rust-analyzer resolves those calls to std,
which is not indexed. So the caller has no semantic edge to any node of that
name, and the D3 filter keeps the row. Example: of the 708 callers of
`.expect` in `untyped_calls`, 382 have semantic edges, yet only 6 drop
out of the `expect` anchors' count (708 to 702). These markers are near-certain noise
on a complete pass. The page cost is bounded: `files` is capped at 20, so
the field never exceeded 1.5 KB.

Run: `reindex` took A `real 860 s, user 2114 s, sys 18 s` and B
`real 620 s, user 1555 s, sys 13 s`. The semantic pass itself took 206 s.
Most of the rest was embedding backfill. Load averages at the start were
174 / 208 / 170, from other work on the machine, and 8 / 27 / 86 at the end.
So these wall times are not a performance figure.

## Owner decision after the noise measurement

Approved by the owner, verbatim: "Первый вариант".

After a complete semantic pass, 14% of method pages still carried the marker.
Nearly all of that came from project methods whose names std also uses
(`expect`, `get`, `push`, `len`, ...). rust-analyzer had answered those calls
with std targets, which are not indexed, so the bridge recorded nothing and
the D3 SQL filter, which looks for a semantic edge to a same-named node,
kept the rows. **D3 is extended:** when the bridge gets an answer for an
untyped receiver-call site, wherever the answer lands, inside the index or
outside it, the call's name leaves the caller's `untypedCalls` once every
untyped site of that name in that caller is answered. The SQL filter stays as
a second guard.

## Measured noise after the extension

GM-486 S17, by the S13 method. Corpus: `git archive 84a82eb` (273 `.rs`
files), the same for every run. Release builds of 7c73009 (**before**, S13's
commit) and 84a82eb (**after**, the extension in 056d5f3), only the Rust plugin
discovered, a fresh `G_MESH_HOME` per run. Three `reindex` runs: A (after,
`semantic_pass = false`), B-before and B-after (both `true`, rust-analyzer
1.97.1). Both B passes completed: `semanticPassAt` set, no
`semanticPassError`, 273 files, 6,312 semantic edges each. SQL mirror and
`find_callers` agreed on 5/5 anchors in every run. B-before's
`untyped_calls` equals A's row for row, so A stands for both arms' structural
state.

| | A: structural | B-before (7c73009) | B-after (84a82eb) |
|---|---|---|---|
| pages with `untypedReceiverCalls` (of 1,389) | 589 (42.4%) | 195 (14.0%) | 51 (3.7%) |
| `count` p50 / p90 / max | 4 / 48 / 710 | 12 / 218 / 704 | 1 / 3 / 3 |
| `expect` count | 710 | 704 | 1 |
| `untyped_calls` rows | 16,172 | 16,172 | 154 |
| field bytes max / p50 / p90 | 1,525 / 505 / 1,369 | 1,525 / 736 / 1,415 | 502 / 389 / 436 |
| bridge node upserts in the pass | - | 3,270 | 7,462 (+4,192 re-sent callers) |
| embeddings computed (backfill + pass) | 8,055 + 0 | 8,055 + 0 | 4,221 + 4,192 = 8,413 |
| embedding time (backfill + pass) | 468 s | 503 s | 318 s + 339 s = 657 s |
| `reindex` real / user / sys | 478 / 1,719 / 10 s | 688 / 1,795 / 13 s | 757 / 1,886 / 15 s |
| load average at start (1/5/15) | 21 / 74 / 60 | 15 / 33 / 44 | 16 / 37 / 42 |

The row counts differ from S13 (16,029, `expect` 708/702) only because the
corpus is the later commit. The extension removed 16,018 of 16,172 rows on
4,192 callers. Top names after it: path 3, accept 2, area 2, len 2, raw 2,
then single calls. The marker is now a short list of real gaps rather than
std-named noise.

Cost. Every caller the bridge shortens is re-sent, which is 4,192 nodes in this
pass. In a `reindex` the semantic pass runs before the embedding backfill, so
none of them has a vector yet, and the "embed only nodes without a vector" rule
in `watcher::apply` filters nothing. Those nodes are embedded on the pass's
path instead of in the backfill (339 s). The total of 9,208 vectors is the
same, but 358 more texts were computed (8,413 against 8,055), because the
pass-path embedding got no cache hits. `reindex` wall time grew 69 s (+10%) and
user time 90 s (+5%), at similar load. The bridge's own pass time (171 s
before, 85 s after) is rust-analyzer variance, not the change. S13 recorded
`reindex` times under load 174/208/170 and no per-arm embedding split, so those
numbers are not compared here.
