# 0044. The rerank model is a selectable variant; the QAT-trained L6 is the default

## Status
Proposed (GM-741; owner decisions D1-D4, D6 of the design note, all as
recommended). Becomes Accepted once the measure slice's go rule passes and
the `qat` pins (Hugging Face commit, fitted beta) replace their placeholders.
Design note: [`GM-741-qat-reranker.md`](../design/GM-741-qat-reranker.md).
Supersedes in part [ADR 0016](0016-cross-encoder-rerank.md) and
[ADR 0043](0043-int8-rerank-one-pair-per-run.md): which weights rerank, and
the blend weight being one constant.

## Context
GM-734 trained `cross-encoder/ms-marco-MiniLM-L6-v2` further on code search
pairs with quantization-aware training. Its quint8 export has the same graph
inputs, output and tokenizer as today's model and 27 KB fewer bytes, but its
logits sit on a much narrower scale, so the blend `logit + beta * cosine`
fitted at beta 40 for today's model does not carry over. A user who sees a
regression needs a way back that does not need a rebuild or a download.

## Decision
- **Variants.** `core/src/embedding/rerank.rs` pins each variant in one
  table entry (`RerankVariant`, `VariantSpec`): name, directory, repo,
  commit, ONNX path, both files' sizes and sha256, and the blend beta.
  `qat` is the QAT-trained model from g-mesh's own Hugging Face repo; `l6`
  is today's model at beta 40, unchanged.
- **Selection.** `G_MESH_RERANK_MODEL`, else `[rerank] model` in the global
  config, else `qat`. A value naming no variant logs one line and gives the
  default.
- **Directories.** Each variant has its own directory under
  `~/.g-mesh/models/` (`g-mesh-reranker-l6-qat`, `ms-marco-MiniLM-L6-v2`), so
  `model fetch` of one never overwrites the other and rollback to `l6` needs
  no download. `G_MESH_RERANK_MODEL_DIR` overrides the selected variant's
  directory. A flat mirror serves `<base>/<directory>/model.onnx`.
- **Fallback.** When the default `qat` is selected by no setting, its
  directory is not overridden and lacks a file, and `l6`'s directory holds
  both files at their pinned sizes, the daemon loads `l6`, logs it once, and
  `g-mesh status` says so. An explicit choice never falls back.
- **Blend weight.** Each variant carries its own beta, fitted on eval v2 FIT
  against its own logits. The similarity floors and the verdict read the
  embedding cosine only and do not change. `G_MESH_RERANK_BETA` overrides the
  loaded variant's beta for measurement.
- **Unpublished pins.** Until the upload, `qat`'s repo and commit are
  `UNPUBLISHED` placeholders and `model fetch` refuses to download it; its
  beta is `UNFITTED_BETA` (NaN), and the daemon refuses to rerank with it
  unless `G_MESH_RERANK_BETA` is set.

## Consequences
- Two pinned models to maintain until `l6` is retired by a later task.
- Every existing user runs `l6` after the upgrade until they run `g-mesh
  model fetch`; the installer says to run it.
- The eval scripts take `--ce l6|qat` (`rerank_eval.SHIPPED_CE`); `l6` is
  the default and reproduces their earlier outputs.
