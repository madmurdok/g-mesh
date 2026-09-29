# 0010. Embedding model: switch to jina-v2-base-code int8

## Status
Accepted (2026-09-29, GM-422/S9). The decision follows mechanically from
the rule the owner fixed before the data existed
([protocol](../architecture/embedding-eval-int8-confirm.md), "Owner
decisions (2026-09-29)": switch if and only if int8 passes). It replaces
GM-398's int8 verdict (fail on Q5). The switch itself is GM-422/S10.

## Context
g-mesh embeds with `jinaai/jina-embeddings-v2-base-code` fp32
(`onnx/model.onnx`, 644 MB). The same repository and revision ship an int8
quantization (`onnx/model_quantized.onnx`). GM-398/S14 measured it on the
g-mesh corpus, 6,774 nodes ([results](../results/gm-398-model-comparison.md),
"Medians and ratios to R"):

| | fp32 | int8 | x fp32 |
|---|---|---|---|
| model size | 644.1 MB | 164.5 MB | 0.26 |
| max RSS during the pass | 1,589 MB | 765 MB | 0.48 |
| embedding pass | 749.4 s | 514.3 s | 0.69 |
| median query embedding | 14.92 ms | 8.03 ms | 0.54 |

That passes D9's cost gate on size and RSS (pass time misses the 0.60x win;
nothing exceeds 1.10x). GM-398 matched fp32 on retrieval but failed Q5
(false alarm +2.1, upper bound +6.2) on about 67 query pairs. GM-422
therefore ran a confirmatory study on 814 new, blind-authored queries,
with the rule, the floors and n fixed first: rule B (each of Q4 and Q5
passes on a one-sided 95% upper bound <= +5 points), Q1-Q3 as in D9, the
frozen GM-398 floors, the new queries only.

## Decision
We will switch the default embedding model to jina-v2-base-code int8.
It passed every gate on the new queries, with all controls passing in the
same run ([verdict](../results/gm-422-int8-confirm.md#verdict-s9)).
Δ is int8 - fp32:

| gate | point | bound | limit |
|---|---|---|---|
| Q1 recall@10 | +0.6 | lower -0.6 | >= -2.0 / >= -5.0 |
| Q2 MRR | +0.005 | lower -0.004 | >= -0.02 / >= -0.05 |
| Q3 recall@10, worst language | 0.0 | - | >= -10 |
| Q4 confident-wrong (n = 814) | +1.5 | upper +2.9 | upper <= +5 |
| Q5 false alarm, pooled (n = 161) | -2.0 | upper +1.2 | upper <= +5 |

Q5 per language (reported, not gated): go +2.1 [upper +8.3], python -8.8,
rust +4.2 [upper +12.5], typescript -5.5. The Go excess was accepted in
advance (owner decision 4).

## Consequences
- **What the switch means (S10).** The model file becomes
  `onnx/model_quantized.onnx` from the same repository and revision.
  Tokenizer, 768 dimensions, mean pooling and the 1,024-token cap do not
  change. It is distributed the way fp32 is today: `g-mesh model fetch`
  and `core/scripts/fetch-embedding-model.sh` with a pinned sha256,
  computed from the downloaded file. `similarity::floor` ships int8's
  fitted floors, go/python/rust/typescript 0.57 / 0.57 / 0.55 / 0.53 (D6:
  a switch ships the fitted floors). `embeddingVersion` is bumped, so
  every stored vector is re-embedded. The embedding cache needs no change,
  because its key already includes the model's sha256. Existing users pay
  one full re-embed, which is cheaper than an fp32 pass (0.69x).
- **Gains.** A 4x smaller download and on-disk model, half the peak RSS
  while embedding, and faster first indexing and query embedding.
- **Risks.**
  - Q4's lower bound is +0.1: at its floors int8 is slightly, measurably
    more often confidently wrong. At fp32's floors it is not (-0.9, upper
    +0.4), so the cause is int8's lower TypeScript and Python floors.
    Rule B accepts it; D9 unchanged (rule A) would have failed it.
  - Go false alarms may be higher (+2.1, upper +8.3). S8's gap analysis
    attributes the Go and TypeScript crossings to the 0.01-0.02 floor gaps,
    not to quantization loss.
  - Rule B passes an exactly +5-worse candidate 6-12% of the time, not 5%.
  - The harness's refit report (not gated) is Undecided: its floor-parity
    check rejects fp32's refit Go and Python floors on these queries, and
    its Q4 fails D9's point condition. That view does not decide here.
  - D9 also names the D10 agent-level check as a veto before a switch is
    proposed. The confirmatory protocol does not mention it, and it has
    not run.
- **fp32 stays the reference** for future candidates. A regression found
  after the switch is reverted by restoring the fp32 pin and floors and
  bumping `embeddingVersion` again.
