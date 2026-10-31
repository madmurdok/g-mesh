#!/usr/bin/env bash
# Creates a task worktree next to the main checkout, ready to build: installs
# the TypeScript and Python plugins' npm dependencies. With `--seed` it also
# seeds its target/debug with an APFS clone of the main checkout's, so the
# first `cargo build --workspace` reuses the third-party artifacts (workspace
# crates rebuild anyway; on a loaded machine the clone can cost more than it
# saves). Builds nothing.
#
#   scripts/new-task-worktree.sh <dir-suffix> <branch> [--base <ref>] [--seed]
#
# The worktree is <main checkout's parent>/<main checkout's name>-wt-<dir-suffix>
# on a new branch <branch> from <ref> (default: HEAD of the checkout this
# script is run from).
#
# Invariants:
# - Never clones a target another build is writing into: when any process has
#   `--out-dir <main>/target` on its command line, seeding is skipped (never
#   waited for, never killed). Objects of a compile that has not linked yet
#   would look unreferenced, and a half-written one would be cloned.
# - Never writes into the main checkout's files: the clone is copy-on-write,
#   and pruning runs on the clone only.
# - An existing directory is refused, never reused.
set -euo pipefail

usage() {
	echo "usage: scripts/new-task-worktree.sh <dir-suffix> <branch> [--base <ref>] [--seed]" >&2
}

die() {
	echo "new-task-worktree: $*" >&2
	exit 2
}

suffix=""
branch=""
base=HEAD
seed=0
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
	--seed)
		seed=1
		shift
		;;
	-*)
		usage
		exit 2
		;;
	*)
		if [ -z "$suffix" ]; then
			suffix="$1"
		elif [ -z "$branch" ]; then
			branch="$1"
		else
			usage
			exit 2
		fi
		shift
		;;
	esac
done
[ -n "$suffix" ] && [ -n "$branch" ] || {
	usage
	exit 2
}
case "$suffix" in */* | .*) die "bad <dir-suffix> '$suffix'" ;; esac

repo="$(cd "$(dirname "$0")/.." && pwd)"
# The first `worktree` entry is the main checkout.
main="$(git -C "$repo" worktree list --porcelain | sed -n '1s/^worktree //p')"
[ -n "$main" ] || die "cannot find the main checkout of $repo"
wt="$(dirname "$main")/$(basename "$main")-wt-$suffix"
[ ! -e "$wt" ] || die "$wt already exists"

echo "== git worktree add $wt -b $branch $base"
git -C "$repo" worktree add "$wt" -b "$branch" "$base"

for dir in plugins/typescript plugins/python; do
	if [ -f "$wt/$dir/package-lock.json" ]; then
		echo "== npm ci in $dir"
		(cd "$wt/$dir" && npm ci --no-audit --no-fund)
	fi
done

seed_target() {
	local src="$main/target/debug" start
	if [ ! -d "$src" ]; then
		echo "== not seeded: no $src"
		return
	fi
	if [ "$(uname -s)" != Darwin ]; then
		echo "== not seeded: copy-on-write clones (cp -c) need macOS"
		return
	fi
	if [ -n "$(pgrep -fl -- "--out-dir $main/target" || true)" ]; then
		echo "== not seeded: a build is writing into $main/target; run cargo build cold, or rerun later"
		return
	fi
	echo "== seeding $wt/target/debug from $src (APFS clone)"
	start="$(date +%s)"
	mkdir -p "$wt/target"
	if ! cp -c -R "$src" "$wt/target/debug"; then
		rm -rf "$wt/target"
		die "cloning $src failed; $wt is left without a target"
	fi
	echo "== cloned in $(($(date +%s) - start))s"
	if [ -x "$wt/scripts/prune-stale-objects.sh" ]; then
		CARGO_TARGET_DIR="$wt/target" "$wt/scripts/prune-stale-objects.sh"
	fi
}

if [ "$seed" -eq 1 ]; then
	seed_target
else
	echo "== not seeded (pass --seed to clone the main checkout's target/debug)"
fi

echo "== ready: $wt"
echo "next: cd $wt && cargo build --workspace"
