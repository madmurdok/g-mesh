# GM-352: rank before truncating, answer-shaped responses, the ambiguity turn

Status: **draft for owner review** (GM-352/S1). Nothing here is decided until the
owner answers "Decisions for the owner" at the end. Code cited at `c545645`
(release-4.0.0 after GM-331).

## Summary

- **Q1, do the reference and caller queries rank before truncating? Yes, but
  only on two coarse keys, and the rest is hash order.** `paginate_edges`
  sorts in SQL before `LIMIT` (`ORDER BY e.resolved DESC, locality ASC, e.id
  ASC`, `core/src/graph/pagination.rs:635-636`), and the byte cut keeps the
  longest prefix of that order (`pagination.rs:147-187`). `locality` only says
  "same file as the anchor or not" (`pagination.rs:623`). Inside the tier that
  holds almost every row (resolved, other file), the order is `e.id`, which is
  a truncated sha256 of `(fromId, kind, toId)` (`ids.rs:102`) (`plugins/sdk/src/ids.rs:155-161`).
  So a newly resolved edge does move up, ahead of the unresolved rows it used
  to sit behind. A newly *found* edge lands at a random spot in the resolved
  tier and pushes a random row off the page. On a small fixture the richer
  index already costs page quality: half of `find_references`' default page
  goes to import lines (10 of 20 rows are `kind: File` import references) and
  only 10 call sites fit.
- **Mechanisms:** adopt (1) a finer ranking, (2) a cheap answer-shaped
  surface (`total`, `limit: 0`, no repeated paths in the excluded tally), and
  part of (3), since `total` gives a single-hop call its notion of "enough".
  Reject (4), provenance as a filter: resolved rows already rank first, and
  dropping unresolved rows breaks the documented contract.
- **Q3, GM-360's ambiguity turn:** the turn is partly self-inflicted.
  `find_definition`'s ambiguous page tells the caller to "re-query with the
  right candidate's `id` as `symbol_id`" (`core/src/mcp/session_hints.rs:67-69`),
  but `find_definition` has no `symbol_id` parameter
  (`core/src/mcp/mod.rs:880-891`). Called with one, it answers "give either
  `symbol_name`, or both `file_path` and `position`" (observed;
  `find_definition.rs:1140-1142`). The candidates carry no line
  (`find_definition.rs:340-372`), so `file_path`+`position` is out of reach
  too. The proposal: answer every reading when there are few candidates, and
  make the advised re-query real.

## Facts this note relies on

| fact | source |
|---|---|
| `paginate_edges` backs `find_references`, `find_callers`, `find_callees` and `find_implementations`. Its callers are `find_references.rs:137`, `find_callers_callees.rs:62` (`list_calls`, both directions) and `find_implementations.rs:117`. `EdgeRow`s are built at `find_references.rs:158`, `find_implementations.rs:139`, `find_callers_callees.rs:402` and `:518` | `grep -rn "paginate_edges(\|EdgeRow {" core/src` (g-mesh MCP failed to connect this session, see the end) |
| SQL order is `resolved DESC, locality ASC, id ASC`, keyset cursor `(resolved, locality, id)`, `LIMIT page_size+1` | `pagination.rs:623-636`, `StructuralCursor` `pagination.rs:223-227` |
| byte cut: longest prefix under `MAX_RESPONSE_BYTES` (20,000) minus reserves; the cursor is re-encoded from the boundary row's `(resolved, locality, edge_id)` | `pagination.rs:44`, `:94`, `:147-187`, `EdgeRow` `:68-73` |
| default page 20, max 200; `limit: 0` is clamped up to 1 | `pagination.rs:10`, `:15`, `:22-23` |
| the `files` tally is computed over the whole edge set, ordered `refs DESC, filePath ASC`, capped at 200, and sent only when `hasMore` or rows repeat files | `pagination.rs:301-372`, `:442-444`, `MAX_FILE_TALLY` `:260` |
| `count_edges` (uncapped) exists | `pagination.rs:382-433` |
| both `find_references` and `find_callers` use `Distinctness::Edges`. One edge per `(from, kind, to[, decl])`, so two calls from one caller are already one row | `find_references.rs:146`, `find_callers_callees.rs:70`, `ids.rs:155` |
| `find_references` asks for `CALLS`+`REFERENCES`+`SUPERTYPE_OF` | `find_references.rs:44` |
| `find_callers` attaches `excludedReferences` (uncapped count + up to 50 files + a 280-byte hint), measured as saving GM-258's follow-up turn | `find_callers_callees.rs:233-316`, `MAX_EXCLUDED_FILE_TALLY` `pagination.rs:279` |
| `edges` has indexes only on `fromId`, `toId`, `linkedFrom`. The anchor's fan-in is fetched by `idx_edges_toId` and sorted in a temp B-tree today, because `locality` is computed | `core/src/storage/schema.rs:294-317` |
| ranking with `rank_expr` exists only for file lookups (`find_files_under`, `find_files_ending_in_dir`) and `find_in_file_named` | `core/src/graph/queries.rs:365`, `:453`, `:526-531`, `:563-568` |
| documented contract: "Structural tool results are ordered `resolved: true` before `resolved: false`, then by locality". Every edge-derived row carries `resolved` | `docs/architecture/g-mesh-v1.md:1072-1077` |
| ambiguous page: `{ambiguous, resolvedBy: nameAmbiguous, explanation, results[{id, qualifiedName, filePath, kind, preview}], hasMore, nextCursor}`, ranked by inbound `REFERENCES`+`CALLS` count, 20 per page. The four anchored tools return the same page in place of their own | `find_definition.rs:148-188`, `:334-372`, `:29` |
| a definition answer carries source up to 80 lines | `find_definition.rs:55-85`, `core/src/mcp/source.rs:42` |

### Observed responses (fixture, `target/debug/g-mesh` 4.0.0)

A Python fixture in the scratchpad: `pkg/core.py` defines `target` and one
same-file caller. `pkg/m00..m29.py` each run `from pkg.core import target` and
call it from `use_NN`. `other/core.py` defines a second `target`. Driven over
`g-mesh mcp-shim` by a 40-line JSON-RPC script, under 2 minutes including the
cold index. Bytes are of the tool result text.

| call | bytes | rows | breakdown |
|---|---|---|---|
| `find_definition target` | 565 | 2 candidates | explanation 183, results 275 |
| `find_definition symbol_id=<id>` | 69 | - | error: "give either `symbol_name`, or both `file_path` and `position`" |
| `find_callers <id>` (default) | 6,000 | 20 | results 3,286 (~164/row), files 963 (31 entries, ~31/entry), excludedReferences 1,269 |
| `find_callers <id> limit:1` | 2,884 | 1 | the cheapest "is it called / which files" answer today |
| `find_callers <id> limit:200` | 6,600 | 31 | no `files` (complete page, one row per file) |
| `find_references <id>` (default) | 4,769 | 20 | **10 Function + 10 File rows**; the File rows are the import lines |
| `find_references <id> limit:200` | 11,440 | 61 | 31 Function + 30 File |

Row order in `find_callers`: the same-file `local_user` first, then `use_07,
use_09, use_03, use_25, use_18, …` (hash order). The 30 excluded references
are the same 30 files the `files` tally already names.

## Q1. Ranking: what exists, what is missing, what to add

**What exists** (above): two keys, then an arbitrary but stable tiebreak. Both
keys are relevance signals: confirmed edges first, and the anchor's own file
first. Nothing below them is.

**Where it bites.** A richer index adds rows mainly to the resolved,
other-file tier, the tier where order is a hash. Two kinds of new rows
compete for the same 20 slots.
- *Same-information rows.* In the fixture each importing file costs two rows,
  a `File`-kind `REFERENCES` (the import) and a symbol-kind `CALLS`, and they
  interleave by hash. Half the default page is imports.
- *More call sites than the page holds.* Which ones survive is decided by
  sha256 digests. It is stable, so it is not noise. It is also not
  relevance: on excalidraw's `pointFrom` the page shows 46 of 81 files, and
  nothing says why those 46.

**Proposed key** (Decision D1, option A):

```sql
ORDER BY e.resolved DESC,
         locality ASC,          -- 0 same file, 1 same directory, 2 elsewhere (was 0/1)
         is_file_row ASC,       -- n.kind = 'File' after symbol rows
         n.filePath ASC, n.startLine ASC,
         e.id ASC
```

- *same directory* is `n.filePath LIKE ?dir || '/%' AND instr(substr(n.filePath,
  ?dirlen), '/') = 0`, with `?dir` computed in Rust from the anchor's path and
  escaped with `queries::escape_like` (`queries.rs:493`, made `pub(crate)`). A
  root-level anchor matches files with no `/`.
- `is_file_row` demotes but never drops. A `File` row is still the only
  evidence of a top-level usage, it stays on later pages, and `files` still
  counts it.
- `filePath, startLine` replaces "hash" with "grouped by file, in reading
  order". Rows from one file become contiguous (one Read per file), the order
  is predictable, and it stays insert-stable for the keyset, because every key
  belongs to the row itself.
- **Cost:** none in complexity. The anchor's fan-in is already read through
  `idx_edges_toId` and sorted, `n` is already joined, and the new keys are
  columns of `n` or a `CASE` over them. No new index. The cursor grows from
  `(resolved, locality, id)` to `(resolved, locality, fileRow, filePath,
  startLine, id)`, about 60 more base64 bytes per `nextCursor`. An old-shape
  cursor is refused by name, as ADR-0013 does for the score cursor (test
  `a_score_cursor_in_the_earlier_json_number_shape_is_refused_by_name`,
  `pagination.rs:1343`).
- **Contract:** `g-mesh-v1.md:1074-1077` stays true ("resolved first, then by
  locality") and gains the remaining keys. The line is edited, not replaced.
- **Fixture effect:** `find_references` default page goes from 10 call sites +
  10 imports to 20 call sites. Bytes go up ~+450 (4,769 to ~5,200), because a
  symbol row (~190 B) is longer than a File row (~146 B). This is a quality
  mechanism. It is not sold as a bytes one.

Rejected for now: **caller centrality** (each row's own inbound edge count, as
`find_definition` ranks candidates, `find_definition.rs:347`). It is closer to
"relevance", but it costs a correlated count per row, and it breaks keyset
stability: a reindex that adds an edge to a caller changes that row's key
between pages and can skip or repeat rows. The guarantee
`pagination_returns_every_row_once_even_with_inserts_between_calls`
(`pagination.rs:987`) would no longer hold. Kept as D1 option B.

Also rejected: **"test files last".** GM-360's note on `find_candidates_by_name`
(`find_definition.rs:222-253`) already refused to encode per-language
test-directory conventions in core, and the same reasoning applies here.

## Q2. The four mechanisms

### 1. Rank before truncating: **adopt** (Q1)

| | before | after |
|---|---|---|
| shape | unchanged | unchanged (field set identical; order and cursor payload differ) |
| fixture `find_references` default | 4,769 B, 10/20 call sites | ~5,200 B, 20/20 call sites |

### 2. Answer-shaped responses: **adopt, the cheap form**

The existing model is `files` (`pagination.rs:286-299`). Three additions, all
of them additive and none of them adding a parameter:

a. **`total`**: the exact number of matching rows over the whole set, from
   `count_edges` (`pagination.rs:382`), present only when `hasMore` (absent
   otherwise, the same "absent, not zero" rule as `files`). +~14 B. It answers
   "how many / is it called" without summing a tally that may be capped, and
   it is the single-hop notion of "enough" (mechanism 3).
b. **`limit: 0` means "the answer without the evidence"**: no rows, and
   `files` + `total` always sent. Today 0 is clamped to 1
   (`pagination.rs:23`). The only schema cost is a few words in the existing
   `limit` doc ("0: counts and files only"). The alternative, a new `answer`
   parameter on `SymbolQueryParams`, shows up in four tool schemas on every
   turn of every session (GM-188: the tax is per turn and mostly prose). That
   is D2 option B.
   - fixture `find_callers limit:0`: 2,884 B (`limit:1` today) to ~1,500 B
     (anchor 141 + files 963 + total + deduped excluded ~330).
   - excalidraw `pointFrom` (task description's own figures): `limit:200`
     ~20 KB (51 rows, still `hasMore`), against ~5 KB for all 81 files.
c. **The excluded tally does not repeat paths `files` or the rows already
   name.** `count` stays exact. `files` inside `excludedReferences` lists only
   the files that hold an excluded usage and no call. GM-258's point (name the
   files so nobody re-asks) still holds, because every file is named once
   somewhere in the response.
   - fixture `find_callers` default: 6,000 B to ~5,060 B (excluded 1,269 to
     ~330). All 30 excluded files were already in `files`.

| response | before | after |
|---|---|---|
| `find_callers` default, fixture | `{anchor, results[20], files[31], hasMore, nextCursor, allUnresolved, hint, excludedReferences{count:30, files[30], hint}}` 6,000 B | `{…, results[20], files[31], total:31, …, excludedReferences{count:30, files[], hint}}` ~5,070 B |
| `find_callers limit:0`, fixture | n/a (clamped to 1: 2,884 B) | `{anchor, results[], files[31], total:31, hasMore:true, …}` ~1,500 B |

`hasMore: true` with an empty `results` and a `nextCursor` stays a correct
statement on `limit: 0`: the evidence exists and is pageable.

### 3. Stop early: **adopt only as `total`, no new walk notion**

`get_dependencies`' bounded walk (`truncated`/`truncatedBy`/`frontierNodes`)
exists because a multi-hop walk has no natural end. A single-hop page already
stops at `limit` and at the byte budget, and it says so with `hasMore`
(`pagination.rs:678`). It cannot say *how much* it left, and `total` is that.
`find_implementations transitive` already has `resume_token`. Nothing else is
proposed.

### 4. Provenance as a filter: **reject**

- Resolved rows already sort first (`pagination.rs:635`). Unresolved rows only
  appear on a page when the resolved ones did not fill it, and they are the
  first cut by the byte budget (`pagination.rs:168-174`). The filter already
  happens wherever it saves bytes.
- Where unresolved rows are the whole answer, `allUnresolved` flags it
  (`find_references.rs:99`). A language with no semantic tier, or one whose
  tier has not finished (`provenance`, `find_references.rs:112-121`), would
  lose real usages under a filter. That is a missing *answer*, not a missing
  edge.
- It breaks the documented contract that every edge-derived row carries
  `resolved` so the agent can weigh it (`g-mesh-v1.md:1077`).
- The cheap part of the idea, counting what was not listed, is covered by
  `total`.

## Q3. GM-360's ambiguity round trip

**Measured cost** (task text): 92/5,420 ripgrep and 54/2,113 gin
`find_definition` queries moved to `nameAmbiguous`. Each is one extra turn
(~9,100 prefix tokens, GMB-150).

**Two defects inside the turn**, both observed on the fixture:
1. The page's own instruction cannot be followed on `find_definition`: it has
   no `symbol_id` (`mod.rs:880-891`, `find_definition.rs:1131-1142`). The
   anchored tools accept it (`mod.rs:897-911`).
2. A candidate has no `startLine`/`endLine` (`find_definition.rs:358-368`).
   The caller cannot fall back to `file_path`+`position`, so it Reads or greps
   the file. That is the turn, sometimes two.

**Proposal (D3 option A), with no tie-break, so GM-360's objection does not
apply:**
- Candidates carry `startLine`, `endLine`: +~30 B each.
- `find_definition` accepts `symbol_id` (exact node, with source), which makes
  `AMBIGUOUS` true as written.
- **When the whole candidate set fits one small page (≤ 3 candidates,
  `hasMore: false`), every candidate carries its `source`**, capped at N lines
  each (proposed 20, against the 80 of a resolved answer) and a total byte cap.
  `ambiguous: true` and `resolvedBy: nameAmbiguous` stay, so every reading is
  answered and none is picked. Most "which one did you mean" turns end here.
  Fixture: 565 B to ~800 B. A `RegexMatcher`-shaped case (4 candidates)
  exceeds the threshold and gets option A's positions only, unless the owner
  sets the threshold at 4.
- The anchored tools on an ambiguous name (same page today) are out of scope
  for inlining: their answer per candidate is a whole page. They get the
  positions, and `symbol_id` already works there.

The alternatives are option B, inlining only a dominant candidate, and option
C, recording the turn as measured-only (see Decisions). What it costs: up to
~3 x 20 lines on ambiguous pages only. The measurement decides it:
probe-level counts of second calls before/after (below), and the excalidraw
`ex-ambiguous-*` tasks in the token sweep.

## Q4. Measurement plan (slice "measure")

Instrument: g-mesh-bench `v0.22.0-gmb158` (`docs/results/v0.22.0-gmb158-ts-no-regression-3.0.0.md`, §Method, §The direct test, §The A/A control). An arm is swapped by `G_MESH_BENCH_BINARY` alone. Each build lives in a detached worktree, built with `cargo build --release`. Arms are narrowed to `gmesh-configured` + `baseline` by a temporary edit of `g-mesh-bench.config.json` that is restored afterwards. `G_MESH_BENCH_WARM_CACHE=yes`, `SAVE_TRANSCRIPTS=yes`.

**Builds**

| build | ref | role |
|---|---|---|
| B0 | `main` `2bc060a` (3.21.0) | before the batch: the AC's "more resolved edges" comparison |
| B1 | `c545645` (release-4.0.0 before GM-352 code) | the batch without GM-352: attributes the change to the mechanisms vs the richer index |
| A | GM-352 branch tip after verify | under test |

Versions are read from `<binary> --version` into `gmeshVersion` on every
record, never assumed.

**Part 1: the response probe (zero API spend, all three builds).**
- Per corpus, about 20 fixed queries over the tools, plus the classes this
  task changes:
  - high fan-out `find_references` / `find_callers` (default, `limit:200`,
    `limit:0`);
  - one anchor with import rows;
  - **the ambiguous-bare-name arm**: every distinct `name` in the index swept
    through `find_definition`, GM-360's own method, recording per query
    `{outcome, candidates, bytes, calls_to_definition_text}`. The last field is
    1 for a single answer, 1 for an inlined small ambiguous set, and 2 for
    anything else.
- Report per corpus: bytes per tool (sum and median) for B0/B1/A, rows per
  kind on the fixed queries, and the count of ambiguous queries needing a
  second call. That last number is the deterministic **turns** figure for
  Go/Rust/Python.
- **Control:** the fixture above, run on B1 and A. `find_references` default
  must go from 10 to 20 call-site rows, and `find_definition target` must go
  from no `source` to two. If either is identical, the probe is dead and
  nothing else in the run is reported. GMB-158 recorded a null that was a
  dead probe: the "index still being built" page. The probe waits for
  `g-mesh init` warm-up and rejects any response under 500 B whose text names
  indexing.
- Cost: one cold index per corpus per build, **strictly sequential, never
  parallel** (ripgrep 552 s alone vs 804 s under load). Roughly 3 x (gin +
  ripgrep 552 s + py-requests + task-tracker-mcp + excalidraw) ≈ 1-1.5 h of
  wall clock, $0.

**Part 2: the token sweep.**
- Corpora, run one invocation per corpus, sequentially: gin (7 tasks),
  ripgrep (8), py-requests (8), task-tracker-mcp (14), plus excalidraw's four
  tasks that hit `nameAmbiguous` in saved transcripts:
  `ex-ambiguous-clamp-math-utils`, `ex-ambiguous-exporttosvg-public-api`,
  `ex-references-getnondeletedelements-medfanout`,
  `ex-multihop-mutateelement-nondeleted-callers` (found by `grep -rl
  nameAmbiguous results/transcripts`). That is 41 tasks.
- Arms: `gmesh-configured`, plus `baseline` as the **A/A control**. It never
  touches g-mesh, so its drift between the two sweeps is the noise floor, and
  the g-mesh effect is reported as a difference-in-differences (GMB-158).
- Builds: B0 vs A (D4 option A).
- Report per language: tokens, **turns**, and **`mcpToolCalls` beside every
  token figure** (GMB-165: a zero-call arm passes oracles and measures
  nothing). Any record with 0 `mcp__*` calls is listed and excluded from the
  g-mesh column, with the count stated. Oracle pass/fail is reported too.
- **REPS.** The token claim needs `REPS=normal` (3). `REPS=low` (1) supports
  only tool-sequence claims, as GMB-169 states. GMB-158's TS variance gives
  ±7.3% pooled over 14 tasks at 3 reps. Per language here that is roughly
  ±10% (7-8 tasks), **if** Go/Rust/Python variance resembles TS's, which no
  run has measured (GMB-169 was 1 rep). The slice reports observed per-task CV
  and states the resulting resolution rather than assuming it.
- Size: 41 tasks x 2 arms x 3 reps x 2 builds = **492 runs**. At the observed
  ~0.4 min/run (GMB-158: 168 runs / 69 min; GMB-169: 46 / 18), that is
  **~3.5 h** wall clock plus builds (~4 min each) and cold indexes. Cost about
  **$30-35** (TS ~$0.10/run, the other three ~$0.04/run, from the same two
  reports).
- Machine state recorded per run: `uptime` before/after, `/usr/bin/time -p`
  on the harness process. Everything is backgrounded, one corpus at a time,
  nothing polled.

**What "did not rise" means.** The pooled g-mesh DiD CI includes 0 or lies
below it, per language. A rise is reported with its size and the mechanism
blamed: probe bytes per tool from B0 to B1 is the richer index, B1 to A is
this task.

## Q5. Proposed slicing (after approval)

| slice | kind | model | touches |
|---|---|---|---|
| S2 | code: ranking (D1) | opus | `core/src/graph/pagination.rs` (`paginate_edges`, `StructuralCursor`, `EdgeRow`, `bound_page_within`), `core/src/graph/queries.rs` (`escape_like` visibility), `core/src/mcp/find_references.rs`, `find_callers_callees.rs`, `find_implementations.rs` (EdgeRow construction) |
| S3 | code: answer-shaped (D2) | opus | `pagination.rs` (`resolve_page_size`), `find_references.rs`, `find_callers_callees.rs` (`total`, `limit:0`, `excluded_references` dedupe), `core/src/mcp/mod.rs` (`limit` doc) |
| S4 | code: ambiguity (D3) | opus | `core/src/mcp/find_definition.rs` (`DefinitionCandidate`, `CandidatePage`, `handle_in`), `mod.rs` (`FindDefinitionParams`), `session_hints.rs` (`AMBIGUOUS`), `core/src/cli/plugin_check/expectations.rs` (candidate-page shape check, `:130`) |
| S5 | tests for S2-S4, each with a described control (revert the code, the test fails) | opus | the `#[cfg(test)]` modules of the files above |
| S6 | docs: ADR-0028 "anchored responses rank by structure and answer before evidence", plus the `g-mesh-v1.md:1072-1077` contract line | sonnet | `docs/adr/0028-*.md`, `docs/adr/README.md`, `docs/architecture/g-mesh-v1.md` |
| S7 | verify: controls in a throwaway worktree, nextest `-p g-mesh` once | opus | none |
| S8 | measure, part 1: probe on B0/B1/A + fixture control | opus | `docs/results/gm-352-response-probe.md` |
| S9 | measure, part 2: token sweep (D4/D5) | opus | `docs/results/gm-352-before-after.md` |

S2-S4 touch disjoint functions but share `pagination.rs`/`find_callers_callees.rs`,
so they run sequentially, not in parallel. If D3 is option C, S4 shrinks to the
two defect fixes (positions + `symbol_id`), or disappears if the owner rejects
those too.

## Decisions for the owner

**D1. Ranking key under `resolved, locality`.**
- **A (Recommended):** 3-level locality (file / directory / elsewhere), symbol
  rows before `File` rows, then `filePath, startLine`. Benefit: no index or
  cost change, insert-stable cursor, call sites stop losing slots to import
  lines, rows grouped per file. Risk: `filePath` order is grouping, not
  relevance, so alphabetically early directories (`benches/`, `examples/`)
  come before `src/`. Old cursors are refused once.
- B: A plus caller centrality (each row's inbound count) before `filePath`.
  Benefit: closest to "the important caller first". Risk: a correlated count
  per row; the keyset is no longer stable across a reindex (skips or repeats
  rows).
- C: keep the current order and rely on `files`. Benefit: zero change. Risk:
  the AC's "a richer graph must not push the needed row off the page" stays
  unmet for the import-row case.

**D2. Answer-shaped surface.**
- **A (Recommended):** `total` + `limit: 0` = counts and files only + excluded
  tally without repeated paths. Benefit: no new parameter (schema tax ~0);
  fixture `limit:0` 2,884 to ~1,500 B, default `find_callers` 6,000 to
  ~5,070 B. Risk: `limit: 0` changes meaning (it returned 1 row); an agent has
  to learn it from one sentence.
- B: a new `answer: rows|files|count` parameter. Benefit: self-describing.
  Risk: it lands in four tool schemas on every turn of every session.
- C: `total` only. Benefit: smallest change. Risk: "which files" stays a
  `limit:1` call carrying ~1.4 KB of evidence nobody asked for.

**D3. GM-360's ambiguity turn.**
- **A (Recommended):** candidates carry `startLine`/`endLine`;
  `find_definition` accepts `symbol_id`; ≤3 candidates with `hasMore: false`
  inline each one's source (≤20 lines each). Benefit: most ambiguous lookups
  finish in one call with no tie-break, and the hint becomes true. Risk: up to
  ~3 x 20 lines on ambiguous pages; one new parameter on `find_definition`'s
  schema.
- B: inline the source of the top candidate only, when its score is ≥2x the
  next. Benefit: smaller pages. Risk: a shown-first reading is how GM-360's
  confident-wrong answer starts. RegexMatcher's 11 vs 7 would not qualify, but
  others will.
- C: measured-only. Fix just the false hint text (no `symbol_id` advice on
  `find_definition`) and count the turns in the probe. Benefit: no shape
  change. Risk: the ~9,100-token turn stays on every ambiguous lookup.

**D4. Which "before" the token sweep uses.**
- **A (Recommended):** B0 (`main` 3.21.0) vs A, with the probe on B0/B1/A for
  attribution. Benefit: answers the AC as written (the batch's richer index
  vs the tool cost) at one sweep's price (~3.5 h, ~$30-35). Risk: the TS port
  (GM-324) also differs between B0 and A, so a TS shift needs the probe to
  attribute it.
- B: B1 vs A. Benefit: isolates GM-352's mechanisms. Risk: says nothing about
  whether the batch raised cost, which is the AC's claim.
- C: both sweeps. Benefit: complete attribution. Risk: double the time and
  spend (~7 h, ~$65).

**D5. The ambiguous-bare-name arm on Go/Rust/Python.**
- **A (Recommended):** for Go/Rust/Python, the probe sweep (deterministic count
  of second calls); in the token sweep, excalidraw's four tasks that are known
  to hit `nameAmbiguous`. Benefit: no bench change, the turn count is exact for
  every language. Risk: the token effect of the ambiguity turn is end-to-end
  measured on TypeScript only.
- B: A plus new g-mesh-bench tasks built around an ambiguous bare name in gin,
  ripgrep and py-requests (a GMB task with pinned oracles, e.g. ripgrep's
  `RegexMatcher`). Benefit: an end-to-end turn figure per language. Risk: a
  cross-repo task first, and new tasks have no variance history.

## Notes on method

- g-mesh MCP was not connected in this session (the server failed to connect
  at session start). Callers of `paginate_edges`, `StructuralCursor` and
  `EdgeRow` came from `grep -rn` over `core/src`. Everything else was read by
  symbol (`grep -n "fn "` then `sed -n` on the function).
- The fixture, its driver script and raw outputs are in the session scratchpad
  (`gm352fx/`), not committed. The daemon was stopped and the index removed
  (`g-mesh stop`, `g-mesh clean`).
