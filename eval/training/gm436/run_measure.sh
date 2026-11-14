#!/usr/bin/env bash
# GM-436 measure run (docs/design/GM-436-laptop-training.md, sections 5-7): one
# fail-fast background script. Run it with bash (not zsh):
#
#   bash eval/training/gm436/run_measure.sh [--dry] [--from STAGE]
#
# Writes $OUT/progress.log (stage lines, every epoch, every encoder pass; read
# by people, not by the agent) and $OUT/summary.txt (one compact line per stage,
# FAILED/STOPPED on the first error). Runs under `caffeinate -i -s`; stops when
# free disk drops below $MIN_FREE_GB (checked before every stage and every
# encoder pass).
#
# D9: every config is chosen on the GM-456 dev split. final_configs.json is
# written once by the selection stage and its sha256 frozen in
# final_configs.sha256; every v2 stage refuses to start unless the file's
# sha256 equals the frozen one.
#
# --dry: one small arm on vectors that already exist (int8-structured, 2 train
# repos, 1 dev repo): import, C0b + its planted fault, a full adapter trained
# uninterrupted and again with a kill after epoch 2 + resume (W sha256 must
# match), Procrustes, export of W.f32 + sha256 + a variants row into a scratch
# copy of v2/variants.toml.
#
# Environment: GMESH_BIN (default <worktree>/target/release/g-mesh),
# GM436_DATA, GM456_DATA, GM436_OUT (default $GM436_DATA/measure or .../dry),
# MIN_FREE_GB (10), THREADS (2), PY (the GM-436 venv python).

set -Eeuo pipefail

if [ -z "${GM436_CAFFEINATED:-}" ]; then
    GM436_CAFFEINATED=1 exec caffeinate -i -s bash "$0" "$@"
fi

DRY=0
FROM=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry) DRY=1 ;;
        --from) FROM="$2"; shift ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
GM436_DATA="${GM436_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm436}"
GM456_DATA="${GM456_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm456}"
EVAL_DATA="${EVAL_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-eval-data/v2}"
PY="${PY:-$GM436_DATA/venv/bin/python}"
BIN="${GMESH_BIN:-$REPO/target/release/g-mesh}"
VERSION="${VERSION:-v0.4-full}"
MINE="$GM456_DATA/$VERSION/mine"
V2="$REPO/eval/embedding/v2"
V2_RUNS="$EVAL_DATA/work/runs"
MIN_FREE_GB="${MIN_FREE_GB:-10}"
THREADS="${THREADS:-2}"
INT8=jina-v2-base-code-int8-structured
GTE=gte-small-structured
BASES="$GTE $INT8"
V2_CORPORA="excalidraw g-mesh gin py-requests ripgrep task-tracker-mcp"

if [ "$DRY" = 1 ]; then OUT="${GM436_OUT:-$GM436_DATA/dry}"; else OUT="${GM436_OUT:-$GM436_DATA/measure}"; fi
mkdir -p "$OUT"
PROGRESS="$OUT/progress.log"
SUMMARY="$OUT/summary.txt"
FINAL="$OUT/final_configs.json"
FROZEN="$OUT/final_configs.sha256"
export GM436_DATA GM456_DATA GM436_PROGRESS="$PROGRESS"
export G_MESH_EMBED_INTRA_THREADS="$THREADS" G_MESH_EMBED_INTER_THREADS=1
STAGE="start"

log() { echo "$(date +%H:%M:%S) $*" >> "$PROGRESS"; }
summary() { echo "$*" >> "$SUMMARY"; log "summary: $*"; }
trap 'summary "FAILED at stage $STAGE (exit $?)"' ERR

disk_guard() {
    local free
    free=$(df -g "$GM436_DATA" | awk 'NR==2 {print $4}')
    if [ "$free" -lt "$MIN_FREE_GB" ]; then
        summary "STOPPED at stage $STAGE: ${free} GB free < ${MIN_FREE_GB} GB"
        trap - ERR
        exit 3
    fi
}

stage() {  # stage <n> <name>: true if this stage runs
    STAGE="$1 $2"
    disk_guard
    if [ "$1" -lt "$FROM" ]; then log "skip stage $STAGE (--from $FROM)"; return 1; fi
    log "stage $STAGE"
    return 0
}

train() {  # train <out dir> <args...>: nice'd, foreground QoS
    local dir="$1"; shift
    nice -n 10 "$PY" "$HERE/train_adapter.py" --version "$VERSION" --threads "$THREADS" --out "$dir" "$@" >> "$OUT/train.log" 2>&1
}

w_sha() { "$PY" -c 'import json,sys; print(json.load(open(sys.argv[1]))["wSha256"])' "$1/result.json"; }
last_sha() { "$PY" -c 'import json,sys; print(json.load(open(sys.argv[1]))["lastWSha256"])' "$1/result.json"; }
dev_mrr() { "$PY" -c 'import json,sys; print(round(json.load(open(sys.argv[1]))["dev"]["mrr"], 4))' "$1/result.json"; }

check_frozen() {
    [ -f "$FINAL" ] && [ -f "$FROZEN" ] || { summary "D9: no frozen final_configs.json; v2 stages refused"; false; }
    local now; now=$(shasum -a 256 "$FINAL" | awk '{print $1}')
    [ "$now" = "$(cat "$FROZEN")" ] || { summary "D9: final_configs.json sha256 $now differs from the frozen $(cat "$FROZEN")"; false; }
}

: > "$SUMMARY"
summary "GM-436 measure $( [ "$DRY" = 1 ] && echo DRY ) $(date '+%F %T'); uptime: $(uptime | sed 's/.*load/load/')"
"$PY" -c 'import torch, numpy; assert torch.__version__.startswith("2.2.2") and numpy.__version__ == "1.26.4"'

# ------------------------------------------------------------------- dry run
if [ "$DRY" = 1 ]; then
    TRAIN_REPOS="zustand bytes"
    DEV_REPOS="mini-redis"
    COMMON=(--base "$INT8" --train-repos $TRAIN_REPOS --dev-repos $DEV_REPOS --batch 128 --epochs 4 --patience 3 --resamples 1000)
    if stage 1 import; then
        "$PY" "$HERE/cache.py" import --version "$VERSION" --variant "$INT8" --repos $TRAIN_REPOS $DEV_REPOS >> "$OUT/train.log" 2>&1
        summary "import: $INT8 $TRAIN_REPOS $DEV_REPOS ok"
    fi
    if stage 2 c0b; then
        "$PY" "$HERE/dev_metric.py" c0b --version "$VERSION" --variant "$INT8" --repos $DEV_REPOS --json "$OUT/c0b.json" >> "$OUT/train.log" 2>&1
        if "$PY" "$HERE/dev_metric.py" c0b --version "$VERSION" --variant "$INT8" --repos $DEV_REPOS --plant-offset >> "$OUT/train.log" 2>&1; then
            summary "C0b planted offset did NOT fail"; false
        fi
        summary "C0b: pass; planted offset fails as it must"
    fi
    if stage 3 train; then
        rm -rf "$OUT/full-a" "$OUT/full-b" "$OUT/procrustes" "$OUT/r2"
        train "$OUT/full-a" "${COMMON[@]}" --form full --tau 0.05 --lam 0.001
        rc=0; train "$OUT/full-b" "${COMMON[@]}" --form full --tau 0.05 --lam 0.001 --stop-after-epochs 2 || rc=$?
        [ "$rc" = 75 ] || { summary "kill run exited $rc, expected 75"; false; }
        if [ ! -f "$OUT/full-b/checkpoint.pt" ] || [ -f "$OUT/full-b/result.json" ]; then
            summary "kill run left no checkpoint, or a finished result"; false
        fi
        train "$OUT/full-b" "${COMMON[@]}" --form full --tau 0.05 --lam 0.001
        a=$(w_sha "$OUT/full-a"); b=$(w_sha "$OUT/full-b")
        [ "$a" = "$b" ] || { summary "resume: best W sha256 differs ($a vs $b)"; false; }
        la=$(last_sha "$OUT/full-a"); lb=$(last_sha "$OUT/full-b")
        [ "$la" = "$lb" ] || { summary "resume: last-epoch W sha256 differs ($la vs $lb)"; false; }
        train "$OUT/procrustes" "${COMMON[@]}" --form procrustes
        tdir=$("$PY" "$HERE/cache.py" teacher --version "$VERSION" --variant "$INT8" --repos $TRAIN_REPOS $DEV_REPOS --out-root "$OUT/teacher" 2>> "$OUT/train.log" | tail -1)
        train "$OUT/r2" --base "$INT8" --train-repos $TRAIN_REPOS --dev-repos $DEV_REPOS --batch 128 --epochs 2 --resamples 1000 \
            --form lowrank --rank 32 --route r2 --alpha 0.5 --teacher-dir "$tdir"
        summary "train: full dev MRR $(dev_mrr "$OUT/full-a") (epoch-0 $("$PY" -c 'import json,sys; print(round(json.load(open(sys.argv[1]))["history"][0]["mrr"],4))' "$OUT/full-a/result.json")), resume W sha256 identical ${a:0:12}; last-epoch W identical ${la:0:12}; procrustes dev MRR $(dev_mrr "$OUT/procrustes"); R2 smoke (int8 teaching itself, low-rank) dev MRR $(dev_mrr "$OUT/r2")"
    fi
    if stage 4 export; then
        cp "$V2/variants.toml" "$OUT/variants.scratch.toml"
        rm -rf "$OUT/export"
        "$PY" "$HERE/export_adapter.py" --run "$OUT/full-b" --name "dry-int8-structured-adapter-full" \
            --reference "$INT8" --role informational --dest "$OUT/export" --append "$OUT/variants.scratch.toml" >> "$OUT/train.log" 2>&1
        sha=$(awk '{print $1}' "$OUT/export/W.f32.sha256")
        [ "$sha" = "$(w_sha "$OUT/full-b")" ] || { summary "export sha256 differs from result.json"; false; }
        grep -q "adapter_sha256 = \"$sha\"" "$OUT/variants.scratch.toml"
        summary "export: $OUT/export/W.f32 ($(stat -f %z "$OUT/export/W.f32") bytes) sha256 ${sha:0:12}; row appended to scratch variants.toml"
    fi
    summary "DRY DONE"
    exit 0
fi

# ------------------------------------------------------------------- full run
[ -x "$BIN" ] || { summary "no g-mesh binary at $BIN"; false; }
ALL_REPOS=$("$PY" -c 'import sys, json; print(" ".join(sorted({json.loads(l)["repo"] for l in open(sys.argv[1])})))' "$GM456_DATA/$VERSION/queries.jsonl")
DEV_ALL=$("$PY" -c 'import sys, json; print(" ".join(sorted({json.loads(l)["repo"] for l in open(sys.argv[1]) if json.loads(l)["split"] == "dev"})))' "$GM456_DATA/$VERSION/queries.jsonl")

if stage 1 encode-gte; then
    n=0; total=$(echo $ALL_REPOS | wc -w | tr -d ' ')
    for repo in $ALL_REPOS; do
        disk_guard
        nice -n 10 "$BIN" debug-embed-eval run --eval-dir "$MINE" --variant "$GTE" --corpus "$repo" \
            --out "$GM456_DATA/work/runs" >> "$OUT/encode.log" 2>&1
        n=$((n + 1)); log "encode $GTE $repo ($n/$total)"
    done
    summary "encode: $GTE over $total GM-456 repos"
fi

if stage 2 import; then
    for v in $BASES; do
        "$PY" "$HERE/cache.py" import --version "$VERSION" --variant "$v" >> "$OUT/train.log" 2>&1
        log "imported $v"
    done
    summary "import: $BASES"
fi

if stage 3 c0b; then
    "$PY" "$HERE/dev_metric.py" c0b --version "$VERSION" --variant "$INT8" --repos $DEV_ALL --json "$OUT/c0b.json" >> "$OUT/train.log" 2>&1
    if "$PY" "$HERE/dev_metric.py" c0b --version "$VERSION" --variant "$INT8" --repos $DEV_ALL --plant-offset >> "$OUT/train.log" 2>&1; then
        summary "C0b planted offset did NOT fail"; false
    fi
    summary "C0b: pass on $DEV_ALL; planted offset fails"
fi

TEACHER_DIR_FILE="$OUT/teacher_dir.txt"
if stage 4 teacher; then
    "$PY" "$HERE/cache.py" teacher --version "$VERSION" --variant "$INT8" > "$TEACHER_DIR_FILE" 2>> "$OUT/train.log"
    summary "teacher: $(tail -1 "$TEACHER_DIR_FILE")"
fi
TEACHER_DIR=$(tail -1 "$TEACHER_DIR_FILE" 2>/dev/null || true)

if stage 5 grid-r1; then
    total=$(( $("$PY" "$HERE/grid.py" r1 | wc -l) * 2 )); n=0
    for v in $BASES; do
        while IFS=$'\t' read -r name args; do
            train "$OUT/grid/$v/$name" --base "$v" $args
            n=$((n + 1)); log "grid r1 $v/$name ($n/$total) dev MRR $(dev_mrr "$OUT/grid/$v/$name")"
        done < <("$PY" "$HERE/grid.py" r1)
    done
    "$PY" "$HERE/grid.py" select --grid "$OUT/grid" --out "$OUT/select_r1.json" > /dev/null
    summary "grid R1: $total runs; best per base: $(for v in $BASES; do printf '%s=%s ' "$v" "$("$PY" "$HERE/grid.py" get --final "$OUT/select_r1.json" "r1.$v.bestTrainable")"; done)"
fi

if stage 6 grid-r2; then
    form=$("$PY" "$HERE/grid.py" get --final "$OUT/select_r1.json" "r1.$GTE.bestTrainable")
    while IFS=$'\t' read -r name args; do
        train "$OUT/grid-r2/$name" --base "$GTE" --teacher-dir "$TEACHER_DIR" $args
        log "grid r2 $name dev MRR $(dev_mrr "$OUT/grid-r2/$name")"
    done < <("$PY" "$HERE/grid.py" r2 --form-name "$form")
    summary "grid R2 on $GTE/$form done"
fi

if stage 7 freeze; then
    "$PY" "$HERE/grid.py" select --grid "$OUT/grid" --r2-grid "$OUT/grid-r2" --r2-base "$GTE" --out "$FINAL.new" > /dev/null
    if [ -f "$FROZEN" ]; then
        [ "$(shasum -a 256 "$FINAL.new" | awk '{print $1}')" = "$(cat "$FROZEN")" ] || { summary "D9: a frozen final_configs.json exists with another sha256; not overwritten"; false; }
        rm "$FINAL.new"
    else
        mv "$FINAL.new" "$FINAL"
        shasum -a 256 "$FINAL" | awk '{print $1}' > "$FROZEN"
    fi
    summary "D9 frozen: final_configs.json sha256 $(cat "$FROZEN") (write it into the results doc before any v2 run)"
fi

get() { "$PY" "$HERE/grid.py" get --final "$FINAL" "$1"; }

if stage 8 finals; then
    check_frozen
    for v in $BASES; do
        best=$(get "r1.$v.bestTrainable")
        args=$(get "r1.$v.forms.${best%%-*}.args")
        for f in 0.25 0.5; do train "$OUT/final/$v/curve-$f" --base "$v" $args --fraction "$f"; done
        train "$OUT/final/$v/c1" --base "$v" $args --control c1
        train "$OUT/final/$v/c5" --base "$v" $args
        [ "$(w_sha "$OUT/final/$v/c5")" = "$(w_sha "$OUT/grid/$v/$best")" ] || { summary "C5: $v retrain W sha256 differs"; false; }
        "$PY" "$HERE/dev_metric.py" curve --r25 "$OUT/final/$v/curve-0.25" --r50 "$OUT/final/$v/curve-0.5" \
            --r100 "$OUT/final/$v/c5" --json "$OUT/final/$v/curve.json" >> "$OUT/summary.curve.txt"
        log "finals $v done"
    done
    best=$(get "r1.$GTE.bestTrainable")
    train "$OUT/final/$GTE/c4-no-go" --base "$GTE" $(get "r1.$GTE.forms.${best%%-*}.args") --exclude-language go
    r2args=$(get "r2.args")
    for f in 0.25 0.5 1.0; do train "$OUT/final/r2/f$f" --base "$GTE" --teacher-dir "$TEACHER_DIR" $r2args --fraction "$f"; done
    train "$OUT/final/r2/c2" --base "$GTE" --teacher-dir "$TEACHER_DIR" $r2args --control c2
    "$PY" "$HERE/dev_metric.py" curve --r25 "$OUT/final/r2/f0.25" --r50 "$OUT/final/r2/f0.5" --r100 "$OUT/final/r2/f1.0" \
        --json "$OUT/final/r2/curve.json" >> "$OUT/summary.curve.txt"
    summary "finals: curves, C1, C4, C5 (W sha256 identical), R2 25/50/100 + C2; curve lines in summary.curve.txt"
fi

ARMS_FILE="$OUT/v2_arms.txt"
if stage 9 export; then
    check_frozen
    : > "$ARMS_FILE"
    exp() {  # exp <arm name> <reference> <role> <run dir | --identity D>
        local name="$1" ref="$2" role="$3"; shift 3
        if grep -q "^name = \"$name\"$" "$V2/variants.toml"; then log "row $name exists"; else
            if [ "$1" = "--identity" ]; then
                "$PY" "$HERE/export_adapter.py" --identity "$2" --name "$name" --reference "$ref" --role "$role" --append "$V2/variants.toml" >> "$OUT/train.log"
            else
                "$PY" "$HERE/export_adapter.py" --run "$1" --name "$name" --reference "$ref" --role "$role" --append "$V2/variants.toml" >> "$OUT/train.log"
            fi
        fi
        echo "$name" >> "$ARMS_FILE"
    }
    for v in $BASES; do
        short=${v%-structured}; short=${short/jina-v2-base-code-/}; short=${short/-small/}
        role=cost; [ "$v" = "$INT8" ] && role=quality
        dim=$("$PY" -c 'import json,sys; print(json.load(open(sys.argv[1]))["dim"])' "$OUT/grid/$v/procrustes/result.json")
        for form in full lowrank procrustes; do
            exp "$short-structured-adapter-$form" "$v" "$role" "$OUT/grid/$v/$(get "r1.$v.forms.$form.name")"
        done
        best=$(get "r1.$v.bestTrainable")
        for f in 0.25 0.5; do exp "$short-structured-adapter-curve$f" "$v" informational "$OUT/final/$v/curve-$f"; done
        exp "$short-structured-adapter-c1" "$v" informational "$OUT/final/$v/c1"
        exp "$short-structured-identity" "$v" informational --identity "$dim"
        exp "$short-structured-ws-adapter-${best%%-*}" "$v-words-shuffled" informational "$OUT/grid/$v/$best"
        exp "$short-structured-ws-identity" "$v-words-shuffled" informational --identity "$dim"
    done
    exp "gte-structured-adapter-c4-no-go" "$GTE" informational "$OUT/final/$GTE/c4-no-go"
    for f in 0.25 0.5 1.0; do exp "gte-structured-distill-f$f" "$GTE" cost "$OUT/final/r2/f$f"; done
    exp "gte-structured-distill-c2" "$GTE" informational "$OUT/final/r2/c2"
    summary "export: $(wc -l < "$ARMS_FILE" | tr -d ' ') adapter rows in v2/variants.toml"
fi

if stage 10 v2-runs; then
    check_frozen
    [ -x "$BIN" ]
    n=0
    arms="$GTE $GTE-words-shuffled $INT8-words-shuffled $(cat "$ARMS_FILE")"
    total=$(( $(echo $arms | wc -w) * 6 ))
    for arm in $arms; do
        for c in $V2_CORPORA; do
            disk_guard
            nice -n 10 "$BIN" debug-embed-eval run --eval-dir "$V2" --variant "$arm" --corpus "$c" --out "$V2_RUNS" >> "$OUT/v2.log" 2>&1
            n=$((n + 1))
        done
        log "v2 $arm ($n/$total)"
    done
    summary "v2 runs: $total (arm, corpus) runs"
fi

if stage 11 report; then
    check_frozen
    arm_args=()
    for arm in jina-v2-base-code-fp32 random shuffled $INT8 $GTE $(cat "$ARMS_FILE"); do arm_args+=(--arm "$arm"); done
    ws=()
    for v in $BASES; do
        short=${v%-structured}; short=${short/jina-v2-base-code-/}; short=${short/-small/}
        best=$(get "r1.$v.bestTrainable")
        ws+=(--ws-pair "$short-structured-adapter-${best%%-*}=$short-structured-ws-adapter-${best%%-*}")
        ws+=(--ws-pair "$short-structured-identity=$short-structured-ws-identity")
    done
    "$PY" "$HERE/report_gm436.py" --bin "$BIN" --runs "$V2_RUNS" --out "$OUT/report" "${arm_args[@]}" "${ws[@]}" >> "$OUT/report.log" 2>&1
    "$PY" "$HERE/hier_headroom.py" --runs "$V2_RUNS" --arm "$INT8" --arm "$GTE" --json "$OUT/report/hier_headroom.json" >> "$OUT/summary.hier.txt"
    summary "report: $OUT/report/results.md; headroom: summary.hier.txt"
fi

summary "DONE"
