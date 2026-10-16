#!/usr/bin/env bash
#
# Offline tests for scripts/ci-green-for-commit.sh (GM-613's release test
# gate) and for the smoke/architecture guards in scripts/lib/runnable.sh.
#
# No network: the gate makes every GitHub API call as
#   $GH api [--paginate] <endpoint> --jq <filter>
# so GH is pointed at a stub written here. The stub maps <endpoint> to a
# fixture JSON file in the case's own directory ($FX) and applies <filter>
# with `jq -r`, the way `gh --jq` prints. A missing fixture is an API error
# (gh exits non-zero on a 404), and a `$FX/fail` file makes every call fail
# the way a bad token does (HTTP 401).
#
# Each case checks stdout exactly (the `key=value` lines release.yml writes
# to $GITHUB_OUTPUT) and that the exit status is 0: the gate must fail toward
# running the tests, never by breaking the job.
#
# The Mach-O cases need `lipo` and a universal /usr/bin/true (macOS); they
# are skipped, with a note, where those are missing.
#
# Usage: bash scripts/test-ci-green-for-commit.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$SCRIPT_DIR/ci-green-for-commit.sh"
RUNNABLE="$SCRIPT_DIR/lib/runnable.sh"
SLUG="example/g-mesh"

command -v jq >/dev/null 2>&1 || {
	echo "test-ci-green-for-commit: jq is required" >&2
	exit 1
}

work="$(mktemp -d "${TMPDIR:-/tmp}/g-mesh-test-ci-green.XXXXXX")"
trap '[[ -n "${KEEP_WORK:-}" ]] || rm -rf "$work"' EXIT

failures=0
fail() {
	echo "FAIL: $*" >&2
	failures=$((failures + 1))
}
pass() {
	echo "PASS: $*"
}

# Commit and tree shas: any 40-hex strings will do.
HEAD_SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
PARENT=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
OTHER=cccccccccccccccccccccccccccccccccccccccc
TREE1=1111111111111111111111111111111111111111
TREE2=2222222222222222222222222222222222222222

# --- the gh stub -----------------------------------------------------------

GH_STUB="$work/gh-stub"
cat >"$GH_STUB" <<'EOF'
#!/usr/bin/env bash
# Fake `gh`: only `api [--paginate] <endpoint> --jq <filter>` is understood.
set -uo pipefail
[ "${1:-}" = api ] || { echo "gh-stub: not an api call: $*" >&2; exit 64; }
shift
[ "${1:-}" = --paginate ] && shift
ep="${1:-}"
[ "${2:-}" = --jq ] || { echo "gh-stub: expected --jq after the endpoint: $*" >&2; exit 64; }
filter="${3:-}"
if [ -e "$FX/fail" ]; then
	echo "HTTP 401: Bad credentials (https://api.github.com/$ep)" >&2
	exit 1
fi
case "$ep" in
*/git/commits/*) f="$FX/c_${ep##*/}.json" ;;
*/actions/workflows/ci.yml/runs\?*) s="${ep#*head_sha=}"; f="$FX/r_${s%%&*}.json" ;;
*/actions/runs/*/jobs*) r="${ep#*/actions/runs/}"; f="$FX/j_${r%%/*}.json" ;;
*) f="" ;;
esac
if [ -z "$f" ] || [ ! -f "$f" ]; then
	echo "HTTP 404: Not Found (https://api.github.com/$ep)" >&2
	exit 1
fi
jq -r "$filter" "$f"
EOF
chmod +x "$GH_STUB"

# --- fixture helpers -------------------------------------------------------

# new_case NAME: a fresh fixture directory; sets FX.
new_case() {
	FX="$work/$1"
	mkdir -p "$FX"
}

# commit SHA TREE [PARENT...]: the git/commits answer for SHA.
commit() {
	local sha="$1" tree="$2"
	shift 2
	local parents="[]" p
	for p in "$@"; do
		parents="$(jq -c --arg p "$p" '. + [{sha: $p}]' <<<"$parents")"
	done
	jq -n --arg sha "$sha" --arg tree "$tree" --argjson parents "$parents" \
		'{sha: $sha, tree: {sha: $tree}, parents: $parents}' >"$FX/c_$sha.json"
}

# runs SHA [ID:HEAD_SHA:CONCLUSION...]: the ci.yml runs answer for
# ?head_sha=SHA. HEAD_SHA is what the API claims each run was for.
runs() {
	local sha="$1" spec id hs concl arr="[]"
	shift
	for spec in "$@"; do
		IFS=: read -r id hs concl <<<"$spec"
		arr="$(jq -c --argjson id "$id" --arg hs "$hs" --arg c "$concl" \
			'. + [{id: $id, head_sha: $hs, conclusion: $c}]' <<<"$arr")"
	done
	jq -n --argjson r "$arr" '{total_count: ($r | length), workflow_runs: $r}' >"$FX/r_$sha.json"
}

# jobs RUN_ID [NAME=CONCLUSION...]: the jobs answer for one run.
jobs() {
	local run="$1" spec arr="[]" n=0
	shift
	for spec in "$@"; do
		n=$((n + 1))
		arr="$(jq -c --argjson id "$((run * 100 + n))" --arg name "${spec%=*}" --arg c "${spec##*=}" \
			'. + [{id: $id, name: $name, conclusion: $c}]' <<<"$arr")"
	done
	jq -n --argjson j "$arr" '{total_count: ($j | length), jobs: $j}' >"$FX/j_$run.json"
}

FULL_OK='Full test suite ran=success'
OTHER_OK='tests (x86_64-unknown-linux-gnu)=success'

# expect_gate NAME WANT_STDOUT [ARGS...]: run the gate against $FX and
# compare stdout exactly; the exit status must be 0.
expect_gate() {
	local name="$1" want="$2" out rc
	shift 2
	[ $# -gt 0 ] || set -- "$SLUG" "$HEAD_SHA"
	rc=0
	out="$(FX="$FX" GH="$GH_STUB" bash "$GATE" "$@" 2>"$FX.err")" || rc=$?
	if [ "$rc" -ne 0 ]; then
		fail "$name: exit status $rc, want 0"
		sed 's/^/    /' "$FX.err" >&2
	elif [ "$out" != "$want" ]; then
		fail "$name: stdout was
$out
want
$want"
		sed 's/^/    /' "$FX.err" >&2
	else
		pass "$name: $(tr '\n' ' ' <<<"$out")"
	fi
}

GREEN_FALSE="green=false"

# --- ci-green-for-commit.sh ------------------------------------------------

# A green full run on the exact SHA: skip. The parent has another tree, so
# only the commit's own run can make it green.
new_case exact-green
commit "$HEAD_SHA" "$TREE1" "$PARENT"
commit "$PARENT" "$TREE2"
runs "$HEAD_SHA" "101:$HEAD_SHA:success"
jobs 101 "$OTHER_OK" "$FULL_OK"
expect_gate exact-green "green=true
run=101
sha=$HEAD_SHA"

# A failed run first, then a green full one: the green one is found.
new_case exact-green-after-failure
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "102:$HEAD_SHA:failure" "103:$HEAD_SHA:success"
jobs 102 "Full test suite ran=failure"
jobs 103 "$FULL_OK"
expect_gate exact-green-after-failure "green=true
run=103
sha=$HEAD_SHA"

# No run of its own (a tag commit), parent with the same tree and a green
# full run: skip, relying on the parent's run.
new_case same-tree-parent
commit "$HEAD_SHA" "$TREE1" "$PARENT"
commit "$PARENT" "$TREE1"
runs "$HEAD_SHA"
runs "$PARENT" "201:$PARENT:success"
jobs 201 "$OTHER_OK" "$FULL_OK"
expect_gate same-tree-parent "green=true
run=201
sha=$PARENT"

# A green run whose full-suite job was skipped (test sections chosen by
# changed paths): partial, so the tests run.
new_case partial-run
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "301:$HEAD_SHA:success"
jobs 301 "$OTHER_OK" "Full test suite ran=skipped"
expect_gate partial-run "$GREEN_FALSE"

# A green run with no full-suite job at all (a single-platform dispatch).
new_case partial-run-no-job
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "302:$HEAD_SHA:success"
jobs 302 "$OTHER_OK"
expect_gate partial-run-no-job "$GREEN_FALSE"

# No run of its own; the parent has a green full run but a different tree
# (the commit changed code): its run is never borrowed, the tests run.
new_case different-tree-parent
commit "$HEAD_SHA" "$TREE1" "$PARENT"
commit "$PARENT" "$TREE2"
runs "$HEAD_SHA"
runs "$PARENT" "401:$PARENT:success"
jobs 401 "$FULL_OK"
expect_gate different-tree-parent "$GREEN_FALSE"

# Every call fails as with a bad or missing token.
new_case bad-token
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "501:$HEAD_SHA:success"
jobs 501 "$FULL_OK"
touch "$FX/fail"
expect_gate bad-token "$GREEN_FALSE"

# The commit is readable but the runs query fails.
new_case api-error-runs
commit "$HEAD_SHA" "$TREE1"
expect_gate api-error-runs "$GREEN_FALSE"

# A green run is listed but its jobs cannot be read.
new_case api-error-jobs
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "502:$HEAD_SHA:success"
expect_gate api-error-jobs "$GREEN_FALSE"

# The commit's tree cannot be read (unknown SHA: 404).
new_case api-error-commit
expect_gate api-error-commit "$GREEN_FALSE"

# An answer that is not what the API documents: a tree that is not a sha.
new_case bad-tree-value
commit "$HEAD_SHA" "not-a-sha"
runs "$HEAD_SHA" "503:$HEAD_SHA:success"
jobs 503 "$FULL_OK"
expect_gate bad-tree-value "$GREEN_FALSE"

# The API ignores ?head_sha= and answers with a green full run of another
# commit: the gate's own head_sha check must reject it.
new_case head-sha-mismatch
commit "$HEAD_SHA" "$TREE1"
runs "$HEAD_SHA" "601:$OTHER:success"
jobs 601 "$OTHER_OK" "$FULL_OK"
expect_gate head-sha-mismatch "$GREEN_FALSE"

# Inputs that are not an owner/repo or a full sha: not green, no API call.
new_case bad-args
touch "$FX/fail"
expect_gate bad-args-sha "$GREEN_FALSE" "$SLUG" "${HEAD_SHA:0:7}"
expect_gate bad-args-slug "$GREEN_FALSE" "not a slug" "$HEAD_SHA"

# Usage error: exit 2, nothing on stdout.
rc=0
out="$(GH="$GH_STUB" bash "$GATE" "$SLUG" 2>/dev/null)" || rc=$?
if [ "$rc" -eq 2 ] && [ -z "$out" ]; then
	pass "usage: exit 2"
else
	fail "usage: exit $rc, stdout '$out' (want exit 2, empty stdout)"
fi

# --- runnable.sh -----------------------------------------------------------

# in_runnable NAME WANT_RC CMD...: run CMD in a bash that sourced runnable.sh,
# with $G_MESH_REQUIRE_SMOKE as the caller set it, and PATH replaced by
# $RUNNABLE_PATH when that is set.
in_runnable() {
	local name="$1" want="$2" rc=0
	shift 2
	bash -c 'source "$1"; shift; PATH="${RUNNABLE_PATH:-$PATH}"; "$@"' bash "$RUNNABLE" "$@" >"$work/$name.out" 2>&1 || rc=$?
	if [ "$rc" -eq "$want" ]; then
		pass "$name: exit $rc"
	else
		fail "$name: exit $rc, want $want"
		sed 's/^/    /' "$work/$name.out" >&2
	fi
}

# A smoke check that cannot run is a failure under G_MESH_REQUIRE_SMOKE=1 ...
G_MESH_REQUIRE_SMOKE=1 in_runnable smoke-skip-required 1 \
	smoke_skip_or_die "the test" x86_64-apple-darwin aarch64-unknown-linux-gnu
# ... and a logged skip without it.
G_MESH_REQUIRE_SMOKE='' in_runnable smoke-skip-allowed 0 \
	smoke_skip_or_die "the test" x86_64-apple-darwin aarch64-unknown-linux-gnu
if grep -q 'smoke test skipped' "$work/smoke-skip-allowed.out"; then
	pass "smoke-skip-allowed: skip logged"
else
	fail "smoke-skip-allowed: no 'smoke test skipped' line"
fi

# Not a macOS target: no architecture to check.
G_MESH_REQUIRE_SMOKE=1 in_runnable macho-non-mac-noop 0 \
	assert_macho_arch /nonexistent x86_64-unknown-linux-gnu

# No lipo on PATH under G_MESH_REQUIRE_SMOKE=1: the check cannot run, fail.
nolipo="$work/nolipo"
mkdir -p "$nolipo"
for tool in uname basename; do
	ln -s "$(command -v "$tool")" "$nolipo/$tool"
done
RUNNABLE_PATH="$nolipo" G_MESH_REQUIRE_SMOKE=1 in_runnable macho-no-lipo-required 1 \
	assert_macho_arch /nonexistent x86_64-apple-darwin

archs=""
if command -v lipo >/dev/null 2>&1; then
	archs="$(lipo -archs /usr/bin/true 2>/dev/null || true)"
fi
case " $archs " in
*" x86_64 "*" "*)
	thin="$work/true-x86_64"
	lipo /usr/bin/true -thin x86_64 -output "$thin"
	G_MESH_REQUIRE_SMOKE=1 in_runnable macho-thin-ok 0 assert_macho_arch "$thin" x86_64-apple-darwin
	G_MESH_REQUIRE_SMOKE=1 in_runnable macho-wrong-arch 1 assert_macho_arch "$thin" aarch64-apple-darwin
	# A fat binary is not "exactly" its target's architecture.
	G_MESH_REQUIRE_SMOKE=1 in_runnable macho-fat 1 assert_macho_arch /usr/bin/true x86_64-apple-darwin
	;;
*)
	echo "NOTE: lipo or a universal /usr/bin/true with an x86_64 slice is missing; Mach-O cases skipped"
	;;
esac

if [[ "$failures" -ne 0 ]]; then
	echo "$failures failure(s)" >&2
	exit 1
fi
echo "all ci-green-for-commit.sh and runnable.sh tests passed"
