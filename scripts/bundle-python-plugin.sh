#!/usr/bin/env bash
#
# Builds the bundled Python plugin (plugins/python) for one release target and
# stages it, with a manifest naming its own binary, as a `python/` plugin
# directory core can discover next to its own executable.
#
#   scripts/bundle-python-plugin.sh                                   # host target, into dist/plugins
#   scripts/bundle-python-plugin.sh x86_64-pc-windows-msvc /tmp/stage # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it
# is runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# ---------------------------------------------------------------------------
# WHY THIS IS A COPY OF bundle-rust-plugin.sh'S SHAPE, NOT bundle-go-plugin.sh'S (GM-298)
#
# The Python plugin is, like the Rust plugin, a cargo binary in the *same
# workspace* as core (see the root Cargo.toml's `members` and
# plugins/python/Cargo.toml's own header), not a plugin written in the
# language it analyzes - it parses Python source with tree-sitter from Rust,
# the same way plugins/rust parses Rust source from Rust. So it is built
# exactly the way build-targets.sh already builds core and the Rust plugin
# for a target: `rustup target add`, then `cargo build --target <target>`
# from the crate's own directory, on whichever runner that target's row in
# .github/workflows/release.yml runs on. No Python interpreter is invoked to
# produce this binary, and none is required to run it - the structural tier
# needs no Python on the machine being indexed either (see
# plugins/python/plugin.toml's `capabilities.semantic_pass = false`; a
# pyright-based semantic tier, when it exists, is what would first need one).
#
# This mirrors bundle-rust-plugin.sh's own reasoning for why it does not
# reuse bundle-go-plugin.sh's cross-compile shape: there is no
# GOOS/GOARCH-style mechanism for a crate that already sits inside core's own
# workspace and is already built once per target by the existing native
# release matrix. One mechanism, reused, is the point - see
# bundle-rust-plugin.sh's own header for the longer version of this argument.
#
# ---------------------------------------------------------------------------
# WINDOWS NAMING, AND WHY THIS PLUGIN NEEDS NO GM-283-STYLE REWRITE
#
# Same as the Rust plugin: building any binary crate with `--target
# x86_64-pc-windows-msvc` names the output `<bin-name>.exe` on its own, so
# `exe_name_for` below only has to know the filename that will exist on disk
# after the build. The manifest is still regenerated - not because of a
# naming defect, but because the checked-in one's `command` is a dev-time
# path into the workspace's own `target/<profile>/`, meaningless in an installed
# layout with no workspace around it (see the next section).
#
# ---------------------------------------------------------------------------
# WHY THE MANIFEST IS STILL GENERATED, NOT REUSED AS-IS
#
# plugins/python/plugin.toml's checked-in `[plugin.spawn] command` is
# `${G_MESH_BIN_DIR}/g-mesh-plugin-python` - a path into the *workspace's*
# build directory (the running g-mesh's own `target/<profile>/`, GM-404),
# meaningful only from inside a checkout
# (`g-mesh plugins check plugins/python --fixture <dir>` run from the repo
# root, per that file's own header comment). The installed manifest this
# script writes is derived from that file with only its `command` line
# rewritten to `./<exe name staged beside it>` - the same one-substitution
# pattern bundle-rust-plugin.sh uses for the Rust plugin's installed
# manifest, chosen for the same reason: the checked-in file is the one place
# `[plugin.languages]`/`[plugin.capabilities]`/`[plugin.workspace]` are
# declared, and a second, independently maintained copy of them is exactly
# the drift this substitution avoids.
#
# Environment:
#   CARGO_PROFILE  cargo profile (default: release; matches build-targets.sh)
# ---------------------------------------------------------------------------
#
# WHAT ENDS UP IN THE STAGED DIRECTORY
#
#   python/
#     plugin.toml                  installed manifest; spawns the binary below
#     g-mesh-plugin-python[.exe]  the plugin binary, statically linking the SDK
#                                  and wire crates - it needs no Python
#                                  interpreter on the machine it runs on for
#                                  the structural tier
#
# `python` as the directory name is required, not cosmetic:
# `core/src/daemon/manifest.rs` enforces that a manifest's `language` equals
# its containing directory's name (the same rule plugins/rust's manifest
# documents for itself).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR="$REPO_ROOT/plugins/python"
CARGO_PROFILE="${CARGO_PROFILE:-release}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

die() {
	echo "bundle-python-plugin: $*" >&2
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
	*-windows-*) echo "g-mesh-plugin-python.exe" ;;
	*) echo "g-mesh-plugin-python" ;;
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

	log "building the Python plugin for $target (profile: $CARGO_PROFILE)"
	(cd "$PLUGIN_DIR" && cargo build --profile "$CARGO_PROFILE" --target "$target")

	# Cargo names the output directory after the profile, with one exception:
	# the `dev` profile builds into `debug/` - the same mapping
	# build-targets.sh applies to core's own build output.
	local profile_dir="$CARGO_PROFILE"
	if [ "$profile_dir" = "dev" ]; then
		profile_dir="debug"
	fi

	local exe_name stage built
	exe_name="$(exe_name_for "$target")"
	stage="$dest/python"
	# `$REPO_ROOT/target`, not `plugins/python/target`: this is the workspace's
	# shared build directory (see the root Cargo.toml's own module doc),
	# whichever member's manifest the build was invoked through.
	built="$REPO_ROOT/target/$target/$profile_dir/$exe_name"
	[ -f "$built" ] || die "expected binary not found: $built"

	rm -rf "$stage"
	mkdir -p "$stage"
	cp "$built" "$stage/$exe_name"
	chmod +x "$stage/$exe_name" 2>/dev/null || true

	# See this script's own header ("WHY THE MANIFEST IS STILL GENERATED") for
	# why the checked-in manifest is not reused as-is.
	log "generating $stage/plugin.toml for $exe_name"
	local src_manifest="$PLUGIN_DIR/plugin.toml"
	local marker='command = "${G_MESH_BIN_DIR}/g-mesh-plugin-python"'
	grep -qF "$marker" "$src_manifest" ||
		die "$src_manifest no longer contains '$marker' - update this script's substitution to match its new spelling"

	{
		echo "# Bundled Python plugin manifest, as installed. Generated by"
		echo "# scripts/bundle-python-plugin.sh from plugins/python/plugin.toml - edit"
		echo "# that file, not this one. Only the [plugin.spawn] command line differs"
		echo "# from it, rewritten to name $exe_name, the binary actually staged beside"
		echo "# this manifest for $target (see GM-298 in that script for why)."
		echo "#"
		# `[$]`: a literal `$` in the sed pattern, whatever position it is in.
		sed "s#${marker/\$/[\$]}#command = \"./$exe_name\"#" "$src_manifest"
	} >"$stage/plugin.toml"

	grep -qF "command = \"./$exe_name\"" "$stage/plugin.toml" ||
		die "failed to rewrite the command line in the staged manifest"

	# Only meaningful when we built for the machine we are standing on - a
	# cross-built binary cannot be executed here (build-targets.sh's own
	# `--version` smoke test is native-only for the same reason).
	local host
	host="$(host_triple)"
	if [ "$target" = "$host" ]; then
		log "smoke test: handshake with no input"
		local handshake
		handshake="$(printf '' | "$stage/$exe_name" "$REPO_ROOT" 2>/dev/null || true)"
		case "$handshake" in
		*'"language":"python"'*) log "handshake ok" ;;
		*) die "the staged Python plugin did not produce a handshake (got: ${handshake:-<nothing>})" ;;
		esac
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
