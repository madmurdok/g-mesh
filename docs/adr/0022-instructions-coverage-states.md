# 0022. Instructions say which languages are covered; per-answer facts move to the answer

## Status
Accepted 2026-10-04 (GM-330/S1, owner review). Completes [ADR 0021](0021-per-language-bulk-outcome.md):
a partial index is only sound once the instructions say it is partial.
Amends [ADR 0003](0003-mcp-instructions-rendering.md) (paragraphs 3 and 4).

## Context
`mcp::instructions::build` renders two states per language: in the index, or
silent. An agent asking about a Python file with no Python plugin gets an
empty answer that reads as "nothing found". ADR 0021 now records, per
language, `Indexed`, `PluginAbsent { files }` or `Failed { error }`
(`schema::language_outcomes`), and the text has to say all of them.

The 1900-byte ceiling (`INSTRUCTIONS_BYTE_CEILING`, under Claude Code's 2 KB
truncation) is already spent. Measured at the start of this work by the module's
own tests (`cargo test -p g-mesh --lib mcp::instructions:: -- --nocapture`), with the
paragraph sizes re-derived from source and matching those totals exactly:

| Rendering today | Bytes |
|---|---|
| TypeScript only (`P4_GENERIC`) | 1836 |
| Rust only, pass done (`P4_STATIC_RECEIVER`) | 1762 |
| TypeScript + Rust, pre-pass (`p4_named`) | 1849 |
| Worst case, every language named | 1888 |
| `p4_fallback` | 1688 |
| Cold start, walking, 103-byte root | 1885 |

Paragraphs: P1 145, P2 348, P3 576, P4 508-582 (fallback 434), P5 177. So at
worst 12 bytes are free, and the new statements need about 300 to 500.

The text is also a stale snapshot. `get_info` is read once, at `initialize`,
and `language_state.semanticPassAt` can be set later. Today a Rust session can
claim the receiver-call gap long after the pass closed it.

The owner's channel rule decides where each statement goes. If the caller
needs it before asking, it goes in the instructions, and only facts about this
project that change what the agent asks qualify. If it is only needed once an
answer exists, it goes in a response field. Facts for people who build or debug
a consumer go in repo docs.

## Decision

### 1. Every current statement, classified

| # | Statement (paragraph) | Kind | Action |
|---|---|---|---|
| 1 | Structural graph tools; prefer over grep for defs/refs/calls/imports (P1) | before asking | keep |
| 2 | An anchored result is resolved per call site; do not re-check with grep; grep only for the gap below (P2) | before asking: it sets whether the agent plans a verification step at all. Its per-answer half already exists (`anchor.resolvedBy`) | keep |
| 3 | `resolved: false` means a cross-file edge the linker could not confirm; same-file edges are always `resolved: true` (P3) | once an answer exists (a row says it) | **move now** to a once-per-session `hint`, triggered by a page with a `resolved: false` row |
| 4 | `allUnresolved: true` on a page where every row is unconfirmed; check it (P3) | once an answer exists | **move now**: drop it. `session_hints::ALL_UNRESOLVED` already rides on every such page of the four tools, so the instructions repeat it |
| 5 | `allUnresolved` is never set on an empty page (P3) | once an answer exists | **move now**: drop it. The flag is absent there, and there is nothing to act on |
| 6 | A method call through a variable receiver may produce no edge in {languages} (P4) | **split**. In a language with no tier that resolves receiver calls (TypeScript today), it is a fixed property of the plugin, so the agent needs it before asking: keep, named. In a language whose semantic pass resolves them, it is temporary state: **move now** to the `provenance` field that already exists (section 2) | split as stated |
| 7 | A method page that may miss such calls carries `untypedReceiverCalls` where the language reports them (P4) | once an answer exists. The field carries its own `hint` | **move now**: drop it |
| 8 | Bare/this/super/qualified-type calls have no such gap; `hasMore: false` without `unlinkedUsages` is exhaustive (P4) | the signal is that a field is *missing*, and a response can only say that by putting a positive field on every healthy answer. So this belongs before asking, with the same reasoning as P2 | keep |
| 9 | Receiver calls bind to the declared/inferred type; an override's caller page under-reports; use find_implementations (P4 static) | once an answer exists (on an override's caller page) | keep; **owed**: a field on `find_callers` when the anchor overrides or implements a base member, pointing at the base. Not in this task |
| 10 | The first index, or a re-index after an upgrade, waits for the walk: slow, not wrong (P4) | before asking, but only while a walk is owed. A warm `build` never renders while one is: an upgrade wipes the index, and the next session is a cold start | **move now** into `cold_start_line`. Drop it from the warm text |
| 11 | Pass `symbol_name` directly; raise `limit` (P5) | before asking | keep. Reword "the four tools above" (its referent was P3) to name the four tools |
| 12 | `p4_fallback`: "a method's page says so in `unlinkedUsages`/`untypedReceiverCalls`" | **false for TypeScript**, which reports neither field (only the Rust plugin sends `untypedCalls`) | replace it (section 4, step 3) |

What remains owed (recorded here, not built by this task):

- (a) TypeScript reporting `untypedCalls`. With that, statement 6's fixed half can also become a response field.
- (b) The override field in row 9.
- (c) **Delivered (GM-503):** path-anchored answers carry `notIndexed`; see `docs/architecture/gm-503-absent-language-field.md`. Original wording: a field on an answer about a path in an absent or failed language, for example `get_file_outline` or `find_definition` on a `.py` file with no Python plugin. It would carry the language, the reason and the install command. `languages::absent_for_path` exists for this and has no caller yet.
- (d) `docs/architecture/tool-answer-guarantees.md`, which does not exist yet. It is the consumer-facing home for statements 3-8.

### 2. The receiver-call gap: capability in the text, state in the answer

An instruction rendering is chosen **from manifest capabilities only, never
from `semanticPassAt`**. Each present language falls into one of three
classes:

- **static**: `receiver_calls_structural = Resolved`. No gap.
- **pass-dependent**: structural tier `Unresolved`, `receiver_calls =
  Resolved` and `semantic_pass = true` (Go, Python and Rust today).
- **never**: anything else. Either `receiver_calls = Unresolved` (TypeScript
  today), or `Resolved` with no semantic tier to deliver it.

The **never** languages are named, as today (`P4_PERM`). When there are none,
`P4_STATIC` renders, which is the old static paragraph without the wait
sentence. If any language is **pass-dependent**, one capability-based sentence
is appended (`S_PASS`). It says the call has no edge until the language's
pass has run, and that a page answered before then carries `provenance`.
That sentence is true at every moment of a session, so it cannot go stale.

The live state is already on the answer. `provenance` (`mcp::provenance`) is
read per call on `find_references`, `find_callers`, `find_callees` and
`find_implementations`. It is `{"language", "semanticTier": "absent" |
"pending"}` exactly when a declared semantic tier has not completed. For a
pass-dependent language, the instructions used to name the gap in exactly
these states: `semantic_pass = true` and no pass done. A once-per-session
`hint` sentence now explains the field (`HINT_PROVENANCE`). Today nothing
tells an agent what `semanticTier: absent` means.

Per-edge provenance (`edges.source`, `edges.engine`) describes an edge that
exists. The gap is about edges that do not, so `provenance` rejected it. That
reasoning is in `mcp::provenance`'s module doc and stands.

**Staleness (deliverable 3).** The pre-pass gap claim leaves the text, so the
stale false claim is gone, not merely tolerated. What can still go stale
within a session are the coverage statements:

- A plugin installed, or a failed language reindexed, after `initialize`
  leaves "not indexed" in the text. That over-reports absence, which is the
  safe direction: the agent may skip a question it could have asked, but it
  never trusts an empty answer.
- A plugin *removed* while a session runs would leave "indexed" in the text.
  That is the unsafe direction. A daemon's plugin list is fixed for its life,
  so the removal takes effect at the next daemon start. A single-project
  session ends with its daemon and never sees the new state; a folder
  (multi-project) session reselects the project against the new daemon and
  then gets the path-anchored `notIndexed` answers (owed item (c), delivered
  by GM-503).

### 3. The text of each state

The coverage paragraph goes second, after P1, because it tells the agent
which questions g-mesh can answer at all. `{list}` uses today's
`format_language_list`. Byte counts below are for the templates with their
placeholders.

| State | Template | Bytes |
|---|---|---|
| working (always, when anything is indexed) | `Indexed here: {list}. g-mesh has no answers about files in any other language.` | 78 |
| unsupported | covered by the second sentence above, which is said once and generically and names no catalogue | 0 extra |
| nothing indexed | `Nothing is indexed here: g-mesh has no answers about this project's files.` | 74 |
| plugin absent | `Not indexed, no plugin installed: {items}.` with item `{lang} ({n} files; `g-mesh plugins install {lang}`)`, or `{lang} (files not counted; `g-mesh plugins install {lang}`)` when `files` is `None` | 42 + 51 or 59 per language |
| failed | `Not indexed, plugin failed: {items} - fix the plugin, then run `g-mesh reindex`.` with item `{lang} ({error})`, where `error` is the innermost cause of the chain (the last non-empty line of the stored error, which keeps one cause per line) with absolute and `~/` paths shortened to their file name, cut to 100 bytes on a char boundary | 80 + 16 + error per language |
| trailer (when anything is absent or failed) | `Until then an empty answer about those files is not evidence of absence.` | 72 |
| receiver, never | `P4_PERM`: "...in {list}, a method call through a variable receiver (`x.foo()`) may produce no edge, so a method's caller/reference list there can under-report; bare function calls and this/super/qualified-type calls have no such gap, and for those `hasMore: false` without `unlinkedUsages` is exhaustive." | 335 |
| receiver, static | `P4_STATIC` (today's static paragraph without its wait sentence) | 343 |
| receiver, pass-dependent | `S_PASS`, appended: `Until a language's semantic pass has run, such a call has no edge at all there; a page answered before then carries `provenance`.` | 129 |
| cold start | `Plugins installed: {list}. g-mesh has no answers about files in any other language.` and, for `languages::missing()`: `If this project has {a or b} files, they are not indexed: no plugin installed ({commands}).` | 83, 85 |

**Failed** is a fourth state: the plugin is present but unusable, and ADR
0021, section 2, guarantees that nothing of the language is in the index. To
the agent it reads like absent: no answers, and an empty result proves
nothing. Only the fix differs. The text names the language, the innermost cause of
the error, and `g-mesh reindex`, which is ADR 0021's retry path. The error is
the first thing the ceiling cuts.

With no language indexed, the receiver paragraph is left out. When every
language failed, `Phase::Failed` gets the same rendering.

### 4. Fitting the ceiling: one ladder, test-pinned

`build_within` tries each step in order and takes the first that fits:

1. Full text.
2. Failed errors dropped. The language names stay.
3. The never-list replaced by a generic, truthful `P4_PERM_FALLBACK` (354
   bytes), which makes no claim that pages disclose the gap.
4. The `Indexed here` list replaced by `g-mesh has no answers about files in a
   language it has not indexed.`

Absent and failed names, and the install commands, are never dropped:
showing them is what this decision is for. `p4_fallback` is kept only as
step 3, rewritten. Its old reason, a long *named gap list*, mostly goes away,
because pass-dependent languages are no longer listed.

Projected bytes after the change, from a prototype of these templates. S2's
tests re-measure them:

| Scenario | Bytes |
|---|---|
| TypeScript only | 1141 |
| Rust only | 1269 |
| TypeScript + Rust | 1280 |
| TypeScript + Python absent (214 files) | 1301 |
| **Four states**: TypeScript indexed, Rust failed (182-byte error), Go and Python absent (5-digit counts) | 1531 |
| Zero plugins, all four absent | 1115 |
| Four discovered, three failed with long errors | 1613 |
| All four failed (`Phase::Failed`) | 1379 |
| Stress: 16 synthetic never-languages + TypeScript + 3 absent | 1695 |
| Stress: 16 synthetic failed + TypeScript + 3 absent | 1617 (step 2) |
| Cold start, 103-byte root, TypeScript only, 3 missing | 1601 unindexed / 1572 walking |

Every realistic case fits at step 1, with at least 287 bytes free.

### 5. Inputs

The warm path reads `present_languages_with_semantic_state`, for the indexed
list, and `language_outcomes` (absent and failed rows) under one store read.
Absent and failed come from the outcomes, not re-derived (ADR 0021, section
5). If the outcomes read fails, the text falls back to the cold-start
`missing()` wording, which needs no I/O. The cold start still never takes the
store, and adds `languages::missing(discovered)` through a new registry
accessor.

## Consequences
- An agent is told which languages are covered, which are not and why, and
  how to fix each. An empty answer about an uncovered language stops looking
  like absence.
- The instructions no longer depend on semantic-pass state. They cannot claim
  a gap that has closed, and `present_languages_with_semantic_state`'s bool
  stops feeding them. The integration test
  `the_generated_mcp_instructions_reflect_a_real_suspended_rust` changes its
  assertions accordingly.
- Two `hint` sentences are added (`resolved: false`, `provenance`). The text
  every session reads changes, which is visible in g-mesh-bench. S4 measures
  it.
- The owed items in section 1 are follow-up tasks, not part of this one.

## Resolved at review (owner, 2026-10-04)
1. The pre-pass gap sentence leaves the text; the live state is `provenance`
   on the answer plus a once-per-session hint, and the text keeps only the
   capability-based `S_PASS`. This meets "working" under the channel rule.
2. A failed language shows the innermost cause of its error chain, with
   absolute and `~/` paths shortened to their file name, up to 100 bytes cut
   on a char boundary; it is the first thing the ceiling cuts. The store keeps
   the whole chain, one cause per line, so the innermost cause is the last
   line even when its own text contains `: `; `g-mesh status` and stderr
   still show every context, joined back with `: `.
3. Cold start uses the conditional wording from `languages::missing()`: no
   count and no I/O at `initialize`. Later sessions read counts from
   `language_outcome`.
4. The absent text does not mention the daemon restart an install needs;
   `g-mesh plugins install` says so in its own output.
5. Paragraph 3 moves to hints as decided; the bench measurement checks the
   prompt change.
6. Owed items (a)-(d) are separate backlog tasks.

## Measurement (GM-330/S4)

Run 2026-10-04 at 89efca1. **Verdict: the projections hold.** Every rendered
scenario is at or under the 1900-byte ceiling (`INSTRUCTIONS_BYTE_CEILING`).
Ten of the eleven projected rows match exactly, and the eleventh is 1.0 %
under. In a live session, each coverage state renders its line when it
should and leaves it out when it should not. Part C (a g-mesh-bench
session) was not run.

### A. Bytes (the module's own tests)

`cargo test -p g-mesh --lib mcp::instructions:: -- --nocapture --test-threads=1`
ran 47 tests, all passing. `uptime` load 4.69, and `/usr/bin/time -p`
gave real 20.82, user 9.21, sys 3.98, mostly compiling. The one projected
row that no test prints (16 never-languages) came from a scratch test in a
throwaway worktree, which was not committed. It used names `never00` to
`never15` with `Capabilities::default()`, plus the real TypeScript
manifest and three absent languages.

| Scenario (section 4) | Projected | Measured | Step | Δ |
|---|---|---|---|---|
| TypeScript only | 1141 | 1141 | 1 | 0 |
| Rust only | 1269 | 1269 | 1 | 0 |
| TypeScript + Rust | 1280 | 1280 | 1 | 0 |
| TypeScript + Python absent (214 files) | 1301 | 1301 | 1 | 0 |
| Four states (Rust failed with a 182-byte error cut to 100; Go and Python absent with 5-digit counts) | 1531 | 1531 | 1 | 0 |
| Zero plugins, all four absent | 1115 | 1115 | 1 | 0 |
| Four discovered, three failed with long errors | 1613 | 1613 | 1 | 0 |
| All four failed | 1379 | 1379 | 1 | 0 |
| Stress: 16 never + TypeScript + 3 absent (5-digit counts; 214: 1689; not counted: 1713) | 1695 | 1695 | 1 | 0 |
| Stress: 16 failed + TypeScript + 3 absent | 1617 | 1601 | 2 | −16 (−1.0 %) |
| Cold start, 103-byte root, TypeScript only, 3 missing, unindexed / walking | 1601 / 1572 | 1601 / 1572 | 1 | 0 |

The tests print other renderings too, none of them over the ceiling:

- Cold start with 0 missing: 1569 / 1540. With 4 missing: 1304 / 1275.
- Ladder step 3 (40 never + 3 absent + 2 failed): 1843.
- Ladder step 4 (80 static + 3 absent + 2 failed): 1496.
- `build_front` with 64 60-byte names under a 103-byte root: 1846. This is
  the largest rendering measured.
- The worst-case cold start, named / fallback: 1776 / 1476.

Before and after:

| Worst case | Before | After |
|---|---|---|
| Every language named (warm) | 1888 | 1319 |
| Worst realistic warm (three failed with long errors) | n/a, no such state | 1613 (287 free) |
| Cold start, walking, 103-byte root | 1885 | 1572 (3 missing) |

### B. Live render

The binary was a release build (`cargo build --release --workspace`:
real 140.15, user 19.79, sys 2.93, `uptime` load 4.24 at the start). The
binaries were copied to a private `bin/`. The corpus was a scratch
directory: 2 `.rs` files with a `Cargo.toml`, 3 `.py` and 2 `.ts`. Each
arm used its own private `G_MESH_HOME`. Plugin discovery followed the
GM-329/S5 recipe: `G_MESH_PLUGIN_ROOTS_OVERRIDE` pointed at a private root
of per-language directories that symlink the checkout's `plugins/<lang>`.
`G_MESH_MODEL_DIR` pointed at a missing directory.

Each warm arm ran `g-mesh reindex` and then one MCP session. The session
was a stdio JSON-RPC `initialize` sent to `g-mesh mcp-shim`, run from the
corpus with `CLAUDE_PROJECT_DIR` unset. That is the same path as
`core/tests/plugin_memory_limit.rs`. `language_outcome` was read back with
sqlite3. After each arm, the daemon and its descendants were killed with
`kill -9`, and no process from the private `bin/` was left. During the run
the machine was loaded (`uptime` 1-minute average 46.6 at the start and
34.5 at the end). The whole run took real 24.05, user 7.20, sys 4.24.

The control that tells the arms apart is whether the state line is
present.

| Arm | Bytes | State line | Check | Verdict |
|---|---|---|---|---|
| absent: Python not discovered | 1438 | `Not indexed, no plugin installed: python (3 files; ...)` + trailer | row `python=plugin_absent, files=3` equals the 3 in the text | pass |
| control: all four discovered | 1288 | none (no "Not indexed") | every present language `indexed` | pass |
| failed: Rust binary removed from a *copied* `rust/` (manifest `command = "./g-mesh-plugin-rust"`) | 1536 | `Not indexed, plugin failed: rust (failed to spawn the rust plugin's bulk index (/private/tmp/...) - fix the plugin, then run `g-mesh reindex`.` + trailer | row `rust=failed` with the spawn error. `reindex` exit 2 | pass |
| restored: binary put back, `g-mesh reindex`, new session | 1288 | none. Rust is back in `Indexed here` | row `rust=indexed` | pass |
| cold: fresh home, first session, Python not discovered | 1706 | `If this project has python files, they are not indexed: no plugin installed (...)` | `Index root:` header (unindexed) | pass |

The cold arm's 1706 bytes is not comparable to the projected 1601. Its
root is 137 bytes rather than 103, it lists three installed plugins and
one missing, and it is still 194 bytes under the ceiling.

### Findings

1. **The failed line spends its 100 bytes on the path.** The rust error
   was `failed to spawn the rust plugin's bulk index (<138-byte plugin
   path>): No such file or directory (os error 2)`. Cut to 100 bytes, it
   keeps the path prefix and loses the cause. The language name and the
   `g-mesh reindex` fix survive, which is what section 3 guarantees. The
   cause survives only on a short install path such as `~/.g-mesh/plugins`.
   One option is to put the cause before the path in that error, or to cut
   the path rather than the tail. That is a separate task if wanted.
2. **A deep `G_MESH_HOME` cannot host a daemon.** Under the scratchpad,
   the socket path was 181 bytes, over the macOS limit of 103. The shim
   says so clearly. The run used a short symlink, `/tmp/g330`, into the
   scratchpad. This is not a GM-330 issue.
3. `g-mesh reindex` exits 2 when a language fails. Coverage is still
   recorded and served.
