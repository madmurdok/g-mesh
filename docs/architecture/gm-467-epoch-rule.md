# GM-467: must `PIPELINE_EPOCH` change with the text format?

Design note for GM-467/S1. No code change. The owner decides before S2.

**Answer: no.** The cache key hashes exactly the string the model receives.
Nothing between `text_to_embed` and the model depends on how that string
was put together. If a format change leaves a node's text unchanged, the
node gets the same vector. The rule "bump the epoch whenever
`text_to_embed`'s output changes" therefore guards against a risk that
does not exist, and it re-embeds every symbol on such an upgrade. On g-mesh
that is about 79% wasted inference time, estimated below.

The review also found one input that neither the fingerprint nor the
epoch rule covers: the `tokenizers` crate version (item 13). It is a
separate gap, and it is real whichever way the epoch question is decided.

## 1. What determines a cached vector

The key is `text_hash(text)` = `sha256(text)` (`core/src/embedding/cache.rs:100-101`),
stored under `models.id` for `fingerprint(onnx, tokenizer)`
(`cache.rs:106-115`). The fingerprint record contains `sha256(model.onnx)`,
`sha256(tokenizer.json)`, `DEFAULT_MAX_SEQUENCE_LENGTH`, `EMBEDDING_DIM`,
`PIPELINE_EPOCH` and `ORT_VERSION`.

`compute` hashes and embeds **the same `String`**:
`keys = pending.map(text_hash(text))` (`core/src/embedding/pipeline.rs:406`),
then `model.embed(&text)` on that same `text` (`pipeline.rs:422`). No
prefix, instruction or other rewrite happens between the two calls.
`EmbeddingModel::embed` passes the text to the tokenizer unchanged
(`core/src/embedding/model.rs:388`).

| # | input | code | covered by |
|---|---|---|---|
| 1 | text bytes, the output of `text_to_embed` | `pipeline.rs:396-401` (build), `:406` (hash), `:422` (embed) | **key** |
| 2 | ONNX weights, including int8 vs fp32 (ADR 0011) | `pipeline.rs:653-657` `identify` -> `cache.file_sha256(model.onnx)` | fingerprint (file sha256) |
| 3 | tokenizer: vocab/merges, normalizer, pre-tokenizer, post-processor (special tokens `<s>`/`</s>`), added tokens, and any truncation/padding block in the file | `identify` -> `file_sha256(tokenizer.json)`; loaded at `model.rs:332` | fingerprint (file sha256) |
| 4 | truncation length (1024, special tokens included) | `model.rs:341`, from `spec.max_sequence_length`; production spec `model.rs:248-256`; the pipeline loader calls `EmbeddingModel::load` (`pipeline.rs:223` -> `model.rs:291-292`) | fingerprint (`DEFAULT_MAX_SEQUENCE_LENGTH`, the same constant `load` uses) |
| 5 | the other truncation parameters (strategy, direction, stride: `..Default::default()`) | `model.rs:341` | epoch (code) |
| 6 | padding switched off, so the file's padding block cannot apply | `model.rs:345` | epoch (code) |
| 7 | `add_special_tokens = true` | `model.rs:388` | epoch (code) |
| 8 | graph inputs: no `token_type_ids` for the production spec | `model.rs:401-416`, `spec.token_type_ids` | epoch (code) |
| 9 | ORT session options: `GraphOptimizationLevel::Level3`, `with_deterministic_compute(true)` | `model.rs:349`, `:353` | epoch (code) |
| 10 | ONNX Runtime version (the native library ships with `ort-sys =2.0.0-rc.9`; default features, CPU only) | `core/Cargo.toml:99,106`; `cache.rs:46` | fingerprint (`ORT_VERSION`, pinned to the manifest by `the_fingerprints_ort_version_is_the_one_the_manifest_pins`) |
| 11 | pooling (mean over masked tokens, in order) | `model.rs:455-475` | epoch (named in the `PIPELINE_EPOCH` doc, `cache.rs:35-41`) |
| 12 | L2 normalization | `model.rs:479-484` | epoch (same doc) |
| 13 | **`tokenizers` crate version** (`Cargo.toml:117` `"0.23"`, a caret range; `Cargo.lock` holds 0.23.1) | the `Tokenizer::encode` implementation | **neither.** A `cargo update` can change how normalization, pre-tokenization or truncation treats the same text, and the cache would keep serving the old vectors. This is the same risk class as ORT (owner's answer 2 in ADR 0007). |
| 14 | output dimension | `pool` checks the shape against `spec.dimension` | fingerprint (`EMBEDDING_DIM`) |
| 15 | vector encoding (LE f32 blob; any other length is a miss) | `cache.rs:435-444` | schema (`SCHEMA_VERSION`) |
| 16 | CPU ISA and target arch: ORT picks AVX2/AVX-512/NEON kernels per machine | ORT internals | neither. The cache is per machine, so the remaining case is an x86_64 binary under Rosetta and an arm64 binary sharing one `~/.g-mesh`. Low risk. Noted only for completeness; it has nothing to do with the epoch question. |

None of these inputs is affected by `text_to_embed`'s format. Items 5-9,
11 and 12 are what `PIPELINE_EPOCH` exists for. The text format reaches the
model only through item 1, and item 1 is the key.

## 2. Who reads `PIPELINE_EPOCH` and the key/fingerprint builders

| g-mesh call | answer |
|---|---|
| `find_references PIPELINE_EPOCH` | `[]`, `hasMore: false`. **Incomplete**: the two uses are a `{PIPELINE_EPOCH}` capture inside `format!` and a path-qualified use in a test, and the indexer does not see either. Fell back to grep. |
| grep `PIPELINE_EPOCH` (core, docs) | `cache.rs:41` (definition); `cache.rs:110` (the fingerprint record, its only production use); `pipeline.rs:1571` (the pinning test); doc mentions at `cache.rs:12`, ADR 0007 :60/:184/:191, ADR 0012 :76/:91 |
| `find_callers embedding::cache::fingerprint` (by id) | `pipeline::identify` (`pipeline.rs:653`), plus the test `the_fingerprint_depends_on_both_files`. Complete. |
| `find_callers text_hash` | `EmbeddingPipeline::compute`, `pipeline::test_support::fake_vector`, and 8 tests in `cache.rs`. Complete. |
| `find_callers text_to_embed` (by id) | `compute`, `current_embeddable_text`, `embed_node` (pipeline.rs), `storage::language_swap::plan_embeddings`, `cli::embed_eval::Node::text_for`, and the Python-port fixture test. Complete. |

**What a bump does.** It changes the fingerprint, which creates a new
`models` row (`cache.rs:274-290`). Every earlier entry becomes unreachable
and is dropped later by LRU or by the 30-day model GC. The next reindex
misses on every text, and with it on every symbol of every project on the
machine. Nothing else reads the epoch.

**Two different mechanisms.** A format change already triggers two
separate things, and only one of them is needed for correctness:

- `TEXT_FORM_TAG` -> `embeddingVersion` (`model.rs:110-127`). Every stored
  vector carries a version tag. When the tag changes, backfill
  (`backfill.rs:131`) and the workspace swap (`language_swap.rs:367`) plan
  every node for `compute` again. **This must stay.** The project index has
  no record of which text a stored vector came from, so without the tag the
  changed nodes would keep stale vectors. That re-planning is cheap with a
  warm cache: an unchanged text costs one point lookup.
- `PIPELINE_EPOCH`. Bumping it is what turns those lookups into misses for
  every text.

## 3. Recommendation: narrow the rule

**New invariant (ADR 0007, `cache.rs`).** The key hashes exactly the string
passed to `Embedder::embed`. `PIPELINE_EPOCH` versions the code between
that string and the stored vector: the tokenizer call options, the
truncation and padding parameters, the graph inputs, the session options,
pooling and normalization. It does **not** change when `text_to_embed`'s
output format changes. That kind of change is versioned by `TEXT_FORM_TAG`,
which re-plans the stored vectors; the cache then re-embeds only texts that
actually changed.

**Tests (S2):**

1. Replace `the_pipeline_epoch_is_pinned_to_the_text_format` with
   **`the_text_form_tag_is_pinned_to_the_text_format`**. Same inputs, same
   digest, but asserted together with `TEXT_FORM_TAG` (expose it
   `pub(crate)`) instead of the epoch. A format change still fails the
   test, and the fix is now to bump the tag. Control: change `full_text`'s
   separator and the test fails.
2. **`the_cache_key_is_the_hash_of_the_text_the_model_receives`**. A
   counting fake embedder records each text it receives. After `compute`,
   every recorded text's `text_hash` must be a key in the cache. Control:
   make `compute` prepend a prefix before `embed` (key unchanged) and the
   test fails.
3. **Across a format change, an unchanged text hits and a changed text
   misses.** Run a fixture through `compute`. Then change `embedding_version`
   (simulating the tag bump) and edit one node's doc so that only its text
   changes. Re-run: embed calls = 1, hits = N-1. Control: put the text-form
   tag into the fingerprint (the old rule) and the result is N calls.
4. (Optional, for item 11/12) pin `pool`'s output on synthetic hidden states
   to `PIPELINE_EPOCH`, so that a change to pooling or normalization fails
   until the epoch is bumped. This test is deterministic and needs no model.
   Items 5-9 stay a manual rule, as they are today.

**Also recommended (separate from the epoch question):** add the
`tokenizers` version to the fingerprint (`TOKENIZERS_VERSION`, checked
against `Cargo.lock` the same way `ORT_VERSION` is checked against the
manifest), or pin the crate with `=`. Doing this changes the fingerprint
once, so the cache refills once.

**ADR 0007 edits:** Decision §1 "Model fingerprint": drop
"`text_to_embed` format" from the epoch's scope. Consequences: replace
"changing `text_to_embed` or pooling without bumping it serves stale
vectors" with the new invariant. Add a line pointing to this note. Also
correct ADR 0012's consequence bullet at :76: the epoch bump was a cost,
not a correctness requirement.

**Keep `PIPELINE_EPOCH = 2`.** Bumping it as part of this change would
cause one more full refill and buy nothing.

### Risks of narrowing

- **A future prefix or instruction added *inside* `embed`** (for example an
  asymmetric "passage:" prefix for another model) would not be in the key.
  Test 2 does not catch this case, because the prefix sits after the hand-off.
  Such a change belongs to the epoch (item 7/8 class), and the PIPELINE_EPOCH
  doc must say so. Today the only safeguard is the manual rule, the same as
  for items 5-9.
- **Forgetting `TEXT_FORM_TAG`** would leave changed nodes with stale vectors
  in project indexes. That was always true: the cache never re-planned
  anything. Test 1 now guards it directly; before, it was guarded only
  indirectly, through the epoch test.
- **A mixed-format window does not get worse.** A cached vector for an
  unchanged text is bit-identical to a fresh one (ADR 0007's byte-identity
  test), so search sees exactly what a full re-embed would produce.
- **The saving assumes a warm cache.** The cache must already hold vectors
  under the same fingerprint, from the previous g-mesh version on the same
  machine. A cold cache, a cache that is off, or an upgrade that also bumps
  ORT or the weights saves nothing.

## 4. Upgrade re-embed time saved

Method: GM-466's combined relative cost curve,
`t(n) = 2.7127 + 0.846790 n + 2.4646e-4 n^2` ms per text
(`docs/results/gm-466-cost-model.md`, Fits). It is applied to each node's
structured-text token count, truncated to 1024, using the GM-423/465 corpus
snapshots (`eval/embedding/work/*.sqlite`), the jina int8 tokenizer, and the
Python port of `structured_doc` in `eval/embedding/gm423_token_lengths.py`.
A node counts as changed when its structured text differs from its full
text, which is the 3.16 -> 3.17 upgrade.

Control: g-mesh's structured total, 344,780 tokens and a predicted 317.8 s,
matches GM-466's "structured" row exactly.

| corpus | texts | changed | changed tokens | full re-embed (pred.) | changed only | saved |
|---|---:|---:|---:|---:|---:|---:|
| g-mesh | 6,774 | 897 (13.2%) | 21.0% | 317.8 s | 65.6 s | **252 s (79%)** |
| ripgrep | 3,428 | 254 (7.4%) | 16.8% | 96.7 s | 15.6 s | 81 s (84%) |
| excalidraw | 2,758 | 155 (5.6%) | 8.7% | 82.6 s | 7.0 s | 76 s (92%) |
| gin | 1,547 | 19 (1.2%) | 2.8% | 33.8 s | 0.9 s | 33 s (97%) |
| py-requests | 961 | 124 (12.9%) | 19.4% | 26.9 s | 5.0 s | 22 s (81%) |
| task-tracker-mcp | 157 | 12 (7.6%) | 19.4% | 6.5 s | 1.2 s | 5 s (81%) |
| pooled | 15,625 | 1,461 (9.4%) | - | 564 s | 95 s | 469 s (83%) |

Caveats:

- GM-466's own validation misses measured pass ratios by up to 0.094, so
  treat the percentages as approximately ±10 points. The absolute seconds
  describe one laptop: GM-466 predicted 504 s for the int8 pass, against
  384-540 s measured.
- Changed texts are the long ones, so the token share (21%) is higher than
  the share of texts (13%).
- The pooled figure, 1,461 changed texts, is close to the task's "1468 of
  6876". The task's number counts only documented nodes, from a source not
  reproduced here.
- The saving applies once per format change, on every project on the
  machine. Lookup cost for the hits (~6k SQLite point lookups) is not
  modelled. ADR 0007 treats it as seconds.

## Owner decides

1. Narrow the epoch rule as in §3 (S2 proceeds), or keep it (S2 and S3 are
   skipped).
2. Whether S2 also adds the `tokenizers` version to the fingerprint (item 13)
   or files it as a separate task.
