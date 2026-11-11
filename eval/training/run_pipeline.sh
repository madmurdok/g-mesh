#!/usr/bin/env bash
# One fail-fast run of the GM-456 training-data pipeline
# (docs/design/GM-456-training-data.md, section 7), meant to be launched once
# in the background. Every stage runs under `taskpolicy -b`; long stages are
# timed with /usr/bin/time -p and the machine's `uptime` is recorded beside
# them. The compact summary goes to <version>/summary.txt, each stage's full
# output to <version>/logs/<stage>.log.
#
# usage: eval/training/run_pipeline.sh --version v0.0-stub [--stub] [--count N]
#          [--live [--jobs 3]] [--skip-setup] [--threads 2] [--bin target/release/g-mesh] <repo>...
#   --stub   stub queries and stub judge verdicts, no model call (the S2 run).
#   --live   generate.py --run and judge.py --run in-line: headless `claude -p`
#            per pack (section 2b). A usage limit stops the run; relaunching
#            the same command skips every pack already stored.
#   With neither, the generate and judge stages stop at --dry-run.
#   --from <stage>  start at setup|nodes|sample|render|generate|mine|judge|filter|export.
# filter runs leak_guard.py --filter on queries.judged.jsonl; export reads the
# filtered rows and still runs leak_guard.py --check last.
# Exit 0 done (or stopped at a dry run), 1 failed, 2 usage, 3 resumable stop.
# Run from the repository root.
set -euo pipefail

version=""; stub=0; live=0; jobs=3; count=""; skip_setup=0; threads=2; bin="target/release/g-mesh"; from="setup"
repos=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift 2 ;;
    --stub) stub=1; shift ;;
    --live) live=1; shift ;;
    --jobs) jobs="$2"; shift 2 ;;
    --count) count="$2"; shift 2 ;;
    --skip-setup) skip_setup=1; shift ;;
    --threads) threads="$2"; shift 2 ;;
    --bin) bin="$2"; shift 2 ;;
    --from) from="$2"; shift 2 ;;
    *) repos+=("$1"); shift ;;
  esac
done
[ -n "$version" ] || { echo "--version is required" >&2; exit 2; }
[ $stub -eq 1 ] && [ $live -eq 1 ] && { echo "--stub and --live exclude each other" >&2; exit 2; }
order="setup nodes sample render generate mine judge filter export"
case " $order " in *" $from "*) ;; *) echo "--from: one of $order" >&2; exit 2 ;; esac

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
  # A step's exit 3 (claude_cli.STOP_EXIT: usage limit, failed probe) is a
  # resumable stop: the run exits 3 with a STOP line; relaunching the same
  # command resumes it. Any other non-zero exit is a failure (exit 1).
  local name="$1"; shift
  local rc=0
  note "== $name  ($(uptime | sed 's/.*load averages*: /load /'))"
  /usr/bin/time -p taskpolicy -b "$@" > "$vdir/logs/$name.log" 2>&1 || rc=$?
  if [ $rc -eq 3 ]; then
    note "STOP: $name stopped, resumable (exit 3; log $vdir/logs/$name.log); relaunch the same command to resume"
    tail -25 "$vdir/logs/$name.log" >> "$summary"
    exit 3
  elif [ $rc -ne 0 ]; then
    note "FAILED: $name (exit $rc; log $vdir/logs/$name.log)"
    tail -25 "$vdir/logs/$name.log" >> "$summary"
    exit 1
  fi
  grep -E '^(real|user|sys) ' "$vdir/logs/$name.log" | tr '\n' ' ' >> "$summary"; echo >> "$summary"
  grep -E '^(nodes |sample:|render |generate|mine:|judge|leak_guard:|  nearest-eval|filter:|export:|collect:|run |STOP)' \
    "$vdir/logs/$name.log" | cut -c1-2000 >> "$summary" || true
}

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
      elif [ $live -eq 1 ]; then
        stage generate-dry $py "$tdir/generate.py" --version "$version" --dry-run
        stage generate $py "$tdir/generate.py" --version "$version" --run --jobs "$jobs"
      else
        stage generate-dry $py "$tdir/generate.py" --version "$version" --dry-run
        note "generate: run generate.py --run (or relaunch with --live), then resume with --from mine"; exit 0
      fi ;;
    mine) stage mine $py "$tdir/mine.py" --version "$version" --bin "$bin" --threads "$threads" ;;
    judge)
      if [ $stub -eq 1 ]; then
        stage judge $py "$tdir/judge.py" --version "$version" --dry-run --stub-verdict
      elif [ $live -eq 1 ]; then
        stage judge $py "$tdir/judge.py" --version "$version" --run --jobs "$jobs"
      else
        stage judge-dry $py "$tdir/judge.py" --version "$version" --dry-run
        note "judge: run judge.py --run (or relaunch with --live), then resume with --from filter"; exit 0
      fi ;;
    filter)
      # The judged rows minus every leak-guard hit: queries.judged.filtered.jsonl,
      # with the dropped hits in filter.leaks.jsonl and the report in
      # filter.guard.json (names export does not delete).
      stage filter $py "$tdir/leak_guard.py" --version "$version" --filter \
        --dataset "$vdir/queries.judged.jsonl" ;;
    export) stage export $py "$tdir/export.py" --version "$version" --filtered ;;
  esac
done
note "OK $version"
