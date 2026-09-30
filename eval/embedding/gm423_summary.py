#!/usr/bin/env python3
"""GM-423 helpers for gm423_measure.sh.

  control <stored run> <re-run>   compare the 1024 re-run with the stored int8 run
  costs <timing.tsv> <manifest> [corpus]  costs.toml for `report --costs` (g-mesh passes)
  summary <out dir>               the compact summary the doc is written from
"""
import json
import statistics
import struct
import sys
from collections import defaultdict
from pathlib import Path

ARMS = [
    "jina-v2-base-code-int8",
    "jina-v2-base-code-int8-seq512",
    "jina-v2-base-code-int8-seq256",
    "jina-v2-base-code-int8-first-paragraph",
]


def read_vectors(path: Path):
    data = path.read_bytes()
    return struct.unpack(f"<{len(data) // 4}f", data)


def control(stored: str, rerun: str):
    stored, rerun = Path(stored), Path(rerun)
    all_same = True
    for d in sorted(p for p in rerun.iterdir() if (p / "rankings.jsonl").exists()):
        s = stored / d.name
        a, b = read_vectors(s / "vectors.bin"), read_vectors(d / "vectors.bin")
        diff = max((abs(x - y) for x, y in zip(a, b)), default=0.0) if len(a) == len(b) else float("inf")
        ra = (s / "rankings.jsonl").read_text().splitlines()
        rb = (d / "rankings.jsonl").read_text().splitlines()
        orders_same = [json.loads(x)["hits"] and [h[0] for h in json.loads(x)["hits"]] for x in ra] == [
            json.loads(x)["hits"] and [h[0] for h in json.loads(x)["hits"]] for x in rb
        ]
        ms = json.loads((s / "manifest.json").read_text())
        mr = json.loads((d / "manifest.json").read_text())
        fp = ms["variantFingerprint"] == mr["variantFingerprint"]
        same = ra == rb
        all_same &= same and fp
        print(
            f"{d.name}: vectors max|diff| {diff:.3g}, rankings byte-identical {same}, "
            f"top-100 order identical {orders_same}, fingerprint equal {fp}"
        )
    print("CONTROL", "PASS" if all_same else "DIFFERS")


def load_tsv(path):
    lines = Path(path).read_text().splitlines()
    head = lines[0].split("\t")
    return [dict(zip(head, l.split("\t"))) for l in lines[1:]]


def costs(tsv, manifest, corpus="g-mesh"):
    rows = [r for r in load_tsv(tsv) if r["corpus"] == corpus]
    model_bytes = sum(f[2] for f in json.loads(Path(manifest).read_text())["modelFiles"])
    for arm in ARMS:
        mine = [r for r in rows if r["arm"] == arm]
        q = [float(r["query_ms_median"]) for r in mine if r["round"] == "1"]
        print("[[variant]]")
        print(f'name = "{arm}"')
        print(f"pass_seconds = {statistics.median(float(r['embed_s']) for r in mine)}")
        print(f"max_rss_bytes = {statistics.median(float(r['maxrss']) for r in mine)}")
        print(f"model_bytes = {model_bytes}")
        print(f"query_latency_ms = {q[0]}")
        print()


def summary(out):
    out = Path(out)
    rows = load_tsv(out / "timing.tsv")
    print("## Timing (round 1: all corpora, quality passes; round 2: g-mesh embed-only)\n")
    print("| arm | r1 embed total s | r1 g-mesh embed s | r2 g-mesh embed s | vs 1024 (total) | vs 1024 (g-mesh, mean of rounds) | r1 real sum | r1 user sum | r1 sys sum | max RSS MB (g-mesh) |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    agg = {}
    for arm in ARMS:
        r1 = [r for r in rows if r["arm"] == arm and r["round"] == "1"]
        r2 = [r for r in rows if r["arm"] == arm and r["round"] == "2"]
        tot = sum(float(r["embed_s"]) for r in r1)
        g1 = sum(float(r["embed_s"]) for r in r1 if r["corpus"] == "g-mesh")
        g2 = sum(float(r["embed_s"]) for r in r2)
        rss = max([float(r["maxrss"]) for r in r1 + r2 if r["corpus"] == "g-mesh"] or [0]) / 2**20
        agg[arm] = (tot, (g1 + g2) / 2)
        base = agg.get(ARMS[0], (tot, (g1 + g2) / 2))
        print(
            f"| {arm} | {tot:.1f} | {g1:.1f} | {g2:.1f} | {tot / base[0]:.3f}x | {(g1 + g2) / 2 / base[1]:.3f}x "
            f"| {sum(float(r['real']) for r in r1):.1f} | {sum(float(r['user']) for r in r1):.1f} "
            f"| {sum(float(r['sys']) for r in r1):.1f} | {rss:.0f} |"
        )
    print("\n### Per invocation (embed s, real, user, sys, load before -> after)\n")
    print("| round | corpus | arm | rc | embed s | real | user | sys | load before | load after |")
    print("|---|---|---|---|---:|---:|---:|---:|---|---|")
    for r in rows:
        print(
            f"| {r['round']} | {r['corpus']} | {r['arm'].replace('jina-v2-base-code-int8', 'int8')} | {r['rc']} | {float(r['embed_s']):.1f} "
            f"| {r['real']} | {r['user']} | {r['sys']} | {r['load_before']} | {r['load_after']} |"
        )
    print("\n## Control (1024 re-run vs stored int8)\n")
    print((out / "control.txt").read_text())
    for name in ("report-vs-int8", "report-vs-fp32"):
        print(f"\n## {name}\n")
        print((out / f"{name}.txt").read_text())
        rep = json.loads((out / f"{name}.json").read_text())
        print("validity errors:", rep["validity"]["errors"])
        for arm, e in rep["arms"].items():
            if "gates" not in e:
                continue
            print(f"\n### {arm}: {e['verdict']}")
            for g in e["gates"]:
                print(f"- {g['id']} {'pass' if g['passed'] else 'FAIL'}: {g['detail']}")
            fl = e.get("floors", {})
            print(f"- floors: {json.dumps(fl.get('floors', fl))}")
    print("\n## GM-434 columns (held-out NL, shipped int8 floors)\n")
    for f in sorted(out.glob("gm434-*.txt")):
        lines = [l for l in f.read_text().splitlines() if l.startswith("| NL held-out |") or l.startswith("| name (mechanical) | all")]
        print(f"### {f.stem[7:]}")
        print("\n".join(lines))


if __name__ == "__main__":
    {"control": control, "costs": costs, "summary": summary}[sys.argv[1]](*sys.argv[2:])
