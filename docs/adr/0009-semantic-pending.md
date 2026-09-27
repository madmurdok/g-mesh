# 0009. Semantic pending: say which files a running post-swap semantic pass has not reached

## Status
Accepted (owner review 2026-09-26). Proposed 2026-09-26 (GM-428/S1). The
owner accepted the three open points as proposed: a failed, incomplete or
not-run pass deletes the pending rows and the language falls back to
`absent`; a resumed transitive `find_implementations` page carries the
pending block, and only that block, when its rows' language is pending;
MCP instructions do not change.

## Context
After a workspace reindex swaps a language `L` in (ADR 0008), `L`'s
semantic pass runs against live (`daemon/workspace_reindex.rs` `run_with`,
after `at(Stage::Swapped)`): rust-analyzer 50-105s on g-mesh, go/types,
pyright. Until it finishes, some edges are not final (ADR 0008 section 3,
"What remains, by case"):

1. a new or changed node has structural edges only;
2. an unchanged node whose live edge's target vanished, or whose syntactic
   edge's staged twin differs, has the walk's structural edge in its place;
3. an unchanged call site whose dependency moved (same node rows) keeps its
   old semantic target. No file can be named for this case.

Today the only per-response signal is `mcp/provenance.rs`: `provenance:
{language, semanticTier: "absent"}` on the four edge-walking tools when
`language_state.semanticPassAt` is NULL. The swap sets that column to NULL
(`storage/language_swap.rs` `swap_attached`), so during the pass every
`L`-anchored response already says `absent`. That is wrong in the other
direction: the tier is not absent, it is running, and every unchanged
node's semantic edges are still served. `SemanticTier`'s own doc comment
anticipated this ("splitting this into `Unavailable` and `Pending` ...
can add variants without changing the field's type").

What the code knows today, and where:
- The plan (`language_swap::plan_attached`) knows every node and edge that
  changes, in staging's plan tables (`plan_upsert_nodes`,
  `plan_unchanged_nodes`, `plan_upsert_edges`, `plan_delete_edges`, ...).
  It keeps no file list; `Plan` carries counts and `to_embed` only, and
  nothing outside `language_swap.rs` reads `Plan`.
- Whole-project pass outcomes are recorded in exactly two schema functions:
  `schema::record_language_semantic_pass` (success, `semanticPassAt` set)
  and `schema::record_language_semantic_pass_failure` (failure or "not
  run"), called from `daemon/semantic.rs` (`SemanticPassRun`,
  `record_failure`, `record_not_run`) and `workspace_reindex::run_with`.
- Per-file passes (after an ordinary reparse) end in
  `watcher/apply.rs` `apply_semantic_pass_in` with `file_paths` of one
  entry; an `incomplete` per-file answer is logged, not an error.
- A daemon killed mid-pass restarts with `meta.semanticPassAt` NULL (the
  swap reconciled it), so `daemon/mod.rs`'s `needs_semantic_pass_retry`
  re-runs every owed language's pass at activation
  (`daemon/activation.rs`).

## Decision

### 1. Storage: two tables, written by the swap, cleared by the pass
```sql
-- One row per language whose semantic pass is owed after a workspace
-- reindex swap and has neither completed nor failed since.
CREATE TABLE IF NOT EXISTS semantic_pending (
    language TEXT PRIMARY KEY,
    since    TEXT NOT NULL          -- swap commit time, RFC 3339 UTC
);
-- The files of `language` whose edges that pass has not yet refreshed.
CREATE TABLE IF NOT EXISTS semantic_pending_files (
    language TEXT NOT NULL,
    filePath TEXT NOT NULL,
    PRIMARY KEY (language, filePath)
);
```
Two tables, not one keyed `(language, file)`: the language-level fact must
exist with zero files (a version bump with unchanged code still owes the
pass, section 3's case 3). `since` is written as
`strftime('%Y-%m-%dT%H:%M:%SZ','now')` so the wire needs no conversion.

**Who writes: the swap, in its own transaction.** `plan_attached` gains a
plan table `plan_pending_files (filePath TEXT PRIMARY KEY)`, filled after
the edge plan with the staged `filePath` of:
- every node in `plan_upsert_nodes` (case 1);
- every staged node that is the `fromId` of an edge in `plan_upsert_edges`
  or `plan_delete_edges` and survives (case 2: its edges changed although
  its row did not);

restricted to paths that have a `File` node in staging (a file the walk
still has; a wholly deleted file has no rows any response could name).
`PlanCounts` gains `pending_files` for the log line. `swap_attached`, when
`L` is in `bookkeeping.semantic_pass_languages` (a language with no
semantic tier owes nothing), runs, next to its `language_state` write:
```sql
INSERT OR REPLACE INTO semantic_pending (language, since) VALUES (?1, <now>);
INSERT OR IGNORE INTO semantic_pending_files (language, filePath)
    SELECT ?1, filePath FROM staging.plan_pending_files;
```
Atomic with the swap: a swap that rolls back leaves no rows; a committed
swap always has them. `INSERT OR IGNORE` keeps files a previous swap left
pending (possible only if its pass never finished; section 2 says when).

**Who clears, and when.** Invariant: a row for `L` exists only while `L`'s
whole-project pass is owed and has neither completed nor recorded a
failure.
- *Whole-project pass completes*: `schema::record_language_semantic_pass`
  deletes `L`'s rows from both tables in the same transaction as setting
  `semanticPassAt`. Every completion path (`SemanticPassRun::record_success`
  for the activation retry and `cli::init`, `workspace_reindex::run_with`)
  already goes through it, so none can forget.
- *Whole-project pass fails, is incomplete, or is not run* (plugin asleep):
  `schema::record_language_semantic_pass_failure` deletes the same rows.
  Every failure path (`semantic::record_failure`, `record_not_run`,
  `SemanticPassRun::record_failure`) goes through it. The language then
  falls back to today's `semanticTier: "absent"` (section 3), which is the
  true state: no pass is working on it.
- *Per-file pass completes* (not `incomplete`): `apply_semantic_pass_in`
  deletes `semantic_pending_files` rows whose `filePath` is in its
  `file_paths`, in its own `store.step` after the diff is committed. By
  `filePath` alone: a file belongs to one language. An incomplete per-file
  pass clears nothing. The language row stays: a per-file pass does not
  address case 3.
- *Reset/wipe* (`schema::wipe`, used by version mismatch and `g-mesh
  reindex`): both tables are added to its `DROP TABLE` list.

**Daemon restart.** A pass that was running dies with its plugin; its diff
is one answer at the end, so nothing of it was committed. The rows persist
(they are on disk), `semanticPassAt` is NULL, so activation's
`needs_semantic_pass_retry` re-runs the pass and the success or failure
record clears them as above. Between restart and that retry the rows still
say pending, which is true (owed, not finished); the wording never claims
the pass is *running* (section 3). At startup, next to
`remove_stale_staging`, the daemon deletes rows of languages that are not
semantic-pass-capable among the discovered plugins (a plugin removed since)
or whose `semanticPassAt` is set (a clear whose write failed). Readers also
ignore such rows (section 3), so the cleanup is housekeeping, not
correctness.

### 2. Which files "touch" a response
Every file path the response itself names, plus the anchor's:

| Tool | Files |
|---|---|
| `find_callers` | anchor's file, each row's `filePath`, the `files` tally, `excludedReferences.files` |
| `find_callees` | anchor's file, each row's `filePath`, `excludedReferences.files` |
| `find_references` | anchor's file, each row's `filePath`, the `files` tally |
| `find_implementations` | anchor's file, each row's `filePath` (single-hop page, fresh transitive walk, and resumed transitive page - see below) |

Rule stated by what it guarantees: a response never names a pending file
without saying so. The anchor's file is included even when no row is in it,
because a changed anchor (case 1) is exactly what makes its incoming and
outgoing edges non-final. Files of rows that do *not* exist (a new caller
in a pending file whose receiver call only the semantic tier resolves) can
not be named from the response; the language-level fact (section 3) covers
them, which is why it is always present while `L` is pending.

A resumed transitive `find_implementations` page resolves no anchor and
carries no provenance today. It gets the pending block (only the pending
block, never `absent`) when the language of its rows is pending, so a later
page naming a pending file is not silent.

The intersection is one query per response, only when `L` is pending:
`SELECT filePath FROM semantic_pending_files WHERE language = ?1 AND
filePath IN (<touched>)`, bounded by the response's own file count.

### 3. Wire shape: a third value of `semanticTier`, inside the one block
`provenance` stays one block per response. `SemanticTier` gains `Pending`,
and `Provenance` gains three optional fields, serialized only with it:

```json
"provenance": {
  "language": "rust",
  "semanticTier": "pending",
  "since": "2026-09-26T10:14:03Z",
  "pendingFiles": ["core/src/daemon/semantic.rs", "core/src/storage/schema.rs"]
}
```
- `semanticTier: "pending"`: `L`'s whole-project semantic pass is owed
  after a reindex swapped in at `since`, and has not completed or failed.
  Edges of unchanged call sites may still point where the old dependency
  graph resolved them (case 3). This is the language-level fact; it is
  present on every `L`-anchored response of the four tools while pending,
  with or without files.
- `pendingFiles`: the touched files (section 2) that are pending, anchor's
  file first when it is one, the rest sorted. Absent when empty (never
  `[]`). At most `MAX_PENDING_FILES = 25`.
- `pendingFilesOmitted`: how many more touched pending files there were,
  present only when the cap cut the list. An exact count of the response's
  own files, not an estimate of anything the pass would find.
- No `since`/`pendingFiles` with `absent`; no count of edges, rows, or
  anything the pass would change: provenance's rule (only known facts,
  never an estimate) holds.

`resolve` becomes, in order: no semantic tier declared -> `None`; pass done
-> `None` (so a stale row can never speak); `semantic_pending` row for `L`
-> `Pending` with `since` and the intersected files; otherwise `Absent`. A
failed read of either new table degrades to `Absent`, the existing
"under-warn, never fail the query" rule.

**Byte budget.** `pendingFiles` is variable, unlike today's block. When `L`
is pending (known before the page is bounded: it is a PK lookup on the
anchor's language) the handler subtracts `PENDING_FILES_RESERVE = 1_500`
from the page budget, on the same pattern as `FILE_TALLY_RESERVE` (~60
bytes per path x 25). When `L` is not pending nothing is reserved, so a
healthy response pages exactly as today.

**MCP instructions: no change.** The field names are self-describing and
the state lasts a minute or two after a manifest edit; paying bytes in every
session's instructions for it is the noise provenance's doc rules out. The
docs that do change: `provenance.rs`'s module doc (a section for
`Pending`), `docs/architecture/multi-language-plugins.md`'s provenance
paragraph, and a line in ADR 0008's Consequences pointing here.
`plugin_check` expectations match `semanticTier = "absent"` exactly and
conformance sessions never swap, so they are unaffected.

### 4. `g-mesh status`
`cli::status::IndexStatus` gains `semantic_pending: Vec<(String, String,
usize)>` (language, since, file count), read in `index_status` with the
same table-exists guard `schema::pending_reindexes` uses (status can open
an index this build has not applied its DDL to), filtered to capable
languages whose pass is not done. Rendered after the semantic-pass lines:
```
  semantic pending: rust since 2026-09-26T10:14:03Z - 37 file(s) changed by the reindex have structural edges until its pass finishes
```
With no running daemon (status already knows), the line ends "...; the
next daemon start runs it" instead.

### 5. Schema migration impact
Two `CREATE TABLE IF NOT EXISTS` in `schema.rs`'s DDL, the precedent ADR
0008 set for `pending_reindex` (owner's answer 3): `ensure_current` applies
the DDL to an existing index, so it gains the tables in place.
`CURRENT_SCHEMA_VERSION` stays `"9"` and the indexer version is not touched:
**no wipe, no reindex** on upgrade. A downgraded build ignores the tables
(it never reads them; its own swap does not write them).

### 6. Interaction with GM-431
GM-431 (later in this batch): the swap deletes placeholder nodes the
semantic pass created (ids no walk emits), and a `pending_reindex` row for
a removed plugin never clears. The first makes every unchanged node with a
semantic edge onto such a placeholder lose it and take the walk's edge
(case 2), so until GM-431 lands, `plan_pending_files` will include nearly
every `L` file with such an edge: correct (those edges really are
structural until the pass) but long. The design derives the list from the
plan tables, so GM-431's fix shrinks it with no change here; its tests may
assert the smaller list. The second is unrelated to these tables; the
startup cleanup (section 1) removes pending rows for a removed plugin, which
GM-431 may reuse for `pending_reindex`.

## Tests (S3), each with its control
Controls are code reverts in the verify slice's own worktree; each must make
its test fail.
1. *Plan records changed files* (`language_swap`): live `a` (a node whose
   signature changes), `b` (unchanged node whose edge target is dropped),
   `c` (unchanged), `d` (every node deleted). `plan_pending_files` = `{a,
   b}`. Controls: drop the upsert-nodes arm -> `a` missing; drop the edge
   arms -> `b` missing; drop the `File` restriction -> a placeholder path or
   `d` appears.
2. *Swap writes rows atomically*: after a swap, one `semantic_pending` row
   and `{a, b}`; a swap forced to fail after the rows are written (a trigger
   on the `pending_reindex` delete, the swap's last statement) leaves none.
   Controls: write the rows outside the swap's transaction -> the failing
   swap leaves rows; drop the capability gate -> a non-capable language
   gets rows. (A failure at the `language_state` write, or rows written
   after `commit`, would not tell the arms apart: neither arm reaches the
   write.)
3. *Unchanged tree still owes the language-level fact*: an unchanged
   reindex writes the `semantic_pending` row with zero files. Control: gate
   the row on a non-empty plan -> no row.
4. *Completion clears only its language*: `record_language_semantic_pass`
   (`rust`) removes rust's rows, keeps go's. Controls: remove the delete ->
   rust rows remain; delete without the language filter -> go's go.
5. *Failure and not-run clear*: after
   `answer_first_semantic_pass_incomplete` and after `record_not_run`, no
   rows, and `resolve` returns `Absent`. Control: remove the delete from
   `record_language_semantic_pass_failure` -> rows remain and `resolve`
   says `Pending`.
6. *Per-file pass*: a complete per-file pass over `a` clears `a`, keeps `b`
   and the language row; an incomplete one clears nothing. Controls:
   remove the clear -> `a` remains; clear regardless of `incomplete` -> the
   incomplete test fails.
7. *Restart*: reopen after a swap with no pass -> rows kept; startup
   cleanup removes rows of a non-capable language and of a done language,
   keeps an owed capable one; the retry pass clears it. Control: skip the
   cleanup -> the removed plugin's rows remain.
8. *`resolve` order* (`provenance.rs` units): pending -> `Pending` with
   `since`; done with a stale row -> `None`; non-capable with a row ->
   `None`; no row, not done -> `Absent`. Control: move the pending check
   before the done check -> the stale-row case discloses.
9. *During a pass, per tool* (integration: a real `language_swap` plan and
   swap, then the four handlers before the pass is recorded - the state the
   `Stage::Swapped` hook sees; the hook itself holds an `IndexStore` the
   handlers' `Arc` cannot share, and tests 3, 5 and 7 cover the reindex
   around it): for each of the four tools, a
   query whose response names `a` has `pendingFiles` naming `a`; a query
   touching only `c` has `semanticTier: "pending"`, `since`, and no
   `pendingFiles` key. After the pass: no `provenance` key. Controls: pass
   an empty touched set -> `a` not named; omit the tally/excluded files
   from the touched set -> a `find_callers` case with `a` only in the tally
   fails; don't write rows in the swap -> `semanticTier: "absent"`.
10. *Resumed transitive page*: a resumed `find_implementations` page whose
    rows are in `a` names `a`. Control: keep resumed pages silent -> fails.
11. *Cap and budget*: 30 touched pending files -> 25 listed,
    `pendingFilesOmitted: 5`, response <= `MAX_RESPONSE_BYTES` with rows
    near the budget. Controls: drop the reserve -> over budget; drop the cap
    -> 30 listed. The existing pagination tests are the control that a
    non-pending page is cut exactly as before.
12. *Wire shape*: exact JSON for pending with files, pending without files
    (no `pendingFiles`, no `pendingFilesOmitted`), and `absent` unchanged
    (`{"language":"python","semanticTier":"absent"}`). Control: drop
    `skip_serializing_if` -> `"pendingFiles":[]` appears.
13. *Status*: a pending language renders the line with its count; after the
    pass, no line. Control: drop the line -> fails.
14. *Wipe*: `schema::reset` leaves both tables empty. Control: leave them out
    of `wipe`'s `DROP` list -> rows survive.

## Alternatives considered
- *A sibling `semanticPending` block* (the task's first sketch): a second
  disclosure block per response, and it would sit next to `semanticTier:
  "absent"`, which is false while the pass runs. One block, one more enum
  value, is what `SemanticTier` was shaped for.
- *Keep `absent` and add the file list*: `absent` is what a missing
  rust-analyzer says; a caller would act on "install the engine" when the
  answer is "ask again in a minute".
- *Keep the file list after a failed pass* until the next success: more
  information, but "pending" then names files nothing is working on, with
  no bound on how long; the acceptance criteria rule it out, and `absent`
  already tells the caller the tier did not contribute.
- *In-memory set in the daemon*: lost on restart while the retry still owes
  the pass, and `g-mesh status` (another process) cannot read it.
- *Derive pending files at query time from `edges.source = 'syntactic'`*:
  syntactic is the final state of most edges; it cannot tell "not yet
  refreshed" from "structural by nature" (`provenance.rs`'s own argument
  against per-row tiers).
- *List every pending file of `L` on every response*: unbounded (hundreds
  under GM-431) and mostly unrelated to the question asked.
- *A column on `nodes` or `indexed_files`*: a schema-version bump, which
  wipes every index on upgrade.

## Risks
- Rows that cannot exist are not named: a new call site in a pending file
  that only the semantic tier resolves is absent from rows, so its file is
  not listed. Only the language-level fact covers it; stated in section 2.
- Until GM-431 lands, `pendingFiles` is long on any reindex of a language
  whose pass creates placeholders (rust, go, python).
- A clear whose write fails leaves stale rows; the done-first order in
  `resolve` and the startup cleanup keep them silent.
- Behaviour change: during a post-swap pass, `L`-anchored responses say
  `pending` where they said `absent`. A caller matching on `"absent"`
  exactly sees a new value; `plugin_check` is not such a caller (no swaps).
- One extra PK lookup per response of the four tools when the pass is not
  done; the file intersection runs only while pending.

## Consequences
- A caller can tell "the semantic tier is missing" from "it is catching up
  after a manifest edit", and which of the files it is looking at are not
  final yet.
- `g-mesh status` shows the pending language and how many files.
- New code: the plan table and swap insert (`language_swap.rs`), clears in
  two schema functions and in `apply_semantic_pass_in`, startup cleanup,
  `resolve`'s third branch and the touched-file set in four handlers, the
  status line.
