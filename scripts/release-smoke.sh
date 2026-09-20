#!/usr/bin/env bash
#
# GM-333: runs a freshly staged release artifact for real, on the runner that
# just built it, instead of trusting that a green `cargo build` means the
# thing works.
#
# `build_one()` in scripts/build-targets.sh already runs `--version` and
# `plugins list` against the staged binary whenever it was built for the host
# (which, in the release matrix, is always - every leg builds natively on its
# own target's runner). That proves the binary starts and that it *sees* the
# plugins staged beside it. It does not spawn a single plugin process, index
# a single file, or touch the index this binary is meant to serve - the exact
# gap GM-206 found by doing this once, by hand, on one platform.
#
# This closes that gap by running `<staged binary> reindex` against
# scripts/release-smoke-fixture, an in-tree fixture with one small file per
# bundled plugin (go, python, rust, typescript - see that directory's own
# README for why one file per language, not one file total). A real reindex:
#   - spawns every bundled plugin's one-shot bulk-index process for real
#     (core/src/daemon/bulk_index.rs's `run`), and fails the whole walk if any
#     one of them fails to spawn or exits non-zero;
#   - has each plugin actually parse its own fixture file and emit nodes and
#     edges over the same NDJSON protocol a real project's index is built
#     from;
#   - writes those nodes and edges into a real SQLite index the same code
#     path a user's first index would use.
#
# `g-mesh reindex`'s own output has no per-language breakdown - its
# `BulkIndexSummary` (core/src/daemon/bulk_index.rs) is one project-wide
# aggregate - so this script's primary assertion (both counts non-zero) proves
# *something* extracted *something*, not that every one of the four languages
# individually did. Where `sqlite3` is available (confirmed present on the
# macOS and Ubuntu runner images this targets; not confirmed on the Windows
# one, so skipped there rather than assumed) this also queries the index
# directly and checks each language contributed at least one node of its own -
# see the per-language section below and scripts/release-smoke-fixture/
# README.md for the full argument.
#
# Deliberately NOT run: `g-mesh model fetch`. GM-206 measured that at ~614MiB
# per platform per release - four downloads of the same ~614MiB per release to
# re-answer a question that varies only in the one thing that is linked
# identically on every target (rustls with bundled roots; see
# .github/workflows/release.yml's header on `ort`/`ring`). Re-test it by hand
# if the TLS or certificate story changes, not on every release.
#
# Usage: scripts/release-smoke.sh <stage_dir> <target>
#   stage_dir  the unpacked staging directory build-targets.sh leaves on disk
#              after packaging, e.g. dist/g-mesh-v2.7.0-x86_64-apple-darwin
#              (get it with `bash scripts/build-targets.sh --stage-dir
#              <target>` so the path is computed the one way this repo
#              computes it, not retyped here)
#   target     the Rust target triple, only to decide the binary's name
#              (g-mesh vs g-mesh.exe)

set -euo pipefail

die() {
	echo "release-smoke: $*" >&2
	exit 1
}

log() {
	echo "==> release-smoke: $*"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$SCRIPT_DIR/release-smoke-fixture"

if [ $# -ne 2 ]; then
	die "usage: release-smoke.sh <stage_dir> <target>"
fi

[ -d "$1" ] || die "staged directory not found: $1 (did the packaging step run first?)"
# Resolved to an absolute path up front: this script later `cd`s into a
# scratch fixture copy before invoking the binary, and a relative stage_dir
# (a caller passing `dist/...` instead of an absolute path) would silently
# stop resolving the moment that `cd` happens - caught by hand while testing
# this against a real local build, where it failed exactly that way.
stage_dir="$(cd "$1" && pwd)"
target="$2"

bin_name="g-mesh"
case "$target" in
*-windows-*) bin_name="g-mesh.exe" ;;
esac

bin="$stage_dir/$bin_name"
[ -f "$bin" ] || die "staged binary not found: $bin (did the packaging step run first?)"
[ -d "$FIXTURE_DIR" ] || die "fixture directory not found: $FIXTURE_DIR"

# A scratch copy of the fixture, so this never writes into the tracked
# checkout, plus a scratch G_MESH_HOME (the documented override -
# core/src/paths.rs's HOME_ENV), so this run's project state lands somewhere
# this script can find it afterward without reimplementing g-mesh's own
# project-path hashing (core/src/storage/connection.rs's `project_dir`), and
# never touches a real developer's or runner's actual ~/.g-mesh.
work="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-release-smoke.XXXXXX")"
trap 'rm -rf "$work"' EXIT

project_dir="$work/project"
home_dir="$work/home"
mkdir -p "$project_dir" "$home_dir"
cp -R "$FIXTURE_DIR"/. "$project_dir"/
rm -f "$project_dir/README.md" # documentation only, not fixture input

log "reindexing scripts/release-smoke-fixture with the staged $target binary"
output="$(cd "$project_dir" && G_MESH_HOME="$home_dir" "$bin" reindex 2>&1)" && status=0 || status=$?
echo "$output"
[ "$status" -eq 0 ] || die "'$bin_name reindex' exited $status against the fixture - see output above"

# g-mesh reindex prints "  index:   N nodes, M edges (X imports linked, Y
# symbols linked)" - core/src/cli/reindex.rs's `render`, which has its own
# unit tests, so a change to this format is a deliberate diff there, not
# silent drift here.
counts_line="$(printf '%s\n' "$output" | grep -E '^[[:space:]]*index:' || true)"
[ -n "$counts_line" ] || die "reindex output has no 'index:' line - cannot verify node/edge counts (see output above)"

nodes="$(printf '%s\n' "$counts_line" | grep -oE '[0-9]+ nodes' | grep -oE '[0-9]+')"
edges="$(printf '%s\n' "$counts_line" | grep -oE '[0-9]+ edges' | grep -oE '[0-9]+')"
[ -n "$nodes" ] && [ -n "$edges" ] || die "could not parse node/edge counts from: $counts_line"

log "reindex reported $nodes node(s), $edges edge(s)"
[ "$nodes" -gt 0 ] || die "0 nodes indexed - the staged binary did not extract anything from the fixture"
[ "$edges" -gt 0 ] || die "0 edges indexed - the staged binary did not link anything in the fixture"

# Best-effort per-language check - see this script's header. Non-fatal to
# skip; fatal if it runs and finds a language with nothing to show for it.
db="$(find "$home_dir/projects" -maxdepth 2 -name 'index.db' -print -quit 2>/dev/null || true)"
if command -v sqlite3 >/dev/null 2>&1 && [ -n "$db" ]; then
	log "per-language check: querying $db"
	for ext in go rs py ts; do
		count="$(sqlite3 "$db" "SELECT COUNT(*) FROM nodes WHERE filePath LIKE '%.$ext';")"
		log "  .$ext: $count node(s)"
		[ "$count" -gt 0 ] || die ".$ext contributed 0 nodes - that plugin ran but extracted nothing from its own fixture file (aggregate nodes=$nodes came entirely from the other languages)"
	done
else
	log "per-language check skipped (sqlite3 not on PATH, or no index.db found) - relying on the aggregate nodes/edges check above only"
fi

log "PASS: $target's staged artifact spawned its plugins, indexed the fixture, and produced a non-empty graph ($nodes nodes, $edges edges)"
