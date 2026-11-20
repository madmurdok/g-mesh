# GM-703: CodeRankEmbed as a base embedding model (design, S1)

Split from GM-436 (D8). The owner decided on 2026-11-13: "Записать «не пробовали» + задача на
CodeRankEmbed (Recommended)". The question is whether `nomic-ai/CodeRankEmbed`, a
code-specific retriever, beats the shipped base model, jina-v2-base-code int8 on the
structured text (ADR 0011, ADR 0012). The comparison uses GM-398's D9 rule on the
GM-460 v2 eval, at about jina's cost.

This slice is design only. Nothing was exported or embedded. The facts below come from
the Hugging Face API, the model's small files at the pinned revision (no weights were
downloaded) and this branch's code.

## 1. Summary

- **Export**: `torch.onnx.export` (TorchScript tracer) on torch 2.2.2, opset 17,
  dynamic `batch`/`sequence` axes. Inputs are `input_ids` and `attention_mask`, and
  the output is `last_hidden_state`. Before tracing, the rotary cos/sin tables are
  pre-built to 2048 positions, so the traced graph slices them by the runtime length.
  The int8 file uses the recipe of the shipped jina int8 file (section 4).
- **Rust**: the eval harness needs **no `core/` change**. `EmbeddingModel` already
  supports CLS pooling, a 768 width, a 1024 cap, no `token_type_ids`, and per-variant
  query and document prefixes (section 5). So the go/no-go is measured directly in the
  Rust harness. Python is used only for the export and the parity check, not for
  scoring. Adopting the model in the product is a separate task (section 5.2).
- **Biggest risk**: the rotary path. If the cos/sin table is frozen at the trace
  length, longer inputs break: they fail with a shape error, or worse, they do not.
  Parity at sequence lengths 1, 7, 512 and 1024, plus a batched padded row, is the
  control for that (section 3).

## 2. Model facts (HF API, 2026-11-20)

| item | value |
|---|---|
| repo / revision | `nomic-ai/CodeRankEmbed` @ `3c4b60807d71f79b43f3c4363786d9493691f8b1` (last modified 2025-06-24) |
| licence | **MIT** (card metadata). The base model, `Snowflake/snowflake-arctic-embed-m-long`, is Apache-2.0 |
| weights | `model.safetensors`, 546,938,168 bytes, fp32. No ONNX in the repo |
| architecture | `NomicBertModel`: 12 layers, 768 wide, 12 heads, SwiGLU MLP 3072, no biases on qkv/MLP, post-norm, rotary (base 1000, fraction 1.0, not interleaved, **no scaling**), `max_trained_positions` 2048, `n_positions` 8192, `type_vocab_size` 2 |
| custom code | `modeling_hf_nomic_bert.py` (sha256 `502ccfb9c2d5dac976109ac2f04dc3125d8540329e5d4af92bc587d6dc65edcd`) and `configuration_hf_nomic_bert.py` (`8632792e922e62ab1a6feaab15baf406e223c0547a21de23ab4f520b2b36d674`), both in **the same repo**. `auto_map` points at local files, so pinning `revision` pins the code too. Its imports are torch, einops, safetensors and transformers only. Despite `use_flash_attn: true` in the config, the attention forward is plain matmul + softmax, with no `flash_attn` import and no network or subprocess calls |
| pooling | CLS (`1_Pooling/config.json`, `pooling_mode_cls_token: true`). There is no `Normalize` module; the Rust loader L2-normalizes anyway |
| prefixes | query: `"Represent this query for searching relevant code: "` (`config_sentence_transformers.json` `prompts.query`; the card says the prefix "*must*" be included). Documents: none |
| tokenizer | BERT WordPiece, vocab 30,528, **uncased** (`BertNormalizer lowercase: true`). `[CLS] ... [SEP]`, `[PAD]` = 0. `tokenizer.json` sha256 `91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854`. It carries `truncation.max_length` 512 and a padding block; the Rust loader overrides both (section 5) |
| community ONNX | at least 10 third-party repos (`*/CodeRankEmbed-onnx`, `*-onnx-int8`, GGUF). **Not used**: their provenance and recipes are unknown. Nomic's own `nomic-embed-text-v1.5` (same NomicBERT code) ships `onnx/model.onnx` 547 MB and `model_quantized.onnx` 137 MB, which shows that this architecture exports cleanly and gives the int8 size to expect |

Licence note: MIT allows redistribution of a derived ONNX file with the copyright
notice. Shipping it would mean hosting our own export, because no official ONNX
exists. That is part of the adoption task, not this one (section 5.2). The custom
code is used only at export time and is never shipped.

## 3. Export and parity

### 3.1 Environment

A new venv at `~/Projects/ClaudeProjects/g-mesh-data/gm703/venv` (outside the repo,
like GM-436's), pinned in `eval/embedding/gm703/requirements.in` and a
hash-locked `requirements.txt` (`uv pip compile --generate-hashes`):

- `torch==2.2.2`: the last macOS x86_64 wheel; the machine is `x86_64`
  (`uname -m`, i7-1068NG7);
- `numpy==1.26.4`;
- `transformers==4.46.3`, `sentence-transformers==3.3.1` (GM-436's pins);
- `einops`, `safetensors`, `onnx==1.16.2`;
- `onnxruntime==1.19.2`, as in GM-730's probe venv.

`HF_HOME` is `g-mesh-data/gm703/hf`. The weights download (547 MB) happens once in the
export step, and every later step runs with `HF_HUB_OFFLINE=1`. The export script
checks the two custom-code sha256s above before `trust_remote_code=True` is honoured.
On a mismatch it stops.

### 3.2 Export: `torch.onnx.export`, not optimum

optimum has no `nomic_bert` ONNX config. Using it would mean writing a custom
`OnnxConfig` and resolving an optimum release compatible with torch 2.2.2 and
transformers 4.46: more moving parts for the same tracer underneath. The dynamo
exporter in torch 2.2 is experimental. Plan:

1. `AutoModel.from_pretrained(repo, revision=REV, trust_remote_code=True,
   add_pooling_layer=False)`. The `add_pooling_layer=False` matches
   `sentence_bert_config.json`. Then `.eval()`, fp32.
2. **Rotary pre-build.** For every layer's `attn.rotary_emb`, call
   `_update_cos_sin_cache(2048, device, torch.float32)`. During tracing the Python
   branch `seqlen > _seq_len_cached` is then false. `_cos_cached`/`_sin_cached` are
   baked in as 2048-row constants, and `apply_rotary_emb` slices them as
   `cos[0 : x.shape[1]]`, which the tracer records as a shape-derived `Slice`. Without
   this step the table would be frozen at the dummy input's length. 2048 covers the
   1024 cap with room to spare and matches the trained positions.
3. A wrapper `forward(input_ids, attention_mask) -> last_hidden_state` that passes no
   `token_type_ids`. The model fills them with `zeros_like(input_ids)`, and this
   matches the loader's `token_type_ids = false` path.
4. `torch.onnx.export(wrapper, (ids[2,16], mask[2,16]), opset_version=17,
   input_names=["input_ids","attention_mask"], output_names=["last_hidden_state"],
   dynamic_axes={both inputs: {0:"batch",1:"sequence"}, output: {0:"batch",1:"sequence"}},
   do_constant_folding=True)`. The dummy mask has one padded row.
5. `onnx.checker.check_model`, then confirm the inputs, the output and its trailing
   dimension 768.

Opset 17 is within ONNX Runtime 1.20 (`ort =2.0.0-rc.9`, the version `core/` links)
and ORT 1.19.2. The shipped jina int8 file is opset 11, so ORT does not require a high
opset. **Fallback: opset 14 once.** einops `rearrange`/`repeat` trace through
`reshape` on symbolic shapes, and the length-1024 parity case below shows whether any
shape got baked in.

### 3.3 Parity (fp32 ONNX vs PyTorch): method and tolerance

The sample is fixed and does not come from the eval's held-out answers. It has 160
texts, chosen by seed 703 from the v2 snapshots' node rows (doc plus code text as
stored) and the mechanical query files:

- 40 short texts (1-32 tokens);
- 40 medium texts (33-256);
- 40 long texts (257-1023);
- 20 texts longer than 1024 tokens, truncated to 1024 by the tokenizer;
- 20 query texts with the query prefix.

The tokens are counted with CodeRankEmbed's own tokenizer. Also included: the two
hand-made texts of length 1 (`""` -> `[CLS][SEP]`) and 7. The sample is written to
`eval/embedding/gm703/parity_sample.jsonl` (ids and texts) and committed.

Reference vectors come from PyTorch: the CLS row of `last_hidden_state`, L2-normalized.
`SentenceTransformer(repo, revision, trust_remote_code)` encodes 10 of the texts as a
cross-check of the reference itself (prefix, pooling).

| check | how | tolerance (stop if exceeded) |
|---|---|---|
| P1 fp32 ONNX vs torch, batch of one | onnxruntime CPU, same ids | per text: max abs diff <= 1e-4 and cosine >= 0.99999 on the normalized vector |
| P1b batched and padded | batches of 8 mixed lengths, right-padded with `[PAD]`=0, mask 0 | each row equals its batch-of-one ONNX vector within 1e-5 max abs |
| P1c sentence-transformers cross-check | 10 texts | cosine >= 0.99999 against the torch reference |
| P1d raw hidden state | full `last_hidden_state`, all positions, lengths 1/7/512/1024 | max abs <= 1e-3 (catches a rotary slice bug that CLS alone might hide) |
| P3 Rust vs Python on queries | the harness's `query_vectors.bin` for the CodeRankEmbed fp32 arm vs onnxruntime-python on the same prefixed query texts | cosine >= 0.99999 for every query. GM-700 saw Rust/Python fp32 differ by exactly 0, so any difference here is a finding |

The tolerances are set now, before any run. 1e-4 is about 100x float32 noise across
12 layers of these widths. A real rotary or mask bug moves vectors by 1e-1 or more.

### 3.4 int8: the shipped jina recipe, checked by fingerprint

The shipped jina int8 file (`model.onnx` sha `ed45870...`, from the jina repo, not
quantized by us) was inspected for this note:

- producer `onnx.quantize`, opset 11;
- 72 `MatMulInteger` with 48 `DynamicQuantizeLinear`;
- INT8 weights **per channel** (zero point of shape [768]) with values in
  **[-64, 64]**, which means `reduce_range=True`;
- `word_embeddings` and `token_type_embeddings` quantized per tensor to UINT8
  behind `DequantizeLinear` (the Gather op).

That is `onnxruntime.quantization.quantize_dynamic(fp32, int8, weight_type=QInt8,
per_channel=True, reduce_range=True)` with the default op types (MatMul and Gather).
The CodeRankEmbed int8 file uses exactly that call. `reduce_range=True` is required,
not inherited by habit: GM-700 (`docs/results/gm-700-int8-parity.md`) found that a Rust
process can get ORT's AVX2 U8S8 kernel, which saturates on 8-bit weights. 7-bit
weights cannot overflow it.

Checks on the int8 file:

| check | how | rule |
|---|---|---|
| Q-F fingerprint | the same inspection as above | every INT8 weight in [-64, 64], per-channel zero points, Gather quantized, no fp32 MatMul left on a 768x768 or 768x3072 weight |
| Q-D drift vs fp32 | cosine int8 vs fp32 ONNX over the parity sample; the same statistic for jina int8 vs jina fp32 (`work/models/jina-v2-base-code-fp32-r`) on the same texts | report the median and p1 of both. **Stop** if CodeRankEmbed's median < 0.98 or its p1 is more than 0.02 below jina's p1 |

Q-D is not parity: int8 is not supposed to equal fp32. Q-D only catches a broken
quantization before an hour of embedding. Whether int8 costs quality is answered by
the eval, from the fp32 arm (section 6).

Expected sizes: fp32 about 547 MB; int8 about 137-140 MB, against jina int8's
161.9 MB (0.85x).

## 4. Edit map (eval tooling only)

| file | change |
|---|---|
| `eval/embedding/gm703/requirements.in`, `requirements.txt` (new) | the pins of section 3.1, hash-locked |
| `eval/embedding/gm703/export_coderankembed.py` (new) | steps 1-5 of 3.2, then `quantize_dynamic`. It writes `<models>/coderankembed-fp32/{model.onnx,tokenizer.json,EXPORT.json}` and `<models>/coderankembed-int8/...`. `EXPORT.json` records the revision, the custom-code sha256s, the torch/onnx/ort versions, the opset, the quantize arguments and each `model.onnx` sha256. `<models>` = `~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work/models` (where the v2 work dir's other models live) |
| `eval/embedding/gm703/parity.py` (new) | builds `parity_sample.jsonl` (seed 703) and runs P1, P1b, P1c, P1d, Q-F and Q-D. It writes `parity.json` and exits non-zero on any stop rule. `--rust-queries <run dir>` runs P3 |
| `eval/embedding/gm703/test_gm703_parity.py` (new) | the Python half of section 8 |
| `eval/embedding/v2/variants.toml` (append) | the two rows of section 6.1 |
| `eval/training/gm436/report_gm436.py` `main()` (line 75, the `report` call) | add `--reference NAME`, passed through to `debug-embed-eval report` (today it gates against `settings.reference` = fp32) |
| `eval/embedding/measure_costs.sh` line 8 and every `debug-embed-eval` call (lines 38, 51, 64, report) | `REF=${REF:-jina-v2-base-code-fp32}` so the cost pass can use int8-structured as R; `EVAL_DIR=${EVAL_DIR:-eval/embedding}` passed as `--eval-dir` (today it reads only `eval/embedding/variants.toml`, which lacks the v2 rows); `SKIP_QUALITY=1` to skip its quality step (line 38), because the quality runs come from step 4/7 and must not `--force` over the shared v2 runs |
| `eval/embedding/gm703/measure.sh` (new) | the one background script of section 7 |

**No `core/` edits for this task.** The checks behind that claim:

- `core/src/embedding/model.rs`:
  - `EncoderSpec` (lines 239-247) carries pooling, dimension, max length and
    `token_type_ids`;
  - `load_with_target` (671-741) overrides the tokenizer's truncation with the spec's
    `max_sequence_length` and calls `with_padding(None)`;
  - `pool_checked` (994-1034) implements `Pooling::Cls` (`values[..dimension]`) and
    L2-normalizes;
  - inputs and outputs are the constants `input_ids` / `attention_mask` /
    `last_hidden_state` (160-166).
- `core/src/cli/embed_eval.rs`: queries are embedded as `query_prefix + text` (line
  1144) and nodes as `document_prefix + text` (961, 1050). The prefixes come from
  `Variant::query_prefix` / `document_prefix` in `cli/embed_eval/config.rs` (lines
  187-189).
- `config.rs:305` `Variant::model_dir` defaults to `work/models/<name>`, and
  `hf_repo`/`onnx_file` are optional (167-172).
- `pad_id`: `token_to_id("<pad>")` (`model.rs:713`) finds nothing in a BERT vocab and
  falls back to 0, which is CodeRankEmbed's `[PAD]`. Padded positions are masked
  anyway (P1b covers this).

## 5. Rust loader

### 5.1 For the eval: no gap

Callers relied on (g-mesh, project `g-mesh-wt-gm703`):

- `find_callers EmbeddingModel::load_with_target` gives `load_with_options`
  (`model.rs:660`), its test, and `cli::embed_eval::run_variant`
  (`embed_eval.rs:824`). So the harness loads through the same function as production.
  `find_definition` on `run_variant` shows that it builds the spec from
  `Variant::encoder_spec()`.
- `find_references EncoderSpec`, as files: `model.rs` (8), `embed_eval/config.rs` (2),
  `embed_eval.rs` (1), `pipeline.rs` (1). Production uses only
  `EncoderSpec::production()` (`pipeline.rs:238`).
- `find_callers Embedder::embed`: 2 refs in `pipeline.rs`, plus 4 untyped-receiver
  calls in `embed_eval.rs`, `embed_eval/cost.rs` and `embed_eval/probe.rs`, all eval.
  The production query path, `EmbeddingPipeline::embed_query` (`pipeline.rs:389`), was
  found with grep for `embed_query`, because g-mesh files that call under the untyped
  receiver. It is called from `mcp/search_code.rs:321`.
- `find_references EMBEDDING_DIM`, as files: 11 files (backfill, cache, pipeline,
  storage/language_swap, mcp tests). It stays 768, so none of them changes.

**The go/no-go does not need to be measured in Python first.** The harness runs the
export as is. Section 7's P3 proves that the Rust and Python vectors agree before any
scoring.

### 5.2 For adoption (a separate task, only on "go")

These items are listed so the owner can size a "go". None of them is done here.

| site | change |
|---|---|
| `core/src/embedding/pipeline.rs:389-397` `embed_query` | prepend the model's query prefix. Today there is no prefix concept in production. This is the only behavioural change in the search path |
| `core/src/embedding/model.rs:253-260` `EncoderSpec::production` | `Pooling::Cls` (dimension and cap unchanged). The test `the_production_spec_is_mean_pooling_768_wide_truncated_at_1024` (1198-1208) and `config.rs:338-350` (reference row == production spec) change with it |
| `model.rs:91-109` `DEFAULT_ONNX_REMOTE_PATH/SIZE/SHA256`, `DEFAULT_WEIGHTS_TAG`; `model.rs:216` `DEFAULT_MODEL_NAME`; `model.rs:124-130` `embedding_version` | new pins and a new version tag, so every stored vector is re-embedded (backfill and language_swap already key on `embeddingVersion`) |
| `core/src/cli/model.rs:78,86,185-205` `MODEL_REPO`, `MODEL_REVISION`, `FILES` | **hosting**: there is no upstream ONNX, so `g-mesh model fetch` needs a URL we control (our own HF repo or a GitHub release asset) that carries the MIT notice. This is an owner decision |
| `model.rs:713` `pad_id` | read the tokenizer's padding token instead of `"<pad>"` (it is correct only by coincidence) |
| `core/src/mcp/similarity.rs:290-301` `floor` | new per-language floors (ADR 0012's table), refit by the GM-697 method |
| ADR 0011 and 0012 | superseded or amended |

## 6. The eval arm

### 6.1 `variants.toml` rows (appended to `eval/embedding/v2/variants.toml`)

```toml
# --- GM-703: CodeRankEmbed on the shipped structured text ------------------
# No ONNX upstream: exported by eval/embedding/gm703/export_coderankembed.py from
# the pinned revision (EXPORT.json beside model.onnx records the sha256s).
# onnx_file names the export so a re-export with another file name changes the
# variant fingerprint; measure.sh also checks model.onnx's sha256 against EXPORT.json.
[[variant]]
name = "coderankembed-int8-structured"
arm = "model"
role = "quality"
hf_repo = "nomic-ai/CodeRankEmbed"
revision = "3c4b60807d71f79b43f3c4363786d9493691f8b1"
onnx_file = "gm703-export/model_int8.onnx"
model_dir = "work/models/coderankembed-int8"
pooling = "cls"
dimension = 768
max_tokens = 1024
token_type_ids = false
query_prefix = "Represent this query for searching relevant code: "
text = "structured"

[[variant]]
name = "coderankembed-fp32-structured"
arm = "model"
role = "informational"
# same as above except:
onnx_file = "gm703-export/model_fp32.onnx"
model_dir = "work/models/coderankembed-fp32"
```

- **Role.** int8 is the gated arm: it is what would ship. It is a *quality* candidate
  (D9: lower bounds of Δ r@10 and Δ MRR > 0, plus Q3-Q5, cost <= 1.5x pass time,
  <= 1.2x RSS, < 1 GB), because it is not cheaper than jina int8. The fp32 arm is
  informational: it separates "the model is worse" from "the quantization is worse".
- **Cap 1024, not the card's 8192.** jina is also capped at 1024 below its card's
  8192. The same cap keeps truncation out of the comparison (D8's rule against
  confounding), and it is below the 2048 trained positions. Not tuned on the eval.
- **Prefixes.** The query prefix is applied by the harness to every query, the
  mechanical name queries of the floor fit included, which is what `embed_query` would
  do in production. No document prefix (card).
- **Text form.** `structured`: the shipped form (`embedding::text`, ADR 0012), the
  same as the reference row `jina-v2-base-code-int8-structured`.
- **Reference.** `report --reference jina-v2-base-code-int8-structured`. That is the
  shipped model and text, and it already has v2 runs in the shared work dir
  (`g-mesh-eval-data/v2/work/runs/`), reused unchanged. `settings.reference` stays
  fp32 for the validity controls.
- **Fingerprint caveat.** The run manifest does not record the ONNX sha256 (the
  fingerprint is the variant's `Debug` text, `embed_eval.rs:614-637`). So a re-export
  under the same row would silently reuse stale vectors. `measure.sh` checks
  `model.onnx` against `EXPORT.json` before each run, and the first run uses `--force`.

### 6.2 What is reported (one table set, for int8 and for fp32)

All numbers come from the v2 runs. Each arm uses its **own floors, refit by D6 on the
v2 FIT half** (`report` refits them per arm and language).

- **D9 vs int8-structured**: Q1 r@10, Q2 MRR (point and one-sided 95% bound,
  bootstrap seed 398, 10,000 resamples), Q3 the worst language, Q4 confident-wrong,
  Q5 false alarm (pooled, plus each language), D7 validity against the random and
  shuffled arms, and the cost gate.
- **Per language**: r@10 and MRR for go, python, rust and typescript. **Rust is called
  out**: it is jina's weakest language on v2 (r@10 0.415 for int8-structured,
  GM-460/S19), so a code-trained retriever has the most room there. It also has the
  fewest held-out Q5 pairs (n 21-23), so its Q5 interval is the widest.
- **GM-434 columns**: `shipped_floor_rates.py --custom <arm's refit floors>` for false
  alarm per language and all, CW positives and CW absent. The jina rows are shown at
  both the shipped floors and the v2 refit floors, as in GM-460's table.
  CodeRankEmbed has no shipped floors.
- **Subsets**: the GM-398 subset and the confirm subset (`gm460_subset.py`), r@10 and
  floor rates at the v2-FIT floors, the same way GM-436 reported adapters.
- **Truncation share**: the share of node texts at 1024 tokens under each tokenizer.
  CodeRankEmbed's WordPiece is uncased and has half jina's vocabulary, so it may cut
  more code.
- **Cost (D11)**: pass time, max RSS, model size and median query latency, as ratios
  to jina int8-structured.

## 7. Measurement plan

Machine: the owner's Intel i7-1068NG7, 4 cores / 8 threads, 32 GB, macOS x86_64. At
the time of writing, `uptime` showed load **37-42** from other agents. Timing (D11)
does not start until the 1-minute load is below 7.0 (GM-398's adjusted threshold).
Every step records `uptime`, `/usr/bin/time -lp` and user/real.

One background script, `eval/embedding/gm703/measure.sh`, runs under
`caffeinate -i -s` and fails fast. It writes `summary.txt`, and the agent wakes once at
the end:

| step | what | script | estimate |
|---|---|---|---|
| 0 | venv, weights download (547 MB, pinned), code sha check | `export_coderankembed.py --fetch` | ~5 min |
| 1 | export fp32, then int8; `EXPORT.json` | `export_coderankembed.py` | ~5 min |
| 2 | P1, P1b, P1c, P1d, Q-F, Q-D (stop rules S1, S2) | `parity.py` | ~5 min |
| 3 | `cargo build --release --bin g-mesh` (this branch, no core change) | | ~10 min incremental |
| 4 | `debug-embed-eval run --eval-dir eval/embedding/v2 --variant coderankembed-int8-structured --force` over all 6 corpora (about 15.6k nodes plus the v2 queries) | harness | ~25-35 min (jina int8 ran 76 ms/node in GM-398) |
| 5 | P3 on that run's `query_vectors.bin` (stop rule S4) | `parity.py --rust-queries` | ~2 min |
| 6 | **early stop S6**: `report --reference jina-v2-base-code-int8-structured`. If the Q1 point Δ r@10 is <= 0, a quality candidate cannot pass, so skip steps 7-8, write "no-go" and finish | `report` | ~1 min |
| 7 | the fp32 arm, same as step 4 | harness | ~40-50 min |
| 8 | cost: 3 interleaved rounds of `--corpus g-mesh --embed-only --force` (6,774 nodes) for jina int8-structured and CodeRankEmbed int8, plus query latency; `costs.toml` | `REF=jina-v2-base-code-int8-structured VARIANTS=coderankembed-int8-structured EVAL_DIR=eval/embedding/v2 SKIP_QUALITY=1 measure_costs.sh` | ~55 min (about 3 x 2 x 9 min) |
| 9 | the full table set of 6.2 | `report_gm436.py --reference jina-v2-base-code-int8-structured --arm coderankembed-int8-structured --arm coderankembed-fp32-structured --arm jina-v2-base-code-int8-structured`, then `report --costs costs.toml` | ~5 min |

The total is about 2.5-3 hours on a quiet machine, or about 1 hour if S6 stops it.
Results go to `docs/results/gm-703-coderankembed.md`. Runs and models stay in the
git-ignored shared work dir.

## 8. Stop rules

| # | condition | action |
|---|---|---|
| S1 | export fails, or P1/P1b/P1d exceed tolerance, after one retry (opset 14, or rotary tables registered as buffers instead of cache attributes) | stop. Record "export not achieved", with the failing case and lengths. No eval |
| S2 | Q-F fails, or Q-D is below its rule | stop the int8 arm. Report whether the fp32 arm would be informative; the owner decides. Shipping fp32 (547 MB, about jina fp32's pass time, about 1.45x int8's) is a different proposal |
| S3 | the Rust harness cannot load the graph (ORT 1.20 rejects an op) | one retry at opset 14, then stop with ORT's error. A loader change would be needed, which is out of scope |
| S4 | P3: any query has a Rust/Python cosine < 0.99999 | stop before scoring and diagnose as GM-700 did: tokenization first, then the kernel |
| S5 | D7 validity fails for the arm | no verdict ("inconclusive = keep jina") |
| S6 | Q1 point Δ r@10 <= 0 vs int8-structured | no-go without the cost pass |

Go/no-go: **go** only if CodeRankEmbed int8 passes every D9 quality-candidate gate
against jina int8-structured, the cost gate included. Anything else is **no-go**:
fail, or between a pass and a fail. The confirm and GM-398 subsets are reported beside
the verdict and do not gate. If they disagree in sign with v2, that is said next to the
verdict. A "go" leads to the adoption task of section 5.2, and the F4 cross-encoder
rerank on top is re-checked there.

## 9. What the tests slice must pin (with controls)

Each behaviour gets one control: revert the code, never the test, and the test must
fail. Six controls in total:

| # | behaviour | test | control (revert) |
|---|---|---|---|
| T1 | the export's rotary tables are built for 2048 positions before tracing, so a length-1024 input matches torch | `test_gm703_parity.py`: export a **2-layer random-init NomicBERT** (same config, `n_layer=2`; no 547 MB download) and compare ONNX vs torch at lengths 7 and 300 | drop the pre-build step (trace at length 16): the length-300 case fails |
| T2 | dynamic axes: batch 3 with padding equals batch-of-one rows | same fixture, P1b logic | export without `dynamic_axes`: running batch 3 fails |
| T3 | the int8 recipe is the shipped one (`reduce_range`, per channel, Gather) | Q-F on the quantized 2-layer fixture | `reduce_range=False`: weights reach +/-127 and the test fails |
| T4 | the custom-code sha guard refuses changed remote code | point the guard at a copy of the modeling file with one byte changed | remove the guard: the test fails |
| T5 | the variant rows parse to the intended spec: CLS, 768, 1024, no token types, the exact query prefix, structured text | Rust unit test in `cli/embed_eval/config.rs`, beside `the_reference_variant_is_todays_production_behaviour` (336-350), reading `eval/embedding/v2/variants.toml` | change the row's pooling to mean: the test fails |
| T6 | `measure.sh` refuses a `model.onnx` whose sha256 differs from `EXPORT.json` | shell or Python test with a fake model dir | remove the check: the test fails |

The PyTorch-vs-ONNX parity of the real weights (P1) is a measure step, not a unit
test: it needs the 547 MB download. Pad id is not pinned: a control on it cannot fail,
because padded keys are masked, and the `model.rs:1862` test already covers that.

## 10. Owner decisions

1. **Run the fp32 arm as well (about +45 min)?** Recommended: yes. Without it, an
   int8 loss cannot be told apart from a model loss.
2. **Cost pass when quality fails (S6)?** Recommended: skip it, because cost cannot
   rescue a quality candidate.
3. **Hosting, if "go"**: our own HF repo, or a GitHub release asset for the exported
   ONNX. This is needed only for the adoption task.
