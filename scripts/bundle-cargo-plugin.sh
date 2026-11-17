#!/usr/bin/env bash
#
# Builds a bundled plugin that is a cargo binary in core's own workspace
# (plugins/rust, plugins/python) for one release target and stages it, with a
# manifest naming its own binary, as a `<language>/` plugin directory core can
# discover next to its own executable.
#
#   scripts/bundle-cargo-plugin.sh rust                                   # host target, into dist/plugins
#   scripts/bundle-cargo-plugin.sh python x86_64-pc-windows-msvc /tmp/stage # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it
# is runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# Build: the plugin is built exactly the way build-targets.sh builds core for
# a target: `rustup target add`, then `cargo build --target <target>` from the
# crate's own directory, on whichever runner that target's row in
# .github/workflows/release.yml runs on. A non-host target is not refused, but
# whether it links from a given host is the same question it is for core.
# Cargo names a Windows binary `<bin-name>.exe` on its own, so
# `exe_name_for` only has to know the filename that exists after the build.
#
# Manifest: plugins/<language>/plugin.toml's `command` is the dev-time path
# `${G_MESH_BIN_DIR}/g-mesh-plugin-<language>` into the workspace's build
# directory, meaningless in an installed layout. The installed manifest is
# derived from it with only that line rewritten (scripts/lib/bundle-tail.sh).
#
# Environment:
#   CARGO_PROFILE  cargo profile (default: release; matches build-targets.sh)
#
# What ends up in the staged directory:
#
#   <language>/
#     plugin.toml                       installed manifest; spawns the binary below
#     g-mesh-plugin-<language>[.exe]    the plugin binary, statically linking the
#                                       SDK, SDK-LSP and wire crates (plugins/sdk,
#                                       plugins/sdk-lsp and wire/ never ship
#                                       separately)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/lib/runnable.sh
. "$REPO_ROOT/scripts/lib/runnable.sh"
# shellcheck source=scripts/lib/bundle-tail.sh
. "$REPO_ROOT/scripts/lib/bundle-tail.sh"
CARGO_PROFILE="${CARGO_PROFILE:-release}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

die() {
	echo "bundle-cargo-plugin: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

host_triple() {
	rustc -vV | awk '/^host: / { print $2 }'
}

exe_name_for() {
	case "$1" in
	*-windows-*) echo "g-mesh-plugin-$LANGUAGE.exe" ;;
	*) echo "g-mesh-plugin-$LANGUAGE" ;;
	esac
}

main() {
	LANGUAGE="${1:-}"
	case "$LANGUAGE" in
	rust) DISPLAY_NAME="Rust" ;;
	python) DISPLAY_NAME="Python" ;;
	*) die "usage: bundle-cargo-plugin.sh <rust|python> [target] [dest]" ;;
	esac
	local plugin_dir="$REPO_ROOT/plugins/$LANGUAGE"
	local target="${2:-}" dest="${3:-$REPO_ROOT/dist/plugins}"
	[ -n "$target" ] || target="$(host_triple)"

	printf '%s\n' "${SUPPORTED_TARGETS[@]}" | grep -qx "$target" ||
		die "unsupported target: $target (see scripts/build-targets.sh --list)"

	command -v rustup >/dev/null 2>&1 || die "rustup is required"
	command -v cargo >/dev/null 2>&1 || die "cargo is required"

	log "installing rust std for $target (no-op if already present)"
	rustup target add "$target" >/dev/null

	log "building the $DISPLAY_NAME plugin for $target (profile: $CARGO_PROFILE)"
	(cd "$plugin_dir" && cargo build --profile "$CARGO_PROFILE" --target "$target")

	# Cargo names the output directory after the profile, with one exception:
	# the `dev` profile builds into `debug/` - the same mapping
	# build-targets.sh applies to core's own build output.
	local profile_dir="$CARGO_PROFILE"
	if [ "$profile_dir" = "dev" ]; then
		profile_dir="debug"
	fi

	local exe_name stage built
	exe_name="$(exe_name_for "$target")"
	stage="$dest/$LANGUAGE"
	# `$REPO_ROOT/target`, not `plugins/<language>/target`: the workspace's
	# shared build directory, whichever member's manifest the build ran through.
	built="$REPO_ROOT/target/$target/$profile_dir/$exe_name"
	[ -f "$built" ] || die "expected binary not found: $built"

	rm -rf "$stage"
	mkdir -p "$stage"
	cp "$built" "$stage/$exe_name"
	chmod +x "$stage/$exe_name" 2>/dev/null || true

	# The placeholder is literal on purpose: it is what the manifest spells.
	# shellcheck disable=SC2016
	local marker='command = "${G_MESH_BIN_DIR}/g-mesh-plugin-'"$LANGUAGE"'"'
	stage_manifest_and_smoke "$LANGUAGE" "$DISPLAY_NAME" "bundle-cargo-plugin.sh" \
		"$plugin_dir" "$stage" "$exe_name" "$target" "$marker"

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
