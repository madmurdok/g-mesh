# GM-441: prioritised embedding backfill (design)

Slices GM-441/S1 and S7. S7 rebases this note on the production stack:
jina int8 weights (ADR 0011, `DEFAULT_WEIGHTS_TAG = "int8"`), the
structured text form (ADR 0012), and the cross-encoder rerank of the top 30
(ADR 0016). Branch `feat/GM-441-prioritised-embedding` off `release-4.24.0`
(b4f96e1).

No code, tests or timing ran in either slice.

- **Probed:** the tier shares come from a read-only probe of the six eval
  snapshots (`eval/embedding/work/<corpus>.sqlite`). It applies the GM-437
  noise rule and the structured text (`gm423_token_lengths.py` port), counts
  tokens with the jina int8 tokenizer, and prices them with GM-466's
  calibrated curve.
- **Quoted:** all seconds come from existing results docs and run
  directories.

**Acceptance (owner, S7):** measure on jina int8 + structured, with the
rerank installed. Report the critical-path reduction and the staleness
window. Recall@10 and MRR, after rerank, must be unchanged once the pass is
complete.

## Verdict

**Recommended: P, a tiered backfill order with `Phase::Ready` unchanged.**

- **The change.** The backfill embeds public non-test nodes first, then
  non-public non-test nodes, then test nodes. Today it embeds in `nodes.id`
  order: 32-hex hashes, effectively random with respect to value.
- **Unchanged once complete.** P embeds the same nodes with the same text
  and the same deterministic int8 compute. The top-30 window and the
  cross-encoder scores are the same, so recall@10 and MRR after rerank are
  identical by construction.
- **What it does not shorten.** The time to `Phase::Ready`, and so the time
  to reranked pages.
- **What it shortens.** The time until the partial, unreranked pages that
  `search_code` returns during the pass hold every gold answer.
  - On the g-mesh corpus that moves from ~100% of the pass to **~55%**:
    ~195 s instead of ~354 s on int8-structured.
  - Pooled over the six corpora, it moves to **~68%** of the cost.

**Owner decision (D): also move `Phase::Ready` to the end of the non-test
tiers.**

- **Gain.** On int8-structured the test tier is **44.8% of the g-mesh
  corpus pass (~159 s of ~354 s)** and 32% pooled. D brings Ready, and with
  it the rerank, that much earlier.
- **Staleness window.** The window is the test tier itself. Pages marked
  complete (reranked, no partial note) leave test symbols out during it.
- **Unmeasurable.** The eval cannot price that risk: its target frame
  excluded test and fixture nodes, so test gold is 0 by construction. This
  is a product call.

## Today's behaviour

### The pass and its phases

- **Activation order.** `ActivationCtx::activate`
  (`core/src/daemon/activation.rs:147-248`) runs, in order: the walk, the
  semantic pass and its retry, the watcher consumer, the resumes, and the
  seed gate. Then `set_phase(Phase::Embedding)` (line 228), then
  `embedding::backfill::run` inside `catch_unwind`, then
  `set_phase(Phase::Ready)` (line 247). `Phase::Structural` covers the whole
  semantic pass. On torpeek the backfill started at +143 s (GM-686).
- **The backfill.** `backfill::run` (`core/src/embedding/backfill.rs:155-212`):
  1. Purges noise and returns early if no model is available.
  2. `count_candidates` (337-345). A candidate is a node with no vector, or a
     vector of another `embeddingVersion`, that has a doc comment or a
     signature and `generated = 0`.
  3. Pages with `fetch_candidate_page` (351-379): keyset `n.id > after_id
     ORDER BY n.id LIMIT 256`.
  4. For each page: `EmbeddingPipeline::compute` (drops noise, uses the
     machine-wide cache), `store_vectors`, `add_embed_done(page_len)`.

  Every candidate that exists when the pass starts is visited exactly once.
- **Edits during the pass** are embedded by the watcher path
  (`watcher::apply::round_trip` -> `compute`), not by the backfill.

### What `search_code` waits for, and when it reranks

- `GMeshMcpServer::search_code` (`core/src/mcp/mod.rs:929-951`):
  `prepare(Need::Structural)`, then `wait_for_embeddings` (592-628), then
  `handle_off_worker`.
- `Need::Embeddings` is satisfied only by `Phase::Ready`
  (`indexing_status.rs:81-86`). The wait bound is 20 s
  (`SEARCH_EMBEDDING_WAIT`, or `G_MESH_SEARCH_EMBEDDING_WAIT_MS`).
- **GM-686 latch:** once one call has waited the bound out, later calls in
  the same pass return a partial page at once. `set_phase` clears the latch
  on `Ready` and on `Walking`.
- **Rerank (ADR 0016, design D6).** `search_code::handle` (312-397) reranks
  only a first page that is complete (`coverage` None). The window is
  `search`'s first `max(limit, 30)` rows, and the first 30 are sorted by
  `logit + 80 * cosine` (ms-marco-MiniLM-L6-v2).
  - A partial page gets no rerank, no `low_similarity` and no hint, and its
    cursor carries the stored count.
  - D6's reason: during a backfill the ranking is a transient mix, and
    nesting a rerank cursor inside `partial:` is complexity for a state that
    lasts minutes.

### Costs today on int8-structured

| item | value | source |
|---|---|---|
| cold pass, g-mesh corpus (6,774 nodes, no noise filter) | **353.8 s** median, 335-354 s on quiet rounds; 386.5 s in GM-465's quality run | `docs/results/gm-453-onnx-tuning.md` Phase B A0; `runs-gm465/.../g-mesh/timings.json` |
| cold pass with production's noise filter (5,608 texts) | not measured. Estimate: a few % below 353.8 s, since the filtered texts are name-only (GM-437: 2.0% of characters) | estimate |
| cold pass, the other eval corpora (one quality run each, load not recorded) | gin 36.7 s, py-requests 35.5 s, ripgrep 126.7 s, excalidraw 94.8 s, task-tracker-mcp 6.9 s (sum of all six with g-mesh: 687.1 s) | `runs-gm465/jina-v2-base-code-int8-structured/<corpus>/timings.json` |
| cold pass, live g-mesh index (18,767 candidates) | not measured. **Estimate ~950-1,000 s**: 2.8x the corpus by node count; noise not removed; the absolute cost model is not trusted (GM-466 predicted 504 s against 384-585 s measured) | estimate |
| first `search_code` while the pass runs | the full 20 s bound, then a partial, unreranked page | GM-686 |
| later calls while the pass runs | ~65 ms p50 (embedding only, no rerank) | GM-686 latch; GM-464 `off` arm |
| first call in a fresh daemon | +~5 s to load the embedding model (GM-686, 4.23.0 = int8), +~0.9 s to load the cross-encoder (GM-464: first call 1,474-1,557 ms with rerank vs 663-666 ms without) | GM-686 run 1; GM-464 |
| complete call after Ready | p50 510 ms, p95 805 ms with rerank (4 threads); 65/85 ms without | GM-464 |

A warm restart is cheap: the embedding cache serves most texts.

**The critical path today:**

- Structural tools never wait for the backfill.
- `search_code` pays one 20 s wait per pass.
- Until `Ready`, it answers from a random-order subset with no rerank.
- Complete, reranked results arrive only when the whole pass ends.

## Data: what each tier holds (int8, structured text, noise rule applied)

Tiers:

- **public**: `visibility = 'public'` and not a test.
- **private**: any other visibility, and not a test.
- **test**: `text::is_test` (a test path, a `test*`/`Test*` name, or a
  `::tests::` qualified name), whatever its visibility.

"Cost" is `t(n) = 2.7127 + 0.84679 n + 2.4646e-4 n^2` ms. This is GM-466's
combined relative curve (`runs-gm466/rel.curve.json`), with `n` the int8
tokenizer's token count capped at 1024, summed per tier. Only the shares are
used.

| tier | nodes | structured chars | tokens | **cost share** | gold answers |
|---|---|---|---|---|---|
| public | 35.7% | 32.8% | 32.3% | **32.5%** | 319 (67%) |
| private | 34.9% | 36.1% | 35.5% | **35.4%** | 156 (33%) |
| test | 29.5% | 31.1% | 32.2% | **32.1%** | 0 (by construction) |
| total | 13,641 texts | | | | 475 |

Per corpus, the cost share of public / private / test:

| corpus | texts | public | private | test | gold public / private |
|---|---|---|---|---|---|
| g-mesh | 5,608 | 0.183 | 0.369 | **0.448** | 22 / 34 |
| gin | 1,478 | 0.493 | 0.097 | 0.409 | 98 / 36 |
| py-requests | 830 | 0.466 | 0.005 | 0.529 | 105 / 2 |
| ripgrep | 2,850 | 0.532 | 0.401 | 0.067 | 29 / 39 |
| excalidraw | 2,726 | 0.473 | 0.464 | 0.064 | 34 / 20 |
| task-tracker-mcp | 149 | 0.687 | 0.283 | 0.029 | 31 / 25 |

- **Structured text moves cost toward tests.** The structured form trims
  long doc comments, and those sit mostly on non-test nodes. The test
  tier's cost share rises from 25.9% (full-text characters, S1) to 32.1%
  pooled, and to 44.8% on g-mesh. Gold positions do not depend on the text
  form.
- **Variables get no tier of their own.** Public variables have the
  highest gold density (20 gold in 286 nodes).
- **Python reduces to non-test first.** Python marks almost everything
  public.

## Options

| option | `Phase::Ready` / `search_code` wait | staleness window (int8-structured, g-mesh corpus) | recall@10 / MRR after rerank, once complete | risk |
|---|---|---|---|---|
| **P. Tier order**, Ready unchanged | Unchanged: Ready and the rerank arrive at the end of the whole pass; one 20 s wait per pass. Partial pages improve sooner. | Public done at 18% (~65 s), private at 55% (~195 s), test at 100% (~354 s) | **Identical**: same vectors, same top 30, same CE logits | Low: scan order only. Progress and "exactly once" must hold across the tier scans. |
| D. P plus Ready at the end of the non-test tiers | Ready, and the rerank, **~159 s earlier on g-mesh (45%)**; 32% of the cost pooled | The test tier, ~159 s, during which pages look complete | Identical after the test tier. During the window, test queries miss without a note. Not measurable on the eval. | Medium: a phase or sub-state, a third `Coverage` state, and a cursor guard. Complete-shaped pages would be reranked over a set that is still growing (see Rerank). |
| P+R. P plus rerank on partial pages (reverses design D6) | Unchanged | as P | Identical | Changes an owner decision (D6). A `rerank:` cursor nested in `partial:`. Adds ~450 ms per partial call. Informational arm only. |
| L1. Lazy on touch | n/a | unbounded | KNN cannot find a node that is not embedded, so the search itself never "touches" one | Rejected |
| L2. Lazy at query time (lexical pre-select, embed top k) | +k x ~50 ms per call at int8 (k = 16: ~0.8 s) | The pass still runs | Unchanged | Query-path latency and CPU contention with the backfill. A follow-up at most. |
| I. Idle / low priority | Ready later | longer | Unchanged | GM-440. Out of scope. |
| R. Recently changed first | as P | as P | Identical | Not measurable on the eval: snapshot mtimes are checkout times. Left out. |

### Rerank: what it means for P and for D

- **P.** Partial pages are never reranked. While the pass runs, the best a
  partial page can reach is the int8-structured **no-rerank** level, even
  after every gold node is embedded (~55% on g-mesh). The rerank's gain
  arrives only at Ready, unchanged from today. ADR 0016 puts that gain at
  +3.7 r@10 points and +0.047 MRR on held-out NL queries. So S5 reports P
  against two references:
  - **t90/t100 against the no-rerank complete value.** This is the ceiling
    a partial page can reach. P moves it.
  - **The rerank gap.** It stays until Ready, and P does not move it.
- **D.** After the early Ready, `coverage` is None, so the first page *is*
  reranked. That is D's real gain: the rerank arrives ~159 s earlier on
  g-mesh. Three consequences:
  1. The CE reorders a window drawn from non-test nodes only. On the eval
     this cannot cost gold: test nodes are never gold, and their absence
     only frees window slots. So D's eval numbers are biased in D's favour.
     For a real "where is X tested" query, the test hit is absent and
     nothing says so.
  2. A `rerank:` cursor issued during the window holds the unshown window
     ids. Its `search` continuation then runs over a vector set that keeps
     growing, the case ADR 0016 rejected for page 2. Under D the cursor
     needs the `partial:`-style stored-count guard during the test tier.
  3. `noMatch`/`lowSimilarity` verdicts come back during the window and
     are judged without test nodes. A test-only query can get a confident
     `noMatch`.

## Recommendation: P

1. **Unchanged once complete.** The same vectors give the same window and
   the same CE scores. This meets "recall@10/MRR after rerank unchanged once
   fully embedded" by construction, and S5 checks it bit for bit.
2. **Partial pages become useful early.**
   - g-mesh corpus: 39% of the gold is in by ~65 s and all of it by ~195 s.
     Today, in random id order, gold accumulates roughly linearly up to
     ~354 s.
   - Pooled: 67% of the gold by 32.5% of the cost, and all of it by 67.9%.
3. **No API, phase, wait or rerank change.** The latch, `Coverage`, D6 and
   the cursors stay as they are.
4. **D stays the owner's decision,** with S5's numbers: Ready and the
   rerank ~45% earlier on g-mesh, against a test-tier window that pages do
   not report.

### Mechanism (P)

- **One keyset scan per tier, classified in Rust.** For each tier in
  [public, private, test], run today's keyset scan by `id` with the same
  `WHERE`, also selecting `visibility`, `kind`, `name`, `qualifiedName` and
  `filePath`. Keep only the rows of the current tier, hand them to
  `compute`, and add only those to progress.
  - The test rule stays in one place, `text::is_test`, and is not copied
    into SQL.
  - Memory stays flat.
  - A node the watcher embeds mid-pass falls out of later scans.
- **Invariants.**
  - Each candidate whose tier does not change mid-pass is processed
    exactly once.
  - A node that changes tier mid-pass can be skipped. The next start then
    embeds it.
  - `embed_done` ends at about `embed_total`, not 3x it.
- **CLI callers.** `cli::init` and `cli::reindex` call `backfill::run`
  synchronously (`init.rs:231,268`, `reindex.rs:143`). Order does not
  matter to them.

## Measurement plan (S5: int8 + structured, rerank installed)

**Run S5 on a quiet machine, after GM-692. Record `uptime` and
`/usr/bin/time -p` user/sys/real for every timed step.**

### Inputs (no new embedding needed)

- **int8-structured eval vectors already exist** for all six corpora:
  `eval/embedding/work/runs-gm465/jina-v2-base-code-int8-structured/<corpus>/`
  (`vectors.bin`, `query_vectors.bin`, `rankings.jsonl`, `manifest.json`,
  `timings.json`; g-mesh 3.17.0).
  - They hold all 15,625 texts. Production's noise filter is applied in the
    replay as a mask (13,641 remain).
  - **Verify once before the replay:** run `debug-embed-eval parity
    --index-db <task-tracker-mcp index.db> --run runs-gm465/...` against a
    production 4.24 int8 index, to confirm these vectors are what today's
    production stores. The `embedding_version` tag includes `TEXT_FORM_TAG`.
  - If parity fails, S5 must re-embed: a `jina-v2-base-code-int8-structured`
    run with `filter = "noise"`, about 690 s for all six corpora at the
    GM-465 timings.
- **Rerank logits:** the GM-464 S1 cache
  `eval/embedding/work/rerank_cache/gm464-ce-minilm.<corpus>.*.json`. It
  holds the ms-marco-MiniLM-L6-v2 logits on structured text for the
  int8-structured top-K windows. `gm464_ce_structured.py` holds the F4
  implementation (`logit + 80 * cosine` over the top 30, stable sort), with
  its controls C1-C4.
- **Per-text cost:** GM-466's calibrated int8 curve
  (`runs-gm466/rel.curve.json`) with int8 token counts. **No new
  calibration.**
  - Shares come from the curve.
  - Seconds come from scaling each corpus's shares by its measured
    int8-structured pass: g-mesh 353.8 s (GM-453), the others from
    `runs-gm465` timings.

### Is the replay valid?

Yes, for ranking:

- **Vectors.** int8 at batch 1 is deterministic and parity-clean (GM-453:
  max |delta| 0). A node's vector does not depend on when it was embedded.
- **Partial pages.** A production partial page is `search` (cosine over
  `vectors JOIN nodes`) over the stored subset. The replay ranks the same
  vectors over the same subset.
- **Rerank.** It is a pure function of (query, the window's structured
  texts, cosines). The cached logits reproduce it to 1e-4 (the ADR 0016 T1
  parity fixture).

What the replay does not model:

- query-time CPU contention with the backfill (affects latency, not
  ranking);
- the watcher's own embeddings;
- the `noMatch` verdict. That is not part of recall@10/MRR, but S5 reports
  the D-window `noMatch` rate on positives as a side column.

### Arms

| arm | order | rerank applied at |
|---|---|---|
| **A0 `id`** | today | 100% only |
| **A1 `tier`** | P | 100% only |
| **A2 `tier-ready-before-tests`** | P | from the private/test boundary on |
| A3 `tier-rerank-partial` (informational, P+R) | P | every checkpoint |
| C `oracle` (informational upper bound) | gold first | 100% only |
| control `tier-reversed` | test, private, public | 100% only |

### Procedure

For each arm and corpus:

1. Order the masked candidates.
2. Accumulate the predicted cost `T(k)`.
3. At checkpoints every 5% of the pass, plus each tier boundary, rank every
   query over the embedded prefix (cosine, `top_hits`, top 100).
4. Where the arm says so, rerank the top 30 with F4 over cached logits.
   Score any pair missing from the cache with the same CE.
5. Compute recall@10 and MRR on positives (`metrics::Outcome`), pooled and
   per corpus.

**Cost of new CE pairs.** Only A2's boundary windows (non-test top 30s)
and A3's partial checkpoints need pairs that the cache lacks.

- A2: at most one window per query, and in practice only the test-displaced
  entries, so a few minutes or less.
- A3: up to 20 checkpoints x ~1,500 queries x 30 pairs, at most about 0.9 M
  pairs at ~15 ms each. At ~3.7 h worst case, it is impractical. **Run A3 on
  g-mesh only, at 4 checkpoints, or drop it.** Owner choice; the
  recommendation is to drop it.

### Metrics

- **Critical path (seconds and fraction of the pass, per corpus and
  pooled):**
  - time to Ready: the total for A0 and A1, the boundary for A2;
  - **t90 and t100** of partial-page recall@10 against the **no-rerank**
    complete value (the reachable ceiling);
  - the time at which the **reranked** complete value is first reached
    (Ready for A0 and A1, the boundary for A2);
  - the share of gold embedded at 20 s, 60 s and 120 s.
- **Staleness window:** for each tier, its end time. For A2, the post-Ready
  test-tier duration.
- **Once complete:** recall@10 and MRR after rerank at 100% for every arm.
  Pass: **bit-identical reranked rankings** across arms.
- **A1 vs A0:** the area under recall-vs-time, and a paired bootstrap at
  25/50/75% (seed 398, 10,000 resamples).

### Runs (one per step)

1. Parity check: one command, no timing.
2. The replay: all arms, six corpora. CPU-cheap except for the new CE pairs.
3. **Daemon confirm, the only wall-clock step:**
   - setup: a release build, the production int8 model and the
     cross-encoder (`g-mesh model fetch`), an isolated `G_MESH_HOME` under
     `/tmp` (socket path under 103 bytes), `G_MESH_EMBEDDING_CACHE=off`, and
     the g-mesh corpus as a project;
   - one cold run each for A0 and A1, plus A2 if the owner picks D;
   - record the `index.phase` / `index.progress` timeline and the per-tier
     log lines;
   - issue one `search_code` at each tier boundary, and check that it is
     partial and unreranked before Ready (P) and reranked after it.
   - Pass: A1's total is A0's within noise (GM-453 spread 335-354 s), and
     the tier boundaries match the predicted shares within 10%.

### Controls

- **Order:** the reversed-tier arm must give t90 later than A0.
- **Completion:** with the mask off and rerank off, the 100% rankings must
  equal `runs-gm465/.../rankings.jsonl`. With rerank on, they must equal
  `gm464_ce_structured.py`'s F4s outcomes. Corrupting one cached logit must
  break that equality.
- **Rerank placement:** A2 at the boundary with rerank off must differ from
  A2 with rerank on (the arms are told apart).
- **Production:** with a single id scan, the A1 daemon run's tier log must
  show interleaved tiers.

## Edit map (P)

Production code is unchanged from S1:

- `text.rs`: `is_test` (157-166) becomes `pub(crate)`. New `EmbedTier` and
  `embed_tier` next to `is_noise` (144).
- `backfill.rs`:
  - module doc (1-73);
  - `Candidate` (306-314): add `visibility`;
  - `fetch_candidate_page` (351-379): select `visibility`;
  - `run` (155-212): a tier loop, filtered pages, `add_embed_done` counting
    the filtered rows, one log line per tier.

S7 changes in the harness:

- **`core/src/cli/embed_eval/schedule.rs`** (new):
  - arguments `--run-root runs-gm465`,
    `--variant jina-v2-base-code-int8-structured`, `--filter noise`,
    `--curve runs-gm466/rel.curve.json`;
  - per checkpoint, writes `rankings.jsonl` (top 100) and `schedule.json`
    (`checkpoints: [{t, frac, embedded, gold_embedded, recall10, mrr,
    reranked}]`, `tierEnds`).

  It does not rerank itself.
- **`eval/embedding/gm441_rerank.py`** (new): applies F4 to the checkpoint
  rankings where the arm reranks, reusing `gm464_ce_structured.py`'s
  structured-text port, cache and CE scorer, and adds `recall10Reranked` /
  `mrrReranked`. It lives in Python so that it uses the same cache and
  scorer as the ADR 0016 numbers. Rust's `rerank::Scorer` has no cache and
  would re-score ~45k pairs per complete checkpoint.
- `core/src/cli/embed_eval.rs`: `Node`/`load_nodes` (262-279, 326-359)
  gain `visibility`; `EmbedEvalCommand`/`run` (63-84, 179-196) gain
  `Schedule`.
- `cost.rs`: reuse its curve reader and tokenizer, with no change.

Read for context, as in S1, plus:

- `search_code::handle` (312-397) and `first_page_reranked` (475-530): the
  rerank only on complete first pages;
- `rerank.rs` `WINDOW`/`BETA` (52-55);
- `gm464_ce_structured.py` `add_structured` (83+), `need_pairs` /
  `score_missing` (139-179).

If D is picked, add to S1's D list: `search_code::handle` must give a
`rerank:` cursor issued during the test tier a stored-count guard, and
`similarity::verdict` gets a note during the window.

## Behaviours tests must pin (P)

Unchanged from S1 (items 1-9), plus:

10. A partial page under P is still never reranked, even after the public
    and private tiers are done. The existing D6 test in
    `search_code_rerank_tests.rs` stays green.
11. Harness: `schedule` marks a checkpoint `reranked` only where the arm
    says so (A0 and A1 at 100%, A2 from the boundary on). At 100%, every
    arm's reranked rankings are identical.

For reference, items 1-9:

1. `embed_tier` classifies as public, private or test (test wins).
2. Tier order is public, then private, then test, even when ids interleave.
3. Each candidate is processed exactly once across the tier scans.
4. The final set of vector ids equals the id-order pass's.
5. `embed_done == embed_total`.
6. Noise and generated nodes are still left out.
7. A candidate embedded by the watcher mid-pass is not re-embedded.
8. `Phase::Ready` comes only after the whole pass.
9. Harness: identical rankings at 100%. With the mask off, they equal the
   stored run's.

## Appendix: g-mesh calls (project `g-mesh`, main checkout; no code change on this branch)

S1 calls, unchanged. The first `select_project g-mesh` failed ("daemon did
not accept connections ... within 10s"); the retry worked.

- **Backfill entry points.**
  - `find_callers embedding::backfill::run`: `cli::init::init`,
    `cli::reindex::reindex`, `ActivationCtx::activate`, and the test
    `Project::walk_then_backfill`. It also listed 2 unlinked usages in
    `plugins/typescript/tests/semantic_runs_no_project_code.rs`.
  - `find_callers fetch_candidate_page`: only `run`. Its unit tests in
    `backfill.rs` were found by grep in that one file.
  - `get_dependencies Incoming backfill.rs` (depth 1, `truncated: false`):
    `cli/init.rs`, `cli/reindex.rs`,
    `tests/it/structural_does_not_wait_for_embeddings.rs`.
  - `find_callers EmbeddingPipeline::compute`: `apply`, `backfill::run`,
    `watcher::apply::round_trip`, plus 20 untyped receiver calls
    (`pipeline.rs`, `workspace_reindex.rs`).
- **Phase readers and waits.**
  - `find_callers wait_for_embeddings`: `search_code` only.
  - `find_references Need::Embeddings`: `wait_for_embeddings` only.
  - `find_references embedding_wait_spent`: `wait_for_embeddings` only.
  - `find_references embed_progress`: `progress_snapshot`,
    `progress_detail`, `wait_for_embeddings`.
  - `find_references add_embed_done`: `backfill::run`, plus untyped calls
    in tests.
- **Files P touches.**
  - `find_references embedding::text::is_test`: `is_noise` only.
  - P touches `text.rs` and `backfill.rs`; nothing under `mcp/` or
    `daemon/`.
- **S7 sources (non-code or single known files, so grep or sed):**
  - the rerank's placement, from ADR 0016 and
    `docs/architecture/gm-464-ce-rerank.md` D6;
  - int8 timings, from `gm-453-onnx-tuning.md`, `gm-466-cost-model.md`,
    `gm-464-rerank-latency.md` and the `runs-gm465` `timings.json` files.

Probe scripts (scratch, not committed), in
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/9007b349-2efd-4fa8-b083-5772fb23af93/scratchpad/gm441/`:
`tiers2.py` (S1, full text), `tiers_int8.py` and `tiers2_lib.py` (S7: int8
tokens, structured text, GM-466 curve).
