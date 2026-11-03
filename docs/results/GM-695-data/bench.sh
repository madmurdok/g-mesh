#!/bin/bash
# GM-695/S1 offline order bench: same 4237 texts, same model, id order (A0) vs tier order (A1), ABBAAB.
set -eu
set -o pipefail
S=$(cd "$(dirname "$0")" && pwd); D=/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm695-a1
B=$S/bench; mkdir -p "$B"; SUM=$B/summary.txt; : > "$SUM"
say() { echo "$(date +%H:%M:%S) $*" >> "$SUM"; }
cp /tmp/gm695/h-r2-A1/projects/8e47eb540a1216c3/index.db* "$B/"
test -z "$(pgrep -fl -- "--out-dir $D/target" || true)"
say "build start load=$(sysctl -n vm.loadavg)"
{ /usr/bin/time -p cargo test --release --manifest-path "$D/Cargo.toml" -p g-mesh --lib --no-run > "$B/build.log" 2>&1 ; } 2> "$B/build.time"
say "build done $(tr '\n' ' ' < "$B/build.time")"
waited=0
while awk -v l="$(sysctl -n vm.loadavg | awk '{print $2}')" 'BEGIN{exit !(l>4)}'; do
  [ $waited -ge 14400 ] && { say "load never <=4"; exit 2; }
  sleep 30; waited=$((waited+30))
done
say "bench start waited=${waited}s $(uptime)"
{ /usr/bin/time -p env GM695_DB="$B/index.db" GM695_OUT="$B/rounds.jsonl" GM695_ROUNDS="A0 A1 A1 A0 A0 A1" \
   cargo test --release --manifest-path "$D/Cargo.toml" -p g-mesh --lib gm695_order_bench -- --ignored --nocapture > "$B/bench.log" 2>&1 ; } 2> "$B/bench.time"
grep "GM695 round\|test result" "$B/bench.log" >> "$SUM"
say "bench done $(uptime) | $(tr '\n' ' ' < "$B/bench.time")"
