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

## After (S7): the shipped rule on the same stored rankings

`core/src/mcp/similarity.rs` at `8e4a9e0`, floors unchanged (go 0.59,
python 0.57, rust 0.55, typescript 0.50). Before = S1 (every below-floor
page is `noMatch`); after = `verdict` + `low_similarity`: a name query below
its floor keeps `noMatch`, a prose query gets `lowSimilarity` instead.
**misled** = the right answer ranks first but the agent is told `noMatch`
(S1's false alarm); **lowSimilarity pages** = the extra confirming reads.
Per query language, as in S1's table.

| set | language | misled before | misled after | confident wrong, absent (before = after) | confident wrong, pos (before = after) | noMatch pages before / after | lowSimilarity pages after |
|---|---|---|---|---|---|---|---|
| NL held-out | go | 31.2% (5/16) | **0.0%** (0/16) | 9.1% (1/11) | 36.1% (22/61) | 38 / 0 | 38 |
| NL held-out | python | 9.5% (2/21) | **0.0%** (0/21) | 36.4% (4/11) | 44.7% (21/47) | 14 / 0 | 14 |
| NL held-out | rust | 14.3% (1/7) | **0.0%** (0/7) | 45.5% (5/11) | 76.6% (36/47) | 11 / 0 | 11 |
| NL held-out | typescript | 7.4% (2/27) | **0.0%** (0/27) | 53.8% (7/13) | 51.7% (31/60) | 10 / 0 | 10 |
| NL held-out | all | 14.1% (10/71) | **0.0%** (0/71) | 37.0% (17/46) | 51.2% (110/215) | 73 / 0 | 73 |
| NL all | go | 30.0% (9/30) | **0.0%** (0/30) | 4.0% (1/25) | 38.0% (38/100) | 65 / 0 | 65 |
| NL all | python | 9.5% (4/42) | **0.0%** (0/42) | 36.0% (9/25) | 42.0% (42/100) | 36 / 0 | 36 |
| NL all | rust | 13.3% (2/15) | **0.0%** (0/15) | 60.0% (15/25) | 79.0% (79/100) | 18 / 0 | 18 |
| NL all | typescript | 10.6% (5/47) | **0.0%** (0/47) | 64.0% (16/25) | 47.0% (47/100) | 20 / 0 | 20 |
| NL all | all | 14.9% (20/134) | **0.0%** (0/134) | 41.0% (41/100) | 51.5% (206/400) | 139 / 0 | 139 |
| name | go | 2.3% (2/86) | 2.3% (2/86) | 8.0% (12/150) | 38.7% (58/150) | 146 / 146 | 0 |
| name | python | 1.0% (1/103) | 1.0% (1/103) | 13.3% (20/150) | 28.0% (42/150) | 136 / 136 | 0 |
| name | rust | 1.3% (2/149) | 1.3% (2/149) | 32.0% (96/300) | 48.3% (145/300) | 212 / 212 | 0 |
| name | typescript | 0.0% (0/231) | 0.0% (0/231) | 31.3% (94/300) | 16.8% (47/279) | 207 / 207 | 0 |
| name | all | 0.9% (5/569) | 0.9% (5/569) | 24.7% (222/900) | 33.2% (292/879) | 701 / 701 | 0 |

Of the 73 NL held-out lowSimilarity pages, 10 have the right answer on top
and 63 do not (139: 20 / 119 on NL all) - S2's risk 1: those 63 stay out of
"confident wrong" only if the confirming read is sceptical.

Rule check, Rust against S2's simulated option f, over all 2,279 queries:

- **Prose predicate.** Rust `query.trim().chars().any(char::is_whitespace)`
  (Unicode White_Space); the script `any(c.isspace() for c in text.strip())`
  (which also counts U+001C-U+001F). Disagreements: **0**; no eval query
  contains whitespace other than U+0020.
- **Specifier verdict.** Rust answers `noMatch` (`QueryIsAPathOrPackage`)
  on a non-prose query starting with `@` or containing `/`, whatever the
  scores; the simulation has no such branch. Eval queries it fires on: **0**.
- **No verdict at all in Rust.** Empty first page (and any continuation
  page): **0** eval queries have an empty page; the eval scores first pages
  only.
- **Page signal** (HARD / SOFT / NONE) per query: **0** disagreements, so
  S2's option f rows reproduce exactly (NL held-out misled 0/71, confident
  wrong 110/215 and 17/46, soft-ok 10, soft-miss 63; name = a).
- Not measurable here, as in S1's caveat: Rust judges **every row** against
  its own language's floor, the eval only the top row against the top hit's
  language; they can differ only on a mixed-language page.

The checks can fire: on synthetic strings the script's Rust port calls
`"a\x1cb"` a name where Python calls it prose, `@scope/pkg` and `src/a.rs`
specifiers, and `serialize/deserialize the config` prose, matching
`similarity.rs`'s own tests.

```sh
python3 eval/embedding/shipped_floor_rates.py --run "$RUN" --after
```

Control: the default, `--floors fitted` and `--options` outputs are
byte-identical before and after adding `--after` (md5
`8ad95fbef93d0f3937913fa46de7104a`, `45cf0e231dcc11a71050825a512df597`,
`c8b3c606c21394cf748e544c3a982aa0`).

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

## Options (S2)

### How an option is scored

Every option maps a first page (top score, top hit's language, query shape)
to one of three signals the agent acts on:

- **HARD** - today's `noMatch`: "read this page as no match ... fall back to
  a structural tool or grep". The agent discards the rows.
- **SOFT** - a low-similarity note that the top row may still be right:
  the agent does one confirming read, and falls back only if that read fails.
- **NONE** - an ordinary page (with GM-389's "one confirming read and stop"
  hint): the agent takes the top row.

Outcomes, per query: **misled** = HARD on a page whose top row is right (the
false alarm; the agent is told to throw away the answer it was handed, and
may conclude it does not exist); **confident wrong** = NONE on a page whose
top row is wrong (positives) or that has no answer (absent) - what the floor
exists to limit; **soft-ok** = SOFT, top row right (one read, found);
**soft-miss** = SOFT, top row wrong or absent (one wasted read, and the
floor's protection now depends on the agent reading sceptically); **hard-ok**
= HARD on a wrong/absent page (the verdict working).

Options that fit a constant on NL queries fit it on D6's NL **fit half**
with `metrics.rs::fit_floor`'s rule (3%, rounded down), per top-hit
language, and are scored on the **held-out half**. "Prose" = the query
contains whitespace; it agrees with the eval's own authored/mechanical split
on 2,279 of 2,279 queries, and the predicate already exists in
`is_specifier_query`.

```sh
python3 eval/embedding/shipped_floor_rates.py --run "$RUN" --options
```

(The default and `--floors fitted` outputs are byte-identical to S1's
script: md5 checked against `2fa0f3d` for both.)

### Results (pooled; per-language below)

| option | set | misled | confident wrong, pos | confident wrong, absent | soft-ok | soft-miss | hard-ok |
|---|---|---|---|---|---|---|---|
| **a** keep | NL held-out | 14.1% (10/71) | 51.2% (110/215) | 37.0% (17/46) | 0 | 0 | 63 |
| | name | 0.9% (5/569) | 33.2% (292/879) | 24.7% (222/900) | 0 | 0 | 696 |
| **b** NL re-fit per language (go .53, py .45, rust .54, ts .41), all queries | NL held-out | 5.6% (4/71) | 60.9% (131/215) | 65.2% (30/46) | 0 | 0 | 29 |
| | name | 0.2% (1/569) | 34.4% (302/879) | 44.1% (397/900) | 0 | 0 | 511 |
| b' NL re-fit pooled (.41) | NL held-out | 1.4% (1/71) | 66.5% (143/215) | 93.5% (43/46) | 0 | 0 | 4 |
| | name | 0.0% (0/569) | 35.3% (310/879) | 68.6% (617/900) | 0 | 0 | 283 |
| **c** soft wording, all queries | NL held-out | 0.0% | 51.2% (110/215) | 37.0% (17/46) | 10 | 63 | 0 |
| | name | 0.0% | 33.2% (292/879) | 24.7% (222/900) | 5 | 696 | 0 |
| **d** rows marked, no verdict (mark ignored) | NL held-out | 0.0% | 67.0% (144/215) | 100% (46/46) | 0 | 0 | 0 |
| | name | 0.0% | 35.3% (310/879) | 100% (900/900) | 0 | 0 | 0 |
| e = b on prose, a on names | NL held-out | 5.6% (4/71) | 60.9% (131/215) | 65.2% (30/46) | 0 | 0 | 29 |
| | name | = a | | | | | |
| **f** = c on prose, a on names | NL held-out | 0.0% | 51.2% (110/215) | 37.0% (17/46) | 10 | 63 | 0 |
| | name | = a | | | | | |
| g two-tier: HARD below NL re-fit, SOFT up to shipped, all queries | NL held-out | 5.6% (4/71) | 51.2% (110/215) | 37.0% (17/46) | 6 | 34 | 29 |
| | name | 0.2% (1/569) | 33.2% (292/879) | 24.7% (222/900) | 4 | 185 | 511 |
| h = g on prose, a on names | NL held-out | 5.6% (4/71) | 51.2% (110/215) | 37.0% (17/46) | 6 | 34 | 29 |
| | name | = a | | | | | |

On **NL all** (both halves; out of sample only for a, c, d, f, since they
fit nothing): a misled 14.9% (20/134), f/c misled 0 with 20 soft-ok and 119
soft-miss, confident wrong unchanged at 51.5% / 41.0%.

Per language, NL held-out (misled / confident wrong pos / absent):

| language | a keep | b NL re-fit | g/h two-tier |
|---|---|---|---|
| go | 31.2% (5/16) / 36.1% / 9.1% | 12.5% (2/16) / 60.7% / 18.2% | 12.5% / 36.1% / 9.1% (16 soft-miss) |
| python | 9.5% (2/21) / 44.7% / 36.4% | 4.8% (1/21) / 53.2% / **90.9%** | 4.8% / 44.7% / 36.4% (10) |
| rust | 14.3% (1/7) / 76.6% / 45.5% | 14.3% (1/7) / 78.7% / 45.5% | 14.3% / 76.6% / 45.5% (1) |
| typescript | 7.4% (2/27) / 51.7% / 53.8% | 0.0% (0/27) / 53.3% / **100%** | 0.0% / 51.7% / 53.8% (7) |

### Reading each option

**(a) Keep.** One in seven NL searches whose right answer is ranked first is
told "no match, fall back to grep". The agent obeys: it greps (several
calls) and may report the thing absent. The name-query guarantee (0.9%)
holds. Survives a model switch in the sense that it is re-fitted as today.

**(b) Re-fit on NL.** The NL floors land far below the shipped ones (python
0.45, typescript 0.41), so the verdict stops firing: misled 14.1% -> 5.6%,
but confident-wrong on absent NL queries 37% -> 65% (python 91%, typescript
100%), and applied to name queries it throws away their guarantee (absent
24.7% -> 44.1%). What the n supports: fit-half rank-1 n is go 14, python 21,
rust **8**, typescript 20; at 3%, `k = floor(0.03 n) = 0` for every one of
them, so each "3% floor" is simply the **lowest single score** observed, and
the expected held-out false alarm of a minimum of n draws is about 1/(n+1) -
11% for rust, 7% for go. A 3% target can be neither fitted nor verified on
this n (a held-out 0/7 would still have a 95% upper bound of 35%). The n supports one
claim - NL right answers score 0.02-0.16 below name right answers, so a
name-fitted floor is the wrong floor for prose - and not a per-language
constant. The pooled fit (0.41) is effectively no floor at all (b').
Does not survive a model switch without a new NL fit on the same small n.

**(c) Soft wording everywhere.** Misled goes to 0 and confident wrong is
unchanged - *if* the agent reads the top row sceptically. On names it buys
5 rescues for 696 wasted reads, weakening a verdict that is right 99% of the
time. Dominated by f.

**(d) Rows marked, no verdict.** Nothing tells the agent *no* any more.
With the mark ignored it is the no-floor baseline: every absent query is
confident wrong (100%), positives 51% -> 67%. With the mark honoured it
collapses into c. It also costs bytes per row (about 18 bytes x 20 rows),
which `NoMatch`'s own doc comment rejected as "paid for per row". Dominated
by c and f.

**(e) Shape-split floors.** Fixes b's damage to names, keeps b's NL trade
(misled 5.6%, absent confident wrong 65%) and b's n problem. Dominated by h,
which gets the same misled rate without the confident-wrong increase.

**(f) Shape-split wording: SOFT on prose below the shipped floor, today's
HARD on names.** NL misled 14.1% -> **0**, NL confident wrong unchanged
(51.2% / 37.0%), names untouched (0.9% / 33.2% / 24.7%). Cost: 63 extra
reads on NL held-out (119 on NL all) to recover 10 (20). No new constant:
the floor table stays as is, so it survives GM-398's model switch unchanged
(the switch re-fits the same floors on names, as it would anyway). Bytes:
the SOFT text replaces the 361-byte HARD text on the pages that get it; a
draft of 214 bytes -
"Every row scored low for its language, so none is a confident match. The
top row may still be right: check it with one read, and if it is not, fall
back to a structural tool or grep rather than rewording the query." -
so prose below-floor pages get cheaper, and no page gains a field. The
GM-389 hint stays off these pages, as today.

**(g/h) Two-tier on prose.** Keeps a HARD "no" for pages far below (under
the NL re-fit) and SOFT between: extra reads 63 -> 34 on NL held-out, at the
price of misled 0 -> 5.6% and a second per-language constant fitted on
n = 8-21 (the minimum-of-n problem of b), re-fitted per model. g (all
queries) adds 185 wasted reads on names for 4 rescues, so h dominates g.
h trades against f rather than dominating it: 29 fewer wasted reads for 4
misled pages, bought with an unverifiable constant.

Dominance: f dominates a, c and d (equal or better on every column except
extra reads, which a and d avoid only by misleading or by not protecting);
h dominates g and e. The real choice is **f vs h**, and on this n it is
f.

### Recommendation: (f)

Prose-shaped queries (any whitespace) whose page is all below its
languages' floors get a SOFT explanation instead of "ignore the rows";
name-shaped queries keep today's HARD verdict unchanged. Floors unchanged.

Risks:

1. **The protection moves into the agent's read.** On NL held-out the SOFT
   pages are right at the top only 10 of 73 times (14%). If an agent takes a
   "may still be right" row after a lenient read, the 63 soft-miss pages
   become confident wrong: worst case positives 51.2% -> 67.0%, absent 37% ->
   100% - i.e. d. f's real value lies between a and d, set by agent
   behaviour this eval cannot measure; S5 re-measures the rates, but only a
   session A/B would show which end agents land on. The wording must not say
   "plausibly matches" or "stop".
2. **The key says `noMatch` while the text says "may be right".** Either a
   new `reason` value on the same key (cheap, but the key name lies) or a
   distinct response-level key for the soft case (honest, a schema change;
   `NoMatch` is referenced only in `similarity.rs` and `search_code.rs`).
   S3 needs this decided.
3. **Whitespace is the shape test.** `parse config` is prose; `parse_config`
   is a name and keeps HARD. The eval split them 2,279/2,279, but agents'
   real queries were not sampled here.
4. **The NL confident-wrong rate (51%) is untouched by every option that does
   not raise misled** - no floor on this model separates wrong NL top answers
   from right ones (S1: 89-93% of wrong rust/typescript NL tops clear it).
   That is a model property for GM-398, not something wording fixes.

Owner's choice: _pending_.

g-mesh calls behind the code facts: `find_references NoMatch` (used by
`similarity::verdict`, `partial_verdict`, `search_code::search_hint`,
`SearchPage`; files `search_code.rs`, `similarity.rs`) and
`find_references BELOW_FLOOR_EXPLANATION` (only `verdict`). `grep` for
`noMatch` in non-code: no agent guidance mentions it (only
`docs/architecture/gm-389-guidance-prefix.md` and tests).

## A/B (S8): agents on a task whose prose queries fall below the floor

g-mesh-bench token-economy, arm `gmesh-configured`, model claude-sonnet-5,
REPS=max, corpus excalidraw `1acf66ed`. Arm A = `release-3.17.0` (`c9e6606`,
reports `g-mesh 3.17.0`), arm B = this branch at `7566680` (not
version-bumped, reports `g-mesh 3.16.0`). Each arm gets its own
`G_MESH_HOME`, and the arms alternate run by run. Scripts are on g-mesh-bench
`chore/GM-434-ab-prose-floor`: `scripts/ab-prose-floor.sh` and
`scripts/ab-prose-floor-summary.py`.

**Task choice.** In 25 arm-A runs, `ex-semantic-collab-conflict-keep-local`,
`gin-semantic-panic-recovery`, `py-semantic-basicauth-header` and
`rs-semantic-detect-binary-content` never got a below-floor page, so they
were dropped. `ex-semantic-arrow-zorder-above-bound` got one in about half
of its runs and is the only task measured.

**Control.** Each `search_code` result in the transcripts was classified.
The arms are told apart:

| arm | runs | runs with a below-floor page | `noMatch` pages | `lowSimilarity` pages | other pages |
|---|---|---|---|---|---|
| A | 35 | 19 | 20 | 0 | 41 |
| B | 30 | 20 | 0 | 20 | 34 |

**Result** (tokens = input + output + cache read + cache creation. Fallback =
Read/Grep/Glob calls after the first below-floor page. CI = bootstrap 95% CI
of B-A):

| subset | arm | n | oracle pass | mean tokens | mean tool calls | mean fallback |
|---|---|---|---|---|---|---|
| all runs | A | 35 | 35/35 | 131,701 | 5.8 | - |
| all runs | B | 30 | 30/30 | 138,553 | 6.7 | - |
| below-floor runs | A | 19 | 19/19 | 155,268 | 6.9 | 4.1 |
| below-floor runs | B | 20 | 20/20 | 153,594 | 7.7 | 5.0 |

B-A over all runs: tokens +6,852 [-34,852, +39,919], calls +0.9 [-0.9, +2.4].
Over below-floor runs: tokens -1,674 [-68,594, +47,674], fallback +1.0
[-1.1, +2.8].

**Reading.** Every CI spans zero, so this A/B shows no measurable
difference in tokens, tool calls or confirming reads. Risk 1 did not happen
here: B's 20 `lowSimilarity` runs all passed the oracle, so no agent took a
soft row as a confident wrong answer. A and B agents behave alike. After a
below-floor page they read about 4-5 files and find the answer either way.
Caveats: this is one task, so the result says nothing about the other
languages. The below-floor subset is chosen by outcome, not by assignment.

**Runs.** 65 in total. Ten result files (40 runs) came from an earlier
attempt that a machine reboot cut off. Each was attributed to its arm by
`gmeshVersion` from the serving binary's serverInfo, and its transcripts
were matched 1:1 in time order. Their `time -p` and uptime logs were lost in
`/tmp`. That attempt's last run, an arm-B run with 4 of 5 transcripts and no
result, was discarded. The remaining 5 runs (B,A,B,A,B, 25 agent runs) were
rerun on 2026-09-29: `real 758.42 user 275.93 sys 61.26`, 126-180 s per
arm run. Load averages were 3.92/25.90/42.96 at the start, still falling
after the reboot, and 7.37/8.62/21.24 at the end. user+sys is about 45% of
real, the rest being the model API wait.
