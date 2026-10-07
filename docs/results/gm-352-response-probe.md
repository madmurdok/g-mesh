# GM-352 response probe: three builds, five corpora, one fixture

Part 1 of the GM-352 measurement plan (design note Q4, owner decisions D4/D5):
the same tool calls sent to three builds, responses compared byte for byte, and
every distinct declaration name swept through `find_definition` to count how
many ambiguous queries still need a second call. No model is involved; $0.

## Builds

| arm | ref | `serverInfo.version` | role |
|---|---|---|---|
| B0 | `2bc060a` (`main`, release 3.21.0) | 3.21.0 | before the batch |
| B1 | `c545645` (release-4.0.0 before GM-352 code) | 4.0.0 | the batch without GM-352 |
| A | `eb27d29` (GM-352 branch tip) | 4.0.0 | under test |

Each arm was a detached worktree, `npm ci` in `plugins/typescript` and
`plugins/python` (plus `npm run build` of the TS plugin on B0), then
`cargo build --workspace --release`. Versions were read from the
`initialize` response of every run, not assumed.

## Method

- A ~100-line Python driver speaks JSON-RPC to `g-mesh mcp-shim` (cwd = a
  fresh `git clone` of the corpus per arm, `G_MESH_HOME` = a separate directory
  per arm, so no index is shared). It records `tools/list`, then each call's
  result text, its byte length and sha256. Any response under 3,000 B naming
  "is still being built" is retried, never recorded.
- Corpora at the bench's pinned revisions: gin `73726dc` (Go), ripgrep
  `e89fff8` (Rust), requests `6e83187` (Python), task-tracker-mcp `35237c8` and
  excalidraw `1acf66e` (TypeScript). All five, so all four languages are
  measured.
- Fixed queries: 30-41 per corpus (186 total): `find_definition` of 4-7 names
  (some ambiguous by design: `Binding`, `ServeHTTP`, `RegexMatcher`, `matched`,
  `search_path`, `send`, `close`, `prepare`, `clamp`, `exportToSvg`,
  `mutateElement`), the same by `symbol_id`, and for 2-3 high-fan-out anchors
  each of `find_references` and `find_callers` in five variants (default,
  `limit:200`, `limit:0`, `answer:files`, `answer:count`), plus
  `find_callees`, `find_implementations`, `get_file_outline`,
  `get_dependencies` (both directions) and `search_code`. B0/B1 do not know
  `answer` or `find_definition`'s `symbol_id`; they get the call anyway, which
  is what an agent would send them.
- Ambiguity sweep: every distinct `name` in `nodes` excluding `File`/`Module`
  and pending placeholders (gin 1,380, ripgrep 1,840, requests 795,
  task-tracker-mcp 214, excalidraw 4,084). The name sets are identical in all
  three builds' indexes. Per query: outcome, candidates, bytes, and
  `calls_to_definition_text` = 1 for a single answer or an ambiguous page whose
  every candidate carries `source` with no `hasMore`; 2 otherwise.
- Indexing strictly sequential, one corpus and one arm at a time.

### A probe that measured the wrong state, and the A/A that rules it out

The first (cold) pass recorded every response right after the bulk index
finished, before the semantic pass. Those responses carry
`provenance.semanticTier: "absent"` and miss receiver calls entirely (gin
`find_callers Abort`: 284 B, 0 rows cold; 1,443 B, 6 rows warm). The cold
pass is discarded. The figures below are the **warm** pass, run against the
same persisted indexes after the sweep, and a **second warm pass** was diffed
against it: **all 627 fixed responses byte-identical** across the two passes
except `search_code` in ripgrep (3 arms) and excalidraw (B0 only), whose
embedding state was still moving. `search_code` is therefore excluded from
attribution. The sweep was run twice (cold and warm): 24,939 responses, 0
differ.

## Control (fixture)

The design note's Python fixture (`pkg/core.py` defines `target` and a
same-file caller, `pkg/m00..m29.py` import and call it, `other/core.py`
defines a second `target`):

| call | B0 | B1 | A |
|---|---|---|---|
| `find_references target` default | 4,958 B, 10 Function + 10 File | 5,119 B, 10 Function + 10 File | **5,588 B, 20 Function**, `total` |
| `find_definition target` | 565 B, 2 candidates, no source | 565 B, no source | **731 B, 2 candidates, both with source** |
| `find_definition symbol_id=<id>` | 69 B error | 69 B error | 307 B, the definition |

Both control conditions hold, so the probe is live. (B0→B1's +161 B on the
default page is the 4.0 `provenance` hint present only while the semantic pass
runs; it is 0 B in the warm pass on every corpus.)

## Schema cost (`tools/list`, per arm)

| | B0 | B1 | A |
|---|---|---|---|
| `tools/list` (compact JSON) | 10,464 B | 10,464 B | **10,964 B (+500)** |
| `initialize` instructions (gin; varies by corpus ±70 B) | 1,762 B | 1,267 B | 1,267 B |

Per tool, B1→A: `find_references`, `find_callers`, `find_callees` +139 B each
(`answer`), `find_definition` +83 B (`symbol_id`); the other four unchanged.
Net B0→A per session: +5 B (the 4.0 instructions shrink by ~495 B).

## Each adopted mechanism, with bytes either side (warm, B1 → A)

| mechanism | response | B1 | A |
|---|---|---|---|
| 3-level locality ranking | fixture `find_references` default | 10 Function + 10 File rows, 5,119 B | 20 Function rows, 5,588 B |
| ranking | excalidraw `find_references getNonDeletedElements` default | 17 Function, 3 Variable, 6,680 B | 18 Function, 1 Type, 1 Variable, 6,803 B |
| ranking | ripgrep `find_references Searcher` default | 4 File + 16 Function, 6,277 B | 3 File + 16 Function + 1 Type, 6,461 B |
| `answer: files` | gin `find_references Context` | page of 20, 5,851 B | 1,445 B (`total` 321) |
| `answer: count` | gin `find_references Context` | page of 20, 5,851 B | 613 B |
| `answer: count` | requests `find_references Session` | 5,506 B | 190 B |
| `total` on truncated pages | gin `find_callers New` default | 5,914 B, no total | 5,903 B, `total: 138` |
| `find_definition symbol_id` | gin `Render` by id | 69 B error | 391 B, the definition |
| candidate lines + source ≤3 | gin `Binding` (2 candidates) | 599 B, no source | 867 B, 2 sources |
| candidate lines (>3, no source) | requests `send` (5 candidates) | 1,601 B | 1,753 B |
| whole-response byte bound | gin `find_references Context limit:200` | 54 rows, 13,044 B, `hasMore` | 86 rows, 19,928 B, `hasMore` |
| whole-response byte bound | gin `find_callers New limit:200` | 46 rows, 10,801 B | 94 rows, 19,928 B |

Note on the byte bound: it bounds the whole response at 20,000 B rather than
reserving for side fields, so a `limit:200` page now fills ~19.9 KB where B1
stopped at 11-13 KB. Larger but within the same ceiling; more rows per call.

## Bytes per tool, fixed queries (warm)

Sum over the queries of that kind; B0 and B1 are byte-identical on gin,
ripgrep and requests, so the 3.21→4.0 index changes nothing in these
responses for Go/Rust/Python. The TS port shows in excalidraw.

| corpus | `find_references` default (B1 → A) | `find_callers` default | `find_definition` by name | refs `answer:count` vs B1 default | total, B0 / B1 / A (excl. by-id) |
|---|---|---|---|---|---|
| gin (Go) | 11,383 → 11,531 | 7,357 → 7,346 | 9,774 → 11,284 | 11,244 → 1,340 | 120,986 / 120,986 / 112,712 |
| ripgrep (Rust) | 9,972 → 10,156 | 2,068 → 2,116 | 20,939 → 23,396 | 9,713 → 905 | 99,495 / 99,495 / 88,840 |
| requests (Python) | 9,995 → 10,072 | 3,290 → 3,165 | 9,777 → 11,494 | 9,736 → 1,030 | 92,038 / 92,038 / 75,893 |
| task-tracker-mcp (TS) | 1,995 → 1,995 | 4,961 → 5,057 | 2,412 → 2,412 | 1,736 → 379 | 48,588 / 48,583 / 39,085 |
| excalidraw (TS) | 12,346 → 12,469 | 10,478 → 10,722 | 4,548 → 8,856 | 12,078 → 955 | 133,967 / 134,358 / 101,104 |

Reading it: a default page costs +0-2% (the `total` field and re-ranked rows);
an ambiguous `find_definition` costs more (+15-95%, the inlined sources); an
`answer:` query costs 8-15% of the page it replaces. The "total" column
is lower in A mostly because `answer:files/count` queries return the short
form, whereas B0/B1 ignore the parameter and return a full page; it is not a
like-for-like saving on the same question.

## Turns: the ambiguity sweep

| corpus | names | ambiguous | of which ≤3 candidates | second calls B0 | B1 | **A** | ambiguous-page bytes B1 → A (median) |
|---|---|---|---|---|---|---|---|
| gin (Go) | 1,380 | 119 | 87 | 119 | 119 | **32** | 110,858 → 157,156 (707 → 1,109) |
| ripgrep (Rust) | 1,840 | 458 | 287 | 458 | 458 | **171** | 540,930 → 734,446 (856 → 1,272) |
| requests (Python) | 795 | 57 | 41 | 57 | 57 | **17** | 57,017 → 94,854 (859 → 1,492) |
| task-tracker-mcp (TS) | 214 | 9 | 6 | 9 | 9 | **3** | 9,003 → 15,501 (734 → 1,632) |
| excalidraw (TS) | 4,084 | 182 | 150 | 182 | 182 | **32** | 170,652 → 306,480 (751 → 1,583) |

Second calls fall 63-82% per corpus (Go -73%, Rust -63%, Python -70%, TS
-67%/-82%). Single answers are byte-identical B1→A (0 of 7,488 changed).

What a saved call buys against what the page costs, per corpus: the median
growth of a sourced page is gin +352 B, ripgrep +329 B, requests +754 B,
task-tracker-mcp +933 B, excalidraw +810 B; the median single definition
answer it replaces is 653 / 724 / 752 / 643 / 708 B, plus one turn. So for Go
and Rust the inlined page costs less than the call it saves; for Python and TS
it costs about the same bytes and still saves the turn. Pages with >3
candidates gain only line numbers: +122 to +223 B median each, no turn saved
(gin 32, ripgrep 171, requests 17, task-tracker-mcp 3, excalidraw 32 queries).

Without `symbol_id` on `find_definition`, B0/B1's own ambiguous-page advice
("re-query with the right candidate's `id` as `symbol_id`") fails with a 69 B
deserialization error; the real second call there is `file_path` +
`position`. A accepts the id.

## Findings worth a decision

1. **The ≤3 threshold leaves the 4-candidate bucket on the table in Rust.**
   ripgrep has 44 four-candidate names (gin 16, excalidraw 8, requests 8); at
   ≤4 the Rust second calls would fall from 171 to ~127. Not measured here
   whether a 4-source page stays under the single-answer cost.
2. **One sourced page is not fully sourced.** requests `__version__`: 2
   candidates, the `Module` one has no `source` (its `endLine` is one past the
   file's last line, so the stale-file check refuses the read; fixed
   wording in a149f32, span in a backlog task), but the explanation said "each with its source". Counted as 2 calls above.
3. **`limit:200` pages grew ~50%** under the whole-response bound (more rows
   per call, same 20 KB ceiling). Fewer pages per walk, larger single
   responses; the token sweep should watch any task that pages.

## Machine state and cold index

Builds (sequential): B0 `real 427 / user 107`, B1 `real 363 / user 124`,
A `real 293 / user 125`, load averages 15-130 (another agent's test suite).
Cold probe runs (bulk index + 30-41 calls; `user` 0.06-0.10 s is the driver,
the daemon is a separate process): gin 38/45/45 s, ripgrep 103/106/108 s,
requests 55/53/53 s, task-tracker-mcp 11/22/15 s, excalidraw 71/131/128 s
(B0/B1/A), load 7-20. Not a performance claim; recorded for context only.

## What was not measured

- Tokens and turns end to end (Part 2, the token sweep; D4/D5).
- Whether an agent actually takes the one-call path on a sourced page.
- `search_code` deltas (not stable between two warm passes).
