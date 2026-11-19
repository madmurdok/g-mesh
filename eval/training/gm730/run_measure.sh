#!/usr/bin/env bash
# GM-730 laptop side (docs/design/GM-730-gpu-training-routes.md, sections 1-3, 6
# and 8): one fail-fast background script per call. Run it with bash (not zsh):
#
#   bash eval/training/gm730/run_measure.sh --dry
#   bash eval/training/gm730/run_measure.sh pre
#   bash eval/training/gm730/run_measure.sh teacher-data --run <pulled RR-T run dir>
#   bash eval/training/gm730/run_measure.sh freeze --route rr|jina|gte --run <pulled grid run dir> [--no-teacher]
#   bash eval/training/gm730/run_measure.sh eval --route rr|jina|gte --run <pulled final run dir>
#
# The GPU jobs themselves are launched by the orchestrator
# (`uv run eval/gpu/gpu_train.py run eval/gpu/jobs/gm730-<job>.toml`), one at a
# time, and pulled (`gpu_train.py pull`) before the next call here.
#
# --dry: the CPU smoke of every trainer on a tiny subset (train: first windows
#   of zustand, dev: first windows of ky): teacher scoring, train_ce.py and
#   train_encoder.py (gte-small full, jina LoRA, jina full), each grid run
#   uninterrupted and again stopped after a step past the first epoch end
#   (--stop-after-steps, exit 75) and resumed: result.json bytes and the last
#   parameters' sha256 must be equal. Final stages with their controls export,
#   quantise and pass C-P. make_jobs.py + freeze.py run on the smoke results.
# pre: C-R per model (export.py recipe-control; a failing model's route does
#   not start, the others continue; FAILED only when every model fails), the
#   plants (must fail with their own FAIL line), C-0 (report of the GM-436 runs,
#   eval_checks.py c0), windows (train, dev; planted offset must fail its join
#   check), C-T pairs, job TOMLs (must equal the committed ones).
# teacher-data: the pulled RR-T outputs copied to $GM730_DATA/teacher (the
#   gm730-teacher data set); C-T must have passed. On a C-T failure it writes
#   the --no-teacher grid TOMLs (gm730-{jina,gte}-grid-nt.toml) and fails.
# freeze: freeze.py on a pulled grid run, then make_jobs.py final.
# eval: register the final run's exports as arms, then the route's v2 runs and
#   reports (J, R3: debug-embed-eval run/report; RR: gm463_bakeoff.py quality,
#   after the ce-minilm reference cache is complete), then eval_checks.py:
#   S5 on dev, C-P2, C-S, C-R2 (J, R3); any failed rule is FAILED.
# Before each GPU job: python eval/training/gm730/budget.py check --job <toml>.
#
# Writes $OUT/progress.log (read by people) and $OUT/summary.txt (one line per
# stage; FAILED on the first error). Environment: PY (the gm730 venv python),
# GMESH_BIN (default <worktree>/target/release/g-mesh, built with
# --features embed-eval), GM730_DATA, GM456_DATA, EVAL_DATA, GM730_OUT, THREADS.

# shellcheck disable=SC2086 # arm and corpus lists are space-separated strings
set -Eeuo pipefail

if [ -z "${GM730_CAFFEINATED:-}" ]; then
    GM730_CAFFEINATED=1 exec caffeinate -i -s bash "$0" "$@"
fi

CMD="${1:-}"; shift || true
ROUTE=""; RUN=""; NO_TEACHER=""
while [ $# -gt 0 ]; do
    case "$1" in
        --route) ROUTE="$2"; shift ;;
        --run) RUN="$2"; shift ;;
        --no-teacher) NO_TEACHER="--no-teacher" ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
GM730_DATA="${GM730_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm730}"
GM456_DATA="${GM456_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm456}"
EVAL_DATA="${EVAL_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-eval-data/v2}"
PY="${PY:-$GM730_DATA/venv-probe/bin/python}"
BIN="${GMESH_BIN:-$REPO/target/release/g-mesh}"
THREADS="${THREADS:-4}"
V2="$REPO/eval/embedding/v2"
V2_RUNS="$EVAL_DATA/work/runs"
V2_CORPORA="excalidraw g-mesh gin py-requests ripgrep task-tracker-mcp"
INT8=jina-v2-base-code-int8-structured
GTE=gte-small-structured
ADAPTER=int8-structured-adapter-full
FINAL="$GM730_DATA/final_configs.json"
case "$CMD" in
    --dry) OUT="${GM730_OUT:-$GM730_DATA/dry}" ;;
    pre|teacher-data|freeze|eval) OUT="${GM730_OUT:-$GM730_DATA/measure}" ;;
    *) echo "usage: run_measure.sh --dry | pre | teacher-data --run DIR | freeze --route R --run DIR | eval --route R --run DIR" >&2; exit 2 ;;
esac
mkdir -p "$OUT"
PROGRESS="$OUT/progress.log"
SUMMARY="$OUT/summary.txt"
export GM730_DATA GM456_DATA GM730_PROGRESS="$PROGRESS"
STAGE="start"

log() { echo "$(date +%H:%M:%S) $*" >> "$PROGRESS"; }
summary() { echo "$*" >> "$SUMMARY"; log "summary: $*"; }
trap 'summary "FAILED at stage $STAGE (exit $?)"' ERR
stage() { STAGE="$1"; log "stage $STAGE"; }
jget() { "$PY" -c 'import json,sys; d=json.load(open(sys.argv[1]))
for k in sys.argv[2].split("."): d=d[k]
print(d)' "$1" "$2"; }
timed() {  # timed <log> <cmd...>: run, append /usr/bin/time -p to the log; rc kept
    local lg="$1"; shift
    local rc=0
    /usr/bin/time -p "$@" >> "$lg" 2>&1 || rc=$?
    return $rc
}
times_of() { grep -E '^(real|user|sys) ' "$1" | tail -3 | tr '\n' ' '; }

summary "GM-730 $CMD $(date '+%F %T'); uptime: $(uptime | sed 's/.*load/load/')"
"$PY" -c 'import torch, transformers, numpy; assert torch.__version__.startswith("2.2.2") and transformers.__version__ == "4.48.3" and numpy.__version__ == "1.26.4"'

# ---------------------------------------------------------------------------- dry
if [ "$CMD" = "--dry" ]; then
    S="$OUT"
    rm -rf "$S/windows" "$S/ct" "$S"/teacher-* "$S"/ce-* "$S"/gte-* "$S"/jl-* "$S"/jf-* "$S/jobs" "$S/fake-run"
    export GM730_WINDOWS="$S/windows" GM730_CT="$S/ct" GM730_TEACHER="$S/teacher-a"
    TL="$S/train.log"; : > "$TL"

    stage "smoke-data"
    "$PY" - "$HERE" "${GM730_SRC_WINDOWS:-$GM730_DATA/windows}" "$S/windows" 24 16 >> "$TL" 2>&1 <<'PYEOF'
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import trainlib as L
src, dst = Path(sys.argv[2]), Path(sys.argv[3])
for split, repo, n in (("train", "zustand", int(sys.argv[4])), ("dev", "ky", int(sys.argv[5]))):
    d = dst / split
    d.mkdir(parents=True, exist_ok=True)
    ws = [w for w in L.read_jsonl(src / split / "windows.jsonl") if w["repo"] == repo][:n]
    ts = [t for t in L.read_jsonl(src / split / "texts.jsonl") if t["repo"] == repo]
    files = {}
    for name, rows in (("windows.jsonl", ws), ("texts.jsonl", ts)):
        with open(d / name, "w") as f:
            for r in rows:
                f.write(json.dumps(r, sort_keys=True) + "\n")
        files[name] = L.S.sha256_file(d / name)
    L.write_json(d / "manifest.json", {"split": split, "smoke": True, "files": files,
                                       "pairsSha256": L.pairs_sha256(ws), "counts": {"windows": len(ws), "texts": len(ts)}})
    print(split, repo, len(ws), "windows", len(ts), "texts")
PYEOF
    summary "smoke data: $(tail -2 "$TL" | tr '\n' ' ')"

    stage "teacher"
    timed "$TL" "$PY" "$HERE/score_teacher.py" make-ct --out-dir "$S/ct" --n 8 --threads "$THREADS"
    timed "$TL" "$PY" "$HERE/score_teacher.py" --out "$S/teacher-a" --chunk 256 --batch 32 --threads "$THREADS"
    ta="$(times_of "$TL")"
    rc=0; "$PY" "$HERE/score_teacher.py" --out "$S/teacher-b" --chunk 256 --batch 32 --threads "$THREADS" --stop-after-steps 2 >> "$TL" 2>&1 || rc=$?
    [ "$rc" = 75 ] || { summary "teacher kill run exited $rc, expected 75"; false; }
    test -f "$S/teacher-b/checkpoint.pt"; test ! -f "$S/teacher-b/teacher.json"
    "$PY" "$HERE/score_teacher.py" --out "$S/teacher-b" --chunk 256 --batch 32 --threads "$THREADS" >> "$TL" 2>&1
    for f in train.f32 dev.f32; do cmp "$S/teacher-a/$f" "$S/teacher-b/$f" || { summary "teacher resume: $f differs"; false; }; done
    [ "$(jget "$S/teacher-a/ct.json" pass)" = True ]
    summary "teacher: $(jget "$S/teacher-a/teacher.json" timing.pairs) pairs, $(jget "$S/teacher-a/teacher.json" timing.pairsPerSecond) pairs/s (CPU, $THREADS threads; $ta); kill@2 chunks + resume: arrays byte-equal; C-T max diff $(jget "$S/teacher-a/ct.json" maxAbsDiff)"

    # kill_resume <label> <kill step> <script> <args...>: run A uninterrupted, run B
    # stopped after <kill step> (past the first epoch end) and resumed; compare.
    kill_resume() {
        local label="$1" kill="$2" script="$3"; shift 3
        rm -rf "$S/$label-a" "$S/$label-b"
        timed "$TL" "$PY" "$HERE/$script" "$@" --threads "$THREADS" --out "$S/$label-a"
        local t; t="$(times_of "$TL")"
        local rc=0
        "$PY" "$HERE/$script" "$@" --threads "$THREADS" --out "$S/$label-b" --stop-after-steps "$kill" >> "$TL" 2>&1 || rc=$?
        [ "$rc" = 75 ] || { summary "$label: kill run exited $rc, expected 75"; false; }
        test -f "$S/$label-b/checkpoint.pt"; test ! -f "$S/$label-b/result.json"
        "$PY" "$HERE/$script" "$@" --threads "$THREADS" --out "$S/$label-b" >> "$TL" 2>&1
        local ra rb
        ra=$(jget "$S/$label-a/result.json" best.mrr); rb=$(jget "$S/$label-b/result.json" best.mrr)
        "$PY" - "$S/$label-a/result.json" "$S/$label-b/result.json" <<'PYEOF' || { summary "$label: resumed result.json differs from the uninterrupted one"; false; }
import json, sys
a, b = (json.load(open(p)) for p in sys.argv[1:3])
a.pop("name"); b.pop("name")
sys.exit(0 if a == b else 1)
PYEOF
        [ "$(jget "$S/$label-a/train.json" lastParamsSha256)" = "$(jget "$S/$label-b/train.json" lastParamsSha256)" ] \
            || { summary "$label: last parameters differ after resume"; false; }
        summary "$label: uninterrupted == kill@$kill + resume (result.json, last params $(jget "$S/$label-a/train.json" lastParamsSha256 | cut -c1-12)); best epoch $(jget "$S/$label-a/result.json" best.epoch) dev MRR $ra; params $(jget "$S/$label-a/train.json" params.trainable) trainable; ckpt $(jget "$S/$label-a/train.json" checkpointBytes) B; train $(jget "$S/$label-a/train.json" trainSeconds) s; $t"
    }
    # final_run <label> <script> <args...>: a final stage with its control; C-P must pass
    final_run() {
        local label="$1" script="$2"; shift 2
        rm -rf "$S/$label"
        local rc=0
        timed "$TL" "$PY" "$HERE/$script" "$@" --threads "$THREADS" --out "$S/$label" || rc=$?
        [ "$rc" = 0 ] || [ "$rc" = 1 ] || { summary "$label: exit $rc"; false; }
        [ "$(jget "$S/$label/export.json" cp.pass)" = True ] || { summary "$label: C-P failed"; false; }
        summary "$label: rc $rc, $("$PY" -c 'import json,sys; e=json.load(open(sys.argv[1])); print("C-P", e["cp"], "C-P2", e.get("cp2"), "bytes", e["bytes"], "quint8VsFp32", e.get("quint8VsFp32"))' "$S/$label/export.json"); ckpt $(jget "$S/$label/train.json" checkpointBytes) B; $(times_of "$TL")"
    }

    SMALL=(--epochs 2 --batch 8 --resamples 1000)
    stage "ce"
    kill_resume ce 4 train_ce.py --stage grid "${SMALL[@]}" --max-len 128 --lr 5e-5 --tau-t 2 --alpha 0.5
    final_run ce-final-shuf train_ce.py --stage final --control shuffled-teacher "${SMALL[@]}" --max-len 128 --lr 5e-5 --tau-t 2 --alpha 0.5

    ENC=(--train-repos zustand --max-train-queries 24 --dev-repos ky --max-dev-queries 16 --docs-per-query 4 --bf16)
    stage "gte"
    kill_resume gte 4 train_encoder.py --model gte-small --stage grid "${SMALL[@]}" "${ENC[@]}" --lr 5e-5 --tau 0.05 --alpha 0.5
    final_run gte-final-shuf train_encoder.py --model gte-small --stage final --control shuffled "${SMALL[@]}" "${ENC[@]}" --lr 5e-5 --tau 0.05 --alpha 0.5

    stage "jina-lora"
    # 16 train queries: 2 steps per epoch, killed in epoch 2
    kill_resume jl 3 train_encoder.py --model jina --stage grid "${SMALL[@]}" "${ENC[@]}" --max-train-queries 16 --mode lora --rank 16 \
        --lora-targets query,key,value,dense,up_gated_layer,down_layer --lr 2e-4 --tau 0.05 --alpha 0.5
    stage "jina-full"
    final_run jf-final-shuf train_encoder.py --model jina --stage final --control shuffled --epochs 1 --batch 8 --resamples 1000 \
        "${ENC[@]}" --max-train-queries 16 --mode full --lr 1e-5 --tau 0.05 --alpha 0

    stage "jobs"
    for r in rr jina gte; do "$PY" "$HERE/make_jobs.py" grid --route "$r" --out-dir "$S/jobs" >> "$TL" 2>&1; done
    "$PY" "$HERE/make_jobs.py" teacher --out-dir "$S/jobs" >> "$TL" 2>&1
    for f in "$S"/jobs/*.toml; do cmp -s "$f" "$REPO/eval/gpu/jobs/$(basename "$f")" || { summary "jobs: $(basename "$f") differs from the committed TOML"; false; }; done
    # freeze + final TOMLs from the smoke results, copied under every grid config's name
    for r in rr jina gte; do
        case "$r" in rr) src="$S/ce-a" ;; jina) src="$S/jl-a" ;; gte) src="$S/gte-a" ;; esac
        "$PY" - "$HERE" "$r" "$src/result.json" "$S/fake-run/$r" <<'PYEOF'
import json, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import make_jobs as MJ
res = json.loads(Path(sys.argv[3]).read_text())
for i, (name, args) in enumerate(MJ.grid(sys.argv[2])):
    d = Path(sys.argv[4]) / "out" / name
    d.mkdir(parents=True, exist_ok=True)
    r = dict(res, name=name, args=args, route=sys.argv[2])
    r["best"] = dict(r["best"], mrr=r["best"]["mrr"] + 0.001 * i)
    r["base"] = dict(r["base"], mrr=r["best"]["mrr"] - 0.05)
    r["gain"] = {"mrr": 0.05, "lower": 0.01, "upper": 0.09}
    (d / "result.json").write_text(json.dumps(r))
PYEOF
        rc=0; "$PY" "$HERE/freeze.py" --route "$r" --run "$S/fake-run/$r" --final "$S/jobs/final_configs.json" >> "$TL" 2>&1 || rc=$?
        [ "$rc" = 0 ] || [ "$rc" = 2 ] || { summary "freeze $r exit $rc"; false; }
        if [ "$rc" = 0 ]; then "$PY" "$HERE/make_jobs.py" final --route "$r" --final "$S/jobs/final_configs.json" --out-dir "$S/jobs" >> "$TL" 2>&1; fi
    done
    summary "jobs: committed TOMLs reproduced; $(ls "$S"/jobs/*.toml | wc -l | tr -d ' ') TOMLs load via gpu_train.load_job (finals from smoke-derived final_configs.json)"
    summary "DRY DONE; uptime: $(uptime | sed 's/.*load/load/')"
    exit 0
fi

# ---------------------------------------------------------------------------- pre
if [ "$CMD" = pre ]; then
    stage "C-R"
    CR_PASS=""; CR_FAIL=""
    for m in l6 gte jina; do
        if "$PY" "$HERE/export.py" recipe-control --model "$m" --n 1000 --out "$OUT/cr" --json "$OUT/cr.jsonl" > "$OUT/cr_$m.log" 2>&1; then
            CR_PASS="$CR_PASS $m"
        else
            CR_FAIL="$CR_FAIL $m"; summary "C-R $m FAILED: its route does not start (S1); $(tail -1 "$OUT/cr_$m.log")"
        fi
    done
    echo "pass:$CR_PASS; fail:$CR_FAIL" > "$OUT/cr_status.txt"
    [ -n "$CR_PASS" ] || { summary "C-R failed for every model: no route starts"; false; }
    # plant_fails <log> <FAIL line regex> <cmd...>: the planted run must exit 1 and print its own FAIL line
    plant_fails() {
        local lg="$1" want="$2"; shift 2
        local rc=0
        "$@" > "$lg" 2>&1 || rc=$?
        [ "$rc" = 1 ] && grep -qE "$want" "$lg"
    }
    for m in l6 jina; do
        plant_fails "$OUT/cr_$m-plant.log" "^C-R $m plant flip-per-channel: FAIL\$" \
            "$PY" "$HERE/export.py" recipe-control --model "$m" --n 100 --out "$OUT/cr-plant" --plant flip-per-channel \
            || { summary "C-F: flipped per_channel did not fail C-R for $m with its FAIL line ($(tail -1 "$OUT/cr_$m-plant.log"))"; false; }
    done
    summary "C-R: pass:$CR_PASS; fail:${CR_FAIL:- none}; planted flags fail with their FAIL line"
    stage "C-0"
    [ -x "$BIN" ] || { summary "no g-mesh binary at $BIN (cargo build --release -p g-mesh --features embed-eval)"; false; }
    "$BIN" debug-embed-eval report --eval-dir "$V2" --json "$OUT/c0_report.json" \
        "$V2_RUNS/jina-v2-base-code-fp32" "$V2_RUNS/random" "$V2_RUNS/shuffled" "$V2_RUNS/$INT8" "$V2_RUNS/$ADAPTER" > "$OUT/c0_report.txt" 2>&1
    "$PY" "$HERE/eval_checks.py" c0 --report "$OUT/c0_report.json" > "$OUT/c0_check.txt" 2>&1 \
        || { summary "C-0 FAILED: $(grep FAILED "$OUT/c0_check.txt" | head -2 | tr '\n' ' ')"; false; }
    summary "C-0: int8 0.608/0.380 and adapter 0.655/0.423 reproduced ($OUT/c0_check.txt)"
    stage "windows"
    for s in train dev; do "$PY" "$HERE/windows.py" build --split "$s" >> "$OUT/windows.log" 2>&1; done
    plant_fails "$OUT/windows-plant.log" "^join check: FAIL " "$PY" "$HERE/windows.py" build --split dev --out "$OUT/win-plant" --plant-offset \
        || { summary "windows: planted offset did not fail the join check ($(grep 'join check' "$OUT/windows-plant.log" | tail -1))"; false; }
    summary "windows: train $(jget "$GM730_DATA/windows/train/manifest.json" counts.pairs) pairs, dev $(jget "$GM730_DATA/windows/dev/manifest.json" counts.pairs) pairs; join checks pass; planted offset fails"
    stage "C-T pairs"
    timed "$OUT/ct.log" "$PY" "$HERE/score_teacher.py" make-ct --out-dir "$GM730_DATA/ct" --n 1000 --threads "$THREADS"
    summary "C-T pairs: $(grep 'dev pairs' "$OUT/ct.log" | tail -1)"
    stage "jobs"
    "$PY" "$HERE/make_jobs.py" teacher >> "$OUT/jobs.log" 2>&1
    for r in rr jina gte; do "$PY" "$HERE/make_jobs.py" grid --route "$r" >> "$OUT/jobs.log" 2>&1; done
    git -C "$REPO" diff --exit-code -- eval/gpu/jobs >> "$OUT/jobs.log" 2>&1 || { summary "jobs: regenerated TOMLs differ from the committed ones"; false; }
    summary "PRE DONE; routes whose C-R failed do not start:${CR_FAIL:- none}; next: budget.py check --job eval/gpu/jobs/gm730-rr-teacher.toml, then gpu_train.py run it"
    exit 0
fi

[ -n "$RUN" ] && [ -d "$RUN" ] || { summary "--run <pulled run dir> is required"; false; }

# ---------------------------------------------------------------------------- teacher-data
if [ "$CMD" = teacher-data ]; then
    stage "teacher-data"
    src="$RUN/out/teacher"
    if [ "$(jget "$src/ct.json" pass)" != True ]; then
        for r in jina gte; do "$PY" "$HERE/make_jobs.py" grid --route "$r" --no-teacher >> "$OUT/jobs.log" 2>&1; done
        summary "C-T failed ($(jget "$src/ct.json" maxAbsDiff)): RR stops; J and R3 run without the teacher (alpha = 0): eval/gpu/jobs/gm730-{jina,gte}-grid-nt.toml written, then freeze/eval with --no-teacher"
        false
    fi
    mkdir -p "$GM730_DATA/teacher"
    cp "$src/train.f32" "$src/dev.f32" "$src/teacher.json" "$src/ct.json" "$GM730_DATA/teacher/"
    GM730_TEACHER="$GM730_DATA/teacher" "$PY" -c 'import sys; sys.path.insert(0, sys.argv[1]); import trainlib as L
for s in ("train", "dev"):
    w, _, _ = L.load_split(s); L.load_teacher(s, w)' "$HERE"
    summary "teacher-data: $GM730_DATA/teacher matches the windows (pairsSha256, sha256); C-T max diff $(jget "$src/ct.json" maxAbsDiff)"
    exit 0
fi

# ---------------------------------------------------------------------------- freeze
if [ "$CMD" = freeze ]; then
    stage "freeze $ROUTE"
    rc=0; "$PY" "$HERE/freeze.py" --route "$ROUTE" --run "$RUN" --final "$FINAL" $NO_TEACHER >> "$OUT/freeze.log" 2>&1 || rc=$?
    if [ "$rc" = 2 ]; then summary "freeze $ROUTE: S4 dev gate STOP ($(tail -2 "$OUT/freeze.log" | head -1))"; exit 0; fi
    [ "$rc" = 0 ]
    "$PY" "$HERE/make_jobs.py" final --route "$ROUTE" --final "$FINAL" $NO_TEACHER >> "$OUT/freeze.log" 2>&1
    summary "freeze $ROUTE: $(grep winner "$OUT/freeze.log" | tail -1); final TOML written"
    exit 0
fi

# ---------------------------------------------------------------------------- eval
stage "eval $ROUTE"
[ -x "$BIN" ] || { summary "no g-mesh binary at $BIN"; false; }
CHECKS="$OUT/checks_$ROUTE.txt"; : > "$CHECKS"
# check <label> <eval_checks.py args...>: one control's pass rules; a failed rule is FAILED
check() {
    local label="$1"; shift
    local rc=0
    echo "== $label" >> "$CHECKS"
    "$PY" "$HERE/eval_checks.py" "$@" > "$CHECKS.last" 2>&1 || rc=$?
    cat "$CHECKS.last" >> "$CHECKS"
    if [ "$rc" != 0 ]; then
        summary "$label FAILED: $(grep 'FAILED' "$CHECKS.last" | head -3 | tr '\n' ' ')"
        return 1
    fi
    summary "$label: pass ($(tr '\n' ' ' < "$CHECKS.last" | cut -c1-300))"
}
# no_load_fail <log>: gm463 drops a model ORT cannot load ("LOAD FAIL"); here that is an error
no_load_fail() {
    if grep -q "^LOAD FAIL" "$1"; then summary "eval $ROUTE: $(grep '^LOAD FAIL' "$1" | head -2 | tr '\n' ' ')"; return 1; fi
}
for step in final shuffled; do
    [ "$(jget "$RUN/out/$step/export.json" cp.pass)" = True ] || { summary "$ROUTE $step: C-P failed on the instance"; false; }
done
check "S5 dev ($ROUTE shuffled control)" s5 --result "$RUN/out/shuffled/result.json"
case "$ROUTE" in
    jina|rr) check "C-P2 ($ROUTE final, shuffled)" cp2 --export "$RUN/out/final/export.json" "$RUN/out/shuffled/export.json" ;;
esac
REG="$HERE/register_arms.py"
case "$ROUTE" in
    jina)
        form=$(jget "$FINAL" jina.name | cut -d- -f1)
        QUALITY_ARM="gm730-jina-$form-int8-structured"
        "$PY" "$REG" embed --name "$QUALITY_ARM" --base jina --src "$RUN/out/final/onnx-int8" --role quality --words-shuffled >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" embed --name "gm730-jina-shuf-int8-structured" --base jina --src "$RUN/out/shuffled/onnx-int8" --role informational >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" embed --name "gm730-jina-requant-base-int8-structured" --base jina --src "$OUT/cr/jina/int8-ours" --role informational >> "$OUT/eval.log" 2>&1
        ARMS="$QUALITY_ARM gm730-jina-shuf-int8-structured gm730-jina-requant-base-int8-structured $QUALITY_ARM-words-shuffled"
        SHUF_ARM=gm730-jina-shuf-int8-structured
        CR2_ARM=gm730-jina-requant-base-int8-structured
        BASE_ARM="$INT8" ;;
    gte)
        "$PY" "$REG" embed --name gm730-gte-student-structured --base gte --src "$RUN/out/final/onnx-fp32" --role cost >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" embed --name gm730-gte-shuf-structured --base gte --src "$RUN/out/shuffled/onnx-fp32" --role informational >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" embed --name gm730-gte-export-base-structured --base gte --src "$OUT/cr/gte/fp32" --role informational >> "$OUT/eval.log" 2>&1
        ARMS="gm730-gte-student-structured gm730-gte-shuf-structured gm730-gte-export-base-structured"
        SHUF_ARM=gm730-gte-shuf-structured
        CR2_ARM=gm730-gte-export-base-structured
        BASE_ARM="$GTE" ;;
    rr)
        "$PY" "$REG" ce --name ce-gm730-l6-distill --src "$RUN/out/final/onnx-fp32" >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" ce --name ce-gm730-l6-distill-quint8 --src "$RUN/out/final/onnx-quint8" --chunk1 >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" ce --name ce-gm730-l6-shuf --src "$RUN/out/shuffled/onnx-quint8" --chunk1 >> "$OUT/eval.log" 2>&1
        "$PY" "$REG" verify >> "$OUT/eval.log" 2>&1
        stage "eval rr: teacher row (ce-gte-mb)"
        "$PY" "$REPO/eval/embedding/fetch_models.py" --ce ce-gte-mb --work "$EVAL_DATA/work" >> "$OUT/eval.log" 2>&1
        stage "eval rr: ce-minilm reference cache"
        # C0 needs every ce-minilm reference pair cached before the measured run (0 scored in it):
        # restore the GM-698 cache files when GM698_RERANK_CACHE names them, then score what is
        # still missing in a ce-minilm-only run, which stops at C0 exactly when it had to score.
        if [ -n "${GM698_RERANK_CACHE:-}" ]; then
            mkdir -p "$EVAL_DATA/work/rerank_cache"
            for f in "$GM698_RERANK_CACHE"/gm462-ce-minilm.*; do
                [ -e "$EVAL_DATA/work/rerank_cache/$(basename "$f")" ] || cp "$f" "$EVAL_DATA/work/rerank_cache/"
            done
        fi
        rc=0
        timed "$OUT/gm463-ref.log" "$PY" "$REPO/eval/embedding/gm463_bakeoff.py" quality --work "$EVAL_DATA/work" --eval-dir "$V2" \
            --st-runs "runs/$INT8" --models ce-minilm --k 30 --summary "$OUT/gm463_ref.txt" || rc=$?
        missing=$(grep -E '^ce-minilm reference pairs missing at K=30: off [0-9]+, ship [0-9]+' "$OUT/gm463-ref.log" | tail -1)
        if [ "$rc" != 0 ]; then
            [ -n "$missing" ] && echo "$missing" | grep -qvE 'off 0, ship 0$' && grep -q "C0 ship-L6 reproduces GM-698: MISMATCH" "$OUT/gm463-ref.log" \
                || { summary "eval rr: the ce-minilm reference run failed (exit $rc): $(tail -2 "$OUT/gm463-ref.log" | tr '\n' ' ')"; false; }
        fi
        no_load_fail "$OUT/gm463-ref.log"
        summary "eval rr: ce-minilm reference cache complete (${missing:-?}; first run exit $rc); $(times_of "$OUT/gm463-ref.log")"
        stage "eval rr"
        timed "$OUT/gm463.log" "$PY" "$REPO/eval/embedding/gm463_bakeoff.py" quality --work "$EVAL_DATA/work" --eval-dir "$V2" \
            --st-runs "runs/$INT8" --models ce-minilm,ce-gm730-l6-distill,ce-gm730-l6-distill-quint8,ce-gm730-l6-shuf,ce-gte-mb \
            --k 30 --table "$OUT/gm463_rr.md" --summary "$OUT/gm463_rr.txt"
        no_load_fail "$OUT/gm463.log"
        grep -q "^C0 ship-L6 reproduces GM-698: pass" "$OUT/gm463.log" || { summary "eval rr: no C0 pass line in $OUT/gm463.log"; false; }
        summary "eval rr: gm463 quality $OUT/gm463_rr.md (C0 pass); $(times_of "$OUT/gm463.log")"
        check "C-S (rr, held-out NL vs ship-L6)" cs-rr --table "$OUT/gm463_rr.md" --arm ce-gm730-l6-shuf --base ship-L6 --k 30
        timed "$OUT/gm463-adapter.log" "$PY" "$REPO/eval/embedding/gm463_bakeoff.py" quality --work "$EVAL_DATA/work" --eval-dir "$V2" \
            --st-runs "$V2_RUNS/$ADAPTER" --models ce-minilm,ce-gm730-l6-distill-quint8 \
            --k 30 --table "$OUT/gm463_rr_adapter.md" --summary "$OUT/gm463_rr_adapter.txt"
        no_load_fail "$OUT/gm463-adapter.log"
        summary "eval rr (D6a, adapter windows): $OUT/gm463_rr_adapter.md; $(times_of "$OUT/gm463-adapter.log")"
        summary "EVAL DONE (S5, C-P2, C-S pass); uptime: $(uptime | sed 's/.*load/load/')"
        exit 0 ;;
    *) summary "--route rr|jina|gte"; false ;;
esac
"$PY" "$REG" verify >> "$OUT/eval.log" 2>&1
n=0
for arm in $ARMS; do
    for c in $V2_CORPORA; do
        nice -n 10 "$BIN" debug-embed-eval run --eval-dir "$V2" --variant "$arm" --corpus "$c" --out "$V2_RUNS" >> "$OUT/v2.log" 2>&1
        n=$((n + 1))
    done
    log "v2 $arm done"
done
"$PY" "$REG" check-run --runs "$V2_RUNS" --names "$(echo $ARMS | tr ' ' ',')" >> "$OUT/eval.log" 2>&1
summary "eval $ROUTE: $n v2 runs ($ARMS); check-run ok"
ARM_DIRS=""
for arm in jina-v2-base-code-fp32 random shuffled "$INT8" "$GTE" "$ADAPTER" $ARMS; do ARM_DIRS="$ARM_DIRS $V2_RUNS/$arm"; done
"$BIN" debug-embed-eval report --eval-dir "$V2" --json "$OUT/report_$ROUTE.json" $ARM_DIRS > "$OUT/report_$ROUTE.txt" 2>&1
"$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$ADAPTER" --json "$OUT/report_${ROUTE}_vs_adapter.json" $ARM_DIRS > "$OUT/report_${ROUTE}_vs_adapter.txt" 2>&1
"$BIN" debug-embed-eval report --eval-dir "$V2" --reference "$BASE_ARM" --json "$OUT/report_${ROUTE}_vs_base.json" $ARM_DIRS > "$OUT/report_${ROUTE}_vs_base.txt" 2>&1
summary "eval $ROUTE: reports $OUT/report_$ROUTE{,_vs_adapter,_vs_base}.txt (gates: arms.<arm>.gates in the JSON)"
ok=1
check "C-S ($ROUTE v2, $SHUF_ARM vs $BASE_ARM)" cs --runs "$V2_RUNS" --eval-dir "$V2" --arm "$SHUF_ARM" --base "$BASE_ARM" || ok=0
check "C-R2 ($ROUTE v2, $CR2_ARM vs $BASE_ARM)" cr2 --runs "$V2_RUNS" --eval-dir "$V2" --arm "$CR2_ARM" --official "$BASE_ARM" || ok=0
[ "$ok" = 1 ] || { summary "EVAL FAILED: a control's pass rule failed ($CHECKS)"; false; }
summary "EVAL DONE (S5, C-P2, C-S, C-R2 pass); uptime: $(uptime | sed 's/.*load/load/')"
