# GM-523: a byte budget for `get_file_outline`

Status: design (GM-523/S1). No production code is changed by this note.
Branch `feat/GM-523-outline-size-budget`, base `release-4.3.0` (31da724).

## Summary

`get_file_outline` is the one row-paging tool that `MAX_RESPONSE_BYTES`
does not bound, and its tool text tells agents to raise `limit` for a big
file. On today's `bridge.rs` that is one 55,000-byte answer, re-read on
every later turn. Recommendation, three parts:

1. **A byte budget that `limit` cannot lift:** at most 8,000 serialized
   bytes per response. Rows are cut at the budget and none are lost; the cut
   reuses the source-order cursor.
2. **Compact rows by default:** `symbolId, name, kind, startLine, endLine,
   exported`. A new `detail: "full"` parameter returns today's row shape
   (it adds `qualifiedName, startCol, endCol, signature`).
3. **Incompleteness is signalled as on the edge tools:** `hasMore` and
   `nextCursor` as today, plus `total` (the file's whole symbol count),
   present only when `hasMore` is true. This is what `find_references` does.

A depth limit (top-level first) is not recommended (see Option C).

## Today (measured)

### Code

- `core/src/mcp/get_file_outline.rs`. `OutlineSymbol` (lines 26-62) sends
  10 fields per row. `list_outline` (92-108) pages by count through
  `pagination::paginate_defines`. `handle_covered` (118-147) resolves
  `limit` with `pagination::resolve_page_size` (default 20, max 200). No
  byte bound is applied.
- `core/src/graph/pagination.rs`. `MAX_RESPONSE_BYTES = 20_000` (26-45)
  and `bound_page`, `bound_page_within`, `longest_prefix_fitting` and
  `fit_budget` (84-241) cut the edge tools' pages at a byte budget. They
  take `EdgeRow<T>`, whose cursor is an `EdgeRank`, so they cannot be used
  as they are for `DEFINES` pages, whose cursor is the private
  `SourceOrderCursor` (834-839). Because of that, get_file_outline was
  never wired to them.
- `core/src/mcp/mod.rs:838-841`. The tool description says "List the
  top-level symbols a file declares". That is not true today: the rows
  include struct fields, impl methods, class members and functions inside
  a `mod tests`.
- `GetFileOutlineParams` (`mod.rs:1018-1027`). The `limit` doc says "raise
  it for a big file rather than paging via `cursor`". That advice is what
  turns the per-row cost into a 50k answer.

### Sizes

The sizes are computed from the live index rows in the order
`paginate_defines` returns them. Each row is serialized the way
`OutlineSymbol` serializes (compact JSON, the same as `serde_json::to_string`
in `rmcp::ContentBlock::json`), inside the `{results, hasMore, nextCursor}`
envelope, with a 90-character cursor allowed for. The script is in the
appendix. Nothing was timed. Machine load at 20:05 was 91.02 / 164.11 /
277.54 (`uptime`).

Indexes used:
- g-mesh main checkout (`~/.g-mesh/projects/959ade85d9a343b1`), at the same
  code as the branch base.
- excalidraw at `1acf66ed` (`~/.g-mesh/projects/7732a5eb37f2a524`, indexed
  2026-10-07). That is after the Rust TS plugin landed (874cc75,
  2026-10-05), so its rows are what today's TS plugin emits.

Where the corpus lives: the bench clones it to
`$TMPDIR/gmesh-bench-corpora/excalidraw`
(`/private/var/folders/ps/r478n82d1zb82phf7k1dsy6w0000gp/T/gmesh-bench-corpora/excalidraw`,
HEAD `1acf66ed`). `~/Projects/excalidraw` is at the same commit.

**The TS file, re-picked.** `plugins/typescript/src/extract.ts` no longer
exists. The largest TS outline in the excalidraw corpus is
`packages/excalidraw/components/App.tsx`: 13,961 lines, 228 symbols
(1 class, 64 methods, 133 arrow functions, ...). It is the largest by both
line count and outline bytes. The runner-up is `components/icons.tsx`
(212 `const` rows).

All numbers are serialized bytes of the whole response.

| file | symbols | today, default (20 rows) | today, `limit: 200` | today, all rows | bytes/row (full) |
|---|---:|---:|---:|---:|---:|
| `plugins/sdk/src/lsp/bridge.rs` (3,995 lines) | 193 | 4,979 | 54,975 | 54,975 | 285 |
| `excalidraw/components/App.tsx` | 228 | 4,802 | 59,754 | 67,888 (needs 2 calls) | 298 |
| `excalidraw/components/icons.tsx` | 212 | 4,471 | 42,573 | 45,021 | 212 |
| `core/src/graph/pagination.rs` | 131 | 5,699 | 41,434 | 41,434 | 316 |

The 4.0.0 evidence (max 47.7k on `bridge.rs`) is the `limit: 200` column on
an older, smaller `bridge.rs`. The ~10k median comes from agents following
the "raise `limit`" advice, not from the default.

**Finding that affects acceptance criterion 1:** today's *default* call
already fits 8k on both files (4,979 and 4,802 bytes). The criterion as
written is met by doing nothing. What breaks the budget is `limit`. See
owner question Q4.

### Where the bytes go (bridge.rs, all 193 rows)

| row shape | fields | bytes | bytes/row | rows in 8,000 bytes |
|---|---|---:|---:|---:|
| full (today) | id, name, qualifiedName, kind, 4 positions, signature, exported | 54,975 | 285 | 31 |
| no signature | full minus `signature` | 42,221 | 219 | 38 |
| **compact (recommended)** | symbolId, name, kind, startLine, endLine, exported | 26,964 | 140 | **59** |
| compact + nativeKind | compact plus `nativeKind` | 31,323 | 162 | 51 |
| minimal, no id | name, kind, startLine, endLine | 14,825 | 77 | 113 |

For App.tsx the same shapes measure 67,888 / 48,428 / **33,143** / 39,176 /
18,851 bytes, and 34 / 38 / **55** / 48 / 98 rows fit in 8,000 bytes.
Signatures total 9,859 bytes on bridge.rs (median 39, max 254) and 15,860
on App.tsx (max 650). The rest of the full shape's overhead is
`qualifiedName` and the two column fields, keys included.

## Options

### A. Compact rows by default, full rows opt-in (`detail`)

The default rows drop `qualifiedName`, `startCol`, `endCol` and `signature`.
`detail: "full"` returns today's row unchanged.
- Benefit: bytes per row fall by about half (285 to 140), so the same
  budget carries about 2x the rows. The full shape is still one parameter
  away.
- Risk: this alone bounds nothing. `limit: 200` compact on App.tsx is still
  about 29k bytes. An agent that wanted signatures makes a second call, or
  uses `find_definition`, which returns source.

### B. Byte budget per response, paging by cursor

Cut the page at N serialized bytes, the way `bound_page` cuts the edge
tools. `hasMore` and `nextCursor` resume right after the last row sent.
- Benefit: the only option that *guarantees* a bound, for any `limit`,
  file or row shape. It matches the contract the edge tools already have
  (`response_bound_tests.rs`), and no rows are lost.
- Risk: with full rows only 31 rows fit in 8k, so reading a whole big file
  takes 7 calls, and each call re-pays the cached prefix. `limit` turns
  into "at most" rather than "this many", and the description has to say
  so.

### C. Depth limit (top-level first, members on request)

Measured with "a row is a member when another row's range strictly
contains it", the outlines shrink to 80/193 rows (bridge.rs) and 41/228
(App.tsx). Not recommended:
- Rust methods live in `impl` blocks, which are not nodes (unless the type
  is undeclared). The methods are not contained in their struct's range, so
  "top level" keeps 23 methods but hides 74 fields. In TS it hides all 64
  methods of `class App`. The same parameter would mean different things
  per language.
- The rows agents usually look for (methods) are the ones hidden on TS.
- `container` holds the module path, not the parent symbol, so no stored
  column says "member". It would need a containment pass per call or a new
  column.
- Even top-level-only full rows are 23,460 bytes on bridge.rs. A budget
  (B) is still needed.

### D. Combination A + B (recommended)

Compact rows by default, an 8,000-byte budget on every response, and
`detail: "full"` opt-in under the same budget.

| | bridge.rs | App.tsx |
|---|---|---|
| default call, today | 20 rows, 4,979 B | 20 rows, 4,802 B |
| `limit: 200`, today | 193 rows, 54,975 B | 200 rows, 59,754 B |
| default call, D with Q2 = fill to the budget | about 59 rows, at most 8,000 B | about 55 rows, at most 8,000 B |
| default call, D with Q2 = keep 20 | 20 rows, about 2,750 B | 20 rows, about 3,010 B |
| any `limit`, `detail: "full"`, D | about 31 rows, at most 8,000 B | about 34 rows, at most 8,000 B |
| whole file, compact, D | 4 calls, about 27k B in total | 5 calls, about 33k B in total |

## Recommendation

**D: compact by default, `detail: "full"` opt-in, an 8,000-byte budget per
response that `limit` cannot lift, and `total` when `hasMore`.**

- **Budget: 8,000 bytes** of serialized response (the `wire_len` unit),
  in a new constant `pagination::OUTLINE_MAX_RESPONSE_BYTES`. Why 8,000:
  it is the criterion's own example, the size `FILE_TALLY_MAX_BYTES`
  already uses, and well under `MAX_RESPONSE_BYTES` (20,000). It is about
  2 to 2.5k tokens per call, at most.
- **Cut rule.** Measure the whole response, not only the rows (the
  `fit_budget` / `bound_page_in_response` pattern), so `total` and the
  cursor count too. Always send at least one row, even when that one row is
  over the budget (the `longest_prefix_fitting` floor). An empty page with
  `hasMore: true` would be a paging loop that never ends. The cursor is a
  `SourceOrderCursor` built from the last row *sent*.
- **`detail`**: `"compact"` (default) or `"full"`. The cursor stays
  position-only, so an agent can switch `detail` between pages.
- **`total`**: the exact count of `DEFINES` rows of the file
  (`SELECT COUNT(*) ... WHERE fromId=? AND kind='DEFINES'`, served by
  `idx_edges_fromId`). Sent only when `hasMore` is true, and placed before
  `hasMore`, as in `find_references`.
- **No new `truncated` / `truncatedBy` field.** The edge tools signal a
  byte cut on a row page with `hasMore` + `nextCursor` (+ `total`).
  `truncatedBy: "responseSize"` exists only on the *walk* tools
  (`find_implementations` transitive, `get_dependencies`), which resume with
  a `resumeToken`. Outline pages resume with a cursor, so they follow the
  row-page convention. "How to get the rest" is the cursor, as on every
  paged tool. The tool description says it once.
- **Default `limit`**: owner question Q2. The recommendation is to fill to
  the budget (default 200), so one call returns as many rows as fit.
- **Wording fixes** in the same edit: the tool description says "the
  symbols a file declares, members included", not "top-level". The `limit`
  doc drops the "raise it for a big file" advice and says each response is
  capped at about 8k bytes, so page with `cursor`.

### For GM-504 (tool-answer-guarantees doc)

Hand GM-504 these statements for the `get_file_outline` section:
1. Every response is at most 8,000 serialized bytes
   (`OUTLINE_MAX_RESPONSE_BYTES`), whatever `limit` and `detail` are, with
   one exception: a single row that alone is over the budget is still sent,
   alone.
2. Rows come in source order (`startLine`, `startCol`, `id`). Following
   `nextCursor` to the end returns every `DEFINES` row of the file exactly
   once. The byte cut never drops a row.
3. `hasMore: true` means rows remain. `total` (present only then) is the
   exact number of rows in the whole file, not the number left.
4. Compact rows carry `symbolId, name, kind, startLine, endLine, exported`.
   `detail: "full"` adds `qualifiedName, startCol, endCol, signature` (the
   pre-GM-523 shape). `exported` keeps its GM-369 meaning.
5. `limit` is an upper bound on rows. A page can hold fewer rows than
   `limit` while `hasMore` is true.

## Edit map

### Change

| file | symbol (1-based lines) | change |
|---|---|---|
| `core/src/graph/pagination.rs` | new const next to `MAX_RESPONSE_BYTES` (26-45) | `pub const OUTLINE_MAX_RESPONSE_BYTES: usize = 8_000;` with the measured rationale |
| `core/src/graph/pagination.rs` | `SourceOrderCursor` (834-839), `paginate_defines` (841-898) | new `pub fn bound_defines_page<T: Serialize>(nodes: Vec<NodeRecord>, has_more, next_cursor, render: impl Fn(NodeRecord) -> T, response_len: impl FnMut(&[T], bool, Option<&str>) -> usize, budget) -> Page<T>`, or an equivalent that keeps `SourceOrderCursor` private. It renders, cuts with `longest_prefix_fitting` under a whole-response measure (`fit_budget` generalised to take the ceiling, or a copy with the ceiling as a parameter), and re-encodes the cursor from the last node kept. Plus `pub fn count_defines(conn, file_node_id) -> Result<usize>`. |
| `core/src/mcp/get_file_outline.rs` | `OutlineSymbol` (26-62), `From<NodeRecord>` (64-78) | make `qualified_name`, `start_col`, `end_col` and `signature` `Option`s with `skip_serializing_if`, filled only by a full render: `fn render(n: NodeRecord, detail: OutlineDetail) -> OutlineSymbol`. A full row must stay byte-identical to today's, including `"signature": null` for a missing signature. A compact row omits the key. |
| `core/src/mcp/get_file_outline.rs` | `OutlinePage` (80-90) | add `#[serde(skip_serializing_if = "Option::is_none")] total: Option<usize>` before `has_more` |
| `core/src/mcp/get_file_outline.rs` | `list_outline` (92-108), `handle_covered` (118-147) | pass `detail` and the budget through. Apply `bound_defines_page`. Compute `total` only when `has_more`, measured as `widest_total` while the cut is searched. Default `limit` per Q2. |
| `core/src/mcp/mod.rs` | `get_file_outline` tool attr (838-841) | description: "members included", and one clause on the budget plus cursor. Keep within the tools/list ceiling. |
| `core/src/mcp/mod.rs` | `GetFileOutlineParams` (1018-1027) | add `detail: Option<OutlineDetail>` (a `#[serde(rename_all = "lowercase")]` enum `Compact`/`Full` with `JsonSchema`). Rewrite the `limit` doc. |
| `core/tests/mcp_e2e.rs` | line 48 | `("get_file_outline", &["file_path", "cursor", "limit", "detail"])` |
| `core/tests/overload_call_resolution.rs` | outline call (202-207) | add `"detail": "full"`. Line 217 reads `signature` from the outline row. |
| `core/src/mcp/get_file_outline.rs` tests | `a_custom_limit_returns_a_large_file_in_one_call` (289-315), `omitting_limit_keeps_the_default_page_size` (317-347), `an_oversized_limit_is_clamped_to_the_ceiling` (349-377) | the clamp test's 205 rows are about 75 B each compact, about 15k in total, so the budget now cuts the page before the count clamp does. Drive the clamp through `list_outline` with an explicit large budget, or assert `<= MAX_PAGE_SIZE` plus the budget. Rewrite the default-size test per Q2. |
| `README.md` | "Tools exposed" (~659-663) | one sentence: outline pages are compact, about 8k bytes each, `detail: "full"` for signatures |
| `docs/architecture/g-mesh-v1.md:1069`, `REQUIREMENTS.md:459` | tool tables | note `detail` and the budget (doc-only) |

No change is needed in `core/src/cli/agent_instructions.rs:62` or README
line 548. They mention `get_file_outline` only as the call that triggers
indexing. `core/src/mcp/instructions*` do not mention the tool. Other
`core/tests/*` call it only as a cheap index-needing call or read `name`
only. The rows they read, by grep for `["signature"]`,
`["qualifiedName"]`, `["startCol"]` and `["endCol"]`, are unaffected,
except `overload_call_resolution.rs:217`. The g-mesh-bench harness
(`harness/search-latency.ts`, `cold-start.ts`,
`scripts/probeLanguageTiers.ts`, `analyzeToolUseLog.ts`) names the tool but
reads no row fields (grep).

### Read for context

- `pagination.rs`: `longest_prefix_fitting` (84-114), `wire_len` (131-134),
  `fit_budget` (136-161), `bound_page_in_response` (163-181),
  `widest_total` (183-187), `bound_page_within` (201-241).
- `find_references.rs`: the `total` field (106-109) and how it is measured
  while the cut is searched (~425-447).
- `answer_tests.rs:212` (`total` only on a truncated page) and
  `response_bound_tests.rs`, the model for the new tests.
- `mod.rs:136-144` `TOOLS_LIST_BYTE_CEILING` (11,800) and
  `tools_list_tests.rs::the_tools_list_fits_its_ceiling`. The new parameter
  must fit. If it does not fit after shortening the `limit` doc, that is an
  owner decision, not a quiet raise.

### Where the call graph came from

- `find_references GetFileOutlineParams`: `GMeshMcpServer::get_file_outline`
  (mod.rs), `get_file_outline::handle`, `handle_covered` (complete, no
  `hasMore`).
- `find_callers get_file_outline::handle_covered`: `handle`,
  `tests::outline_of`, `GMeshMcpServer::get_file_outline` (complete).
- `find_definition paginate_defines`, `resolve_page_size`,
  `tool_result::success`: read from the returned source.
- grep, for non-code and single known strings: `truncated`,
  `responseSize`, `MAX_RESPONSE_BYTES`, the `get_file_outline` mentions in
  tests, docs and bench, and the row-field reads in `core/tests`.

## Behaviours for the tests slice (one control each)

These are unit tests in `get_file_outline.rs` over an in-memory index,
except B6. The fixture is a file with N rows and long signatures (a
generated 200-char signature per row).

1. **B1. The budget holds for any `limit` / `detail`.** With 200 rows,
   `limit: 200`, `detail` compact and full: `wire_len(body) <= 8_000` and
   `hasMore: true`. *Control:* skip `bound_defines_page` (send the SQL page
   as it is). Expect a body far over 8,000 bytes.
2. **B2. No row lost or repeated across byte-cut pages.** Follow
   `nextCursor` to the end, switching `detail` between pages. The
   concatenated `symbolId`s equal the N ids in source order. *Control:*
   keep the SQL page's `next_cursor` after the cut, instead of the cursor
   of the last row sent. Rows are skipped.
3. **B3. A byte cut sets `hasMore` even when `limit` covers the file.**
   N=120, `limit: 200`. The SQL `has_more` is false, but the page is cut,
   so `hasMore: true`, `nextCursor` is set and `total == 120`. *Control:*
   pass the SQL `has_more` through unchanged.
4. **B4. `total` only on an incomplete page.** 3 rows: no `total` key.
   *Control:* always emit `total`.
5. **B5. Compact by default, full on request.** The default row's key set
   is exactly `{symbolId,name,kind,startLine,endLine,exported}`.
   `detail: "full"` gives exactly today's 10 keys, including
   `"signature": null` for a row without one. *Control:* render full
   always (or drop `signature` from the full renderer). One of the two
   assertions fails.
6. **B6. One oversized row is still sent.** A single row with a 9,000-char
   signature, `detail: "full"`, plus 2 more rows. Page 1 holds exactly that
   row, with `hasMore: true`, and page 2 holds the other two. *Control:*
   allow a cut to zero rows (a `truncate_to_bytes`-style floor). Page 1 is
   empty with `hasMore: true`.
7. **B7. Default page bound (per Q2).** If Q2 is "fill": 25 short rows and
   no `limit` give all 25 rows and no `hasMore`. If Q2 is "keep 20": the
   existing test `omitting_limit_keeps_the_default_page_size` stays.
   *Control:* revert the default.
8. **B8 (e2e schema).** `mcp_e2e.rs` parameter list includes `detail`, and
   `the_tools_list_fits_its_ceiling` passes. *Control:* none of its own.
   These are existing guard tests, listed so the tests slice does not
   forget them. Counted outside the 7 controls above.

That is seven controls (B1 to B7). Revert the code, never the test. B1 to
B7 are in one test binary, so one control build each in the throwaway
worktree.

## Must confirm (verify / measure slice)

- **Before/after on the real tool** (criterion 4). On a fresh index of the
  branch, call `get_file_outline` through MCP on `bridge.rs` and App.tsx:
  default; `limit: 200`; `detail: "full", limit: 200`. Record the response
  bytes and rows next to the "today" table above. The appendix script is
  the cheap pre-check. It needs no build, only the index DB.
- `tools/list` stays at or under 11,800 bytes with `detail` added.
- Whether the guidance prefix / "answered from project" line counts toward
  what the client sees. It is outside `success`'s JSON, so the budget does
  not include it. Say so in the GM-504 text if it matters.
- `count_defines` costs one indexed query, and only on cut pages.

## Risks

- **More calls for whole-file reads.** At 8k per page, reading all of
  App.tsx takes 5 calls instead of 2. Each call re-pays the cached prefix.
  This is acceptable because a whole-file outline is rarely needed, and the
  bytes that drop out are re-read on every later turn. It is still a real
  trade-off: an agent that pages to the end pays about 33k across 5 turns
  instead of 68k in 2.
- **Signatures need a second call** (`detail: "full"`, or
  `find_definition`, which returns source). An agent that scanned
  signatures to choose a function loses that on the first call.
- **Output shape change** for any external consumer that reads
  `signature` / `qualifiedName` from outline rows. In-repo, only
  `overload_call_resolution.rs` does. The bench reads no row fields.
- **`limit` semantics shift** from "this many" to "at most this many".
  An agent that sets `limit: 200` and gets 59 rows must follow `hasMore`.
  The description has to say so.
- **tools/list growth** of the new enum parameter, against a ceiling with
  less headroom than the smallest tool's entry.

## Owner questions

### Q1. What should a default outline row contain?

Today every row carries 10 fields: `symbolId, name, qualifiedName, kind,
startLine, startCol, endLine, endCol, signature, exported`. That is about
285 bytes per row on `bridge.rs`.

Example: the `Budgets.request` field row today includes
`"qualifiedName":"lsp::bridge::Budgets.request"`, both columns, and
`"signature":"pub request: Duration"`.

- **(a) Compact by default, `detail: "full"` opt-in (Recommended).** Rows
  keep `symbolId, name, kind, startLine, endLine, exported` (140 B per
  row). 59 rows of bridge.rs fit in 8k instead of 31.
  Benefit: twice the rows per budget. Risk: an agent that wants signatures
  makes one more call.
- **(b) Drop only `signature` by default (`include_signatures` opt-in).**
  219 B per row, 38 rows per 8k. Benefit: `qualifiedName` and columns
  stay. Risk: about 60% more bytes per row than (a), for fields an
  in-file outline rarely needs.
- **(c) Keep today's full rows, budget only.** 31 rows per 8k, and 7 calls
  for all of bridge.rs. Benefit: no shape change. Risk: the most calls for
  the same information.

### Q2. When `limit` is not given, how many rows does a call return?

Today the default is 20 rows: about 5k bytes in full rows, about 2.8k
compact. Agents are told to raise `limit` for big files.

Example: a default call on bridge.rs today returns 20 of 193 symbols.

- **(a) Fill to the budget: default `limit` becomes 200, so the 8k budget
  is the real bound (Recommended).** A default call returns about 59
  compact rows of bridge.rs. Benefit: one call answers most files in full
  (every file under about 55 symbols), and agents no longer need to know
  about `limit`. Risk: a default call on a big file costs up to 8k instead
  of about 3k. Small files are unchanged.
- **(b) Keep the default at 20.** Benefit: the smallest default answer
  (about 2.8k). Risk: big files need 3x more calls to read through, and
  agents keep raising `limit`, which now only reaches the budget anyway.

### Q3. Should compact rows keep `symbolId`?

`symbolId` is 32 hex characters plus its key, about 46 bytes, a third of a
compact row. It lets an agent anchor `find_callers` and the other tools on
an exact symbol, with no name ambiguity.

Example: without it, the agent calls `find_callers symbol_name: "handle"`
and may get a ranked list of candidates, because several files declare
`handle`.

- **(a) Keep it (Recommended).** Benefit: exact anchoring and unchanged
  agent habits. Risk: 59 rows per 8k instead of about 110.
- **(b) Drop it from compact (it is still in `detail: "full"`).** Benefit:
  about 2x more rows per page. Risk: more ambiguous-name round trips, and
  the outline-then-`symbol_id` habit that the tool text teaches breaks.

### Q4. Restate acceptance criterion 1?

Today criterion 1 reads: "default outline of bridge.rs and of the largest
TS file in a corpus fits a stated budget (e.g. 8k chars)". Measured, it is
already met: the default call is 4,979 B (bridge.rs) and 4,802 B
(App.tsx), because the default is 20 rows. The 47.7k/55k answers come
from `limit: 200`. The TS file it named (`plugins/typescript/src/extract.ts`)
no longer exists.

Proposed wording: "Every `get_file_outline` response is at most 8,000
serialized bytes, for any `limit` and `detail`. Measured on
`plugins/sdk/src/lsp/bridge.rs` and excalidraw@1acf66e
`packages/excalidraw/components/App.tsx`, the largest TS outline in that
corpus at 228 symbols and 67.9k bytes in full rows."

- **(a) Adopt the proposed wording (Recommended).** Benefit: the criterion
  tests the real failure (a raised `limit`) and names a file that exists.
  Risk: none beyond a stricter criterion.
- **(b) Keep the wording, swap only the file.** Benefit: minimal change.
  Risk: the criterion passes on the old code, so it cannot tell the fix
  apart from no fix.
- **(c) A different budget (for example 12,000 bytes).** Benefit: fewer
  calls per file (about 85 compact rows). Risk: up to 1.5x the bytes
  re-read every turn.

## Owner decisions (2026-10-09)

- Q1: "Короткие, detail:\"full\" по запросу (Recommended)" -> compact rows by default, `detail: "full"` opt-in.
- Q2: "Заполнять до лимита 8k (Recommended)" -> default `limit` 200, the 8,000 B budget is the real bound.
- Q3: "Оставить (Recommended)" -> compact rows keep `symbolId`.
- Q4: "Любой ответ ≤ 8000 байт (Recommended)" -> acceptance criterion 1 restated (task amended).

## Appendix: size script (no build needed)

Run `python3 -I gm523_measure.py <index.db> <file>...`. The index DB is
`~/.g-mesh/projects/<hash>/index.db`; find the hash by grepping
`*/project.root`. It reproduces the tables above. It is an estimate of the
wire format: the real cursor length can differ by about 10 bytes.

```python
import json, sqlite3, sys
def rows(db, path):
    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True); c.row_factory = sqlite3.Row
    f = c.execute("select id from nodes where kind='File' and filePath=?", (path,)).fetchone()
    return [dict(r) for r in c.execute("select n.* from edges e join nodes n on n.id=e.toId where e.fromId=? and e.kind='DEFINES' order by n.startLine,n.startCol,n.id", (f[0],))]
def full(r): return {"symbolId":r["id"],"name":r["name"],"qualifiedName":r["qualifiedName"],"kind":r["kind"],"startLine":r["startLine"],"startCol":r["startCol"],"endLine":r["endLine"],"endCol":r["endCol"],"signature":r["signature"],"exported":bool(r["exported"])}
def compact(r): return {"symbolId":r["id"],"name":r["name"],"kind":r["kind"],"startLine":r["startLine"],"endLine":r["endLine"],"exported":bool(r["exported"])}
CUR = '"' + "x"*90 + '"'
def env(items, more): return len(json.dumps({"results":items,"hasMore":more,"nextCursor":None},separators=(',',':'),ensure_ascii=False)) + (len(CUR)-4 if more else 0)
def fit(items, budget):
    n=0
    for k in range(1,len(items)+1):
        if env(items[:k], k<len(items))<=budget: n=k
        else: break
    return n
for p in sys.argv[2:]:
    rs=rows(sys.argv[1],p)
    for nm,fn in [("full",full),("compact",compact)]:
        a=[fn(r) for r in rs]
        print(p, nm, len(a), "default20", env(a[:20],len(a)>20), "limit200", env(a[:200],len(a)>200), "fit8k", fit(a,8000))
```
