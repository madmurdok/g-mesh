#!/usr/bin/env bash
#
# Builds the bundled TypeScript/JavaScript plugin (plugins/typescript) for one
# release target and stages it, with a manifest naming its own binary, as a
# `typescript/` plugin directory core can discover next to its own executable.
#
#   scripts/bundle-plugin.sh                                   # host target, into dist/plugins
#   scripts/bundle-plugin.sh x86_64-pc-windows-msvc /tmp/stage # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it is
# runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# ---------------------------------------------------------------------------
# WHY THIS IS A CARGO BUILD, NOT A NODE SEA (GM-326)
#
# Until GM-324/GM-351 the TS plugin was an npm package, and this script
# compiled it into a Node single-executable application: esbuild, a SEA blob
# injected with postject into a copy of the build machine's own `node`, the
# grammars' native addons in `node_modules/` beside it, and the embedded
# runtime's LICENSE-nodejs. All of that is gone. The plugin is now a Rust
# binary crate in the *same cargo workspace* as core (see the root
# Cargo.toml), so it is built exactly the way scripts/bundle-rust-plugin.sh
# (GM-288) and scripts/bundle-python-plugin.sh (GM-298) build theirs: `rustup
# target add`, then `cargo build --target <target>` from the crate's own
# directory, on whichever runner that target's row in
# .github/workflows/release.yml runs on. Building it needs no Node.js, no npm
# and no network beyond what cargo already fetches for core.
#
# Like those two, this does not refuse a non-host target (there is no
# runtime to embed any more), but it does not promise a non-host target will
# link either: the tree-sitter grammars compile C for the target, the same
# question core's own cross-build answers (see bundle-rust-plugin.sh's
# header).
#
# The semantic tier is not bundled. It talks to vtsls, an external language
# server found at runtime - `PATH`, the indexed project's own
# `node_modules/.bin`, then `npx` - per plugins/typescript/plugin.toml's
# `[plugin.semantic]` section. Without one the structural tier answers alone.
#
# ---------------------------------------------------------------------------
# WHY THE MANIFEST IS GENERATED, NOT REUSED AS-IS
#
# plugins/typescript/plugin.toml's checked-in `[plugin.spawn] command` is
# `${G_MESH_BIN_DIR}/g-mesh-plugin-typescript` - a path into the running
# g-mesh's own build directory (GM-404), meaningful only from inside a
# checkout. The installed manifest this script writes is that file with only
# its `command` line rewritten to `./<exe name staged beside it>`, the same
# one-substitution pattern the Rust and Python bundlers use, so
# `plugin_version` and every other table are carried over unchanged:
# `plugin_version` tracks the release (GM-303's rule for a workspace member),
# and scripts/cut-release.sh checks it against this crate's Cargo.toml.
#
# Cargo names a Windows build `<bin>.exe` on its own, so `exe_name_for` only
# has to know the filename that will exist on disk.
#
# Environment:
#   CARGO_PROFILE  cargo profile (default: release; matches build-targets.sh)
# ---------------------------------------------------------------------------
#
# WHAT ENDS UP IN THE STAGED DIRECTORY
#
#   typescript/
#     plugin.toml                      installed manifest; spawns the binary below
#     g-mesh-plugin-typescript[.exe]   the plugin binary, statically linking the
#                                      SDK, wire and tree-sitter grammar crates
#
# `typescript` as the directory name is required, not cosmetic:
# `core/src/daemon/manifest.rs` enforces that a manifest's `language` equals
# its containing directory's name.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR="$REPO_ROOT/plugins/typescript"
CARGO_PROFILE="${CARGO_PROFILE:-release}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

die() {
	echo "bundle-plugin: $*" >&2
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
	*-windows-*) echo "g-mesh-plugin-typescript.exe" ;;
	*) echo "g-mesh-plugin-typescript" ;;
	esac
}

main() {
	local target="${1:-}" dest="${2:-$REPO_ROOT/dist/plugins}"
	[ -n "$target" ] || target="$(host_triple)"

	printf '%s\n' "${SUPPORTED_TARGETS[@]}" | grep -qx "$target" ||
		die "unsupported target: $target (see scripts/build-targets.sh --list)"

	command -v rustup >/dev/null 2>&1 || die "rustup is required"
	command -v cargo >/dev/null 2>&1 || die "cargo is required"

	log "installing rust std for $target (no-op if already present)"
	rustup target add "$target" >/dev/null

	log "building the TypeScript plugin for $target (profile: $CARGO_PROFILE)"
	(cd "$PLUGIN_DIR" && cargo build --profile "$CARGO_PROFILE" --target "$target")

	# Cargo names the output directory after the profile, with one exception:
	# the `dev` profile builds into `debug/`.
	local profile_dir="$CARGO_PROFILE"
	if [ "$profile_dir" = "dev" ]; then
		profile_dir="debug"
	fi

	local exe_name stage built
	exe_name="$(exe_name_for "$target")"
	stage="$dest/typescript"
	# `$REPO_ROOT/target`, not `plugins/typescript/target`: this is the
	# workspace's shared build directory.
	built="$REPO_ROOT/target/$target/$profile_dir/$exe_name"
	[ -f "$built" ] || die "expected binary not found: $built"

	rm -rf "$stage"
	mkdir -p "$stage"
	cp "$built" "$stage/$exe_name"
	chmod +x "$stage/$exe_name" 2>/dev/null || true

	log "generating $stage/plugin.toml for $exe_name"
	local src_manifest="$PLUGIN_DIR/plugin.toml"
	# The placeholder is literal on purpose: it is what the manifest spells (GM-404).
	# shellcheck disable=SC2016
	local marker='command = "${G_MESH_BIN_DIR}/g-mesh-plugin-typescript"'
	grep -qF "$marker" "$src_manifest" ||
		die "$src_manifest no longer contains '$marker' - update this script's substitution to match its new spelling"

	{
		echo "# Bundled TypeScript/JavaScript plugin manifest, as installed. Generated"
		echo "# by scripts/bundle-plugin.sh from plugins/typescript/plugin.toml - edit"
		echo "# that file, not this one. Only the [plugin.spawn] command line differs"
		echo "# from it, rewritten to name $exe_name, the binary actually staged beside"
		echo "# this manifest for $target (see GM-326 in that script for why)."
		echo "#"
		# `[$]`: a literal `$` in the sed pattern, whatever position it is in.
		sed "s#${marker/\$/[\$]}#command = \"./$exe_name\"#" "$src_manifest"
	} >"$stage/plugin.toml"

	grep -qF "command = \"./$exe_name\"" "$stage/plugin.toml" ||
		die "failed to rewrite the command line in the staged manifest"

	# Only meaningful when we built for the machine we are standing on - a
	# cross-built binary cannot be executed here.
	local host
	host="$(host_triple)"
	if [ "$target" = "$host" ]; then
		log "smoke test: handshake with no input"
		local handshake
		handshake="$(printf '' | "$stage/$exe_name" "$REPO_ROOT" 2>/dev/null || true)"
		case "$handshake" in
		*'"language":"typescript"'*) log "handshake ok" ;;
		*) die "the staged TypeScript plugin did not produce a handshake (got: ${handshake:-<nothing>})" ;;
		esac
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
