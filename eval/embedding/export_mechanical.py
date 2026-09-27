#!/usr/bin/env python3
"""Mechanical name queries for the floor fit set (D6 of
docs/architecture/embedding-eval.md), written to
eval/embedding/queries/mechanical/<corpus>.jsonl.

Positives: embeddable, non-test declarations sampled from the corpus's
snapshot by a fixed seed, queried by their own name; names carried by at most
three embeddable nodes, all of which are expected (a method name shared by two
types is answered by either, as the calibration's name match allowed).
Negatives: names sampled from the other corpora's snapshots that no node of
this snapshot carries and that occur as a whole word in no file of the pinned
checkout.

Everything is structural; no model output is read. The harness treats these
rows as fit-half only and never scores them in recall or MRR.

Usage: export_mechanical.py [--count 150] [--seed 398] [corpus ...]
Reads work/<corpus>.sqlite (the snapshots) and work/corpora/<corpus>.
"""

import argparse
import json
import os
import random
import re
import sqlite3
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from check_queries import LANGUAGE, PREFIX  # noqa: E402
from sample_targets import TEST_PATH, TEST_QNAME, embeddable  # noqa: E402

TEXT_EXT = re.compile(r"\.(rs|go|py|ts|tsx|js|jsx|mjs|cjs|md|toml|json|ya?ml|txt|html|css)$")


def snapshot_rows(corpus):
    conn = sqlite3.connect(os.path.join(HERE, "work", f"{corpus}.sqlite"))
    conn.row_factory = sqlite3.Row
    return conn.execute(
        "SELECT kind, name, qualifiedName, filePath, signature, docComment, language FROM nodes ORDER BY id"
    ).fetchall()


def checkout_words(corpus):
    root = os.path.join(HERE, "work", "corpora", corpus)
    words = set()
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in (".git", "node_modules", "target")]
        for f in filenames:
            if not TEXT_EXT.search(f):
                continue
            try:
                with open(os.path.join(dirpath, f), encoding="utf-8", errors="replace") as fh:
                    words.update(re.findall(r"[A-Za-z_$][A-Za-z0-9_$]*", fh.read()))
            except OSError:
                pass
    return words


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--count", type=int, default=150)
    ap.add_argument("--seed", type=int, default=398)
    ap.add_argument("corpora", nargs="*", default=list(PREFIX))
    args = ap.parse_args()

    rows = {c: snapshot_rows(c) for c in PREFIX}
    candidates = {}
    mechanical_expected = {}
    for c, rs in rows.items():
        names = {}
        for r in rs:
            if embeddable(r["docComment"], r["signature"]):
                names.setdefault(r["name"], []).append(r)
        candidates[c] = [
            r
            for r in rs
            if r["language"] == LANGUAGE[c]
            and r["kind"] in ("Function", "Type", "Variable")
            and embeddable(r["docComment"], r["signature"])
            and not TEST_PATH.search(r["filePath"])
            and not TEST_QNAME.search(r["qualifiedName"])
            and len(r["name"]) >= 4
            and re.fullmatch(r"[A-Za-z_$][A-Za-z0-9_$]*", r["name"])
            and len(names[r["name"]]) <= 3
        ]
        # One query per name: the first sampled declaration stands for all.
        seen, unique = set(), []
        for r in candidates[c]:
            if r["name"] not in seen:
                seen.add(r["name"])
                unique.append(r)
        candidates[c] = unique
        mechanical_expected[c] = names

    for c in args.corpora:
        rng = random.Random(f"{args.seed}:{c}")
        pos = rng.sample(candidates[c], min(args.count, len(candidates[c])))
        present = {r["name"] for r in rows[c]} | checkout_words(c)
        foreign = sorted({r["name"] for o in PREFIX if o != c for r in candidates[o]} - present)
        neg = rng.sample(foreign, min(args.count, len(foreign)))

        out = os.path.join(HERE, "queries", "mechanical", f"{c}.jsonl")
        os.makedirs(os.path.dirname(out), exist_ok=True)
        with open(out, "w") as f:
            for i, r in enumerate(pos, 1):
                f.write(
                    json.dumps(
                        {
                            "id": f"{PREFIX[c]}-m{i:03d}",
                            "corpus": c,
                            "language": LANGUAGE[c],
                            "kind": "positive",
                            "shape": "name",
                            "text": r["name"],
                            "expected": [
                                {"filePath": e["filePath"], "qualifiedName": e["qualifiedName"], "kind": e["kind"]}
                                for e in mechanical_expected[c][r["name"]]
                            ],
                            "derivation": f"Mechanical: sampled (seed {args.seed}) from the snapshot's embeddable, "
                            "non-test declarations whose name at most 3 embeddable nodes carry; queried by that name, "
                            "every embeddable node of that name expected.",
                            "author": "export_mechanical.py",
                        }
                    )
                    + "\n"
                )
            for i, name in enumerate(neg, 1):
                f.write(
                    json.dumps(
                        {
                            "id": f"{PREFIX[c]}-mn{i:03d}",
                            "corpus": c,
                            "language": LANGUAGE[c],
                            "kind": "absent",
                            "shape": "name",
                            "text": name,
                            "expected": [],
                            "derivation": f"Mechanical negative: a declaration name from another corpus (seed {args.seed}); "
                            "absent: no node of this snapshot has this name and it is no whole word of any "
                            "text file of the pinned checkout.",
                            "author": "export_mechanical.py",
                        }
                    )
                    + "\n"
                )
        print(json.dumps({"corpus": c, "positives": len(pos), "negatives": len(neg), "pool": len(candidates[c])}))


if __name__ == "__main__":
    main()
