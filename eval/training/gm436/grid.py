#!/usr/bin/env python3
"""The pre-registered GM-436 dev grid and its selection (docs/design/GM-436-laptop-training.md,
D9a and section 7). Everything is chosen on the GM-456 dev split; the chosen
configs go to final_configs.json, whose sha256 run_measure.sh freezes before
any v2 run.

  grid.py r1                         -> lines "<name>\\t<train_adapter args>" (full 6, low-rank 4, Procrustes 1)
  grid.py r2                         -> lines for the R2 alpha grid ({0, 0.5}); needs --form
  grid.py select --grid DIR --out final_configs.json [--r2-grid DIR]
"""

import argparse
import json
import sys
from pathlib import Path

TAUS = [0.02, 0.05]
LAMS = [0.0, 1e-3, 1e-2]
RANKS = [32, 128]
ALPHAS = [0.0, 0.5]


def r1_grid():
    out = [(f"full-tau{t}-lam{l}", ["--form", "full", "--tau", str(t), "--lam", str(l)]) for t in TAUS for l in LAMS]
    out += [(f"lowrank-tau{t}-r{r}", ["--form", "lowrank", "--tau", str(t), "--rank", str(r)]) for t in TAUS for r in RANKS]
    out.append(("procrustes", ["--form", "procrustes"]))
    return out


def r2_grid(form_args):
    return [(f"r2-alpha{a}", ["--route", "r2", "--alpha", str(a)] + form_args) for a in ALPHAS]


def args_of(name, grid):
    return dict(grid)[name]


def best(results):
    """Highest dev MRR; ties to the name that sorts first."""
    return sorted(results, key=lambda r: (-r["mrr"], r["name"]))[0]


def collect(gdir):
    out = []
    for d in sorted(Path(gdir).iterdir()):
        rj = d / "result.json"
        if rj.exists():
            r = json.loads(rj.read_text())
            out.append({"name": d.name, "mrr": r["dev"]["mrr"], "run": str(d), "wSha256": r["wSha256"]})
    return out


def select(grid_root, r2_root=None, r2_base=None):
    """{base: {form: best run}} from <grid_root>/<base>/<name>/result.json; plus R2's alpha."""
    grid = dict(r1_grid())
    final = {"r1": {}}
    for bdir in sorted(p for p in Path(grid_root).iterdir() if p.is_dir()):
        runs = collect(bdir)
        missing = set(grid) - {r["name"] for r in runs}
        if missing:
            sys.exit(f"{bdir}: grid incomplete, missing {sorted(missing)}")
        per = {}
        for form in ("full", "lowrank", "procrustes"):
            b = best([r for r in runs if r["name"].startswith(form)])
            per[form] = dict(b, args=grid[b["name"]])
        trainable = best([per["full"], per["lowrank"]])
        final["r1"][bdir.name] = {"forms": per, "bestForm": best(list(per.values()))["name"].split("-")[0],
                                  "bestTrainable": trainable["name"]}
    if r2_root:
        runs = collect(r2_root)
        b = best(runs)
        alpha = json.loads((Path(b["run"]) / "result.json").read_text())["config"]["alpha"]
        form = final["r1"][r2_base]["bestTrainable"]
        final["r2"] = dict(b, base=r2_base, form=form, alpha=alpha,
                           args=["--route", "r2", "--alpha", str(alpha)] + grid[form])
    return final


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("r1")
    p2 = sub.add_parser("r2")
    p2.add_argument("--form-name", required=True, help="an R1 grid name, e.g. full-tau0.05-lam0.001")
    p3 = sub.add_parser("select")
    p3.add_argument("--grid", required=True)
    p3.add_argument("--r2-grid")
    p3.add_argument("--r2-base", default="gte-small-structured")
    p3.add_argument("--out", required=True)
    p4 = sub.add_parser("get", help="print one value of final_configs.json by a dotted path")
    p4.add_argument("--final", required=True)
    p4.add_argument("path")
    a = ap.parse_args()
    if a.cmd == "r1":
        for name, args in r1_grid():
            print(name + "\t" + " ".join(args))
    elif a.cmd == "r2":
        form_args = args_of(a.form_name, r1_grid())
        for name, args in r2_grid(form_args):
            print(name + "\t" + " ".join(args))
    elif a.cmd == "select":
        final = select(a.grid, a.r2_grid, a.r2_base)
        Path(a.out).write_text(json.dumps(final, indent=1, sort_keys=True) + "\n")
        print(a.out)
    else:
        v = json.loads(Path(a.final).read_text())
        for part in a.path.split("."):
            v = v[part]
        print(" ".join(v) if isinstance(v, list) else v)


if __name__ == "__main__":
    main()
