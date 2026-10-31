#!/usr/bin/env bash
# Observes which core tests start a real plugin process, and checks or writes
# scripts/test-plugin-spawns.txt, the map behind
# `scripts/test-sections.sh filter core-spawns-<plugin>`. Why a map:
# docs/adr/0031-local-test-runs.md.
#
#   scripts/test-plugin-spawns.sh observe [--profile P] [--write | -E <filterset>]
#
# `-E` observes only the core tests that also match <filterset> (a hand check
# of a few modules); it never writes, and reports no unexercised entries.
#
# How: builds the workspace and lists its test binaries, replaces each real
# plugin binary (target/debug/g-mesh-plugin-{typescript,rust,python},
# plugins/go/g-mesh-plugin-go) with a wrapper that appends
# "$NEXTEST_BINARY_ID <tab> $NEXTEST_TEST_NAME <tab> <plugin>" to a log and
# execs the real binary, then runs every core section from the listed binaries
# without a cargo build, and restores the binaries. Test failures are ignored:
# a wrapped binary can fail tests that hash or stat it, and only spawns count.
# The log and the observed map stay in <target>/plugin-spawns/.
#
# Without --write it compares the observation with the map: a spawn the map
# lacks is an error (exit 1), a map entry nothing exercised is a warning. With
# --write it writes the map: one row per `core/tests/it` module (`none` when it
# spawned nothing), `lib:`/`scripts:` rows only for modules that spawned; a
# plugin an existing row lists is kept (rows of removed `it` modules go).
#
# Invariants:
# - Nothing rebuilds while the wrappers are installed: a cargo build re-links
#   target/debug/<bin> from deps/ and silently drops them.
# - The real binaries are back in place when this script exits, also after a
#   failure; a later run first restores any left by a killed one.
# - An observation without a single spawn is an error, never an empty map.
set -euo pipefail

cd "$(dirname "$0")/.."

MAP=scripts/test-plugin-spawns.txt
TARGET="${CARGO_TARGET_DIR:-target}"
case "$TARGET" in /*) ;; *) TARGET="$PWD/$TARGET" ;; esac
OUT="$TARGET/plugin-spawns"
REAL_SUFFIX=.observe-real

usage() {
	echo "usage: scripts/test-plugin-spawns.sh observe [--profile P] [--write | -E <filterset>]" >&2
}

die() {
	echo "test-plugin-spawns: $*" >&2
	exit 2
}

# Prints "<plugin> <binary path>" for every plugin the observer wraps.
plugin_binaries() {
	local p
	for p in typescript rust python; do
		echo "$p $TARGET/debug/g-mesh-plugin-$p"
	done
	echo "go $PWD/plugins/go/g-mesh-plugin-go"
}

restore_binaries() {
	local p bin
	while read -r p bin; do
		if [ -e "$bin$REAL_SUFFIX" ]; then
			mv -f "$bin$REAL_SUFFIX" "$bin"
		fi
	done < <(plugin_binaries)
}

install_wrappers() {
	local p bin
	while read -r p bin; do
		if [ ! -x "$bin" ]; then
			echo "== $p: no binary at $bin, not observed"
			continue
		fi
		mv "$bin" "$bin$REAL_SUFFIX"
		cat >"$bin" <<EOF
#!/bin/sh
printf '%s\t%s\t%s\n' "\${NEXTEST_BINARY_ID:--}" "\${NEXTEST_TEST_NAME:--}" $p >>'$OUT/spawns.log'
exec '$bin$REAL_SUFFIX' "\$@"
EOF
		chmod +x "$bin"
	done < <(plugin_binaries)
}

cmd_observe() {
	local profile=default write=0 only=""
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
		--write)
			write=1
			shift
			;;
		-E)
			[ $# -ge 2 ] || {
				usage
				exit 2
			}
			only="$2"
			shift 2
			;;
		*)
			usage
			exit 2
			;;
		esac
	done
	[ "$write" -eq 0 ] || [ -z "$only" ] || die "--write and -E are exclusive: a partial run cannot write the map"
	command -v python3 >/dev/null || die "python3 is required"
	cargo nextest --version >/dev/null 2>&1 || die "cargo-nextest is not installed"

	restore_binaries
	mkdir -p "$OUT"
	rm -f "$OUT/spawns.log" "$OUT/observed.txt"
	: >"$OUT/spawns.log"

	local name filter=""
	for name in $(scripts/test-sections.sh list); do
		case "$name" in
		core-*) filter="${filter:+$filter | }($(scripts/test-sections.sh filter "$name"))" ;;
		esac
	done
	[ -n "$filter" ] || die "scripts/test-sections.sh list printed no core section"
	[ -z "$only" ] || filter="($filter) & ($only)"

	echo "== building and listing the workspace's test binaries"
	cargo nextest list --workspace --list-type binaries-only --message-format json >"$OUT/binaries.json"
	cargo metadata --format-version 1 --all-features --no-deps >"$OUT/cargo-metadata.json"

	trap restore_binaries EXIT
	trap 'exit 130' INT TERM
	install_wrappers
	echo "== running the core sections, profile $profile, wrappers installed"
	local status=0
	G_MESH_CONTAINERS_SEEDS="${G_MESH_CONTAINERS_SEEDS:-2}" \
		cargo nextest run --profile "$profile" --no-fail-fast --no-tests=pass \
		--binaries-metadata "$OUT/binaries.json" --cargo-metadata "$OUT/cargo-metadata.json" \
		-E "$filter" || status=$?
	restore_binaries
	trap - EXIT INT TERM
	# 100: tests failed, which is expected here; anything else: no test ran.
	case "$status" in
	0 | 100) echo "== core run finished (exit $status; test failures are ignored here)" ;;
	*) die "cargo nextest run could not run the tests (exit $status)" ;;
	esac

	local partial=0
	[ -z "$only" ] || partial=1
	python3 - "$OUT/spawns.log" "$OUT/observed.txt" "$MAP" "$write" core/tests/it/main.rs "$partial" <<'PY'
import re
import sys

log_path, observed_path, map_path, write, main_rs, partial = sys.argv[1:7]
write = write == "1"
partial = partial == "1"
PREFIX = {"g-mesh::it": "it", "g-mesh::scripts": "scripts", "g-mesh": "lib"}

lines = [l.rstrip("\n").split("\t") for l in open(log_path) if l.strip()]
observed = {}
outside = other = 0
for binary, test, plugin in lines:
    if binary == "-" or test == "-":
        outside += 1
        continue
    prefix = PREFIX.get(binary)
    if prefix is None:
        other += 1
        continue
    if prefix == "lib":
        module = test.rsplit("::", 1)[0]
    else:
        module = test.split("::", 1)[0]
    observed.setdefault(f"{prefix}:{module}", set()).add(plugin)
print(f"== {len(lines)} spawns logged: {len(lines) - outside - other} by core tests, "
      f"{outside} without a nextest test name, {other} by other binaries")
if not observed:
    sys.exit("test-plugin-spawns: observed nothing: no core test started a wrapped plugin")

if not partial:
    it_modules = [m for m in re.findall(r"^\s*(?:pub\s+)?mod\s+(\w+)\s*;", open(main_rs).read(), re.M)
                  if m != "common"]
    for m in it_modules:
        observed.setdefault(f"it:{m}", set())

ORDER = ["typescript", "rust", "python", "go"]
def order(key):
    prefix, module = key.split(":", 1)
    return (["it", "scripts", "lib"].index(prefix), module)
def row(key, plugins):
    names = " ".join(p for p in ORDER if p in plugins) or "none"
    return f"{key:<56} {names}\n"

open(observed_path, "w").writelines(row(k, observed[k]) for k in sorted(observed, key=order))

mapped = {}
try:
    map_lines = open(map_path).readlines()
except FileNotFoundError:
    if not write:
        sys.exit(f"test-plugin-spawns: no {map_path}; --write creates it")
    map_lines = []
for line in map_lines:
    line = line.split("#", 1)[0].split()
    if line:
        mapped[line[0]] = set(line[1:]) - {"none"}

if write:
    # A plugin already in a row stays: rows widened by review (a spawn this
    # run did not exercise, such as an ignored test's) are not lost.
    merged = {k: set(v) for k, v in observed.items()}
    for key, plugins in mapped.items():
        if key.startswith("it:") and key not in observed:
            continue
        merged.setdefault(key, set()).update(plugins)
    rows = [row(k, merged[k]) for k in sorted(merged, key=order)]
    header = (
        "# Core test modules and the real plugin processes they start.\n"
        "# Read by `scripts/test-sections.sh filter core-spawns-<plugin>`; written\n"
        "# and checked by `scripts/test-plugin-spawns.sh observe`.\n"
        "#\n"
        "# <binary>:<module>   <plugins it spawns, or none>\n"
        "# binary: it (core/tests/it, one row per module), scripts (core/tests/scripts),\n"
        "# lib (core lib test modules; only those that spawn a plugin).\n"
        "# `observe --write` adds what it sees and never drops a listed plugin.\n"
    )
    open(map_path, "w").writelines([header] + rows)
    print(f"== wrote {map_path} ({len(rows)} rows)")
    sys.exit(0)

errors = 0
for key in sorted(observed, key=order):
    if key not in mapped:
        if observed[key] or key.startswith("it:"):
            print(f"error: {key} has no row in {map_path} (observed: {' '.join(sorted(observed[key])) or 'none'})")
            errors += 1
        continue
    for p in sorted(observed[key] - mapped[key]):
        print(f"error: {key} spawns {p}, its row does not list it")
        errors += 1
for key in sorted(mapped, key=order) if not partial else []:
    for p in sorted(mapped[key] - observed.get(key, set())):
        print(f"warning: {key} lists {p}, no test of it spawned {p} in this run")
if errors:
    sys.exit(f"test-plugin-spawns: {errors} spawn(s) missing from {map_path}")
print(f"== {map_path} covers every observed spawn")
PY
}

case "${1:-}" in
observe)
	shift
	cmd_observe "$@"
	;;
*)
	usage
	exit 2
	;;
esac
