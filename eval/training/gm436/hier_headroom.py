#!/usr/bin/env python3
"""Hierarchical-retrieval headroom (docs/design/GM-436-laptop-training.md, D6a).

Reads stored v2 `rankings.jsonl` and the corpus snapshots; embeds nothing.
Over positive queries (non-empty `expected`), per arm and corpus:

- missSameFile: among misses (no expected node in the top 10), the share whose
  expected node's file is among the files of the top-10 hits;
- hitDroppedF: among top-10 hits, the share a file stage keeping the top F
  files (files ordered by their best hit) would drop;
- r@10 of the flat ranking, of that top-F file stage (symbols re-ranked inside
  the kept files), and of a file oracle (only the expected file's hits remain).
  Only the top-100 kept hits are known, so a node outside them stays a miss.

Gate: a full hierarchical arm is worth building only if the oracle gains
>= 5 points r@10 over flat; the oracle is an upper bound, not a result.

usage: hier_headroom.py --runs <v2 runs dir> --arm jina-v2-base-code-int8-structured --arm gte-small-structured
                        [--work ~/Projects/ClaudeProjects/g-mesh-eval-data/v2/work] [--files 5] [--json out.json]
"""

import argparse
import json
import sqlite3
from pathlib import Path

GATE_POINTS = 5.0


def file_of(work, corpus):
    conn = sqlite3.connect(f"file:{Path(work) / (corpus + '.sqlite')}?mode=ro", uri=True)
    out = dict(conn.execute("SELECT id, filePath FROM nodes"))
    conn.close()
    return out


def first_rank(ids, expected):
    for i, n in enumerate(ids, 1):
        if n in expected:
            return i
    return None


def query_outcome(row, files, F):
    expected = set(row["expected"])
    hits = [h[0] for h in row["hits"]]
    flat = first_rank(hits, expected)
    top10_files = {files.get(n) for n in hits[:10]}
    exp_files = {files.get(n) for n in expected}
    order = []
    for n in hits:
        f = files.get(n)
        if f not in order:
            order.append(f)
    kept = set(order[:F])
    staged = first_rank([n for n in hits if files.get(n) in kept], expected)
    oracle = first_rank([n for n in hits if files.get(n) in exp_files], expected)
    return {"flat": flat, "staged": staged, "oracle": oracle,
            "missSameFile": bool(exp_files & top10_files) if not (flat and flat <= 10) else None,
            "hitDropped": (not (exp_files & kept)) if flat and flat <= 10 else None}


def summarise(outcomes):
    n = len(outcomes)
    r10 = lambda k: 100.0 * sum(1 for o in outcomes if o[k] and o[k] <= 10) / n if n else 0.0
    misses = [o["missSameFile"] for o in outcomes if o["missSameFile"] is not None]
    hits = [o["hitDropped"] for o in outcomes if o["hitDropped"] is not None]
    return {"queries": n, "r10Flat": r10("flat"), "r10Staged": r10("staged"), "r10Oracle": r10("oracle"),
            "missSameFile": 100.0 * sum(misses) / len(misses) if misses else None,
            "hitDroppedF": 100.0 * sum(hits) / len(hits) if hits else None}


def headroom(runs, arm, work, F):
    per, allq = {}, []
    for d in sorted((Path(runs) / arm).iterdir()):
        if not (d / "rankings.jsonl").exists():
            continue
        files = file_of(work, d.name)
        outs = []
        with open(d / "rankings.jsonl") as f:
            for line in f:
                row = json.loads(line)
                if row["expected"]:
                    outs.append(query_outcome(row, files, F))
        per[d.name] = summarise(outs)
        allq += outs
    pooled = summarise(allq)
    pooled["oracleGainPoints"] = pooled["r10Oracle"] - pooled["r10Flat"]
    pooled["fullArmWarranted"] = pooled["oracleGainPoints"] >= GATE_POINTS
    return {"pooled": pooled, "byCorpus": per}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", required=True)
    ap.add_argument("--arm", action="append", required=True)
    ap.add_argument("--work", default=str(Path.home() / "Projects/ClaudeProjects/g-mesh-eval-data/v2/work"))
    ap.add_argument("--files", type=int, default=5)
    ap.add_argument("--json")
    a = ap.parse_args()
    res = {arm: headroom(a.runs, arm, a.work, a.files) for arm in a.arm}
    for arm, r in res.items():
        p = r["pooled"]
        print(f"{arm}: r@10 flat {p['r10Flat']:.1f}, top-{a.files}-files {p['r10Staged']:.1f}, file oracle {p['r10Oracle']:.1f} "
              f"(gain {p['oracleGainPoints']:+.1f}, gate {GATE_POINTS}: {'build' if p['fullArmWarranted'] else 'stop'}); "
              f"misses with the file in the top 10: {p['missSameFile']:.1f}%; hits a top-{a.files} file stage drops: {p['hitDroppedF']:.1f}%")
    if a.json:
        Path(a.json).write_text(json.dumps(res, indent=1) + "\n")


if __name__ == "__main__":
    main()
