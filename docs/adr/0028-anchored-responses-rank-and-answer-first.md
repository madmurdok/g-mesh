# 0028. Anchored responses rank by structure and answer before evidence

## Status
Accepted 2026-10-06 (owner review of the design note
[`GM-352-rank-and-answer-shaped-responses.md`](../design/GM-352-rank-and-answer-shaped-responses.md),
D1, D2 option B, D3). Measurement of the effect is pending.

## Context
`find_references`, `find_callers` and `find_callees` paginate edges and cut a
page by `limit` and by a byte budget. Before this decision the order under
`resolved` was only "the anchor's own file first", then the edge id, a
truncated hash. Inside the tier that holds almost every row (resolved, other
file) which rows survived a cut was decided by digests, not by relevance: on
the design note's fixture half of `find_references`' default page was
`kind: File` import rows. The caller who only wants "which files" or "is it
called at all" had to take a page of evidence to get it, or sum a tally.

An ambiguous `find_definition` page told the caller to re-query with a
candidate's `id` as `symbol_id`, but `find_definition` had no such parameter,
and its candidates carried no line, so the file+position fallback was out of
reach too. Each such lookup cost one extra turn.

## Decision
**Ranking.** Edge pages are ordered by: `resolved` first; then locality of the
row's file to the anchor (the anchor's own file, then its directory, then
elsewhere); then symbol rows before `File` rows; then `filePath`, then
`startLine`; then the edge id as the final tiebreak. Every key belongs to the
row itself, so the keyset cursor stays stable across a reindex. `File` rows are
demoted, never dropped. Caller centrality (a per-row inbound count) was
rejected because it breaks that stability.

**Answer shape.** `find_references`, `find_callers` and `find_callees` take
`answer: "rows" | "files" | "count"` (default `rows`). `files` returns the
whole set's per-file tally and `total` with no rows; `count` returns `total`
and `unresolved` only. `limit: 0` keeps its meaning (clamped to one row); it is
not overloaded. A `rows` page that is truncated (`hasMore`) also carries
`total`, the exact size of the whole set; it is absent when the page is
complete. The excluded-references tally no longer repeats files the rows or
`files` already name.

**Ambiguity.** `find_definition` accepts `symbol_id` (an exact node, answered
with its source). Ambiguous candidates carry `startLine` and `endLine`. When
the whole candidate set is one first page of at most three candidates, each
candidate whose span can be read also carries its `source`, capped (20 lines,
1,500 characters); the explanation says whether all or only some carry it, and
that no candidate is preferred; `ambiguous: true` and
`resolvedBy: nameAmbiguous` stay. Larger sets get positions only. No candidate
is ever picked for the caller.

## Consequences
- The three tools' schemas grow by about 500 bytes over the 11,167 measured
  before (the `answer` parameter and the `find_definition` `symbol_id`); the
  enum's per-value explanation lives in the field's one-line doc, not in
  schema prose.
- The structural cursor gains fields. A cursor from the earlier ordering is
  refused once, by name, with a request to repeat the query (the same
  treatment as ADR 0013's score cursor).
- Rows from one file are contiguous and in reading order. `filePath` order is
  grouping, not relevance: an alphabetically early directory (`benches/`,
  `examples/`) precedes `src/` within the same locality tier.
- Byte size of a default `find_references` page goes up slightly (symbol rows
  are longer than `File` rows); this is a quality change, not a size one.
- `answer` removes the need for a `limit:1` call to learn which files hold
  usages, at the cost of one more parameter the agent has to learn.
- Ambiguous pages of up to three candidates grow by up to about 3 x 20 lines.
- Whether the changes pay off in tokens is not established yet: the probe and
  the before/after sweep are pending.

## Rejected alternatives
- **`limit: 0` as "answer without evidence".** No schema cost, but it changes
  the meaning of an existing value and has to be learned from one sentence.
- **Caller centrality in the ranking key.** Closer to "most important caller",
  but a correlated count per row and an unstable keyset across a reindex.
- **Provenance as a filter.** Resolved rows already sort first, and dropping
  unresolved rows would hide real usages in languages whose semantic tier is
  absent or unfinished.
- **Inlining only a dominant candidate's source.** A candidate shown first is
  how a confident wrong answer starts.
