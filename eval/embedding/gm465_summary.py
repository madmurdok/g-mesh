#!/usr/bin/env python3
"""GM-465 helpers for gm465_measure.sh (reuses gm423_summary.py).

  vectors <a.bin> <b.bin>                 max |diff| between two vector files
  costs <timing.tsv> <manifest> [corpus]  costs.toml for `report --costs` (timed corpus)
  summary <out dir>                       the compact summary the doc is written from
"""
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gm423_summary as g  # noqa: E402

ARMS = [
    "jina-v2-base-code-int8",
    "jina-v2-base-code-int8-first-paragraph",
    "jina-v2-base-code-int8-structured",
]
g.ARMS = ARMS


def vectors(a, b):
    va, vb = g.read_vectors(Path(a)), g.read_vectors(Path(b))
    diff = max((abs(x - y) for x, y in zip(va, vb)), default=0.0) if len(va) == len(vb) else float("inf")
    print(f"{Path(b).parent.parent.name}/{Path(b).parent.name}: n {len(va)}/{len(vb)} max|diff| {diff:.3g}")


def costs(tsv, manifest, corpus="g-mesh"):
    """gm423_summary.costs over the rounds in COST_ROUNDS only (default 1,2).

    GM-465's round 2 was discarded (thermal throttling), so its costs.toml is
    built with COST_ROUNDS=1; the discarded rows stay in timing.tsv.
    """
    import os
    import tempfile

    keep = set(os.environ.get("COST_ROUNDS", "1,2").split(","))
    lines = Path(tsv).read_text().splitlines()
    kept = [lines[0]] + [l for l in lines[1:] if l.split("\t")[0] in keep]
    with tempfile.NamedTemporaryFile("w", suffix=".tsv", delete=False) as f:
        f.write("\n".join(kept) + "\n")
    g.costs(f.name, manifest, corpus)


def summary(out):
    out = Path(out)
    rows = g.load_tsv(out / "timing.tsv")
    timed = [r for r in rows if r["round"] in ("1", "2")]
    print("## Timed corpus (round 1 full, round 2 embed-only, reversed)\n")
    print("| arm | r1 embed s | r2 embed s | mean vs int8 | r1 vs int8 | r2 vs int8 | RSS MiB r1/r2 | query ms (r1) |")
    print("|---|---:|---:|---:|---:|---:|---|---:|")
    base = {}
    for arm in ARMS:
        e = {r["round"]: r for r in timed if r["arm"] == arm}
        if not e:
            continue
        r1, r2 = float(e["1"]["embed_s"]), float(e["2"]["embed_s"]) if "2" in e else float("nan")
        base.setdefault("r1", r1)
        base.setdefault("r2", r2)
        print(
            f"| {arm} | {r1:.1f} | {r2:.1f} | {(r1 + r2) / (base['r1'] + base['r2']):.3f}x | {r1 / base['r1']:.3f}x "
            f"| {r2 / base['r2']:.3f}x | {float(e['1']['maxrss']) / 2**20:.0f}/"
            f"{float(e['2']['maxrss']) / 2**20 if '2' in e else float('nan'):.0f} | {float(e['1']['query_ms_median']):.2f} |"
        )
    print("\n### Per invocation\n")
    print("| round | corpus | arm | rc | embed s | real | user | sys | user/real | RSS MiB | load before | load after |")
    print("|---|---|---|---|---:|---:|---:|---:|---:|---:|---|---|")
    for r in rows:
        ur = float(r["user"]) / float(r["real"]) if r["real"] else float("nan")
        print(
            f"| {r['round']} | {r['corpus']} | {r['arm'].replace('jina-v2-base-code-int8', 'int8')} | {r['rc']} "
            f"| {float(r['embed_s']):.1f} | {r['real']} | {r['user']} | {r['sys']} | {ur:.2f} "
            f"| {float(r['maxrss'] or 'nan') / 2**20:.0f} | {r['load_before']} | {r['load_after']} |"
        )
    print("\n## Controls\n")
    print((out / "control.txt").read_text())
    print("\n## Token lengths\n")
    print((out / "token_lengths.md").read_text())
    for name in ("report-vs-int8", "report-vs-fp32"):
        print(f"\n## {name}\n")
        try:
            rep = json.loads((out / f"{name}.json").read_text())
        except (OSError, ValueError) as e:
            print(f"no json ({e}); text:\n" + (out / f"{name}.txt").read_text())
            continue
        print("validity errors:", rep["validity"]["errors"])
        for arm, e in rep["arms"].items():
            if "gates" not in e:
                continue
            print(f"\n### {arm}: {e['verdict']}")
            for gate in e["gates"]:
                print(f"- {gate['id']} {'pass' if gate['passed'] else 'FAIL'}: {gate['detail']}")
            fl = e.get("floors", {})
            print(f"- floors: {json.dumps(fl.get('floors', fl))}")
    print("\n## GM-434 columns (held-out NL, shipped int8 floors)\n")
    for f in sorted(out.glob("gm434-*.txt")):
        lines = [
            l for l in f.read_text().splitlines()
            if l.startswith("| NL held-out |") or l.startswith("| name (mechanical) | all")
        ]
        print(f"### {f.stem[7:]}")
        print("\n".join(lines))


if __name__ == "__main__":
    {"vectors": vectors, "costs": costs, "summary": summary}[sys.argv[1]](*sys.argv[2:])
