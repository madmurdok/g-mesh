#!/usr/bin/env python3
"""Pass rules of the GM-730 controls on the laptop side (docs/design/GM-730-gpu-training-routes.md,
section 2 and 6 S5). Each command prints one line per rule, `<rule>: pass ...` or
`<rule>: FAILED ...`, and exits 1 when any rule failed.

    c0    --report c0_report.json
          C-0: int8-structured r@10 / MRR 0.608 / 0.380 and adapter full 0.655 / 0.423, at 3 decimals.
    cr2   --runs DIR --eval-dir DIR --arm A --official B
          C-R2: |delta| r@10 and MRR <= 0.002 (pooled scored positives) and the top-10 ids identical
          on >= 99% of the queries.
    cs    --runs DIR --eval-dir DIR --arm A --base B
          C-S on v2: paired (A - B) one-sided 95% lower bounds of MRR and r@10 both <= 0.
    cs-rr --table gm463_rr.md --arm M [--base ship-L6] [--k 30]
          C-S of route RR on held-out NL: the paired row (b = M, a = base) lower bounds <= 0.
    s5    --result <shuffled step>/result.json
          S5 on dev: the shuffled control's paired dev MRR gain lower bound over its epoch 0 <= 0.
    cp2   --export <step>/export.json [...]
          C-P2 recorded by the instance: `cp2.pass` is true.

The v2 metrics are recall_at_k.py's (D5 definitions: first expected id among the kept
hits, pooled by language over positive non-mechanical queries); its bootstrap is
stratified by language with Python's RNG, so its bounds are not bit-equal to the
Rust report's.
"""

import argparse
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[1] / "embedding"))
import recall_at_k as RK  # noqa: E402

C0_WANT = {"jina-v2-base-code-int8-structured": ("0.608", "0.380"),
           "int8-structured-adapter-full": ("0.655", "0.423")}
CR2_TOL = 0.002
CR2_TOP10_SHARE = 0.99


def line(rule, ok, detail):
    print(f"{rule}: {'pass' if ok else 'FAILED'} {detail}", flush=True)
    return ok


def c0(report):
    arms = json.loads(Path(report).read_text())["arms"]
    ok = True
    for arm, (r10, mrr) in C0_WANT.items():
        a = arms.get(arm)
        got = (f"{a['recall@10']['point']:.3f}", f"{a['mrr']['point']:.3f}") if a else None
        ok &= line(f"C-0 {arm}", got == (r10, mrr), f"r@10 / MRR {got} want {(r10, mrr)}")
    return ok


def nl(rows):
    return [r for r in rows if r["stratum"] == "nl"]


def top10(run_dir):
    """{(corpus, query id): top-10 hit ids} over every query of every corpus run."""
    out = {}
    for d in sorted(p for p in Path(run_dir).iterdir() if (p / "rankings.jsonl").exists()):
        for x in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(x)
            out[(d.name, r["id"])] = [h[0] for h in r["hits"][:10]]
    return out


def cr2(runs, eval_dir, arm, official):
    a, _ = RK.load_arm(Path(eval_dir), Path(runs) / arm)
    b, _ = RK.load_arm(Path(eval_dir), Path(runs) / official)
    a, b = nl(a), nl(b)
    ok = True
    for name, f in (("r@10", RK.hit(10)), ("MRR", RK.rr)):
        x, y = RK.pooled(RK.by_lang(a, f)), RK.pooled(RK.by_lang(b, f))
        ok &= line(f"C-R2 {name}", abs(x - y) <= CR2_TOL, f"{arm} {x:.4f} vs {official} {y:.4f} (|d| <= {CR2_TOL})")
    ta, tb = top10(Path(runs) / arm), top10(Path(runs) / official)
    if set(ta) != set(tb):
        return line("C-R2 top-10", False, f"query sets differ ({len(ta)} vs {len(tb)})") and False
    same = sum(ta[k] == tb[k] for k in ta) / len(ta) if ta else 0.0
    ok &= line("C-R2 top-10", same >= CR2_TOP10_SHARE, f"identical on {same:.4f} of {len(ta)} queries (>= {CR2_TOP10_SHARE})")
    return ok


def cs(runs, eval_dir, arm, base):
    a, _ = RK.load_arm(Path(eval_dir), Path(runs) / arm)
    b, _ = RK.load_arm(Path(eval_dir), Path(runs) / base)
    a, b = nl(a), nl(b)
    ids_a, ids_b = {r["id"] for r in a}, {r["id"] for r in b}
    if ids_a != ids_b:
        return line("C-S", False, f"{arm} and {base} cover different queries")
    ok = True
    for name, f in (("MRR", RK.rr), ("r@10", RK.hit(10))):
        point, lower = RK.paired_lower(a, b, f, f)
        ok &= line(f"C-S {name}", lower <= 0, f"{arm} - {base}: {point:+.4f}, lower {lower:+.4f} (must be <= 0)")
    return ok


ROW = re.compile(r"^\| (?P<b>[^|]+) \| (?P<a>[^|]+) \| (?P<sub>[^|]+) \| (?P<r10>[^|]+) \| (?P<mrr>[^|]+) \|$")
BOUND = re.compile(r"^\s*([+-][0-9.]+) \[([+-][0-9.]+), ([+-][0-9.]+)\]\s*$")


def cs_rr(table, arm, base="ship-L6", k=30):
    section, found = None, None
    for x in Path(table).read_text().splitlines():
        if x.startswith("## K="):
            section = int(x[5:].strip())
            continue
        m = ROW.match(x.strip())
        if section == k and m and m["b"].strip() == arm and m["a"].strip() == base and m["sub"].strip() == "held-out NL":
            found = m
    if found is None:
        return line("C-S rr", False, f"no paired row b={arm} a={base} held-out NL at K={k} in {table}")
    ok = True
    for name, cell, scale in (("r@10", found["r10"], 100.0), ("MRR", found["mrr"], 1.0)):
        b = BOUND.match(cell)
        if not b:
            ok &= line(f"C-S rr {name}", False, f"cannot parse {cell!r}")
            continue
        lower = float(b[2]) / scale
        ok &= line(f"C-S rr {name}", lower <= 0, f"{arm} - {base} held-out NL {cell.strip()} (lower must be <= 0)")
    return ok


def s5(result):
    r = json.loads(Path(result).read_text())
    g = r["gain"]
    return line("S5 dev", g["lower"] <= 0,
                f"{result}: dev MRR gain {g['mrr']:+.4f} [{g['lower']:+.4f}, {g['upper']:+.4f}] over epoch 0 (lower must be <= 0)")


def cp2(exports):
    ok = True
    for e in exports:
        c = json.loads(Path(e).read_text()).get("cp2")
        ok &= line("C-P2", bool(c) and c.get("pass") is True,
                   f"{e}: " + (json.dumps({k: c[k] for k in sorted(c) if k != 'check'}) if c else "no cp2 record"))
    return ok


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("c0")
    p.add_argument("--report", required=True, type=Path)
    for name, other in (("cr2", "--official"), ("cs", "--base")):
        p = sub.add_parser(name)
        p.add_argument("--runs", required=True, type=Path)
        p.add_argument("--eval-dir", required=True, type=Path)
        p.add_argument("--arm", required=True)
        p.add_argument(other, required=True)
    p = sub.add_parser("cs-rr")
    p.add_argument("--table", required=True, type=Path)
    p.add_argument("--arm", required=True)
    p.add_argument("--base", default="ship-L6")
    p.add_argument("--k", type=int, default=30)
    p = sub.add_parser("s5")
    p.add_argument("--result", required=True, type=Path)
    p = sub.add_parser("cp2")
    p.add_argument("--export", required=True, type=Path, nargs="+")
    a = ap.parse_args(argv)
    if a.cmd == "c0":
        ok = c0(a.report)
    elif a.cmd == "cr2":
        ok = cr2(a.runs, a.eval_dir, a.arm, a.official)
    elif a.cmd == "cs":
        ok = cs(a.runs, a.eval_dir, a.arm, a.base)
    elif a.cmd == "cs-rr":
        ok = cs_rr(a.table, a.arm, a.base, a.k)
    elif a.cmd == "s5":
        ok = s5(a.result)
    else:
        ok = cp2(a.export)
    if not ok:
        print(f"FAILED: {a.cmd}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
