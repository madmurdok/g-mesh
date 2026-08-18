#!/usr/bin/env bash
#
# Checks that a directory holds exactly the assets a complete four-target
# release consists of, then writes the combined `SHA256SUMS` that gets
# published alongside them.
#
#   scripts/prepare-release-assets.sh dist
#
# Environment:
#   G_MESH_VERSION  the version the assets must be named after (default: the
#                   `version` field of core/Cargo.toml). CI sets this from the
#                   git tag, which is what turns "the tag and the crate version
#                   disagree" into a failed release instead of a published one.
#
# ---------------------------------------------------------------------------
# WHY THIS IS A SCRIPT AND NOT A FEW LINES OF YAML
#
# The publishing job in .github/workflows/release.yml uploads whatever this
# script blesses. The names it expects come from `build-targets.sh
# --asset-names`, i.e. from the same function that names the archives while
# building them - so the one failure this whole pipeline cannot afford, a
# Release whose asset URLs do not match what the install script fetches, is not
# guarded by two developers keeping two spellings in step. It is guarded by
# there being one spelling.
#
# Running it here rather than in YAML also means the exact check CI performs
# can be run against a local `dist/` after `build-targets.sh`, with no GitHub
# involved.
#
# WHAT IT REFUSES TO BLESS
#
#   - a missing or empty archive, or a missing `.sha256` beside one
#   - a checksum that does not match the bytes of the archive next to it
#   - a `.sha256` naming some other path than the archive's bare basename
#     (`sha256sum -c` runs in the user's download directory, so a `dist/...`
#     prefix in there would break verification for everyone downstream)
#   - an unexpected archive: a leftover from another version means the
#     directory is not one clean release
#
# It deliberately does NOT check that a release is complete in any sense
# beyond names and checksums. Whether the binaries inside work is what the
# smoke tests in build-targets.sh are for.
# ---------------------------------------------------------------------------

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASSET_DIR="${1:-$REPO_ROOT/dist}"

die() {
	echo "prepare-release-assets: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

# Same fallback chain as build-targets.sh: GNU coreutils on Linux, BSD's
# shasum on macOS. Duplicated rather than shared because sourcing that script
# would mean running it.
sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1"
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1"
	else
		die "no sha256sum/shasum available to checksum $1"
	fi
}

[ -d "$ASSET_DIR" ] || die "not a directory: $ASSET_DIR"

version="$(bash "$REPO_ROOT/scripts/build-targets.sh" --version)"

# `mapfile` would be shorter but does not exist in bash 3.2, which is what
# macOS ships and therefore what a local run of this script uses.
expected=()
while IFS= read -r line; do
	# `[ -n "$line" ] && expected+=(...)` would abort the whole script under
	# `set -e` the first time the test is false. Same reason for the explicit
	# `if` in the unexpected-file loop below.
	if [ -n "$line" ]; then
		expected+=("$line")
	fi
done < <(bash "$REPO_ROOT/scripts/build-targets.sh" --asset-names)

[ ${#expected[@]} -gt 0 ] || die "build-targets.sh --asset-names produced nothing"

log "expecting ${#expected[@]} assets for g-mesh v$version in $ASSET_DIR"

archives=()
for name in "${expected[@]}"; do
	path="$ASSET_DIR/$name"
	[ -f "$path" ] || die "missing release asset: $name"
	[ -s "$path" ] || die "release asset is empty: $name"
	case "$name" in
	*.sha256) ;;
	*) archives+=("$name") ;;
	esac
done

# Nothing but this release may be in the directory: a stale archive from an
# earlier version here means someone is publishing a mixed bag.
shopt -s nullglob
for path in "$ASSET_DIR"/*.tar.gz "$ASSET_DIR"/*.zip "$ASSET_DIR"/*.sha256; do
	name="$(basename "$path")"
	found=""
	for want in "${expected[@]}"; do
		if [ "$name" = "$want" ]; then
			found=1
			break
		fi
	done
	[ -n "$found" ] || die "unexpected file in $ASSET_DIR: $name (not part of the v$version release)"
done
shopt -u nullglob

for archive in "${archives[@]}"; do
	declared_sum="$(awk 'NR == 1 { print $1 }' "$ASSET_DIR/$archive.sha256")"
	# GNU coreutils marks a binary-mode digest as `<hash> *name`, and the
	# sha256sum Git Bash ships on the Windows runner may well produce that form
	# for the .zip. `sha256sum -c` accepts both, so this check has to as well -
	# otherwise the Windows asset alone would block every release.
	declared_name="$(awk 'NR == 1 { sub(/^\*/, "", $NF); print $NF }' "$ASSET_DIR/$archive.sha256")"
	actual_sum="$(cd "$ASSET_DIR" && sha256_of "$archive" | awk '{ print $1 }')"

	[ "$declared_name" = "$archive" ] ||
		die "$archive.sha256 refers to '$declared_name', not to '$archive' - sha256sum -c would fail after download"
	[ "$declared_sum" = "$actual_sum" ] ||
		die "checksum mismatch for $archive: file says $declared_sum, bytes hash to $actual_sum"

	log "verified $archive ($actual_sum)"
done

# One file a human can run `sha256sum -c SHA256SUMS` against, assembled from
# the per-asset files rather than recomputed, so the two can never disagree.
sums_path="$ASSET_DIR/SHA256SUMS"
: >"$sums_path"
for archive in "${archives[@]}"; do
	cat "$ASSET_DIR/$archive.sha256" >>"$sums_path"
done

line_count="$(wc -l <"$sums_path" | tr -d ' ')"
[ "$line_count" = "${#archives[@]}" ] ||
	die "SHA256SUMS has $line_count lines, expected ${#archives[@]} (a .sha256 file is missing its trailing newline?)"

log "wrote $sums_path ($line_count entries)"
log "release assets for v$version are complete and self-consistent"
