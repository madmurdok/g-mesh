# GM-432: a `search_code` call with no answer for 30 minutes

Status: diagnosis and design (GM-432/S1). No production code changed in this slice.

## The incident

On 2026-09-26 the GM-398/S1 design agent (Claude Code session `45462a7e`,
subagent `a468555487c38d41b`) issued `search_code` at 15:02:43Z (18:02:43
local). Claude Code aborted it at 15:33:15Z with:

> MCP server "g-mesh" tool "search_code" sent no response or progress for
> 1815s; aborting.

The agent gave up on semantic search for the rest of its slice.

## Root cause

**The answer was lost in the shim, not computed late.** A sibling subagent in
the same Claude Code session called `select_project("g-mesh")` while the call
was in flight. Every `select_project` makes the shim open a new connection to
the project's daemon and half-close the old one, even when the project is the
one already served. The daemon's MCP service (rmcp 2.2.0) treats the
half-close as "client gone": it waits 5 s for in-flight handlers, then closes
the transport. `search_code` was waiting for the embedding backfill (18 more
minutes), so its response, and every progress notification it tried to send,
had nowhere to go. The shim does not track which request ids it forwarded on
which connection, so nobody answered the client's id. The only thing that
ended the call was Claude Code's own idle timeout.

Subagents share their parent's MCP connection, and the project's server
instructions tell every agent to call `select_project` first. So parallel
slices re-selecting the same project is the normal case, not an edge case.

### Evidence from 2026-09-26

Transcript timeline, all in session `45462a7e`. Every g-mesh call from 14:50Z
to 15:40Z was extracted from the subagent transcripts:

| time (UTC) | agent | call | outcome |
|---|---|---|---|
| 15:02:36.4 | a468 | `select_project g-mesh` | switch #1 |
| 15:02:36.8 | acc2 | `select_project g-mesh` | switch #2: connection U2 is now current |
| 15:02:43.6 | a468 | `search_code` "apply similarity threshold..." | sent on U2, waits for `Need::Embeddings` |
| 15:02:44.0 | a468 | `search_code` "load embedding ONNX model..." | queued by Claude Code behind the first (same assistant turn; MCP tools run serially per agent) |
| 15:02:58.8 | a8fa | `select_project g-mesh` | **switch #3: U3 is current, U2 half-closed** |
| ~15:03:04 | daemon | `could not send a progress notification for request 8: Transport closed` | rmcp's 5 s drain on U2 is over |
| 15:03:26.6 | a8fa | `search_code` on U3 | answered at **15:20:51.7** (1045 s), when the backfill finished |
| 15:02:54 to 15:05:11 | acc2 | 8 structural calls on U3 | each answered in about 2 s |
| 15:33:15.4 | a468 | first `search_code` | **aborted by the client after 1815 s idle** |
| 15:33:15.5 | a468 | second `search_code` | executed only now, answered in 0.1 s |

Daemon log for the g-mesh project (`~/.g-mesh/projects/959ade85d9a343b1/daemon.log`,
lines 329-582 by line number; the lines carry no timestamps except
rust-analyzer's):

- line 329, `index (re)initialized - a full reindex is needed`: the daemon of
  the 17:53 build started at 17:55 local and walked the project from scratch.
- line 398, `could not send a progress notification for request 8: Transport
  closed`: this lies between rust-analyzer lines stamped 18:02:57 and 18:03:11
  local, which is 5 to 12 s after switch #3.
- line 581, `embeddings [backfill]: 6771 texts, 745 cache hits, 6026 embedded,
  0 cache errors, 1406.0s`. `index.phase` was last written at 18:20 local,
  which matches a8fa's answer at 18:20:51.
- `~/.g-mesh/embedding-cache/cache.sqlite` was created 2026-09-26 17:57:22.
  This was the cache's first fill, so there were almost no cache hits. That,
  plus a loaded machine, is why the backfill took 23 minutes.

The daemon was healthy throughout. The call that went through U3 got its
answer at the moment the phase became `ready`. The lost call would have got
the same answer at the same moment, had its connection still been open.

### Reproduction

`repro.py` (GM-432/S1 scratchpad; the whole script is summarised here) runs the
incident's binary (`g-mesh/target/release/g-mesh`, built 2026-09-26 17:53)
through a real `mcp-shim`. Each arm uses its own `G_MESH_HOME` and
`G_MESH_EMBEDDING_CACHE=off`. The folder holds two candidates: `goproj`, a copy
of `plugins/go`, and `tiny`.

- **bug arm**: `select_project goproj`, then `search_code` (id 3, with a
  progress token), then `select_project goproj` again 2 s later.
- **control arm**: the same without the second `select_project`.
- In both arms, once `index.phase` reads `ready`, a fresh `search_code` (id 5)
  shows that the session itself still works.

Result: **reproduced**. It was run once on 2026-09-28. The load average was
about 370 at the start and 396 at the end; the harness's own `/usr/bin/time`
showed `real 365.8 user 0.31 sys 0.43`, so the harness only waited.

| | bug arm | control arm |
|---|---|---|
| `search_code` id 3 answer | **none**, 127 s after it was sent, including 60 s after `ready` | results at t=229.9 s |
| progress notifications for id 3 | 0 | 45 (every 5 s) |
| daemon trace for request 3 | `could not send a progress notification for request 3: Transport closed`, then `prepare: cancelled ... waited_ms=7007` | `wait over ... outcome=satisfied waited_ms=228867 progress_sent=45` |
| embedding backfill (368 texts, cache off) | 62.1 s | 221.5 s |
| fresh `search_code` id 5 after `ready` | answered in 0.0 s | answered in 0.1 s |

`waited_ms=7007` is the rmcp drain: 5 s after the second `select_project`
(sent at t=3 s), the daemon cancels the call's token. So the lost call is not
just unanswered; it is abandoned on the daemon side. The session itself keeps
working (id 5), which is exactly what the agent saw: later calls worked and
only the one in flight disappeared. The control arm differs only by the
second `select_project`, and its call is answered.

The backfill times differ between the arms (62 s against 221 s) because of the
machine's load, which swung between about 15 and 396 during the run. That
difference does not affect the verdict: the bug arm was still unanswered 60 s
after its own `ready`.

## Candidates ruled out

| candidate | evidence | verdict |
|---|---|---|
| The daemon was stuck or had crashed | Structural calls on U3 answered in about 2 s all through the window. The backfill completed and logged its summary. a8fa's `search_code` was answered at 18:20:51. | ruled out |
| A lock (`IndexStore`'s connection `Mutex`, `EmbeddingPipeline`'s `OnceLock` model or cache `Mutex`) held for the whole window | Any hold long enough to block `search_code` would also have blocked the structural calls, which take the same store mutex (`prepare` → `mark_used`, `handle` → `store.read()`), and a8fa's `search_code`. None was blocked. | ruled out |
| The embedding wait was never bounded | `wait_for_index` caps every wait at `DEFAULT_INDEX_WAIT_CAP` = 25 min (`core/src/mcp/mod.rs:100`) with a retryable "still being built" error. Here the wait itself would have ended at about 1088 s, when the phase became `ready`. | not the cause of the silence. The wait is still far too long for `search_code` (see the proposal). |
| CPU contention (GM-429 measurements, other daemons, load average above 200) | It stretched the backfill to 1406 s, together with the cold cache. It cannot explain why an answer that was ready at 18:20:51 was never delivered. | contributing (made the call be in flight for longer than 5 s), not the cause |
| Claude Code queued the call and never sent it | The daemon logged a progress-send failure for a waiting request, 5 to 12 s after switch #3. The companion call that Claude Code did queue ran at 15:33:15 and answered in 0.1 s. | ruled out |

## Call path (file:line, this branch)

| hop | where | waits / locks / timeouts |
|---|---|---|
| Claude Code → shim stdio | `core/src/shim.rs`, `core/src/shim/router.rs:214` `client_loop` | Client side: an idle abort after 1815 s without a response or progress, and a move to the background after 120 s (seen in other transcripts). Shim: no timeout. |
| shim routing | `router.rs:238` `on_client_frame`; `:270` `ClientFrame::Other => router.current()` | Forwards to the current upstream and keeps no record of forwarded ids. |
| select_project answer | `router.rs:284` `on_front_frame` → `:345` `switch` | Always connects anew, even to the project already served. **`:393` `previous.close(Shutdown::Write)`**. |
| upstream end | `router.rs:398` `upstream_ended` | **Answers only pending `select_project` ids (`:409`). Tool calls in flight on a non-current upstream are dropped silently.** |
| daemon MCP service | rmcp 2.2.0 `service.rs:1279` | On input EOF, drains in-flight responses for **5 s**, then closes the transport. |
| tool entry | `core/src/mcp/mod.rs:653` `search_code` → `:658` `prepare(.., Need::Embeddings)` | |
| prepare | `mod.rs:182` | `request_activation`, then `wait_for_index`, `mark_used` (store mutex) and `replay_queued_changes` (`spawn_blocking`, no timeout). |
| index wait | `mod.rs:384` `wait_for_index` → `core/src/daemon/indexing_status.rs:324` `wait_for` | `Need::Embeddings` is satisfied only by `Phase::Ready` (`indexing_status.rs:84`). Capped at 25 min (`mod.rs:100`, `G_MESH_INDEX_WAIT_CAP_MS`). Cancellable through `ctx.ct`. Progress every 5 s, only when the request carries a token. |
| handler | `core/src/mcp/search_code.rs:159` `embed_query` → `core/src/embedding/pipeline.rs:252` `model()` (`OnceLock::get_or_init`: model load on the first call, about 1 s) → ONNX `Session::run` | Synchronous, on the async worker. No timeout. |
| | `search_code.rs:167` `store.read()` → `core/src/storage/index_store.rs:198` | `Mutex<Connection>`, blocking, no timeout. |
| | `search_code.rs:169` `search` | Full scan of `vec_distance_cosine` over all vectors. |

g-mesh calls behind this table: `get_file_outline(search_code.rs)`,
`find_callers(mcp::search_code::handle)` (the only production caller is
`GMeshMcpServer::search_code`), `find_definition(embed_query)` (source
returned), `find_references(SWITCH_PROJECT_META)` (led to
`shim::router::Shared::on_front_frame`). `find_definition("IndexStore::read")`
and `find_definition("IndexingStatus::wait_for")` returned only
`semanticNeighbours` (they are inherent methods; the qualified names did not
match), so those two hops were read with grep and sed. `search_code` itself
was not needed.

## Proposal

Two fixes, because two separate things went wrong. The response was lost
(fix A, the bug), and the call had to be in flight for 18 minutes in the
first place (fix B, the bound).

### A. The shim never drops a forwarded request (fixes the incident)

- **A1 (dropped by the owner).** The proposal was to treat re-selecting the
  project already served as no switch: keep the upstream and answer with the
  guidance cached from the last switch. It contradicts Q9
  (`lazy-indexing.md`: reselecting the current project is a full switch, so
  the guidance is re-rendered for the project's state now). A2 and A3 cover
  the same-project case without it: the reselect connects anew, and the old
  upstream stays open until it has answered what it owes.
- **A2. Track in-flight ids per upstream.** `on_client_frame` records the id of
  every request it forwards, with the upstream it went to. Each upstream's
  reader removes the id when the response passes through. On a real switch,
  the old upstream stays open (no `Shutdown::Write`) until its pending set is
  empty, and is closed then.
  - This is bounded: every daemon wait already has a bound (fix B for
    `search_code`, the 25-min cap for structural tools).
- **A3. If an upstream ends with ids still pending, the shim answers each one
  itself.** It sends a tool error (`isError: true`):

  > g-mesh: this call was not answered: the connection to the daemon serving
  > `<root>` ended `<N>` s after the call was sent (`<reason>`). Nothing was
  > computed for it - call the tool again.

  `<reason>` is only what the shim knows: "the session switched to `<other
  root>`", or "the daemon closed the connection". This also covers a daemon
  that crashes or is replaced mid-call, which today loses the answer the same
  way.

### B. `search_code` answers within a bound, from what exists

Today `search_code` waits for `Phase::Ready` for up to 25 min.

- The bound lives in `wait_for_index` / `prepare` in `core/src/mcp/mod.rs`.
  `search_code` passes `Need::Structural` for the walk, so the walk keeps the
  shared cap and progress, exactly as the structural tools have them. Then it
  waits at most **`SEARCH_EMBEDDING_WAIT` = 20 s** (`G_MESH_SEARCH_EMBEDDING_WAIT_MS`,
  where `0` means don't wait) for `Phase::Ready`, and then answers from the
  vectors already stored.
- **Why 20 s.** It is well under Claude Code's 120 s move-to-background
  threshold and its 1815 s idle abort. It is long enough for a small project's
  whole backfill, or a file-change batch (the log shows 0.0 to 13.2 s per
  batch). A cold backfill of g-mesh took 1406 s, and no bound a caller would
  wait for covers that, so the honest answer is a partial one that says so.
- **Response while the backfill is not done.** It is a normal result, not an
  error. The rows come from the stored vectors, and the page adds
  `"partial": {"embedded": N, "total": M}` plus a leading text note built only
  from `IndexingStatus::embed_progress()`:
  - `(N, M)` with `M > 0`: "g-mesh: the embedding pass for `<root>` is still
    running (embeddings N of M computed, this call waited 20 s). Only symbols
    embedded so far were ranked; a symbol missing from these results may
    simply not be embedded yet. Call again later for complete results."
  - `M == 0` (still counting): "... the embedding pass has started but has not
    yet counted what needs embedding; `<stored>` symbols from an earlier pass
    were ranked."
  - `Phase::Structural` (pass not started): "... the embedding pass has not
    started yet; ..." with the stored-vector count.
- **The `no_match` verdict** (`similarity::verdict`) is suppressed on a
  partial page. "Nothing close" is not a known fact while vectors are missing.
- **Risks.**
  - A partial page can mislead an agent that ignores the note. The note leads
    the text, and the JSON carries `partial`.
  - A first page and a continuation cursor taken across the phase change may
    rank different sets. Cursors issued on a partial page should be refused
    once the phase changes (a cursor carries the vector count).
- **Also recommended, lower priority.** `search_code::handle` runs `embed_query`
  and `store.read()` synchronously on the async worker (the runtime has two).
  Move it to `spawn_blocking` so a slow inference cannot stall other sessions'
  calls. This was not the cause here.

## Test plan (for the implement and verify slices)

1. **Shim, unit (`core/src/shim/router.rs` tests).** Use the existing
   pipe-based harness (`single_project_frames_are_byte_identical`) with a
   `Connector` that hands out scripted fake upstreams.
   - a. Select A, forward `tools/call` id 7 to U1 (which does not answer
     yet), select A again. Assert a second connect (U2), that U1 is not
     half-closed while id 7 is pending, and that U1's late answer for id 7
     reaches the client. The fake daemon closes its side on a half-close, as
     rmcp does. Control: restore `previous.close(Shutdown::Write)` and drop
     the pending-id tracking. U1 is half-closed at once and id 7's answer
     never arrives (read with a 2 s bound; the test fails).
   - b. Select A, id 7 on U1, select B. Assert U1 is not half-closed while id 7
     is pending, and that U1's late answer reaches the client. Then close U1
     with id 8 pending and assert the client receives the A3 error for id 8
     naming the switch. Control: restore `previous.close(Shutdown::Write)` and
     drop the pending-id tracking. No frame for id 7 or id 8 arrives within
     2 s, and the test fails.
2. **Daemon, bounded wait (`core/src/mcp/semantic_pending_tests.rs` style).**
   Use an `IndexingStatus` in `Phase::Embedding` with `embed_progress` (3, 10),
   a store with 3 vectors, and `G_MESH_SEARCH_EMBEDDING_WAIT_MS=200`.
   - Assert `search_code` returns within a 5 s `tokio::time::timeout`, with 3
     rows, `partial = {3, 10}`, the note text, and no `no_match`.
   - Second case: flip the phase to `Ready` 50 ms in. Assert a full answer with
     no `partial`.
   - Control: pass `Need::Embeddings` as today. The call does not return within
     5 s (the test's timeout fires), which fails the first case.
3. **End to end (optional, `core/tests/multi_project_front.rs`).** Take the
   repro's bug arm with the toy plugin slowed past 5 s, using the structural
   wait so it does not depend on a model. Assert the in-flight id is answered.
   Control: the unfixed shim leaves it unanswered within a 30 s bound.
