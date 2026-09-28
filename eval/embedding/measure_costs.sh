#!/usr/bin/env bash
# GM-398 S14 (docs/results/gm-398-model-comparison.md): quality runs, query latency, timed passes (D11), report.
# Env: VARIANTS (candidates), QCORPORA ("" = all), TCORPUS, ROUNDS, OUT, RUNS
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
cd "$HERE/../.." || exit
BIN=$PWD/target/release/g-mesh
REF=jina-v2-base-code-fp32
VARIANTS=${VARIANTS:-"jina-v2-base-code-int8 bge-small-en-v1.5 gte-small snowflake-arctic-embed-s"}
QCORPORA=${QCORPORA:-}
TCORPUS=${TCORPUS:-g-mesh}
ROUNDS=${ROUNDS:-3}
OUT=${OUT:?}
RUNS=${RUNS:-$PWD/eval/embedding/work/runs}
mkdir -p "$OUT"
LOG=$OUT/log.txt
: > "$LOG"
log() { echo "[$(date '+%H:%M:%S')] $*" | tee -a "$LOG"; }
load1() { sysctl -n vm.loadavg | awk '{print $2}'; }
wait_quiet() {
  [ "${NOWAIT:-0}" = 1 ] && return
  local n=0
  while awk -v l="$(load1)" 'BEGIN{exit !(l>=7.0)}'; do
    n=$((n+1)); [ $n -gt 10 ] && { log "WARN load still $(load1) after 5 min, proceeding"; return; }
    sleep 30
  done
}
corpus_args() { local a=""; for c in $QCORPORA; do a="$a --corpus $c"; done; echo "$a"; }

log "start; daemons: $(pgrep -fl 'g-mesh daemon' | tr '\n' ';')"
log "uptime $(uptime)"

# 1. quality runs, one candidate at a time
for v in ${QVARIANTS-$VARIANTS}; do
  wait_quiet
  log "quality $v: uptime $(uptime)"
  # shellcheck disable=SC2046 # corpus_args expands to multiple "--corpus X" flags, one per word
  /usr/bin/time -p "$BIN" debug-embed-eval run --variant "$v" $(corpus_args) --out "$RUNS" \
    > "$OUT/quality-$v.out" 2> "$OUT/quality-$v.err"
  rc=$?
  log "quality $v rc=$rc $(grep -E '^(real|user|sys)' "$OUT/quality-$v.err" | tr '\n' ' ') after: $(uptime)"
done

# 2. query latency: same query set for every model arm, node vectors reused
for v in $REF $VARIANTS; do
  d=$OUT/qlat/$v/$TCORPUS
  rm -rf "$d"; mkdir -p "$d"
  cp "$RUNS/$v/$TCORPUS/manifest.json" "$RUNS/$v/$TCORPUS/vectors.bin" "$d/"
  wait_quiet
  log "qlat $v: uptime $(uptime)"
  G_MESH_EMBEDDING_CACHE=off "$BIN" debug-embed-eval run --variant "$v" --corpus "$TCORPUS" --out "$OUT/qlat" \
    > "$OUT/qlat-$v.out" 2>&1
  log "qlat $v rc=$? after: $(uptime)"
done

# 3. timed passes, interleaved R, C1, C2, ... per round; redo once if after-load > 3.0
for r in $(seq 1 "$ROUNDS"); do
  for v in $REF $VARIANTS; do
    for attempt in 1 2; do
      wait_quiet
      before=$(uptime)
      f=$OUT/timed/r$r/$v.time
      mkdir -p "$(dirname "$f")"
      G_MESH_EMBEDDING_CACHE=off /usr/bin/time -lp "$BIN" debug-embed-eval run --variant "$v" \
        --corpus "$TCORPUS" --embed-only --force --out "$OUT/timed/r$r" > "$f.out" 2> "$f"
      rc=$?
      after=$(uptime); la=$(load1)
      { echo "before: $before"; echo "after: $after"; echo "attempt: $attempt"; } >> "$f"
      log "timed r$r $v attempt $attempt rc=$rc $(grep -E '^(real|user|sys)' "$f" | tr '\n' ' ') load-after $la"
      # Owner-approved noisy machine: start below 7.0 (not 2.0); redo if after-load > 8.0 (not 3.0).
      # D11's after-load includes the run's own ORT threads (user/real ~3.4),
      # so the redo threshold is 3.0 plus the run's own parallelism.
      own=$(awk '/^real/{r=$2} /^user/{u=$2} END{printf "%.2f", u/r}' "$f")
      awk -v l="$la" -v o="$own" 'BEGIN{exit !(l>8.0+o)}' || break
      log "after-load $la > 8.0 + own $own: redo"
    done
  done
done

# 4. costs.toml and report
# shellcheck disable=SC2086 # $VARIANTS is a space-separated list; each name must be its own arg
python3 "$HERE/measure_summary.py" costs "$OUT" "$REF" $VARIANTS > "$OUT/costs.toml"
log "costs written"
ARMS="$RUNS/$REF $RUNS/random $RUNS/shuffled $RUNS/words-shuffled $RUNS/bm25"
for v in $VARIANTS; do ARMS="$ARMS $RUNS/$v"; done
# shellcheck disable=SC2086 # $ARMS is a space-separated list of paths; each must be its own arg
"$BIN" debug-embed-eval report $ARMS --costs "$OUT/costs.toml" --json "$OUT/report.json" > "$OUT/report.txt" 2>&1
log "report rc=$?"
# shellcheck disable=SC2086 # $VARIANTS is a space-separated list; each name must be its own arg
python3 "$HERE/measure_summary.py" table "$OUT" "$REF" $VARIANTS > "$OUT/summary.md" 2>&1
log "summary rc=$? uptime $(uptime)"
log "done"
