# GM-499: retry a failed language on the next daemon start

Status: design note (GM-499/S1), for owner review. Builds on
[ADR 0021](../adr/0021-per-language-bulk-outcome.md), whose "Resolved at
review" item 1 left automatic retry to this task.

## 1. Today

- **The failure.** `daemon::bulk_index::run_with_progress`
  (`core/src/daemon/bulk_index.rs:153-300`) walks every discovered language;
  one whose `walk_one_language` fails is purged (`purge_language`, 305-324)
  and recorded `Failed { error }` by `schema::record_language_outcomes`
  (`core/src/storage/schema.rs:958-984`), which rewrites the whole
  `language_outcome` table. The walk still returns `Ok`, so
  `ActivationCtx::walk` (`core/src/daemon/activation.rs:212-297`) records
  `meta.bulkIndexedAt` and logs "left out of the index until `g-mesh reindex`".
- **The next start.** `daemon::run` (`core/src/daemon/mod.rs:239-438`) sees
  `bulk_index_completed`, so `needs_walk = false` (293-295);
  `PluginRegistry::seed_failed_languages` (`registry.rs:682-694`) reloads the
  failed set, and `is_failed_language` (697-699) keeps the watcher, query-time
  reindex and `path_coverage` from routing that language's files. Nothing ever
  walks it again in this or any later process.
- **What heals it.** `g-mesh reindex` (a full walk), or an `indexer_version`
  change (plugin installed, removed or rebuilt; core upgrade), whose `reset`
  wipes the index and `language_outcome` (`wipe`, `schema.rs:1784-1806`).
- **What the agent sees.** `mcp::instructions::coverage_paragraph`
  (`instructions.rs:315-370`): "Not indexed, plugin failed: python (cause) -
  fix the plugin, then run `g-mesh reindex`." A tool call on such a file is
  refused through `mcp::not_indexed::NotIndexed::sentence` with the same advice.

So a transient failure (a plugin timeout under load, a binary caught
mid-rebuild without a fingerprint change) costs that language until a human
notices.

### Callers and references relied on (g-mesh, project `g-mesh`)

| Question | Call | Answer |
|---|---|---|
| Who walks one language | `find_callers walk_one_language` | `bulk_index::run_with_progress`, `workspace_reindex::rebuild` (complete) |
| Who reads outcomes | `find_references language_outcomes` | `registry::seed_failed_languages`, `mcp::GMeshMcpServer::instructions`, `mcp::not_indexed::NotIndexed::from_coverage`, tests (complete) |
| Who writes outcomes | `find_callers record_language_outcomes` | `bulk_index::run_with_progress` only, plus tests (complete) |
| Who honours the failed set | `find_callers is_failed_language` | `path_coverage`, `route_settled_path`, `announce_created`, `file_changed`, `gitignore_changed`, `ensure_fresh` (complete) |
| Who runs the per-language reindex | `find_callers daemon::workspace_reindex::run` | `registry::reindex_workspace`, `registry::run_owed_reindex`, `config_reindex::run`, `config_reindex::resume_owed_reextracts`, tests (complete) |

No g-mesh call failed. Single-file reads (`activation.rs`, `mod.rs`,
`schema.rs` tables) used grep/sed.

## 2. Decisions

### D1. How the failed language is re-walked

- **A. Staging walk and swap, the workspace-reindex machinery** (ADR 0008):
  `workspace_reindex::rebuild` walks the one language into
  `staging-<lang>.db`, links it, plans against live and swaps it in under
  `PluginSupervisor::with_exclusive_access`; `swap` already writes
  `language_state` (`bulkIndexedAt`, fingerprint) and reconciles both meta
  roll-ups. Benefit: live is untouched until one transaction adds the whole
  language, which is exactly ADR 0021's "wholly or not at all"; a failed retry
  needs no purge; edits to other languages are never blocked. Risk: staging is
  linked alone, so an edge *from another language* into the retried one is not
  linked (the same property every workspace reindex already has; no shipped
  plugin pair emits cross-language imports); `get_or_spawn` starts the
  interactive plugin, which the language needs for its semantic pass anyway.
- **B. A live walk of the subset**: `run_with_progress` over a
  `DiscoveredPlugins` filtered to the failed languages. Benefit: cold-start
  code, `link_all` links across languages. Risk: rows commit batch by batch
  into a live, serving index, so queries see a half language mid-walk (ADR
  0002's objection, back); a failure needs the purge; `record_language_outcomes`
  would wipe the other languages' rows; `link_all` reruns over the whole project.
- **C. Mark `pending_reindex` and let `resume_pending` run it.** Benefit: no
  new entry point. Risk: ADR 0021 already rejected it (interactive trigger
  semantics, a `changed_file` that does not exist, `notifyWorkspaceChanged`
  sent for nothing), and a resumed row would bypass the attempt bound.

**Recommendation: A**, through a new entry `workspace_reindex::retry_failed`
that shares `rebuild` and the semantic-pass tail of `run_with` but sends no
`workspaceChanged`, writes no `pending_reindex` row (a killed retry is retried
by the bound below, not by `resume_pending`), records staleness baselines for
the walked files, and swaps **without vectors** (the embedding backfill that
follows in `activate` embeds them; see must-confirm 4).

### D2. The attempt bound and where the count lives

**Bound: 2 retries per failure, i.e. at most 3 walks of a language between
two full walks** (the cold start's own walk plus one on each of the next two
daemon starts). It matches `MAX_OWED_ATTEMPTS = 3` (`schema.rs:1438`, "asked
on at most 3 starts"). New constant `schema::MAX_LANGUAGE_RETRIES = 2`. The
count is taken **before** the walk, so a retry that hangs, crashes or gets the
daemon killed still uses its attempt.

Where the count lives:

- **A. A new table `language_retry (language TEXT PRIMARY KEY, retries
  INTEGER NOT NULL, lastRetryAt TEXT NOT NULL)`, no schema bump.** A missing
  row reads as 0 retries, which is the right default for every existing
  index, so an empty table is harmless (the `pending_reindex`/`semantic_pending`
  precedent ADR 0021 section 5 cites). Benefit: no upgrade re-walk, no clash
  with another batch task's `CURRENT_SCHEMA_VERSION` bump, `LanguageOutcome`
  and its four matchers unchanged. Risk: a second table to clear in step with
  `language_outcome` (one `DELETE` in `record_language_outcomes`'s savepoint,
  one `DROP` in `wipe`).
- **B. A `retries` column on `language_outcome`, schema "14" to "15".**
  Benefit: one row per language says everything. Risk: every index re-walks
  once on upgrade; `CREATE TABLE IF NOT EXISTS` cannot add a column, so it
  must be a bump, and parallel tasks bumping the same constant collide.
- **C. `meta` keys `languageRetries.<lang>`.** Benefit: no DDL. Risk: `meta`
  holds project roll-ups; a per-language key there is a second convention
  nobody else reads, and `reset` handling stays implicit.

**Recommendation: A.**

**Resets** (the count goes back to 0, so the language gets 2 fresh retries
after its next failure):

| Event | Resets? | Why |
|---|---|---|
| A successful retry | yes, row deleted | the language is `indexed` |
| Any full walk (`g-mesh reindex`, `init`, an unwalked start, the all-failed retry) | yes | `record_language_outcomes` clears `language_retry` with `language_outcome` |
| Plugin rebuilt/installed/removed, core upgraded, schema bump | yes | `indexer_version` changes, `reset` wipes both tables, the full walk follows |
| Daemon restart, edits to the language's files, time passing | no | the bound would mean nothing |

A plugin's version is part of its fingerprint, so "the plugin changed" needs
no rule of its own: it is the `indexer_version` row above.

### D3. When a retry runs

- **A. Once per daemon start, in activation, before the watcher's consumer
  starts.** `daemon::run` computes `retry_languages` next to
  `needs_semantic_pass_retry` (`mod.rs:293-300`, before the bind, where GM-543
  also picks its `ColdCause`), and when it is non-empty holds the watcher back
  exactly as it does for the semantic retry (`mod.rs:~395-410`). `activate`
  (`activation.rs:132-208`) runs the retries after the semantic retry and
  before `spawn_watch_consumer`. The phase stays `Structural`: tool calls are
  answered throughout, and a retried language's files are refused as "failed"
  until its swap. Edits queued during the retry route after it, to a language
  no longer in the failed set, so none is lost. Benefit: one walk per start at
  most, bound counted in starts, reuses a proven ordering. Risk: other
  languages' watcher events wait for the retry walk (query-time `ensure_fresh`
  still serves fresh answers for touched files), and a daemon that stays up for
  days keeps a transiently failed language out until it restarts.
- **B. A plus a mid-session timer** (e.g. every 10 minutes while retries
  remain). Benefit: a long-lived daemon heals. Risk: one long session can burn
  both retries during the very outage that caused the failure; a rebuilt plugin
  is not picked up mid-session anyway (`discover()` runs once, and a rebuild
  changes `indexer_version`, which only a restart acts on).
- **C. A plus on demand**, when a tool call is refused for that language.
  Benefit: retries only when someone needs the language. Risk: a refused call
  would start a background walk the agent cannot see finish; the refusal
  wording becomes "maybe in a while".

**Recommendation: A.** No mid-session retry.

The all-failed case (`Phase::Failed`, `bulkIndexedAt` unset) is unchanged:
it already retries the full walk on the next tool call, which resets the count.

### D4. What the instructions say

- **A. The existing failed sentence, split by whether a retry is owed**:
  - retries left: "Not indexed, plugin failed: python (cause) - g-mesh retries
    it on its next start (retry 1 of 2); if it keeps failing, fix the plugin,
    then run `g-mesh reindex`."
  - none left: "Not indexed, plugin failed: python (cause) - retried 2 times
    without success; fix the plugin, then run `g-mesh reindex`."
  Benefit: the agent learns the absence may heal by itself and still gets the
  command; one clause per state. Risk: a few bytes more in the budgeted
  paragraph (`build_within` already drops errors and lists first).
- **B. A separate "Retrying: python" sentence.** Benefit: scannable. Risk: the
  language would appear twice; more bytes.
- **C. No change.** Benefit: none to build. Risk: the advice "fix the plugin"
  is wrong for a transient failure that heals on the next start.

**Recommendation: A.** "Next start" is accurate for D3-A: a session's
`initialize` runs before the activation that retries, so within one session a
retry may succeed after the instructions were rendered; the tools then answer
for the language (the failed set is cleared), which only improves on what the
instructions promised. `NotIndexed::sentence` (PluginFailed) gets the same
clause; `from_coverage` reads the retry count with the error.

## 3. Schema change and migration

```sql
-- Retries of a language recorded `failed` in language_outcome since the last
-- full walk (GM-499). No row = no retry yet. Cleared by every full walk
-- (record_language_outcomes) and dropped by wipe.
CREATE TABLE IF NOT EXISTS language_retry (
    language    TEXT PRIMARY KEY,
    retries     INTEGER NOT NULL,
    lastRetryAt TEXT NOT NULL
);
```

No `CURRENT_SCHEMA_VERSION` bump: `ensure_current`'s DDL creates it on an
existing "14" index, and the missing rows read as 0 retries. Writes:

- `begin_language_retry(conn, language) -> u32`: upsert `retries + 1` before
  the walk; returns the new count.
- On failure: `record_language_retry_failed(conn, language, error)` updates the
  `failed` row's `error` and `recordedAt` (the latest cause is what the agent
  should see).
- On success, **inside the swap transaction** (`SwapBookkeeping` gains
  `retried: bool`): the `language_outcome` row becomes `indexed` and the
  `language_retry` row is deleted, so no crash can leave rows present under a
  `failed` outcome.
- Reads: `languages_owed_a_retry(conn, discovered) -> Vec<String>` (failed,
  discovered, `retries < MAX_LANGUAGE_RETRIES`) and
  `language_retries(conn) -> BTreeMap<String, u32>` for instructions and
  status.

## 4. What status shows (GM-500 composition)

GM-500's languages block reads `language_outcome`; the failed line gains the
retry state from `language_retries`: `python  failed  retry 1 of 2 on next
start  (cause)` or `python  failed  retries used up - run g-mesh reindex`.
`--json` adds `"retries": 1, "maxRetries": 2` to a failed entry. Whichever of
GM-499/GM-500 lands second wires the read; neither depends on the other's
code. GM-543's `ColdCause` is untouched: a retry is never a cold start, and
`retry_languages` is empty whenever `needs_walk` is set.

## 5. Edit map

Change:

| File:lines | Function | Change |
|---|---|---|
| `core/src/storage/schema.rs:482-494` | DDL | add `language_retry` |
| `schema.rs:958-984` | `record_language_outcomes` | `DELETE FROM language_retry` in the savepoint |
| `schema.rs:~1438` | new | `MAX_LANGUAGE_RETRIES`, the four functions of section 3 |
| `schema.rs:1784-1806` | `wipe` | drop `language_retry` |
| `core/src/storage/language_swap.rs:389-396, 423-582` | `SwapBookkeeping`, `swap_attached` | `retried` flag: outcome `indexed`, delete retry row |
| `core/src/daemon/workspace_reindex.rs:192-290, 294-357` | `run_with`, `rebuild` | extract the semantic tail; `rebuild` optionally collects `walked_files` and skips vectors; new `retry_failed` |
| `core/src/daemon/mod.rs:293-300, ~395-425` | `run` | compute `retry_languages`; hold the watcher when non-empty; pass to `ActivationCtx` |
| `core/src/daemon/activation.rs:57-85, 132-208` | `ActivationCtx`, `activate` | new field; `retry_failed_languages` before `spawn_watch_consumer` |
| `core/src/daemon/registry.rs:665-699` | failed set | `clear_failed_language(language)`, after baselines are recorded |
| `core/src/mcp/instructions.rs:99-115, 129-155, 315-370` | `Uncovered::Recorded`, `from_outcomes`, `coverage_paragraph` | failed entries carry `retries`; D4 wording |
| `core/src/mcp/mod.rs:641-690` | `GMeshMcpServer::instructions` | read `language_retries` with the outcomes |
| `core/src/mcp/not_indexed.rs:90-125` | `from_coverage`, `sentence` | same clause |
| `docs/adr/0021-per-language-bulk-outcome.md` | section 2 "Retry", Consequences | amend: bounded automatic retry |

Read, do not change: `bulk_index::walk_one_language` (347),
`watcher/staleness.rs:370 record_walk_baselines` (call it after the swap, with
`walk_started` taken before the plugin spawns), `registry::get_or_spawn`
(954), `PluginSupervisor::with_exclusive_access`, `semantic::indexed_file_count`.

Retry order per language: `begin_language_retry` -> `get_or_spawn` ->
`retry_failed` (walk, link, plan, swap with `retried`) -> baselines ->
`clear_failed_language` -> semantic pass. Any error before the swap ->
`record_language_retry_failed`, log "retry n of 2 failed", next language.

## 6. Tests (acceptance)

Unit, `workspace_reindex`/activation level, test plugins with
`test_plugin::set_bulk_stream(.., exit_code)` and a per-language count of
`--bulk-index` runs (add one to `test_plugin` if none exists):

1. **Fails once, then succeeds, without `reindex`**: alpha and beta; beta's
   stream exits 1 on the cold walk, 0 afterwards. Next start: beta present,
   outcome `indexed`, no retry row, beta's files routed; alpha walked once in
   total and its rows unchanged. Control: drop the `retry_failed_languages`
   call, beta stays absent.
2. **Deterministic failure is bounded**: beta always exits 1; four starts walk
   beta 3 times (cold + 2), the fourth start spawns nothing. Control: drop the
   `retries < MAX` filter, beta is walked 5 times.
3. **Count before the walk**: a retry whose plugin stalls past its timeout
   still counts. Control: increment after the walk.
4. **A full walk resets**: after the bound is used up, `bulk_index::run` then a
   start retries again. Control: remove the `DELETE` from
   `record_language_outcomes`.
5. Instructions/`not_indexed`: both D4 wordings, pinned by text.

Each start-sequence test is repeated 5 times in the tests slice (it spawns
processes).

## 7. Must confirm

1. **Bound of 2 retries.** Today a failed language is walked once and never
   again. Change: it is walked on each of the next 2 daemon starts, then given
   up until a full walk. Example: a Python plugin whose binary is missing costs
   two extra spawn attempts (milliseconds); one that hangs costs its plugin
   timeout on two starts. Consequence: transient failures heal by the second
   restart; a broken plugin costs at most 2 extra walks per full walk.
2. **No mid-session retry.** Today nothing retries. Change: only a daemon start
   retries. Example: a plugin times out at 09:00 in a daemon that stays up all
   day; Python stays absent until that daemon exits (idle timeout) or the user
   runs `g-mesh reindex`. Consequence: the bound is counted in starts and one
   session cannot burn it.
3. **Other languages' watcher events wait for the retry.** Today a walked
   project's watcher consumer starts at `daemon::run`. Change: when a retry is
   owed, it starts after the retry, as it already does for a semantic-pass
   retry. Example: editing `main.rs` while Python is re-walked for 20 s; the
   edit applies after 20 s, though a query on `main.rs` refreshes it at once
   through `ensure_fresh`. Consequence: no edit to the retried language is
   lost; others are briefly delayed.
4. **The retry swap computes no vectors.** Today a workspace reindex embeds
   changed texts before its swap. Change: a retried language swaps in
   structurally and the embedding backfill (which runs right after in
   `activate`) embeds it. Example: a 1,000-file TypeScript retry appears in
   `find_*` answers minutes earlier; `search_code` covers it once the backfill
   ends. Consequence: faster structural recovery; `search_code` lags briefly.

## 8. Owner decisions (2026-10-10)

1. Bound: "2 повтора (Recommended)".
2. Mid-session retry: "Только при старте (Recommended)".
3. Watcher events of other languages: "Да, пусть ждут (Recommended)".
4. Vectors: "Без эмбеддингов, дозаполнить позже (Recommended)".
