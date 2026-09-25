//! The MCP server on the daemon's socket: the structural tool surface, and
//! one `rmcp` session per accepted connection. Async stops here: handlers
//! take the same synchronous `IndexStore` read guard as the rest of the daemon.
//!
//! Every `///` on a parameter struct below becomes that tool's JSON Schema,
//! re-read from the model's cached prompt on every request: state only what
//! changes a caller's behaviour (defaults, caps, exclusions, the ambiguity
//! protocol); the why goes in a `//` comment, cross-tool guidance in
//! `get_info`'s instructions (`docs/adr/0003-mcp-instructions-rendering.md`).

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
// `pub(crate)` so `cli::plugin_check::expectations` calls the same handler
// functions as the tools below; every other submodule stays private.
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

/// Logs a step of request handling when [`TRACE_CALLS_ENV`] is set. Every
/// tool call goes through `prepare`, so its lines split a hang three ways:
/// none (never reached the daemon), entry without exit (hung in `prepare`),
/// both (hung later). M1 in `docs/architecture/lazy-indexing.md` reads its
/// results off these lines (`outcome`: `satisfied`, `failed`, `timed_out`):
///
/// ```text
/// g-mesh daemon: prepare: entered tool=get_file_outline request=3 progressToken=present
/// g-mesh daemon: prepare: wait over tool=get_file_outline request=3 outcome=satisfied waited_ms=4212 progress_sent=21
/// g-mesh daemon: prepare: cancelled tool=get_file_outline request=3 waited_ms=900 progress_sent=4
/// ```
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
/// unset, empty or not a number. Read per call, so a test can change it
/// without starting a new daemon.
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
/// disconnects; the shim opens one connection per client, so a session dies
/// with its shim. `indexing` is consulted per call, so a session opened
/// mid-walk needs no reconnect: its calls wait and get the real answer.
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

/// The structural query surface. Every handler answers out of `store` alone;
/// `registry` is there to be woken, not queried: a tool call replays the files
/// queued while a plugin slept before the index is read (see
/// `daemon::lifecycle`), and it cannot know ahead which languages it touches.
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
    /// order allowed. `Ok(Some)` is a finished tool result (a failed walk, or
    /// the D7 cap); `Err` means the request was cancelled while it waited.
    /// 0. [`IndexingStatus::request_activation`]: starts the walk, semantic
    ///    pass and embedding backfill once, however many calls ask.
    /// 1. [`wait_for_index`](Self::wait_for_index): later steps may assume
    ///    `need`'s phase; nothing is answered off a half-built graph.
    /// 2. [`mark_used`](Self::mark_used): must not be nested inside the
    ///    plugin lock the replay holds (`storage::index_store`'s lock order).
    /// 3. The replay last, so the rows read include every change made while a
    ///    plugin slept.
    async fn prepare(
        &self,
        ctx: &RequestContext<RoleServer>,
        tool: &str,
        need: Need,
    ) -> Result<Option<CallToolResult>, ErrorData> {
        // Taken before the indexing wait, so the replay's heartbeat continues
        // the wait's `progress` values and stays strictly increasing.
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

    /// Replays whatever changed while a plugin was asleep; takes no lock when
    /// nothing did. On the blocking pool: waking a plugin spawns a process,
    /// and the accept loop's runtime has only two workers. Failures are logged
    /// in `PluginRegistry::replay_pending`, never returned (the queue stays for
    /// the next call to retry). Heartbeats like
    /// [`ensure_file_fresh`](Self::ensure_file_fresh), with a
    /// [`PluginRegistry::pending_summary`] read once before the queue drains.
    /// Not cancellable.
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
    /// four edge-walking tools hand to `provenance::resolve`. Read fresh per
    /// call: it never changes while the daemon runs
    /// (`daemon::manifest::discover`), so a cache would save nothing.
    fn capabilities(&self) -> HashMap<String, Capabilities> {
        self.registry.receiver_call_capabilities()
    }

    /// Query-time staleness check (`watcher::staleness::ensure_fresh`) for the
    /// three tools anchored on one file (`find_definition`, `get_file_outline`,
    /// `get_dependencies`): catches changes the watcher never saw, which never
    /// enter the replay queue. The symbol-anchored tools skip it: their files
    /// are known only after the query runs.
    ///
    /// Blocking pool and best-effort: a failure is logged and the handler
    /// answers from the current index. A reindex can take over a minute on a
    /// cold rust-analyzer, so it heartbeats (only with a `progressToken`, first
    /// tick one interval in, failed sends ignored); `progress` counts from
    /// `call_started`, taken before [`prepare`](Self::prepare), so it keeps
    /// increasing across both heartbeats of one call. Not cancellable.
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

    /// Waits until the index reaches the phase `need` requires. A tool call is
    /// never answered "not ready" or from a partial index (D2, D4, D6, D7 in
    /// `docs/architecture/lazy-indexing.md`). Raced in one `select!`:
    /// - **Progress** (only with a `progressToken`): every
    ///   [`PROGRESS_INTERVAL_ENV`]; `progress` is seconds waited, the one
    ///   strictly increasing value; a failed send is logged and ignored.
    /// - **The cap** ([`INDEX_WAIT_CAP_ENV`]): a retryable tool error that
    ///   answers no part of the question, before the client's idle window.
    /// - **Cancellation** (`ctx.ct`): an error at once; indexing continues.
    ///
    /// Structural tools pass [`Need::Structural`]; only `search_code` passes
    /// [`Need::Embeddings`]. `get_info` never calls this. It runs before
    /// [`mark_used`](Self::mark_used), which takes the store the walk holds
    /// during batch commits. The fast path never suspends; a slow one
    /// suspends the task, not the worker thread.
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
            // A failed walk is a tool error carrying its message, never an
            // answer read off an empty or partial graph; the next call retries
            // (`IndexingStatus::request_activation`).
            WaitOutcome::Failed(message) => {
                Some(CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                    "g-mesh could not build this project's index: {message}. The next tool call retries \
                     the build."
                ))]))
            }
        })
    }

    /// Advances the project's durable `lastUsed` stamp (`gc::last_used`, read
    /// by a later GC scan) and this process's in-memory idle clock
    /// (`daemon::lifecycle::CoreActivity`); neither can stand in for the
    /// other. Called per tool call, not per connection: a client holds one
    /// session for its whole lifetime. Best-effort: a failure is logged, never
    /// a tool error. The store is taken and released here, before the
    /// handler's own read guard.
    fn mark_used(&self) {
        self.core_activity.request();
        if let Err(err) = self.store.with(last_used::touch) {
            eprintln!("g-mesh daemon: failed to record lastUsed: {err:#}");
        }
    }

    /// `get_info`'s `with_instructions` string, built per session by
    /// [`instructions::build`]. Never takes the store while a bulk-index batch
    /// may hold it (through embedding inference): a blocked `initialize` looks
    /// like a hung server. So the lock-free [`phase`](IndexingStatus::phase)
    /// comes first, and [`Phase::Unindexed`]/[`Phase::Walking`] render
    /// capabilities-only text via [`instructions::cold_start`]; `Phase::Failed`
    /// queries, since nothing holds the store after a failed walk. If the
    /// present-languages query fails, every discovered manifest is used with
    /// `semantic_pass_done: false`: nothing may claim a pass it did not read.
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
        // The union of every plugin's declared entry points (see
        // `PluginRegistry::entry_points` and
        // `graph::queries::entry_point_rank_expr`). Read fresh per call: it
        // never changes while the daemon runs.
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
        // Claude Code truncates this field at 2KB, independently of each
        // tool's description budget, and with deferred tool loading it is the
        // only trust signal a model sees before schemas load: the anti-grep
        // rule goes first and the exceptions stay concrete. `instructions`
        // keeps every rendering under `INSTRUCTIONS_BYTE_CEILING`.
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

// Shared by find_references/find_callers/find_callees: they differ only in
// which edges they walk, never in what the caller supplies.
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

// The first five fields must stay identical in name, type and semantics to
// `SymbolQueryParams`'s: `find_implementations::dispatch` builds a
// `SymbolQueryParams` from them to run the single-hop `handle`.
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
