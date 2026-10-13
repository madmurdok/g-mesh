#!/bin/bash
# Runs every GM-519 fixture through `g-mesh plugins check` with its semantic
# tier on PATH, and writes one summary line per expectation.
# Usage: run-all.sh <g-mesh binary> <g-mesh checkout holding node_modules + the Go plugin build> <out dir>
set -u
GM=${1:?g-mesh binary}; MAIN=${2:?main checkout}; OUT=${3:?out dir}
HERE=$(cd "$(dirname "$0")" && pwd); WT=$(cd "$HERE/../../.." && pwd)
mkdir -p "$OUT"; : > "$OUT/summary.txt"
export PATH="$MAIN/plugins/typescript/node_modules/.bin:$MAIN/plugins/python/node_modules/.bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
for dir in "$HERE"/*/*/; do
  case=$(basename "$dir"); lang=$(basename "$(dirname "$dir")")
  plugin="$WT/plugins/$lang"; [ "$lang" = go ] && plugin="$MAIN/plugins/go"
  log="$OUT/$lang-$case.log"
  perl -e 'alarm 600; exec @ARGV' "$GM" plugins check "$plugin" --fixture "$dir/project" --expect "$dir/expect.toml" > "$log" 2>&1
  tier=$(grep -m1 "semantic tier" "$log" | sed 's/.*semantic tier: //')
  echo "## $lang/$case  [${tier:-no semantic tier line}]" >> "$OUT/summary.txt"
  sed -n '/^expectations:/,/^result:/p' "$log" | grep -v '^expectations:' >> "$OUT/summary.txt"
done
echo DONE >> "$OUT/summary.txt"
