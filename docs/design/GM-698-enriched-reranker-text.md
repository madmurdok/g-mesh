# GM-698: port the enriched reranker text to the product (design)

Slice GM-698/S1. Source: GM-462 (`docs/results/gm-462-richer-reranker-input.md`,
verdict **helps** on v2; field rules in
`docs/design/GM-462-richer-reranker-input.md` section 2; reference
implementation `eval/embedding/gm462_ce_text.py`). Read-only analysis at
ed08d04 (release-4.26.0, version bump); no builds, no runs.

## Verdict

Port **E + P + B(48)** (no I) as a new Rust module that builds the
cross-encoder text at query time, in `search_code::window_texts`, from
columns the `nodes` table already has plus the node's source file read from
disk. **No schema change, no re-index, no change to vectors.** Re-tune
`rerank::BETA` on v2 FIT with a new eval arm that is exactly the shipped text
(E P B48, all queries), before the Rust code needs the number.

Three facts the owner should see before approving:

1. **The arm we ship was never measured.** GM-462's `all-b48` row is
   E **I** P B48 and its name queries kept the off order. E P B48 without I,
   with name queries rescored, is a new arm. The `-I` arm (body 96) suggests
   it is at least as good (dMRR +0.013 [+0.006, +0.020] for removing I), but
   that is evidence, not a measurement. Slice S2 measures it.
2. **The latency cost is large.** The rerank costs ~450 ms p50 today
   (ADR 0016: 510 ms vs 65 ms off). Mean text grows 38 -> ~82 tokens (86 for
   all-b48 minus ~4 for I), so a pair grows ~49 -> ~93 tokens with query and
   specials. Expect roughly **+350-450 ms p50 per first page** (to ~0.9 s).
   The S7 gate measures it; the owner accepts or rejects the number.
3. **The rerank score distribution changes** (enriched text and beta 80 ->
   ~20). Today's refusal verdict ignores it; if GM-697 picks an F4/rerank
   score as its refusal signal, its threshold must be calibrated on GM-698's
   text and beta, not today's. Section 6.

## 1. Where the CE text is built today

There is exactly one product path that feeds the cross-encoder:

```
mcp::GMeshMcpServer::search_code            core/src/mcp/mod.rs:929-951
 -> search_code::handle_off_worker           core/src/mcp/search_code.rs:292-305
 -> search_code::handle                      core/src/mcp/search_code.rs:316-404
    scorer = embedding.reranker().scorer()   (first, complete page only)
 -> search_code::first_page_reranked         core/src/mcp/search_code.rs:481-517
    texts   = window_texts(conn, rows[..30])
    cosines = rows[..30].score
 -> rerank::Reranker::order                  core/src/embedding/rerank.rs:510-541
 -> Scorer::score  (CrossEncoder impl)       core/src/embedding/rerank.rs:190-251
 -> rerank::blended_order (logit + BETA*cos) core/src/embedding/rerank.rs:125-141
```

`window_texts` (search_code.rs:522-533) runs
`SELECT docComment, signature FROM nodes WHERE id = ?1` per row and returns
`text_to_embed(doc, sig)` (text.rs:99): the structured text the embedding
model also embeds. Nothing else builds CE text: `Scorer::score` has one
production caller (`Reranker::order`), which has one caller
(`first_page_reranked`), which has one caller (`handle`) (appendix, calls
1-3). `pipeline.rs`'s own `text_to_embed`/`node_text` path is the
**embedding** text and is not touched by this task.

## 2. Edit map

### Change

| file | symbol (1-based lines) | change |
|---|---|---|
| `core/src/embedding/ce_text.rs` (new) | - | The builder (section 3): `split_words`, `enclosing`, `path_words`, `signature_end`, `body_lines`, `body`, `ce_text`, `source_lines`. `pub(crate)`. Constants `BODY_BUDGET = 48`, `LINE_CHARS = 200`, `BODY_KINDS = ["Function", "Type"]`. |
| `core/src/embedding/mod.rs` | module list, 19-24 | `pub(crate) mod ce_text;` |
| `core/src/embedding/rerank.rs` | `Scorer` trait, 77-79 | add `fn count_tokens(&self, text: &str) -> usize;` (decision D3) |
| `core/src/embedding/rerank.rs` | `impl Scorer for CrossEncoder`, 190-251 | implement `count_tokens`: `self.tokenizer.encode(text, false)` id count |
| `core/src/embedding/rerank.rs` | `BETA`, 57 | the FIT-tuned value from S2 (expected 20) |
| `core/src/embedding/rerank.rs` | tests: `Unused` 671-676; fixture tests 586-601; `the_real_model_reproduces_the_reference_logits_and_order` 882-~925 | `Unused::count_tokens`; the real-model test builds its texts with `ce_text` (it builds them itself today with `text_to_embed`) |
| `core/src/mcp/search_code.rs` | `window_texts`, 522-533 | select `docComment, signature, kind, nativeKind, name, qualifiedName, filePath, startLine, endLine, language`; read each distinct `filePath` once per call (`source_lines`, a `HashMap` local to the call); return `ce_text(...)`. New params: `project_root: Option<&Path>`, `tok: &dyn Fn(&str) -> usize` (from `scorer.count_tokens`) |
| `core/src/mcp/search_code.rs` | `first_page_reranked`, 481-517 | pass `project_root` and the scorer's counter into `window_texts` |
| `core/src/mcp/search_code.rs` | `handle`, 316-404; `handle_off_worker`, 292-305 | new param `project_root: Option<&Path>` (`None` = no body field) |
| `core/src/mcp/mod.rs` | `search_code` tool, 929-951 | pass `self.registry.project_root().to_path_buf()`, as `find_definition` does at 742 |
| `core/src/mcp/search_code.rs` tests | `handle_reports_..._no_embedding_model_is_loaded` (~749), `..._model_is_unavailable` (~767), `a_query_related_to_a_docstring_...` (~971) | add `None` for `project_root` |
| `core/src/mcp/search_code_rerank_tests.rs` | `call` (89), `a_partial_page_is_not_reranked` (527), `search_code_embeds_a_prefixed_query_as_typed` (680), `a_partial_page_does_not_spend_the_hint` (856); `Stub` (263-270), `Capture` (593-603) | `None` root; `count_tokens` on both stubs (whitespace words) |
| `core/src/mcp/search_code_rerank_tests.rs` | `store_for_parity` 607-625, `the_handler_scores_the_texts_the_reference_scored` 634-676, `ParityRow` 577 | insert each row's real `kind/nativeKind/name/qualifiedName/filePath/startLine/endLine/language` (today hard-coded `'Function', 'a.src', 1, 3, 'typescript'`), write each row's stored source span into a tempdir used as `project_root` |
| `core/src/embedding/testdata/ce_text.json` (new) | - | builder fixture (section 4) |
| `core/src/embedding/testdata/rerank_parity.json` | - | regenerated: enriched texts, new logits, new order, new `beta` |
| `eval/embedding/gm698_ce_text_fixture.py` (new) | - | writes `ce_text.json` from `gm462_ce_text.build` |
| `eval/embedding/gm464_rerank_fixture.py` | `BETA` 35, row dict 77-80 | text = `T.build(row, src, "EPB", tok, 48)`; rows carry the node columns and source span; `BETA` from S2 |
| `eval/embedding/gm462_ce_enriched.py` | `ARMS` 74-82 | new arm `"ship": (frozenset("EPB"), 48, False)` (all queries) |
| `eval/embedding/test_gm462_ce_text.py` | - | `SIGNATURE_KEYWORDS` sync test (section 4) and a fixture check |
| `docs/adr/0042-enriched-reranker-text.md` (new) | - | amends ADR 0016's "the cross-encoder reading the same structured text" and `80 * cosine` (the README's "next free number" line says 0033 but 0041 exists; use 0042) |
| `docs/architecture/gm-464-ce-rerank.md` | lines 7, 40 | `80` -> new beta, text -> enriched |

### Read for context (do not change)

`core/src/embedding/text.rs`: `text_to_embed` (99-~118), `SIGNATURE_KEYWORDS`
(126-129), `structured_doc` (267). `core/src/mcp/source.rs`: `read_span_within`
(91-~125), the existing project-file reader (not reused, see D2).
`core/src/storage/schema.rs:220-250` (`nodes` columns). `eval/embedding/gm462_ce_text.py`
(whole, 201 lines: the spec). `docs/design/GM-462-richer-reranker-input.md`
section 2.

### Not touched

`embedding/pipeline.rs` (embedding text, vectors, cache fingerprint),
`storage/schema.rs` (no new column), `cli/status*`, `cli/model.rs`,
`daemon/mod.rs`, `session_hints.rs` (they reference the reranker's state, not
its text; appendix, call 9), `mcp/similarity.rs` (the verdict).

## 3. The builder

Layout, as GM-462: `<E>\n<P>\n<base>\n<B>`, an empty field dropped with its
newline. `base` = `text_to_embed(doc, sig).unwrap_or_default()` (today's text).
The Rust builder has **no switches**: it always builds E P B48. Rules are the
Python's, byte for byte; the subtle ones a port gets wrong:

| rule | Python | Rust must |
|---|---|---|
| words | `[A-Z]+(?=[A-Z][a-z])\|[A-Z]?[a-z]+\|[A-Z]+\|[0-9]+`, lowercased | hand-written ASCII scanner (no lookahead in the `regex` crate); the fixture pins `HTTPServer`, `safeInt8`, `send_quit`, non-ASCII |
| E kind | `nativeKind or kind` (empty string falls back) | treat `NULL` and `""` alike |
| E prefix | `qualifiedName` minus a trailing `name`; whole `qualifiedName` if it does not end in `name` or `name` is empty | same; **not** `qualifiedPath` (GM-462 suggested it, but it would break byte equality with the measured text) |
| file read | `read_text(errors="replace")` (universal newlines: `\r\n` and lone `\r` become `\n`), then `split("\n")` | `String::from_utf8_lossy`, replace `\r\n` then `\r` with `\n`, `split('\n')`; **not** `source::read_span` (it refuses invalid UTF-8 and spans past EOF) |
| span past EOF | clipped silently (`min(end, len-1)`, slicing) | clip; never `None` for the whole text |
| whitespace collapse | `str.split()` (Python `isspace`, includes `\x1c-\x1f`) | a predicate equal to Python's `isspace`, not `char::is_whitespace` (which excludes `\x1c-\x1f`); same for `_PUNCT_ONLY`'s `\s` and `tail` in `signature_end` |
| 200-char cut | code points | `chars().take(200)`, not bytes |
| budget | per line `n += tok(line)`; stop at the first line with `n > 48` | same; `tok` excludes special tokens |
| kinds | body only for `Function`, `Type` | same strings as `nodes.kind` |

Example (real node, this checkout): `mcp::search_code::window_texts`, kind
Function, nativeKind `function`, lines 521-532 (0-based), rust. Today the CE
reads the doc comment and the signature. After the port (built with
`gm462_ce_text.build(row, src, "EPB", minilm, 48)`):

```
function mcp search code
core src mcp search code rs
The text each row was embedded from, which is also what the
cross-encoder reads: `text_to_embed` of the node's current doc comment
and signature.

fn window_texts(conn: &Connection, rows: &[SearchResult]) -> anyhow::Result<Vec<String>>
let mut statement = conn.prepare_cached("SELECT docComment, signature FROM nodes WHERE id = ?1")?;
rows.iter()
.map(|row| {
```

## 4. Decisions

### D1. Which fields ship

- **Today:** the CE reads the structured text only.
- **Change:** E + P + B(48); I stays out.
- **Example:** above; I would add `window texts conn connection rows search result anyhow result vec string`.
- **Consequence:** ~82 tokens per node instead of 38.

| option | benefit | risk |
|---|---|---|
| **a. E P B48 (recommended; the task as written)** | Drops I, which hurt MRR in the leave-one-out; 4 tokens fewer than all-b48 | Not measured as such; S2 must clear the bar |
| b. P B48 | E was within noise (-E: dr@10 +0.6, dMRR -0.003); 4 fewer tokens again | Second unmeasured arm; another ~40 min of scoring; deviates from the task |
| c. E I P B48 (the measured all-b48) | The only arm with a published row | I measured as harmful to MRR; the task says I only if re-measured |

### D2. Stored in the index, or read at query time

- **Today:** `nodes` already has everything E and P need (`kind`,
  `nativeKind`, `name`, `qualifiedName`, `filePath`); B needs source lines,
  which the index does not store.
- **Change (recommended):** read B's source at query time.
- **Example:** a first page reads at most 30 nodes' files, usually far fewer
  distinct files (a query's top 30 cluster); each read once per call.
- **Consequence:** no schema change, no re-index; a file edited since its
  last index pass can give body lines that do not match the indexed
  signature until the watcher re-indexes it (seconds), and a deleted file
  gives an empty body.

| option | benefit | risk |
|---|---|---|
| **a. query time, from disk (recommended)** | No schema/version bump, no re-index of every project, no disk growth; reuses the project root `find_definition` already reads from; budget applied with the real tokenizer | File I/O on the hot path (expected < 5 ms for ~10-30 cached files; S7 measures it inside the total); brief staleness after an edit |
| b. store a body snippet per node (new `nodes` column, written at walk time) | No I/O at query time; body always matches the indexed signature | Schema bump and a full re-index of every user project; the walk has no CE tokenizer (the model may be absent), so it must store raw lines and still apply the 48-token budget at query time; ~150-300 B/node (~10-15 MB on 50k nodes); the plugin/walk path grows for a rerank-only field |
| c. store E/P text too | nothing: they are free columns already | as b, for no gain |

### D3. How the builder counts tokens

- **Today:** the tokenizer lives inside `CrossEncoder`, behind `&dyn Scorer`.
- **Change:** `Scorer::count_tokens(&self, text) -> usize`; `window_texts`
  takes the counter as a closure. Stubs count whitespace words.
- **Consequence:** four impls (CrossEncoder, `Unused`, `Stub`, `Capture`,
  appendix call 4). `CrossEncoder`'s tokenizer carries 512-token truncation;
  a counted line is at most 200 characters, so it never reaches the cap.

| option | benefit | risk |
|---|---|---|
| **a. trait method, no default (recommended)** | Real tokenizer in production; every new scorer must decide | 3 test stubs get a one-line impl |
| b. trait method with a whitespace default | Stubs unchanged | A future production scorer silently gets the wrong budget |
| c. budget in characters | No tokenizer needed | Not the measured text; breaks byte equality |

### D4. Shared fixture and `SIGNATURE_KEYWORDS`

`core/src/embedding/testdata/ce_text.json`, written by
`eval/embedding/gm698_ce_text_fixture.py` (calls `gm462_ce_text.build`):

```json
{
 "about": "GM-698 CE text fixture, written by eval/embedding/gm698_ce_text_fixture.py",
 "bodyBudget": 48,
 "cases": [
  {"label": "rust method in impl", "source": "v2 gin 1a2b...",
   "node": {"kind": "Function", "nativeKind": "method", "name": "...", "qualifiedName": "...",
            "filePath": "...", "startLine": 0, "endLine": 14, "language": "rust",
            "signature": "...", "docComment": null},
   "srcLines": ["...", "..."],
   "words": "<expected text, tok = whitespace words>",
   "minilm": "<expected text, tok = ce-minilm tokenizer>"}
 ]
}
```

- `srcLines` is the node's span from `startLine`, rebased to 0 and cut after
  the line the budget stops at; the generator asserts `build(rebased, cut)
  == build(original, whole file)` per case, so the cut cannot change a text.
- Cases: every synthetic case of `test_gm462_ce_text.py` (decorators,
  multi-line headers, lifetimes, arrows, braces in strings, docstrings,
  punctuation-only lines, the 200-char cut, exact-budget fit, other kinds,
  no source) plus edge cases this port adds (CRLF, lone `\r`, invalid UTF-8,
  `\xa0` and `\x1f` in a line, empty `nativeKind`, empty `name`, span past
  EOF), plus ~15 real nodes per language sampled from the v2 eval DBs.
- Rust always-run test: every case's `words` text equals `ce_text` with a
  whitespace counter. `#[ignore]`d test (needs the model, like today's
  parity test): the `minilm` text with the real tokenizer.
- **`SIGNATURE_KEYWORDS`.** g-mesh shows its only Rust use is
  `signature_identifiers` (the noise rule) and its only Python use is
  `identifiers` (the I field) (appendix, calls 5-6). With I not ported, the
  Rust CE builder does not use the list. The sync is still kept, for a later
  I re-measure and for the noise rule's eval copy:

| option | benefit | risk |
|---|---|---|
| **a. Python test parses the Rust const out of `text.rs` and compares sets (recommended)** | One source of truth; no regeneration when the list changes; fails the moment either side drifts | A reformat of the Rust array can break the parse (fails loudly, never silently) |
| b. list copied into `ce_text.json`; a Rust test and a Python test each compare to it | Both languages test the same artefact | Three copies; the fixture must be regenerated to change the list |

### D5. Beta re-tune

- **Today:** `BETA = 80` (rerank.rs:57), FIT-tuned for the structured text.
  GM-462: all-b48 peaks at 20, all (b96) at 40.
- **Change:** run `eval/embedding/gm462_ce_enriched.py --work <GM-460 work dir>
  --eval-dir eval/embedding/v2 --st-runs runs-gm465/jina-v2-base-code-int8-structured
  --k 30 --arms off,ship --table ... --summary ...` with the new `ship` arm
  (E P B48, **all** queries: NL and name). Grid: `rerank_eval.BETAS`
  (0, 1, 2.5, 5, 10, 20, 40, 80, 160, 320), unchanged so rows compare with
  GM-462. Objective: the driver's `fit_obj` on the FIT half (best FIT NL
  r@10, then MRR, then the smaller beta). Verified on held-out NL; controls
  C1-C6 as GM-462 (C4 reproduces the off row; off cache reused).
- **Pass bar (pre-registered):** held-out NL r@10 >= 0.647 and MRR >= 0.425
  (the lower CI bounds of GM-462's all-b48 beta-20 row); D9 vs int8-structured
  at the shipped floors passes. Guard (owner decision Q2): name-query MRR
  paired vs off (beta 80), lower bound >= -0.010.
- **Cost:** ~100k new pairs (all queries x 30) at 20-26 ms/pair, ~35-45 min.
- **Consequence:** `BETA` and `rerank_parity.json`'s `beta` change together;
  the fixture test `the_fixture_was_made_with_this_window_and_blend` holds
  them equal.

The Python driver is the measurement; the Rust port is tied to it by the two
fixtures (text byte-equal, then logits and order in the real-model parity
test). No Rust-side eval is needed.

### D6. Latency method (gate)

- Driver: `eval/embedding/gm464_latency.py` (product path: `g-mesh mcp-shim`
  over stdio, real daemon, g-mesh's own index, 30 queries, 3 timed passes).
  Arms: `base` = release-4.26.0 binary (structured text, beta 80), `enr` =
  the branch binary; both rerank on, 4 threads, same model dir. Extend
  `BINS` with the two arms; order `base enr enr base base enr` (3 per arm,
  fresh daemon each).
- **Metric:** median added ms per query at K=30 = median(enr) - median(base)
  over all timed calls (also p95, and off as reference from one `off` arm).
  The first (model-loading) call is reported separately, not in the median.
- **Machine state per arm:** `uptime` before and after, `pmset -g therm`,
  `/usr/bin/time -p` real/user/sys of the arm; the script's load gate
  (1-min load < 4 held 30 s) on. A real >> user+sys arm is marked as waiting.
- **Separates arms (control):** the rerank-on share (0/90 vs 90/90 in
  GM-464) and, new, the mean pair token count per arm logged by a debug env
  line or derived offline from the same query set (~49 vs ~93).
- Owner accepts or rejects the number (Q1 below).

## 5. Behaviours to pin (tests slice)

| # | behaviour | control (revert in code) |
|---|---|---|
| T1 | `ce_text` equals the fixture's `words` text on every case | drop the P field |
| T2 | `#[ignore]` real tokenizer equals `minilm` text | body budget 96 |
| T3 | handler scores the reference's enriched texts (`the_handler_scores_...`, root = tempdir with spans) | `window_texts` back to `text_to_embed` |
| T4 | body only for Function/Type | allow every kind |
| T5 | missing file, span past EOF, invalid UTF-8 give a text (empty or lossy body), never a failed call | `source_lines` with `read_to_string` + `?` |
| T6 | `BETA` equals the fixture's beta; blend of stored logits gives stored order | `BETA = 80` |
| T7 | verdict identical with rerank on/off (existing `the_verdict_is_the_same_with_the_rerank_on_and_off`) | judge on the reranked page |
| T8 | Python: `SIGNATURE_KEYWORDS` equals the Rust const | add a word to the Python set |

## 6. GM-697 (refusal signal): affected

Plainly: **yes, the rerank score distribution changes.** Two causes:

- **Logits rise.** GM-462 (all, b96): median logit of the best expected node
  -2.98 -> -1.64 (documented), -9.89 -> -7.71 (signature-only); nodes below
  -8 fell 92 -> 71 and 127 -> 82. E P B48 will shift similarly.
- **The blend's scale changes.** `logit + beta * cosine` with beta 80 -> ~20
  shrinks the cosine term from ~40-60 to ~10-15 at typical cosines.

What does **not** change: today's verdict (`noMatch`/`lowSimilarity`) is
judged on the embedding order's page by cosine (ADR 0016; `handle`, judged
rows), and D9's Q5 is +0.0 for every rerank row. If GM-697 keeps a
cosine-based signal, GM-698 does not affect it. If GM-697 chooses the F4
logit or blend, any threshold it fits on the structured text / beta 80 is
invalid after GM-698. Recommended sequencing: GM-697 fits such a threshold
on scores from the `ship` arm (S2's cache, `gm462-ce-minilm.*` files) at the
new beta, or GM-698 lands first. Both tasks also edit `search_code::handle`;
whichever merges second rebases (signature change here, verdict change
there).

## 7. Slicing of the rest

| slice | phase | model | exit criterion |
|---|---|---|---|
| S2 | measure: `ship` arm, beta re-tune, held-out, D9 (Python only; runs in parallel with S3) | sonnet | Tuned beta recorded; held-out NL r@10 >= 0.647 and MRR >= 0.425; D9 vs int8-structured passes; name guard reported |
| S3 | code: `ce_text.rs`, `Scorer::count_tokens`, `window_texts`/`handle` plumbing, `gm698_ce_text_fixture.py` + `ce_text.json`, `BETA` from S2, ADR 0042 | opus (port with edge semantics) | `cargo build` + clippy clean; T1 passes on the generated fixture |
| S4 | tests: T1-T8, regenerate `rerank_parity.json` (enriched, new beta), 6-8 controls listed | sonnet | Every listed control fails when its fix is reverted (in a throwaway worktree) |
| S5 | review: fresh agent on the diff vs this note | opus | No open correctness finding |
| S6 | verify: `scripts/test-local.sh --base release-4.26.0` (ADR 0031), ignored real-model tests with the model, controls, 5x for any test with files/threads | opus | Suite green, T2 and real-model parity pass, every control fails |
| S7 | measure: latency (D6) | sonnet | Median added ms/query at K=30 with machine state recorded; owner accepts |

S2 failing its bar stops the task before S3's fixture/beta work lands
(S3 can start on the builder, which does not depend on beta).

## Risks

- **Unmeasured arm** (D1): S2 may miss the bar; fallback is option c (ship I)
  with a fresh owner decision.
- **Latency** roughly doubles the rerank cost; on 1-2-core machines (1 thread:
  1.1 s today) a first page may pass 2 s.
- **Truncation:** pairs over 512 tokens lose body first, then base tail,
  never the header (GM-462: 4 -> 14 pairs of 11,750 at b96; fewer at b48).
- **Python/Rust whitespace and newline semantics** (section 3): the only
  realistic source of byte drift; the fixture's edge cases pin them.
- **Snapshot vs product qualified names:** v2 DBs predate some qualified-name
  changes (GM-462 M4 not done); E is the field affected and it is within
  noise.
- **Merge conflict with GM-697** in `search_code::handle`.

## Decisions for the owner

- **Q1. Latency gate.** What median added latency per first page is
  acceptable? Expected +350-450 ms (to ~0.9 s p50 at 4 threads).
- **Q2. Name-query guard** for S2: paired name MRR vs off, lower bound
  >= -0.010 (recommended), or report only.
- **Q3. Fields:** E P B48 (recommended) vs P B48.
- **Q4. Body source:** query-time read (recommended) vs stored column.

## Appendix: code facts and the g-mesh calls that produced them

Project: `g-mesh` (main checkout) via `select_project`, not
`g-mesh-wt-gm698`: both are at ed08d04 and the branch has no code changes,
so the warm index answers for the branch (avoids the ~5-8 min cold build).
Line numbers were checked with `sed -n` in the worktree.

| # | question | call | answer |
|---|---|---|---|
| 1 | who calls the CE scorer | `find_callers(symbol_id=<CrossEncoder as Scorer>::score)` then `find_callers(Scorer::score)` | only tests call the impl directly; the trait method's one caller is `Reranker::order` (rerank.rs) |
| 2 | who calls `Reranker::order` | `find_callers(symbol_id=Reranker::order)` | `search_code::first_page_reranked` only |
| 3 | who calls `first_page_reranked`, `window_texts`, `handle`, `handle_off_worker` | `find_callers` each | `handle`; `first_page_reranked`; `handle_off_worker` + 3 tests in search_code.rs + 4 in search_code_rerank_tests.rs (one non-CALLS usage excluded, same files); `GMeshMcpServer::search_code` (mod.rs) |
| 4 | implementations of `Scorer::score` | `find_implementations(Scorer::score)` | `CrossEncoder`, tests `Unused`, `Stub`, `Capture` |
| 5 | references to Rust `SIGNATURE_KEYWORDS` | `find_references(symbol_name)` -> ambiguous (Rust + Python), then `find_references(symbol_id=embedding::text::SIGNATURE_KEYWORDS)` | `signature_identifiers` (text.rs) only |
| 6 | references to the Python copy | `find_references(symbol_id=gm462_ce_text SIGNATURE_KEYWORDS)` | `identifiers` only (python semantic tier absent; module-level use, so no gap) |
| 7 | `window_texts`, `first_page_reranked` source | `find_definition` | the SELECT and `text_to_embed` call (section 1) |
| 8 | existing project-file reader | `search_code(query=...)` -> `find_definition(read_span)` | `mcp::source::read_span` -> `read_span_within` (refuses invalid UTF-8 and spans past EOF) |
| 9 | other rerank users (index schema / query path) | grep `rerank::|Reranker|BETA` in core/src (grep: non-call module paths) | `embedding/{mod,pipeline}.rs`, `mcp/{search_code,search_code_rerank_tests,session_hints}.rs`, `cli/{status,status/json,model}.rs`, `daemon/mod.rs`: state/switch only, no CE text; `nodes` schema (schema.rs:220) has every E/P column |

Example text built by `gm462_ce_text.build` against
`~/.g-mesh/projects/959ade85d9a343b1/index.db` with the ms-marco-MiniLM
tokenizer from `eval/embedding/work/models/`.
