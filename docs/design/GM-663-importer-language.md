# GM-663: owe a gone file's importers under the importer's language

Status: design (S1). No production or test change in this slice; no probe
was needed (the question is answered from the code). Line numbers are
1-based at `7f7036e` (release-4.20.0, GM-665 merged; code equal to main).
Follows GM-659 (`GM-659-reextract-importers-of-a-gone-file.md`).

## The problem in one example

`lib.ts` is deleted. `use.ts` and `tool.py` both have an `IMPORTS` edge
linked onto `File(lib.ts)`. GM-659's `file_rows::importers_of`
(`core/src/storage/file_rows.rs` 110-127) reads the gone file's language
(`typescript`) and every importer path regardless of its own language, and
`Writer::apply_file_diff_linked` (`core/src/storage/index_store.rs`
470-495) owes all of them under `typescript` and returns them to the
watcher. The watcher re-extracts `tool.py` through the TypeScript plugin's
process (`watcher/apply.rs` 266-293, `reextract_affected` 366-427), and the
next start's `config_reindex::resume_owed_reextracts` (`config_reindex.rs`
526-586) would do the same: the wrong plugin for that file.

## 1. Can a cross-language IMPORTS edge exist today?

**No, with every shipped plugin (typescript, python, rust, go).** An
`IMPORTS` edge lands on a real node only through `graph::imports::link`
(`core/src/graph/imports.rs` 322-422), which repoints a resolved
placeholder's edge onto what its structured address names. The two scopes:

- **`container` scope** (python, rust, go): looked up as
  `SELECT nodeId FROM containers WHERE language = ?1 AND key = ?2`
  (`imports.rs` 349-351, bound at 391-394) with the placeholder's own
  language. Language-keyed: it cannot land on another language's container.
  Python emits only `container_target` (`plugins/python/src/extractor/emit.rs`
  454, used from `decls.rs`/`bodies.rs`), Rust likewise
  (`plugins/rust/src/extractor/emit.rs` 330), Go addresses an import path as
  a container key (`plugins/go/extract.go` 543-560).
- **`file` scope** (only TypeScript, `plugins/typescript/src/extractor/keys.rs`
  120 `file_target`, used at `imports.rs` 216/245/262): looked up as
  `SELECT id FROM nodes WHERE kind = 'File' AND filePath = ?2`
  (`imports.rs` 343-345) - **no language filter in core**. The guarantee is
  on the plugin side: the TS resolver only returns a candidate in its
  existence set (`plugins/typescript/src/project/resolve.rs` 92-102,
  module doc 17-20), which is the SDK walk over the plugin's own extensions
  (`plugins/typescript/src/project/mod.rs` 92) plus presence updates, and the
  SDK applies presence only to paths the plugin `claims` by extension
  (`plugins/sdk/src/run.rs` 789, 817-819). Its candidates are TS/JS
  extensions only (`resolve.rs` 34-43, 71-90): `./x.json`, `./styles.css`,
  `./script.py` never resolve.

The catalogue's extensions are disjoint (`core/src/languages.rs` 118-139:
`.ts .tsx .mts .cts .js .jsx .mjs .cjs` all map to `typescript`; `.d.ts` is
`.ts`; `.py .pyi` are `python`), and a discovered manifest claiming an
extension another plugin claims is refused (`core/src/daemon/manifest.rs`
723). So `.js`, `.d.ts` and `.pyi` imports are same-language by
construction. The semantic pass emits no `IMPORTS` edges (the SDK's only
`EdgeKind::Imports` builder is `graph.rs` 585-597, the unresolved
placeholder edge).

The project already states it: "imports never cross a language boundary
(design doc: Non-goals - Cross-language edges)" (`imports.rs` 136-139,
`docs/architecture/multi-language-plugins.md` 59-60), and GM-499 accepted
the same assumption ("no shipped plugin pair emits cross-language imports",
`docs/architecture/gm-499-retry-failed-language.md` 62).

**Where it could still happen**: a third-party (or future) plugin whose
`file`-scope target names a path another plugin indexes. Core does not stop
it (the `find_file` lookup above), and a node's `language` is whatever the
plugin sent (`watcher/apply.rs` 1191; only `g-mesh plugin check` compares it
with the manifest, `cli/plugin_check/checks.rs` 523). So the gap is
unreachable today but one plugin away, and core's handling should not depend
on it.

## 2. Options

### (A) Key each owed row by the importer's language; return only the gone file's language (recommended)

`importers_of` returns each importer with its own `File` node's language.
`apply_file_diff_linked` groups them by language and calls
`schema::owe_reextracts` once per language (trigger = the gone file), in the
same commit as today, and returns only the importers whose language equals
the gone file's: the ones the calling plugin (which serves the gone file's
extension) can re-extract in its live loop. Other-language rows stay owed
and the next start's `resume_owed_reextracts` re-extracts them with their
own plugin (it already iterates per language, `config_reindex.rs` 534, and
drops rows of a language with no plugin, 535-540).

- Benefit: each importer is re-extracted by its own plugin; no other caller
  changes (`watcher::apply`, `retire_file`, `purge_language` keep their code);
  identical rows and return value for every same-language importer, so no
  visible change with shipped plugins.
- Risk: a cross-language importer is fixed at the next start, not live (the
  edge into the deleted node dangles until then or until the importer is
  edited - the pre-GM-659 behaviour, for that importer only). `purge_language(L)`
  clears only `L`'s rows (`bulk_index.rs` 284-306), so an other-language
  importer's row survives the purge - correct, since its import into the
  purged file does need re-resolving.

### (B) Filter the importers to the gone file's language

Add `AND src.language = t.language` to the importer query.

- Benefit: one-line change; nothing re-extracted by a wrong plugin.
- Risk: a cross-language importer is silently never re-extracted (dangling
  edge until its own edit or a reindex of its language): GM-659's bug
  returns for exactly that case, with no row recording it.

### (C) Route other-language importers to their own plugin live

Return a per-language map and have the daemon re-extract each group through
`registry.get_or_spawn(language)`.

- Benefit: live fix for every importer.
- Risk: the watcher's apply loop runs inside one plugin's process
  (`apply_file_change_in`'s reader/writer); reaching another plugin from there
  is a new cross-plugin path (locks, threshold, fallback reindex) for a case
  no shipped plugin produces. Not worth it now.

### (D) Document the restriction only

Acceptance allows it, since (section 1) the edge cannot exist with shipped
plugins. Risk: the code keeps doing the wrong thing the day a plugin emits
one; (A) costs a few lines more and removes the dependency.

### Link semantics

Unchanged under (A), (B) and (D): `graph::imports` is not touched, and with
shipped plugins every owed row, every returned importer and every re-extract
is the same as today. (A) changes only which language an owed row is keyed
by when an importer's language differs, which cannot happen today. **No
owner review of semantics needed**; the A-vs-D scope choice is minor.

## 3. Edit map (option A)

| Where | Change |
|---|---|
| `core/src/storage/file_rows.rs` `importers_of` 110-127 (doc 102-109) | Return the gone file's language plus `(language, path)` per importer, sorted by path. Read the importer's language from its `File` node: `JOIN nodes f ON f.filePath = src.filePath AND f.kind = 'File'`, `SELECT DISTINCT COALESCE(f.language, src.language), src.filePath` (LEFT JOIN, so an importer without a `File` node, which no plugin produces, still falls back to its node's language rather than vanishing). Doc: per-importer language. |
| `core/src/storage/index_store.rs` `Writer::apply_file_diff_linked` 470-495 (doc 456-469) | Group the pairs by language (`BTreeMap`), `owe_reextracts(conn, &importer_language, file_path, &paths)` per group, return the paths whose language equals the gone file's. Doc: other-language importers are only owed, for the next start. |
| `core/src/storage/file_rows.rs` module doc 23-26 | One clause: each importer is owed under its own language. |
| `core/src/watcher/apply.rs` `apply_file_change_in` doc (about 225-232) | Optional one line: `gone_importers` are the plugin's own language only. |

Not changed: `schema::owe_reextracts` (`schema.rs` 1235-1246),
`schema::owed_reextracts` (1251-1282, groups by language already),
`watcher::apply::round_trip`/`reextract_affected`,
`config_reindex::resume_owed_reextracts`, `select_with` (its threshold now
sees only same-language importers, which is the right denominator for
`indexed_file_count(language)`), `PluginRegistry::retire_file`,
`bulk_index::purge_language`.

Read for context: `config_reindex::resume_owed_reextracts` 526-586,
`watcher/apply.rs` 234-347 and 366-427, `bulk_index::purge_language`
284-306, the GM-659 tests `index_store.rs` 869-937.

## 4. Behaviours for tests

Store-level tests in `index_store.rs`'s test module (`file_rows` has none),
seeded like `imported_lib` (869-893) with `store_without_foreign_keys`:

1. A gone file with a same-language importer and an other-language importer
   (a `File` node `tool.py`, language `python`, with a linked `IMPORTS` edge
   into `File:lib.ts`): `owed_reextracts` holds `("python", "lib.ts",
   ["tool.py"])` and `("typescript", "lib.ts", [... ts importers])`; the
   returned vector holds the TS importers only.
2. A gone file whose only importers are of another language: returns empty
   (the watcher starts no live re-extract), and their rows are recorded under
   their language.
3. Same-language case unchanged: the existing
   `a_gone_file_owes_its_importers_a_re_extract_in_the_same_commit` (898-917)
   keeps passing unmodified.
4. (Optional) `purge_language("typescript")` leaves the `python` importer's
   owed row (`clear_owed_reextracts` is per language).

How to construct a cross-language importer: none occurs naturally (section
1), so seed it directly as rows: `NodeRecord::new("File:tool.py", "File",
"tool.py", "tool.py", "tool.py", "python")` plus an `EdgeRecord`
`File:tool.py -> File:lib.ts`, `IMPORTS`, resolved, through
`apply_diff_linked`. An end-to-end variant would need a fake plugin that emits
a `file`-scope `resolved_module` onto another language's path
(`daemon/test_plugin.rs` or `plugins/sdk/fake/toy.rs`); not recommended - it
tests the store logic through much more machinery.

## 5. Suggested controls (revert code, test must fail)

1. `importers_of` keys every importer by the gone file's language (today's
   code) -> behaviour 1 fails (no `python` row).
2. `apply_file_diff_linked` returns every importer, not only same-language
   -> behaviours 1 and 2 fail on the returned vector.
3. Option B's filter (`AND src.language = t.language`) in place of the
   grouping -> behaviours 1 and 2 fail (no `python` row).
4. Owe only the gone file's language group (skip the others) -> behaviours 1
   and 2 fail.
5. Return the other-language group instead of the same-language one
   (inverted comparison) -> behaviours 1 and 3 fail.

All in one test binary (the core lib tests): one build for the group.

## Cross-file answers relied on

| Question | Call | Answer |
|---|---|---|
| Callers of `file_rows::importers_of` | `find_callers` (symbol_id of `storage::file_rows::importers_of`) | only `Writer::apply_file_diff_linked` |
| Callers of `apply_file_diff_linked` | `find_callers` (its symbol_id) | `watcher::apply::round_trip`, `PluginRegistry::retire_file`, `bulk_index::purge_language`, 4 tests in `index_store.rs` |
| Where owed rows are consumed | `find_definition` `resume_owed_reextracts`, `owe_reextracts`; grep `owe_reextracts\|settle_owed` in `core/src` | owed in `apply_file_diff_linked` and `reextract_affected`; settled by `settle_reextracts` (watcher, plugin's language) and `resume_owed_reextracts` (per language, own plugin) |
| Where a node's language comes from | grep in `watcher/apply.rs`/`storage/write.rs` | the plugin's `node.language`, stored as sent (`apply.rs` 1191, `write.rs` 499) |
| Who emits `IMPORTS` and how targets resolve | `search_code` (import linking), `get_file_outline` `graph/imports.rs` and `typescript/src/project/resolve.rs`, grep `EdgeKind::Imports` / `container_target` / `file_target` in `plugins` | TS file scope (own existence set), python/rust/go container scope (language-keyed lookup) |
| Presence only for claimed paths | `find_callers` `Extractor::file_presence_changed` -> `run::presence_caught`; read `run.rs` 760-819 | gated by `claims` (own extensions) |
