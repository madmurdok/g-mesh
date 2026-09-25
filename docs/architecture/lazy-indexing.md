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

*Revised for GM-399 against the code at `release-3.12.0` (commit `bcaa317`).
Line references below are to that commit.*

New module `core/src/daemon/candidates.rs`:
`pub fn detect(root: &Path, limits: Limits) -> Detection`.

- **Markers:** `.git` (directory = a repo; *file* = a worktree or submodule),
  `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`. Each one is a
  `stat` (`symlink_metadata`) of `<dir>/<marker>`, never a listing.
- **Root is a single project** (normal mode, lazy exactly as GM-395 shipped
  it) if any of these holds, checked **in this order** so a normal project
  pays only rule 1:
  1. the root has a marker itself: at most 5 `stat`s. A monorepo such as
     g-mesh, with nested `plugins/*/package.json`, stays single;
  2. the root already has a completed index: `<state dir>/index.db` exists
     and `schema::bulk_index_completed` (`storage/schema.rs:555`) is true. It
     is opened **read-write without `CREATE`**, the way `cli/status.rs:429`
     and `gc/last_used.rs:106` already open it (WAL recovery needs write
     access; a missing file must never be conjured). Never through
     `connection::open` (`storage/connection.rs:101`), which creates the file.
     A missing `meta` table or row counts as "not completed". See Q2 and Q7;
  3. fewer than 2 candidates are found. See Q3.
- **Walk** (only when rules 1 and 2 did not settle it): breadth-first from
  the root.
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
  entries_read, elapsed, truncated }`, sorted by `rel_path`. `abs_path` is
  canonical (the shim compares it against its own canonical root, D11 step 3).
  *As built (slice 4):* `Mode::Single` carries which rule settled it, and
  `Detection.walked` says whether the walk ran; the walk itself is also
  exposed alone as `candidates::walk`, which `select_project` and `g-mesh
  status` use to re-list a folder already known to be a front.
- **Where it runs:** in `daemon::run` (`daemon/mod.rs:370`), after the
  singleton lock (`:383`) and the build stamp (`:396-403`), and **before
  plugin discovery** (`:426`) and `connection::open` (`:429`).
  - *Changed from the accepted text* ("before `connection::open`"): moving
    it one step earlier, ahead of discovery, keeps a malformed plugin
    manifest from stopping a daemon that needs no plugins at all. Discovery
    only reads `plugin.toml` files and depends on nothing detection produces,
    so single mode is unaffected by the reorder.
  - `ensure_project_dir` (`:375`) has already created the state dir and its
    `project.root` by then; the front needs that dir for its lock, socket and
    pid file, so this is correct as is.
  - Its cost is bounded by `max_entries` (milliseconds), well inside the 10 s
    bootstrap (`shim.rs:28`) and the 30 s `MCP_TIMEOUT`. M4 measures it.
- A hidden CLI, `g-mesh debug-candidates [DIR] [--json]` (`#[command(hide =
  true)]`, as `cli/mod.rs:101` already does for another subcommand), prints
  `Detection` including `entries_read` and `elapsed`. It exists for M4 and for
  support. *As built:* when rule 1 or 2 settled the mode without a walk, it
  runs the walk anyway (and says so, `walkNeeded: false`), so M4 can measure
  folders that already have an index.

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
   detection)` right after detection, so none of `daemon/mod.rs:426-666`
   runs: no discovery, no `connection::open`, no `schema::ensure_current`, no
   `last_used::touch`, no `IndexingStatus`, no watcher, no
   `activation::spawn` (`:651`).
   - It repeats the normal daemon's publish sequence in the same order:
     `endpoint.clear_stale()` (`:483`), `ipc::Listener::bind` (`:499`), the
     pid file (`:506-507`), then `record_serving_owner` (`:533`). The shim's
     bootstrap and `cli::status` see a front exactly as they see any daemon.
   - **Mode marker:** it writes the word `front` to `index.phase` with
     `write_state_file_atomic` (`daemon/mod.rs:305`), *instead of* the new
     `daemon.mode` file the accepted text named. *Changed* because the phase
     file already has every property wanted: `release_state_files` removes it
     on exit (`daemon/lifecycle.rs:1253`), `cli::status` already reads it
     (`daemon::read_phase_in`, `daemon/mod.rs:207`; `cli/status.rs:615`), and
     tests already wait on it (`tests/common/mod.rs:300`,
     `wait_until_phase`). A second file would need its own teardown and its
     own reader.
   - The accept loop is `serve_forever`'s shape (`daemon/mod.rs:809-853`):
     one tokio runtime on its own thread, each accepted connection holding a
     `CoreActivity::connection_opened()` guard, but serving
     `mcp::front::FrontServer` instead of `mcp::serve_connection`.
   - It calls `lifecycle::supervise` (`daemon/lifecycle.rs:1165`) with a
     `PluginRegistry` built from an **empty** discovered list (it spawns
     nothing and needs no discovery; *changed* from "built from discovery",
     which would re-introduce the manifest dependency removed above), and
     `IdleTimeouts { plugin: None, core: Some(FRONT_CORE_IDLE) }`.
     `FRONT_CORE_IDLE` = 60 s, fixed rather than read from `config.toml`, and
     overridable by `G_MESH_FRONT_IDLE_MS` for tests. The orphan check that
     `supervise` runs on each tick applies unchanged.
2. **What the front serves** (`mcp::front::FrontServer`, a hand-written
   `ServerHandler` in a child module of `mcp`, so it may call the private
   `GMeshMcpServer::tool_router()` that `#[tool_router]` generates at
   `mcp/mod.rs:218`):
   - `get_info`: the same `ServerInfo` shape as `mcp/mod.rs:954-968` (same
     name and version), with instructions from
     `instructions::build_front(root, &detection, &indexed)` (D12).
   - `list_tools`: `GMeshMcpServer::tool_router().list_all()` (the 8 tools,
     `mcp/mod.rs:812-934`) plus `select_project`. The 8 schemas are
     byte-identical to a normal daemon's.
   - `select_project { project?: string }`:
     - Without `project`, it returns the candidate list, re-detected on each
       call (cheap, and always current), as text: `rel_path`, markers,
       worktree flag, and `truncated`.
     - With `project`, it validates the value against a fresh detection
       (`rel_path` or absolute path) and returns a success result with
       `CallToolResult.meta` (serialized as `_meta`; rmcp 2.2.0 has the
       field) set to `{"g-mesh/switchProject": {"root": "<canonical abs
       path>"}}`, and text `"Selected <abs>"`.
     - An unknown `project` is a tool error that lists the candidates.
   - Any other tool: a tool error such as `"g-mesh: /abs/root is a folder of
     N projects and none is selected. Call select_project with one of: a, b,
     c (or ask the user which one they are working on)."`. When the call has
     a `file_path` whose first segments match a candidate, it adds `"The
     file_path you passed lies in '<cand>': select it, then pass paths
     relative to it."`. This is not a partial answer, since there is no index
     to answer from and nothing was asked of one. The front has no `prepare`
     (`mcp/mod.rs:261`): it answers at once, triggers nothing and sends no
     progress notifications.
3. **Session switch (shim).** Today's shim is `proxy`/`pump`
   (`shim.rs:469-507`), whose stated invariant is that it "never parses a
   payload" (`shim.rs:498-501`). Slice S3 breaks that invariant on purpose,
   and only as far as needed: it parses client frames and the *front's*
   response frames, never a sub-project daemon's. It records the client's
   `initialize` request frame and its `notifications/initialized` frame, and
   remembers the ids of `tools/call` frames whose `params.name ==
   "select_project"`.
   - When a response to one of those ids comes back **from the front** with
     `result._meta["g-mesh/switchProject"].root = C`, and `C` is a
     descendant of the shim's own canonical root (a cheap guard against a
     directive the shim did not expect), the shim:
     1. runs `connect_or_bootstrap(C)` (`shim.rs:146`), which is the existing
        code, so `C`'s daemon is a completely normal (lazy) daemon that is
        reused if one is already running, and retired if it is outdated;
     2. replays the recorded `initialize` to `C` with a shim-owned id
        (`"g-mesh-shim-replay-<n>"`), reads that one response synchronously
        (before `C`'s reader thread exists, so it can never reach the
        client), and keeps its `result.instructions`;
     3. sends the recorded `notifications/initialized`;
     4. starts a reader thread for `C`;
     5. routes every later client frame to `C`, **except** `select_project`
        calls and `tools/list`, which keep going to the front. That lets the
        agent switch again, and keeps `select_project` in the tool list.
        Everything else (`ping`, `notifications/cancelled`, any other
        method) goes to the current upstream;
     6. rewrites the `select_project` result as the "Instructions after a
        switch" subsection below specifies, removes the `_meta` directive,
        and forwards it;
     7. if a sub-project daemon was already current, shuts down its write
        half (its reader drains in-flight responses, then ends).
   - **Reselection is always a full switch**, including selecting the
     project that is already current: a new connection, a new replay, the
     old write half shut down. There is no same-project shortcut, because the
     replay is what yields the project's *current* instructions (see below).
   - If bootstrapping `C` or the replay fails, the shim replaces the
     response with an `isError` result naming the failure, and routing is
     left unchanged.
   - All frames to stdout go through **one writer thread fed by a channel**,
     not a mutex around stdout: interleaving two readers' frames then becomes
     impossible by construction rather than by discipline.
   - If the front's connection ends after a switch (for example, it was
     retired as outdated), `tools/list` falls back to the current upstream
     and `select_project` calls get an error saying the front is gone. The
     session itself continues on `C`.
   - A **single-project** session never sees a `switchProject` directive, so
     for it the shim forwards every frame byte-for-byte as today (a frame that
     fails to parse is forwarded raw).
4. **Why the switch sits in the shim and not in the front:** the shim
   already owns stdio, bootstrapping and the per-session lifetime. A daemon
   cannot hand a client's stdio to another process. The paths the agent sees
   after the switch are `C`-relative, exactly as if it had been launched in
   `C`, which is the best-tested configuration g-mesh has.
5. **Instructions after a switch (the known gap).** MCP's `instructions`
   field is read once, from the `initialize` response. After the switch the
   client keeps the *front's* text for the rest of the session and never
   sees `C`'s. MCP has no notification that replaces it. Three ways for the
   agent to learn `C`'s state:

   | Option | How | For | Against |
   |---|---|---|---|
   | **(A) The `select_project` result carries `C`'s own instructions (recommended)** | Step 2's replay already receives `C`'s `initialize` response. Its `instructions` is `GMeshMcpServer::instructions()` (`mcp/mod.rs:784`), rendered by `C`'s daemon for `C`'s *actual* phase: `instructions::cold_start` (`mcp/instructions.rs:175`, the `Index root: <C>. Not indexed yet ...` / `Being built now ...` line) while `Unindexed`/`Walking`, `instructions::build` (`:589`) otherwise. The shim puts it in the result verbatim | The exact text a session launched in `C` gets, from the one process that knows `C`'s phase and languages. No new rendering code, no extra tool schema, works with any client. It is a snapshot at switch time, but so are a single-project session's instructions (fixed at `initialize`) | Lives in a tool result, so it can be lost on context compaction, while the front's (now stale) instructions persist. Mitigated by D12's front wording ("call it again to re-read") and by reselection being a full switch that re-renders |
   | (B) `notifications/tools/list_changed` plus a per-session `select_project` description | After the switch the shim sends `tools/list_changed`; the re-listed `select_project` description states the current project and its guidance | Persists across compaction, like instructions | Depends on the client re-fetching the list; Claude Code's deferred tool search may not load the description at all. The description has its own ~2 KB budget, and `C`'s guidance alone is up to 1,900 bytes. The selection is known to the shim, not the front, and one front serves several sessions, so the shim would have to rewrite `tools/list` responses too |
   | (C) The front renders a status itself | The front reads `C`'s `index.phase` and `bulkIndexedAt` off `C`'s state dir and describes them | No dependence on the replay for text | Duplicates the instructions logic outside the daemon that owns it, cannot render the language paragraphs (P2-P5 need `C`'s registry and index), and can disagree with what `C` would say |

   **Recommendation: (A).** The shim rewrites the result's content to:

   > `g-mesh: this session now serves <C>; file paths are relative to it.
   > Guidance for this project, as a session started in <C> would receive
   > it:` followed by a blank line and `C`'s instructions verbatim.

   `C`'s own text already states whether its index exists (the `Index root:`
   line), so the shim adds no index-state claim of its own that could
   disagree with it. (B) stays a follow-up if M3 part b shows agents losing
   the guidance after compaction. See Q8.
6. **Risks:**
   - The gap above: `C`'s guidance is transient; the front's is permanent.
     D12 words the front's text so it stays true after a switch.
   - Replay assumes the front and `C` negotiate the same protocol version.
     They are the same binary, and `connect_or_bootstrap` already retires a
     daemon with a different build stamp.
   - Requests in flight to the old upstream at the moment of a switch still
     complete, because its reader drains them. A `notifications/cancelled`
     for such a request goes to the new upstream, which ignores the unknown
     id, so that request cannot be cancelled. Accepted: switches are rare and
     the call still ends (at worst at the D7 cap).
   - After a switch the front's connection stays open (it serves
     `tools/list` and `select_project`), so the front lives as long as the
     session. It holds no index and no plugins, so this costs one idle
     process.
   - Selection is not persisted: a new session in the same root asks again.
     That is deliberate, since the right sub-project can change from session
     to session.

### D12. Instructions text and the byte budget

- **Normal mode: done in GM-395.** `instructions::cold_start`
  (`mcp/instructions.rs:175`) prefixes `cold_start_line` (`:134`, `"Index
  root: <abs root>. Not indexed yet - ..."` or `"... Being built now - ..."`)
  to the `build` rendering, falling back to `cold_start_line_fallback`
  (`:153`) when the root would push it over `INSTRUCTIONS_BYTE_CEILING`
  (`:110`, 1,900 bytes). `GMeshMcpServer::instructions` (`mcp/mod.rs:784-791`)
  uses it for `Phase::Unindexed | Phase::Walking` and `build` for every other
  phase. The ceiling tests exist (`mcp/instructions.rs:1242-1295`). GM-399
  adds nothing here; D11 step 5 reuses this rendering as `C`'s guidance.
- **The `P4_*` wait clause** stays true and needs no edit.
- **Front mode:** new `pub fn build_front(root: &Path, detection:
  &Detection, indexed: &HashSet<&str>) -> String` in `mcp/instructions.rs`.
  It renders `P1` (`:268`, which stays true after a switch), a blank line,
  then:

  > `<abs root> is a folder of N projects; g-mesh serves one at a time and
  > has indexed none of them. Before any other g-mesh tool, call
  > select_project with the one you are working on (ask the user if
  > unclear). Its result names the project this session then serves and
  > carries that project's guidance; call it again to switch, or to re-read
  > that guidance. Projects: `

  then candidate `rel_path`s joined by `, ` until the ceiling, then `" (+K
  more - call select_project with no argument for the full list)"`. With
  `truncated`, `N` renders as `N+`.
  - *As built (GM-399 follow-up):* `indexed` holds the candidates that
    already have a completed index of their own - rule 2's check
    (`candidates::has_completed_index`), run per candidate by
    `mcp::front::Front::new`. When it is non-empty, "has indexed none of
    them" becomes "has already indexed K of them, listed first and marked
    (indexed)", and those candidates lead the list as `<rel_path>
    (indexed)`, so the ceiling cuts unindexed names first. With none
    indexed the text is exactly the one above.
  - The wording is chosen to stay true after a switch (D11 step 5): it does
    not say "nothing is selected", only "before any other tool".
  - The language paragraphs (`P2`-`P5`) are omitted: no language is known
    yet, and they arrive in the `select_project` result.
  - If the root path alone would break the ceiling, drop it (`"This folder
    holds N projects; ..."`), the same fallback `cold_start` uses. *As
    built:* the path is also dropped when keeping it would leave no room for
    even the first project name.
  - A test asserts the result is ≤ ceiling for 64 candidates with 60-byte
    names under a 103-byte root (the same worst-case root as the existing
    `cold_start` test), and that at least one name is listed.

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
  - A root served in front mode (`index.phase` = `front`, D11 step 1) shows
    "folder of N projects - no index; a session selects one", and skips the
    coverage walk (slice 4).
  - Next to it the daemon writes `index.progress`: its progress counters as
    JSON (`pid`, `updatedAtMs`, `phase`, `walk`, `semantic`, `embeddings`),
    atomically, on every phase change and stage boundary and at most every
    500 ms for counter updates, and removes it on exit. Status adds the
    numbers per stage (walk languages done/total and nodes so far; the
    semantic pass's running language; embeddings done/total with a
    percentage), an `overall:` line labelled as an estimate (fixed stage
    weights: walk 40%, semantic pass 20%, embeddings 40%), and an index line
    for `structural` and `ready` too.
  - Liveness: the progress file is shown only when its `pid` is the pid of
    the daemon status finds running (pid alive and socket accepting). A
    daemon killed without cleanup leaves the file with its own pid, which no
    longer matches; a new daemon replaces the file with its pid as it
    starts. A pid check rather than a heartbeat age, because an embedding
    batch or a semantic pass can legitimately go longer than any fixed age
    without a counter changing. A phase word is likewise ignored when no
    daemon is running.
  - While a live daemon is in `unindexed|walking|structural|embedding`, an
    unfinished semantic pass reads as its work in progress, not as
    "never completed - run `g-mesh reindex`"; that advice needs no daemon
    working, or phase `ready`/`failed`.
- `cli/agent_instructions.rs:55`: keep the sentence, and add "Launched from a
  folder of several projects, g-mesh asks you to pick one with
  `select_project` first."
- `README.md` and `docs/architecture/g-mesh-v1.md`: one paragraph each,
  pointing at this file.
- `g-mesh clean` and `stop`: no semantic change. A front-mode state dir (a
  socket, a pid, `index.phase` and no `index.db`) is already tolerated by
  the code: `cli/status.rs:414` and `gc/last_used.rs:106` both check for a
  missing `index.db`. Slice 4 keeps a regression test on it.

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

**Open for GM-399 (2026-09-24).** Q2, Q3, Q4 and Q6 above stand as accepted.
The revision of D10-D12 against the current code raised these:

- **Q7. Rule 2 and an index from an older generation.** D10 rule 2 serves a
  root as one project when `bulkIndexedAt` is set. But `schema::
  ensure_current` (`daemon/mod.rs:437`) wipes an index whose schema or
  generation differs, and the next tool call re-walks the *whole folder*.
  Should rule 2 also require the stored generation to match (which needs
  plugin discovery before detection, undoing D10's reorder), or count any
  completed index? **Recommend: any completed index,** as Q2 accepted: `g-mesh
  init` in a parent folder stays an explicit choice, and `g-mesh clean`
  restores the front. (Affects slice 4's rule 2 test only.)
- **Q8. The instructions gap (D11 step 5).** Accept option (A), `C`'s
  guidance carried in the `select_project` result and therefore transient
  (lost on compaction), with (B) as a follow-up only if M3 part b shows
  agents losing it? **Recommend: yes.**
- **Q9. Three small departures from the accepted text,** each argued where
  it is made: detection runs before plugin discovery (D10); the front marks
  itself with `index.phase` = `front` instead of a new `daemon.mode` file,
  and supervises an empty plugin registry (D11 step 1); reselecting the
  current project is a full switch (D11 step 3). **Recommend: accept all
  three.**

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

### Slice 4 (GM-399 S2): Candidate detection, front mode and `select_project`

*GM-399 works this and slice 5 on `feat/GM-399-multi-project-front`, cut
from `release-3.12.0`; the "Common rules" above apply with `wt-gm399-ctl` as
the control worktree. Every test below names its **control**: the revert (of
code, never of the test) that must make it fail. The verification slice
builds each control in its own worktree and reports any that does not fail.*

**Goal:** A2 (front side) and A3.

**Change:**

- `daemon/candidates.rs` (new): D10.
- `cli/mod.rs`: the hidden `debug-candidates` subcommand.
- `daemon/mod.rs::run` (`:370`): after the build stamp (`:403`) and before
  discovery (`:426`), the mode decision (D10 rules, in order), then `Multi`
  goes to `daemon::front::run`.
- `daemon/front.rs` (new): D11 step 1 (bind, pid, `index.phase` = `front`,
  serving owner, accept loop with `FrontServer`, `supervise` with an empty
  registry and `FRONT_CORE_IDLE`).
- `mcp/front.rs` (new): `FrontServer` (D11 step 2). `mcp/instructions.rs`:
  `build_front` (D12).
- `cli/status.rs`: a `Some("front")` arm (`:615`) printing "folder of N
  projects - no index; a session selects one", and **skipping
  `index_status`** (`:302`) in that case: its `discover_source_files` walk
  over the whole folder is exactly the cost front mode exists to avoid.
  `index_status` (`:414`) and `gc::last_used` (`:106`) already tolerate a
  missing `index.db`, so `clean`, `clean orphaned` and GC need no change
  beyond the test in item 4.
- Before starting: check that no existing integration fixture has two or
  more marked subdirectories and no root marker (it would silently turn into
  a front). `tests/reexport_linking.rs:37` has a root `package.json`, so it
  stays single by rule 1.

**Tests:**

1. `candidates.rs` unit tests on tempdir fixtures:

   | Test | Control |
   |---|---|
   | three repos plus one worktree (a `.git` *file*) at depth 2 give exactly those four, `is_worktree` set on the one | accept only a `.git` *directory* as a marker: the worktree is missing |
   | a repo containing nested `package.json`s is listed once | remove "do not descend into a candidate": the nested packages appear |
   | `node_modules/x/package.json` is not listed | drop `node_modules` from the skip list |
   | a marker at depth 3 is not listed | `max_depth` = 3 |
   | a root with a marker and two marked children gives `Single` | remove rule 1 |
   | one candidate gives `Single` | threshold 1 instead of 2 |
   | a root with two marked children and an `index.db` whose `bulkIndexedAt` is set gives `Single`; the same with `bulkIndexedAt` NULL gives `Multi` | remove rule 2 (first half fails); treat "`index.db` exists" as completed (second half fails) |
   | the mode decision on a root with no `index.db` leaves none behind | open it through `connection::open` |
   | `max_entries` = 10 on a wide fixture gives `truncated` and `entries_read <= 10` | remove the entry-count check |
   | a symlink to a repo is not listed (Unix only) | use `metadata` instead of `symlink_metadata` |

2. `tests/multi_project_front.rs`: a fixture root holding `a/` (`.git`
   directory plus `a.ts`), `b/` (`.git` plus `b.ts`) and `c/` (`go.mod`), no
   root marker, driven through the real `mcp-shim` with rmcp's
   `TokioChildProcess`, as `tests/mcp_e2e.rs:101-110` does. Until slice 5
   lands the shim forwards the `_meta` untouched, which is what this test
   checks.

   | Assertion | Control |
   |---|---|
   | `peer_info().instructions` contains `a`, `b`, `c` and `select_project` | force `Single` in the mode decision |
   | `list_tools` is the 8 usual tools plus `select_project` | leave `select_project` out of `list_tools` |
   | `select_project {}` lists all three; an unknown `project` is `isError` listing them | return an empty list |
   | `select_project {project:"b"}` has `_meta["g-mesh/switchProject"].root` equal to `b`'s canonical path | omit `meta` |
   | `find_references` returns the "none is selected" error naming `a, b, c` | answer with an empty success result |
   | `get_file_outline {file_path:"b/b.ts"}` names `b` | drop the `file_path` hint |
   | `project_dir(root)` has no `index.db`, `index.phase` reads `front` (`common::wait_until_phase`), and no plugin pid file exists | force `Single`: `connection::open` creates `index.db` and the phase reads `unindexed` |
   | with `G_MESH_FRONT_IDLE_MS=500`, the front's pid file is gone within `startup_timeout()` after the client disconnects | ignore the env var (60 s) |

3. `mcp/instructions.rs` unit tests for `build_front`:
   - 64 candidates with 60-byte names under a 103-byte root: ≤ ceiling, at
     least one name, and the `(+K more` suffix. *Control:* list every name
     with no ceiling check.
   - A 1,400-byte root: ≤ ceiling and no root path in the text. *Control:*
     remove the no-path fallback. (*Changed from 600 bytes in slice 4:* the
     front's text has no language paragraphs, so a 600-byte root still fits
     beside it and would never reach the fallback.)
4. `tests/cli_status.rs`: `g-mesh status` in a front-served root prints the
   front line and no coverage line. *Control:* remove the `Some("front")`
   arm. `tests/cli_clean.rs`: `clean` and `clean orphaned` on a front state
   dir (socket, pid, `index.phase`, no `index.db`) succeed. This one is a
   regression guard with no code change behind it, so it has no control;
   say so in the report.

**Exit:** tests green; M4 run and recorded.

### Slice 5 (GM-399 S3): Session switch in the shim

**Goal:** A2 (select, then work on the selected project), end to end.

**Change:** in `shim.rs`, replace `proxy`/`pump` (`:469-507`) with a
switchable router (D11 step 3). *As built* it lives in `shim/router.rs`,
generic over its streams so the byte-identity unit test drives it through
`std::io::pipe`; stdout is written by the thread running `router::serve`,
fed by the channel. Slice 4's front test no longer checks the `_meta`
directive through the shim, since the shim now consumes it:

- `Router { front: Upstream, current: Upstream, init_frame,
  initialized_frame, select_ids: HashSet<serde_json::Value>, replay_seq }`
  behind a `Mutex`; stdout owned by a single writer thread fed by an
  `mpsc` channel from every reader.
- The stdin thread parses each client frame with `serde_json`. A frame that
  fails to parse is forwarded raw to `current`.
- Each upstream reader checks `select_ids` only on the front and only for
  response frames, so large tool results from sub-project daemons are never
  parsed.
- The switch holds the router lock across bootstrap and replay: client
  frames wait, at most the bootstrap timeout (`shim.rs:399`).
- Exit: on stdin EOF, shut down the write half of every upstream. The shim
  exits when the current upstream's reader ends: the same "shim lives as
  long as its session" semantics as today.
- Update `shim.rs`'s module and `pump` doc comments: the "never parses a
  payload" invariant now holds only for single-project sessions.

**Tests** (all in `tests/multi_project_front.rs`, same fixture as slice 4,
plus router unit tests in `shim.rs`):

| Test | What it asserts | Control |
|---|---|---|
| `selecting_a_project_switches_the_session` | after `select_project {project:"b"}`, `get_file_outline {file_path:"b.ts"}` returns `b.ts`'s outline; afterwards `project_dir(b)/index.db` has `bulkIndexedAt` set, and `project_dir(root)` and `project_dir(a)` have no `index.db` | the shim ignores the directive: the outline call gets the front's "none is selected" error |
| `select_project_carries_the_selected_projects_own_guidance` **(the instructions gap)** | (1) with `b` unindexed, the result text contains `this session now serves <canon b>`, then guidance that opens with `cold_start`'s line (`Index root: <canon b>. Not indexed yet`, or, *as built*, its no-root fallback `Not indexed yet`, which a long temp dir triggers under the byte ceiling), and `P1`'s first sentence; (2) `peer_info().instructions` is still the front's text, pinning the known limitation so a change in it is noticed; (3) the result has no `_meta` directive | (1) skip D11 step 3.6 (forward the front's result as is); (3) keep the directive |
| `guidance_reflects_the_projects_current_state` | index `b` first (a separate shim session started in `b`, then `common::wait_until_indexed`), then from the root session select `b`: the text contains `P1` and does **not** contain `Not indexed yet` | the shim appends a text rendered from `instructions::cold_start(b, false, ..)` instead of the replayed one. This is the control that tells "`C`'s own live text" apart from "a plausible text about `C`" |
| `reselecting_the_same_project_refreshes_its_guidance` | select `b` (text says `Not indexed yet`), call `get_file_outline` and wait until indexed, select `b` again: the second text no longer says `Not indexed yet` | a same-project shortcut that re-sends the first switch's cached instructions |
| `reselecting_switches_again` | select `b`, then `a`: `get_file_outline {file_path:"a.ts"}` works and `{file_path:"b.ts"}` is a not-found result from `a`'s daemon (the session no longer talks to `b`) | keep routing to the first sub-project upstream |
| `a_failed_switch_leaves_the_session_on_the_front` (Unix) | pre-create `project_dir(b)` with mode `000` so `b`'s bootstrap fails; `select_project {project:"b"}` is `isError` naming the failure, and a following `find_references` still gets the front's "none is selected" error | set `current` before the bootstrap result is checked: the next call errors at transport level or reaches no daemon |
| `progress_passes_through_after_a_switch` | with `G_MESH_PROGRESS_INTERVAL_MS=200`, a first `get_file_outline` on unindexed `b` sent with a progress token receives at least one `notifications/progress` | the sub-project reader forwards only response frames |
| router unit test: single-project frames are byte-identical | a scripted upstream and client exchange frames with unusual key order and whitespace, plus one unparsable line; the output bytes equal the input bytes | re-serialize parsed frames instead of forwarding the original bytes |

Regression: `tests/mcp_e2e.rs`, `tests/shim_bootstrap.rs`,
`tests/shim_handle_inheritance.rs` and `tests/index_wait_progress.rs` stay
green, since the single-project path must be unchanged. The stdout
single-writer design has no test of its own: interleaving is impossible by
construction, and no reliable control exists for a race; the report says so.

**Exit:** tests green, and a manual session in `~/Projects/ClaudeProjects`
(M3 part b) recorded, including one context compaction to see whether the
agent re-calls `select_project` for the guidance (Q8).

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
