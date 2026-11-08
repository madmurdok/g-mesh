# GM-463: off-the-shelf reranker bake-off, quality x CPU latency (design)

Slice GM-463/S1. Read-only analysis of `docs/GM-463-reranker-bakeoff` at
b9441115 (release-4.27.0), plus Hugging Face API / model-card reads on
2026-11-05. No builds, no runs, no model downloads (only API metadata,
`config.json`, `tokenizer.json` and the last 300 kB of each ONNX file, to
read input names).

## Verdict

Run the bake-off as a **new driver, `eval/embedding/gm463_bakeoff.py`**, that
imports the GM-698 machinery (`gm462_ce_enriched.py`, `gm462_ce_text.py`,
`rerank_eval.py`) and adds models, a model-keyed cache and an offline latency
mode. **`gm462_ce_enriched.py` and `gm464_latency.py` are not edited**, so the
GM-698 runs stay reproducible by construction; `rerank_eval.py` and
`fetch_models.py` get additive changes only (new dict entries, defaults equal
today's behaviour).

Candidates (section 1): MiniLM-L6 (shipped, control), MiniLM-L12,
jina-reranker-v1-turbo-en, gte-reranker-modernbert-base, and for reference
only jina-reranker-v2-base-multilingual (CC-BY-NC-4.0, not shippable) and
bge-reranker-v2-m3 (0.57B). **Qwen3-Reranker-0.6B is dropped** (causal LM
scored through a chat prompt, no fp32 ONNX; bge-v2-m3 covers the 0.5B+
reference).

Four facts the owner should see before approving:

1. **Latency cannot go through the product path for any model but MiniLM-L6.**
   `rerank.rs` pins the ONNX/tokenizer sha256 (`check_rerank_files`,
   rerank.rs:97-110), always feeds `token_type_ids` (rerank.rs:234-243; no
   other candidate's ONNX takes it), and `WINDOW` is a const 30
   (rerank.rs:54). S5 therefore measures **offline**, with an ORT session set
   up exactly like `rerank.rs` (4 intra-op threads, level-3 graph
   optimisation, deterministic compute, CHUNK 16, length-sorted). MiniLM-L6
   is the anchor; the product's absolute numbers on a quiet machine come from
   GM-700/S1.
2. **The 0.5B reference is expensive.** Estimated ~28x MiniLM-L6's compute
   per pair: ~6 h for NL queries only at K=30, ~12 h for all queries
   (section 3.3). Recommended: NL only (Q1).
3. **The GM-698 work dir survives only in a `/private/tmp` scratchpad**
   (`.../5a9935bd-8e60-4173-91c1-12caf45deb4e/scratchpad/{work,cache}`,
   241 MB runs + 62 MB cache). S2 must copy it into the worktree first
   (section 2.0); if it is gone, the control needs a full re-score of the
   ship arm (~102.9k pairs).
4. **A reranker cannot fix recall.** It only reorders int8-structured's top
   K. S4 reports the window ceiling (share of held-out NL positives with an
   expected node in the top K); "GM-436 unnecessary" is defined against that
   ceiling (section 3.6, Q6).

## 1. Candidates

Revisions are the HF API `sha` read on 2026-11-05
(`https://huggingface.co/api/models/<id>`); licences from the model card
front matter (`https://huggingface.co/<id>/raw/main/README.md`); shapes from
`config.json` at that revision. ONNX input names are read from the tail of
the ONNX file (strings `input_ids`, `attention_mask`, `token_type_ids`,
`logits`); S2 confirms them with `sess.get_inputs()` (C8).

| key (CE_DIR name) | HF repo @ revision (lastModified) | params / shape | max seq | licence | ONNX (official unless noted) | tokenizer / pair template | ONNX inputs | trust_remote_code |
|---|---|---|---|---|---|---|---|---|
| `ce-minilm` (shipped) | `cross-encoder/ms-marco-MiniLM-L6-v2` @ `233902d25c440f23af6f7d6e94d2946bac0bee0a` (2026-08-09; same sha as `CES` and `RERANK_MODEL_REVISION`) | 22.7M; 6 L x 384 | 512 | Apache-2.0 (weights; trained on MS MARCO, see R4) | `onnx/model.onnx` 91,011,230 B, sha256 `5d3e70fd...1d4d4a` (= `RERANK_ONNX_SHA256`); int8: `model_qint8_avx512_vnni.onnx` 23.2 MB, `model_quint8_avx2.onnx` | WordPiece; `[CLS] A [SEP] B [SEP]`, type ids 0/1 | ids, mask, token_type_ids | no |
| `ce-minilm-l12` | `cross-encoder/ms-marco-MiniLM-L12-v2` @ `7b0235231ca2674cb8ca8f022859a6eba2b1c968` (2025-08-29) | 33.4M; 12 L x 384 | 512 | Apache-2.0 (MS MARCO data, R4) | `onnx/model.onnx` 133,743,863 B, sha256 `2ac389ab6abe08dcbddf2c41a69b6a18853e0dca440adbc2abc1adfcee46483f`; int8: `model_qint8_avx512_vnni.onnx` 34.3 MB | same WordPiece vocab as L6 (tokenizer.json 711,396 B) | ids, mask, token_type_ids | no |
| `ce-jina-turbo` | `jinaai/jina-reranker-v1-turbo-en` @ `b8c14f4e723d9e0aab4732a7b7b93741eeeb77c2` (2025-09-10) | 37.8M; 6 L x 384, ALiBi | 8192 | **Apache-2.0** (card front matter; not NC) | `onnx/model.onnx` 151,296,975 B, sha256 `c1296c66c119de645fa9cdee536d8637740efe85224cfa270281e50f213aa565`; int8: `model_int8.onnx` 38.3 MB | BPE (61k vocab), RobertaProcessing `<s> A </s></s> B </s>`, pad `<pad>` | ids, mask (no token_type_ids) | yes for PyTorch (JinaBert `auto_map`); **not needed for ONNX** |
| `ce-jina-v2` (reference only) | `jinaai/jina-reranker-v2-base-multilingual` @ `9cfeff2df7d40d1b78e75e5e9cebec92a99813c9` (2025-10-21) | 278M; 12 L x 768 (XLM-R) | 1024 | **CC-BY-NC-4.0**: "licenced for research and evaluation purposes ... For commercial usage, please refer to Jina AI's APIs" (card line 41) | `onnx/model.onnx` 1,114,040,223 B, sha256 `0ef3f7978f7bc52360864d74edc1a0e03d159af770a7767c4d5943496e616012`; int8: `model_int8.onnx` 279.6 MB | SentencePiece Unigram (250k), `<s> A </s></s> B </s>`, pad `<pad>`; tokenizer.json sha256 `3a56def25aa40fac...` | ids, mask | yes for PyTorch (flash-attn XLM-R `auto_map`); not for ONNX |
| `ce-gte-mb` | `Alibaba-NLP/gte-reranker-modernbert-base` @ `f7481e6055501a30fb19d090657df9ec1f79ab2c` (2025-07-04) | 149.6M; 22 L x 768, GLU 1152, local attn 128 / global every 3rd | 8192 | Apache-2.0 | `onnx/model.onnx` 598,803,940 B, sha256 `c6d3226502addbcd4d2cf273802957ebf8a2a6bf94037dcb9b1d95bfc01e5d93`; int8: `model_int8.onnx` 150.9 MB | BPE (50k, OLMo-style), `[CLS] A [SEP] B [SEP]`; **tokenizer.json ships padding and truncation(8000) enabled** (R2) | ids, mask | no (native in transformers >= 4.48) |
| `ce-bge-m3` (0.5B+ reference) | weights `BAAI/bge-reranker-v2-m3` @ `953dc6f6f85a1b2dbfca4c34a2796e7dde08d41e` (2024-06-24); ONNX `onnx-community/bge-reranker-v2-m3-ONNX` @ `6f5ff65298512715a1e669753bc754d2bc8f367b` (2025-09-01) | 568M; 24 L x 1024 (XLM-R large) | 8192 | Apache-2.0 (base); the ONNX repo has no licence field, derived from the base | community export: `onnx/model.onnx` 656,891 B (sha256 `faae32b1...012486`) + `onnx/model.onnx_data` 2,271,088,656 B (sha256 `f009aa6c6cf21986fd7e0021fa66b20ccce27abc6900a57c7109c8496811bcbe`); int8 `model_quantized.onnx` 570.7 MB | Unigram (250k), `<s> A </s></s> B </s>`; use the ONNX repo's tokenizer.json (17,082,900 B, sha256 `8bf8afbf...48d3`; differs in bytes from BAAI's 17,098,273 B) | ids, mask | no |
| ~~Qwen3-Reranker-0.6B~~ dropped | `Qwen/Qwen3-Reranker-0.6B` @ `e61197ed45024b0ed8a2d74b80b4d909f1255473` (2026-04-16) | 0.6B; 28 L x 1024, causal LM | 32k | Apache-2.0 | none official; `onnx-community/Qwen3-Reranker-0.6B-ONNX` has only `model_q4.onnx` / `model_quantized.onnx` (no fp32), transformers.js causal-LM export | Qwen BPE, chat template | - | no |

Why Qwen3 is dropped: it is `Qwen3ForCausalLM`, scored as the last-token
logit of `yes` vs `no` after a prompt (`<|im_start|>system\nJudge whether
the Document meets the requirements ... "yes" or "no".<|im_end|>\n<|im_start|>user\n<Instruct>: ...\n<Query>: ...\n<Document>: ...<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n`,
card lines 110-149). Scoring it would mean: build that prompt per pair
(~60 extra tokens, so ~160 tokens per pair), run the causal LM, take
`logit(yes) - logit(no)` at the last position as the CE score (what
`tomaarsen/Qwen3-Reranker-0.6B-seq-cls` bakes into a one-logit head, but that
repo has no ONNX). The only ONNX is quantized (q4/int8), so its quality is not
comparable with the fp32 arms, and the export needs past-key-value inputs.
Exporting fp32 needs torch + optimum, which the machine does not have
(Python 3.9.6, onnxruntime 1.19.2, tokenizers 0.22.2, numpy 2.0.2; no torch,
transformers, optimum, onnx). bge-reranker-v2-m3 is a plain cross-encoder on
the existing scoring path and covers the 0.5B+ reference.

Considered and not added (owner may add, Q7):
`ibm-granite/granite-embedding-reranker-english-r2` @ `d09d3d6971b689bf9c23839e45a470874d46e13a`
(149M ModernBERT, Apache-2.0, card: "we do not use the popular MS-MARCO
retrieval dataset ... due to its non-commercial license", tokenizer "optimized
with code and text data"). It has no official ONNX (only small community
exports), so it needs an export environment. It is the cleanest
data-licence option if a ModernBERT model wins.

Published code numbers (context, not our metric): gte-reranker-modernbert
CoIR 79.99 (card table); jina-v2 CodeSearchNet MRR@10 71.36 vs bge-v2-m3
62.86 (jina card); MiniLM L6 vs L12 are equal on MS MARCO (MRR@10 39.01 vs
39.02, card) at half the speed, so L12 is the "more depth, same data" probe.

## 2. Harness edit map for S2

### 2.0 Work dir (first, before any code)

- Copy the GM-698/S7 work into the worktree's git-ignored
  `eval/embedding/v2/work/` (`.gitignore:22`): `runs/` as real files
  (`cp -R`), the 12 `*.sqlite` / `*.snapshot.json` and `corpora` as symlinks
  to `g-mesh/eval/embedding/work/` as the scratchpad has them, and the
  scratchpad `cache/` to `eval/embedding/v2/work/rerank_cache/` (the six
  `gm462-ce-minilm.<corpus>.052802538ed47171.json` files hold the ship and
  off pairs). Source:
  `/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/5a9935bd-8e60-4173-91c1-12caf45deb4e/scratchpad/{work,cache}`
  (run script `.../scratchpad/gm698m/run.sh`).
- `models/` must be a **real directory** in the worktree's work dir (the
  scratchpad's `models` is a symlink into the main checkout; downloads must
  not land there). Put a symlink `models/ms-marco-MiniLM-L6-v2 ->
  g-mesh/eval/embedding/work/models/ms-marco-MiniLM-L6-v2` in it; new models
  download beside it (~4.3 GB fp32 in total, bge 2.27 GB of it).

### 2.1 `eval/embedding/rerank_eval.py` (additive)

| lines | what | change |
|---|---|---|
| 65-69 `CES`, `CE_DIR` | name -> (repo, revision), name -> dir | Add the five new keys of section 1 with their repo/revision and dirs (`ms-marco-MiniLM-L12-v2`, `jina-reranker-v1-turbo-en`, `jina-reranker-v2-base-multilingual`, `gte-reranker-modernbert-base`, `bge-reranker-v2-m3`). **Do not iterate `CES` blindly:** `latency_only` (529) and `main` (777) loop over all of `CES`; add a new dict `CE_BAKEOFF` for the new keys instead of growing `CES`, and give `CE_DIR` all keys. |
| new, beside 65 | `CE_FILES` | name -> {remote path: local name}: `onnx/model.onnx` -> `model.onnx` (+ `onnx/model.onnx_data` -> `model.onnx_data` for bge, the name ORT resolves from the graph), `tokenizer.json`, `config.json`; plus pinned LFS sha256 for the ONNX files from section 1. bge's revision for files is the onnx-community one. |
| 373-387 `CrossEncoder.__init__` | loads tokenizer + session | (a) after `enable_truncation`, call `self.tok.no_padding()` (no-op for MiniLM, whose tokenizer.json has `padding: null`; required for gte-mb, R2). (b) hash `model.onnx_data` into `self.files` when it exists (else the cache key misses bge's weights). (c) optional kwarg `rust_like=False`: when true set `so.graph_optimization_level = ORT_ENABLE_ALL` and `so.use_deterministic_compute = True` (rerank.rs:174-178); used only by the latency mode, default keeps every existing caller's session unchanged. The pad lookup (`[PAD]`, `<pad>`) already covers every kept candidate. |
| 389-408 `CrossEncoder.score` | length-sorted chunks of 16 | No change. It already feeds `token_type_ids` only when the graph has it (406) and takes the first output reshaped to (n,). |

Callers of `CrossEncoder` that must see no behaviour change (C3 proves it
for MiniLM): `ce_scores` 411, `latency_only` 522, `latency_f4` 558 (g-mesh),
and `R.CrossEncoder` in `gm441_rerank.py:111`, `gm464_ce_structured.py:220`,
`gm462_ce_enriched.py:568`, `gm464_rerank_fixture.py:78` (grep, see
appendix).

### 2.2 `eval/embedding/fetch_models.py` (additive)

| lines | change |
|---|---|
| 49-66 `main` | New flag `--ce [name ...]`: for each key of `R.CE_BAKEOFF` (import `rerank_eval` by path as the other scripts do), download `CE_FILES` from `https://huggingface.co/<repo>/resolve/<revision>/<remote>` into `<work>/models/<CE_DIR[name]>/` with the existing `fetch` (41-46), take `--work` (default `eval/embedding/v2/work`), verify the pinned sha256 of every LFS file and **stop on a mismatch**, print bytes + sha256 of every file. Without `--ce`, behaviour unchanged. |

### 2.3 New `eval/embedding/gm463_bakeoff.py`

Imports `rerank_eval as R`, `gm462_ce_text as T`, `gm462_ce_enriched as E`,
`from gm464_ce_structured import c1_fixture, flips, render`. Sets
`E.PROGRESS` to its own file before any `E.Progress` is made (the class
reads the module global at write time, `gm462_ce_enriched.py:66,90-121`).

Reused unchanged from `gm462_ce_enriched.py`: `add_structured` 129, `f4` 151,
`row` 169, `sig` 180, `fit_obj` 185, `EnrichedCache` 200-235 (for
`ce-minilm` only), `score_texts` 238, `load_nodes` 243, `build_texts` 264,
`in_scope`/`text_of` 272-277, `need_pairs` 280, `score_missing` 297,
`window_pairs` 326, `c1_off_is_base` 330, `paired` 415, `fmt_d` 427,
`d9_pass` 435, `bucket_of` 451, `strict_ho` 501. Read for context: `main`
546-801 (the order of loading, controls and rows to copy), `c3_rescore`
352-374, `bucket_table` 472-498 (hard-wires the labels off/all, so the new
driver writes its own split table with `bucket_of` + `paired`).

Pieces S2 writes:

- **`ModelCache(E.EnrichedCache)`**: for any model but `ce-minilm`, file
  `gm463-<name>.<corpus>.<key>.json`, key = sha16 of
  `[files sha256 (onnx [+ data] + tokenizer), R.MAX_TOKENS, text tokenizer id, "gm463 text-keyed v1"]`,
  no base (gm464) fallback (`has`/`get` look only at its own file). Pairs are
  keyed inside by (qid, sha16(text)) as today, so text variants (off, ship,
  any B budget) share one file per model without collisions. For
  `ce-minilm` the driver uses `E.EnrichedCache` itself, unchanged, so the
  control reads the GM-698 file `gm462-ce-minilm.*.052802538ed47171.json`.
- **Texts**: arms `off` and `ship` from `E.ARMS`. `build_texts(c, arms, tok,
  bb)` with `tok = T.token_counter(<tokenizer>)`, the tokenizer per Q3
  (recommended: the model's own, as `ce_text.rs` counts with the reranker's
  tokenizer, rerank.rs `count_tokens`). With per-model texts, keep
  `c.atext` per model (a dict `c.atext_by[model]`) and swap it in before
  `text_of`/`need_pairs` run for that model. For the off arm only MiniLM-L6
  is scored (it is the "off" reference row), not every model.
- **`quality` mode**: `--models`, `--k` (scoring window, default 30),
  `--report-k 20,30` (prefix windows reported from the same scores),
  `--scope all|nl` per model (bge: `nl`, Q1), `--dry-run-pairs N` (score N
  ship pairs per model, print ms/pair and the ETA, stop), `--table`,
  `--summary`. Per model: score missing ship pairs, then controls C3'/C7/C8
  (section 3.4), beta tune on FIT with `E.fit_obj` over `R.BETAS`, extended
  per the edge rule (3.2), rows via `E.row`, paired deltas via `E.paired`.
- **`latency` mode** (section 3.5): one model per process
  (`latency-arm --model X`), driven by `latency --models ...` which runs each
  arm under `/usr/bin/time -p` like `gm464_latency.main_run`
  (`gm464_latency.py:341-363`) and reuses its `load1`, `uptime`, `therm`,
  `gate` (72-85, 215-238) by import. The timed op is `latency_f4`'s inner `f4`
  (rerank_eval.py:572-579: tokenize + ORT + blend + sort), with the model
  under test and the ship texts.
- **Mean pair tokens**: query + text + specials with the model's tokenizer
  (`len(tok.encode(q, t).ids)`), over (a) the latency set and (b) the whole
  K=30 window of every corpus, per model.

### 2.4 Not touched

`gm462_ce_enriched.py`, `gm462_ce_text.py`, `gm464_ce_structured.py`,
`gm464_latency.py`, `core/` (no Rust change: product latency is GM-700's;
porting a new model to Rust is a follow-up only if one wins), the GM-698
caches (read, never written: `ce-minilm` has 0 missing ship pairs at K=30).
If K=50 quality runs for MiniLM-L6 (Q5), its new pairs go to a `gm463-`
file, not the GM-698 one: give `ce-minilm` a `ModelCache` for writes and
read the GM-698 file as its base.

## 3. Measurement plan (S3-S5)

### 3.1 Data and split (as GM-698)

Eval dir `eval/embedding/v2` (1652 NL queries: 330 positives + 83 absent per
language; 1779 name queries; 3431 total = 102,930 / 30, the GM-698 pair
count). Recall stage int8-structured (`--st-runs runs/jina-v2-base-code-int8-structured`).
FIT/held-out: `R.held_out` (rerank_eval.py:88: sha256(qid)[0] odd and not
mechanical = held-out); beta tuned on FIT NL with `E.fit_obj` (best FIT NL
r@10, then MRR, then smaller beta); everything reported on held-out NL;
strict held-out (`E.strict_ho`) as a second line. Bootstrap seed/resamples
from `v2/variants.toml` as `E.main` does.

### 3.2 Arms and rows (per model, held-out NL)

Rows: int8-structured (no rerank); off = MiniLM-L6 on the structured text at
beta 80 (GM-698's off); ship-L6 at beta 40 (the shipped arm); each candidate
on the ship text at its FIT-tuned beta, and at beta 40 for reference.
Columns: r@10 and MRR [CI], D9 vs int8-structured at the shipped floors
(`row()["vs"]["int8"]`), name r@10 / MRR, GM-434 columns (as `E.row`).
Paired candidate minus ship-L6 and candidate minus off: dr@10, dMRR on
held-out NL, strict held-out, and name queries. Split: documented /
signature-only / mixed (`E.bucket_of`) with n, r@10 / MRR per arm and the
paired delta vs ship-L6. Report K=20 and K=30 from the same scores (K=20 is a
prefix of K=30).

Beta grid: `R.BETAS` (0 ... 320). Logit scales differ by model (bge's card
example spans -8.2..5.3, gte-mb's -1.7..2.5), so the pre-registered edge rule:
if a model's FIT optimum is the grid's first or last value, extend
geometrically on that side (x2 up to 2560, or /2 down to 0.125) until the
optimum is interior, and report the extension.

Window ceiling: share of held-out NL positives with an expected node in
int8-structured's top K (K=20, 30, 50); no model can exceed it at r@K.

### 3.3 Pair counts and wall time (S4)

Pairs per model at K=30, all queries: 102,930 (NL only: 49,560). K=50 adds
68,620 (all) / 33,040 (NL). MiniLM-L6 needs 0 new pairs at K=30.

Cost per pair relative to MiniLM-L6, from non-embedding parameters (the
per-token FLOPs at ~100-token pairs; attention is small at that length):
L6 10.6M -> 1x; L12 21.2M -> 2x; jina-turbo 6 L x 384 -> ~1x (more BPE
tokens per pair possible); jina-v2 85M -> ~8x; gte-mb 22 x 5.0M = 110M ->
~10x; bge-m3 302M -> ~28x. Base rate: GM-698 measured 13.2 ms/pair on the
off text under load ~60 (C3, `gm698m/stdout.txt`) and 37-46 ms/pair on ship
pairs under load 54-62; an idle ship-pair rate of ~15 ms is assumed here
(uncertain by 2x; each model's dry run replaces it).

| model | ms/pair (est.) | K=30 all queries | K=30 NL only | +K=50 (all) |
|---|---|---|---|---|
| MiniLM-L6 | 15 | 0 (cached) | 0 | 17 min |
| MiniLM-L12 | 30 | 51 min | 25 min | 34 min |
| jina-turbo | 15-20 | 26-34 min | 12-17 min | 17-23 min |
| jina-v2 (ref) | 120 | 3.4 h | 1.7 h | 2.3 h |
| gte-mb | 150 | 4.3 h | 2.1 h | 2.9 h |
| bge-m3 (ref) | 420 | **12 h** | **5.8 h** | 8 h |

S4 order: cheap to expensive, one background run per model (its own summary
file, fail-fast), each preceded by a 200-pair dry run that prints ms/pair and
the ETA; **stop and report** if any model's ETA exceeds 2x the table. The
metrics pass on cached scores took 994 s real / 2852 s user in GM-698 (two
arms); budget ~10 min per model for metrics.

Progress: one overwritten line per stage in
`~/.claude/progress/GM-463-S4.txt` (S5: `GM-463-S5.txt`) via `E.Progress`
(start, every 10%, every 5 min, end; load average on the line). The agent
never reads it while waiting.

### 3.4 Controls (S3 runs C0, C3', C7, C8 before S4's long runs)

- **C0 (acceptance control):** `gm463_bakeoff.py quality --models ce-minilm
  --k 30` on the copied work dir reproduces gm-698-ship-arm.md: held-out NL
  r@10 0.670 [0.641, 0.700], MRR 0.465 [0.439, 0.493], FIT-tuned beta 40,
  name 0.971 / 0.889, off at beta 80 0.649 / 0.427; 0 pairs scored. A
  mismatch stops S4.
- **C1, C5:** as GM-698 (`E.c1_off_is_base`; rerank disabled ==
  int8-structured on every outcome).
- **C3' (per model):** 200 cached ship pairs rescored in different batches,
  max |diff| <= 1e-4 (fp32 batch independence; 1e-5 for MiniLM as GM-698).
- **C7 known answer (per model):** the card's example pairs scored through
  `R.CrossEncoder` match the card's printed logits: MiniLM-L12
  `[9.218911, -4.0780287]` (card lines 28-33), tolerance 1e-3; gte-mb
  `[2.1387, 2.4609, -1.6729]` (card lines 61-72, printed at reduced
  precision), tolerance 0.05; bge-m3 `[-8.1875, 5.26171875]` (card lines 69-70,
  fp16), tolerance 0.05. The jina cards print no scores: check that the
  card's relevant example document ranks first, and mark C7 as weaker.
- **C8 input format (per model):** `sess.get_inputs()` names equal section
  1; one encoded pair's special-token layout equals the tokenizer's pair
  template; no pad ids inside an unpadded encoding (catches R2); a pair of
  700 tokens is truncated to 512.
- **Separates the arms:** each candidate's held-out ranking differs from
  ship-L6's on at least one query (flip counts right->wrong / wrong->right,
  `flips`), and its mean pair tokens are reported beside L6's.

### 3.5 Latency (S5)

Method mirrors gm-698-latency.md where it can and says where it cannot:

- **Path:** offline, Python ORT 1.19.2 CPU EP, `CrossEncoder(...,
  rust_like=True)`: intra-op threads 4 (= rerank.rs `min(4, physical
  cores)`; this machine has 4 physical cores, Intel i7-1068NG7, 32 GB),
  level-3 graph optimisation, deterministic compute, CHUNK 16,
  length-sorted. The product uses ort 2.0.0-rc.9 (`core/Cargo.toml:110`);
  ORT versions differ, so absolute ms are offline ms, compared across models
  on equal terms. Excludes recall, text building and source reads.
- **Queries:** the 30 queries `gm464_latency.queries()` uses (first 30
  positives of `queries/g-mesh.jsonl`, the GM-698 latency set), their
  int8-structured top-K windows from the v2 runs, ship texts. Control: L6's
  mean pair tokens on this set at K=30 reproduce gm-698-latency.md's 101.9.
- **Per model arm:** fresh process; load model; first call timed alone
  (load + first inference, reported separately); one untimed warm-up pass;
  3 timed passes at each K in {20, 30, 50} (bge: 1 timed pass, ~1 h per arm
  otherwise). Metric per K: p50 / p95 / mean over 90 timed calls, plus ms per
  pair.
- **Order and gate:** arms `L6 L12 turbo gte jv2 bge bge jv2 gte turbo L12 L6`
  (each model twice, mirrored). Before each arm: load gate (1-min load < 4
  held 30 s, cap 1800 s, as gm464_latency) **and** pmset `CPU_Speed_Limit ==
  100` (GM-698 saw 46-78% by arm end). Per arm: `uptime` and `pmset -g therm`
  before/after, `/usr/bin/time -p` real/user/sys. Here the process itself
  computes, so user/real ~ 3-4 is expected; user/real << 1 marks the arm as
  waiting and invalid.
- **Optional int8 rows (feeds GM-700, latency only):** L6 and the
  recommended model with their official int8 ONNX
  (`model_qint8_avx512_vnni.onnx` for MiniLM: this CPU has AVX-512 VNNI;
  `model_int8.onnx` for jina/gte). Quality of int8 is GM-700's.

### 3.6 What decides (pre-registered, owner confirms Q6)

A **shippable** candidate (Apache/MIT, ONNX available, so not jina-v2) is
recommended over MiniLM-L6 if all hold on held-out NL at K=30:
1. paired dMRR vs ship-L6, lower bound > 0;
2. paired dr@10 vs ship-L6, lower bound >= -1.0 pt;
3. D9 vs int8-structured at the shipped floors passes;
4. name-query MRR paired vs ship-L6, lower bound >= -0.010 (GM-698's guard);
5. offline p50 at K=30 <= 2.0x MiniLM-L6's (Q6).
Among passing models: highest dMRR point estimate, ties to the cheaper.
Documented and signature-only deltas are reported, not gated.

**GM-436 unnecessary (for the reranker side) if** the best shippable model's
held-out NL r@10 reaches >= 90% of the K=30 window ceiling (3.2). Then the remaining loss is recall, which only the embedding
side (GM-436 proper: adapter, distillation) can move. If the best model stays
well below the ceiling, it (or MiniLM-L6 if nothing passes) is the base
model for any cross-encoder fine-tune in GM-436. jina-v2 and bge-m3 rows
bound what a bigger off-the-shelf model buys; if even bge-m3 does not pass
rule 1, model size is not the lever and fine-tuning on code pairs is.

## 4. Risks and questions for the owner

Each question has the recommended option first.

**R1 / Q1. Scope of the 0.5B+ reference.** Today: no model above 37M has
been run in this setup. Change: bge-reranker-v2-m3 (568M) as the size
reference. Example: at ~28x MiniLM-L6's cost per pair, scoring all 3431
queries at K=30 takes ~12 h; NL only (1652 queries) ~6 h. Consequence: NL
only gives held-out r@10/MRR and the documented split but no name-query MRR
and no D9 (which needs all queries).
- (a) **NL only, K=30 (Recommended).** ~6 h; answers "does size help" on the
  metric that decides. Risk: no D9/name numbers for it (it is not shippable
  at this size anyway).
- (b) All queries. ~12 h; full row. Risk: half a day of machine time for a
  reference row.
- (c) Drop the 0.5B reference. Saves 6-12 h. Risk: no answer to "is a
  bigger model the lever", which decides between switching models and
  fine-tuning (GM-436).

**R2. gte-mb tokenizer pads and truncates by default.** Today:
`R.CrossEncoder` never disables padding; MiniLM's tokenizer.json has none, so
it never mattered. gte-modernbert's tokenizer.json enables padding and
truncation at 8000. With padding on, `encode_batch` returns padded ids and
`score()` would set attention 1 on pad positions, silently corrupting
logits. Fix in 2.1 (`no_padding()`), proven by C8 and C7. Not a question.

**Q2. jina-reranker-v2 (CC-BY-NC-4.0).** Today: not evaluated. Change: run it
as a reference row. Example: it is the only candidate with code in its
training mix and published CodeSearchNet numbers (MRR@10 71.4 vs bge-m3's
62.9). Consequence: its row can show what code training buys, but the card
says commercial use needs Jina's API or marketplace offerings, so it cannot
ship in g-mesh.
- (a) **Include as reference only, flagged "not shippable" (Recommended).**
  ~3.4 h. Risk: a winning row nobody can ship.
- (b) Drop. Saves ~3.4 h. Risk: loses the only code-trained data point.

**Q3. Which tokenizer counts the 48-token body budget.** Today: the B48
budget is counted with MiniLM's WordPiece (Python `T.token_counter`, Rust
`count_tokens` of the loaded reranker). Change: other models have other
tokenizers. Example: a 48-WordPiece-token body may be 40 or 60 BPE tokens, so
the text bytes differ by model. Consequence: own tokenizer measures what would
ship; MiniLM's tokenizer keeps the text bytes identical across models.
- (a) **Each model's own tokenizer (Recommended):** what the product would
  do after a switch; mean pair tokens reported. Risk: the comparison mixes
  "model" with "slightly different text".
- (b) MiniLM's tokenizer for all: identical text. Risk: the winning row is
  not exactly what would ship; a re-run would be needed.

**Q4. Latency path.** Today: gm-698-latency.md measures the real product
(`g-mesh mcp-shim`, real daemon). Change: other models cannot load in the
product without Rust changes (sha pin, `token_type_ids` always fed, `WINDOW`
const 30). Example: pointing `G_MESH_RERANK_MODEL_DIR` at gte-mb fails at
`check_rerank_files`. Consequence: offline numbers compare models with each
other, not with the product's end-to-end ms.
- (a) **Offline ORT bench mirroring rerank.rs, MiniLM-L6 as anchor
  (Recommended).** No Rust change. Risk: offline != product ms (ORT 1.19.2 vs
  the ort crate's runtime; no text build); GM-700/S1 supplies product ms.
- (b) A Rust branch that loads any model (unpinned dir, optional
  token_type_ids, env K) and reuses gm464_latency. Risk: a code + review +
  tests slice inside a research task.

**Q5. Quality window.** Today: the product reranks K=30. Change: latency is
asked at K=20 and K=50. Example: K=50 raises the window ceiling (more
positives reachable) but adds 68.6k pairs per model (+67%). Consequence:
K=30 alone cannot say whether a stronger model profits from a wider window.
- (a) **K=30 for all (K=20 free as its prefix); K=50 only for models that
  pass 3.6 at K=30 (Recommended).** Risk: a model that only wins at K=50 is
  missed.
- (b) K=50 for every model. Risk: ~+8 h in total.

**Q6. Decision thresholds.** Today: GM-698 used fixed bars against GM-462's
lower bounds. Change: section 3.6 rules 1-5 and the GM-436 ceiling rule.
Example: gte-mb at +0.02 MRR [+0.005, +0.035] and 3.5x L6's latency would
fail rule 5 at 2.0x. Consequence: the latency multiplier decides whether any
base-size model can be recommended.
- (a) **Rules 1-5 as written, latency <= 2.0x, ceiling share >= 90%
  (Recommended).** Risk: excludes base-size models unless int8 (GM-700)
  closes the gap; the report still shows them.
- (b) Latency <= 4.0x. Risk: +1 s per first page is plausible at the
  product's current ~0.8 s p50.
- (c) No latency rule; report the trade-off only. Risk: no pre-registered
  verdict.

**Q7. granite-embedding-reranker-english-r2.** Today: not a candidate. It is
ModernBERT (149M, Apache-2.0), trained without MS MARCO (cleaner data
licence than every other candidate), code-aware tokenizer, but no official
ONNX. Consequence of adding: an export environment (torch + optimum) and a
PyTorch-vs-ONNX parity control.
- (a) **Not this round; revisit if gte-mb wins (Recommended).** Risk: may
  miss a better shippable model.
- (b) Add now with the export. Risk: a new toolchain on this machine and ~4 h
  more scoring.

**R3. Work dir in /private/tmp.** The only copy of the GM-698 v2 runs and
ship cache is in a session scratchpad. If it is cleaned before S2 copies it,
C0 needs ~102.9k ship pairs rescored (~45 min at 26 ms/pair, longer under
load) and the v2 runs rebuilt per `v2/setup_work.sh`. S2 copies it first.

**R4. MS MARCO data terms.** MiniLM-L6/L12 weights are Apache-2.0 but trained
on MS MARCO, whose terms are non-commercial (the granite card says so
explicitly). This is today's shipped posture, not a new risk from this task;
noted because jina-turbo, gte-mb and bge-m3 publish no training-data
restriction on their cards.

**R5. Python 3.9 / ORT 1.19.2 and ModernBERT.** The gte-mb ONNX is a
transformers.js export; ORT 1.19.2 may lack an op it uses. C8 loads every
model before S4 starts; a load failure drops the model with the error quoted.

## Appendix: code facts and the g-mesh calls that produced them

g-mesh project `g-mesh` (main checkout at release-4.27.0 b9441115, same
commit as the worktree; `diff -rq` of `eval/embedding` showed no
differences). The worktree's own index was not built (cold, minutes) because
the code is identical.

| fact | source |
|---|---|
| Who imports `rerank_eval.py` / `gm462_ce_enriched.py` | `get_dependencies(Incoming, eval/embedding/rerank_eval.py, depth 1)` and same for `gm462_ce_enriched.py`: both **empty**, because the scripts import by bare name after a `sys.path` insert (`import rerank_eval as R`), which g-mesh does not resolve to the file's module `eval.embedding.rerank_eval`. Fell back to grep: importers of `rerank_eval` are `gm441_rerank`, `gm441_summary`, `gm462_ce_enriched`, `gm462_token_lengths`, `gm464_ce_structured`, `gm464_rerank_fixture`, `rerank_q5_diag`; of `gm462_ce_enriched`: `test_gm462_ce_text.py` |
| References to `CrossEncoder` (Python class) | `find_references(CrossEncoder)` -> ambiguous (rerank_eval.py class, rerank.rs struct); `find_references(symbol_id 7643a5bb...)` -> `ce_scores`, `latency_only`, `latency_f4` (all in rerank_eval.py), `provenance: python semanticTier absent`. Cross-module `R.CrossEncoder(...)` is an attribute call through a module alias g-mesh does not link; grep found `gm441_rerank.py:111`, `gm462_ce_enriched.py:568`, `gm464_ce_structured.py:220`, `gm464_rerank_fixture.py:78` |
| Callers of `score_texts` | `find_callers(score_texts)` -> `score_missing`, `c3_rescore` (gm462_ce_enriched.py), complete |
| `R.CES` loops | grep `rerank_eval.py:529` (`latency_only`), `:777` (`main`) |
| Product reranker constraints | read `core/src/embedding/rerank.rs`: sha pin 97-110, `WINDOW` 54, `BETA` 57, `MAX_THREADS` 68, session options 172-181, `token_type_ids` always fed 234-243; `ort = "=2.0.0-rc.9"` `core/Cargo.toml:110` |
| GM-698 S7 invocation and work dir | `.../5a9935bd-.../scratchpad/gm698m/run.sh`, `stdout.txt` (C3 13.20 ms/pair, 0 pairs to score) |
| Query counts | `wc -l eval/embedding/v2/queries/*.jsonl` 1652, `mechanical/*.jsonl` 1779 |

## Owner decisions (2026-11-05)

- Q1: "Только NL, K=30 (Recommended)"
- Q2: "Только для сравнения (Recommended)"
- Q6: "≤ 2.0x MiniLM-L6 (Recommended)"
- Q3, Q4, Q5, Q7: "Да, все рекомендованные (Recommended)"
