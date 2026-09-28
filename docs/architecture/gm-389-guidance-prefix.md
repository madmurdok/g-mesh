# A lighter guidance prefix (GM-389, with GM-420 folded in)

Status: **design, awaiting owner review.** No production code changed in this slice.
Branch: `feat/GM-389-lighter-guidance-prefix`, cut from `release-3.16.0` (9174b86).
Line numbers are for that tree; later slices will move them.

## 0. What this fixes, re-measured today

GMB-176 (3.10.0, 27 Go/Rust/Python tasks, REPS=normal) put 97.2% of g-mesh's
cost gap in `cacheRead`: +7,107 tokens on every turn. The largest single part of
that prefix is the guidance block `g-mesh init --agent` installs. The server's own
`instructions` are capped at `INSTRUCTIONS_BYTE_CEILING = 1900`
(`core/src/mcp/instructions.rs:22`). The guidance block has no cap.

Re-measured on this branch (python byte counts of the raw string, cross-checked with `wc -c`):

| copy | bytes today | ticket said | note |
|---|---|---|---|
| `AGENTS_MD_SNIPPET`, `core/src/cli/agent_instructions.rs:52` | **13,926** | 13,844 | GM-381 (`ac630de`) added the `noMatch` sentence after the ticket was written |
| g-mesh-bench `GMESH_CONFIGURED_CLAUDE_MD` (`harness/lib/armConfig.ts:311`, release-0.24.0) | 13,926 | 13,792 | byte-identical to the shipped constant; the drift guard holds |
| same, de-scoped for Go/Rust/Python by `descopeGuidance` | 13,874 | 13,792 | what a non-TS arm actually receives |
| README.md "Reducing self-verification cost" fenced block | 13,862 | - | **a fourth copy**, one line already drifted; nothing tests it |
| `~/.claude/CLAUDE.md` "Code search" section, this machine | **2,983** | 13,649 | the owner hand-trimmed it after the handoff note; it already carries most GM-420 items |

So the copy this machine's agents actually pay for is already 2,983 B, but it
was never measured. The bench and every `g-mesh init` user still get 13,926 B.

Where today's 13,926 B go (split at top-level bullets):

| part | bytes | category |
|---|---|---|
| heading + "prefer g-mesh over grep for X" | 48 + 895 | prefix-shaped |
| no manual indexing | 303 | prefix-shaped |
| "How to use the tools" catalogue (incl. ambiguity + `resolvedBy`) | 3,340 | duplicates schemas/descriptions |
| typical flow | 347 | duplicates `instructions` P5 |
| completeness (`resolved`/`allUnresolved`) | 1,389 | field-reading rule |
| ambiguous name -> `symbol_id` stays final | 780 | field-reading rule |
| `find_callers` CALLS-only / `referenceKind` / `kind: File` | 1,678 | field-reading rule |
| `files` | 1,162 | field-reading rule |
| `get_dependencies` `truncated`/`truncatedBy` | 1,247 | field-reading rule |
| which imports produce `get_dependencies` rows | 1,411 | field-reading rule |
| `search_code` / `noMatch` | 991 | field-reading rule |
| `find_implementations` `transitive` | 336 | duplicates the tool description |

## 1. Copies: one source, pinned mirrors, installed blocks that refresh

**Decision: one canonical text, `AGENTS_MD_SNIPPET`.** Everything else is either a
mirror pinned to it by a test, or an installed copy that `g-mesh init` can refresh.

| copy | role after GM-389 | what keeps it in sync |
|---|---|---|
| `AGENTS_MD_SNIPPET` | canonical | the new ceiling test (section 6) |
| README.md fenced block | mirror, kept inline because people who don't run `init` (a global `~/.claude/CLAUDE.md`) copy it from there | **new core test**: extract the fenced block after the "Reducing self-verification cost" heading and assert it equals `AGENTS_MD_SNIPPET` byte for byte. It is the same idea as the bench's drift guard, applied to the one copy nothing pinned |
| bench `GMESH_CONFIGURED_CLAUDE_MD` | mirror | existing drift guard in `armConfig.test.ts:457`; updated deliberately by a GMB task (section 8) |
| project `AGENTS.md` written by `init` | installed, legitimately ages | refresh on re-run (below) |
| a user's own `~/.claude/CLAUDE.md` | installed by hand, outside g-mesh's reach | README says "re-copy after an upgrade". Owner decision D3 |

**Refresh.** Today `ensure_agents_md` (`agent_instructions.rs:101`) returns
`Ok(false)` as soon as the file contains `BEGIN_MARKER`, so an upgraded g-mesh
never updates an installed block. Proposed behaviour:

1. BEGIN present exactly once and END present after it: replace the whole span
   `BEGIN..=END` with the current block. If the bytes are identical, it is a no-op.
2. No BEGIN: append, as today.
3. BEGIN without a following END, or more than one BEGIN: **refuse** with an
   error naming the file and the fix. Don't guess where the block ends, because
   a wrong guess deletes user text.

`Outcome.agents_md_written: bool` becomes a small enum
(`Created`/`Appended`/`Refreshed`/`Unchanged`), and `init.rs:318` prints
"refreshed the g-mesh block in AGENTS.md" for the new case.

Risks. (a) Hand edits *inside* the markers are overwritten. That is the contract,
so state it in the module doc and README ("edit outside the markers"). A `.bak`
would be safer, but it leaves litter in a repo. Not proposed. (b) A stale binary
run against a newer block would "refresh" it backwards. This is accepted: the
block is whatever the binary you run ships, the same rule the bench's drift
guard enforces. (c) The markers are already matched by exact string, so their
text must not change in this task. No block is installed on this machine today
(`grep -rl g-mesh:agents-md:begin` over ClaudeProjects finds none), so nothing
local needs migrating.

## 2. The 3,340-B catalogue: already in the prefix, except two points

Checked point by point against `core/src/mcp/mod.rs` (tool descriptions and param docs) and `instructions.rs`:

| catalogue point | already carried by | verdict |
|---|---|---|
| `get_file_outline` lists a file's symbols | `mod.rs:610` "List the top-level symbols a file declares, in source order." | cut |
| `find_definition` returns `source.text`, don't Read after; `include_source: false` | `mod.rs:533` "The response carries the declaration's own text, so a follow-up read of that file is usually unnecessary; pass include_source: false if you only want coordinates." | cut |
| not needed before the four tools; `anchor` gives the site | `instructions.rs:186` P5 "pass `symbol_name` directly to the four tools above instead of calling find_definition first"; `anchor` is in every response | cut |
| `find_references` = every usage | `mod.rs:552` "List every place a declared symbol ... is referenced, across the whole project." | cut |
| `find_callers`/`find_callees` | `mod.rs:566`, `mod.rs:579` | cut |
| `find_implementations`, `transitive` | `mod.rs:594` "Direct implementors/extenders only by default; pass `transitive: true`..." | cut (also the last 336-B bullet) |
| `get_dependencies` direction | `mod.rs:627` + `GetDependenciesParams.direction` doc "`Outgoing` for what this file imports, `Incoming` for what imports it." | cut |
| `search_code` is semantic, needs the embedding model | `mod.rs:651` | cut |
| **`search_code` first when no symbol name is given** | not carried | **add to `mod.rs:651` description**, one sentence (~95 B): "Use it first for a \"find the code that does X\" prompt that names no symbol." |
| ambiguous -> re-query by candidate `id`, not `qualifiedName` | `SymbolQueryParams.symbol_name` doc (`mod.rs:707`) says only "returns ranked candidates to re-call with" | **moves into the candidate page** (rule R3 below), not the schema |
| `resolvedBy` ladder, `semanticNeighbours` weakest | `semanticNeighbours` and `fileName` pages already carry an `explanation` (`find_definition.rs:560`, `:714`). Observed live in this session: `find_references("all_unresolved")` answered with "These are the closest declarations by meaning, not by name ... Check one before relying on it" | cut; `nameAmbiguous` gap covered by R3 |
| "Typical flow" bullet (347 B) | P5 + `anchor` | cut |

Budget: the schema gains ~95 B (10,163 -> ~10,258). There is no schema ceiling
today, and this task doesn't add one (see D6). `instructions` is untouched and stays at 1,804.

## 3. The field-reading rules: where each one goes

Rule for choosing between "every time" and "once per session", with both
kinds attached only when the triggering field value is present:

- A trigger seen in fewer than ~5% of calls fires **every time**. It is rare, so
  it costs little. Repeating it also survives a context compaction.
- A trigger seen more often fires **once per session**, on the first response that carries it.

**Precedents, stated accurately.** `provenance` (`core/src/mcp/provenance.rs`;
`find_references` found it referenced from `find_references.rs`,
`find_callers_callees.rs` and `find_implementations.rs`) and the existing
`hint`/`excludedReferences`/`noMatch`/`explanation` fields fire only when
relevant, every time. GM-385 (`4dd3112`) is "once per session" only because
`get_info`'s `instructions` are read once per session. That makes it prefix
text, not a response. **No response-side once-per-session mechanism exists yet.**
S3 adds one: a `SessionHints` set (`Arc<Mutex<HashSet<HintKey>>>`) on
`GMeshMcpServer` (`mod.rs:150`, one instance per connection via
`serve_connection`, `mod.rs:129`), passed to the handlers below. Risk: after
compaction the one-time sentence can be lost. That is accepted for the
frequent triggers only.

Frequencies come from the only saved transcripts with response bodies:
749 `gmesh-configured` runs, 1,354 g-mesh calls, 2026-08-05..09-20, mostly TS
corpora and older versions (`results/transcripts/*`). GMB-176's own
transcripts were not retained, and its records keep tool names/paths but not
bodies. So these numbers are indicative, and S5 recounts them on its own transcripts.

| # | rule (today's bullet) | trigger | seen | destination | frequency | response builders touched |
|---|---|---|---|---|---|---|
| R1 | complete = anchored + all `resolved: true` + no `allUnresolved` (1,389) | - | - | **stays in `instructions` P2/P3** (`instructions.rs:122`, `:128`), already said once per session; the snippet keeps one "trust a complete answer" sentence | - | none |
| R1a | `allUnresolved: true` -> check these rows, not the project | `allUnresolved == true` | 0/1354 | new `hint` sentence | every time | `ReferencePage` `find_references.rs:78` (built `:212`), `CallerPage` `find_callers_callees.rs:170` (built `:399`), `ImplementationPage` `find_implementations.rs:77` (built `:190`) |
| R1b | a `resolved: false` row | row flag | 0/1354 | no new text (P3 covers it) | - | none |
| R2 | ambiguous -> pick `id`, that page is final, don't reconfirm (780 + catalogue) | candidate page (`resolvedBy: nameAmbiguous`) | 44/1354 (3.2%) | add `explanation` to `CandidatePage`, as `FileNamePage` already has | every time | `CandidatePage` `find_definition.rs:147` (built `:435` in `resolve_symbol_name`, `:399`), which the four symbol tools return through `anchor::resolve` |
| R3 | `resolvedBy` ladder (catalogue) | `semanticNeighbours`/`fileName` | 33/1354 | **already in the response** (`find_definition.rs:560`, `:714`) | every time | none |
| R4 | `find_callers` is CALLS-only; use `find_references` instead for exhaustive lists (part of 1,678) | REFERENCES edges excluded | 60/1354 | **already in the response**: `excludedReferences.hint`, `EXCLUDED_REFERENCES_HINT` `find_callers_callees.rs:270` | every time | none |
| R5 | a `kind: File` row is a file-level answer (rest of 1,678) | a row with `kind: File` | 178/1354 (13%) | one sentence | once per session | `ReferencePage` (`find_references.rs:212`), `CallerPage` (`:399`) |
| R6 | `files` covers the whole edge set, answer "which files" from it (1,162) | `files` present | 189/1354 (14%) | one sentence | once per session | `ReferencePage.files` `find_references.rs:92`, `CallerPage.files` `find_callers_callees.rs:187` |
| R7 | `truncated: false` is complete for the depth asked; `max_depth: 1` = every direct importer; don't re-derive with a `from '...'` grep (1,247 + 1,411) | any `get_dependencies` response with `truncated: false` | 32/1354 (2.4%) of calls; 11 of 186 in GMB-176 | one sentence; the `import type` clause only for a TS/JS anchor (GMB-163: elsewhere a module is not a file). A static prefix could not make that distinction | once per session | `DependencyWalk` `get_dependencies.rs:126` (built in `bound_walk` `:256`, `:270`, `:301`) |
| R8 | `truncated: true` -> follow `truncatedBy` (`maxDepth` -> `frontierNodes`, `maxFanout` -> page that node, `explorationBudget`/`responseSize` -> `resumeToken`) | `truncated == true` | 6/1354 | one sentence keyed by `truncatedBy` | every time | `DependencyWalk` (above), `TransitiveImplementationWalk` `find_implementations.rs:256` (built `:344`, `:382`) |
| R9 | `hasMore` | `hasMore == true` | 366/1354, **320 of them `search_code`, which always says `true`** | **no response text**. A hint keyed on it would fire on every `search_code`. "Raise `limit`" is already in the `limit` param doc and P5, and "`hasMore: false` is exhaustive for bare calls" is in P4 | - | none |
| R10 | `search_code`: one confirming read, then stop; no reworded re-queries (991) | first `search_code` page with hits and no `noMatch` | 320/1354 | one sentence (`noMatch` is **already** explained, `similarity.rs:227`/`:234`) | once per session | `SearchPage` `search_code.rs:84` (built `:174`) |

**How these were found (g-mesh calls).** `find_references("provenance")`
returned the three files above. `find_references("ResolvedBy")` returned
`find_definition.rs` x5, `anchor.rs` x2 and `find_implementations.rs` x1, with `files` present.
`find_definition("ResolvedBy")` gave the ladder and its doc.
`get_file_outline("core/src/mcp/mod.rs")` gave the server struct, handlers and
param types. `search_code` gave the candidate-page tests and `ResolvedBy`.
**Where g-mesh could not answer:** `find_references("all_unresolved")` and
`find_references("truncated_by")` came back `resolvedBy: semanticNeighbours`
("Nothing is named ..."), because Rust struct *fields* are not indexed as symbols.
So the per-field builder lines above come from one `grep -nE` over `core/src/mcp`
for the serde field declarations, and a second grep for the struct literals.
`search_code` for "static receiver sentence once per session" didn't find GM-385.
`git log`/grep located it in `instructions.rs`.

**Cost side.** Prefix text is paid on every turn of every session, including sessions that never call g-mesh (GMB-157's `tt-diag-local-literal-severity`: zero g-mesh calls, 1.72x cost).
Response text is paid from the turn it arrives onward, and only in sessions
that trigger it. Upper bound for R1a-R10 in one run: four once-per-session sentences
(about 150 B each) plus rare every-time ones, roughly 600-800 B, carried for the
remaining turns only. The every-time ones average about 13 B per run across the
749 runs (R2: 44 x ~200 B; R8: 6 x ~150 B). Against that, the snippet sheds
11,695 B on every turn.

## 4. The 1,190 prefix-shaped bytes stay

"Prefer g-mesh over grep for X" (876 B after de-scoping) and "No manual indexing
command" (303 B) are knowledge needed *before* the first call. They stay, verbatim.
The one exception is the GMB-165 de-scoping of the first bullet's opener
("- In TS/JS projects, prefer g-mesh (" -> "- Prefer g-mesh ("), which is exactly the span
GMB-165 measured, 5/16 -> 16/16 runs calling g-mesh on psf/requests. They are not
reworded, so that this task's A/B varies the cut and not these two bullets.

## 5. GM-420 content, placed

| item | where | why there |
|---|---|---|
| all plugin languages | snippet heading, static: "TypeScript/JavaScript, Rust, Python, Go" (`plugins/` bundles exactly typescript, rust, python, go) | GMB-165 showed the scope line changes tool use (5/16 vs 16/16, hardened scope 0/6). A heading derived at `init` time from installed plugins would make the bench's byte pin plugin-dependent. S2 adds a test that each bundled plugin manifest's language appears in the heading, so a fifth plugin fails CI instead of being silently excluded |
| `select_project` first | snippet, one bullet, combined with "tools may be deferred; load them" | the front server already says it in its `instructions` and refusal (`front.rs:132`). The snippet line pays for the deferred-tools half, which the server cannot say before it is loaded |
| delegation | snippet, one bullet | subagents do not inherit CLAUDE.md-level text they are not briefed with (GM 3.13.0: 41 agents, 0 calls, 144 greps) |
| worktrees | snippet, one bullet | must be known before the first call. No response can tell which checkout the caller is standing in |
| refresh | `init` behaviour + README (section 1), **not** in the snippet | agents don't run `init`, people do |

## 6. Ceiling: `AGENTS_MD_SNIPPET_BYTE_CEILING = 2560`

Placed in `core/src/cli/agent_instructions.rs` beside the snippet, with a doc
line cross-referencing `INSTRUCTIONS_BYTE_CEILING`.
Test `agents_md_snippet_fits_its_ceiling` asserts
`AGENTS_MD_SNIPPET.len() <= AGENTS_MD_SNIPPET_BYTE_CEILING`. Its control: append 400 B in a worktree and the test fails.

Why 2,560:
- The draft is 2,231 B, which leaves 329 B of headroom, about one short bullet.
  Anything larger than one bullet is a behaviour change that needs a measurement
  anyway (GMB-165: one span moved tool use in 11 of 16 runs). The ceiling forces that
  conversation instead of letting text accrete, which is how 13,926 B happened.
- Combined with the instructions cap, the guidance g-mesh itself controls is capped at
  1,900 + 2,560 = 4,460 B. At GMB-176's 3.6 B/token that is about 1,240 tokens per turn,
  against about 4,400 today (13,874 + 1,804 B).
- It is not the 1,900 figure, because that one comes from Claude Code's 2 KB
  truncation of `instructions`. `CLAUDE.md`/`AGENTS.md` has no truncation, so its
  bound is a cost decision rather than a transport limit, and the number says so.

## 7. Draft snippet (2,231 B by `wc -c`, down from 13,926: -84%)

```markdown
# Code search (TypeScript/JavaScript, Rust, Python, Go projects)

- Prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact analysis, ambiguous naming (same symbol name declared in different scopes/files), and call-graph/multi-hop questions (callers, implementations, transitive dependencies) — grep can't resolve these reliably and has real unbounded cost (many round-trips, occasionally very expensive) when it tries. For simple, unambiguous single-symbol lookups, grep/`Explore`/manual reading is often just as fast and cheaper — g-mesh's tool schema adds fixed overhead per turn that doesn't pay for itself on easy questions (measured: g-mesh costs *more* tokens than grep on simple lookups, both isolated and in a long session — see `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh returns no result, errors, or the target isn't something it tracks (non-code files, config, CSS, etc.).
- No manual indexing command exists or is needed. The g-mesh daemon bootstraps and indexes a project automatically on its first tool call in that project's directory. On first use in a new project, just issue any g-mesh call (e.g. `get_file_outline` on a source file) to trigger indexing, then proceed.
- When the g-mesh server covers a folder of several projects, call `select_project` first. In Claude Code its tools may be deferred: load them (ToolSearch) before the first call.
- Trust a complete answer from the structural tools (`find_*`, `get_dependencies`). A response says when it is not complete or not exact (`hasMore`, `truncated`, `allUnresolved`, a `resolved: false` row, a `resolvedBy` other than `id`/`qualifiedName`/`name`), and its `hint`/`explanation` says what to do next. Absent those, do not re-check it with grep or Read: that re-verification is the most expensive habit these tools have.
- The index serves the checkout it was built on. In a `git worktree` on another branch, trust g-mesh for code the branch has not changed and read the changed files directly.
- When delegating, put this section in the subagent's brief: a subagent does not inherit it, and it may need to load the g-mesh tools too. grep is still right there for one known symbol or for non-code.
```

Bullets and bytes: heading 65, prefer 876, indexing 303, select_project 179, trust 430, worktrees 174, delegation 203.

## 8. A/B plan (S5)

**Method:** GMB-176's own. `token-economy` harness, the 27 Go/Rust/Python tasks,
REPS=normal (3), corpora serial, arm concurrency 2, `G_MESH_BENCH_SAVE_TRANSCRIPTS=yes`
(needed to count re-verification and hint firing exactly), explicit
`G_MESH_BENCH_CORE_IDLE_TIMEOUT_MS`. REPS=low is not used for any before/after number.

**Baseline file** `results/token-economy/2026-09-22T13-45-40-293Z.json` (3.10.0), recomputed today:

| arm | n | tokens/run | turns | cacheRead/turn | oracle | g-mesh calls | grep after a structural result* |
|---|---|---|---|---|---|---|---|
| gmesh-configured | 81 | 108,500 | 5.51 | 18,113 | 79/81 | 186 | 53 |
| baseline | 81 | 81,798 | 6.70 | 11,006 | 77/81 | 0 | - |

*A proxy from `toolResults` order: a Grep/Glob/`grep`-in-Bash call anywhere after a `find_*`/`get_dependencies` call in the same run.

**Why GMB-176 is the anchor, not the control arm.** It ran 3.10.0. Diffing a
3.16+ treatment against it would fold six releases into "the trim". So both
arms run fresh, and GMB-176 serves as the sanity reference:

- **Invocation A (control):** `release-3.16.0` binary with the current 13,874-B de-scoped snippet. The bench runs from a worktree at its pre-GMB-task commit, with `G_MESH_BENCH_REPO` pointing at a g-mesh worktree on `release-3.16.0` so the drift guard still passes. Arms: `gmesh-configured`, `baseline`.
- **Invocation B (treatment):** the GM-389 S2+S3 binary with the new snippet, and the bench at the GMB task's commit. Arms: `gmesh-configured`, `baseline`.
- The baseline arm runs twice, which gives a free run-to-run noise figure. If either
  baseline arm is far from GMB-176's 81,798, the environment drifted, and the run is
  investigated before any conclusion.

**Cheap distinguishing observation, first (one run, about $0.40, about 2 min):** one task
(`gin-find-impl-render`), REPS=low, `gmesh-configured` under A and under B.
Expected: per-turn `cacheRead` lower by about 11,700 B / 3.6 = about 3,250 tokens (turn 1's
`cacheCreation` too). If it does not move by at least 2,500, the new snippet
did not reach the arm, and the full run is not spent.

**Reported per arm, always together:** tokens/run (mean, and per-task median with
range-separation as GMB-176 used), turns/run, oracle x/81, g-mesh calls/run, and
re-verification turns. Re-verification turns are counted exactly from transcripts
with `fields.py`-style logic: a Grep/Glob/Bash-grep, or a Read of a file already
in a `find_definition` `source`, directly after a *complete* structural result.
Also report how often each R-hint fired.

**Decision rule.** Ship the trim only if tokens/run drop, oracle is not lower,
g-mesh calls/run do not fall (the GMB-165 suppression failure mode), and neither
turns nor re-verification turns rise beyond the two baseline arms' spread. A
token drop bought with correctness or extra turns is reported as such, not as a saving.

**Estimate.** GMB-176 took **53m03s** of wall time and cost $10.00 in arms, $0.91 for the judge and $0.01 for the narrative
(its run log, quoted in the write-up). Two invocations come to **about 1h50m and about $22**.
Treatment should come in lower, since the smaller prefix is re-read on every turn. The cheap check adds about $0.40.
Record `uptime` and `/usr/bin/time -p` with each invocation.

**Bench pin (a GMB task in g-mesh-bench, created in S5, before invocation B):**
copy the new `AGENTS_MD_SNIPPET` into `GMESH_CONFIGURED_CLAUDE_MD` and state the
new byte count in the commit. Delete `descopeGuidance` and its TS-span constants
(`armConfig.ts:350-...`): the new heading has no TS-only span, so `descopeGuidance`
would throw by design, and its own error message says to delete it.
`gmeshConfiguredClaudeMd` then returns one document for every language, and
`GMESH_MAP_CONFIGURED_CLAUDE_MD` follows. The de-scoping tests in
`armConfig.test.ts` go with it. Historical TS byte-identity is lost, deliberately, and the GMB write-up says so.

## 9. Slices after this one

- **S2** (implement): new snippet, ceiling + test, README mirror test, plugin-language
  test, `init` refresh (enum outcome, refuse on broken markers), `search_code`
  description sentence. Controls: oversized snippet fails; README edit fails;
  refresh over an old block replaces only the marked span (text before and after preserved);
  a missing END refuses.
- **S3** (implement): `SessionHints` plus R1a, R2, R5-R8 and R10, with a test per rule: the
  sentence is present when its trigger is, absent otherwise, and once-per-session
  rules appear exactly once across two calls on one server.
- **S4** verify, **S5** measure, as sliced.

## 10. Decisions for the owner

- **D1** Ceiling 2,560 (section 6), or tighter?
- **D2** README keeps an inline copy pinned by a test (proposed), or points to the source?
- **D3** Your own `~/.claude/CLAUDE.md` (2,983 B, unmeasured): replace it by hand with the new snippet after S5, or keep your version? g-mesh has no command that writes there. A `--file <path>` refresh target is possible but not proposed.
- **D4** A/B with a fresh control invocation (about $22, 1h50m, proposed), or treatment only against the GMB-176 file (about $11, confounded by 3.10 -> 3.16)?
- **D5** TS corpora are not in GMB-176's set, yet TS users' text changes most (the heading). Add a TS spot-check (about 6 tasks, REPS=normal, about $5)?
- **D6** No schema byte ceiling in this task. File one separately?
