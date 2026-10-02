# GM-464: cross-encoder rerank in `search_code`

Status: design for GM-464/S2, awaiting the owner's review. No code change.

Decided by the owner, not reopened here: ship F4. Take int8's top 30 and order
them by `ce + 80 * cosine`, where `ce` is the raw logit of
`cross-encoder/ms-marco-MiniLM-L6-v2`. Rows after the 30th keep int8 order.
The CE reads `text_to_embed(docComment, signature)`, the same structured text
int8 embeds. The noMatch/lowSimilarity verdict stays exactly as today
(`similarity.rs`). Evidence: `docs/results/gm-443-recall-rerank.md` S11, S12,
S14 and "GM-464 S1" (held-out NL r@10 +3.7 [+1.8], MRR +0.047 [+0.021] vs
int8-structured; every D9 gate passes).

Owner decisions are marked **[D1]** to **[D6]**, each with a recommendation.

## The shape

A new `embedding::rerank` module holds a `Reranker`. Like the embedding model,
it is a lazily loaded `OnceLock<Option<Session + Tokenizer>>`, and
`EmbeddingPipeline` owns it. That way it reaches `search_code::handle` through
the `Arc<EmbeddingPipeline>` the daemon already passes down
(`daemon::run` -> `serve_connection` -> `GMeshMcpServer` -> `handle_off_worker`),
and no new plumbing is needed. The rerank happens only in `handle`.
`search_code::search` stays as it is, so its other callers are untouched:
`find_definition::by_semantic_neighbours` (the resolution ladder's semantic
rung) and `top_k_for_eval` (the embed-eval harness parity check).

`handle` on a first page (`cursor` None, no `coverage`), rerank enabled:

1. `window = search(conn, q, max(page_size, 30), None)`. This is today's SQL
   and ordering.
2. `int8_page = window[..page_size]`. The verdict is computed on it with
   today's `similarity::verdict` / `low_similarity` (see "Verdict" below).
3. The query and the 30 node texts are read back from `nodes`, with the same
   `text_to_embed` that `current_embeddable_text` uses. Each pair is scored
   in length-sorted chunks of 16, with a pad mask and `token_type_ids`, and
   pair truncation at 512 tokens (LongestFirst). This is what
   `rerank_eval.py::CrossEncoder.score` does.
4. `s_i = logit_i + 80 * cos_i` (f64). The window is stably sorted by
   `s` descending (`total_cmp`), so ties keep int8 order, which is the
   Python `lexsort` tiebreak. Rows after the 30th are appended in int8 order.
5. The rows shown are the first `page_size`. For pagination, see Q6.

The rerank is skipped, and the code path is exactly today's, when any of these
holds: the switch is off (Q5), the CE is unavailable (Q3), `cursor` is a cosine
cursor, or the page is partial (`coverage` Some, **[D6]**: rerank only
complete pages). During a backfill the ranking is a transient mix of old and
new vectors. The verdict is withheld then anyway, and nesting a rerank cursor
inside `partial:` is complexity for a state that lasts minutes.

## Answers

**Q1. Where the model lives [D1].** The brief's premise does not hold: the
embedding model is not bundled. `g-mesh model fetch` downloads it
(`cli::model`, the only module allowed an HTTP client; the daemon never reaches
the network) into `resolve_model_dir` -> `default_model_dir`, which is
`$G_MESH_MODEL_DIR` or else `~/.g-mesh/models/<name>/`, pinned by revision,
size and sha256.

*Recommendation:* the CE follows the same path. `model fetch` downloads both
models by default, and the CE goes to `~/.g-mesh/models/ms-marco-MiniLM-L6-v2/`
(`model.onnx` from `onnx/model.onnx`, 91,011,230 B,
sha256 `5d3e70fd…1d4d4a`; `tokenizer.json`, sha256 `d241a60d…9e5c66`;
revision `233902d25c440f23af6f7d6e94d2946bac0bee0a`, the same files S1 scored).
There is a `--no-rerank` flag, and its own override `G_MESH_RERANK_MODEL_DIR`,
because `G_MESH_MODEL_DIR` names the embedding model's *directory*, not a
parent. `model status` reports both models. The load checks the shas, as
`check_default_weights` does.

*Trade-off:* the fetch grows by about 92 MB, to about 246 MB. Existing installs
get no rerank until they re-run `model fetch` (Q3 makes this visible).
Bundling would add 91 MB to every release archive and break the "weights are
not vendored" rule, so it is rejected. The fp32 file is the one evaluated. The
23 MB `model_qint8_*` exports at the same revision are unmeasured, so they are
left for a follow-up.

**Q2. Licence [D2].** The model card at the pinned revision says
`license: apache-2.0`, the same as jina. One caveat goes on the record: it was
trained on MS MARCO, whose data terms are "non-commercial research purposes".
The weights' licence is Apache-2.0, and the training data's terms are not a
licence on the weights, but a cautious redistributor may ask.
*Recommendation:* accept, and add one README sentence beside the jina note
naming the licence and the dataset.

**Q3. Missing model.** `Reranker::get()` resolves on the first reranking call.
It does not resolve at daemon start, because of the startup budget in
`pipeline.rs`'s "Where the model lives" doc. On failure it writes once to the
daemon log: `g-mesh daemon: rerank model ms-marco-MiniLM-L6-v2 is not
available (<cause>) - search_code keeps the embedding order. Run g-mesh model
fetch to enable it.` From then on, every call gets int8 order and the page is
byte-identical to rerank-off. An inference error or a non-finite logit on one
call falls back for that call only, with one log line per failing call. It is
never a tool error.

**Q4. Threads [D5].** The embedding session sets no thread count, so
onnxruntime uses its default, the number of physical cores (S12: 4 on the
measurement machine). `Session::run` takes `&self` (ort rc.9), so concurrent
`search_code` calls on the blocking pool (GM-448) can share one CE session.
*Recommendation:* `with_intra_threads(min(4, physical cores))`, plus the
embedding session's `Level3` and `with_deterministic_compute(true)`. The
deterministic flag makes the parity test reproducible. S12 and the estimate
were measured at 4 threads. Capping at 4 keeps a 16-core machine from fanning
one query out across every core while a backfill also runs.
*Trade-off:* while the embedding pass runs, the two pools oversubscribe the
cores (4 + default). S5 measures 4 threads and 1 thread, and the cap is a
constant that S5 can move.

**Q5. Disable switch [D3].** Config lives in two places. The per-project file
`~/.g-mesh/projects/<hash>/config.toml` (`ProjectConfig`: `[plugin]`,
`[daemon]`, `[embedding]`) is read by `daemon::run` at start. The machine-wide
`~/.g-mesh/config.toml` (`GlobalConfig`: `[cleanup]`, `[embeddingCache]`) is
read by `CacheSettings::from_global_config`, with an env override
(`G_MESH_EMBEDDING_CACHE=off`).
*Recommendation:* a global `[rerank] enabled = true` with the env override
`G_MESH_RERANK=off`, mirroring `[embeddingCache]`. It is read once when the
pipeline is built, so a change takes effect on daemon restart. Off skips the
new code entirely: the `else` branch is today's `search(conn, q, page_size,
cursor)` call, not "K = 0". The env var is also what the eval and bench A/B
arms switch on.
*Trade-off:* there is no per-project setting. Latency and CPU are properties
of the machine, and a per-project `[search]` key can be added later without
breaking anything.

**Q6. Pagination, the score cursor, the GM-434 page rule [D4].** The cosine
cursor (ADR 0013: `score_bits`, `id`, resuming at `score < s OR (score = s AND
id > id)`) stays the only cursor for rows after the window. It is exact
there, because the window is exactly int8's top-30 set.

- `limit >= 30`: the whole window is on page 1. `next_cursor` is unchanged:
  `search`'s own cursor at the last int8 row of the page, which is the same set
  as the reranked page.
- `limit < 30` (the default limit is 20): page 1 shows `reranked[..limit]`,
  and the cursor becomes a new kind,
  `rerank:<base64 {rest: [ids of reranked[limit..30]], after: <cosine cursor
  at int8 row 30, or none>}>`. A continuation re-reads the `rest` rows by id,
  with the cosine recomputed, skipping any that were deleted. It emits them
  in that order and then continues with `search(after, …)`, so no CE runs on
  continuation pages. *Trade-off:* the cursor grows to at most about 1.3 KB
  (29 ids). The alternatives were rejected. Recomputing the window and the CE
  on page 2 costs another ~200 ms, and a re-index between the calls would
  duplicate or skip rows. Reranking only within the page (K = limit) at the
  default limit is S14's K = 20, which loses about a third of the r@10 gain.
- GM-434's rule (only a first page is judged, and a continuation carries no
  verdict) is unchanged. A `rerank:` cursor is `Some`, so `verdict` and
  `low_similarity` stay silent, as they do for any continuation.

**Verdict.** It is computed on `int8_page`: today's rows, today's
`page_size`, before any reordering. `below_floor` checks *every* row of the
page against its own language's floor. That matches the task's wording
("int8's un-reranked top-1 cosine") only on single-language pages, and only
today's code is byte-identical. Running the verdict on the reranked page would
be wrong whenever `limit < 30`, because the rerank can bring a row from int8
positions 21-30 into page 1 and push out the row that cleared the floor.

**Q7. Score shown.** It stays the cosine, unchanged in the JSON, so the floor
semantics, the cursor and every client keep working. The CE logit is not
calibrated (S8: signature-only code scores near −10), so the verdict could not
be read from it and it is not exposed. Consequence: within the top 30, `score`
no longer falls monotonically. The tool description's "Results are ranked by
similarity, most relevant first" becomes "ranked by relevance (a
cross-encoder reorders the top 30); `score` is the embedding cosine", and the
`SearchResult::score` doc comment says the same.

**Q8. Epoch and cache.** Nothing changes. The CE produces no stored state:
vectors, `embeddingVersion` (`embedding_version`, `+int8+structured`),
`PIPELINE_EPOCH` and the cache fingerprint (onnx/tokenizer sha,
`DEFAULT_MAX_SEQUENCE_LENGTH`, `EMBEDDING_DIM`, epoch, `ORT_VERSION`) all
describe the embedding model only. The CE's inputs (its weight and tokenizer
shas, the 512 cap, the ORT and `tokenizers` versions) affect each call's order
and nothing persisted. The sha check at load and the parity test (T1) pin
them. There is no CE score cache: scores depend on the query, and a cache
would need its own key for no measured gain.

## Q9. Test plan

Unit tests use a `Scorer` trait with a deterministic stub, the same pattern as
`Embedder` and `test_support`. Tests that need the real model are `#[ignore]`,
like `model.rs`'s, and S4 runs them. Each test names its control, which is a
revert of code and never of the test.

| # | Test | Control (must fail) |
|---|---|---|
| T1 | `#[ignore]` parity: fixture `core/src/embedding/testdata/rerank_parity.json`, made by a small script reusing `rerank_eval.CrossEncoder` and `gm464_ce_structured`'s structured texts. About 20 NL and name queries × int8 top-30 (id, text, cosine), the Python logits and the F4 order. Asserts logits within 1e-4 and the order exactly, with tie-tolerance only where the blend gap is < 1e-3. | β 40 instead of 80, and full text instead of structured. The generator asserts that the fixture's order differs under both, so the fixture can tell them apart. |
| T2 | The verdict is byte-identical with the rerank on and off. A polyglot fixture where the stub pulls int8 row 25 onto page 1 and pushes out the only row above the floor, for a name query (noMatch) and a prose query (lowSimilarity). | Verdict computed on the reranked page. |
| T3 | Switch off equals today: a golden `CallToolResult` JSON is captured from the *current* handler before any change, then the switch-off output is compared to it byte for byte, at limits 5, 20, 30 and 50 and on a continuation. | Switch ignored (the stub reorders). |
| T4 | Missing CE dir: output equals T3's golden, and one log line appears across two calls. | Load error returned as a tool error, or logged per call. |
| T5 | Pagination at limits 1, 5, 20, 29, 30 and 50: concatenating all pages gives the reranked window followed by int8 rows 31+, with no duplicate or skip. Plus a deleted `rest` id. | Page 2 resumes from the cosine cursor when limit < 30. |
| T6 | Ties keep int8 order. | Unstable sort, or a tiebreak by id. |

At the eval level (S4, acceptance criterion 2): index the eval snapshots and
run every eval query through the MCP `search_code` with `G_MESH_RERANK=off`
and with it on. `noMatch`/`lowSimilarity` must be byte-identical, and the
rerank-on top-30 order must equal `gm464_ce_structured.py`'s F4s K = 30 β = 80
for each query. The control is that the off arm equals today's release binary.

Existing tests for `by_semantic_neighbours` and `top_k_for_eval` must pass
unchanged. They show that the rerank stays out of `search`.

## Owner decisions

- **D1** Location: `model fetch` gets both models by default (`--no-rerank`
  to skip), into `~/.g-mesh/models/ms-marco-MiniLM-L6-v2/`, with the fp32 file.
- **D2** Licence: Apache-2.0 accepted, and the MS MARCO data terms noted in
  the README.
- **D3** Switch: global `[rerank] enabled`, plus `G_MESH_RERANK=off`.
- **D4** Pagination: the `rerank:` cursor carries the unshown window ids.
- **D5** Threads: intra-op `min(4, physical cores)`, deterministic compute.
- **D6** Partial pages (embedding pass running): no rerank.

## Appendix: cross-file answers (g-mesh, project `g-mesh`)

The index is of the main checkout. This branch changes only `docs/` and
`eval/`, so the answers hold for it.

| Question | Call | Answer |
|---|---|---|
| Who calls the search_code handler | `find_callers mcp::search_code::handle` | `handle_off_worker`, `GMeshMcpServer::search_code` (mod.rs), 3 tests |
| | `find_callers mcp::search_code::handle_off_worker` | `GMeshMcpServer::search_code` only |
| Who calls `search` (must stay un-reranked) | `find_callers mcp::search_code::search` | `handle`, `top_k_for_eval`, `find_definition::by_semantic_neighbours`, tests |
| Who calls the floor check | `find_callers mcp::similarity::verdict` | `handle`, `partial_verdict`, tests |
| | `find_callers mcp::similarity::low_similarity` / `partial_verdict` | `handle` only |
| | `find_callers mcp::similarity::floor` | `below_floor`, `cli::embed_eval::report`, `find_definition::by_semantic_neighbours`, a test (plus a non-call use in `mcp/mod.rs`) |
| Where the model path is resolved | `find_definition resolve_model_dir` | `embedding::model`: explicit, else `default_model_dir` |
| Who calls it | `find_callers resolve_model_dir` | `cli::model::model_dir` (plus a re-export in `embedding/mod.rs`) |
| | `find_callers embedding::model::default_model_dir` | `resolve_model_dir`, `EmbeddingPipeline::model_dir`, 4 tests |
| Who builds ORT sessions | grep `Session::builder` (single known symbol) | `EmbeddingModel::load_with_spec` only (model.rs:349) |
| Its callers | `find_callers …load_with_spec` and `…load_with_max_sequence_length` | `load_with_max_sequence_length` -> `load` (product, via the pipeline's loader closure in `build`); `cli::embed_eval::run_variant`, `embed_eval::cost::calibrate` |
| Who reads the project config | `find_callers config::read_project_config` | `daemon::run`, `cli::reindex`, `cli::init`, `cli::config_wizard::run`, `cli::model::resolved_model_in` |
| Who reads the global config | `find_callers config::read_global_config` | `CacheSettings::from_global_config`, `cli::clean::run`, `cli::config_wizard::run`, `gc::warning` |
| Who uses `EmbeddingConfig` | `find_references config::EmbeddingConfig` | `config/mod.rs`, `embedding/pipeline.rs`, `cli/config_wizard.rs`, `cli/model.rs`, a test |
| Who builds the CE text | `find_callers embedding::text::text_to_embed` | `pipeline::compute`, `current_embeddable_text`, `embed_node`, `language_swap::plan_embeddings`, `embed_eval::Node::text_for`, tests |

g-mesh did not fail on any of these calls. `get_file_outline core/src/config.rs`
returned "no file found", because the module is `core/src/config/mod.rs`, and
the outline call was not repeated after that. The model card and the ONNX
shas were read from the Hugging Face API at the pinned revision. The local
eval copy's sha256 matches `onnx/model.onnx`.
