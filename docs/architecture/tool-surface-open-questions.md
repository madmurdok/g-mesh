# Tool-surface open questions

Recorded for GM-387, from a design conversation on 2026-09-22.

This document shrinks. An entry leaves it in one of two ways: it becomes a
task (the observation came back and said "do this"), or it becomes a recorded
null result ("measured, and the answer was no"). An entry that only ever
stays is a wishlist, and should be deleted. The document asserts nothing about
what the tools do; it records what is unknown and what would settle it.

**The missing input, common to all four.** Every entry below needs a log of
real Claude Code sessions in which g-mesh is connected. The tool that records
it exists: GMB-177 (session-log hook and analyzer) is done, in g-mesh-bench
release-0.24.0. No such log exists yet. The hook is not wired into
`~/.claude/settings.json` (checked 2026-10-08: no GMB-177 or g-mesh-bench
entry there), and the only data collected so far was from a run in which
g-mesh reported `CONNECTION_CLOSED`, so it says nothing about tool use.
Wiring the hook and collecting sessions is the one step all four wait on.

**No behaviour change.** This note changes no code, so there is nothing to
test.

## 1. A combined call (batching a sequence in one round trip)

**Weighed.** One call carrying several tool calls saves per-turn framing.
It helps only where the next call does not depend on the previous answer.
Most tool use is adaptive, so the question is how much is not.

**Known.** The cheaper alternative has already worked once: fold a common
follow-up habit into the response. `find_references`' `files` array is that.
Verified: it is computed over the whole edge set, not the page. The tally is
built by `pagination::tally_edge_files_bounded` in
`handle_in_covered` (`core/src/mcp/find_references.rs`, around line 374) from
the anchor's id, the usage edge kinds and the optional `file_paths` scope,
with no cursor or page involved. `ReferencePage.files` documents the same
("computed over the whole edge set rather than this page"). It is capped at
200 entries and 8,000 bytes (`MAX_FILE_TALLY`, `FILE_TALLY_MAX_BYTES` in
`core/src/graph/pagination.rs`), and `files_truncated` says when a cap cut it.
Checked with g-mesh `get_file_outline` and `find_definition` on
`ReferenceParts::response`, plus a read of the call site.

**Closing observation.** In the session log, take every run of two or more
g-mesh calls in one assistant turn sequence and mark each follow-up call as
predetermined (its arguments could have been written before the first answer
came back) or adaptive (they used the answer). Then, for the predetermined
runs, check whether one extra response field would have removed the follow-up.
Many predetermined runs with no such field: build the combined call (task).
Few, or most removable by a field: record a null result and fold the habits
into responses instead.

## 2. Routing rules: prompt text or a hook

**Weighed.** The "Code search" section of `~/.claude/CLAUDE.md` is paid on
every turn of every session, including those where nobody searches. A
`PreToolUse` hook costs one wasted round trip only when it fires. The trade
is prompt cost per turn against the rate at which the hook fires.

**Known.** A hook cannot turn a grep into a g-mesh call. Per the Claude Code
hooks reference (https://code.claude.com/docs/en/hooks.md, PreToolUse), as
reported by the claude-code-guide agent on 2026-10-08 (the page itself was not
opened for this note): a `PreToolUse` hook can block a call with exit code 2 or
`permissionDecision: "deny"` plus `permissionDecisionReason`, which is fed back
to the model; and it can rewrite the same tool's arguments with `updatedInput`
(for example the Bash `command`), but it cannot change which tool runs. So the
levers are refuse-with-a-reason, or rewrite a Bash command that is itself still
Bash. Rewriting a grep command line into something else that Bash runs is
possible in principle; routing to an MCP tool is not. No firing rate exists
yet, so the trade cannot be computed.

**Closing observation.** From the log: the fraction of sessions with at least
one grep/search-shaped Bash or Grep call on code the index covers (the hook's
firing rate), and the number of turns per session. Then cost of the prompt
section = tokens of the section x turns, against cost of the hook = firing
rate x one refused round trip. Whichever is lower wins; the result is either a
task (move the rule) or a recorded null result (keep the prompt rule).

## 3. What the local model can and cannot take on

**Weighed.** Whether local compute can reduce what the caller reads or pays.

**Known.**
- The model is `jinaai/jina-embeddings-v2-base-code`, an embedding model:
  12 layers, hidden size 768, mean pooling (`core/src/embedding/model.rs`,
  module doc). It ranks, clusters and retrieves. It does not generate, so it
  cannot summarise, answer or reason. "Let the local model read the file so
  the caller does not" is not available with what ships.
- Licence: Apache-2.0 (`core/src/embedding/model.rs` module doc; `README.md`
  line 244 and the License section). The shipped default is the int8
  quantization, pinned to revision `516f4baf13dec4ddddda8631e019b5737c8bc250`
  (`README.md`; `docs/adr/0011-embedding-model-int8.md`).
- Cost anchor: GM-372 measured go-gin indexing at 142.9s with the model
  against 10.4s with `G_MESH_MODEL_DIR` empty, byte-identical dumps. Verified
  with `get_task GM-372` (completion summary). Caveat from that summary: it was
  a side observation made while re-indexing under load, reported and not
  filed, not a controlled benchmark. Per-call local compute is no cheaper.
- Two uses that fit an embedding model:
  - Ordering a page by similarity to the caller's query instead of only by
    graph centrality. Verified: GM-373 (`get_task GM-373`) ranks the
    file-name rung by inbound `REFERENCES`+`CALLS` count, descending, with
    the old `exported DESC, startLine ASC` as tie-break, in
    `graph::queries::find_in_file_named`. That is the file-stem rung, which
    `docs/architecture/symbol-resolution-ladder.md` (table row 4, and
    "Rung 4 ranks, it does not just page (GM-373)") numbers as rung 4. Its
    own stated bound: inbound edges measure internal use, so exported API a
    project barely uses ranks low.
  - Disclosing a near-miss that no floor can separate. Verified: GM-381
    (`get_task GM-381`, the task description's calibration, whose source is
    `g-mesh-bench/docs/results/v0.21.0-semantic-threshold-calibration.md`,
    not opened here) gives `AppState` returning `createAppState` at 0.845.
    The same figure, and the statement that "no threshold can separate
    those, because the wrong answer scores like a right one", are in
    `core/src/mcp/similarity.rs` (module doc, around line 170) and
    `core/src/mcp/find_definition.rs` (around line 894, which adds
    `ExcalidrawImperativeAPI` returning `App#createExcalidrawAPI` at 0.839).
    GM-381's completion summary says the same: a floor fixes "nothing
    matched" and cannot fix the near-miss.
- Rejected: clustering results and labelling the clusters. A label is a
  generated claim from a model that cannot generate.

**Closing observation.** In the log, for `find_*` pages whose ranking was
structural: how often the row the caller went on to open was not in the first
few rows but was in the page (a query-similarity order would have helped), and
how often a caller acted on a near-miss as if it were the requested symbol.
Frequent: a task for one of the two uses. Rare: a recorded null result.

## 4. Where the caller's tokens actually go

**Weighed.** A caller pays for payload, so local compute only helps if it
reduces what is sent. The proven way here is aggregation over pagination.
Whether that generalises beyond `find_references` is a measurement.

**Known.** The `files` tally exists and is whole-set (see entry 1). The
excalidraw `pointFrom` figures are NOT re-verified, and the sources disagree:
- GM-352's task description (`get_task GM-352`): at `limit: 200`, 51 rows
  spanning 46 files, `hasMore: true`; `files` lists all 81 referencing files.
  `docs/design/GM-352-rank-and-answer-shaped-responses.md` quotes it as "the
  task description's own figures".
- Commit ad33932 (`feat: answer file-level impact questions at file
  granularity`), the commit that introduced the tally, repeats it in its
  message: rows "top out at 51 of 86 edges over 46 files", `files` lists all
  81 files.
- The comment on `MAX_FILE_TALLY` in `core/src/graph/pagination.rs` (added by
  that same commit) says excalidraw's `pointFrom` is "~52 referencing files".
  No task is named there and no test or assertion backs it.
- `core/src/mcp/provenance.rs` (module doc) says only 51 rows at `limit: 200`.
The code backs neither file count: 51 rows appears in a comment, 52 files
appears in a comment, and 81 appears nowhere in the code. Both the 52-file
comment and the 81-file claim come from the same commit, so one of them is
wrong, and 52 may be a slip for the 51 rows. No excalidraw index was queried.
Treat the file count as unknown until someone runs the call.

**Closing observation.** From the log, the token size of every g-mesh result
in a session, split by tool, and for each large result whether the caller used
more than the aggregate would have held. Large paginated results that were
mostly reduced to a per-file or per-symbol answer: extend aggregation to
those tools (task). Otherwise a recorded null result. A re-run of the
excalidraw `pointFrom` call settles the figures above independently.
