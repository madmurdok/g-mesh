#!/bin/bash
# GM-441 S5 (docs/design/GM-441-prioritised-embedding.md, "Measurement plan"):
# schedule replay of A0 id, A1 tier, C oracle and the tier-reversed control over
# every corpus, F4 rerank at 100%, then one cold daemon confirm run per arm
# (A0 on the pre-tier binary, A1 on this branch). Fail-fast. Progress lines go
# to $OUT/progress.log, the compact result to $OUT/summary.txt (+ summary.json).
#
# Env: OUT, MAIN (main checkout's eval/embedding), BIN_A1, BIN_A0,
#   CORPORA (default all), CONFIRM_CORPUS (default g-mesh), CONFIRM=0 to skip,
#   LOAD_MAX (8), LOAD_WAIT_S (2400), TIMEOUT_S (1500).
set -eu
set -o pipefail
WT=$(cd "$(dirname "$0")/../.." && pwd)
MAIN=${MAIN:-/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh/eval/embedding}
OUT=${OUT:-/tmp/gm441/run}
BIN_A1=${BIN_A1:-$WT/target/release/g-mesh}
BIN_A0=${BIN_A0:-$WT/../g-mesh-wt-gm441-base/target/release/g-mesh}
CONFIRM_CORPUS=${CONFIRM_CORPUS:-g-mesh}
LOAD_MAX=${LOAD_MAX:-8}
LOAD_WAIT_S=${LOAD_WAIT_S:-2400}
TIMEOUT_S=${TIMEOUT_S:-1500}
VARIANT=jina-v2-base-code-int8-structured
RUNS=$MAIN/work/runs-gm465
EV=$WT/eval/embedding
corp=()
for c in ${CORPORA:-}; do corp+=(--corpus "$c"); done

mkdir -p "$OUT"
PROG=$OUT/progress.log
SUM=$OUT/summary.txt
: > "$SUM"
progress() { echo "[$(date '+%H:%M:%S')] $*" >> "$PROG"; }
sumln() { echo "$*" >> "$SUM"; }
state() { sumln "  uptime: $(uptime | sed 's/^ *//')"; }
timed() { # label, command...
  local label=$1; shift
  progress "start $label"
  sumln "== $label"; state
  local t="$OUT/time.$$.txt"
  /usr/bin/time -p "$@" > "$OUT/$label.out" 2> "$t" || { cat "$t" >> "$OUT/$label.err"; progress "FAIL $label"; sumln "FAILED $label (see $OUT/$label.out/.err)"; exit 1; }
  cp "$t" "$OUT/$label.err"
  sumln "  time -p: $(tr '\n' ' ' < "$t" | sed 's/.*\(real [0-9.]*\).*\(user [0-9.]*\).*\(sys [0-9.]*\).*/\1 \2 \3/')"
  state
  progress "done $label"
}

cd "$WT"

# 0. binaries beside which the language plugins must be current
timed build-a1 cargo build --release --workspace
(cd "$WT/../g-mesh-wt-gm441-base" && timed build-a0 cargo build --release --workspace)

# 1. replay: all arms, with the noise mask
timed schedule "$BIN_A1" debug-embed-eval schedule --eval-dir "$MAIN" --run-root "$RUNS" --variant $VARIANT \
  --filter noise --curve "$RUNS/../runs-gm466/rel.curve.json" ${corp[@]+"${corp[@]}"} --out "$OUT/sched"

# 2. control: mask off, arm id, 100% rankings must equal the stored run's
timed schedule-nomask "$BIN_A1" debug-embed-eval schedule --eval-dir "$MAIN" --run-root "$RUNS" --variant $VARIANT \
  --filter none --curve "$RUNS/../runs-gm466/rel.curve.json" --arm id --final-rankings-only ${corp[@]+"${corp[@]}"} --out "$OUT/ctl"

# 3. rerank at 100% (F4 over cached logits)
timed rerank python3 "$EV/gm441_rerank.py" --max-score "${MAX_SCORE:-5000}" --work "$MAIN/work" --schedule-root "$OUT/sched" ${corp[@]+"${corp[@]}"}
sumln "-- rerank output"; cat "$OUT/rerank.out" >> "$SUM"
timed rerank-unit python3 "$EV/test_gm441_rerank.py"

# 4. daemon confirm, one cold run per arm
if [ "${CONFIRM:-1}" = 1 ]; then
  mkdir -p "$OUT/confirm"
  load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
  for arm in A0 A1; do
    if [ $arm = A0 ]; then bin=$BIN_A0; else bin=$BIN_A1; fi
    waited=0
    while awk -v l="$(load1)" -v m="$LOAD_MAX" 'BEGIN{exit !(l>m)}'; do
      if [ $waited -ge "$LOAD_WAIT_S" ]; then sumln "WARN confirm $arm: load1 $(load1) still above $LOAD_MAX after ${waited}s, running anyway"; break; fi
      sleep 30; waited=$((waited+30))
    done
    sumln "confirm $arm: gated load1 $(load1) (waited ${waited}s, max $LOAD_MAX)"
    src=/tmp/gm441/src-$arm
    rm -rf "$src" "/tmp/gm441/h-$arm"
    cp -c -R "$MAIN/work/corpora/$CONFIRM_CORPUS" "$src"
    export G_MESH_RERANK_MODEL_DIR=$MAIN/work/models/ms-marco-MiniLM-L6-v2
    timed confirm-$arm python3 "$EV/gm441_confirm.py" --bin "$bin" --project "$src" --home "/tmp/gm441/h-$arm" \
      --out "$OUT/confirm/$arm.json" --timeout "$TIMEOUT_S"
    sumln "  confirm $arm result: $(cat "$OUT/confirm-$arm.out")"
    pid=$(python3 -c "import json;print(json.load(open('$OUT/confirm/$arm.json'))['daemonPid'])")
    sumln "  daemon pid $pid alive after: $(kill -0 "$pid" 2>/dev/null && echo yes || echo no)"
  done
fi

# 5. summary
sumln "== summary"
python3 "$EV/gm441_summary.py" --sched "$OUT/sched" --ctl "$OUT/ctl" --work "$MAIN/work" --eval-dir "$MAIN" \
  --variant-dir "$RUNS/$VARIANT" --confirm "$OUT/confirm" --confirm-corpus "$CONFIRM_CORPUS" \
  --out "$OUT/summary.json" >> "$SUM"
progress "ALL DONE"
sumln "ALL DONE"
