#!/usr/bin/env bash
# Prepares eval/embedding/v2 as a `debug-embed-eval --eval-dir` for the GM-460
# enlarged-eval measure (docs/design/GM-460-enlarged-nl-eval.md, section 7).
# Same scheme as confirm/setup_work.sh. v2/work/ is local and gitignored:
# - snapshots, pinned checkouts and models are symlinks into the stored work
#   dir, so the harness verifies the same snapshot sha256s;
# - each v2 run dir is seeded with the stored run's manifest.json and
#   vectors.bin, so the harness reuses the node vectors (same variant
#   fingerprint, snapshot, node ids, dimension) and embeds only the queries
#   the stored run lacks. Control C0 checks the vectors.bin sha256 afterwards.
#
# Arms and where each is seeded from:
#   jina-v2-base-code-fp32, -int8, random, shuffled   <src>/runs
#   jina-v2-base-code-int8-structured                 <src>/runs-gm465
#   gte-small                                         <src>/runs (rerank_eval.py's Corpus
#                                                     loader reads it for the F4 step)
#
# usage: v2/setup_work.sh <stored work dir>   e.g. <main checkout>/eval/embedding/work
set -euo pipefail
src="$(cd "${1:?stored work dir}" && pwd)"
here="$(cd "$(dirname "$0")" && pwd)"
work="$here/work"
corpora="gin py-requests ripgrep g-mesh excalidraw task-tracker-mcp"
arms="jina-v2-base-code-fp32 jina-v2-base-code-int8 jina-v2-base-code-int8-structured random shuffled gte-small"

mkdir -p "$work/runs"
for c in $corpora; do
  ln -sfn "$src/$c.sqlite" "$work/$c.sqlite"
  ln -sfn "$src/$c.snapshot.json" "$work/$c.snapshot.json"
done
ln -sfn "$src/corpora" "$work/corpora"
ln -sfn "$src/models" "$work/models"

for a in $arms; do
  case "$a" in
    *-structured) from="$src/runs-gm465" ;;
    *) from="$src/runs" ;;
  esac
  for c in $corpora; do
    d="$work/runs/$a/$c"
    if [ -e "$d/rankings.jsonl" ]; then
      echo "STOP: $d already holds rankings; not re-seeding" >&2
      exit 1
    fi
    mkdir -p "$d"
    cp "$from/$a/$c/manifest.json" "$from/$a/$c/vectors.bin" "$d/"
  done
done
echo "v2 work dir ready: $work"
