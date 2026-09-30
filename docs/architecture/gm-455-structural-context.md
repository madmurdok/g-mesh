# GM-455: structural context in the embedded text (design)

Status: design (GM-455/S1, 2026-09-30, on `release-3.17.0`). Nothing here has
been run. Eval only: no product default (`text_to_embed`, `embeddingVersion`,
floors) changes in this task. Instrument and rules: GM-398's eval
([`embedding-eval.md`](embedding-eval.md)), D5-D9 unchanged; the GM-422
confirmatory query set stays held out and is not scored here.

## Question

`text_to_embed(doc, sig)` is `doc + "\n\n" + sig`. A symbol with no doc
comment embeds as its signature alone: 130 of the eval's 400 positive
queries (plus 16 mixed) target such symbols, and GM-422/S8 found these texts
shift most under int8. g-mesh knows things about every symbol that a plain
embedder does not: its file, its enclosing type or trait, its call
neighbours. Does putting some of that into the text improve ranking, at
what cost in tokens and in re-embed churn?

## Code facts (g-mesh, main checkout at release-3.17.0)

| call | returned |
|---|---|
| `find_definition text_to_embed` | `embedding::pipeline::text_to_embed(doc_comment, signature) -> Option<String>`, `core/src/embedding/pipeline.rs:751`; inputs are only the two strings |
| `find_callers text_to_embed` | 5, complete (`hasMore: false`): `pipeline::current_embeddable_text`, `EmbeddingPipeline::compute`, `pipeline::embed_node`, `storage::language_swap::plan_embeddings`, `cli::embed_eval::load_nodes` |
| `search_code "SQLite schema creating nodes and edges tables"` | pointed to `storage::schema`; read `core/src/storage/schema.rs:179` directly |
| `find_definition EdgeKind` | ambiguous: `wire/src/lib.rs` (Rust enum) and the TS plugin's type; read the Rust one by grep: `Defines, Imports, Calls, SupertypeOf, References, Exports` |

Read directly (one known file each), and by `sqlite3` on the eval snapshots:

- `debug-embed-eval snapshot` is a **plain file copy of the corpus's
  `index.db`** (`embed_eval.rs:286-302`). Every snapshot therefore already
  holds `nodes` (with `name`, `qualifiedName`, `filePath`, `container`,
  `nativeKind`, `startLine..endLine`) and `edges` (g-mesh snapshot: 11,926
  CALLS, 9,601 resolved to Function nodes). `load_nodes` reads only
  `id, kind, qualifiedName, filePath, language, docComment, signature`; the
  harness has to select more columns, not a new data source. No graph store
  or daemon is involved.
- **Parent is not an edge.** DEFINES edges come only from File and container
  Module nodes (g-mesh: 6,679 + 6,157), never from a type to its methods.
  The parent is in `qualifiedName`, per language:
  Rust `decompress::DecompressionMatcherBuilder::new`,
  `decompress::<DecompressionMatcherBuilder as Default>::default`;
  Python `Session.prepare_request`; Go `authPairs.searchCredential`;
  TS `Collab#componentDidMount`.
- `container` is the module path (Rust `grep_cli::decompress`, Python
  `requests.sessions`, Go the package import path) and is **empty for TS**.
  So a module arm is dropped: `filePath` carries the same words for Rust and
  Python, more for Go (the file inside the package), and exists for TS.
- Go signatures already contain the receiver (`func (a authPairs) ...`): the
  parent arm is expected to change little on gin.
- Rust trait-impl methods are the extreme case: 141 of 148 on g-mesh are
  signature-only (`fn drop(&mut self)`).
- The production embedding cache is keyed by `sha256(text)`
  (`embedding/cache.rs:98`), so "churn" = texts whose hash changes = model
  calls.

## Arms

All arms are model arms of the shipped `jina-v2-base-code-int8`, max 1024
tokens. A new `variants.toml` field `context` (`none` default | `path` |
`parent` | `path-parent` | `path-parent-shuffled` | `callees`) is orthogonal
to GM-423's `text` (`full` | `first-paragraph` | `structured`). The
fingerprint drops a default `context`, as it already drops a default `text`,
so stored runs stay valid.

**Text format** (fixed now, not tuned on the eval): a header of context
lines, one per field present, in the order path then parent, joined by
`"\n"`; then `"\n\n"`; then `text_for(form)` exactly as today. No labels.
An empty header adds nothing, not even the separator, so `context = none`
is the old string byte for byte. The context is a **prefix**: jina mean-pools,
so position matters only for truncation, and no first-paragraph text is near
1024 tokens (max 741); under `full`, 0.24% of texts are truncated already.

- **path**: `filePath` as stored (project-relative).
- **parent**: only when stripping `name` and its separator (`::`, `.`, `#`)
  off `qualifiedName` leaves a remainder whose last segment is a Rust
  `<X as T>` (rendered `impl T for X`) or equals the `qualifiedName` of a
  `Type` node of the same language in the snapshot (rendered
  `{parent.nativeKind} {parent.name}`, e.g. `struct Foo`, `class Session`,
  `trait Embedder`). Otherwise no parent line (free functions, top-level
  types, modules). Deterministic, from the snapshot alone.
- **callees** (optional): a third header line, the distinct `name`s of the
  node's resolved CALLS targets that are Function nodes, sorted, joined by
  `", "`, at most 8 (the first 8 in sort order). Callers are not an arm: their churn is
  paid by edits to *other* files (see Churn).
- **body head** (not budgeted): the first 3 non-blank body lines of an
  undocumented symbol, read from the pinned checkout (`work/corpora/<id>`,
  HEAD verified as `snapshot` does) at `startLine..endLine`. Excluded by
  default: it turns every body edit into a re-embed (E1 below), and it
  lengthens exactly the texts GM-423 just shortened. Owner decides.

Rust, ripgrep, signature-only, `path-parent` x `first-paragraph`:

```text
crates/cli/src/decompress.rs
impl Default for DecompressionMatcherBuilder

fn default() -> DecompressionMatcherBuilder
```

Python, py-requests, documented, `path-parent` x `first-paragraph`:

```text
src/requests/sessions.py
class Session

Constructs a :class:`PreparedRequest <PreparedRequest>` for
transmission and returns it. The :class:`PreparedRequest` has settings
merged from the :class:`Request <Request>` instance and those of the
:class:`Session`.

def prepare_request(self, request: Request) -> PreparedRequest
```

(`path` alone drops the second line; `parent` alone drops the first.)

### Run plan and budget

Base form is **first-paragraph**, the only form that passed D9 (GM-423).
Doc form and context interact only on documented targets: a
signature-only symbol's text is identical under every doc form. So the
context factor is explored on one form and only the winner is crossed.

| stage | runs (six corpora each) | est. |
|---|---|---|
| 1 | `fp-ctx-none` (control C0), `fp-path`, `fp-parent`, `fp-path-parent`, `fp-path-parent-shuffled` (control C1) | 5 x ~9 min = 45 min |
| 1b | g-mesh re-time (embed only, reverse order) of the three real arms | 3 x ~4 min = 12 min |
| 2 | only if a stage-1 arm passes: winner x `full`, winner x `structured` (if GM-465 has landed) | 2 x ~10 min = 20 min |
| 3 | optional: `fp-path-parent-callees` if stage 1 passes and owner asks | ~10 min |
| churn | in-memory, g-mesh snapshot, all arms (below) | < 2 min |

~9 min per run = GM-423's first-paragraph pass (405 s embed, all corpora)
plus an estimated 10-20 header tokens on a 24-token median text; the run
reports the real ratio. **Budget: 80 min core, 2 h cap with stage 3.**
Beyond 2 h: stop and report.

## Harness changes (S2, eval only)

- `Node` gains `name`, `native_kind`, `container`, `start_line`,
  `end_line`; `load_nodes` selects them. Parent lookup = a map
  `(language, qualifiedName) -> Type node` built once per snapshot.
- Callees: one query `SELECT fromId, t.name FROM edges JOIN nodes t ...
  WHERE kind='CALLS' AND resolved=1 AND t.kind='Function'`.
- Context applies only where `text_for(form)` is `Some`: the candidate set
  and `node_ids_sha` do not change with `context`.
- Shuffled arm: path strings deranged across **files** (seeded, `arm_seed`),
  so symbols of one file share one wrong path; parent lines deranged across
  the nodes that have one. The header shape and token count stay realistic.
- Rankings gain, per query, `targetDoc` (`doc` | `sig` | `mixed`: whether
  the expected symbols have a doc comment) and `pathOverlap` (the query
  shares a sub-token of length >= 4, D3's rule, with an expected symbol's
  file path or parent name). The report splits on both.
- `debug-embed-eval churn --variant <v> --corpus g-mesh`: applies the
  synthetic edits below to the in-memory `Node`s/edges and recomputes texts
  with the **same builder** the run uses, so the count cannot drift from the
  embedded text.

## Churn

Churn per edit = number of embedded texts whose `sha256` differs or is new
after the edit, i.e. cache misses. Measured on the g-mesh snapshot over
**every instance** of each edit (mean, p50, p90, max), not a hand-picked
one:

| edit | instances | none | path | parent | callees |
|---|---|---|---|---|---|
| E1 body edit that adds one call | every Function | 0 | 0 | 0 | 1 |
| E2 rename a function | every Function | 1 | 1 | 1 | 1 + distinct callers |
| E3 add a method to a type | every Type with members | 1 | 1 | 1 | 1 |
| E4 rename a type | every Type with members | 1 + sigs naming it | as none | as none + members | as none |
| E5 rename/move a file | every file | 0 | symbols in file | 0 | 0 |

The table's cells are the expected values; the command computes the actual
distributions. E1 is the common edit and is what makes callees and body
head expensive: today a body edit costs zero embeddings.

**File-locality** matters more than the count. Path and parent derive from
the node's own file (`filePath`, `qualifiedName`), so their changes arrive
inside the diff that already re-extracts that file. Callees (E2) change
texts in *other* files that no diff touches: the product would need a new
invalidation path (re-embed callers on a callee rename), which does not
exist. That is a product cost, not just a count.

Control for the simulator: one real E4 on a scratch copy of the g-mesh
checkout (rename one type with >= 3 methods), re-index, snapshot, dump
`(id, sha256(text))` per arm before and after; the diff count must equal the
simulator's count for that instance, per arm.

## Controls

- **C0 empty context**: `fp-ctx-none` must reproduce the stored GM-423
  `first-paragraph` run bit-identically (vectors max |Δ| = 0, byte-identical
  rankings, same fingerprint). Plus a unit test: header builder with no
  fields returns `text_for(form)` unchanged; control = always emitting the
  `"\n\n"` separator fails it.
- **C1 broken context**: `fp-path-parent-shuffled`. Report paired Δ
  (`path-parent` - shuffled). If that Δ's lower bound is <= 0 **and**
  `path-parent` vs `none` is also ~0, the result is "measured nothing", not
  "context does not help".
- **D7 broken arms**: random and shuffled vectors, as in GM-398, for
  validity of every report.
- **Token counts**: the report's shares above 512/1024 recomputed by
  `gm423_token_lengths.py` must match the harness's to the node (GM-423's
  check).

## Reporting

`docs/results/gm-455-structural-context.md`. For every arm:

1. **Context effect**: D9 as a *quality candidate* against `fp-ctx-none`
   (same form, no context): Δ recall@10 and Δ MRR lower bounds > 0, Q3-Q5,
   pass time <= 1.5x.
2. **Against the shipped model**: D9 vs `jina-v2-base-code-int8` at 1024
   tokens, `full` (stored run, `report --reference`), as GM-423 did; role
   *cost* if pass time <= 0.60x, else *quality*.
3. **GM-434 columns**: false alarm, confident-wrong (positives, absent),
   mechanical false alarm at the shipped int8 floors, not re-fitted
   (`shipped_floor_rates.py --floors shipped-int8`), plus each arm's fitted
   floors. A shared header raises in-file similarity, so floors may move.
4. **Splits** (descriptive, not gated): recall@10 and MRR with Δ and bounds
   for `targetDoc` = doc (n=254) / sig (n=130) / mixed (16), and for
   `pathOverlap` true/false.
5. **Tokens per text**: p50/p90/p99/max and added header tokens per arm.
6. **Pass time**: g-mesh `embedNodesMs`, median of two rounds, max RSS,
   `uptime` and `/usr/bin/time -p` user/sys/real per invocation.
7. **Churn** table (above) with distributions.

## Go / no-go

**Go** (to a product task, which the GM-423 ADR folds into its single choice
of embedded-text form): an arm passes (1) as a quality candidate, passes
(2) in some role, C0 and C1 pass, and its E1 churn is 0. Preference among
passing arms: fewer header tokens, then file-local churn only.

**No-go**, any of:
- no arm passes (1): "inconclusive = keep", as D9 says;
- the gain lives only in `pathOverlap = true` (path words matching query
  words the author saw while reading the file) with nothing on the rest;
- the arm passes (1) but fails (2) because the added tokens push pass time
  past the GM-423 cost win: reported as a trade-off for the owner, not
  shipped by default;
- callees: any passing result still needs cross-file invalidation; no-go
  for this task, a separate design if the gain is large.

## Risks

- **Hubness / file clustering**: a shared path makes a file's symbols more
  alike; an NL query matching path words may pull a whole file above the
  floor and raise confident-wrong (Q4). Measured, not assumed.
- **Author leak**: D3 authors read the target file, so its path vocabulary
  may be in the query. The `pathOverlap` split exposes it; queries are not
  rewritten.
- **Power**: 400 positives; a real +2-point gain may not clear "lower bound
  > 0". The sig split (n=130) is too small to gate on.
- **Floors shift**: any go means new fitted floors (D6), as for any model
  change, and a one-time full re-embed (`embeddingVersion`).
- **E5 renames**: a directory move re-embeds every symbol under it with
  `path`; rare, but unbounded.
- **Go and TS parity**: Go's receiver is already in the signature and TS has
  no `container`; per-language Q3 shows whether one language pays for
  another's gain.

## Owner decisions

Approved 2026-09-30 ("455 - давай попробуем, да") with the proposed answers:

1. Base form: first-paragraph; `structured` is crossed with the winner in
   stage 2.
2. Go bar: quality candidate (both lower bounds > 0), as written. A result
   below it is "inconclusive = keep"; the remedy is a larger eval (GM-460),
   not a softer bar chosen after seeing the data.
3. Body-head arm: out.
4. Header lines: unlabelled; only that format is run.
5. Stage 3 (callees): not run, since cross-file invalidation makes it a
   no-go for this task whatever it scores.
