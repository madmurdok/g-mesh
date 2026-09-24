# Lazy indexing (GM-395)

Status: **design, awaiting owner review.** No code for it has been written yet.
Branch: `feat/GM-395-lazy-indexing`, cut from `release-3.11.1`.

The work is carried out in phases. Each phase is done by an agent that knows
only this document, so the document says everything that phase needs.
Line numbers refer to the tree as of `release-3.11.1` with GM-393, GM-394 and
GM-396 merged. Treat them as a starting point for search, since a slice that
runs earlier will move them.

## 0. Problem and acceptance criteria

Today the MCP shim (`g-mesh mcp-shim`) starts a daemon for the client's cwd,
and that daemon's startup runs a full bulk index, embeddings included, at once.
Launched from a directory that is not really a project (seen with
`~/Projects/ClaudeProjects`, a parent folder of about a dozen repos and
worktrees), that meant many minutes at 200%+ CPU for a session that might
never call a g-mesh tool.

Acceptance criteria (verbatim from the tracker), with the slice that proves each one:

| # | Criterion | Proved by |
|---|---|---|
| A1 | Starting a session and not calling any tool leaves no bulk index running (verified by process CPU/ps after connect). | Slice 2 test, measurement M3 |
| A2 | Right after the handshake, with no index built, the agent can see the root and - for a folder of several projects - the list of candidate sub-projects, and select which to index; verified on a multi-project fixture. | Slices 2 (root line), 4, 5 |
| A3 | Candidate detection is bounded (shallow, marker-based) and measured on a large folder. | Slice 4 unit tests, measurement M4 |
| A4 | The first structural tool call on an unindexed project blocks until the structural index is complete and returns a full answer; progress notifications are sent during the wait; behaviour against Claude Code's tool timeout is measured and recorded. | Slices 2, 3, measurement M1 |
| A5 | Structural tools do not wait for embeddings; search_code has a defined behaviour while embeddings are still running. | Slice 1 |
| A6 | Existing up-to-date index is used without a re-index. | Slice 2 test |
| A7 | Design decision recorded in docs/architecture. | This file |

---

## 1. Current behaviour

### 1.1 Startup path: shim to daemon to bulk index to ready

**Shim** (`core/src/shim.rs`)

1. `run` (`shim.rs:92`) calls `resolve_project_root` (`:106`). The root is
   `$CLAUDE_PROJECT_DIR` if set and non-empty, otherwise the cwd. The root is
   not validated or inspected in any way.
2. `connect_or_bootstrap(root)` (`:146`) connects to
   `~/.g-mesh/projects/<hash(root)>/daemon.sock`. If no current daemon is
   serving, it takes `bootstrap.lock`, retires an outdated daemon (build
   stamp) or evicts a wedged one, then calls `spawn_detached_daemon(root)`
   (`:216`, `:422`) and waits up to `BOOTSTRAP_TIMEOUT` = 10 s (`:28`) for the
   socket.
3. `proxy(stream)` (`:469`) is a dumb, JSON-unaware NDJSON frame pump with two
   threads: stdin to daemon, and daemon to stdout. The shim never parses a
   frame. One shim is one MCP session is one daemon connection.

**Daemon** (`core/src/daemon/mod.rs`, `pub fn run`, `:327`). This is the path
that makes indexing eager. Everything below runs at process start, whether or
not anyone ever asks a question:

| Line | Step | Eager cost |
|---|---|---|
| `:340` | `acquire_singleton_lock` | none |
| `:384` | `manifest::discover` (reads `plugin.toml` files) | tiny |
| `:386` | `connection::open(root)`: creates/opens `index.db` | tiny |
| `:394` | `schema::ensure_current(conn, indexer_version(&discovered))`: **wipes the index** if the schema or generation changed | DROP of a large DB |
| `:403` | `last_used::touch` | tiny |
| `:408` | `needs_bulk_index = !schema::bulk_index_completed` (`meta.bulkIndexedAt`) | tiny |
| `:419` | `needs_semantic_pass_retry` | tiny |
| `:456` | `ipc::Listener::bind`: from here a shim's connect succeeds | none |
| `:499` | `EmbeddingPipeline::load`: stores the config only; the ONNX model loads lazily on the first `compute` | none |
| `:539` | `IndexingStatus::indexing()` if a walk is owed, else `ready()` | none |
| `:557` | accept loop thread (`serve_forever`, `:867`; a 2-worker tokio runtime at `:878`; `mcp::serve_connection` per connection) | none |
| `:583` | `ProjectWatcher::new(root)`: **recursive OS watch of the whole root.** On Linux (inotify) that is itself a walk of every directory | eager, and large on a parent folder |
| `:601-620` | **`bulk_index::run`**, then `indexing.mark_ready()`, then `schema::record_bulk_index` | the eager bulk index, **embeddings included** |
| `:682` | `semantic::run_with_registry`: whole-project semantic pass; spawns each language's plugin and type-checks | eager, heavy |
| `:699` | the same pass again, on a *restart*, when an earlier pass was interrupted (`needs_semantic_pass_retry`) | eager, heavy, **even for an already-indexed project** |
| `:737` | watcher consumer thread (debounce, then plugin round trips) | none |
| `:757` | `lifecycle::supervise`: idle timers and orphan check | none |

A failed `bulk_index::run` is fatal: `?` at `:602` makes the daemon exit, and
every connected session goes with it. That was chosen on purpose (`:588-598`).

**Bulk index** (`core/src/daemon/bulk_index.rs`)

- `run` (`:138`) walks every discovered language in order through
  `walk_one_language` (`:245`). That function spawns `<plugin> --bulk-index <root>`
  and `ingest`s its NDJSON stdout (`:366`) in batches of `BATCH_ITEMS` = 2,000
  (`:59`). After every language, it links imports and symbols (`:218-226`).
- `commit` (`:436`) runs **`embedding.compute(batch)` for every batch, inside
  the walk** (outside the SQLite mutex since GM-394), then `apply_diff` and
  `embedding.store` under the lock. **Embeddings are therefore interleaved
  with the structural walk.** No structural-only phase exists that could
  finish first. On g-mesh itself the walk takes about 31 s structural-only and
  about 814 s with embeddings (GM-393).
- Test knobs already in place: `WALK_DELAY_ENV` (`G_MESH_BULK_INDEX_DELAY_MS`,
  `:79`), `WALK_HOLD_FILE_ENV` (`G_MESH_BULK_INDEX_HOLD_FILE`, `:100`, which
  holds `run` open after the last batch and before it returns), and
  `HOLD_LOCK_FILE_ENV` (`:470`, which holds the SQLite lock inside `commit`).
- Other callers of the same walk: `cli::init` (`cli/init.rs:226`),
  `cli::reindex` (`cli/reindex.rs:105`), both in the foreground and in
  process, and `daemon::workspace_reindex` (`:348`, which re-walks one
  language through `walk_one_language`).

**Readiness** (`core/src/daemon/indexing_status.rs:154-262`)

`IndexingStatus(Arc<Inner { indexing: AtomicBool, ready: Notify }>)`. It has
one bit, and that bit flips at `mod.rs:619`, once the walk (with its
embeddings) has finished and before the semantic pass. `wait_until_ready`
(`:254`) has no timeout. `wait_ready(timeout)` (`:229`) is unused outside
tests.

**MCP layer** (`core/src/mcp/mod.rs`, rmcp **2.2.0**, `core/Cargo.toml:107`)

- Every tool handler (`:426-528`) starts with `self.prepare().await` (`:182`),
  which calls `still_indexing` (`:316` = `indexing.wait_until_ready()`, with no
  timeout and no progress), then `mark_used`, then `replay_queued_changes`.
  `find_definition`, `get_file_outline` and `get_dependencies` then also run
  `ensure_file_fresh` (`:265`).
- `search_code` (`:522`) waits on the same single bit, so today it waits for
  embeddings. Every structural tool waits for them too.
- `get_info` (`:533`) calls `instructions()` (`:400`). While `is_indexing()`,
  it returns capabilities-only text prefixed with `instructions::INDEXING_NOTE`
  (`mcp/instructions.rs:122`, 85 bytes). It never takes the SQLite lock while
  indexing (GM-394).
- rmcp spawns a task per request (`rmcp-2.2.0/src/service.rs:1113`), so a
  waiting tool call blocks neither the handshake nor other requests. That was
  proved by `tests/handshake_independent_of_indexing.rs`.

### 1.2 Every place that assumes eager indexing

| Place | Assumption |
|---|---|
| `daemon/mod.rs:601-705` | Walk, semantic pass and semantic retry run at startup |
| `daemon/mod.rs:583` | Watcher registered at startup |
| `daemon/mod.rs:602` (`?`) | A walk failure kills the daemon. Under a lazy trigger that would kill the session of the call that triggered it |
| `daemon/mod.rs:539` | Readiness is decided once at startup and only ever goes `indexing` to `ready` |
| `mcp/mod.rs:316`, `:403` | One bit, "indexing", which implies "a walk is running" |
| `mcp/instructions.rs:122` (`INDEXING_NOTE`) and `P4_*` (`:275-280`, `:395-401`) | "is being built right now"; "a tool call waits for the walk to finish" |
| `cli/status.rs:583-597` | "live daemon + `!bulk_indexed`" is rendered as "building now - first walk in progress". Under lazy indexing it can also mean "idle, waiting for the first call" |
| `cli/agent_instructions.rs:55` | "bootstraps and indexes a project automatically on its first tool call" (still true, and more accurate after this change) |
| `cli/init.rs` module doc | "the first MCP call ... bootstraps a detached daemon [whose] cold start ... walks the project" |
| `tests/common/mod.rs:126-205` `wait_until_indexed` | Polls `meta.bulkIndexedAt` with no tool call, assuming the daemon walks by itself. **23 test files** call it |
| `tests/serving_while_indexing.rs`, `tests/handshake_independent_of_indexing.rs`, `tests/daemon_missing_plugin_binary.rs` | Assume the walk starts at daemon start, and that a failed walk exits the daemon |
| `tests/embedding_generation_pipeline.rs` (`#[ignore]`, needs real weights) | Assumes a finished walk has embeddings |

### 1.3 Index reuse (how "up to date" is decided today)

- **Generation**: `schema::ensure_current` (`storage/schema.rs:485`) compares
  `meta.schema_version` with `CURRENT_SCHEMA_VERSION` ("8"), and
  `meta.indexer_version` with `registry::indexer_version(&discovered)`. The
  latter combines core's `CURRENT_INDEXER_VERSION` ("2") with every
  discovered plugin's build fingerprint. On any mismatch, `reset` wipes
  everything, including `bulkIndexedAt`. **So an index built by an older
  version is re-indexed only if its schema or generation differs.** A core
  binary bump alone (the daemon build stamp) does not invalidate the index.
- **Completion**: `meta.bulkIndexedAt` (`schema.rs:555`
  `bulk_index_completed`) is set by `record_bulk_index` (`:715`) only once every
  present language has `language_state.bulkIndexedAt`. A killed walk leaves it
  unset, so the next start walks again (every batch is an upsert).
- **Semantic pass**: `meta` roll-up plus `language_state.semanticPassAt`
  (`semantic_pass_completed`, `:735`).
- **Per-file freshness**: `indexed_files(filePath, mtimeMillis, contentHash)`,
  read by `watcher::staleness`. Nothing re-walks a current index at restart.
  Edits made while no daemon ran are caught only per file, by
  `ensure_file_fresh`, and only for the three file-anchored tools.
  `find_references` and the other symbol-anchored tools can read stale rows
  until the watcher or a file-anchored call touches that file. **This is an
  existing gap. GM-395 does not change it**; it is noted so nobody mistakes
  it for a regression.
- **Embeddings**: no marker exists, and **no backfill exists**. A node
  missing a `vectors` row (the model was fetched after indexing, or one
  inference failed) stays unembedded until its file is reparsed.

### 1.4 Facts that shape the design

- MCP instructions: Claude Code cuts them at 2 KB.
  `INSTRUCTIONS_BYTE_CEILING` = 1,900 (`instructions.rs:109`). The
  eight-language worst case of `build()` is 1,780 bytes **without**
  `INDEXING_NOTE`. With the note and its `\n\n` it is about 1,867, which
  leaves about 33 bytes. No test asserts the indexing branch against the
  ceiling. (It renders every *discovered* language, which is four bundled
  ones today, so the realistic figure is lower.)
- rmcp 2.2.0 API for progress:
  - A `#[tool]` fn can take `ctx: RequestContext<RoleServer>` as an extractor
    (`handler/server/common.rs:144`). Do not also take the `Meta` extractor:
    it *moves* `meta` out of the context (`:204-213`).
  - `ctx.meta.get_progress_token() -> Option<ProgressToken>` (`model/meta.rs:261`).
  - `ctx.peer.notify_progress(ProgressNotificationParam::new(token, progress)`
    with `.total`/`.message` set) (`service/server.rs:481`, `model.rs:1136`).
    `ProgressNotificationParam` is `#[non_exhaustive]`, so build it with `new`
    and then assign its fields.
  - `ctx.ct` is a `CancellationToken`, cancelled on `notifications/cancelled`.
  - `CallToolResult` has `meta: Option<Meta>` (`model.rs:2942`), serialized
    as `_meta`.
  - `ToolRouter::list_all()` (`handler/server/router/tool.rs:582`), and
    `GMeshMcpServer::tool_router()` is an associated fn. The tool schemas
    can therefore be listed without a live `GMeshMcpServer`.
- Claude Code timeouts. **Documented**, from code.claude.com/docs/en/mcp and
  /env-vars, fetched 2026-09-24; installed client 2.1.281:
  - `MCP_TIMEOUT`: server **startup** timeout, default 30 s.
  - `MCP_TOOL_TIMEOUT` / per-server `"timeout"` in `.mcp.json`: a hard
    **wall-clock** limit per tool call, default about 28 h. *"progress
    notifications from the server don't extend it."*
  - **Idle timeout**: *"A tool call to an MCP server that sends no response and
    no progress notification for the idle window aborts with an error"*.
    30 min for stdio servers (v2.1.203+), set by
    `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` (ms; `0` disables it). A per-server
    `timeout` ≥ 1000 raises the idle window to at least that value.
  - **Auto-backgrounding**: a main-conversation call still running after 2 min
    (`CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS`) becomes a background task, and the
    result arrives later as a task notification. Subagent calls and headless
    runs are never backgrounded.
  - **Not documented, must be measured**: whether Claude Code puts
    `_meta.progressToken` on `tools/call` at all. Without a token the server
    may not send progress, and then the idle window cannot be reset. Also
    unmeasured: whether a progress notification really resets the idle timer
    in practice.
- Observed while writing this document: this very Claude Code session reported
  `g-mesh (CONNECT_TIMEOUT): connection timed out after 30000ms`. The cause
  was not investigated. It is a reminder that the handshake is on a 30 s
  budget (`MCP_TIMEOUT`), so nothing added before the socket bind may be
  unbounded.

---

## 2. Decisions

### D1. Where laziness lives: the daemon starts idle; the shim is unchanged in when it spawns

- **Option A, the shim does not spawn a daemon until a `tools/call`.** The
  shim would have to answer `initialize` and `tools/list` itself (server info,
  protocol version negotiation, instructions, all tool schemas) by hand, as
  raw JSON. That duplicates rmcp and `instructions::build` in a second place
  that can drift from them. The only saving is one idle process.
- **Option B, the daemon starts idle (chosen).** The shim keeps spawning the
  daemon on connect exactly as today. `daemon::run` still does the cheap
  startup (lock, discovery, open DB, generation check, bind, accept loop), but
  **no walk, no semantic pass or retry, no embedding pass, and no watcher for
  an unindexed project.** The owed work runs on a dedicated *activation*
  thread when a tool call first asks for it (D2).
- **Why B:** the handshake needs rmcp, the manifests' capabilities and the
  index state, and all of those already live in the daemon. An idle daemon
  costs one process, an open SQLite handle and a bound socket, with no
  plugin, no ONNX model (already lazy, `mod.rs:482-499`) and no CPU. It also
  keeps the design to one daemon per root.
- **Risk:** the generation check still runs at startup, so an invalidated
  index is dropped even if no tool is ever called. That is acceptable: the
  check is cheap, and a stale index is unusable anyway. DROP TABLE on a very
  large DB can take seconds inside the 10 s bootstrap budget. If M3 shows
  this to be a problem, move the `reset` into activation (a follow-up, not
  in scope).

### D2. Activation: any index-needing tool call triggers it; it runs once, independently of the caller; failure is reported, not fatal

- A new module, `core/src/daemon/activation.rs`, owns what `mod.rs:583-750`
  does today: register the watcher, walk, mark structural, record
  `bulkIndexedAt`, run the semantic pass, start the watcher consumer, run the
  embedding pass (D3) and mark ready. It runs on a dedicated `std::thread`
  that parks until it receives a trigger.
- **Trigger:** `IndexingStatus::request_activation()`, called by every tool
  handler's `prepare` (a compare-and-swap, so concurrent calls from several
  sessions start it exactly once). An explicit trigger from outside MCP
  already exists: `g-mesh init` and `g-mesh reindex` run in the foreground
  and in process, unchanged in role.
- **Independent of the caller:** a triggering call that is cancelled or
  times out, or whose session disconnects, does **not** stop the walk. The
  next call finds it running or finished.
- **Failure:** today a failed walk exits the daemon. Under a lazy trigger
  that would drop the session of the very call that asked. New behaviour: the
  phase becomes `Failed(message)`; every waiter returns a **tool error**
  carrying that message (for example `missing_plugin_binary_hint`); the next
  tool call re-requests activation, which retries the walk. The original
  rationale ("a daemon that stayed up would hold the lock while serving
  nothing, and every later shim would reuse it forever") no longer applies,
  because the daemon now says why on every call and retries. That is better
  than a session that silently disappears.
  - Embedding-pass failures stay best-effort, as today (`commit`'s
    `store`). They never produce `Failed`.
- **Rejected:** starting activation on `initialize`. That would bring back
  exactly the cost this task removes.

### D3. Two-phase readiness; embeddings leave the walk and become a separate pass

Phases, in a new `IndexingStatus` (`daemon/indexing_status.rs`), backed by an
`AtomicU8` plus a `Notify` plus a small `Mutex` for the failure message and
progress:

```
Unindexed ──request──▶ Walking ──walk+link ok──▶ Structural ──(activation continues)──▶ Embedding ──▶ Ready
    ▲                     │                          ▲
    └──── request ──── Failed(msg) ◀── walk error    └── initial phase of an already-walked index
```

- `Unindexed`: the startup phase when `!bulk_index_completed`.
- `Structural`: the startup phase when the index is already walked. It also
  covers "embedding pass owed but not yet running". Structural tools do not
  wait here. They call `request_activation()` (which starts any owed
  semantic retry and the embedding pass in the background) and answer at
  once.
- `Embedding`: the embedding pass is running. Structural tools answer.
  `search_code` waits.
- `Ready`: everything done.
- API: `phase()`; `request_activation() -> bool`;
  `async wait_for(need: Need, deadline: Option<Instant>) -> WaitOutcome`, with
  `Need::Structural` satisfied by `Structural`, `Embedding` or `Ready`, and
  `Need::Embeddings` satisfied by `Ready`. `WaitOutcome` is
  `{ Satisfied, Failed(String), TimedOut }`. Keep the lost-wakeup-safe
  pattern the current code uses (`notified()` is created before the phase
  check), and loop, because a phase can move several steps.
- Progress counters, in the same struct: `items_ingested: AtomicU64`,
  `languages_done/total: AtomicU32`, `current_language`,
  `embed_done/embed_total: AtomicU64`, and `phase_since: Instant`. `ingest`
  and the embedding pass increment them.

**Splitting the embedding work out of the walk:**

- `bulk_index::run`, `walk_one_language`, `ingest` and `commit` take
  `embedding: Option<&EmbeddingPipeline>`. The cold-start walk passes `None`,
  so it becomes structural-only (about 31 s on g-mesh instead of 814 s).
  `workspace_reindex` keeps passing `Some` (unchanged behaviour, since it is
  an incremental re-walk of one language).
- New `core/src/embedding/backfill.rs`, `pub fn run(conn: &Mutex<Connection>,
  embedding: &EmbeddingPipeline, progress: &IndexingStatus) -> BackfillSummary`:
  - If `embedding.is_available()` is false (a new cheap check: model files
    present, without loading), return immediately. `Ready` then follows, and
    `search_code` keeps its existing "semantic search unavailable" error.
  - Total: `SELECT COUNT(*) FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id
    WHERE v.nodeId IS NULL AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL)`.
  - Keyset pagination over `nodes.id > last_id ORDER BY id LIMIT 256` with the
    same `LEFT JOIN ... IS NULL` filter. Read under the lock, build a `Diff`
    with only `upsert_nodes`, and release the lock. Then
    `embedding.compute(&diff)` outside the lock, and `embedding.store(&conn,
    &computed)` under it. `store` already re-checks staleness (GM-396), so
    races with watcher reparses are safe.
  - Keyset (not "while any unembedded row remains") guarantees termination
    even when a node's inference fails or its text trims to nothing.
  - Test knob `G_MESH_EMBED_PASS_HOLD_FILE`: while that file exists, hold
    **before** the first batch. The hold is independent of whether a model is
    present, so phase-gating tests need no weights.
- **No schema or indexer-version bump.** The graph is unchanged, so existing
  indexes stay valid (A6). "Embeddings complete" is recorded nowhere: the
  backfill pass *is* the check, and on a fully embedded index it is one
  `COUNT(*)` that returns 0. A side effect worth having: it closes the
  "model fetched after indexing" gap from §1.3.
- `cli::init` and `cli::reindex` call walk(None), then semantic pass, then
  `backfill::run` in the foreground. One path, same postcondition as today:
  structural, semantic and embeddings all present.

### D4. `search_code` while embeddings are running: **it waits** (with progress and the D7 cap)

- **Partial results with a flag.** Rejected. Similarity ranking over a
  partial set silently drops the best match. An agent (see the user's own
  CLAUDE.md on trusting `hasMore: false`) treats a page as complete, and a
  flag in the payload is exactly what gets ignored. It also contradicts the
  GM-394 owner decision (never answer partially or "not ready").
- **Immediate "not ready" error.** Rejected. It pushes a retry loop onto the
  agent, and the instructions explicitly tell the agent not to abandon a slow
  call.
- **Wait (chosen).** This is consistent with the structural tools. Because
  the embedding pass starts automatically right after the structural phase
  (D5), the wait is usually what is left of it, not all of it. A model that
  is not available makes the pass a no-op, so `search_code` answers at once
  with its existing "unavailable" error.
- **Risk:** on a large project the first `search_code` can wait more than
  10 min (814 s on g-mesh). Past 2 min Claude Code backgrounds the call (main
  conversation), and progress keeps the 30 min idle window alive. The D7 cap
  bounds the remaining risk.

### D5. When the embedding pass starts: right after the structural phase and the semantic pass, as part of the same activation

- **On the first `search_code` only.** This is cheaper when nobody searches,
  but the first search then pays the whole 13+ min.
- **Chosen: immediately after structural and semantic.** A structural call
  proves the session uses g-mesh, and the reported problem was CPU spent for
  sessions that never call anything. Owner question Q1.

### D6. Progress notifications: a heartbeat while a call waits, only when the request carries a token

- Every `#[tool]` fn gains `ctx: RequestContext<RoleServer>`, and
  `prepare(&ctx, need)` replaces `prepare()`.
- If `wait_for` would block and `ctx.meta.get_progress_token()` is `Some`,
  run a `tokio::select!` over three branches: the wait, a
  `tokio::time::interval(PROGRESS_INTERVAL)` ticker, and `ctx.ct.cancelled()`.
  - The default `PROGRESS_INTERVAL` is 5 s, overridable with
    `G_MESH_PROGRESS_INTERVAL_MS`; `0` disables progress, which the M1 "off"
    arm uses.
  - On each tick, `ctx.peer.notify_progress(...)`. Send failures are logged
    and ignored.
- **What is sent:**
  - `progress` = seconds waited so far (strictly increasing, as the spec
    requires).
  - `total` = `None`.
  - `message` = the real counters, for example `"indexing /abs/root: walking
    rust (2/4 languages), 48,210 symbols so far"`, `"linking imports"`,
    `"embeddings 12,400/40,113"`.
- **Why seconds rather than work units:** work counters stall during
  linking, the semantic pass and the one-time model load, and a notification
  whose `progress` does not increase violates the spec. The only guarantee
  the client needs is liveness; the numbers belong in `message`.
- **What is measurable during a bulk index:**
  - Walking: items ingested (nodes and edges, counted in `ingest`) and
    languages done out of total. The file total is unknown up front, because
    the plugin walks by itself; a pre-count would be a second walk.
  - Linking: phase name only.
  - Embedding: done out of total, exact (the backfill counts first).
- **No token** (possibly Claude Code's case, see M1): send nothing, because
  the spec ties progress to a requester's token. D7 is the guard.
- **Cancellation:** `ctx.ct` cancelled returns `ErrorData` at once (the
  client has already gone). Indexing continues (D2).

### D7. Wait cap and the "still indexing" answer

- **Why a cap is needed at all:**
  - The wall-clock limit (per-server `timeout`) is not extended by progress
    (documented), and the server cannot see it.
  - Without a progress token, the 30 min stdio idle window applies to the
    whole wait.
  - A call killed by the client gets a client-side error that says nothing
    about indexing.
- `INDEX_WAIT_CAP`: default **25 min**, overridable with
  `G_MESH_INDEX_WAIT_CAP_MS`. That is under the 30 min stdio idle default,
  so even a call without a token ends before Claude Code aborts it. Once M1
  has run, the default may change (§5).
- On `TimedOut`, the tool returns `CallToolResult::error` with text such as
  `"g-mesh: the index for /abs/root is still being built (walking rust, 2/4
  languages, 18m elapsed). No answer was computed - call this tool again; the
  index keeps building in the background."`.
- **Why this does not break "never a partial answer":** the call answers no
  part of the question. It carries no rows, no `hasMore`, nothing that could
  be read as a result. It is an explicit, retryable precondition failure. The
  rule it replaces forbade *answering off a half-built graph*, and this does
  not do that. It only fires in the pathological case (a walk longer than
  25 min, or a client that kills calls sooner). The normal path still blocks
  and answers.

### D8. The watcher

- **Unindexed project:** no watcher at startup. Activation registers it
  **before** the walk (keeping the GM-250 "no unobserved window" ordering) and
  starts the consumer after the walk and the semantic pass. This is the same
  sequence as today, moved.
- **Already-walked project:** register it at startup as today, because it
  keeps a live index fresh and costs nothing on macOS (FSEvents). The
  consumer starts at startup, **unless** a semantic retry is owed; then
  activation starts it after the retry. Events queue in the watcher's channel
  in the meantime, which is the same guarantee as today's cold start.
- **Risk:** on Linux, registering the watcher for an already-walked project
  still costs an inotify walk at startup. That is unchanged from today, and
  not a GM-395 regression.

### D9. Reusing an existing index

This is unchanged in rule (§1.3), with two clarifications:

1. **A startup never re-walks an index that has `bulkIndexedAt` set and a
   current generation.** The startup phase is `Structural`, and a structural
   call answers at once. `Structural` triggers only the owed semantic retry
   and the backfill, and the backfill is a no-op on a fully embedded index.
2. **An index built by an older g-mesh** is re-walked only if its schema
   version or generation (core indexer version plus plugin fingerprints)
   differs. GM-395 must **not** bump either.

### D10. Multi-project detection (cheap, bounded, marker-based)

New module `core/src/daemon/candidates.rs`:
`pub fn detect(root: &Path, limits: Limits) -> Detection`.

- **Markers:** `.git` (directory = a repo; *file* = a worktree or submodule),
  `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`. Each one is a
  `stat` of `<dir>/<marker>`, never a listing.
- **Root is a single project** (normal mode, as today but lazy) if any of
  these holds:
  1. the root has a marker itself (a monorepo such as g-mesh, with nested
     `plugins/*/package.json`, stays single);
  2. the root already has a completed index (`bulkIndexedAt` set; open
     `index.db` read-only if it exists; never create it). See Q2;
  3. fewer than 2 candidates are found. See Q3.
- **Walk:** breadth-first from the root.
  - `max_depth` = 2: the root's children and grandchildren. That covers
    `ClaudeProjects/g-mesh` and `ClaudeProjects/torpeek-worktrees/<wt>`.
  - **Do not descend into a candidate:** once a directory has a marker, its
    interior is not listed. That keeps nested repos and packages out, and is
    what keeps the walk cheap.
  - Skip, by name: every dot-directory (only `.git` is probed, as a marker),
    `node_modules`, `target`, `dist`, `build`, `out`, `vendor`, `.venv`,
    `venv`, `__pycache__`.
  - Never follow symlinks.
  - `max_entries` = 5,000 directory entries read in total, and
    `max_candidates` = 64. Hitting either sets `truncated: true` and keeps
    what was found.
- **Output:** `Detection { mode: Single|Multi, candidates: Vec<Candidate {
  rel_path, abs_path, markers: Vec<&'static str>, is_worktree: bool }>,
  entries_read, elapsed, truncated }`, sorted by `rel_path`.
- **Where it runs:** in `daemon::run`, after the singleton lock and build
  stamp and **before** `connection::open`, because front mode must not create
  an `index.db` for the root. Its cost is bounded by `max_entries`
  (milliseconds), well inside the 10 s bootstrap and the 30 s `MCP_TIMEOUT`.
  M4 measures it.
- A hidden CLI, `g-mesh debug-candidates [DIR] [--json]`, prints `Detection`
  including `entries_read` and `elapsed`. It exists for M4 and for support.

### D11. Multi-project roots: a front daemon plus a session switch in the shim

Options considered:

| Option | Summary | Verdict |
|---|---|---|
| (a) Index the whole folder | today's behaviour | the bug |
| (b) Scoped index inside the parent daemon | the root daemon walks only `C/` | **Rejected.** Every plugin's `--bulk-index <root>` walks the whole root, so scoping needs a protocol and implementation change in 4 plugins, plus prefixing ids and paths. The long-lived plugin still resolves imports from the parent root. It builds a *second* index of `C` beside `C`'s own (the same repo opened directly gets a separate state dir), which doubles the 13 min embedding cost |
| (c) Per-call routing by `file_path` | the shim or daemon sends each call to the sub-project daemon that contains the path | **Rejected for v1.** Evaluated below |
| (d) The root daemon proxies to sub-daemons as an MCP client | daemon-to-daemon forwarding | **Rejected.** Same path-rewriting problem as (c), plus an rmcp client in production code and progress relaying |
| (e) Front daemon plus session switch **(chosen)** | the root daemon serves a *front* (candidate list and `select_project`); on selection the shim re-points the session to `C`'s own, normal daemon | reuses every existing per-root path unchanged |

**Evaluation of (c), per-call routing by `file_path`:**

- *For it:* no explicit step for file-anchored calls, and one session can
  span sub-projects.
- *Against it:*
  1. Only 3 of 8 tools take a `file_path` (`find_definition` optionally,
     `get_file_outline`, `get_dependencies`). The four symbol-anchored tools
     and `search_code`, the most frequent calls, have none, so selection is
     needed anyway.
  2. The request path has to be rewritten (`g-mesh/core/x.rs` becomes
     `core/x.rs`, since `find_file_node` matches the stored root-relative path
     exactly, `graph/queries.rs:626`). **Every path in every response** also
     has to be rewritten back: `filePath` in rows, `files[]`, `anchor`,
     `frontierNodes`, and opaque cursors and resume tokens that may embed ids
     or paths. Otherwise the agent gets paths it cannot pass back. That is a
     per-tool, per-field contract that breaks silently whenever a tool's
     output changes.
  3. `symbol_id` inputs would be ambiguous across sub-projects.
  4. `search_code` would need fan-out and a merged ranking.

  (c) is kept as a possible follow-up. In v1 the front's error for a
  path-bearing call names the candidate that contains the path (see below),
  which delivers most of (c)'s convenience at none of its cost.

**How (e) works:**

1. **Front mode (daemon).** When `candidates::detect` returns `Multi`,
   `daemon::run` branches into `daemon::front::run(root, dir, singleton,
   detection)` before `connection::open`.
   - It binds the endpoint, writes the pid and serving-owner files and runs
     an accept loop that serves `mcp::front::FrontServer`, a hand-written
     `ServerHandler` with no DB, no watcher and no plugins. It calls
     `lifecycle::supervise` with a registry built from discovery, which
     spawns nothing.
   - Core idle timeout: `FRONT_CORE_IDLE` = 60 s, so the front leaves soon
     after its last session closes.
   - It writes `daemon.mode` = `front` into the state dir for `status`.
2. **What the front serves:**
   - `get_info`: instructions from `instructions::build_front(root,
     &candidates)` (D12).
   - `list_tools`: `GMeshMcpServer::tool_router().list_all()` plus
     `select_project`. The schemas are identical to a normal daemon's.
   - `select_project { project?: string }`:
     - Without `project`, it returns the candidate list, re-detected on each
       call (cheap, and always current), as text: `rel_path`, markers,
       worktree flag, and `truncated`.
     - With `project`, it validates the value against a fresh detection
       (`rel_path` or absolute path) and returns a success result whose
       `_meta` is `{"g-mesh/switchProject": {"root": "<abs path>"}}`, with
       text such as `"Selected <abs>"`.
     - An unknown `project` is a tool error that lists the candidates.
   - Any other tool: a tool error such as `"g-mesh: /abs/root is a folder of
     N projects and none is selected. Call select_project with one of: a, b,
     c (or ask the user which one they are working on)."`. When the call has
     a `file_path` whose first segments match a candidate, it adds `"The
     file_path you passed lies in '<cand>': select it, then pass paths
     relative to it."`. This is not a partial answer, since there is no index
     to answer from and nothing was asked of one.
3. **Session switch (shim).** The shim becomes JSON-aware, but only
   minimally (slice 5). It records the client's `initialize` request frame
   and its `notifications/initialized` frame. It remembers the ids of
   `tools/call` frames whose `params.name == "select_project"`.
   - When a response to one of those ids comes back from the front with
     `result._meta["g-mesh/switchProject"].root = C`, the shim:
     1. runs `connect_or_bootstrap(C)`, which is the existing code, so
        `C`'s daemon is a completely normal (lazy) daemon that is reused if
        one is already running;
     2. replays the recorded `initialize` to `C` with a shim-owned id
        (`"g-mesh-shim-replay-<n>"`), consumes the response and keeps its
        `result.instructions`;
     3. sends the recorded `notifications/initialized`;
     4. starts a reader thread for `C`;
     5. routes every later client frame to `C`, **except** `select_project`
        calls and `tools/list`, which keep going to the front. That lets the
        agent switch again, and keeps `select_project` in the tool list;
     6. appends a text block to the `select_project` result: `"This session
        now serves <C>. File paths are relative to <C>. Its index is built on
        the first tool call. Guidance for this project:\n\n<C's
        instructions>"`, then forwards it;
     7. if a sub-project daemon was already current, shuts down its write
        half (its reader drains in-flight responses, then ends).
   - If bootstrapping `C` or the replay fails, the shim replaces the
     response with an `isError` result naming the failure, and routing is
     left unchanged.
   - A **single-project** session never sees a `switchProject` directive, so
     for it the shim's behaviour is byte-for-byte today's.
4. **Why the switch sits in the shim and not in the front:** the shim
   already owns stdio, bootstrapping and the per-session lifetime. A daemon
   cannot hand a client's stdio to another process. The paths the agent sees
   after the switch are `C`-relative, exactly as if it had been launched in
   `C`, which is the best-tested configuration g-mesh has.
5. **Risks:**
   - The MCP `instructions` field cannot change after `initialize`. The
     client keeps the front's text, and the up-to-date guidance comes only
     in the `select_project` result. That is acceptable, because the agent
     reads that result right after acting on it.
   - Replay assumes the front and `C` negotiate the same protocol version.
     They are the same binary, and `connect_or_bootstrap` already retires a
     daemon with a different build stamp.
   - Requests in flight to the old upstream at the moment of a switch still
     complete, because its reader drains them.
   - Selection is not persisted: a new session in the same root asks again.
     That is deliberate, since the right sub-project can change from session
     to session.

### D12. Instructions text and the byte budget

- **Normal mode, `Unindexed`:** replace `INDEXING_NOTE` with a state line,
  `"Index root: <abs root>. Not indexed yet - the first tool call builds it
  (structural first; semantic search after) and waits for it."`. While
  `Walking`, use `"Index root: <abs root>. Being built now - ..."`. The root
  path is what A2 asks to be visible in a single project. If the rendered
  string would exceed `INSTRUCTIONS_BYTE_CEILING`, fall back to the same line
  without the path (a test covers the eight-language worst case with a
  103-byte root). `Structural`, `Embedding` and `Ready` render as today,
  without the prefix: structural answers are immediate, and `search_code`'s
  own description already covers waiting.
- **The `P4_*` wait clause** ("On a project's first index ... a tool call
  waits for the walk to finish before answering - slow, not wrong; do not
  abandon it for grep.") stays true and needs no edit.
- **Front mode:** `build_front` renders `P1`, then `"<abs root> is a folder
  of N projects; nothing is indexed. Before using any tool, call
  select_project with the one you are working on (ask the user if unclear):
  "`, then candidate `rel_path`s joined by `, ` until the ceiling, then
  `" (+K more - call select_project with no argument for the full list)"`.
  The language paragraphs (`P2`-`P5`) are omitted: no language is present
  yet, and they arrive in the `select_project` result (D11, step 6). A test
  asserts ≤ ceiling for 64 candidates with 60-byte names.

### D13. CLI and other paths

- `g-mesh init` and `g-mesh reindex` use walk(None), then semantic pass, then
  backfill, in the foreground (D3). Their role and output do not change.
- `g-mesh status` needs the running daemon's phase. The daemon writes
  `index.phase` (a single line: `unindexed|walking|structural|embedding|ready|failed`)
  atomically into its state dir on every transition (same helper as the pid
  file) and removes it on exit. Status renders:
  - `unindexed`: "not indexed yet - builds on the first tool call";
  - `walking`: "building now";
  - `embedding`: "structural index ready; embeddings being computed";
  - `failed`: "last build failed - see daemon log; retried on the next tool call".
  - A root served in front mode (`daemon.mode` = `front`) shows "folder of N
    projects - no index; a session selects one".
- `cli/agent_instructions.rs:55`: keep the sentence, and add "Launched from a
  folder of several projects, g-mesh asks you to pick one with
  `select_project` first."
- `README.md` and `docs/architecture/g-mesh-v1.md`: one paragraph each,
  pointing at this file.
- `g-mesh clean` and `stop`: no semantic change. Slice 4 must check that a
  front-mode state dir (a socket, a pid and no `index.db`) does not trip
  `status`, `clean orphaned` or `clean` (they may assume `index.db` exists).

### D14. Test-suite migration

- `tests/common::wait_until_indexed(root)` must *trigger* activation before
  it polls, because otherwise 23 test files hang. Add
  `common::trigger_activation(root)`: a small **synchronous** raw-NDJSON MCP
  client (about 50 lines). It connects to the daemon endpoint (retrying until
  `startup_timeout`), sends `initialize` and `notifications/initialized`, then
  a `tools/call` of `get_file_outline` with `file_path: "\u0000"` (any cheap
  call triggers), and **does not wait** for the answer, since activation
  survives the disconnect (D2). Then it polls `bulkIndexedAt` as today.
  - This keeps the whole suite on the real lazy path. **Rejected:** an
    eager-mode env knob, which would leave 23 files testing a mode no user
    runs.
- Add `common::wait_until_phase(root, "ready")`, which reads `index.phase`,
  for the tests that need embeddings.
- `serving_while_indexing.rs` and `handshake_independent_of_indexing.rs`:
  trigger first, then run their existing assertions against a held walk.
- `daemon_missing_plugin_binary.rs`: change the expectation from "the daemon
  exits" to "the tool call returns an error containing the hint, and the
  daemon keeps serving".

---

## 3. Open questions for the owner

**Resolved 2026-09-24:** the owner accepted every recommendation below
(Q1-Q6 as recommended). Scope was also split: GM-395 ships slices 1-3 in
3.11.1. Slices 4-5 (candidate detection, the front daemon, the shim's session
switch) move to a separate task for a later minor release, so until then a
multi-project root is indexed as one project, lazily.

Each question has a recommended answer. The slices assume the recommendation;
changing it changes only the slice named.

- **Q1. Should the embedding pass start automatically after the structural
  phase (D5), or only on the first `search_code`?**
  **Recommend: automatically.** The CPU is spent only in sessions that have
  already used g-mesh, and it makes `search_code` usually ready by the time
  it is asked. The cost: a session that only ever uses structural tools still
  pays about 13 min of CPU on a g-mesh-sized project. (Affects slice 2's
  activation sequence only.)
- **Q2. A multi-project root that already has a *completed* index (for
  example, `~/Projects/ClaudeProjects` from the incident that motivated
  this): serve it as a single project, or show the front anyway?**
  **Recommend: serve it.** A completed index means someone paid for it, and
  `g-mesh init` in a parent folder is an explicit choice. To get the front
  back, run `g-mesh clean` in that folder. The accidental index from the
  incident is therefore removed once, by hand. (Affects the D10 rule 2 test
  in slice 4.)
- **Q3. A root with no marker and exactly one candidate below it: index the
  root lazily (single mode), or show the front with one choice?**
  **Recommend: single mode** (threshold 2). This is closest to today's
  behaviour, and the extra files indexed beside the one repo are few. Its
  cost: paths are root-relative, and the index is separate from the repo's
  own.
- **Q4. Should `select_project` also *start* indexing the selected project
  at once, as a prefetch, or wait for its first tool call?**
  **Recommend: wait (v1).** There is one trigger path, and selection alone
  does not prove a tool call will follow. Revisit after M1/M2 if the first
  call's wait turns out to hurt.
- **Q5. `INDEX_WAIT_CAP` default: 25 min, pending M1.** **Recommend: 25 min,
  env-overridable.** §5 says which M1 outcome changes it.
- **Q6. Is `select_project` the right name and shape (one tool, optional
  `project`), rather than two tools (`list_projects` plus
  `select_project`)?**
  **Recommend: one tool.** Each listed tool costs schema tokens every turn,
  and the tool exists only in front-mode sessions.

---

## 4. Implementation slices, in order

Common rules for every slice:

- Branch off `feat/GM-395-lazy-indexing` if the owner asks for per-slice
  branches. Otherwise commit on it only when the owner asks.
- Build controls in a **separate `git worktree`** (`git worktree add
  ../wt-gm395-ctl HEAD`). Revert the named change there, run the named test,
  and watch it fail. Never make a control edit in the main checkout. A fresh
  worktree needs `npm ci && npm run build` in `plugins/typescript` and a
  `cargo build --workspace` before any daemon test works, or the daemon spawn
  times out and names the wrong cause.
- Before finishing a slice, run the full `cargo test -p g-mesh` (in the
  background) plus `cargo build --workspace`, and keep
  `cargo clippy --workspace --all-targets` clean.

### Slice 1: Two-phase readiness and the embedding backfill pass (still eager)

**Goal:** structural answers stop waiting for embeddings, and `search_code`
waits for them. Nothing is lazy yet; the daemon still starts all work at
launch.

**Change:**

- `daemon/indexing_status.rs`: the phase machine and API from D3
  (`Unindexed` is not used yet; a cold start begins at `Walking`). Remove
  `wait_until_ready` and `wait_ready`, and migrate their callers and tests.
- `daemon/bulk_index.rs`: `embedding: Option<&EmbeddingPipeline>` through
  `run`, `walk_one_language`, `ingest` and `commit`. `commit` skips
  `compute`/`store` on `None`. Increment the progress counters in `ingest`
  and per language in `run`.
- `embedding/backfill.rs` (new) and `EmbeddingPipeline::is_available()` (new,
  in `embedding/pipeline.rs`), with the `G_MESH_EMBED_PASS_HOLD_FILE` knob.
- `daemon/mod.rs` cold start (`:601-705`): walk(None), then
  `set_phase(Structural)`, then `record_bulk_index`, then the semantic pass,
  then `set_phase(Embedding)`, `backfill::run` and `set_phase(Ready)`.
  - The backfill must run *after* the watcher consumer thread is spawned
    (move that spawn up), so that incremental edits are served during the
    long pass.
  - An already-walked project also runs backfill at startup here (it becomes
    lazy in slice 2).
- `daemon/workspace_reindex.rs:348`: pass `Some(embedding)`.
- `cli/init.rs:226` and `cli/reindex.rs:105`: walk(None), then semantic, then
  backfill.
- `mcp/mod.rs`: `prepare(need: Need)`. The seven structural tools use
  `Need::Structural`, and `search_code` uses `Need::Embeddings`.
  `instructions()` uses `phase() == Walking` where it used `is_indexing()`.

**Behaviour after the slice:** on g-mesh, a structural call on a cold start
answers after about 31 s, not about 814 s. `search_code` answers once
backfill is done. An index built by a model-less machine gets embedded once a
model appears.

**Tests:**

1. `tests/structural_does_not_wait_for_embeddings.rs`: a TS fixture, daemon
   via `mcp-shim`, `G_MESH_EMBED_PASS_HOLD_FILE` set.
   `wait_until_indexed(root)`, then `find_definition` must return the
   expected declaration within `startup_timeout()` while the hold file still
   exists.
   *Control:* in the worktree, make the structural tools use
   `Need::Embeddings`, and the call does not return before the hold is
   released (assert with a timeout).
2. Same file: `search_code` issued while held has **not** returned after 3 s.
   Remove the hold file, and it returns (an "unavailable" error is fine
   without weights; the point is *when* it returns).
   *Control:* make `search_code` use `Need::Structural`, and it returns
   within 3 s while held.
3. `embedding/backfill.rs` unit tests (no weights):
   - candidate selection returns exactly the nodes with embeddable text and
     no vector row;
   - keyset pagination visits each node once across pages of 2;
   - `is_available() == false` makes the pass return without a query.
   *Control:* drop the `LEFT JOIN ... IS NULL` filter, and the "already
   embedded node excluded" assertion fails.
4. `#[ignore]` (needs weights), in `tests/embedding_generation_pipeline.rs`:
   after walk(None) plus backfill, `vectors` holds one row per embeddable
   node. That equals the count the old inline walk produced; compare against
   the existing assertion in that file.
5. Unit tests in `indexing_status.rs`: `wait_for(Structural)` resolves at
   `Structural` and at `Embedding`; `wait_for(Embeddings)` resolves only at
   `Ready`; `Failed` resolves both waits with the message; a deadline returns
   `TimedOut`.

**Exit:** the tests above plus the full suite green. The M2 timing is
recorded (structural answer time and backfill time on g-mesh).

### Slice 2: Lazy activation for a single project

**Goal:** A1, A4 (blocking part), A6. Connecting costs nothing, and the first
tool call builds what is owed.

**Change:**

- `daemon/activation.rs` (new): move `mod.rs`'s post-bind block into
  `fn run(ctx: ActivationCtx)` on a parked thread, as D2 and D8 describe.
  - `ActivationCtx` holds `conn`, `registry`, `embedding`,
    `discovered_for_bulk_index`, `canonical_root`, `root`, `indexing`,
    `core_activity`, an optional pre-registered watcher, and
    `needs_semantic_pass_retry`.
  - The loop: wait for a trigger (`std::sync::mpsc::Receiver<()>`, whose
    sender is held by `IndexingStatus`). Then do the owed work. On a walk
    error, set `Failed(msg)` and go back to waiting.
- `daemon/mod.rs`: the startup phase is `Unindexed` if `needs_bulk_index`,
  else `Structural`. Register the watcher at startup only when the project is
  already walked. Start the consumer at startup only when no semantic retry
  is owed. Remove the eager walk, the semantic pass and the retry from `run`.
- `IndexingStatus::request_activation()`, called at the top of `prepare`.
- `index.phase` state file writes (D13), in `daemon/mod.rs` next to
  `write_pid_file`, removed in `lifecycle::release_state_files`.
- `mcp/mod.rs` and `mcp/instructions.rs`: the D12 normal-mode state line
  replaces `INDEXING_NOTE`, with the ceiling test at a 103-byte root.
- `cli/status.rs:583-597`: render from `index.phase` (D13).
- `cli/agent_instructions.rs`, `README.md`, and the `cli/init.rs` module doc:
  wording updates.
- `tests/common/mod.rs`: `trigger_activation`, the trigger inside
  `wait_until_indexed`, and `wait_until_phase` (D14). Update
  `serving_while_indexing.rs`, `handshake_independent_of_indexing.rs` and
  `daemon_missing_plugin_binary.rs`.

**Behaviour after the slice:** a session that never calls a tool leaves an
idle daemon: 0% CPU, no `--bulk-index` child, no plugin, no watcher (for an
unindexed project). The first structural call blocks until the walk and link
finish, then answers fully. A failed walk becomes a tool error, and the next
call retries.

**Tests:**

1. `tests/lazy_activation.rs::connecting_alone_indexes_nothing`: an rmcp
   client via `mcp-shim` on an unindexed fixture. Initialize, `list_tools`,
   sleep 2 s. Assert that `bulkIndexedAt` is unset, that `index.phase` reads
   `unindexed`, and that no process exists whose argv contains
   `--bulk-index` and the fixture root (scan `ps -axo pid,command`; on
   Windows use `tasklist /v` or skip with a reason).
   *Control:* in the worktree, call `request_activation()` right after bind,
   and the test fails on each of the three assertions.
2. `...::first_structural_call_blocks_and_answers_fully`: on the same
   fixture, the first `get_file_outline` returns the fixture's full outline
   (compare to the literal expected list), and afterwards `bulkIndexedAt` is
   set.
   *Control:* make `prepare` skip the wait, and the call returns "no file
   found in the index" or a shorter list.
3. `...::an_existing_index_is_not_rewalked`: index the fixture with
   `g-mesh init`, record `bulkIndexedAt` and `max(rowid)` of `nodes`, start a
   session and call `find_references`. Assert that `bulkIndexedAt` and the
   node rowids are unchanged, and that the daemon log
   (`G_MESH_DAEMON_LOG`) has no "initial index built" line.
   *Control:* force `needs_bulk_index = true`, and the log line appears and
   `bulkIndexedAt` changes.
4. `...::a_failed_walk_is_a_tool_error_and_is_retried`: point one manifest
   at a missing binary (reuse `daemon_missing_plugin_binary.rs`'s fixture).
   The first call is an `isError` result containing the hint, and the
   session is still alive (a second `list_tools` succeeds). Fix the binary
   path through the fixture, and the next call answers.
   *Control:* restore `?`-fatal behaviour, and the session dies (the
   transport closes).
5. Unit test in `mcp/instructions.rs`: the `Unindexed` rendering at the
   eight-language worst case with a 103-byte root is ≤ 1,900 bytes, and one
   longer root triggers the no-path fallback.

**Exit:** tests 1-5, and the full suite green with the migrated helpers.

### Slice 3: Progress notifications, the wait cap and cancellation

**Goal:** A4 (progress part), and D6/D7.

**Change:**

- `mcp/mod.rs`: every `#[tool]` fn takes `ctx: RequestContext<RoleServer>`,
  and `prepare(&ctx, need)` implements the D6 `select!` (wait, ticker,
  `ctx.ct`) and the D7 cap (`G_MESH_INDEX_WAIT_CAP_MS`, default 25 min). Add
  `G_MESH_PROGRESS_INTERVAL_MS` (default 5,000; 0 = off).
- `IndexingStatus::progress_message()` renders the counters (D6).
- `trace_call` (`mcp/mod.rs:93`): when `G_MESH_TRACE_CALLS` is set, also log
  whether the request carried a `progressToken`. M1 depends on this line.

**Tests:** `tests/index_wait_progress.rs`. Use an rmcp client with a
`ClientHandler` that records `on_progress`, and send `tools/call` with
`_meta.progressToken` set (rmcp client: `peer.send_request` with meta, or
`call_tool` through `RequestOptions`; check the 2.2.0 client API). Hold the
walk with `G_MESH_BULK_INDEX_HOLD_FILE` and set
`G_MESH_PROGRESS_INTERVAL_MS=200`.

1. After about 1 s of holding, release. Assert ≥ 3 notifications, strictly
   increasing `progress`, `message` containing the root, and a full final
   answer.
   *Control:* set the interval to 0 in the worktree (or delete the ticker
   branch), and 0 notifications arrive.
2. The same call without a `progressToken` receives 0 notifications and
   still answers.
   *Control:* send progress regardless of the token, and notifications
   arrive.
3. `G_MESH_INDEX_WAIT_CAP_MS=500` with the walk held: the call returns
   `isError` with "still indexing" within 2 s. Release the hold, and the same
   call then answers.
   *Control:* disable the cap, and the call has not returned after 3 s.
4. Cancellation: send `notifications/cancelled` for the waiting request
   (rmcp client `RequestHandle::cancel`). The handler returns quickly (the
   daemon trace log shows "prepare: cancelled"), the walk continues, and a
   later call answers.

**Exit:** tests 1-4 green. M1 has been run and recorded (§5) before Slice 5
starts, because its outcome may change `INDEX_WAIT_CAP`.

### Slice 4: Candidate detection and the front daemon

**Goal:** A2 (front side) and A3.

**Change:**

- `daemon/candidates.rs` (new): D10.
- `cli/mod.rs`: the hidden `debug-candidates` subcommand.
- `daemon/mod.rs::run`: after the build stamp, a mode decision (D10 rules;
  rule 2 opens `index.db` read-only only if it exists), then `Multi` goes to
  `daemon::front::run`.
- `daemon/front.rs` (new): bind, pid and serving-owner files,
  `daemon.mode`, an accept loop with `FrontServer`, and `supervise` with
  `FRONT_CORE_IDLE`.
- `mcp/front.rs` (new): `FrontServer` (D11 step 2). `mcp/instructions.rs`:
  `build_front` (D12).
- `cli/status.rs` and `cli/clean.rs`: make them tolerate a state dir without
  `index.db` (D13).

**Tests:**

1. `candidates.rs` unit tests on tempdir fixtures:
   - three repos plus one worktree (a `.git` *file*) at depth 2 give exactly
     those four, with `is_worktree` set;
   - a repo containing nested `package.json`s is listed once;
   - `node_modules/x/package.json` is not listed;
   - a marker at depth 3 is not listed;
   - a root marker gives `Single`;
   - one candidate gives `Single`;
   - `max_entries = 10` on a wide fixture gives `truncated` and
     `entries_read <= 10`;
   - a symlink to a repo is not followed.
   *Control:* remove the "do not descend into a candidate" rule, and the
   nested-package test fails.
2. `tests/multi_project_front.rs`: a fixture root holding `a/` (`.git`
   directory plus a TS file), `b/` (the same) and `c/` (`go.mod`), with no
   root marker, served through `mcp-shim`.
   - The handshake instructions contain `a`, `b` and `c`.
   - `list_tools` contains `select_project` and the 8 usual tools.
   - `select_project {}` lists all three.
   - `select_project {project:"b"}` returns `_meta["g-mesh/switchProject"].root`
     equal to the canonical path of `b`.
   - `find_references` returns the "none is selected" error naming `a, b, c`.
   - `get_file_outline {file_path:"b/x.ts"}` names `b`.
   - `project_dir(root)` has no `index.db`, and no `--bulk-index` process runs.
   (Until slice 5 lands, the shim forwards the `_meta` untouched, which is
   what this test checks.)
   *Control:* force `Single` in the mode decision, and the instructions lack
   the candidates and an `index.db` appears.
3. The `build_front` ceiling test from D12.

**Exit:** tests green; M4 run and recorded.

### Slice 5: Session switch in the shim

**Goal:** A2 (select, then work on the selected project), end to end.

**Change:** in `shim.rs`, replace `proxy`/`pump` (`:469-530`) with a
switchable router (D11 step 3):

- `Router { front: Upstream, current: Upstream, init_frame, initialized_frame,
  select_ids: HashSet<serde_json::Value>, replay_seq }` behind a `Mutex`;
  stdout behind its own `Mutex`, so that frames from two readers never
  interleave.
- The stdin thread parses each client frame with `serde_json`. A frame that
  fails to parse is forwarded raw to `current`.
- Each upstream reader thread checks `select_ids` only on the front and only
  for response frames, so large tool results from sub-project daemons are
  never parsed.
- The switch holds the router lock across bootstrap and replay: client
  frames wait, at most the bootstrap timeout.
- Exit: on stdin EOF, shut down the write half of every upstream. The shim
  exits when the current upstream's reader ends. That is the same "shim
  lives as long as its session" semantics as today.

**Tests:**

1. `tests/multi_project_front.rs::selecting_a_project_switches_the_session`:
   the fixture from slice 4.
   - `select_project {project:"b"}`: the result text contains `b`'s absolute
     path and the language guidance (`P1` text).
   - Then `get_file_outline {file_path:"x.ts"}` (relative to `b`) returns
     `b/x.ts`'s outline.
   - `project_dir(b)/index.db` has `bulkIndexedAt` set, and
     `project_dir(root)` and `project_dir(a)` have no `index.db`.
   *Control:* in the worktree, make the shim ignore the directive, and
   `get_file_outline` returns the front's "none is selected" error.
2. `...::reselecting_switches_again`: select `b`, then `a`, and an outline of
   `a`'s file works. The old `b` daemon keeps running, but the session no
   longer talks to it: `b`'s daemon trace log shows no further calls.
3. `...::a_failed_switch_leaves_the_session_on_the_front`: select a candidate
   whose daemon cannot start (for example, make its state dir path exceed the
   socket limit by setting `G_MESH_HOME` to a long path, or use an injected
   failure env var). The result is `isError` naming the failure, and a
   following `select_project {}` still answers.
4. Regression: `tests/mcp_e2e.rs` and `tests/shim_bootstrap.rs` stay green,
   since the single-project path must be unchanged.

**Exit:** tests green, and a manual session in `~/Projects/ClaudeProjects`
(M3 part b) recorded.

---

## 5. Measurement plan

Record every run with `claude --version`, `uptime`, the g-mesh commit, and,
for timings, `/usr/bin/time -p` (`real`/`user`/`sys`). Store the results in
`docs/results/gm-395-lazy-indexing.md`.

### M1. Claude Code's tool timeout, with progress on and off (after slice 3)

**Setup:**

- A release build of the branch.
- A small TS fixture copied to `$TMP/proj`.
- `$TMP/mcp.json`:

  ```json
  {"mcpServers":{"g-mesh":{"command":"<abs g-mesh>","args":["mcp-shim"],
   "env":{"G_MESH_HOME":"$TMP/home","G_MESH_BULK_INDEX_HOLD_FILE":"$TMP/hold",
          "G_MESH_TRACE_CALLS":"1","G_MESH_DAEMON_LOG":"$TMP/daemon.log"}}}}
  ```

**Run:**

1. `touch $TMP/hold`. Schedule the release: `(sleep 180; rm $TMP/hold) &`.
2. `cd $TMP/proj && CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000 claude -p
   --mcp-config $TMP/mcp.json --strict-mcp-config "Call the g-mesh
   get_file_outline tool on src/a.ts and print its raw result."`. Headless,
   so no auto-backgrounding. The idle window is shrunk to 60 s.
3. Arm **A**: default (progress every 5 s). Arm **B**: add
   `"G_MESH_PROGRESS_INTERVAL_MS":"0"` to the env block.
4. Three reps per arm. Record the elapsed time to the result or error, the
   error text, and from `daemon.log` whether a `progressToken` was present
   and how many notifications were sent.
5. Extra run (A only): per-server `"timeout": 90000` in `mcp.json`, idle
   default. Expect an abort at about 90 s. Progress should not extend it,
   per the documentation.
6. Extra run (A, interactive `claude`): observe backgrounding at 2 min and
   that the result arrives as a task notification after release.

**Discriminating observation (the control):** arm B must abort at about 60 s
with an idle-timeout error. If it does not, the idle timer was not active (a
Claude Code version below 2.1.203, or the env var was not honoured) and the
run measured nothing. Fix that before reading arm A.

**What the result changes:**

- A token is present, A survives to the release, and B aborts at 60 s:
  design confirmed. Keep `INDEX_WAIT_CAP` at 25 min as the guard against a
  per-server wall-clock limit.
- **No token is present:** progress can never help Claude Code, and the cap
  is the only guard.
  - Keep the cap under the 30 min stdio idle default.
  - Document that a user who sets a lower `timeout` or
    `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` gets the "still indexing" error.
  - Consider a follow-up: the shim inherits Claude Code's environment, so it
    can read `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT` and `MCP_TOOL_TIMEOUT` and
    hand a per-session cap to the daemon.
- A token is present but A also aborts at 60 s: progress does not reset the
  idle timer, contrary to the documentation. Report it upstream. Lower the
  default cap below the smallest documented idle window that applies (30 min
  for stdio), and keep the cap as the primary mechanism.
- The extra run in step 5 not aborting would contradict the documentation.
  Record it, but do not rely on it.

### M2. Real timings (after slice 1, repeated after slice 2)

- A fresh `G_MESH_HOME`, and the g-mesh repo as the project.
- Time to the first `get_file_outline` answer, time to the first
  `search_code` answer, and total backfill time.
- Sample `ps -o pid,%cpu,time,command -p <daemon pid>` every 10 s during
  both phases.
- **Expectation:** about 31 s structural, and the backfill about as long as
  the old inline embedding (about 800 s).
- If the structural time exceeds the 2 min auto-background threshold on
  realistic projects, record it. It argues for Q4's prefetch.

### M3. Idle connect costs nothing (after slice 2, and slice 5 for part b)

- **(a)** An unindexed fixture and a fresh home. Start interactive `claude`
  in it, do not call a tool, and wait 60 s. Record `ps -axo
  pid,%cpu,time,command | grep -E 'g-mesh|bulk-index'` (the whole output,
  not truncated), and the daemon's accumulated CPU `time`. Expect: one
  daemon, CPU time under 1 s, no `--bulk-index`, no plugin process.
- **(b)** The same in `~/Projects/ClaudeProjects`, after Q2 is decided (if
  its accidental index is kept, run `g-mesh clean` there first). Expect the
  front daemon only, and instructions listing the repos. Then
  `select_project` g-mesh and one call: the g-mesh daemon walks only g-mesh.
  If an index of g-mesh already exists, it is reused, with no
  "initial index built" line in its log.

### M4. Candidate-detection cost (after slice 4)

- `g-mesh debug-candidates <dir> --json`, 5 runs each, on
  `~/Projects/ClaudeProjects`, `~/Projects`, `$HOME`, and a synthetic wide
  folder (`mkdir -p wide/d{1..3000}`).
- Record `entries_read`, `elapsed`, `truncated`, and the candidate count.
- Run once after a reboot, or with `sudo purge` if that is available, for the
  cold cache, and say which was used.
- **Ground truth for completeness:** `find <dir> -maxdepth 3 -name .git`,
  pruned the same way, compared against the candidate list.

**What the result changes:**

- Max `elapsed` on `$HOME` above 100 ms: lower `max_entries`.
- `truncated` on `~/Projects/ClaudeProjects`, or a repo the ground truth
  finds but detection missed: revisit the depth and skip list before
  shipping.
- The synthetic wide folder must stop at `max_entries`, which proves the
  bound holds.
