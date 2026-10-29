# GM-655: indexed lookup of the importers an Affected delta selects

Status: design (S9), for the owner decision "(B) S1 + индексный поиск в core
(Recommended)". No production code changes in this slice.

## Background, for a reader with no context

`src/a.ts` has `import './missing'`. No such file exists, so the TypeScript
plugin leaves an unlinked placeholder: an `IMPORTS` edge from `src/a.ts` to a
placeholder node whose `placeholder_targets.scope` is `src/missing`
(`scopeKind = 'file'`). Then `src/missing.ts` (or `src/missing/index.ts`) is
created. Core links a placeholder by itself only when a new `File` node's path
*equals* the scope, and `src/missing` never equals `src/missing.ts`, so the
import stayed unlinked until a full reindex.

- **48bf50b (S1)** fixed this in the TS plugin: on a file's first `fileChanged`
  per plugin model, `TsProject::source_changed` answers
  `ResolutionDelta::Affected` with one `ImportSelector { importers: everywhere,
  by: Target { File, Exact(address) } }` per address the file can satisfy
  (`resolve::addresses_resolving_to`: `src/missing.ts` -> `src/missing`,
  `src/missing.js`). Core re-extracts every importer stored as pointing at one
  of those addresses; the re-extract resolves in the resolver's own order.
  Stateless about importers, so it works after a restart and for any
  specifier shape the extractor folds.
- **Cost (S2):** core answers every such delta with a scan of all imports of
  the language, inside the exclusive section, once per first-seen file (after
  every plugin start and `workspaceChanged`, and per created file).
- **eff9f36 (S5)** tried to avoid the delta from the plugin side (a waiting
  set plus a word scan of all files). S6 found it loses links (split-literal
  specifiers after a restart; `./m.js` not re-pointed to a new `src/m.ts`) and
  costs a 22.5 s synchronous scan on a 22.9k-file repo.
- **Owner decision:** go back to S1 and make core's lookup indexed.

## 1. Where core scans today

Call chain (g-mesh `find_callers`, worktree index):

- `watcher::apply::apply_file_change_in` (`core/src/watcher/apply.rs`
  222-345) receives the plugin's `affected` from the `fileChanged` round trip;
  on `Affected { files, imports }` (line 262, ignored for `reextract` round
  trips) it calls
- `watcher::apply::reextract_affected` (346-399; its only caller), which runs
  `store.step(|conn| select(conn, language, files, imports, Some(file_path)))`
  (line 362) inside the caller's exclusive section, then re-extracts the
  selection one `fileChanged{reextract:true}` at a time.
- `daemon::config_reindex::select` (`core/src/daemon/config_reindex.rs`
  186-206; callers: `reextract_affected` and `config_reindex::selective`, the
  config-change path) applies the 30% `FALLBACK_SHARE_PERCENT` threshold to
- `daemon::config_reindex::select_affected` (218-267; callers: `select` and
  the unit-test helper `config_reindex::tests::select`), which runs two queries:

```sql
-- (a) every indexed file of the language, for file scopes and the 30% share
SELECT filePath FROM nodes WHERE kind = 'File' AND language = ?1;

-- (b) every IMPORTS edge of the language, matched in Rust (StoredImport::matches)
SELECT src.filePath, e.specifier, t.kind, t.filePath, c.key, pt.scopeKind, pt.scope
FROM edges e
JOIN nodes src ON src.id = e.fromId
JOIN nodes t ON t.id = e.toId
LEFT JOIN containers c ON c.nodeId = t.id
LEFT JOIN placeholder_targets pt ON pt.nodeId = t.id
WHERE e.kind = 'IMPORTS' AND src.language = ?1;
```

Producers of `ImportSelector` (g-mesh `find_references(ImportMatch)` plus its
unlinked-usage files, then grep in those files):

| producer | selectors | when |
|---|---|---|
| TS `TsProject::source_changed` (S1) | `Target{File, Exact}` | first `fileChanged` per path per model |
| SDK `resolution::container_delta` (Rust/Python `facts::delta`) | `Target{Container, Exact}`, `Specifier(Under)` | config facts change |
| Rust/Python `facts.rs` | `Target{File/Container, Exact}`, `Specifier(Under)` | config facts change |
| TS `facts.rs` | `Specifier(Under / StartsWith / Exact / NonRelative)` | tsconfig/package change |

**Why it is a full scan.** `EXPLAIN QUERY PLAN` of (b) on the 912c53c index:

```
SEARCH src USING INDEX idx_nodes_container (language=?)
SEARCH e USING INDEX idx_edges_fromId (fromId=?)
SEARCH t USING INDEX sqlite_autoindex_nodes_1 (id=?)
SEARCH c USING INDEX sqlite_autoindex_containers_1 (nodeId=?) LEFT-JOIN
SEARCH pt USING INDEX sqlite_autoindex_placeholder_targets_1 (nodeId=?) LEFT-JOIN
```

SQLite walks *every node of the language* (the `language` prefix of
`idx_nodes_container`), then *every outgoing edge of each* (all kinds), and
keeps the `IMPORTS` ones. The selector is applied afterwards, in Rust. So the
cost is proportional to the language's nodes plus their edges, whatever the
selector asks, and (a) also walks every node of the language. A correction to
S2: the cost does not come from `edges.kind` having no index. An index on
`kind` would still read every `IMPORTS` edge of every language; it is the
*selector* that carries the key, and nothing uses it in SQL today.

## 2. The indexed form

**Decision 2.1: look up `Target{_, Exact}` selectors by their key through
indexes that already exist; keep the scan only for the other selectors.**

- Today: every selector, including the exact-key ones S1 sends, costs one
  scan of the language's imports.
- Change: in `select_affected`, a `Target { scope_kind, matcher: Exact(key) }`
  selector runs two keyed queries, each starting at the key. An unlinked edge
  is found through its placeholder's scope. A linked edge is found through the
  node it was linked onto:

```sql
-- File, unlinked: placeholder scope == key
SELECT src.filePath FROM placeholder_targets pt
JOIN edges e ON e.toId = pt.nodeId
JOIN nodes src ON src.id = e.fromId
WHERE pt.scopeKind = 'file' AND pt.scope = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1;

-- File, linked: target File node's path == key (no placeholder row, as StoredImport::target)
SELECT src.filePath FROM nodes t
JOIN edges e ON e.toId = t.id
JOIN nodes src ON src.id = e.fromId
WHERE t.filePath = ?2 AND t.kind = 'File'
  AND NOT EXISTS (SELECT 1 FROM placeholder_targets pt WHERE pt.nodeId = t.id)
  AND e.kind = 'IMPORTS' AND src.language = ?1;

-- Container, unlinked: same as File with pt.scopeKind = 'container'
-- Container, linked:
SELECT src.filePath FROM containers c
JOIN edges e ON e.toId = c.nodeId
JOIN nodes src ON src.id = e.fromId
WHERE c.language = ?1 AND c.key = ?2 AND e.kind = 'IMPORTS' AND src.language = ?1;
```

  Each importer row is kept only if `selector.importers.contains(path)`, as
  `StoredImport::matches` does now. If any selector is not exact-key
  (`Specifier(_)`, or `Target` with `Under`/`StartsWith`/`NonRelative`), the
  existing scan runs once for those selectors only, unchanged.
- Plans (EXPLAIN QUERY PLAN, 912c53c copy):
  - File unlinked: `SEARCH pt USING INDEX idx_targets_scope (scopeKind=? AND scope=?)`
    -> `SEARCH e USING INDEX idx_edges_toId` -> `SEARCH src USING INDEX sqlite_autoindex_nodes_1`.
  - File linked: `SEARCH t USING INDEX idx_nodes_filePath (filePath=?)`, a
    correlated `SEARCH pt USING COVERING INDEX sqlite_autoindex_placeholder_targets_1`,
    then `idx_edges_toId`, `nodes` pk.
  - Container linked: `SEARCH c USING INDEX sqlite_autoindex_containers_2 (language=? AND key=?)`,
    then `idx_edges_toId`, `nodes` pk. Container unlinked: same as File unlinked.
- Example: `src/missing.ts` created. S1 sends `Exact("src/missing")` and
  `Exact("src/missing.js")`. That is four index probes, each returning the few
  edges pointing there, and no walk over the language.
- Consequence: the cost grows with the number of matching edges, not with the
  size of the index. Results are the same row for row, so the existing
  `config_reindex::tests` (file target linked and placeholder, container
  exact, importer scope, union) must stay green unchanged.

**Decision 2.2: no new index, no schema change, no migration.**

- Today: `idx_targets_scope (scopeKind, scope, key)`, `idx_nodes_filePath`,
  `idx_edges_toId` and `containers UNIQUE(language, key)` already exist; the
  linker (`graph::imports`) already uses the last two for the same lookups.
- Change: nothing in `storage::schema`.
- Consequence: the write path costs nothing extra. An index on
  `edges(kind, toId)` was considered and is not needed: `idx_edges_toId`
  already starts at the target, and the few edges into one target are
  filtered on `kind` cheaply.

**Decision 2.3: count the language's indexed files only when the count is used.**

- Today: query (a) runs for every delta. For a Rust-sized language it costs
  ~26 ms (measured below), even when the selection is empty, and the empty
  selection is the common case: a newly seen file that nobody waits for.
- Change: `select_affected` returns only the selected set. Query (a) moves
  into a small `indexed_files_of(conn, language)` called (i) by
  `select_affected` only when `files` is non-empty (a file scope needs the
  list), and (ii) by `select` only after the selection turned out non-empty
  (the 30% share). `Selection::Nothing` returns before any count.
- Example: `src/util2.ts` created with nothing waiting for it. Today the cost
  is (a) plus (b). Indexed, it is four probes and returns `Nothing`.
- Consequence: the `tests::select` helper's
  `assert_eq!(indexed, TS_FILES.len())` moves to a direct test of the count
  helper. Alternative, not recommended: a partial index
  `nodes(language) WHERE kind = 'File'`. It would make (a) cheap always, but
  it is a schema migration plus a write on every File upsert, to speed up a
  query that the lazy form already skips in the common case.

## 3. Measured cost, today vs indexed

Setup: a copy of the cached 912c53c index (`index.db`, 96,331 edges; the
cached file was not written to), plus one synthetic unlinked TS placeholder
at `plugins/typescript/conformance/project/src/missing`. The real index has
no unlinked file-scoped TS import. The `sqlite3` CLI ran each query 200 times
in one process under `/usr/bin/time -p`, and per-call cost is the total
divided by 200. One process opening the database costs real 0.04 / user 0.02
(this sets the floor for the indexed rows). `uptime` load averages were
33.78 / 36.05 / 22.92 at the start and 45.65 / 39.38 / 25.43 at the end, so
the machine was busy and `real` minus `user+sys` is waiting time.

The language decides the scan cost. This index has only 261 TS nodes, so a
Rust-sized language (31,767 nodes, 86,243 outgoing edges, 2,711 IMPORTS)
stands in for a TS-heavy project of the same size:

| query (per call) | language | real | user | sys | rows |
|---|---|---|---|---|---|
| (b) import scan, today | typescript (261 nodes) | 1.5 ms | 1.1 ms | 0.1 ms | 37 |
| (a) file list, today | typescript | 0.35 ms | 0.25 ms | 0.05 ms | 67 |
| (b) import scan, today | rust-sized | **287 ms** | **159 ms** | 80 ms | 2,711 |
| (a) file list, today | rust-sized | 26 ms | 15.8 ms | 8.7 ms | 416 |
| File unlinked, indexed | rust-sized | 1.6 ms | 0.2 ms | 0.05 ms | 0 |
| File linked, indexed | rust-sized | 0.4 ms | 0.15 ms | 0.05 ms | 0 |
| File unlinked, indexed | typescript | 0.35 ms | 0.2 ms | 0.05 ms | 1 |
| File linked, indexed | typescript | 0.35 ms | 0.2 ms | 0.05 ms | 1 |

On the indexed rows, user time sits at the process-start floor. The 1.6 ms
`real` against 0.2 ms `user` is load wait, not work. The S2 figure
(user 0.16 s per scan) matches the rust-sized scan row.

Today, one S1 delta on a large language costs about 160 ms user and 290 ms
real (query (a) plus (b)), inside the exclusive section. Indexed, it is 2-4
addresses x 2 probes, under 1 ms user. A checkout that creates N TS files
pays N x that. The cost no longer scales with the index.

## 4. eff9f36: revert all of it

**Decision 4.1: `git revert eff9f36` (the whole commit), not a partial.**

- Today (HEAD = eff9f36): `project/waiting.rs` (the `Waiting` set and the
  `Mentions` word scan), `TsProject.root`, `TsProject.waiting`, and
  `extractor::unresolved_relative_addresses`, which runs a second parse and
  `Declarer` pass on every `fileChanged`. `TsProject::source_changed` takes
  `Option<BTreeSet<String>>` and asks `waiting.waited(..)` before offering
  anything. `addresses_resolving_to` filters with `!paths::escapes(base)`.
- Change: a full revert restores 48bf50b's
  `TsProject::source_changed(path, source: Option<&str>)`, which offers every
  address on first sighting, keeps the `announced` once-per-model set, and
  restores the `!base.is_empty()` filter. The commit touches only the four TS
  plugin files and no tests, so the revert is clean on HEAD (and on HEAD plus
  this note).
- Why not keep a part: the only standalone hunk is the `escapes` filter plus
  its doc line (`src/index.ts -> src`). `addresses_resolving_to` builds bases
  by stripping an extension or `/index` from a project-relative path, so it
  never produces `.` or `..`. `!base.is_empty()` and `!paths::escapes(base)`
  therefore keep the same set. Keeping the hunk would be a partial revert for
  no behaviour change.
- What S1 behaviour returns: stateless offers. Every first-seen file names
  all its addresses; core finds whoever waits there, from any process and
  after any restart, by what is stored.
- Consequence: no waiting set, no word scan (the 22.5 s first-question scan
  goes away), and no second `Declarer` pass per edit.

**S6 findings 1-2 go away:**

1. *Split-literal specifier after a restart* (`./${'mis'}sing`): the
   extractor folds it to `./missing`, so the stored placeholder scope is
   `src/missing`. S1 offers `Exact("src/missing")` without looking at any
   source text, and the File-unlinked probe finds it. The link is made.
2. *`./m.js` linked to `src/m.js`, then `src/m.ts` created*: S1 offers
   `src/m` and `src/m.js`. The File-*linked* probe finds the edge whose
   target `File` path is `src/m.js`, and the re-extract resolves `./m.js` in
   resolver order to `src/m.ts`. This needs the linked half of decision 2.1.
   An unlinked-only lookup would bring this regression back.

**What remains for GM-657** (re-point to a higher-priority candidate): S1
re-points a linked import only when its current target path *is* one of the
new file's addresses (`./m.js` on `src/m.js` -> `src/m.ts`). It does not
re-point when the linked target is a different candidate of the same
address: `./m` on `src/m.tsx` or `src/m/index.ts` when `src/m.ts` appears
(S2 finding 3, S6 c10). The stored target `src/m.tsx` is not an address of
`src/m.ts`, so no selector names it. That is GM-657's scope.

Also out of scope and unchanged: GM-658 (root `..` / `.`), and the core gap
"linked target deleted -> importer keeps no IMPORTS row -> re-created as
another candidate never relinks" (S6 finding 3; no row exists for any
selector to match).

## 5. Edit map

| # | fn | file | lines (HEAD eff9f36) | change |
|---|---|---|---|---|
| 1 | (whole commit) | `plugins/typescript/src/{extractor/mod.rs, project/mod.rs, project/resolve.rs, project/waiting.rs}` | - | `git revert eff9f36` as its own commit (`revert:` message) |
| 2 | `daemon::config_reindex::select_affected` | `core/src/daemon/config_reindex.rs` | 218-267 | split `imports` into exact-key `Target` selectors (keyed probes, decision 2.1) and the rest (existing scan, only if non-empty); return `BTreeSet<String>` only; file list only when `files` non-empty |
| 3 | new `indexed_importers(conn, language, scope_kind, key)` | same file, next to `select_affected` | - | the four keyed queries from 2.1 (`prepare_cached`), returning importer paths |
| 4 | new `indexed_files_of` / count | same file | - | query (a), shared by `select_affected` (file scopes) and `select` |
| 5 | `daemon::config_reindex::select` | same file | 186-206 | call the count only after `selected` is non-empty (decision 2.3); same `Selection` results |
| 6 | `StoredImport` / `matches` / `target` | same file | 270-311 | unchanged; used only by the residual scan |
| 7 | `tests::select` helper | `core/src/daemon/config_reindex/tests.rs` | 125-129 | adapt to the new return type; assert the count via the count helper |

Callers of each changed function (g-mesh `find_callers`): `select_affected` ->
`select`, `tests::select`; `select` -> `selective` (called by
`config_reindex::run`), `reextract_affected` (called by
`apply_file_change_in` only). No signature outside `config_reindex.rs`
changes. `select`'s signature and `Selection` stay as they are.

### Must-confirm for code/tests

1. `graph::imports` links a container placeholder with
   `find_container(placeholder.language, scope)`. Seen at
   `core/src/graph/imports.rs` ~340/380, so `c.language = ?1` (the
   importer's language) in the container-linked probe matches the scan.
   Confirm that the placeholder's language is always the importer's.
2. Equivalence: the indexed path returns exactly the scan's set for every
   `Target/Exact` case in `config_reindex::tests` (file linked, file
   placeholder, container exact, container exact not matching a file path,
   importer scope, union with file scopes). Those tests stay unchanged and
   green.
3. A test that tells indexed from scan apart (a control: revert item 2 and it
   must fail). Proposal: move the scan into its own `fn scan_imports` and make
   it observable in tests (a `#[cfg(test)]` call counter, or rusqlite
   `trace`/`profile` if that feature is enabled). Assert that a delta with only
   `Target/Exact` selectors never runs it, and that a `Specifier` selector
   does.
4. The count is lazy: a delta that selects nothing never runs query (a). Use
   the same observation as item 3.
5. S1 behaviours after the revert (S1 report 1-5): `.ts` and `dir/index.ts`
   link on create without a language reindex; `addresses_resolving_to`
   order; the second `fileChanged` returns `None`; delete forgets.
6. S6 regressions as tests: (a) plugin level: after a fresh `TsProject` load
   (a restart), `source_changed("src/missing.ts", Some(..))` returns
   `Affected` with `src/missing`, whatever the importer's text; (b) core `it`:
   `./m.js` linked to `src/m.js`, create `src/m.ts`, and the edge ends on
   `src/m.ts`. Remember the 30% fallback: the fixture needs filler files (S1
   used 6 + `a.ts`), or the fallback links too and the control cannot tell.
7. `Selection::TooMany` and `Nothing` semantics are unchanged for
   `config_reindex::selective`. A config change with `Specifier` selectors
   still scans, with the same result.
