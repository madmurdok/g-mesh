# 0010. search_code: a below-floor name query is told `noMatch`, a below-floor prose query `lowSimilarity`

## Status
Accepted

## Context
`search_code` adds a `noMatch` block (`belowSimilarityFloor`) when every row
of a first page scores below its language's similarity floor, and its
sentence tells the caller to discard the rows and fall back to a structural
tool or grep. The floors (`core/src/mcp/similarity.rs::floor`) were fitted on
symbol-name queries. On natural-language queries a right answer scores
0.02-0.16 lower, and on the NL eval about one below-floor prose page in seven
(14.1%) has the right answer ranked first: the verdict steered the caller
away from it.

Re-fitting the floors on prose was measured and rejected: the fit half holds
8-21 rank-one answers per language, so a "3% floor" is just the lowest score
seen, and the verdict stops firing on absent answers (NL absent confident
wrong 37% -> 65%). Options, scores and risks:
[`docs/results/gm-434-floor-nl-queries.md`](../results/gm-434-floor-nl-queries.md),
"Options (S2)", option (f).

## Decision
We keep the floors and split the *wording* on the query's shape.

- **Prose** (whitespace inside the trimmed query,
  `similarity::is_prose_query` - the predicate the eval scored): a
  below-floor first page carries a response-level `lowSimilarity` string, and
  no `noMatch`. The sentence says none of the rows is a confident match, the
  top row may still be right, check it with one read, and otherwise fall
  back to a structural tool or grep. It never says the row "plausibly
  matches" or to "stop".
- **Name** (no inner whitespace): `noMatch` exactly as before.
- A new key rather than a new `noMatch.reason`: a key named `noMatch` above
  a sentence saying the top row may be right would contradict itself. A plain
  string rather than an object, like `hint`: there is one reason, so a
  `reason` field would be a constant.
- `lowSimilarity` follows `noMatch`'s silences: absent on a continuation
  page, an empty page and a partial page (embedding pass owed). The GM-389
  search hint stays off a `lowSimilarity` page, as it does off a `noMatch`
  page.

## Consequences
- NL pages whose right answer is ranked first are no longer told "no match"
  (misled 14.1% -> 0 on the eval); name-query behaviour is unchanged
  (false alarm 0.9%).
- The protection on below-floor prose pages moves into the caller's
  confirming read: the top row is right on 14% of them, so a caller that
  accepts it unread turns the rest into confident wrong answers. Only a
  session A/B can show which way agents land.
- Whitespace is the shape test: `parse config` is prose, `parse_config` a
  name. Agents' real queries were not sampled.
- No new constant: the floors re-fit on names at a model switch as before.
