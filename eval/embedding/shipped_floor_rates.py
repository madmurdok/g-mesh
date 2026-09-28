#!/usr/bin/env python3
"""GM-434: false-alarm and confident-wrong rates of the SHIPPED similarity floors.

Reads the stored rankings of a GM-398 run (no re-embedding) and judges each
query exactly as `core/src/cli/embed_eval/metrics.rs` does, but at fixed
floors instead of re-fitted ones:

- false alarm: positive query, top hit is an expected symbol, top score below
  the floor of the top hit's language (search_code would say "no match").
- confident wrong: top score at or above that floor, and the top hit is not
  expected (positive) or the query has no answer (absent).

Control: `--floors fitted` must reproduce the report's `falseAlarmHeldOut` and
confident-wrong counts for the reference arm (report-phaseB.json).

Usage:
  python3 eval/embedding/shipped_floor_rates.py --run <runs/jina-v2-base-code-fp32> [--floors shipped|fitted]
"""
import argparse
import hashlib
import json
from collections import defaultdict
from pathlib import Path

EVAL = Path(__file__).resolve().parent
# core/src/mcp/similarity.rs::floor, release-3.16.0 (read, not remembered).
SHIPPED = {"go": 0.59, "python": 0.57, "rust": 0.55, "typescript": 0.50}
# report-phaseB.json, arms["jina-v2-base-code-fp32"].floors.floors (GM-398 D6).
FITTED = {"go": 0.56, "python": 0.58, "rust": 0.56, "typescript": 0.55}
DEFAULT_FLOOR = 0.50


def held_out(qid: str, mechanical: bool) -> bool:
    return not mechanical and hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load_queries(corpus: str):
    out = {}
    for rel, mech in ((f"queries/{corpus}.jsonl", False), (f"queries/mechanical/{corpus}.jsonl", True)):
        p = EVAL / rel
        if not p.exists():
            continue
        for line in p.read_text().splitlines():
            if line.strip():
                q = json.loads(line)
                q["_mech"] = mech
                out[q["id"]] = q
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--floors", choices=["shipped", "fitted"], default="shipped")
    ap.add_argument("--json")
    a = ap.parse_args()
    floors = SHIPPED if a.floors == "shipped" else FITTED

    rows = []
    for d in sorted(Path(a.run).iterdir()):
        if not (d / "rankings.jsonl").exists():
            continue
        man = json.loads((d / "manifest.json").read_text())
        for rel, sha in man["queryFiles"]:
            got = hashlib.sha256((EVAL / rel).read_bytes()).hexdigest()
            if got != sha:
                raise SystemExit(f"{rel} changed since the run ({got} != {sha})")
        qs = load_queries(man["corpus"])
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            q = qs[r["id"]]
            hits = r["hits"]
            rank = next((i + 1 for i, (nid, _) in enumerate(hits) if nid in r["expected"]), None)
            top = hits[0][1] if hits else None
            tl = r.get("topLanguage")
            rows.append(dict(
                id=r["id"], corpus=man["corpus"], lang=q["language"], mech=q["_mech"],
                pos=q["kind"] == "positive", held=held_out(r["id"], q["_mech"]),
                rank1=rank == 1, clears=None if top is None else top >= floors.get(tl, DEFAULT_FLOOR),
            ))

    def rates(sel):
        fa = [r for r in sel if r["pos"] and r["rank1"] and r["clears"] is not None]
        cwp = [r for r in sel if r["pos"] and r["clears"] is not None]
        cwa = [r for r in sel if not r["pos"] and r["clears"] is not None]
        return dict(
            fa=(sum(not r["clears"] for r in fa), len(fa)),
            cw_pos=(sum(r["clears"] and not r["rank1"] for r in cwp), len(cwp)),
            cw_abs=(sum(r["clears"] for r in cwa), len(cwa)),
            n_pos=sum(r["pos"] for r in sel), n_abs=sum(not r["pos"] for r in sel),
        )

    sets = {
        "NL held-out": lambda r: not r["mech"] and r["held"],
        "NL all": lambda r: not r["mech"],
        "name (mechanical)": lambda r: r["mech"],
    }
    langs = sorted({r["lang"] for r in rows})
    result = {}
    def pct(t):
        return f"{100*t[0]/t[1]:.1f}% ({t[0]}/{t[1]})" if t[1] else "-"
    print(f"floors={a.floors} {floors}")
    print("| set | language | floor | false alarm | confident wrong (positives) | confident wrong (absent) | n pos / n absent |")
    print("|---|---|---|---|---|---|---|")
    for name, f in sets.items():
        for lang in langs + ["all"]:
            sel = [r for r in rows if f(r) and (lang == "all" or r["lang"] == lang)]
            x = rates(sel)
            result[f"{name}/{lang}"] = x
            fl = floors.get(lang, "-") if lang != "all" else "-"
            print(f"| {name} | {lang} | {fl} | {pct(x['fa'])} | {pct(x['cw_pos'])} | {pct(x['cw_abs'])} | {x['n_pos']} / {x['n_abs']} |")
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
