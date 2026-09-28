# Embedding search-quality eval and the model decision rule (GM-398)

Status: design, written before any measurement (GM-398/S1, 2026-09-26, on
`release-3.15.0` at 805686f). Nothing below has been run yet. Results go to
`docs/results/gm-398-embedding-eval.md`; this note is not edited after the
first variant is scored, except to append a dated "Deviations" entry.

## Context

`search_code` and `find_definition`'s semantic rung rank nodes by cosine
similarity of one embedding per node. The model was an architectural pick
(REQUIREMENTS.md, "Embedding-модель для локального запуска";
`docs/architecture/g-mesh-v1.md`) that was never validated against an
alternative: `jina-embeddings-v2-base-code`, fp32, `onnx/model.onnx`
641.5 MB, rev `516f4baf13dec4ddddda8631e019b5737c8bc250`, CPU EP, mean
pooling, 768 dims, input truncated to 1024 tokens.

What production embeds, and how it is used (from g-mesh, see "Code facts"
below): the node text is `text_to_embed(docComment, signature)` = trimmed doc
comment + `"\n\n"` + trimmed signature, either alone, neither = no vector.
The query is embedded by the same `EmbeddingModel::embed` with no prefix.
Ranking is brute-force `1 - vec_distance_cosine` over every row of `vectors`
(`search_code::search`). A page is judged by `similarity::verdict`, which
compares each row with a per-language floor (`similarity::floor`: go 0.59,
python 0.57, rust 0.55, typescript 0.50, default 0.50), calibrated on jina
only (g-mesh-bench `v0.21.0-semantic-threshold-calibration.md`). The same
`floor` gates `find_definition::by_semantic_neighbours`. A model switch
therefore changes three things at once: the vectors, the floors, and (ADR
0007) the cache fingerprint, so every project re-embeds once (604 s for
g-mesh's 6,536 nodes on the 3.13 baseline).

Three tasks need one instrument: this one (is jina the right model?), GM-422
(int8 jina) and GM-423 (shorter input cap). The eval below is that
instrument; GM-422/423 run it unchanged with a different variant row.

## Decisions

### D1. Corpora: the benchmark's pinned corpora plus g-mesh itself

| corpus | language | pinned at | why |
|---|---|---|---|
| task-tracker-mcp | TypeScript | `35237c8` (bench registry) | owner's code, small, used in floor calibration |
| excalidraw | TypeScript | `1acf66e` (bench registry) | large TS, the sparsest doc comments (20.2%) |
| ripgrep | Rust | `15.2.0` / `e89fff8` | public Rust, used in floor calibration |
| g-mesh | Rust | `805686f` (this note's base) | owner's heaviest daily corpus, dense docs |
| gin | Go | `v1.12.0` / `73726dc` | used in floor calibration |
| py-requests | Python | `6e83187` | used in floor calibration |

Revisions are copied from `g-mesh-bench/corpora/registry.json`, not
re-typed; the harness refuses a checkout whose `HEAD` differs. Using the
calibration corpora makes re-derived jina floors directly comparable with
the shipped ones (a harness control, D7). g-mesh is added because it is
where the owner actually searches, and it is Rust with long module docs,
which is where the 1024 vs 512 token cap bites.

Rejected: torpeek (Go+JS; gin covers Go, and JS is not a bundled language),
expense-bot (TS, small, overlaps ttm), GoogleAgenticHackaton (Python, 158
files, unpinned and not the owner's current work; py-requests is pinned and
already calibrated). Public corpora may be in some models' training data;
results are reported per corpus so a public/private split is visible.

### D2. Query set: size, and why that size

Per language: **100 positive queries** and **25 absent-answer queries**.
TypeScript 50 ttm + 50 excalidraw, Rust 50 ripgrep + 50 g-mesh, Go 100 gin,
Python 100 py-requests (if requests has fewer than 100 distinct behaviours
worth asking for, at least 70, and the note of the shortfall goes in the
results). Total ~400 positives, ~100 absent.

Statistical argument. Every comparison is paired (same queries, same node
set). For recall@10 each query contributes d = -1, 0 or +1; with a
discordance rate p (queries where exactly one arm hits) the SE of the mean
difference is about sqrt(p)/sqrt(n). Assuming p = 0.20 (to be reported, not
assumed afterwards): n = 400 gives SE = 2.2 points, so a one-sided 95% lower
bound sits 3.7 points below the point estimate. With the margin of 5 points
in D9, a model that is truly as good as jina passes about 72% of the time, a
model truly 3 points worse about 22%, and one truly 5 points worse 5% (by
construction). That asymmetry is intended: a switch re-embeds every project,
so an inconclusive result keeps the status quo. Per language (n = 100, SE
~4.5 points) only a coarse guard is possible, so D9's per-language gate is
-10 points: an equal model trips it by chance ~1.3% per language (~5% over
four), a real -15 point regression in one language is caught ~87% of the
time. For MRR (per-query difference SD ~0.3, SE ~0.015) the 0.05 margin
leaves an equal model a ~95% pass rate.

Pre-registered second stage, used at most once and only when a cost
candidate's recall@10 lower bound lands in [-7, -5): the same authors add
300 positives by the same protocol, without seeing per-query results, and
the rule is re-applied on all ~700 at a one-sided 97.5% bound. No other
extension of the set is allowed after measuring.

### D3. How queries are authored and recorded

Protocol, per corpus, done before any variant is scored:

1. **Target first.** Sample target symbols from the corpus's embeddable
   nodes (the snapshot of D4, rows where `text_to_embed` is `Some`), by a
   fixed seed, stratified by kind (functions/methods ~60%, types/traits/
   interfaces ~30%, other ~10%). Sampling the target first stops the author
   from picking only the well-documented, famous symbols.
2. **Read, then write.** The author reads the target's code at the pinned
   revision and writes the query an agent would type when it does not know
   the name: behaviour, not identifier. Two shapes, alternating: a short
   phrase (3-7 words) and a sentence (10-25 words), matching the shapes the
   floor calibration used.
3. **Expected set** = every symbol that satisfies the query equally (an
   interface and its only impl, an overload pair), found by reading and by
   structural tools (`find_references`, `find_implementations`) and grep.
   Any one of them counts as a hit. Keep it at 1-3 symbols; if more qualify,
   the query is too vague and is rewritten.
4. **Absent queries**: plausible behaviour for the domain that the corpus
   does not implement ("retry an HTTP request with exponential backoff" for
   ripgrep), proven absent by grep and structural lookup. Expected set empty.
5. **Blind to models.** Authors must not call `search_code`, the semantic
   rung of `find_definition`, or any embedding. Structural g-mesh tools,
   grep and Read are allowed. No query, expected set or wording may come
   from any model's ranking.
6. **Lexical overlap is recorded, not banned.** The harness computes
   `overlap` = the query shares an identifier sub-token (camel/snake split,
   length >= 4) with an expected symbol's name. At least half of each
   corpus's positives must have `overlap = false`; metrics are also reported
   per stratum, so a model that only wins on name-matching is visible.
7. **Freeze.** The query files are committed, and their sha256 recorded in
   the results doc, before the first variant is scored. A later fix (an
   expected symbol that is plainly wrong) is allowed only as a logged
   deviation, applied to every arm, and reported.

Record format, one JSON object per line in
`eval/embedding/queries/<corpus>.jsonl`:

```json
{"id": "rg-017", "corpus": "ripgrep", "language": "rust",
 "kind": "positive", "shape": "sentence",
 "text": "decide whether a path should be skipped because an ignore file excludes it",
 "expected": [{"filePath": "crates/ignore/src/dir.rs", "qualifiedName": "...", "kind": "Function"}],
 "derivation": "Target sampled (seed 398, #17). Read crates/ignore/src/dir.rs:410-470: matched() walks the ignore stack... Also acceptable: X, because ... Not Y: Y only parses the file.",
 "author": "GM-398/S2 agent"}
```

The harness resolves every `expected` entry to a node id in the snapshot and
fails hard on an unresolved one (drift or typo), and fails on an expected
node with no embeddable text (no model could ever find it).

### D4. Embedding each variant: g-mesh's own pipeline, offline

A hidden CLI subcommand in g-mesh, `g-mesh debug-embed-eval` (precedent:
the hidden `debug-candidates`), in `core/src/cli/embed_eval.rs`:

1. **Snapshot.** Each corpus is indexed once by the workspace build with the
   embedding model switched off (structural and semantic passes only); the
   index sqlite is copied to `eval/embedding/work/<corpus>.sqlite` and its
   sha256 recorded. Every variant reads the same snapshot, so the candidate
   node set is identical across arms and only the model varies.
2. **Node text** = `SELECT id, ..., docComment, signature FROM nodes`,
   passed through the crate's own `text_to_embed` (it is `pub(crate)`, which
   is why the harness lives inside the crate, not in an example or in
   g-mesh-bench). Same function, same text as production.
3. **Embed** with `EmbeddingModel` loaded from the variant's directory, with
   production session options (deterministic compute, batch of one, CPU EP).
   The implement slice adds a variant spec the harness passes and production
   does not use yet: pooling (mean | cls), dimension (checked, not assumed:
   today `EMBEDDING_DIM = 768` is a const), max input tokens (default
   `min(1024, model max)`), query prefix, document prefix. jina's spec is
   exactly today's behaviour; a test pins that.
4. **Rank** each query brute force, cosine, top 100 kept, ties broken by node
   id; write `rankings.jsonl`, `vectors.bin` (reused by broken arms and
   reruns), `timings.json`.
5. **Report**: `g-mesh debug-embed-eval report <run dirs>` computes D5-D6
   and applies D9 mechanically, with a fixed bootstrap seed, and prints the
   verdict per candidate. GM-422 and GM-423 add a row to
   `eval/embedding/variants.toml` and run the same two commands.

The machine-wide embedding cache (ADR 0007) may serve quality runs (it keys
on the model's bytes, so it cannot mix variants); timing runs set
`G_MESH_EMBEDDING_CACHE=off`.

Where it lives: harness, query files, corpus pins (`corpora.toml`, copied
from the bench registry) and variants in **g-mesh** (`eval/embedding/`,
engine in `core/src/cli/embed_eval.rs`). Reason: the thing measured is the
workspace's own embedding code, the tasks that reuse the eval (GM-422/423)
change that code, and `text_to_embed` is crate-private. g-mesh-bench keeps
only the agent-level check (D10), which is what it is for. Rejected: running
each variant through the daemon and `search_code` over MCP, as the floor
calibration did. It measures the exact production path, but needs a full
production model switch (dims, pooling, prefix) before a candidate can even
be scored, and a full reindex per variant per corpus; the parity control in
D7 buys the same confidence for jina at a fraction of the cost.

### D5. Metrics

Per variant, per corpus, per language, pooled (languages weighted equally),
and per overlap stratum; positives only unless stated.

- **recall@k** (k = 5, 10): 1 if any expected symbol is in the top k, else 0,
  averaged. (Expected sets list acceptable answers, so "any" is the right
  hit definition for a tool whose job is to hand an agent an entry point.)
- **MRR**: mean of 1/rank of the first expected symbol, 0 if outside the top
  100.
- **confident-wrong rate**, at the variant's own recalibrated floors (D6), on
  the held-out half only: for a positive query, the top hit's score is at or
  above the floor of the top hit's language and the top hit is not expected;
  for an absent query, the top hit is at or above its floor. Reported for
  positives, absent, and combined.
- **false-alarm rate** at those floors (held-out): the right answer is
  ranked first but every row is below its floor, so `verdict` would say "no
  match". The floor is fitted to hold it at 3% on the fit half (D6); on
  the held-out half it runs higher, so D9 gates it against R, not against 3%.
- Also reported, not gated: recall@1, the discordance rate p against the
  reference (D2's assumption, checked), and the share of node texts longer
  than 512 and 1024 tokens under each tokenizer (the truncation confound).

Uncertainty: paired bootstrap over queries, 10,000 resamples, stratified by
language, seed fixed in `variants.toml`; one-sided 95% bounds.

### D6. Floors recalibrated per model, without touching the test half

Every query gets a split by the parity of the first byte of
`sha256(id)`: **fit** or **held-out** (the calibration's rule; stable, not
chosen). Per variant and language, the floor is fitted exactly as
`similarity.rs` documents: the largest value at which the fit half's
false-alarm rate stays at or below 3%, rounded down to two decimals. The fit
half is enlarged with the calibration's mechanical queries (bench
`calibrateSemanticThreshold.ts`: symbols queried by their own name, n=150
per corpus, and names sampled from other corpora as negatives), exported to
the same JSONL format; they involve no model output either. Confident-wrong
and false-alarm are then read on the held-out half of the authored queries
only. recall@k and MRR do not depend on the floor and use all queries.

A switch ships the fitted floors as the new `similarity::floor` constants,
with the table in its doc comment re-derived.

### D7. Controls: broken arms and harness parity

- **random**: every node and query vector drawn i.i.d. Gaussian from a fixed
  seed, L2-normalised. Also a metric check: its recall@10 must be within
  3x the analytic chance level (mean over queries of 10·|E|/N) plus 2
  points, or the harness is leaking order (e.g. ties) and the run is void.
- **shuffled**: the reference model's vectors, reassigned to nodes by a
  seeded derangement: the model is intact, the text-to-symbol link is not.
- **words-shuffled** (informational, not a gate): each node's text with its
  words shuffled, re-embedded with the reference model: how much of the
  score is bag-of-words.
- **bm25** (informational): lexical ranking over the same texts. If jina
  does not beat it, that is the headline.

"Clearly lower", numerically: the reference's pooled recall@10 must exceed
each gated broken arm's by at least **20 points**, the broken arm must be at
most **0.25x** the reference, and the one-sided 95% bounds must not overlap.
If jina fails this, the eval is broken, not the model; stop. Every candidate
must also pass the same test against the broken arms; one that does not is
presumed misconfigured (pooling, prefix, tokenizer) and investigated, not
reported as a quality result.

**Harness parity**: 20 queries on task-tracker-mcp scored both by the
harness (jina spec) and by a production daemon's `search_code` on the same
snapshot must give the same top-10 order and scores within 1e-4. Second
parity check: jina's re-derived floors on the fit half must land within
0.03 of the shipped ones for go, python and rust (TypeScript's 0.50 came
from a different query mix, so it is reported, not gated). A failure stops
the eval until explained.

### D8. Candidates

Sizes and licences from the Hugging Face API on 2026-09-26; each is pinned
to a commit sha in `variants.toml` when it is added.

| variant | params | ONNX file (size) | dims | pooling | max tokens | licence | status |
|---|---|---|---|---|---|---|---|
| jina-v2-base-code fp32 | 161M | `onnx/model.onnx` 641.5 MB | 768 | mean | 8192 (cap 1024) | Apache-2.0 | **reference** |
| jina-v2-base-code int8 | 161M | `onnx/model_quantized.onnx` 161.9 MB | 768 | mean | 8192 (cap 1024) | Apache-2.0 | candidate (also GM-422) |
| bge-small-en-v1.5 | 33M | `onnx/model.onnx` 133.1 MB | 384 | cls | 512 | MIT | candidate; query prefix "Represent this sentence for searching relevant passages: " per its card |
| gte-small | 33M | `onnx/model.onnx` 133.1 MB | 384 | mean | 512 | MIT | candidate; no prefixes |
| snowflake-arctic-embed-s | 33M | `onnx/model.onnx` 133.1 MB | 384 | cls | 512 | Apache-2.0 | candidate; same query prefix as bge, per its card |
| CodeRankEmbed | 137M | none published (547 MB safetensors, `nomic_bert` custom code) | 768 | cls | 8192 | MIT | stretch: only if an ONNX export yields `input_ids`/`attention_mask` -> `last_hidden_state`; owner decides whether to spend on it |
| nomic-embed-code | ~7B | none (~28 GB safetensors) | - | - | - | Apache-2.0 | **out**: 28x over the 1 GB budget, GPU-class |
| jina-code-embeddings-0.5b | 494M | none (988 MB safetensors, decoder, last-token pooling) | - | - | - | CC-BY-NC-4.0 | **out**: non-commercial licence blocks distribution |
| nomic-embed-text-v1.5 | 137M | 547 MB | 768 | mean | 8192 | Apache-2.0 | **out**: general text, jina's size, so it cannot win on cost and has no code training to win on quality |

Each candidate is run with its model card's recommended configuration,
fixed in `variants.toml` before scoring. No prefix, pooling or cap is tuned
on the eval; tuning on the test set is exactly the leak D6 avoids. The three
small models cap at 512 tokens, below today's 1024, so their result mixes
model and truncation; the truncation share (D5) and GM-423's jina-at-512 row
separate the two.

### D9. The decision rule (fixed now)

Candidate C against reference R = jina fp32, pooled over languages.
Δ = C - R, bounds are one-sided 95% from D5's bootstrap.

**Validity** (else no verdict): D7's broken-arm and parity controls pass for
R, and C beats the broken arms by the same margins.

**Quality gates, a cost candidate must pass all:**

| gate | point estimate | bound |
|---|---|---|
| Q1 recall@10 | Δ >= -2.0 points | lower >= -5.0 points |
| Q2 MRR | Δ >= -0.02 | lower >= -0.05 |
| Q3 per language recall@10 | Δ >= -10 points in every language | - |
| Q4 confident-wrong (held-out, combined) | Δ <= 0 points | upper <= +5 points |
| Q5 false alarm (held-out, pooled; each language reported) | Δ <= 0 points | upper <= +5 points |

Q5 was first an absolute "<= 3% per language, as the floors promise". The
reference itself fails that: at its own fitted floors jina's held-out false
alarm is go 18.8%, python 9.5%, rust 14.3%, typescript 14.8% (run at
620eae2): the 3% holds on the fit half, not on the authored held-out
queries. Q5 is therefore
relative to R and shaped like Q4: per query, each arm at its own floors,
paired over the held-out positives both arms rank right first, pooled over
languages with the per-language Δ reported beside it. Pooled, like Q4,
because a language holds few such queries: one discordant query moves a
per-language rate by several points, so a per-language +5 bound would veto
almost any candidate not identical to R. Owner decision 2026-09-27.

recall@5 is reported beside recall@10 and is not a separate gate (it is
strongly correlated and would only add a multiple-comparison veto).

**Cost gates, measured per D11:**

- A **cost candidate** (smaller or faster) must pass Q1-Q5 **and** win on at
  least one of: embedding pass time <= 0.60x R, model size <= 0.50x R, max
  RSS during the pass <= 0.70x R; **and** be no worse than 1.10x R on any of
  the three, nor on median query-embedding latency.
- A **quality candidate** (not cheaper) must be clearly better: lower bound
  of Δ recall@10 > 0 **and** lower bound of Δ MRR > 0, plus Q3-Q5; and cost
  no worse than 1.5x pass time, 1.2x RSS, model size < 1 GB.
- **Ties**: among passing candidates, the lowest pass time wins, then RSS,
  then size. Pass time dominates because it is paid on every first index and
  on every re-embed, including the one the switch itself triggers.
- **Inconclusive = keep jina.** Anything that fails a gate or lands between
  a pass and a fail is reported as such; it is not rounded towards a switch.
- **Agent-level veto** (D10) applies to the winner before the switch is
  proposed to the owner.

For GM-422 (int8) and GM-423 (shorter cap) the same rule applies with the
variant as a cost candidate. The switch itself (floors, dims, pooling,
prefix, `g-mesh model fetch` pins) is a separate implementation task.

### D10. Agent-level check for finalists (g-mesh-bench)

Two arms only, both `gmesh-configured`: g-mesh built with R, g-mesh built
with the finalist; no baseline or Serena arm. Tasks: those where
`search_code` is used, found before the run from GMB-150's saved tool-use
logs (`scripts/analyzeToolUseLog.ts`) plus GMB-180's semantic-tier bucket;
the list is fixed before the run. If fewer than 10 tasks qualify, the check
is reported as low-power rather than skipped.

Proposed run length (**owner to approve**): **5 repetitions** per task and
arm, as in GMB-150. REPS=low (one run) is not valid for token numbers. Cost
estimate from GMB-150 ($53.35 for 690 records, ~$0.08 per record): ~20 tasks
x 2 arms x 5 = 200 records, about $15 and 2 hours.

Veto if any holds: oracle passes lower for C by more than 2 of the paired
runs in total; any task R passes 5/5 that C passes 3/5 or fewer; median
per-task token Δ > +10%; median turns Δ > +0.5. It is a veto, not a win
condition: at this size it cannot show an improvement.

### D11. Measuring pass time, RSS and size

- **Quiet machine**: `uptime` before and after each run, recorded; start
  only when the 1-minute load is below 2.0 on the 8-CPU host, no
  `g-mesh daemon` process is alive (`pgrep -fl`), and no other measurement
  is running (another agent's timing runs share this machine; runs are
  scheduled, not overlapped). A run whose after-load exceeded 3.0 is redone.
- **Pass time**: the harness's embed-all step over the g-mesh snapshot
  (~6,500 embeddable nodes, the ADR 0007 baseline corpus), with
  `G_MESH_EMBEDDING_CACHE=off`, under `/usr/bin/time -lp` (`-p` for
  real/user/sys, `-l` for maximum resident set size on macOS). Three runs
  per variant, interleaved R, C1, C2, ..., R, C1, ... to spread drift; the
  median is reported with all three. `user` far below `real` x cores means
  the process waited, and the run is explained before it counts.
- **Headline check** for finalists only: one production
  `g-mesh reindex` of g-mesh at 805686f with the cache off, against the
  668 s / 604 s baseline in ADR 0007.
- **RSS**: max RSS of the embed-all step (loaded model plus the arena at the
  variant's cap).
- **Size**: bytes of `model.onnx` plus any external data file plus
  `tokenizer.json`, as downloaded.
- **Query latency**: median wall time of embedding the 500 queries, one by
  one, in the same process.

## Code facts this relies on (from g-mesh, 2026-09-26)

- `text_to_embed` is defined at `core/src/embedding/pipeline.rs:719`,
  `pub(crate)` (`find_definition`). Callers: `EmbeddingPipeline::compute`,
  `current_embeddable_text`, `embed_node` (pipeline.rs) and
  `storage::language_swap::plan_embeddings` (`find_callers`).
- The floor: `similarity::floor` is referenced by `similarity::verdict` and
  `find_definition::by_semantic_neighbours` (`find_references`); `verdict` is
  called by `search_code::handle` plus tests (`find_callers` by id, after a
  `nameAmbiguous` answer on the bare name).
- The model: `EmbeddingModel` in `core/src/embedding/model.rs`, mean pooling
  and `EMBEDDING_DIM = 768` hard-coded; revision pinned in
  `core/src/cli/model.rs` (`MODEL_REVISION`); cache fingerprint from
  `model.onnx` + `tokenizer.json` bytes in `pipeline.rs` (`search_code`
  located these; details read directly).

## Risks

- **Author bias.** Queries written by an LLM reading the code may echo the
  doc comments' wording, favouring lexical overlap. Mitigated by
  target-first sampling, the overlap stratum and the bm25 arm; not
  eliminated. The authors being agents is also the point: agents are
  `search_code`'s users.
- **Power.** 400 positives give an equal cost candidate ~72% pass odds. Low
  power errs towards keeping jina, which is the cheap error here; the stage-2
  extension exists for the borderline case only.
- **Truncation confound** for the 512-token models (D8).
- **Training-set contamination** on the public corpora (per-corpus report).
- **Harness drift from production** after this lands: the jina-spec test and
  the parity control are the guard, and should be re-run whenever
  `text_to_embed` or `EmbeddingModel` changes.
- **Shared machine**: timing is only as good as D11's quiet-machine checks.

## Alternatives rejected

- Deriving expected answers from the corpora's `tasks.json` oracles alone
  (the calibration's method): 79 tasks, mostly named-symbol lookups, too few
  and too lexical for a quality comparison. Kept only as the floor fit set.
- Scoring against a model-produced ranking (e.g. an LLM judge over the top
  10): violates the "never from a model's output" rule and rewards whatever
  the judge prefers.
- One global floor for all models: the existing calibration already showed
  one number is wrong across languages; it is at least as wrong across
  models.
- Harness in g-mesh-bench over MCP: see D4.
