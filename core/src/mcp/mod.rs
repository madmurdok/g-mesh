//! The MCP server the daemon exposes on its AF_UNIX socket: the structural
//! tool surface an agent sees, plus the plumbing that binds one accepted
//! connection to one `rmcp` session.
//!
//! Every tool is registered with its real name, description and parameter
//! schema, and each one's answer lives in a module of its own next to this
//! one - the schemas were registered up front, before the handlers existed,
//! so a client could be wired against the finished surface rather than watch
//! tools appear one by one; this file has stayed pure router wiring since.
//!
//! Why `rmcp` (and therefore tokio) only here: the rest of the daemon - SQLite,
//! the plugin bridge, the watcher - is plainly synchronous and has no reason
//! not to be. `daemon::run` enters a small runtime for its accept loop alone,
//! so async stops at this module's front door; handlers below take the same
//! synchronous `IndexStore` read guard as the rest of the daemon.
//!
//! # Why the parameter doc comments below are terse
//!
//! Every `///` on a parameter struct in this file is compiled by `schemars`
//! into that tool's JSON Schema `description`, and the whole `tools/list`
//! response sits in the model's cached prompt prefix - so it is re-read, and
//! re-billed as `cacheReadTokens`, on *every* request of *every* conversation,
//! not once. A sentence of rationale here therefore costs far more over a
//! session than the same sentence in a `//` comment, which never reaches the
//! wire at all.
//!
//! Measured (task f05b320f, against serena's 5-tool surface as a reference
//! point): the eight schemas were 11,722 bytes, 62% of it description text,
//! with `file_paths`/`symbol_name`/`limit`/`symbol_id`/`cursor` each repeated
//! near-verbatim across three to six tools. Compressing that prose - without
//! dropping a single fact a caller acts on - took the surface to 9,845 bytes,
//! roughly 600 fewer prompt tokens on every request.
//!
//! So: state defaults, caps, mutual exclusions and the ambiguity protocol,
//! because a caller's behaviour changes on them. Put the *why* in a `//`
//! comment or this module doc instead, and keep guidance that spans tools in
//! `get_info`'s `instructions` (sent once per session, not once per tool).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Implementation, ProgressNotificationParam, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::daemon::indexing_status::{IndexingStatus, Need, Phase, WaitOutcome};
use crate::daemon::lifecycle::CoreActivity;
use crate::daemon::manifest::Capabilities;
use crate::daemon::registry::PluginRegistry;
use crate::embedding::EmbeddingPipeline;
use crate::gc::last_used;
use crate::graph::pagination::Direction;
use crate::ipc::AsyncStream;
use crate::protocol::types::Position;
use crate::storage::index_store::IndexStore;

mod anchor;
// `pub(crate)` on these five - not `mod` - so `cli::plugin_check::expectations`
// (GM-277) can call the exact handler functions these modules' own tool
// methods below call, rather than re-implementing the queries. Every other
// submodule here stays private to this one: nothing outside `mcp` needs
// `get_file_outline`/`instructions`/`search_code`/`source`/`tool_result`, and
// widening them would just be surface nothing uses.
pub(crate) mod find_callers_callees;
pub(crate) mod find_definition;
pub(crate) mod find_implementations;
pub(crate) mod find_references;
pub mod front;
pub(crate) mod get_dependencies;
mod get_file_outline;
mod instructions;
mod provenance;
mod search_code;
mod similarity;
mod source;
mod tool_result;

/// Names a step of request handling in the daemon's log, when
/// [`TRACE_CALLS_ENV`] is set.
///
/// Every tool call funnels through `prepare`, so tracing its entry and exit
/// splits a hang three ways with one line of output: nothing logged means the
/// request never reached the daemon, an entry without a matching exit means it
/// hung inside `prepare`, and both means it hung in the handler or on the way
/// back. Windows currently hangs somewhere in there and the log is the only
/// window into a detached daemon (GM-246).
///
/// Off unless asked for: a line per call would bury the log's real content on
/// a busy daemon.
///
/// GM-395 slice 3 widened what `prepare` traces, because M1 in
/// `docs/architecture/lazy-indexing.md` reads its results off these lines:
/// entry names the tool, the request id and whether the request carried a
/// `progressToken` (the one fact about a client's behaviour the daemon can
/// see and M1 cannot otherwise observe), and the end of the wait names its
/// outcome, how long it waited and how many progress notifications it sent.
/// The shapes, one line each:
///
/// ```text
/// g-mesh daemon: prepare: entered tool=get_file_outline request=3 progressToken=present
/// g-mesh daemon: prepare: wait over tool=get_file_outline request=3 outcome=satisfied waited_ms=4212 progress_sent=21
/// g-mesh daemon: prepare: cancelled tool=get_file_outline request=3 waited_ms=900 progress_sent=4
/// ```
///
/// `outcome` is `satisfied`, `failed` or `timed_out` (the D7 cap).
fn trace_call(what: std::fmt::Arguments<'_>) {
    if std::env::var_os(TRACE_CALLS_ENV).is_some_and(|v| !v.is_empty()) {
        eprintln!("g-mesh daemon: {what}");
    }
}

/// Turns on [`trace_call`]. Any non-empty value.
pub const TRACE_CALLS_ENV: &str = "G_MESH_TRACE_CALLS";

/// How often a tool call that is waiting for the index sends a progress
/// notification, in milliseconds - only ever to a request that carried a
/// `progressToken` (D6 in `docs/architecture/lazy-indexing.md`). `0` sends
/// none at all, which is M1's "progress off" arm. Unset or unparsable means
/// [`DEFAULT_PROGRESS_INTERVAL`].
pub const PROGRESS_INTERVAL_ENV: &str = "G_MESH_PROGRESS_INTERVAL_MS";

/// D6's heartbeat: often enough to keep an idle timer measured in tens of
/// seconds or more alive, rarely enough to be no load at all.
pub const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// The longest a tool call waits for the index before it gives up with an
/// explicit "still indexing, call again" tool error, in milliseconds (D7).
/// `0` removes the cap. Unset or unparsable means [`DEFAULT_INDEX_WAIT_CAP`].
pub const INDEX_WAIT_CAP_ENV: &str = "G_MESH_INDEX_WAIT_CAP_MS";

/// D7's default: under Claude Code's 30 min stdio idle window, so even a call
/// that carries no progress token ends with an answer the agent can act on
/// before the client aborts it with one that says nothing about indexing.
pub const DEFAULT_INDEX_WAIT_CAP: Duration = Duration::from_secs(25 * 60);

/// Reads a millisecond-valued env var, falling back to `default` when it is
/// unset, empty or not a number. Read per call, not once per process: it is a
/// few nanoseconds, and a knob that only took effect on the next daemon would
/// be one more thing to get wrong in a test.
fn env_millis(name: &str, default: Duration) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(default)
}

/// A wait duration for a person: `"950ms"`, `"42s"`, `"18m 5s"`.
fn human_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs == 0 {
        format!("{}ms", elapsed.as_millis())
    } else if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}

/// Serves one accepted connection as an MCP session until the peer
/// disconnects. One session per connection, and the shim opens exactly one
/// connection per MCP client, so a client's session dies with its shim.
///
/// `stream` is whatever `crate::ipc` accepted - a `tokio::net::UnixStream` on
/// Unix, a connected named-pipe instance on Windows. Neither is named here:
/// `rmcp` only ever needed `AsyncRead + AsyncWrite`.
///
/// `indexing` is consulted per *call*, not per connection, which is what
/// makes a session opened during the cold-start walk recover on its own: a
/// tool call issued on a connection made mid-walk simply waits
/// (`GMeshMcpServer::wait_for_index`) and gets the real answer once
/// the walk finishes, on the very same session - nothing to reconnect and
/// nothing to re-initialize.
pub async fn serve_connection(
    stream: AsyncStream,
    store: Arc<IndexStore>,
    registry: Arc<PluginRegistry>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
    embedding: Arc<EmbeddingPipeline>,
) -> Result<()> {
    let service = GMeshMcpServer::new(store, registry, core_activity, indexing, embedding)
        .serve(stream)
        .await
        .context("MCP initialization failed")?;
    service.waiting().await.context("MCP session task failed")?;
    Ok(())
}

/// The structural query surface, backed by the project's index and the
/// language plugins.
///
/// Every handler answers out of `store` alone. `registry` is not there to be
/// queried - no tool asks a language server a question - but to be *woken*:
/// while a language's plugin sleeps on its idle timeout the core queues the
/// files that changed, and a tool call is the moment that queue has to be
/// replayed before the index is read (see `daemon::lifecycle`). It is the
/// whole `PluginRegistry` because a tool call has no way to know ahead of
/// time which language(s) its answer might touch.
#[derive(Clone)]
pub struct GMeshMcpServer {
    store: Arc<IndexStore>,
    registry: Arc<PluginRegistry>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
    embedding: Arc<EmbeddingPipeline>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl GMeshMcpServer {
    pub fn new(
        store: Arc<IndexStore>,
        registry: Arc<PluginRegistry>,
        core_activity: Arc<CoreActivity>,
        indexing: IndexingStatus,
        embedding: Arc<EmbeddingPipeline>,
    ) -> Self {
        Self { store, registry, core_activity, indexing, embedding, tool_router: Self::tool_router() }
    }

    /// Everything every handler owes before it reads the index, in the one
    /// order they may happen in.
    ///
    /// `Ok(Some)` is a finished tool result the handler returns as is: a
    /// failed walk ([`Phase::Failed`]), reported as a tool error carrying the
    /// failure's message, or a wait that reached the D7 cap ("still
    /// indexing, call again"). `Err` means the client cancelled the request
    /// (or its session ended) while it waited - see
    /// [`wait_for_index`](Self::wait_for_index). `tool` only names the call
    /// in [`trace_call`]'s lines.
    ///
    /// The order is not arbitrary:
    ///
    /// 0. [`IndexingStatus::request_activation`] before anything else
    ///    (GM-395 slice 2): the daemon builds nothing until a tool call
    ///    needs it, so this is what starts the walk, the semantic pass and
    ///    the embedding backfill pass - once, however many calls ask. It
    ///    returns at once; the work runs on `daemon::activation`'s thread,
    ///    independently of this call.
    /// 1. [`wait_for_index`](Self::wait_for_index) next: a project that has
    ///    not yet reached `need`'s phase has no graph (or no complete-enough
    ///    graph) to bring up to date, so every step after this one may assume
    ///    it does rather than each having to ask again. It never answers
    ///    off a half-built graph; the one way it ends without the phase it
    ///    needs is the D7 cap, which answers nothing at all - see
    ///    [`wait_for_index`](Self::wait_for_index)'s own doc comment.
    /// 2. [`mark_used`](Self::mark_used) next: it takes and releases the
    ///    store on its own, and it must not be nested inside the plugin lock
    ///    the replay below holds (see `storage::index_store`'s lock order).
    /// 3. The replay last, so the rows this call is about to read already
    ///    include every change made while the plugin was asleep.
    async fn prepare(
        &self,
        ctx: &RequestContext<RoleServer>,
        tool: &str,
        need: Need,
    ) -> Result<Option<CallToolResult>, ErrorData> {
        // Taken before anything below runs, including the indexing wait, so
        // that if the replay at the bottom also has to heartbeat (GM-403) its
        // `progress` values pick up exactly where the wait's own left off -
        // the same reason `ensure_file_fresh`'s caller takes its own
        // `call_started` ahead of `prepare` (see that method's doc comment).
        let call_started = Instant::now();
        let token = ctx.meta.get_progress_token();
        trace_call(format_args!(
            "prepare: entered tool={tool} request={} progressToken={}",
            ctx.id,
            if token.is_some() { "present" } else { "absent" }
        ));
        self.indexing.request_activation();
        if let Some(early) = self.wait_for_index(ctx, tool, need).await? {
            return Ok(Some(early));
        }
        trace_call(format_args!("prepare: past the indexing wait tool={tool} request={}", ctx.id));
        self.mark_used();
        self.replay_queued_changes(ctx, tool, call_started).await;
        trace_call(format_args!("prepare: done tool={tool} request={}", ctx.id));
        Ok(None)
    }

    /// Brings the index up to date with whatever changed while any language's
    /// plugin was asleep, and does nothing at all - not even a lock - when
    /// nothing did, which is every call on a daemon whose active plugins are
    /// all awake.
    ///
    /// On the blocking thread pool rather than inline: waking a plugin means
    /// spawning a process (for the bundled one, Node plus warming tree-sitter),
    /// and the accept loop's runtime has two worker threads (see
    /// `daemon::serve_forever`), so doing it on one of them would stall every
    /// other session for the length of a process spawn.
    ///
    /// Failures are logged inside `PluginRegistry::replay_pending`, one per
    /// language, not returned here. The caller asked a structural question the
    /// index can already answer; refusing it because a *later* edit could not
    /// be replayed would turn one unreadable file into a dead tool surface,
    /// and each language's queue is left intact for the next call to retry.
    ///
    /// # GM-403: heartbeats while a replay runs
    ///
    /// A replay is a `fileChanged` round trip (plus a per-file semantic pass)
    /// for every queued file, sent to whichever language's plugin was asleep
    /// - on a cold language server that is the same tens-of-seconds cost
    /// [`ensure_file_fresh`](Self::ensure_file_fresh) already heartbeats for
    /// GM-401, just paid for a whole queue instead of one file. Without a
    /// ticker of its own this step was the one silent gap GM-401 left: the
    /// indexing wait's heartbeat had already ended by the time `prepare`
    /// reaches here.
    ///
    /// The same rules as both of those tickers: only for a request that
    /// carried a `progressToken`, one interval in, a failed send logged and
    /// ignored. `message` names the language(s) and how many files each owes
    /// ([`PluginRegistry::pending_summary`]), read once before the replay
    /// starts draining the queue it describes, not on every tick - a ticker
    /// that re-asked mid-replay would watch the count fall to zero and call
    /// that news. `progress` is `call_started.elapsed()` - taken by
    /// [`prepare`](Self::prepare) before the indexing wait, so it keeps
    /// strictly increasing whether or not that wait also heartbeated.
    ///
    /// Not cancellable, as before: the replay runs on the blocking pool and
    /// finishes whether or not anyone is still listening.
    async fn replay_queued_changes(
        &self,
        ctx: &RequestContext<RoleServer>,
        tool: &str,
        call_started: Instant,
    ) {
        if !self.registry.has_pending() {
            return;
        }
        let summary = self.registry.pending_summary();
        let registry = Arc::clone(&self.registry);
        let store = Arc::clone(&self.store);
        let started = Instant::now();
        let task = tokio::task::spawn_blocking(move || registry.replay_pending(&store));
        tokio::pin!(task);

        let interval = env_millis(PROGRESS_INTERVAL_ENV, DEFAULT_PROGRESS_INTERVAL);
        let token = ctx.meta.get_progress_token().filter(|_| !interval.is_zero());
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::from_std(started) + interval.max(Duration::from_millis(1)),
            interval.max(Duration::from_millis(1)),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sent = 0u32;

        let joined = loop {
            tokio::select! {
                biased;
                joined = &mut task => break joined,
                _ = ticker.tick(), if token.is_some() => {
                    let token = token.clone().expect("the branch is only enabled with a token");
                    let message = format!(
                        "indexing {}: replaying queued changes for {summary} before answering ({} so far)",
                        self.registry.project_root().display(),
                        human_duration(started.elapsed())
                    );
                    let param = ProgressNotificationParam::new(token, call_started.elapsed().as_secs_f64())
                        .with_message(message);
                    match ctx.peer.notify_progress(param).await {
                        Ok(()) => sent += 1,
                        Err(err) => eprintln!(
                            "g-mesh daemon: could not send a progress notification for request {}: {err}",
                            ctx.id
                        ),
                    }
                }
            }
        };

        let replayed = match joined {
            Ok(count) => count,
            Err(err) => {
                eprintln!("g-mesh daemon: the plugin wake task failed: {err}");
                0
            }
        };
        trace_call(format_args!(
            "replay: tool={tool} request={} summary={summary} replayed={replayed} elapsed_ms={} \
             progress_sent={sent}",
            ctx.id,
            started.elapsed().as_millis()
        ));
    }

    /// Every discovered plugin's declared `[plugin.capabilities]`, which the
    /// four edge-walking tools hand to `provenance::resolve` so it can tell a
    /// plugin that declares a semantic tier from one that does not.
    ///
    /// Read fresh per call, and cloned, for exactly the reason
    /// `get_dependencies`' own `entry_points` is: it is a map walk over data
    /// that cannot change while this daemon runs
    /// (`daemon::manifest::discover`'s contract), so a cache on `self` would
    /// save nothing that holding the borrow does not already give away, and
    /// four tools reaching through `self.registry` inline would say this
    /// once each instead of once.
    fn capabilities(&self) -> HashMap<String, Capabilities> {
        self.registry.receiver_call_capabilities()
    }

    /// Query-time staleness safety net (`watcher::staleness::ensure_fresh`,
    /// wired via `daemon::lifecycle::PluginSupervisor::ensure_fresh`) for the
    /// three tools that anchor their answer on one specific file:
    /// `find_definition`, `get_file_outline`, `get_dependencies`. Closes a
    /// gap [`replay_queued_changes`](Self::replay_queued_changes) does not:
    /// that one catches up on changes the watcher *did* see while the plugin
    /// slept, but a change the watcher never saw at all - because this
    /// project's daemon was not running when it happened, or because the
    /// watcher backend missed the filesystem event outright - never enters
    /// that queue in the first place. See `watcher::staleness`'s module doc
    /// for the full mtime/hash procedure this runs, and
    /// `PluginSupervisor::ensure_fresh`'s doc for why this is a different gap
    /// from the one `daemon::indexing_status` documents leaving open.
    ///
    /// Not called for the four symbol-anchored tools (`find_references`,
    /// `find_callers`, `find_callees`, `find_implementations`): resolving
    /// which file(s) their answer even touches requires running the query
    /// itself, so there is no single file to check ahead of it without
    /// paying to read and hash every file the query *might* touch - exactly
    /// the cost `watcher::staleness`'s own docs rule out.
    ///
    /// On the blocking thread pool for the same reason
    /// [`replay_queued_changes`](Self::replay_queued_changes) is: the rare
    /// case where this actually reindexes means a synchronous round trip to
    /// the plugin process. Best-effort like that method too - a failure here
    /// must not turn an otherwise-answerable query into a tool error, so it
    /// is logged and the handler proceeds with whatever the index currently
    /// holds.
    ///
    /// # GM-401: heartbeats while a reindex runs
    ///
    /// A reindex here is a `fileChanged` round trip plus a per-file semantic
    /// pass, and the latter waits on the language server: over a minute on a
    /// cold rust-analyzer. [`wait_for_index`](Self::wait_for_index)'s
    /// heartbeat has ended by then, so without one of its own the client
    /// heard nothing for that whole stretch and its idle timer, not this
    /// daemon, decided when the call ended. The same ticker runs here, under
    /// the same rules: only for a request that carried a `progressToken`, one
    /// interval in (the fast path sends nothing), and a failed send is logged
    /// and ignored. `progress` is seconds since `call_started`, which the
    /// handler takes *before* [`prepare`](Self::prepare) - earlier than the
    /// indexing wait's own start - so it keeps strictly increasing across
    /// both heartbeats of one call.
    ///
    /// Not cancellable, as before: the reindex runs on the blocking pool and
    /// finishes whether or not anyone is still listening.
    async fn ensure_file_fresh(
        &self,
        ctx: &RequestContext<RoleServer>,
        tool: &str,
        call_started: Instant,
        file_path: &str,
    ) {
        let registry = Arc::clone(&self.registry);
        let store = Arc::clone(&self.store);
        let owned_path = file_path.to_string();
        let task_path = owned_path.clone();
        let started = Instant::now();
        let task = tokio::task::spawn_blocking(move || registry.ensure_fresh(&store, &task_path));
        tokio::pin!(task);

        let interval = env_millis(PROGRESS_INTERVAL_ENV, DEFAULT_PROGRESS_INTERVAL);
        let token = ctx.meta.get_progress_token().filter(|_| !interval.is_zero());
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::from_std(started) + interval.max(Duration::from_millis(1)),
            interval.max(Duration::from_millis(1)),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sent = 0u32;

        let joined = loop {
            tokio::select! {
                biased;
                joined = &mut task => break joined,
                _ = ticker.tick(), if token.is_some() => {
                    let token = token.clone().expect("the branch is only enabled with a token");
                    let message = format!(
                        "indexing {}: bringing {owned_path} up to date before answering ({} so far)",
                        self.registry.project_root().display(),
                        human_duration(started.elapsed())
                    );
                    let param = ProgressNotificationParam::new(token, call_started.elapsed().as_secs_f64())
                        .with_message(message);
                    match ctx.peer.notify_progress(param).await {
                        Ok(()) => sent += 1,
                        Err(err) => eprintln!(
                            "g-mesh daemon: could not send a progress notification for request {}: {err}",
                            ctx.id
                        ),
                    }
                }
            }
        };

        let outcome = match &joined {
            Ok(Ok(Some(outcome))) => format!("{outcome:?}"),
            Ok(Ok(None)) => "no_plugin".to_string(),
            Ok(Err(_)) => "failed".to_string(),
            Err(_) => "task_failed".to_string(),
        };
        trace_call(format_args!(
            "ensure_fresh: tool={tool} request={} file={owned_path} outcome={outcome} elapsed_ms={} \
             progress_sent={sent}",
            ctx.id,
            started.elapsed().as_millis()
        ));
        match joined {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => {
                eprintln!("g-mesh daemon: query-time staleness check failed for {owned_path}: {err:#}")
            }
            Err(err) => eprintln!("g-mesh daemon: the staleness-check task failed: {err}"),
        }
    }

    /// Waits until the index has reached the phase `need` requires, if it has
    /// not already.
    ///
    /// # GM-394: a tool call is never answered "not ready"
    ///
    /// Task 105 answered a call landing mid-walk with an immediate
    /// `STILL_INDEXING` tool error, and task 107 gave a call landing a few
    /// milliseconds before completion a short grace wait before committing to
    /// that refusal. Both are gone: GM-394's owner decision is that no tool
    /// call may ever be answered "not ready" or partially while the index is
    /// being built - a caller retrying an error is strictly worse than a
    /// caller that simply waits a little longer for the truth - so this now
    /// waits - bounded only by the D7 cap below - for `need` to be satisfied
    /// (`daemon::indexing_status::IndexingStatus::wait_for`) and lets the
    /// handler serve the real answer.
    ///
    /// # GM-395 slice 3: progress, the wait cap and cancellation (D6, D7)
    ///
    /// Three things can end the wait besides the phase being reached, raced
    /// in one `select!`:
    ///
    /// - **Progress.** While it waits, and only if the request carried a
    ///   `progressToken` (the spec ties progress to a requester's token), a
    ///   ticker sends `notifications/progress` every
    ///   [`PROGRESS_INTERVAL_ENV`] (default 5 s). `progress` is the seconds
    ///   waited so far - strictly increasing as the spec requires, which no
    ///   work counter is (they stall while linking and during the one-time
    ///   model load) - and `message` carries the real counters
    ///   (`IndexingStatus::progress_message`). A send failure is logged and
    ///   ignored: the client may already be gone, and the cancellation branch
    ///   is what reacts to that.
    /// - **The cap** ([`INDEX_WAIT_CAP_ENV`], default 25 min). On reaching
    ///   it the call returns a tool error saying the index is still being
    ///   built and no answer was computed. That does not break "never a
    ///   partial answer": it answers no part of the question - no rows, no
    ///   `hasMore` - it is an explicit, retryable precondition failure, and it
    ///   only fires when a walk outlasts the cap. Without it a call with no
    ///   progress token would outlive the client's own idle window and be
    ///   killed with an error that says nothing about indexing.
    /// - **Cancellation.** `ctx.ct` fires on `notifications/cancelled` for
    ///   this request and when the session ends. The call returns an error at
    ///   once (the client has stopped listening for it). Indexing itself runs
    ///   on `daemon::activation`'s thread and does not notice (D2): the next
    ///   call finds it running or finished.
    ///
    /// # GM-395: which phase `need` names is what makes structural tools stop
    /// waiting on embeddings
    ///
    /// The seven structural tools pass [`Need::Structural`], satisfied the
    /// moment the walk itself is linked - the embedding backfill pass
    /// (`embedding::backfill::run`) may still be running, or not yet started,
    /// and they do not care. `search_code` alone passes
    /// [`Need::Embeddings`], satisfied only once that pass has finished.
    ///
    /// This is unrelated to why `get_info`/`instructions` never call this
    /// method at all: those run during MCP `initialize`, before a session has
    /// asked a single tool question, and must answer however long the walk's
    /// batch-commit lock is held for rather than wait on it - see
    /// [`instructions`](Self::instructions)'s own doc comment.
    ///
    /// [`prepare`](Self::prepare) calls this as its *first* step, ahead of
    /// [`mark_used`](Self::mark_used): that one takes the index store, which
    /// the walk holds for the length of each batch commit, and asking it to
    /// record usage while the store might still be held would defeat the
    /// point of waiting here first.
    ///
    /// The fast path - `need` was already satisfied - never suspends: a
    /// project that owes no walk, or one whose walk finished before this call
    /// arrived, resolves on the spot inside `wait_for`. Only a call that
    /// actually lands before its phase is reached suspends, and it suspends
    /// the task, not the worker thread it runs on (see
    /// `daemon::serve_forever`'s two-worker runtime, and `IndexingStatus`'s
    /// own doc comment for why a `Notify` rather than a blocking primitive).
    async fn wait_for_index(
        &self,
        ctx: &RequestContext<RoleServer>,
        tool: &str,
        need: Need,
    ) -> Result<Option<CallToolResult>, ErrorData> {
        let started = Instant::now();
        let cap = env_millis(INDEX_WAIT_CAP_ENV, DEFAULT_INDEX_WAIT_CAP);
        // `0` is "no cap": `checked_add` failing (a cap too large to be a
        // real instant) means the same.
        let deadline = if cap.is_zero() { None } else { started.checked_add(cap) };
        let interval = env_millis(PROGRESS_INTERVAL_ENV, DEFAULT_PROGRESS_INTERVAL);
        let token = ctx.meta.get_progress_token().filter(|_| !interval.is_zero());

        // The ticker's first tick is one interval in, not immediately: a
        // call that resolves on the fast path, or within one interval, sends
        // nothing. `Delay` so a notification send that stalled does not come
        // back to a burst of catch-up ticks.
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::from_std(started) + interval.max(Duration::from_millis(1)),
            interval.max(Duration::from_millis(1)),
        );
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sent = 0u32;

        let wait = self.indexing.wait_for(need, deadline);
        tokio::pin!(wait);
        let outcome = loop {
            tokio::select! {
                // Biased, wait first: an outcome that is ready wins over a
                // tick or a cancellation that happens to be ready as well.
                biased;
                outcome = &mut wait => break outcome,
                () = ctx.ct.cancelled() => {
                    trace_call(format_args!(
                        "prepare: cancelled tool={tool} request={} waited_ms={} progress_sent={sent}",
                        ctx.id,
                        started.elapsed().as_millis()
                    ));
                    return Err(ErrorData::internal_error(
                        "g-mesh: the request was cancelled while it waited for the index; the index keeps \
                         building in the background",
                        None,
                    ));
                }
                _ = ticker.tick(), if token.is_some() => {
                    let token = token.clone().expect("the branch is only enabled with a token");
                    let param = ProgressNotificationParam::new(token, started.elapsed().as_secs_f64())
                        .with_message(self.indexing.progress_message(self.registry.project_root()));
                    match ctx.peer.notify_progress(param).await {
                        Ok(()) => sent += 1,
                        Err(err) => eprintln!(
                            "g-mesh daemon: could not send a progress notification for request {}: {err}",
                            ctx.id
                        ),
                    }
                }
            }
        };

        trace_call(format_args!(
            "prepare: wait over tool={tool} request={} outcome={} waited_ms={} progress_sent={sent}",
            ctx.id,
            match &outcome {
                WaitOutcome::Satisfied => "satisfied",
                WaitOutcome::Failed(_) => "failed",
                WaitOutcome::TimedOut => "timed_out",
            },
            started.elapsed().as_millis()
        ));

        Ok(match outcome {
            WaitOutcome::Satisfied => None,
            // D7: the cap. No part of the question is answered - no rows,
            // nothing that could be read as a result - only why, and that
            // calling again is the remedy.
            WaitOutcome::TimedOut => {
                Some(CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                    "g-mesh: the index for {} is still being built ({}; this call waited {}). No answer was \
                 computed - call this tool again; the index keeps building in the background.",
                    self.registry.project_root().display(),
                    self.indexing.progress_detail(),
                    human_duration(started.elapsed())
                ))]))
            }
            // GM-395 slice 2 (D2): a failed walk is a tool error carrying its
            // message, not an answer read off an empty or partial graph -
            // that would be confidently wrong, where this says why and that
            // the next call retries (`IndexingStatus::request_activation`).
            // Not "not ready" either: the wait is over, and it failed.
            WaitOutcome::Failed(message) => {
                Some(CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                    "g-mesh could not build this project's index: {message}. The next tool call retries \
                     the build."
                ))]))
            }
        })
    }

    /// Advances the project's `lastUsed` stamp, which a later GC scan reads
    /// back off disk to decide how long a project has been idle
    /// (`gc::last_used`), and the core's own in-memory idle clock, which
    /// decides when this daemon has been unused long enough to exit
    /// (`daemon::lifecycle::CoreActivity`).
    ///
    /// The two are separate on purpose and neither can stand in for the other:
    /// `lastUsed` is a durable record about the *project*, read by a command
    /// that may run days later on disk this daemon no longer owns, while the
    /// idle clock is a fact about this *process* that means nothing once it
    /// exits. They are advanced together because one thing advances both - a
    /// tool call.
    ///
    /// Called by every tool handler rather than once per connection: a client
    /// holds one session open for its whole lifetime, so per-connection would
    /// stamp a week-long editor session exactly once, at the start.
    ///
    /// Best-effort on purpose - a failure is reported and dropped. Bookkeeping
    /// for a cleanup command that only ever prints warnings has no business
    /// turning an answerable query into a tool error. The store is taken and
    /// released here, before the handler takes its own read guard.
    fn mark_used(&self) {
        self.core_activity.request();
        if let Err(err) = self.store.with(last_used::touch) {
            eprintln!("g-mesh daemon: failed to record lastUsed: {err:#}");
        }
    }

    /// `get_info`'s `with_instructions` string, assembled fresh for each
    /// session by [`instructions::build`] from the languages actually present
    /// in this project's index and their capabilities - see that module's own
    /// doc comment for the design (why the receiver-call gap sentence varies
    /// per language, the byte budget it renders under, and the two fallbacks
    /// below).
    ///
    /// # Never takes the store while a bulk-index batch may be holding it
    ///
    /// `get_info` is called during MCP `initialize`, before a client has
    /// asked a single tool question - unlike every tool handler above, which
    /// waits out the cold-start walk via [`prepare`](Self::prepare) before it
    /// ever reaches for `self.store`, this method cannot afford to wait on
    /// anything: blocking the handshake is indistinguishable from the whole
    /// server hanging, and a batch commit holds the store through its
    /// embedding inference (see `daemon::bulk_index::commit` and
    /// `daemon::indexing_status`).
    ///
    /// So [`self.indexing.phase()`](IndexingStatus::phase) - a lock-free
    /// atomic read - is checked *first*, and only a caller that finds it past
    /// [`Phase::Walking`] ever takes `self.store` at all. A caller in
    /// [`Phase::Unindexed`] or [`Phase::Walking`] skips the query entirely
    /// and gets capabilities-only instructions (the same shape the `Err`
    /// fallback below produces, for the same "if the index isn't open yet,
    /// fall back to capabilities only" reason), through
    /// [`instructions::cold_start`], so the one fact that is true only for
    /// this moment (the project's own root, and whether its walk has started
    /// or is still owed) is stated rather than left for a caller to infer
    /// from an unusually generic paragraph. `Phase::Failed` is not included
    /// here: nothing holds the store once a walk has failed and returned, so
    /// taking it cannot block, and this method's ordinary query-then-render
    /// path already handles a project with nothing indexed yet (`present`
    /// comes back empty, and [`build`](instructions::build) renders the same
    /// unqualified paragraph a fresh project always has).
    ///
    /// Two independent data sources feed the builder once the index is open,
    /// and only one of them can fail in a way this method has to handle
    /// itself:
    /// - `self.registry.receiver_call_capabilities()` reads
    ///   `DiscoveredPlugins`, an in-memory value read once at daemon startup
    ///   (see `PluginRegistry`'s own doc comment) - infallible.
    /// - `storage::schema::present_languages_with_semantic_state` is a real
    ///   query against `self.store`, which - unlike every tool handler above -
    ///   this method cannot refuse to answer around: there is no error
    ///   response to return here, only better or worse instructions text.
    ///   `Err` here (a corrupt schema, a locked or otherwise unreadable DB -
    ///   not the ordinary "cold start, zero File nodes yet" case, which is
    ///   `Ok(vec![])` and handled by [`instructions::build`] itself) falls
    ///   back to every *discovered* manifest's capabilities with
    ///   `semantic_pass_done: false` for all of them.
    ///   Forcing `semantic_pass_done` to `false` is what makes that fallback
    ///   honest under this uncertainty: without a real `language_state` read
    ///   there is no fact to claim a semantic pass has completed, so only
    ///   `receiver_calls_structural` (which needs no such fact) can close a
    ///   language's gap here - see `instructions::has_open_receiver_gap`'s
    ///   own doc comment for why that field alone is sufficient for a
    ///   language like Go.
    fn instructions(&self) -> String {
        let capabilities = self.registry.receiver_call_capabilities();

        let phase = self.indexing.phase();
        if let Phase::Unindexed | Phase::Walking = phase {
            let present = capabilities.keys().map(|language| (language.clone(), false)).collect();
            let present = instructions::present_languages(present, &capabilities);
            return instructions::cold_start(self.registry.project_root(), phase == Phase::Walking, &present);
        }

        let present = {
            let conn = self.store.read();
            crate::storage::schema::present_languages_with_semantic_state(&conn)
        };
        let present = match present {
            Ok(present) => present,
            Err(err) => {
                eprintln!(
                    "g-mesh daemon: failed to read present languages for the MCP instructions, \
                     falling back to manifest capabilities only: {err:#}"
                );
                capabilities.keys().map(|language| (language.clone(), false)).collect()
            }
        };
        instructions::build(&instructions::present_languages(present, &capabilities))
    }

    #[tool(
        name = "find_definition",
        description = "Find where a symbol is defined, and get the declaration's source back with it. Give either a symbol name, or a file path with a cursor position to resolve the symbol under it. The response carries the declaration's own text, so a follow-up read of that file is usually unnecessary; pass include_source: false if you only want coordinates. Line and column numbers in the response are zero-based; source.firstLine beside them is one-based, as an editor shows it."
    )]
    async fn find_definition(
        &self,
        params: Parameters<FindDefinitionParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call_started = Instant::now();
        if let Some(early) = self.prepare(&ctx, "find_definition", Need::Structural).await? {
            return Ok(early);
        }
        if let Some(file_path) = &params.0.file_path {
            self.ensure_file_fresh(&ctx, "find_definition", call_started, file_path).await;
        }
        find_definition::handle(&self.store, self.registry.project_root(), &self.embedding, params.0)
    }

    #[tool(
        name = "find_references",
        description = "List every place a declared symbol (function, type, etc.) is referenced, across the whole project. Not for files/modules - for \"what imports this file\" use get_dependencies instead."
    )]
    async fn find_references(
        &self,
        params: Parameters<SymbolQueryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(early) = self.prepare(&ctx, "find_references", Need::Structural).await? {
            return Ok(early);
        }
        let capabilities = self.capabilities();
        find_references::handle(&self.store, &self.embedding, &capabilities, params.0)
    }

    #[tool(name = "find_callers", description = "List the functions that call the given function.")]
    async fn find_callers(
        &self,
        params: Parameters<SymbolQueryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(early) = self.prepare(&ctx, "find_callers", Need::Structural).await? {
            return Ok(early);
        }
        let capabilities = self.capabilities();
        find_callers_callees::handle_callers(&self.store, &self.embedding, &capabilities, params.0)
    }

    #[tool(name = "find_callees", description = "List the functions the given function calls.")]
    async fn find_callees(
        &self,
        params: Parameters<SymbolQueryParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(early) = self.prepare(&ctx, "find_callees", Need::Structural).await? {
            return Ok(early);
        }
        let capabilities = self.capabilities();
        find_callers_callees::handle_callees(&self.store, &self.embedding, &capabilities, params.0)
    }

    #[tool(
        name = "find_implementations",
        description = "List the types that implement or extend the given interface, base class or abstract type. Direct implementors/extenders only by default; pass `transitive: true` to also include indirect ones (X extends Y extends the anchor), walked up to a bounded depth."
    )]
    async fn find_implementations(
        &self,
        params: Parameters<FindImplementationsParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(early) = self.prepare(&ctx, "find_implementations", Need::Structural).await? {
            return Ok(early);
        }
        let capabilities = self.capabilities();
        find_implementations::dispatch(&self.store, &self.embedding, &capabilities, params.0)
    }

    #[tool(
        name = "get_file_outline",
        description = "List the top-level symbols a file declares, in source order. Line and column numbers are zero-based - add one to cite a line to a human or to compare against a grep. `exported` means reachable from outside the file, not that the symbol's own line carries a visibility keyword - e.g. in Rust, a trait method or a trait-impl method is exported through the trait/impl even where the language forbids writing `pub` on that line itself."
    )]
    async fn get_file_outline(
        &self,
        params: Parameters<GetFileOutlineParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call_started = Instant::now();
        if let Some(early) = self.prepare(&ctx, "get_file_outline", Need::Structural).await? {
            return Ok(early);
        }
        self.ensure_file_fresh(&ctx, "get_file_outline", call_started, &params.0.file_path).await;
        get_file_outline::handle(&self.store, params.0)
    }

    #[tool(
        name = "get_dependencies",
        description = "Walk the import graph out of (or into) a file or module, up to a bounded depth and fan-out."
    )]
    async fn get_dependencies(
        &self,
        params: Parameters<GetDependenciesParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let call_started = Instant::now();
        if let Some(early) = self.prepare(&ctx, "get_dependencies", Need::Structural).await? {
            return Ok(early);
        }
        if let Some(file_path) = &params.0.file_path {
            self.ensure_file_fresh(&ctx, "get_dependencies", call_started, file_path).await;
        }
        // The union of every discovered plugin's declared entry points (GM-273)
        // - see `PluginRegistry::entry_points` and
        // `graph::queries::entry_point_rank_expr` for how a miss-path
        // directory lookup uses it. Read fresh per call rather than cached on
        // `self`: it is a cheap map walk over data that never changes while
        // this daemon runs (`daemon::manifest::discover`'s own contract), so
        // there is nothing a cache would save beyond what the borrow checker
        // already makes free.
        let entry_points = self.registry.entry_points();
        get_dependencies::handle(&self.store, &entry_points, params.0)
    }

    #[tool(
        name = "search_code",
        description = "Semantic search over the project's indexed symbols: find functions/types by what they do, described in free text, rather than by name or grep. Results are ranked by similarity, most relevant first. Needs the project's embedding model to be available - if it errors saying semantic search is unavailable, fall back to the structural tools instead."
    )]
    async fn search_code(
        &self,
        params: Parameters<SearchCodeParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(early) = self.prepare(&ctx, "search_code", Need::Embeddings).await? {
            return Ok(early);
        }
        search_code::handle(&self.store, &self.embedding, params.0)
    }
}

// `router = self.tool_router` on purpose: the attribute's default is
// `Self::tool_router()`, which rebuilds the whole router - all seven schemas -
// on every single tools/list and tools/call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for GMeshMcpServer {
    fn get_info(&self) -> ServerInfo {
        // Claude Code truncates this field at 2KB (independent of, and not shared with,
        // each tool's own 2KB description budget), and with tool search's default
        // deferred loading this is the only trust signal a model sees before individual
        // tool schemas even load - so the core anti-grep rule goes first, and the
        // legitimate exceptions stay concrete rather than getting cut mid-sentence.
        // `instructions::build` keeps the result under `instructions::
        // INSTRUCTIONS_BYTE_CEILING` (~1900 bytes, a safety margin under the 2KB cut) for
        // every language mix it can be asked to render - see that module's own doc
        // comment (GM-262) for the receiver-call gap this text used to state as a fixed,
        // TypeScript-only fact and now assembles per project.
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("g-mesh", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions())
    }
}

/// A symbol is addressable either by name or by where the cursor sits, because
/// an agent reading code has one or the other, rarely both.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDefinitionParams {
    /// Name of the symbol to look up.
    pub symbol_name: Option<String>,
    /// Project-relative path of the file the cursor is in.
    pub file_path: Option<String>,
    /// Cursor position within `file_path`, used to resolve the symbol under it.
    pub position: Option<Position>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Whether to return the declaration's source alongside its coordinates.
    /// Defaults to true; pass false when you genuinely only want the position.
    pub include_source: Option<bool>,
}

/// find_references/find_callers/find_callees/find_implementations differ only
/// in which edges they walk, never in what the caller has to supply - so they
/// share one parameter shape instead of four identical ones.
///
/// `Default` is for the tests that construct this by hand: with two
/// alternative addressing fields plus two paging ones, spelling all four out
/// at every call site is noise that hides which one the test is about.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SymbolQueryParams {
    /// Anchor symbol id from `find_definition`. Give this or `symbol_name`,
    /// never both.
    pub symbol_id: Option<String>,
    /// Anchor by name instead. Qualified name resolved first, then bare; an
    /// ambiguous name returns ranked candidates to re-call with.
    pub symbol_name: Option<String>,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Maximum results (default 20, max 200) - raise it rather than paging.
    pub limit: Option<u32>,
    /// Restrict to rows in these files: project-relative, exactly as
    /// `filePath` appears in output (no globs). Omit for the whole project.
    pub file_paths: Option<Vec<String>>,
}

/// `find_implementations`'s own params, not folded into `SymbolQueryParams`:
/// the three fields below (`transitive`/`max_depth`/`resume_token`) name a
/// transitive-walk concept `find_references`/`find_callers`/`find_callees`
/// have no equivalent of, and adding them to the shared struct would put a
/// `resume_token` field in front of three tools that can never populate or
/// consume one.
///
/// The first five fields are a deliberate duplicate of `SymbolQueryParams`'s
/// own - `find_implementations::dispatch` builds a `SymbolQueryParams` from
/// them to reuse the existing single-hop `handle` unchanged, so their names,
/// types and semantics must stay identical to that struct's.
///
/// `Default` is for the tests that construct this by hand - see
/// `SymbolQueryParams`'s doc comment for why.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct FindImplementationsParams {
    /// Anchor symbol id from `find_definition`. Give this or `symbol_name`,
    /// never both.
    pub symbol_id: Option<String>,
    /// Anchor by name instead. Qualified name resolved first, then bare; an
    /// ambiguous name returns ranked candidates to re-call with.
    pub symbol_name: Option<String>,
    /// Opaque cursor from a previous page. Ignored when `transitive: true`,
    /// which continues via `resume_token`.
    pub cursor: Option<String>,
    /// Maximum results (default 20, max 200) - raise it rather than paging.
    /// Ignored when `transitive: true`.
    pub limit: Option<u32>,
    /// Restrict to rows in these files: project-relative, exactly as
    /// `filePath` appears in output (no globs). Omit for the whole project.
    /// Ignored when `transitive: true`.
    pub file_paths: Option<Vec<String>>,
    /// Walk the whole implementer/extender hierarchy (e.g. a class extending
    /// a class that implements the anchor), up to `max_depth` hops. Absent
    /// or `false`: direct implementors/extenders only.
    pub transitive: Option<bool>,
    /// How many `extends`/`implements` hops to follow (default 5). Ignored
    /// without `transitive: true`.
    pub max_depth: Option<u32>,
    /// Token from a previous, truncated transitive walk - continues it
    /// exactly. Give it alone, without `symbol_id`/`symbol_name`/
    /// `transitive`: the token already carries the walk it continues.
    pub resume_token: Option<String>,
}

/// `Default` is for tests that construct this by hand - see
/// `SymbolQueryParams`'s doc comment for why.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct GetFileOutlineParams {
    /// Project-relative path of the file to outline.
    pub file_path: String,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Maximum symbols (default 20, max 200) - raise it for a big file rather
    /// than paging via `cursor`: one call costs far less than several, each
    /// of which re-pays the whole conversation's cached prefix.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDependenciesParams {
    /// Project-relative path of the file to start from. Also accepts the
    /// exact key of a logical container (a Go import path, a Rust module
    /// path, ...) to anchor on the package as a whole.
    pub file_path: Option<String>,
    /// Opaque node id from a previous result - not a module name or a path.
    /// For either of those use `file_path`; this accepts one anyway rather
    /// than refusing on a label.
    pub module_id: Option<String>,
    /// `Outgoing` for what this file imports, `Incoming` for what imports it.
    pub direction: Direction,
    /// How many import hops to follow.
    pub max_depth: Option<u32>,
    /// How many dependencies to expand per node before truncating.
    pub max_fanout: Option<u32>,
    /// Opaque token from a previous, truncated traversal.
    pub resume_token: Option<String>,
}

/// `Default` is for tests that construct this by hand - see
/// `SymbolQueryParams`'s doc comment for why.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SearchCodeParams {
    /// What you're looking for, in free text (e.g. "parses a config file").
    /// Matched by semantic similarity against each symbol's doc comment and
    /// signature, not by keyword - different wording for the same idea still
    /// ranks well.
    pub query: String,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
    /// Maximum results (default 20, max 200) - raise it rather than paging
    /// via `cursor`.
    pub limit: Option<u32>,
}
