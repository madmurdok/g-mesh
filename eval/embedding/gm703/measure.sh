#!/usr/bin/env bash
# GM-703 measure (docs/design/GM-703-coderankembed.md, section 7): export,
# parity, the int8 eval arm on v2, Rust-vs-Python query parity, the cost pass
# and the report. One background run, fail-fast; summary in $OUT/summary.txt.
#
# usage: measure.sh [--from N] [--until N]
#        measure.sh --check-export <model dir>   # model.onnx sha256 vs EXPORT.json, then exit
#
# Steps: 0 fetch, 1 export, 2 parity, 3 build, 4 int8 runs, 5 P3,
#        6 report vs int8-structured (S6), 8 cost pass, 9 report set.
# The cost pass runs whatever S6 says.
#
# Environment: GM703_DATA, EVAL_DATA, GMESH_BIN, OUT (default $GM703_DATA/measure), PY,
# P3_INT8_RUNS and P3_INT8_CORPORA (step 5's int8 runs; default the step-4 runs).

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
P3_CORPUS=task-tracker-mcp
# int8 P3 reads the step-4 runs; a smoke run can point it at other runs.
P3_INT8_RUNS="${P3_INT8_RUNS:-$RUNS}"
P3_INT8_CORPORA="${P3_INT8_CORPORA:-$V2_CORPORA}"

export HF_HOME="$GM703_DATA/hf" TOKENIZERS_PARALLELISM=false
mkdir -p "$OUT"
SUMMARY="$OUT/summary.txt"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$SUMMARY"; }
trap 'log "FAILED at line $LINENO (rc $?)"' ERR
step() { [ "$1" -ge "$FROM" ] && [ "$1" -le "$UNTIL" ]; }
# Runs "$@" under /usr/bin/time -lp, logs uptime before and after and user/sys/real.
timed() {
    local name="$1"; shift
    log "$name: start; uptime $(uptime)"
    local rc=0
    /usr/bin/time -lp "$@" > "$OUT/$name.out" 2> "$OUT/$name.err" || rc=$?
    log "$name: rc=$rc $(grep -E '^(real|user|sys) ' "$OUT/$name.err" | tr '\n' ' ')maxrss $(awk '/maximum resident set size/{print $1}' "$OUT/$name.err"); uptime $(uptime)"
    return $rc
}

log "measure.sh from $FROM until $UNTIL; HEAD $(git -C "$REPO" rev-parse --short HEAD)"

# The v2 eval dir's work/ is the shared v2 work dir (snapshots, models, runs).
[ -e "$V2/work" ] || ln -s "$EVAL_DATA/work" "$V2/work"

if step 0; then
    [ -x "$PY" ] || { uv venv -q --python 3.11 "$GM703_DATA/venv" && uv pip install -q --python "$PY" -r "$HERE/requirements.txt"; }
    timed s0-fetch "$PY" -I "$HERE/export_coderankembed.py" --fetch
    cat "$OUT/s0-fetch.out" >> "$SUMMARY"
fi
export HF_HUB_OFFLINE=1

if step 1; then
    timed s1-export "$PY" -I "$HERE/export_coderankembed.py" --out "$MODELS"
    cat "$OUT/s1-export.out" >> "$SUMMARY"
    ls -l "$MODELS"/coderankembed-*/model.onnx >> "$SUMMARY"
fi

if step 2; then
    rc=0
    timed s2-parity "$PY" -I "$HERE/parity.py" --out "$OUT" --models "$MODELS" || rc=$?
    case $rc in
        0) log "parity: all checks pass" ;;
        2) log "STOP S1: P1/P1b/P1c/P1d out of tolerance (parity.json)"; exit 2 ;;
        3) log "STOP S2: Q-F or Q-D failed (parity.json)"; exit 3 ;;
        *) log "parity.py failed rc=$rc"; exit "$rc" ;;
    esac
fi

if step 3; then
    timed s3-build cargo build --release --features embed-eval --bin g-mesh --manifest-path "$REPO/Cargo.toml"
fi

check_export "$MODELS/coderankembed-int8"
check_export "$MODELS/coderankembed-fp32"

if step 4; then
    for c in $V2_CORPORA; do
        timed "s4-run-$c" nice -n 10 "$BIN" debug-embed-eval run --eval-dir "$V2" --variant "$INT8" \
            --corpus "$c" --out "$RUNS" --force
    done
fi

if step 5; then
    # fp32 is the gated check (S4); int8 is reported.
    timed s5-run-fp32 "$BIN" debug-embed-eval run --eval-dir "$V2" --variant "$FP32" \
        --corpus "$P3_CORPUS" --out "$OUT/p3-runs" --force
    rc=0
    timed s5-p3-fp32 "$PY" -I "$HERE/parity.py" --rust-queries "$OUT/p3-runs/$FP32/$P3_CORPUS" \
        --model-dir "$MODELS/coderankembed-fp32" --eval-dir "$V2" --out "$OUT" --name "p3-fp32.json" || rc=$?
    [ $rc = 0 ] || { log "STOP S4: P3 fp32 below cosine 0.99999 (p3-fp32.json)"; exit 4; }
    for c in $P3_INT8_CORPORA; do
        timed "s5-p3-int8-$c" "$PY" -I "$HERE/parity.py" --rust-queries "$P3_INT8_RUNS/$INT8/$c" \
            --model-dir "$MODELS/coderankembed-int8" --eval-dir "$V2" --out "$OUT" \
            --name "p3-int8-$c.json" --min-cosine 0
    done
fi

if step 6; then
    timed s6-report "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$JINA" \
        --json "$OUT/s6-report.json" "$RUNS/$JINA" "$RUNS/$INT8" "$RUNS/random" "$RUNS/shuffled"
    tail -n 40 "$OUT/s6-report.out" >> "$SUMMARY"
fi

if step 8; then
    timed s8-costs env REF="$JINA" VARIANTS="$INT8" EVAL_DIR="$V2" SKIP_QUALITY=1 CONTROLS="random shuffled" \
        RUNS="$RUNS" OUT="$OUT/costs" bash "$REPO/eval/embedding/measure_costs.sh"
fi

if step 9; then
    timed s9-report "$PY" -I "$REPO/eval/training/gm436/report_gm436.py" --bin "$BIN" --runs "$RUNS" \
        --eval-dir "$V2" --reference "$JINA" --arm "$JINA" --arm "$INT8" --out "$OUT/report"
    timed s9-report-costs "$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$JINA" \
        --costs "$OUT/costs/costs.toml" --json "$OUT/report-costs.json" \
        "$RUNS/$JINA" "$RUNS/$INT8" "$RUNS/random" "$RUNS/shuffled"
fi

log "done"
