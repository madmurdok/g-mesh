#!/usr/bin/env python3
"""Compares a full run's test times against the `local` profile's heavy list.

    test-heavy-report.py <nextest.toml> <junit.xml>...

The heavy list is the `(binary_id(B) & test(=N))` terms of `[profile.local]`'s
`default-filter` in <nextest.toml>. The JUnit files are nextest's (`--profile
ci`), one `<testcase classname="<binary id>" name="<test>" time="<s>">` per
test. Prints four groups and always exits 0 unless its input is unreadable:

- per crate (the package part of the binary id, so every crate of the run,
  plugin crates included): how many tests were timed, and how many of those
  are listed and still at or over the threshold. A listed test that stays
  heavy is in no other group, so this is where its crate shows it was timed;
- listed but under the threshold: a candidate to drop from the list;
- not listed but at or over it: a candidate to add;
- listed but absent from the JUnit files: renamed or deleted, the term matches
  nothing.

Terms and test cases match per binary id and test name, exactly, so a test of
the same name in another binary or crate is a different test.

Only tests in the JUnit files are timed. scripts/test-local.sh --full runs the
lib tests that are not on the list outside nextest (one libtest process per
crate), with no per-test time: a lib test that reached the threshold there is
in no group, whatever its crate.

Why the list is static and checked here: docs/adr/0031-local-test-runs.md.
"""

import re
import sys
import xml.etree.ElementTree as ET

THRESHOLD_SECS = 10.0

# Run locally with fewer seeds (`G_MESH_CONTAINERS_SEEDS`) instead of being
# skipped, so their full-run time says nothing about the list.
REDUCED_LOCALLY = {
    ("g-mesh", "graph::containers::tests::membership_invariants_hold_after_every_diff_of_a_random_sequence"),
    ("g-mesh", "graph::containers::tests::bulk_batch_boundaries_do_not_change_the_result"),
}

TERM = re.compile(r"binary_id\(([^)]+)\)\s*&\s*test\(=([^)]+)\)")


def heavy_terms(config_path):
    with open(config_path, encoding="utf-8") as f:
        text = f.read()
    start = text.find("[profile.local]")
    if start < 0:
        sys.exit(f"test-heavy-report: no [profile.local] in {config_path}")
    # The section ends at the next table header at the start of a line.
    end = re.search(r"^\[", text[start + 1 :], re.MULTILINE)
    section = text[start : start + 1 + end.start()] if end else text[start:]
    terms = {(b.strip(), n.strip()) for b, n in TERM.findall(section)}
    if not terms:
        sys.exit(f"test-heavy-report: [profile.local] in {config_path} has no heavy terms")
    return terms


def junit_times(paths):
    times = {}
    for path in paths:
        for case in ET.parse(path).getroot().iter("testcase"):
            key = (case.get("classname", ""), case.get("name", ""))
            times[key] = float(case.get("time") or 0)
    return times


def crate_of(binary_id):
    """The package of a nextest binary id: `<package>[::<binary>]`."""
    return binary_id.split("::", 1)[0]


def per_crate(times, heavy):
    """`{crate: (tests timed, listed tests still at or over the threshold)}`."""
    rows = {}
    for (binary, name), t in times.items():
        timed, still = rows.get(crate_of(binary), (0, 0))
        rows[crate_of(binary)] = (timed + 1, still + ((binary, name) in heavy and t >= THRESHOLD_SECS))
    return rows


def main(argv):
    if len(argv) < 3:
        print(__doc__.strip().splitlines()[2].strip(), file=sys.stderr)
        return 2
    heavy = heavy_terms(argv[1])
    times = junit_times(argv[2:])
    lighter = sorted((t, k) for k, t in times.items() if k in heavy and t < THRESHOLD_SECS)
    heavier = sorted(
        ((t, k) for k, t in times.items()
         if k not in heavy and k not in REDUCED_LOCALLY and t >= THRESHOLD_SECS),
        reverse=True,
    )
    missing = sorted(k for k in heavy if k not in times)

    crates = per_crate(times, heavy)

    print(f"== heavy list: {len(heavy)} terms, {len(times)} tests timed, threshold {THRESHOLD_SECS:g}s")
    print(f"== timed per crate (tests timed, listed and still at or over {THRESHOLD_SECS:g}s): {len(crates)}")
    for crate, (timed, still) in sorted(crates.items()):
        print(f"  {timed:7d}  {still:7d}  {crate}")
    print(f"== listed, now under {THRESHOLD_SECS:g}s: {len(lighter)}")
    for t, (b, n) in lighter:
        print(f"  {t:7.1f}  {b}  {n}")
    print(f"== not listed, now at or over {THRESHOLD_SECS:g}s: {len(heavier)}")
    for t, (b, n) in heavier:
        print(f"  {t:7.1f}  {b}  {n}")
    print(f"== listed, not in this run (renamed or deleted): {len(missing)}")
    for b, n in missing:
        print(f"           {b}  {n}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
