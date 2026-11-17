# GM-709: `mcp` behind a host trait, one tool-call context, one heartbeat, eval behind a feature (R-11, R-17, R-19, R-27)

Source findings: `docs/review/4.30/README.md` R-11, R-17, R-19, R-27 (and K-2, K-11).
Read against `main` (release-4.32.0 differs only by the version bump). Line numbers are
`main` at the time of writing; each code slice re-reads by symbol, not by line.

## 0. Where we start

- `mcp -> daemon` has **10** `use` sites in non-test code (`module-cycles.py --edges`:
  `mcp -> daemon 10/0`): `mcp/mod.rs:26-27` (`CoreActivity`, `PathCoverage`,
  `PluginRegistry`), `mcp/front.rs:32` (`CoreActivity`), and `PathCoverage` in seven handler
  files (`find_definition.rs:14`, `find_callers_callees.rs:18`, `find_implementations.rs:29`,
  `get_dependencies.rs:17`, `find_references.rs:15`, `get_file_outline.rs:20`,
  `not_indexed.rs:29`). The review counted registry methods and `CoreActivity`, but not
  `PathCoverage`; it has to move too or the acceptance check fails.
- `daemon -> mcp` (the right direction, the daemon hosts the server): `daemon/mod.rs:755`
  (`mcp::serve_connection`), `daemon/front.rs:88,112,131` (`mcp::front`), and the wrong-way
  one, `daemon/registry.rs:847-849` `PluginRegistry::query_shapes` building
  `mcp::query_shapes::QueryShapes` (the review's `:960-961` has drifted).
- `IndexingStatus` is **not** in `daemon` any more: GM-705 (R-6) moved it to the top-level
  `indexing_status` module, whose only dependency is `paths`. `mcp -> indexing_status` is a
  downward edge and stays.
- Today's SCCs: `{daemon, mcp, watcher}` and `{embedding, graph, storage}`.
- Three heartbeat copies in `mcp/mod.rs`: `replay_queued_changes` (fn at 298; ticker
  314-321, loop 323-345), `ensure_file_fresh` (394; 409-416, 418-440), `wait_until` (513;
  521-533, 535-567).
- Embedding eval: `core/src/cli/embed_eval.rs` (2848 lines) + `cli/embed_eval/*.rs` (13
  files), 8270 lines; wired at `cli/mod.rs:37` (`pub mod embed_eval;`), `:239-242`
  (`Command::DebugEmbedEval`, `#[command(hide = true)]`), `:384` (dispatch). Core already has
  one eval-only feature, `ep-coreml` (`core/Cargo.toml` `[features]`).

## 1. Decisions

### D1. One `mcp`-owned trait `McpHost`, object-safe and synchronous; `Arc<dyn McpHost>`

- **Today:** `GMeshMcpServer` holds `Arc<PluginRegistry>` and `Arc<CoreActivity>`, two
  concrete `daemon` types; a unit test of the server must build a real `PluginRegistry`
  (`session_hints.rs:213-227` builds one with `DiscoveredPlugins::default()` just to get a
  server).
- **Change:** `mcp/host.rs` declares `trait Activity` (one method, `request`) and
  `trait McpHost: Activity` (ten methods, section 2). The server holds
  `host: Arc<dyn McpHost>`. `daemon/mcp_host.rs` adds `DaemonHost { registry, activity }`
  implementing both by delegation, and `impl Activity for CoreActivity` (for `mcp::front`).
  Every method is synchronous.
- **Example:** `GMeshMcpServer::new(store, host, indexing, embedding)`; a test passes
  `Arc::new(FakeHost::new(root))`; the daemon passes
  `Arc::new(DaemonHost::new(Arc::clone(&registry), Arc::clone(&core_activity)))`.
- **Consequence:** `mcp` names no `daemon` type; the two blocking methods (`replay_pending`,
  `ensure_fresh`) are still called from `spawn_blocking`, now with an
  `Arc<dyn McpHost>` clone (the trait is `Send + Sync + 'static`).

**The async shape (the review's open question).** The only async member of the old surface
is `IndexingStatus::wait_for`, and `IndexingStatus` is not a `daemon` type (D2). With it
out, the host has no async method and the question disappears. The options, for the
record and for D2's alternative:

| Shape | dyn-compatible | Cost | Verdict |
|---|---|---|---|
| A. Sync trait, `Arc<dyn McpHost>` | yes | one vtable call per host call (~10 per tool call) | **taken** |
| B. Boxed future, `fn wait_for(..) -> Pin<Box<dyn Future<Output = WaitOutcome> + Send + '_>>` | yes | one allocation per wait | fallback if the owner puts indexing behind the trait |
| C. `async fn` / RPITIT (`-> impl Future + Send`) in the trait | **no** | server becomes `GMeshMcpServer<H: McpHost>` | rejected: generics infect `serve_connection`, every test and the `#[tool_router]`/`#[tool_handler]` macro impls (not verified that rmcp's macros accept a generic impl); a bare `async fn` also gives no `Send` bound, so `tokio::spawn` of the session would fail to compile |
| D. `async-trait` crate | yes | new dependency, boxes like B | rejected: B without a dependency |

Risks: `dyn` dispatch cost is irrelevant next to a SQLite read. The trait has a pub
visibility problem: `GMeshMcpServer`/`serve_connection` are `pub`, so `McpHost`,
`Activity`, `PathCoverage` and `Freshness` must be `pub` too or `private_interfaces` fails
`clippy -D warnings` (S3 confirms; alternative is narrowing `serve_connection` and
`GMeshMcpServer::new` to `pub(crate)`, which nothing outside the crate calls: g-mesh
`find_callers GMeshMcpServer::new` lists only `mcp/` files).

### D2. `IndexingStatus` stays a concrete value next to the host, not trait methods

- **Today:** the server holds `indexing: IndexingStatus` (a cheap `Arc` clone) and calls
  nine of its methods (section 2.2), one of them async (`wait_for`).
- **Change:** none. It is passed as today. The trait docs list it as the host's other half.
- **Example:** a test drives the phase with `IndexingStatus::unindexed()` /
  `structural()` / `set_phase(..)`, which `session_hints.rs:226` and the wait tests
  already do; a fake gains nothing a real `IndexingStatus` lacks.
- **Consequence:** no async in the trait; acceptance (no `daemon::` import in `mcp/`) holds
  because `indexing_status` is not `daemon`.
- **Alternative (owner's call, see open questions):** put the nine methods behind
  `McpHost` with shape B for `wait_for`. Benefit: literally one injected object. Risk: an
  allocation per wait, a fake that must re-implement the phase/notify state machine
  (`indexing_status.rs:390-415`), and the biased `select!` then races a boxed future whose
  wake semantics the fake must reproduce for the cancellation tests.

### D3. `PathCoverage` moves into `mcp::host`; `ensure_fresh` returns an `mcp` type

- **Today:** `PathCoverage` is declared in `daemon/registry.rs:466` and used only by `mcp/`
  (8 files) and the registry (g-mesh `find_references PathCoverage`: 24 refs, all in `mcp/`
  plus one in `registry.rs`). `PluginRegistry::ensure_fresh` returns
  `Result<Option<watcher::staleness::StalenessOutcome>>`.
- **Change:** `PathCoverage` moves verbatim to `mcp/host.rs`, re-exported as
  `mcp::PathCoverage`; `registry.rs` imports it from `mcp` (downward). The trait's
  `ensure_fresh` returns `anyhow::Result<Option<String>>`: the outcome's `Debug` label, the
  only thing `mcp` does with it (`mod.rs:442-447`, trace line).
- **Example:** `DaemonHost::ensure_fresh` is
  `self.registry.ensure_fresh(store, path).map(|o| o.map(|o| format!("{o:?}")))`.
- **Consequence:** the trace line `ensure_fresh: ... outcome=AlreadyFresh` stays
  byte-identical, which `core/tests/it/first_query_after_walk.rs:132,264,297,364` assert.
  Returning `StalenessOutcome` instead would add `mcp -> watcher`, and with
  `watcher -> daemon -> mcp` re-close the cycle (projection in section 5).

### D4. `GMeshMcpServer::new` builds `QueryShapes` itself (K-2)

- **Today:** `mod.rs:230` `Arc::new(registry.query_shapes())`; `registry.rs:847-849` calls
  `QueryShapes::from_manifests(self.discovered.manifests.values())`.
- **Change:** trait method `manifests(&self) -> Vec<&PluginManifest>`;
  `new` does `QueryShapes::from_manifests(host.manifests())`; delete
  `PluginRegistry::query_shapes` (only caller `mcp/mod.rs`, g-mesh `find_callers`).
- **Consequence:** `registry.rs` no longer names `mcp::query_shapes`; `query_shapes.rs`
  keeps importing only `plugin_manifest`.

### D5. `mark_used` stays in `mcp`; only its `CoreActivity::request` half goes through the trait

`mark_used` (`mod.rs:637-642`) is `core_activity.request()` + `store.with(last_used::touch)`.
`gc::last_used` is not `daemon`; the store write stays in `mcp`. The lock-order rule is
restated in the trait docs (section 2.3).

### D6. R-11: one `with_heartbeat` in `mcp/progress.rs`, behaviour byte-identical

- **Today:** three copies of: read `PROGRESS_INTERVAL_ENV`, token filter, `interval_at`
  first tick one interval in, `MissedTickBehavior::Delay`, biased `select!` (task first),
  `notify_progress`, count `sent`, log a failed send.
- **Change:** one helper (signature in 3.1). Each caller keeps its own message, its own
  `progress` origin and its own trace line.
- **Consequence:** one `interval_at`/`MissedTickBehavior::Delay` site. Invariants kept:
  (1) `biased;` with branch order task, cancellation, tick; (2) `progress` origin is passed
  explicitly: `wait_until` uses its own `started`, replay and staleness use the
  `call_started` taken in `prepare`/the tool method, so values within one call keep rising
  exactly as today (do not "simplify" all three to one origin: the wait's values would
  change).

### D7. R-19 first half (one context struct) is in this task; the second half (typed `Answer<Page>`, a `query` module) is a follow-up

- **Today:** per tool a ladder `handle` -> `handle_in` -> `handle_in_covered` (17 entry
  points, section 3.3); each adds one argument (`semantic`, `uncovered`/`coverage`), and the
  test-only wrappers invent the missing ones, e.g. `handle_callees` (`find_callers_callees.rs:679-689`,
  `#[cfg(test)]`) creates `SessionHints::default()`, so a test through it never sees the
  session's once-per-session hints, unlike `handle_callers` which takes `hints`.
- **Change:** `mcp/context.rs` `QueryCtx` (fields in 3.3). One entry point per tool:
  `handle(ctx: &QueryCtx, semantic: &SemanticRung<'_>, params)` for the five symbol tools,
  `handle(ctx: &QueryCtx, params)` for outline/dependencies/definition-by-position. The
  `resolve_lazily*` wrappers move onto `QueryCtx`. `plugin_check`'s `EvalContext`
  (`expectations.rs:937`) builds a `QueryCtx` instead of passing five arguments.
- **Consequence:** the `handle_callees` bug disappears structurally (hints come from the
  context, never from a wrapper). Answers stay `CallToolResult`.
- **Why the second half waits:** typed answers change every handler's return type and the
  1.8k-line `plugin_check/expectations.rs` (`tool_outcome`, 1827-1838), and the place a typed
  page becomes bytes is exactly the byte budget GM-708 (R-18) is moving into `mcp` right now.
  Doing it in GM-709 means designing against a budget API that does not exist on `main` yet.
  Proposed follow-up task: "typed tool answers and an `mcp::query` module" after GM-708 and
  GM-709. R-27 does not need it (D8).

### D8. R-27: cargo feature `embed-eval`, off by default

- **Today:** the eval is compiled into every shipped binary as a hidden subcommand.
- **Change:** `[features] embed-eval = []`; `ep-coreml` gains `"embed-eval"` (it only
  exists for `debug-embed-eval run --ep coreml`, `Cargo.toml` comment). `cfg` on the module
  declaration, the `Command` variant and its match arm, and the eval-only `pub(crate)` hooks.
  Tests and clippy build with the feature (section 4) so the 8.3k lines stay checked.
- **Example:** `cargo build --release -p g-mesh --features embed-eval` then
  `g-mesh debug-embed-eval report ...`; a default build answers
  `error: unrecognized subcommand 'debug-embed-eval'`.
- **Consequence:** shipped binary smaller and without eval code; `top_k_for_eval`,
  `shipped_similarity_floor` stay `pub(crate)` (K-11). Separate crate rejected for now (K-11):
  it would need those hooks `pub`.
- **Risk:** code used only by the eval becomes dead in a default build and fails
  `clippy -D warnings`; the exact list is a compile result (S5 must-confirm).

## 2. The trait

### 2.1 `mcp/host.rs`

```rust
/// Records that the core did something a user asked for (the idle clock).
pub trait Activity: Send + Sync {
    fn request(&self);
}

/// Everything the MCP server needs from the process hosting it, besides the
/// store, the embedding pipeline and `IndexingStatus`, which it holds directly.
///
/// # Lock order
///
/// `replay_pending` and `ensure_fresh` take plugin-side locks (registry
/// supervisors, supervisor state, plugin process state) and the store only
/// inside them (`storage::index_store`'s module doc). So a caller:
/// - holds no store guard when it calls either of them;
/// - records use (`Activity::request` and the `lastUsed` store write) before
///   the replay, never from inside it;
/// - calls them off the async workers (`spawn_blocking`): they may spawn a
///   plugin process.
/// Every other method takes no store lock and does no I/O except
/// `path_coverage` (one `canonicalize`).
pub trait McpHost: Activity + 'static {
    fn project_root(&self) -> &Path;
    fn manifests(&self) -> Vec<&PluginManifest>;
    fn receiver_call_capabilities(&self) -> HashMap<String, Capabilities>;
    fn entry_points(&self) -> Vec<String>;
    fn missing_languages(&self) -> Vec<&'static str>;
    fn path_coverage(&self, file_path: &str) -> Option<PathCoverage>;
    fn has_pending(&self) -> bool;
    fn pending_summary(&self) -> String;
    fn replay_pending(&self, store: &IndexStore) -> usize;
    /// `Ok(None)`: no plugin claims the file. `Ok(Some(label))`: the check's
    /// outcome, as the trace line prints it.
    fn ensure_fresh(&self, store: &IndexStore, file_path: &str) -> anyhow::Result<Option<String>>;
}

pub enum PathCoverage { /* moved verbatim from daemon/registry.rs:464-474 */ }
```

### 2.2 Mapping

| Trait member | Today | Called from (mcp, non-test) |
|---|---|---|
| `Activity::request` | `CoreActivity::request` (`lifecycle.rs:749`) | `mod.rs:638` `mark_used`; `front.rs:297` `call_tool` |
| `project_root` | `PluginRegistry::project_root` (`registry.rs:548`) | `mod.rs:331,426,501,558,623,667,742,948` |
| `manifests` | new; replaces `PluginRegistry::query_shapes` (`registry.rs:847`) | `mod.rs:230` (`new`) |
| `receiver_call_capabilities` | `registry.rs:834` | `mod.rs:367,656` |
| `entry_points` | `registry.rs:804` | `mod.rs:920` |
| `missing_languages` | `registry.rs:841` | `mod.rs:657` |
| `path_coverage` | `registry.rs:645` (`pub(crate)`) | `mod.rs:378,744,896,921` |
| `has_pending` | `registry.rs:1799` | `mod.rs:304` |
| `pending_summary` | `registry.rs:1814` | `mod.rs:307` |
| `replay_pending` | `registry.rs:1838` | `mod.rs:311` (blocking pool) |
| `ensure_fresh` | `registry.rs:1876`, result mapped to its `Debug` label | `mod.rs:406` (blocking pool) |

Not in the trait (D2), held as `IndexingStatus`: `request_activation` (`mod.rs:279`),
`wait_for` (535, async), `progress_detail` (502), `progress_message` (558),
`embedding_wait_spent` (597), `mark_embedding_wait_spent` (614), `embed_progress` (616),
`phase` (617, 660), `cold_cause` (669).

### 2.3 Order documented on `prepare` and the trait

`prepare` (`mod.rs:256-263` doc) keeps its numbered order: activation, wait for index,
`mark_used`, replay. The trait doc carries the lock half (above). `mark_used`'s doc
(`mod.rs:629-636`) keeps "the store is taken and released here, before the handler's own
read guard", and drops the `daemon::lifecycle::CoreActivity` path in favour of
`Activity::request`.

## 3. Edit map

### 3.1 R-11 (S2, sonnet): heartbeat helper

Read for context: `mod.rs` `replay_queued_changes` (298-359), `ensure_file_fresh`
(394-461), `wait_until` (513-583), `env_millis` (153), `human_duration` (162), `trace_call` (95).

1. New `core/src/mcp/progress.rs`; add `mod progress;` beside the other `mod` lines in
   `mod.rs` (alphabetical, after `overrides`).
2. In it:

```rust
/// Whether a heartbeated wait also ends on the request's cancellation.
pub(super) enum Cancel { Honour, Ignore }

pub(super) enum Beat<T> {
    Done(T),
    /// Only with `Cancel::Honour`.
    Cancelled,
}

/// Awaits `task`, sending a progress notification every
/// `PROGRESS_INTERVAL_ENV` while it runs (only with a `progressToken`; `0`
/// sends none). The first tick is one interval after `started`, so a task
/// that finishes within one interval sends nothing; `Delay` so a stalled send
/// does not come back to a burst of catch-up ticks. `progress` is
/// `progress_origin.elapsed()`: one origin per tool call keeps it strictly
/// increasing across that call's heartbeats. Biased in this order: the task,
/// then cancellation, then the tick - a ready outcome wins over a tick or a
/// cancellation ready at the same moment. A failed send is logged and ignored.
/// Returns the outcome and how many notifications were sent.
pub(super) async fn with_heartbeat<F: Future>(
    ctx: &RequestContext<RoleServer>,
    started: Instant,
    progress_origin: Instant,
    cancel: Cancel,
    task: F,
    message: impl Fn() -> String,
) -> (Beat<F::Output>, u32)
```

   Body = the `wait_until` loop (535-567) generalised: `tokio::pin!(task)`; interval/token
   lines from 521-533 verbatim (including the comment at 524-527, moved to the doc above);
   `select!` with `biased;`, `out = &mut task => break Beat::Done(out)`,
   `() = ctx.ct.cancelled(), if matches!(cancel, Cancel::Honour) => break Beat::Cancelled`,
   tick branch building `ProgressNotificationParam::new(token, progress_origin.elapsed().as_secs_f64()).with_message(message())`
   and the existing log line on a failed send (text unchanged).
3. `replay_queued_changes` (314-345): replace with
   `let (beat, sent) = progress::with_heartbeat(ctx, started, call_started, Cancel::Ignore, task, || format!(<same text>, ...)).await;`
   then `let Beat::Done(joined) = beat else { unreachable!(..) };` (or a `match` with an
   `unreachable!` arm). The message closure keeps `started.elapsed()` and
   `self.registry.project_root()` (S3 later renames to `self.host`).
4. `ensure_file_fresh` (409-440): same, message text unchanged.
5. `wait_until` (521-567): `let (beat, sent) = with_heartbeat(ctx, started, started, Cancel::Honour, self.indexing.wait_for(need, deadline), || self.indexing.progress_message(self.registry.project_root())).await;`
   `Beat::Cancelled` runs the existing cancellation `trace_call` + `return Err(..)` (548-556)
   verbatim; `Beat::Done(outcome)` continues to the existing trailing trace.
6. Imports: `ProgressNotificationParam` and `MissedTickBehavior` leave `mod.rs` if unused.
7. Check: `grep -rn "interval_at\|MissedTickBehavior" core/src/mcp` prints only
   `progress.rs`. Tests touched: none in this slice; run `core/tests/it` files
   `index_wait_progress`, `replay_progress`, `first_query_after_walk`, `daemon_log_lines`.

### 3.2 R-17 (S3, opus): host trait

Read for context: `mod.rs` `serve_connection` (186-199), struct (206-218), `new` (222-241),
`mark_used` (637-642), `instructions` (655-...); `daemon/mod.rs:740-760`;
`daemon/front.rs:85-135`; `mcp/front.rs:250-300`; `registry.rs` 464-474, 845-849.

1. New `core/src/mcp/host.rs` (section 2.1); `pub mod host;` in `mod.rs`; `pub use
   host::{Activity, McpHost, PathCoverage};`.
2. Move `PathCoverage` (`registry.rs:462-474`, doc included) to `host.rs`; in `registry.rs`
   `use crate::mcp::PathCoverage;`.
3. Replace `use crate::daemon::registry::PathCoverage;` with `use super::PathCoverage;` in
   `find_definition.rs:14`, `find_callers_callees.rs:18`, `find_implementations.rs:29`,
   `get_dependencies.rs:17`, `find_references.rs:15`, `get_file_outline.rs:20`,
   `not_indexed.rs:29` (and the test import at `not_indexed.rs:225`).
4. `registry.rs:845-849`: delete `query_shapes`; add
   `pub(crate) fn manifests(&self) -> impl Iterator<Item = &PluginManifest>` over
   `self.discovered.manifests.values()`.
5. New `core/src/daemon/mcp_host.rs` (`pub(crate) mod mcp_host;` in `daemon/mod.rs`):
   `pub(crate) struct DaemonHost { registry: Arc<PluginRegistry>, activity: Arc<CoreActivity> }`,
   `impl Activity for DaemonHost` (delegates), `impl McpHost for DaemonHost` (table 2.2),
   `impl Activity for CoreActivity { fn request(&self) { CoreActivity::request(self) } }`
   (explicit path, no accidental recursion).
6. `mod.rs`: field `registry` + `core_activity` -> `host: Arc<dyn McpHost>`;
   `serve_connection(stream, store, host, indexing, embedding)`; `new(store, host, indexing,
   embedding)` with `shapes = Arc::new(QueryShapes::from_manifests(host.manifests()))`;
   every `self.registry.` -> `self.host.`; `self.core_activity.request()` ->
   `self.host.request()`; `spawn_blocking` closures clone `self.host`. Delete lines 26-27.
   Struct doc (201-205) loses the `daemon::lifecycle` reference.
7. `mcp/front.rs:32,255,272`: `Arc<CoreActivity>` -> `Arc<dyn Activity>`; delete the import.
8. `daemon/mod.rs:742-757`: build one `Arc<dyn McpHost>` before the accept loop
   (`Arc::new(DaemonHost::new(..))`), clone it per connection; keep
   `core_activity.connection_opened()` as is (it is the daemon's, not the trait's).
   `daemon/front.rs:131`: passes `core_activity` (coerces to `Arc<dyn Activity>`).
9. Tests: the 7 files that call `GMeshMcpServer::new` (`session_hints.rs:213`,
   `search_code_wait_tests.rs`, `search_code_worker_tests.rs`, `semantic_pending_tests.rs`,
   `semantic_rung_worker_tests.rs`, `search_code_rerank_tests.rs:848`,
   `instructions/tests.rs:592`) switch to a `#[cfg(test)]` `mcp/host_fake.rs`
   `FakeHost` (root, manifests, capabilities, a pending counter, `requests: AtomicUsize`,
   a `path_coverage` map, `ensure_fresh` -> `Ok(None)`). A test that needs a real plugin
   replay keeps `DaemonHost` over a real registry (test code may import `daemon`).
10. Check: `python3 scripts/module-cycles.py --absent 'mcp->daemon'` exits 0;
    `grep -rn "mcp::" core/src/daemon/registry.rs` shows only the `PathCoverage` import.

### 3.3 R-19 (S4, opus): one tool-call context

Read for context: the 17 entry points below; `find_definition.rs` `resolve_lazily` (593),
`resolve_lazily_off_worker` (611); `mod.rs` tool methods 729-955;
`expectations.rs` `EvalContext` (937) and `SymbolTool::call` (1052-1100), 1101, 1264, 1550.

Entry points today (g-mesh outline/grep, `#[cfg(test)]` marked T):

| File | Entry points |
|---|---|
| `find_references.rs` | `handle` 223, `handle_in` 318, `handle_in_covered` 331 |
| `find_callers_callees.rs` | `handle_callers` 446, `handle_callers_in` 549, `handle_callers_in_covered` 562, `handle_callees` 679 T, `handle_callees_in` 751 T, `handle_callees_in_covered` 766 |
| `find_definition.rs` | `handle` 1215, `handle_in` 1230 |
| `find_implementations.rs` | `handle` 166 T, `handle_in` 182, `dispatch` 570, `dispatch_in` 584, `dispatch_in_covered` 598 |
| `get_dependencies.rs` | `handle` 787, `handle_covered` 803 |
| `get_file_outline.rs` | `handle` 158 T, `handle_covered` 168 |
| `search_code.rs` | `handle` 319, `handle_off_worker` 293 |

1. New `core/src/mcp/context.rs`:

```rust
/// Everything one tool call reads besides its parameters. Built once per call
/// (by `GMeshMcpServer` or `plugin_check`), moved into the blocking closure.
#[derive(Clone)]
pub(crate) struct QueryCtx {
    pub(crate) store: Arc<IndexStore>,
    pub(crate) embedding: Arc<EmbeddingPipeline>,
    pub(crate) shapes: Arc<QueryShapes>,
    pub(crate) hints: SessionHints,            // a clone shares the session's set
    pub(crate) project_root: PathBuf,
    pub(crate) capabilities: HashMap<String, Capabilities>,
    pub(crate) entry_points: Vec<String>,      // empty except get_dependencies
    pub(crate) uncovered: Vec<(String, PathCoverage)>, // file_paths filter coverage
    pub(crate) coverage: Option<PathCoverage>, // the anchoring file's coverage
}
impl QueryCtx {
    pub(crate) fn resolve_lazily(&self, h: impl Fn(&SemanticRung<'_>) -> ..) -> ..;
    pub(crate) async fn resolve_lazily_off_worker(self, h: ..) -> ..;
    #[cfg(test)] pub(crate) fn for_test(store: Arc<IndexStore>) -> Self; // fresh hints, empty rest
}
```

2. Each ladder collapses to one `handle(ctx, semantic, params)` (symbol tools) or
   `handle(ctx, params)`; `handle_callees` and the other T wrappers become
   `ctx.resolve_lazily(|s| handle(&ctx, s, params.clone()))` at their test call sites, or
   a `#[cfg(test)]` one-liner that takes `&QueryCtx` (never builds hints itself).
3. `mod.rs`: `fn query_ctx(&self, file_paths: Option<&[String]>, file: Option<&str>) -> QueryCtx`
   replaces the per-tool `(store, capabilities, hints, params)` tuples and
   `filter_coverage`/`path_coverage` calls; `find_implementations` keeps "transitive names
   no coverage" (865-869) by passing `None`.
4. `plugin_check/expectations.rs`: `EvalContext` gets `fn query_ctx(&self) -> QueryCtx` with
   `SessionHints::default()` per call (today's behaviour: every call sees a fresh session;
   keep it, the expectations compare JSON that may carry hints).
5. Test files calling the old names (g-mesh `find_callers`, section 8): `semantic_pending_tests`,
   `answer_tests`, `find_definition/tests.rs`, `member_name_collision_tests`,
   `response_bound_tests`, `unlinked_tests`, `untyped_tests`, `get_dependencies/tests.rs`,
   `semantic_rung_worker_tests`, plus in-file tests of `find_references.rs` and
   `find_callers_callees.rs`.

### 3.4 R-27 (S5, sonnet): feature gate

See section 4 for the full list; the edit map is that list.

## 4. Feature-gate plan (R-27)

- `core/Cargo.toml`: `embed-eval = []`; `ep-coreml = ["embed-eval", "ort/coreml",
  "dep:tracing-subscriber"]`; update the comment above it.
- `cli/mod.rs:37` `#[cfg(feature = "embed-eval")] pub mod embed_eval;`; `:234-242` the
  `DebugEmbedEval` variant `#[cfg(feature = "embed-eval")]`; `:384` match arm likewise.
- Eval-only hooks: `mcp/mod.rs:79-82` the two `pub(crate) use` re-exports and
  `search_code.rs:278` `top_k_for_eval` get `#[cfg(feature = "embed-eval")]`;
  `similarity::floor` stays (used by `search_code`).
- Anything else that turns dead without the feature (candidates: `embedding::model`
  non-production `EncoderSpec`/`InferenceOptions` paths, `embedding::text::embed_tier`,
  `embedding::backfill::PAGE_SIZE` visibility): only `pub(crate)`/private items warn
  (`embedding` is `pub mod`, so `pub` items do not). Gate each found item with
  `#[cfg(feature = "embed-eval")]`, never `allow(dead_code)`.
- CI and scripts (none calls `debug-embed-eval` today; K-11, re-checked: no hit in
  `scripts/` or `.github/`):
  - `scripts/check.sh` `run_clippy`: keep `cargo clippy --workspace --all-targets -- -D warnings`
    (default build, catches dead code) and add
    `cargo clippy -p g-mesh --all-targets --features embed-eval -- -D warnings`.
  - `scripts/test-sections.sh:250`: `cargo nextest run --workspace --features g-mesh/embed-eval ...`
    so `core-cli` keeps running the eval's unit tests; one feature set for every section,
    so still one build.
  - `scripts/test-local.sh:281` (`cargo test -p "$crate" --lib`): add the same feature when
    `$crate` is `g-mesh`.
  - Release (`build-targets.sh`, `release.yml`, `cut-release.sh`): unchanged, default
    features, so the shipped binary excludes the eval. `cut-release.sh`'s GM-302
    `preserve_order` check is unaffected (the feature adds no dependency).
  - `docs/architecture/embedding-eval.md` (around 146): one sentence, "build with
    `--features embed-eval`". Historical design notes stay as they are.

## 5. Projected checker

`python3 scripts/module-cycles.py --help`: `--drop/--add` edit the computed graph before
Tarjan; `--absent` checks after them. Runs on `main`:

```
$ python3 scripts/module-cycles.py
SCC (3): daemon, mcp, watcher
SCC (3): embedding, graph, storage

$ python3 scripts/module-cycles.py --drop 'mcp->daemon' --absent 'mcp->daemon'
SCC (3): embedding, graph, storage
SCC (2): daemon, watcher
ok: edge mcp->daemon absent
```

`daemon -> mcp` stays (the daemon hosts the server, implements the trait, imports
`PathCoverage`): no new edge. The remaining `{daemon, watcher}` is GM-707's (R-15). With
GM-707's `watcher->daemon` also dropped (`--drop 'mcp->daemon' --drop 'watcher->daemon'`)
only `{embedding, graph, storage}` remains (GM-708). Counter-projection for D3: returning
`StalenessOutcome` would add `mcp->watcher`; with `watcher->daemon` and `daemon->mcp` that
closes `{daemon, mcp, watcher}` again.

## 6. Must confirm, per slice

- **S2 (R-11):** the four `core/tests/it` progress/log files pass unchanged; the trace
  lines (`replay:`, `ensure_fresh:`, `prepare: cancelled`, `prepare: wait over`) and their
  `progress_sent` values are unchanged; `wait_until` passes `started` (not
  `call_started`) as the progress origin.
- **S3 (R-17):** `private_interfaces` under `clippy -D warnings` (D1 risk); the
  `#[tool_router]` impl compiles with an `Arc<dyn McpHost>` field; `--absent 'mcp->daemon'`
  passes and `--include-tests` lists which test files still import `daemon` (allowed);
  `DaemonHost` is built once, not per connection; `connection_opened()` still precedes the
  spawn (`daemon/mod.rs:745-747`). Rebase over GM-707 if it moved the accept loop.
- **S4 (R-19):** GM-708 merged first (section 7); the find_implementations transitive
  rule; `plugin_check`'s fresh hints per call kept; `cargo nextest` for sections
  `core-mcp`, `core-cli` (plugin_check).
- **S5 (R-27):** the dead-code list from `cargo clippy -p g-mesh --all-targets -- -D warnings`
  without the feature; `cargo run -p g-mesh -- debug-embed-eval --help` fails without the
  feature and succeeds with it; `cargo nextest --features g-mesh/embed-eval` accepted with
  `--workspace` on the pinned toolchain; `tracing-subscriber` used only by eval code.
- **Verify:** both clippy lines, sections `core-mcp`, `core-cli`, `core-daemon`, `core-it`;
  `module-cycles.py --absent 'mcp->daemon'`; one `interval_at` site; `grep -n
  "mcp::query_shapes" core/src/daemon` empty.

## 7. Slicing and merge order

Proposed re-cut of the draft (S2 R-11 sonnet, S3 R-17 opus, S4 R-19+R-27 opus, S5 review,
S6 tests, S7 verify):

| Slice | Content | Model | Note |
|---|---|---|---|
| S2 | R-11 | sonnet | edit map 3.1 |
| S3 | R-17, incl. `FakeHost` and the 7 test call sites | opus | tests must compile in this slice anyway: `new`'s signature changes |
| S4 | R-19 context struct only | opus | starts after GM-708 is merged into the release branch and into this task branch |
| S5 | R-27 | sonnet | split out of S4: independent of R-19, mechanical with section 4 |
| S6 | review | opus | |
| S7 | tests: fake-host server tests (lock order: no store held at `replay_pending`; `request` counted once per call), heartbeat order tests, the default build lacks `debug-embed-eval` | opus | controls per behaviour |
| S8 | verify | opus | |

**With GM-708 (shared files: `mcp/mod.rs`, the six handler files,
`cli/plugin_check/expectations.rs`, `cli/embed_eval.rs`/`schedule.rs` via R-12):**
GM-708 lands first for the handler files. S2, S3 and S5 may run in parallel with GM-708:
their `mcp/mod.rs` regions (heartbeat, `prepare`/`new`/fields, lines 79-82) differ from
GM-708's (`instructions()` SQL/R-32, budget imports); the one overlap is
`self.registry` -> `self.host` inside `instructions()` (3 lines), which the second to merge
rebases. S4 rewrites every handler's head and its test callers, GM-708 moves SQL and budget
calls out of the same bodies verbatim; a verbatim move is the harder thing to redo, so
GM-708 goes first and S4 adapts. GM-709 merges into the release branch after GM-708.
With GM-707: only `daemon/mod.rs`'s accept loop (3 lines, S3).

## 8. g-mesh calls behind this note (project `g-mesh`)

| Question | Call | Result |
|---|---|---|
| Callers of `PluginRegistry::query_shapes` | `find_callers` | 1 file: `mcp/mod.rs` |
| References of `CoreActivity` | `find_references` | 22 refs: `daemon/{lifecycle,activation,front,mod}.rs`, `mcp/mod.rs`, `mcp/front.rs`, 4 mcp test files |
| References of `PathCoverage` | `find_references` | 24 refs: 8 `mcp/` files + `registry.rs` |
| Callers of `GMeshMcpServer::new` | `find_callers` | 6 test files + `mod.rs`; `unlinkedUsages` 1 in `search_code_rerank_tests.rs` (grep confirms its `PluginRegistry` setup at 848-849) |
| Callers of `find_references::handle` | `find_callers` | `find_references.rs`, `plugin_check/expectations.rs`, 7 mcp test files |
| Callers of `find_callers_callees::handle_callers` | `find_callers` | same set + `find_references.rs` |
| Callers of `find_definition::handle` | `find_callers` | `find_definition/tests.rs`, `expectations.rs` (2), `semantic_rung_worker_tests.rs` |
| Callers of `get_dependencies::handle` | `find_callers` | `get_dependencies/tests.rs`, `expectations.rs` |
| Callers of `find_definition::resolve_lazily` | `find_callers` | `find_callers_callees`, `find_implementations`, `find_references`, `find_definition`, `semantic_pending_tests` |
| `mcp/mod.rs` symbols and lines | `get_file_outline` | used for every line anchor in 3.1-3.2 |

grep was used for: the registry/activity/indexing call list in `mcp/` (one module, receiver
calls through fields), `daemon` imports in `mcp/`, the `handle*` line list, embed-eval
importers and CI/scripts (non-code), and the trace-string assertions in `core/tests/it`.

## 9. Owner decisions (2026-11-16)

- D2 `IndexingStatus`: "Оставить вне трейта (Recommended)".
- D7 R-19 second half (typed `Answer<Page>`, `mcp::query`): "В бэклог отдельной задачей (Recommended)".
- Slicing (section 7): "Принять (Recommended)". S4 (R-19) starts after GM-708 and GM-723's mcp part
  are merged into the release branch.
- Design: "Утвердить (Recommended)".
