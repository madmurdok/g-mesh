# 0042. The cross-encoder reads an enriched text: enclosing, path words and a 48-token body

## Status
Accepted (GM-698, owner decisions: fields E + P + B48 without I; body read
from disk at query time; `Scorer::count_tokens` with no default;
`SIGNATURE_KEYWORDS` kept in sync by a Python test).
Design note: [`GM-698-enriched-reranker-text.md`](../design/GM-698-enriched-reranker-text.md).
Amends [ADR 0016](0016-cross-encoder-rerank.md): "the cross-encoder reading
the same structured text the embedding model embeds" no longer holds, and the
blend weight is re-fitted for the new text (`80` becomes `40`).

## Context
ADR 0016's cross-encoder reads only the structured text (`text_to_embed` of
the doc comment and signature). GM-462 measured richer inputs on the eval
set: the node's enclosing scope (E), its signature identifiers (I), its file
path words (P) and its body lines (B). I hurt MRR in the leave-one-out; E, P
and a body cut at 48 tokens carried the gain
(`docs/design/GM-462-richer-reranker-input.md`).

## Decision
- **The text.** `embedding::ce_text::ce_text` builds `<E>\n<P>\n<base>\n<B>`,
  an empty field dropped with its newline: E the words of the native kind
  (else the kind) and of the qualified name minus the trailing name; P the
  words of the file path; base the structured text, unchanged; B, for
  `Function` and `Type` only, the source lines after the signature, a Python
  docstring skipped, punctuation-only lines dropped, whitespace collapsed,
  each line cut at 200 characters, whole lines while the field stays within
  48 tokens. No I field.
- **One spec.** The rules are `eval/embedding/gm462_ce_text.py`'s
  `build(row, src, "EPB", tok, 48)` byte for byte, the text the blend weight
  is measured on. `core/src/embedding/testdata/ce_text.json`, written from the
  Python by `eval/embedding/gm698_ce_text_fixture.py`, is checked by a Rust
  test and a Python test; `rerank_parity.json` carries the enriched texts and
  their logits.
- **Body from disk at query time.** `search_code` reads each distinct file of
  the window once per call under the project root (invalid UTF-8 replaced,
  CRLF and lone CR read as LF, a span past the end clipped). No schema
  change and no re-index. A file edited since its last index pass can give
  body lines that do not match the indexed signature until the watcher
  re-indexes it; a missing or unreadable file gives no body, never a failed
  call.
- **Token budget.** `Scorer::count_tokens` (no default: every scorer decides
  its own unit) counts a line with the cross-encoder's tokenizer, special
  tokens excluded. Test scorers count whitespace words.
- **`SIGNATURE_KEYWORDS`.** The Rust const in `embedding/text.rs` is the
  source; `eval/embedding/test_gm462_ce_text.py` parses it and compares it
  with the Python copy. The Rust builder does not use the list (no I field).
- **What does not change.** Where the rerank runs, the window, the order's
  tiebreak, `score` and the verdicts are ADR 0016's.

## Consequences
- About 82 tokens per node instead of 38; still far under the 512-token pair
  limit.
- File reads on the first page's hot path, at most one per distinct file of
  the 30-row window.
- The blend weight is re-fitted for this text: the shipped `BETA` is 40, the
  FIT optimum of the ship arm on the v2 corpora (`docs/results/gm-698-ship-arm.md`).
