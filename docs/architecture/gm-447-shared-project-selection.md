# GM-447: one project selection shared by every agent on a connection

Status: option 2 approved and built; the decision is
[ADR 0014](../adr/0014-answering-project-stamp.md). The reproduction test
below is now the regression test
`another_agents_select_is_named_in_this_agents_answer`.

## The problem

Claude Code's subagents share their parent's MCP connection
(`docs/architecture/gm-432-search-code-hang.md`, "Root cause"), so the shim
(`core/src/shim/router.rs`) serves one session for all of them. The session's
project is one value, `Router::sub` (falling back to `Router::front`), read by
`Router::current()`. When agent B calls `select_project` for another project,
agent A's next call is routed to B's project and answered from it as a
success, and nothing in the answer says which project served it. GM-432 made
sure a call *in flight* during a switch is answered; it did not change where
later calls go.

## Reproduction

Unit test `shim::router::tests::known_bug_another_agents_select_reroutes_this_agents_calls`
(in `core/src/shim/router.rs`), driving the real `serve` loop over pipes with
the existing `Session`/`Fake` harness (fake daemons, no real daemon, no
socket, nothing to leak):

1. "Agent A" selects `a` (id 1), calls a tool (id 10): it reaches daemon `a`.
2. "Agent B" selects `b` (id 2) on the same connection.
3. Agent A calls again (id 11).

Result: id 11 reaches daemon `b`, is answered from `b` with no `isError`,
and the answer carries no `_meta` or text naming the project; daemon `a` is
half-closed as soon as it owes nothing. The JSON-RPC ids are the only thing
that differs between the two agents' frames, exactly as on a real shared
connection. The test asserts this buggy behaviour, labelled `KNOWN BUG`, so
the suite stays green; S2 replaces its assertions with the fix's.

Run: `cargo test -p g-mesh --lib shim::router::tests::known_bug`.

The live incident in GM-432's timeline (three subagents of session
`45462a7e` each calling `select_project` within 22 s) is the same mechanism;
there all three picked the same project, so nothing visible went wrong.

## Can the shim tell callers apart?

Known facts, from the Claude Code 2.1.286 binary
(`~/.local/share/claude/versions/2.1.286`, read with `strings`, not observed
on the wire):

- Every `tools/call` carries `params._meta["claudecode/toolUseId"]`, the id of
  that one tool use. It is unique per call and does not link two calls of one
  agent, so it cannot identify a caller.
- `claudecode/agentId` / `claudecode/agentType` exist, but are added only for
  the CLI-owned server named `claude-code-remote` (the gate compares the
  server name with that constant), and never for the main session. g-mesh
  does not receive them.
- JSON-RPC ids are allocated per connection by the client; they say nothing
  about the agent.

So **on one connection the caller is not knowable** today. Any fix that keys
on "who asked" must get the caller to say so in the call itself.

## Options

Costs: the stamp in option 2 is 43 characters for
`g-mesh: answered from project g-mesh-bench.`; the optional `project`
parameter in option 1 is about 142 characters of JSON schema per tool, times
8 project tools (`#[tool(` in `core/src/mcp/mod.rs`), in the tool list every
turn re-reads.

| # | Option | Meets acceptance? | Per-call / per-turn cost | MCP compatibility | Shared connection | Single-agent users |
|---|---|---|---|---|---|---|
| 1 | Optional `project` argument on every project tool; the shim routes the call to that project's daemon (several upstreams open at once) | Only for calls that pass it; a call without it still goes to the latest selection, silently | ~1.1 k chars of schema every turn; the argument on every call | Plain tool arguments | Fixes routing for agents that pass it | Pay the schema; nothing else changes |
| 1b | Same, but `project` required in multi-project sessions | Yes | Same schema plus an argument on every call; agents that forget get an error and retry | Plain arguments, but the schema differs between single- and multi-project sessions | Fixed | Single-project sessions keep today's schema |
| 2 | Every answer in a session that has switched names the project that answered it (a text line the model reads; `_meta` alone is not shown to the model) | Yes, the second branch ("states which project answered") | ~43 chars per answer, none in a single-project session | Content only; `_meta` key optional extra | Misroute is still possible but visible | A single-project session stays byte-identical (`single_project_frames_are_byte_identical` stays as it is) |
| 3 | After a switch, reject calls until the caller re-confirms with `select_project` | No: the shim cannot tell who re-confirmed, so B's own select counts as A's confirmation | One extra round trip after every switch | Fine | Does not separate agents | Harmless but noisy |
| 4 | `select_project` returns a session token that every call must carry | Yes, if required | Like 1b, plus an opaque value to copy | Plain arguments | Fixed | Pays for nothing it needs |
| 5 | Key the selection on Claude Code's `_meta` (agentId) | Not possible: g-mesh does not receive agentId; toolUseId is per call | None | Client-specific | — | — |
| 6 | One connection per subagent | Not ours to change: Claude Code owns the connection | — | — | — | — |

Option 3 is ruled out by the "callers are not knowable" fact; 4 is dominated
by 1b (same guarantee, an opaque value instead of a name the agent already
has); 5 and 6 are outside g-mesh.

## Recommendation: option 2

Every `tools/call` answer that the shim forwards from a sub-project upstream
gets a first text item naming the project that served it, relative to the
shim's root, e.g. `g-mesh: answered from project g-mesh-bench.` The shim
knows this as a fact: it is `Upstream::root` of the upstream the request was
recorded on (`InFlight::upstream`). Answers from the front (`select_project`,
`tools/list`) and every frame of a session that never switched are untouched.

Why: it meets the acceptance criterion with the smallest, local change (the
router alone), costs nothing for single-project users, and costs a multi-
project session about 43 characters per answer instead of a schema every turn
re-reads. The agent's guidance (`select_project`'s result) can tell it to
re-select when the line names a project it did not choose.

Main risk: the call is still *answered from the wrong project*; correctness
depends on the agent reading the line and re-selecting, and two agents that
keep alternating can ping-pong. Second risk: the shim must parse and
re-serialize each sub-project answer to add the line, where today it only
scans `id`/`method` - a CPU cost on large results that S2 should measure.

If the owner prefers a hard guarantee, option 1b is the one that delivers
it, at the cost of an argument on every call and a schema that depends on the
session type. Options 2 and 1 also combine: the stamp now, an optional
`project` argument later.
