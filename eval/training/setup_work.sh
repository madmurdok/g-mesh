#!/usr/bin/env bash
# Prepares eval/training as a `debug-embed-eval --eval-dir` for the GM-456
# training data (docs/design/GM-456-training-data.md, sections 5 and 7):
# - eval/training/work is a symlink to $GM456_DATA/work, outside git;
# - work/models is a symlink to the stored eval work dir's models
#   (jina int8, MiniLM tokenizer);
# - each named repo is cloned at its pin (blobless), HEAD is checked against
#   `revision`, its licence files' sha256 go to work/licences/<repo>.tsv, and
#   it is indexed with the model off and snapshotted by
#   eval/embedding/make_snapshot.sh (EVAL_DIR=eval/training).
#
# usage: eval/training/setup_work.sh [--bin <g-mesh>] [--eval-work <dir>] <repo id>...
#   (no repo ids: every repo of corpora.toml). Run from the repository root.
set -euo pipefail

bin="target/release/g-mesh"
eval_work=""
repos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --bin) bin="$2"; shift 2 ;;
    --eval-work) eval_work="$2"; shift 2 ;;
    *) repos+=("$1"); shift ;;
  esac
done

here="eval/training"
data="${GM456_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm456}"
if [ -z "$eval_work" ]; then
  # The main checkout's stored eval work dir (a worktree has none).
  main="$(git worktree list --porcelain | sed -n '1s/^worktree //p')"
  eval_work="$main/eval/embedding/work"
fi
[ -d "$eval_work/models/jina-v2-base-code-int8" ] || { echo "no jina int8 model under $eval_work/models" >&2; exit 1; }

mkdir -p "$data/work/corpora" "$data/work/licences"
if [ -L "$here/work" ]; then
  [ "$(readlink "$here/work")" = "$data/work" ] || { echo "$here/work points elsewhere: $(readlink "$here/work")" >&2; exit 1; }
elif [ -e "$here/work" ]; then
  echo "$here/work exists and is not a symlink" >&2; exit 1
else
  ln -s "$data/work" "$here/work"
fi
ln -sfn "$eval_work/models" "$data/work/models"

field() {  # field <repo> <key>: one key of a [[corpus]] block of corpora.toml
  awk -v id="$1" -v key="$2" '
    /^\[\[corpus\]\]/ { cur = "" }
    $1 == "id" { gsub(/"/, "", $3); cur = $3 }
    cur == id && $1 == key { v = $0; sub(/^[^=]*= *"/, "", v); sub(/" *$/, "", v); print v; exit }
  ' "$here/corpora.toml"
}

if [ ${#repos[@]} -eq 0 ]; then
  while read -r r; do repos+=("$r"); done < <(awk '$1 == "id" { gsub(/"/, "", $3); print $3 }' "$here/corpora.toml")
fi

for repo in "${repos[@]}"; do
  url="$(field "$repo" url)"
  rev="$(field "$repo" revision)"
  [ -n "$url" ] && [ -n "$rev" ] || { echo "no corpus $repo in corpora.toml" >&2; exit 1; }
  dir="$data/work/corpora/$repo"
  if [ ! -d "$dir/.git" ]; then
    git clone --quiet --filter=blob:none --no-checkout "$url" "$dir"
  fi
  git -C "$dir" checkout --quiet --detach "$rev"
  head="$(git -C "$dir" rev-parse HEAD)"
  [ "$head" = "$rev" ] || { echo "$repo: HEAD $head, pinned $rev" >&2; exit 1; }

  lic="$data/work/licences/$repo.tsv"
  : > "$lic"
  for f in "$dir"/LICEN[CS]E* "$dir"/COPYING* "$dir"/licen[cs]e* ; do
    [ -f "$f" ] || continue
    printf '%s\t%s\n' "$(basename "$f")" "$(shasum -a 256 "$f" | cut -d' ' -f1)" >> "$lic"
  done
  [ -s "$lic" ] || { echo "$repo: no licence file at $rev" >&2; exit 1; }

  EVAL_DIR="$here" eval/embedding/make_snapshot.sh "$repo" "$bin"
  embeddable="$(sed -n 's/^ *"embeddable": *\([0-9]*\).*/\1/p' "$data/work/$repo.snapshot.json")"
  [ "${embeddable:-0}" -gt 0 ] || { echo "$repo: the snapshot has no embeddable node (a language plugin missing?)" >&2; exit 1; }
done
echo "training work dir ready: $data/work"
