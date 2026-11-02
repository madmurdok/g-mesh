#!/bin/bash
# GM-441 S25: daemon confirm, 3 runs per arm interleaved (A0 A1 A0 A1 A0 A1), fail-fast.
# Each run: wait for load1 <= LOAD_MAX, fresh corpus copy + home, gm441_confirm.py under
# /usr/bin/time -p, daemon killed by its recorded PID. Compact result: $OUT/summary.txt.
# Env: OUT, BIN_A0, BIN_A1, LOAD_MAX (4), LOAD_WAIT_S (7200), TIMEOUT_S (1500), ORDER, DRY=1
set -eu
set -o pipefail
WT=$(cd "$(dirname "$0")/../.." && pwd)
MAIN=${MAIN:-/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh/eval/embedding}
OUT=${OUT:-/tmp/gm441/confirm3}
BIN_A1=${BIN_A1:-$WT/target/release/g-mesh}
BIN_A0=${BIN_A0:-$WT/../g-mesh-wt-gm441-base/target/release/g-mesh}
LOAD_MAX=${LOAD_MAX:-4}
LOAD_WAIT_S=${LOAD_WAIT_S:-7200}
TIMEOUT_S=${TIMEOUT_S:-1500}
ORDER=${ORDER:-"A0 A1 A0 A1 A0 A1"}
EV=$WT/eval/embedding
mkdir -p "$OUT"
SUM=$OUT/summary.txt
: > "$SUM"
sumln() { echo "$*" >> "$SUM"; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
export G_MESH_RERANK_MODEL_DIR=$MAIN/work/models/ms-marco-MiniLM-L6-v2

n=0
for arm in $ORDER; do
  n=$((n+1))
  if [ $arm = A0 ]; then bin=$BIN_A0; else bin=$BIN_A1; fi
  waited=0
  while awk -v l="$(load1)" -v m="$LOAD_MAX" 'BEGIN{exit !(l>m)}'; do
    if [ $waited -ge "$LOAD_WAIT_S" ]; then echo "load1 never reached $LOAD_MAX" >> "$SUM"; exit 2; fi
    sleep 30; waited=$((waited+30))
  done
  run=r$n-$arm
  src=/tmp/gm441/src-$run
  home=/tmp/gm441/h-$run
  rm -rf "$src" "$home"
  cp -c -R "$MAIN/work/corpora/g-mesh" "$src"
  load_before=$(load1)
  if [ "${DRY:-0}" = 1 ]; then echo '{"ready_t":1,"pass_s":1,"tiers":[],"daemonPid":null}' > "$OUT/$run.out"; continue; fi
  { /usr/bin/time -p python3 "$EV/gm441_confirm.py" --bin "$bin" --project "$src" --home "$home" \
      --out "$OUT/$run.json" --timeout "$TIMEOUT_S" > "$OUT/$run.out" ; } 2> "$OUT/$run.time"
  load_after=$(load1)
  pid=$(python3 -c "import json;print(json.load(open('$OUT/$run.json'))['daemonPid'])")
  # kill by recorded PID if still alive
  if [ "$pid" != None ] && kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" || true; sumln "  $run: daemon $pid had to be killed"; fi
  sumln "$run load_before=$load_before load_after=$load_after pid=$pid"
  sumln "  $(uptime)"
  sumln "  $(tr '\n' ' ' < "$OUT/$run.time")"
  rm -rf "$src"
done

python3 - "$OUT" "$ORDER" >> "$SUM" <<'PY'
import json, re, statistics, sys
out, order = sys.argv[1], sys.argv[2].split()
rows = {"A0": [], "A1": []}
print("== per run")
for i, arm in enumerate(order, 1):
    run = f"r{i}-{arm}"
    j = json.load(open(f"{out}/{run}.json"))
    t = dict(re.findall(r"(real|user|sys)\s+([\d.]+)", open(f"{out}/{run}.time").read()))
    tiers = {x["tier"]: x["s"] for x in j["tiers"]}
    cpu = j.get("daemonCpuTime") or "0:0"
    parts = [float(x) for x in cpu.split(":")]
    cpu_s = sum(v * 60 ** k for k, v in enumerate(reversed(parts)))
    rows[arm].append((j["pass_s"], float(t["real"]), float(t["user"]), float(t["sys"]), tiers, cpu_s))
    print(f"{run}: embed {j['pass_s']}s tiers {tiers} real {t['real']} user {t['user']} sys {t['sys']} daemonCPU {cpu_s:.0f}s (driver time excludes the daemon)")
print("== per arm (median [min-max])")
for arm, r in rows.items():
    f = lambda k: f"{statistics.median(x[k] for x in r):.1f} [{min(x[k] for x in r):.1f}-{max(x[k] for x in r):.1f}]"
    print(f"{arm}: embed {f(0)}  real {f(1)}  user {f(2)}  sys {f(3)}  daemonCPU {f(5)}")
    for tier in ("public", "private", "test"):
        v = [x[4][tier] for x in r if tier in x[4]]
        if v:
            print(f"   {tier}: {statistics.median(v):.1f} [{min(v):.1f}-{max(v):.1f}]")
PY
echo "ALL DONE" >> "$SUM"
