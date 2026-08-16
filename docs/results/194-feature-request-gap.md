# The `feature-request` 2x is a sampling artifact, not a g-mesh cost (task #194)

Investigating the one inverted category in the 2026-08-16 full sweep
(`results/token-economy/2026-08-16T00-35-12-254Z.json` in g-mesh-bench, 387
runs, the first sweep where all three arms genuinely ran):

```
tt-feature-bulk-cancel-epic-tasks   gmesh 7112 out / 12 turns | serena 7405 / 28 | baseline 3562 / 10
```

That category holds exactly one task, so this is **one task measured three
times, not a category trend**. Nothing here generalizes to "feature work" —
its `implementation` neighbours are a win (`ex-implement-mutateelement-elbow-
zero-position` 1896 vs baseline 2601) and a wash (`tt-implement-release-
cancelled-task-bug` 4603 vs 4543).

**Conclusion: no g-mesh change. The 2x does not survive more repetitions.**
Pooled over 9 reps per arm the difference is +18% with a 95% CI of
[-909, +2778] output tokens — it does not exclude zero, and it excludes the
+3550 the sweep reported. What the sweep sampled was a bimodal per-run
extended-thinking event worth ~3000 output tokens, which lands on either arm
with roughly equal probability.

## Method

Two fresh 3-rep runs of `tt-feature-bulk-cancel-epic-tasks`
(`gmesh-configured` vs `baseline`, `G_MESH_BENCH_SAVE_TRANSCRIPTS=yes`,
g-mesh built from `research/194-feature-request-gap`), 12 transcripts read
line by line, plus the 9 sweep records. Total extra spend $3.45 (the full
sweep is $37, deliberately not re-run).

The split below is exact, not estimated. Thinking text is redacted in a saved
transcript (`"thinking": ""` plus a signature), so it cannot be measured by
character count — but the CLI's final `result` event reports it directly as
`usage.output_tokens_details.thinking_tokens`. Everything else the model
produced is a visible `text` or `tool_use` block, apportioned by character
share of the non-thinking remainder, so the three buckets always sum back to
the `outputTokens` the benchmark recorded.

## Where the tokens go

Means over the 12 transcripts measured here (6 per arm):

| bucket | gmesh-configured | baseline | delta |
| --- | --- | --- | --- |
| extended thinking | 2040 | 2322 | **-282** |
| visible text (narration + answer) | 2851 | 2465 | +386 |
| tool-call JSON | 718 | 654 | +64 |
| **output total** | **5609** | **5441** | **+168** |
| tool-result chars *consumed* | 39 740 | 53 381 | **-26%** |

Three things follow.

**1. The gap is extended thinking, and thinking is bimodal noise.** Sorted,
every thinking measurement taken here:

```
245, 408, 494, 596, 684, 1364 | 2996, 3380, 3542, 3700, 3760, 5001
```

Two clusters, nothing in between, and the split is exactly 3/3 *within each
arm*. A run either enters a long deliberation mode (~+3000 output tokens) or
it doesn't, and which arm it happens to is a coin flip. In the run where both
arms saw the same corpus at HEAD, g-mesh drew 3 high and baseline 3 low
(gmesh 6408 vs baseline 3811 mean, "g-mesh costs 68% more"); in the very next
run ten minutes later the draw reversed and so did the verdict (gmesh 4809 vs
baseline 7069, "g-mesh costs 32% less"). The sweep drew the first pattern.

This is the same effect #190 measured on `tt-scenario-impact-requireproject`
(388 thinking vs baseline's 47), except an order of magnitude larger in
absolute terms because a "design a feature" prompt invites deliberation that a
lookup prompt does not — which is precisely why it swamps this one task's
measurement and no other's.

**2. The reproducible part of the difference is small.** Strip the noisy
bucket out and compare only what the model demonstrably produced — visible
text plus tool-call JSON — and g-mesh is +450 tokens (+14%), 95% CI
[-216, +1117]. That is the honest upper bound on what the tool costs on this
task, not +3550.

**3. The "g-mesh's precision pulls the agent into more files" hypothesis is
false — the opposite happens.** The g-mesh arm consumed **26% fewer**
tool-result characters. Both arms read the same set of files (`lifecycle.ts`,
`status.ts`, `epics.ts`, `tasks.ts`, `mcp/tools/lifecycle.ts`,
`mcp/toolResult.ts`); baseline `Read`s each of them whole, while the g-mesh
arm opens two `search_code` calls to locate and then `Read`s targeted line
ranges (`offset`/`limit`). It does take ~2 more turns, spent on verification
greps baseline skips.

## Does the extra spend buy anything?

The oracle passes 3/3 for both arms and cannot answer this, so the six
same-revision answers were compared directly. They are the same length
(g-mesh 5433 chars mean, baseline 5239) and both arms hit all four rubric
points every time. Two differences are real but small, both in g-mesh's
favour:

- **A factual error only baseline made.** baseline rep2: *"better-sqlite3
  transactions aren't meant to nest"* — they do, via savepoints — and it
  designs around the mistake by inlining the mutation instead of reusing
  `cancelTask`. Two g-mesh reps state the savepoint behaviour correctly, and
  the transcript shows why: they spent one of those extra turns grepping
  `db.transaction(` and `src/db/connection.ts` to check the driver.
- **A subtle bug only g-mesh caught.** g-mesh rep2 noticed that
  `findUnblockedDependents` looks at *current* status, so a same-epic
  dependent still `pending` when the loop reaches it gets reported as
  "unblocked" even though the same batch is about to cancel it, and adds a
  correcting pass. No baseline rep raised it.

So: **modestly better, not 1.7x better.** Which is consistent with the
statistics — the reproducible cost delta is ~14%, and the answers differ by
about that much in depth. There is nothing here to optimize away; a guidance
change aimed at this number would be tuning against a coin flip.

## Separate defect found: the two arms were reading different source code

Not the cause of the gap, and it does not change the conclusion above — but it
is real, undocumented, and affects every `kind: "local"` corpus task in the
benchmark.

`baseline` runs from `resolveWarm()`'s persistent clone under
`$TMPDIR/gmesh-bench-corpora/`, which is cloned **once, ever**, and refreshed
never (the guard is `git rev-parse HEAD` succeeding). `gmesh-configured` and
`serena-configured` run from `resolveConfigured()` → `resolveFresh()`, a new
`mkdtemp` clone of the registry path's **current** HEAD on every invocation.
The `task-tracker-mcp` cache clone dates from 2026-08-10; by the sweep its
`lifecycle.ts` was 53 lines behind, missing a whole `cancelRelease` function
whose docstring is specifically about cancellation semantics and about *not*
cascading to tasks.

The visible symptom in the sweep's own output: every baseline answer cites
`src/domain/lifecycle.ts:372` for `cancelTask` and every g-mesh answer cites
`:425`. Both are correct — for different checkouts. Read side by side they
look like one arm hallucinated a line number.

Measured impact on this task's numbers: none that is separable from the
thinking noise (g-mesh went *down* on the older corpus while baseline went
*up*, which no single-cause corpus effect can produce). But it silently
compares two revisions on every local corpus, and it should be fixed on the
benchmark side — refresh or pin the warm clone, and record the resolved commit
in each run record the way `serenaRevision` already is. That is a g-mesh-bench
task, not a g-mesh one.

## Numbers

Per-rep output tokens, `tt-feature-bulk-cancel-epic-tasks`:

| session | corpus revision | gmesh-configured | baseline |
| --- | --- | --- | --- |
| sweep 2026-08-16T00-35 | gmesh HEAD / baseline 08-10 | 7112, 8506, 5643 | 3105, 3562, 7192 |
| this run 09-33 | both HEAD | 5181, 8058, 5984 | 3367, 3157, 4910 |
| this run 09-43 | both 08-10 | 3618, 3956, 6854 | 6199, 6746, 8262 |

Pooled: gmesh mean 6101, median 5984, sd 1697 (n=9); baseline mean 5167,
median 4910, sd 1981 (n=9). Welch t = 1.07, p ≈ 0.3. Oracle 9/9 for both arms
across every session.
