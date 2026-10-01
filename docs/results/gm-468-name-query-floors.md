# GM-468 S1: name-query floors for find_definition's semantic rung

Question: `find_definition::by_semantic_neighbours` only ever embeds a
*symbol name*, but reads `similarity::floor`, which GM-423 / GM-465 fitted on
free-text queries for `search_code` (int8 model, structured doc text). At that
shared table, how often does the rung refuse a page that held the right answer,
and how often does it offer candidates for a name that exists nowhere? What
would a table fitted on name queries alone look like? Measure only: no
product change.

## Data: stored rankings, no re-embedding

- **Shipped arm:** `eval/embedding/work/runs-gm465/jina-v2-base-code-int8-structured`
  (GM-465, g-mesh 3.17.0 build, int8 `model_quantized.onnx`, structured text):
  the model and text 3.18.0 ships.
- **Reference arms:** `work/runs/jina-v2-base-code-fp32` (GM-398, the old
  model) and `work/runs/jina-v2-base-code-int8` (int8, untrimmed text).
- **Queries:** the mechanical `shape == "name"` arm of GM-398
  (`eval/embedding/queries/mechanical/*.jsonl`, seed 398): 150 positives per
  corpus (a declaration queried by its own name, every node of that name
  expected) and 150 absent negatives per corpus (a name from another corpus,
  proven absent from this one). Corpora: gin (go), py-requests (python),
  ripgrep + g-mesh (rust), excalidraw + task-tracker-mcp (typescript). The
  script checks every query file's sha256 against the run's manifest.

Floors read from `core/src/mcp/similarity.rs::floor` at `dfb6b4f`
(release-3.18.0): go 0.57, python 0.59, rust 0.57, typescript 0.53. The rung's
page size, `SEMANTIC_CANDIDATES`, is 3 (`find_definition.rs`).

## Definitions (the rung's cost matrix)

- **page positive:** a positive query whose top 3 holds an expected node, i.e.
  the page the rung would show holds the right answer.
- **false refusal** (expensive): a page positive whose top-1 score is below
  the floor, so the rung returns nothing and `find_definition` falls through to
  the terse "not found" that GM-234 existed to stop.
- **answer dropped:** a page positive where no expected hit reaches the floor
  (superset of false refusals; the rung may still offer other candidates).
- **hopeless offer** (cheap): an absent query whose top-1 reaches the floor,
  so the rung offers labelled "did you mean" candidates for a name that exists
  nowhere.
- **3% floor:** the largest floor with false refusals <= 3% (the find_definition
  comment's budget): with n page positives and k = floor(0.03 n), the (k+1)-th
  smallest top-1 score. "2dp down" rounds it down, as GM-381 did.

## Results: int8 + structured (the shipped model and text)

Page definition (top 3), pooled per language:

| language | page pos | absent | 3% floor (exact) | 2dp down | false refusal @2dp | hopeless offer @2dp |
|---|---|---|---|---|---|---|
| go | 131 | 150 | 0.571 | 0.57 | 2.3% | 12.0% |
| python | 127 | 150 | 0.5895 | 0.58 | 2.4% | 12.7% |
| rust | 209 | 300 | 0.573 | 0.57 | 2.9% | 25.7% |
| typescript | 267 | 300 | 0.563 | 0.56 | 3.0% | 16.7% |

(python's exact value is 0.5895, so it rounds down to 0.58.)

At the shared table:

| language | floor | false refusal (k/n, Wilson 95% upper) | answer dropped | hopeless offer |
|---|---|---|---|---|
| go | 0.57 | 2.3% (3/131, <= 6.5%) | 6.1% | 12.0% |
| python | 0.59 | 3.1% (4/127, <= 7.8%) | 5.5% | 11.3% |
| rust | 0.57 | **2.9% (6/209, <= 6.1%)** | 5.7% | 25.7% |
| typescript | 0.53 | 1.5% (4/267, <= 3.8%) | 1.9% | 23.7% |

**Rust at 0.57, per corpus**: ripgrep 0.9% (1/113, hopeless offer 22.7%),
g-mesh **5.2% (5/96**, hopeless offer 28.7%). The pooled 2.9% is the average of
a corpus well inside the budget and one outside it. On g-mesh four of the five
refusals sit in [0.55, 0.56): at 0.55 g-mesh falls to 1.0% (1/96) and pooled
Rust to 1.0% (2/209), for hopeless offers 25.7% -> 32% pooled.

The curve around each shared floor (false refusal / hopeless offer, pooled):

| floor | go | python | rust | typescript |
|---|---|---|---|---|
| 0.53 | 1.5% / 15% | 0.0% / 21% | 1.0% / 40% | 1.5% / 24% |
| 0.55 | 1.5% / 13% | 0.8% / 17% | 1.0% / 32% | 2.2% / 20% |
| 0.56 | 2.3% / 12% | 0.8% / 16% | 2.9% / 29% | 3.0% / 17% |
| 0.57 | 2.3% / 12% | 1.6% / 13% | 2.9% / 26% | 3.4% / 15% |
| 0.58 | 3.1% / 9% | 2.4% / 13% | 3.8% / 24% | 4.5% / 13% |
| 0.59 | 3.8% / 7% | 3.1% / 11% | 4.8% / 21% | 4.5% / 12% |
| 0.60 | 4.6% / 5% | 3.9% / 10% | 7.2% / 20% | 4.9% / 10% |

Strict definition (rank-1 hit expected) gives 3% floors 0.594 / 0.618 / 0.573 /
0.576 and false refusals at the shared table of 1.1% / 0.9% / 2.5% / 1.2%: the
page definition is the binding one, and it is the rung's (it shows 3 rows).

## Model and text moved the name-query floors little

Same queries, same definition (page, pooled), 3% floor exact:

| arm | go | python | rust | typescript |
|---|---|---|---|---|
| fp32, untrimmed (GM-398) | 0.575 | 0.598 | 0.559 | 0.556 |
| int8, untrimmed | 0.571 | 0.576 | 0.554 | 0.553 |
| int8, structured (shipped) | 0.571 | 0.590 | 0.573 | 0.563 |

The structured text raised Rust's name-query floor by about 0.02 (int8 0.554 ->
0.573), which is what pulled it up to the shared 0.57 rather than leaving it
below.

## The quoted fp32 calibration does not reproduce on this set

The comment above `by_semantic_neighbours` quotes 0.648 / 0.628 / 0.555 /
0.538 on 149 / 145 / 143 / 291 positives (GM-381, fp32). That query set is not
in the repository or anywhere under the workspace (searched), and GM-398's
name arm is not it: its positive counts cannot match (typescript has 279 name
positives in total, fewer than 291). Restricted to GM-381's corpora (rust =
ripgrep only), the stored fp32 run gives 0.575 / 0.598 / 0.587 / 0.556 on 131 /
119 / 105 / 265 page positives. So the quoted numbers could not be used as a
control, and the comparison above is within one query set across arms rather
than against GM-381. The comment's go/python numbers (0.648 / 0.628) are not
supported by this set, which puts both at about 0.57-0.60.

## Limits

- **Proxy positives.** The rung is only reached after exact-name lookup
  fails, so a real input is a near-miss name; these positives are exact names
  that exist. GM-381 used the same proxy.
- **One query is 0.4-1.0 points.** The Wilson upper bounds at the shared table
  are 3.8-7.8%; every difference between the shared table and a name-fitted
  table below is one to three queries.

## Recommendation: keep the shared table (owner decides)

Fitted on name queries for the shipped model and text, the table would be
0.57 / 0.58 / 0.57 / 0.56 against the shared 0.57 / 0.59 / 0.57 / 0.53:

- go and rust: identical.
- python: 0.01 lower, one query (3.1% -> 2.4%).
- typescript: a name-fitted floor would be *higher*; the shared 0.53 refuses
  less (1.5% vs 3.0%) at 7 more points of hopeless offers, the direction the
  cost matrix asks for.

A separate table buys at most one python query, inside the noise, at the cost
of a second calibration to keep in step with every model or text change. The
one real exposure is Rust on the g-mesh corpus (5.2% at 0.57, 1.0% at 0.55).
If the owner wants margin there, the targeted option is a find_definition-only
Rust floor of 0.55 (pooled false refusal 2.9% -> 1.0%, hopeless offers 26% ->
32%), which does need its own table; it is not needed to meet the 3% budget
pooled. Either way S2 should replace the comment's unreproducible fp32 numbers
with these.

## Reproducing

```sh
# from the repository root; W is the main checkout's eval/embedding/work
python3 eval/embedding/gm468_name_floors.py --run $W/runs-gm465/jina-v2-base-code-int8-structured [--strict] [--corpora g-mesh] [--json out.json]
python3 eval/embedding/gm468_name_floors.py --run $W/runs/jina-v2-base-code-fp32 --corpora gin py-requests ripgrep excalidraw task-tracker-mcp
```

Reads stored rankings only; runs in under a second. `--json` adds the full
curve (0.45-0.70, every 0.01) per language.
