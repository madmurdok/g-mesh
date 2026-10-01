#!/usr/bin/env bash
# GM-455 (docs/results/gm-455-structural-context.md): structural context in the
# embedded text, plus GM-465's second timing round (docs/results/
# gm-465-structured-trim.md). Adapted from gm465_measure.sh.
#   1. quality (ungated; deterministic): QARMS on CORPORA, full mode
#   2. churn on g-mesh for every context arm, and churn --dump (text hashes)
#      on every corpus for the token-length control
#   3. timing (gated, embed-only, g-mesh): TIMING, a list of task:round:variant
#      entries run in order. Before each one the gate waits until pmset shows
#      CPU_Speed_Limit = CPU_Scheduler_Limit = 100, 1-min load < GATE_LOAD, no
#      `jamf policy` / cargo / rustc process, all held for GATE_HOLD seconds.
#      An invocation is valid when it started from the open gate and the
#      1-min load stayed <= LOAD_MAX (default 20: the pass itself holds it
#      near 5, so above 20 means ~15 from outside, e.g. jamf); it is retried
#      once otherwise. Throttling during the pass (pmset limit < 100) is
#      inherent to this 4-core laptop under a 4-thread pass: it is recorded
#      (limit at start, minimum in the run, at the end), not grounds for a
#      retry. Total gate waiting is capped at GATE_CAP seconds; past it,
#      timing stops.
#   4. controls, costs, reports, token table, summary (gm455_summary.py).
# Env: OUT (default eval/embedding/work/runs-gm455), QARMS, CORPORA, TIMING,
# SKIP_QUALITY=1, SKIP_CHURN=1, SKIP_TIMING=1, SKIP_REPORTS=1, GATE=0 (dry run:
# log the gate state, do not wait).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.." || exit
BIN=$PWD/target/release/g-mesh
EVAL=eval/embedding
OUT=${OUT:-$PWD/$EVAL/work/runs-gm455}
INT8=jina-v2-base-code-int8
FP=jina-v2-base-code-int8-first-paragraph
ST=jina-v2-base-code-int8-structured
read -r -a QARMS <<< "${QARMS:-fp-ctx-none fp-path fp-parent fp-path-parent fp-path-parent-shuffled}"
read -r -a CORPORA <<< "${CORPORA:-task-tracker-mcp gin py-requests excalidraw ripgrep g-mesh}"
# GM-455 round 1, GM-465 round 2 (reverse of its round 1), GM-455 round 2 (reversed)
read -r -a TIMING <<< "${TIMING:-455:1:fp-ctx-none 455:1:fp-path 455:1:fp-parent 455:1:fp-path-parent \
465:2:$ST 465:2:$FP 465:2:$INT8 \
455:2:fp-path-parent 455:2:fp-parent 455:2:fp-path 455:2:fp-ctx-none}"
CTX_ARMS=(fp-ctx-none fp-path fp-parent fp-path-parent fp-path-parent-shuffled)
TC=${TC:-g-mesh}                     # the timed corpus
GATE=${GATE:-1}
GATE_LOAD=${GATE_LOAD:-4}
GATE_HOLD=${GATE_HOLD:-120}
GATE_CAP=${GATE_CAP:-10800}
LOAD_MAX=${LOAD_MAX:-20}
export G_MESH_EMBEDDING_CACHE=off
mkdir -p "$OUT/logs"
LOG=$OUT/log.txt
QTSV=$OUT/quality.tsv
TSV=$OUT/timing.tsv
START=$(date +%s)
GATE_WAITED=0
SPID=
# the in-run sampler must not outlive the script (an orphan kept appending)
trap '[ -n "$SPID" ] && kill "$SPID" 2> /dev/null' EXIT
trap 'exit 143' TERM INT
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
therm() { # "speed/scheduler", "na" when pmset does not print a limit
  pmset -g therm | awk '/CPU_Speed_Limit/{s=$3} /CPU_Scheduler_Limit/{c=$3} END{printf "%s/%s", (s==""?"na":s), (c==""?"na":c)}'
}
state() { echo "load $(load1) therm $(therm)"; }
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
el() { echo $(( $(date +%s) - START )); }

# 0 when the machine is quiet enough right now
quiet_now() {
  local th s c
  th=$(therm); s=${th%/*}; c=${th#*/}
  [ "$s" = 100 ] && [ "$c" = 100 ] || return 1
  [ "$(echo "$(load1) < $GATE_LOAD" | bc -l)" = 1 ] || return 1
  ! pgrep -f 'jamf policy' > /dev/null && ! pgrep -x cargo > /dev/null && ! pgrep -x rustc > /dev/null
}

# Wait until quiet_now holds for GATE_HOLD s (sampled every 20 s). 1 when the
# total wait passes GATE_CAP.
gate() {
  if [ "$GATE" != 1 ]; then log "gate off (dry run): $(state)"; return 0; fi
  local held=0 waited=0
  while :; do
    if quiet_now; then
      [ "$held" -ge "$GATE_HOLD" ] && break
      held=$((held + 20))
    else
      held=0
    fi
    if [ $((waited % 600)) = 0 ] && [ "$waited" -gt 0 ]; then
      log "gate waiting ${waited}s (total $((GATE_WAITED))s): $(state) jamf/cargo [$(pgrep -fl 'jamf policy|^cargo|rustc' | tr '\n' ' ')]"
    fi
    if [ "$GATE_WAITED" -ge "$GATE_CAP" ]; then log "gate cap ${GATE_CAP}s reached: $(state)"; return 1; fi
    sleep 20; waited=$((waited + 20)); GATE_WAITED=$((GATE_WAITED + 20))
  done
  log "gate open after ${waited}s (total ${GATE_WAITED}s): $(state)"
}

# one <tsv> <stage> <round> <corpus> <variant> <full|embed-only> <outdir> <tag>
one() {
  local tsv=$1 stage=$2 round=$3 c=$4 v=$5 mode=$6 out=$7 tag=$8 extra=""
  [ "$mode" = embed-only ] && extra="--embed-only"
  local lb tb; lb=$(uptime | sed 's/.*load averages*: //'); tb=$(therm)
  # in-run sampler: min pmset limits and max 1-min load every 10 s
  local samp=$OUT/logs/$tag.therm
  : > "$samp"
  ( while :; do echo "$(therm) $(load1)" >> "$samp"; sleep 10; done ) &
  SPID=$!
  local spid=$SPID
  # shellcheck disable=SC2086
  /usr/bin/time -lp "$BIN" debug-embed-eval run --eval-dir "$EVAL" --variant "$v" --corpus "$c" \
    --out "$out" --force $extra > "$OUT/logs/$tag.out" 2> "$OUT/logs/$tag.err"
  local rc=$?
  kill "$spid" 2> /dev/null; wait "$spid" 2> /dev/null
  local la ta; la=$(uptime | sed 's/.*load averages*: //'); ta=$(therm)
  local tmin lmax
  tmin=$(awk '{split($1,a,"/"); s=(s==""||a[1]<s)?a[1]:s; c=(c==""||a[2]<c)?a[2]:c} END{printf "%s/%s", s, c}' "$samp")
  lmax=$(awk '{if($2>m)m=$2} END{print m+0}' "$samp")
  local real user sys rss embed qmed
  real=$(awk '/^real/{print $2}' "$OUT/logs/$tag.err"); user=$(awk '/^user/{print $2}' "$OUT/logs/$tag.err")
  sys=$(awk '/^sys/{print $2}' "$OUT/logs/$tag.err"); rss=$(awk '/maximum resident set size/{print $1}' "$OUT/logs/$tag.err")
  read -r embed qmed < <(python3 -c "import json;d=json.load(open('$out/$v/$c/timings.json'));print(d['embedNodesMs']/1000, d.get('queryEmbedMsMedian'))" 2>/dev/null || echo "nan None")
  local valid=1 s=${ta%/*}
  { [ "$GATE" != 1 ] || [ "$tb" != 100/100 ] || [ "$(echo "$lmax > $LOAD_MAX" | bc -l)" = 1 ] || [ "$rc" != 0 ]; } && valid=0
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$stage" "$round" "$c" "$v" "$rc" \
    "$embed" "$real" "$user" "$sys" "$rss" "$qmed" "$lb" "$la" "$tb" "$ta" "$tmin" "$lmax" "$valid" >> "$tsv"
  LAST_VALID=$valid
  DONE=$((DONE + 1))
  log "$(printf 'done %s %d/%d %s %s %s rc=%s embed=%ss real=%s user=%s valid=%s | elapsed %ds | load %s therm %s -> %s (min in run %s)' \
    "$stage" "$DONE" "$TOTAL" "$c" "$v" "$round" "$rc" "$embed" "$real" "$user" "$valid" "$(el)" "$(load1)" "$tb" "$ta" "$tmin")"
}

HDR='stage\tround\tcorpus\tarm\trc\tembed_s\treal\tuser\tsys\tmaxrss\tquery_ms_median\tload_before\tload_after\ttherm_before\ttherm_after\ttherm_min_run\tload1_max_run\tvalid\n'
DONE=0
TOTAL=0
[ "${SKIP_QUALITY:-0}" != 1 ] && TOTAL=$((TOTAL + ${#QARMS[@]} * ${#CORPORA[@]}))
[ "${SKIP_TIMING:-0}" != 1 ] && TOTAL=$((TOTAL + ${#TIMING[@]}))
log "start; daemons: $(pgrep -fl 'g-mesh daemon' | tr '\n' ';') uptime: $(uptime) therm $(therm)"

if [ "${SKIP_QUALITY:-0}" != 1 ]; then
  [ -f "$QTSV" ] || printf '%b' "$HDR" > "$QTSV"
  for v in "${QARMS[@]}"; do
    for c in "${CORPORA[@]}"; do one "$QTSV" quality q "$c" "$v" full "$OUT" "q-$c-$v"; done
  done
fi

if [ "${SKIP_CHURN:-0}" != 1 ]; then
  args=(); for v in "${CTX_ARMS[@]}"; do args+=(--variant "$v"); done
  log "churn on g-mesh: $(state)"
  /usr/bin/time -p "$BIN" debug-embed-eval churn --eval-dir "$EVAL" --corpus g-mesh "${args[@]}" \
    --json "$OUT/churn.json" > "$OUT/churn.md" 2> "$OUT/logs/churn.err"
  log "churn rc=$? $(state)"
  for c in task-tracker-mcp gin py-requests excalidraw ripgrep g-mesh; do
    mkdir -p "$OUT/dump/$c"
    "$BIN" debug-embed-eval churn --eval-dir "$EVAL" --corpus "$c" "${args[@]}" --edit e1 \
      --dump "$OUT/dump/$c" > /dev/null 2>> "$OUT/logs/dump.err"
  done
  log "dumps done $(state)"
fi

if [ "${SKIP_TIMING:-0}" != 1 ]; then
  [ -f "$TSV" ] || printf '%b' "$HDR" > "$TSV"
  for e in "${TIMING[@]}"; do
    task=${e%%:*}; rest=${e#*:}; round=${rest%%:*}; v=${rest#*:}
    for attempt in 1 2; do
      gate || { log "timing stopped at $e (gate cap)"; break 2; }
      tag="t$task-r$round-$v-a$attempt"
      one "$TSV" "t$task" "$round" "$TC" "$v" embed-only "$OUT/timing/$tag" "$tag"
      [ "$LAST_VALID" = 1 ] && break
      [ "$attempt" = 1 ] && { log "invalid; retrying $e once"; TOTAL=$((TOTAL + 1)); }
    done
  done
  log "timing done; gate waited ${GATE_WAITED}s; uptime: $(uptime)"
fi

if [ "${SKIP_REPORTS:-0}" != 1 ]; then
  bash -c "OUT='$OUT' python3 '$EVAL/gm455_summary.py' reports" >> "$LOG" 2>&1
fi
log "done; summary at $OUT/summary.md"
