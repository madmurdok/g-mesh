#!/usr/bin/env bash
# GM-736 measure (docs/design/GM-736-cre-int8.md, sections 1-4): per weight form
# (s7, u8) the R1 alpha sweep on the selection sample, the export, the gate on
# the frozen parity sample, and on a miss R2 (sensitivity, greedy, re-export,
# gate). Every arm that passes then runs the GM-703 pipeline from step 3 and the
# reports vs J (6b) and vs CRE fp32 (6c). One background run, fail-fast;
# summary in $OUT/summary.txt.
#
# usage: measure.sh
#
# Environment: GM736_DATA, GM703_DATA, EVAL_DATA, GMESH_BIN, PY, OUT (default $GM736_DATA/measure),
# WEIGHTS (default "s7 u8"), ALPHAS (default 0.3,0.4,0.5,0.6,0.7),
# GM703_UNTIL (last gm703/measure.sh step per arm, default 9).
#
# Exit status: 0 at least one arm passed the gate and was measured; 3 no arm passed (G3).

set -Eeuo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"

if [ -z "${GM736_CAFFEINATED:-}" ]; then
    GM736_CAFFEINATED=1 exec caffeinate -i -s bash "$0" "$@"
fi

GM736_DATA="${GM736_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm736}"
GM703_DATA="${GM703_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm703}"
EVAL_DATA="${EVAL_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-eval-data/v2}"
PY="${PY:-$GM703_DATA/venv/bin/python}"
BIN="${GMESH_BIN:-$REPO/target/release/g-mesh}"
OUT="${OUT:-$GM736_DATA/measure}"
WEIGHTS="${WEIGHTS:-s7 u8}"
ALPHAS="${ALPHAS:-0.3,0.4,0.5,0.6,0.7}"
GM703_UNTIL="${GM703_UNTIL:-9}"
WORK="$GM736_DATA/work"
MODELS="$GM736_DATA/models"
V2="$REPO/eval/embedding/v2"
RUNS="$EVAL_DATA/work/runs"
J=gm730-jina-full-int8-structured
FP32=coderankembed-fp32-structured
RECIPE=("$PY" -I "$HERE/recipe.py" --work "$WORK" --models "$MODELS")

export GM736_DATA GM703_DATA EVAL_DATA HF_HUB_OFFLINE=1 TOKENIZERS_PARALLELISM=false
mkdir -p "$OUT" "$WORK" "$MODELS"
SUMMARY="$OUT/summary.txt"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$SUMMARY"; }
trap 'log "FAILED at line $LINENO (rc $?)"' ERR
# Runs "$@" under /usr/bin/time -lp, logs uptime before and after and user/sys/real.
timed() {
    local name="$1"; shift
    log "$name: start; uptime $(uptime)"
    local rc=0
    /usr/bin/time -lp "$@" > "$OUT/$name.out" 2> "$OUT/$name.err" || rc=$?
    log "$name: rc=$rc $(grep -E '^(real|user|sys) ' "$OUT/$name.err" | tr '\n' ' ')maxrss $(awk '/maximum resident set size/{print $1}' "$OUT/$name.err"); uptime $(uptime)"
    return $rc
}
arm_of() { [ "$1" = s7 ] && echo coderankembed-int8sq-structured || echo coderankembed-int8squ8-structured; }
json_field() { "$PY" -I -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }

log "gm736/measure.sh weights '$WEIGHTS' alphas $ALPHAS; HEAD $(git -C "$REPO" rev-parse --short HEAD)"
[ -e "$V2/work" ] || ln -s "$EVAL_DATA/work" "$V2/work"

for f in calib_sample.jsonl select_sample.jsonl; do
    [ -f "$HERE/$f" ] || { log "missing $HERE/$f (samples.py)"; exit 2; }
    log "$f sha256 $(shasum -a 256 "$HERE/$f" | awk '{print $1}')"
done

PASSED=""
for w in $WEIGHTS; do
    timed "r1-sweep-$w" "${RECIPE[@]}" --sweep "$ALPHAS" --weights "$w"
    alpha=$(json_field "$WORK/sweep-$w.json" best_alpha)
    log "R1 $w: best alpha $alpha on the selection sample"
    timed "r1-export-$w" "${RECIPE[@]}" --export --alpha "$alpha" --weights "$w"
    rc=0
    timed "r1-gate-$w" "${RECIPE[@]}" --gate --weights "$w" || rc=$?
    if [ $rc = 3 ]; then
        log "G2 $w: R1 missed the gate; R2"
        timed "r2-sensitivity-$w" "${RECIPE[@]}" --sensitivity --alpha "$alpha" --weights "$w"
        rc=0
        timed "r2-greedy-$w" "${RECIPE[@]}" --greedy --alpha "$alpha" --weights "$w" || rc=$?
        if [ $rc = 3 ]; then log "STOP G3 $w: R2 did not reach the selection target within the size budget"; continue; fi
        [ $rc = 0 ] || exit $rc
        timed "r2-export-$w" "${RECIPE[@]}" --export --alpha "$alpha" --weights "$w" --exclude-file "$WORK/exclude-$w.json"
        timed "r2-gate-$w" "${RECIPE[@]}" --gate --weights "$w" || rc=$?
        if [ $rc = 3 ]; then log "STOP G3 $w: R2 missed the gate"; continue; fi
    fi
    [ $rc = 0 ] || exit $rc
    log "gate $w: pass ($(cat "$WORK/gate-$w.json" | tr -d '\n ' | cut -c1-300))"
    PASSED="$PASSED $w"
done

[ -n "$PASSED" ] || { log "STOP G3: no weight form passed the gate; the owner decides (D3: QAT is GM-737's)"; exit 3; }

# J's row is committed with its gm730 pins; its runs must still be those files.
timed m2-check-run "$PY" -I "$REPO/eval/training/gm730/register_arms.py" check-run --runs "$RUNS" --names "$J"

for w in $PASSED; do
    arm=$(arm_of "$w")
    timed "gm703-$w" env ARM="$arm" ARM_MODEL="$MODELS/coderankembed-int8-sq-$w" OUT="$OUT/$arm" GMESH_BIN="$BIN" \
        GM703_CAFFEINATED=1 bash "$HERE/../gm703/measure.sh" --from 3 --until "$GM703_UNTIL"
    timed "s6b-vs-j-$w" "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$J" \
        --json "$OUT/$arm/s6b-report.json" "$RUNS/$J" "$RUNS/$arm" "$RUNS/random" "$RUNS/shuffled"
    timed "s6c-vs-fp32-$w" "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$FP32" \
        --json "$OUT/$arm/s6c-report.json" "$RUNS/$FP32" "$RUNS/$arm" "$RUNS/random" "$RUNS/shuffled"
done

log "done; passed:$PASSED"
