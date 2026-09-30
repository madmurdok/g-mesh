#!/usr/bin/env python3
"""GM-455 helpers for gm455_measure.sh (reuses gm423_summary / gm465_summary).

  reports    controls, costs, `report` runs, GM-434 columns, token table and
             the summary the docs are written from; everything under $OUT.

Costs (`report --costs`):
  costs-455.toml  this run's timing window only: each arm's pass time and max
                  RSS = median of its valid g-mesh embed-only invocations;
                  query latency = GM-465 round 1's int8 median for every arm
                  (queries are the same text on the same model in every arm).
  costs-465.toml  GM-465's three arms, median of GM-465 round 1 and this run's
                  valid round 2; query latency from round 1 (the only full-mode
                  round).
"""
import json
import os
import statistics
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gm423_summary as g  # noqa: E402

EVAL = Path(__file__).resolve().parent
ROOT = EVAL.parent.parent
BIN = ROOT / "target/release/g-mesh"
OUT = Path(os.environ.get("OUT", EVAL / "work/runs-gm455"))
RUNS = EVAL / "work/runs"
GM423 = EVAL / "work/runs-gm423"
GM465 = EVAL / "work/runs-gm465"
INT8 = "jina-v2-base-code-int8"
FP = "jina-v2-base-code-int8-first-paragraph"
ST = "jina-v2-base-code-int8-structured"
CTX = ["fp-ctx-none", "fp-path", "fp-parent", "fp-path-parent", "fp-path-parent-shuffled"]
REAL = ["fp-path", "fp-parent", "fp-path-parent"]
STAGE2 = os.environ.get("STAGE2", "").split()
TC = "g-mesh"


def sh(args, out):
    with open(out, "w") as f:
        subprocess.run([str(a) for a in args], stdout=f, stderr=subprocess.STDOUT, cwd=ROOT)


def tsv(path):
    return g.load_tsv(path) if Path(path).exists() else []


def vec_diff(a, b):
    va, vb = g.read_vectors(Path(a)), g.read_vectors(Path(b))
    if len(va) != len(vb):
        return f"n {len(va)}/{len(vb)} (lengths differ)"
    return f"n {len(va)} max|diff| {max((abs(x - y) for x, y in zip(va, vb)), default=0.0):.3g}"


def strip_labels(line):
    d = json.loads(line)
    d.pop("targetDoc", None)
    d.pop("pathOverlap", None)
    return d


def c0():
    """fp-ctx-none vs GM-423's stored first-paragraph run, per corpus."""
    ok = True
    lines = []
    for c in sorted(p.name for p in (OUT / "fp-ctx-none").iterdir() if (p / "rankings.jsonl").exists()):
        s, r = GM423 / FP / c, OUT / "fp-ctx-none" / c
        va, vb = g.read_vectors(s / "vectors.bin"), g.read_vectors(r / "vectors.bin")
        diff = max((abs(x - y) for x, y in zip(va, vb)), default=0.0) if len(va) == len(vb) else float("inf")
        ra = [json.loads(x) for x in (s / "rankings.jsonl").read_text().splitlines()]
        rb = [strip_labels(x) for x in (r / "rankings.jsonl").read_text().splitlines()]
        same = ra == rb
        ms, mr = (json.loads((d / "manifest.json").read_text()) for d in (s, r))
        other = {k for k in set(ms) | set(mr) if ms.get(k) != mr.get(k)} - {"variant", "variantFingerprint"}
        ok &= diff == 0 and same and not other
        lines.append(
            f"{c}: vectors n {len(va)}/{len(vb)} max|diff| {diff:.3g}; rankings identical after dropping "
            f"targetDoc/pathOverlap {same}; fingerprint equal {ms['variantFingerprint'] == mr['variantFingerprint']}; "
            f"other manifest keys differing {sorted(other) or 'none'}"
        )
    return lines + [f"C0 {'PASS' if ok else 'FAILS'}"]


LOAD_MAX = float(os.environ.get("LOAD_MAX", 20))


def is_valid(r):
    """gm455_measure.sh's rule, recomputed so rows logged under its first
    rule (after-state throttling = invalid) are judged the same way: started
    from the open gate (pmset 100/100) and 1-min load <= LOAD_MAX in the run."""
    return r["rc"] == "0" and r["therm_before"] == "100/100" and float(r["load1_max_run"]) <= LOAD_MAX


def valid_rows(rows, arm):
    return [r for r in rows if r["arm"] == arm and is_valid(r) and r["corpus"] == TC]


def med(rows, key):
    return statistics.median(float(r[key]) for r in rows)


def write_costs(rows):
    model_bytes = sum(f[2] for f in json.loads((RUNS / INT8 / TC / "manifest.json").read_text())["modelFiles"])
    r465 = [r for r in tsv(GM465 / "timing.tsv") if r["round"] == "1"]
    q_int8 = float(next(r for r in r465 if r["arm"] == INT8)["query_ms_median"])
    out = []
    for arm in [INT8, FP, ST] + CTX + STAGE2:
        v = valid_rows(rows, arm)
        if not v:
            continue
        out.append(
            f'[[variant]]\nname = "{arm}"\npass_seconds = {med(v, "embed_s")}\nmax_rss_bytes = {med(v, "maxrss")}\n'
            f"model_bytes = {model_bytes}\nquery_latency_ms = {q_int8}\n"
        )
    (OUT / "costs-455.toml").write_text("\n".join(out))
    out = []
    for arm in [INT8, FP, ST]:
        r1 = next(r for r in r465 if r["arm"] == arm)
        v = [r1] + valid_rows(rows, arm)
        out.append(
            f'[[variant]]\nname = "{arm}"\npass_seconds = {med(v, "embed_s")}\nmax_rss_bytes = {med(v, "maxrss")}\n'
            f"model_bytes = {model_bytes}\nquery_latency_ms = {float(r1['query_ms_median'])}\n"
        )
    (OUT / "costs-465.toml").write_text("\n".join(out))


def report(name, reference, runs, costs=None):
    args = [BIN, "debug-embed-eval", "report", "--eval-dir", EVAL, "--reference", reference,
            "--json", OUT / f"{name}.json"]
    if costs and (OUT / costs).exists() and (OUT / costs).stat().st_size:
        args += ["--costs", OUT / costs]
    sh(args + [r for r in runs if Path(r).exists()], OUT / f"{name}.txt")


def fmt_b(b):
    return f"{b['point']:+.4f} [{b['lower']:+.4f}, {b['upper']:+.4f}]" if b else "-"


def digest(name):
    lines = [f"\n## {name}\n"]
    try:
        rep = json.loads((OUT / f"{name}.json").read_text())
    except (OSError, ValueError) as e:
        return lines + [f"no json ({e}); text:\n" + (OUT / f"{name}.txt").read_text()]
    lines.append(f"validity errors: {rep['validity']['errors']}")
    for arm, e in rep["arms"].items():
        if "gates" not in e:
            continue
        lines.append(f"\n### {arm}: {e['verdict']}")
        lines += [f"- {x['id']} {'pass' if x['passed'] else 'FAIL'}: {x['detail']}" for x in e["gates"]]
        fl = e.get("floors", {})
        lines.append(f"- floors: {json.dumps(fl.get('floors', fl))}")
        for split, d in e.get("splitDeltas", {}).items():
            lines.append(f"- split {split} n={d['n']}: recall@10 {fmt_b(d['recall@10Delta'])}, MRR {fmt_b(d['mrrDelta'])}")
    return lines


def timing_table(rows):
    lines = ["| stage | round | arm | rc | embed s | real | user | sys | user/real | RSS MiB | load before | load after | therm before -> after (min in run) | max load1 in run | valid |",
             "|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---|---|---:|---|"]
    for r in rows:
        ur = float(r["user"]) / float(r["real"]) if r["real"] else float("nan")
        lines.append(
            f"| {r['stage']} | {r['round']} | {r['arm'].replace('jina-v2-base-code-int8', 'int8')} | {r['rc']} "
            f"| {float(r['embed_s']):.1f} | {r['real']} | {r['user']} | {r['sys']} | {ur:.2f} "
            f"| {float(r['maxrss'] or 'nan') / 2**20:.0f} | {r['load_before']} | {r['load_after']} "
            f"| {r['therm_before']} -> {r['therm_after']} ({r['therm_min_run']}) | {r['load1_max_run']} | {'yes' if is_valid(r) else 'no'} |"
        )
    lines += ["", "| arm | valid runs | median embed s | vs fp-ctx-none | vs int8 (this window) | median RSS MiB |", "|---|---:|---:|---:|---:|---:|"]
    base = {a: valid_rows(rows, a) for a in ["fp-ctx-none", INT8]}
    for arm in [INT8, FP, ST] + CTX + STAGE2:
        v = valid_rows(rows, arm)
        if not v:
            continue
        m = med(v, "embed_s")
        rn = m / med(base["fp-ctx-none"], "embed_s") if base["fp-ctx-none"] else float("nan")
        ri = m / med(base[INT8], "embed_s") if base[INT8] else float("nan")
        lines.append(f"| {arm} | {len(v)} | {m:.1f} | {rn:.3f}x | {ri:.3f}x | {med(v, 'maxrss') / 2**20:.0f} |")
    return lines


def reports():
    rows = tsv(OUT / "timing.tsv")
    write_costs(rows)
    ctl = ["## C0: fp-ctx-none vs GM-423's stored first-paragraph run"] + c0()
    ctl.append("\n## Timing vectors vs quality vectors (g-mesh)")
    for r in rows:
        tag = f"t{r['stage'][1:]}-r{r['round']}-{r['arm']}"
        for d in sorted((OUT / "timing").glob(f"{tag}-a*")):
            a = d / r["arm"] / TC / "vectors.bin"
            ref = (OUT if r["arm"] in CTX + STAGE2 else GM465) / r["arm"] / TC / "vectors.bin"
            if a.exists() and ref.exists():
                ctl.append(f"{d.name} vs {ref.parent.parent.parent.name}/{r['arm']}: {vec_diff(ref, a)}")
    ctl = list(dict.fromkeys(ctl))
    (OUT / "control.txt").write_text("\n".join(ctl) + "\n")

    broken = [RUNS / "random", RUNS / "shuffled"]
    report("report-vs-none", "fp-ctx-none", [OUT / a for a in CTX] + broken, "costs-455.toml")
    report("report-c1", "fp-path-parent-shuffled",
           [OUT / "fp-path-parent-shuffled", OUT / "fp-path-parent"] + broken)
    report("report-vs-int8", INT8, [RUNS / INT8, RUNS / "jina-v2-base-code-fp32"] + broken
           + [GM423 / FP] + [OUT / a for a in CTX + STAGE2], "costs-455.toml")
    if STAGE2:
        report("report-stage2-vs-structured", ST, [GM465 / ST] + broken + [OUT / a for a in STAGE2])
        report("report-stage2-vs-int8-full", INT8, [RUNS / INT8] + broken + [OUT / a for a in STAGE2])
    report("report-465-vs-int8", INT8, [RUNS / INT8, RUNS / "jina-v2-base-code-fp32"] + broken
           + [GM423 / FP, GM465 / ST], "costs-465.toml")
    for a in CTX + STAGE2:
        if (OUT / a).exists():
            sh([sys.executable, EVAL / "shipped_floor_rates.py", "--run", OUT / a, "--floors", "shipped-int8"],
               OUT / f"gm434-{a}.txt")
    variants = ["fp-ctx-none:first-paragraph:none", "fp-path:first-paragraph:path",
                "fp-parent:first-paragraph:parent", "fp-path-parent:first-paragraph:path-parent"]
    variants += [v for v in os.environ.get("STAGE2_TOKENS", "").split()]
    sh([sys.executable, EVAL / "gm455_token_lengths.py", "--out", OUT, "--dump", OUT / "dump",
        "--variants", ",".join(variants), "--json", OUT / "token_lengths.json"], OUT / "token_lengths.md")

    s = ["# GM-455 / GM-465 summary\n", "## Timing (g-mesh, embed-only)\n"] + timing_table(rows)
    s += ["\n## Controls\n", (OUT / "control.txt").read_text()]
    s += ["\n## Token lengths\n", (OUT / "token_lengths.md").read_text()]
    names = ["report-vs-none", "report-c1", "report-vs-int8", "report-465-vs-int8"]
    if STAGE2:
        names += ["report-stage2-vs-structured", "report-stage2-vs-int8-full"]
    for n in names:
        s += digest(n)
    s.append("\n## GM-434 columns (held-out NL, shipped int8 floors)\n")
    for f in sorted(OUT.glob("gm434-*.txt")):
        s.append(f"### {f.stem[7:]}")
        s += [l for l in f.read_text().splitlines()
              if l.startswith("| NL held-out |") or l.startswith("| name (mechanical) | all")]
    if (OUT / "churn.md").exists():
        s += ["\n## Churn (g-mesh)\n", (OUT / "churn.md").read_text()]
    (OUT / "summary.md").write_text("\n".join(s) + "\n")


if __name__ == "__main__":
    {"reports": reports}[sys.argv[1]]()
