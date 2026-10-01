#!/usr/bin/env bash
# GM-465 (docs/results/gm-465-structured-trim.md): the "structured" doc trim on
# the shipped int8 model, against the int8 1024 baseline and GM-423's
# first-paragraph. Adapted from gm423_measure.sh.
#   1. quality: structured on every corpus except the timed one (full mode)
#   2. timing on g-mesh: round 1 (full mode, gives query latency) int8,
#      first-paragraph, structured; round 2 (embed only) in the reverse order.
#      structured's round-1 g-mesh pass is also its quality run for g-mesh.
#   3. controls: int8 / first-paragraph round-1 re-runs vs their stored runs,
#      a second structured pass on task-tracker-mcp (reproducible vectors),
#      round-2 vectors vs round-1 vectors
#   4. reports vs the stored int8 run and vs fp32, GM-434 columns, token table,
#      summary.
# Env: OUT (default eval/embedding/work/runs-gm465), SKIP_RUNS=1 to only report,
# CORPORA / R (timed corpus) / REPRO (repro corpus) to shrink the set (dry runs).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.." || exit
BIN=$PWD/target/release/g-mesh
EVAL=eval/embedding
OUT=${OUT:-$PWD/$EVAL/work/runs-gm465}
RUNS=$PWD/$EVAL/work/runs
GM423=$PWD/$EVAL/work/runs-gm423
INT8=jina-v2-base-code-int8
FP=jina-v2-base-code-int8-first-paragraph
ST=jina-v2-base-code-int8-structured
read -r -a CORPORA <<< "${CORPORA:-task-tracker-mcp gin py-requests excalidraw ripgrep}"
R=${R:-g-mesh}                       # the timed corpus
REPRO=${REPRO:-task-tracker-mcp}     # the reproducibility corpus
export G_MESH_EMBEDDING_CACHE=off
mkdir -p "$OUT/logs"
LOG=$OUT/log.txt
TSV=$OUT/timing.tsv
START=$(date +%s)
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }

# stored per-corpus embed seconds of the int8 run, as the ETA weight
weight() { python3 -c "import json;print(json.load(open('$RUNS/$INT8/$1/timings.json'))['embedNodesMs']/1000)"; }
TOTAL_W=0; for c in "${CORPORA[@]}" "$REPRO"; do TOTAL_W=$(echo "$TOTAL_W + $(weight "$c")" | bc -l); done
TOTAL_W=$(echo "$TOTAL_W + 6 * $(weight "$R")" | bc -l)
DONE_W=0

# Before each timed invocation: wait (checking every 60 s, logged) until the
# 1-minute load is below QUIET_LOAD and no jamf / cargo / rustc process runs.
# GM-465's first attempt overlapped a corporate `jamf policy` run (load 600+).
QUIET_LOAD=${QUIET_LOAD:-5}
quiet() {
  local waited=0
  while :; do
    local l busy
    l=$(load1); busy=$(pgrep -l 'jamf|^cargo$|^rustc$' | tr '\n' ' ')
    if [ -z "$busy" ] && [ "$(echo "$l < $QUIET_LOAD" | bc -l)" = 1 ]; then break; fi
    [ $((waited % 600)) = 0 ] && log "waiting for a quiet machine: load $l busy [$busy] waited ${waited}s"
    sleep 60; waited=$((waited + 60))
  done
  log "quiet: load $(load1) after ${waited}s"
}

one() { # round corpus arm mode(full|embed-only) outdir
  local round=$1 c=$2 v=$3 mode=$4 out=$5 extra=""
  [ "$mode" = embed-only ] && extra="--embed-only"
  local tag="$round-$c-$v"
  local before; before=$(uptime | sed 's/.*load averages*: //')
  log "start round $round $c $v ($mode) load $before"
  # stdout to its own file; stderr (the harness's phase progress and time -lp) to the .err log
  # shellcheck disable=SC2086
  /usr/bin/time -lp "$BIN" debug-embed-eval run --eval-dir "$EVAL" --variant "$v" --corpus "$c" \
    --out "$out" --force $extra > "$OUT/logs/$tag.out" 2> "$OUT/logs/$tag.err"
  local rc=$?
  local after; after=$(uptime | sed 's/.*load averages*: //')
  local real user sys rss embed qmed
  real=$(awk '/^real/{print $2}' "$OUT/logs/$tag.err"); user=$(awk '/^user/{print $2}' "$OUT/logs/$tag.err")
  sys=$(awk '/^sys/{print $2}' "$OUT/logs/$tag.err"); rss=$(awk '/maximum resident set size/{print $1}' "$OUT/logs/$tag.err")
  read -r embed qmed < <(python3 -c "import json;d=json.load(open('$out/$v/$c/timings.json'));print(d['embedNodesMs']/1000, d.get('queryEmbedMsMedian'))" 2>/dev/null || echo "nan None")
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$round" "$c" "$v" "$rc" "$embed" "$real" "$user" "$sys" "$rss" "$qmed" "$before" "$after" >> "$TSV"
  DONE_W=$(echo "$DONE_W + $(weight "$c")" | bc -l)
  local el=$(( $(date +%s) - START ))
  local pct; pct=$(echo "100 * $DONE_W / $TOTAL_W" | bc -l)
  local eta; eta=$(echo "$el * (100 - $pct) / ($pct + 0.001)" | bc -l)
  log "$(printf 'done  round %s %s %s rc=%s embed=%ss real=%s user=%s | %.0f%% elapsed %ds ETA %.0fs load %s' "$round" "$c" "$v" "$rc" "$embed" "$real" "$user" "$pct" "$el" "$eta" "$(load1)")"
}

if [ "${SKIP_RUNS:-0}" != 1 ]; then
  # SKIP_QUALITY=1: keep the finished quality/repro passes, move the old
  # timing.tsv aside (timing-attempt<N>.tsv) and redo only the timed rounds.
  if [ "${SKIP_QUALITY:-0}" = 1 ] && [ -f "$TSV" ]; then
    n=1; while [ -e "$OUT/timing-attempt$n.tsv" ]; do n=$((n + 1)); done
    mv "$TSV" "$OUT/timing-attempt$n.tsv"; log "timing.tsv moved to timing-attempt$n.tsv"
  fi
  printf 'round\tcorpus\tarm\trc\tembed_s\treal\tuser\tsys\tmaxrss\tquery_ms_median\tload_before\tload_after\n' > "$TSV"
  log "start; daemons: $(pgrep -fl 'g-mesh daemon' | tr '\n' ';') uptime: $(uptime)"
  if [ "${SKIP_QUALITY:-0}" != 1 ]; then
    for c in "${CORPORA[@]}"; do one q "$c" "$ST" full "$OUT"; done
    one repro "$REPRO" "$ST" full "$OUT/repro"
  else
    DONE_W=$(echo "$TOTAL_W - 6 * $(weight "$R")" | bc -l)
  fi
  for v in "$INT8" "$FP" "$ST"; do quiet; one 1 "$R" "$v" full "$OUT"; done
  for v in "$ST" "$FP" "$INT8"; do quiet; one 2 "$R" "$v" embed-only "$OUT/round2"; done
  log "runs done; uptime: $(uptime)"
fi

{
  echo "## int8 round-1 re-run vs stored int8 (GM-398)"
  python3 "$EVAL/gm423_summary.py" control "$RUNS/$INT8" "$OUT/$INT8"
  echo "## first-paragraph round-1 re-run vs stored GM-423 run"
  python3 "$EVAL/gm423_summary.py" control "$GM423/$FP" "$OUT/$FP"
  echo "## structured: second pass on $REPRO vs the first"
  python3 "$EVAL/gm423_summary.py" control "$OUT/$ST" "$OUT/repro/$ST"
  echo "## round-2 vectors vs round-1 vectors ($R)"
  for v in "$INT8" "$FP" "$ST"; do python3 "$EVAL/gm465_summary.py" vectors "$OUT/$v/$R/vectors.bin" "$OUT/round2/$v/$R/vectors.bin"; done
} > "$OUT/control.txt" 2>&1

COST_ROUNDS=${COST_ROUNDS:-1,2} python3 "$EVAL/gm465_summary.py" costs "$TSV" "$RUNS/$INT8/$R/manifest.json" "$R" > "$OUT/costs.toml"

# D9 against the int8 baseline (stored run), and against fp32; first-paragraph
# is GM-423's stored quality run (its round-1 re-run here is the control).
CANDS=("$GM423/$FP" "$OUT/$ST")
"$BIN" debug-embed-eval report --eval-dir "$EVAL" --reference "$INT8" --costs "$OUT/costs.toml" \
  --json "$OUT/report-vs-int8.json" "$RUNS/$INT8" "$RUNS/jina-v2-base-code-fp32" \
  "$RUNS/random" "$RUNS/shuffled" "${CANDS[@]}" > "$OUT/report-vs-int8.txt" 2>&1
"$BIN" debug-embed-eval report --eval-dir "$EVAL" \
  --json "$OUT/report-vs-fp32.json" "$RUNS/jina-v2-base-code-fp32" "$RUNS/$INT8" \
  "$RUNS/random" "$RUNS/shuffled" "${CANDS[@]}" > "$OUT/report-vs-fp32.txt" 2>&1

for d in "$RUNS/$INT8" "${CANDS[@]}"; do
  python3 "$EVAL/shipped_floor_rates.py" --run "$d" --floors shipped-int8 > "$OUT/gm434-$(basename "$d").txt" 2>&1
done

python3 "$EVAL/gm423_token_lengths.py" --forms full,first-paragraph,structured \
  --runs "runs,runs-gm423,$(basename "$OUT")" --json "$OUT/token_lengths.json" > "$OUT/token_lengths.md" 2>&1

python3 "$EVAL/gm465_summary.py" summary "$OUT" > "$OUT/summary.md" 2>&1
log "done; summary at $OUT/summary.md"
