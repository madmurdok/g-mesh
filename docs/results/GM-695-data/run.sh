#!/bin/bash
# GM-695/S1: build instrumented A0/A1 in throwaway worktrees, then traced daemon runs.
# Fail-fast; compact summary in $OUT/summary.txt. DRY=1: setup checks only, no build, no runs.
set -eu
set -o pipefail
P=/Users/Valentin_Taiurskii/Projects/ClaudeProjects
WT=$P/g-mesh-wt-gm695
S=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-$S/out}
ORDER=${ORDER:-"A0 A1 A1N"}
LOAD_MAX=${LOAD_MAX:-4}
LOAD_WAIT_S=${LOAD_WAIT_S:-14400}
CORPUS=$P/g-mesh/eval/embedding/work/corpora/g-mesh
export G_MESH_RERANK_MODEL_DIR=$P/g-mesh/eval/embedding/work/models/ms-marco-MiniLM-L6-v2
mkdir -p "$OUT"; SUM=$OUT/summary.txt; : > "$SUM"
say() { echo "$(date +%H:%M:%S) $*" >> "$SUM"; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
test -d "$CORPUS"; test -d "$G_MESH_RERANK_MODEL_DIR"
test -z "$(pgrep -fl -- "--out-dir $P/g-mesh/target" || true)"

setup() { # arm rev tier_expr
  local arm=$1 rev=$2 expr=$3 d=$P/g-mesh-wt-gm695-$1
  if [ ! -d "$d" ]; then
    git -C "$WT" worktree add --detach "$d" "$rev" >> "$OUT/setup.log" 2>&1
  fi
  test "$(git -C "$d" rev-parse HEAD)" = "$(git -C "$WT" rev-parse "$rev")"
  if [ "${DRY:-0}" = 1 ]; then say "dry: $arm worktree $d at $rev ok"; return; fi
  if [ ! -d "$d/target/release" ]; then mkdir -p "$d/target"; cp -c -R "$P/g-mesh/target/release" "$d/target/release"; fi
  (cd "$d/plugins/typescript" && npm ci --silent) >> "$OUT/setup.log" 2>&1
  (cd "$d/plugins/python" && npm ci --silent) >> "$OUT/setup.log" 2>&1
  git -C "$d" checkout -- core/src/embedding/backfill.rs
  python3 "$S/instrument.py" "$d/core/src/embedding/backfill.rs" "$expr" >> "$OUT/setup.log"
  say "build $arm start load1=$(load1)"
  { /usr/bin/time -p cargo build --release --workspace --manifest-path "$d/Cargo.toml" > "$OUT/build-$arm.log" 2>&1 ; } 2> "$OUT/build-$arm.time"
  grep -q GM695 "$d/target/release/g-mesh"
  say "build $arm done $(tr '\n' ' ' < "$OUT/build-$arm.time") load1=$(load1)"
}
if [ "${SKIP_SETUP:-0}" != 1 ]; then
setup a0 2727ea5 '"all"'
setup a1 de338a6 'tier.as_str()'
fi
therm() { pmset -g therm | awk '/CPU_Speed_Limit/{print $3}'; }
if [ "${DRY:-0}" = 1 ]; then say "DRY DONE"; exit 0; fi

n=0
for arm in $ORDER; do
  n=$((n+1))
  case $arm in A0) bin=$P/g-mesh-wt-gm695-a0/target/release/g-mesh; extra="";;
               A1) bin=$P/g-mesh-wt-gm695-a1/target/release/g-mesh; extra="";;
               A1N) bin=$P/g-mesh-wt-gm695-a1/target/release/g-mesh; extra="--no-tier-search";; esac
  waited=0
  while awk -v l="$(load1)" -v m="$LOAD_MAX" 'BEGIN{exit !(l>m)}'; do
    if [ $waited -ge "$LOAD_WAIT_S" ]; then say "load1 never reached $LOAD_MAX"; exit 2; fi
    sleep 30; waited=$((waited+30))
  done
  run=${PREFIX:-r}$n-$arm; src=/tmp/gm695/src-$run; home=/tmp/gm695/h-$run
  rm -rf "$src" "$home"; mkdir -p /tmp/gm695
  cp -c -R "$CORPUS" "$src"
  say "$run start load1=$(load1) speed_limit=$(therm) waited=${waited}s $(uptime)"
  rc=0
  { /usr/bin/time -p python3 "$S/driver.py" --bin "$bin" --project "$src" --home "$home" \
      --out "$OUT/$run.json" --timeout 1800 ${extra:+"$extra"} > "$OUT/$run.out" ; } 2> "$OUT/$run.time" || rc=$?
  pid=$(python3 -c "import json;print(json.load(open('$OUT/$run.json'))['daemonPid'])")
  if [ "$pid" != None ] && kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" || true; say "  $run: daemon $pid killed"; fi
  cp "$home/daemon.log" "$OUT/$run.daemon.log"
  say "$run end rc=$rc load1=$(load1) speed_limit=$(therm) $(uptime) | $(tr '\n' ' ' < "$OUT/$run.time")"
  grep -E "GM695 pass|embeddings \[|finished the" "$OUT/$run.daemon.log" >> "$SUM"
  rm -rf "$src"
  test $rc = 0
done
say "ALL DONE"
