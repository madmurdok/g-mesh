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
# The same care applies in the other direction. A test that stops failing is
# FIXED only when history does not contradict that - if the history runs show
# it both passing and failing, it is reported as INTERMITTENT instead, with
# how many of those runs went each way, because "3 of 5 green" and "19 of 20
# green" are different statements. With no history runs supplied, fail->pass
# is genuinely indistinguishable from fixed, and the output says so rather
# than defaulting to FIXED as if it had checked.
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

old_runs, new_runs = read(sys.argv[1]), read(sys.argv[2])
history = [read(d) for d in sys.argv[3:]]

def history_counts(target, ident):
    """(passed, failed) counts for this ident across history runs that tested it."""
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
        # BOTH states is a flake candidate whichever direction it moved. The
        # older run's own failure is already known - it is exactly why the
        # ident is in `fixed` - so it only takes history independently
        # showing this SAME ident passing too to establish "seen both
        # states"; that is the non-tautological half history has to supply,
        # the same role it plays on the newly-failing side.
        confirmed = []
        intermittent = []
        for ident in fixed:
            p, f = history_counts(target, ident)
            if history and p:
                intermittent.append((ident, p, f))
            else:
                confirmed.append(ident)

        if confirmed:
            print(f"  FIXED ({len(confirmed)})")
            for ident in confirmed:
                print(f"    {ident}")
            if not history:
                print("  (no history runs supplied - fail->pass and a flake that "
                      "happened to pass on the newer run look identical from two "
                      "runs alone, so this is not confirmed as a real fix)")
        if intermittent:
            print(f"  INTERMITTENT, not fixed ({len(intermittent)})")
            for ident, p, f in intermittent:
                total = 1 + p + f  # the older run's failure, plus history
                print(f"    {ident}  [failed in the older run; {p} of {total} known runs passed]")
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
