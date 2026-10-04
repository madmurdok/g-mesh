# 0021. Bulk walk: one language's failure costs only that language, and the index says which

## Status
Accepted 2026-10-04 (GM-329/S1, owner review). Supersedes decision 2 of
[ADR 0002](0002-bulk-walk.md); decision 1 (one one-shot process per
language) stands. Ships together with GM-330 (instructions show coverage):
this ADR is only sound once that one is, see Context.

## Context
`daemon::bulk_index::run_with_progress` loops
`walk_one_language(...)?` over every discovered manifest, so the first
language that fails aborts the whole cold-start index (ADR 0002, decision
2). Release 4.0.0 stops bundling every plugin
([plugin-distribution.md](../architecture/plugin-distribution.md)), so a
project routinely has files of a language whose plugin is not installed,
and one broken plugin (an unbuilt binary, a crash) must not take the other
languages down with it.

ADR 0002 (and GM-316, which added `plugin::missing_plugin_binary_hint` to
that failure) rejected "skip and continue" for one reason: *a partial index
that keeps serving is indistinguishable to an MCP caller from a complete
one* - "this symbol does not exist" and "the plugin that would have found it
never ran" give the same empty answer. That argument is right, and this ADR
does not overrule it. It answers it: GM-330 makes the MCP instructions name
every language the index does **not** cover and why (plugin absent, with a
file count and the install command; plugin failed, with its error). A
partial index that *says* it is partial is distinguishable. Without GM-330
the objection holds in full, which is why the two ship in one release.

Facts from the source that shape the decision (where
`plugin-distribution.md` says otherwise, the source wins):

- Core does not walk the project. Each plugin walks in its own
  `--bulk-index` process; the design doc's "the walk already visits every
  file ... counting is nearly free" is not true of the code. The only core
  walker is `cli::status::discover_source_files` (an `ignore::WalkBuilder`
  configured to match the plugins' walks, pruning `BASELINE_EXCLUDED_DIRS`
  plus the dirs *every* discovered manifest excludes, filtering per file by
  `DiscoveredPlugins::indexing_language`) - it does not hard-code
  TypeScript's exclusions.
- A language's stream is committed batch by batch (`Unit::BulkWalk` is
  `Hold::PerStep`), so a plugin that dies mid-stream leaves part of its
  language in live.
- `meta.bulkIndexedAt` is a roll-up over "present" languages (those with a
  `File` node). A language with some rows but no `language_state.
  bulkIndexedAt` keeps the roll-up unset, i.e. every later start re-walks.
- `meta.indexer_version` digests every discovered plugin's fingerprint
  (`registry::indexer_version`), so installing, removing or rebuilding a
  plugin (inside its manifest dir) wipes and re-walks the index.

## Decision

### 1. A per-language outcome; the walk fails only when every discovered language failed

`bulk_index` produces, per language,
`crate::languages::LanguageOutcome`:

```rust
pub enum LanguageOutcome {
    Indexed { files: usize },
    PluginAbsent { files: Option<usize> },
    Failed { error: String },
}
```

- `Indexed` - a discovered plugin walked it to the end. `files` is its
  `File`-node count. A discovered language with no files is `Indexed { 0 }`.
- `PluginAbsent` - a catalogue language with no discovered manifest and at
  least one file in the project. Not an error. `files` is `None` only under
  the fallback in section 4. A catalogue language with no files gets no
  outcome at all.
- `Failed` - a discovered plugin that could not be used: spawn failure
  (including `missing_plugin_binary_hint`), a mid-stream ingest error, a
  non-zero exit, or its `language_state` write failing. `error` is the
  `{err:#}` chain.

`run`/`run_with_progress` keep `Result<BulkIndexSummary>`; the summary gains
`outcomes: BTreeMap<String, LanguageOutcome>`. They return `Err` when the
discovered set is non-empty and **every** discovered language is `Failed`
(the message lists each language's error), or on a core-side failure
(linking, the outcome write). Zero discovered plugins is `Ok`: an empty
index plus `PluginAbsent` rows.

The install command is **not** in the enum (the design doc's `install:
String`): `languages::entry(language).install_command()` derives it, and a
stored copy could only drift.

### 2. A language is in the index wholly or not at all

When `walk_one_language` fails, every row that language already committed is
removed before the walk moves on: each `filePath` of that language's nodes
goes through `Writer::apply_file_diff_linked(.., FileScope::Gone, ..)`, the
watcher's own path for a deleted file, and those paths leave `walked_files`
(no staleness baseline). This runs before `link_all`, so no cross-file edge
into the failed language exists yet. Consequences that follow and are relied
on: a failed language is not "present", so the `bulkIndexedAt` roll-up and
the semantic pass ignore it; the instructions never describe a half-walked
language as present.

For the rest of that daemon's life the registry does not route a failed
language's files (watcher events, query-time reindex), so single-file updates
cannot re-populate part of it.

**Retry**: none automatic. `meta.bulkIndexedAt` is set (the index is complete
for every language it could index), so the next start does not re-walk. A
failed language is retried by `g-mesh reindex`, or by anything that changes
`indexer_version` (installing, removing or rebuilding a plugin inside its
manifest dir). The all-failed case keeps today's retry-on-next-tool-call
(`Phase::Failed`). See Open question 1.

Rejected: keep partial rows (a half language looks complete - exactly
ADR 0002's objection, now per language); leave `meta.bulkIndexedAt` unset
while any language failed (every daemon start re-walks every language while
one plugin stays broken - an unbounded cost for a deterministic failure);
reuse `pending_reindex`/`workspace_reindex::resume_pending` as the retry (it
goes through the interactive `get_or_spawn` and the workspace-reindex
semantics, built for a different trigger).

### 3. A daemon-side walk counts files of absent languages only

`languages::count_absent_files(root, &DiscoveredPlugins) ->
BTreeMap<&'static str, usize>`:

- **Runs only when `languages::missing(discovered)` is non-empty.** With
  every catalogue plugin installed it costs nothing.
- **Counts absent languages only.** `Indexed { files }` comes from the
  index; counting it again from a core walk could only disagree with what
  the plugin actually indexed. `Failed` carries no count: its error, not a
  number, is what the caller acts on.
- **Classification per file** is `languages::absent_for_path` (the
  catalogue's precedence: a discovered manifest claiming the extension
  wins), then the entry's own `exclude_dirs` via
  `manifest::under_excluded_dir`.
- **Whose `exclude_dirs`**: the absent language's own, from a new
  `CatalogueEntry::exclude_dirs` copied from that plugin's
  `plugin.toml` `[plugin.workspace] exclude_dirs` and pinned to it by the
  same test that pins `extensions`. Rejected: the union or intersection of
  the *discovered* manifests' lists (another language's rules: TypeScript's
  `dist` would hide `dist/app.py`, and nothing would exclude `.venv`, so an
  absent Python is over-counted by every vendored package); no exclusions
  (a `.venv` or `site-packages` that is not gitignored inflates the count by
  orders of magnitude); a hard-coded "common" list (a second, unpinned copy).
  `exclude_dirs` is not a capability, so the catalogue's "no capability
  fields" rule holds: like `extensions`, it says which files the absent
  plugin *would* claim. GM-328's test
  `a_catalogue_entry_holds_only_a_language_and_its_extensions` changes with
  it (Open question 2).
- **Shared walker**: the `WalkBuilder` configuration (`hidden(false)`,
  `parents(false)`, `ignore(false)`, no global/`info/exclude`,
  `require_git(false)`, no symlinks, pruned dir names) moves out of
  `cli/status.rs` into one function both callers use, with the pruned list
  as a parameter. Status prunes `BASELINE_EXCLUDED_DIRS` + dirs every
  discovered manifest excludes; the absent count prunes
  `BASELINE_EXCLUDED_DIRS` + dirs every *absent* entry excludes. Each keeps
  its own per-file filter. One walker, so the two can never disagree about
  which tree "the project" is.
- **Concurrency**: the count runs on a scoped thread started before the
  language loop and joined after it. It touches no database, and in the
  common case it finishes inside the plugins' own walks, off the critical
  path to `Phase::Structural`.

### 4. Cost is measured (GM-329/S5); the fallback is a deadline

Too slow means, on any measured corpus: the median added time to
`Phase::Structural` with the count on versus off (same discovered set) is
above **max(5 % of the walk-off median, 500 ms)** and above the A/A spread;
or, with **zero** plugins discovered (the count is then the whole cold
start), the count alone takes over **2 s** on the largest corpus. 5 % is
about cold-start run-to-run noise and below what a waiting agent notices;
the 500 ms floor keeps small corpora from failing on noise; 2 s is the
absolute wait an agent sees before its first answer when nothing else runs.

Fallback if too slow: the count runs under that deadline. A language seen
before it gets `PluginAbsent { files: None }` (it has files; the count is
unknown); one not seen gets no outcome, and the log says the count was cut.
That is why `files` is an `Option` from the start: GM-330 renders `None`
once, and the fallback needs no schema or API change.

### 5. Persisted in a `language_outcome` table, read through one function

```sql
CREATE TABLE IF NOT EXISTS language_outcome (
    language   TEXT PRIMARY KEY,
    outcome    TEXT NOT NULL CHECK (outcome IN ('indexed', 'plugin_absent', 'failed')),
    files      INTEGER, -- plugin_absent only; NULL = not counted
    error      TEXT,    -- failed only
    recordedAt TEXT NOT NULL
);
```

- **Written** once per walk by `run_with_progress`, after the language loop
  and the count, as one step: `DELETE` every row, then insert one per
  discovered language and per counted absent language. Written in the
  all-failed case too, before `Err` is returned, so a `Phase::Failed`
  session can show why. A walk killed before the write leaves the previous
  rows, and `meta.bulkIndexedAt` unset, so the next start re-walks and
  rewrites them.
- **Incremental updates** (watcher, query-time reindex) do not touch it.
  `indexed` rows store no count: the reader takes it live from the `File`
  nodes, so it never goes stale. A `plugin_absent` count is as of the last
  walk.
- **Workspace reindex** of one language (ADR 0008) leaves the row alone: it
  only runs for a discovered, already indexed language, and a failed
  reindex leaves live unchanged.
- **A plugin installed or removed later** takes a daemon restart
  (`discover()` runs once); `indexer_version` then changes, `reset` wipes
  the index **and this table**, and the re-walk rewrites both.
- **Migration**: a new table plus a `CURRENT_SCHEMA_VERSION` bump to "13",
  and `language_outcome` added to `wipe`. The repo adds tables without a bump
  when an empty one is harmless (`pending_reindex`, `semantic_pending`);
  here an empty table reads as "everything covered", the same silent gap
  the "11" bump avoided for `untyped_calls`. The cost is one re-walk per
  existing index on upgrade to 4.0.0.

Read API for GM-330, in `storage::schema`:

```rust
/// Every recorded outcome, sorted by language. `Indexed { files }` is the
/// live `File`-node count. Empty before any walk has recorded outcomes.
pub fn language_outcomes(conn: &Connection) -> Result<Vec<(String, LanguageOutcome)>>
```

Rejected: columns on `language_state` (needs the bump anyway, and absent
languages would get `language_state` rows that every `semanticPass*`
reader would then have to skip); in memory only on `IndexingStatus` (gone in
the next MCP session, which is when `mcp::instructions` reads it).

### 6. Where the reasoning lives

Per the repo's comment rule, this ADR holds the decision and the GM-316
history. The code holds invariants only: in `run_with_progress`, "a
language is wholly in the index or not at all; the walk fails only when
every discovered language failed (ADR 0021, which answers ADR 0002's
partial-index objection)". The comment names the ADRs, not GM-316 or
GM-330: ticket ids in comments are history, which the rule sends to commits
and ADRs.

## Consequences
- One broken plugin costs its language, not the project; zero installed
  plugins is a valid, empty index.
- A failed language stays out of the index until `g-mesh reindex` or a
  plugin change. The instructions (GM-330) must say so and name the
  command; the CLI `init`/`reindex` print each `Failed`/`PluginAbsent` line.
- A project with an absent catalogue language pays one extra, metadata-only
  tree walk per cold start, measured in S5, bounded by the section 4
  fallback.
- Adding a catalogue language now copies two lists from its `plugin.toml`
  (`extensions`, `exclude_dirs`), both test-pinned.
- Every existing index re-walks once on upgrade (schema "13").

## Resolved at review (owner, 2026-10-04)
1. **No automatic retry** of a `Failed` language in this ADR's scope; it is a
   separate backlog task. Until then, section 2's retry paths stand.
2. **`CatalogueEntry` gains `exclude_dirs`.** It says which files the absent
   plugin would claim, like `extensions`, and is not a capability.
3. **CLI exit code.** `g-mesh init`/`reindex` exit **0** when every discovered
   language indexed, including when some catalogue languages are
   `PluginAbsent` (an archive without a plugin is a valid install);
   **2** when some but not all discovered languages `Failed` (the index is
   written and usable for the rest); **1** when all failed, as today. Each
   `Failed` and `PluginAbsent` language gets one stderr line naming it, and
   for `Failed` the error. The code tells a script to look; stderr says
   which. A non-zero code for a partial failure keeps today's behaviour,
   where any plugin failure already exits non-zero. Showing the
   per-language outcome in `g-mesh status`, machine-readably, is a separate
   backlog task.
4. **Comments link ADRs, not tickets.** The code comment links ADR 0002 and
   this ADR; GM-316 is named here only.

## Measurement (GM-329/S5)

Run 2026-10-04 at f48f0b9, release build, against the rule in section 4.
**Verdict: not too slow. The deadline fallback is not needed.** No corpus
crosses its threshold, and with zero plugins discovered the count costs
0.04 s on the largest corpus, against a 2 s limit.

**Metric.** Wall time (`/usr/bin/time -p`) of `g-mesh reindex`: wipe, bulk
walk and link, which is the work the daemon does before `Phase::Structural`.
Nothing runs after it: every plugin's `semantic_pass`/`semantic_sweep`/
`semantic_prepare` is switched off in a copied manifest, and
`G_MESH_MODEL_DIR` points at a missing directory, so there is no semantic or
embedding pass. Nothing logs the count's own duration, so arm Z is its cost.

**Arms.** Discovery uses `G_MESH_PLUGIN_ROOTS_OVERRIDE` pointed at a private
plugin root: plugin directories made of symlinks to the checkout plus the
edited `plugin.toml`. B = rust, go and typescript discovered with python
absent, count on. C and C2 = the same set with
`G_MESH_BULK_INDEX_NO_ABSENT_COUNT=1`. C2 is the A/A arm, and the A/A
spread is |median C − median C2|. Z and Zoff = zero plugins discovered,
count on or off. A = all four plugins discovered. Each corpus got one
discarded B warm-up, then A once, then 5 interleaved rounds of B, C and C2,
then 5 interleaved rounds of Z and Zoff. Corpora ran one at a time.
`G_MESH_HOME` was private.

**Control, checked on every run** (`language_outcome` read back): every B
run has a `python=plugin_absent:N` row. No C, C2 or Zoff run has a
`plugin_absent` row. Every Z run has one per language present. A has
`python=indexed` and no `plugin_absent` row, so the count did not run.
0 of 135 runs failed this check. gin, ripgrep and excalidraw contain no
Python, so one untracked `gm329_marker.py` was added to each scratch clone.
Without it B and C would have no observable difference.

**Corpora.** Pinned clones: py-requests 6e83187, gin 73726dc, ripgrep
e89fff8, excalidraw 1acf66e (a clone of the local checkout). The fifth
corpus is the g-mesh checkout itself. Walked file count comes from
`git ls-files -co --exclude-standard`, which is what the walker sees because
it keeps `git_ignore` on: py-requests 129, gin 131, ripgrep 223, g-mesh 641,
excalidraw 1261. **Headline = excalidraw.** g-mesh's 151k-file `target/` and
its `node_modules` are gitignored. The walker skips them via `.gitignore`
before the pruned-name list applies, so they did not stress the count. The
Z rows for g-mesh confirm this (rust=278 = tracked `.rs` files).

| corpus | B on (s) | C off (s) | B − C | threshold max(5 %·C, 0.5) | A/A spread | Z on | Z off | Z on − off | verdict |
|---|---|---|---|---|---|---|---|---|---|
| py-requests | 0.21 | 0.20 | +0.01 | 0.50 | 0.03 | 0.04 | 0.04 | +0.00 | ok |
| gin | 0.98 | 0.93 | +0.05 | 0.50 | 0.01 | 0.04 | 0.04 | +0.00 | ok |
| ripgrep | 2.85 | 2.69 | +0.16 | 0.50 | 0.27 | 0.06 | 0.06 | +0.00 | ok |
| **excalidraw** | 19.02 | 21.80 | −2.78 | 1.09 | 1.21 | 0.11 | 0.07 | +0.04 | ok |
| g-mesh | 15.85 | 15.64 | +0.21 | 0.78 | 0.21 | 0.05 | 0.05 | +0.00 | ok |

All values are medians of 5 runs.

**Machine state.** 8 CPUs, shared with other work. The 1-minute load
average was 9.7 at the start, peaked at 41.8 during the excalidraw rounds,
and was 14.8 at the end (5-minute averages 20 to 32). Each corpus's
before/after `uptime` and the per-run load are in the run's `machine.txt`
and `runs.csv`. CPU time accounts for most of the wall time in the bulk-walk
arms: excalidraw user ≈ 18–19 s out of 19–22 s real. So these runs did
compute and were not stalled waiting. The Z arms show user 0.01–0.04 s,
which means the count itself is cheap and is not merely hidden behind a
slow walk. Excalidraw's negative delta is load noise: in round 3, B ran
while the 1-minute load was at 41.8. That corpus's A/A spread, 1.21 s, is
of the same order. A negative delta cannot fail the rule.

**Commands** (scripts are in the session's scratchpad and are not part of
the repo):
`cargo build --release --workspace`, then `go build` for plugins/go and
`npm run build` for plugins/typescript. The binaries were copied to a private
`bin/` so that any leftover process could be identified and killed; none
was left. Each run was
`cd <corpus> && G_MESH_HOME=<private> G_MESH_MODEL_DIR=/nonexistent
G_MESH_PLUGIN_ROOTS_OVERRIDE=<root> [G_MESH_BULK_INDEX_NO_ABSENT_COUNT=1]
/usr/bin/time -p g-mesh reindex`.
