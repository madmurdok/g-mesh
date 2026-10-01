# 0012. Embedded text: the doc comment trimmed to its prose outline

## Status
Accepted (2026-10-01)

## Context

Every symbol is embedded as its doc comment, a blank line, then its
signature, and the tokenizer truncates the result at
`DEFAULT_MAX_SEQUENCE_LENGTH` (1024 tokens). Embedding time grows faster
than linearly with length, and the long tail is mostly text a search query
never matches: code examples, parameter and return lists, link lines. On
g-mesh's own corpus the full text has a p99 of 705 tokens and a maximum of
8,591; 0.52% of symbols are truncated at 1024.

Options measured with the shipped model (jina-v2-base-code int8,
[ADR 0011](0011-embedding-model-int8.md)) on GM-398's eval
([`embedding-eval.md`](../architecture/embedding-eval.md), D5-D9), against
its own 1024-token full-text run:

| option | recall@10 Δ | worst language | Q4 confident-wrong Δ | pass time | max RSS | D9 |
|---|---|---|---|---|---|---|
| full text, 1024 (today) | 0.637 | - | - | 1.00x | 1.00x | baseline |
| full text, truncated at 512 | +0.0 | go +0.0 | **+0.9** | 0.88x | 0.75x | fail (Q4, C-win) |
| full text, truncated at 256 | -0.2 | rust -1.0 | **+0.4** | 0.77x | 0.65x | fail (Q4) |
| first paragraph of the doc | -1.2 (lower -2.5) | rust -4.0 | -2.2 | 0.58-0.65x | 0.68-0.71x | pass in GM-423, C-no-worse unsettled in GM-465 |
| **structured** | **+0.0** (lower -0.5) | rust -1.0 | -1.8 | 0.68x | **0.684x** | **pass** (C-win on RSS) |
| first paragraph + file path / parent header | +5.0 / +4.7 vs no header | - | **+12.3 / +14.8** vs no header | 1.25x / 1.26x vs no header (predicted) | - | no-go |

- Truncation alone ([`gm-423-sequence-length.md`](../results/gm-423-sequence-length.md))
  keeps recall but raises confident-wrong answers, and 512 saves too little.
- `structured` ([`gm-465-structured-trim.md`](../results/gm-465-structured-trim.md))
  keeps the doc's summary, headings and short paragraphs and drops fenced and
  indented code, doctests, dropped sections (`# Arguments`, `# Examples`,
  `Args:`, `Raises:`, ...), JSDoc tags, reST fields and link-only lines. It
  is the only trim with no recall loss, its false-alarm rate drops 1.3 points,
  and on g-mesh nothing is left above 512 tokens (p99 201). Cost is the
  median of two rounds per arm against one int8 round.
- A structural header (file path, parent type) helps recall but fails Q4 at
  its re-fitted floors ([`gm-455-structural-context.md`](../results/gm-455-structural-context.md));
  the owner accepted the no-go ("да, no-go").
- The token-based cost model ([`gm-466-cost-model.md`](../results/gm-466-cost-model.md))
  predicts structured at 0.630x pass time on g-mesh and 0.712x pooled,
  first-paragraph at 0.597x / 0.663x. Its error (0.05-0.09) is wider than
  the 0.60x gate's margin, so D9's pass-time gate stays on measured passes;
  structured passes C-win on max RSS (0.684-0.696x against the 0.70x gate)
  and C-no-worse. The D9 rule itself is unchanged.

## Decision

We embed the `structured` form: `embedding::text::text_to_embed` trims the
doc comment by the rules in that module's doc, then appends the signature as
before. A doc that trims to nothing falls back to the untrimmed text when
there is no signature, so the set of nodes with a vector does not change.
`DEFAULT_MAX_SEQUENCE_LENGTH` stays 1024: nothing measured supports a lower
cap once the tail is trimmed.

The eval harness's `structured` form calls the same function, and a fixture
of real and written doc comments pins it to the independent Python port
(`eval/embedding/gm423_text_fixture.py`), so eval and product cannot drift.

The similarity floors move to the ones fitted for this text (GM-465, D6):
go / python / rust / typescript **0.57 / 0.59 / 0.57 / 0.53**, default 0.53
(python and rust rise from 0.57 and 0.55). At these floors the held-out
authored-query false-alarm rate is 12.1% and confident-wrong on positives
49.8%, against 14.3% and 51.6% for the full text at its floors.

Owner, 2026-10-01: "хорошо, берем structured".

## Consequences

- **One full re-embed on upgrade.** `embeddingVersion` gains a text-form tag
  (`<model>+int8+structured` for the default model, `<model>+structured` for
  any other), so backfill and the workspace swap re-embed every stored
  vector once, in the background. `search_code` serves a mix of old and new
  vectors until that pass completes. `PIPELINE_EPOCH` was bumped to 2 under
  ADR 0007's rule at the time, so the machine-wide cache did not serve the
  unchanged texts either. That bump was a cost, not a correctness
  requirement: the cache key is the text itself, and ADR 0007 no longer
  ties the epoch to the text format.
- **New python and rust floors.** `find_definition`'s semantic rung reads the
  same table; rust's 0.57 is 0.015 above the fp32 name-query calibration, the
  one place that rung now leans toward refusing.
- **Heuristic rules.** The trim is syntactic. A comment format it does not
  know (a custom section name, an unusual code marker) is kept as prose, or,
  past the first paragraph, dropped when longer than 200 characters. Only the
  six eval corpora were measured.
- **The RSS margin is small.** structured passes C-win by 0.004-0.016 on max
  RSS, measured on one laptop with a wide wall-clock spread; it misses the
  pass-time gate (0.68x against 0.60x).
- **Follow-ups.** Re-check the text form after any model adaptation (GM-436),
  and re-run it on the larger eval when it exists (GM-460). Whether
  `PIPELINE_EPOCH` must change with the text format: it need not, see
  ADR 0007 (GM-467). Calibrate name-query floors for `find_definition`'s semantic
  rung, which now refuses more for Rust (0.57 against its 0.555 name-query
  calibration) (GM-468).
