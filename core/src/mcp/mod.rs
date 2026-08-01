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
//! so async stops at this module's front door; handlers below hold the same
//! plain `Mutex` guards the synchronous code always has.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::UnixStream;

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::lifecycle::{CoreActivity, PluginSupervisor};
use crate::gc::last_used;
use crate::graph::pagination::Direction;
use crate::protocol::types::Position;

mod anchor;
mod find_callers_callees;
mod find_definition;
mod find_implementations;
mod find_references;
mod get_dependencies;
mod get_file_outline;
mod tool_result;

/// What every tool answers while the daemon's cold-start bulk walk is still
/// running.
///
/// A tool-level error rather than an empty success page, for the reason task
/// 104 gave for `allUnresolved`: an empty `results` array with
/// `hasMore: false` is indistinguishable from a real, complete "nothing
/// found", and an agent that reads it as one stops looking and goes off to
/// grep. An `isError` result cannot be mistaken for an answer, and the
/// wording says which kind of error it is - "not yet" rather than "no such
/// symbol" or "bad arguments" - so a caller can act on it (wait and ask
/// again) instead of concluding the project has no such symbol.
///
/// Deliberately one constant shared by all seven tools: the caller's next
/// move does not depend on which tool it happened to ask, and a per-tool
/// phrasing would only make the signal harder to recognize.
///
/// Public so the acceptance tests can assert against the text an agent
/// actually receives rather than against a copy of it that could drift.
pub const STILL_INDEXING: &str =
    "g-mesh: this project's index is still being built - a first walk of the whole project, \
     either because it has never been indexed or because an upgrade invalidated what it had. \
     No structural answer exists yet, so this is NOT 'no results': do not conclude anything \
     about the code from it. Retry the same call in a few seconds; a large repository can take \
     a minute or two to finish its first walk.";

/// How long a tool call that lands while the cold-start walk is still running
/// waits for it to finish before committing to [`STILL_INDEXING`].
///
/// Task 105 made every such call fail immediately, which fixed the case it
/// was written for (a walk with real minutes left to run) but introduced a
/// smaller one of its own: on a project small enough for the walk to finish
/// in milliseconds, a call that happened to arrive a moment before
/// `IndexingStatus::mark_ready` now paid for a whole extra round trip - a
/// refusal, noticing it, a retry - purely because it asked on the wrong side
/// of an instant. This window absorbs exactly that race: a call that arrives
/// during it gets the walk's *actual* remaining time to finish before this
/// module gives up on it.
///
/// Tens to low hundreds of milliseconds on purpose, not seconds - long enough
/// that "the walk was basically done anyway" covers it, short enough that a
/// project whose walk genuinely has real time left still gets task 105's
/// fast, honest refusal rather than a second, quieter version of the problem
/// task 105 exists to prevent. This is a race-absorber for calls arriving
/// close to completion, not a general "wait a bit and hope" budget - a walk
/// with 30 seconds left to run fails in `INDEXING_GRACE_WINDOW`, not in 30
/// seconds.
///
/// Public for the same reason [`STILL_INDEXING`] is: the acceptance tests
/// assert real elapsed time against this number, and a copy hand-maintained
/// in `tests/` could drift from it silently.
pub const INDEXING_GRACE_WINDOW: Duration = Duration::from_millis(150);

/// Serves one accepted socket connection as an MCP session until the peer
/// disconnects. One session per connection, and the shim opens exactly one
/// connection per MCP client, so a client's session dies with its shim.
///
/// `indexing` is consulted per *call*, not per connection, which is what
/// makes a session opened during the cold-start walk recover on its own: the
/// same long-lived session that was told "still indexing" gets real answers
/// from the first call after the walk commits, with nothing to reconnect and
/// nothing to re-initialize.
pub async fn serve_connection(
    stream: UnixStream,
    conn: Arc<Mutex<Connection>>,
    plugin: Arc<PluginSupervisor>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
) -> Result<()> {
    let service = GMeshMcpServer::new(conn, plugin, core_activity, indexing)
        .serve(stream)
        .await
        .context("MCP initialization failed")?;
    service.waiting().await.context("MCP session task failed")?;
    Ok(())
}

/// The structural query surface, backed by the project's index and the
/// language plugin.
///
/// Every handler answers out of `conn` alone. `plugin` is not there to be
/// queried - no tool asks the language server a question - but to be *woken*:
/// while it sleeps on its idle timeout the core queues the files that changed,
/// and a tool call is the moment that queue has to be replayed before the
/// index is read (see `daemon::lifecycle`).
#[derive(Clone)]
pub struct GMeshMcpServer {
    conn: Arc<Mutex<Connection>>,
    plugin: Arc<PluginSupervisor>,
    core_activity: Arc<CoreActivity>,
    indexing: IndexingStatus,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl GMeshMcpServer {
    pub fn new(
        conn: Arc<Mutex<Connection>>,
        plugin: Arc<PluginSupervisor>,
        core_activity: Arc<CoreActivity>,
        indexing: IndexingStatus,
    ) -> Self {
        Self { conn, plugin, core_activity, indexing, tool_router: Self::tool_router() }
    }

    /// Everything every handler owes before it reads the index, in the one
    /// order they may happen in. `Some(...)` is a finished response the
    /// handler must return unchanged; `None` means carry on and answer.
    ///
    /// The order is not arbitrary:
    ///
    /// 1. [`still_indexing`](Self::still_indexing) first, because a project
    ///    whose first walk has not finished has no graph to bring up to date
    ///    and no answer to give either way.
    /// 2. [`mark_used`](Self::mark_used) next: it takes and releases the
    ///    SQLite mutex on its own, and it must not be nested inside the plugin
    ///    lock the replay below holds (see `daemon::lifecycle`'s lock order).
    /// 3. The replay last, so the rows this call is about to read already
    ///    include every change made while the plugin was asleep.
    async fn prepare(&self) -> Option<Result<CallToolResult, ErrorData>> {
        if let Some(not_ready) = self.still_indexing().await {
            return Some(not_ready);
        }
        self.mark_used();
        self.replay_queued_changes().await;
        None
    }

    /// Brings the index up to date with whatever changed while the plugin was
    /// asleep, and does nothing at all - not even a lock - when nothing did,
    /// which is every call on a daemon whose plugin is awake.
    ///
    /// On the blocking thread pool rather than inline: waking the plugin means
    /// spawning Node and warming tree-sitter, and the accept loop's runtime
    /// has two worker threads (see `daemon::serve_forever`), so doing it on
    /// one of them would stall every other session for the length of a
    /// process spawn.
    ///
    /// A failure is logged, not returned. The caller asked a structural
    /// question the index can already answer; refusing it because a *later*
    /// edit could not be replayed would turn one unreadable file into a dead
    /// tool surface, and the queue is left intact for the next call to retry.
    async fn replay_queued_changes(&self) {
        if !self.plugin.has_pending() {
            return;
        }
        let plugin = Arc::clone(&self.plugin);
        let conn = Arc::clone(&self.conn);
        match tokio::task::spawn_blocking(move || plugin.replay_pending(&conn)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => eprintln!(
                "g-mesh daemon: could not replay the changes queued while the plugin slept: {err:#}"
            ),
            Err(err) => eprintln!("g-mesh daemon: the plugin wake task failed: {err}"),
        }
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
    async fn ensure_file_fresh(&self, file_path: &str) {
        let plugin = Arc::clone(&self.plugin);
        let conn = Arc::clone(&self.conn);
        let owned_path = file_path.to_string();
        let task_path = owned_path.clone();
        match tokio::task::spawn_blocking(move || plugin.ensure_fresh(&conn, &task_path)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => eprintln!(
                "g-mesh daemon: query-time staleness check failed for {owned_path}: {err:#}"
            ),
            Err(err) => eprintln!("g-mesh daemon: the staleness-check task failed: {err}"),
        }
    }

    /// `Some(...)` - a finished response the handler must return unchanged -
    /// once the cold-start walk has been given up on; `None` once the index
    /// is complete, whether it already was when this call arrived or it
    /// finished during the grace wait below. Shaped like
    /// `find_definition::resolve_symbol_name`'s "finished response or carry
    /// on" rather than a bare bool, so the wording lives in exactly one place
    /// and no handler can invent its own.
    ///
    /// [`prepare`](Self::prepare) calls this as its *first* step, ahead of
    /// [`mark_used`](Self::mark_used): that one takes the daemon's single
    /// SQLite mutex, which the walk holds for the length of each batch
    /// commit, and waiting on it to say "I am not going to read the database"
    /// would defeat the point of an atomic flag.
    ///
    /// The fast path - the walk was already done - never touches the wait: a
    /// project that owes no walk, or one whose walk finished before this call
    /// arrived, reads `is_indexing() == false` and returns on the spot, same
    /// as before task 107. Only a call that actually lands mid-walk pays for
    /// [`INDEXING_GRACE_WINDOW`], and it pays for it by `await`ing
    /// `IndexingStatus::wait_ready` - suspending this task, not blocking the
    /// worker thread it runs on (see `daemon::serve_forever`'s two-worker
    /// runtime, and `IndexingStatus`'s own doc comment for why a `Notify`
    /// rather than a blocking primitive).
    async fn still_indexing(&self) -> Option<Result<CallToolResult, ErrorData>> {
        if !self.indexing.is_indexing() {
            return None;
        }
        if self.indexing.wait_ready(INDEXING_GRACE_WINDOW).await {
            return None;
        }
        Some(tool_result::error(STILL_INDEXING))
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
    /// turning an answerable query into a tool error. The guard is taken and
    /// released here, before the handler takes its own.
    fn mark_used(&self) {
        self.core_activity.request();
        if let Err(err) = last_used::touch(&self.conn.lock().unwrap()) {
            eprintln!("g-mesh daemon: failed to record lastUsed: {err:#}");
        }
    }

    #[tool(
        name = "find_definition",
        description = "Find where a symbol is defined. Give either a symbol name, or a file path with a cursor position to resolve the symbol under it."
    )]
    async fn find_definition(
        &self,
        params: Parameters<FindDefinitionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        if let Some(file_path) = &params.0.file_path {
            self.ensure_file_fresh(file_path).await;
        }
        find_definition::handle(&self.conn, params.0)
    }

    #[tool(
        name = "find_references",
        description = "List every place a declared symbol (function, type, etc.) is referenced, across the whole project. Not for files/modules - for \"what imports this file\" use get_dependencies instead."
    )]
    async fn find_references(
        &self,
        params: Parameters<SymbolQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        find_references::handle(&self.conn, params.0)
    }

    #[tool(name = "find_callers", description = "List the functions that call the given function.")]
    async fn find_callers(&self, params: Parameters<SymbolQueryParams>) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        find_callers_callees::handle_callers(&self.conn, params.0)
    }

    #[tool(name = "find_callees", description = "List the functions the given function calls.")]
    async fn find_callees(&self, params: Parameters<SymbolQueryParams>) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        find_callers_callees::handle_callees(&self.conn, params.0)
    }

    #[tool(
        name = "find_implementations",
        description = "List the types that implement or extend the given interface, base class or abstract type."
    )]
    async fn find_implementations(
        &self,
        params: Parameters<SymbolQueryParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        find_implementations::handle(&self.conn, params.0)
    }

    #[tool(
        name = "get_file_outline",
        description = "List the top-level symbols a file declares, in source order."
    )]
    async fn get_file_outline(
        &self,
        params: Parameters<GetFileOutlineParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        self.ensure_file_fresh(&params.0.file_path).await;
        get_file_outline::handle(&self.conn, params.0)
    }

    #[tool(
        name = "get_dependencies",
        description = "Walk the import graph out of (or into) a file or module, up to a bounded depth and fan-out."
    )]
    async fn get_dependencies(
        &self,
        params: Parameters<GetDependenciesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(not_ready) = self.prepare().await {
            return not_ready;
        }
        if let Some(file_path) = &params.0.file_path {
            self.ensure_file_fresh(file_path).await;
        }
        get_dependencies::handle(&self.conn, params.0)
    }
}

// `router = self.tool_router` on purpose: the attribute's default is
// `Self::tool_router()`, which rebuilds the whole router - all seven schemas -
// on every single tools/list and tools/call.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for GMeshMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("g-mesh", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Structural code-graph queries over this project's index. Prefer these over \
                 grepping when you need definitions, references, call edges or imports.\n\n\
                 Efficient usage: pass `symbol_name` directly to find_references/find_callers/\
                 find_callees/find_implementations instead of calling find_definition first, and \
                 raise `limit` for symbols with many results instead of paging - see each tool's \
                 parameter docs for the exact mechanics (ambiguity handling, defaults).\n\n\
                 A result anchored by `symbol_id`, or by an unambiguous `symbol_name` (resolved to \
                 the same symbol_id internally - identical guarantee either way, so passing \
                 symbol_name directly per the efficiency note above does not lose it), is already \
                 resolved per call site to that exact declaration - other same-named declarations' \
                 call sites are excluded, so re-checking one with grep is wasted work. \
                 `resolved: false` marks the one thing the indexer could not settle on its own: \
                 an edge whose target is in *another* file, where whether that file is indexed \
                 and really exports the name is not knowable from the usage alone. Everything \
                 else - every edge whose target is declared in the same file as the usage - is \
                 `resolved: true`, matched against the declarations actually in scope there, so \
                 a same-file call site is never the reason to reach for grep. That makes \
                 `resolved: false` a narrow, informative signal rather than a general disclaimer: \
                 it is worth a look on that row, and only that row. \
                 `find_references`/`find_callers`/`find_callees`/\
                 `find_implementations` also carry a response-level `allUnresolved: true` when \
                 *every* row in a non-empty `results` page is `resolved: false` - a legitimate \
                 shape (genuinely unconfirmed name-matched edges), but one that otherwise reads \
                 identically to an ordinary complete answer (`hasMore: false`, a plausible-looking \
                 `results` array) unless every row is individually checked. Treat that whole page \
                 as unconfirmed, not just its rows; it is never set on an empty page, since an \
                 empty result has nothing to be suspicious of. One honest gap: a method call \
                 reached through a variable receiver (`x.foo()`) produces no edge by design, so \
                 caller/reference lists for methods can under-report. This is the ONLY \
                 completeness gap: for bare function calls and `this`/`super`/qualified-type \
                 calls, a `hasMore: false` page is exhaustive - every call site is already \
                 accounted for, and grepping afterward to check for more just re-derives what \
                 the response already guarantees. Only fall back to grep when you specifically \
                 suspect a variable-receiver method call was missed, not as a routine \
                 double-check.\n\n\
                 One more response shape worth recognizing: on a project being indexed for the \
                 first time (or re-indexed after an upgrade), every tool returns an error whose \
                 text says the index is still being built. That is a temporary \"not yet\", not \
                 \"not found\" - retry the same call after a few seconds rather than falling \
                 back to grep or concluding the symbol does not exist. Any *other* error, and \
                 any successful-but-empty result, means what it says.",
            )
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
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
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
    /// Id of the anchor symbol, as returned by `find_definition`. Give this
    /// or `symbol_name`, never both.
    pub symbol_id: Option<String>,
    /// Alternative to `symbol_id` that skips the `find_definition` call -
    /// resolved the same way (qualified name first, then bare name). If
    /// ambiguous, returns a ranked candidate list (`ambiguous: true`);
    /// re-call with a candidate's `id` as `symbol_id`.
    pub symbol_name: Option<String>,
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
    /// Maximum results (default 20, capped at 200) - raise for a wide result
    /// set instead of paging via `cursor`.
    pub limit: Option<u32>,
    /// Restrict results to rows whose referencing/calling/implementing node
    /// lives in one of these files (project-relative, matching `file_path`
    /// exactly as it appears elsewhere in this tool's own output - no
    /// prefix or glob matching). Use this to answer "of these known files,
    /// which ones reference/call this symbol?" in one call instead of one
    /// unscoped call plus a grep per file. Omit for the default, unscoped
    /// search across the whole project; an empty array behaves identically
    /// to omitting it, not "match nothing".
    pub file_paths: Option<Vec<String>>,
}

/// `Default` is for tests that construct this by hand - see
/// `SymbolQueryParams`'s doc comment for why.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct GetFileOutlineParams {
    /// Project-relative path of the file to outline.
    pub file_path: String,
    /// Opaque cursor from a previous page of results.
    pub cursor: Option<String>,
    /// Maximum symbols to return (default 20, capped at 200) - raise this for
    /// a file with many top-level symbols instead of paging via `cursor`. A
    /// file that fits in one call with `limit` set high enough is one round
    /// trip instead of several, each of which re-pays the whole
    /// conversation's cached prefix for a handful more rows.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDependenciesParams {
    /// Project-relative path of the file to start from.
    pub file_path: Option<String>,
    /// Id of the module to start from, as an alternative to `file_path`.
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
