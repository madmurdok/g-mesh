#!/usr/bin/env bash
# The local test run: the sections a change can affect, faster than CI runs
# them. Why and what it trades: docs/adr/0031-local-test-runs.md.
#
#   scripts/test-local.sh [--base <ref> | --paths-from <file>] [--per-process] [--keep-going] [--dry-run]
#   scripts/test-local.sh --full
#
# Without --full, in two parts:
#   1. lib: the selected core lib tests, in one libtest process
#      (`cargo test --workspace --lib -- --exact <names>`), names from
#      `cargo nextest list --profile local`. Skipped with --per-process.
#   2. nextest: every other selected test, one `cargo nextest run --profile
#      local` (integration tests, binaries, ISOLATED, plugin/sdk/wire sections).
# Both parts set G_MESH_CONTAINERS_SEEDS=2. --dry-run lists what each part
# would run and runs no test.
#
# --full is the batch-end run: `scripts/test-sections.sh run --profile ci
# --keep-going all` (every section, heavy tests included, every seed), then
# scripts/test-heavy-report.py against the `local` profile's heavy list.
#
# Invariants:
# - Sections and their filtersets come from scripts/test-sections.sh, the
#   selection from scripts/test-select.sh; nothing here copies either.
# - Every selected test is in exactly one part: part 2's filter is the union
#   of the selected sections minus part 1's filter.
# - The `default` and `ci` profiles are never changed by this script; CI does
#   not call it.
set -euo pipefail

cd "$(dirname "$0")/.."

# Lib tests that write process-wide state (env vars) or assert on spawn
# timing. They keep nextest's one process per test until each is changed to
# take its override as an argument.
ISOLATED='test(=shim::tests::a_bootstrap_lock_alone_is_enough_to_record_the_project_root)
 | test(=mcp::search_code::tests::handle_reports_a_tool_error_when_the_configured_model_is_unavailable)
 | test(/^mcp::search_code_wait_tests::/)
 | test(=daemon::lifecycle::tests::a_timed_out_file_change_relaunches_the_plugin_and_replays_the_dirty_file_without_blocking_another_language)
 | test(=daemon::lifecycle::tests::a_default_config_resolves_to_the_documented_defaults)
 | test(=daemon::lifecycle::tests::a_configured_plugin_idle_timeout_overrides_the_default)
 | test(=daemon::lifecycle::tests::a_configured_core_idle_timeout_overrides_the_default)
 | test(=daemon::lifecycle::tests::the_env_override_still_wins_over_a_configured_value)
 | test(=daemon::lifecycle::tests::only_a_well_formed_pid_in_the_environment_is_a_lifeline)
 | test(=daemon::semantic::tests::an_interrupted_pass_for_one_language_is_retried_without_rerunning_the_other)
 | test(=daemon::manifest::tests::default_roots_bundled_entry_resolves_to_the_sibling_plugins_directory)
 | test(=daemon::manifest::tests::the_override_env_var_replaces_the_entire_default_roots_list)
 | test(=daemon::registry::tests::two_languages_spawn_at_the_same_time_rather_than_one_after_the_other)'

# The sections whose tests are (partly) the g-mesh lib.
CORE_LIB_SECTIONS=" core-mcp core-daemon core-cli core-graph core-rest "

# Seconds the one-process lib run may take before it is killed: libtest has
# no per-test timeout.
LIB_BUDGET_SECS="${G_MESH_LIB_BUDGET_SECS:-600}"

usage() {
	cat >&2 <<'EOF'
usage: scripts/test-local.sh [--base <ref> | --paths-from <file>] [--per-process] [--keep-going] [--dry-run]
       scripts/test-local.sh --full
EOF
}

die() {
	echo "test-local: $*" >&2
	exit 2
}

python_cmd() {
	command -v python3 || command -v python || die "python3 is required"
}

full=0 per_process=0 keep_going=0 dry_run=0
select_args=()
while [ $# -gt 0 ]; do
	case "$1" in
	--base | --paths-from)
		[ $# -ge 2 ] || {
			usage
			exit 2
		}
		select_args+=("$1" "$2")
		shift 2
		;;
	--full)
		full=1
		shift
		;;
	--per-process)
		per_process=1
		shift
		;;
	--keep-going)
		keep_going=1
		shift
		;;
	--dry-run)
		dry_run=1
		shift
		;;
	*)
		usage
		exit 2
		;;
	esac
done

target_dir="${CARGO_TARGET_DIR:-target}"

if [ "$full" -eq 1 ]; then
	[ ${#select_args[@]} -eq 0 ] && [ "$per_process" -eq 0 ] && [ "$dry_run" -eq 0 ] ||
		die "--full takes no other option"
	junit_dir="$target_dir/nextest/ci"
	rm -f "$junit_dir"/junit-*.xml
	status=0
	env -u G_MESH_CONTAINERS_SEEDS scripts/test-sections.sh run --profile ci --keep-going all || status=$?
	shopt -s nullglob
	junits=("$junit_dir"/junit-*.xml)
	shopt -u nullglob
	if [ ${#junits[@]} -gt 0 ]; then
		"$(python_cmd)" scripts/test-heavy-report.py .config/nextest.toml "${junits[@]}" ||
			echo "test-local: the heavy-list report failed" >&2
	else
		echo "test-local: no JUnit files in $junit_dir; no heavy-list report" >&2
	fi
	exit "$status"
fi

selection="$(scripts/test-select.sh "${select_args[@]+"${select_args[@]}"}")"
echo "== selected: $selection"
if [ "$selection" = none ]; then
	echo "== no sections selected"
	exit 0
fi
# shellcheck disable=SC2206,SC2207 # one section name per word
if [ "$selection" = full ]; then
	sections=($(scripts/test-sections.sh list))
else
	sections=($selection)
fi

# Joins the filtersets of the given sections with `|`.
union_of() {
	local s out=""
	for s in "$@"; do
		out="${out:+$out | }($(scripts/test-sections.sh filter "$s"))"
	done
	echo "$out"
}

lib_sections=()
for s in "${sections[@]}"; do
	case "$CORE_LIB_SECTIONS" in *" $s "*) lib_sections+=("$s") ;; esac
done

all_filter="$(union_of "${sections[@]}")"
lib_filter=""
if [ ${#lib_sections[@]} -gt 0 ] && [ "$per_process" -eq 0 ]; then
	lib_filter="package(g-mesh) & kind(lib) & ($(union_of "${lib_sections[@]}")) & not ($ISOLATED)"
	rest_filter="($all_filter) & not ($lib_filter)"
else
	rest_filter="$all_filter"
fi

export G_MESH_CONTAINERS_SEEDS=2

if [ "$dry_run" -eq 0 ]; then
	scripts/prune-stale-objects.sh
	cargo nextest run --workspace --no-run
fi

# The tests matching $2 under the `local` profile, one `<binary id> <name>`
# per line; with $1 set, only those of binary $1, name only.
listed() {
	cargo nextest list --workspace --profile local --message-format json -E "$2" |
		"$(python_cmd)" -c '
import json, sys
only = sys.argv[1]
for suite in json.load(sys.stdin)["rust-suites"].values():
    if only and suite["binary-id"] != only:
        continue
    for name, case in suite["testcases"].items():
        if case["filter-match"]["status"] == "matches":
            print(name if only else suite["binary-id"] + " " + name)
' "$1"
}

# Runs "$@" in its own process group, killed whole after $1 seconds.
run_with_budget() {
	local budget="$1" pid status=0
	shift
	set -m
	"$@" &
	pid=$!
	set +m
	trap 'kill -TERM -- "-$pid" 2>/dev/null; exit 130' INT TERM
	local start=$SECONDS
	while kill -0 "$pid" 2>/dev/null; do
		if [ $((SECONDS - start)) -ge "$budget" ]; then
			kill -TERM -- "-$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
			trap - INT TERM
			echo "test-local: the lib run passed its ${budget}s budget and was killed;" \
				"re-run with --per-process to find the hung test" >&2
			return 124
		fi
		sleep 1
	done
	wait "$pid" || status=$?
	trap - INT TERM
	return "$status"
}

log_dir="$target_dir/test-local"
mkdir -p "$log_dir"
failed=0
summary=()

if [ -n "$lib_filter" ]; then
	# A substitution, not `< <(...)`: a failing listing must stop the script,
	# never read as "no tests".
	names_text="$(listed g-mesh "$lib_filter")"
	names=()
	while IFS= read -r name; do
		[ -z "$name" ] || names+=("$name")
	done <<<"$names_text"
	if [ "$dry_run" -eq 1 ]; then
		echo "== lib (one process): ${#names[@]} tests: -E $lib_filter"
	elif [ ${#names[@]} -eq 0 ]; then
		summary+=("== lib (one process): no tests")
	else
		echo "== lib (one process): ${#names[@]} tests"
		start=$SECONDS
		status=0
		# shellcheck disable=SC2016 # expanded by the inner bash
		run_with_budget "$LIB_BUDGET_SECS" bash -c \
			'log="$1"; shift; cargo test --workspace --lib -- --exact "$@" 2>&1 | tee "$log"; exit "${PIPESTATUS[0]}"' \
			_ "$log_dir/lib.log" "${names[@]}" || status=$?
		passed="$(sed -n 's/^test result: .* \([0-9]*\) passed; \([0-9]*\) failed.*/\1/p' "$log_dir/lib.log" | awk '{s+=$1} END {print s+0}')"
		nfailed="$(sed -n 's/^test result: .* \([0-9]*\) passed; \([0-9]*\) failed.*/\2/p' "$log_dir/lib.log" | awk '{s+=$1} END {print s+0}')"
		result=PASS
		[ "$status" -eq 0 ] || {
			result=FAIL
			failed=1
		}
		summary+=("== lib (one process): $result, $passed passed, $nfailed failed, $((SECONDS - start))s (log: $log_dir/lib.log)")
	fi
fi

if [ "$dry_run" -eq 1 ]; then
	rest_text="$(listed "" "$rest_filter")"
	echo "== nextest: $(grep -c . <<<"$rest_text" || true) tests: -E $rest_filter"
elif [ "$failed" -eq 1 ] && [ "$keep_going" -eq 0 ]; then
	summary+=("== nextest: not run, the lib part failed (--keep-going runs it)")
else
	start=$SECONDS
	status=0
	cargo nextest run --workspace --profile local --no-tests=pass -E "$rest_filter" 2>&1 |
		tee "$log_dir/nextest.log" || status=$?
	result=PASS
	[ "$status" -eq 0 ] || {
		result=FAIL
		failed=1
	}
	line="$(grep -E '^ *Summary \[' "$log_dir/nextest.log" | tail -n 1 | sed 's/^ *//' || true)"
	summary+=("== nextest: $result, $((SECONDS - start))s: ${line:-no summary line} (log: $log_dir/nextest.log)")
fi

heavy="$(awk '/^\[profile\.local\]/ {p = 1; next} /^\[/ {p = 0} p' .config/nextest.toml | grep -c 'binary_id(' || true)"
summary+=("== skipped locally: $heavy heavy tests (run: scripts/test-local.sh --full)")
printf '%s\n' "${summary[@]}"
exit "$failed"
