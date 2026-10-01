# GM-447: do real agents act on the answering-project stamp?

Measured 2026-10-01 (task GM-447, slice S6). Harness: `eval/gm447/`.

## Question

Subagents of one Claude Code session share one MCP connection, so one
agent's `select_project` redirects the other agents' later calls (see
`docs/architecture/gm-447-shared-project-selection.md`). ADR 0014 adds a first
text item `g-mesh: answered from project <rel>.` to every result once a
session has switched, plus a sentence in the `select_project` header telling
agents to re-select if the stamp names another project. Unit and e2e tests
show the stamp is sent. This measurement asks whether a real agent that gets a
misrouted answer notices the stamp and recovers, compared with a binary that
has no stamp.

## Setup

- **Fixture**: a folder with two Python projects, `p1` and `p2` (each just a
  `pyproject.toml` and `src/settings.py`). Both define `retry_limit()` with the
  same docstring at the same path; `p1` returns `41`, `p2` returns `97`. `p2`
  also defines `export_format()`. The only difference in a `find_definition`
  answer is the returned value (and, on the branch, the stamp).
- **Session**: `claude -p` (Claude Code 2.1.286), `--model sonnet`
  (`claude-sonnet-5-5` for the parent and both subagents), `--strict-mcp-config`
  with g-mesh as the only server, `--setting-sources ""`,
  `CLAUDE_CODE_DISABLE_CLAUDE_MDS=1`, built-in tools limited to `Task,Bash`,
  and an isolated `G_MESH_HOME` for each run. The parent launches two
  general-purpose subagents in one message:
  - **A**: `select_project p1`, touch `a.ready`, wait for `b.ready` ("the p1
    build is still running"), then find `retry_limit` and report
    `RETRY_LIMIT=<n>`.
  - **B**: wait for `a.ready`, `select_project p2`, touch `b.ready`, then
    find `export_format`.

  The markers put B's select between A's select and A's lookup. A's brief
  does not mention the other agent, project switching or the stamp.
- **Misroute detection**: `eval/gm447/tap.py` sits between Claude Code and the
  shim and logs every JSON-RPC frame. A run counts as misrouted only if the
  wire shows an A lookup (a call whose arguments mention `retry_limit`), sent
  after B's `select_project p2`, that was answered with `return 97`. A run with
  no misroute would have been excluded; none were.
- **Arms**: *branch* is `fix/GM-447-shim-shared-project-selection` at
  `73b22cb` (debug build). *control* is `18fb7fd`, the `release-3.18.0` commit
  the branch merged (debug build, TS plugin built), so the control lacks
  only the GM-447 work: `git diff 18fb7fd 73b22cb` touches
  `core/src/shim/router.rs`, `core/tests/multi_project_front.rs` and docs. (`release-3.18.0` has since moved to `08426ff`, with
  embedding and pagination changes that do not touch the shim.)

Run: `eval/gm447/run_set.py --arm branch=<bin> --arm control=<bin> --runs 5
--fixture <dir> --home-root <short dir> --out <dir>`. `G_MESH_HOME` must be
short, because a deep path exceeds the 103-byte socket-path limit.

## Results

5 runs per arm, both after one dry run of the branch arm (which matched branch
run 1 below). Load average 3.9–5.1 throughout.

| arm | misrouted runs | stamp in misrouted answer | A re-selected p1 | A's final answer correct (41) | A reported p2's value (97) |
|---|---|---|---|---|---|
| branch | 5/5 | 5/5 | 5/5 | **5/5** | 0/5 |
| control | 5/5 | 0/5 (no stamp exists) | 0/5 | **0/5** | 5/5 |

Every branch run went the same way: `find_definition` was answered from p2,
then `select_project p1`, then `find_definition` was answered from p1, then
`RETRY_LIMIT=41`. Three of five A answers also explained it unprompted, e.g.
"The first lookup was answered from project p2 (it returned 97), so I
re-selected p1 and repeated it." Every control run used the one misrouted
answer and reported `RETRY_LIMIT=97` with no sign that anything was wrong.
B answered `EXPORT_FORMAT=parquet` in all 10 runs.

Per run (wall seconds, `/usr/bin/time -p` user seconds of the `claude`
process, inner tokens = input + output + cache read + cache creation over all
models, inner cost from the result event):

| arm | run | wall s | user s | inner tokens | cost USD |
|---|---|---|---|---|---|
| branch | 1 | 21 | 2.32 | 156,297 | 0.0880 |
| branch | 2 | 18 | 2.22 | 156,178 | 0.0873 |
| branch | 3 | 19 | 2.26 | 156,281 | 0.0878 |
| branch | 4 | 18 | 2.56 | 156,278 | 0.0879 |
| branch | 5 | 18 | 2.35 | 156,347 | 0.0881 |
| control | 1 | 21 | 2.37 | 141,414 | 0.0790 |
| control | 2 | 16 | 2.08 | 141,486 | 0.0794 |
| control | 3 | 20 | 2.35 | 141,404 | 0.0790 |
| control | 4 | 16 | 2.21 | 141,430 | 0.0793 |
| control | 5 | 17 | 2.15 | 141,525 | 0.0795 |

Totals: branch $0.439 / 781 k tokens, control $0.396 / 707 k tokens, plus
$0.26 for two dry runs (the first failed, see below). The roughly 15 k extra
tokens per branch run are the re-select and the repeated lookup, which is the
cost of getting the right answer.

## Verdict

With the stamp, real Sonnet subagents noticed every misrouted answer,
re-selected their project, repeated the lookup and reported the correct value
(5/5). Without it, they reported the other project's value every time (5/5
wrong). This is a forced worst case: one symbol, one call, and a value that
cannot be checked any other way. It shows the stamp is acted on. It does not
show how often misroutes happen in practice.

## Finding outside GM-447: `server/discover` probe

In the first dry run, Claude Code 2.1.286 marked g-mesh **failed** at
connect. For a server it has no remembered verdict for, Claude Code sends
`server/discover` (protocol `2026-07-28`) before `initialize`. The front
daemon (rmcp) logs `MCP initialization failed: expect initialized request,
but received ... server/discover` and drops the connection. The shim then
answers the probe with `-32603 ... the connection to the daemon ... ended`,
and the client gives up without falling back to `initialize`. The tap now
answers that probe itself with `-32601 Method not found`, and with that the
client falls back to `initialize` and connects normally. Both arms run through
the same tap. Existing user setups probably still work because the client
remembers earlier discover verdicts. A fresh install, or a cleared verdict
cache, may not. This is worth its own task: the shim should answer
`server/discover` with -32601 rather than pass it to the daemon.
