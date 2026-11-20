#!/usr/bin/env bash
# GM-703 measure (docs/design/GM-703-coderankembed.md, section 7): export,
# parity, one CodeRankEmbed eval arm on v2 (ARM, default fp32), Rust-vs-Python
# query parity, the cost pass and the report. One background run, fail-fast; summary in $OUT/summary.txt.
#
# usage: measure.sh [--from N] [--until N]
#        measure.sh --check-export <model dir>   # model.onnx sha256 vs EXPORT.json, then exit
#
# Steps: 0 fetch, 1 export, 2 parity, 3 build, 4 the arm's runs, 5 P3,
#        6 report vs int8-structured (S6), 8 cost pass, 9 report set.
# The cost pass runs whatever S6 says.
#
# Environment: GM703_DATA, EVAL_DATA, GMESH_BIN, OUT (default $GM703_DATA/measure), PY,
# ARM (coderankembed-fp32-structured or coderankembed-int8-structured),
# P3_RUNS and P3_CORPORA (step 5's runs; default the step-4 runs, all corpora),
# DRY=1 (log each step's command instead of running it).

set -Eeuo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"

# model.onnx must be the file EXPORT.json records: the run manifest's
# fingerprint does not cover the ONNX bytes, so a stale export would be reused.
check_export() {
    local dir="$1" want got
    [ -f "$dir/model.onnx" ] || { echo "check_export: $dir/model.onnx missing" >&2; return 1; }
    [ -f "$dir/EXPORT.json" ] || { echo "check_export: $dir/EXPORT.json missing" >&2; return 1; }
    want=$(sed -n 's/.*"model_onnx_sha256": "\([0-9a-f]*\)".*/\1/p' "$dir/EXPORT.json")
    got=$(shasum -a 256 "$dir/model.onnx" | awk '{print $1}')
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
        echo "check_export: $dir/model.onnx sha256 $got, EXPORT.json records '${want}'" >&2
        return 1
    fi
}

if [ "${1:-}" = "--check-export" ]; then
    check_export "${2:?model dir}"
    exit
fi

if [ -z "${GM703_CAFFEINATED:-}" ]; then
    GM703_CAFFEINATED=1 exec caffeinate -i -s bash "$0" "$@"
fi

FROM=0
UNTIL=99
while [ $# -gt 0 ]; do
    case "$1" in
        --from) FROM="$2"; shift ;;
        --until) UNTIL="$2"; shift ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

GM703_DATA="${GM703_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm703}"
EVAL_DATA="${EVAL_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-eval-data/v2}"
PY="${PY:-$GM703_DATA/venv/bin/python}"
BIN="${GMESH_BIN:-$REPO/target/release/g-mesh}"
OUT="${OUT:-$GM703_DATA/measure}"
MODELS="$GM703_DATA/models"
V2="$REPO/eval/embedding/v2"
RUNS="$EVAL_DATA/work/runs"
INT8=coderankembed-int8-structured
FP32=coderankembed-fp32-structured
JINA=jina-v2-base-code-int8-structured
V2_CORPORA="excalidraw g-mesh gin py-requests ripgrep task-tracker-mcp"
ARM="${ARM:-$FP32}"
case "$ARM" in
    "$FP32") ARM_MODEL="$MODELS/coderankembed-fp32" ;;
    "$INT8") ARM_MODEL="$MODELS/coderankembed-int8" ;;
    *) echo "ARM must be $FP32 or $INT8, not $ARM" >&2; exit 2 ;;
esac
P3_RUNS="${P3_RUNS:-$RUNS}"
P3_CORPORA="${P3_CORPORA:-$V2_CORPORA}"
DRY="${DRY:-0}"

export HF_HOME="$GM703_DATA/hf" TOKENIZERS_PARALLELISM=false
mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$SUMMARY"; }
trap 'log "FAILED at line $LINENO (rc $?)"' ERR
step() { [ "$1" -ge "$FROM" ] && [ "$1" -le "$UNTIL" ]; }
# Runs "$@" under /usr/bin/time -lp, logs uptime before and after and user/sys/real.
timed() {
    local name="$1"; shift
    if [ "$DRY" = 1 ]; then log "DRY $name: $*"; return 0; fi
    log "$name: start; uptime $(uptime)"
    local rc=0
    /usr/bin/time -lp "$@" > "$OUT/$name.out" 2> "$OUT/$name.err" || rc=$?
    log "$name: rc=$rc $(grep -E '^(real|user|sys) ' "$OUT/$name.err" | tr '\n' ' ')maxrss $(awk '/maximum resident set size/{print $1}' "$OUT/$name.err"); uptime $(uptime)"
    return $rc
}

log "measure.sh arm $ARM from $FROM until $UNTIL; HEAD $(git -C "$REPO" rev-parse --short HEAD)"

# The v2 eval dir's work/ is the shared v2 work dir (snapshots, models, runs).
[ -e "$V2/work" ] || ln -s "$EVAL_DATA/work" "$V2/work"

if step 0; then
    [ -x "$PY" ] || { uv venv -q --python 3.11 "$GM703_DATA/venv" && uv pip install -q --python "$PY" -r "$HERE/requirements.txt"; }
    timed s0-fetch "$PY" -I "$HERE/export_coderankembed.py" --fetch
    [ "$DRY" = 1 ] || cat "$OUT/s0-fetch.out" >> "$SUMMARY"
fi
export HF_HUB_OFFLINE=1

if step 1; then
    timed s1-export "$PY" -I "$HERE/export_coderankembed.py" --out "$MODELS"
    [ "$DRY" = 1 ] || cat "$OUT/s1-export.out" >> "$SUMMARY"
    [ "$DRY" = 1 ] || ls -l "$MODELS"/coderankembed-*/model.onnx >> "$SUMMARY"
fi

if step 2; then
    rc=0
    timed s2-parity "$PY" -I "$HERE/parity.py" --out "$OUT" --models "$MODELS" || rc=$?
    case $rc in
        0) log "parity: all checks pass" ;;
        2) log "STOP S1: P1/P1b/P1c/P1d out of tolerance (parity.json)"; exit 2 ;;
        3)
            # Q-F/Q-D judge only the int8 file.
            if [ "$ARM" = "$INT8" ]; then log "STOP S2: Q-F or Q-D failed (parity.json)"; exit 3; fi
            log "parity: Q-F or Q-D failed (int8 only); arm $ARM continues" ;;
        *) log "parity.py failed rc=$rc"; exit "$rc" ;;
    esac
fi

if step 3; then
    timed s3-build cargo build --release --features embed-eval --bin g-mesh --manifest-path "$REPO/Cargo.toml"
fi

if step 4; then
    check_export "$ARM_MODEL"
    for c in $V2_CORPORA; do
        timed "s4-run-$c" nice -n 10 "$BIN" debug-embed-eval run --eval-dir "$V2" --variant "$ARM" \
            --corpus "$c" --out "$RUNS" --force
    done
fi

if step 5; then
    # P3 on the arm's own runs; gated (S4) for fp32, reported for int8.
    min_cos=0.99999
    [ "$ARM" = "$INT8" ] && min_cos=0
    for c in $P3_CORPORA; do
        rc=0
        timed "s5-p3-$c" "$PY" -I "$HERE/parity.py" --rust-queries "$P3_RUNS/$ARM/$c" \
            --model-dir "$ARM_MODEL" --eval-dir "$V2" --out "$OUT" --name "p3-$ARM-$c.json" \
            --min-cosine "$min_cos" || rc=$?
        [ $rc = 0 ] || { log "STOP S4: P3 on $c below cosine $min_cos (p3-$ARM-$c.json)"; exit 4; }
    done
fi

if step 6; then
    # S6 decides the verdict only; the cost pass runs either way.
    timed s6-report "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$JINA" \
        --json "$OUT/s6-report.json" "$RUNS/$JINA" "$RUNS/$ARM" "$RUNS/random" "$RUNS/shuffled"
    [ "$DRY" = 1 ] || tail -n 40 "$OUT/s6-report.out" >> "$SUMMARY"
fi

if step 8; then
    timed s8-costs env REF="$JINA" VARIANTS="$ARM" EVAL_DIR="$V2" SKIP_QUALITY=1 CONTROLS="random shuffled" \
        RUNS="$RUNS" OUT="$OUT/costs" bash "$REPO/eval/embedding/measure_costs.sh"
fi

if step 9; then
    timed s9-report "$PY" -I "$REPO/eval/training/gm436/report_gm436.py" --bin "$BIN" --runs "$RUNS" \
        --eval-dir "$V2" --reference "$JINA" --arm "$JINA" --arm "$ARM" --out "$OUT/report"
    timed s9-report-costs "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$JINA" \
        --costs "$OUT/costs/costs.toml" --json "$OUT/report-costs.json" \
        "$RUNS/$JINA" "$RUNS/$ARM" "$RUNS/random" "$RUNS/shuffled"
fi

log "done"
