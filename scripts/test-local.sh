#!/usr/bin/env bash
# The local test run: the sections a change can affect, faster than CI runs
# them. Why and what it trades: docs/adr/0031-local-test-runs.md.
#
#   scripts/test-local.sh [--base <ref> | --paths-from <file>] [--per-process] [--keep-going] [--dry-run]
#   scripts/test-local.sh --full [--dry-run]
#
# In two parts, then the Go step:
#   1. lib: the selected lib tests, one libtest process per crate
#      (`cargo test -p <crate> --lib -- --exact <names>`), names from
#      `cargo nextest list`. Skipped with --per-process.
#   2. nextest: every other selected test, one `cargo nextest run`
#      (integration tests, binaries, ISOLATED).
#   Go: `go test ./...` in plugins/go, when scripts/test-select.sh --go says
#      the diff needs it, and always with --full.
# Without --full: the sections scripts/test-select.sh picks, profile `local`
# (its heavy tests skipped), G_MESH_CONTAINERS_SEEDS=2. --dry-run lists what
# each part would run and runs no test.
#
# --full is the batch-end run: every section under profile `ci` (heavy tests
# included, every seed), --keep-going forced. The heavy lib tests stay in
# part 2, so that its JUnit times every heavy test; then
# scripts/test-heavy-report.py checks that JUnit against the `local`
# profile's heavy list. Lib tests in part 1 have no per-test time.
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

# Lib tests kept on nextest's one process per test. No lib test writes
# process-wide state (env vars, the working directory): one that needs an
# override takes it as an argument (GM-549). The one left asserts that two
# plugin processes start while each other's spawn is in flight, within a
# deadline; the one-process lib run puts every other spawning test beside it,
# the load that breaks it, so it stays in the `daemon-spawning` group.
ISOLATED='test(=daemon::registry::tests::two_languages_spawn_at_the_same_time_rather_than_one_after_the_other)'

# The sections whose tests are (partly) a lib: the g-mesh lib, and the
# plugin, sdk and wire crates' libs.
LIB_SECTIONS=" core-mcp core-daemon core-cli core-graph core-rest sdk plugin-typescript plugin-rust plugin-python wire "

# Seconds the one-process lib run may take before it is killed: libtest has
# no per-test timeout.
LIB_BUDGET_SECS="${G_MESH_LIB_BUDGET_SECS:-600}"

usage() {
	cat >&2 <<'EOF'
usage: scripts/test-local.sh [--base <ref> | --paths-from <file>] [--per-process] [--keep-going] [--dry-run]
       scripts/test-local.sh --full [--dry-run]
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

# The plugins' test dependencies (vtsls, pyright) are found on PATH only
# (ADR 0032 D7); CI puts each plugin's node_modules/.bin there, so this does
# too. A dry run runs no test and needs none of them.
plugin_bin_dirs=""
for plugin in typescript python; do
	bin_dir="$PWD/plugins/$plugin/node_modules/.bin"
	if [ -d "$bin_dir" ]; then
		plugin_bin_dirs="${plugin_bin_dirs:+$plugin_bin_dirs:}$bin_dir"
	elif [ "$dry_run" -eq 0 ]; then
		die "plugins/$plugin/node_modules/.bin is missing; run scripts/test-deps.sh"
	fi
done
[ -z "$plugin_bin_dirs" ] || export PATH="$plugin_bin_dirs:$PATH"

target_dir="${CARGO_TARGET_DIR:-target}"

if [ "$full" -eq 1 ]; then
	[ ${#select_args[@]} -eq 0 ] && [ "$per_process" -eq 0 ] ||
		die "--full takes no option but --dry-run"
	profile=ci
	keep_going=1
	selection=full
	unset G_MESH_CONTAINERS_SEEDS
else
	profile=local
	selection="$(scripts/test-select.sh --narrow "${select_args[@]+"${select_args[@]}"}")"
	export G_MESH_CONTAINERS_SEEDS=2
fi
# The Go plugin's own suite is not a nextest section (CI's Go job runs it), so
# the selector answers separately: --full always, else a diff under plugins/go/.
if [ "$full" -eq 1 ]; then
	run_go=1
else
	run_go="$(scripts/test-select.sh --go "${select_args[@]+"${select_args[@]}"}")"
fi
echo "== selected: $selection$([ "$run_go" -eq 1 ] && echo " (+ go tests)")"
if [ "$selection" = none ] && [ "$run_go" -eq 0 ]; then
	echo "== no sections selected"
	exit 0
fi
# shellcheck disable=SC2206,SC2207 # one section name per word
if [ "$selection" = full ]; then
	sections=($(scripts/test-sections.sh list))
else
	sections=($selection)
fi

# The `local` profile's default-filter, on one line: `not (<heavy terms>)`.
local_default_filter() {
	local f
	f="$(awk '
		/^\[profile\.local\]/ { p = 1; next }
		/^\[/ { p = 0 }
		p && /^default-filter = """/ { d = 1; next }
		d && /^"""/ { exit }
		d
	' .config/nextest.toml | tr -s ' \n' ' ')"
	[ -n "$f" ] || die "no default-filter in [profile.local] of .config/nextest.toml"
	echo "$f"
}

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
	case "$LIB_SECTIONS" in *" $s "*) lib_sections+=("$s") ;; esac
done

all_filter="$(union_of "${sections[@]}")"
lib_filter=""
if [ ${#lib_sections[@]} -gt 0 ] && [ "$per_process" -eq 0 ]; then
	lib_filter="kind(lib) & ($(union_of "${lib_sections[@]}")) & not ($ISOLATED)"
	# `ci` has no default-filter: --full keeps the heavy lib tests out of part
	# 1 by the `local` one, so part 2 runs and times them. Without --full the
	# `local` profile skips them in both parts already.
	[ "$full" -eq 0 ] || lib_filter="$lib_filter & ($(local_default_filter))"
	rest_filter="($all_filter) & not ($lib_filter)"
else
	rest_filter="$all_filter"
fi

if [ "$dry_run" -eq 0 ]; then
	scripts/prune-stale-objects.sh
	cargo nextest run --workspace --no-run
fi

# The tests matching $1 under $profile, one `<package> <binary id> <name>`
# per line, grouped by binary.
listed() {
	cargo nextest list --workspace --profile "$profile" --message-format json -E "$1" |
		"$(python_cmd)" -c '
import json, sys
for suite in json.load(sys.stdin)["rust-suites"].values():
    for name, case in suite["testcases"].items():
        if case["filter-match"]["status"] == "matches":
            print(suite["package-name"], suite["binary-id"], name)
'
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
	listing="$(listed "$lib_filter")"
	# One argument list for every crate: `--crate=<package>` then its names.
	# The names are passed per crate because a name is unique only within its
	# binary: another crate's test of the same name is not selected.
	crate_args=() ntests=0 crates=() crate=""
	while read -r pkg bin name; do
		# A lib's binary id is its package name; any other binary is not libtest's.
		[ -n "$name" ] && [ "$bin" = "$pkg" ] || continue
		if [ "$pkg" != "$crate" ]; then
			crate="$pkg"
			crates+=("$pkg")
			crate_args+=("--crate=$pkg")
		fi
		crate_args+=("$name")
		ntests=$((ntests + 1))
	done <<<"$listing"
	if [ "$dry_run" -eq 1 ]; then
		echo "== lib (one process per crate): $ntests tests in ${crates[*]-no crate}: -E $lib_filter"
		for crate in "${crates[@]+"${crates[@]}"}"; do
			echo "   $crate: $(grep -c "^$crate $crate " <<<"$listing")"
		done
	elif [ "$ntests" -eq 0 ]; then
		summary+=("== lib (one process per crate): no tests")
	else
		echo "== lib (one process per crate): $ntests tests in ${crates[*]}"
		start=$SECONDS
		status=0
		# Every crate runs even after one fails; the status is the last failing
		# crate's (libtest: 101).
		# shellcheck disable=SC2016 # expanded by the inner bash
		run_with_budget "$LIB_BUDGET_SECS" bash -c '
			log="$1"
			shift
			: >"$log"
			status=0
			while [ $# -gt 0 ]; do
				crate="${1#--crate=}"
				shift
				names=()
				while [ $# -gt 0 ] && [ "${1#--crate=}" = "$1" ]; do
					names+=("$1")
					shift
				done
				cargo test -p "$crate" --lib -- --exact "${names[@]}" 2>&1 | tee -a "$log"
				s="${PIPESTATUS[0]}"
				[ "$s" -eq 0 ] || status="$s"
			done
			exit "$status"' \
			_ "$log_dir/lib.log" "${crate_args[@]}" || status=$?
		passed="$(sed -n 's/^test result: .* \([0-9]*\) passed; \([0-9]*\) failed.*/\1/p' "$log_dir/lib.log" | awk '{s+=$1} END {print s+0}')"
		nfailed="$(sed -n 's/^test result: .* \([0-9]*\) passed; \([0-9]*\) failed.*/\2/p' "$log_dir/lib.log" | awk '{s+=$1} END {print s+0}')"
		result=PASS
		[ "$status" -eq 0 ] || {
			result=FAIL
			failed=1
		}
		summary+=("== lib (one process per crate): $result, $passed passed, $nfailed failed, $((SECONDS - start))s (log: $log_dir/lib.log)")
	fi
fi

junit="$target_dir/nextest/ci/junit.xml"
if [ "$dry_run" -eq 1 ]; then
	rest_text="$(listed "$rest_filter")"
	echo "== nextest: $(grep -c . <<<"$rest_text" || true) tests: -E $rest_filter"
elif [ "$failed" -eq 1 ] && [ "$keep_going" -eq 0 ]; then
	summary+=("== nextest: not run, the lib part failed (--keep-going runs it)")
else
	start=$SECONDS
	status=0
	[ "$full" -eq 0 ] || rm -f "$junit"
	cargo nextest run --workspace --profile "$profile" --no-tests=pass -E "$rest_filter" 2>&1 |
		tee "$log_dir/nextest.log" || status=$?
	result=PASS
	[ "$status" -eq 0 ] || {
		result=FAIL
		failed=1
	}
	line="$(grep -E '^ *Summary \[' "$log_dir/nextest.log" | tail -n 1 | sed 's/^ *//' || true)"
	summary+=("== nextest: $result, $((SECONDS - start))s: ${line:-no summary line} (log: $log_dir/nextest.log)")
	if [ "$full" -eq 1 ]; then
		if [ -f "$junit" ]; then
			"$(python_cmd)" scripts/test-heavy-report.py .config/nextest.toml "$junit" ||
				echo "test-local: the heavy-list report failed" >&2
		else
			echo "test-local: no $junit; no heavy-list report" >&2
		fi
	fi
fi

# Go plugin tests, as CI's Go job runs them (`go test ./...` in plugins/go).
# A missing toolchain fails the run for a Go diff; it never skips.
if [ "$run_go" -eq 1 ]; then
	if [ "$dry_run" -eq 1 ]; then
		echo "== go: go test ./... (in plugins/go)"
	elif [ "$failed" -eq 1 ] && [ "$keep_going" -eq 0 ]; then
		summary+=("== go: not run, an earlier part failed (--keep-going runs it)")
	elif ! command -v go >/dev/null 2>&1; then
		summary+=("== go: FAIL, the go toolchain is not installed (plugins/go changed or --full)")
		failed=1
	else
		start=$SECONDS
		status=0
		(cd plugins/go && go test ./...) 2>&1 | tee "$log_dir/go.log" || status=$?
		result=PASS
		[ "$status" -eq 0 ] || {
			result=FAIL
			failed=1
		}
		summary+=("== go: $result, $((SECONDS - start))s (log: $log_dir/go.log)")
	fi
fi

if [ "$full" -eq 0 ]; then
	heavy="$(awk '/^\[profile\.local\]/ {p = 1; next} /^\[/ {p = 0} p' .config/nextest.toml | grep -c 'binary_id(' || true)"
	summary+=("== skipped locally: $heavy heavy tests (run: scripts/test-local.sh --full)")
fi
printf '%s\n' "${summary[@]+"${summary[@]}"}"
exit "$failed"
