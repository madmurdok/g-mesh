#!/usr/bin/env python3
"""GM-460 section 8: cut an older query set out of a v2 run dir.

Writes, for every <arm>/<corpus> of a v2 runs dir, a subset run dir that holds
only the rankings rows whose ids are in the given eval dir's query files (the
authored file and the mechanical file), in the v2 file's order. Its manifest's
`queryIds` and `queryFiles` are rewritten to that eval dir's files and their
sha256, so the existing scripts (q5_floor_sensitivity.py, recall_at_k.py,
int8_confirm_report.py, shipped_floor_rates.py, `debug-embed-eval report`) run
on it unchanged against that older eval dir. vectors.bin is symlinked.

usage: gm460_subset.py --runs <v2 runs dir> --eval-dir <older eval dir> --out <dir>
                       [--arm NAME ...]   (default: every arm under --runs)
"""
import argparse
import hashlib
import json
import sys
from pathlib import Path


def ids_of(path):
    return [json.loads(l)["id"] for l in path.read_text().splitlines() if l.strip()]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", required=True, type=Path)
    ap.add_argument("--eval-dir", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--arm", action="append")
    a = ap.parse_args()
    arms = a.arm or sorted(p.name for p in a.runs.iterdir() if p.is_dir())
    for arm in arms:
        for d in sorted(p for p in (a.runs / arm).iterdir() if (p / "rankings.jsonl").exists()):
            man = json.loads((d / "manifest.json").read_text())
            c = man["corpus"]
            files = [f"queries/{c}.jsonl", f"queries/mechanical/{c}.jsonl"]
            keep = []
            for f in files:
                keep += ids_of(a.eval_dir / f)
            keepset = set(keep)
            rows = {}
            for line in (d / "rankings.jsonl").read_text().splitlines(keepends=True):
                rid = json.loads(line)["id"]
                if rid in keepset:
                    rows[rid] = line
            missing = [i for i in keep if i not in rows]
            if missing:
                sys.exit(f"STOP: {arm}/{c} lacks {len(missing)} ids of {a.eval_dir}, e.g. {missing[:3]}")
            man["queryIds"] = keep
            man["queryFiles"] = [[f, hashlib.sha256((a.eval_dir / f).read_bytes()).hexdigest()] for f in files]
            o = a.out / arm / c
            o.mkdir(parents=True, exist_ok=True)
            (o / "rankings.jsonl").write_text("".join(rows[i] for i in keep))
            (o / "manifest.json").write_text(json.dumps(man, indent=1) + "\n")
            link = o / "vectors.bin"
            link.unlink(missing_ok=True)
            link.symlink_to((d / "vectors.bin").resolve())
            print(f"{arm}/{c}: {len(keep)} rows")


if __name__ == "__main__":
    main()
