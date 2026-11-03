#!/bin/bash
# GM-692: GM-690 fresh-function repro with the cross-encoder installed.
# usage: run.sh dry|full   (dry = build + prep + ONE edit; full = the whole set, reusing the prepared home/index)
# env: WT (g-mesh worktree), SCR (scratch dir for P/out/logs). Fail-fast.
set -euo pipefail
MODE=$1
WT=${WT:-/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm692}
SCR=${SCR:?scratch dir}
HERE=$WT/eval/gm692
MAIN=/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh
H=/tmp/gm692h   # short: unix socket path limit 103 bytes
OUT=$SCR/out-$MODE
PROG=$SCR/progress.log
RERANK=$H/rerank/ms-marco-MiniLM-L6-v2
mkdir -p "$OUT" "$H"
: > "$OUT/pids.txt"
log() { echo "[$(date +%T)] $*" >> "$OUT/run.log"; }
cleanup() {
  while read -r pid; do [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true; done < "$OUT/pids.txt"
}
trap cleanup EXIT
log "run.sh $MODE start"; uptime >> "$OUT/run.log"
# 1. target seed + release build (once)
if [ ! -x "$WT/target/release/g-mesh" ]; then
  if [ ! -d "$WT/target" ]; then
    test -z "$(pgrep -fl -- '[-]-out-dir '"$MAIN"'/target' || true)" || { log "main target busy"; exit 1; }
    cp -c -R "$MAIN/target" "$WT/target"
  fi
  test -z "$(pgrep -fl -- '[-]-out-dir '"$WT"'/target' || true)" || { log "wt target busy"; exit 1; }
  ( cd "$WT" && /usr/bin/time -p cargo build --release --workspace ) >> "$OUT/build.log" 2>&1
  grep -E '^(real|user|sys)' "$OUT/build.log" >> "$OUT/run.log"
fi
BIN=$WT/target/release/g-mesh
{
  "$BIN" --version 2>&1
  git -C "$WT" rev-parse HEAD
  # 2. LSP plugin deps
  (cd "$WT/plugins/typescript" && npm ci --no-audit --no-fund) 2>&1
  (cd "$WT/plugins/python" && if [ -f package.json ]; then npm ci --no-audit --no-fund; fi) 2>&1
} >> "$OUT/run.log"
# 3. project P: detached worktree of db1074a (GM-690's project), clean
P=$SCR/P
if [ ! -d "$P" ]; then git -C "$WT" worktree add --detach "$P" db1074a >> "$OUT/run.log" 2>&1; fi
git -C "$P" checkout -- .
test -z "$(git -C "$P" status --short)"
git -C "$P" rev-parse HEAD >> "$OUT/run.log"
# 4. home: embedding model symlinked, embedding cache copied, rerank model fetched (once)
if [ ! -d "$H/embedding-cache" ]; then
  mkdir -p "$H/embedding-cache" "$H/models"
  ln -s "$HOME/.g-mesh/models/jina-embeddings-v2-base-code" "$H/models/jina-embeddings-v2-base-code"
  sqlite3 "$HOME/.g-mesh/embedding-cache/cache.sqlite" ".backup $H/embedding-cache/cache.sqlite"
fi
if [ ! -f "$RERANK/model.onnx" ]; then
  G_MESH_HOME=$H G_MESH_RERANK_MODEL_DIR=$RERANK "$BIN" model fetch --dir "$H/models/jina-embeddings-v2-base-code" >> "$OUT/fetch.log" 2>&1
fi
G_MESH_HOME=$H G_MESH_RERANK_MODEL_DIR=$RERANK "$BIN" model status --dir "$H/models/jina-embeddings-v2-base-code" >> "$OUT/run.log" 2>&1
shasum -a 256 "$RERANK/model.onnx" "$RERANK/tokenizer.json" >> "$OUT/run.log"
# 5. the run
mkdir -p "$H/old-logs"; for f in "$H"/daemon-*.log; do [ -e "$f" ] && mv "$f" "$H/old-logs/$(date +%s)-$(basename "$f")"; done || true
ARGS=()
if [ "$MODE" = dry ]; then ARGS=(--only-reps 1); fi
{
  uptime
  /usr/bin/time -p python3 "$HERE/repro.py" --bin "$BIN" --home "$H" --work "$SCR" --out "$OUT" \
    --progress "$PROG" --rerank-dir "$RERANK" ${ARGS[@]+"${ARGS[@]}"} 2>&1
  uptime
} >> "$OUT/run.log"
python3 "$HERE/summarize.py" "$OUT/raw.jsonl" "$H"/daemon-*.log > "$OUT/summary.md"
while read -r pid; do if kill -0 "$pid" 2>/dev/null; then echo "LEFT daemon $pid" >> "$OUT/summary.md"; fi; done < "$OUT/pids.txt"
log "run.sh $MODE end"
echo DONE
