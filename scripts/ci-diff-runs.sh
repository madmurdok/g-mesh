#!/usr/bin/env bash
# What changed between two CI runs (GM-345).
#
# Through the 3.6.0 and 3.7.0 batches the useful question after a run was
# never "how many are red" - it was "what changed". Answering it meant
# downloading two logs and diffing them by hand, and that hand-diff is what
# produced the two most load-bearing findings of those batches: a fix
# confirmed by one test going FAIL -> PASS, and a memory-limit test spotted as
# *new* rather than pre-existing, which is the only reason it was filed as a
# flake instead of a regression.
#
# This reads the `junit-<target>` artifacts that `ci.yml` keeps on every run
# (GM-344), so it never parses human-readable test output - the regex over a
# log is exactly what these two tasks exist to retire.
#
#   scripts/ci-diff-runs.sh <older-run-id> <newer-run-id> [history-run-id...]
#
# With extra run ids, a newly-failing test that has BOTH passed and failed
# across them is labelled a flake candidate rather than a regression. Two runs
# alone cannot make that call and this does not pretend to: a test that passed
# in the older run and fails in the newer one is reported as "was passing",
# which is a regression until history says otherwise.
#
# The same care applies in the other direction, but "seen passing somewhere
# in history" is NOT on its own evidence against a fix - the normal workflow
# is fix it, then re-run CI to confirm, then diff using those runs as
# history, and a corroborating rerun of the FIX must not count against it.
# What does count is which COMMIT a history run actually built, from `gh`'s
# own record of it, not just whether the test passed there:
#   - a history run on the OLDER run's commit that passes: the same code
#     both failed (the older run) and passed (here) - a flake, not a fix.
#     This is the card's own case, one commit rerun five times, one red.
#   - a history run on the NEWER run's commit that fails: the fix commit
#     does not reliably pass either - also not a confirmed fix.
#   - a history run on neither commit says nothing about this pair and is
#     not used for this check (though it still counts on the newly-failing
#     side above, which asks a different question: is this ident flaky at
#     all, not "is this specific transition real").
# Reported as INTERMITTENT with which of the two applied. With no history,
# or when the older/newer run's own commit can't be determined (expired or
# deleted run), fail->pass is genuinely indistinguishable from fixed, and
# the output says so rather than defaulting to FIXED as if it had checked -
# same when the older and newer run turn out to be the identical commit,
# which makes "fixed" impossible outright: nothing changed between them.
#
# Only runs that carry JUnit artifacts can be read, which means runs from
# GM-344 onwards. Earlier runs have logs and nothing else.
set -euo pipefail

die() {
	echo "ci-diff-runs: $*" >&2
	exit 1
}

[ $# -ge 2 ] || die "usage: $0 <older-run-id> <newer-run-id> [history-run-id...]"

command -v gh >/dev/null 2>&1 || die "the gh CLI is required"
py="$(command -v python3 || command -v python)" || die "python is required"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM HUP

fetch() {
	local run="$1" dest="$work/$1"
	mkdir -p "$dest"
	# A run whose artifacts have expired, or that predates GM-344, downloads
	# nothing - which must not look like "it had no failures".
	if ! gh run download "$run" --pattern 'junit-*' --dir "$dest" >/dev/null 2>&1; then
		die "run $run has no junit-* artifacts - it predates GM-344, or they have expired"
	fi
	find "$dest" -name '*.xml' | grep -q . || die "run $run produced junit-* artifacts with no XML in them"
	# The commit this run actually built, so the fixed-direction check below
	# can tell a same-commit rerun apart from an unrelated run instead of
	# guessing from pass/fail alone. A run whose metadata is gone (expired or
	# deleted) writes no file here - read_sha() below treats that as unknown,
	# never as a match or a mismatch, which is the safe direction.
	gh run view "$run" --json headSha --jq .headSha >"$dest/.sha" 2>/dev/null || true
	echo "$dest"
}

old="$(fetch "$1")"
new="$(fetch "$2")"
shift 2
history=()
for run in "$@"; do
	history+=("$(fetch "$run")")
done

"$py" - "$old" "$new" "${history[@]+"${history[@]}"}" <<'PY'
import sys, os, xml.etree.ElementTree as ET

def read(root_dir):
    """{target: {"failed": set, "passed": set}} for one run's artifacts.

    The artifact directory is named `junit-<target>`, which is where the
    platform comes from - the JUnit file itself does not carry it.
    """
    runs = {}
    for dirpath, _dirnames, filenames in os.walk(root_dir):
        base = os.path.basename(dirpath)
        target = base[len("junit-"):] if base.startswith("junit-") else base
        for name in filenames:
            if not name.endswith(".xml"):
                continue
            state = runs.setdefault(target, {"failed": set(), "passed": set()})
            for suite in ET.parse(os.path.join(dirpath, name)).getroot().iter("testsuite"):
                binary = suite.get("name", "?")
                for case in suite.iter("testcase"):
                    ident = f"{binary} {case.get('name', '?')}"
                    bad = case.find("failure") is not None or case.find("error") is not None
                    state["failed" if bad else "passed"].add(ident)
    return runs

def read_sha(root_dir):
    """The commit `fetch` recorded for this run, or None if gh could not
    report one (expired or deleted run metadata). None is always treated as
    unknown - never as a match, never as a mismatch - which is the safe
    direction when a run can no longer say what it built.
    """
    try:
        with open(os.path.join(root_dir, ".sha")) as f:
            sha = f.read().strip()
    except OSError:
        return None
    return sha or None

old_dir, new_dir = sys.argv[1], sys.argv[2]
history_dirs = sys.argv[3:]

old_runs, new_runs = read(old_dir), read(new_dir)
old_sha, new_sha = read_sha(old_dir), read_sha(new_dir)
history = [read(d) for d in history_dirs]
history_shas = [read_sha(d) for d in history_dirs]

def history_counts(target, ident):
    """(passed, failed) counts for this ident across ALL history runs that
    tested it, with no regard to which commit each one built. This is the
    newly-failing side's own question - is this ident flaky at all - and it
    is unchanged: more samples of any commit are legitimate evidence there.
    """
    p = f = 0
    for run in history:
        state = run.get(target)
        if not state:
            continue
        p += ident in state["passed"]
        f += ident in state["failed"]
    return p, f


def flaky(target, ident):
    """Has this test been seen BOTH passing and failing in the history runs?"""
    p, f = history_counts(target, ident)
    return bool(p and f)

# The fixed-direction check asks a different question - not "is this ident
# flaky in general" but "is THIS transition, from the older run's commit to
# the newer run's, a real fix" - which only history runs on one of those two
# specific commits can answer; a run on some other commit is a legitimate
# flakiness sample above, but says nothing about this pair.
shas_known = old_sha is not None and new_sha is not None
same_commit = shas_known and old_sha == new_sha
if shas_known:
    hist_old = [i for i, s in enumerate(history_shas) if s == old_sha]
    hist_new = [i for i, s in enumerate(history_shas) if s == new_sha and s != old_sha]
    hist_other = [i for i in range(len(history)) if i not in hist_old and i not in hist_new]
else:
    hist_old, hist_new = [], []
    hist_other = list(range(len(history)))

if history:
    if shas_known:
        print(f"history: {len(hist_old)} run(s) share the older run's commit, "
              f"{len(hist_new)} share the newer run's commit, "
              f"{len(hist_other)} are on a different or unrecorded commit")
    else:
        print("history: the older or newer run's own commit could not be "
              "determined (expired or deleted run) - history cannot be told "
              "apart from an unrelated run, so it is not used for the "
              "fail->pass check below")
    print()

def commit_counts(indices, target, ident):
    """(passed, failed) counts for this ident, restricted to the given
    subset of history runs (those sharing one specific commit)."""
    p = f = 0
    for i in indices:
        state = history[i].get(target)
        if not state:
            continue
        p += ident in state["passed"]
        f += ident in state["failed"]
    return p, f

exit_code = 0
for target in sorted(set(old_runs) | set(new_runs)):
    # A platform one run never tested is NOT a platform with no failures.
    # A Windows-only dispatch carries one artifact; diffed against a full
    # run it would otherwise print three serene "0 failing -> 0" lines about
    # platforms it never touched - the same "absent renders like passed"
    # trap the partial-run notice in ci.yml exists to close, reproduced
    # inside the tool built to close it. Caught by running this against a
    # real dispatch rather than by reading it.
    if target not in old_runs or target not in new_runs:
        missing = "the older" if target not in old_runs else "the newer"
        present = new_runs.get(target) or old_runs.get(target)
        print(f"=== {target}: NOT COMPARABLE ===")
        print(f"  {missing} run has no artifact for this platform - it was not tested there,")
        print(f"  which is not the same as having had no failures. The other run has "
              f"{len(present['failed'])} failing of "
              f"{len(present['failed']) + len(present['passed'])}.")
        print()
        continue

    before = old_runs[target]
    after = new_runs[target]
    fixed = sorted(before["failed"] - after["failed"])
    broke = sorted(after["failed"] - before["failed"])
    still = sorted(after["failed"] & before["failed"])

    print(f"=== {target}: {len(before['failed'])} failing -> {len(after['failed'])} ===")
    if not (fixed or broke or still):
        print("  no failures on either side")
        print()
        continue

    if fixed:
        # fail -> pass gets the same care as pass -> fail: a test seen in
        # BOTH states, on the SAME commit, is a flake candidate whichever
        # direction it moved. Unlike the newly-failing side, which side of
        # history counts is not "any run that mentions this ident" - a run
        # confirming the fix (same commit as `new`, still passing) must not
        # read as evidence against it, so only two things move an ident out
        # of FIXED: the older commit rerun and passing, or the newer commit
        # rerun and failing.
        confirmed = []
        intermittent = []
        for ident in fixed:
            if same_commit:
                intermittent.append((ident, [
                    f"the older and newer run are the same commit "
                    f"({old_sha[:12]}) - nothing changed between them"]))
                continue
            if not shas_known:
                confirmed.append(ident)
                continue
            op, of_ = commit_counts(hist_old, target, ident)
            np_, nf = commit_counts(hist_new, target, ident)
            reasons = []
            if op:
                reasons.append(f"the older commit also passed in {op} of {op + of_} rerun(s)")
            if nf:
                reasons.append(f"the newer commit failed in {nf} of {nf + np_} rerun(s)")
            if reasons:
                intermittent.append((ident, reasons))
            else:
                confirmed.append(ident)

        if confirmed:
            print(f"  FIXED ({len(confirmed)})")
            for ident in confirmed:
                print(f"    {ident}")
            if not history:
                why = "no history runs were supplied"
            elif not shas_known:
                why = ("the older or newer run's commit could not be "
                       "determined, so history could not be classified against them")
            elif not hist_old and not hist_new:
                why = "none of the history runs share a commit with the older or newer run"
            else:
                why = None
            if why:
                print(f"  ({why} - fail->pass here is not confirmed as a real fix)")
            elif hist_new:
                print(f"  (confirmed by {len(hist_new)} rerun(s) of the newer "
                      f"commit, none of which failed)")
        if intermittent:
            print(f"  INTERMITTENT, not fixed ({len(intermittent)})")
            for ident, reasons in intermittent:
                print(f"    {ident}  [{'; '.join(reasons)}]")
    if broke:
        exit_code = 1
        print(f"  NEWLY FAILING ({len(broke)})")
        for ident in broke:
            if flaky(target, ident):
                note = "  [flake candidate: has both passed and failed in the history runs]"
            elif ident in before["passed"]:
                note = "  [was passing in the older run]"
            else:
                note = "  [not present in the older run at all - new or renamed test]"
            print(f"    {ident}{note}")
    if still:
        print(f"  STILL FAILING ({len(still)})")
        for ident in still:
            print(f"    {ident}")
    print()

sys.exit(exit_code)
PY
