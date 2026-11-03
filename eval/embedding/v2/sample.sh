#!/usr/bin/env bash
# Seed-460 target lists of the GM-460 v2 top-up
# (docs/design/GM-460-enlarged-nl-eval.md, section 5 step 2).
# Samples over the D4 snapshots GM-398 embedded, stops on a snapshot sha256
# other than the protocol's, and excludes every target GM-398 and the GM-422
# confirm study consumed and every symbol in either study's expected sets.
# Count = stage-2 positives x 1.3, rounded up. Re-running must give
# byte-identical files.
#
# usage: eval/embedding/v2/sample.sh [work dir]   (default eval/embedding/work)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/../../.."   # repo root: meta records the excluded studies as eval/embedding[/confirm]
work="${1:-eval/embedding/work}"

# corpus language count snapshot-sha256
while read -r corpus language count sha; do
  python3 eval/embedding/sample_targets.py \
    --corpus "$corpus" --language "$language" \
    --index-db "$work/$corpus.sqlite" --checkout "$work/corpora/$corpus" \
    --expect-sha256 "$sha" --seed 460 --count "$count" \
    --exclude eval/embedding --exclude eval/embedding/confirm \
    --out "eval/embedding/v2/new/targets/$corpus.jsonl"
done <<'LIST'
gin go 39 1e013b7630a1f154dbe946ca243ee881d5dfe0bea9861816a100c74b4e068c62
py-requests python 104 d24c70266c36a27af0d923c8fc6ab65d37cdad3c0a71d8c7c7c38de3e8afe7fd
ripgrep rust 52 d00f8a256d17c0cda5fe81c1a0c744a697d8c2cce9d5a6075e11f9ff3954aca0
g-mesh rust 52 1fd9a1563cd55afa2eb9be061735dc56394027ad061bfac64e2ac0c65d03fd02
excalidraw typescript 85 3c7bcb4601c64f93bf8b6b043f8140f8b0f9781a6e292e0b236fdfd2102dca4d
task-tracker-mcp typescript 20 6e2381ba9123712a3114c5a6127fe4f29e0a27e372147ac392c8ad2ce74041ff
LIST
