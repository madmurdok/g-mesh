# Tool answer guarantees

For anyone building or debugging an MCP consumer of g-mesh. It states, per
answer field, when the field appears, what it guarantees, what it does not,
and which function emits it. The session `instructions` carry only what an
agent needs before its first call (ADR 0022); the per-answer fields below are
explained here and, for a few, by a once-per-session `hint` in the answer.

Paths are under `core/src/mcp/` unless they start with another directory.
"Emitter" is `file:fn`, checked against the tree at release 4.3.0.

## Scope

The four **anchored tools** resolve a symbol with `anchor::resolve` (by
`symbol_name`) or `anchor::by_id` (by `symbol_id`) and then walk edges:

| tool | handler | edge kinds walked |
|---|---|---|
| `find_references` | `find_references.rs:handle_in_covered` | `CALLS`, `REFERENCES`, `SUPERTYPE_OF` |
| `find_callers` | `find_callers_callees.rs:handle_callers_in_covered` | `CALLS`, incoming |
| `find_callees` | `find_callers_callees.rs:handle_callees_in_covered` | `CALLS`, outgoing |
| `find_implementations` | `find_implementations.rs:dispatch_in_covered` | `SUPERTYPE_OF`, incoming |

`find_definition` is the resolver itself (`find_definition.rs:handle_in`), not
an edge walk. `get_file_outline` is covered last because it has its own
pagination guarantees.

Two rules hold for every field below:

- **Absent, not null or zero.** A disclosure field is left out of the JSON
  when it has nothing to say. Its absence is only meaningful where this
  document says so.
- **Disclosures are best-effort footnotes.** `unlinked::probe`,
  `untyped::probe`, `overrides::probe` and `excluded_references` turn a
  database error into "no field" (`.ok()?`) rather than failing an answer that
  already succeeded. `provenance::resolve` turns a failed read of
  `language_state` into silence. So a missing field is never proof that the
  condition is false; it is proof only that g-mesh found nothing and had no
  error doing so.

## Which tool carries which field

| field | references | callers | callees | implementations | definition |
|---|---|---|---|---|---|
| `anchor.resolvedBy` / `anchor.queriedAs` | yes | yes | yes | yes | `resolvedBy`/`queriedAs` on the node |
| row `resolved`, `allUnresolved` | yes | yes | yes | yes (single-hop) | no |
| `unlinkedUsages` | yes | yes | no | no | no |
| `untypedReceiverCalls` | yes | yes | no | no | no |
| `overrides`, `overridesTruncated` | yes | yes | no | no | no |
| `excludedReferences` | no | yes | yes | no | no |
| `provenance` | yes | yes | yes | yes | no |
| `notIndexed` | yes | yes | yes | single-hop only | refusal, see below |
| `hint` | yes | yes | yes | yes | no (`explanation` instead) |
| `total`, `files`, `filesTruncated` | yes | yes | `total` only | no | no |

`find_implementations` with `transitive: true` or `resume_token` returns a
different shape (`TransitiveImplementationWalk`): `truncated`, `truncatedBy`,
`frontierNodes`, `resumeToken`, `hint`, and `provenance`; no `hasMore`, no
`allUnresolved`, no `notIndexed`. See "truncated" below.

Non-row answers (`answer: "count"` and `answer: "files"` on references,
callers and callees; emitter `answer.rs:respond`) carry `anchor`, `total`,
`unresolved` (count only), `files` (files only), `filesTruncated`, `hint`, and
the tool's disclosures flattened in. They carry **no** `allUnresolved`, no
`hasMore`, and no per-row `resolved`; `unresolved` is the count of unconfirmed
rows instead.

## Resolution fields

### `anchor.resolvedBy`, `anchor.queriedAs`

- **Appears:** on every row answer of the four tools, in `anchor`
  (`anchor.rs:AnchorInfo::with_rung`). Absent when the anchor was carried, not
  resolved: a resumed `find_implementations` walk. On `find_definition` it is
  on the node (`find_definition.rs:DefinitionNode::resolved`), and absent on a
  file + position lookup (`DefinitionNode::from`), which cannot be anything but
  exact.
- **Values** (`find_definition.rs:ResolvedBy`): `id`, `qualifiedName`, `name`,
  `qualifiedNameSuffix`, `nameAmbiguous`, `fileName`, `semanticNeighbours`.
- **Guarantees:** the first four establish that this is the symbol asked for.
  `by_id` always reports `id`.
- **Does not guarantee:** `nameAmbiguous`, `fileName` and `semanticNeighbours`
  establish only candidates worth re-querying. `semanticNeighbours` is
  similarity, not resolution. `queriedAs` is the name actually looked up when
  it differs from `symbol_name`.

### `find_definition` candidate pages

- **Appears:** when the name does not resolve to one declaration. Emitters:
  `find_definition.rs:CandidatePage::ambiguous` (`ambiguous: true`,
  `resolvedBy: nameAmbiguous`, `hasMore`, `nextCursor`) and the `FileNamePage`
  built for `fileName` and `semanticNeighbours` (`ambiguous: false`).
- **Guarantees:** a candidate page is never presented as one answer: it carries
  `ambiguous` and `resolvedBy`, plus `explanation` (a `session_hints`
  sentence on the ambiguous page, free text on the other).
- **Does not guarantee:** that any candidate is the intended symbol. On an
  ambiguous page, every candidate carries `source` only when the whole set is
  on this one page and at most `SOURCED_CANDIDATES`; a candidate whose file
  cannot be read comes back without `source`, and the explanation then says
  "some".
- `find_definition` has no `hint`, `truncated` or `provenance` field.

### `source.omittedLines` (`find_definition`)

- **Appears:** inside `source` (`source.rs:Snippet`), only when the snippet was
  cut (`source.rs:read_span_within`, `omitted_lines: (kept < span.len())`).
  Cut at `MAX_LINES` (80) lines or `MAX_CHARS` (6,000) characters; a sourced
  candidate has a quarter of that (`find_definition.rs:CANDIDATE_SOURCE_LINES`,
  `CANDIDATE_SOURCE_CHARS`).
- **Guarantees:** absent means the snippet is the whole declaration span as it
  is on disk now. Present is the exact number of lines left out.
- **Does not guarantee:** that `source` exists. It is omitted when
  `include_source` is false, or the file is gone, unreadable, not UTF-8, or
  shorter than the index believes; the two cases look the same.
  `firstLine` is 1-based while `startLine` is 0-based.

## Row confidence

### Row `resolved` (and `resolved: false`)

- **Appears:** on every row of references, callers, callees and single-hop
  implementations (`ReferenceSite`, `CallerSite`, `CalleeSite`,
  `ImplementationSite`). It is stored on the edge (`edges.resolved`) and passed
  through unchanged.
- **Guarantees:** within a page, `resolved: true` rows sort before
  `resolved: false` rows (`graph/pagination.rs:paginate_edges`).
- **Does not guarantee:** what `false` means beyond "the linker did not confirm
  this edge". The reading "a cross-file edge the linker could not confirm; a
  same-file edge is always `true`" is the plugins' convention, which core
  stores but does not enforce, so treat the same-file half as a plugin
  guarantee, not a core one.
- **Hint:** the first page in a session with a `resolved: false` row, and not
  an `allUnresolved` page, gets `session_hints::UNRESOLVED_ROW`
  (`HintKey::UnresolvedRow`, once per session).

### `allUnresolved`

- **Appears:** as a bare boolean on every row answer of references, callers,
  callees and single-hop implementations. It is serialized even when `false`
  (no `skip_serializing_if`). Not present on non-row answers or on the
  transitive implementations walk.
- **Emitter:** `graph/pagination.rs:bound_page_within` sets it from the rows
  that survive the byte cut, so it describes the page as sent.
- **Guarantees:** `true` only when `results` is non-empty and every row is
  `resolved: false`. Always `false` on an empty page.
- **Does not guarantee:** that the rows are wrong, only that none was
  confirmed. It is `false` for any page built outside the edge-row path.
- **Hint:** `session_hints::ALL_UNRESOLVED` rides on every such page (not once
  per session). The earlier ADR 0022 note that the flag is "absent" on an empty
  page is wrong for the code: it is present and `false`.

## Page exhaustiveness

### `hasMore`, `nextCursor`, `total`

- **Appears:** `hasMore` and `nextCursor` on every row answer (`nextCursor` is
  `null` when there is no more); `total` only when `hasMore` is true
  (`skip_serializing_if`), and then it is the exact count over the whole set.
  On a complete page it is the length of `results`.
- **Guarantees:** `hasMore: true` means rows remain after the cursor. The
  page is cut by byte size as well as by `limit`
  (`graph/pagination.rs:bound_page_within`, `MAX_RESPONSE_BYTES` 20,000), so a
  page may hold fewer rows than `limit` with `hasMore: true`.
- **Does not guarantee:** that `hasMore: false` means no usage is missing. It
  means no more rows of the edge set this tool walks. See the next two fields.

### `hasMore: false` without `unlinkedUsages` or `untypedReceiverCalls`

This is ADR 0022 statement 8. It holds only for `find_callers` and
`find_references`, and only as far as the two probes below can tell:

- **Guaranteed:** the page lists every edge of the walked kinds whose target is
  the anchor, for the `file_paths` filter given.
- **Best-effort:** that no call is missing for another reason. Both probes
  match by name, swallow their own errors, and cover only the shapes below.
- **Not applicable:** `find_callees` and `find_implementations` never carry
  either field, so their absence says nothing.

### `unlinkedUsages`

- **Emitter:** `unlinked.rs:probe`, serialized from `unlinked.rs:CandidateTally`;
  attached in `find_callers_callees.rs:handle_callers_in_covered` and
  `find_references.rs:handle_in_covered`.
- **Appears:** on references and callers pages, when at least one usage edge
  sits on a `pending_symbol` placeholder whose bare name equals the anchor's
  (and, for a `T::f` key, whose second-to-last segment matches too). Not for a
  `File` anchor, a placeholder or a re-export node. Absent otherwise, never
  `0`.
- **Fields:** `count` (uncapped candidates, in usage edges), `files` (top 20
  files by count, 1,000 bytes), `filesTruncated`, `hint` (fixed sentence).
- **Guarantees:** `count` is exact for the name match, and every file that is
  named holds a candidate.
- **Does not guarantee:** that any candidate is a usage of this symbol. The
  match is by name, so the field can only say "may". A one-segment key counts
  only for an anchor that is not a type member.

### `untypedReceiverCalls`

- **Emitter:** `untyped.rs:probe` (same `CandidateTally` shape as above).
- **Appears:** on references and callers pages when the anchor is a method
  (`untyped.rs:is_method`) and at least one function calls a method of that
  bare name through a receiver whose type the plugin did not infer, with no
  edge to the anchor of the page's kinds, and no `semantic` edge to any node of
  that name. Only languages whose plugin reports such calls
  (`untypedCalls`) can produce it; the ADR names Rust as the only one today.
- **Fields:** `count` is the number of calling **functions**, not call sites;
  `files`, `filesTruncated`, `hint` as above.
- **Guarantees:** a completed semantic pass hides only the calls it answered.
- **Does not guarantee:** that it appears for a language whose plugin reports
  no untyped calls (TypeScript today): there the gap exists and nothing says
  so, which is why the instructions state it once per session. It never counts
  what `unlinkedUsages` counts.

### `excludedReferences`

- **Emitter:** `find_callers_callees.rs:excluded_references`.
- **Appears:** on callers and callees answers when `REFERENCES` edges touch the
  anchor, which the `CALLS` walk does not list (a call at file top level or in
  an anonymous callback).
- **Guarantees:** `count` is exact and uncapped; `files` names files the
  response does not already name, capped (`filesTruncated` says so).
- **Does not guarantee:** the calling symbol or line; use `find_references`
  for those.

## Tier and language coverage

### `provenance`

- **Emitter:** `provenance.rs:resolve` then `Resolved::disclose`.
- **Appears:** on the four edge-walking tools, and on the transitive
  `find_implementations` walk, only when the anchor's language declares a
  semantic tier (`Capabilities::semantic_pass`) that has not completed for this
  project. Never on `find_definition`, `get_file_outline`, `get_dependencies`
  or `search_code`.
- **Shape:** `{language, semanticTier}` with `semanticTier` `absent` or
  `pending`. `pending` adds `since` (RFC 3339), `pendingFiles` (at most 25
  files, 1,500 bytes, anchor's file first) and `pendingFilesOmitted`, all
  about files this response names.
- **Guarantees:** the language is the **anchor's**. Silence (no field) is
  returned when no semantic tier is declared or its pass is done.
- **Does not guarantee:** how much the missing tier would have added. It
  refuses to estimate. `absent` does not say why the tier is missing. A failed
  read of `language_state` yields silence, so it can under-warn.
- **Hint:** `session_hints::PROVENANCE`, once per session.

### `notIndexed`

- **Emitters:** `not_indexed.rs:group` for the four find tools,
  `not_indexed.rs:refusal` / `not_indexed.rs:miss` for path-anchored tools;
  the uncovered set comes from `mod.rs:filter_coverage`.
- **Appears (find tools):** a list on references, callers, callees and
  single-hop `find_implementations` answers, only when the `file_paths` filter
  names a file in a language with no indexed files. One entry per language:
  `language`, `reason` (`pluginAbsent` or `pluginFailed`), `command`, `error`
  (failed only), `filePaths` (the requested paths it covers). The answer
  itself is produced as usual. The transitive and resumed
  `find_implementations` paths ignore `file_paths` and so omit it.
- **Appears (path-anchored):** `find_definition` by file + position,
  `get_file_outline` and `get_dependencies` by `file_path` return a tool error
  whose JSON body is `{error, notIndexed}` when the path misses and its
  language is not indexed. A path that hits never carries the key.
- **Guarantees:** a listed language has no indexed files, so an empty result
  for those paths is not evidence the file is empty or missing.
- **Does not guarantee:** anything about paths in covered languages.

## Hints

### `hint`

- **Appears:** on row answers of the four tools, absent (not null) when no
  sentence applies. It is one string, sentences joined by a space, in this
  order for callers and references: the file-anchor sentence
  (`anchor.rs:file_anchor_hint`), `ALL_UNRESOLVED`, `FILE_ROW`, `FILES_TALLY`,
  `UNRESOLVED_ROW`, `PROVENANCE`, `OVERRIDES`.
- **Guarantees:** each sentence is present exactly when its trigger is in the
  same response.
- **Does not guarantee:** that a once-per-session sentence repeats. Do not
  parse `hint` for state; read the fields.

### Session hints (once per session)

Emitter: `session_hints.rs:SessionHints::once` / `offer`. A key is recorded as
sent per MCP connection (`GMeshMcpServer::new` makes a fresh set), and a
trigger that is false records nothing. Keys (`HintKey`): `FileRow`,
`FilesTally`, `WalkComplete`, `SearchHits`, `UnresolvedRow`, `SemanticTier`
(provenance), `Overrides`. `ALL_UNRESOLVED` is not a session hint and rides on
every unconfirmed page. `Overrides` is one key across `find_callers` and
`find_references`. A consumer that joins a new session after a reconnect gets
each sentence again.

### `overrides`, `overridesTruncated`

- **Emitter:** `overrides.rs:probe`, attached in
  `find_callers_callees.rs:handle_callers_in_covered` and
  `find_references.rs:handle_in_covered` (GM-502, GM-536). Design:
  `docs/architecture/gm-502-override-callers-field.md`.
- **Appears:** when the anchor is a `Function`, the language's manifest
  `member_overrides` is `by_name` or `declared` (not `none`), and at least one
  base member is found. Absent, never `[]`.
- **Fields:** `overrides` is up to 8 base members (`id`, `qualifiedName`,
  `filePath`, `startLine`, 0-based), ordered by depth then `qualifiedName`;
  `overridesTruncated` is present only when more than 8 were found.
- **Guarantees:** each listed member is a base the anchor overrides or
  implements, found over resolved `SUPERTYPE_OF` edges (`by_name` walk is
  bounded by depth 8 and 64 visited nodes).
- **Does not guarantee:** that the list is complete for `none` languages
  (nothing is said), nor that the base member's page holds the calls: ask
  `find_callers` or `find_references` for each `id`.
- **Hint:** `session_hints::OVERRIDES`, once per session.

## Cut walks

### `truncated`, `truncatedBy`

- **Appears:** only on the transitive `find_implementations` walk
  (`find_implementations.rs:TransitiveImplementationWalk`), with
  `frontierNodes` and `resumeToken`. Causes: `maxDepth`, `maxFanout`,
  `explorationBudget`, `responseSize`. Not emitted by the single-hop tools.
- **Guarantees:** `truncated: false` means the walk finished for the depth
  asked. A truncated walk carries the matching `session_hints::truncated_by`
  sentence in `hint`.
- **Does not guarantee:** `hasMore` or `allUnresolved`; this shape has neither.

## `get_file_outline` (GM-523)

Emitter: `get_file_outline.rs:list_outline`, cut by
`graph/pagination.rs:bound_defines_page`.

1. A response is at most 8,000 serialized bytes
   (`pagination::OUTLINE_MAX_RESPONSE_BYTES`) for any `limit` and `detail`,
   except one row that alone exceeds it is still sent alone
   (`longest_prefix_fitting` never returns zero rows).
2. Rows are in source order (`startLine`, `startCol`, `id`;
   `paginate_defines`). Following `nextCursor` returns every row exactly once:
   a byte cut re-encodes the cursor from the last row kept
   (`source_order_cursor`), so no row is dropped or repeated.
3. `hasMore: true` means rows remain, including after a byte cut with `limit`
   not reached. `total` is present only then, and is the exact row count of the
   whole file (`pagination::count_defines`).
4. Compact rows (default): `symbolId`, `name`, `kind`, `startLine`, `endLine`,
   `exported`. `detail: "full"` adds `qualifiedName`, `startCol`, `endCol`,
   `signature` (`null` when none) and is the pre-GM-523 shape
   (`get_file_outline.rs:render`). `exported` means reachable from outside the
   file, not "the line has a visibility keyword" (GM-369).
5. `limit` is an upper bound, so a page may hold fewer rows with `hasMore`
   true. When omitted the row ceiling is 200 (`MAX_PAGE_SIZE`) and the byte
   budget does the bounding; an explicit `limit` is clamped to 1-200.

Not confirmed: the GM-523 note says a guidance prefix line outside the JSON is
not counted in the budget. No code in `core/src/mcp` adds such a line to a
tool result (the "guidance" in `gm-389-guidance-prefix.md` is the installed
`AGENTS.md` block), so that statement is left out here.
