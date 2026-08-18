#!/usr/bin/env bash
#
# One command to validate, test and tag a release, instead of the four manual
# steps described in .github/workflows/release.yml's header (check the crate
# version, run the workflow once by hand, tag, approve the draft). The version
# is typed here exactly once.
#
#   scripts/cut-release.sh <version>              # verify, test, tag locally
#   scripts/cut-release.sh <version> --push       # ...and push the tag
#   scripts/cut-release.sh <version> --skip-tests # skip the `cargo test` run
#
# This script does NOT bump the version - that already happened as the
# release branch's first commit. What it does instead is VERIFY that
# `core/Cargo.toml` already says <version>, using the exact check
# .github/workflows/release.yml's "Check the tag matches the crate version"
# step runs (`build-targets.sh --version`), and refuse otherwise. That check
# is the whole point: it is the guard against the tag and the crate disagreeing
# - the failure #197 recorded, where the crate sat at 2.0.0 through five
# releases because nothing caught it.
#
# It tags `main`, and only after the release branch has already been merged
# into it - the tag is meant to point at the code someone gets by cloning, not
# at a branch tip nobody else can see. It does not create the release branch,
# does not merge anything, does not publish the draft Release GitHub builds
# (that stays a human decision, per #67's design), and does not touch the
# task tracker.
#
# Pushing the tag starts a public four-platform build and a draft Release, so
# it takes the explicit --push above; without it, the tag is created locally
# and the exact command to push it is printed instead.
#
# `cargo test` takes 10+ minutes on this machine. Skipping the announcement of
# that fact makes the wait look like a hang, so this script says what it is
# doing before it goes quiet. --skip-tests exists for re-running this script
# after a preflight check fails post-test (e.g. to fix --push without paying
# for the suite twice) - use it deliberately, not as a default habit.
#
# ---------------------------------------------------------------------------
# WHAT IT REFUSES ON
#
# Every one of these is a refusal, not a warning, and each says what to do
# about it:
#   - not on `main`
#   - a dirty working tree
#   - `main` out of sync with its upstream (a commit only local, or only
#     remote, cannot be the commit CI tags when the push happens)
#   - the tag already exists, locally or on the remote
#   - a version string that is not `X.Y.Z`
#   - `core/Cargo.toml`'s version disagreeing with the argument
# ---------------------------------------------------------------------------

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() {
	echo "cut-release: $*" >&2
	exit 1
}

log() {
	echo "==> $*"
}

usage() {
	cat <<'EOF'
usage: scripts/cut-release.sh <version> [--push] [--skip-tests]

  <version>      the release version, X.Y.Z, matching core/Cargo.toml
  --push         also push the tag (starts the build/publish workflow)
  --skip-tests   skip the `cargo test` run before tagging
EOF
}

# owner/repo parsed from the `origin` remote, for printing real URLs at the
# end rather than a placeholder someone has to mentally substitute.
origin_slug() {
	local url
	url="$(git -C "$REPO_ROOT" remote get-url origin 2>/dev/null)" || return 1
	# Handles both git@github.com:owner/repo.git and https://github.com/owner/repo.git
	url="${url%.git}"
	url="${url#git@github.com:}"
	url="${url#https://github.com/}"
	url="${url#http://github.com/}"
	printf '%s\n' "$url"
}

main() {
	local version="" push=0 skip_tests=0
	while [ $# -gt 0 ]; do
		case "$1" in
		--push)
			push=1
			;;
		--skip-tests)
			skip_tests=1
			;;
		-h | --help)
			usage
			return 0
			;;
		-*)
			die "unknown flag: $1 (try --help)"
			;;
		*)
			[ -z "$version" ] || die "unexpected extra argument: $1"
			version="$1"
			;;
		esac
		shift
	done

	[ -n "$version" ] || {
		usage >&2
		die "missing <version> argument"
	}

	[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
		die "malformed version: '$version' (expected X.Y.Z, three dot-separated numbers, e.g. 2.7.0)"

	local tag="v$version"

	local branch
	branch="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD)"
	[ "$branch" = "main" ] ||
		die "not on main (currently on '$branch') - run 'git checkout main' first"

	[ -z "$(git -C "$REPO_ROOT" status --porcelain)" ] ||
		die "working tree is dirty - commit, stash, or discard changes before cutting a release (see 'git status')"

	log "fetching origin to check main is in sync"
	git -C "$REPO_ROOT" fetch --quiet origin main ||
		die "could not fetch origin/main - check network/remote access and try again"

	local local_sha upstream_sha
	local_sha="$(git -C "$REPO_ROOT" rev-parse main)"
	upstream_sha="$(git -C "$REPO_ROOT" rev-parse origin/main)"
	if [ "$local_sha" != "$upstream_sha" ]; then
		local ahead behind
		ahead="$(git -C "$REPO_ROOT" rev-list --count origin/main..main)"
		behind="$(git -C "$REPO_ROOT" rev-list --count main..origin/main)"
		if [ "$ahead" -gt 0 ]; then
			die "main is $ahead commit(s) ahead of origin/main - push it first ('git push origin main'); tagging a commit CI can never see produces a tag nobody can build"
		else
			die "main is $behind commit(s) behind origin/main - pull first ('git pull origin main')"
		fi
	fi

	[ -z "$(git -C "$REPO_ROOT" tag -l "$tag")" ] ||
		die "tag $tag already exists locally - delete it first ('git tag -d $tag') if you mean to recut it"

	[ -z "$(git -C "$REPO_ROOT" ls-remote --tags origin "refs/tags/$tag")" ] ||
		die "tag $tag already exists on origin - a release for $version has already been cut"

	# The exact check .github/workflows/release.yml's "Check the tag matches
	# the crate version" step runs, so a mismatch is caught here instead of
	# costing four ~90-minute builds. --version needs no toolchain.
	local crate_version
	crate_version="$(bash "$REPO_ROOT/scripts/build-targets.sh" --version)"
	[ "$crate_version" = "$version" ] ||
		die "core/Cargo.toml says $crate_version, not $version - the release branch's first commit should have bumped it; fix core/Cargo.toml (or pass the version that's actually there) before tagging"

	if [ "$skip_tests" -eq 1 ]; then
		log "skipping cargo test (--skip-tests)"
	else
		log "running cargo test - this takes 10+ minutes on this machine, not hung, just slow"
		local test_start test_end
		test_start="$(date +%s)"
		(cd "$REPO_ROOT/core" && cargo test) ||
			die "cargo test failed - fix the failure before cutting a release"
		test_end="$(date +%s)"
		log "cargo test passed in $((test_end - test_start))s"
	fi

	log "tagging $tag on $(git -C "$REPO_ROOT" rev-parse --short main)"
	git -C "$REPO_ROOT" tag -a "$tag" -m "g-mesh $tag" ||
		die "failed to create tag $tag"

	if [ "$push" -eq 1 ]; then
		log "pushing $tag to origin"
		git -C "$REPO_ROOT" push origin "$tag" ||
			die "failed to push $tag - the tag still exists locally; retry with 'git push origin $tag'"
	else
		log "tag $tag created locally. Push it when ready:"
		echo
		echo "  git push origin $tag"
		echo
	fi

	local slug
	slug="$(origin_slug)" || slug="<owner>/<repo>"

	echo
	log "remaining steps"
	if [ "$push" -eq 1 ]; then
		echo "  1. Watch the build: https://github.com/$slug/actions"
		echo "  2. Review and publish the draft release: https://github.com/$slug/releases"
	else
		echo "  1. Push the tag:    git push origin $tag"
		echo "  2. Watch the build: https://github.com/$slug/actions"
		echo "  3. Review and publish the draft release: https://github.com/$slug/releases"
	fi
}

main "$@"
