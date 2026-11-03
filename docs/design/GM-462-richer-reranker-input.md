# GM-462: a richer cross-encoder input (design)

Status: design (GM-462/S1, 2026-11-03, branch `feat/GM-462-richer-reranker-input`
off `release-4.25.0`). Nothing has been scored. Every token count below comes
from `eval/embedding/gm462_token_lengths.py`, which reads stored rankings,
the snapshot DBs, the corpus checkouts and the tokenizer only:

```sh
python3 eval/embedding/gm462_token_lengths.py <main checkout>/eval/embedding/work
```

## Question

GM-443/S8 found that ms-marco-MiniLM-L6-v2 gives a bare signature a logit near
-10, right or wrong. 5 of the 6 TypeScript Q5 flips were signature-only tops,
and 10 of the 29 held-out TypeScript rank-1 tops had no doc comment. The
cross-encoder reads only `text_to_embed` (trimmed doc, a blank line, the
signature). For an undocumented symbol that is one identifier and a
parameter list. The tokenizer lowercases, so `releaseTaskCounts` reaches
WordPiece as `releasetaskcounts`.

GM-462 asks whether a richer text helps the cross-encoder. The candidate
additions are the file path, the enclosing class or module, the identifiers
split into words, and the first lines of the body. The text stays under the
512-token pair cap. This note specifies that text, how to measure it and
what decides the result. It is research only: no product code changes.

**What this can and cannot move.** The shipped rerank is F4 (ADR 0016). The
cross-encoder only orders the window, and the noMatch verdict is
int8-structured's own un-reranked top-1 cosine. So a new CE input cannot
change Q5 or NL CW on absent queries: both stay at +0.0 by construction. It
can move r@10, MRR, CW on positives (a cleared query whose top row changes)
and misled on queries it newly brings to rank 1. The S8 problem in the F4
world is ranking: a right but undocumented row that the CE pushes down.

## 1. Setup: which F4

The task says "GM-443's K=50 candidates over int8 (the F4 setup)". Since
then two things changed. 3.17.0 embeds the structured text (ADR 0012). F4
shipped at K=30, beta=80, over int8-structured, with the CE reading the
structured text (ADR 0016; `core/src/mcp/search_code.rs::window_texts`).
GM-460's v2 eval runs that same arm (its section 7 step 4).

| | **A: shipped F4 (recommended)** | B: GM-443 S11 F4 |
|---|---|---|
| recall stage | int8-structured top-30 (`work/runs-gm465/jina-v2-base-code-int8-structured`) | int8 full-text top-50 (`work/runs/jina-v2-base-code-int8`) |
| beta | 80 (FIT-tuned in GM-464) | 40 (FIT-tuned in S4) |
| floors | shipped 0.57 / 0.59 / 0.57 / 0.53 | old 0.57 / 0.57 / 0.55 / 0.53 |
| base CE text | structured (`text_to_embed`) | full (doc + signature, untrimmed) |
| cached CE scores | `work/rerank_cache/gm464-ce-minilm.<corpus>.a0168968689691f2.json`, text-keyed; covers all 68,370 window pairs (checked: 0 missing) | `work/rerank_cache/ce-minilm.<corpus>.pairs.<key>.json`, node-keyed, full text |
| control row | GM-464 S1 "F4s K=30 beta=80 (FIT-tuned), CE on structured text" | GM-443 S11 "F4" row |
| pairs per arm, all 2,279 queries | 68,370 | 113,950 |

A is what `search_code` serves, so a result on A can become a product
change. B answers the question exactly as it was worded, but its recall
stage and floors are no longer shipped. **This note designs A.** B fits the
same script through a `--setup gm443-k50` switch (section 9). It costs
about 1.7x the pairs and is run only if the owner picks it (decision 1).

## 2. The enriched text

### Fields

Five fields, each from data the snapshot DB or the corpus checkout already
holds. Words come from `split_words`: the regex
`[A-Z]+(?=[A-Z][a-z])|[A-Z]?[a-z]+|[A-Z]+|[0-9]+` over the string, each match
lowercased and joined by one space. For example `HTTPServer` gives
`http server`, `safeInt8` gives `safe int 8` and `send_quit` gives
`send quit`.

| switch | field | content | source columns (eval DB) |
|---|---|---|---|
| (always) | **base** | today's CE text: `form_text("structured", doc, sig, full)`, which equals Rust `text_to_embed` | `docComment`, `signature` |
| `E` | **enclosing** | `split_words(nativeKind or kind)`, then `split_words` of the qualified-name prefix: `qualifiedName` minus its trailing `name`, or the whole `qualifiedName` when it does not end in `name` | `nativeKind`, `kind`, `qualifiedName`, `name` |
| `I` | **identifiers** | `split_words(name)`, then `split_words` of each distinct identifier in the signature. Identifiers are matched by `[A-Za-z_][A-Za-z0-9_]*`, in source order, minus `SIGNATURE_KEYWORDS` (text.rs) and minus `name` itself. Cut at 48 tokens on a word boundary. | `name`, `signature` |
| `P` | **path** | `split_words(filePath)` | `filePath` |
| `B` | **body** | Only for `kind` Function or Type. Source lines `startLine+1 ..= endLine` (0-based, so the declaration's own first line is skipped). A leading Python docstring is skipped. Lines holding only `{}()[];,` and whitespace are dropped. Whitespace is collapsed and each line is cut at 200 characters. Whole lines are added while the field stays within **96 tokens**, stopping at the first line that does not fit. | `startLine`, `endLine`, `language` + `work/corpora/<corpus>/<filePath>` |

Examples of the enclosing field: `Context.Next` gives `method context`;
`walk::Worker::send_quit` gives `method walk worker`;
`TaskConflictError#constructor` gives `constructor task conflict error`; and
a top-level TS `requireTask` (nativeKind `function`) gives `function`.

The module (`container`) is not a separate field. For Rust and Python it
repeats the path. For Go it is an import path such as
`github.com/gin-gonic/gin`, which is mostly noise. TypeScript leaves it
empty.

### Order and layout

```
<E>\n<I>\n<P>\n<base>\n<B>
```

An empty field is omitted, together with its newline. The tokenizer reads
`\n` as whitespace, so the layout costs no tokens. The order follows how
the pair is truncated. The Python and Rust tokenizers both truncate
longest-first (`rerank.rs::load`, `TruncationParams` default;
`rerank_eval.CrossEncoder`, `enable_truncation(512)`), and the text is
always the longer side (queries max out at 33 tokens). So an over-long pair
loses the text's tail first: the body, then the end of the base. The short
header fields go first, so they always survive.

**All switches off** gives exactly `<base>`, byte for byte. That is the
control in section 5.

### Budget (measured, not guessed)

These are ms-marco-MiniLM tokens on the text side, with no special tokens,
over the 11,750 distinct nodes in int8-structured's top-30 across every
query. The K=50 int8 window (13,266 nodes) differs by at most 3 tokens at
p99.

| field | p50 | p90 | p99 | max | mean |
|---|---|---|---|---|---|
| base (today) | 24 | 78 | 171 | 1013 | 35.9 |
| E | 3 | 7 | 13 | 21 | 3.8 |
| I (uncapped) | 7 | 17 | 42 | 184 | 9.1 |
| P | 7 | 11 | 15 | 20 | 7.3 |
| body, first 5 lines | 44 | 96 | 145 | 373 | 46.6 |
| body, first 10 lines | 64 | 161 | 250 | 484 | 73.7 |
| body, 48-token budget | 33 | 46 | 48 | 48 | 28.0 |
| **body, 96-token budget** | 66 | 93 | 96 | 96 | 53.5 |
| header (E + I + P) | 22 | 33 | 57 | 201 | 23.2 |
| all on, body 48 | 80 | 144 | 250 | 1260 | 88.0 |
| **all on, body 96** | 115 | 186 | 295 | 1310 | 113.6 |

- Query lengths: p50 5, p99 28, max 33 tokens. A pair adds 3 special
  tokens.
- Pairs that pass 512 with the longest query: 4 today, 14 with everything
  on (body 96). Those few are the nodes whose base alone runs to hundreds
  of tokens. Truncation there cuts body, then base, never the header.
- 4,281 of the 11,750 nodes are documented, and 7,469 are signature-only.
  Signature-only nodes by language: go 1,105/1,722, python 661/1,003, rust
  3,461/6,257, typescript 2,242/2,768.
- No source file is missing: all six checkouts sit at their snapshot
  revision, with clean worktrees (checked).

Why a 96-token body budget: it covers about 5 lines at the median. It keeps
p99 of the whole text under 300 tokens, which leaves about 180 tokens of
headroom. The mean pair grows from about 46 to about 124 tokens (2.7x).
A 48-token budget costs 1.9x. Section 4 says when to try it.

### Where the fields live in the product (for a later port, not this task)

- `nodes` (core/src/storage/schema.rs:220) already has `kind`, `name`,
  `qualifiedName`, `qualifiedPath` (segments joined by U+001F), `nativeKind`,
  `filePath`, `container`, `startLine` and `endLine`. A Rust builder could
  read E, I and P in the same `SELECT` that `window_texts` runs today.
  `qualifiedPath` is the robust way to get the enclosing prefix there. The
  eval snapshots predate that column, so the eval builder strips the name
  suffix instead (must-confirm M4).
- **The body is not stored anywhere in the index.** The product would have
  to read 30 source files per reranked query (stale against an unsaved or
  changed file, plus I/O on the hot path), or store a body snippet per node
  (a schema change plus a re-index). If B wins, that cost decides the port,
  not this measurement (decision 4).

## 3. Arms

Text-keyed caching dedups for free: two arms that give the same text for a
(query, node) pair share one score.

| arm | switches | queries | new pairs (A, current set) |
|---|---|---|---|
| **off** (control = F4 today) | none | all | 0 (cached) |
| **all** | E I P B | all (NL + name) | ≤ 68,370 |
| −E | I P B | authored NL only (500) | ≤ 15,000 |
| −I | E P B | authored NL only | ≤ 15,000 |
| −P | E I B | authored NL only | ≤ 15,000 |
| −B (header only) | E I P | authored NL only | ≤ 15,000 |

Leave-one-out arms answer which addition carries the effect. They run on
authored NL only (FIT and held-out), because that is where the gates and
beta tuning live; name queries add 1,779 queries x 30 pairs per arm and
answer no question here. A full 2^4 factorial is not worth 2.7x the pairs.

**Cost.** GM-443 S12 measured 9.62 ms per pair at default threads on an
idle machine, at today's pair length. At 2.7x the tokens, assume 20-26 ms.
That gives about 128k pairs, or 45-55 min idle, on the current set. On v2
(section 7) it is about 300k pairs, or 1.7-2.2 h. The measure slice's dry
run prints the real per-pair time and an ETA before the full pass.

## 4. Beta

The richer text shifts the logit distribution: S8's undocumented TypeScript
tops had a median logit of -8.58. So beta = 80 is not assumed to stay right. Every arm is reported
at:

- **beta = 80** (shipped; nothing refit);
- **its FIT-tuned beta**: S4's objective (best FIT r@10, then MRR, then the
  smaller beta) over `R.BETAS`, on the FIT half only. This is the arm's
  verdict row.

There is also a held-out sweep table for the all arm, as GM-464 has. If
"all" helps at the tuned beta, the measure slice adds one row: all with
the 48-token body. That row shows how much of the gain survives at 1.9x
the cost instead of 2.7x. It runs on authored NL only, about 15k pairs.

## 5. Controls

Any failure is a STOP. They run before the full CE pass, except C4 and C5,
which need cached scores only.

| # | what | must hold |
|---|---|---|
| C0 | builder unit tests (`test_gm462_ce_text.py`) | pass. They cover: split_words cases (`HTTPServer`, `safeInt8`, `send_quit`, `parseURL2Fast`); enclosing for one node per language (Go method, Rust trait-impl method, Python method, TS `#constructor`); Python docstring skip (one-line, multi-line, `r"""`); the punctuation-line drop; the 200-char cut; the body budget stopping at the first line that does not fit; the I cap at 48 tokens; no body for File/Module/Variable; empty fields omitted; all off == base for a documented node, a signature-only node and a doc-only node |
| C1 | all off == base, on real data | for every node in every window, `build(all off)` is byte-equal to the structured text that `gm464_ce_structured.add_structured` computes. Current set: 11,750 of 11,750 |
| C2 | the cache covers the control | for every (query, window node) pair, sha16(all-off text) is in the gm464 cache file `gm464-ce-minilm.<corpus>.a0168968689691f2.json`. Current set: 68,370 of 68,370 (already checked) |
| C3 | the all-off scores reproduce when rescored | 200 all-off pairs (`random.Random(462)` over all window pairs) are rescored through this script's scoring path and compared with the gm464 cache. **Expected max\|diff\| = 0** (GM-464 C2 got 0 over 120 pairs on the same model file, sha256 `5d3e70fd...4d4a`). STOP above 1e-5, and report the value either way. A diff between 0 and 1e-5 means the ONNX Runtime or tokenizers version moved since GM-464: name both versions in the results |
| C4 | the all-off arm reproduces F4 | the off row equals GM-464 S1's "F4s K=30 beta=80 (FIT-tuned), CE on structured text" at the published precision. Held-out r@10 0.676 [0.625, 0.727], MRR 0.456 [0.409, 0.502], name 0.970 / 0.880. vs int8-structured: +3.7 [+1.8], +0.047 [+0.021], Q4 -3.0 [-0.2], Q5 +0.0 [+0.0]. vs fp32: +4.3 [+1.2], +0.033 [+0.004], Q4 -6.2 [-2.3], Q5 -2.5 [+0.0]. GM-434: NL 14% (11/77) / 46% (99/215) / 22% (10/46), name 3% (22/714) / 18% (156/879) / 20% (184/900). Flips NL 6 / 14 (n=165), name 1 / 108 (n=848). FIT-tuned beta 80 |
| C5 | rerank off == int8-structured | as `gm464_ce_structured` C4: every outcome and every reported cell |
| C6 | C1-C4 can fail | the all arm's text differs from base on 100% of window nodes (E is never empty), and its row differs from the off row in at least one cell. Print both counts |

C1 and C2 are exact identities. C3 is the only check that actually runs the
model, and it is what makes "reproduces the cached CE scores" more than a
cache lookup. C4 ties the new script's metric path to the published numbers.

## 6. Metrics, buckets and the verdict rule

**Per arm** (the same tables as `gm464_ce_structured.render`):

- held-out NL r@10 and MRR with [lo, hi], plus name r@10 / MRR;
- D9 Q1-Q5 vs int8-structured at the shipped floors, and vs fp32 at its D6
  floors;
- GM-434 option-a columns, NL and name, overall and per language;
- flips among cleared positives vs int8-structured.

Rows: fp32, int8-structured (no rerank), off (= F4), all, and the four LOO
arms, each at beta 80 and at its FIT-tuned beta.

**The comparison that decides.** Paired, held-out NL, all minus off, at
each arm's tuned beta: Δr@10 and ΔMRR with [lo, hi], from `R.gates(off,
all, R.ho, rf=shipped)`, as GM-464 does for full vs structured. The same
pairing is reported for each LOO arm vs all.

**Buckets.** Each held-out NL positive is labelled by its expected
symbol(s):

- documented: every expected node has a non-empty trimmed doc;
- signature-only: no expected node has one;
- mixed: the rest (reported, expected to be small).

For each bucket, overall and for TypeScript separately:

- n;
- r@10, MRR, and the paired all-minus-off Δ with [lo, hi];
- the median CE logit of the best-ranked expected node inside the window,
  for off and for all;
- the count of expected nodes with logit < -8, for off and for all.

The logit columns test S8's mechanism directly. The rank columns test
whether fixing that mechanism matters.

**Verdict rule, fixed before measuring:**

- **helps**: paired ΔMRR or Δr@10 lower bound ≥ 0 with a point estimate >
  0, the other metric's point estimate ≥ 0, and D9 passes vs
  int8-structured and vs fp32;
- **hurts**: either upper bound < 0;
- otherwise **no effect**.

The signature-only bucket is reported but cannot carry the verdict alone:
on the current set its held-out n is small. If "helps" holds, the results
doc names the cheapest LOO arm (fewest tokens) whose paired Δ against all
is within noise. That arm is the candidate a port would ship.

## 7. Eval set: v2 first, current set as fallback

The plan runs this measure after GM-460's measure, on the enlarged set
(`docs/design/GM-460-enlarged-nl-eval.md` on `feat/GM-460-enlarge-nl-eval`,
not yet approved).

**Use v2 when all of these hold at the start of GM-462's measure:**

- GM-460's freeze commit is merged;
- its controls C0-C4 passed;
- its F4 arm (GM-460 decision 3) has run, or the owner accepts that GM-462
  scores the off arm for the new queries itself.

**Otherwise fall back to the current set**: eval dir `eval/embedding`, work
`<main checkout>/eval/embedding/work`. That set is fully designed and
checked here (C2 coverage is already 100%).

What changes on v2:

| | current set | v2 |
|---|---|---|
| eval dir / work | `eval/embedding` / `<main>/eval/embedding/work` | `eval/embedding/v2` / `eval/embedding/v2/work` (`--eval-dir`, `--work`) |
| int8-structured rankings | `work/runs-gm465/jina-v2-base-code-int8-structured` | `v2/work/runs/jina-v2-base-code-int8-structured` (`--st-runs`) |
| queries | 500 authored (215 held-out positives, 46 absent) + 1,779 mechanical | about 1,650 authored (about 660 held-out positives, 166 absent) + the same mechanical |
| off-arm cache | gm464 cache, complete | `v2/work/rerank_cache/gm464-*` as GM-460 step 4 leaves it. C2 must reach 100%, or the missing off pairs are scored first and C2 is reported as "covered after scoring N" |
| C4 target | GM-464 S1 row (above) | (a) GM-460's published F4 row on v2, and (b) the off arm on the GM-398-id subset (`gm460_subset.py`) reproduces the GM-464 row above |
| extra view | none | GM-460's **strict held-out** view (held-out queries sharing an expected symbol with a FIT query dropped), reported for the deciding pairing |
| beta | FIT-tuned on the FIT half (239 authored NL) | FIT-tuned on v2's FIT half; may differ from 80. Both rows are reported |
| cost | about 128k pairs | about 300k pairs |

The builder, fields, budget and verdict rule do not change between the two
sets. The script takes the set as arguments only.

## 8. Progress output for the CE pass

The CE pass is the only long step. It writes one overwritten line to
`~/.claude/progress/GM-462-measure.txt`, the same format as gm464's
`progress()`:

```
GM-462 <stage>: <done>/<total> (<pct>%) elapsed <s>s eta <s>s load <1m 5m 15m>
```

It writes at every 10% of the pairs and at least every 5 min, whichever
comes first, and once at the start and end of each stage. Stages are dry
run, C3 rescoring, CE scoring <arm>, metrics. The cache is saved at each
progress write, so a killed run resumes. The measuring agent never reads
this file. Its one wakeup is the background script's exit, and the script
writes a compact summary file (controls, verdict rows, ETA vs actual).

## 9. Edit map (code and tests slices)

No Rust changes. New files, all Python under `eval/embedding/`:

| file | what |
|---|---|
| `gm462_ce_text.py` (new) | `split_words(s)`, `enclosing(kind, native_kind, name, qualified_name)`, `identifiers(name, signature, tok, cap=48)`, `body(lines, start, end, kind, language, tok, budget=96)`, `build(row, src_lines, switches, tok, body_budget=96) -> str`. `switches` is a frozenset of `"E", "I", "P", "B"`. `row` holds the eval DB columns. `base` comes from `gm423_token_lengths.form_text("structured", ...)`. `SIGNATURE_KEYWORDS` is copied from core/src/embedding/text.rs:125-128, with a comment naming the source. The draft rules in `gm462_token_lengths.py` (`words`, `sig_idents`, `body_lines`, `capped`) are the starting point; the design table in section 2 governs where they differ |
| `test_gm462_ce_text.py` (new) | C0, in the style of `test_gm441_rerank.py` (unittest, no work dir, tokenizer stand-in that counts whitespace words) |
| `gm462_ce_enriched.py` (new) | the measure driver. It imports from `gm464_ce_structured`: `c1_fixture` (61-81), `add_structured` (83-102, which needs an `--st-runs` root instead of the hard-coded `runs-gm465`; pass it in, do not edit gm464), `Cache` (105-136, read-only use for the off arm), `flips` (378-397) and `render` (400-427). It imports from `rerank_eval`: `Corpus` (119), `CrossEncoder` (373-408), `outcome` (168), `reranked` (479), `first_rank` (490), `pooled` (305), `gates` (313), `gm434` (344), `summarize` (513), `shipped_floors` (364), `bootstrap` (203). From `rerank_q5_diag` it imports `fixed_floors_gates` (280). It **copies** gm464's nested `f4` (241-260), `row` (262-270), `sig` (272-274) and `fit_obj` (303-316) to module level with a header naming the source, rather than hoisting them in gm464: gm464 is a frozen result script, and changing it would need its own byte-identical rerun. New pieces: an `EnrichedCache` (file `rerank_cache/gm462-ce-minilm.<corpus>.<key>.json`, key = sha256 of `[model sha, 512, "gm462 text-keyed v1"]`, lookup gm462 file first, then the gm464 file read-only); `need_pairs(arm)`; `score_missing` with section 8's progress; controls C1-C6; buckets; the paired tables. Arguments: `--work`, `--eval-dir` (default `eval/embedding`), `--st-runs` (default `runs-gm465/jina-v2-base-code-int8-structured`), `--k 30`, `--setup {shipped,gm443-k50}`, `--arms`, `--body-budget`, `--dry-run CORPUS`, `--table`, `--summary` |
| `gm462_token_lengths.py` (this slice) | kept as the design probe |
| `docs/results/gm-462-richer-reranker-input.md` (measure slice) | method, controls, tables, verdict, owner's decision |

`--setup gm443-k50` (decision 1, option B) swaps the recall lists to
`work/runs/jina-v2-base-code-int8`, K to 50, beta to 40, the floors to
`OLD_FLOORS`, base to the full text, and the off-arm cache to the GM-443
node-keyed pair cache. C4's target becomes GM-443 S11's F4 row: r@10 0.693
[0.642, 0.744], MRR 0.477 [0.430, 0.523], vs int8 +4.8 [+1.2], +0.058
[+0.027], Q4 -2.8 [+0.2].

Product files a later port would touch (informational, out of scope):

- `core/src/mcp/search_code.rs:522` `window_texts`: the `SELECT` and the
  builder call;
- `core/src/embedding/text.rs:99` `text_to_embed`, next to which a Rust
  `rerank_text` would sit, pinned to the Python builder by a shared JSON
  fixture as `testdata/structured_text.json` does;
- `core/src/embedding/rerank.rs:61` `MAX_PAIR_TOKENS` (unchanged).

## 10. Must confirm

**Verify slice:**

- V1: C0 passes, and each C0 case fails under a one-line revert of the rule
  it pins (the standing controls rule: 6-8 reverts, grouped in one run).
- V2: the builder's field rules match section 2's table exactly. In
  particular, startLine is 0-based (checked here on
  `task-tracker-mcp:requireRelease`, startLine 103 = file line 104 =
  `export function requireRelease(`), and the body starts after line
  `startLine`.
- V3: `gm462_ce_enriched.py --dry-run gin` passes C1, C2 and C3 on one
  corpus and prints per-pair ms.
- V4: gm464 and rerank_eval are imported, not modified (`git diff
  release-4.25.0 -- eval/embedding/gm464_ce_structured.py
  eval/embedding/rerank_eval.py` is empty).

**Measure slice:**

- M1: C1-C6 pass before the full CE pass. C3's max|diff| and the
  onnxruntime and tokenizers versions are recorded (GM-464 S1's versions
  are not in its results doc; if they differ and C3 is non-zero, say so).
- M2: the eval-set choice of section 7, with its preconditions checked and
  stated.
- M3: the dry run's per-pair ms and ETA, with `uptime` and the load gate.
  No timing is reported as a latency result; latency is a port's question.
- M4: the enclosing prefix is right for the window's nodes. Count nodes
  whose `qualifiedName` does not end in `name`, per language. Print 5
  examples per language of the E field.
- M5: the re-read of the GM-460 status (frozen? F4 arm run?) at the start.

## Risks

- **The body field is not portable for free.** The index stores no source
  text (section 2). A win carried by B costs a schema change or disk reads
  on the hot path. The LOO arms exist to show whether E+I+P alone carry it.
  E, I and P are cheap to port.
- **Latency.** The mean pair grows 2.7x (1.9x with a 48-token body). The
  product rerank p50 is 510 ms today at 4 threads (gm-464-rerank-latency).
  A port would need its own latency gate.
- **Small buckets on the current set.** 60 held-out TypeScript positives
  (GM-443 S11 per-language table), fewer signature-only ones. On the fallback set the
  bucket results are directional. The verdict rests on the pooled paired
  Δ.
- **Python-only builder.** Parity with a later Rust builder is a port's
  job (shared fixture). This task measures the idea, not shipped code.
- **The model may simply not read code.** If MiniLM still scores split
  identifiers low, the result is "no effect", and GM-463 (a reranker
  bake-off) is the next lever. That is a valid outcome.

## Decisions for the owner

1. **Setup**: A, the shipped F4 (int8-structured top-30, beta 80,
   structured base) (recommended: a win can ship, and it matches GM-460's
   v2 arm). Or B, GM-443's K=50 over int8 full text, as the task worded it.
   Or both (1.7x more pairs for B).
2. **Ablation scope**: all on, on every query, plus four leave-one-out arms
   on authored NL (recommended: shows which field carries the effect, about
   128k pairs). Or all on only (about 68k pairs, no attribution). Or a full
   2^4 factorial on NL (about 2.7x the LOO cost).
3. **Body budget**: 96 tokens, with a 48-token follow-up row if "all" helps
   (recommended). Or 48 only (cheaper, may under-read the body).
4. **What a "helps" verdict leads to**: a follow-up port task gated on
   latency, porting E+I+P first and B only if the LOO shows it carries the
   gain (recommended). Or no port until GM-463's bake-off.

## Appendix: code facts and the calls that produced them

| fact | from |
|---|---|
| `text_to_embed` lives in `core/src/embedding/text.rs:99` (not pipeline.rs). Its callers: `node_text`, `cli::embed_eval::Node::text_for`, `mcp::search_code::window_texts`, and tests in text.rs, pipeline.rs and rerank.rs | g-mesh `find_callers` (symbol_id of `embedding::text::text_to_embed`) |
| `window_texts` is the CE input builder. Its only caller is `first_page_reranked` | g-mesh `find_references` `window_texts` |
| `CrossEncoder::score` has no production caller other than through the `Scorer` trait (`Reranker::order`); its direct callers are tests | g-mesh `find_callers` (symbol_id of `<CrossEncoder as Scorer>::score`) |
| rerank.rs constants: `WINDOW` 30, `BETA` 80, `MAX_PAIR_TOKENS` 512, `CHUNK` 16 | g-mesh `get_file_outline` core/src/embedding/rerank.rs, then sed on the lines |
| `qualifiedPath` encoding helper: `storage::qualified_path::encode` | g-mesh `search_code` |
| The `nodes` columns (no body or source text stored) | grep on core/src/storage/schema.rs (one known file) |
| The eval snapshot DBs lack `qualifiedPath`; startLine is 0-based; the corpus checkouts are at their snapshot revisions, clean | sqlite3 and git on `<main>/eval/embedding/work` |
| The gm464 cache key is `a0168968689691f2`; coverage is 68,370 / 68,370 | computed by a scratch script that reuses gm464's key formula |

## Owner decisions (2026-11-03)

1. Setup: "A: отгружаемый F4 K=30 (Recommended)". `--setup gm443-k50` is
   left as a stub that raises NotImplementedError.
2. Ablation scope: "Всё вкл + 4 leave-one-out (Recommended)".
3. Body budget: "96 + строка с 48 при эффекте (Recommended)".
4. On "helps": "Задача на перенос (Recommended)".
