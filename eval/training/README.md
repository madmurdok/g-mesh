# GM-456 training data

Builds (natural-language query, code, hard negatives) rows from pinned public
repos that no eval set uses, for GM-436 and GM-452. The design is
[`docs/design/GM-456-training-data.md`](../../docs/design/GM-456-training-data.md).

## Layout

`eval/training/` is laid out as a `debug-embed-eval --eval-dir`:

| path | what |
|---|---|
| `corpora.toml` | The source repos and their pins, plus `licence`, `split`, `url` |
| `variants.toml` | The mining arms: shipped jina int8 on the structured text, and BM25 |
| `prompts/` | The generator and judge prompts (their sha256 go in the manifest) |
| `work` | Symlink to `$GM456_DATA/work`, made by `setup_work.sh` (gitignored) |

Data lives outside git, in `$GM456_DATA` (default
`~/Projects/ClaudeProjects/g-mesh-data/gm456/`):

| path | what |
|---|---|
| `work/corpora/<repo>` | Blobless clones at their pins |
| `work/<repo>.sqlite`, `.snapshot.json` | Index snapshots (model off) |
| `work/licences/<repo>.tsv` | Licence file names and sha256 at the pin |
| `work/runs/<variant>/<repo>/` | `debug-embed-eval run` dirs (node vectors reused across versions) |
| `work/qvec-cache/` | The guard's query vectors, by text sha256 |
| `v<major>.<minor>[-tag]/` | One dataset version (below) |

A version folder holds `nodes/<repo>.jsonl` (raw fields), `targets.jsonl`,
`texts/<renderId>/`, `queries.gen.jsonl`, `mine/`, `queries.mined.jsonl`,
`responses/` and `judge/` (batch requests and raw results),
`queries.judged.jsonl`, and the export: `queries.jsonl`, `guard.json`,
`leaks.jsonl`, `sample.md`, `card.md`, `manifest.json`, `summary.txt`.

Major versions change targets or generation; minor versions change mining,
judging, rendering or the guard.

## Commands

Run from the repository root. Build first:
`cargo build --release -p g-mesh -p g-mesh-plugin-rust` (plus the other
language plugins for Go, Python and TypeScript repos).

```
# The S2 pipeline check: mini-redis, 50 targets, stub queries and verdicts.
bash eval/training/run_pipeline.sh --version v0.0-stub --stub --count 50 mini-redis

# Stages one by one
bash eval/training/setup_work.sh mini-redis
python3 eval/training/nodes.py    --version v0.0-stub --repo mini-redis
python3 eval/training/sample.py   --version v0.0-stub --count 50
python3 eval/training/render.py   --version v0.0-stub
python3 eval/training/generate.py --version v0.0-stub --stub        # or --dry-run / --submit / --collect
python3 eval/training/mine.py     --version v0.0-stub --threads 2
python3 eval/training/judge.py    --version v0.0-stub --dry-run --stub-verdict
python3 eval/training/export.py   --version v0.0-stub              # runs leak_guard --check last

# The leak guard alone, and its pooling sanity check against a Rust run
python3 eval/training/leak_guard.py --version v0.0-stub --check
python3 eval/training/leak_guard.py --pooling-check <eval work>/runs-gm465/jina-v2-base-code-int8-structured/ripgrep
```

`run_pipeline.sh` runs every stage under `taskpolicy -b`, fails fast, and
writes `<version>/summary.txt`. Without `--stub` it stops after each
`--dry-run` so the batches can be submitted and collected by hand; resume with
`--from mine` or `--from export`.
