#!/usr/bin/env python3
"""SQL statements written outside the index's own modules (core/src).

A site is a string literal whose text starts with an SQL keyword (SELECT, INSERT, UPDATE, DELETE,
CREATE, DROP, ALTER, PRAGMA, WITH, REPLACE, ATTACH, DETACH, BEGIN, SAVEPOINT, RELEASE), on a line
that is neither test code nor a comment, in a file outside `storage/`, `graph/`, `link/` and
`model/`. Test code is `module-cycles.py`'s rule: files under `tests/` or `testdata/`, files named
`tests.rs`, `*_tests.rs`, `*_test.rs`, and `#[cfg(test)]` items in other files.

Prints `count path` per file with a site, then the total. With `--check ALLOW` it also compares
each file's count with ALLOW, whose lines read `path count  # reason` (`#` starts a comment), and
exits 1 when a file with sites is not listed, when a count is above its allowance, or when it is
below it: a lowered count must be recorded by lowering the allowance, so the list only goes down.
"""

import argparse
import importlib.util
import os
import re
import sys

OWNERS = ("storage", "graph", "link", "model")
SQL = re.compile(
    r'"\s*(SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|PRAGMA|WITH|REPLACE|ATTACH|DETACH|BEGIN|SAVEPOINT|RELEASE)\b'
)


def load_module_cycles():
    here = os.path.dirname(os.path.abspath(__file__))
    spec = importlib.util.spec_from_file_location("module_cycles", os.path.join(here, "module-cycles.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def mask(src):
    """Blank comments and the bodies of string and char literals, keeping every newline and the
    quotes, so a masked line is code only and lines up with the original."""
    out = list(src)
    n = len(src)

    def blank(a, b):
        for k in range(a, b):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
            continue
        if src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
            continue
        starts_ident = i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")
        raw = re.match(r'b?r(#*)"', src[i : i + 260]) if c in "rb" and starts_ident else None
        if raw:
            end = '"' + raw.group(1)
            j = src.find(end, i + raw.end())
            j = n if j < 0 else j + len(end)
            blank(i + raw.end(), j - len(end))
            i = j
            continue
        if c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i + 1, j)
            i = min(j + 1, n)
            continue
        if c == "'":
            char = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", src[i : i + 12])
            if char:
                blank(i + 1, i + char.end() - 1)
                i += char.end()
                continue
        i += 1
    return "".join(out)


def count_sites(src_root):
    """`{path relative to src_root: number of sites}` for every file with at least one."""
    cycles = load_module_cycles()
    counts = {}
    for dirpath, _, files in os.walk(src_root):
        for name in files:
            if not name.endswith(".rs"):
                continue
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, src_root)
            top = rel.split(os.sep)[0]
            if top.removesuffix(".rs") in OWNERS or cycles.is_test_file(rel):
                continue
            with open(path, encoding="utf-8") as f:
                src = f.read()
            live = cycles.strip_cfg_test(mask(src)).split("\n")
            original = src.split("\n")
            if len(live) != len(original):
                sys.exit(f"sql-sites: masking changed the line count of {rel}")
            n = sum(len(SQL.findall(o)) for o, l in zip(original, live) if l.strip() and SQL.search(o))
            if n:
                counts[rel.replace(os.sep, "/")] = n
    return counts


def read_allow(path):
    """`{path: allowance}` from an allow file; exits on a malformed or repeated line."""
    allow = {}
    with open(path, encoding="utf-8") as f:
        for number, line in enumerate(f, 1):
            text = line.split("#", 1)[0].strip()
            if not text:
                continue
            parts = text.split()
            if len(parts) != 2 or not parts[1].isdigit():
                sys.exit(f"sql-sites: {path}:{number}: expected `path count  # reason`, got {line.rstrip()!r}")
            if parts[0] in allow:
                sys.exit(f"sql-sites: {path}:{number}: {parts[0]} is listed twice")
            allow[parts[0]] = int(parts[1])
    return allow


def check(counts, allow):
    """The violations of `allow` by `counts`, one message each."""
    problems = []
    for rel in sorted(set(counts) | set(allow)):
        found, allowed = counts.get(rel, 0), allow.get(rel)
        if allowed is None:
            problems.append(f"{rel}: {found} SQL site(s), not in the allow list (move them to graph/ or storage/)")
        elif found > allowed:
            problems.append(f"{rel}: {found} SQL site(s), allowed {allowed} (move the new ones to graph/ or storage/)")
        elif found < allowed:
            problems.append(f"{rel}: {found} SQL site(s), allowed {allowed} (lower the allowance to {found})")
    return problems


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--src", default=os.path.join(here, "..", "core", "src"), help="crate src dir (default: core/src)")
    ap.add_argument("--check", metavar="ALLOW", help="fail unless every file's count equals its allowance in ALLOW")
    args = ap.parse_args()

    counts = count_sites(args.src)
    for rel, n in sorted(counts.items(), key=lambda item: (-item[1], item[0])):
        print(f"{n} {rel}")
    print(f"total {sum(counts.values())}")
    if args.check:
        problems = check(counts, read_allow(args.check))
        for problem in problems:
            print(f"sql-sites: {problem}", file=sys.stderr)
        if problems:
            sys.exit(1)


if __name__ == "__main__":
    main()
