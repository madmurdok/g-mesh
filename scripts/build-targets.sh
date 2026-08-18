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
#   scripts/build-targets.sh --version            # version the artifacts are named after
#   scripts/build-targets.sh --asset-names [target...]
#                                                 # names a full release publishes
#
# The three query flags answer questions *without building*, so they need
# neither cargo nor rustup and cost nothing to call. `--asset-names` exists for
# the publishing job in .github/workflows/release.yml: it uploads exactly the
# names this script says it emits, computed by the same code that emits them,
# so renaming an artifact here cannot silently desync the Release from the
# install script that fetches those names by URL.
#
# Environment:
#   G_MESH_VERSION  version string used in artifact names (default: the
#                   `version` field of core/Cargo.toml)
#   DIST_DIR        where archives land (default: <repo>/dist)
#   CARGO_PROFILE   cargo profile (default: release)
#   G_MESH_SKIP_PLUGIN_BUNDLE
#                   set to 1 to package the core binary alone - see below
#
# ---------------------------------------------------------------------------
# WHAT AN ARCHIVE CONTAINS
#
#   g-mesh[.exe]                     the core binary
#   plugins/typescript/              the JS/TS plugin as a self-contained
#                                    executable, its native addons, and the
#                                    plugin.toml core discovers it through
#                                    (built by scripts/bundle-plugin.sh)
#   LICENSE, LICENSE-MIT, LICENSE-APACHE, README.md
#
# The plugin is not optional dressing: core cannot index anything without one,
# so an archive without it is not a release artifact. Both halves have to be
# for the *same* platform, which is why each target is built on its own runner
# (see .github/workflows/release.yml) - a single-executable plugin embeds the
# build machine's own Node runtime and cannot be cross-built.
#
# `G_MESH_SKIP_PLUGIN_BUNDLE=1` exists for the one case that is still useful
# without a plugin: exercising the Rust cross-build path (macOS x86_64 ->
# aarch64 does work) when the resulting archive is known not to be shippable.
# It warns loudly, because that is exactly the state task 64 shipped in and
# task 65 was opened to fix.
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

resolve_version() {
	local version="${G_MESH_VERSION:-$(crate_version)}"
	[ -n "$version" ] || die "could not determine version from core/Cargo.toml"
	printf '%s\n' "$version"
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

# The two names every artifact is derived from, in one place. `build_one` and
# `--asset-names` both go through these, which is what makes "the names CI
# publishes" and "the names this script writes" the same string by
# construction rather than by two developers keeping two spellings in step.
artifact_stem_for() {
	echo "g-mesh-v$2-$1" # $1 = target, $2 = version
}

archive_name_for() {
	echo "$(artifact_stem_for "$1" "$2").$(archive_ext_for "$1")"
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
	local bin_name="g-mesh" stage_dir archive_path

	case "$target" in
	*-windows-*) bin_name="g-mesh.exe" ;;
	esac

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

	stage_dir="$DIST_DIR/$(artifact_stem_for "$target" "$version")"
	rm -rf "$stage_dir"
	mkdir -p "$stage_dir"
	cp "$built" "$stage_dir/$bin_name"
	# License texts must travel with the binary (MIT OR Apache-2.0 both require
	# the notice be distributed); the README explains `g-mesh model fetch`,
	# which a downloaded binary needs before `search_code` works.
	cp "$REPO_ROOT/LICENSE" "$REPO_ROOT/LICENSE-MIT" "$REPO_ROOT/LICENSE-APACHE" \
		"$REPO_ROOT/README.md" "$stage_dir/"

	# `plugins/` beside the binary is not a convention this script invents - it
	# is where `daemon::manifest::bundled_roots` looks in an installed layout,
	# which is what makes the unpacked archive work from any directory.
	if [ "${G_MESH_SKIP_PLUGIN_BUNDLE:-}" = "1" ]; then
		echo "build-targets: WARNING: G_MESH_SKIP_PLUGIN_BUNDLE=1 - packaging $target with no plugin. The resulting archive CANNOT index anything and must not be published." >&2
	else
		log "bundling the JS/TS plugin for $target"
		bash "$REPO_ROOT/scripts/bundle-plugin.sh" "$target" "$stage_dir/plugins"
	fi

	archive_path="$DIST_DIR/$(archive_name_for "$target" "$version")"
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

		# The check task 64 could not make: that the *staged* binary finds the
		# *staged* plugin, through the same discovery an unpacked archive uses
		# (`plugins/` beside the executable). This is the one that fails if the
		# path resolution regresses back to a compile-time path.
		if [ "${G_MESH_SKIP_PLUGIN_BUNDLE:-}" != "1" ]; then
			log "smoke test: the staged binary discovers the staged plugin"
			"$stage_dir/$bin_name" plugins list | grep -q "typescript" ||
				die "the staged binary does not discover the plugin staged beside it"
		fi
	else
		log "smoke test skipped: $target is not the host ($host)"
	fi

	log "artifact: $archive_path"
}

main() {
	# The query flags deliberately return before the toolchain checks below:
	# CI's publishing job asks this script for names on a runner that has no
	# Rust installed, and installing one just to print a string would be absurd.
	case "${1:-}" in
	--list)
		printf '%s\n' "${SUPPORTED_TARGETS[@]}"
		return 0
		;;
	--version)
		resolve_version
		return 0
		;;
	--asset-names)
		shift
		local asset_version asset_targets=("$@") asset_target archive
		asset_version="$(resolve_version)"
		if [ ${#asset_targets[@]} -eq 0 ]; then
			asset_targets=("${SUPPORTED_TARGETS[@]}")
		fi
		for asset_target in "${asset_targets[@]}"; do
			archive="$(archive_name_for "$asset_target" "$asset_version")"
			# Archive first, its checksum second: consumers that want only the
			# archive can take the odd lines, and the pair stays adjacent.
			printf '%s\n%s\n' "$archive" "$archive.sha256"
		done
		return 0
		;;
	-*)
		die "unknown flag: $1 (try --list, --version, --asset-names)"
		;;
	esac

	command -v rustup >/dev/null 2>&1 || die "rustup is required"
	command -v cargo >/dev/null 2>&1 || die "cargo is required"

	local host version
	host="$(host_triple)"
	version="$(resolve_version)"

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
