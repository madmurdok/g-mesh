#!/usr/bin/env bash
# GM-423 (docs/results/gm-423-sequence-length.md): the shipped int8 model at
# max_tokens 1024 (control re-run of the stored int8 run), 512, 256, and the
# first-paragraph text at 1024. Quality and timing come from the same passes:
# per corpus every arm runs once, the arm order rotated by corpus so no arm is
# always first or last. Then a second timed round on g-mesh (embed only),
# order reversed. Then the control comparison, the reports and the summary.
# Env: OUT (default eval/embedding/work/runs-gm423), SKIP_RUNS=1 to only report,
# CORPORA / R2 to shrink the set (dry runs).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.." || exit
BIN=$PWD/target/release/g-mesh
EVAL=eval/embedding
OUT=${OUT:-$PWD/$EVAL/work/runs-gm423}
RUNS=$PWD/$EVAL/work/runs
ARMS=(jina-v2-base-code-int8 jina-v2-base-code-int8-seq512 jina-v2-base-code-int8-seq256 jina-v2-base-code-int8-first-paragraph)
read -r -a CORPORA <<< "${CORPORA:-task-tracker-mcp gin py-requests excalidraw ripgrep g-mesh}"
R2=${R2:-g-mesh}  # the round-2 corpus
export G_MESH_EMBEDDING_CACHE=off
mkdir -p "$OUT/logs"
LOG=$OUT/log.txt
TSV=$OUT/timing.tsv
START=$(date +%s)
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }

# stored per-corpus embed seconds of the int8 run, as the ETA weight
weight() { python3 -c "import json;print(json.load(open('$RUNS/jina-v2-base-code-int8/$1/timings.json'))['embedNodesMs']/1000)"; }
TOTAL_W=0; for c in "${CORPORA[@]}"; do TOTAL_W=$(echo "$TOTAL_W + 4 * $(weight "$c")" | bc -l); done
TOTAL_W=$(echo "$TOTAL_W + 4 * $(weight "$R2")" | bc -l)
DONE_W=0

one() { # round corpus arm mode(full|embed-only) outdir
  local round=$1 c=$2 v=$3 mode=$4 out=$5 extra=""
  [ "$mode" = embed-only ] && extra="--embed-only"
  local tag="$round-$c-$v"
  local before; before=$(uptime | sed 's/.*load averages*: //')
  # shellcheck disable=SC2086
  /usr/bin/time -lp "$BIN" debug-embed-eval run --eval-dir "$EVAL" --variant "$v" --corpus "$c" \
    --out "$out" --force $extra > "$OUT/logs/$tag.out" 2> "$OUT/logs/$tag.err"
  local rc=$?
  local after; after=$(uptime | sed 's/.*load averages*: //')
  local real user sys rss embed qmed
  real=$(awk '/^real/{print $2}' "$OUT/logs/$tag.err"); user=$(awk '/^user/{print $2}' "$OUT/logs/$tag.err")
  sys=$(awk '/^sys/{print $2}' "$OUT/logs/$tag.err"); rss=$(awk '/maximum resident set size/{print $1}' "$OUT/logs/$tag.err")
  read -r embed qmed < <(python3 -c "import json;d=json.load(open('$out/$v/$c/timings.json'));print(d['embedNodesMs']/1000, d.get('queryEmbedMsMedian'))")
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$round" "$c" "$v" "$rc" "$embed" "$real" "$user" "$sys" "$rss" "$qmed" "$before" "$after" >> "$TSV"
  DONE_W=$(echo "$DONE_W + $(weight "$c")" | bc -l)
  local el=$(( $(date +%s) - START ))
  local pct; pct=$(echo "100 * $DONE_W / $TOTAL_W" | bc -l)
  local eta; eta=$(echo "$el * (100 - $pct) / ($pct + 0.001)" | bc -l)
  log "$(printf 'round %s %s %s rc=%s embed=%.1fs real=%s user=%s | %.0f%% elapsed %ds ETA %.0fs load %s' "$round" "$c" "$v" "$rc" "$embed" "$real" "$user" "$pct" "$el" "$eta" "$(load1)")"
}

if [ "${SKIP_RUNS:-0}" != 1 ]; then
  printf 'round\tcorpus\tarm\trc\tembed_s\treal\tuser\tsys\tmaxrss\tquery_ms_median\tload_before\tload_after\n' > "$TSV"
  log "start; daemons: $(pgrep -fl 'g-mesh daemon' | tr '\n' ';') uptime: $(uptime)"
  i=0
  for c in "${CORPORA[@]}"; do
    for k in 0 1 2 3; do one 1 "$c" "${ARMS[$(( (k + i) % 4 ))]}" full "$OUT"; done
    i=$((i + 1))
  done
  # round 2: g-mesh only, embed only, reverse of round 1's g-mesh order
  i=5
  for k in 3 2 1 0; do one 2 "$R2" "${ARMS[$(( (k + i) % 4 ))]}" embed-only "$OUT/round2"; done
  log "runs done; uptime: $(uptime)"
fi

# control: the 1024 re-run against the stored int8 run
python3 "$EVAL/gm423_summary.py" control "$RUNS/jina-v2-base-code-int8" "$OUT/jina-v2-base-code-int8" > "$OUT/control.txt" 2>&1

# costs.toml from this run (g-mesh passes: median of the two rounds per arm)
python3 "$EVAL/gm423_summary.py" costs "$TSV" "$RUNS/jina-v2-base-code-int8/g-mesh/manifest.json" "$R2" > "$OUT/costs.toml"

# D9 against the int8 baseline (stored run), and against fp32
CANDS=("$OUT/jina-v2-base-code-int8-seq512" "$OUT/jina-v2-base-code-int8-seq256" "$OUT/jina-v2-base-code-int8-first-paragraph")
"$BIN" debug-embed-eval report --eval-dir "$EVAL" --reference jina-v2-base-code-int8 --costs "$OUT/costs.toml" \
  --json "$OUT/report-vs-int8.json" "$RUNS/jina-v2-base-code-int8" "$RUNS/jina-v2-base-code-fp32" \
  "$RUNS/random" "$RUNS/shuffled" "${CANDS[@]}" > "$OUT/report-vs-int8.txt" 2>&1
"$BIN" debug-embed-eval report --eval-dir "$EVAL" \
  --json "$OUT/report-vs-fp32.json" "$RUNS/jina-v2-base-code-fp32" "$RUNS/jina-v2-base-code-int8" \
  "$RUNS/random" "$RUNS/shuffled" "${CANDS[@]}" > "$OUT/report-vs-fp32.txt" 2>&1

# GM-434 columns: false alarm / confident wrong at the shipped int8 floors
for d in "$RUNS/jina-v2-base-code-int8" "${CANDS[@]}"; do
  python3 "$EVAL/shipped_floor_rates.py" --run "$d" --floors shipped-int8 > "$OUT/gm434-$(basename "$d").txt" 2>&1
done

python3 "$EVAL/gm423_summary.py" summary "$OUT" > "$OUT/summary.md" 2>&1
log "done; summary at $OUT/summary.md"
