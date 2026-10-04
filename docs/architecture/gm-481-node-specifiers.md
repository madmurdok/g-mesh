# GM-481: TypeScript declares `node:` as not-a-symbol

The TypeScript plugin's `[plugin.non_symbol_queries]` gains
`starts_with = ["@", "node:"]`, in `plugins/typescript/plugin.toml` and in
the installed manifest `scripts/bundle-plugin.sh` writes. A Node.js built-in
specifier (`node:fs`, `node:path`) is never a TypeScript/JavaScript symbol:
no identifier contains `:`. Before this, a slash-free `node:` query matched no
TypeScript shape, so find_definition offered semantic neighbours for it and
search_code returned its rows with no verdict or with the generic
below-floor one. Mechanism and rules: ADR 0018 and
`gm-475-specifier-shapes-in-manifests.md`.

No core change: the table is read by `PluginRegistry::query_shapes` ->
`QueryShapes::from_manifests`; `QueryShapes::refuses` is called by
`similarity::refuses_row` (from `below_floor`) and `is_specifier_page` (from
`verdict`), and `refused_by_all` by `is_specifier_page`.

## Measurement

Same harness as GM-475 §7 (`drive.py` through the real MCP shim, one release
build of this branch shared by both arms, fresh `G_MESH_HOME` per arm).
`before` is the TypeScript manifest at the merge-base, `after` this branch's;
the other three plugins are identical in both. Query sets: GM-475's
find_definition and search_code sets for each corpus, plus 12 extra `node:`
queries (`node:util`, `node:buffer`, `node:`, `node:fs.readFileSync`, ...),
8 near misses (`node`, `nodes`, `NodePath`, `nodeFs`, `NODE_ENV`, ...) and 3
prose queries containing `node:`.

| corpus / plugin root | tool | queries | `node:` queries | diffs on `node:` | diffs elsewhere |
|---|---|---|---|---|---|
| ts-corpus / TS only | find_definition | 421 | 24 | 5 | 0 |
| ts-corpus / TS only | search_code | 429 | 25 | 22 | 0 |
| ts-corpus / all four | find_definition | 421 | 24 | 5 | 0 |
| ts-corpus / all four | search_code | 429 | 25 | 22 | 0 |
| g-mesh (mixed) / all four | find_definition | 1346 | 24 | 2 | 13 (index noise, below) |
| g-mesh (mixed) / all four | search_code | 1716 | 25 | 0 | 0 |

What changed, all on `node:` queries:

- **find_definition, ts-corpus**: 5 queries go from `semanticNeighbours`
  (1-3 junk candidates) to "no symbol named ... found". These are the
  specifiers the corpus does not import (`node:buffer`, `node:process`,
  `node:`, `node:fs.readFileSync`, `node:path.join`). Imported ones
  (`node:fs`, `node:path`, `node:child_process`, ...) were already answered by
  the import-record rung ("names something this project imports") and do not
  change; specifiers with `/` (`node:fs/promises`) were already refused.
- **search_code, ts-corpus**: 22 pages gain `noMatch: queryIsAPathOrPackage`;
  15 had `belowSimilarityFloor`, 7 had no verdict at all (junk rows presented
  as matches). Rows stay on the page, as for every verdict.
- **g-mesh corpus**: 2 find_definition answers lose their TypeScript
  neighbour (3 candidates -> 2). Nothing else changes for `node:` there: the
  Rust, Go and Python plugins do not declare `node:`, so their candidates
  survive and a mixed page is not a specifier page (by design, ADR 0018: a
  plugin only refuses its own candidates).
- Near misses (`node`, `NodePath`, `nodeFs`, ...) and the prose queries
  (`node: built-in modules`: whitespace makes it prose) never change.

The 13 non-`node:` diffs on the g-mesh corpus are all `nameAmbiguous`
answers (the structural rung, which the shapes never reach) whose candidate
lists differ only in Rust rows or ids: the two arms' indexes differ by 3,046
Rust nodes (18,262 vs 15,216; Go, Python and TypeScript counts identical),
measured at load average ~90-110 with other worktrees building. The TS
manifest does not touch Rust indexing, so these are run-to-run Rust index
noise, not this change.

Timing (`/usr/bin/time -p`, per arm and tool): ts-corpus 112-139 s real,
g-mesh corpus 222-446 s real, at `user` under 1 s each (the harness waits on
the daemon). `uptime` load averages 85.7 at start, 63.1 at end.
