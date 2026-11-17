# GM-724: typed MCP tool answers and an `mcp::query` module (R-19, second half)

Source findings: `docs/review/4.30/README.md` R-19 (C-fable's half: typed `Answer<Page>`
results and a `query` module so `plugin_check` stops re-parsing JSON);
`docs/design/GM-709-mcp-host-trait.md` D7 (why this half was split out). Read against
`release-4.33.0` (= `main` + version bump, so GM-708's `response_bound` and GM-709's
`QueryCtx` are in). Line numbers are this branch at the time of writing; each code slice
re-reads by symbol, not by line.

## 0. Where we start

### 0.1 The tool handlers and how their bytes are made

Eight `#[tool]` methods on `GMeshMcpServer` (`core/src/mcp/mod.rs` 670-809). Each runs
`prepare` (an early `CallToolResult` when the index wait times out or the walk failed),
builds a `QueryCtx` (`query_ctx`, 352) and calls one handler that returns
`Result<CallToolResult, ErrorData>`:

| Tool (mod.rs) | Handler | Shapes it answers with today |
|---|---|---|
| `find_definition` 670-686 | `find_definition::handle` 1160-1196 via `resolve_lazily_off_worker` | `DefinitionNode` (`by_name` 1129, `by_symbol_id` 1145, `by_position` 439); `CandidatePage` (ambiguous; `resolve_symbol_name` 632, `by_qualified_name_suffix` 700, `by_stripped_prefix` 732); `FileNamePage` (`by_semantic_neighbours` 906, `by_file_name` 1061); text refusal (`import_only_refusal` 1006, parameter errors); `not_indexed` JSON refusal (`by_position` miss) |
| `find_references` 692-704 | `find_references::handle` 304-419 | `ReferencePage<'a>` (rows, `parts.response`, 239-297); `answer::Summary<'a, ReferenceDisclosures>` for `answer: files/count` (`answer::respond` 52-86); the anchor's candidate/file-name page or refusal (`anchor::resolve` 118-136) |
| `find_callers` 707-719 | `find_callers_callees::handle_callers` 537-659 | `CallerPage<'a>` (465-530); `Summary<CallerDisclosures>`; anchor redirects |
| `find_callees` 722-734 | `find_callers_callees::handle_callees` 721-808 | `CalleePage<'a>` (675-716); `Summary<CalleeDisclosures>`; anchor redirects |
| `find_implementations` 740-754 | `find_implementations::dispatch` 553-599 | `ImplementationPage<'a>` (`handle` 166-242); `TransitiveImplementationWalk` (`from_root` 454-489, `continued` 506-543); anchor redirects; text refusal |
| `get_file_outline` 760-771 | `get_file_outline::handle` 156-188 (sync, no rung) | `OutlinePage<'a>` (`Outline::body` 112-119); `not_indexed::miss` |
| `get_dependencies` 777-790 | `get_dependencies::handle` 788-814 (sync, no rung) | `DependencyWalk` (`from_root_reporting` 358-382, `continued` 755-778); text refusals (`from_file` 408-443, `incoming_from_file` 495-518, `from_module` 732-750); `not_indexed::miss` |
| `search_code` 796-809 | `search_code::handle_off_worker` 275-283 -> `handle` 295-389 | `SearchPage`, or **two** content blocks (coverage note text, then the page) while the embedding pass is owed (384-387); text refusals |

The front server (`mcp/front.rs`, `select_project` and the not-selected answer) is a
separate `ServerHandler` that answers text only (`text_result` 195-200); it calls no
handler above.

Where the bytes are produced today (grep, non-test code):

- `tool_result::success` (`tool_result.rs` 10-12): `CallToolResult::success(vec![ContentBlock::json(value)?])`;
  `ContentBlock::json` is `serde_json::to_string` (rmcp 2.2.0, `model/content.rs` 268).
  Callers: `answer.rs` 77, `find_definition.rs` 451/669/714/787/803/946/1100/1137/1151,
  `find_references.rs` 418, `find_callers_callees.rs` 658/807, `find_implementations.rs`
  232/488/542, `get_dependencies.rs` 381/777, `get_file_outline.rs` 187, `search_code.rs` 383.
- `tool_result::error` (14-16): text `isError: true`. Used from `anchor.rs`, `find_definition.rs`,
  `find_implementations.rs`, `get_dependencies.rs`, `search_code.rs`, `not_indexed.rs`.
- Built in place, not through `tool_result`: `not_indexed::refusal` 197-200 (JSON body under
  `isError: true`), `search_code::handle` 384-387 (note + JSON), `mod.rs` `walk_failed` 187-191
  and `wait_for_index` 469 (text errors), `front.rs` 152/195-200.
- Finished answers travel *inside* the handlers as `CallToolResult`: `anchor::Anchor =
  Result<Result<Resolved, CallToolResult>, ErrorData>` (`anchor.rs` 40),
  `resolve_symbol_name`/`by_file_name` return the same nested shape, `by_semantic_neighbours`
  `Option<Result<CallToolResult, _>>`, `import_only_refusal` `Result<Option<CallToolResult>, _>`,
  `wait_for_embeddings` `Result<Result<Option<Coverage>, CallToolResult>, _>`.
- `QueryCtx::resolve_lazily` 55-69 / `resolve_lazily_off_worker` 77-102 (`context.rs`) are
  typed on `Result<CallToolResult, ErrorData>`.

Where the byte budget applies: **inside each handler, before** `success`. Every bounded
handler builds candidate responses and measures them with `response_bound::wire_len`
(`serde_json::to_vec`, the same bytes as `ContentBlock::json`): `bound_page_in_response`
(find_references `list_references`, callers, callees, implementations' `handle`),
`fit_budget` (`answer::respond`), `bound_defines_page` (outline), `bound_walk` /
`longest_prefix_fitting` (dependencies). The measured response depends on per-call state
that is only *spent* on the sent one (`send: bool` -> once-per-session hints), and the cut
decides `has_more`, the cursor and whether `total` is counted (one more SQL `COUNT`).

### 0.2 Who reads tool JSON back

- `cli/plugin_check/expectations.rs` - the only in-process consumer (g-mesh `find_callers
  tool_outcome`: `SymbolTool::call` 1046, `call_definition` 1077, `dependency_walk` 1229,
  `resolve_definition` 1511 (two calls); plus the test
  `a_tool_level_refusal_is_a_refusal_and_a_protocol_error_is_not`, `expectations/tests.rs` 479).
  `tool_outcome` (1794-1806) takes `content.first()`'s text, returns `Refusal(text)` on
  `is_error`, else `serde_json::from_str` into `Value`. Every finding (`rows_to_list`,
  `provenance_finding`, `files_finding`, `is_candidate_page` 1716, `pick_candidate`, ...)
  reads that `Value`. Its `QueryCtx` is built once in `cli/plugin_check/mod.rs`
  `expectations_section` 411-479 (`EvalContext { query: QueryCtx { .. } }`, 465-477), with
  `coverage: None`, `uncovered: []`, the disabled embedding pipeline; `EvalContext::query_ctx`
  (942-944) clones it with fresh `SessionHints` per call.
- `daemon/client.rs` `answer_of` 91-101 (used by `g-mesh deps`): reads `isError` and text of
  a `get_file_outline` call **over the wire** (a real MCP client). Out of scope: it must parse.
- Unit tests: ten `json_body`/`error_text` helpers (`find_references.rs` 450/458,
  `find_callers_callees.rs` 839/847, `get_file_outline.rs` 207/215, `search_code.rs` 646/654,
  `anchor.rs` 178, `host_tests.rs` 150, `member_name_collision_tests.rs` 154,
  `unlinked_tests.rs` 118, `get_dependencies/tests.rs` 52/60, `find_implementations/tests.rs`
  26/34, `find_definition/tests.rs` 41/49, `not_indexed.rs` `refusal_body` 256 / `plain_error` 267).
  They call handlers *and internal functions* that return `CallToolResult`:
  `find_definition/tests.rs` `by_name` (~20 sites), `get_dependencies/tests.rs`
  `from_file`/`from_module` (~15 sites, shadow helpers 31-38), `find_implementations/tests.rs`
  `continued` (2), `not_indexed.rs` `miss` (2).
- `tests/it/*` read the wire through a real daemon (35 files mention `isError`/`CallToolResult`;
  e.g. `lazy_activation.rs` 398 `a_failed_walk_is_a_tool_error_and_is_retried`,
  `serving_while_indexing.rs`, `multi_project_front.rs`, `plugin_check.rs` 1154).

### 0.3 Tests that pin output bytes today

None pins raw bytes per tool. What exists: JSON-level assertions in every handler test
module (parsed `Value`, so key order and whitespace are not pinned);
`response_bound_tests.rs` (pins `wire_len` of whole responses against the ceilings - sensitive
to any added/removed field); `search_code_golden.json` (`search_code_rerank_tests.rs` 161,
JSON-level with a score tolerance); `search_code_wait_tests.rs` 166 (pins two content blocks,
note first); `find_implementations/tests.rs` 627 ("byte-identically", compares `json_body`);
`instructions/tests.rs` 116 (instructions, not answers); `protocol_conformance.rs` (tools/list,
schemas). **Gap: field order and the exact `isError`/block layout per shape are not pinned.**
Section 5 closes it before any code moves.

## 1. Decisions

### D1. One generic outer type `ToolAnswer<P>` plus a per-tool page `P` (recommended) - vs per-tool flat enums, vs one crate-wide enum

- **Today:** every handler returns `CallToolResult`; three cross-tool outcomes (the anchor's
  candidate page, its file-name page, a refusal) are ready-made `CallToolResult`s that the
  four anchored tools and `find_definition` pass through unchanged.
- **Change (recommended, A):** `mcp/query.rs` declares
  `enum ToolAnswer<P> { Page(P), Candidates(Candidates), Refused(Refusal) }`. The three
  shared outcomes are written once; each tool picks `P` (section 2.2). The anchor's nested
  `Result<Result<Resolved, CallToolResult>, _>` becomes `Result<Result<Resolved, Redirect>, _>`
  with `type Redirect = ToolAnswer<Infallible>` and `fn widen<P>(self) -> ToolAnswer<P>`.
- **Option B, per-tool flat enums** (`enum ReferencesAnswer { Rows, Files, Count, Ambiguous,
  FileName, Refused }` x 8): every enum restates the three shared variants, and the anchor
  needs a conversion into each of five enums. More code, same bytes.
- **Option C, one crate-wide `enum Answer { References(..), Callers(..), ... }`:** a consumer
  must match a variant it already knows it gets; `plugin_check` would carry `unreachable!`s.
- **Example:** `find_references::answer(ctx, rung, params) -> Result<ToolAnswer<EdgeAnswer<ReferencePage, ReferenceDisclosures>>, ErrorData>`;
  an ambiguous name returns `Ok(redirect.widen())`.
- **Consequence:** `resolve_lazily*` become generic over the answer (`R` instead of
  `CallToolResult`); one `render` matches three variants. Name: **not** `Answer` -
  `mcp::Answer` (`mod.rs` 876) is the `answer: rows|files|count` parameter enum (15
  references; `#[schemars(inline)]`, so its name is not in the schema, but renaming it is
  churn outside this task's subject). `ToolAnswer` it is; the owner may pick another name.

### D2. Truncation and paging stay in the handlers (recommended) - vs the type knows its growable part and `response_bound` bounds it

- **Today:** each handler measures candidate responses (`wire_len`) inside `fit_budget` /
  `bound_page_in_response` / `bound_defines_page`, and the cut feeds SQL (`total` is counted
  only when `has_more`), the cursor encoding (edge rank vs source order vs resume token), and
  once-per-session hints (`send`).
- **Change (recommended, A):** a handler returns an **already bounded** answer; `response_bound`
  keeps its cut helpers unchanged and gains only `render` (section 2.3), the one place a
  `ToolAnswer` becomes a `CallToolResult`. Measuring still uses `wire_len` on the same struct
  that `render` serializes, so "measured bytes = sent bytes" holds by construction.
- **Option B:** a `trait Bounded { fn growable(&mut self) -> ...; fn cut(..) }` per page and one
  generic fit loop in `response_bound`. It would have to model four cursor kinds, the
  `total`-needs-a-COUNT dependency and the spend-hints-only-on-send rule; every one is a byte
  risk, none is asked for by R-19.
- **Consequence:** the acceptance line "bytes are produced in one place" is met for the
  *rendering* (one `ContentBlock::json` call site); measurement stays where the cut is decided.

### D3. Wire structs become owned (recommended) - vs a borrowed view behind an owned answer

- **Today:** `ReferencePage<'a>`, `CallerPage<'a>`, `CalleePage<'a>`, `ImplementationPage<'a>`,
  `OutlinePage<'a>`, `answer::Summary<'a, D>`, the `*Disclosures<'a>` borrow from the handler's
  locals (`ReferenceParts`, `CallerParts`, ...), so they cannot be returned.
- **Change (recommended, A):** drop the lifetimes; fields own their values (`AnchorInfo`,
  `Vec<FileTally>`, `Option<UnlinkedUsages>`, ...). The measuring closure builds the same owned
  struct (one definition = one field order, byte identity by construction). Cost: one clone of
  the non-row parts (bounded by their own caps, `FILE_TALLY_MAX_BYTES` 8,000 etc.) per
  measurement round; the rows are already cloned per round by `bound_page_in_response`
  (`rows.clone()`). Small next to the SQL.
- **Option B:** keep the borrowed structs for measuring; add owned `*Answer` types whose
  `Serialize` builds the borrowed view. No clones, but two definitions per shape (drift risk on
  field order) and the hints must still be resolved before the view exists.
- **Consequence:** `parts.response(page, total, send)` returns an owned page; `send: true` is
  called exactly once, as today.

### D4. `plugin_check` takes typed outcomes and serializes pages to `Value` (recommended) - vs typed field access everywhere

- **Today:** `tool_outcome` re-parses rendered text; ~20 finding functions read `Value`.
- **Change (recommended, A):** `plugin_check` calls `mcp::query::*` and gets
  `Result<ToolAnswer<P>, ErrorData>`. `tool_outcome<P: Serialize>` matches the variant:
  `Refused(r)` -> `ToolOutcome::Refusal(r.message())`; `Page(p)` / `Candidates(c)` ->
  `ToolOutcome::Answer(serde_json::to_value(&p))`. No `from_str`, no `ContentBlock`, no
  `CallToolResult`, no `is_error` in `expectations.rs`. The finding functions, `ToolOutcome`
  and `is_candidate_page` (pinned by `expectations/tests.rs` 35/58/70) are unchanged.
  `to_value` gives the same `Value` as the old parse for every shape `plugin_check` reads: none
  carries a float (`f32`/`f64` grep over the page modules: only `SemanticRung::Embedded`'s query
  vector), and object key order does not matter to `Value` either way.
- **Option B:** make the page fields `pub(crate)` and rewrite the finders on typed fields. Real
  typing of the kit, but ~20 functions and the module doc's decision 2 ("reads back their wire
  JSON") change, and the `Value` path is what makes the kit check the wire shape a client sees.
  A separate task if wanted.
- **`Refusal::message()` vs the wire text:** they differ only for `Refusal::NotIndexed` (the wire
  text is the JSON body). `plugin_check`'s context has `coverage: None` and `uncovered: []`, so
  it never gets one; `message()` is the readable form if it ever does.
- **Consequence:** the two tests that build a `CallToolResult` for `tool_outcome`
  (`expectations/tests.rs` 479-506) must be rewritten on `ToolAnswer` (D6).

### D5. All tools convert in this task; code in two slices split by coupling (recommended) - vs one code slice, vs tools across tasks

- **Today:** five handlers share `anchor::Anchor` / `resolve_symbol_name` (`find_definition`,
  references, callers, callees, implementations), so they convert together. Outline,
  dependencies and search_code do not use the anchor.
- **Change (recommended):** with `resolve_lazily` generic and `render` at each `#[tool]` method,
  a tool can be converted while another still returns `CallToolResult` - the build stays green
  between slices. S3 (opus): foundation + the five anchored handlers. S4 (sonnet, edit map):
  outline, dependencies, search_code, `mod.rs` early answers, `plugin_check`, delete
  `tool_result::{success, error}`.
- **Option B, one opus code slice:** fewer handoffs, one ~12-file diff for one reviewer.
- **Option C, convert only what `plugin_check` uses** (5 of 8 tools): leaves two answer styles
  in `mcp` - the inconsistency R-19 is about.

### D6. Existing tests: assertions unchanged, plumbing adapted (recommended) - vs literally no test file touched

- **Today:** ~40 unit-test call sites call internal functions that return `CallToolResult`
  (section 0.2) and read `.content` / `.is_error` through ten helpers.
- **Change (recommended, A):** make the ten helpers take `&impl Rendered` (`trait Rendered { fn rendered(&self) -> CallToolResult }`, `#[cfg(test)]`,
  implemented for `CallToolResult` and `ToolAnswer<P: WirePage>` via `render`). Call sites stay
  as written; sites that read `result.is_error` directly gain `.rendered()`. No expected value,
  no assertion changes. The two `tool_outcome` tests in `plugin_check` are rewritten (their
  subject's input type changed; the behaviour they pin is kept).
- **Option B:** keep `CallToolResult`-returning wrappers next to every typed internal function
  so test files compile untouched. Dead-in-production wrappers (`#[cfg(test)]` on ~12 fns)
  whose only job is to pin nothing new.
- **Consequence:** "existing tests unchanged" is read as: no assertion, expected string or
  fixture changes; helper signatures may change. Owner to confirm.

### D7. Server-level early answers render through the same `render`; `front.rs` stays out (recommended)

- **Today:** `walk_failed` (187), `wait_for_index` (469), `wait_for_embeddings` (539-575) build
  text `CallToolResult::error`s in `mod.rs`.
- **Change:** they return `Refusal::Text(..)`; each `#[tool]` method ends with
  `response_bound::render(..)`. `front.rs` is another server (a project picker with `_meta`),
  answers no index question and is not read by `plugin_check`; converting it adds a
  `WithMeta` case to `render` for no consumer. Leave it.

## 2. Types

### 2.1 `core/src/mcp/query.rs`

```rust
//! Typed answers of the index-backed MCP tools, and the one door to them.
//! The server renders them (`response_bound::render`); `plugin_check` reads them.

pub(crate) enum ToolAnswer<P> {
    /// The tool's own answer (`isError` unset).
    Page(P),
    /// `symbol_name` did not pin one declaration: the ranked candidates, or
    /// the file-name / semantic-neighbour suggestions (`isError` unset).
    Candidates(Candidates),
    /// A tool-level refusal (`isError: true`).
    Refused(Refusal),
}

/// What `find_definition`'s name ladder hands back instead of a node.
pub(crate) type Redirect = ToolAnswer<std::convert::Infallible>;
impl Redirect { pub(crate) fn widen<P>(self) -> ToolAnswer<P> { .. } }

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Candidates {
    Ambiguous(find_definition::CandidatePage),   // resolvedBy: nameAmbiguous
    Suggested(find_definition::FileNamePage),    // resolvedBy: fileName | semanticNeighbours
}

pub(crate) enum Refusal {
    /// Prose, one text block.
    Text(String),
    /// A miss on a path whose language is not indexed: JSON `{error, notIndexed}`.
    NotIndexed { error: String, not_indexed: NotIndexed },
}
impl Refusal { pub(crate) fn message(&self) -> &str; }

/// A page that leads with a prose note in its own content block
/// (`search_code` while embeddings are owed). Default: none.
pub(crate) trait WirePage: Serialize { fn lead_note(&self) -> Option<&str> { None } }

// The door: one fn per tool, both passes of the semantic rung included.
pub(crate) fn definition(ctx: &QueryCtx, p: FindDefinitionParams) -> Result<ToolAnswer<DefinitionNode>, ErrorData>;
pub(crate) fn references(ctx: &QueryCtx, p: SymbolQueryParams) -> Result<ToolAnswer<ReferencesAnswer>, ErrorData>;
pub(crate) fn callers(ctx: &QueryCtx, p: SymbolQueryParams) -> Result<ToolAnswer<CallersAnswer>, ErrorData>;
pub(crate) fn callees(ctx: &QueryCtx, p: SymbolQueryParams) -> Result<ToolAnswer<CalleesAnswer>, ErrorData>;
pub(crate) fn implementations(ctx: &QueryCtx, p: FindImplementationsParams) -> Result<ToolAnswer<ImplementationAnswer>, ErrorData>;
pub(crate) fn dependencies(ctx: &QueryCtx, p: GetDependenciesParams) -> Result<ToolAnswer<DependencyWalk>, ErrorData>;
pub(crate) fn outline(ctx: &QueryCtx, p: GetFileOutlineParams) -> Result<ToolAnswer<OutlinePage>, ErrorData>;
// + `*_off_worker(ctx: QueryCtx, p) -> impl Future<..>` for the five laddered tools and search.
pub(crate) async fn search_off_worker(ctx: QueryCtx, p: SearchCodeParams, c: Option<Coverage>) -> Result<ToolAnswer<SearchAnswer>, ErrorData>;
```

The sync doors are `ctx.resolve_lazily(<module>::answer, p)`; the async ones
`ctx.resolve_lazily_off_worker(<module>::answer, p)`. `plugin_check` uses the sync ones.

### 2.2 Per-tool pages

| Tool | `P` | Notes |
|---|---|---|
| find_definition | `DefinitionNode` | candidates/file-name pages go to `Candidates` |
| find_references | `type ReferencesAnswer = EdgeAnswer<ReferencePage, ReferenceDisclosures>` | |
| find_callers | `type CallersAnswer = EdgeAnswer<CallerPage, CallerDisclosures>` | |
| find_callees | `type CalleesAnswer = EdgeAnswer<CalleePage, CalleeDisclosures>` | |
| find_implementations | `#[serde(untagged)] enum ImplementationAnswer { Direct(ImplementationPage), Transitive(TransitiveImplementationWalk) }` | |
| get_dependencies | `DependencyWalk` | |
| get_file_outline | `OutlinePage` | owned (D3) |
| search_code | `struct SearchAnswer { note: Option<String>, page: SearchPage }`, `Serialize` delegates to `page` by hand, `lead_note` = `note` | not `#[serde(flatten)]` (map serializer; keep the derive path) |

```rust
// answer.rs, next to Summary
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum EdgeAnswer<R, D> { Rows(R), Summary(Summary<D>) }
```

`#[serde(untagged)]` on a `Serialize`-only enum writes the inner value as is, so each
variant's bytes are its struct's bytes. Types and the fields `render` needs become
`pub(crate)`; field visibility otherwise stays private (D4 A reads `Value`).

### 2.3 One renderer: `response_bound::render`

```rust
/// The one place a tool answer becomes bytes. `wire_len` measures the same
/// serialization (`serde_json` compact; `ContentBlock::json` is `to_string`).
pub(crate) fn render<P: WirePage>(answer: ToolAnswer<P>) -> Result<CallToolResult, ErrorData> {
    match answer {
        ToolAnswer::Page(page) => {
            let mut blocks = Vec::with_capacity(2);
            if let Some(note) = page.lead_note() { blocks.push(ContentBlock::text(note.to_owned())); }
            blocks.push(ContentBlock::json(&page)?);
            Ok(CallToolResult::success(blocks))
        }
        ToolAnswer::Candidates(c) => Ok(CallToolResult::success(vec![ContentBlock::json(&c)?])),
        ToolAnswer::Refused(Refusal::Text(t)) => Ok(CallToolResult::error(vec![ContentBlock::text(t)])),
        ToolAnswer::Refused(Refusal::NotIndexed { error, not_indexed }) =>
            Ok(CallToolResult::error(vec![ContentBlock::json(&not_indexed::Refusal { error, not_indexed: &not_indexed })?])),
    }
}
```

`render` for `ToolAnswer<Infallible>` (the server's early answers) needs `impl WirePage for
Infallible` (or a `render_refusal(Refusal)`; S3 picks). `tool_result::{success, error}` are
deleted at the end of S4; `internal_error` stays.

### 2.4 `plugin_check` through `mcp::query`

- `QueryCtx` is still built once in `cli/plugin_check/mod.rs` `expectations_section`
  (465-477) and cloned per call with fresh hints by `EvalContext::query_ctx` (942-944):
  **neither changes** (keeps GM-711's area in `mod.rs` untouched).
- `SymbolTool::call` (1046-1071): `query::callers(&ctx.query_ctx(), params)` /
  `query::references(..)` / `query::implementations(.., transitive: Some(false))`, each through
  `tool_outcome`.
- `call_definition` (1077-1080), `resolve_definition` (1523 retry), `dependency_walk`
  (1242): `query::definition` / `query::dependencies`.
- `tool_outcome` (1794-1806) as in D4; `use rmcp::model::{CallToolResult, ContentBlock}` (621)
  goes; module doc lines 6-22 updated ("reads back typed answers, serialized as a client sees
  them").

## 3. Edit map

### 3.1 S3 (opus): foundation + the five anchored handlers

Change:

1. New `core/src/mcp/query.rs` (2.1), `mod query;` in `mcp/mod.rs`.
2. `response_bound.rs`: add `render` (2.3) next to `wire_len` (106-108).
3. `context.rs` `resolve_lazily` 55-69, `resolve_lazily_off_worker` 77-102: generic `R`
   instead of `CallToolResult`.
4. `anchor.rs`: `Anchor` (40) -> `Result<Result<Resolved, Redirect>, ErrorData>`; `resolve`
   118-136, `by_id` 154-163 return `Refusal::Text` redirects.
5. `find_definition.rs`: `CandidatePage` (183), `FileNamePage` (266), `DefinitionNode` (52)
   `pub(crate)`; `CandidatePage::ambiguous` 221-257 call sites wrap in `Candidates::Ambiguous`;
   `resolve_symbol_name` 632-689, `by_qualified_name_suffix` 700, `by_stripped_prefix` 732,
   `by_semantic_neighbours` 906, `import_only_refusal` 1006, `by_file_name` 1061,
   `by_position` 439-456, `by_name` 1129-1140, `by_symbol_id` 1145-1154, `handle` 1160-1196
   -> `answer` returning `ToolAnswer<DefinitionNode>`.
6. `not_indexed.rs`: `refusal` 197-200 and `miss` 205-214 return `Refusal`; `Refusal` struct
   (191-195) `pub(crate)` for `render`.
7. `answer.rs`: `Summary` owned (D3), `respond` 52-86 returns `EdgeAnswer::Summary`; add
   `EdgeAnswer`.
8. `find_references.rs`: `ReferencePage` (82), `ReferenceDisclosures` (150) owned;
   `ReferenceParts::response` 239-297 returns owned; `handle` 304-419 -> `answer`.
9. `find_callers_callees.rs`: `CallerPage` (174), `CalleePage` (370), `CallerDisclosures` (410),
   `CalleeDisclosures` (430) owned; `CallerParts::response` 465-530, `CalleeParts::response`
   675-716; `handle_callers` 537-659, `handle_callees` 721-808 -> typed.
10. `find_implementations.rs`: `ImplementationPage` (79) owned; `handle` 166-242, `from_root`
    454-489, `continued` 506-543, `dispatch` 553-599 typed; add `ImplementationAnswer`.
11. `mod.rs` tool methods `find_definition` 670-686, `find_references` 692-704, `find_callers`
    707-719, `find_callees` 722-734, `find_implementations` 740-754: call `query::*_off_worker`
    then `response_bound::render`.
12. `plugin_check/expectations.rs` bridge only if needed to compile (S4 does the real change):
    the five call sites `render` then the old `tool_outcome`.
13. Test helpers (D6): `Rendered` trait in a `#[cfg(test)]` spot (`query.rs` `pub(crate) mod
    test_support`), the helpers in `find_references.rs` 450/458, `find_callers_callees.rs`
    839/847, `anchor.rs` 178, `member_name_collision_tests.rs` 154, `unlinked_tests.rs` 118,
    `find_implementations/tests.rs` 26/34, `find_definition/tests.rs` 41/49, `host_tests.rs` 150,
    `not_indexed.rs` 256/267 take `&impl Rendered`.

Read for context: `response_bound.rs` `bound_page_in_response` 148-161, `fit_budget_within`
130-140, `bound_defines_page` 298-322; `find_references.rs` `list_references` 178-218;
`session_hints.rs` (what `send` spends); `context.rs` 1-50.

### 3.2 S4 (sonnet, this map): the other three tools, the server, `plugin_check`

1. `get_file_outline.rs`: `OutlinePage` (95) owned; `Outline::body` 112-119; `handle` 156-188
   -> `answer`.
2. `get_dependencies.rs`: `from_root` 350-352, `from_root_reporting` 358-382, `from_file`
   408-443, `incoming_from_file` 495-518, `from_module` 732-750, `continued` 755-778, `handle`
   788-814 -> `ToolAnswer<DependencyWalk>`; tests' shadow helpers `get_dependencies/tests.rs`
   26-38 and `json_body`/`error_text` 52/60 (D6).
3. `search_code.rs`: `handle` 295-389 -> `ToolAnswer<SearchAnswer>` (the 384-387 two-block
   branch becomes `SearchAnswer { note: Some(..) }`); `handle_off_worker` 275-283; helpers
   646/654.
4. `mod.rs`: `walk_failed` 187-191, `wait_for_index` 450-479, `prepare` 273-297,
   `wait_for_embeddings` 539-575 -> `Refusal`; `get_file_outline` 760-771, `get_dependencies`
   777-790, `search_code` 796-809 -> `query::*` + `render`.
5. `tool_result.rs`: delete `success`, `error` (10-16) once nothing calls them.
6. `cli/plugin_check/expectations.rs`: `SymbolTool::call` 1046-1071, `call_definition`
   1077-1080, `dependency_walk` 1229-1243, `resolve_definition` 1511-1533 (retry 1523),
   `tool_outcome` 1794-1806 (D4), imports 621-640, module doc 6-22;
   `expectations/tests.rs` 479-506 rewritten on `ToolAnswer` (same three cases: refusal,
   answer, protocol error).

**Merge contact GM-711** (`PluginCheck::from_manifest`, R-24): it changes how
`plugin_check` reads the manifest, i.e. `cli/plugin_check/mod.rs` (incl.
`expectations_section` 411-479, which builds `QueryCtx` from the manifest) and likely
`checks.rs`/`session.rs`. GM-724 does **not** edit `mod.rs` or `expectations_section`; its
`plugin_check` diff is confined to `expectations.rs` 621-640, 1046-1080, 1229-1243,
1511-1533, 1794-1806 and `expectations/tests.rs` 479-506. Whichever merges second rebases
`use` lines only.

## 4. What does not change

Tool parameters, schemas and descriptions; `response_bound` constants and cut helpers;
`QueryCtx` fields; `front.rs`; `daemon/client.rs`; every expected string in tests.

## 5. Behaviours to pin (tests slice) and candidate controls

### 5.1 Pins

1. **Wire bytes per shape, captured before the refactor** (S2, below): a new
   `core/src/mcp/wire_golden_tests.rs` renders each shape on one small fixture through the
   handler entry and compares the full `CallToolResult` (`is_error`, every block's exact text)
   to `core/src/mcp/testdata/wire_golden/<shape>.txt`. Shapes (16): definition node;
   definition by position; definition ambiguous page; file-name page; text refusal (unknown
   id); references rows (cut, with `total` + cursor); references `files`; references `count`;
   callers rows; callees rows; implementations direct; implementations transitive;
   dependencies walk; outline page (cut at `OUTLINE_MAX_RESPONSE_BYTES`); `not_indexed`
   refusal (outline miss on an uncovered language); search_code partial (note + page, fake
   pipeline as in `search_code_rerank_tests.rs`). Generated once with an env switch
   (`G_MESH_UPDATE_WIRE_GOLDEN=1`, the pattern `search_code_rerank_tests.rs` 161-170 uses), on
   the **unchanged** code, committed before S3.
2. `plugin_check` reads typed answers: a `NotIndexed` refusal reaches `ToolOutcome::Refusal`
   as `message()`, not as JSON text (only a typed read can tell them apart).
3. `plugin_check` source carries no `serde_json::from_str`, `CallToolResult` or `ContentBlock`
   (a grep ratchet test in `expectations/tests.rs`, `include_str!("../expectations.rs")`).
4. Hints are spent only on the sent answer after D3 (existing `response_bound_tests.rs`
   438-area tests cover it; confirm they still fail under C7).
5. Server early answers stay `isError: true` text (existing `tests/it/lazy_activation.rs`
   398).

### 5.2 Candidate controls (revert = edit in a throwaway worktree; binary)

| # | Control | Expected failure | Binary |
|---|---|---|---|
| C1 | `EdgeAnswer`: drop `#[serde(untagged)]` (externally tagged `{"Rows":..}`) | wire golden (refs/callers/callees), `answer_tests` | `--lib` |
| C2 | `ImplementationAnswer`: drop `untagged` | wire golden (impl direct/transitive), `find_implementations/tests.rs` 627 | `--lib` |
| C3 | `render`: `Candidates` as `CallToolResult::error` | wire golden (ambiguous, file-name), `find_callers_callees.rs` 1390 | `--lib` |
| C4 | `render`: put `lead_note` after the JSON block | wire golden (search partial), `search_code_wait_tests.rs` 166 | `--lib` |
| C5 | `render`: `Refusal::NotIndexed` as `ContentBlock::text(error)` | wire golden (not_indexed), `not_indexed.rs` 412 | `--lib` |
| C6 | `tool_outcome`: `Refused(r)` -> `Answer(Value::String(..))` | pin 2, `tests/it/plugin_check.rs` 1154 | `--lib` + `--test it` (one build group) |
| C7 | a `parts.response(.., true)` in a measuring closure (hints spent while measuring) | `response_bound_tests` / `session_hints` tests | `--lib` |
| C8 | `walk_failed` rendered as `Page`-like success | `tests/it/lazy_activation.rs` 398 | `--test it` |

C1-C5 and C7 share one `--lib` build each in turn; C6/C8 group the `--test it` builds.

## 6. Proposed re-slicing

Current draft: S2 code (opus), S3 review, S4 tests, S5 verify. Proposed:

| Slice | Model | Work |
|---|---|---|
| S2 tests-first | sonnet | wire goldens (5.1 pin 1) on unchanged code; `--lib` filter for the new file only; commit |
| S3 code | opus | 3.1: foundation (`query.rs`, `render`, generic `resolve_lazily`) + the five anchored handlers + their tool methods + test helpers |
| S4 code | sonnet | 3.2: outline, dependencies, search_code, `mod.rs` early answers, `plugin_check`, delete `tool_result::{success,error}` |
| S5 review | opus | diff vs this note; byte-risk points: field order of owned structs, `send` once, `untagged` |
| S6 tests | sonnet | pins 2-3, controls C1-C8 chosen/ran per the standing rules |
| S7 verify | opus | build, `--lib` + `--test it` suite of `-p g-mesh`, controls in a throwaway worktree, 5x repeat not needed (no new timing/threads) |

Why S2 first: no existing test pins raw bytes per shape (0.3), and the acceptance is "byte-
identical"; a golden written after the refactor would pin the new output, not the old.
Why S3/S4 split: the anchor couples five handlers (one opus slice), the rest are mechanical
with this map, and the build stays green between them (D5). Alternative: one opus code slice
(S3+S4) if fewer handoffs matter more than model cost.

## 7. g-mesh calls behind this note (project `g-mesh`)

- `find_definition tool_outcome` -> `expectations.rs` 1794-1806 source.
- `find_callers cli::plugin_check::expectations::tool_outcome` -> `SymbolTool::call`,
  `call_definition`, `dependency_walk`, `resolve_definition`, test
  `a_tool_level_refusal_is_a_refusal_and_a_protocol_error_is_not` (complete, `hasMore: false`).
- `find_references ...::ToolOutcome` -> five sites in `expectations.rs` (complete).
- `find_references ...::EvalContext` -> 13 in `expectations.rs`, `expectations_section` in
  `plugin_check/mod.rs` (complete).
- `find_definition cli::plugin_check::expectations_section` -> `QueryCtx` construction 465-477.
- `find_references mcp::context::QueryCtx answer:files` -> 22 files, 88 refs.
- `find_definition mcp::Answer` + `find_references ... answer:count` -> the parameter enum, 15
  refs (D1 naming).
- `find_references CallToolResult` -> no node (external rmcp type, not indexed; g-mesh
  answered with semantic neighbours). Fallback: grep for `CallToolResult`/`ContentBlock`/
  `success(`/`error(` over `core/src/mcp`, `core/src/cli`, `core/src/daemon`, `core/tests`
  (section 0 lists the results).
