# GM-389: lighter guidance prefix, A/B result

Method per `docs/architecture/gm-389-guidance-prefix.md` section 8: g-mesh-bench
`token-economy`, REPS=normal (3), arm concurrency 2, transcripts saved, arms
`gmesh-configured` and `baseline`, model `claude-sonnet-5`, Claude Code **2.1.283**
in both invocations (every one of A's 198 and B's 462 transcripts records
`claude_code_version: 2.1.283`; B ran through a PATH shim with
`DISABLE_AUTOUPDATER=1` because the machine had auto-updated to 2.1.284).

| | A (control) | B (treatment) |
|---|---|---|
| g-mesh | `release-3.16.0` binary, 13,926-B snippet (de-scoped in the bench to 13,874 B) | `feat/GM-389-lighter-guidance-prefix` at 608e4dc (release build, cargo fingerprint up to date), snippet 2,231 B + response-side hints (S6/S7) |
| bench | worktree at 4b7cf43 (pre-GMB-183) | `chore/GMB-183-pin-gm389-snippet` at 25742bd |
| result | `results/token-economy/2026-09-28T16-04-36-661Z.json` (bench-wt-ab-control) | `results/token-economy/2026-09-28T18-23-33-931Z.json` (bench-wt-gmb183) |
| transcripts | `results/transcripts/2026-09-28T16-05-02-109Z` | `results/transcripts/2026-09-28T18-23-51-034Z` |
| records | 198 (33 tasks x 2 arms x 3) | 462 (77 tasks x 2 arms x 3); **the 198 on A's 33 tasks are compared** |

**Task-set note.** B's rerun used the bench's default task selection, which in the
GMB-183 worktree is 77 tasks, not A's 33 (27 Go/Rust/Python + 6 TS: 3 excalidraw,
3 task-tracker-mcp). A's invocation had narrowed the set in a way its result file
does not record. All 33 of A's tasks exist in B with an **identical `taskDefHash`**,
and each (task, rep) runs as its own fresh session, so the comparison below uses
B's 198 records on exactly A's tasks. The extra 44 tasks cost about $28 and about 2h of
wall time and are not used for any figure here. The difference between
the invocations is only the interleaving order and the machine's load.

**Machine (B).** `uptime` at start 21:23, up 29 min, load 3.87 4.72 15.46; at end 00:46, load 8.16 5.04 4.13.
`/usr/bin/time -p`: real 12193.12, user 3308.22, sys 871.35 (the whole 462-record run;
real far above user+sys is the harness waiting on the API, as expected).
Spend: arms $38.63, judge $2.99, narrative $0.02 (the 198 comparable records: arms $10.33).
A's machine state was recorded by the earlier agent, whose session ended at the 20:54 reboot, and is not reproduced here.

## Cheap distinguishing observation (section 8), first

`gin-find-impl-render`, REPS=low, `gmesh-configured`: control
`2026-09-28T15-59-48-494Z.json`, treatment `2026-09-28T16-02-08-206Z.json`.

| | turn-1 cacheCreation | turn-2 cacheRead | per-API-call cacheRead series |
|---|---|---|---|
| control | 22,139 | 22,139 | 0, 22,139, 22,420, 23,263, 27,277, 30,680 |
| treatment | 18,268 | 18,268 | 0, 18,268, 19,140, 21,661, 23,582, 26,433, ... |

The prefix moved by **3,871 tokens** (>= 2,500 required; predicted about 3,250). The new
snippet reached the arm, so the full run was worth spending.

## Results, 27 Go/Rust/Python tasks (n = 81 per arm per invocation, all `ok`)

| | A gmesh | B gmesh | A baseline | B baseline |
|---|---|---|---|---|
| tokens/run (mean) | 114,333 | **98,727 (-13.6%)** | 75,734 | 75,384 (-0.5%) |
| input / output | 9 / 1,567 | 9 / 1,525 | 9 / 1,665 | 9 / 1,703 |
| cacheRead / run | 105,363 | 91,167 | 68,287 | 67,571 |
| cacheCreation / run | 7,394 | 6,026 | 5,772 | 6,101 |
| cacheRead per turn | 17,597 | **15,984** | 10,740 | 10,774 |
| turns/run | 5.99 | **5.70** | 6.36 | 6.27 |
| oracle | 80/81 | **78/81** | 79/81 | 75/81 |
| g-mesh calls (total, per run) | 190, 2.35 | **136, 1.68 (-28%)** | 0 | 0 |
| complete structural results | 130 | 95 | - | - |
| re-verification turns | 17 (0.21/run) | **13 (0.16/run)** | - | - |
| re-verification per complete result | 13.1% | 13.7% | - | - |
| grep after a structural call (GMB-176 proxy) | 87 | 59 | - | - |

Per task (median of 3 reps), `gmesh-configured`: B below A on 21 of 27 tasks, ranges
fully separated downward on 9 and upward on 1, median per-task B/A ratio **0.831**.
The same statistic on `baseline` is 12 of 27 lower, 2 down and 2 up, ratio 1.001: the
baseline arm did not move, so the environment held.

Sanity against GMB-176 (3.10.0): baseline 75,734 / 75,384 against 81,798 (-7%, both
invocations agree with each other to 0.5%); A's gmesh arm 114,333 against 108,500 (3.16.0
carries more text than 3.10.0 did).

## TS spot-check, 6 tasks (n = 18 per arm per invocation)

| | A gmesh | B gmesh | A baseline | B baseline |
|---|---|---|---|---|
| tokens/run | 78,887 | **63,254 (-19.8%)** | 58,435 | 56,262 |
| cacheRead per turn | 21,414 | 16,420 | 8,395 | 10,981 |
| turns/run | 3.33 | 3.56 | 6.39 | 4.78 |
| oracle | 18/18 | 18/18 | 16/18 | 14/18 |
| g-mesh calls/run | 1.67 | 1.39 | 0 | 0 |
| re-verification turns | 2 | 3 | - | - |

Per task: B lower on 5 of 6, 4 separated downward, none upward, median ratio 0.783.

## Hint firing (B, `gmesh-configured`, count of hint sentences in g-mesh responses)

| rule | 27 tasks | TS 6 | total |
|---|---|---|---|
| R1a `allUnresolved` | 0 | 0 | 0 |
| R2 ambiguous / candidates | 15 | 6 | 21 |
| R5 `kind: File` row | 9 | 1 | 10 |
| R6 `files` tally | 26 | 9 | 35 |
| R7 `truncated: false` walk complete | 7 | 3 | 10 |
| R8 `truncatedBy` | 0 | 0 | 0 |
| R10 search hits | 5 | 3 | 8 |

A carries none of these sentences (0 for every rule, as expected). R1a and R8 never
triggered on these tasks, so their effect is unmeasured here.

## Where the g-mesh calls went (both scopes, `gmesh-configured`, from transcripts)

| tool | A | B | delta |
|---|---|---|---|
| `find_definition` | 90 | 54 | **-36** |
| `find_references` | 41 | 19 | **-22** |
| `get_dependencies` | 17 | 13 | -4 |
| `find_callers` | 33 | 35 | +2 |
| `find_implementations` | 13 | 14 | +1 |
| `find_callees` / `search_code` / `get_file_outline` | 8 / 9 / 9 | 9 / 8 / 9 | 0 |
| `Read` | 99 | 133 | **+34** |
| `Grep` / `Glob` | 116 / 11 | 119 / 14 | +3 / +3 |

The drop is concentrated in `find_definition` and `find_references`; the call-graph
tools are unchanged. `Read` rose by almost exactly the `find_definition` drop: without the
old catalogue line "`find_definition` returns the source in `source.text`", agents read the
file instead of asking for the definition. It is still cheaper overall (the prefix saving dominates),
but it is a substitution of g-mesh by Read, not only the removal of redundant calls.

## Oracle failures

- A gmesh: `py-references-httpbasicauth` rep3.
- B gmesh: `rs-callers-sink-matched` rep3 (A 3/3), and **`rs-deps-json-printer-workspace-crates` rep2 and rep3** (A 3/3).
  In A all three reps followed the complete `get_dependencies` answer with a grep (a
  counted re-verification turn) and answered correctly. In B, rep1 did the same and passed; reps 2 and 3 made one
  `get_dependencies` call, trusted it, and listed `grep-regex`, which `json.rs` imports only
  in test code. That is the trust rule doing what it says on a complete answer whose
  scope (tests included) differs from the question's. This is a real cost of the trim, not noise.
- Baselines: A 4/99 failures, B 10/99, on identical inputs. Oracle moves by up to 4/81 between
  two invocations of an unchanged arm.

## Decision rule (section 8)

| criterion | result | met |
|---|---|---|
| tokens/run drop | -13.6% (27 tasks), -19.8% (TS), per-task ratio 0.83 / 0.78, baseline flat | **yes** |
| oracle not lower | 80/81 -> 78/81 (27 tasks), 18/18 -> 18/18 (TS) | **no** (-2; within the 4/81 baseline spread, but 2 of the 3 failures have a mechanism, above) |
| g-mesh calls/run do not fall | 2.35 -> 1.68 (-28%) | **no** (`find_definition` and `find_references` replaced by `Read`) |
| turns do not rise beyond the baseline spread | 5.99 -> 5.70 (baseline spread 0.09) | yes (fell) |
| re-verification turns do not rise beyond the spread | 17 -> 13; per complete result 13.1% -> 13.7% | yes |

**Verdict: the rule as written is not met.** Tokens drop clearly (about 15,600 per run, 13.6%)
with fewer turns and no rise in re-verification, but two of the rule's guards fail:
g-mesh calls/run fell 28% (the GMB-165 failure mode the rule guards against, here as
`find_definition` -> `Read`), and oracle is 2 lower, of which 2 failures trace to the trust
rule on a test-scoped dependency question. By the rule's own wording this is a token drop that
came with fewer g-mesh calls and slightly lower correctness, and it is reported as such, not as a clean saving.
Whether to ship anyway (for example, restoring the one-line `find_definition`-returns-source note,
about 100 B under the 2,560-B ceiling, and re-measuring) is the owner's decision.

## D3: owner's `~/.claude/CLAUDE.md` "Code search" section vs the new `AGENTS_MD_SNIPPET`

Read only; `~/.claude/CLAUDE.md` was not edited. The owner's section is 2,983 B (hand-trimmed),
the new snippet 2,231 B. Both share the heading. The owner's copy carries the tool catalogue and the
field-reading rules that the snippet moved into responses (S7). The snippet carries the
verbatim "Prefer" and "indexing" bullets, plus the delegation bullet the owner's copy lacks.
`-` = owner's section, `+` = new snippet:

```diff
@@ -1,14 +1,8 @@
 # Code search (TypeScript/JavaScript, Rust, Python, Go projects)
 
-- **First call `select_project`** with the project's name; its tools are deferred, so load them with ToolSearch before that. The index serves the main checkout: in a `git worktree` on another branch, trust g-mesh for code the branch has not changed and read the changed files directly.
-- Prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact, ambiguous names, and call-graph/multi-hop questions. For simple single-symbol lookups grep/`Explore` is cheaper (measured in `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh errors, returns nothing, or the target is not code.
-- No indexing command: the first g-mesh call in a project indexes it.
-- Tools: `get_file_outline` (symbols of a file); `find_definition` (returns the source in `source.text`: do not Read the file after it unless `source.omittedLines`); `find_references`, `find_callers`/`find_callees`, `find_implementations` (take `symbol_name` directly; their `anchor` gives the declaration, so call `find_definition` first only when you expect ambiguity); `get_dependencies` (import graph); `search_code` (semantic; the first move on "find the code that does X" with no symbol name).
-- Ambiguity: `ambiguous: true` or `resolvedBy` of `nameAmbiguous`/`fileName`/`semanticNeighbours` means candidates. Re-query by candidate `id`, never `qualifiedName`. `semanticNeighbours` is weakest: check the candidate.
-- **Trust complete results; do not re-verify them with grep/Read.** A page anchored by `symbol_id` or an unambiguous name, all rows `resolved: true`, no `allUnresolved`, is complete. Check only `resolved: false` rows, `allUnresolved: true`, and what the tool does not cover (method calls through a variable receiver, other same-named symbols).
-- `find_callers` sees only calls inside named functions; calls at top level or in inline callbacks (test `it(...)` bodies) are `REFERENCES`. For an exhaustive list (rename, removal, tests) use `find_references` instead of, not as well as, `find_callers`. A `kind: File` row is a whole-file usage and already answers "which files".
-- For "which files are affected", answer from the `files` array: it covers the whole edge set, not just the page. Absent `files` means the rows are already one per file.
-- `get_dependencies`: `truncated: false` is complete for the depth asked (`max_depth: 1` = all direct importers/imports, including `import type`). On `truncated: true` follow `truncatedBy`: `maxDepth` → re-anchor on `frontierNodes`; `maxFanout` → page that node with single-hop tools; `explorationBudget`/`responseSize` → `resumeToken`. Do not re-derive importers with a `from '...'` grep. Rows carry no bound names or line: Read the one file when needed.
-- `search_code`: once a hit plausibly matches, one confirming read, then stop. No reworded re-queries, no grep sweep.
-- `find_implementations` is direct-only; `transitive: true` for the whole hierarchy.
-
+- Prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact analysis, ambiguous naming (same symbol name declared in different scopes/files), and call-graph/multi-hop questions (callers, implementations, transitive dependencies) — grep can't resolve these reliably and has real unbounded cost (many round-trips, occasionally very expensive) when it tries. For simple, unambiguous single-symbol lookups, grep/`Explore`/manual reading is often just as fast and cheaper — g-mesh's tool schema adds fixed overhead per turn that doesn't pay for itself on easy questions (measured: g-mesh costs *more* tokens than grep on simple lookups, both isolated and in a long session — see `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh returns no result, errors, or the target isn't something it tracks (non-code files, config, CSS, etc.).
+- No manual indexing command exists or is needed. The g-mesh daemon bootstraps and indexes a project automatically on its first tool call in that project's directory. On first use in a new project, just issue any g-mesh call (e.g. `get_file_outline` on a source file) to trigger indexing, then proceed.
+- When the g-mesh server covers a folder of several projects, call `select_project` first. In Claude Code its tools may be deferred: load them (ToolSearch) before the first call.
+- Trust a complete answer from the structural tools (`find_*`, `get_dependencies`). A response says when it is not complete or not exact (`hasMore`, `truncated`, `allUnresolved`, a `resolved: false` row, a `resolvedBy` other than `id`/`qualifiedName`/`name`), and its `hint`/`explanation` says what to do next. Absent those, do not re-check it with grep or Read: that re-verification is the most expensive habit these tools have.
+- The index serves the checkout it was built on. In a `git worktree` on another branch, trust g-mesh for code the branch has not changed and read the changed files directly.
+- When delegating, put this section in the subagent's brief: a subagent does not inherit it, and it may need to load the g-mesh tools too. grep is still right there for one known symbol or for non-code.
```

## Reproducing

Analysis: `ab.py` (in the S9 agent's scratchpad; logic in brief). Tokens = input + output + cacheRead + cacheCreation;
cacheRead per turn = sum cacheRead / sum `numTurns`. A *complete structural result* is a
`find_*`/`get_dependencies` response that parses, has no `error`, `ambiguous`, `allUnresolved`,
`truncated: true` or `hasMore`, no `resolved: false` row, and no anchor `resolvedBy` of
`nameAmbiguous`/`fileName`/`semanticNeighbours`. A *re-verification turn* is the next assistant
message after such a result containing a Grep, a Glob, a Bash grep/rg/find, or a Read of the file a
`find_definition` result named. Hint counts are substring counts of each rule's sentence in g-mesh tool results.
