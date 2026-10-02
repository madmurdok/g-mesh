# 0014. A switched session's tool results name the project that answered

## Status
Accepted (2026-10-01)

## Context

Claude Code's subagents share their parent's MCP connection, so every agent
on it shares one shim session and one project selection. When one agent
calls `select_project`, the others' later calls go to that project and are
answered from it as successes. On one connection the shim cannot tell the
callers apart: JSON-RPC ids are per connection, Claude Code's
`claudecode/toolUseId` is per call, and its agent id is not sent to g-mesh.
Reproduction, the facts behind it and the options considered (per-call
`project` argument, required argument, re-confirmation, session token):
[`gm-447-shared-project-selection.md`](../architecture/gm-447-shared-project-selection.md).

## Decision

The shim (`core/src/shim/router.rs`) puts a first text item in every
`tools/call` result that a sub-project's daemon answers:

    g-mesh: answered from project <name>.

`<name>` is that daemon's root relative to the shim's root, `/`-separated.
It is the root of the upstream the call was sent to (the one its pending
record names), not the selection when the answer arrives, so a call in
flight across a switch names the project that really served it.

- **Stamped:** tool results from a sub-project, including tool errors
  (`isError: true`): an error such as "symbol not found" is as misleading as
  a success when it comes from another project.
- **Not stamped:** anything the front answers (`select_project`,
  `tools/list`, any call before the first switch), JSON-RPC error responses
  (no `content` to carry the line), and the shim's own answers to calls an
  ended connection still owed (their text already names the daemon's root).
- A session that never switches has no sub-project upstream, so its frames
  still cross byte-for-byte.
- The `select_project` result after a switch tells the agent that agents on
  one connection share the choice, quotes the stamp's shape, and says to call
  `select_project` again when it names a project other than the one it
  selected. This sentence is in the shim's header, not in the project's
  instructions, so it does not count against `INSTRUCTIONS_BYTE_CEILING`;
  no tool description changes.

## Consequences

- A misrouted call is still answered from the wrong project; it is now
  visible, and correctness depends on the agent reading the line. Two agents
  that keep alternating can still ping-pong between projects.
- Every stamped answer costs about 40 characters of context, and the shim
  parses and re-serializes it (where it otherwise scans only `id` and
  `method`). Key order inside the result may change; content does not.
- A client that reads only the first text item of a result now reads the
  stamp. Clients that concatenate text items get the line in front of the
  tool's own text, so a consumer parsing the result as JSON must take the
  last text item.
- A hard guarantee (a `project` argument required in multi-project sessions)
  remains possible later and combines with this.
