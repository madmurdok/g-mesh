#!/usr/bin/env bash
# Builds one corpus's snapshot for the embedding eval (D4 step 1 of
# docs/architecture/embedding-eval.md): indexes the pinned checkout with this
# workspace's build and the embedding model switched off, in an isolated
# G_MESH_HOME, then hands the index to `g-mesh debug-embed-eval snapshot`,
# which checks the revision, copies it to work/<corpus>.sqlite and records
# its sha256.
#
# Usage: eval/embedding/make_snapshot.sh <corpus> [g-mesh binary]
# Run from the repository root. MODEL=on keeps the model (for the parity
# corpus, whose production vectors `debug-embed-eval parity` compares with).
set -euo pipefail

corpus="$1"
bin="${2:-target/release/g-mesh}"
eval_dir="eval/embedding"
checkout="$eval_dir/work/corpora/$corpus"
home="$(cd "$eval_dir" && pwd)/work/home/$corpus"

[ -d "$checkout/.git" ] || { echo "no checkout at $checkout" >&2; exit 1; }
rm -rf "$home"
mkdir -p "$home"

if [ "${MODEL:-off}" = "on" ]; then
  model_env=()
else
  mkdir -p "$home/no-model"
  model_env=(G_MESH_MODEL_DIR="$home/no-model")
fi

abs_bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
(cd "$checkout" && env G_MESH_HOME="$home" ${model_env[@]+"${model_env[@]}"} "$abs_bin" reindex)

db=""
for dir in "$home"/projects/*/; do
  if [ -f "$dir/project.root" ] && [ -f "$dir/index.db" ]; then db="$dir/index.db"; fi
done
if [ -z "$db" ]; then
  db="$(find "$home/projects" -mindepth 2 -maxdepth 2 -name index.db -print -quit)"
fi
[ -f "$db" ] || { echo "no index.db under $home/projects" >&2; exit 1; }

"$abs_bin" debug-embed-eval snapshot --eval-dir "$eval_dir" --corpus "$corpus" --index-db "$db"
