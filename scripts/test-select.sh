#!/usr/bin/env bash
# Maps a set of changed paths to the test sections of scripts/test-sections.sh
# that the change can affect. The rule and why: docs/adr/0030-test-sections.md
# (and the rule table in docs/design/GM-540-GM-539-test-sections.md, section 6).
#
#   scripts/test-select.sh [--base <ref>]       paths changed since <ref>
#   scripts/test-select.sh --paths-from <file>  paths listed in <file>, one per line
#
# Prints exactly one line: `full`, `none`, or section names in run order,
# space-separated. The output is valid arguments for
# `scripts/test-sections.sh run`.
#
# With git, the changed paths are `git diff <base>...HEAD` plus uncommitted and
# untracked files. The default base is the merge-base with the nearest
# `release-*` branch, local or on origin.
#
# Invariants:
# - A path no rule names selects `full`: an unknown path is never skipped.
# - A change to anything every crate builds on (wire, the SDK, cargo and
#   nextest config, CI and this selection machinery) selects `full`.
# - A plugin change selects that plugin's section and every core section, never
#   another plugin's section.
set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
	cat >&2 <<'EOF'
usage: scripts/test-select.sh [--base <ref>]
       scripts/test-select.sh --paths-from <file>
EOF
}

die() {
	echo "test-select: $*" >&2
	exit 2
}

base=""
paths_from=""
while [ $# -gt 0 ]; do
	case "$1" in
	--base)
		[ $# -ge 2 ] || {
			usage
			exit 2
		}
		base="$2"
		shift 2
		;;
	--paths-from)
		[ $# -ge 2 ] || {
			usage
			exit 2
		}
		paths_from="$2"
		shift 2
		;;
	*)
		usage
		exit 2
		;;
	esac
done
[ -z "$base" ] || [ -z "$paths_from" ] || die "--base and --paths-from are exclusive"

# The `release-*` ref, local or on origin, whose merge-base with HEAD is the
# fewest commits behind HEAD.
nearest_release_base() {
	local ref mb n best="" best_n=""
	while IFS= read -r ref; do
		mb="$(git merge-base HEAD "$ref" 2>/dev/null)" || continue
		n="$(git rev-list --count "$mb..HEAD")"
		if [ -z "$best_n" ] || [ "$n" -lt "$best_n" ]; then
			best="$ref"
			best_n="$n"
		fi
	done < <(git for-each-ref --format='%(refname:short)' 'refs/heads/release-*' 'refs/remotes/origin/release-*')
	[ -n "$best" ] || return 1
	echo "$best"
}

changed_paths() {
	if [ -n "$paths_from" ]; then
		[ -r "$paths_from" ] || die "cannot read $paths_from"
		cat "$paths_from"
		return
	fi
	if [ -z "$base" ]; then
		base="$(nearest_release_base)" || die "no release-* branch shares history with HEAD; pass --base <ref>"
		echo "test-select: base $base" >&2
	fi
	git rev-parse --verify --quiet "$base^{commit}" >/dev/null || die "unknown base '$base'"
	# --no-renames: a move out of wire/ must report the wire/ path too.
	git diff --name-only --no-renames "$base...HEAD" || die "git diff $base...HEAD failed"
	git diff --name-only --no-renames HEAD || die "git diff HEAD failed"
	git ls-files --others --exclude-standard || die "git ls-files failed"
}

# Section names come from test-sections.sh, so the two scripts cannot disagree.
# Unquoted on purpose: one name per word.
# shellcheck disable=SC2207
SECTIONS=($(scripts/test-sections.sh list))
[ ${#SECTIONS[@]} -gt 0 ] || die "scripts/test-sections.sh list printed nothing"
CORE=()
for s in "${SECTIONS[@]}"; do
	case "$s" in core-*) CORE+=("$s") ;; esac
done

full=0
selected=" "
add() {
	local s
	for s in "$@"; do
		case "$selected" in *" $s "*) ;; *) selected="$selected$s " ;; esac
	done
}

# One path, first matching row wins. Sets `full` or adds sections.
classify() {
	case "$1" in
	wire/* | plugins/sdk/*) full=1 ;;
	Cargo.toml | Cargo.lock | .cargo/* | .config/nextest.toml | rust-toolchain*) full=1 ;;
	.github/workflows/* | scripts/test-sections.sh | scripts/test-sections-check.py | scripts/test-select.sh) full=1 ;;
	.gitattributes) full=1 ;;
	core/*) add "${CORE[@]}" ;;
	plugins/python/* | plugins/rust/* | plugins/typescript/*)
		local plugin="${1#plugins/}"
		add "plugin-${plugin%%/*}" "${CORE[@]}"
		;;
	plugins/go/*) add "${CORE[@]}" ;;
	README.md) add core-cli ;;
	scripts/*) add core-it core-cli ;;
	eval/*) add core-cli ;;
	docs/* | *.md | LICENSE* | .gitignore | .git-blame-ignore-revs | clippy.toml | rustfmt.toml) ;;
	*) full=1 ;;
	esac
}

# A command substitution, not a process substitution: a failing `die` in
# changed_paths must stop this script, never read as "no paths changed".
paths="$(changed_paths)"
while IFS= read -r path; do
	# Tolerates CRLF lists and blank lines.
	path="${path%$'\r'}"
	[ -n "$path" ] || continue
	classify "$path"
	[ "$full" -eq 0 ] || break
done <<<"$paths"

if [ "$full" -eq 1 ]; then
	echo full
elif [ "$selected" = " " ]; then
	echo none
else
	out=()
	for s in "${SECTIONS[@]}"; do
		case "$selected" in *" $s "*) out+=("$s") ;; esac
	done
	echo "${out[*]}"
fi
