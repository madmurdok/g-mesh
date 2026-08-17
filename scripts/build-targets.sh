#!/usr/bin/env bash
#
# Builds `g-mesh` for one or more Rust target triples and packages each build
# into a release archive with a stable, predictable name.
#
# This script is the *single* implementation of "build -> stage -> archive ->
# checksum". CI (.github/workflows/release.yml) calls it instead of repeating
# the same tar/zip incantations in YAML, so the artifact names a release
# produces are the ones you can reproduce on your own machine by running this
# script - there is no second, subtly different code path that only exists
# inside a workflow file.
#
#   scripts/build-targets.sh                      # host target only
#   scripts/build-targets.sh x86_64-apple-darwin  # one explicit target
#   scripts/build-targets.sh --list               # the four supported triples
#
# Environment:
#   G_MESH_VERSION  version string used in artifact names (default: the
#                   `version` field of core/Cargo.toml)
#   DIST_DIR        where archives land (default: <repo>/dist)
#   CARGO_PROFILE   cargo profile (default: release)
#
# ---------------------------------------------------------------------------
# KNOWN LIMITATION - the artifacts this produces are NOT yet independently
# usable, and that is expected at this point in the project.
#
# `core/src/daemon/plugin.rs` resolves the bundled JS/TS plugin through
# `env!("CARGO_MANIFEST_DIR")`, i.e. the path to *this checkout* is baked into
# the binary at compile time. A binary unpacked anywhere else therefore cannot
# find `plugins/typescript/dist/src/index.js`, and the daemon refuses to start
# without it. Teaching the binary to find a plugin next to itself, and putting
# that plugin in the archive, is task #65 (plugin bundling) - deliberately not
# done here. Until then these archives are useful for testing the *pipeline*,
# not for shipping to users.
# ---------------------------------------------------------------------------

set -euo pipefail

# The platform set REQUIREMENTS.md implies. Kept here rather than only in the
# CI matrix so `--list` can state it in one authoritative place.
SUPPORTED_TARGETS=(
	x86_64-apple-darwin
	aarch64-apple-darwin
	x86_64-unknown-linux-gnu
	x86_64-pc-windows-msvc
)

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${DIST_DIR:-$REPO_ROOT/dist}"
CARGO_PROFILE="${CARGO_PROFILE:-release}"

die() {
	echo "build-targets: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

host_triple() {
	rustc -vV | awk '/^host: / { print $2 }'
}

# The `version` of the [package] section of core/Cargo.toml. Restricted to that
# section on purpose: `version = ` also appears under [dependencies].
crate_version() {
	awk '
		/^\[/ { in_package = ($0 == "[package]") }
		in_package && /^version[[:space:]]*=/ {
			gsub(/[",]/, "", $3); print $3; exit
		}
	' "$REPO_ROOT/core/Cargo.toml"
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1"
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1"
	else
		die "no sha256sum/shasum available to checksum $1"
	fi
}

# Windows gets a .zip (what someone downloading a Windows build expects, and
# what every other Rust CLI ships); everything else gets a .tar.gz.
archive_ext_for() {
	case "$1" in
	*-windows-*) echo "zip" ;;
	*) echo "tar.gz" ;;
	esac
}

# Creates $2 (an archive) from the directory $1, which must live inside
# $DIST_DIR. Both branches store the staging directory itself as the single
# top-level entry, so unpacking never scatters files into the current
# directory.
make_archive() {
	local stage_dir="$1" archive_path="$2"
	local parent base
	parent="$(dirname "$stage_dir")"
	base="$(basename "$stage_dir")"

	case "$archive_path" in
	*.zip)
		# GitHub's windows runners ship 7-Zip; Git Bash's GNU tar cannot write
		# zip files, so PowerShell is the fallback rather than `tar -a`.
		if command -v 7z >/dev/null 2>&1; then
			(cd "$parent" && 7z a -tzip -bso0 -bsp0 "$(basename "$archive_path")" "$base")
		elif command -v powershell.exe >/dev/null 2>&1; then
			# PowerShell cannot read Git Bash's `/c/...` style paths, so hand it
			# native Windows ones.
			local win_stage="$stage_dir" win_archive="$archive_path"
			if command -v cygpath >/dev/null 2>&1; then
				win_stage="$(cygpath -w "$stage_dir")"
				win_archive="$(cygpath -w "$archive_path")"
			fi
			powershell.exe -NoProfile -NonInteractive -Command \
				"Compress-Archive -Path '$win_stage' -DestinationPath '$win_archive' -Force"
		else
			die "cannot create $archive_path: neither 7z nor powershell.exe found"
		fi
		;;
	*)
		tar -czf "$archive_path" -C "$parent" "$base"
		;;
	esac
}

build_one() {
	local target="$1" version="$2" host="$3"
	local bin_name="g-mesh" ext stage_dir archive_path

	case "$target" in
	*-windows-*) bin_name="g-mesh.exe" ;;
	esac
	ext="$(archive_ext_for "$target")"

	log "installing rust std for $target (no-op if already present)"
	rustup target add "$target" >/dev/null

	log "building g-mesh $version for $target (profile: $CARGO_PROFILE)"
	(cd "$REPO_ROOT/core" && cargo build --profile "$CARGO_PROFILE" --target "$target")

	# Cargo names the output directory after the profile, with one exception:
	# the `dev` profile builds into `debug/`.
	local profile_dir="$CARGO_PROFILE"
	if [ "$profile_dir" = "dev" ]; then
		profile_dir="debug"
	fi

	local built="$REPO_ROOT/core/target/$target/$profile_dir/$bin_name"
	[ -f "$built" ] || die "expected binary not found: $built"

	stage_dir="$DIST_DIR/g-mesh-v$version-$target"
	rm -rf "$stage_dir"
	mkdir -p "$stage_dir"
	cp "$built" "$stage_dir/$bin_name"
	# License texts must travel with the binary (MIT OR Apache-2.0 both require
	# the notice be distributed); the README explains `g-mesh model fetch`,
	# which a downloaded binary needs before `search_code` works.
	cp "$REPO_ROOT/LICENSE" "$REPO_ROOT/LICENSE-MIT" "$REPO_ROOT/LICENSE-APACHE" \
		"$REPO_ROOT/README.md" "$stage_dir/"

	archive_path="$DIST_DIR/g-mesh-v$version-$target.$ext"
	rm -f "$archive_path"
	make_archive "$stage_dir" "$archive_path"
	[ -f "$archive_path" ] || die "archive was not created: $archive_path"

	(cd "$DIST_DIR" && sha256_of "$(basename "$archive_path")" >"$(basename "$archive_path").sha256")

	# Only meaningful when we built for the machine we are standing on. A
	# cross-built binary cannot be executed here, so this stays a native-only
	# check rather than a silently skipped one that looks like it passed.
	if [ "$target" = "$host" ]; then
		log "smoke test: $bin_name --version"
		local reported
		reported="$("$stage_dir/$bin_name" --version)"
		echo "$reported"
		# The binary's version comes from CARGO_PKG_VERSION (clap's bare
		# `version` in cli/mod.rs), so it must agree with the version in the
		# archive's name. When it does not, something stale got packaged - the
		# exact failure that is invisible until a user reports it.
		case "$reported" in
		*"$version"*) ;;
		*) die "version mismatch: archive says $version, binary says '$reported'" ;;
		esac
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "artifact: $archive_path"
}

main() {
	if [ "${1:-}" = "--list" ]; then
		printf '%s\n' "${SUPPORTED_TARGETS[@]}"
		return 0
	fi

	command -v rustup >/dev/null 2>&1 || die "rustup is required"
	command -v cargo >/dev/null 2>&1 || die "cargo is required"

	local host version
	host="$(host_triple)"
	version="${G_MESH_VERSION:-$(crate_version)}"
	[ -n "$version" ] || die "could not determine version from core/Cargo.toml"

	local targets=("$@")
	if [ ${#targets[@]} -eq 0 ]; then
		targets=("$host")
	fi

	mkdir -p "$DIST_DIR"
	log "version=$version host=$host dist=$DIST_DIR"

	local target
	for target in "${targets[@]}"; do
		build_one "$target" "$version" "$host"
	done

	log "done: ${#targets[@]} target(s)"
}

main "$@"
