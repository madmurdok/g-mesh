# Embedding search-quality eval

The instrument specified by `docs/architecture/embedding-eval.md` (GM-398).
The engine is the hidden `g-mesh debug-embed-eval` command
(`core/src/cli/embed_eval.rs`); this directory holds its inputs.

| path | what |
|---|---|
| `corpora.toml` | the six corpora and their pinned revisions (D1) |
| `variants.toml` | reference, candidates and broken arms (D4, D7, D8) |
| `queries/<corpus>.jsonl` | authored queries, the frozen ground truth (D2, D3) |
| `queries/mechanical/<corpus>.jsonl` | name queries for the floor fit set (D6), from `export_mechanical.py` |
| `targets/<corpus>.jsonl` | the seeded target sample authors worked through, in order (D3 step 1) |
| `targets/<corpus>.skips.jsonl` | targets skipped, each with its reason |
| `sample_targets.py` | writes `targets/` |
| `check_queries.py` | authoring-time format and overlap check |
| `export_mechanical.py` | writes `queries/mechanical/` from the snapshots |
| `make_snapshot.sh` | indexes a pinned checkout with the model off and records the snapshot |
| `fetch_models.py` | downloads candidate models at their pinned revisions |
| `measure_costs.sh` | one script: candidate quality runs, query latency, D11 timed passes, report (GM-398 S14) |
| `measure_summary.py` | writes `costs.toml` and the summary tables for `measure_costs.sh` |
| `work/` | git-ignored: checkouts, snapshots, models, runs |

## Target sampling frame

D3 samples targets from the snapshot's embeddable nodes. The query set was
authored before any snapshot could be built (no indexing was allowed while
another measurement ran on the machine), so `targets/` were sampled from
existing g-mesh indexes of the same corpora (recorded in each file's first
line with their sha256), keeping only nodes whose file and name exist at the
pinned checkout. Sampling is still seeded and kind-stratified and blind to
models. The harness resolves every expected symbol against the real snapshot
and fails on any that does not resolve, so a frame/snapshot mismatch cannot
reach a score; fixes it forces happen before the freeze.

## Running

From the repository root, with a release build:

```sh
# checkouts at the pinned revisions go to eval/embedding/work/corpora/<id>
eval/embedding/make_snapshot.sh <corpus>            # once per corpus
MODEL=on eval/embedding/make_snapshot.sh task-tracker-mcp   # parity corpus
python3 eval/embedding/export_mechanical.py
python3 eval/embedding/fetch_models.py
g-mesh debug-embed-eval run --variant jina-v2-base-code-fp32
g-mesh debug-embed-eval run --variant random      # and shuffled, words-shuffled, bm25, candidates
g-mesh debug-embed-eval parity --index-db <task-tracker-mcp index.db> \
    --run eval/embedding/work/runs/jina-v2-base-code-fp32
g-mesh debug-embed-eval report eval/embedding/work/runs/* [--costs costs.toml] --json report.json
```

Timing (D11): `G_MESH_EMBEDDING_CACHE=off /usr/bin/time -lp g-mesh
debug-embed-eval run --variant <v> --corpus g-mesh --embed-only --force`.
