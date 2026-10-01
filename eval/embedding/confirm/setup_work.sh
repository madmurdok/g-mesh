#!/usr/bin/env bash
# Prepares eval/embedding/confirm as a `debug-embed-eval --eval-dir` for the
# GM-422 confirmatory study (docs/architecture/embedding-eval-int8-confirm.md,
# Runbook). confirm/work/ is local and gitignored:
# - the D4 snapshots, pinned checkouts and models are symlinks into the
#   GM-398 work dir, so the harness verifies the same snapshot sha256s;
# - each confirm run dir is seeded with the GM-398 run's manifest.json and
#   vectors.bin, which is what makes the harness reuse the node vectors
#   (same variant fingerprint, snapshot, node ids and dimension) and embed
#   only the new queries. Control 2 of the report checks the reuse happened.
#
# usage: setup_work.sh <GM-398 work dir>   e.g. <main checkout>/eval/embedding/work
set -euo pipefail
src="$(cd "${1:?GM-398 work dir}" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
work="$here/work"
corpora="gin py-requests ripgrep g-mesh excalidraw task-tracker-mcp"
arms="jina-v2-base-code-fp32 jina-v2-base-code-int8 random shuffled"

mkdir -p "$work/runs"
for c in $corpora; do
  ln -sfn "$src/$c.sqlite" "$work/$c.sqlite"
  ln -sfn "$src/$c.snapshot.json" "$work/$c.snapshot.json"
done
ln -sfn "$src/corpora" "$work/corpora"
ln -sfn "$src/models" "$work/models"

for a in $arms; do
  for c in $corpora; do
    d="$work/runs/$a/$c"
    if [ -e "$d/rankings.jsonl" ]; then
      echo "STOP: $d already holds rankings; not re-seeding" >&2
      exit 1
    fi
    mkdir -p "$d"
    cp "$src/runs/$a/$c/manifest.json" "$src/runs/$a/$c/vectors.bin" "$d/"
  done
done
echo "confirm work dir ready: $work"
