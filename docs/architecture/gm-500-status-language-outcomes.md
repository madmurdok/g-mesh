# GM-500: per-language cold-start outcomes in `g-mesh status`

Design note (GM-500/S1). Builds on [ADR 0021](../adr/0021-per-language-bulk-outcome.md)
(the `language_outcome` table, open question 3 names this task).

## Today

`g-mesh status` in this repo (4.2.0 binary, daemon live, walk done):

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

How it is built (`core/src/cli/status.rs`): `run` → `collect` gathers a
`Report` entirely from outside the daemon (pid files, socket, `index.phase`,
`index.progress`, and `index.db` opened read-write without CREATE in
`index_status`); `render` turns it into text; `run` prints it, then
`gc::warning::maybe_print_stale_projects_warning` prints to **stdout**.
Nothing reads `language_outcome`. A failed or absent language is visible
only in `init`/`reindex` stderr (`cli::language_outcome_lines`) and in the
MCP instructions. The only existing `--json` precedent is the hidden
`debug-candidates` (`serde_json::json!` + `to_string_pretty`).

## Proposed text

A `languages:` block right after `dirty files:` (it explains coverage), one
row per `language_outcome` row, sorted by language (the reader's order):

```
  index coverage:  71.4% (5/7 source files)
  dirty files:     0 awaiting reindex
  languages:       3 recorded by the last walk
    go:            plugin absent - 1 file(s) not indexed; install it with `g-mesh plugins install go`
    python:        failed - not in the index: failed to start the python plugin: <innermost cause>
    rust:          indexed (5 files)
  syntax errors:   none
```

- `indexed (N files)`: N is the live `File`-node count (`schema::language_outcomes`).
- `plugin absent`: `files: None` renders `its files are not indexed (not counted)`;
  a language missing from `languages::CATALOGUE` gets the row without the install clause.
- `failed`: the stored chain through `languages::error_on_one_line` (same
  text as init's stderr line).
- A front returns before the index lines today and keeps doing so: no
  index, no `languages:` block.

No-rows states (acceptance criterion 3), one line each, never an empty list:

| Index state | Detection | Line |
|---|---|---|
| no `index.db` | `!db_path.exists()` | `languages:       none recorded - no index yet` |
| schema < 13 | no `language_outcome` table in `sqlite_master` | `languages:       not recorded - this index predates per-language outcomes (schema 12); the next daemon start rebuilds it` |
| table empty, `phase` live and `walking`/`unindexed` | rows = 0 | `languages:       not recorded yet - the walk in progress records them when it finishes` |
| table empty otherwise | rows = 0 | `languages:       none recorded - no walk has finished on this index; run `g-mesh reindex`` |

Schema 14 is current and `schema::ensure_current` resets any other version
on the next daemon start, so the "predates" state lasts only until then;
the message says so instead of telling the user to act.

## `--json`

`g-mesh status --json` prints one object (`serde_json::to_string_pretty`):

```json
{
  "formatVersion": 1,
  "projectRoot": "/path/to/project",
  "languages": {
    "state": "recorded",
    "outcomes": [
      { "language": "go", "outcome": "plugin_absent", "files": 1,
        "installCommand": "g-mesh plugins install go" },
      { "language": "python", "outcome": "failed",
        "error": "failed to start the python plugin: <innermost cause>",
        "causes": ["failed to start the python plugin", "<innermost cause>"] },
      { "language": "rust", "outcome": "indexed", "files": 5 }
    ]
  }
}
```

- `state`: `recorded` | `none_recorded` (table empty) | `walk_in_progress`
  (table empty under a live walk) | `predates_outcomes` (adds `"schemaVersion": "12"`)
  | `no_index` | `front`. `outcomes` is `[]` unless `recorded`.
- `outcome` strings are the table's own CHECK values. `files` is `null` for
  an uncounted absent language; `installCommand` is `null` for a
  non-catalogue name. `error` is one line, `causes` the stored lines.
- Script use: `g-mesh status --json | jq -r '.languages.outcomes[] | select(.outcome=="failed") | .language'`.
- Exit code 0 whatever the outcomes (status reports; it does not judge).
- The GC idle warning goes to stderr under `--json`, so stdout parses.

**Scope - recommended: languages only, in an extensible object.** The
object carries `formatVersion` and `languages` now; other sections can be
added later as new keys without breaking a script. Under `--json`,
`collect` is skipped: no `discover_source_files` walk, no pid/socket probes;
it opens `index.db` and reads the table, so it stays fast on a big project.

- Benefit: the contract is one table's shape, already pinned by ADR 0021;
  no serde on `Report`, `CoreState`, `BuildState`, `ProgressSnapshot`,
  `LastUsed`, `IndexStatus`, whose every field would become public API.
- Risk: `--json` reads as "all of status"; a user expecting `daemon core` in
  it finds only `languages`. Mitigated by the help text and `formatVersion`.
- Alternative (whole status as JSON): one complete machine form, but ~6
  types gain `Serialize`, every later status change becomes a breaking-change
  question, and the coverage walk runs on every scripted call.

## Where the install command comes from

`languages::entry(language)?.install_command()` →
`g-mesh plugins install <language>` (`core/src/languages.rs`,
`CatalogueEntry::install_command`). Not stored in the table (ADR 0021 §1:
a stored copy could only drift); status derives it exactly as
`cli::language_outcome_lines` does.

## Daemon not running

Status reads `index.db` directly, never asks the daemon (module doc). The
table is persisted by `bulk_index::run_with_progress` →
`schema::record_language_outcomes` at the end of each walk, so the rows are
there with no daemon. During a re-walk after a wipe the table is empty
(`wipe` drops it), which is the `walk_in_progress` / `none_recorded` row.
SQLite WAL lets status read while a live daemon writes.

## Edit map

| Change | Where |
|---|---|
| `Command::Status` → `Status { #[arg(long)] json: bool }`, dispatch `status::run(json)` | `core/src/cli/mod.rs` 156-157, 320 |
| `run(json)`: text path as today; JSON path reads only the languages section; warning to stderr under JSON | `core/src/cli/status.rs` `run` 233-238 |
| New `LanguageSection` enum (`NoIndex`, `Front`, `PredatesOutcomes { schema }`, `WalkInProgress`, `NoneRecorded`, `Recorded(Vec<(String, LanguageOutcome)>)`) and `pub fn language_section(db_path, phase_live) -> Result<LanguageSection>` (own read-only-intent connection, same flags as `index_status`; table probe via `sqlite_master`, schema via `meta.schema_version`) | `core/src/cli/status.rs`, beside `index_status` 369-442 |
| `Report.languages: LanguageSection`, filled in `collect` (front → `Front`) | `Report` 192-221, `collect` 241-288 |
| `render`: `languages:` block after `dirty files:` | `render` 626-751, after the dirty-files `if` (~696-700) |
| JSON shape: `fn languages_json(&LanguageSection, &Path) -> serde_json::Value` | `core/src/cli/status.rs` |
| `index_status` unchanged (keeps its unit-test callers) | |

Read, not changed: `schema::language_outcomes` (`core/src/storage/schema.rs`
988-1027), `languages::{entry, error_on_one_line, LanguageOutcome}`,
`cli::language_outcome_lines` (`core/src/cli/mod.rs` 85-111, wording to match).

Tests: unit tests in `core/src/cli/status/tests.rs` for each
`LanguageSection` render and JSON; the real-index test in
`core/tests/one_plugin_binary_missing.rs` (its `init` already yields failed
python + absent go + indexed rust) runs `g-mesh status --json` and asserts
the three objects; a pre-13 test builds a db with `schema_version '12'` and
no table. Risk for impl: other `index_status` readers may fail on a pre-13
db before the languages line is reached; the pre-13 test exercises the full
`status` command, not `language_section` alone.

g-mesh calls relied on: `find_callers language_outcomes` → bulk_index tests,
`mcp::GMeshMcpServer::instructions`, `NotIndexed::from_coverage`,
`one_plugin_binary_missing.rs::Project::outcomes` (+1 non-call ref in
`daemon/registry.rs`); `find_references record_language_outcomes` → only
writer in production is `bulk_index::run_with_progress`;
`find_callers cli::status::collect` / `cli::status::render` → only
`core/tests/wedged_daemon.rs` outside `run`. `find_references
crate::cli::status::collect` returned "no symbol" (path form); the
`cli::status::collect` form resolved. Grep used for `Command::Status`,
`--json` precedent and the schema DDL (single known files).

## Must confirm

1. **`--json` scope.** Today: no machine-readable status at all. Change:
   `--json` emits `{formatVersion, projectRoot, languages}` only (recommended),
   or the whole report. Example: a CI script runs `jq '.languages.outcomes[]'`
   either way; with whole-status it could also read `.core.state`.
   Consequence: languages-only is a small contract and fast; whole-status
   makes ~6 internal types public API and runs the coverage walk each call.
2. **Exit code.** Today: status always exits 0; `init`/`reindex` exit 2 on a
   partial failure. Change (recommended): status stays 0 with `--json`, the
   script reads `outcome`. Alternative: exit 2 when any row is `failed`.
   Example: `g-mesh status --json || alert` would fire under the alternative
   only. Consequence: exit 2 lets a script skip jq, but makes "status worked"
   and "a plugin is broken" share one signal.
3. **Indexed rows in text.** Today: nothing per language. Change: every row,
   including `rust: indexed (5 files)` (the acceptance criterion says every
   language). Example: a 4-language project gets 5 more lines. Consequence:
   longer output on healthy projects; alternative is a one-line
   `languages: all 4 indexed` when none failed or is absent.
4. **Failed error length.** Today: init prints the whole chain on one line.
   Change (recommended): same in status text; JSON adds `causes`.
   Alternative: text shows only the innermost cause. Consequence: the whole
   chain can be long (a binary path, a spawn error), but drops nothing.
