# 0013. Score cursor: the score travels as its f64 bits

## Status
Accepted

## Context
`paginate_by_score` (`search_code`, `find_definition`'s candidate list) keys
its continuation on the last row's `(score, id)` and resumes with
`score < ? OR (score = ? AND id > ?)`. The score went into the cursor as a
JSON number. `serde_json` without its `float_roundtrip` feature decodes about
one score in ten 1 ULP off: decoded higher, rows tied at that score are served
again; decoded lower, the remaining tied rows are skipped.

## Decision
The cursor carries `score_bits: u64` (`f64::to_bits`), decoded with
`f64::from_bits`: exact by construction, independent of any JSON float parser.

Rejected: enabling `float_roundtrip`. It is a workspace-wide feature (feature
unification) that changes every float parse in the process and slows them,
to fix one field, and the cursor would stay correct only as long as no build
drops the feature.

A cursor in the earlier `score` shape is refused with an error telling the
caller to repeat the query without a cursor, not decoded: its score may be the
1-ULP-off value this fixes, and cursors are short-lived.
