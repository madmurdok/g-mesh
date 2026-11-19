#!/usr/bin/env python3
"""GM-457 job TOMLs for the GM-730 routes (docs/design/GM-730-gpu-training-routes.md,
sections 1, 5 and 7: D7a, one job per stage, every input a `[[data]]` set).

Jobs (eval/gpu/jobs/gm730-<job>.toml):
    rr-teacher            score_teacher.py over the train + dev windows and the C-T pairs
    rr-grid, rr-final     train_ce.py: 8-config dev grid; frozen config + shuffled-teacher control
    jina-grid, jina-final train_encoder.py --model jina: 4 full + 4 LoRA; frozen config + shuffled control
    gte-grid, gte-final   train_encoder.py --model gte-small: 6 configs; frozen config + shuffled control
A grid job has one step per config, named as `grid(route)` names it; a final
job's steps are `final` and `shuffled`, with the args frozen in
final_configs.json (freeze.py). `max_minutes` is the section 5 table and is
shared by all attempts of a job (`max_minutes_scope = "job"`: a relaunch gets
what the earlier attempts left). Every step carries `--job-steps` (this step
and the ones after it) and `--eta-limit-minutes` (2x the job's section 5
estimate), so the S2 probe stops the job when its ETA is over the limit.
Without the teacher (C-T failed), `--no-teacher` keeps the alpha = 0 configs
(route gte, which has none, gets one per learning rate) and drops the teacher
data set; those jobs are `gm730-<route>-grid-nt` / `-final-nt`.
Before launching any job: `budget.py check --job <toml>` (S3 and route caps).

usage: make_jobs.py teacher [--out-dir DIR]
       make_jobs.py grid --route rr|jina|gte [--no-teacher] [--out-dir DIR]
       make_jobs.py final --route rr|jina|gte --final final_configs.json [--no-teacher] [--out-dir DIR]
"""

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
JOBS = REPO / "eval" / "gpu" / "jobs"
REL = "eval/training/gm730"
REQUIREMENTS = f"{REL}/requirements-linux-cu121.txt"
SEED = "730"
CHECKPOINT_MINUTES = 10
GRACE_SECONDS = 30
MAX_ATTEMPTS = 5
MAX_MINUTES = {"rr-teacher": 60, "rr-grid": 190, "rr-final": 60, "jina-grid": 240, "jina-final": 180,
               "gte-grid": 60, "gte-final": 60}
# S2: 2x the section 5 estimate (instance-hours) of each job, in minutes; rr-grid is
# the measured bf16 job ETA (156 min) plus margin
ETA_LIMIT_MINUTES = {"rr-teacher": 48, "rr-grid": 170, "rr-final": 48, "jina-grid": 216, "jina-final": 180,
                     "gte-grid": 60, "gte-final": 48}
NO_TEACHER_SUFFIX = "-nt"
ROUTES = ("rr", "jina", "gte")
ENTRY = {"rr": f"{REL}/train_ce.py", "jina": f"{REL}/train_encoder.py", "gte": f"{REL}/train_encoder.py"}
COMMON = {
    "rr": ["--epochs", "2", "--batch", "32", "--max-len", "256", "--bf16", "--seed", SEED],
    "jina": ["--model", "jina", "--epochs", "2", "--batch", "32", "--docs-per-query", "8",
             "--max-len-doc", "256", "--max-len-query", "64", "--bf16", "--seed", SEED],
    "gte": ["--model", "gte-small", "--epochs", "3", "--batch", "64", "--docs-per-query", "8",
            "--max-len-doc", "256", "--max-len-query", "64", "--bf16", "--seed", SEED],
}
LORA_TARGETS = "query,key,value,dense,up_gated_layer,down_layer"  # the six Linear names of JinaBert (M9)
CONTROL = {"rr": "shuffled-teacher", "jina": "shuffled", "gte": "shuffled"}
SNAPSHOT = {"rr-teacher": "gte-mb", "rr": "l6", "jina": "jina", "gte": "gte-small"}
D = "~/Projects/ClaudeProjects/g-mesh-data"


def grid(route, teacher=True):
    """[(config name, args)] of the route's pre-registered dev grid (section 1)."""
    if route == "rr":
        out = [(f"lr{lr}-tt{tt}-a{a}", ["--lr", lr, "--tau-t", tt, "--alpha", a])
               for lr in ("2e-5", "5e-5") for tt in ("1", "2") for a in ("0", "0.5")]
    elif route == "jina":
        out = [(f"full-lr{lr}-a{a}", ["--mode", "full", "--lr", lr, "--tau", "0.05", "--alpha", a])
               for lr in ("1e-5", "2e-5") for a in ("0", "0.5")]
        out += [(f"lora-r{r}-a{a}", ["--mode", "lora", "--rank", r, "--lora-targets", LORA_TARGETS,
                                     "--lr", "2e-4", "--tau", "0.05", "--alpha", a])
                for r in ("16", "32") for a in ("0", "0.5")]
    elif route == "gte":
        out = [(f"lr{lr}-a{a}", ["--lr", lr, "--tau", "0.05", "--alpha", a])
               for lr in ("2e-5", "5e-5", "1e-4") for a in ("0.5", "1.0")]
    else:
        raise SystemExit(f"unknown route {route}")
    if not teacher and route == "rr":
        raise SystemExit("route rr distils the teacher; it has no form without it")
    if not teacher and route == "gte":
        out = [(f"lr{lr}-a0", ["--lr", lr, "--tau", "0.05", "--alpha", "0"]) for lr in ("2e-5", "5e-5", "1e-4")]
    if not teacher:
        out = [(n, a) for n, a in out if a[a.index("--alpha") + 1] == "0"]
        if not out:
            raise SystemExit(f"route {route}: no alpha = 0 config, so it cannot run without the teacher")
    return out


def hf_set(snapshot):
    sys.path.insert(0, str(HERE))
    import snapshots as S
    repo, rev, _, _ = S.PINS[snapshot]
    d = f"{repo.split('/')[-1]}@{rev}"
    return {"name": f"gm730-hf-{snapshot}", "env": "GM730_HF", "local_root": f"{D}/gm730/hf", "include": [f"{d}/*"]}


def data_sets(job, route, teacher=True):
    """The `[[data]]` entries a job reads."""
    windows = {"name": "gm730-windows", "env": "GM730_WINDOWS", "local_root": f"{D}/gm730/windows",
               "include": ["train/*", "dev/*"]}
    out = [windows]
    if job == "rr-teacher":
        out += [{"name": "gm730-ct-pairs", "env": "GM730_CT", "local_root": f"{D}/gm730/ct", "include": ["*"]},
                hf_set(SNAPSHOT["rr-teacher"])]
        return out
    if route in ("jina", "gte"):
        out.append({"name": "gm456-v0.4-full-gm730", "env": "GM456_DATA", "local_root": f"{D}/gm456",
                    "include": ["v0.4-full/queries.jsonl", "v0.4-full/manifest.json", "v0.4-full/targets.jsonl"]})
    if teacher:
        out.append({"name": "gm730-teacher", "env": "GM730_TEACHER", "local_root": f"{D}/gm730/teacher", "include": ["*"]})
    out.append(hf_set(SNAPSHOT[route]))
    return out


def q(s):
    return json.dumps(s)


def job_name(job, teacher=True):
    return job if teacher else job + NO_TEACHER_SUFFIX


def with_job_args(job, steps):
    """Each step's args plus --job-steps (this step and the ones after it) and the job's
    --eta-limit-minutes."""
    n = len(steps)
    return [(name, entry, list(args) + ["--job-steps", str(n - i), "--eta-limit-minutes", str(ETA_LIMIT_MINUTES[job])])
            for i, (name, entry, args) in enumerate(steps)]


def render(job, header, steps, data, teacher=True):
    """GM-457 job TOML text; `job` is the section 5 key (rr-grid, ...)."""
    lines = [f"# {line}" if line else "#" for line in header] + [
        f"name = {q('gm730-' + job_name(job, teacher))}",
        f"requirements = {q(REQUIREMENTS)}",
        "smoke = true",
        f"max_minutes = {MAX_MINUTES[job]}",
        'max_minutes_scope = "job"',
        f"checkpoint_minutes = {CHECKPOINT_MINUTES}",
        f"grace_seconds = {GRACE_SECONDS}",
        f"max_attempts = {MAX_ATTEMPTS}",
    ]
    for name, entry, args in with_job_args(job, steps):
        lines += ["", "[[steps]]", f"name = {q(name)}", f"entry = {q(entry)}",
                  "args = [" + ", ".join(q(a) for a in args) + "]", 'device = "cuda"']
    for ds in data:
        lines += ["", "[[data]]", f"name = {q(ds['name'])}", f"env = {q(ds['env'])}", f"local_root = {q(ds['local_root'])}",
                  "include = [" + ", ".join(q(i) for i in ds["include"]) + "]"]
    return "\n".join(lines) + "\n"


def teacher_job():
    steps = [("teacher", f"{REL}/score_teacher.py", ["--seed", SEED])]
    return render("rr-teacher", ["GM-730 job RR-T: gte-mb fp32 logits for every train and dev window pair,",
                                 "plus the C-T parity pairs (written by make_jobs.py)."],
                  steps, data_sets("rr-teacher", "rr"))


def grid_job(route, teacher=True):
    steps = [(name, ENTRY[route], ["--stage", "grid"] + COMMON[route] + args) for name, args in grid(route, teacher)]
    hdr = [f"GM-730 job {route}-grid: the pre-registered dev grid, one step per config",
           f"({'with' if teacher else 'without'} the RR-T teacher; written by make_jobs.py)."]
    return render(f"{route}-grid", hdr, steps, data_sets(f"{route}-grid", route, teacher), teacher)


def final_job(route, final_configs, teacher=True):
    import hashlib
    final_configs = Path(final_configs)
    sidecar = final_configs.with_suffix(".sha256")
    sha = hashlib.sha256(final_configs.read_bytes()).hexdigest()
    if not sidecar.exists() or sidecar.read_text().split()[0] != sha:
        raise SystemExit(f"{final_configs}: sha256 {sha} is not the one freeze.py wrote to {sidecar}")
    fc = json.loads(final_configs.read_text())
    if route not in fc:
        raise SystemExit(f"{final_configs}: no frozen config for route {route}; run freeze.py first")
    entry = fc[route]
    if not entry.get("s4", {}).get("pass"):
        raise SystemExit(f"{final_configs}: route {route} failed its S4 dev gate; no final job")
    args = list(entry["args"])
    steps = [("final", ENTRY[route], ["--stage", "final"] + COMMON[route] + args),
             ("shuffled", ENTRY[route], ["--stage", "final", "--control", CONTROL[route]] + COMMON[route] + args)]
    hdr = [f"GM-730 job {route}-final: frozen config {entry['name']} and its {CONTROL[route]} control,",
           "each exported, quantised and parity-checked at the end of its step",
           f"(final_configs.json sha256 {sha}; written by make_jobs.py)."]
    return render(f"{route}-final", hdr, steps, data_sets(f"{route}-final", route, teacher), teacher)


def write(job, text, out_dir):
    """Write the TOML and check it loads as a GM-457 job."""
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"gm730-{job}.toml"
    path.write_text(text)
    sys.path.insert(0, str(REPO / "eval" / "gpu"))
    import gpu_train
    j = gpu_train.load_job(path)
    print(f"{path}: {len(j['steps'])} steps, {len(j['data'])} data sets, max_minutes {j['max_minutes']}")
    return path


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("teacher")
    g = sub.add_parser("grid")
    f = sub.add_parser("final")
    for p in (g, f):
        p.add_argument("--route", required=True, choices=ROUTES)
        p.add_argument("--no-teacher", action="store_true")
    f.add_argument("--final", required=True, type=Path)
    for p in (t, g, f):
        p.add_argument("--out-dir", type=Path, default=JOBS)
    a = ap.parse_args(argv)
    if a.cmd == "teacher":
        write("rr-teacher", teacher_job(), a.out_dir)
    elif a.cmd == "grid":
        write(job_name(f"{a.route}-grid", not a.no_teacher), grid_job(a.route, not a.no_teacher), a.out_dir)
    else:
        write(job_name(f"{a.route}-final", not a.no_teacher), final_job(a.route, a.final, not a.no_teacher), a.out_dir)
    return 0


if __name__ == "__main__":
    sys.exit(main())
