# GM-352 before/after: token sweep, 3.21.0 vs the GM-352 tip

Slice GM-352/S41. Design note Q4 part 2, owner decisions D4 (option A: `main`
3.21.0 against the GM-352 tip) and D5 (option A: the response probe covers
Go/Rust/Python ambiguity turns, plus excalidraw's 4 `nameAmbiguous` tasks
here). The response probe is `docs/results/gm-352-response-probe.md`. This
run uses it to interpret results and does not repeat it.

## Verdict

- **No language shows a measured rise in tokens.** The difference-in-differences
  (g-mesh arm change minus baseline arm change) CI includes 0 for every
  language and for the pooled set: the pooled value is **−4.3 pp
  [−15.8, +8.0]** over 42 tasks.
- **Turns fall on Go, Rust and Python** (−0.5 to −0.6 per task) and rise on TypeScript
  (+0.5). The TS rise comes from two edit tasks and one outlier rep, and no
  bigger g-mesh response causes it (see *Rises, argued*).
- **The ambiguity turn is visible end to end on Go.** On
  `gin-ambiguous-binding`, B0 needed 3 `mcp` calls and 6 turns; A needs
  1 call and 2.7 turns, for **−43.7%** tokens with all reps passing. On TS,
  `ex-ambiguous-clamp-math-utils` falls 9.3% (3.7 → 3.0 turns).
- **Paging: one task pays for the larger `limit:200` page**, namely
  `gin-scenario-handlerfunc-references`. Its page grows from 14,232 to about
  17,900 chars, which is roughly +1k tokens on every later turn. The +73% on
  that task comes from 2 extra turns in 2 of 3 reps. Those turns chase an
  `unlinkedUsages` entry that is byte-identical in both builds. No other task
  reached the old ~13 KB ceiling with an edge tool.
- **Resolution, stated plainly.** Per-language CIs are ±15-20 pp, and the
  pooled CI is about ±12 pp. This run can rule out a large rise. It cannot
  resolve changes of 5-10%.

## Builds and method

| arm | worktree | commit | `--version` (read from records) |
|---|---|---|---|
| B0 | `g-mesh-wt-gm352-m-b0` | `2bc060a` (main, release 3.21.0) | `g-mesh 3.21.0` |
| A | `g-mesh-wt-gm352-m-tip` | `8d89774` (GM-352 tip) | `g-mesh 4.0.0` |

Both are `cargo build --release --workspace`. Every gmesh-configured
record carries exactly the version named for its build: 270 + 270 = 540
records.

- Instrument: g-mesh-bench `fed5396` (0.25.0) in a throwaway worktree
  `g-mesh-bench-wt-gm352-s41`. In that worktree only,
  `g-mesh-bench.config.json`'s `tokenEconomy.arms` was narrowed to
  `gmesh-configured` + `baseline`. The main bench checkout was not touched.
  The arm binary was swapped by `G_MESH_BENCH_BINARY` alone. The run set
  `G_MESH_BENCH_REPS=normal` (3), `WARM_CACHE=yes` and `SAVE_TRANSCRIPTS=yes`.
  Model: `claude-sonnet-5`. The gmesh-configured guidance is pinned (GMB-183),
  so it is identical for both builds.
- Tasks: the design note's 41 tasks, plus the 4 high-fan-out callers tasks
  that GMB-180 added since the note was written
  (`gin-callers-writeheadernow-dispatch`, `rs-callers-flag-name-long-dyn`,
  `py-callers-prepare-two-classes`, `py-callers-register-hook-mixin`). They
  were added because callers tasks are the ones that page. That makes 45
  tasks × 2 arms × 3 reps × 2 builds = **540 runs**. The `tt-diag-*` tasks
  are not in the note's set and were not run.
- Each corpus ran as its own invocation, strictly in sequence. Builds
  alternated within each corpus so that model-side drift does not line up with
  the build: gin B0→A, ripgrep A→B0, requests B0→A, task-tracker-mcp A→B0,
  excalidraw B0→A.
- `baseline` never calls g-mesh, so it serves as the A/A control. Its change
  between a corpus's two invocations is the noise floor. The g-mesh effect is
  the DiD, with a 95% CI from a bootstrap over reps within each
  (task, arm, build) cell (2,000 resamples).
- **0-call records are excluded.** A gmesh-configured run that made no `mcp__*`
  call does not measure the tool (GMB-165). There were 21 such runs, all with
  the server `connected`, so this is agent choice and not miswiring.
  Three tasks lose a whole cell this way and are dropped from the language
  totals:
  - `gin-semantic-panic-recovery`: all 3 B0 reps.
  - `py-deps-packages-boundary`: all 6 runs.
  - `py-semantic-basicauth-header`: 3 B0 reps and 2 A reps.

  The other partial exclusions are one rep each:
  - gin-find-impl-render and gin-deps-render-incoming: one B0 rep each.
  - rs-deps-json-printer-workspace-crates and rs-outline-sink-vs-lines: one A
    rep each.
  - rs-semantic-detect-binary-content: one B0 rep.
  - tt-implement-split-task-cancelled-release: one B0 rep.

## Per language

Tokens are the sum, over the language's tasks, of each task's mean tokens per
run (input + output + cache read + cache creation). In parentheses:
**mean `mcpToolCalls` per run, mean turns per run**.

| language | tasks | g-mesh B0 (mcp, turns) | g-mesh A (mcp, turns) | Δ g-mesh | baseline, B0 run → A run | Δ baseline | DiD [95% CI] | pass g-mesh B0→A | pass baseline B0→A |
|---|---|---|---|---|---|---|---|---|---|
| Go | 7 | 774,255 (2.1, 7.0) | 761,977 (2.6, 6.4) | −1.6% | 509,483 → 488,023 | −4.2% | +2.6 pp [−14.0, +19.3] | 19/19 → 21/21 | 21/21 → 20/21 |
| Rust | 9 | 951,157 (1.8, 6.2) | 875,731 (2.6, 5.6) | −7.9% | 775,286 → 770,139 | −0.7% | −7.3 pp [−30.6, +18.2] | 25/26 → 23/25 | 26/27 → 27/27 |
| Python | 8 | 811,319 (2.7, 6.0) | 725,957 (2.3, 5.5) | −10.5% | 671,315 → 542,927 | −19.1% | +8.6 pp [−7.0, +25.9] | 23/24 → 24/24 | 23/24 → 22/24 |
| TypeScript | 18 | 2,691,164 (2.5, 6.2) | 2,712,714 (2.6, 6.7) | +0.8% | 2,706,908 → 2,925,176 | +8.1% | −7.3 pp [−27.4, +12.6] | 53/53 → 51/54 | 51/54 → 54/54 |

Pooled:

| set | tasks | Δ g-mesh | Δ baseline | DiD [95% CI] |
|---|---|---|---|---|
| all | 42 | −2.9% | +1.4% | −4.3 pp [−15.8, +8.0] |
| Go + Rust + Python | 24 | −6.8% | −7.9% | +1.1 pp [−10.4, +12.5] |
| all, without the 3 edit tasks (`tt-implement-*`, `tt-feature-*`) | 39 | −4.6% | −0.7% | −3.9 pp [−16.5, +8.0] |
| TS, without the edit tasks | 15 | +1.2% | +9.3% | −8.1 pp [−36.4, +15.8] |

On Python, the g-mesh arm's raw tokens fall 10.5%, but the baseline arm fell 19.1% between
the same two invocations. The positive DiD therefore comes from the baseline arm moving, not from the g-mesh arm rising.
Its CI includes 0.

Observed per-task CV of tokens, gmesh-configured, both builds (the design
note asked for this instead of assuming TS's variance): Go median 10%
(max 37%), Rust 13% (max 64%), Python 18% (max 35%), TS 1.6% (max 64%).
Go/Rust/Python are noisier than TS, which explains the wide per-language CIs.

## Per task, gmesh-configured

All values are means over the reps kept. "mcp" is mean `mcpToolCalls` per
run. "g-mesh chars/run" is the summed size of the g-mesh tool responses in a
run: the direct measure of what this task changed.

| lang | task | n B0/A | tokens B0 | mcp B0 | turns B0 | tokens A | mcp A | turns A | Δ g-mesh | Δ baseline | g-mesh chars/run B0→A | pass B0→A |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| TypeScript | ex-ambiguous-clamp-math-utils | 3/3 | 68,699 | 2.3 | 3.7 | 62,343 | 2.0 | 3.0 | -9.3% | +22.8% | 8,068 → 8,135 | 3/3→3/3 |
| TypeScript | ex-ambiguous-exporttosvg-public-api | 3/3 | 67,638 | 2.3 | 3.3 | 112,770 | 2.0 | 7.7 | +66.7% | -41.2% | 4,007 → 4,881 | 3/3→3/3 |
| TypeScript | ex-multihop-mutateelement-nondeleted-callers | 3/3 | 81,194 | 4.0 | 6.0 | 107,969 | 4.0 | 6.3 | +33.0% | +41.2% | 19,417 → 22,311 | 3/3→3/3 |
| TypeScript | ex-references-getnondeletedelements-medfanout | 3/3 | 63,949 | 2.0 | 3.0 | 64,929 | 2.0 | 3.0 | +1.5% | +10.9% | 12,302 → 13,744 | 3/3→3/3 |
| Go | gin-ambiguous-binding | 3/3 | 78,989 | 3.0 | 6.0 | 44,464 | 1.0 | 2.7 | -43.7% | +0.0% | 824 → 984 | 3/3→3/3 |
| Go | gin-callees-dispatch | 3/3 | 66,774 | 2.7 | 4.0 | 80,469 | 2.0 | 4.0 | +20.5% | -8.6% | 5,016 → 3,271 | 3/3→3/3 |
| Go | gin-callers-writeheadernow-dispatch | 3/3 | 187,838 | 2.3 | 12.7 | 197,928 | 4.7 | 12.7 | +5.4% | -13.0% | 4,027 → 7,505 | 3/3→3/3 |
| Go | gin-deps-render-incoming | 2/3 | 98,412 | 2.0 | 6.0 | 78,234 | 2.0 | 5.0 | -20.5% | +10.9% | 1,431 → 1,118 | 2/2→3/3 |
| Go | gin-find-impl-render | 2/3 | 211,424 | 1.0 | 12.5 | 203,083 | 4.3 | 13.0 | -3.9% | -21.5% | 4,482 → 14,766 | 2/2→3/3 |
| Go | gin-scenario-abort-callers | 3/3 | 73,277 | 2.0 | 4.7 | 58,284 | 2.0 | 3.0 | -20.5% | +33.2% | 2,581 → 2,584 | 3/3→3/3 |
| Go | gin-scenario-handlerfunc-references | 3/3 | 57,540 | 2.0 | 3.0 | 99,514 | 2.3 | 4.3 | +72.9% | +30.9% | 14,715 → 18,031 | 3/3→3/3 |
| Go | gin-semantic-panic-recovery | excluded: 0-call g-mesh runs | | | | | | | | | | |
| Python | py-ambiguous-close-session | 3/3 | 37,330 | 1.0 | 2.0 | 37,477 | 1.0 | 2.0 | +0.4% | -24.6% | 610 → 610 | 3/3→3/3 |
| Python | py-callees-session-send | 3/3 | 105,430 | 2.7 | 5.0 | 83,319 | 2.0 | 4.3 | -21.0% | -6.9% | 7,554 → 5,865 | 3/3→3/3 |
| Python | py-callers-prepare-two-classes | 3/3 | 129,958 | 3.7 | 8.0 | 91,273 | 3.0 | 6.0 | -29.8% | -1.2% | 3,380 → 3,169 | 3/3→3/3 |
| Python | py-callers-register-hook-mixin | 3/3 | 143,541 | 2.0 | 10.3 | 154,038 | 2.3 | 10.0 | +7.3% | -38.0% | 3,585 → 5,158 | 3/3→3/3 |
| Python | py-deps-packages-boundary | excluded: 0-call g-mesh runs | | | | | | | | | | |
| Python | py-find-impl-authbase-transitive | 3/3 | 57,779 | 1.7 | 3.0 | 51,638 | 1.7 | 3.0 | -10.6% | +7.1% | 1,680 → 1,740 | 3/3→3/3 |
| Python | py-find-impl-supportsread-protocol | 3/3 | 149,253 | 3.3 | 8.3 | 164,871 | 2.3 | 10.0 | +10.5% | -32.0% | 2,685 → 2,302 | 3/3→3/3 |
| Python | py-references-httpbasicauth | 3/3 | 51,151 | 2.0 | 3.0 | 57,337 | 2.0 | 3.0 | +12.1% | +0.7% | 2,842 → 1,834 | 2/3→3/3 |
| Python | py-scenario-callers-httpadapter-send | 3/3 | 136,877 | 5.0 | 8.7 | 86,004 | 4.3 | 5.7 | -37.2% | -33.5% | 11,630 → 9,350 | 3/3→3/3 |
| Python | py-semantic-basicauth-header | excluded: 0-call g-mesh runs | | | | | | | | | | |
| Rust | rs-ambiguous-regexmatcher | 3/3 | 97,849 | 2.3 | 5.7 | 95,158 | 1.7 | 5.0 | -2.8% | -24.3% | 5,194 → 3,286 | 3/3→3/3 |
| Rust | rs-callers-flag-name-long-dyn | 3/3 | 184,343 | 2.3 | 14.0 | 204,596 | 5.7 | 14.0 | +11.0% | -13.6% | 9,641 → 18,445 | 3/3→3/3 |
| Rust | rs-callers-sink-matched | 3/3 | 151,588 | 2.7 | 9.3 | 141,910 | 6.7 | 9.0 | -6.4% | +1.5% | 6,748 → 6,142 | 3/3→3/3 |
| Rust | rs-deps-json-printer-workspace-crates | 3/2 | 168,177 | 1.0 | 9.0 | 89,794 | 1.0 | 5.5 | -46.6% | -5.0% | 1,751 → 1,751 | 3/3→1/2 |
| Rust | rs-deps-sink-module-incoming | 3/3 | 38,027 | 1.0 | 2.0 | 38,061 | 1.0 | 2.0 | +0.1% | -7.5% | 1,492 → 1,492 | 3/3→3/3 |
| Rust | rs-find-impl-sink-trait | 3/3 | 72,813 | 1.7 | 5.0 | 87,846 | 2.3 | 5.3 | +20.6% | -18.8% | 6,318 → 5,645 | 3/3→3/3 |
| Rust | rs-outline-sink-vs-lines | 3/2 | 53,547 | 2.0 | 3.0 | 104,914 | 2.0 | 4.5 | +95.9% | +73.7% | 31,862 → 31,862 | 3/3→2/2 |
| Rust | rs-references-sinkfinish | 3/3 | 39,396 | 1.0 | 2.0 | 37,556 | 1.0 | 2.0 | -4.7% | +0.9% | 4,401 → 648 | 3/3→3/3 |
| Rust | rs-semantic-detect-binary-content | 2/3 | 145,416 | 2.0 | 6.0 | 75,896 | 2.0 | 3.3 | -47.8% | +47.2% | 14,418 → 13,620 | 1/2→2/3 |
| TypeScript | tt-deps-incoming-db-connection | 3/3 | 116,532 | 2.0 | 7.7 | 62,415 | 1.7 | 3.7 | -46.4% | +8.2% | 9,043 → 7,848 | 3/3→3/3 |
| TypeScript | tt-feature-bulk-cancel-epic-tasks | 3/3 | 337,262 | 5.3 | 16.3 | 301,801 | 8.0 | 19.7 | -10.5% | +36.5% | 13,821 → 17,080 | 3/3→3/3 |
| TypeScript | tt-find-impl-completionverifier | 3/3 | 37,324 | 1.0 | 2.0 | 37,381 | 1.0 | 2.0 | +0.2% | -22.0% | 538 → 538 | 3/3→3/3 |
| TypeScript | tt-implement-release-cancelled-task-bug | 3/3 | 372,657 | 2.3 | 13.0 | 493,725 | 1.7 | 15.7 | +32.5% | -8.4% | 4,365 → 6,447 | 3/3→3/3 |
| TypeScript | tt-implement-split-task-cancelled-release | 2/3 | 998,341 | 6.5 | 27.5 | 922,275 | 4.0 | 27.3 | -7.6% | +9.4% | 22,128 → 12,926 | 2/2→0/3 |
| TypeScript | tt-outline-lifecycle-vs-small | 3/3 | 42,210 | 2.0 | 3.0 | 42,038 | 2.0 | 3.0 | -0.4% | +0.3% | 8,192 → 8,187 | 3/3→3/3 |
| TypeScript | tt-references-requiretask | 3/3 | 101,009 | 2.3 | 5.0 | 99,742 | 2.3 | 6.0 | -1.3% | +16.8% | 3,022 → 2,093 | 3/3→3/3 |
| TypeScript | tt-scenario-bugtrace-createtasks-selfdep | 3/3 | 63,123 | 2.0 | 4.0 | 62,942 | 2.7 | 4.0 | -0.3% | +40.9% | 3,058 → 4,857 | 3/3→3/3 |
| TypeScript | tt-scenario-deletesafe-taskcode | 3/3 | 57,082 | 2.0 | 3.0 | 57,096 | 2.0 | 3.0 | +0.0% | +21.8% | 1,269 → 1,269 | 3/3→3/3 |
| TypeScript | tt-scenario-impact-requireproject | 3/3 | 45,808 | 1.3 | 2.3 | 39,005 | 1.0 | 2.0 | -14.9% | +14.9% | 4,385 → 3,118 | 3/3→3/3 |
| TypeScript | tt-semantic-board-stale-task-flag | 3/3 | 61,005 | 2.7 | 3.7 | 61,321 | 3.0 | 4.0 | +0.5% | +1.1% | 5,879 → 6,044 | 3/3→3/3 |
| TypeScript | tt-semantic-dedupe-prefix-collision | 3/3 | 60,848 | 1.7 | 3.0 | 60,834 | 2.0 | 3.0 | -0.0% | +0.2% | 5,348 → 5,706 | 3/3→3/3 |
| TypeScript | tt-semantic-doc-drift-check | 3/3 | 62,564 | 1.7 | 3.3 | 62,751 | 3.0 | 4.3 | +0.3% | -11.4% | 5,381 → 7,252 | 3/3→3/3 |
| TypeScript | tt-stale-name-doc-sync-check | 3/3 | 53,919 | 1.7 | 2.7 | 61,377 | 2.0 | 3.0 | +13.8% | -19.6% | 6,271 → 7,079 | 3/3→3/3 |

## The ambiguity turn (D5)

| task | B0: mcp, turns, tokens | A: mcp, turns, tokens | reading |
|---|---|---|---|
| gin-ambiguous-binding | 3.0, 6.0, 78,989 | 1.0, 2.7, 44,464 | B0 needs a second `find_definition` round. A's inlined candidate source answers in one call: −43.7%, 3/3 pass both. |
| rs-ambiguous-regexmatcher | 2.3, 5.7, 97,849 | 1.7, 5.0, 95,158 | −0.6 calls, −2.8% |
| py-ambiguous-close-session | 1.0, 2.0, 37,330 | 1.0, 2.0, 37,477 | already one call on B0, flat |
| ex-ambiguous-clamp-math-utils | 2.3, 3.7, 68,699 | 2.0, 3.0, 62,343 | −9.3% |
| ex-ambiguous-exporttosvg-public-api | 2.3, 3.3, 67,638 | 2.0, 7.7, 112,770 | +66.7%, one outlier rep (see below) |

The probe's deterministic second-call counts per corpus remain the exact
turn figure for Go/Rust/Python. The rows above show that the saved call is
taken end to end where a second call existed on B0 (gin, ripgrep, clamp).

## Paging (`limit:200`, the whole-response byte bound)

The concern: under GM-352, a `limit:200` page fills to ~19.9 KB instead of
~13 KB. I counted every g-mesh response over 12,000 chars and every call
that carried `limit`, `cursor` or `answer`. B0 had 65 such calls and A had 79.

| build | g-mesh responses > 12,000 chars |
|---|---|
| B0 | `find_references HandlerFunc limit:200` ×3 at 14,232 (`hasMore: true`, 54 rows); `get_file_outline` sink.rs ×3 at 21,684; one `get_file_outline` line_buffer.rs at 17,661 |
| A | `find_references HandlerFunc limit:200` at 17,888, 17,899 and 15,625 (file-filtered) (`hasMore: false`, 70 rows, the whole edge set); `get_file_outline` sink.rs ×2 at 21,684 (unchanged); `get_file_outline` line_buffer.rs at 22,598; `find_references getNonDeletedElements limit:50` at 12,593 |

- **Only `gin-scenario-handlerfunc-references` meets the larger page.** On B0, the
  page stopped at 54 rows with `hasMore`. On A, the same call returns all
  70 rows, complete, in about 17.9k chars. On a single page, that is
  about +3.7k chars, or ~+1k tokens, carried by each later turn. A rep 1
  (3 turns, 65.8k tokens) is within B0's own spread (44.8k-63.9k).
- **The task's +73% does not come from the page bytes.** In A reps 2 and 3, the
  agent spent 2 more turns on `utils.go`: a Read, or two Greps, plus a
  file-filtered re-query in rep 2. It was following the response's
  `unlinkedUsages` entry (`utils.go`, 1 ref; this is `http.HandlerFunc`). That
  field and its hint are byte-identical in B0's responses, where no rep
  followed them. This is behavioural variance on an unchanged signal (n=3),
  not a cost of the byte bound. On B0, no agent asked for the
  remaining 16 rows behind `hasMore`, so the bigger page saved no call in
  this task either.
- The callers tasks that page in principle (`rs-callers-flag-name-long-dyn`,
  `gin-callers-writeheadernow-dispatch`, `rs-callers-sink-matched`,
  `py-callers-*`) never produced an edge page over 4.3k chars in either build.
  Their higher `mcp` count on A (for example, rs-callers-flag 2.3 → 5.7) comes from
  `find_definition` by `symbol_id` replacing Reads: other-tool chars per run
  fell from 17,825 to 6,561.
- The 21-22 KB outline responses are `get_file_outline`, which is not an edge
  tool. They are the same size on both builds (sink.rs 21,684 = 21,684).

## Rises, argued

- **`ex-ambiguous-exporttosvg-public-api` +66.7%**: A reps 1-2 are 61.5k and
  61.4k, in line with B0's 60.2k-82.3k. A rep 3 is 215k over 17 turns: after
  `find_callers` (1,691 chars, against 1,643 on B0), the agent ran a 12-step
  Grep/Read chain over test files. The g-mesh bytes in that rep are 4.7k. The
  rise is one rep's exploration and not the tool. The inlined
  ambiguous candidates make `find_definition` larger (1,580 → 2,986 chars).
  In this task they save no call, because B0 already went straight to
  `symbol_id`.
- **`gin-scenario-handlerfunc-references` +72.9%**: see *Paging*.
- **`tt-implement-release-cancelled-task-bug` +32.5%** (372.7k → 493.7k, turns
  13.0 → 15.7): this is an edit-and-test task. g-mesh chars per run rose by 2.1k
  (4,365 → 6,447), against +121k tokens. The `mcp` count fell (2.3 → 1.7).
  The rise is the edit loop's length. GMB-158 measured the same task at
  533k-657k on a single binary.
- **`ex-multihop-mutateelement-nondeleted-callers` +33.0%**: the baseline
  moved +41.2% between the same invocations, and g-mesh chars rose 2.9k. It is
  within drift.
- **`gin-callees-dispatch` +20.5%, `rs-find-impl-sink-trait` +20.6%,
  `tt-stale-name-doc-sync-check` +13.8%**: all within the per-task CV of their
  corpus. For each, the g-mesh chars per run fell or rose by under 1k.
- **`rs-outline-sink-vs-lines` +95.9%**: g-mesh chars are identical
  (31,862 both). The baseline arm moved +73.7% in the same invocations. It is
  drift, not the tool.
- **TypeScript turns +0.5/task**: the increase comes from tt-feature-bulk-cancel
  (16.3 → 19.7), tt-implement-release (13.0 → 15.7) and exporttosvg rep 3.
  Without the edit tasks, TS's DiD is −8.1 pp.

## Oracle failures

- **`tt-implement-split-task-cancelled-release`: g-mesh A 0/3 against B0 2/2.**
  All five failures in this task across both arms fail the same holdout test in
  `split-cancelled-release.test.ts`. The baseline arm failed it 3/3 in the B0
  invocation and passed 3/3 in the A invocation, and g-mesh shows the reverse.
  Overall, 5 of 12 runs failed, split evenly between the arms. The A
  failures used 2, 9 and 1 g-mesh calls. This is the task's known instability
  (GMB-158: 2/3), not a GM-352 regression. With 3 reps per cell it cannot be
  separated from noise, so it is flagged rather than dismissed.
- rs-deps-json-printer-workspace-crates A rep 3: the judge faulted the
  answer for listing `grep-regex` without calling it test-only. The g-mesh
  response is identical across builds (1,751 chars).
- rs-semantic-detect-binary-content: rep 3 failed on both builds. The
  baseline also failed in the B0 invocation.
- py-references-httpbasicauth B0 rep 3: a B0-only failure.

## Machine state

- Sweep: 2026-10-06 19:30:30Z → 23:19:13Z.
  `/usr/bin/time -p` on the whole sweep: **real 13,722.7 s, user 3,103.9 s,
  sys 782.8 s**. The harness spends most of its time waiting on API calls.
- `uptime` at start: load 4.66 / 10.45 / 9.52. At end: 1.89 / 2.52 / 3.12.
  The 1-minute load was sampled every 60 s (229 samples): median 4.4, max 35.6,
  min 1.7. One heavy window came from other work during gin-A, at 11-12 at its
  end. Tokens, not wall clock, are the measured quantity, and no run
  lost its MCP server (0 records with a non-`connected` server).
- The per-invocation `real`/`user` figures are in the scratch log. Cold index
  per invocation (`g-mesh init`):
  - gin: 8.1 s (B0) and 11.6 s (A).
  - ripgrep: 52.0 s (B0) and 214.2 s (A, started right after gin-A's busy window).
  - requests: 58.6 s (B0) and 21.0 s (A).
  - excalidraw: 117.7 s (B0) and 46.6 s (A).
  - task-tracker-mcp: 1.6-2.6 s (B0) and 5.9-9.4 s (A), per edit-task clone.
- Spend: $43.40 (agent and judge), above the note's $30-35 estimate because
  of the 4 added tasks and TS edit tasks.
- g-mesh daemons: `pgrep -fl g-mesh` before and after the sweep lists the same
  three processes. They belong to the main checkout and other sessions, and none
  is from this run. Nothing leaked.

## What was not measured

- B1 (release-4.0.0 without GM-352) was not swept (D4 option A). A TS
  shift therefore mixes the GM-324 port with GM-352's mechanisms; the probe's
  B1/A bytes are the attribution.
- No per-language ambiguity task exists for Python beyond the one that
  already took one call on B0 (D5 option A). The probe's counts remain the
  turn figure there.
- Changes smaller than about ±12 pp pooled, or ±20 pp per language, are below
  this run's resolution at 3 reps.

Raw records: `g-mesh-bench-wt-gm352-s41/results/token-economy/2026-10-06T19-30-31-968Z.json`
… `2026-10-06T23-00-06-729Z.json` (10 files, one per corpus × build),
transcripts under `results/transcripts/` there.
