# 0036. A call names its project; a contested selection refuses calls that do not

## Status
Accepted (2026-10-14)

## Context

Claude Code's subagents share their parent's MCP connection, so every agent
on it shares one shim session. One agent's `select_project` rerouted every
other agent's later calls; ADR 0014's stamp made that visible but did not
prevent it. The shim cannot tell callers apart (JSON-RPC ids are per
connection, `claudecode/toolUseId` is per call, the agent id is never sent),
so isolation has to come from the call itself. The facts and the options
weighed: [`GM-571-isolated-project-selection.md`](../design/GM-571-isolated-project-selection.md)
(D1-D6, D8) and [`gm-447-shared-project-selection.md`](../architecture/gm-447-shared-project-selection.md).

## Decision

- **Per-call `project` (D1).** Each of the eight index tools takes an
  optional `project`: a path relative to the folder, or absolute. The shim
  (`core/src/shim/router.rs`) routes a call that carries it to that
  project's daemon, whatever is selected, and the answer's stamp names it.
- **Injected by the front only (D2).** `front::listed_tools` adds the
  property to the eight tools' schemas; daemons' parameter structs do not
  change. The shim removes the argument before forwarding, so no daemon sees
  it. A single-project session pays no schema bytes; there `project` may
  only name the root itself, and anything else is refused by the shim.
- **Contested refusal (D3).** The shim records every distinct project a
  `select_project` switched to. While only one was, a call without
  `project` goes there. Once a second, different one has been, the session
  is contested for good: a call without `project` is answered by the shim
  with a tool error naming the selections, and nothing is sent upstream.
  Re-selecting the same project does not contest. The switch and the flag
  change under the router lock.
- **Guidance on first answer (D4).** A project reached only through
  `project` gets its guidance (its daemon's `instructions`) as one more text
  item of its first `tools/call` answer, once per project per session. It
  follows the stamp (and the restart line, if any) rather than trailing the
  tool's own items, so the last text item stays the tool's own (ADR 0014).
- **Pool (D5).** Sub-project connections form a pool keyed by canonical
  root, at most 4 live; above that the least recently used one is retired
  (it delivers what it owes, then half-closes). Reopening an evicted project
  is not a restart, so its answer carries no restart line.
- **Resolve by probe (D6).** `candidates::resolve` probes the one path a
  name gives, applying the walk's rules (depth, skipped names, a real
  directory, a marker, no marked directory above it) instead of searching a
  walk: fresh by construction and not bounded by the walk's caps. Both
  `Front::select_project` and the shim use it; the walk remains for listing.
- **`select_project` (D8)** keeps its shape. Its result says calls without
  `project` go to the selection only while it is the only one, and to pass
  `project` on every call when other agents share the connection.

## Consequences

- An agent that passes `project` on every call is never answered from
  another agent's project. An agent that never passes it keeps today's flow
  until a second project is selected on its connection; then its calls fail
  loudly instead of answering wrongly, and one refused call teaches it.
- A single agent that switches projects itself must pass `project`
  afterwards.
- Residual hole: an agent that never selected and never passes `project`, in
  a session where exactly one other project was selected, is answered from
  that project. The stamp names it.
- The front's `tools/list` grows by the `project` property on eight tools;
  `TOOLS_LIST_BYTE_CEILING` rises accordingly. When the front is unreachable
  and the selected daemon answers `tools/list`, the property drops out of
  that answer.
- Every call that carries `project` is parsed and re-serialized by the shim.
  A first call to a cold project's daemon bootstraps it under the router
  lock, so other frames wait that long.
- Two implementations of "what is a candidate" (walk and resolve) exist and
  are pinned to agree by test.
