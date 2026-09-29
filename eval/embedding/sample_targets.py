#!/usr/bin/env python3
"""Target-first sampling for the embedding eval's query set (D3 step 1 of
docs/architecture/embedding-eval.md).

Reads the embeddable nodes of a g-mesh index (rows where `text_to_embed`
would return Some: a non-blank doc comment or signature), keeps those that
exist at the pinned checkout, drops test/fixture code, and writes them in a
seeded, kind-stratified order to eval/embedding/targets/<corpus>.jsonl.

The order is the authoring order: authors take targets from the top and may
skip one only with a recorded reason, then take the next. Any prefix of the
list keeps the D3 mix (functions ~60%, types ~30%, other ~10%) as long as
every stratum has rows left.

The frame is structural data only; no model output is read.

`--exclude <eval dir of an earlier study>` makes the list disjoint from that
study (GM-422 confirmatory protocol, section 3 step 3): before ordering, the
frame drops (a) every target the study consumed, rows 1..k of its
`targets/<corpus>.jsonl` where k is the highest target number used by a
query derivation or recorded in `targets/<corpus>.skips.jsonl`, and (b) every
symbol in the `expected` set of any of its authored queries. Both match on
filePath plus qualifiedName. The dropped counts go into `meta`.

`--expect-sha256` stops unless the frame database has exactly that sha256
(the confirmatory study samples over the D4 snapshot GM-398 embedded).
"""

import argparse
import hashlib
import json
import os
import random
import re
import sqlite3
import sys

# Pattern of strata over ten consecutive picks: 6 functions, 3 types, 1 other.
STRATA_PATTERN = ["fn", "fn", "type", "fn", "fn", "type", "fn", "type", "fn", "other"]

TEST_PATH = re.compile(
    r"(^|/)(tests?|__tests__|testdata|fixtures?|conformance|benches|examples?|docs)(/|$)"
    r"|_test\.go$|(^|/)test_[^/]*\.py$|_test\.py$|conftest\.py$"
    r"|\.(test|spec)\.[cm]?[jt]sx?$"
)
TEST_QNAME = re.compile(r"(^|::|\.|#)tests?(::|\.|#|$)|(^|::|\.|#)Test[A-Z_]|(^|::|\.|#)test_")


def stratum(kind):
    if kind == "Function":
        return "fn"
    if kind == "Type":
        return "type"
    return "other"


def embeddable(doc, sig):
    return bool((doc or "").strip()) or bool((sig or "").strip())


def line_at_pin(checkout, row):
    """1-based line of the row's declaration in the pinned checkout, or None.

    The frame index may come from a nearby revision, so the line is
    re-anchored: the whole-word occurrence of the name nearest the indexed
    line. A name that is not a plain identifier (a Rust `<T as Trait>` impl)
    cannot be anchored and is dropped. File nodes anchor at line 1.
    """
    path = os.path.join(checkout, row["filePath"])
    if not os.path.isfile(path):
        return None
    if row["kind"] == "File":
        return 1
    name = row["name"]
    if not re.fullmatch(r"[A-Za-z_$][\w$]*", name):
        return None
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.read().split("\n")
    except OSError:
        return None
    word = re.compile(r"(?<![\w$])" + re.escape(name) + r"(?![\w$])")
    hits = [i for i, line in enumerate(lines) if word.search(line)]
    if not hits:
        return None
    return min(hits, key=lambda i: (abs(i - row["startLine"]), i)) + 1


DERIVED_TARGET = re.compile(r"Target sampled \(seed \d+, #(\d+)\)")


def exclusion(study_dir, corpus):
    """Keys (filePath, qualifiedName) an earlier study consumed, and a summary."""
    targets = [json.loads(l) for l in open(os.path.join(study_dir, "targets", f"{corpus}.jsonl")) if l.strip()]
    targets = [t for t in targets if "meta" not in t]
    queries = [json.loads(l) for l in open(os.path.join(study_dir, "queries", f"{corpus}.jsonl")) if l.strip()]
    skips_path = os.path.join(study_dir, "targets", f"{corpus}.skips.jsonl")
    skips = [json.loads(l) for l in open(skips_path) if l.strip()] if os.path.exists(skips_path) else []
    used = [int(m.group(1)) for q in queries if (m := DERIVED_TARGET.search(q.get("derivation", "")))]
    k = max(used + [s["n"] for s in skips] + [0])
    consumed = {(t["filePath"], t["qualifiedName"]) for t in targets if t["n"] <= k}
    expected = {(e["filePath"], e["qualifiedName"]) for q in queries for e in q.get("expected", [])}
    summary = {
        "study": study_dir,
        "consumedPrefix": k,
        "consumedTargets": len(consumed),
        "expectedSymbols": len(expected),
        "keys": len(consumed | expected),
    }
    return consumed | expected, summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--index-db", required=True)
    ap.add_argument("--checkout", required=True)
    ap.add_argument("--language", required=True)
    ap.add_argument("--seed", type=int, default=398)
    ap.add_argument("--count", type=int, required=True, help="targets to emit (authoring oversamples)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--exclude", help="eval dir of an earlier study whose targets and expected symbols are dropped")
    ap.add_argument("--expect-sha256", help="stop unless the frame database has this sha256")
    args = ap.parse_args()

    with open(args.index_db, "rb") as f:
        db_sha = hashlib.sha256(f.read()).hexdigest()
    if args.expect_sha256 and db_sha != args.expect_sha256:
        sys.exit(f"STOP: {args.index_db} has sha256 {db_sha}, expected {args.expect_sha256}")
    excluded, exclude_meta = exclusion(args.exclude, args.corpus) if args.exclude else (set(), None)

    conn = sqlite3.connect(args.index_db)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        "SELECT kind, nativeKind, name, qualifiedName, filePath, startLine, endLine, signature, docComment "
        "FROM nodes WHERE language = ? ORDER BY filePath, startLine, qualifiedName",
        (args.language,),
    ).fetchall()

    seen = set()
    frame = {"fn": [], "type": [], "other": []}
    dropped = {"not_embeddable": 0, "test_or_fixture": 0, "not_at_pin": 0, "duplicate": 0}
    if args.exclude:
        dropped["excluded"] = 0
    for r in rows:
        if not embeddable(r["docComment"], r["signature"]):
            dropped["not_embeddable"] += 1
            continue
        if TEST_PATH.search(r["filePath"]) or TEST_QNAME.search(r["qualifiedName"]):
            dropped["test_or_fixture"] += 1
            continue
        if (r["filePath"], r["qualifiedName"]) in excluded:
            dropped["excluded"] += 1
            continue
        key = (r["filePath"], r["qualifiedName"], r["kind"])
        if key in seen:
            dropped["duplicate"] += 1
            continue
        seen.add(key)
        line = line_at_pin(args.checkout, r)
        if line is None:
            dropped["not_at_pin"] += 1
            continue
        frame[stratum(r["kind"])].append((r, line))

    rng = random.Random(args.seed)
    for s in ("fn", "type", "other"):
        rng.shuffle(frame[s])

    out = []
    cursor = {"fn": 0, "type": 0, "other": 0}
    i = 0
    while len(out) < args.count and any(cursor[s] < len(frame[s]) for s in frame):
        want = STRATA_PATTERN[i % len(STRATA_PATTERN)]
        i += 1
        # An exhausted stratum hands its slot to the next non-empty one in fn, type, other order.
        for s in [want, "fn", "type", "other"]:
            if cursor[s] < len(frame[s]):
                r, line = frame[s][cursor[s]]
                cursor[s] += 1
                out.append(
                    {
                        "n": len(out) + 1,
                        "stratum": s,
                        "kind": r["kind"],
                        "nativeKind": r["nativeKind"],
                        "qualifiedName": r["qualifiedName"],
                        "filePath": r["filePath"],
                        "lineAtPin": line,
                    }
                )
                break

    meta = {
        "corpus": args.corpus,
        "seed": args.seed,
        "frameSource": os.path.abspath(args.index_db),
        "frameSha256": db_sha,
        "frameSizes": {s: len(frame[s]) for s in frame},
        "dropped": dropped,
    }
    if exclude_meta:
        meta["exclude"] = exclude_meta
    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w") as f:
        f.write(json.dumps({"meta": meta}) + "\n")
        for t in out:
            f.write(json.dumps(t) + "\n")
    print(json.dumps(meta), file=sys.stderr)


if __name__ == "__main__":
    main()
