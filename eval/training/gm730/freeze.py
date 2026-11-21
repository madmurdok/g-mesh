#!/usr/bin/env python3
"""Freeze each GM-730 route's dev winner (docs/design/GM-730-gpu-training-routes.md,
section 0 "GM-436 D9 rule", sections 1 and 6 S4).

Reads a pulled grid run (`gpu_train.py pull`: <run>/out/<config>/result.json,
one per config of make_jobs.grid(route)), picks the highest dev MRR (ties to
the config name that sorts first; route jina picks one form, LoRA or full),
applies the route's S4 dev gate, and adds the route to final_configs.json.
A route already in final_configs.json is never replaced. The file's sha256 is
written to final_configs.sha256 ("<sha256>  final_configs.json") after every
change; make_jobs.py final checks it.

result.json, as the trainers write it at the end of a grid step:
    {"route", "name", "args": [config args],
     "best": {"epoch", "mrr", "recall@10"}, "base": {"mrr", "recall@10"},
     "gain": {"mrr", "lower", "upper"}}   (best - base, paired over dev queries, clustered by target)

S4 gates: rr: gain.lower > 0; jina: best.mrr - base.mrr >= 0.034 (the
adapter's dev gain); gte: best.mrr >= 0.41.

usage: freeze.py --route rr|jina|gte --run <pulled run dir> [--final final_configs.json] [--no-teacher]
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import make_jobs as MJ  # noqa: E402

ADAPTER_DEV_GAIN = 0.034
GTE_DEV_FLOOR = 0.41


def sha256_file(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def collect(run_dir):
    """[{name, mrr, result, path, sha256}] for every <run>/out/<step>/result.json."""
    out = []
    for rj in sorted(Path(run_dir).glob("out/*/result.json")):
        r = json.loads(rj.read_text())
        out.append({"name": rj.parent.name, "mrr": float(r["best"]["mrr"]), "result": r, "path": str(rj),
                    "sha256": sha256_file(rj)})
    return out


def best(runs):
    """Highest dev MRR; ties to the name that sorts first."""
    return sorted(runs, key=lambda r: (-r["mrr"], r["name"]))[0]


def s4(route, r):
    """{pass, rule, value} of the route's dev gate on the winner's result.json."""
    if route == "rr":
        v = float(r["gain"]["lower"])
        return {"pass": v > 0, "rule": "paired dev MRR gain lower bound > 0", "value": v}
    if route == "jina":
        v = float(r["best"]["mrr"]) - float(r["base"]["mrr"])
        return {"pass": v >= ADAPTER_DEV_GAIN, "rule": f"dev MRR gain over epoch 0 >= {ADAPTER_DEV_GAIN}", "value": v}
    v = float(r["best"]["mrr"])
    return {"pass": v >= GTE_DEV_FLOOR, "rule": f"dev MRR >= {GTE_DEV_FLOOR}", "value": v}


def select(route, run_dir, teacher=True):
    """The final_configs.json entry of `route` from a pulled grid run."""
    grid = dict(MJ.grid(route, teacher))
    runs = collect(run_dir)
    names = {r["name"] for r in runs}
    missing, extra = sorted(set(grid) - names), sorted(names - set(grid))
    if missing or extra:
        raise SystemExit(f"{run_dir}: grid of {route} incomplete or foreign: missing {missing}, extra {extra}")
    for r in runs:
        if r["result"].get("args") is not None and list(r["result"]["args"]) != grid[r["name"]]:
            raise SystemExit(f"{r['path']}: args {r['result']['args']} differ from the grid's {grid[r['name']]}")
    b = best(runs)
    res = b["result"]
    return {"name": b["name"], "args": grid[b["name"]], "best": res["best"], "base": res["base"], "gain": res["gain"],
            "run": str(run_dir), "resultSha256": b["sha256"], "teacher": teacher, "s4": s4(route, res),
            "grid": [{"name": r["name"], "mrr": r["mrr"]} for r in sorted(runs, key=lambda r: r["name"])]}


def freeze(route, run_dir, final_path, teacher=True):
    """Add the route to final_configs.json; returns (entry, file sha256)."""
    final_path = Path(final_path)
    fc = json.loads(final_path.read_text()) if final_path.exists() else {}
    if route in fc:
        raise SystemExit(f"{final_path}: route {route} is already frozen ({fc[route]['name']}); it is never replaced")
    fc[route] = select(route, run_dir, teacher)
    tmp = final_path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(fc, indent=1, sort_keys=True) + "\n")
    tmp.replace(final_path)
    sha = sha256_file(final_path)
    final_path.with_suffix(".sha256").write_text(f"{sha}  {final_path.name}\n")
    return fc[route], sha


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--route", required=True, choices=MJ.ROUTES)
    ap.add_argument("--run", required=True, type=Path, help="pulled grid run dir (holds out/<config>/result.json)")
    ap.add_argument("--final", type=Path, default=Path("final_configs.json"))
    ap.add_argument("--no-teacher", action="store_true")
    a = ap.parse_args(argv)
    entry, sha = freeze(a.route, a.run, a.final, not a.no_teacher)
    print(f"{a.route}: winner {entry['name']} dev MRR {entry['best']['mrr']:.4f}; S4 {entry['s4']['rule']}: "
          f"{entry['s4']['value']:.4f} -> {'pass' if entry['s4']['pass'] else 'STOP'}")
    print(f"{a.final} sha256 {sha}")
    return 0 if entry["s4"]["pass"] else 2


if __name__ == "__main__":
    sys.exit(main())
