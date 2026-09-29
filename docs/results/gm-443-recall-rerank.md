# GM-443 S1: can a small embedder serve as the recall stage?

Question: gte-small and bge-small-en-v1.5 cost ~0.22x jina's indexing
(GM-398's cost table). If their top-K holds the right answer about as often
as jina's top 10, a reranker over that top-K could restore jina-level
quality at small-model indexing cost. This slice measures only the
candidate sets, from the stored GM-398 runs. Nothing was re-embedded, and no
reranker was chosen or run.

## Method

- Data: the stored `g-mesh embed-eval run` output
  (`eval/embedding/work/runs/<variant>/<corpus>/rankings.jsonl`, 6 corpora),
  the same runs GM-398 reports. Each query keeps its top 100 hits
  (`KEPT_HITS`), so **the largest K available is 100**. Every query in every
  arm holds exactly 100 hits. All three arms share each corpus's snapshot and
  node-id hash, so their hit lists can be compared and unioned.
- Script: `eval/embedding/recall_at_k.py --runs <runs dir>`. It uses the
  definitions from `embed_eval.rs` / D5: a query's rank is the first expected
  node id among the kept hits, recall@K is 1 if that rank is <= K, and the
  pooled value is the mean of per-language means, summed in language order.
  It also checks the query files against each manifest (D3 freeze).
- Strata: **NL** is the authored positives, GM-398's scored set (n=400, 100
  per language). **name** is the mechanical name queries' positives (D6;
  n=879), which GM-398 never scores and which appear here only for
  information.

### Control: GM-398 reproduced

Before reporting anything, the script parses GM-398's pooled table (r@1,
r@5, r@10, MRR) and per-language r@10 from
`docs/results/gm-398-model-comparison.md`. It exits if any value differs at
the table's precision. All three arms match exactly: jina fp32
0.335 / 0.542 / 0.633 / 0.427, gte-small 0.275 / 0.478 / 0.580 / 0.373 and
bge-small 0.258 / 0.485 / 0.557 / 0.360, plus every per-language cell.

The check does catch real differences. The first version summed the
language means in insertion order and got gte r@5 = 0.4775 → 0.477 against
GM-398's 0.478, and the script stopped. Summing in BTreeMap order, as Rust
does, fixed it.

## recall@K, NL (pooled over languages)

| arm | @10 | @20 | @50 | @100 |
|---|---|---|---|---|
| jina fp32 (reference) | **0.633** | 0.718 | 0.810 | 0.880 |
| gte-small | 0.580 | 0.657 | **0.725** | 0.807 |
| bge-small-en-v1.5 | 0.557 | 0.625 | **0.718** | 0.792 |
| union gte ∪ bge top-K (aside) | 0.610 (13 cand.) | 0.680 (26) | 0.748 (65) | - |

Per language, NL:

| arm | lang | @10 | @20 | @50 | @100 |
|---|---|---|---|---|---|
| jina | go / python / rust / ts | 0.74 / 0.69 / 0.34 / 0.76 | 0.85 / 0.77 / 0.44 / 0.81 | 0.94 / 0.86 / 0.58 / 0.86 | 0.95 / 0.92 / 0.72 / 0.93 |
| gte-small | go / python / rust / ts | 0.70 / 0.63 / 0.32 / 0.67 | 0.74 / 0.74 / 0.39 / 0.76 | 0.79 / 0.82 / 0.48 / 0.81 | 0.82 / 0.91 / 0.61 / 0.89 |
| bge-small | go / python / rust / ts | 0.67 / 0.59 / 0.31 / 0.66 | 0.72 / 0.68 / 0.36 / 0.74 | 0.79 / 0.78 / 0.49 / 0.81 | 0.85 / 0.85 / 0.60 / 0.87 |

## recall@K, name queries (not in GM-398's score)

| arm | @10 | @20 | @50 | @100 |
|---|---|---|---|---|
| jina fp32 | 0.917 | 0.957 | 0.979 | 0.986 |
| gte-small | 0.943 | 0.963 | 0.976 | 0.987 |
| bge-small-en-v1.5 | 0.948 | 0.971 | 0.987 | 0.992 |

On name queries both small models already match or beat jina at every K,
so the recall stage costs nothing there. Rust is the weakest language for
every model: at @10, 0.83-0.87 against >= 0.9 elsewhere.

## Gate: small recall@50 >= jina fp32 recall@10 (0.633), NL pooled

| arm | r@50 | Δ vs jina r@10 [one-sided 95% lower] | first K reaching 0.633 | verdict |
|---|---|---|---|---|
| gte-small | 0.725 | +0.092 [+0.050] | 14 | **GO** |
| bge-small-en-v1.5 | 0.718 | +0.085 [+0.043] | 22 | **GO** |

The bound comes from a paired bootstrap over queries, stratified by
language, with 10,000 resamples and seed 398 (D5's method). It runs on
Python's RNG, so its values are not bit-identical to the Rust bootstrap.
The gate also holds in every language: small r@50 against jina r@10 is
go 0.79 vs 0.74, python 0.82/0.78 vs 0.69, rust 0.48/0.49 vs 0.34 and ts 0.81
vs 0.76.

## For S2: the ceiling

A reranker only reorders the candidate set, so its recall@10 (and any
recall@k) can be no higher than the set's recall@K. The table gives that
ceiling for NL queries:

| candidates | ceiling | vs jina r@10 | small r@10 today |
|---|---|---|---|
| gte top-20 | 0.657 | +0.025 | 0.580 |
| gte top-50 | 0.725 | +0.092 | 0.580 |
| gte top-100 | 0.807 | +0.175 | 0.580 |
| bge top-50 | 0.718 | +0.085 | 0.557 |
| bge top-100 | 0.792 | +0.160 | 0.557 |

Points for S2:

- **Clearing the gate says the ceiling is high enough, not that a reranker
  will reach it.** To match jina, a reranker over gte's top-50 must put the
  right answer in its top 10 for 0.633 / 0.725 ≈ 87% of the queries whose
  answer is in the set. Over top-100 the figure is ≈ 78%, but that means
  twice the reranker work per query.
- The candidate set loses some ground that jina has: 32 of 400 NL queries
  that jina ranks in its top 10 are outside gte's top 50. On the other
  side, 28 of 400 are outside all three arms' top 100, and no reranker can
  recover those.
- For comparison, jina's own top-50 ceiling is 0.810, so a reranker over
  jina would have more headroom. The trade this task studies is 0.725 vs
  0.810 ceiling at ~0.22x indexing cost.
- The union of both small models barely helps. Top-50 of each goes to 0.748
  from 0.725, with ~65 candidates and two indexes, so it is not worth
  pursuing.
- Rust is the weak spot. gte's rust top-50 reaches 0.48, against 0.58 for
  jina's rust top-50, and gte's rust ceiling at K=100 is only 0.61.

## Reproduce

```
python3 eval/embedding/recall_at_k.py --runs <main checkout>/eval/embedding/work/runs
```

This runs in about 5 s (`real 5.45`, `user 5.26`, load average 7.6). Most
of that is the bootstrap.
