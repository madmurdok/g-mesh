# 0043. The rerank runs the int8 cross-encoder one pair per run, and skips an exact-name window

## Status
Accepted (GM-700, owner decisions: the same local name `model.onnx`,
replaced on `g-mesh model fetch`; quality no worse than fp32 at BETA 40; K
stays 30; one file, the unsigned quint8 AVX2 export; the name-query floor is
GM-463's "off").
Design note: [`GM-700-rerank-speedup.md`](../design/GM-700-rerank-speedup.md).
Results: [`gm-700-offline.md`](../results/gm-700-offline.md),
[`gm-700-int8-parity.md`](../results/gm-700-int8-parity.md),
[`gm-700-rust-quality.md`](../results/gm-700-rust-quality.md).
Amends [ADR 0016](0016-cross-encoder-rerank.md) (the weights, the batching)
and keeps [ADR 0042](0042-enriched-reranker-text.md)'s text and blend weight.

## Context
The rerank (`embedding::rerank`) scored the 30-row window with the fp32
export of `cross-encoder/ms-marco-MiniLM-L6-v2` in length-sorted chunks of
16 pairs; it was most of a reranked `search_code` call's time. The pinned
revision `233902d2` also carries official dynamic-int8 exports with the same
inputs, output and tokenizer. Dynamic quantization scales each run's
activations by that run's own range, so a pair's logit moves with the other
pairs in its batch (probe deltas up to 0.8 at 16 pairs per run).

## Decision
- **Weights.** `onnx/model_quint8_avx2.onnx` (unsigned-activation, u8
  weights), 23,200,716 bytes, sha256
  `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9`, stored
  under the same local name `model.onnx`. The signed qint8 files (arm64,
  avx512, avx512_vnni) are not used: on the AVX2 U8S8 kernel ONNX Runtime
  picks for them, int16 intermediate sums saturate and the logits drift.
  The u8 weights of the quint8 export do not saturate there. On macOS the
  AVX512 state is enabled lazily, so a fresh process selects the AVX2 U8S8
  kernel even on a CPU that has AVX512; the file must therefore be correct
  on that kernel, not only on the one its name suggests. See
  [`gm-700-int8-parity.md`](../results/gm-700-int8-parity.md).
  `g-mesh model fetch` replaces an older file (its sha256 differs); until
  then the daemon refuses it by sha256 with a message naming
  `g-mesh model fetch`, and `search_code` keeps the embedding order.
- **One pair per run** (`CHUNK = 1`), so a pair's logit does not depend on
  the other pairs. The length sort and padding stay in the code and do
  nothing at one pair.
- **BETA 40, WINDOW 30**, unchanged.
- **Exact-name skip.** When the words (`ce_text::split_words`) of the query
  equal the words of the embedding ranking's first row's last
  qualified-name segment (segments split on `::`, `.`, `/`, `#`, `\`), and
  are not empty, the window is not scored: the page is the plain page,
  exactly as with the rerank off. The rule is the eval's simulation
  (`eval/embedding/gm463_bakeoff.py::skip_flags`, rule (a)) byte for byte.
- **No margin skip.** A cosine-margin rule (top-1 minus top-2 >= m) is not
  used.

## Consequences
- On the v2 eval set the shipped quint8 file, scored by this crate's
  scorer, matches fp32 within the non-inferiority bounds (held-out NL dMRR
  -0.002 [-0.005, +0.001], dr@10 +0.5 pt [-0.0, +1.0]; name MRR 0.891), and
  int8 at one pair per run takes about 0.26x of fp32's rerank p50 at K=30
  offline. The download drops from 91 MB to 23 MB.
- The exact-name skip changes no held-out NL cell and name MRR by +0.001;
  it skips 608 of 1779 name queries and none of the NL ones in that set.
- The margin rule at its FIT-picked m = 0.08 lost held-out NL MRR (CI
  [-0.012, -0.003]) and took name MRR below GM-463's off floor, so it is
  rejected.
- The order-parity fixture (`core/src/embedding/testdata/rerank_parity.json`)
  holds the quint8 model's logits at one pair per run.
