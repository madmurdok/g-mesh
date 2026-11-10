#!/usr/bin/env bash
# One fail-fast run of the GM-456 training-data pipeline
# (docs/design/GM-456-training-data.md, section 7), meant to be launched once
# in the background. Every stage runs under `taskpolicy -b`; long stages are
# timed with /usr/bin/time -p and the machine's `uptime` is recorded beside
# them. The compact summary goes to <version>/summary.txt, each stage's full
# output to <version>/logs/<stage>.log.
#
# usage: eval/training/run_pipeline.sh --version v0.0-stub [--stub] [--count N]
#          [--skip-setup] [--threads 2] [--bin target/release/g-mesh] <repo>...
#   --stub   stub queries and stub judge verdicts, no API call (the S2 run).
#   Without --stub the generate and judge stages stop at --dry-run: the
#   batches are submitted and collected by hand (generate.py/judge.py
#   --submit/--collect), then the run is resumed with --from mine / export.
# Run from the repository root.
set -euo pipefail

version=""; stub=0; count=""; skip_setup=0; threads=2; bin="target/release/g-mesh"; from="setup"
repos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --stub) stub=1; shift ;;
    --count) count="$2"; shift 2 ;;
    --skip-setup) skip_setup=1; shift ;;
    --threads) threads="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    --from) from="$2"; shift 2 ;;
    *) repos+=("$1"); shift ;;
  esac
done
[ -n "$version" ] || { echo "--version is required" >&2; exit 2; }

data="${GM456_DATA:-$HOME/Projects/ClaudeProjects/g-mesh-data/gm456}"
vdir="$data/$version"
mkdir -p "$vdir/logs"
summary="$vdir/summary.txt"
py="python3"
tdir="eval/training"
repo_args=()
for r in ${repos[@]+"${repos[@]}"}; do repo_args+=(--repo "$r"); done

: > "$summary"
note() { echo "$*" | tee -a "$summary"; }

stage() {  # stage <name> <command...>: timed, logged, fail-fast
  local name="$1"; shift
  note "== $name  ($(uptime | sed 's/.*load averages*: /load /'))"
  if ! /usr/bin/time -p taskpolicy -b "$@" > "$vdir/logs/$name.log" 2>&1; then
    note "FAILED: $name (log $vdir/logs/$name.log)"
    tail -25 "$vdir/logs/$name.log" >> "$summary"
    exit 1
  fi
  grep -E '^(real|user|sys) ' "$vdir/logs/$name.log" | tr '\n' ' ' >> "$summary"; echo >> "$summary"
  grep -E '^(nodes |sample:|render |generate|mine:|judge|leak_guard:|  nearest-eval|export:|collect:)' \
    "$vdir/logs/$name.log" | cut -c1-2000 >> "$summary" || true
}

order="setup nodes sample render generate mine judge export"
skip=1
for s in $order; do
  [ "$s" = "$from" ] && skip=0
  [ $skip -eq 1 ] && continue
  case "$s" in
    setup)
      [ $skip_setup -eq 1 ] || stage setup bash "$tdir/setup_work.sh" --bin "$bin" ${repos[@]+"${repos[@]}"} ;;
    nodes) stage nodes $py "$tdir/nodes.py" --version "$version" ${repo_args[@]+"${repo_args[@]}"} ;;
    sample)
      if [ -n "$count" ]; then
        stage sample $py "$tdir/sample.py" --version "$version" --count "$count" ${repo_args[@]+"${repo_args[@]}"}
      else
        stage sample $py "$tdir/sample.py" --version "$version" ${repo_args[@]+"${repo_args[@]}"}
      fi ;;
    render) stage render $py "$tdir/render.py" --version "$version" ;;
    generate)
      if [ $stub -eq 1 ]; then
        stage generate-dry $py "$tdir/generate.py" --version "$version" --dry-run
        stage generate $py "$tdir/generate.py" --version "$version" --stub
      else
        stage generate-dry $py "$tdir/generate.py" --version "$version" --dry-run
        note "generate: submit with generate.py --submit, collect, then resume with --from mine"; exit 0
      fi ;;
    mine) stage mine $py "$tdir/mine.py" --version "$version" --bin "$bin" --threads "$threads" ;;
    judge)
      if [ $stub -eq 1 ]; then
        stage judge $py "$tdir/judge.py" --version "$version" --dry-run --stub-verdict
      else
        stage judge-dry $py "$tdir/judge.py" --version "$version" --dry-run
        note "judge: submit with judge.py --submit, collect, then resume with --from export"; exit 0
      fi ;;
    export) stage export $py "$tdir/export.py" --version "$version" ;;
  esac
done
note "OK $version"
