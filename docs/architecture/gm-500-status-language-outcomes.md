# GM-500: light and full `g-mesh status`, with per-language outcomes

Design note (GM-500/S1, revised after owner review). Builds on
[ADR 0021](../adr/0021-per-language-bulk-outcome.md) (`language_outcome`).
Owner decisions: "Лёгкий, полный по --full (Recommended)"; "Расширить в этой
задаче (Recommended)"; whole status as `--json`; exit code 0; GC warning to
stderr under `--json`; error chain on one line, `causes` in JSON.

## Today

`g-mesh status` in this repo (4.2.0, daemon live, 499 files, 30,615 nodes):

```
g-mesh status for /Users/…/g-mesh
  project id:      959ade85d9a343b1
  state directory: /Users/…/.g-mesh/projects/959ade85d9a343b1
  daemon core:     running (pid 32528)
  daemon build:    this build
  plugin (go):     active (pid 33008)
  plugin (python):     active (pid 33449)
  plugin (rust):     active (pid 32998)
  last used:       2026-10-10 15:17:21.590 UTC (just now)
  index:           structural index ready; semantic pass running - python (1/4 languages done)
  overall:         ~45% (estimate: walk 40%, semantic pass 20%, embeddings 40% of the work)
  index coverage:  100.0% (499/499 source files)
  dirty files:     0 awaiting reindex
  semantic pass:   running - python (1/4 languages done)
  syntax errors:   none
```

`run` → `collect` (`core/src/cli/status.rs`) builds a `Report` from outside the
daemon; `render` prints it; `gc::warning::maybe_print_stale_projects_warning`
then prints to stdout. Whole command: `real 0.50 user 0.06 sys 0.08` at load
average 41 (mostly waiting, not computing).

## 1. Cost of each line (verified in the code)

| Line | Source | Cost | Mode |
|---|---|---|---|
| project id, state dir | path hash | none | light |
| daemon core | pid file, `kill(0)`, flock probe, one local socket connect | O(1) | light |
| daemon build | stat exe + `plugin::discovered_fingerprint`: SHA-256 over every discovered plugin dir's **bytes** (ignoring `node_modules`, `.git`, …); only when a daemon is up | O(plugin install), here 253 files / 11 MB; independent of the project | light |
| plugins, semantic suspended, last used, phase, progress, overall | small state files, `meta` row | O(1) | light |
| index | `meta.bulkIndexedAt` + phase | O(1) | light |
| semantic pass / pending / pending reindex / leftovers | `language_state`, `semantic_*`, `pending_reindex`; owed languages run `SELECT DISTINCT language FROM nodes WHERE kind='File'` | O(nodes) scan, ~30 ms here | light |
| syntax errors | `SELECT DISTINCT filePath FROM nodes WHERE hasSyntaxErrors = 1` (no index on the flag: full scan) | O(nodes), 31 ms here | light |
| languages (new) | `schema::language_outcomes` (per-language `File` count via `idx_nodes_container`) + `manifest::discover` (reads each `plugin.toml`) | 35 ms here | light |
| front | `candidates::walk`, capped at 5,000 entries / depth 2 | bounded | light |
| **index coverage** | `discover_source_files`: a gitignore-aware walk of the whole project, one `stat` per file, plus every `File` node | O(project files) | **full** |
| **dirty files** | the same walk: disk mtime vs `indexed_files` baseline | O(project files) | **full** (see Must confirm 1) |

Only coverage and dirty files touch the project tree. The SQL timings are
`sqlite3 -readonly` on this repo's index (113 MB) under load average ~21.

## 2. Light and full text

Light (default) prints every line above except coverage and dirty, which
become one line; then the `languages:` block before `syntax errors:`:

```
  index:           ready
  index coverage:  not checked - `g-mesh status --full` walks the project for coverage and dirty files
  semantic pass:   complete
  languages:       3 recorded by the last walk
    go:            plugin absent - 1 file(s) not indexed; install it with `g-mesh plugins install go`
    python:        failed (plugin 4.4.0) - not in the index: failed to start the python plugin: <innermost cause>
    rust:          indexed, 5 files (plugin 4.4.0)
  syntax errors:   none
```

`--full` replaces the `not checked` line with today's two lines, unchanged.
A front still returns right after its `index:` line in both modes.

Rows, sorted by language: `indexed, N files` (N is the live `File` count);
`plugin absent` (`files: None` → `its files are not indexed (not counted)`; a
name outside `languages::CATALOGUE` gets no install clause); `failed` (the
stored chain through `languages::error_on_one_line`, as `init` prints it).

No rows, one line each, never an empty list:

| Index state | Detection | Line |
|---|---|---|
| no `index.db` | `!db_path.exists()` | `languages:       none recorded - no index yet` |
| schema < 13 | no `language_outcome` in `sqlite_master` | `languages:       not recorded - this index predates per-language outcomes (schema 12); the next daemon start rebuilds it` |
| empty, live phase `walking`/`unindexed` | 0 rows | `languages:       not recorded yet - the walk in progress records them when it finishes` |
| empty otherwise | 0 rows | `languages:       none recorded - no walk has finished on this index; run `g-mesh reindex`` |

`schema::ensure_current` resets any schema other than 14 on the next daemon
start, so the pre-13 state lasts only until then. Other readers must not fail
first on such an index: `semantic_leftovers` already probes `sqlite_master`;
the impl slice checks the rest with the pre-13 test.

## 3. Installed plugin version

Source: `manifest::discover(&manifest::default_roots())` →
`PluginManifest.plugin_version` (`plugin.toml` `[plugin] plugin_version`, e.g.
`4.4.0` for rust/python/typescript, `0.5.0` for go in this checkout), the
same precedence `g-mesh plugins` reports. Not the handshake: a plugin's
`Handshake.plugin_version` exists only inside a live daemon, and a running
plugin older than the installed one is already `daemon build: plugin rebuilt`.

| Case | Text | JSON `pluginVersion` |
|---|---|---|
| row indexed/failed, manifest discovered | `(plugin 4.4.0)` | `"4.4.0"` |
| row `plugin_absent`, still no manifest | install clause, no version | `null` |
| row `plugin_absent`, manifest installed since | `plugin absent at the last walk; 4.4.0 installed since - the next daemon start re-walks` | `"4.4.0"` |
| row indexed/failed, manifest removed since | `(plugin no longer installed)` | `null` |
| manifest discovered, no row | `installed 4.4.0 - not in the last walk` | `"4.4.0"` (Must confirm 2) |
| `discover` fails (bad `plugin.toml`) | rows without versions + `plugins:  discovery failed - <error>` | `null`; `pluginDiscoveryError` set |

Today a discovery failure aborts all of `status` (`collect` uses `?`); light
mode reports it instead. Install command: `languages::entry(l)?.install_command()`
(`g-mesh plugins install <l>`), derived, never stored (ADR 0021 §1).

## 4. `--json` (whole status, both modes)

Built by one explicit mapping, `fn to_json(&Report) -> serde_json::Value`
(precedent: `debug_candidates`), **not** `#[derive(Serialize)]`: the JSON
names are then a contract pinned by one test, and renaming a Rust field or
variant cannot change them. With derive instead, `Report`, `CoreState`,
`BuildState`, `PluginReport`, `PluginState`, `SuspendedLanguage`,
`IndexStatus`, `FrontSummary`, `LastUsed`, `ProgressSnapshot` (+3 sub-structs),
`SemanticLeftover`, `LanguageOutcome` would all need it. Keys camelCase (as
the wire protocol); state values snake_case (as the `outcome` column).
`formatVersion` changes only on a removal or rename; adding keys does not.

```json
{
  "formatVersion": 1, "mode": "full",
  "projectRoot": "/p", "projectId": "959ade85d9a343b1", "stateDir": "/…/959ade85d9a343b1",
  "daemon": { "state": "running", "pid": 32528, "build": "current" },
  "plugins": [ { "language": "rust", "state": "active", "pid": 32998 } ],
  "suspended": [],
  "lastUsed": { "timestamp": "2026-10-10 15:17:21.590", "idleSeconds": 4 },
  "index": {
    "phase": "ready", "bulkIndexed": true, "progress": null,
    "semanticPass": { "completed": true, "owed": [], "failures": [],
                      "pending": [], "pendingReindex": [], "leftovers": [] },
    "syntaxErrorFiles": [],
    "coverage": { "discovered": 7, "indexed": 5, "dirty": 0 }
  },
  "front": null,
  "languages": {
    "state": "recorded", "pluginDiscoveryError": null,
    "outcomes": [
      { "language": "go", "outcome": "plugin_absent", "files": 1, "pluginVersion": null,
        "installCommand": "g-mesh plugins install go" },
      { "language": "python", "outcome": "failed", "pluginVersion": "4.4.0",
        "error": "failed to start the python plugin: <innermost cause>",
        "causes": ["failed to start the python plugin", "<innermost cause>"] },
      { "language": "rust", "outcome": "indexed", "files": 5, "pluginVersion": "4.4.0" }
    ]
  }
}
```

- Light: `"mode": "light"`, `"coverage": null`.
- Daemon not running: `daemon {"state": "not_running", "pid": null, "build": null}`,
  `phase`/`progress` `null` (a dead daemon's leftovers are not live, as in
  `render`), plugins left alive appear as `"orphaned"`; everything from
  `index.db` (outcomes, semantic, syntax errors) is still there.
  `daemon.state` ∈ `running | not_accepting | wedged | not_running`;
  `build` ∈ `current | outdated | plugin_changed | unknown | null`.
- Front: `"front": {"projects": 2, "truncated": false}`, `"index": null`,
  `languages.state = "front"`. No index: `index` keeps its keys with
  `bulkIndexed: false`, `languages.state = "no_index"`.
- `languages.state` ∈ `recorded | walk_in_progress | none_recorded |
  predates_outcomes` (+ `schemaVersion`) `| no_index | front`; `outcomes` is
  `[]` unless `recorded`. `files` is `null` for an uncounted absent language.
- Exit 0 whatever the content; the GC warning goes to stderr under `--json`.
- `jq -r '.languages.outcomes[] | select(.outcome=="failed") | .language'`.

## 5. Tests and docs that expect coverage by default

Must change (run `status` without `--full` and assert coverage/dirty):
`core/tests/cli_status.rs` 203-204, 248-249, 268-269 (three tests →
`--full`, plus a light assertion of the `not checked` line);
`core/tests/cli_init.rs` 145-146. Unchanged: `cli_status.rs` 355-356 (front
asserts absence), `state_isolation.rs` 55 and `cli_clean_sweeping.rs` 244
(read `state directory:` only), `plugin_build_staleness.rs` 281 and
`daemon_build_staleness.rs` 357 (daemon build line), `wedged_daemon.rs` 366
(calls `collect`/`render`: gains the mode argument).
`core/src/cli/status/tests.rs`: 9 `IndexStatus { .. }` literals move
`discovered/indexed/dirty` into `coverage: Some(..)`; `index_status` callers
pass full. Docs: `README.md` 776-777 ("how much of the project the index
covers" → `--full`); the module doc of `status.rs`;
`docs/results/gm-425-…` 163 is history, left as is.

New: `one_plugin_binary_missing.rs` (its `init` yields failed python + absent
go + indexed rust) runs `status --json` and `status --full --json`, asserts the
three outcomes, versions and `coverage` present only in full; a pre-13 test
(`schema_version '12'`, no table); unit tests per `LanguageSection` and
version case; a key-set test pinning the JSON shape.

## Edit map

| Change | Where |
|---|---|
| `Status { #[arg(long)] full: bool, #[arg(long)] json: bool }`; dispatch | `core/src/cli/mod.rs` 156-157, 320 |
| `run(full, json)`; warning to stderr under JSON | `status.rs` `run` 233-238 |
| `collect(root, full)`: `discover` once, non-fatal; walk only when `full` | `collect` 241-288 (discover at 270) |
| `IndexStatus.coverage: Option<Coverage{discovered, indexed, dirty}>`; `index_status(.., full)` skips `discover_source_files`/`indexed_file_paths`/`recorded_baselines` when light | `IndexStatus` 134-189, `index_status` 369-442 |
| `LanguageSection` enum + `language_section(db_path, plugins, phase_live)` | new, beside `index_status` |
| `render`: `not checked` line; `languages:` block before syntax errors | `render` 626-751 (coverage 682-694, syntax 738) |
| `to_json(&Report)` | new, `status.rs` (or `status/json.rs`) |

Read, unchanged: `schema::language_outcomes` (`storage/schema.rs` 988-1027),
`languages::{entry, error_on_one_line}`, `cli::language_outcome_lines`
(`cli/mod.rs` 85-111), `PluginManifest.plugin_version` (`daemon/manifest.rs` 353).

g-mesh calls: `find_callers language_outcomes` → bulk_index tests,
`GMeshMcpServer::instructions`, `NotIndexed::from_coverage`,
`one_plugin_binary_missing.rs::Project::outcomes`; `find_references
record_language_outcomes` → only production writer `bulk_index::run_with_progress`;
`find_callers cli::status::collect` / `render` → only `wedged_daemon.rs`;
`find_definition discovered_fingerprint` / `plugins_digest` (byte hashing
cost). `find_references crate::cli::status::collect` returned no symbol (path
form). Grep for CLI wiring, test/doc line expectations, SQL.

## Must confirm

1. **Dirty files is heavy.** Today: `dirty files: N awaiting reindex` on every
   `status`, computed by walking every project file and comparing mtimes.
   Change (recommended): it moves to `--full` with coverage; light prints
   `not checked`. Example: a 200k-file monorepo no longer stats 200k files per
   `status`. Consequence: the light view cannot say "the index is behind
   disk"; the alternative keeps dirty in light and pays the full walk anyway,
   so light would save only the `File`-node load.
2. **Installed plugins with no outcome row.** Today: nothing per language.
   Change (recommended): the block lists outcome rows plus every discovered
   plugin without a row (`installed 4.4.0 - not in the last walk`). Example:
   `g-mesh plugins install go` after the walk, in a project with no `.go`
   files. Consequence: one more row kind; the alternative (rows only) hides
   an installed plugin until the next walk.

## Owner decisions (2026-10-10)

- Two modes: "Лёгкий, полный по --full (Recommended)"; scope widened in
  this task: "Расширить в этой задаче (Recommended)". Owner's words: "а можем
  сделать 2 режима: просто статус того что можно получить легко + языки(
  версии плагинов в том числе ) и полный статус - с "тяжелыми операциями"."
- Dirty files: "Перенести в --full (Recommended)".
- Plugins installed after the last walk: "Да, показывать (Recommended)".
