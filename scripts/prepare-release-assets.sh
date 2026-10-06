#!/usr/bin/env bash
#
# Checks that a directory holds exactly the assets a complete four-target
# release consists of - per target the main archive and one plugin asset per
# bundled language, each with its `.sha256` - then writes the combined `SHA256SUMS` that gets
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
#   - an archive count other than targets x (1 + bundled plugins), with both
#     factors taken from build-targets.sh (`--list`, `--plugins`)
#   - a plugin asset that is not exactly `<language>/plugin.toml` plus the one
#     binary that manifest's `command` names, whose manifest does not say
#     `language = "<language>"`, or whose files are not byte-identical to
#     `plugins/<language>/` inside the main archive of the same target
#
# It deliberately does NOT check that the binaries inside work: that is what
# the smoke tests in build-targets.sh and scripts/release-smoke.sh are for.
# Nothing here is executed, so it runs on any host.
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

targets=()
while IFS= read -r line; do
	if [ -n "$line" ]; then
		targets+=("$line")
	fi
done < <(bash "$REPO_ROOT/scripts/build-targets.sh" --list)
plugins=()
while IFS= read -r line; do
	if [ -n "$line" ]; then
		plugins+=("$line")
	fi
done < <(bash "$REPO_ROOT/scripts/build-targets.sh" --plugins)

# Asserted separately from the name list, so that dropping a language from
# BUNDLED_PLUGINS (and with it from the build and the names) still shows up as
# a different number here.
expected_archives=$((${#targets[@]} * (1 + ${#plugins[@]})))
log "${#targets[@]} target(s) x (1 + ${#plugins[@]} plugin(s)) = $expected_archives archives"
[ "${#archives[@]}" -eq "$expected_archives" ] ||
	die "found ${#archives[@]} archives, expected $expected_archives (${#targets[@]} targets x (1 main + ${#plugins[@]} plugins))"

work="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-prepare-release.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Unpacks archive $1 into the empty directory $2.
unpack_into() {
	case "$1" in
	*.zip)
		command -v unzip >/dev/null 2>&1 || die "unzip is required to inspect $(basename "$1")"
		# Status 1 is unzip's "warning" (e.g. backslash separators, which it
		# converts); the layout checks below catch anything it got wrong.
		local status=0
		unzip -q "$1" -d "$2" || status=$?
		[ "$status" -le 1 ] || die "could not unpack $(basename "$1") (unzip exited $status)"
		;;
	*)
		tar -xzf "$1" -C "$2" || die "could not unpack $(basename "$1")"
		;;
	esac
}

# Prints the regular files under directory $1, relative to it, sorted.
files_under() {
	(cd "$1" && find . -type f | sed 's#^\./##' | LC_ALL=C sort)
}

# Per target: --asset-names lists the main archive first, then one
# asset/checksum pair per plugin in --plugins order, so the plugin asset for
# plugins[i] is line 2 + 2i.
for target in "${targets[@]}"; do
	names=()
	while IFS= read -r line; do
		if [ -n "$line" ]; then
			names+=("$line")
		fi
	done < <(bash "$REPO_ROOT/scripts/build-targets.sh" --asset-names "$target")
	main_archive="${names[0]}"
	main_stem="$(basename "$(bash "$REPO_ROOT/scripts/build-targets.sh" --stage-dir "$target")")"

	main_dir="$work/main"
	rm -rf "$main_dir"
	mkdir -p "$main_dir"
	unpack_into "$ASSET_DIR/$main_archive" "$main_dir"
	[ -d "$main_dir/$main_stem/plugins" ] ||
		die "$main_archive has no $main_stem/plugins/ directory to compare the plugin assets with"

	i=0
	for lang in "${plugins[@]}"; do
		asset="${names[$((2 + 2 * i))]}"
		i=$((i + 1))

		# Every entry must sit under `<lang>/`; directory entries aside, exactly
		# two files: the manifest and one binary.
		asset_files=()
		while IFS= read -r entry; do
			case "$entry" in
			"$lang"/) ;;
			"$lang"/*/) die "$asset has a subdirectory '$entry'; a plugin asset is $lang/plugin.toml plus one binary" ;;
			"$lang"/*) asset_files+=("${entry#"$lang"/}") ;;
			*) die "$asset has an entry outside $lang/: '$entry'" ;;
			esac
		done < <(tar -tzf "$ASSET_DIR/$asset" | sed 's#^\./##')
		[ "${#asset_files[@]}" -eq 2 ] ||
			die "$asset holds ${#asset_files[@]} file(s) (${asset_files[*]:-none}), expected exactly $lang/plugin.toml and one binary"

		plugin_dir="$work/plugin"
		rm -rf "$plugin_dir"
		mkdir -p "$plugin_dir"
		unpack_into "$ASSET_DIR/$asset" "$plugin_dir"
		manifest="$plugin_dir/$lang/plugin.toml"
		[ -f "$manifest" ] || die "$asset carries no $lang/plugin.toml"
		grep -Eq "^language[[:space:]]*=[[:space:]]*\"$lang\"" "$manifest" ||
			die "$asset's plugin.toml does not declare language = \"$lang\""
		exe="$(awk -F'"' '/^command[[:space:]]*=/ { sub(/^\.\//, "", $2); print $2; exit }' "$manifest")"
		[ -n "$exe" ] && [ -f "$plugin_dir/$lang/$exe" ] ||
			die "$asset's plugin.toml command names '$exe', which is not the binary beside it"

		bundled_dir="$main_dir/$main_stem/plugins/$lang"
		[ -d "$bundled_dir" ] || die "$main_archive carries no plugins/$lang/ to compare $asset with"
		[ "$(files_under "$plugin_dir/$lang")" = "$(files_under "$bundled_dir")" ] ||
			die "$asset and plugins/$lang/ in $main_archive hold different files"
		while IFS= read -r rel; do
			a="$(sha256_of "$plugin_dir/$lang/$rel" | awk '{ print $1 }')"
			b="$(sha256_of "$bundled_dir/$rel" | awk '{ print $1 }')"
			[ "$a" = "$b" ] ||
				die "$asset's $lang/$rel differs from plugins/$lang/$rel in $main_archive ($a vs $b)"
		done < <(files_under "$bundled_dir")

		log "plugin asset $asset matches plugins/$lang/ in $main_archive"
	done
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
