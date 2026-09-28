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

## B2: `find_definition` bullet restored

The owner asked for one re-measurement with the one-line note restored:
"`find_definition` returns the declaration's source in `source.text`: do not Read the file
after it unless `source.omittedLines` says it was cut." (g-mesh 4abe6c8, snippet **2,378 B**).
Everything else in B is unchanged.

| | B2 |
|---|---|
| g-mesh | `feat/GM-389-lighter-guidance-prefix` at 4abe6c8, `cargo build --release -p g-mesh` rerun (it relinked), `G_MESH_BENCH_BINARY` pointed at it, `g-mesh 3.16.0` |
| bench | `chore/GMB-183-pin-gm389-snippet` at dd952b7; the drift test passes against this g-mesh worktree (`G_MESH_BENCH_REPO`) |
| task set | **A's 33 task ids passed on the command line** (`npm run token-economy -- <ids>`, the harness's only task filter): "Running 33 of 79 tasks", 198 records, all `ok`; `taskDefHash` identical to A for all 33 |
| result | `results/token-economy/2026-09-28T22-01-18-232Z.json` (bench-wt-gmb183) |
| transcripts | `results/transcripts/2026-09-28T22-01-40-962Z`, 198 files, every one `claude_code_version: 2.1.283` |
| machine | start 01:01, load 71.08 75.62 39.24 (a parallel verify agent's cargo/npm tests); end 01:56, load 4.87 4.62 5.10; `time -p` real 3296.21, user 687.99, sys 208.07 |
| spend | arms $10.27, judge $1.77 |

Figures below come from a rebuilt analysis (`ab2.py`, same definitions as *Reproducing*). It reproduces
this document's A and B1 tokens, turns, oracle, per-tool counts and per-task ratios exactly; its
complete-result and re-verification counts come out 1-2 lower than S9's for A and B1 (128/16 and 94/12
against 130/17 and 95/13), so those two rows are given from the rebuilt script for all three columns.

### 27 Go/Rust/Python tasks (n = 81 per arm per invocation)

| | A gmesh | B1 gmesh | **B2 gmesh** | A base | B1 base | B2 base |
|---|---|---|---|---|---|---|
| tokens/run | 114,333 | 98,727 | **98,507 (-13.8%)** | 75,734 | 75,384 | 69,826 |
| cacheRead / run | 105,363 | 91,167 | 90,898 | 68,287 | 67,571 | 62,176 |
| cacheCreation / run | 7,394 | 6,026 | 6,124 | 5,772 | 6,101 | 6,011 |
| cacheRead per turn | 17,597 | 15,984 | **15,119** | 10,740 | 10,774 | 10,113 |
| turns/run | 5.99 | 5.70 | **6.01** | 6.36 | 6.27 | 6.15 |
| oracle | 80/81 | 78/81 | **79/81** | 79/81 | 75/81 | 76/81 |
| g-mesh calls (total, per run) | 190, 2.35 | 136, 1.68 | **179, 2.21 (-6%)** | 0 | 0 | 0 |
| `find_definition` / `find_references` | 82 / 34 | 48 / 18 | **79 / 21** | - | - | - |
| `find_callers` / `find_implementations` / `get_dependencies` | 27 / 13 / 11 | 23 / 14 / 10 | 33 / 15 / 9 | - | - | - |
| `find_callees` / `search_code` / `get_file_outline` | 8 / 6 / 9 | 9 / 5 / 9 | 7 / 6 / 9 | - | - | - |
| `Read` calls | 97 | 127 | **117** | 157 | 175 | 166 |
| `Grep` / `Glob` | 110 / 7 | 111 / 7 | 99 / 11 | 264 / 13 | 243 / 9 | 238 / 13 |
| complete structural results | 128 | 94 | 116 | - | - | - |
| re-verification turns (per complete result) | 16 (12.5%) | 12 (12.8%) | **12 (10.3%)** | - | - | - |
| grep after a structural call | 87 | 59 | 65 | - | - | - |

Per task (median of 3 reps), B2 against A: `gmesh-configured` lower on 21 of 27, 9 separated downward,
0 upward, median ratio **0.836**; `baseline` 13 of 27 lower, 3 down, 1 up, ratio 1.000. B2 against B1:
`gmesh-configured` ratio 1.002 (13 lower, 0 down, 5 up), `baseline` 0.999. The bullet cost no measurable
tokens against B1; the prefix saving is intact. B2's baseline mean is 7.8% below A's while its per-task
median ratio is 1.000: a few high-variance tasks moved the mean, not the environment.

### TS spot-check, 6 tasks (n = 18 per arm per invocation)

| | A gmesh | B1 gmesh | **B2 gmesh** | A base | B1 base | B2 base |
|---|---|---|---|---|---|---|
| tokens/run | 78,887 | 63,254 | **69,414 (-12.0%)** | 58,435 | 56,262 | 54,855 |
| cacheRead / cacheCreation per run | 71,381 / 6,796 | 58,382 / 4,091 | 63,591 / 5,051 | 53,637 / 3,570 | 52,463 / 2,811 | 50,311 / 3,587 |
| cacheRead per turn | 21,414 | 16,420 | 17,084 | 8,395 | 10,981 | 11,320 |
| turns/run | 3.33 | 3.56 | 3.72 | 6.39 | 4.78 | 4.44 |
| oracle | 18/18 | 18/18 | 18/18 | 16/18 | 14/18 | 16/18 |
| g-mesh calls (total, per run) | 30, 1.67 | 25, 1.39 | **31, 1.72** | 0 | 0 | 0 |
| `find_definition` / `find_references` / `find_callers` / `get_dependencies` / `search_code` | 8 / 7 / 6 / 6 / 3 | 6 / 1 / 12 / 3 / 3 | 9 / 3 / 11 / 3 / 5 | - | - | - |
| `Read` calls | 2 | 6 | 1 | 12 | 8 | 9 |
| complete results / re-verification turns | 16 / 2 | 16 / 3 | 20 / 3 | - | - | - |

Per task B2 against A, `gmesh-configured`: lower on 4 of 6, 4 separated downward, 1 upward, median ratio 0.790.

### Did the `find_definition` -> `Read` substitution reverse?

Mostly. On the 27 tasks `find_definition` went 82 (A) -> 48 (B1) -> **79** (B2) and `Read` 97 -> 127 -> **117**;
both scopes together, `find_definition` 90 -> 54 -> 88 and `Read` 99 -> 133 -> 118. `Read` stays 20 above A, but
the baseline arm's own `Read` count moves by 18 between invocations of an unchanged arm (157 / 175 / 166), so the
remainder is not distinguishable from noise. What did not come back is `find_references`: 34 -> 18 -> 21 on the 27
tasks (41 -> 19 -> 24 both scopes). Its -13 is more than B2's whole remaining g-mesh-call deficit (179 against 190,
-11), and it is a change of habit the restored bullet does not address.

### `rs-deps-json-printer-workspace-crates`, per rep (`gmesh-configured`)

| rep | A | B1 | B2 |
|---|---|---|---|
| 1 | pass (`get_dependencies`, then grep) | pass (same) | pass: `get_dependencies`, `Grep ^use` + Read of the first 25 lines, `Grep grep_regex`; excluded `grep-regex` as test-only |
| 2 | pass (same) | **fail**: trusted `get_dependencies`, listed `grep-regex` | pass: `get_dependencies`, one `Grep` for the three crate names; listed `grep-regex` marked "only in `#[cfg(test)]`" |
| 3 | pass (same) | **fail** (same as rep 2) | pass: `get_dependencies`, two `Grep`s; same answer as rep 2 |

All three B2 reps checked the complete `get_dependencies` answer with a grep, as all of A's did. The bullet does not
touch `get_dependencies`, so this is the agent's choice varying between invocations, not an effect of the change: B1's
two failures had a real mechanism, and B2 shows it does not fire every time.

### Oracle failures (B2)

- gmesh: `py-references-httpbasicauth` rep1 and rep2. In A, B1 and B2 all nine reps made the same two calls
  (`find_definition`, `find_references`) and gave the same answer (`src/requests/models.py`, `prepare_auth`, plus one
  file-scope reference); A passed 2/3 with it, B1 3/3, B2 1/3. The judge is splitting identical answers; this is grader
  variance, not agent behaviour.
- baseline: 5 of 81 (A 2, B1 6) and 2 of 18 on TS.

### Decision rule (section 8) applied to B2 (B2 against A)

| criterion | result | met |
|---|---|---|
| tokens/run drop | -13.8% (27 tasks), -12.0% (TS); per-task ratio 0.836 / 0.790 with baseline 1.000 / 1.025 | **yes** |
| oracle not lower | 80/81 -> 79/81 (27 tasks), 18/18 -> 18/18 (TS) | **no, by 1** (the one extra failure is the judge splitting identical answers, above; baseline moves by up to 4/81 between invocations) |
| g-mesh calls/run do not fall | 2.35 -> 2.21 (-6%) on the 27 tasks; 1.67 -> 1.72 on TS; both scopes 220 -> 210 | **no, by 6%** (all of it `find_references`; B1 was -28%) |
| turns do not rise beyond the baseline spread | 5.99 -> 6.01 (+0.02; baseline spread 0.21); TS 3.33 -> 3.72 (baseline spread 1.95) | yes |
| re-verification turns do not rise beyond the spread | 16 -> 12 (12.5% -> 10.3% of complete results); TS 2 -> 3 | yes |

**Verdict: as written, the rule is still not met, but only by margins inside the measured run-to-run spread.** The restored
bullet brought `find_definition` back to A's level (79 against 82) at no token cost against B1 (per-task ratio 1.002), kept
the -13.8% saving, and removed B1's one failure with a mechanism (json-printer passes 3/3). The two guards still miss by
one oracle point that traces to grader variance on identical answers and by 6% of g-mesh calls, all in `find_references`.
Reading "not lower" and "do not fall" as strict inequalities fails B2; reading them against the baseline arms' spread
passes it. Which reading the rule intends is for the owner to decide.

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

Analysis: `ab.py` (in the S9 agent's scratchpad, since lost; logic in brief), rebuilt for B2 as `ab2.py` (S12 agent's scratchpad). Tokens = input + output + cacheRead + cacheCreation;
cacheRead per turn = sum cacheRead / sum `numTurns`. A *complete structural result* is a
`find_*`/`get_dependencies` response that parses, has no `error`, `ambiguous`, `allUnresolved`,
`truncated: true` or `hasMore`, no `resolved: false` row, and no anchor `resolvedBy` of
`nameAmbiguous`/`fileName`/`semanticNeighbours`. A *re-verification turn* is the next assistant
message after such a result containing a Grep, a Glob, a Bash grep/rg/find, or a Read of the file a
`find_definition` result named. Hint counts are substring counts of each rule's sentence in g-mesh tool results.
