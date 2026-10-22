#!/usr/bin/env bash
# GM-453 (docs/design/GM-453-onnx-tuning.md §5.3, §7): ONNX Runtime session and
# batching settings, timed through the GM-398 pass with G_MESH_EMBED_* per arm.
#   A  screening: int8-structured on gin + py-requests, 13 arms x ROUNDS_A
#   B  confirmation: int8-structured on g-mesh, A0 + the two best adoptable
#      phase-A arms x ROUNDS_B, quality once per arm (skipped by the §10.2 gate)
#   C  fp32 (R): jina-v2-base-code-fp32-r on g-mesh, C0/C1/C2 x ROUNDS_C,
#      quality once per arm, then the real parity control (int8 vs fp32)
#   D  interactive latency: the probe alone, then beside an fp32 pass at
#      normal priority, under `taskpolicy -b`, and with the background thread rule
# Parity (`debug-embed-eval compare`) failing is a result: it is recorded and
# the run goes on. Any other non-zero exit stops the script.
#
# Env: OUT (default eval/embedding/work/runs-gm453), PHASES (default "A B C D"),
#   ROUNDS_A/B/C (2/3/2), CORPORA_A ("gin py-requests"), CORPUS_BC (g-mesh),
#   B_ARMS (phase-B arms after A0, default: picked by gm453_summary.py best),
#   C1_ENV / C2_ENV (phase C's bucketing and background-thread settings),
#   PROBE_IDLE_S (300), NOWAIT=1 (skip the quiet-machine wait, dry runs only),
#   FORCE_B=1 (run B even when the gate says stop), BIN (release binary).
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.." || exit 1
BIN=${BIN:-$PWD/target/release/g-mesh}
EVAL=eval/embedding
OUT=${OUT:-$PWD/$EVAL/work/runs-gm453}
PHASES=${PHASES:-A B C D}
ROUNDS_A=${ROUNDS_A:-2}
ROUNDS_B=${ROUNDS_B:-3}
ROUNDS_C=${ROUNDS_C:-2}
read -r -a CORPORA_A <<< "${CORPORA_A:-gin py-requests}"
CORPUS_BC=${CORPUS_BC:-g-mesh}
PROBE_IDLE_S=${PROBE_IDLE_S:-300}
INT8=jina-v2-base-code-int8-structured
FP32=jina-v2-base-code-fp32-r
SUMMARY=(python3 "$EVAL/gm453_summary.py")

# The arms' settings come only from this script, never from the caller.
for v in $(compgen -v G_MESH_EMBED_); do unset "$v"; done
export G_MESH_EMBEDDING_CACHE=off

# Phase A arms, "name|settings" (settings are VAR=value words for `env`).
ARMS_A=(
  "A0|"
  "A1|G_MESH_EMBED_INTRA_THREADS=1"
  "A2|G_MESH_EMBED_INTRA_THREADS=2"
  "A3|G_MESH_EMBED_INTRA_THREADS=physical"
  "A4|G_MESH_EMBED_INTRA_THREADS=logical"
  "A5|G_MESH_EMBED_OPT_LEVEL=basic"
  "A6|G_MESH_EMBED_OPT_LEVEL=extended"
  "A7|G_MESH_EMBED_SPIN=0"
  "A8|G_MESH_EMBED_INTER_THREADS=2"
  "A9|G_MESH_EMBED_BATCH=8 G_MESH_EMBED_TOKEN_BUDGET=8192 G_MESH_EMBED_BUCKET=0"
  "A10|G_MESH_EMBED_BATCH=8 G_MESH_EMBED_TOKEN_BUDGET=8192 G_MESH_EMBED_BUCKET=1"
  "A11|G_MESH_EMBED_BATCH=32 G_MESH_EMBED_TOKEN_BUDGET=16384 G_MESH_EMBED_BUCKET=1"
  "A12|G_MESH_EMBED_INTRA_THREADS=physical G_MESH_EMBED_BATCH=8 G_MESH_EMBED_BUCKET=1 G_MESH_EMBED_SPIN=0"
)
C1_ENV=${C1_ENV:-G_MESH_EMBED_BATCH=8 G_MESH_EMBED_TOKEN_BUDGET=8192 G_MESH_EMBED_BUCKET=1}
C2_ENV=${C2_ENV:-G_MESH_EMBED_INTRA_THREADS=2}
ARMS_C=("C0|" "C1|$C1_ENV" "C2|$C2_ENV")

mkdir -p "$OUT/logs"
LOG=$OUT/log.txt
TSV=$OUT/timing.tsv
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
die() { log "FATAL: $*"; exit 1; }
arm_name() { echo "${1%%|*}"; }
arm_env() { echo "${1#*|}"; }
arm_env_of() { # name, then the "name|settings" entries to look it up in
  local name=$1; shift
  local a; for a in "$@"; do [ "$(arm_name "$a")" = "$name" ] && { arm_env "$a"; return; }; done
  die "unknown arm $name"
}

# measure_costs.sh's quiet wait: start below load 7.0, give up waiting after 5 min.
wait_quiet() {
  [ "${NOWAIT:-0}" = 1 ] && return
  local n=0
  while awk -v l="$(load1)" 'BEGIN{exit !(l>=7.0)}'; do
    n=$((n+1)); [ $n -gt 10 ] && { log "WARN load still $(load1) after 5 min, proceeding"; return; }
    sleep 30
  done
}

[ -x "$BIN" ] || die "no release binary at $BIN"
[ -f "$TSV" ] || printf 'phase\tround\tarm\tvariant\tcorpora\trc\tembed_ms\treal\tuser\tsys\tmaxrss\tload_before\tload_after\tattempt\tpolicy\tsettings\n' > "$TSV"
log "start phases [$PHASES]; uptime: $(uptime); daemons: $(pgrep -fl 'g-mesh daemon' | tr '\n' ';')"

# --- Probe (phase D) -----------------------------------------------------------
# A probe that is running has its stop file in PROBE_STOP; any exit, die()
# included, touches it so no probe outlives the script.
PROBE_STOP=
PROBE_WITH=
cleanup() { if [ -n "$PROBE_STOP" ]; then touch "$PROBE_STOP"; fi; }
trap cleanup EXIT
# The probe runs at normal priority with default options and a clean
# G_MESH_EMBED_* environment; only the pass beside it changes per arm.
probe_start() { # arm, then extra probe flags
  local arm=$1; shift
  PROBE_STOP="$OUT/D/$arm.stop"; rm -f "$PROBE_STOP"
  mkdir -p "$OUT/D"
  "$BIN" debug-embed-eval probe --eval-dir "$EVAL" --variant "$FP32" --corpus "$CORPUS_BC" \
    --burst 5 --interval-ms 2000 --until "$PROBE_STOP" --out "$OUT/D/$arm.jsonl" "$@" \
    > "$OUT/logs/probe-$arm.out" 2> "$OUT/logs/probe-$arm.err" &
  PROBE_PID=$!
}
probe_stop() {
  touch "$PROBE_STOP"
  local stop=$PROBE_STOP; PROBE_STOP=
  wait "$PROBE_PID" || die "probe exited non-zero (see $OUT/logs/; stop file $stop)"
}
probe_alone() { # arm settings extra-flags...
  local arm=$1 settings=$2; shift 2
  mkdir -p "$OUT/D"; rm -f "$OUT/D/$arm.stop"
  wait_quiet
  log "probe $arm alone for ${PROBE_IDLE_S}s [$settings] uptime: $(uptime)"
  # shellcheck disable=SC2086
  env $settings "$BIN" debug-embed-eval probe --eval-dir "$EVAL" --variant "$FP32" --corpus "$CORPUS_BC" \
    --burst 5 --interval-ms 2000 --until "$OUT/D/$arm.stop" --out "$OUT/D/$arm.jsonl" "$@" \
    > "$OUT/logs/probe-$arm.out" 2> "$OUT/logs/probe-$arm.err" &
  local pid=$!
  PROBE_STOP="$OUT/D/$arm.stop"
  sleep "$PROBE_IDLE_S"
  touch "$PROBE_STOP"; PROBE_STOP=
  wait "$pid" || die "probe $arm exited non-zero"
}
# One timed embed-only pass: phase round arm variant settings policy(normal|background) corpora...
# Leaves timings-embed.json beside each corpus's timings.json, so a later
# quality run in the same directory cannot overwrite the pass's timings.
# Redone once when the load after the run exceeds 8.0 plus the run's own
# parallelism (GM-398's owner-approved noisy-machine rule). With PROBE_WITH
# set to an arm name, a probe runs beside each attempt: started after the
# quiet wait and stopped when the pass ends, so it samples only the pass
# (a redo restarts it, truncating that arm's jsonl).
timed() {
  local phase=$1 round=$2 arm=$3 variant=$4 settings=$5 policy=$6; shift 6
  local corpora=("$@") out="$OUT/$phase/r$round/$arm" cargs=() c
  for c in "${corpora[@]}"; do cargs+=(--corpus "$c"); done
  local prefix=()
  [ "$policy" = background ] && prefix=(taskpolicy -b)
  local attempt
  for attempt in 1 2; do
    wait_quiet
    [ -n "$PROBE_WITH" ] && probe_start "$PROBE_WITH"
    local tag="$phase-r$round-$arm-a$attempt" before after rc
    before=$(load1)
    log "start $phase r$round $arm ($variant, ${corpora[*]}, $policy) [$settings] uptime: $(uptime)"
    # shellcheck disable=SC2086 # settings expands to VAR=value words for env
    /usr/bin/time -lp ${prefix[@]+"${prefix[@]}"} env $settings "$BIN" debug-embed-eval run --eval-dir "$EVAL" \
      --variant "$variant" "${cargs[@]}" --out "$out" --embed-only --force \
      > "$OUT/logs/$tag.out" 2> "$OUT/logs/$tag.err"
    rc=$?
    after=$(load1)
    [ -n "$PROBE_WITH" ] && probe_stop
    [ $rc = 0 ] || die "$tag exited $rc (see $OUT/logs/$tag.err)"
    local embed=0 real user sys rss
    for c in "${corpora[@]}"; do
      cp "$out/$variant/$c/timings.json" "$out/$variant/$c/timings-embed.json"
      embed=$(python3 -c "import json;print($embed + json.load(open('$out/$variant/$c/timings.json'))['embedNodesMs'])")
    done
    real=$(awk '/^real/{print $2}' "$OUT/logs/$tag.err"); user=$(awk '/^user/{print $2}' "$OUT/logs/$tag.err")
    sys=$(awk '/^sys/{print $2}' "$OUT/logs/$tag.err"); rss=$(awk '/maximum resident set size/{print $1}' "$OUT/logs/$tag.err")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$phase" "$round" "$arm" "$variant" \
      "${corpora[*]}" "$rc" "$embed" "$real" "$user" "$sys" "$rss" "$before" "$after" "$attempt" "$policy" "$settings" >> "$TSV"
    log "done  $phase r$round $arm embed=${embed}ms real=$real user=$user sys=$sys rss=$rss load $before -> $after"
    local own; own=$(awk -v r="$real" -v u="$user" 'BEGIN{printf "%.2f", (r > 0 ? u / r : 0)}')
    awk -v l="$after" -v o="$own" 'BEGIN{exit !(l>8.0+o)}' || return 0
    [ "$attempt" = 1 ] && log "after-load $after > 8.0 + own $own: redo"
  done
}

# Parity of a candidate run against a baseline run (dirs <out>/<variant>/<corpus>).
# Writes <name>.json; a tolerance failure is recorded, anything else is fatal.
parity() {
  local name=$1 baseline=$2 candidate=$3; shift 3
  local json="$OUT/parity/$name.json"
  mkdir -p "$OUT/parity"
  rm -f "$json"
  "$BIN" debug-embed-eval compare --eval-dir "$EVAL" --baseline "$baseline" --candidate "$candidate" \
    --json "$json" "$@" > "$OUT/parity/$name.txt" 2>&1
  local rc=$?
  [ -f "$json" ] || die "compare $name failed without a result (rc $rc, see $OUT/parity/$name.txt)"
  log "parity $name rc=$rc $(head -1 "$OUT/parity/$name.txt")"
}

# Recall/MRR: a full `run` in the timed pass's directory reuses its vectors
# (same manifest) and only embeds and ranks the queries.
quality() {
  local phase=$1 arm=$2 variant=$3 settings=$4 corpus=$5
  wait_quiet
  # shellcheck disable=SC2086
  env $settings "$BIN" debug-embed-eval run --eval-dir "$EVAL" --variant "$variant" --corpus "$corpus" \
    --out "$OUT/$phase/r1/$arm" > "$OUT/logs/q-$phase-$arm.out" 2> "$OUT/logs/q-$phase-$arm.err" \
    || die "quality $phase $arm failed (see $OUT/logs/q-$phase-$arm.err)"
  log "quality $phase $arm done"
}

run_phase_rounds() { # phase rounds variant entries...  (odd rounds in order, even rounds reversed)
  local phase=$1 rounds=$2 variant=$3; shift 3
  local entries=("$@") r i n=$#
  for r in $(seq 1 "$rounds"); do
    for i in $(seq 0 $((n - 1))); do
      local e
      if [ $((r % 2)) = 1 ]; then e=${entries[$i]}; else e=${entries[$((n - 1 - i))]}; fi
      if [ "$phase" = A ]; then
        timed "$phase" "$r" "$(arm_name "$e")" "$variant" "$(arm_env "$e")" normal "${CORPORA_A[@]}"
      else
        timed "$phase" "$r" "$(arm_name "$e")" "$variant" "$(arm_env "$e")" normal "$CORPUS_BC"
      fi
    done
  done
}

# Every arm's every round against the phase's baseline arm's round 1, per corpus.
phase_parity() { # phase variant baseline-arm rounds corpora-list arms...
  local phase=$1 variant=$2 base=$3 rounds=$4 corpus_list=$5; shift 5
  local arm r c
  for arm in "$@"; do
    for r in $(seq 1 "$rounds"); do
      [ "$arm" = "$base" ] && [ "$r" = 1 ] && continue
      for c in $corpus_list; do
        parity "$phase-r$r-$arm-$c" "$OUT/$phase/r1/$base/$variant/$c" "$OUT/$phase/r$r/$arm/$variant/$c"
      done
    done
  done
}

has_phase() { case " $PHASES " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# --- Phase A ------------------------------------------------------------------
if has_phase A; then
  run_phase_rounds A "$ROUNDS_A" "$INT8" "${ARMS_A[@]}"
  names=(); for e in "${ARMS_A[@]}"; do names+=("$(arm_name "$e")"); done
  phase_parity A "$INT8" A0 "$ROUNDS_A" "${CORPORA_A[*]}" "${names[@]}"
  log "phase A done; uptime: $(uptime)"
fi

# --- Phase B ------------------------------------------------------------------
if has_phase B; then
  if ! gate=$("${SUMMARY[@]}" gate "$OUT" 2>> "$LOG"); then
    [ -n "${B_ARMS:-}" ] || die "phase B needs phase A's timings in $OUT (run phase A into this OUT, or set B_ARMS)"
    gate="go B_ARMS given, no phase-A gate in $OUT"
  fi
  log "phase A gate: $gate"
  if [ "${gate%% *}" = stop ] && [ "${FORCE_B:-0}" != 1 ]; then
    log "phase B skipped by the gate (FORCE_B=1 runs it)"
  else
    if [ -z "${B_ARMS:-}" ]; then
      B_ARMS=$("${SUMMARY[@]}" best "$OUT") || die "picking phase-B arms failed"
    fi
    log "phase B arms: A0 $B_ARMS"
    entries=("A0|"); for a in $B_ARMS; do entries+=("$a|$(arm_env_of "$a" "${ARMS_A[@]}")"); done
    run_phase_rounds B "$ROUNDS_B" "$INT8" "${entries[@]}"
    names=(); for e in "${entries[@]}"; do names+=("$(arm_name "$e")"); done
    phase_parity B "$INT8" A0 "$ROUNDS_B" "$CORPUS_BC" "${names[@]}"
    for e in "${entries[@]}"; do quality B "$(arm_name "$e")" "$INT8" "$(arm_env "$e")" "$CORPUS_BC"; done
    log "phase B done; uptime: $(uptime)"
  fi
fi

# --- Phase C ------------------------------------------------------------------
if has_phase C; then
  log "fetching $FP32 (not timed)"
  python3 "$EVAL/fetch_models.py" "$FP32" 2>&1 | tee -a "$LOG" || die "fetch $FP32 failed"
  run_phase_rounds C "$ROUNDS_C" "$FP32" "${ARMS_C[@]}"
  phase_parity C "$FP32" C0 "$ROUNDS_C" "$CORPUS_BC" C0 C1 C2
  for e in "${ARMS_C[@]}"; do quality C "$(arm_name "$e")" "$FP32" "$(arm_env "$e")" "$CORPUS_BC"; done
  # Real control (§6.4): int8 against fp32 on the same nodes must fail parity.
  if [ -d "$OUT/B/r1/A0/$INT8/$CORPUS_BC" ]; then
    int8_base="$OUT/B/r1/A0/$INT8/$CORPUS_BC"
  else
    timed C 1 int8-control "$INT8" "" normal "$CORPUS_BC"
    int8_base="$OUT/C/r1/int8-control/$INT8/$CORPUS_BC"
  fi
  parity control-int8-vs-fp32 "$int8_base" "$OUT/C/r1/C0/$FP32/$CORPUS_BC" --allow-variant-mismatch
  log "phase C done; uptime: $(uptime)"
fi

# --- Phase D ------------------------------------------------------------------
if has_phase D; then
  mkdir -p "$OUT/D"; rm -f "$OUT"/D/*.stop
  probe_alone P-idle ""
  for e in "P0|normal|" "P1|background|" "P2|normal|$C2_ENV"; do
    arm=${e%%|*}; rest=${e#*|}; policy=${rest%%|*}; settings=${rest#*|}
    PROBE_WITH=$arm
    timed D 1 "$arm" "$FP32" "$settings" "$policy" "$CORPUS_BC"
    PROBE_WITH=
  done
  # Option (i) also gives queries fewer threads: the probe alone with that rule.
  probe_alone P2q "$C2_ENV" --options-from-env
  log "phase D done; uptime: $(uptime)"
fi

"${SUMMARY[@]}" summary "$OUT" > "$OUT/summary.md" 2>> "$LOG" || die "summary failed"
log "done; summary at $OUT/summary.md; uptime: $(uptime)"
