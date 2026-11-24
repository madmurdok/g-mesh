#!/usr/bin/env python3
"""Module-level dependency cycles of the core crate (top-level modules under core/src).

An edge A -> B means code in top-level module A refers to `crate::B` (A != B), either in a
`use crate::...` statement (groups `use crate::{a::x, b}` give one edge per first-level target)
or as an inline `crate::B::...` path. Comments and string literals are ignored. By default test
code is excluded: files under a `tests/` or `testdata/` directory, files named `tests.rs`,
`*_tests.rs`, `*_test.rs`, and brace-balanced `#[cfg(test)]` items in other files. This is the
edge definition of docs/review/4.30/module-map.md sections 3-5. Textual, not name-resolved:
`super::`, `self::` and `g_mesh::` paths and `use crate::*` globs are not counted.

Prints the strongly connected components (Tarjan) with more than one module, and optionally
the edge list. `--absent A->B` (repeatable) exits 1 if that edge exists; `--max-scc N` exits 1
if the largest component has more than N modules. `--drop A->B` / `--add A->B` (repeatable)
edit the computed graph before Tarjan runs, to project the components a planned move leaves.
Quote edge arguments in a shell (`'A->B'`): an unquoted `>` is a redirection.

`--layers` checks the target crate DAG of docs/design/GM-718-core-crate-decomposition.md
(`CRATES`, lowest first, and `CRATE_MAP`, module path prefix -> crate). It exits 1 on any
reference from a module of a lower crate to a module of a higher one, and on any module the map
does not cover. It always counts test code (test files and `#[cfg(test)]` items), `use` and inline
`crate::` paths at full depth (so `crate::config::embedding` maps to its own crate), and `mod x`
declarations whose child maps to another crate; `--include-tests`, `--use-only`, `--drop` and
`--add` do not change it. Each failure prints from/to module, from/to crate, prod or test, the
kind and `file:line`.

Examples:
    scripts/module-cycles.py
    scripts/module-cycles.py --edges --use-only
    scripts/module-cycles.py --absent 'project_walk->watcher' --absent 'protocol->graph'
    scripts/module-cycles.py --drop 'storage->daemon' --add 'plugin_manifest->graph'
    scripts/module-cycles.py --layers
"""

import argparse
import os
import re
import sys
from collections import defaultdict

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def strip_comments_and_strings(src):
    """Replace comments and string/char literal bodies with spaces, keeping newlines."""
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
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
            out.append(re.sub(r"[^\n]", " ", src[i:j]))
            i = j
            continue
        m = re.match(r'b?r(#*)"', src[i:i + 260]) if c in "rb" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")) else None
        if m:
            end = '"' + m.group(1)
            j = src.find(end, i + m.end())
            j = n if j < 0 else j + len(end)
            out.append('""' + re.sub(r"[^\n]", " ", src[i + 2:j]))
            i = j
            continue
        if c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            out.append('""' + re.sub(r"[^\n]", " ", src[i + 1:j]))
            i = j
            continue
        if c == "'":
            # char literal ('x', '\n', '\u{..}') vs lifetime ('a)
            m = re.match(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'", src[i:i + 12])
            if m:
                out.append("' '")
                i += m.end()
                continue
        out.append(c)
        i += 1
    return "".join(out)


def strip_cfg_test(src):
    """Remove `#[cfg(test)]` items (the attribute through the end of the next item's braces or `;`)."""
    out, i = [], 0
    pat = re.compile(r"#\[cfg\(test\)\]")
    while True:
        m = pat.search(src, i)
        if not m:
            out.append(src[i:])
            return "".join(out)
        out.append(src[i:m.start()])
        j = m.end()
        # find the item's first `{` or `;` at depth 0
        while j < len(src) and src[j] not in "{;":
            j += 1
        if j < len(src) and src[j] == "{":
            depth = 0
            while j < len(src):
                if src[j] == "{":
                    depth += 1
                elif src[j] == "}":
                    depth -= 1
                    if depth == 0:
                        j += 1
                        break
                j += 1
        else:
            j += 1
        out.append(re.sub(r"[^\n]", " ", src[m.start():j]))
        i = j


def split_top(s):
    """Split a `{a, b::{c, d}, e}` body at depth-0 commas."""
    parts, depth, cur = [], 0, []
    for ch in s:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return [p.strip() for p in parts if p.strip()]


USE_STMT = re.compile(r"\buse\s+([^;]*);", re.S)


def targets_of_use(body):
    """First-level `crate::X` targets of one `use` tree body (text between `use` and `;`)."""
    body = re.sub(r"\s+", "", body)
    out = []

    def walk(tree, prefix_is_crate):
        if prefix_is_crate:
            if tree.startswith("{"):
                for p in split_top(tree[1:-1]):
                    m = IDENT.match(p)
                    if m and m.group(0) not in ("self", "super", "crate"):
                        out.append(m.group(0))
            else:
                m = IDENT.match(tree)
                if m and m.group(0) not in ("self", "super", "crate"):
                    out.append(m.group(0))
            return
        if tree.startswith("{"):
            for p in split_top(tree[1:-1]):
                walk(p, False)
        elif tree.startswith("crate::"):
            walk(tree[len("crate::"):], True)

    walk(body.lstrip(":"), False)
    return out


INLINE = re.compile(r"(?<![A-Za-z0-9_:])crate::([A-Za-z_][A-Za-z0-9_]*)")


def is_test_file(rel):
    parts = rel.split(os.sep)
    if any(p in ("tests", "testdata") for p in parts[:-1]):
        return True
    name = parts[-1]
    return name == "tests.rs" or name.endswith("_tests.rs") or name.endswith("_test.rs")


def collect(src_root, include_tests, use_only):
    modules = set()
    for name in os.listdir(src_root):
        p = os.path.join(src_root, name)
        if os.path.isdir(p):
            modules.add(name)
        elif name.endswith(".rs") and name not in ("lib.rs", "main.rs"):
            modules.add(name[:-3])
    edges = defaultdict(lambda: [0, 0])  # (a, b) -> [use sites, inline sites]
    for dirpath, _, files in os.walk(src_root):
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            path = os.path.join(dirpath, f)
            rel = os.path.relpath(path, src_root)
            top = rel.split(os.sep)[0]
            if top.endswith(".rs"):
                top = top[:-3]
            if top not in modules:
                continue
            if not include_tests and is_test_file(rel):
                continue
            with open(path, encoding="utf-8") as fh:
                src = strip_comments_and_strings(fh.read())
            if not include_tests:
                src = strip_cfg_test(src)
            use_spans = []
            for m in USE_STMT.finditer(src):
                use_spans.append((m.start(), m.end()))
                for t in set(targets_of_use(m.group(1))):
                    if t in modules and t != top:
                        edges[(top, t)][0] += 1
            if use_only:
                continue
            for m in INLINE.finditer(src):
                if any(s <= m.start() < e for s, e in use_spans):
                    continue
                t = m.group(1)
                if t in modules and t != top:
                    edges[(top, t)][1] += 1
    return sorted(modules), dict(edges)


# Target crates of docs/design/GM-718-core-crate-decomposition.md section 2, lowest first. An edge
# may point only to the same crate or to a lower one.
CRATES = ["g-mesh-base", "g-mesh-index", "g-mesh-embed", "g-mesh-mcp", "g-mesh"]

# Module path prefix (under `crate::`) -> target crate; the longest matching prefix wins. Every
# top-level module must be covered: `--layers` reports an unmapped one as a failure.
CRATE_MAP = {
    "knobs": "g-mesh-base",
    "fsutil": "g-mesh-base",
    "paths": "g-mesh-base",
    "log": "g-mesh-base",
    "process": "g-mesh-base",
    "ipc": "g-mesh-base",
    "control": "g-mesh-base",
    "indexing_status": "g-mesh-base",
    "protocol": "g-mesh-base",
    "project_walk": "g-mesh-base",
    "config": "g-mesh-base",
    "model": "g-mesh-index",
    "graph": "g-mesh-index",
    "link": "g-mesh-index",
    "storage": "g-mesh-index",
    "project_detect": "g-mesh-index",
    "languages": "g-mesh-index",
    "plugin_manifest": "g-mesh-index",
    "index_health": "g-mesh-index",
    "gc": "g-mesh-index",
    "test_plugin": "g-mesh-index",
    "embedding": "g-mesh-embed",
    "config::embedding": "g-mesh-embed",
    "mcp": "g-mesh-mcp",
    "daemon": "g-mesh",
    "shim": "g-mesh",
    "cli": "g-mesh",
    "ingest": "g-mesh",
    "watcher": "g-mesh",
}

MOD_DECL = re.compile(r"\bmod\s+([A-Za-z_][A-Za-z0-9_]*)\s*[;{]")


def crate_of(path, crate_map):
    """Target crate of a module path (list of segments under `crate::`), or None if unmapped."""
    for k in range(len(path), 0, -1):
        c = crate_map.get("::".join(path[:k]))
        if c is not None:
            return c
    return None


def module_path_of_file(rel):
    """`config/embedding.rs` -> [config, embedding]; `config/mod.rs` -> [config]."""
    parts = rel[:-3].split(os.sep)
    if parts[-1] == "mod":
        parts = parts[:-1]
    return parts


def crate_paths_of_use(body):
    """Every `crate::...` path a `use` tree body names, as segment lists (`self` in a group names
    the group's prefix; globs and `as` renames end the path)."""
    body = re.sub(r"\s+as\s+[A-Za-z_][A-Za-z0-9_]*", "", body)
    body = re.sub(r"\s+", "", body)
    out = []

    def walk(tree, prefix):
        # prefix: None outside `crate::`, else the segments after `crate::` so far
        if tree.startswith("{"):
            for p in split_top(tree[1:-1]):
                walk(p, prefix)
            return
        if prefix is None:
            if tree.startswith("crate::"):
                walk(tree[len("crate::"):], [])
            return
        segs = []
        rest = tree
        while True:
            m = IDENT.match(rest)
            if not m:
                break
            segs.append(m.group(0))
            rest = rest[m.end():]
            if not rest.startswith("::"):
                break
            rest = rest[2:]
            if rest.startswith("{"):
                for p in split_top(rest[1:-1]):
                    walk(p, prefix + segs)
                return
        for i, s in enumerate(segs):
            if s in ("self", "super", "crate"):
                segs = segs[:i]
                break
        if prefix + segs:
            out.append(prefix + segs)

    walk(body, None)
    return out


INLINE_PATH = re.compile(r"(?<![A-Za-z0-9_:])crate((?:::[A-Za-z_][A-Za-z0-9_]*)+)")


MACRO_EXPORT = re.compile(r"#\[macro_export\]\s*macro_rules!\s*([A-Za-z_][A-Za-z0-9_]*)")


def exported_macros(src_root):
    """`#[macro_export]` macro name -> module path of the file defining it (such a macro is named
    `crate::name` but belongs to that module's crate)."""
    out = {}
    for dirpath, _, files in os.walk(src_root):
        for f in files:
            if not f.endswith(".rs"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, f), src_root)
            if rel in ("lib.rs", "main.rs"):
                continue
            with open(os.path.join(dirpath, f), encoding="utf-8") as fh:
                src = strip_comments_and_strings(fh.read())
            for m in MACRO_EXPORT.finditer(src):
                out[m.group(1)] = module_path_of_file(rel)
    return out


def collect_sites(src_root, crate_map):
    """Every crate-crossing reference site, test code included.

    Returns (unmapped, sites): `unmapped` is the sorted list of top-level modules the crate map
    does not cover; each site is (rel file, line, from module path, to module path, from crate,
    to crate, kind, is_test). Kind is `use`, `inline`, or `mod` (a `mod x` declaration whose child
    maps to another crate). A site is test code when its file is a test file (`is_test_file`) or
    it lies in a `#[cfg(test)]` item.
    """
    unmapped = set()
    sites = []
    macros = exported_macros(src_root)
    for dirpath, dirs, files in os.walk(src_root):
        dirs.sort()
        for f in sorted(files):
            if not f.endswith(".rs"):
                continue
            path = os.path.join(dirpath, f)
            rel = os.path.relpath(path, src_root)
            if rel in ("lib.rs", "main.rs"):
                continue
            from_path = module_path_of_file(rel)
            from_crate = crate_of(from_path, crate_map)
            if from_crate is None:
                unmapped.add(from_path[0])
                continue
            file_is_test = is_test_file(rel)
            with open(path, encoding="utf-8") as fh:
                src = strip_comments_and_strings(fh.read())
            # same length as src; a `#[cfg(test)]` item is blanked
            prod = src if file_is_test else strip_cfg_test(src)

            def add(pos, to_path, kind):
                if to_path[0] in macros:
                    to_path = macros[to_path[0]] + to_path
                to_crate = crate_of(to_path, crate_map)
                if to_crate is None:
                    unmapped.add(to_path[0])
                    return
                if to_crate == from_crate:
                    return
                is_test = file_is_test or prod[pos] != src[pos]
                line = src.count("\n", 0, pos) + 1
                sites.append((rel, line, "::".join(from_path), "::".join(to_path), from_crate, to_crate, kind, is_test))

            use_spans = []
            for m in USE_STMT.finditer(src):
                use_spans.append((m.start(), m.end()))
                for p in crate_paths_of_use(m.group(1)):
                    add(m.start(), p, "use")
            for m in INLINE_PATH.finditer(src):
                if any(s <= m.start() < e for s, e in use_spans):
                    continue
                add(m.start(), m.group(1)[2:].split("::"), "inline")
            for m in MOD_DECL.finditer(src):
                child = from_path + [m.group(1)]
                if crate_of(child, crate_map) != from_crate:
                    add(m.start(), child, "mod")
    return sorted(unmapped), sites


def check_layers(src_root, crate_map=None):
    """Print every upward crate edge and every unmapped module; return True if there is none."""
    crate_map = CRATE_MAP if crate_map is None else crate_map
    rank = {c: i for i, c in enumerate(CRATES)}
    unmapped, sites = collect_sites(src_root, crate_map)
    ok = True
    for mod in unmapped:
        print(f"FAIL: module {mod} is not in the crate map (CRATE_MAP in scripts/module-cycles.py)")
        ok = False
    upward = [s for s in sites if rank[s[5]] > rank[s[4]]]
    for rel, line, a, b, ca, cb, kind, is_test in sorted(upward, key=lambda s: (s[7], s[4], s[5], s[0], s[1])):
        where = "test" if is_test else "prod"
        print(f"FAIL: upward edge {a} -> {b} ({ca} -> {cb}) {where} {kind} {rel}:{line}")
        ok = False
    if ok:
        print("ok: no upward crate edges")
    else:
        n_test = sum(1 for s in upward if s[7])
        print(f"layers: {len(upward)} upward sites ({len(upward) - n_test} prod, {n_test} test), {len(unmapped)} unmapped modules")
    return ok


def tarjan(nodes, adj):
    index, low, on, stack, sccs, counter = {}, {}, set(), [], [], [0]

    def strong(v):
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on.add(v)
        for w in sorted(adj.get(v, ())):
            if w not in index:
                strong(w)
                low[v] = min(low[v], low[w])
            elif w in on:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop()
                on.discard(w)
                comp.append(w)
                if w == v:
                    break
            sccs.append(sorted(comp))

    for v in nodes:
        if v not in index:
            strong(v)
    return sccs


def parse_edge(spec):
    a, sep, b = spec.partition("->")
    if not sep or not a.strip() or not b.strip():
        sys.exit(f"bad edge {spec!r}: expected A->B")
    return a.strip(), b.strip()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--src", default=os.path.join(here, "..", "core", "src"), help="crate src dir (default: core/src)")
    ap.add_argument("--include-tests", action="store_true", help="count test files and #[cfg(test)] items too")
    ap.add_argument("--use-only", action="store_true", help="count `use` statements only, not inline paths")
    ap.add_argument("--edges", action="store_true", help="print the edge list (use/inline sites)")
    ap.add_argument("--absent", action="append", default=[], metavar="A->B", help="fail if edge A->B exists")
    ap.add_argument("--drop", action="append", default=[], metavar="A->B", help="remove edge A->B before Tarjan")
    ap.add_argument("--add", action="append", default=[], metavar="A->B", help="add edge A->B before Tarjan")
    ap.add_argument("--max-scc", type=int, help="fail if the largest component has more than N modules")
    ap.add_argument("--layers", action="store_true", help="fail on an edge from a lower target crate to a higher one (test code included)")
    args = ap.parse_args()

    modules, edges = collect(os.path.normpath(args.src), args.include_tests, args.use_only)
    for spec in args.drop:
        edges.pop(parse_edge(spec), None)
    for spec in args.add:
        a, b = parse_edge(spec)
        edges.setdefault((a, b), [0, 0])
        modules = sorted(set(modules) | {a, b})
    adj = defaultdict(set)
    for a, b in edges:
        adj[a].add(b)
    if args.edges:
        for a in modules:
            outs = [f"{b} {edges[(a, b)][0]}/{edges[(a, b)][1]}" for b in sorted(adj.get(a, ()))]
            if outs:
                print(f"{a} -> " + ", ".join(outs))
    sccs = sorted((c for c in tarjan(modules, adj) if len(c) > 1), key=lambda c: (-len(c), c))
    for c in sccs:
        print(f"SCC ({len(c)}): {', '.join(c)}")
    if not sccs:
        print("SCC: none (module graph is acyclic)")
    failed = False
    for spec in args.absent:
        a, b = parse_edge(spec)
        if (a, b) in edges:
            u, i = edges[(a, b)]
            print(f"FAIL: edge {a}->{b} present ({u} use / {i} inline sites)")
            failed = True
        else:
            print(f"ok: edge {a}->{b} absent")
    largest = max((len(c) for c in sccs), default=1)
    if args.max_scc is not None and largest > args.max_scc:
        print(f"FAIL: largest SCC has {largest} modules > {args.max_scc}")
        failed = True
    if args.layers and not check_layers(os.path.normpath(args.src)):
        failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
