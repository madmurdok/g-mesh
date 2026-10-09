# GM-498 + GM-521: owed files and incomplete semantic passes

Status: design (GM-498/S1, also serving as GM-521/S1), for owner review. No
production code yet. Line numbers are from `release-4.3.0` at `07b8a12`.

- **GM-498**: semantic-pending stays set for files the SDK bridge re-asks on
  its own (`owed`, GM-487).
- **GM-521**: an incomplete whole-project semantic pass is never recorded, so
  every daemon start re-runs the whole pass.

Both touch `watcher::apply::apply_semantic_pass_in` and the wire type
`FileChangeResponse`, so this note gives them one shape.

## 1. Today's state

### 1.1 Core

- `core/src/watcher/apply.rs:234-315` `apply_semantic_pass_in`. After the
  round trip (`round_trip`, :454-518, returning `RoundTrip` :384-389 with
  `incomplete`, `incomplete_reason`, `upserted_edges`):
  - whole-project pass, incomplete: `bail!` (:279-283). The caller records a
    failure (`daemon::semantic::record_failure`, `semantic.rs:486`), so
    `language_state.semanticPassAt` stays unset and the next daemon start asks
    the whole project again (`schema::owed_semantic_pass_languages`,
    `schema.rs:1230-1250`, read by `run_with_registry_and_progress`
    `semantic.rs:316-366` and `run_once` `semantic.rs:421-484`).
  - per-file pass, incomplete: one log line (:284-288), nothing cleared.
  - per-file pass, complete: `schema::clear_semantic_pending_files(conn,
    &file_paths)` (:289-298) for **the requested paths only**.
- `apply_file_change_in` (:122-172) is the only source of per-file passes: it
  always sends exactly `vec![file_path]` (:161). Every other caller of
  `PluginProcess::semantic_pass` sends `Vec::new()` (whole project):
  `daemon/semantic.rs:347,461`, `daemon/workspace_reindex.rs:239`.
- `core/src/storage/schema.rs:1099-1107` `clear_semantic_pending_files`: deletes
  `semantic_pending_files` rows by path. Its only caller is
  `apply_semantic_pass_in` (g-mesh `find_callers`).
- **Where pending rows live.** `semantic_pending` / `semantic_pending_files`
  (`schema.rs:560-579`, ADR 0009) are written only by a workspace-reindex swap
  (`storage/language_swap.rs:540-551`) and cleared by *every* outcome of the
  whole-project pass that follows it: success (`record_language_semantic_pass`,
  `schema.rs:993`), failure or "not run" (`record_language_semantic_pass_failure`,
  `schema.rs:1013`, called from `record_failure`/`record_not_run`,
  `semantic.rs:486-510`). That pass is asked right after the swap
  (`workspace_reindex.rs:237-258`). So a pending row exists only in the window
  between the swap commit and the end of that whole-project pass (or until the
  next start, if the daemon died in between).

### 1.2 SDK bridge (used by Rust, Python and TypeScript)

- `plugins/sdk/src/lsp/bridge.rs:530` `owed: BTreeMap<RelPath, u8>`;
  `MAX_OWED_ATTEMPTS = 3` (:536); `settle_owed` (:607-632).
- `LspBridge::answer` (:2774-2970): a per-file pass's scope is the requested
  files plus every `owed` key (:2784-2788). It computes `finished` (asked
  about and covered, :2905) and `unfinished` (:2967), and settles `owed`
  (:2833-2835, :2893-2897, :2961-2968). An owed file that is also requested
  restarts at attempt 1 (:617-618).
- `plugins/sdk/src/run.rs:781-796` `pass_response`: `incomplete =
  whole_project && !answer.complete`. **A per-file pass is never reported
  incomplete**, by design (`respond_to_pass` doc, :708-727: avoid a core log
  line per save).
- `plugins/sdk/src/semantic.rs:91-106` `SemanticAnswer { diff, complete,
  reason }`: no per-file detail.
- TypeScript (`plugins/typescript/src/semantic.rs:99`) builds an `LspBridge`,
  so it has the same `owed` set. Python and rust-analyzer likewise.

### 1.3 Go

`plugins/go/semantic.go:472-521` `run` has no owed set. It reports incomplete
on per-file passes too (`control.go:299-301`, `writeResult` :370-377). Its
incompleteness is per Go module: `loadFor` (:568) records failed modules, and
`withoutFailedModules` (:526-536) drops their files from the scope. When no
module loaded (`outcome.loaded == 0`, :497) or `go` is missing (:477-481), the
whole pass is empty.

### 1.4 What actually goes wrong

1. **GM-498 as filed (over-warn).** File A is owed by the bridge (an earlier
   per-file pass did not finish it). A workspace reindex swap marks A pending.
   Before the whole-project pass starts, a per-file pass for B runs and the
   bridge finishes A too. Core clears only B. A reads as `pending` until the
   whole-project pass ends. The window is short (the swap and the pass are
   back to back, the plugin is serialized) but real.
2. **The inverse, found while reading (under-warn).** The SDK reports every
   per-file pass as complete, so core clears the pending row of a requested
   file that the bridge did *not* finish (a cold server, a timeout). That file
   then reads as fresh while its sites are unanswered. Same window.
3. **GM-521.** A whole-project pass that finished 9,900 of 10,000 files is
   recorded as a failure. Every start re-asks all 10,000. With a server that
   cannot answer some sites inside the budget (excalidraw + vtsls, GM-325/S33),
   every start pays the full pass and fails again, without bound.
4. **Owed lives in plugin memory.** The bridge's `owed` is lost when the plugin
   exits (idle sleep, memory suspension, daemon restart). Core cannot see it.

## 2. Options

### Decision 1: how core learns which files a pass settled

**(a) `settledFiles` on `FileChangeResponse`: the files the pass finished
(requested + owed).** Absent = today's behaviour.

- Benefit: smallest change. The bridge keeps `owed`; core clears pending for
  `settledFiles`.
- Risks: a whole-project pass would list every file (10,000 paths) or the field
  would be per-file only. It fixes problem 1 but not 2 unless the bridge also
  stops calling unfinished requested files complete. It does nothing for
  GM-521, which needs the *unfinished* files; GM-521 would then add a second,
  overlapping field. `owed` stays in plugin memory (problem 4).

**(b) Core infers settled files from the diff.** Rejected. A finished file
whose sites all answered "no target", or whose answers equal the edges it
already has, produces no upsert and no retraction, so it is indistinguishable
from a file that was never asked. A partly finished file produces rows too, so
"has rows" does not mean "finished". Retractions name edge ids, not files.

**(c) `unfinishedFiles` on `FileChangeResponse`, and core owns the owed set
(recommended).** The plugin reports which files in the pass's scope it did not
finish. Core keeps those files in a table and puts them into the scope of the
next pass itself. Because core then always knows the full scope it sent,
`settled = sent - unfinishedFiles`.

- Benefit: one field serves both tasks. GM-498 (problems 1 and 2): core clears
  pending for exactly the settled files. GM-521: an incomplete whole-project
  pass is recorded as its list, and the next start asks only those files. The
  owed set survives plugin restarts and daemon restarts (problem 4). The retry
  bound lives in one place (core), for every plugin, including Go.
- Risks: bigger than (a). The bridge's `owed` is removed (GM-487's behaviour
  moves to core), so GM-487's bridge tests are rewritten. A per-file pass may
  carry several files, so its latency is the latency of the owed files too
  (already true today, bounded by the 90 s single-file budget). Version skew: a
  new SDK plugin against an old core loses the re-ask until the file is edited
  (pre-GM-487 behaviour); bundled plugins ship with core, so only a third-party
  plugin rebuilt on a newer SDK meets this.

**Granularity: files, not sites.** Core has no notion of a site; open sites are
plugin-side (`questions`, bridge.rs:952). Re-asking a file re-asks all its
sites, of which most are cheap; the expensive one is the one that timed out
and would be asked anyway. GM-521's acceptance allows "or a stated policy";
this is the stated policy (question Q5).

### Decision 2: an incomplete per-file pass that finished some files

With (c): yes, clear `sent - unfinishedFiles`. The list is per file, so a file
in it is exactly a file whose answers are partial, and every other file in the
scope was finished. When `unfinishedFiles` is absent, keep today's rule: clear
the requested files only if the pass is not incomplete.

### Decision 3: do TypeScript and Go have an owed set?

- TypeScript: yes, it is the SDK bridge's `owed` (`semantic.rs:99`). Same for
  Python and Rust. With (c) all three get the change through the SDK alone.
- Go: no owed set. It can attribute incompleteness to files only when some
  modules loaded (the files of the failed modules). When nothing loaded or `go`
  is missing, it cannot, and sends no list. Whether Go reports the list is
  question Q4.

### Decision 4: GM-521 in the same shape

- **Record.** Core table `semantic_owed_files(language, filePath, attempts)`
  (new, `CREATE TABLE IF NOT EXISTS` next to `semantic_pending_files`; same
  no-schema-bump precedent as ADR 0009's tables). Per-file passes (GM-498) and
  incomplete whole-project passes (GM-521) write the same rows. A second table
  `semantic_residual(language PRIMARY KEY, since, reason)` marks "the last
  whole-project pass was incomplete and attributable; the next start asks only
  the owed files".
- **What the incomplete answer carries.** `incomplete: true`,
  `incompleteReason`, and `unfinishedFiles`. A list present means "exactly
  these"; absent means "unknown", and core keeps today's behaviour (the next
  start re-asks the whole project). That keeps LazyEngine's "server not
  installed" answer (`plugins/typescript/src/semantic.rs:28-33`) and Go's "no
  `go` binary" retrying once per start, cheaply, so installing the server and
  restarting still gets the pass.
- **Next start.** `owed_semantic_pass_languages` still names the language
  (semanticPassAt unset). If it has a `semantic_residual` row, the start sends
  `semanticPass { filePaths: <owed rows> }` instead of the whole project, with
  `semantic_pass_project_timeout(rows.len())`.
- **Bounded retry.** Every pass that asked a file and did not finish it adds 1
  to `attempts`; at 3 (today's `MAX_OWED_ATTEMPTS`, moved to core) the row is
  dropped with a log line and counted as a gap. A file therefore costs at most
  3 passes per content version, whether those passes are per-file ride-alongs
  or residual passes at start. When the residual set is empty (all answered or
  dropped), core records the pass (question Q3 for what it records).
- **Invalidation on edit.** A per-file pass for an edited file X restarts X's
  attempts at 1 if unfinished and deletes its row if finished (the bridge's
  rule today, `settle_owed` :617-618). A deleted file is not in the plugin's
  index, so it is never in `unfinishedFiles` and its row is deleted as settled.
  A workspace reindex swap deletes the language's owed and residual rows (the
  whole-project pass that follows writes fresh ones).
- **GM-515 presence batches.** `filesCreated` is a notification and every
  created file still gets its own `fileChanged` and per-file pass
  (`gm-515-batch-presence.md` D1). Owed files ride along on the first of those
  passes; if they stay unfinished they ride along on the next ones too, each
  costing an attempt, so a batch of 3+ creations can use up an owed file's 3
  attempts at once. Same as the bridge today; stated, not changed. A created
  file has no owed row.
- **Status.** `cli::status::semantic_pass_lines` (`status.rs:467`) shows
  "N file(s) left, next start asks only those" for a residual language and
  "N file(s) never answered" for gaps.
- **ADR 0009 pending rows.** Unchanged: any whole-project outcome still clears
  them. The residual pass at start is not a swap, so it writes no pending rows.

### Decision 5: wire compatibility

Optional field, no version bump. `CURRENT_PROTOCOL_VERSION`
(`wire/src/lib.rs:36-47`) is "bumped on any breaking change", and a missing
optional field is not one: `incomplete` and `incompleteReason` were added the
same way (`#[serde(default, skip_serializing_if)]`, :714-721), and nothing in
`wire` or `core/src/protocol` uses `deny_unknown_fields`, so an old core ignores
the new key. Shape:

```rust
/// semanticPass only: the files of this pass's scope it did not finish.
/// Absent = unknown (core keeps today's behaviour); present and empty =
/// every file in scope finished.
#[serde(rename = "unfinishedFiles", default, skip_serializing_if = "Option::is_none")]
pub unfinished_files: Option<Vec<String>>,
```

The spec text goes into `docs/architecture/multi-language-plugins.md` ("Wire
v2"), beside `incomplete`.

## 3. Recommendation

Option (c): `unfinishedFiles` on the wire, owed set owned by core in
`semantic_owed_files`, bridge `owed` removed. GM-498 lands first and introduces
the field, the table and per-file ride-along; GM-521 builds the residual
whole-project path on the same table. The SDK keeps not sending `incomplete`
on per-file passes (no new core log line per save); core reads the list
whenever it is present, whatever `incomplete` says.

## 4. Edit map

### GM-498/S2 (code)

| Where | What |
|---|---|
| `wire/src/lib.rs:690-721` `FileChangeResponse` | add `unfinished_files` (shape above) |
| `plugins/sdk/src/semantic.rs:91-125` `SemanticAnswer` + `complete`/`incomplete`/`incomplete_because` | add `unfinished: Option<BTreeSet<RelPath>>`, `None` by default |
| `plugins/sdk/src/run.rs:781-796` `pass_response` | send `unfinishedFiles` when `Some`, on per-file and whole-project passes; keep the `incomplete` gating |
| `plugins/sdk/src/lsp/bridge.rs:2774-2970` `answer` | fill `unfinished` on every return: empty scope / nothing to ask (:2795, :2817-2837) = empty; ceiling admitted nothing (:2805-2816) = the scope; no server / not ready (:2885-2899) = `asked_about`; normal (:2940-2970) = `asked_about - finished`, plus scope files the ceiling left unasked when `plan.truncated` |
| same file :523-536, :601-632, :2776-2788, :2831-2836, :2893-2897, :2950-2969 | remove `owed`, `MAX_OWED_ATTEMPTS`, `settle_owed` and the scope extension |
| `core/src/storage/schema.rs` DDL :560-579 | `semantic_owed_files(language, filePath, attempts)`; add to the drop list :1350 |
| same, beside :1099 | `owed_files(conn, language) -> Vec<String>`; `settle_owed_files(conn, language, requested, settled, unfinished)` (attempts rule as `settle_owed`); `record_language_semantic_pass` :993 deletes the language's owed rows |
| `core/src/storage/language_swap.rs:540-551` | delete the swapped language's owed rows in the swap transaction |
| `core/src/watcher/apply.rs:384-389` `RoundTrip`, :512-517 | carry `unfinished_files` |
| same :122-172 `apply_file_change_in` | per-file scope = `[file_path] ∪ owed_files(language)` |
| same :279-298 `apply_semantic_pass_in` per-file branch | with a list: clear pending for `sent - unfinished`, `settle_owed_files`; without: today's code |

Go: no change. TypeScript/Python/Rust plugins: no change (SDK only).

### GM-521/S2 (core)

| Where | What |
|---|---|
| `core/src/storage/schema.rs` DDL | `semantic_residual(language PRIMARY KEY, since, reason)`; helpers to set/read/clear it; drop list :1350 |
| `core/src/watcher/apply.rs:279-283` whole-project incomplete branch | with a list: write owed rows (attempts +1), set `semantic_residual`, return a distinct outcome (not `bail!`) so the caller records "residual" instead of a failure; without a list: today's `bail!` |
| `core/src/daemon/semantic.rs:316-366`, :421-484 | a language with a residual row is asked `semanticPass { filePaths: owed rows }` with `semantic_pass_project_timeout(n)`; when no rows remain, record per Q3 |
| `core/src/daemon/workspace_reindex.rs:237-258` | handle the new outcome like `semantic.rs` does |
| `core/src/cli/status.rs:452-480` | residual and gap lines |
| `core/src/storage/language_swap.rs:540-551` | clear `semantic_residual` too |

### GM-521/S4 (plugins)

| Where | What |
|---|---|
| `plugins/sdk/src/lsp/bridge.rs:633-641` `pass_budget` | a multi-file scoped pass (residual) gets the scaled budget (`project_floor.max(per_file * n)`), not the 90 s single-file budget; a one-file pass unchanged |
| `plugins/go/wire.go:302-315`, `control.go:367-377`, `semantic.go:472-536` | only if Q4 = yes: `UnfinishedFiles` = files of failed modules when `loaded > 0`; omitted when nothing loaded |

## 5. Behaviours for the tests slices

Control = the production revert that must make the test fail.

GM-498/S3:

1. **Owed file answered later is no longer pending** (core,
   `core/src/watcher/apply/tests.rs`). Pending rows for A and B; per-file pass
   for A answers `unfinishedFiles: [A]`; per-file pass for B (core sends B and
   A) answers `unfinishedFiles: []`. A and B are no longer pending. Control:
   drop the `owed_files` ride-along in `apply_file_change_in`.
2. **Unfinished requested file stays pending** (core). Per-file pass for A
   answers `unfinishedFiles: [A]`: A stays pending (today it is cleared).
   Control: clear `file_paths` instead of `sent - unfinished`.
3. **Absent list keeps today's behaviour** (core). An answer without the field
   clears the requested file when complete and nothing when incomplete
   (extends `an_incomplete_per_file_pass_is_not_an_error`, tests.rs:866, and
   `per_file_pass_over_a`, :1039). Control: treat absent as empty.
4. **Attempts bound** (core). A file unfinished on 3 passes is dropped from
   the owed rows; an edit (requested again) restarts at 1. Control: remove the
   cap.
5. **Bridge reports what it did not finish** (SDK,
   `plugins/sdk/tests/lsp_bridge.rs`, rewrite of the GM-487 owed tests near
   :2834). A fake server that stalls on one file: the answer's `unfinished`
   names that file only; a server never ready: `unfinished` = every asked
   file. Control: return `None` from `answer`.
6. **Wire round trip** (`wire/src/lib.rs` tests near :1238-1270). Absent reads
   `None`; `[]` and `["a.rs"]` round-trip. Control: drop `default`.

GM-521/S3 and S5:

7. **Next start asks only the residual files** (core, scripted plugin). Start
   1 answers incomplete with `unfinishedFiles: [x]`; start 2 sends
   `filePaths: [x]`, not `[]`. Control: today's `bail!` path.
8. **Bounded across starts** (core). A file unfinished on every start is asked
   on at most 3 passes, then the language is recorded per Q3 and start 4 sends
   no pass. Control: remove the cap.
9. **Edit invalidates** (core). After start 1 records `[x]`, an edit of x whose
   per-file pass finishes it removes the row; start 2 sends no residual pass.
10. **Absent list keeps whole-project retry** (core). Incomplete without the
    field: start 2 sends `filePaths: []`. Control: treat absent as empty.
11. **Swap clears the record** (core). A workspace reindex deletes the
    language's owed and residual rows.
12. **Residual pass gets a scaled budget** (SDK fake server, timer test, 5x).
    Control: `pass_budget` returns `single_file`.
13. **Go (if Q4 = yes).** One failed module of two: `unfinishedFiles` = its
    files; none loaded: field omitted. Control: always omit.

Pick 6-8 controls across both tasks (CLAUDE.md); 1, 2, 5, 7, 8, 10 are the
ones that tell the arms apart most directly.

## 6. Questions for the owner

**Q1. Do GM-498 at all, and in which shape?** Today a semantic-pending row
exists only between a workspace reindex (for example after `git checkout`
changes `Cargo.toml`) and the end of the whole-project semantic pass that
follows it, seconds to minutes. GM-498's bug: in that window, if the plugin
finishes file A while answering a pass for file B, A keeps saying "pending"
until the window closes. Reading the code shows the opposite bug too: if the
plugin fails to finish file A during A's own pass, core still clears A's row,
so A says "fresh" while its calls are unresolved.
- (1) Fix both with the shared design (c), GM-498 first (Recommended).
  Benefit: correct warnings both ways, and GM-521 reuses the field and table.
  Risk: GM-498 grows from a field to a core table and moves GM-487's re-ask
  into core.
- (2) Minimal `settledFiles` field, bridge keeps `owed`. Benefit: small.
  Risk: fixes only the over-warn; GM-521 adds a second field later.
- (3) Close GM-498 as not worth fixing (narrow window), do GM-521 alone.
  Benefit: no work now. Risk: GM-521 must still design the field alone, and
  the under-warn stays.

**Q2. Who remembers the files a pass did not finish: core or the plugin?**
Today the SDK plugin remembers them in memory (`owed`) and re-asks them on its
own; core does not know they exist, and the memory is lost when the plugin
sleeps or the daemon restarts. Example: rust-analyzer is cold, the pass for
`lib.rs` does not finish it; the plugin is put to sleep after idling; `lib.rs`
is never re-asked until edited.
- (1) Core, in a table (Recommended). Benefit: survives restarts, one retry
  bound for every plugin, core knows exactly what it asked. Risk: more core
  code; GM-487's bridge tests are rewritten.
- (2) The plugin, reporting settled files to core. Benefit: GM-487 stays as
  is. Risk: lost on restart; GM-521's "next start asks only those" needs core
  to persist them anyway.

**Q3. What does a whole-project pass record once its leftover files are all
answered or given up on?** Today `semanticPassAt` is set only for a pass that
answered everything; otherwise it stays unset and the MCP instructions keep
listing the "method calls through a variable may be missing" gap for that
language. Example: on excalidraw, vtsls answers 9,990 files; 10 time out on
three starts in a row and are given up.
- (1) Set `semanticPassAt` and show "10 file(s) never answered" in `g-mesh
  status` (Recommended). Benefit: starts stop paying for the pass; the gap is
  still visible. Risk: the MCP instructions stop listing the gap for the whole
  language although 10 files still have it.
- (2) Leave `semanticPassAt` unset but stop retrying. Benefit: the
  instructions keep warning. Risk: "pass owed" is shown forever for a pass
  nobody will run; only `g-mesh reindex` resets it.

**Q4. Should the Go plugin report which files it did not finish?** Today Go
reports a pass incomplete when a module fails to load (for example a module
with a broken `go.mod`), and every start re-loads the whole project.
- (1) Yes, list the failed modules' files (Recommended). Benefit: bounded,
  cheaper starts for Go too. Risk: a small Go change and test; a module fixed
  by an edit to `go.mod` (not a `.go` file) is retried only after a reindex,
  which a `go.mod` edit triggers anyway.
- (2) No, Go sends no list and keeps today's whole-project retry. Benefit: no
  Go work. Risk: GM-521's bound does not cover Go.

**Q5. Is "re-ask the unfinished files" acceptable where GM-521 says "re-ask
only unanswered sites"?** A file can hold 200 call sites, of which 1 timed
out. Core has no idea what a site is; only the plugin does.
- (1) Files (Recommended). Benefit: one simple list on the wire, core can
  store and bound it. Risk: the 199 answered sites are asked again (cheap
  compared with the one that timed out).
- (2) Sites. Benefit: minimal re-asking. Risk: site ids on the wire and in
  core's table, invalidated by every edit; much larger change.

## g-mesh calls this note relied on

- `find_callers(clear_semantic_pending_files)` -> only `apply_semantic_pass_in`.
- `find_references(FileChangeResponse)` -> `round_trip` (apply.rs), staleness
  tests, `cli/plugin_check/session.rs`. The SDK and Go do not use the type;
  they build the JSON by hand (`run.rs:781`, `go/wire.go:302`), found by grep.
- `find_definition(FileChangeResponse | apply_semantic_pass_in |
  clear_semantic_pending_files | round_trip | SemanticAnswer)` for sources.
- `find_references(SemanticAnswer, files)` -> SDK `run.rs`, `semantic.rs`,
  `bridge.rs`, toy, TS/Python tests.
- `find_callers(apply_semantic_pass)`, `find_callers(PluginProcess::semantic_pass)`,
  `find_callers(PluginSupervisor::semantic_pass)` -> every non-test caller
  sends a whole-project pass; per-file passes come only from
  `apply_file_change_in`.
- `find_callers(language_semantic_pass_done | owed_semantic_pass_languages,
  files)` -> `status.rs`, `provenance.rs`, `daemon/semantic.rs`.
- `get_file_outline(bridge.rs)` overflowed its output limit; grep was used for
  bridge line numbers. Readers of `incomplete` were found by grep
  (`apply.rs`, `run.rs`, Go `control.go`/`wire.go`).

## 7. Owner decisions (2026-10-09)

- **Q1: "Чинить оба по дизайну (c)".** GM-498 fixes both the over-warn and the
  under-warn with option (c); GM-498 lands first and introduces the field and
  the table.
- **Q2: "Core, в таблице".** The owed set lives in core's
  `semantic_owed_files`; the bridge's `owed` is removed.
- **Q3: "Ставить semanticPassAt + счётчик".** Once a residual pass's files are
  all answered or given up, `semanticPassAt` is set and `g-mesh status` shows
  the count of files never answered.
- **Q4: "Да, файлы упавших модулей".** The Go plugin reports the files of
  modules that failed to load as `unfinishedFiles` (GM-521/S4).
- **Q5: "Файлы целиком".** Re-asking is per file, not per call site; GM-521's
  acceptance criterion is amended to match.

### Implementation notes (GM-521/S7)

- **Never-answered count.** Q3's counter is a table, `semantic_gap_files
  (language, filePath)`: `settle_owed_files` writes a row when it drops a
  file at `MAX_OWED_ATTEMPTS` and deletes it when a later pass settles or
  re-owes the file, so an edit clears it. A complete whole-project pass and a
  workspace reindex swap clear the language's rows;
  `record_language_semantic_pass_settled` (Q3) keeps them for `g-mesh status`.
- **A residual pass that fails outright** (timeout, crash) costs each of its
  files an attempt, so a file whose pass always times out is asked on at most
  3 starts.
- **GM-515 presence batches.** Unchanged from Decision 4: owed files, residual
  ones included, ride along on every per-file pass of a creation batch, so a
  batch of 3+ creations can spend an owed file's attempts at once. The bound
  holds; it is reached sooner.
