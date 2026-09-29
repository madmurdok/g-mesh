#!/usr/bin/env bash
# Target lists of the GM-422 confirmatory int8 study
# (docs/architecture/embedding-eval-int8-confirm.md, section 3 steps 1-3).
# Samples over the D4 snapshots GM-398 embedded, stops on a snapshot sha256
# other than the protocol's, and excludes GM-398's consumed targets and
# expected symbols. Re-running must give byte-identical files.
#
# usage: eval/embedding/confirm/sample.sh [work dir]   (default eval/embedding/work)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/../../.."   # repo root: meta records the excluded study as eval/embedding
work="${1:-eval/embedding/work}"

# corpus language count snapshot-sha256
while read -r corpus language count sha; do
  python3 eval/embedding/sample_targets.py \
    --corpus "$corpus" --language "$language" \
    --index-db "$work/$corpus.sqlite" --checkout "$work/corpora/$corpus" \
    --expect-sha256 "$sha" --seed 4222 --count "$count" \
    --exclude eval/embedding --out "eval/embedding/confirm/targets/$corpus.jsonl"
done <<'LIST'
gin go 400 1e013b7630a1f154dbe946ca243ee881d5dfe0bea9861816a100c74b4e068c62
py-requests python 298 d24c70266c36a27af0d923c8fc6ab65d37cdad3c0a71d8c7c7c38de3e8afe7fd
ripgrep rust 200 d00f8a256d17c0cda5fe81c1a0c744a697d8c2cce9d5a6075e11f9ff3954aca0
g-mesh rust 150 1fd9a1563cd55afa2eb9be061735dc56394027ad061bfac64e2ac0c65d03fd02
excalidraw typescript 180 3c7bcb4601c64f93bf8b6b043f8140f8b0f9781a6e292e0b236fdfd2102dca4d
task-tracker-mcp typescript 80 6e2381ba9123712a3114c5a6127fe4f29e0a27e372147ac392c8ad2ce74041ff
LIST
