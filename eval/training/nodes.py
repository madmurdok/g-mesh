#!/usr/bin/env python3
"""Snapshot plus pinned checkout to `<version>/nodes/<repo>.jsonl`
(docs/design/GM-456-training-data.md, sections 5 and 7).

One row per embeddable, non-test node of the repo's snapshot
(`work/<repo>.sqlite`), raw fields only. A node is embeddable when its doc
comment or signature is non-blank (`sample_targets.embeddable`, the rule
`text_to_embed` is Some for); test and fixture code is dropped by
`sample_targets.TEST_PATH`/`TEST_QNAME`. `srcLines` is the node's span read
from the checkout the way the eval reads a source file
(`gm698_ce_text_fixture.read_lines`), rebased to line 0; `spanSha256` is the
whitespace-normalised span's sha256 (leak-guard check 2).

A file with a line longer than `MAX_LINE_CHARS` (minified or bundled code) is
dropped whole: its nodes are counted under `dropped["minified_file"]`.

usage: nodes.py --version v0.0-stub [--repo mini-redis ...]
"""

import argparse
import json
import sqlite3
import sys
from collections import Counter
from pathlib import Path

import common as C
from sample_targets import TEST_PATH, TEST_QNAME, embeddable

MAX_LINE_CHARS = 2000  # a file with a longer line contributes no nodes

COLS = ("id", "kind", "nativeKind", "name", "qualifiedName", "filePath", "startLine", "endLine",
        "signature", "docComment", "language")


def owner_of(qualified_name, name):
    """`qualifiedName` without `name` and the separator before it (`::`, `.`,
    `#`); None when that leaves nothing or `name` is not its suffix. A port of
    `context::owner_of` (core/src/cli/embed_eval/context.rs)."""
    if not name or not qualified_name.endswith(name):
        return None
    rest = qualified_name[: len(qualified_name) - len(name)]
    for sep in ("::", ".", "#"):
        if rest.endswith(sep):
            r = rest[: len(rest) - len(sep)]
            return r or None
    return None


def trait_impl_segment(owner):
    """The last `::` segment of `owner` when it is `<X as T>`, as (X, T).
    A port of `context::trait_impl_segment`: angle brackets nest and ` as `
    splits at depth 1."""
    if not owner or not owner.endswith(">"):
        return None
    depth, open_at = 0, None
    for i in range(len(owner) - 1, -1, -1):
        c = owner[i]
        if c == ">":
            depth += 1
        elif c == "<":
            depth -= 1
            if depth == 0:
                open_at = i
                break
    if open_at is None:
        return None
    if open_at != 0 and not owner[:open_at].endswith("::"):
        return None
    inner = owner[open_at + 1: -1]
    depth = 0
    for i, c in enumerate(inner):
        if c == "<":
            depth += 1
        elif c == ">":
            depth -= 1
        elif c == " " and depth == 0 and inner[i:].startswith(" as "):
            x, t = inner[:i].strip(), inner[i + 4:].strip()
            return (x, t) if x and t else None
    return None


def read_lines(path):
    """A source file's lines exactly as the eval reads them."""
    return Path(path).read_text(errors="replace").split("\n")


def snapshot_rows(db):
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(f"SELECT {', '.join(COLS)} FROM nodes ORDER BY filePath, startLine, qualifiedName, id").fetchall()
    conn.close()
    return [dict(r) for r in rows]


def frame(repo, meta, rows, checkout):
    """The node rows of one repo, and the drop counts."""
    out, dropped, seen = [], Counter(), set()
    files = {}
    for r in rows:
        if not embeddable(r["docComment"], r["signature"]):
            dropped["not_embeddable"] += 1
            continue
        if TEST_PATH.search(r["filePath"]) or TEST_QNAME.search(r["qualifiedName"]):
            dropped["test_or_fixture"] += 1
            continue
        key = C.key_of(repo, r["filePath"], r["qualifiedName"], r["startLine"])
        if key in seen:
            dropped["duplicate_key"] += 1
            continue
        if r["filePath"] not in files:
            p = Path(checkout) / r["filePath"]
            files[r["filePath"]] = read_lines(p) if p.is_file() else None
        lines = files[r["filePath"]]
        if lines is None:
            dropped["no_source_file"] += 1
            continue
        if max(map(len, lines), default=0) > MAX_LINE_CHARS:
            dropped["minified_file"] += 1
            continue
        seen.add(key)
        span = lines[r["startLine"]: r["endLine"] + 1]
        out.append({
            "key": key,
            "repo": repo,
            "commit": meta["revision"],
            "language": r["language"],
            "kind": r["kind"],
            "nativeKind": r["nativeKind"],
            "name": r["name"],
            "qualifiedName": r["qualifiedName"],
            "parent": owner_of(r["qualifiedName"], r["name"]),
            "filePath": r["filePath"],
            "startLine": r["startLine"],
            "endLine": r["endLine"],
            "signature": r["signature"],
            "docComment": r["docComment"],
            "srcLines": span,
            "spanSha256": C.normalised_span_sha256(span),
        })
    return out, dropped


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo", action="append", default=[])
    args = ap.parse_args()
    vdir = C.version_dir(args.version)
    repos = C.corpora()
    work = C.work_dir()
    names = args.repo or [r for r in repos if (work / f"{r}.sqlite").exists()]
    summary = {}
    for repo in names:
        meta = repos[repo]
        record = json.load(open(work / f"{repo}.snapshot.json"))
        if record["revision"] != meta["revision"]:
            sys.exit(f"STOP: snapshot of {repo} is at {record['revision']}, corpora.toml pins {meta['revision']}")
        db = work / f"{repo}.sqlite"
        if C.sha256_file(db) != record["sha256"]:
            sys.exit(f"STOP: snapshot of {repo} changed since it was recorded")
        rows = snapshot_rows(db)
        nodes, dropped = frame(repo, meta, rows, work / "corpora" / repo)
        langs = Counter(n["language"] for n in nodes)
        off = {l: c for l, c in langs.items() if l != meta["language"]}
        C.write_jsonl(vdir / "nodes" / f"{repo}.jsonl", nodes)
        summary[repo] = {
            "snapshotNodes": len(rows),
            "frame": len(nodes),
            "byKind": dict(Counter(n["kind"] for n in nodes)),
            "otherLanguages": off,
            "dropped": dict(dropped),
            "snapshotSha256": record["sha256"],
        }
        print(f"nodes {repo}: {json.dumps(summary[repo])}")
    C.write_json(vdir / "nodes" / "summary.json", summary)


if __name__ == "__main__":
    main()
