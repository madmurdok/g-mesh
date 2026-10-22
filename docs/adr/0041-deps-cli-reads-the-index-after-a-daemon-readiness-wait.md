# 0041. `g-mesh deps` reads the index directly after a daemon readiness wait

## Status
Accepted 2026-10-22 (GM-647, owner decision D2 "Гибрид").
Design note: [`GM-647-deps-json-cli.md`](../design/GM-647-deps-json-cli.md) §2.
Format: [`docs/cli/deps-json.md`](../cli/deps-json.md).

## Context
`g-mesh deps --json` prints a project's whole import graph for a script
(BlueprintAI's `scan.py`). It must work on a cold project: no daemon, no
index. No daemon request returns the whole graph: `get_dependencies` walks
from one anchor. Three ways to get the data were considered:

- **A.** A new daemon-side request (a hidden MCP tool). One reader of the
  index, but a tool the agent must not see (`tools/list` filtering), a
  multi-MB answer in one JSON-RPC message, and a second public contract.
- **B.** Open `index.db` without the daemon. Cannot build a cold project's
  index without doing what `reindex` does (stop the daemon, wipe, rebuild),
  and prints a stale index as current.
- **C.** The daemon for bootstrap and readiness, a direct read for the data.

## Decision
C. `deps`:

1. reaches or bootstraps the project's daemon with `shim::connect_or_bootstrap`,
   the same code every MCP session uses (version retire, rebuild check,
   bootstrap lock);
2. sends a raw NDJSON MCP exchange over that connection
   (`daemon::client::activate_and_wait`: `initialize`,
   `notifications/initialized`, `tools/call get_file_outline "\0"`) and waits
   for the call's answer, which the daemon sends only after its `prepare`
   step (activation, structural wait, queued-change replay);
3. reads `index.phase`: `structural`/`embedding`/`ready` is ready, `failed`
   is "index unavailable", anything else (the daemon's own wait cap fired)
   asks again;
4. opens `index.db` read-write without CREATE plus `PRAGMA query_only = ON`,
   and runs every export query in one read transaction, a single WAL snapshot
   while the daemon keeps writing.

One client-side deadline (`--timeout`) bounds steps 1-3; the wait runs on a
thread the CLI abandons on expiry, so socket reads need no timeout of their
own.

## Consequences
- No new daemon surface: `tools/list`, the instructions text and the MCP
  protocol are unchanged.
- The CLI reads the index schema directly, like `g-mesh status`. Both are the
  same binary as the daemon, and step 1 retires a daemon older than the CLI,
  so the schema they see matches.
- The daemon keeps indexing after a timed-out or killed `deps`; the next run
  finds it further along. The walk is never duplicated.
- Readiness is two signals (the answer, then the phase word), not one typed
  answer.
- Retiring an outdated daemon disconnects that project's live MCP sessions
  once; their shims reconnect, as for any new shim.
