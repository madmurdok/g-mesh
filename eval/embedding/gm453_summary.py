#!/usr/bin/env python3
"""GM-453 helpers for gm453_measure.sh (docs/design/GM-453-onnx-tuning.md §5.3, §7).

  gate <out dir>      "go <reason>" or "stop <reason>": the §10.2 phase-A gate;
                      exits non-zero when OUT holds no phase-A timing
  best <out dir>      the two fastest adoptable phase-A arms, space-separated
  summary <out dir>   summary.md: per-run rows, medians and x baseline per arm,
                      quality per arm, probe latency
"""
import csv
import json
import statistics
import sys
from pathlib import Path

INT8 = "jina-v2-base-code-int8-structured"
FP32 = "jina-v2-base-code-fp32-r"
BASELINE = {"A": "A0", "B": "A0", "C": "C0", "D": "P0"}
BATCHING_ARMS = {"A9", "A10", "A11", "A12"}
GAIN = 0.05  # §10.2: a thread or opt arm must save more than 5%
K = 10
QUERIES = Path(__file__).resolve().parent / "queries"


def runs(out):
    """Timed runs, the last attempt per (phase, round, arm)."""
    tsv = Path(out) / "timing.tsv"
    if not tsv.exists():
        return []
    last = {}
    with open(tsv) as f:
        for row in csv.DictReader(f, delimiter="\t"):
            last[(row["phase"], row["round"], row["arm"])] = row
    return list(last.values())


def num(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def parity(out):
    """(phase, round, arm) -> merged compare result over its corpora."""
    merged = {}
    for path in sorted((Path(out) / "parity").glob("*.json")):
        parts = path.stem.split("-", 3)
        if len(parts) < 4 or not parts[1].startswith("r"):
            continue
        phase, rnd, arm = parts[0], parts[1][1:], parts[2]
        d = json.loads(path.read_text())
        m = merged.setdefault((phase, rnd, arm), {"compared": 0, "bit": 0, "maxL2": 0.0, "minCos": 1.0, "over": 0})
        m["compared"] += d["compared"]
        m["bit"] += d["bitIdentical"]
        # A NaN distance is written as null: it counts as the worst value.
        max_l2 = float("inf") if d["maxL2"] is None else d["maxL2"]
        min_cos = float("-inf") if d["minCos"] is None else d["minCos"]
        m["maxL2"] = max(m["maxL2"], max_l2)
        m["minCos"] = min(m["minCos"], min_cos)
        m["over"] += d["overTolerance"] + (1 if d["maxL2"] is None and not d["overTolerance"] else 0)
    return merged


def arm_passes(par, phase, arm):
    """True when every recorded comparison of the arm passes; the baseline arm's round 1 has none."""
    results = [v for (p, _, a), v in par.items() if p == phase and a == arm]
    return all(v["over"] == 0 for v in results)


def medians(rows, phase, key="embed_ms"):
    by_arm = {}
    for r in rows:
        if r["phase"] == phase and num(r[key]) is not None:
            by_arm.setdefault(r["arm"], []).append(num(r[key]))
    return {arm: statistics.median(v) for arm, v in by_arm.items()}


def gate(out):
    rows, par = runs(out), parity(out)
    med = medians(rows, "A")
    base = med.get("A0")
    if not base:
        sys.exit(f"no phase-A A0 timing in {out}/timing.tsv: run phase A into this OUT first, or set B_ARMS")
    batching_ok = [a for a in sorted(BATCHING_ARMS) if a in med and arm_passes(par, "A", a)]
    gainers = [
        a for a, m in med.items()
        if a not in BATCHING_ARMS and a != "A0" and arm_passes(par, "A", a) and m <= (1 - GAIN) * base
    ]
    if not batching_ok and not gainers:
        print("stop every batching arm fails parity and no thread/opt arm gains more than 5%")
    else:
        print(f"go adoptable batching arms {batching_ok or '-'}; thread/opt arms over 5%: {sorted(gainers) or '-'}")


def best(out):
    rows, par = runs(out), parity(out)
    med = medians(rows, "A")
    base = med.get("A0", float("inf"))
    ranked = sorted(
        (m, a) for a, m in med.items() if a != "A0" and arm_passes(par, "A", a) and m < base
    )
    print(" ".join(a for _, a in ranked[:2]))


def run_dirs(out, phase, arm):
    return sorted((Path(out) / phase / "r1" / arm).glob("*/*/"))


def scored_queries(corpus):
    """Ids of the corpus's authored positive queries: the set the harness's
    `report` computes recall@k and MRR over (metrics::is_scored_positive)."""
    path = QUERIES / f"{corpus}.jsonl"
    ids = set()
    for line in path.read_text().splitlines():
        if line.strip():
            q = json.loads(line)
            if q.get("kind") == "positive":
                ids.add(q["id"])
    return ids


def quality(run_dir):
    """recall@10 and MRR as `report` defines them (metrics.rs): over the
    authored positives, recall@10 = share whose first expected node ranks
    <= 10, MRR = mean of 1/first expected rank over the kept hits. One
    corpus is one language, so the language-pooled mean is the plain mean."""
    path = run_dir / "rankings.jsonl"
    if not path.exists():
        return None
    scored = scored_queries(run_dir.name)
    hits, rr = [], []
    for line in path.read_text().splitlines():
        q = json.loads(line)
        if q["id"] not in scored:
            continue
        expected = set(q["expected"])
        rank = next((i + 1 for i, hit in enumerate(q["hits"]) if hit[0] in expected), None)
        hits.append(1.0 if rank is not None and rank <= K else 0.0)
        rr.append(1 / rank if rank is not None else 0.0)
    if not hits:
        return None
    return statistics.mean(hits), statistics.mean(rr), len(hits)


def probe(path):
    ms = sorted(json.loads(line)["ms"] for line in path.read_text().splitlines() if line.strip())
    if not ms:
        return None
    p95 = ms[min(len(ms) - 1, int(round(0.95 * (len(ms) - 1))))]
    return statistics.median(ms), p95, ms[-1], len(ms)


def fmt(value, spec):
    return "-" if value is None else format(value, spec)


def summary(out):
    out = Path(out)
    rows, par = runs(out), parity(out)
    print("# GM-453 measurement summary\n")
    print("## Runs\n")
    print("| phase | arm | round | embedNodesMs | emb/s | real | user | sys | user/real | maxRSS MB "
          "| load1 before/after | parity | min cos | max L2 | bit-identical % | settings |")
    print("|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|---:|---:|---:|---|")
    for r in sorted(rows, key=lambda r: (r["phase"], int(r["round"]), r["arm"])):
        emb_s = None
        nodes = 0
        for timings in (out / r["phase"] / f"r{r['round']}" / r["arm"]).glob("*/*/timings-embed.json"):
            nodes += json.loads(timings.read_text())["nodeCount"]
        if nodes and num(r["embed_ms"]):
            emb_s = nodes / (num(r["embed_ms"]) / 1000)
        real, user = num(r["real"]), num(r["user"])
        p = par.get((r["phase"], r["round"], r["arm"]))
        verdict = "base" if p is None else ("pass" if p["over"] == 0 else f"FAIL ({p['over']})")
        rss = num(r["maxrss"])
        print(f"| {r['phase']} | {r['arm']} | {r['round']} | {fmt(num(r['embed_ms']), '.0f')} | {fmt(emb_s, '.1f')} "
              f"| {r['real']} | {r['user']} | {r['sys']} | {fmt(user / real if real and user else None, '.2f')} "
              f"| {fmt(rss / 2**20 if rss else None, '.0f')} | {r['load_before']} / {r['load_after']} | {verdict} "
              f"| {fmt(p and p['minCos'], '.9f')} | {fmt(p and p['maxL2'], '.2e')} "
              f"| {fmt(p and 100 * p['bit'] / p['compared'] if p and p['compared'] else None, '.1f')} "
              f"| {r['settings'] or 'default'} |")

    print("\n## Medians per arm\n")
    print("| phase | arm | median embedNodesMs | x baseline | median real | median user/real | parity (all rounds) |")
    print("|---|---|---:|---:|---:|---:|---|")
    for phase in sorted({r["phase"] for r in rows}):
        med = medians(rows, phase)
        real = medians(rows, phase, "real")
        base = med.get(BASELINE.get(phase, ""))
        for arm in sorted(med, key=lambda a: (len(a), a)):
            ratios = [num(r["user"]) / num(r["real"]) for r in rows
                      if r["phase"] == phase and r["arm"] == arm and num(r["real"]) and num(r["user"])]
            print(f"| {phase} | {arm} | {med[arm]:.0f} | {fmt(med[arm] / base if base else None, '.3f')} "
                  f"| {fmt(real.get(arm), '.1f')} | {fmt(statistics.median(ratios) if ratios else None, '.2f')} "
                  f"| {'pass' if arm_passes(par, phase, arm) else 'FAIL'} |")

    print("\n## Quality (recall@10, MRR as the harness report defines them, from rankings.jsonl)\n")
    print("| phase | arm | run | queries | recall@10 | MRR | delta recall vs baseline |")
    print("|---|---|---|---:|---:|---:|---:|")
    for phase in ("B", "C"):
        arms = sorted({r["arm"] for r in rows if r["phase"] == phase}, key=lambda a: (len(a), a))
        base_q = {}
        for d in run_dirs(out, phase, BASELINE[phase]):
            base_q[(d.parent.name, d.name)] = quality(d)
        for arm in arms:
            for d in run_dirs(out, phase, arm):
                q = quality(d)
                if q is None:
                    continue
                b = base_q.get((d.parent.name, d.name))
                delta = q[0] - b[0] if b else None
                print(f"| {phase} | {arm} | {d.parent.name}/{d.name} | {q[2]} | {q[0]:.4f} | {q[1]:.4f} "
                      f"| {fmt(delta, '+.4f')} |")

    print("\n## Probe latency (phase D)\n")
    c0_real = medians(rows, "C", "real").get("C0")
    print(f"Pass wall-clock without the probe (C0 median real): {fmt(c0_real, '.1f')} s\n")
    print("| arm | queries | p50 ms | p95 ms | max ms | pass real s | pass real x C0 |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    for path in sorted((out / "D").glob("*.jsonl")):
        arm = path.stem
        p = probe(path)
        if p is None:
            continue
        real = next((num(r["real"]) for r in rows if r["phase"] == "D" and r["arm"] == arm), None)
        print(f"| {arm} | {p[3]} | {p[0]:.1f} | {p[1]:.1f} | {p[2]:.1f} | {fmt(real, '.1f')} "
              f"| {fmt(real / c0_real if real and c0_real else None, '.3f')} |")

    print("\n## Parity controls\n")
    for path in sorted((out / "parity").glob("control-*.json")):
        d = json.loads(path.read_text())
        verdict = "fails parity (expected)" if d["overTolerance"] else "PASSES parity (control did not fire)"
        print(f"- {path.stem}: {verdict}; {d['compared']} nodes, max L2 {d['maxL2']:.3e}, min cos {d['minCos']:.6f}")


def main():
    if len(sys.argv) != 3 or sys.argv[1] not in ("gate", "best", "summary"):
        sys.exit(__doc__)
    {"gate": gate, "best": best, "summary": summary}[sys.argv[1]](sys.argv[2])


if __name__ == "__main__":
    main()
