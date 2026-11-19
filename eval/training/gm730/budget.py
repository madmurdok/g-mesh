#!/usr/bin/env python3
"""Pre-launch money check of a GM-730 GPU job (docs/design/GM-730-gpu-training-routes.md,
section 5 caps and section 6 S3).

    budget.py check --job eval/gpu/jobs/gm730-<job>.toml [--runs DIR] [--usd-per-hour 0.70]

Spent = the sum of `totalUsd` of every pulled GM-730 run (<runs>/gm730-*/cost.json; gpu_train.py
run writes it there after the run); a pulled GM-730 run dir without cost.json makes the spend
unknown and is refused. Worst case of the job = (max_minutes + BOOT_MINUTES per attempt) at
--usd-per-hour plus the root volume's EBS, since a job's attempts share max_minutes
(`max_minutes_scope = "job"`, required here). Refused (exit 3) when:
  - the job's route spend + its worst case > the route cap (rr $4, jina $8, gte $3);
  - the cumulative spend + its worst case > $20 (S3: no new job at $20);
  - for route gte (R3): the cumulative spend + its worst case > $15 (S3: R3 is skipped at $15).
A run of the job itself must be pulled before the next launch: this check reads the laptop only.
"""

import argparse
import json
import os
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
GPU_CONFIG = REPO / "eval" / "gpu" / "config.toml"
ROUTE_CAP = {"rr": 4.0, "jina": 8.0, "gte": 3.0}
NO_NEW_JOB = 20.0
R3_SKIP = 15.0
BOOT_MINUTES = 5  # boot, venv and data sync before the in-OS lifetime matters, per attempt
USD_PER_HOUR = 0.70  # section 5's planning price of a g5 spot hour
REFUSED = 3


def route_of(job_name):
    parts = job_name.split("-")
    if len(parts) < 3 or parts[0] != "gm730" or parts[1] not in ROUTE_CAP:
        raise SystemExit(f"{job_name}: not a GM-730 job name (gm730-<rr|jina|gte>-...)")
    return parts[1]


def spent(runs_dir):
    """({route: $}, total $, problems) over the pulled GM-730 runs."""
    by, problems = {r: 0.0 for r in ROUTE_CAP}, []
    for d in sorted(Path(runs_dir).glob("gm730-*")):
        if not d.is_dir():
            continue
        cost = d / "cost.json"
        if not cost.exists():
            problems.append(f"{d}: no cost.json (pull the run again after gpu_train.py wrote it)")
            continue
        by[route_of(d.name)] += float(json.loads(cost.read_text())["totalUsd"])
    return by, sum(by.values()), problems


def worst_usd(job, cfg, usd_per_hour):
    attempts = int(job.get("max_attempts", cfg["max_attempts"]))
    hours = (float(job["max_minutes"]) + BOOT_MINUTES * attempts) / 60
    ebs = cfg["root_volume_gb"] * cfg["ebs_gp3_usd_gb_month"] / 730
    return hours * (usd_per_hour + ebs)


def check(job_path, runs_dir, usd_per_hour, cfg):
    job = tomllib.loads(Path(job_path).read_text())
    if job.get("max_minutes_scope") != "job":
        return [f"{job_path}: max_minutes_scope is not \"job\", so max_minutes does not bound the job"], None
    route = route_of(job["name"])
    by, total, problems = spent(runs_dir)
    w = worst_usd(job, cfg, usd_per_hour)
    rules = [(f"route {route}: spent ${by[route]:.2f} + worst ${w:.2f} <= cap ${ROUTE_CAP[route]:.2f}",
              by[route] + w <= ROUTE_CAP[route]),
             (f"GM-730 total: spent ${total:.2f} + worst ${w:.2f} <= ${NO_NEW_JOB:.2f} (S3 no new job)",
              total + w <= NO_NEW_JOB)]
    if route == "gte":
        rules.append((f"R3: spent ${total:.2f} + worst ${w:.2f} <= ${R3_SKIP:.2f} (S3 R3 skipped)", total + w <= R3_SKIP))
    problems += [f"refused: {text}" for text, ok in rules if not ok]
    return problems, {"job": job["name"], "route": route, "spentByRoute": by, "spentTotal": total, "worstUsd": w,
                      "usdPerHour": usd_per_hour, "rules": [{"rule": t, "pass": ok} for t, ok in rules]}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("check")
    c.add_argument("--job", required=True, type=Path)
    c.add_argument("--runs", type=Path, help="pulled runs (default: config.toml local_runs)")
    c.add_argument("--usd-per-hour", type=float, default=USD_PER_HOUR)
    a = ap.parse_args(argv)
    cfg = tomllib.loads(GPU_CONFIG.read_text())
    runs = a.runs or Path(os.path.expanduser(cfg["local_runs"]))
    problems, info = check(a.job, runs, a.usd_per_hour, cfg)
    if info:
        for r in info["rules"]:
            print(f"{'pass' if r['pass'] else 'REFUSED'}: {r['rule']}")
    for p in problems:
        print(p)
    print(f"budget {a.job.name}: {'REFUSED' if problems else 'ok'}")
    return REFUSED if problems else 0


if __name__ == "__main__":
    sys.exit(main())
