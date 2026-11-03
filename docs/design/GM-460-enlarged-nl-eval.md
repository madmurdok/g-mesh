# GM-460: an enlarged NL eval (design)

Status: design (GM-460/S1, 2026-11-03, branch `feat/GM-460-enlarge-nl-eval`
off `release-4.25.0`). Nothing here has been authored or run. Numbers are
from `eval/embedding/gm460_sizing.py`, which reads stored rankings and query
files only:

```sh
python3 eval/embedding/gm460_sizing.py <main checkout>/eval/embedding/work
```

## Question

GM-398's eval has 400 authored NL positives and 100 absent queries, 100 + 25
per language. 215 positives and 46 absent queries are held-out. The
per-language quantities behind Q4 and Q5 are much smaller than that:

- Q5 counts only held-out positives that both arms rank right at rank 1.
  That is 6 to 25 per language (GM-398, int8 vs fp32: go 16, python 20,
  rust 6, typescript 25).
- GM-443/S8's TypeScript Q5 verdict rested on 23 such pairs.
- GM-434's per-language false-alarm denominators were 7 to 47.

Any layer fitted on this set, such as F4's beta = 80 (ADR 0016), a floor or a
future learned reranker, is fitted and judged on these counts. This note
sets a per-language target n, the CI half-width it buys, and how the next
slices get there without changing what old results mean.

## Rules this builds on (cited, not restated)

- **Authoring**: `docs/architecture/embedding-eval.md` D3 (target-first,
  read-then-write, expected set of 1-3, absent proven by grep and structural
  lookup, blind to models, overlap recorded, freeze), plus the stricter
  choices of `docs/architecture/embedding-eval-int8-confirm.md` section 3
  (new seed, disjoint from earlier studies, fresh blind authors, `-c` ids,
  freeze and hash).
- **Split**: the task calls it "D5's split". It is defined in D6 of
  `embedding-eval.md`: the parity of the first byte of `sha256(id)`. Odd is
  held-out. Code: `core/src/cli/embed_eval/queries.rs::is_held_out_id`. The
  Python port is `q5_floor_sensitivity.py::held_out`. Metrics are D5, gates
  are D9 (Q4 = confident-wrong, Q5 = false alarm).

## 1. What exists

There is already a second authored set: GM-422's confirmatory set,
`eval/embedding/confirm/queries/` (814 queries: 650 positives and 164
absent). It was authored under D3 and confirm section 3, checked by a fresh
verifier, and frozen with sha256 recorded in
`docs/results/gm-422-int8-confirm.md`.

So far it has been scored only for int8 vs fp32: the S7/S9 verdict and the
descriptive S8 gap analysis. GM-455 kept it unscored on purpose
(`gm-455-structural-context.md`: "stays held out and is not scored here").
GM-443, GM-464 and GM-465 used only GM-398's set. Nothing has been fitted or
tuned on it.

Counts under the D6 split (held-out = odd):

| set | language | pos FIT | pos held-out | abs FIT | abs held-out |
|---|---|---|---|---|---|
| GM-398 | go | 39 | 61 | 14 | 11 |
| GM-398 | python | 53 | 47 | 14 | 11 |
| GM-398 | rust | 53 | 47 | 14 | 11 |
| GM-398 | typescript | 40 | 60 | 12 | 13 |
| GM-422 confirm | go | 94 | 106 | 24 | 26 |
| GM-422 confirm | python | 79 | 71 | 21 | 17 |
| GM-422 confirm | rust | 74 | 76 | 17 | 21 |
| GM-422 confirm | typescript | 71 | 79 | 20 | 18 |
| **merged** | go | 133 | 167 | 38 | 37 |
| **merged** | python | 132 | 118 | 35 | 28 |
| **merged** | rust | 127 | 123 | 31 | 32 |
| **merged** | typescript | 111 | 139 | 32 | 31 |

## 2. Observed variance of per-query Q4/Q5 deltas

These are held-out queries, each arm at its own D6 floors, in the same code
path as `g-mesh debug-embed-eval report` (the ported functions of
`q5_floor_sensitivity.py`). d is in {-1, 0, +1}. Each cell gives n, then the
SD of d, then the 95% half-width in points (1.96·SD/√n).

| pair | metric | go | python | rust | typescript |
|---|---|---|---|---|---|
| int8 vs fp32 | Q5 | 16, .342, 16.7 | 20, 0, 0 | 6, 0, 0 | 25, .200, 7.8 |
| int8 vs fp32 | Q4 | 72, .298, 6.9 | 58, .187, 4.8 | 58, .187, 4.8 | 73, .260, 6.0 |
| int8-structured vs int8 | Q5 | 17, 0, 0 | 19, .229, 10.3 | 5, 0, 0 | 25, 0, 0 |
| int8-structured vs int8 | Q4 | 72, 0, 0 | 58, .256, 6.6 | 58, .296, 7.6 | 73, .117, 2.7 |
| gte vs fp32 | Q5 | 6, 0, 0 | 11, .302, 17.8 | 4, .500, 49.0 | 14, .475, 24.9 |
| gte vs fp32 | Q4 | 72, .561, 13.0 | 58, .561, 14.4 | 58, .475, 12.2 | 73, .624, 14.3 |
| bge vs fp32 | Q5 | 5, .447, 39.2 | 12, .289, 16.3 | 5, .447, 39.2 | 13, .577, 31.4 |
| bge vs fp32 | Q4 | 72, .580, 13.4 | 58, .529, 13.6 | 58, .417, 10.7 | 73, .598, 13.7 |

GM-443/S8's TypeScript Q5 for the ce-minilm + int8 blend was 6 flips up and
0 down on n = 23, so SD = .449.

**Planning values.**

- **SD.** Q5 SD = 0.45 (the rerank blend's .449, the same level as gte's
  .475 and int8's Go .342). Q4 SD = 0.55 (the median of the model-change
  arms, .42-.62). Arms that barely move the ranking (int8, structured) sit
  at 0-0.3, so for them these values are conservative. A learned layer
  moves the top row, like the rerank and model arms, so the planning values
  follow those.
- **Pair rate r**, the share of held-out positives that both arms rank
  right first. Start from int8 vs fp32 over GM-398 + confirm: go 0.245,
  python 0.274, rust 0.152, typescript 0.381. Multiply by 0.857, the
  GM-443/S8 ratio of blend pairs to int8 rank-1 (60/70), because a layer
  that reorders loses some pairs. That gives go 0.210, python 0.235,
  rust 0.131, typescript 0.327.

## 3. Target n and the CI width it buys

### What would make a per-language CI useful

D9 gates Q4/Q5 pooled. Per language it guards only recall, through Q3 with
a -10 point tolerance. The later decisions needed more than D9 gives:

- **GM-443/S8** had to decide whether a +26-point TypeScript Q5 excess was
  real. Its interval was [+13, +43.5].
- **GM-422** reported Go's Q5 beside a pooled gate it could not gate.
- **GM-434** could neither fit nor verify a 3% per-language NL floor on
  7-47 queries.

The useful bar is the same 10-point per-language guard for Q4 and Q5 that
Q3 gives recall: a +10-point regression in one language should not hide
behind a pooled pass.

**Target**:

- **Q4**: per-language 95% half-width ≤ 7.5 points. That is SE ≤ 3.8, which
  gives about 80% power to flag a +10-point per-language Q4 regression at a
  one-sided 95% bound (10 / (1.645 + 0.84) = 4.0).
- **Q5**: no per-language target is affordable at that bar. It would need
  480-1,200 positives per language (table below), and python's frame cannot
  supply that (section 4). Q5 per language gets what the Q4 target buys:
  12-15 points in go, python and typescript. That is enough to show a
  GM-443-sized excess (+26) with a lower bound above +10. It cannot resolve
  a +5-sized one. Rust gets 19 points and is read as direction only,
  because its rank-1 rate is 0.13-0.15.

Positives per language (both halves; held-out is half; absent = positives/4)
needed for a 95% half-width h:

| h (pts) | Q5 pairs | Q5: go | python | rust | ts | Q4 held-out rows | Q4: pos per lang |
|---|---|---|---|---|---|---|---|
| 20 | 20 | 191 | 171 | 307 | 123 | 30 | 48 |
| 15 | 35 | 334 | 298 | 537 | 215 | 52 | 84 |
| 12.5 | 50 | 476 | 426 | 767 | 307 | 75 | 120 |
| 10 | 78 | 743 | 664 | 1,196 | 478 | 117 | 188 |
| 7.5 | 139 | 1,323 | 1,184 | 2,130 | 852 | 207 | 332 |
| 5 | 312 | 2,969 | 2,656 | 4,781 | 1,912 | 465 | 744 |

Half-width bought at a given size (planning values):

| pos / lang (abs = /4) | Q4 h | Q5 h go | python | rust | ts | pooled Q5 h | pooled Q4 h |
|---|---|---|---|---|---|---|---|
| 100 (today) | 13.6 | 27.2 | 25.7 | 34.5 | 21.8 | 13.9 | 6.8 |
| 250 (merge only, py/rust/ts) | 8.6 | 17.2 | 16.3 | 21.8 | 13.8 | 8.8 | 4.3 |
| 300 (merge only, go) | 7.9 | 15.7 | 14.9 | 19.9 | 12.6 | 8.0 | 3.9 |
| **330 (target)** | **7.5** | **15.0** | **14.2** | **19.0** | **12.0** | **7.6** | **3.8** |
| 400 | 6.8 | 13.6 | 12.9 | 17.3 | 10.9 | 6.9 | 3.4 |

The planning formula agrees with what was observed at 100 per language:
model arms' observed Q4 half-widths are 10.7-14.4 against 13.6 planned.

**Target: 330 positives and 83 absent per language, 1,320 positives and
332 absent in all.** The held-out half is about 165 positives and 41 absent
per language.

### Volume, staged

| stage | what | new queries authored | per language (pos / abs) |
|---|---|---|---|
| **1** | merge GM-398 + GM-422 confirm into one eval dir (no authoring) | 0 | go 300/75, py 250/63, rust 250/63, ts 250/63 |
| **2** | top-up to the target | **270 pos + 68 abs = 338** | 330/83 each |

Stage 1 alone already reaches Q4 h ≤ 8.6 and pooled Q5 h 8.8 (from 13.9).
Stage 2 equalises the languages at the 7.5 target. 338 queries is about 40%
of GM-422's 814-query authoring run. If the owner stops after stage 1, the
achieved half-widths are the "250" and "300" rows above.

Stage 2 allocation (new positives / new absent):

| language | corpus | positives | absent | frame left after GM-398 + confirm |
|---|---|---|---|---|
| go | gin | 30 | 8 | ~312 of 527 |
| python | py-requests | 80 | 20 | 144 of 298 |
| rust | ripgrep / g-mesh | 40 / 40 | 10 / 10 | > 2,000 each |
| typescript | excalidraw / task-tracker-mcp | 65 / 15 | 12 / 8 | > 2,000 / 18 |

"Frame left" is computed from the confirm target lists: the rows after the
last consumed one, minus confirm expected symbols. gin's and the large
corpora's are bounds, because their confirm lists were truncated at the
oversampling count and the seed-460 sample over the snapshot decides the
rest. task-tracker-mcp has only 18 targets left, so TypeScript's top-up is
mostly excalidraw. py-requests caps python at about 390 positives in total.
That is why Q5 h = 10 is out of reach without a second Python corpus, which
D1 does not have and this task does not add.

## 4. The enlarged eval dir: `eval/embedding/v2/`

`eval/embedding/queries/` and `eval/embedding/confirm/queries/` are not
touched. Stored manifests record their sha256 (`queryFiles`) and every
analysis script stops on a mismatch. So old runs, old scripts and both old
studies stay valid.

`v2` is built on the pattern of `confirm/` (confirm Deviations 1-2):

- `v2/corpora.toml`, `v2/variants.toml` and `v2/queries/mechanical` are
  committed symlinks to the GM-398 files. `v2/work/` is local and
  gitignored.
- `v2/queries/<corpus>.jsonl` is generated and committed. It is the GM-398
  file's bytes, then the confirm file's bytes, then the GM-460 file's bytes
  (`v2/new/queries/<corpus>.jsonl`), each verbatim. `v2/build.py --check`
  fails unless:
  - the three prefixes are byte-equal to their sources, whose sha256 are
    pinned in the script, copied from the GM-398 manifests and the GM-422
    results table;
  - ids are unique across the file;
  - every line ends in `\n`.

  It prints per-language, per-side counts and the cross-side overlap list
  (section 6).

This is a new version of the instrument, not an extension of GM-398's
decision set. D2's "no other extension of the set is allowed after
measuring" protects GM-398's model verdict and GM-422's int8 verdict. v2
re-opens neither: no int8-vs-fp32 decision may be taken on v2. v2 is the
instrument for decisions not yet made, such as learned layers, floors and
rerank changes.

## 5. Authoring procedure (stage 2)

1. **Source code**: the six D1 corpora at their pins and the D4 snapshots
   GM-398 embedded, identified by the sha256 values in confirm section 3
   step 1. A different snapshot sha stops the slice.
2. **Targets**: `sample_targets.py` over each snapshot with **seed 460**.
   It uses D3's filters and 6/3/1 strata, and the sampler gives a slot to
   functions when a stratum runs out, as in confirm Deviation 3. It runs
   with `--exclude eval/embedding --exclude eval/embedding/confirm`, which
   drops every target either study consumed and every symbol in either
   study's expected sets. `--exclude` must become repeatable (edit map).
   Count: allocation × 1.3 to absorb skips. Output:
   `v2/new/targets/<corpus>.jsonl` and `.skips.jsonl`, with the frame sha256
   and dropped counts in `meta`.
3. **Ids and skeleton** are fixed before any text is written:
   - Positives are `<GM-398 prefix>-e<NNN>` in target order, with the
     shapes alternating phrase / sentence.
   - Absent queries continue the numbering, as in confirm Deviation 4.
   - The split is therefore fixed by ids nobody chose by content.
4. **Authors**: one fresh agent per corpus, briefed with D3, confirm
   section 3 and this section only. Authors must not see:
   - `docs/results/`;
   - any `rankings.jsonl` or `work/` dir;
   - the floors;
   - this note's variance tables;
   - which queries are held-out.

   They do not call `search_code`, the semantic rung or any embedding.
   Structural g-mesh tools, grep and Read are allowed. Rules from D3: at
   least half of each corpus's positives have `overlap = false`; expected
   sets have 1-3 symbols; an absent query must be proven absent. A new
   query may list an earlier study's symbol only as an *additional*
   acceptable answer, and only when it plainly qualifies (confirm section 3
   step 3).
5. **Checks before freeze**:
   - `check_queries.py --eval-dir eval/embedding/v2/new --seed 460
     --disjoint-from eval/embedding --disjoint-from eval/embedding/confirm`
     (`--disjoint-from` must become repeatable);
   - `confirm/check_targets.py`, adapted to `v2/new`;
   - the harness's resolve step, a dry `run` that fails on unresolved or
     non-embeddable expected nodes;
   - the verify slice (section 9).
6. **Freeze**: one commit adds `v2/new/queries/*`, the generated
   `v2/queries/*` and `docs/results/gm-460-enlarged-eval.md`. That doc
   holds each v2 file's sha256 and the per-language, per-side counts. This
   happens before any arm runs on v2. A later fix is a logged deviation
   applied to every arm (D3 step 7).

## 6. Split and leakage between FIT and held-out

- **Deterministic split.** The split is `is_held_out_id` on the id alone. Old
  ids are unchanged, so every GM-398 and confirm query keeps its side. Old
  GM-398 queries have the same side in v2 as in every stored GM-398 run.
  Confirm queries were "all held-out" for the int8 verdict only (confirm
  section 3 step 7). In v2 they follow D6 like everything else. Nothing is
  hand-labelled and no id is chosen after its text exists.
- **Symbol leakage.** A held-out query whose expected symbol is also
  expected by a FIT query hands a layer fitted on FIT its answer.
  - New targets exclude every earlier target and expected symbol (step 2),
    so new-vs-old overlap can arise only through additional acceptable
    answers.
  - Old-vs-old today: exactly 1 held-out positive shares an expected
    symbol with a FIT positive, `rg-042` (held-out) and `rg-c047` (FIT).
    This is the documented overlap in confirm's verifier deviation 3.
  - `build.py --check` lists every such pair. Each v2 report adds a
    **strict held-out** view that drops those held-out queries. Any learned
    layer is judged on both views. The split itself is not changed, because
    changing it would move old queries' sides.
- **Text leakage.** The exact-match check is mechanical (step 5). The verify
  slice reviews near-duplicates: for each new text, the nearest old text by
  token Jaccard, and any pair at 0.5 or more is read by hand.
- **Mechanical name queries** stay FIT-only (D6), unchanged.

## 7. Re-running stored runs on v2

No node is re-embedded. Only queries are new (about 1,650 authored plus
the existing mechanical queries).

1. `cargo build --release --bin g-mesh` on the branch.
2. `v2/setup_work.sh <main checkout>/eval/embedding/work`, a copy of
   `confirm/setup_work.sh`. It symlinks snapshots, corpora and models, and
   seeds each v2 run dir with the stored `manifest.json` and `vectors.bin`,
   so the harness reuses node vectors (confirm Deviation 2). Arms and the
   stored dirs they are seeded from:
   - `jina-v2-base-code-fp32` (D9 reference) from `work/runs/`;
   - `jina-v2-base-code-int8` (GM-398/GM-422 control) from `work/runs/`;
   - `jina-v2-base-code-int8-structured` (the shipped embedder, ADR 0012)
     from `work/runs-gm465/`;
   - `random` and `shuffled` (D7 validity) from `work/runs/`.
3. `g-mesh debug-embed-eval run --eval-dir eval/embedding/v2 --variant <v>`
   for each arm, then `report` (D6 refit on v2's FIT half, D9 vs fp32 and
   vs int8-structured).
4. **Shipped ranking**: F4 rerank K = 30, beta = 80, over int8-structured
   (ADR 0016), via `gm464_ce_structured.py --work eval/embedding/v2/work`.
   The CE cache is seeded from `work/rerank_cache/` and new pairs are
   scored. beta is not refit. This step is optional; it is decision 3.
5. **Two floor views**. The product floors do not change in this task.
   - At the **shipped** floors, 0.57 / 0.59 / 0.57 / 0.53
     (`similarity.rs::floor`): GM-434 columns via `shipped_floor_rates.py`.
   - At **D6-refit** floors on v2 FIT: the new baseline. If they differ from
     the shipped floors, the difference is reported, not acted on.
6. Everything runs in one background script with fail-fast, controls first
   (section 8). Its summary file goes to `docs/results/gm-460-enlarged-eval.md`.
   Query-embedding timings are not a cost measurement.

## 8. The control: the old subset reproduces old numbers exactly

`gm460_subset.py` (new) writes, from a v2 run dir, a subset run dir that
holds only the rankings rows whose ids are in a given old query file, plus
the mechanical rows. Its manifest's `queryIds` and `queryFiles` are
rewritten to that old file and its sha256, so the existing scripts run on
it unchanged, against the old eval dir. Each check below must match at the
published precision, or the measure slice stops.

| # | what | must equal | source |
|---|---|---|---|
| C0 | node vectors reused | `vectors.bin` sha256 of every v2 run = the stored run it was seeded from | `work/runs/*/<corpus>/`, `work/runs-gm465/jina-v2-base-code-int8-structured/<corpus>/` |
| C1 | rows | every GM-398-id row of v2 `rankings.jsonl` byte-identical to the stored row, for fp32, int8 and int8-structured | same dirs |
| C2 | GM-398 / GM-422 S1 | `q5_floor_sensitivity.py <subset> eval/embedding` step 0 passes: floors fp32 0.56/0.58/0.56/0.55, int8 0.57/0.57/0.55/0.53; held-out FA fp32 18.8/9.5/14.3/14.8%, int8 29.4/10.0/14.3/7.7%; Q5 +2.1 [-1.4, +6.2], go +12.5 n=16, python 0.0 n=20, rust 0.0 n=6, ts -4.0 n=25 | `docs/results/gm-398-model-comparison.md` (Quality per language, Q5 table); `docs/results/gm-422-int8-q5.md` control |
| C2b | GM-398 recall | `recall_at_k.py` control: fp32 r@1/5/10/MRR 0.335/0.542/0.633/0.427, int8 0.328/0.532/0.637/0.424, every per-language r@10 cell | `gm-398-model-comparison.md` "Quality, pooled"; `gm-443-recall-rerank.md` "Control" |
| C3 | GM-422 confirm | `int8_confirm_report.py` on the confirm-id subset (`--eval-dir eval/embedding/confirm`): Q1 +0.6 [lower -0.6], Q2 +0.005 [-0.004], Q4 +1.5 [upper +2.9] n=814, Q5 -2.0 [upper +1.2] n=161; per language Q5 go +2.1 [-4.2, +8.3] 48 pairs, python -8.8 [-17.6, -2.9] 34, rust +4.2 [0.0, +12.5] 24, ts -5.5 [-10.9, -1.8] 55 | `docs/results/gm-422-int8-confirm.md` "Gates" |
| C4 | GM-465 (shipped embedder) | structured vs int8: r@10 0.637, MRR 0.421, Q1 +0.0 (-0.5), Q2 -0.003 (-0.010), Q4 -1.8 (+0.3), Q5 -1.3 (+0.0), floors .57/.59/.57/.53; at shipped floors FA 12.1% (8/66), CW pos 53.0% (114/215), CW absent 21.7% (10/46) | `docs/results/gm-465-structured-trim.md` "D9 against the int8 1024 baseline", "GM-434 columns" |
| C5 | GM-464 F4 (only if step 4 runs) | vs int8-structured r@10 +3.7 [+1.8], MRR +0.047 [+0.021], Q4 -3.0 [-0.2], Q5 +0.0 | `docs/results/gm-443-recall-rerank.md` "GM-464 reading" |

C3 is the only reproduction of the confirm numbers available. The confirm
runs' `work/` dir no longer exists in any checkout, so C1 cannot cover
confirm ids.

C2 is only a real control if it can fail. It fails a subset that leaks new
FIT rows into the fit only when v2's refit floors (full FIT half) differ
from GM-398's in at least one language. The measure slice prints both sets
of floors. If they are equal everywhere, it says that C2 cannot catch a
leak, and it adds a row-count check: the subset's FIT rows equal GM-398's
FIT rows plus the mechanical ones.

## 9. Edit map

**Authoring slice (stage 2; after owner approval of this note):**

| file | change |
|---|---|
| `eval/embedding/sample_targets.py` | `--exclude` repeatable (union of the excluded targets and expected symbols) |
| `eval/embedding/check_queries.py` | `--disjoint-from` repeatable |
| `eval/embedding/confirm/check_targets.py` | take the study dir as an argument, or a copy in `v2/` |
| `eval/embedding/v2/{corpora.toml,variants.toml,queries/mechanical}` | symlinks to the GM-398 files |
| `eval/embedding/v2/build.py` | generate and `--check` `v2/queries/*` (section 4), print counts and cross-side overlap |
| `eval/embedding/v2/new/targets/*.jsonl`, `*.skips.jsonl` | seed-460 samples |
| `eval/embedding/v2/new/queries/*.jsonl` | 338 authored queries (section 3 allocation) |
| `eval/embedding/v2/queries/*.jsonl` | generated by `build.py` |
| `eval/embedding/v2/README.md` | what v2 is, how it is built, the freeze |
| `docs/results/gm-460-enlarged-eval.md` | freeze table: sha256 and counts per file, per language and side |
| `.gitignore` | `eval/embedding/v2/work/` if not covered |

**Measure slice:** `eval/embedding/v2/setup_work.sh`, `eval/embedding/gm460_subset.py`,
and one background script for section 7 + section 8. The results go into
`docs/results/gm-460-enlarged-eval.md`, including the achieved per-language
half-widths from the observed SDs.

**Doc slice:** `docs/architecture/embedding-eval.md` gets a dated entry (D2:
v2 and why it does not re-open old verdicts; D6: split unchanged, strict
held-out view; target n and h) and `eval/embedding/README.md` gets a v2
row.

## 10. Must confirm

**Verify (after authoring, before freeze):**

- The `build.py --check` prefixes are byte-equal to the GM-398 and confirm
  files. Their sha256 match `gm-422-int8-confirm.md`'s table and the
  GM-398 manifests' `queryFiles`.
- No new target is in either earlier target list or expected set.
  Recompute this from the files; do not trust the sampler's `meta`.
- `check_queries.py` passes per corpus: overlap = false ≥ half, shapes
  alternate, expected sets 1-3, no exact repeat of an earlier text.
- 60 sampled positives are re-derived from the code. Every new absent query
  is re-grepped.
- The near-duplicate review (section 6) is done; every pair at 0.5 or more
  is resolved.
- Blindness: the author transcripts contain no `search_code`, no semantic
  rung, no embedding, and no read of `docs/results/` or `work/`.
- Per-language, per-side counts are within ±15% of half. They are not
  adjusted if not; the deviation is reported. The cross-side overlap list
  is given.

**Measure:**

- C0-C4 (and C5 if run) pass before any v2 number is written.
- Observed Q4/Q5 SDs and pair rates on v2 are compared with the planning
  values (0.55 / 0.45; r 0.21 / 0.235 / 0.131 / 0.327). The achieved
  per-language half-widths are stated. If they miss the 7.5 Q4 target, say
  by how much; do not author more.
- D7 validity holds on v2 (random, shuffled).
- The refit floors on v2 are reported beside the shipped floors.
- Machine state (`uptime`, `/usr/bin/time -p`) is recorded with the run.

## Risks

- **Reusing the confirm set.** It was written to judge int8 alone, and its
  per-query int8/fp32 outcomes were inspected in GM-422/S8. For any
  question other than int8 vs fp32 it is as unseen as new queries.
  Mitigation: v2 is never used to re-decide int8. If the owner rejects the
  merge, stage 2 grows to about 1,150 new queries for the same target, and
  python cannot reach 330 at all (frame).
- **Strata skew.** confirm Deviation 3 left types under 30% in gin,
  py-requests, excalidraw and task-tracker-mcp. Seed 460 makes it worse.
  Per-stratum reporting (D3 step 6, D5) keeps it visible; nothing can fix
  it within the frame.
- **Planning SDs are borrowed.** A layer that moves fewer top rows than the
  rerank does gets narrower intervals than planned. One that moves more
  gets wider ones. The measure slice reports the achieved widths.
- **Rust Q5 stays weak at any affordable n** (rank-1 rate 0.13-0.15).
  Rust Q5 is read as direction only. GM-434's per-language NL floor fit
  (3% quantile, which needs ≥ 100 rank-1 FIT positives per language) is
  still out of reach for rust (~800 FIT positives) and python (frame).
- **Harness drift.** The harness changed after GM-465 (GM-441: prioritised
  embedding, rerank step). C1 (byte-identical rows) is the guard. If it
  fails, the variant spec is pinned before anything else runs.

## Decisions for the owner

1. **Merge the GM-422 confirm set into v2** (recommended), or author
   everything new.
2. **Target**: 330 + 83 per language via stage 2's 338 new queries
   (recommended); stage 1 only (0 authoring, Q4 h ≤ 8.6); or 400 per
   language (about 600 new, python capped near 390 by its frame).
3. **Shipped-ranking arm**: re-run F4 rerank K = 30 on v2 (recommended,
   because it is what `search_code` serves), or embedding arms only.
4. **Floors**: report shipped and D6-refit floors on v2 and change nothing
   in the product (recommended), or let a v2 refit propose new floors in a
   follow-up task.

## Code facts

From grep on known files; no cross-file symbol question arose, so g-mesh
was not called.

- `core/src/cli/embed_eval/queries.rs::is_held_out_id` (split).
- `core/src/cli/embed_eval.rs` `--eval-dir` (default `eval/embedding`)
  and `work/` under it.
- `core/src/mcp/similarity.rs::floor` (shipped floors 0.57 / 0.59 / 0.57 /
  0.53, ADR 0012).
- Stored-run manifests carry `queryIds` and `queryFiles` (sha256), checked
  by `q5_floor_sensitivity.py::load_arm`.
