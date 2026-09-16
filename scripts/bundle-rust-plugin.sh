#!/usr/bin/env bash
#
# Builds the bundled Rust plugin (plugins/rust) for one release target and
# stages it, with a manifest naming its own binary, as a `rust/` plugin
# directory core can discover next to its own executable.
#
#   scripts/bundle-rust-plugin.sh                                   # host target, into dist/plugins
#   scripts/bundle-rust-plugin.sh x86_64-pc-windows-msvc /tmp/stage # explicit target and destination
#
# `scripts/build-targets.sh` calls this while staging a release archive; it
# is runnable on its own so the bundle can be built and inspected without
# packaging a whole release.
#
# ---------------------------------------------------------------------------
# WHY THIS BUILDS THE SAME WAY CORE DOES, NOT LIKE EITHER OTHER BUNDLER (GM-288)
#
# The Rust plugin is a cargo binary in the *same workspace* as core (see the
# root Cargo.toml and docs/architecture/multi-language-plugins.md's Rust
# plugin "Distribution" line), so it is built exactly the way `build-targets.sh`
# already builds core for a target: `rustup target add`, then `cargo build
# --target <target>` from the crate's own directory, on whichever runner that
# target's row in .github/workflows/release.yml runs on. That is a deliberate
# choice not to invent a second mechanism the way the other two bundlers each
# had to:
#   - scripts/bundle-plugin.sh (JS/TS) embeds the *host's own* Node runtime
#     (Node SEA), so it can only ever be built on the platform it targets -
#     there is no cross-build option for it at all.
#   - A hypothetical GOOS/GOARCH-style cross-compile (the shape
#     scripts/bundle-go-plugin.sh uses on the Go plugin, when that plugin
#     exists in this branch's history) would technically also work here -
#     plugins/rust has no C dependency of its own, unlike core - but that
#     would be a second, plugin-specific build path for a crate that already
#     sits inside core's own workspace and is already built once per target by
#     the existing native release matrix. One mechanism, reused, is the
#     point.
#
# This script therefore does not refuse a non-host target the way
# bundle-plugin.sh does (there is no runtime to embed that would make that
# refusal correct), but it also does not promise a non-host target will link:
# whether `cargo build --target <target>` succeeds from a given host is
# exactly the same question it is for core, answered the same way (a native
# runner per target in CI; the one proven exception - macOS x86_64 ->
# aarch64 - noted in release.yml's own "WHY A MATRIX ON NATIVE RUNNERS"
# section applies here too, since it is a property of the host's toolchain,
# not of this crate).
#
# ---------------------------------------------------------------------------
# WINDOWS NAMING, AND WHY THIS PLUGIN NEEDS NO GM-283-STYLE REWRITE
#
# The Go plugin's checked-in manifest names a command with no `.exe`, and `go
# build -o <name>` never appends one even for a Windows target - so
# bundle-go-plugin.sh has to generate a manifest whose command is rewritten
# per target. Cargo does not have that gap: building *any* binary crate with
# `--target x86_64-pc-windows-msvc` names the output `<bin-name>.exe` on its
# own, the same way core/Cargo.toml's own binary already does (see
# build-targets.sh's `bin_name="g-mesh.exe"` for the windows case). So
# `exe_name_for` below only has to know the *filename* that will exist on
# disk after the build; the manifest still has to be regenerated (its command
# is a relative path, and the checked-in one is the dev-time path into
# `target/debug/`, not the installed one), but not because of a naming defect
# - see the next section.
#
# ---------------------------------------------------------------------------
# WHY THE MANIFEST IS STILL GENERATED, NOT REUSED AS-IS
#
# plugins/rust/plugin.toml's checked-in `[plugin.spawn] command` is
# `../../target/debug/g-mesh-plugin-rust` - a path into the *workspace's*
# build directory, meaningful only from inside a checkout
# (`g-mesh plugins check plugins/rust --fixture <dir>` run from the repo
# root, per that file's own header comment). An installed layout has no
# workspace around it at all. The installed manifest this script writes is
# derived from that file with only its `command` line rewritten to `./<exe
# name staged beside it>` - the same one-substitution pattern
# scripts/bundle-go-plugin.sh uses for the Go plugin's installed manifest and
# scripts/bundle-plugin.sh hand-writes field-by-field for the TS plugin -
# chosen over hand-duplicating every other field because the checked-in file
# is the one place `[plugin.languages]`/`[plugin.capabilities]`/
# `[plugin.workspace]` are declared, and a second, independently maintained
# copy of them is exactly the drift this substitution avoids.
#
# Environment:
#   CARGO_PROFILE  cargo profile (default: release; matches build-targets.sh)
# ---------------------------------------------------------------------------
#
# WHAT ENDS UP IN THE STAGED DIRECTORY
#
#   rust/
#     plugin.toml                installed manifest; spawns the binary below
#     g-mesh-plugin-rust[.exe]   the plugin binary, statically linking the SDK
#                                and wire crates - see this script's own
#                                "decision 4" note in the caller for why
#                                plugins/sdk and wire/ never ship separately
#
# `rust` as the directory name is required, not cosmetic:
# `core/src/daemon/manifest.rs` enforces that a manifest's `language` equals
# its containing directory's name (the same rule plugins/typescript's and
# plugins/go's manifests document).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLUGIN_DIR="$REPO_ROOT/plugins/rust"
CARGO_PROFILE="${CARGO_PROFILE:-release}"

# Kept in step with `SUPPORTED_TARGETS` in scripts/build-targets.sh.
declare -a SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

die() {
	echo "bundle-rust-plugin: $*" >&2
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
	*-windows-*) echo "g-mesh-plugin-rust.exe" ;;
	*) echo "g-mesh-plugin-rust" ;;
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

	log "building the Rust plugin for $target (profile: $CARGO_PROFILE)"
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
	stage="$dest/rust"
	# `$REPO_ROOT/target`, not `plugins/rust/target`: this is the workspace's
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
	local marker='command = "../../target/debug/g-mesh-plugin-rust"'
	grep -qF "$marker" "$src_manifest" ||
		die "$src_manifest no longer contains '$marker' - update this script's substitution to match its new spelling"

	{
		echo "# Bundled Rust plugin manifest, as installed. Generated by"
		echo "# scripts/bundle-rust-plugin.sh from plugins/rust/plugin.toml - edit that"
		echo "# file, not this one. Only the [plugin.spawn] command line differs from it,"
		echo "# rewritten to name $exe_name, the binary actually staged beside this"
		echo "# manifest for $target (see GM-288 in that script for why)."
		echo "#"
		sed "s#$marker#command = \"./$exe_name\"#" "$src_manifest"
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
		*'"language":"rust"'*) log "handshake ok" ;;
		*) die "the staged Rust plugin did not produce a handshake (got: ${handshake:-<nothing>})" ;;
		esac
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "staged: $stage ($(du -sh "$stage" | cut -f1))"
}

main "$@"
