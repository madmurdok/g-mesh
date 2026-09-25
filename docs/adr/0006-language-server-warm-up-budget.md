# 0006. Language server warm-up: a one-time first-answer budget per server

## Status
Proposed (2026-09-25, GM-416). Measured in GM-416/S1; implementation is
GM-416/S2 in `plugins/sdk/src/lsp/bridge.rs` (`run_pass`, `Budgets`) and
`plugins/sdk/src/lsp/client.rs` (the per-server "has answered" latch).

## Context
GM-415/S5 recorded the Python semantic pass failing on a cold index of
g-mesh itself: "the language server did not answer a question about
plugins/python/conformance/project/pkg/mod.py within 10s", pass 12.89s
(incomplete), pyright 15.3s CPU in 21s wall, load 3.2. A reindex right
after succeeded in 2.99s. The 10s is `Budgets::request`, measured from when
each question is sent. The Python manifest declares `readiness =
"on-demand"`, so the first pass sends its first 8 questions (`concurrency`)
the moment `initialized` and the `didOpen`s are out.

What was measured (pyright 1.1.414 via `npx --yes --package pyright`, node
20.6.1, 8-core macOS; every message was timestamped by a transparent stdio
tap placed in front of the real `npx`, with no change to g-mesh):

- **pyright blocks once, at start-up, and then answers everything at
  once.** It answers `initialize` in 1.2-1.8s (4.8s under stress), asks
  `workspace/configuration` twice, then sends nothing at all until one
  instant when its log lines and all 8 queued answers arrive together
  (the 8 latencies agree to within 1ms). Every later answer takes
  0.07-0.23s (1.0-1.8s at load 318). A `sample` of the node process during
  the block shows its main thread in synchronous `uv_fs_scandir`
  (`readdirSync`): about 1.05s of a 3.6s block. The block depends on the
  tree under the root, and on a race between pyright's scan and the first
  request:

  | Root | Runs | First answer (s) | Runs over 5s |
  |---|---|---|---|
  | g-mesh worktree without `target/` | 7 | 0.85-1.89 | 0 |
  | g-mesh worktree, 369k files in `target/` | 22 | 1.48-7.15 | 9 |
  | same, client advertises `didChangeWatchedFiles` | 5 | 1.22-8.60 | 1 |
  | 1.1M-file tree (3 `target/` clones) | 4 | 1.29-7.93 | 1 |
  | daemon cold start (`mcp-shim`, fresh `G_MESH_HOME`) | 4 | 6.57-7.33 | 4 |
  | `g-mesh reindex`, fresh `G_MESH_HOME` | 1 | 2.09 | 0 |

  Machine state: daemon runs at load 15, 7, 6, and 43 going up to 318 with 16
  busy loops (7.33s); the reindex at load 267 (`time -p`: real 1117, user
  3516, sys 28, mostly the embedding pass); pyright's own CPU in two of
  the daemon runs was 13.0-14.1s user in 13.3-13.4s wall. That is busy,
  not idle, as in S5.
- **The failure did not reproduce at 10s in this session:** 0 of 4 daemon
  cold starts, 0 of 1 reindex, 0 of about 40 standalone starts. The worst
  was 8.60s, which is 86% of the budget. S5's run on the main checkout was
  over it. The mechanism is the same in every trace: one warm-up block in
  front of the first answer, followed by fast answers.
- **pyright sends no readiness signal.** No `$/progress`, and no
  `window/workDoneProgress/create`, appeared in any run, although the
  client advertises `window.workDoneProgress`. Traces ran up to 20s after
  spawn. The only sign that it has finished is a `window/logMessage` text,
  "Found N source files". That text is not protocol, and it arrives after
  the first answers (for example 12.3s, against a first answer at about
  7.8s).

## Decision
We will give each server a **one-time first-answer budget**,
`Budgets::first_answer`, defaulting to **60s**. It extends
`Budgets::request` only until that server answers anything.

1. `LspClient` latches `answered` the first time any response to a
   question arrives, whether a result or an error. The latch lasts for the
   server's lifetime, across passes. A restarted server is a new client and
   gets its own warm-up.
2. In `run_pass`, a question is expired at `sent + first_answer` while the
   latch is unset. Once it is set, a question expires at
   `max(sent, answered_at) + request`. Questions that were queued behind
   the warm-up get the normal 10s, counted from the moment the server
   became responsive.
3. **Bound for a server that never answers:** when a question expires
   while the latch is still unset, the pass ends immediately as incomplete.
   All in-flight questions are cancelled, and every asked, queued or
   deferred file is marked failed. The reason is "the language server
   answered nothing within {first_answer:?} of its first question". So a
   pass on a silent server ends within `first_answer` plus one poll tick,
   and never later than the pass deadline (`project_floor` 15 min,
   `single_file` 90s). The warm-up is also spent at that point, so a later
   pass on the same live server falls back to today's per-request 10s.
4. This applies to every language, not only Python. It only lengthens a
   wait for a server that has not yet answered once, so rust-analyzer, gopls
   and the rest are unaffected once they are warm.

60s is about 7 times the worst start measured here (8.60s), and fits inside
both pass budgets. It costs nothing when the server does answer: the pass
proceeds at the first answer, not at 60s.

## Rejected options
- **(a) Wait for the server's own analysis-complete signal.** pyright has
  none: it sends no `$/progress` and no `workDoneProgress/create`. Its
  "Found N source files" log line is free text, depends on the version,
  and arrives *after* the first answers, so waiting for it would slow down
  a pass that already works. The generic form of (a) already exists:
  default readiness, a quiet `settle`. Python opts out of it with
  `on-demand` because the quiet period tells nothing about a server that
  reports no progress. And (a) would still need a timeout for a server that
  never signals, which makes it (b) with extra steps.
- **(b) as a plain constant: raise `request`, or give "the first request"
  a longer budget.** Raising `request` to cover the warm-up, say 60s, also
  makes every later hung question cost 60s, and the per-file passes pay
  that. "The first request" alone is not enough either, because pyright
  holds all 8 in-flight questions (and whatever is queued behind them)
  behind one block. The latch in this decision is the targeted form of
  (b).
- **(c) Retry the pass once.** It would have worked on the S5 data (the
  reindex 3s later succeeded). But it spends a whole failed pass first, and
  it cancels questions that pyright then answers anyway. A pass-level retry
  cannot tell a warm-up apart from a refusal, a crash or a hung server
  without the information this decision already uses. Retrying every
  failure doubles the bound for a silent server, to 2 x (questions/8 x 10s)
  capped by the pass budget. And a warm-up longer than two request budgets
  still fails.
- **Advertising `workspace.didChangeWatchedFiles.dynamicRegistration`**
  (so that pyright does not watch the tree itself) did not remove the slow
  mode: 8.60s and 7.93s were measured with it.

## Consequences
- The cold-start failure turns into a successful pass that is delayed by
  pyright's own warm-up, which is 1-9s as measured.
- A silent server costs `first_answer` once per server start, instead of
  a series of 10s expiries, and ends with its own reason in `status`.
- Tests S2 must add, in `plugins/sdk/tests/lsp_bridge.rs` with `g-mesh-fake-lsp`:
  1. **A late first answer.** Add a new fake-lsp option `firstAnswerDelayMs`:
     the server blocks its read loop before answering its first
     `definition`/`implementation` request, so every queued request waits
     behind that block (the same shape as pyright), and it answers
     immediately afterwards. Use `request` 300ms, `first_answer` 5s, a
     delay of 1.5s, and more questions than `concurrency`. The pass must be
     complete, with every edge and no reason. Control: use `request` for
     unanswered servers too. The pass then reports "did not answer ...
     within 300ms".
  2. **A server that never answers** (`silentFrom: 1`). Use `request`
     300ms, `first_answer` 1s, concurrency 2 and at least 10 questions. The
     pass must be incomplete, with the "answered nothing within 1s" reason,
     and elapsed must be at least 1s and under 1s plus 1.5s of slack.
     Control: drop the early end. The pass then expires question by
     question, at about 5 x 1s, and the elapsed bound fails.
  3. **The warm-up is spent once per server.** First a pass that warms the
     server, then one where it goes silent (`silentFrom` past the first
     pass's count). The second pass's expiry must be at `request`, not at
     `first_answer`. Control: reset the latch per pass.
  4. `Budgets` invariants: `request < first_answer <= single_file <=
     project_floor`.
- S4 measures 3 cold daemon starts of g-mesh with pyright via npx, and
  records the first-answer latency next to the pass result.
- Follow-up, not decided here: the warm-up grows with the untracked tree
  pyright scans (`target/`). Passing gitignored directories to pyright as
  excludes could shrink it, but that is not verified for settings delivered
  through `workspace/configuration`.
