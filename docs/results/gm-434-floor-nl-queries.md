# GM-434 S1: the shipped similarity floors on natural-language queries

Question: at the floors `search_code` ships today, how often does a right
top answer get reported as "no match" (false alarm), and how often does a
wrong top answer clear the floor (confident wrong), per language, on the
authored natural-language queries of GM-398's eval? Mechanical name queries
beside them for contrast.

## Floors measured

The shipped constants, read from `core/src/mcp/similarity.rs::floor` on
release-3.16.0 (`d324b97`):

| language | shipped floor | GM-398 re-fitted (D6) |
|---|---|---|
| go | 0.59 | 0.56 |
| python | 0.57 | 0.58 |
| rust | 0.55 | 0.56 |
| typescript | 0.50 | 0.55 |

GM-434's description quotes 0.56 / 0.58 / 0.56 as "the shipped constants";
those are the floors GM-398 re-fitted (`report-phaseB.json`). The shipped
ones are the left column. GM-398's 10-19% was measured at the re-fitted
floors; this doc measures the shipped ones.

## Results (jina-embeddings-v2-base-code fp32, the shipped model)

Definitions, identical to `core/src/cli/embed_eval/metrics.rs`:

- **false alarm** = positive query whose top hit is an expected symbol, but
  whose top score is below the floor of the top hit's language. n = positives
  ranked right at 1.
- **confident wrong (positives)** = top score at or above its floor and the
  top hit is not expected. n = all positives.
- **confident wrong (absent)** = absent-answer query whose top score is at or
  above its floor. n = all absent queries.

"NL held-out" is D6's held-out half (odd first byte of `sha256(id)`), the
set GM-398 reports. The shipped floors were fitted on the bench calibration
sweep, not on any authored query, so for them **every** authored query is
out of sample; "NL all" uses both halves and doubles n.

| set | language | false alarm | confident wrong (positives) | confident wrong (absent) |
|---|---|---|---|---|
| NL held-out | go | **31.2%** (5/16) | 36.1% (22/61) | 9.1% (1/11) |
| NL held-out | python | **9.5%** (2/21) | 44.7% (21/47) | 36.4% (4/11) |
| NL held-out | rust | **14.3%** (1/7) | 76.6% (36/47) | 45.5% (5/11) |
| NL held-out | typescript | **7.4%** (2/27) | 51.7% (31/60) | 53.8% (7/13) |
| NL held-out | all | **14.1%** (10/71) | 51.2% (110/215) | 37.0% (17/46) |
| NL all | go | **30.0%** (9/30) | 38.0% (38/100) | 4.0% (1/25) |
| NL all | python | **9.5%** (4/42) | 42.0% (42/100) | 36.0% (9/25) |
| NL all | rust | **13.3%** (2/15) | 79.0% (79/100) | 60.0% (15/25) |
| NL all | typescript | **10.6%** (5/47) | 47.0% (47/100) | 64.0% (16/25) |
| NL all | all | **14.9%** (20/134) | 51.5% (206/400) | 41.0% (41/100) |
| name | go | 2.3% (2/86) | 38.7% (58/150) | 8.0% (12/150) |
| name | python | 1.0% (1/103) | 28.0% (42/150) | 13.3% (20/150) |
| name | rust | 1.3% (2/149) | 48.3% (145/300) | 32.0% (96/300) |
| name | typescript | 0.0% (0/231) | 16.8% (47/279) | 31.3% (94/300) |
| name | all | 0.9% (5/569) | 33.2% (292/879) | 24.7% (222/900) |

Languages: go = gin, python = py-requests, rust = ripgrep + g-mesh,
typescript = excalidraw + task-tracker-mcp (each query's own `language`).
"all" is pooled over queries, not language-weighted.

What it says:

- On name queries the shipped floors hold their promise (0-2.3% false
  alarm). On natural-language queries they miss it by 3-10x: **14-15%
  pooled**, and go is the outlier at **30%** because its shipped floor (0.59)
  sits 0.03 above the re-fitted one. TypeScript is *lower* at the shipped
  0.50 than at the re-fitted 0.55 (7.4% vs 14.8% held-out), at the cost of
  more confident-wrong absent pages (53.8% vs 15.4%).
- The floor buys little on NL wrong answers. Of the NL positives whose top
  hit is wrong ("NL all"), the share that still clears the floor: go 38/70
  (54%), python 42/58 (72%), rust 79/85 (93%), typescript 47/53 (89%). For
  rust and typescript the floor almost never turns a wrong NL top answer
  into "no match"; its protection there is mostly on absent-answer queries,
  and even that is 36-64% confident wrong outside go.
- n is small where it matters: the false-alarm denominator is the number of
  NL queries ranked right at 1 - rust 7 held-out / 15 all, go 16 / 30. One
  query moves rust's held-out rate by 14 points. Read per-language rates as
  direction, the pooled 14-15% as the number.

Control: the same script at the re-fitted floors (`--floors fitted`)
reproduces GM-398's `report-phaseB.json` reference arm exactly: held-out
false alarm go 18.8%, python 9.5%, rust 14.3%, typescript 14.8%;
confident wrong 113/215 positives and 12/46 absent. The two floor settings
give different rows (go 5/16 vs 3/16, typescript 2/27 vs 4/27), so the
control distinguishes them.

Caveat on rule: the eval judges the **top hit** against its language's
floor; `search_code` fires when **every row on the page** is below its own
language's floor. On a page sorted by score these agree unless the page
mixes languages with different floors (g-mesh's corpus is rust + TS); the
eval stores only the top hit's language, so the difference is not measured
here.

## How search_code presents a below-floor page today

From code (`core/src/mcp/search_code.rs::handle`, `SearchPage`;
`core/src/mcp/similarity.rs::verdict`, `NoMatch`, `BELOW_FLOOR_EXPLANATION`):

- **The rows are returned.** `results` is the normal page (default 20 rows,
  each `symbolId`, `qualifiedName`, `kind`, `filePath`, `startLine`,
  `startCol`, `score`). Nothing is hidden or truncated; the per-row
  `language` is `#[serde(skip)]`, and the floor value is never on the wire.
- **A `noMatch` block is added** when every row's `score < floor(row.language)`,
  on a first page only (no `cursor`), non-empty page, and no embedding pass
  owed:

  ```json
  "noMatch": {
    "reason": "belowSimilarityFloor",
    "explanation": "Nothing here reached the similarity floor for its language, so read this page as 'no declaration in this index matches' rather than as candidates. The rows are still listed, and their `score` column is what this verdict was computed from - but they are the nearest vectors, not matches. Fall back to a structural tool or to grep rather than rewording the query."
  }
  ```

  So the wording tells the agent to *disregard* the rows and fall back to
  structural tools or grep - on the 14-15% of NL pages where the top row is
  in fact the answer, it steers the agent away from it.
- The other reason, `queryIsAPathOrPackage`, fires on specifier-shaped
  queries regardless of scores.
- **`hint`** (GM-389): `session_hints::SEARCH_HITS` ("These hits are ranked
  by similarity, not resolved: once one plausibly matches, do one confirming
  read and stop, without rewording the query or grepping the repo."), once
  per session, only on a page with rows **and no `noMatch`**. A below-floor
  page never carries it.
- **Partial pages** (embedding pass still owed): `partial_verdict` withholds
  `belowSimilarityFloor` (keeps only the specifier verdict), `hint` is
  absent, `partial: {embedded, total}` is added, and a text note precedes
  the JSON.
- Pinned by `search_code::tests::a_page_below_the_floor_says_no_and_a_page_above_it_says_nothing`
  (healthy page: no `noMatch` key at all).

A live call was attempted (`search_code` on the g-mesh project, NL query,
limit 3) but the daemon had just indexed the project and returned the
partial shape `{"results":[],"hasMore":false,"nextCursor":null,"partial":{"embedded":0,"total":0}}`
with the "embedding pass has not started yet" note, twice; the below-floor
shape above is from code and its test, not from a live page.

## Method and reproduction

No re-embedding: the script reads GM-398's stored rankings (top 100 hits
with scores and the top hit's language per query) and the frozen query
files, checks each run manifest's query-file sha256 against the files in
this checkout (fails on drift), and applies the fixed floors.

Inputs: `eval/embedding/work/runs/jina-v2-base-code-fp32/<corpus>/`
(`rankings.jsonl`, `manifest.json`; gmeshVersion 3.15.0, model
`jinaai/jina-embeddings-v2-base-code@516f4ba`), produced by GM-398 S2 in the
main checkout (`work/` is git-ignored); query files `eval/embedding/queries/`
at `620eae2` (identical in this branch).

```sh
# from the repository root; RUN is where GM-398's run lives
RUN=../g-mesh/eval/embedding/work/runs/jina-v2-base-code-fp32
python3 eval/embedding/shipped_floor_rates.py --run "$RUN"                  # shipped floors (this table)
python3 eval/embedding/shipped_floor_rates.py --run "$RUN" --floors fitted  # control: reproduces report-phaseB.json
```

To regenerate the rankings themselves, see `eval/embedding/README.md`
(`make_snapshot.sh`, then `g-mesh debug-embed-eval run --variant jina-v2-base-code-fp32`).
