# shellcheck shell=bash
#
# GM-613: "can this machine execute what it just built, and is it really the
# architecture it claims to be". Sourced by scripts/build-targets.sh and the
# four scripts/bundle-*-plugin.sh scripts, which used to each decide this with
# `[ "$target" = "$host" ]` on their own.
#
# Since GM-613 release.yml builds x86_64-apple-darwin on an arm64 `macos-15`
# runner (cross-built: Apple clang carries both SDK slices) and executes the
# result under Rosetta, so every smoke check those scripts and
# scripts/release-smoke.sh already had runs on the shipped x86_64 binary.
# Rosetta would just as happily run an arm64 binary that was mislabelled
# x86_64, which is why the architecture is asserted with `lipo`, not assumed.
#
# A smoke check that cannot run must not look like one that passed:
# with `G_MESH_REQUIRE_SMOKE=1` (release.yml's build job sets it) a skip is a
# failure. Without it (a local cross-build) the skip is logged, as before.

# can_run_target <target> <host>: true when a binary built for <target> can be
# executed here - natively, or an x86_64 macOS binary on an arm64 Mac with
# Rosetta installed (probed, not assumed).
can_run_target() {
	local target="$1" host="$2"
	[ "$target" = "$host" ] && return 0
	if [ "$host" = "aarch64-apple-darwin" ] && [ "$target" = "x86_64-apple-darwin" ]; then
		arch -x86_64 /usr/bin/true >/dev/null 2>&1 && return 0
	fi
	return 1
}

# smoke_skip_or_die <what> <target> <host>: the one place a smoke check is
# skipped. Fatal under G_MESH_REQUIRE_SMOKE=1.
smoke_skip_or_die() {
	local what="$1" target="$2" host="$3"
	if [ "${G_MESH_REQUIRE_SMOKE:-}" = "1" ]; then
		echo "runnable: $what cannot run: $target cannot be executed on $host (for x86_64-apple-darwin on an arm64 Mac: is Rosetta installed?), and G_MESH_REQUIRE_SMOKE=1 forbids skipping it" >&2
		exit 1
	fi
	echo "==> smoke test skipped: $target is not the host ($host) and cannot be executed here ($what)"
}

# assert_macho_arch <binary> <target>: on a macOS target, `lipo -archs` must
# print exactly that target's architecture (no fat binary, no wrong slice).
# A no-op on every other target.
assert_macho_arch() {
	local bin="$1" target="$2" want got
	case "$target" in
	x86_64-apple-darwin) want="x86_64" ;;
	aarch64-apple-darwin) want="arm64" ;;
	*) return 0 ;;
	esac
	if ! command -v lipo >/dev/null 2>&1; then
		smoke_skip_or_die "the architecture check of $bin (no lipo on PATH)" "$target" "$(uname -sm)"
		return 0
	fi
	got="$(lipo -archs "$bin")" || {
		echo "runnable: lipo could not read $bin" >&2
		exit 1
	}
	if [ "$got" != "$want" ]; then
		echo "runnable: $bin is '$got', expected exactly '$want' for $target" >&2
		exit 1
	fi
	echo "==> architecture: $(basename "$bin") is $got"
}
