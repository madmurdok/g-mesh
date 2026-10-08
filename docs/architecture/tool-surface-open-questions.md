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

**Known.** A hook cannot turn a grep into a g-mesh call. Hooks run on a tool
call, not instead of it; the only lever is to refuse with a reason. No firing
rate exists yet, so the trade cannot be computed. (This is a statement about
Claude Code hooks, not about this repository's code; it was not re-checked
here.)

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
- Two uses that fit an embedding model: ordering a page by similarity to the
  caller's query instead of only by graph centrality (GM-373 ranks rung 4 by
  inbound edges, a structural proxy), and disclosing a near-miss that GM-381
  found no floor can separate (`AppState` returns `createAppState` at 0.845).
  These two figures are carried from the conversation and were not re-checked
  for this note.
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
specific figures from the conversation, excalidraw `pointFrom` at
`limit: 200` returning a 51-row page over 46 files with `hasMore: true`, while
`files` lists 81 files in a quarter of the bytes, were NOT re-verified. Their
only source is the GM-352 task description (and
`docs/design/GM-352-rank-and-answer-shaped-responses.md`, which quotes it as
"the task description's own figures"). The code carries two related but
different numbers: `core/src/mcp/provenance.rs` says 51 rows at `limit: 200`,
and `core/src/graph/pagination.rs` (`MAX_FILE_TALLY` doc) says about 52
referencing files, which does not match 81. No excalidraw index was queried
for this note. Treat 51 rows, 46 files and 81 files as unverified until
someone runs the call.

**Closing observation.** From the log, the token size of every g-mesh result
in a session, split by tool, and for each large result whether the caller used
more than the aggregate would have held. Large paginated results that were
mostly reduced to a per-file or per-symbol answer: extend aggregation to
those tools (task). Otherwise a recorded null result. A re-run of the
excalidraw `pointFrom` call settles the figures above independently.
