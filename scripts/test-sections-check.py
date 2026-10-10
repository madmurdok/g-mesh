#!/usr/bin/env python3
"""Check that test sections partition the suite.

usage: test-sections-check.py <all.json> <section>=<section.json>...

Each JSON file is the output of `cargo nextest list --message-format json`:
`<all.json>` lists the whole suite, each `<section>.json` the same suite under
that section's filterset. A test is keyed by (binary-id, test name). A section
holds the tests whose `filter-match.status` is `matches`; the whole suite is
every listed test.

Exits 0 when every test is in exactly one section and no section matches a
test the whole-suite list lacks. Otherwise prints every offending test, one per
line, and exits 1. Uses no cargo and only the standard library, so it runs on
fixture files. The sections themselves: scripts/test-sections.sh.
"""

import json
import sys


def load(path):
    """Returns {(binary_id, test): matched} for one nextest list file."""
    with open(path, encoding="utf-8") as f:
        doc = json.load(f)
    out = {}
    for binary_id, suite in doc.get("rust-suites", {}).items():
        for test, case in suite.get("testcases", {}).items():
            status = case.get("filter-match", {}).get("status")
            out[(binary_id, test)] = status == "matches"
    return out


def check(full, sections):
    """full: set of keys; sections: list of (name, set of keys).

    Returns (missing, overlap, phantom): missing is a sorted key list, overlap
    a sorted list of (key, [section names]), phantom a sorted list of
    (key, section name).
    """
    owners = {}
    phantom = []
    for name, keys in sections:
        for key in keys:
            owners.setdefault(key, []).append(name)
            if key not in full:
                phantom.append((key, name))
    missing = sorted(k for k in full if k not in owners)
    overlap = sorted((k, names) for k, names in owners.items() if len(names) > 1)
    return missing, overlap, sorted(phantom)


def main(argv):
    if len(argv) < 3:
        print(f"usage: {argv[0]} <all.json> <section>=<section.json>...", file=sys.stderr)
        return 2
    full = set(load(argv[1]))
    sections = []
    for arg in argv[2:]:
        name, sep, path = arg.partition("=")
        if not sep or not name or not path:
            print(f"bad section argument {arg!r}, want <section>=<file>", file=sys.stderr)
            return 2
        sections.append((name, {k for k, m in load(path).items() if m}))

    missing, overlap, phantom = check(full, sections)

    for name, keys in sections:
        print(f"{name}: {len(keys)}")
    print(f"sum of sections: {sum(len(k) for _, k in sections)}")
    print(f"whole suite: {len(full)}")
    for (binary, test) in missing:
        print(f"missing: {binary} {test}")
    for (binary, test), names in overlap:
        print(f"overlap: {binary} {test} in {','.join(names)}")
    for (binary, test), name in phantom:
        print(f"phantom: {binary} {test} in {name}")
    if missing or overlap or phantom:
        print(
            f"FAIL: {len(missing)} missing, {len(overlap)} in two or more sections, "
            f"{len(phantom)} phantom"
        )
        return 1
    print("OK: every test is in exactly one section")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
