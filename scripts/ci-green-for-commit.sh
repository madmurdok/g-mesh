#!/usr/bin/env bash
#
# GM-613: has ci.yml already passed a *full* run on this exact code?
#
#   scripts/ci-green-for-commit.sh <owner/repo> <commit-sha>
#
# release.yml's `ci-gate` job asks this before deciding whether to call ci.yml
# again. A tag is cut by scripts/cut-release.sh only after GM-391 proved the
# release-side parent passed a full ci.yml run, and the tag commit's tree is
# that parent's tree byte for byte, so re-running ~41-60 minutes of tests on
# it repeats a result already in hand.
#
# Prints, on stdout, `key=value` lines for $GITHUB_OUTPUT:
#
#   green=true      a green full run was found
#   run=<run id>    (only when green) the ci.yml run relied on
#   sha=<sha>       (only when green) the commit that run was for
#
# or the single line `green=false`. Reasons go to stderr. Exit status is 0
# either way; 2 only for a usage error.
#
# What counts (the same query as cut-release.sh's
# check_release_branch_ci_passed, GM-391):
#   * a ci.yml run whose head_sha is the candidate and whose conclusion is
#     `success`, AND
#   * whose jobs include "$CI_FULL_SUITE_JOB" with conclusion `success` - a
#     partial run (test sections selected by changed paths, a single-platform
#     dispatch) does not count.
# Candidates: the commit itself, then each parent whose *tree* equals the
# commit's tree (owner decision D2: same code, not same SHA). A parent with a
# different tree is never borrowed from.
#
# Fails toward running the tests, never toward skipping them: any API error,
# unparseable answer or unexpected value is `green=false`.
#
# Test seam: every API call goes through "$GH" (default `gh`), always as
#   $GH api [--paginate] <endpoint> --jq <filter>
# so a test can point GH at a stub that maps <endpoint> to a fixture JSON file
# and applies <filter> with `jq -r`, the way `gh --jq` prints.

set -uo pipefail

GH="${GH:-gh}"

# The name of ci.yml's `full-suite` job. Must stay equal to ci.yml's job name
# and to CI_FULL_SUITE_JOB in scripts/cut-release.sh. If they drift, this
# never finds a green run and the tests always run (the safe direction).
CI_FULL_SUITE_JOB="Full test suite ran"

say() {
	echo "ci-green-for-commit: $*" >&2
}

not_green() {
	say "$* - the tests will run"
	echo "green=false"
	exit 0
}

is_sha() {
	[[ "$1" =~ ^[0-9a-f]{40}$ ]]
}

# The tree sha of commit $1, or failure.
tree_of() {
	local tree
	tree="$("$GH" api "repos/$slug/git/commits/$1" --jq '.tree.sha')" || return 1
	is_sha "$tree" || return 1
	printf '%s\n' "$tree"
}

# The id of a successful ci.yml run on commit $1 whose full-suite job
# succeeded. Returns 0 and prints the id when found, 1 when there is none,
# 2 on any API error or unexpected answer.
green_full_run_for() {
	local sha="$1" runs run_id jobs
	# `.head_sha == $sha` again in the filter: should the API ever ignore the
	# head_sha parameter, every green run of the repository would match.
	runs="$("$GH" api --paginate "repos/$slug/actions/workflows/ci.yml/runs?head_sha=$sha&per_page=100" \
		--jq ".workflow_runs[] | select(.head_sha == \"$sha\" and .conclusion == \"success\") | .id")" || return 2
	for run_id in $runs; do
		[[ "$run_id" =~ ^[0-9]+$ ]] || return 2
		jobs="$("$GH" api --paginate "repos/$slug/actions/runs/$run_id/jobs?per_page=100" \
			--jq ".jobs[] | select(.name == \"$CI_FULL_SUITE_JOB\" and .conclusion == \"success\") | .id")" || return 2
		if [ -n "$jobs" ]; then
			printf '%s\n' "$run_id"
			return 0
		fi
		say "ci.yml run $run_id on ${sha:0:7} is green but partial (no successful '$CI_FULL_SUITE_JOB' job)"
	done
	return 1
}

main() {
	if [ $# -ne 2 ]; then
		echo "usage: $0 <owner/repo> <commit-sha>" >&2
		exit 2
	fi
	slug="$1"
	local sha="$2"
	[[ "$slug" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || not_green "'$slug' is not an owner/repo"
	is_sha "$sha" || not_green "'$sha' is not a full commit sha"

	local tree parents parent ptree
	tree="$(tree_of "$sha")" || not_green "could not read the tree of ${sha:0:7}"
	parents="$("$GH" api "repos/$slug/git/commits/$sha" --jq '.parents[].sha')" ||
		not_green "could not read the parents of ${sha:0:7}"

	local candidates=("$sha")
	for parent in $parents; do
		is_sha "$parent" || not_green "unexpected parent '$parent' of ${sha:0:7}"
		ptree="$(tree_of "$parent")" || not_green "could not read the tree of parent ${parent:0:7}"
		if [ "$ptree" = "$tree" ]; then
			candidates+=("$parent")
		else
			say "parent ${parent:0:7} has a different tree - its runs do not count"
		fi
	done

	local candidate run_id rc
	for candidate in "${candidates[@]}"; do
		run_id="$(green_full_run_for "$candidate")"
		rc=$?
		case "$rc" in
		0)
			say "ci.yml run $run_id passed a full run on ${candidate:0:7} (tree ${tree:0:7}) - the tests need not run again"
			echo "green=true"
			echo "run=$run_id"
			echo "sha=$candidate"
			exit 0
			;;
		1) say "no green full ci.yml run on ${candidate:0:7}" ;;
		*) not_green "could not query ci.yml runs for ${candidate:0:7}" ;;
		esac
	done
	not_green "no green full ci.yml run on ${sha:0:7} or a parent with the same tree"
}

main "$@"
