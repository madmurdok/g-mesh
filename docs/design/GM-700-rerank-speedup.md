# GM-700: speed up the cross-encoder rerank (design, S2)

Task: GM-700. Baseline: `docs/results/gm-700-baseline-latency.md` (S1, `ea676bca`).
Context: ADR 0016 (rerank), ADR 0042 (enriched text), `docs/results/gm-463-reranker-bakeoff.md`.

## 0. Summary

- **Where the time goes:** the ONNX Runtime forward pass. Tokenization is 1-2% of the rerank; text building is not separately timed but bounded small (section 1.3).
- **Recommended set, in order:**
  1. **int8 MiniLM-L6 with one pair per run (`CHUNK` 16 -> 1).** Use the official dynamic-int8 export from the pinned revision (23.2 MB, sha pinned). In a same-run probe it took 0.34-0.55x of fp32 time.
  2. **Confident-skip.** Simulated offline from cached scores at no CPU cost, and shipped only if it passes the bars.
  3. **K 30 -> 20** only if the owner chooses it (Q3).
- **Rejected for GM-700:** B48 -> B24, TinyBERT-L2, CoreML EP, thread changes.
- **New finding that shapes the design:** dynamic int8 makes a pair's logit depend on the other pairs in its batch (up to 0.86 logit apart). One pair per run removes that dependence at no cost: chunk 1 runs as fast as chunk 8 and faster than chunk 16.
- **gte int8 probe:** its own measure slice (S4). Bar: p50 <= 2.0x the shipped-precision L6 in the same run. A pass is unlikely (section 3.4).

## 1. Current pipeline

### 1.1 What one `search_code` call does when it reranks

From `core/src/mcp/search_code.rs` and `core/src/embedding/rerank.rs`:

1. `handle` (search_code.rs:319-415) embeds the query and picks a scorer. Only a first, complete page reranks (`cursor == None && coverage == None`); every other call takes the plain path.
2. `first_page_reranked` (search_code.rs:491-529):
   - Runs `search` for `max(page_size, WINDOW)` rows. `WINDOW` is 30.
   - With fewer than 2 rows it returns the plain page.
   - Otherwise it builds the texts, scores them, blends and reorders the window. The rows after the window keep the embedding order.
   - If scoring fails, it runs `search` again and returns the plain page.
3. `window_texts` (search_code.rs:535-561):
   - Does one cached-statement SQL row read per window row.
   - Reads each distinct source file once per call (`source_lines` reads the whole file).
   - `ce_text` builds E + P + doc/signature + B48. The body field calls the tokenizer (`count_tokens`) once per body line until 48 tokens.
4. `Reranker::order` -> `CrossEncoder::score` (rerank.rs:198-256):
   - Encodes each (query, text) pair with the HF tokenizer (truncation 512, no padding).
   - Sorts the pairs by token length and runs them in chunks of `CHUNK = 16`.
   - Pads each chunk to its longest pair. The mask is 0 on pads, and `token_type_ids` is always fed.
   - Makes one `session.run` per chunk. At K=30 that is 2 runs.
5. `blended_order` sorts by `logit + 40 * cosine`, with ties broken by window position.

### 1.2 What is already done

These were checked before proposing anything:

- **Session reuse:** `Reranker::outcome` holds the loaded `CrossEncoder` in a `OnceLock`. It is loaded once per daemon, on the first reranking call. That first call costs 1.4-1.7 s, against 0.7 s for `off` (S1).
- **Dynamic padding and length-sorted batching:** already implemented, as described in step 4.
- **Session options:** graph optimisation Level3, `deterministic_compute`, intra-op threads `min(4, physical cores)` (`intra_threads`, rerank.rs:187-192).
- **Threads:** GM-464 measured t1 at 1123 ms and t4 at 510 ms (`gm-464-rerank-latency.md`). The measurement machine has exactly 4 physical cores, so 4 is already the ceiling.
- **Thread spinning:** ORT's default intra-op spinning is left on. It is not a latency lever.

### 1.3 Where time goes

**Product (S1, K=30, pooled p50):**

| arm | p50 ms |
|---|---|
| off | 100.8 |
| enr | 726.0 |

The rerank (texts + tokenize + ORT + blend) is therefore about **625 ms of 726**, 86% of the call.

**Offline (GM-463/S20):** L6 fp32 at K=30, timing tokenize + ORT + blend only, gives a p50 of 645.6 ms. That is the same order as the product's 625 ms. The product's extra work (SQL rows, file reads, per-line token counts) is therefore within the noise of these two runs, and the forward pass dominates.

**Offline probe in this slice** (`/private/tmp/.../scratchpad/probe.py` and `probe2.py`, not committed):

- **Setup:** Python ORT 1.19.2, L6 at the pinned revision, Level3 + deterministic, 4 threads.
- **Inputs:** 30 synthetic ship-like texts (enclosing words, path words, 5 code lines from `core/src/**/*.rs`), mean pair tokens 89.2 (min 38, max 155); 10 NL queries; 1 warm-up pass + 3 timed passes.
- **Machine state:**
  - Probe 1: start `20:29 up 18 days, 8:31, load 3.07 4.26 5.51`, CPU_Speed_Limit 100 -> 56 at the end. `time -p` real 153.89, user 484.55, sys 5.18.
  - Probe 2: start `20:34 ... load 3.47 4.49 5.36`, CPU_Speed_Limit 100 -> 51. `time -p` real 160.42, user 581.27, sys 6.99.
  - user/real of 3.1-3.6 means the process was computing, not waiting.
  - The machine throttled during both runs: compare only arms next to each other, never absolute ms.

**Probe 1, time split** (p50 per 30-pair call, ms):

| arm (threads, chunk, K) | tokenize | ORT run | total p50 | total p95 |
|---|---|---|---|---|
| fp32 4t c16 K30 (first) | 5.1 | 377.1 | 383.0 | 634.8 |
| qint8 4t c16 K30 | 7.5 | 251.7 | 258.0 | 294.0 |
| quint8_avx2 4t c16 K30 | 7.0 | 251.9 | 258.9 | 283.2 |
| fp32 4t c32 K30 | 6.8 | 569.5 | 577.1 | 650.1 |
| fp32 2t c16 K30 | 9.2 | 837.0 | 847.1 | 1250.4 |
| qint8 2t c16 K30 | 5.9 | 316.5 | 323.4 | 399.4 |
| fp32 4t c16 K20 | 6.2 | 320.0 | 328.4 | 420.6 |
| qint8 4t c16 K20 | 6.5 | 191.3 | 198.5 | 242.6 |
| fp32 4t c16 K30 (last) | 8.2 | 495.6 | 503.5 | 640.2 |

**Probe 2, chunk size and batch dependence** (p50/p95 ms, run order top to bottom):

| arm | p50 | p95 |
|---|---|---|
| fp32 c16 | 446.2 | 589.5 |
| qint8 c16 | 244.8 | 266.2 |
| qint8 c8 | 195.1 | 215.8 |
| qint8 c1 | 200.3 | 221.2 |
| fp32 c8 | 661.4 | 2066.5 (throttle spike) |
| quint8_avx2 c8 | 228.8 | 279.7 |
| qint8 c16 | 311.8 | 414.8 |
| fp32 c16 | 589.2 | 753.6 |

Batch dependence is the largest |logit(pair in a 16-chunk) - logit(pair alone)| over 10 queries x 30 pairs:

| model | max \|chunk16 - alone\| | same batch twice gives equal logits |
|---|---|---|
| fp32 | 0.0 | yes |
| qint8 | 0.859 | yes |
| quint8_avx2 | 0.800 | yes |

Agreement with fp32 on logit-only order, chunk 16:

| model | max \|dlogit\| | median per-query max | top-1 same | top-10 overlap |
|---|---|---|---|---|
| qint8 | 0.673 | 0.243 | 8/10 | 9.2 |
| quint8_avx2 | 0.679 | 0.074 | 8/10 | 9.4 |

**Readings:**

1. **Tokenization is 5-9 ms of 250-850 ms**, which is 1-2%. The forward pass is everything.
2. **int8 vs fp32 in adjacent arms:** 258/383 = 0.67 and 245/446 = 0.55 at chunk 16; 200/446 = 0.45 and 200/589 = 0.34 at chunk 1. Two caveats: the probe used synthetic texts and the machine throttled. S3 measures on the real latency set.
3. **Smaller chunks are faster:** chunk 32 (one padded batch) was slower than chunk 16, and int8 chunk 1 matches chunk 8. Padding waste outweighs per-run overhead at these lengths.
4. **K=20 vs K=30** costs 0.86x (fp32, adjacent arms) in the probe and 0.56x in GM-463/S20 offline (364.5 / 645.6).
5. **Dynamic int8 is batch-dependent.** `DynamicQuantizeLinear` takes one activation scale per tensor, i.e. over the whole chunk, pad positions included. A pair's logit then depends on its window neighbours and on padding. Two consequences:
   - The existing invariant "scoring is independent of the batch" (test `scoring_is_deterministic_and_independent_of_the_batch`) would break at chunk 16.
   - The eval harness's per-pair score cache would be wrong for int8 at chunk 16.

   **Chunk 1 removes the dependence at no speed cost**, so int8 ships with `CHUNK = 1`.

## 2. Candidates

"Gain" is the share of the ~625 ms product rerank cost removed. Quality bars throughout: held-out NL r@10 >= 0.647, MRR >= 0.425, D9 vs int8-structured passes, and name-query MRR not below off.

### 2.1 Dynamic int8 L6, `CHUNK = 1` (recommend: yes, first)

- **Expected gain:** 0.34-0.55x of fp32 in the probe; take about 0.5x as the planning figure. Product p50 of roughly 100 + 0.5 x 625 ≈ 410 ms against 726 today.
- **No self-quantization needed:** the pinned revision `233902d2` already carries official dynamic-int8 exports (HF tree API, read in this slice):

  | file | size | sha256 |
  |---|---|---|
  | `onnx/model_qint8_arm64.onnx`, `onnx/model_qint8_avx512.onnx`, `onnx/model_qint8_avx512_vnni.onnx` (byte-identical) | 23,200,716 | `3573b6b9593cb2f75987a31815d409ca3dd8808629118fd20451bb1a5d90cec7` |
  | `onnx/model_quint8_avx2.onnx` | 23,200,716 | `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9` |

  Both were downloaded and the sha256 recomputed locally; both match. The I/O is unchanged (`input_ids`, `attention_mask`, `token_type_ids` int64 -> `logits` float), and the tokenizer is the same file.
- **Quality risk: real.** The probe moved logits by up to 0.67 and changed top-1 on 2 of 10 logit-only orders. The blend adds `40 * cosine`, which damps this, but only a re-score of the v2 eval set can say whether the bars hold. BETA stays 40 unless the bars fail at 40 (Q2).
- **Product cost:**
  - The download shrinks from 91 MB to 23 MB.
  - One new sha/size pin.
  - The fp32 `model.onnx` on existing installs is refused until `g-mesh model fetch` runs again (Q1).
  - **Platform risk:** the s8 (`qint8`) file is the one the repo labels for arm64 and AVX-512. On AVX2-only x86 (older Intel, Zen 2), ORT's s8s8 kernels may be slower than u8s8. This Ice Lake machine cannot measure it. Q4 decides whether to ship one file.
- **How measured:** S3 runs a full quality re-score (sections 4.1, 4.2) plus offline latency arms.

### 2.2 Confident-skip (recommend: measure in S3, ship if it passes)

- **What it does:** skip the cross-encoder when the embedding ranking is already confident. Two rules, each tuned or checked on FIT and reported on held-out:
  - **(a) Exact-name rule.** The query's `split_words` equals the `split_words` of the top-1 row's last qualified-name segment. Example: the query `blended_order` and the top-1 row `embedding::rerank::blended_order`.
  - **(b) Margin rule.** cos(top-1) - cos(top-2) >= m. FIT picks the largest skip share whose FIT MRR loss is <= 0.005, from a grid of m.
- **Expected gain:** the skipped share times the rerank cost on mean latency. p50 moves only if more than 50% of calls skip (GM-463). p95 belongs to the non-skipped calls.
- **Quality risk:**
  - Rule (a) on a name query can only fall back to the no-rerank order, which is int8-structured.
  - The gates have two different "off" references:
    - **The rerank switched off (int8-structured):** name MRR 0.768. Skipping can never put name MRR below this one.
    - **"off" in the GM-463 sense, i.e. L6 on the structured text at beta 80:** name MRR 0.878 (K=20 run, `s15-quality.md`). This is a stricter floor. Today's ship-L6 sits at 0.889.

    Which one the AC means is Q5.
  - Rule (a) also removes the rerank's name-query gain where the CE would have reordered an exact-name top-1 (GM-463 flips: L6 breaks 5 correct name tops and fixes 123).
  - Rule (b)'s risk is measured on held-out NL.
- **Product cost:** no model. One pure function with a constant. The verdict path is unchanged: a skipped page is the plain page, exactly as with the rerank off (ADR 0016).
- **How measured:** zero-CPU simulation over S3's cached int8 scores (section 4.3).

### 2.3 K 30 -> 20 (recommend: owner choice, Q3)

- **Expected gain:** 0.56x (GM-463 offline) to 0.86x (probe) of the rerank cost.
- **Quality, already measured on fp32** (`s15-quality.md`), ship-L6 at beta 40:

  | K | r@10 | MRR | name r@10 / MRR |
  |---|---|---|---|
  | 30 | 0.670 | 0.465 | 0.971 / 0.889 |
  | 20 | 0.659 | 0.459 | 0.965 / 0.886 |

  K=20 passes the bars with a cost of 0.011 r@10 and 0.006 MRR.
- **Window ceiling:** 0.758 at K=30 vs 0.715 at K=20. A future stronger reranker (GM-436 distilled gte) would lose headroom.
- **Product cost:**
  - Change the `WINDOW` const.
  - The parity fixture's `k`.
  - `expected_walk` in `search_code_rerank_tests.rs:454-458`, which hard-codes 30.
- **How measured:** S3 reports K=20 from the same int8 scores. Chunk 1 makes a pair's score independent of K.

### 2.4 B48 -> B24 (recommend: no)

- **Expected gain:** fewer tokens per pair (mean ~102 -> roughly 80), so about 20% of the rerank cost.
- **Why not:** never measured on quality. GM-698 added the body field because it carries the gain over base. int8 + skip give more speed for less quality risk.
- **Status:** kept as a fallback if S3's set misses a latency target the owner sets (Q3).

### 2.5 A lighter cross-encoder: TinyBERT-L2 or MiniLM-L2/L4 (recommend: no, defer to GM-436)

- **Why not:** not in the harness, and quality on v2 is unknown. Off-the-shelf small MS MARCO cross-encoders score clearly below L6 on MS MARCO, so the MRR bar is at risk.
- **Where it belongs:** GM-436, which distils gte into an L6-size (or smaller) cross-encoder. That is the planned route to a lighter model and is owner-approved.

### 2.6 CoreML EP on macOS (recommend: no)

- **No ANE on the measurement machine:** it is Intel (i7-1068NG7), so CoreML falls back to GPU/CPU there.
- **Prebuilt binaries:** whether the prebuilt ORT behind `ort =2.0.0-rc.9` ships the CoreML EP is unverified.
- **Dynamic shapes:** CoreML handles a varying width per chunk poorly.
- **Coverage:** a macOS-only code path in a 4-platform CI.
- **Gain:** platform-specific, and unmeasurable here.

### 2.7 Other cheap levers checked

- **Threads:** at the ceiling (section 1.2).
- **Session reuse:** done.
- **Dynamic padding:** done; chunk 1 makes it exact.
- **Prewarm at daemon start:** load the CE in the background once the index is ready. This would move the 1.5 s first-call cost off the first search, but it does not touch p50/p95 and is out of GM-700 scope. It is noted for a follow-up task if the owner wants one.

### 2.8 Recommended order

1. int8 + chunk 1.
2. Skip, if it passes.
3. K=20 only by Q3.

Each is independent in code. All three are measured offline in one slice (S3) before any product code, then implemented together (S5) and measured in the product once (S9).

## 3. gte int8 probe (S4, its own measure slice)

### 3.1 Harness change (S4 does it; eval only)

- **`eval/embedding/rerank_eval.py`:**
  - Add entries to `CE_BAKEOFF` / `CE_DIR` / `CE_FILES` (lines 69-96) for three variants, each with its own local dir so caches keyed by model sha never collide:
    - `ce-gte-mb-int8`: `onnx/model_int8.onnx`, 150,871,837 bytes, sha256 `ecc6a0ae67cee3d898167802383112d9185ca9250e07bd5d1fa65019b050179d`, same revision `f7481e60`, same tokenizer.
    - `ce-minilm-int8`: `onnx/model_qint8_avx512_vnni.onnx` (sha in section 2.1).
    - `ce-minilm-quint8`: `onnx/model_quint8_avx2.onnx`.
  - `CrossEncoder.score` already takes `chunk`. Add a per-model default chunk: 1 for int8 variants, 16 otherwise.
- **`eval/embedding/gm463_bakeoff.py`:**
  - Add the new names to `MODELS` / `LAT_ORDER` / `C8_SPEC`.
  - C7 for int8 variants is rank-only: the card logits' 0.05 tolerance is fp32's.
  - C3' tolerance stays 1e-5 at chunk 1; batch-independence must then be exact.
  - Add `--chunk` to `latency-arm` / `latency`.

  These are the "int8 switch" the task asks for.

### 3.2 Steps

1. **Dry run:** `quality --controls-only --models ce-gte-mb-int8`. C1, C5, C7 (rank), C8 and C3' must pass. Then `--dry-run-pairs 50` for ms/pair and the ETA.
2. **Latency, K=30 only:**
   - Arms in mirrored order: `ce-minilm-<ship> ce-minilm(fp32) ce-gte-mb-int8 ce-gte-mb-int8 ce-minilm(fp32) ce-minilm-<ship>`.
   - Each arm runs in a fresh process behind the load gate and CPU_Speed_Limit == 100, under `caffeinate -i -s`, with 30 queries, 1 warm-up pass and 3 timed passes.
   - `<ship>` is the precision S3 picked, normally `int8` at chunk 1.
   - Record uptime, `time -p` and CPU_Speed_Limit at both ends of each arm.
3. **Bar (pre-registered):**
   - Pooled p50(gte-int8) <= 2.0 x pooled p50(L6 at ship precision), same run.
   - Also reported, not gating: the ratio to fp32 L6 (the GM-463 9.50x baseline) and gte-int8 / gte-fp32 from GM-463's 6131.7 ms (machine-state caveat).
4. **Pass:**
   - Re-score gte-int8 quality on held-out NL and name queries at K=30 with FIT-tuned beta (GM-463 rules 1-4) vs ship-L6 at ship precision.
   - Run it as one background script under caffeinate. At roughly 100 ms/pair x ~100k pairs this takes several hours; step 1's ETA decides.
   - Verdict and the owner's decision go in `docs/results/gm-700-gte-int8-probe.md`. Shipping gte would be a new task (151 MB download, beta re-fit), not GM-700.
5. **Fail:**
   - No quality re-score.
   - Verdict in the same results doc.
   - GM-436 (distil gte) proceeds as the owner already decided.

### 3.3 Control

A latency control tells gte-int8 from L6 apart:

- Per-arm `files` sha in `arms.jsonl` differs.
- `ms_per_pair` differs by more than the arm-to-arm spread of the two L6 arms.
- Mean pair tokens on the latency set are within 10% (GM-463: gte 104.4 vs L6 101.9), so the ratio is model cost, not input length.

### 3.4 Expected outcome (stated so a null is meaningful)

- gte fp32 sits at 9.50x fp32 L6.
- If int8 speeds both models by the same factor, gte-int8 stays near 9.5x int8-L6 and fails by ~4.7x.
- To pass, int8 would have to speed up gte about 4.75x more than it speeds up L6.
- Likely verdict: fail. The probe still settles the owner's question cheaply (two gte arms of ~5-10 min each).

## 4. Quality gates and A/B designs

Bars for every shipped change, on held-out NL (v2, `R.held_out`) and name queries with bootstrap CIs as GM-463:

- r@10 >= 0.647.
- MRR >= 0.425.
- D9 vs int8-structured at the shipped floors passes.
- Name-query MRR not below the "off" reference fixed by Q5.
- Plus the non-inferiority margin vs fp32 ship-L6 that Q2 sets.

### 4.1 int8 quality (S3)

- **Arms:**
  - `ce-minilm` (fp32, chunk 16): today, read from the GM-698/GM-463 caches.
  - `ce-minilm-int8` (qint8, chunk 1).
  - `ce-minilm-quint8` (chunk 1), a reference for Q4.
  - Each at beta 40. Also report the FIT-tuned beta, which only decides anything under Q2.
- **Output:** the bars, paired dr@10 / dMRR vs fp32, and flips.
- **Cost:** roughly 100k pairs x ~7 ms ≈ 12-15 min per int8 model, run in the background.
- **Control (tells int8 from fp32 apart):** the number of queries whose reranked top-10 differs between fp32 and int8 must be > 0. The probe already shows logit deltas up to 0.67. Zero would mean the int8 file was not loaded.
- **Batching control:**
  - C3' at chunk 1 gives exact agreement between a pair scored alone and the same pair in its window.
  - A one-off chunk 16 run of 200 pairs must disagree (> 1e-3).

  This is what makes the per-pair cache valid.

### 4.2 int8 latency, offline (S3)

- **Arms:** `latency` mode, arms `ce-minilm(fp32, c16) ce-minilm-int8(c1) ce-minilm-quint8(c1)` mirrored, at K 20 and 30, under caffeinate with the gates.
- **Report:** pooled p50/p95, ratio vs fp32.
- **Control:** the files sha per arm, and ms/pair differing by more than the L6 arm-pair spread.

### 4.3 Confident-skip (S3, simulation)

- **Method:** from the cached scores, rebuild each query's page with "skip -> int8-structured order, else blend". Sweep m on FIT, fix it, report on held-out:
  - skip share (NL, name);
  - r@10 / MRR / D9 / name MRR;
  - mean simulated latency = (1 - skip share) x rerank cost + plain cost.
- **Control:**
  - At m = +inf with the name rule off, the output must equal the no-skip arm on every cell.
  - With every query skipped, it must equal int8-structured on every cell (C5's twin).
  - Both checks are required. Without them, a simulation bug reads as "skip is free".

### 4.4 K=20 (S3, same scores)

- **Report:** K=20 vs K=30 rows from the int8 scores.
- **Control:** the number of held-out queries whose top-10 differs between K=20 and K=30 must be > 0.

### 4.5 Product before/after (S9, the AC "before and after")

- **Run:** `eval/embedding/gm464_latency.py` with paired arms in one run:
  - `base` = release-4.27.0 build at the branch point (`9bb789cd`, fp32, K30);
  - `new` = branch tip;
  - `off`.
  - Order: `base new new base base new off` under caffeinate, load gate, CPU_Speed_Limit recorded, sleep check.
- **Model dirs:** a model dir per arm, via `G_MESH_RERANK_MODEL_DIR`: fp32 for base, int8 for new.
- **Controls:**
  - **Rerank-on share** (pages not in descending cosine order): base 90/90, new 90/90 minus the latency set's skipped queries (expected count from S3's rule applied to the 30 queries), off 0/90.
  - **Model loaded:** the daemon's outcome record / log shows `Loaded` for new with the int8 dir. The new binary handed the fp32 dir must log the refusal (one cheap arm-identity check, not timed).
- **Quality:** product quality is not re-measured. Parity of the product with the offline int8 scores is pinned by the regenerated fixture test (section 5.3).

## 5. Edit map

g-mesh calls (project `g-mesh`, main checkout; the four files below are byte-identical on the branch, checked with `cmp`):

- `find_references RERANK_ONNX_REMOTE_PATH` -> `cli/model.rs` `RERANK_FILES` only.
- `find_references RERANK_ONNX_SHA256` -> `check_rerank_files` and `RERANK_FILES`.
- `find_callers first_page_reranked` -> `handle` only.
- `find_references WINDOW` -> `first_page_reranked` only. The tests hard-code `30` (grep found them; see section 5.3).

### 5.1 Change (S5)

| file:lines | function / item | change |
|---|---|---|
| `core/src/embedding/rerank.rs:32-44` | `RERANK_ONNX_REMOTE_PATH`, `_SIZE`, `_SHA256` | the int8 file (section 2.1); doc comments say int8 |
| `core/src/embedding/rerank.rs:63-65` | `CHUNK` | 1; doc states the invariant "a pair's logit does not depend on the other pairs" (dynamic int8 scales per batch) |
| `core/src/embedding/rerank.rs:198-256` | `CrossEncoder::score` | with CHUNK 1 the length sort and padding become no-ops; keep the loop (simplest) or drop padding; the doc comment changes |
| `core/src/embedding/rerank.rs:97-110` | `check_rerank_files` | message for a wrong-sha `model.onnx` names the likely cause (old fp32 file, `g-mesh model fetch`), as `model.rs::check_default_weights` (`core/src/embedding/model.rs:142-157`) does; Q1 may make this a fallback instead |
| `core/src/embedding/rerank.rs:54` | `WINDOW` | 20 only if Q3 says so |
| `core/src/embedding/rerank.rs` (new, near `blended_order` :129) | `confident(query, top_qualified_name, cosines) -> bool` + `SKIP_MARGIN` const | only if S3's skip passes; pure, uses `ce_text::split_words` |
| `core/src/mcp/search_code.rs:491-529` | `first_page_reranked` | after the window `search`: if `rerank::confident(..)`, return the plain page (same path as `reranked < 2`) before `window_texts` |
| `core/src/cli/model.rs:216-235` | `RERANK_FILES` | picks up the new consts; the doc comment says int8 |
| `docs/adr/` | new ADR 0043 "the rerank runs the int8 cross-encoder one pair per run" (+ skip rule if shipped); link from `rerank.rs` module doc | decision text lives there, not in comments |
| `eval/embedding/gm464_rerank_fixture.py` | fixture generator | the int8 model, chunk 1 (and K if Q3); regenerate `core/src/embedding/testdata/rerank_parity.json` |

### 5.2 Read for context (no change expected)

- `Reranker::outcome` (rerank.rs:479-513), for the log line and the Absent/Failed split on an old fp32 file.
- `RerankModelState::of` (rerank.rs:313-330), which judges by `RERANK_ONNX_SIZE` and changes with the const.
- `cli/model.rs` `on_disk` (471-477), `fetch_rerank` (409-421), `report_files` (545): size-based, so the old 91 MB file reads as `Other` and gets replaced.
- `core/tests/it/cli_model_fetch.rs:20-35`: `pinned_size` reads `RERANK_ONNX_SIZE` from the source text by name, so the const name must stay.
- `core/src/embedding/ce_text.rs::split_words` (86).
- ADR 0016, ADR 0042.

### 5.3 Tests to pin behaviours (S7), each with one control

1. **The int8 weights load, and the old fp32 file is refused with a message naming `g-mesh model fetch`.** Unit test in `rerank.rs` tests (next to `other_weights_are_refused_by_sha`, :699). Control: restore the fp32 sha const, and the test fails.
2. **On-disk state by the new pinned size.** Update `the_on_disk_state_is_judged_by_the_switch_and_the_pinned_sizes` (:882-903), adding an fp32-size file -> `Missing`. Control: compare against the old size, and it fails.
3. **Batch independence at int8.** `scoring_is_deterministic_and_independent_of_the_batch` (:960, `#[ignore]`, real model) is tightened to exact equality. Control: `CHUNK = 16` makes it fail; the probe shows deltas of 0.8.
   - The verify slice must fetch the int8 model and run `cargo test rerank -- --ignored --test-threads=1`.
4. **Parity with the Python reference on int8.** `the_real_model_reproduces_the_reference_logits_and_order` (:925) with the regenerated fixture. The tolerance starts at 1e-4; if Rust's ORT (the version `ort-sys =2.0.0-rc.9` fetches; read it in S5) differs from Python ORT 1.19.2 on int8 kernels, widen it to the measured value and record why in the test doc. Control: point at the fp32 fixture logits, and it fails.
5. **Skip, if shipped.** In `search_code_rerank_tests.rs`, a confident window gives exactly the plain page, and the counting stub scorer is never called; a window below the margin is reranked; an exact-name top-1 skips. Control: `confident` always `false`, and the first test fails.
6. **K, if changed.** `expected_walk` (`search_code_rerank_tests.rs:454-458`) uses `rerank::WINDOW` instead of `30`; `the_fixture_was_made_with_this_window_and_blend` (:625) checks the new k. Control: `WINDOW` back to 30, and the fixture test fails.
7. **Migration:** a daemon with only the old fp32 `model.onnx` reports `Failed` once and keeps the embedding order. This extends `a_missing_model_keeps_todays_output_and_logs_once` (`search_code_rerank_tests.rs:345`) with a fp32-size file, or reuses it. Control: accept any sha, and it fails.

That is 6-7 controls, within the 6-8 rule.

## 6. Proposed re-slicing (S3..S9)

Today S3..S7 are: code, review, tests, verify, measure. The offline measurement has to precede code, because it picks the int8 file, the skip margin and K. The gte probe is its own measure slice. Proposal (one kind of work each):

| slice | phase | model | work | AC owned |
|---|---|---|---|---|
| S3 | measure | sonnet | harness int8/chunk switches (eval only); L6 int8/quint8 quality re-score, offline latency arms, skip simulation, K=20 rows (sections 4.1-4.4); results `docs/results/gm-700-offline.md`; picks the int8 file, m and K by the pre-registered rules + Q2-Q5 answers | AC2 (quality bars of the chosen set) |
| S4 | measure | sonnet | gte int8 probe (section 3); results `docs/results/gm-700-gte-int8-probe.md` + owner decision | AC4 |
| S5 | implement | opus | product code per section 5.1, ADR 0043, fixture regeneration | - |
| S6 | verify | opus | review of the S5 diff | - |
| S7 | implement | sonnet | tests and controls per section 5.3 | - |
| S8 | verify | opus | fresh agent: changed sections (`scripts/test-local.sh --base release-4.27.0`), the ignored real-model tests with the int8 model, controls in a throwaway worktree | - |
| S9 | measure | sonnet | product before/after (section 4.5) + collects every A/B's control into the results doc | AC1 (before/after median at K=30, quiet machine), AC3 (each A/B has a control; S3, S4 and S9 each record theirs, S9's doc lists them all) |

- **Order:** S3 -> S4 (S4's bar needs S3's ship precision; both are CPU-heavy, so they run serially) -> S5 -> S6 -> S7 -> S8 -> S9.
- **If S3 finds that no int8 file passes:** stop and bring the numbers to the owner before S5. Skip and K alone would then be the set.

## 7. Open questions for the owner

### Q1. How do installs with today's fp32 file move to int8?

- **Today:** `~/.g-mesh/models/ms-marco-MiniLM-L6-v2/model.onnx` is the 91 MB fp32 file, and the daemon accepts only its sha.
- **Change:** the pinned file becomes the 23 MB int8 export.
- **Example:** a user upgrades g-mesh without running `g-mesh model fetch`. Their first `search_code` finds a wrong-sha `model.onnx`.
- **Consequence:** depends on the option.

Options:

- **A. Same local name, replace on fetch, refuse the old file until then (Recommended).** This is the embedding model's own int8 migration (ADR 0011).
  - Benefit: one code path, no orphan 91 MB file, the existing hint already says "run g-mesh model fetch".
  - Risk: the rerank is off for that user until they fetch (one hint, embedding order meanwhile).
- **B. Accept both files: int8 if present, else the pinned fp32.**
  - Benefit: no quality drop for anyone between upgrade and fetch.
  - Risk: two pinned models, two parity fixtures and a doubled test matrix. The fp32 path then also needs `CHUNK 16` to stay fast, so `CHUNK` becomes per-model.
- **C. New local name `model_int8.onnx` beside the old one.**
  - Benefit: nothing is overwritten.
  - Risk: the 91 MB fp32 file stays on disk forever unless the user deletes it.

### Q2. How much quality may int8 lose against today's fp32 L6?

- **Today:** ship-L6 has held-out r@10 0.670 and MRR 0.465. The AC bars are 0.647 / 0.425, so int8 could lose 0.04 MRR and still "pass".
- **Change:** int8 moves logits by up to ~0.7 (probe).
- **Example:** int8 at 0.660 / 0.445 passes the AC bars but gives back 40% of the rerank's MRR gain over no rerank (0.384).
- **Consequence:** this sets S3's ship rule.

Options:

- **A. AC bars plus non-inferiority vs fp32: dMRR CI lower bound >= -0.015 and dr@10 lower bound >= -1.5 pt; BETA stays 40 (Recommended).**
  - Benefit: keeps nearly all of GM-698's gain and pins a number before the run.
  - Risk: the CI is ±0.015 wide, so a truly equal model can miss it by chance. If it misses, S3 reports the FIT-tuned beta and asks.
- **B. AC bars only.**
  - Benefit: the simplest; the fastest model ships.
  - Risk: can ship a visible MRR loss.
- **C. No measurable loss (dMRR CI includes 0).**
  - Benefit: strictest.
  - Risk: likely blocks int8 entirely.

### Q3. Change K from 30 to 20?

- **Today:** 30 rows are reranked.
- **Change:** rerank 20.
- **Example:** fp32 L6 at K=20 gave r@10 0.659 / MRR 0.459 (vs 0.670 / 0.465 at K=30), both above the bars. Rerank time is about 0.56-0.86x.
- **Consequence:** a positive at rank 21-30 can no longer be lifted into the top 10. The window ceiling drops from 0.758 to 0.715, which also caps GM-436's distilled model.

Options:

- **A. Keep K=30 and take the speed from int8 (+ skip) (Recommended).**
  - Benefit: no recall headroom lost; int8 alone is expected to halve the rerank cost.
  - Risk: p50 stays around 0.4 s if int8 gains less on the real set than in the probe.
- **B. K=20 together with int8.**
  - Benefit: the largest speed-up, about 0.25-0.4x of today's rerank cost.
  - Risk: -0.011 r@10 measured (fp32), and less headroom for GM-436.
- **C. Decide after S3, against a p50 target the owner names now (e.g. product p50 <= 400 ms).**
  - Benefit: a pre-registered numeric rule.
  - Risk: needs a target number today.

### Q4. One int8 file for every platform, or per CPU?

- **Today:** one fp32 file everywhere.
- **Change:** the repo offers a signed-int8 file (labelled arm64 / AVX-512 / VNNI, one sha) and an unsigned AVX2 file.
- **Example:** a Linux x86 box without AVX-512 VNNI (older Intel, AMD Zen 2) may run the signed file slower than the AVX2 file. This Ice Lake Mac cannot show it.
- **Consequence:** sets the number of pins and download branches.

Options:

- **A. One file: the signed qint8, unless S3 shows quint8 better on quality (Recommended).**
  - Benefit: one sha, one fixture, one code path.
  - Risk: possibly slower on AVX2-only x86; unmeasured.
- **B. Pick per CPU at fetch time (qint8 on arm64 / AVX-512, quint8 on AVX2).**
  - Benefit: best kernel per machine.
  - Risk: two pins, two fixtures, two quality runs, and CPU detection in `model fetch`.
- **C. Ship quint8_avx2 everywhere.**
  - Benefit: the probe's median logit delta to fp32 was smaller (0.074 vs 0.243).
  - Risk: u8 activations on arm64 kernels are less tuned (unmeasured).

### Q5. Which "off" is the name-query floor in the AC?

- **Today:** the AC says "name-query MRR not below off". GM-463 uses "off" for L6 on the structured text at beta 80 (name MRR 0.878 at K=20); the product's rerank switched off is int8-structured (0.768).
- **Change:** the skip rule sends some name queries back to the no-rerank order.
- **Example:** with an exact-name skip, name MRR could land at 0.87: above int8-structured, below GM-463's off.
- **Consequence:** decides whether the name skip can ship.

Options:

- **A. GM-463's off (L6, structured text, beta 80), the stricter floor (Recommended).**
  - Benefit: matches how GM-698/GM-463 read the AC; no silent name loss.
  - Risk: may reject the exact-name skip, leaving only the margin rule.
- **B. The product's rerank-off (int8-structured).**
  - Benefit: the name skip almost surely passes.
  - Risk: allows a name-MRR drop of up to ~0.1 vs today.

## 8. Owner decisions (2026-11-08)

Quoted verbatim from the owner's answers:

- **Q1:** "То же имя, заменить при fetch (Recommended)". This is option A: the same local name, replaced on fetch, and the old fp32 file refused until `g-mesh model fetch` runs.
- **Q2:** "Пороги + не хуже fp32 (Recommended)". This is option A, the S3 ship rule: the AC bars, plus a dMRR CI lower bound >= -0.015 and a dr@10 lower bound >= -1.5 pt against fp32 L6, with BETA 40. If the rule fails at BETA 40, S3 reports the FIT-tuned beta and asks the owner.
- **Q3:** "K=30, скорость за счёт int8 (Recommended)". K stays 30. The K=20 rows are reported for information only.
- **Q4:** "Один файл, qint8 (Recommended)". One file, the signed qint8 export, unless S3 shows quint8 is better on quality.
- **Q5:** "off из GM-463, строже (Recommended)". The name-query floor is GM-463's off: L6 on the structured text at beta 80. S3 measures it at K=30.
