#!/usr/bin/env bash
# The test suite as named sections, each a nextest filterset over the whole
# workspace. Why sections live here rather than in nextest profiles:
# docs/adr/0030-test-sections.md.
#
#   scripts/test-sections.sh list                    section names, in run order
#   scripts/test-sections.sh filter <section>        the section's filterset
#   scripts/test-sections.sh run [--profile P] [--keep-going] <section>...|all|full|none
#   scripts/test-sections.sh check                   the sections partition the suite
#
# `filter` and `run` also take the selectors `core-spawns-<plugin>` (plugin:
# typescript, rust, python, go): the core tests that start that plugin's real
# process, by module, as scripts/test-plugin-spawns.txt lists them.
#
# Invariants:
# - The sections partition the suite: every test, ignored ones included, is in
#   exactly one section. `check` proves it; CI and the release gate run it.
# - `core-rest` is the complement of the other core lib sections, so a new
#   top-level core module lands in it without an edit here.
# - Every section runs with `--workspace` and selects by `package()`, never
#   `-p`: all sections share one build and one feature unification, and the
#   plugin and SDK binaries that core tests spawn always exist.
# - `core-spawns-<plugin>` selectors are not sections: they overlap core
#   sections, are not in `list`, and `check` does not see them.
#
# A section can be run by hand without this script:
#   cargo nextest run --workspace -E "$(scripts/test-sections.sh filter core-mcp)"
set -euo pipefail

cd "$(dirname "$0")/.."

SECTIONS=(
	core-it
	core-mcp
	core-daemon
	core-cli
	core-graph
	core-rest
	sdk
	plugin-typescript
	plugin-rust
	plugin-python
	wire
)

SPAWN_MAP=scripts/test-plugin-spawns.txt

# Prints the modules of binary $2 (`it`, `scripts` or `lib`) whose row in
# SPAWN_MAP names plugin $1, joined with `|`. A row of another binary fails.
spawn_modules() {
	[ -r "$SPAWN_MAP" ] || {
		echo "test-sections: cannot read $SPAWN_MAP" >&2
		exit 2
	}
	awk -v p="$1" -v b="$2" -v map="$SPAWN_MAP" '
		{ sub(/\r$/, ""); sub(/#.*/, "") }
		NF == 0 { next }
		{
			i = index($1, ":")
			bin = substr($1, 1, i - 1)
			if (i == 0 || (bin != "it" && bin != "scripts" && bin != "lib")) {
				printf "test-sections: %s:%d: bad row \"%s\"\n", map, NR, $1 > "/dev/stderr"
				bad = 1
				exit 2
			}
			if (bin != b) next
			for (f = 2; f <= NF; f++) if ($f == p) { out = out (out == "" ? "" : "|") substr($1, i + 1); break }
		}
		END { if (!bad) print out }
	' "$SPAWN_MAP"
}

# Prints the filterset of the core tests that SPAWN_MAP says spawn plugin $1.
spawns_filter() {
	local mods clauses=""
	mods="$(spawn_modules "$1" it)" || exit 2
	[ -z "$mods" ] || clauses="(binary_id(g-mesh::it) & test(/^($mods)::/))"
	mods="$(spawn_modules "$1" scripts)" || exit 2
	[ -z "$mods" ] || clauses="${clauses:+$clauses | }(binary_id(g-mesh::scripts) & test(/^($mods)::/))"
	mods="$(spawn_modules "$1" lib)" || exit 2
	[ -z "$mods" ] || clauses="${clauses:+$clauses | }(kind(lib) & test(/^($mods)::/))"
	echo "package(g-mesh) & (${clauses:-none()})"
}

# Prints the filterset of section (or `core-spawns-<plugin>` selector) $1;
# returns 1 for an unknown name.
section_filter() {
	case "$1" in
	core-spawns-typescript | core-spawns-rust | core-spawns-python | core-spawns-go)
		spawns_filter "${1#core-spawns-}"
		;;
	core-it) echo 'package(g-mesh) & kind(test)' ;;
	core-mcp) echo 'package(g-mesh) & kind(lib) & test(/^mcp::/)' ;;
	core-daemon) echo 'package(g-mesh) & kind(lib) & test(/^daemon::/)' ;;
	core-cli) echo 'package(g-mesh) & kind(lib) & test(/^cli::/)' ;;
	core-graph) echo 'package(g-mesh) & kind(lib) & test(/^graph::/)' ;;
	core-rest) echo 'package(g-mesh) & (kind(bin) | (kind(lib) & !test(/^(mcp|daemon|cli|graph)::/)))' ;;
	sdk) echo 'package(g-mesh-plugin-sdk)' ;;
	plugin-typescript) echo 'package(g-mesh-plugin-typescript)' ;;
	plugin-rust) echo 'package(g-mesh-plugin-rust)' ;;
	plugin-python) echo 'package(g-mesh-plugin-python)' ;;
	wire) echo 'package(g-mesh-wire)' ;;
	*) return 1 ;;
	esac
}

usage() {
	cat >&2 <<'EOF'
usage: scripts/test-sections.sh list
       scripts/test-sections.sh filter <section>
       scripts/test-sections.sh run [--profile P] [--keep-going] <section>...|all|full|none
       scripts/test-sections.sh check
<section> may also be core-spawns-<plugin> (typescript, rust, python, go).
EOF
}

die_unknown() {
	echo "test-sections: unknown section '$1' (known: ${SECTIONS[*]}, core-spawns-<plugin>)" >&2
	exit 2
}

python_cmd() {
	# `python3` on the Unix images, `python` on windows-2022.
	command -v python3 || command -v python || {
		echo "test-sections: python3 is required" >&2
		exit 2
	}
}

require_nextest() {
	if ! cargo nextest --version >/dev/null 2>&1; then
		echo "test-sections: cargo-nextest is not installed (cargo install --locked cargo-nextest)" >&2
		exit 2
	fi
}

# Prints "<tests> <failures+errors>" from a JUnit file's root element, or "- -".
junit_counts() {
	local root tests failures errors
	root="$(grep -o -m1 '<testsuites[^>]*>' "$1" 2>/dev/null || true)"
	tests="$(sed -n 's/.* tests="\([0-9]*\)".*/\1/p' <<<"$root")"
	failures="$(sed -n 's/.* failures="\([0-9]*\)".*/\1/p' <<<"$root")"
	errors="$(sed -n 's/.* errors="\([0-9]*\)".*/\1/p' <<<"$root")"
	if [ -z "$tests" ]; then
		echo "- -"
	else
		echo "$tests $((${failures:-0} + ${errors:-0}))"
	fi
}

cmd_run() {
	local profile=default keep_going=0 saw_none=0
	local -a names=()
	while [ $# -gt 0 ]; do
		case "$1" in
		--profile)
			[ $# -ge 2 ] || {
				usage
				exit 2
			}
			profile="$2"
			shift 2
			;;
		--keep-going)
			keep_going=1
			shift
			;;
		# `full` and `none` are scripts/test-select.sh's answers.
		all | full)
			names+=("${SECTIONS[@]}")
			shift
			;;
		none)
			saw_none=1
			shift
			;;
		-*)
			usage
			exit 2
			;;
		*)
			names+=("$1")
			shift
			;;
		esac
	done
	if [ ${#names[@]} -eq 0 ] && [ "$saw_none" -eq 1 ]; then
		echo "== no sections selected"
		return 0
	fi
	[ ${#names[@]} -gt 0 ] || {
		usage
		exit 2
	}
	local name
	for name in "${names[@]}"; do
		section_filter "$name" >/dev/null || die_unknown "$name"
	done
	require_nextest
	# Before any build: only earlier builds' objects exist yet, so nothing
	# this run links can be pruned.
	scripts/prune-stale-objects.sh

	local junit_dir="${CARGO_TARGET_DIR:-target}/nextest/$profile"
	local failed=()
	if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
		printf '| section | result | seconds | tests | failed |\n| --- | --- | ---: | ---: | ---: |\n' >>"$GITHUB_STEP_SUMMARY"
	fi
	for name in "${names[@]}"; do
		local filter start status=0 secs result counts
		filter="$(section_filter "$name")"
		echo "== $name: $filter"
		# A stale JUnit file from an earlier invocation must not be taken for
		# this section's.
		rm -f "$junit_dir/junit.xml"
		start="$(date +%s)"
		cargo nextest run --workspace --profile "$profile" --no-tests=fail -E "$filter" || status=$?
		secs=$(($(date +%s) - start))
		result=PASS
		[ "$status" -eq 0 ] || result=FAIL
		counts="- -"
		if [ -f "$junit_dir/junit.xml" ]; then
			mv "$junit_dir/junit.xml" "$junit_dir/junit-$name.xml"
			counts="$(junit_counts "$junit_dir/junit-$name.xml")"
		fi
		echo "== $name: $result in ${secs}s"
		if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
			# shellcheck disable=SC2086 # word-splits "<tests> <failed>" on purpose
			printf '| %s | %s | %s | %s | %s |\n' "$name" "$result" "$secs" $counts >>"$GITHUB_STEP_SUMMARY"
		fi
		if [ "$status" -ne 0 ]; then
			failed+=("$name")
			[ "$keep_going" -eq 1 ] || break
		fi
	done
	if [ ${#failed[@]} -gt 0 ]; then
		echo "== failed sections: ${failed[*]}" >&2
		return 1
	fi
}

cmd_check() {
	require_nextest
	local py name
	py="$(python_cmd)"
	# Global, not local: the EXIT trap runs after this function returns.
	CHECK_DIR="$(mktemp -d)"
	trap 'rm -rf "$CHECK_DIR"' EXIT
	local dir="$CHECK_DIR"
	local -a args=("$dir/all.json")
	echo "== listing the whole suite"
	cargo nextest list --workspace --run-ignored all --message-format json >"$dir/all.json"
	for name in "${SECTIONS[@]}"; do
		echo "== listing $name"
		cargo nextest list --workspace --run-ignored all --message-format json \
			-E "$(section_filter "$name")" >"$dir/$name.json"
		args+=("$name=$dir/$name.json")
	done
	"$py" scripts/test-sections-check.py "${args[@]}"
}

case "${1:-}" in
list)
	printf '%s\n' "${SECTIONS[@]}"
	;;
filter)
	[ $# -eq 2 ] || {
		usage
		exit 2
	}
	section_filter "$2" || die_unknown "$2"
	;;
run)
	shift
	cmd_run "$@"
	;;
check)
	[ $# -eq 1 ] || {
		usage
		exit 2
	}
	cmd_check
	;;
*)
	usage
	exit 2
	;;
esac
