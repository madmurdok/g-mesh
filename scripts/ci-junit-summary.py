#!/usr/bin/env python3
"""Summarise a nextest JUnit file as a per-binary failure table (GM-344).

Reads one or more nextest JUnit files - one per test section,
`target/nextest/ci/junit-<section>.xml` (see `scripts/test-sections.sh`) - and
prints one table over all of them as GitHub-flavoured Markdown for
`$GITHUB_STEP_SUMMARY`, so the state of a platform is readable on the run page rather than by
downloading a six-thousand-line log.

The grouping is the test *binary*, which is the unit a cause maps onto: 25
failures in one binary is one problem, 25 spread across twelve is a different
situation entirely, and that distinction is what made GM-337 diagnosable.

Written as a file rather than inlined into the workflow so that it can be run
- and therefore tested - without a runner. `scripts/ci-diff-runs.sh` (GM-345)
reads the same artifact.
"""

import sys
import xml.etree.ElementTree as ET


def summarise(junit_paths: list, target: str) -> str:
    # One binary can appear in several files (core's lib is split across
    # sections), so counts are summed per binary name.
    per_binary = {}
    for path in junit_paths:
        for suite in ET.parse(path).getroot().iter("testsuite"):
            tests = int(suite.get("tests", "0"))
            # nextest distinguishes an assertion failure from a harness error;
            # for "is this binary red" they are the same thing.
            bad = int(suite.get("failures", "0")) + int(suite.get("errors", "0"))
            name = suite.get("name", "?")
            prev_bad, prev_tests = per_binary.get(name, (0, 0))
            per_binary[name] = (prev_bad + bad, prev_tests + tests)
    total = sum(tests for _, tests in per_binary.values())
    failed_total = sum(bad for bad, _ in per_binary.values())
    rows = [(bad, tests, name) for name, (bad, tests) in per_binary.items() if bad]

    out = [f"## {target}", ""]
    if total == 0:
        out.append("The JUnit files hold no test suites - nextest wrote them but ran nothing.")
        return "\n".join(out)
    if not failed_total:
        out.append(f"All **{total}** tests passed.")
        return "\n".join(out)

    plural = "binary" if len(rows) == 1 else "binaries"
    out.append(f"**{failed_total}** of **{total}** tests failed, in **{len(rows)}** {plural}.")
    out.append("")
    out.append("| failed | of | binary |")
    out.append("| ---: | ---: | --- |")
    for bad, tests, name in sorted(rows, reverse=True):
        out.append(f"| {bad} | {tests} | `{name}` |")
    out.append("")
    out.append("The failing tests are named in this run's `junit-*` artifact.")
    return "\n".join(out)


def main() -> int:
    if len(sys.argv) < 3:
        print(f"usage: {sys.argv[0]} <junit.xml>... <target>", file=sys.stderr)
        return 2
    print(summarise(sys.argv[1:-1], sys.argv[-1]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
