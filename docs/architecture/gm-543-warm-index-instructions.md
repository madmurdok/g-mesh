# GM-543: session instructions over an index already on disk

Status: design note (S1). No code changed yet.

## 1. Today's behaviour

The task's stated cause (a new daemon "only looks at the store on its lazy
bootstrap") does not match the 4.4.0 code. The daemon reads the index
synchronously, before it binds its socket:

- `daemon::run` (`core/src/daemon/mod.rs:239-330`):
  1. `connection::open(root)` (`:280`);
  2. `schema::ensure_current(&conn, &registry::indexer_version(&discovered))`
     (`:281`) - returns `true` and **wipes** the index (`schema::reset`) when
     `meta.schema_version != CURRENT_SCHEMA_VERSION` (`"14"`) or
     `meta.indexer_version != CURRENT_INDEXER_VERSION + "+" + plugins_digest`
     (`core/src/storage/schema.rs:699-746`, `core/src/daemon/registry.rs:283`);
     a fresh file has no `meta` row and also returns `true`;
  3. `needs_bulk_index = !schema::bulk_index_completed(&conn)` (`:294`),
     i.e. `meta.bulkIndexedAt IS NULL`;
  4. `IndexingStatus::unindexed()` if owed, else `IndexingStatus::structural()`
     (`:330`), before the accept loop.
- `GMeshMcpServer::instructions` (`core/src/mcp/mod.rs:641-660`) renders
  `instructions::cold_start` (`core/src/mcp/instructions.rs:65`) for
  `Phase::Unindexed | Phase::Walking`, with no store access; any other phase
  reads the store and renders the warm `build`.

So a daemon over a **current, complete** index already starts at `Structural`
and renders the warm text (pinned only indirectly, by
`core/tests/lazy_activation.rs::an_existing_index_is_not_rewalked`, which
checks no re-walk but never reads the instructions).

"Not indexed yet" over an index the user believes current comes from one of:

| # | Path | Index after startup | Text today | Accurate? |
|---|------|---------------------|------------|-----------|
| a | Generation mismatch: g-mesh upgraded, a plugin rebuilt, or `g-mesh reindex` run by a different binary/plugin root than the daemon's (`manifest::default_roots` is exe- and env-relative) | wiped by `ensure_current` | "Not indexed yet" | true, but unexplained |
| b | `bulkIndexedAt` NULL with rows present: a walk killed part way, or `record_bulk_index`'s reconciler clearing it (`schema.rs:946`) | partial | "Not indexed yet" | misleading |
| c | Front lists the project "(indexed)" | n/a | front says indexed, daemon then says not | contradiction |

Live reproduction (this session, 2026-10-10): the front at
`ClaudeProjects` listed `g-mesh (indexed)`; `select_project g-mesh` returned
"Index root: .../g-mesh. Not indexed yet - ...". The project's
`daemon.log` shows the daemon restarted on a new build (`daemon.build`
18:16) and logged `index (re)initialized - a full reindex is needed`: path
(a), seen through (c). The front's marker is
`candidates::has_completed_index` (`core/src/daemon/candidates.rs:178-201`),
which reads `bulkIndexedAt` only and never the generation, so it called a
doomed index "indexed". The 4.2.0 report ("after `g-mesh reindex`, a fresh
daemon on `select_project`") fits the same pair; its "answered at once" is
consistent with a small project re-walking quickly.

## 2. How "current" is decided (cheaply)

All of it is one row: `meta(schema_version, indexer_version, bulkIndexedAt)`
in `<state dir>/index.db`. "Current" = `schema_version == CURRENT_SCHEMA_VERSION`
and `indexer_version == registry::indexer_version(&discovered)` and
`bulkIndexedAt IS NOT NULL`. The daemon already computes all three before the
bind, so any cause it needs is known before `initialize` and costs no extra
I/O at `initialize`. An upgrade invalidation is `ensure_current` returning
`true` on a row that existed (old `schema_version` or `indexer_version`); a
fresh index is `true` on no row.

## 3. Options

**A. Record the cold cause at startup; render a line per cause (recommended).**
`daemon::run` reads the stored generation (one `SELECT schema_version,
indexer_version FROM meta`, `Option`) before `ensure_current`, then derives
`ColdCause::{Fresh, Discarded, Incomplete}` and starts at
`IndexingStatus::unindexed_because(cause)`. `instructions()` reads the cause
from memory (lock-free, like `phase()`), passes it to `cold_start`.
Benefit: no I/O at `initialize`, no new phase, no change to `ensure_current`'s
27 callers; the agent learns why it waits. Risk: the two new lines are up to
47 bytes longer than today's, so `cold_start`'s body budget
(`INSTRUCTIONS_BYTE_CEILING - fallback.len() - 2`) shrinks and the trim ladder
may step earlier on worst-case renderings; the ceiling itself holds.

**B. Cheap metadata check inside `instructions()`.** Open a second read-only
connection to `index.db` (or `stat` it) at `initialize` when `Unindexed`.
Benefit: works even if the open ever moves after the bind. Risk: I/O on the
handshake path the ADR 0022 / D12 rule keeps I/O-free; a stat cannot tell a
wiped or partial file from a current one (the wipe keeps the file); a second
connection during a `Walking` batch commit can wait on the WAL. Rejected.

**C. A third phase "index present, not yet opened".** Benefit: matches the
task's wording. Risk: the state does not exist - the index is opened before
the bind, so the phase would never be observed; it adds an arm to every
`Phase` match (`wait_for`, `request_activation`, phase file, status CLI).
Rejected.

**A + front fix (part of the recommendation, see must-confirm 2).**
`completed_index_in` also compares `schema_version` with
`CURRENT_SCHEMA_VERSION` and the `indexer_version` prefix with
`CURRENT_INDEXER_VERSION + "+"`. Catches a g-mesh upgrade without plugin
discovery (the front never discovers plugins, D10/D11); a plugin-only
rebuild still shows "(indexed)", and the daemon's `Discarded` line then
explains it.

## 4. What the agent sees

The warm rendering is unchanged (`instructions::build`, starts with P1
"Structural code-graph queries over this project's index."; no "Index root"
line). Cold lines, each followed by the unchanged body and with the
`Index root: <root>. ` prefix when it fits (`cold_start_line`):

| Startup state | Phase | Line (bytes without root) |
|---|---|---|
| Current, complete index | `Structural` | warm `build`, no cold line |
| Index present, discarded by `ensure_current` | `Unindexed`, `Discarded` | `Index discarded (built by an earlier g-mesh or plugin build) - the first tool call rebuilds it (structural first; semantic search after) and waits for it - slow, not wrong; do not abandon it for grep.` (200) |
| Current generation, `bulkIndexedAt` NULL, rows present | `Unindexed`, `Incomplete` | `Index incomplete (an earlier walk stopped part way) - the first tool call finishes it (structural first; semantic search after) and waits for it - slow, not wrong; do not abandon it for grep.` (191) |
| No index (no `meta` row) | `Unindexed`, `Fresh` | `Not indexed yet - the first tool call builds it (structural first; semantic search after) and waits for it - slow, not wrong; do not abandon it for grep.` (153, unchanged) |
| Walk running | `Walking` | `Being built now - ...` (unchanged; cause not shown) |

`Incomplete` vs `Fresh` is decided by "a `meta` row existed and was not
reset" (no row count query). A failed walk keeps today's `Failed` rendering.

## 5. Tests and controls

- `core/tests/lazy_activation.rs`, next to `an_existing_index_is_not_rewalked`:
  1. `an_existing_index_renders_the_warm_instructions`: `g-mesh init`, connect,
     read `client.peer_info().instructions`; assert it starts with P1 and
     contains neither "Not indexed yet" nor "Index root:".
  2. `a_discarded_index_says_so`: `g-mesh init`, then
     `UPDATE meta SET indexer_version = 'stale'`, connect; assert the
     `Discarded` line.
- `core/src/mcp/instructions/tests.rs`: `cold_start` per cause, each within
  `INSTRUCTIONS_BYTE_CEILING` with the worst-case coverage already used there.
- Controls (2): (i) `daemon::run:330` always `IndexingStatus::unindexed()` -
  test 1 fails; (ii) `cold_start` ignores the cause (always the `Fresh` line) -
  test 2 fails. The task's literal control ("always `cold_start` for
  `Unindexed`") cannot fail test 1, because that daemon is not `Unindexed`.

## 6. Edit map

| Change | File:lines |
|---|---|
| Read stored generation before `ensure_current`; derive cause; `unindexed_because` | `core/src/daemon/mod.rs:277-330` (`run`) |
| New `pub fn stored_generation(conn) -> Result<Option<(String, String)>>` (schema-safe: `schema_version` first, as `ensure_current` does) | `core/src/storage/schema.rs` near `ensure_current` `:699` |
| `ColdCause` enum; `Inner.cold_cause` (immutable); `unindexed_because`; `cold_cause()` getter; `unindexed()` = `Fresh` | `core/src/daemon/indexing_status.rs:116-150` (`Inner`), `:223-249` |
| `cold_start(root, walking, cause, coverage)`; `cold_start_line`/`_fallback` per cause | `core/src/mcp/instructions.rs:34-74` |
| Pass `self.indexing.cold_cause()` | `core/src/mcp/mod.rs:641-660` (`instructions`) |
| Front: generation-aware "(indexed)" (if confirmed) | `core/src/daemon/candidates.rs:190-201` (`completed_index_in`) |
| ADR 0022 row 10 / D12 wording: name the three cold lines | `docs/adr/0022-instructions-coverage-states.md:57`, `docs/architecture/lazy-indexing.md` D12 (`:719`) |

Read for context: `schema::ensure_current`/`reset` (`schema.rs:699-760`),
`bulk_index_completed` (`:769`), `record_bulk_index` (`:946`),
`registry::indexer_version` (`registry.rs:283`), `cli::reindex::reindex`
(`core/src/cli/reindex.rs:84-144`, stamps the same generation),
`Front::new` (`core/src/mcp/front.rs:76-90`),
`instructions/tests.rs::server_over` (`:1477`).

Callers/references relied on (g-mesh, project `g-mesh`):
- `find_callers IndexingStatus::unindexed` -> only `daemon::run` in production
  (+5 tests; 1 unlinked usage in `mcp/session_hints.rs` test, confirmed by grep).
- `find_callers IndexingStatus::structural` -> `daemon::run`, `cli::init`,
  `cli::reindex`, tests.
- `find_callers instructions::cold_start` -> only `GMeshMcpServer::instructions`.
- `find_callers GMeshMcpServer::instructions` -> `get_info` + 5 tests in
  `instructions/tests.rs` (signature unchanged, so no edits there).
- `find_callers schema::ensure_current` (`answer: files`) -> 27 sites in 20
  files: why A leaves its signature alone.
- `find_definition` of `Phase`, `cli::reindex::reindex`,
  `registry::indexer_version`, `manifest::default_roots`.
- grep: the front's guidance path (`shim/router.rs:544-596`, `switch` copies the
  target daemon's `initialize` `instructions`), `has_completed_index` users.

## 7. Must confirm

1. **The fix is the cause line, not a phase change.** Today a daemon over a
   current, complete index already renders the warm text (it reads
   `bulkIndexedAt` before the bind). The change: keep that, and add the
   `Discarded` and `Incomplete` lines. Example: after upgrading g-mesh, the
   agent reads "Index discarded (built by an earlier g-mesh or plugin build)
   - the first tool call rebuilds it ..." instead of "Not indexed yet".
   Consequence: AC1 is pinned by a test rather than fixed by new logic; if
   you expected the 4.2.0 symptom to reappear on 4.4.0 with a truly current
   index, say so and S2 starts with a reproduction instead.
2. **Make the front's "(indexed)" generation-aware.** Today the front marks a
   project indexed when `bulkIndexedAt` is set, even if the daemon will wipe
   it on start (seen live today for `g-mesh`). Change: also require the
   current schema version and the core half of `indexer_version`. Example:
   after a g-mesh upgrade the front lists `g-mesh` without "(indexed)".
   Consequence: one more file and test; a plugin-only rebuild still shows
   "(indexed)" because the front does no plugin discovery.
3. **Include `Incomplete`.** Today a walk killed part way renders "Not indexed
   yet" though rows exist. Change: a third line, "Index incomplete (an
   earlier walk stopped part way) - the first tool call finishes it ...".
   Consequence: one more variant and test; dropping it keeps the old line for
   that case.
4. **Longer cold lines shrink the body budget.** Today `cold_start` reserves
   153 bytes for its line; `Discarded` needs 200. Example: a rendering with
   many failed languages may drop their error text (ladder step 2) 47 bytes
   sooner, only during a cold start. Consequence: the 1900-byte ceiling
   holds; alternatively shorten the lines (e.g. drop "(structural first;
   semantic search after)" from the two new ones).
5. **Controls differ from the task's.** The task's control ("always
   `cold_start` for `Unindexed`") cannot fail a warm-index test, because that
   daemon starts at `Structural`. Change: control (i) forces `Unindexed` at
   startup, control (ii) ignores the cause. Consequence: each test has a
   control that tells the arms apart.

## 8. Owner decisions (2026-10-10)

1. "Строка причины + тесты (Recommended)" - option A, the cause line plus
   tests; controls (i) and (ii) from section 5 replace the task's control.
2. "Да, проверять версию (Recommended)" - the front's "(indexed)" also
   checks `schema_version` and the core half of `indexer_version`.
3. "Да, добавить (Recommended)" - the `Incomplete` line is in.
4. "Сократить новые строки (Recommended)" - the `Discarded` and `Incomplete`
   lines drop "(structural first; semantic search after)", so they stay
   about as long as today's line.
