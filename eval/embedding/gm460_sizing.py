#!/usr/bin/env python3
"""GM-460/S1: sizing of the enlarged NL eval (docs/design/GM-460-enlarged-nl-eval.md).

Reads stored rankings and query files only; embeds nothing.
  1. counts per language and side, and the observed per-query Q4/Q5 deltas
     (held-out, each arm at its own D6 floors) of four stored pairs;
  2. split counts of the GM-398 and GM-422 confirm sets, cross-side
     expected-symbol overlap, and the n a per-language CI half-width needs.

usage: gm460_sizing.py <runs work dir>   e.g. <main checkout>/eval/embedding/work
"""
import hashlib, json, math, sys
from pathlib import Path
EV = Path(__file__).resolve().parent
W = Path(sys.argv[1])
sys.path.insert(0, str(EV))
import q5_floor_sensitivity as q
LANGS = q.LANGS
arms = {
  "fp32": W / "runs/jina-v2-base-code-fp32",
  "int8": W / "runs/jina-v2-base-code-int8",
  "gte": W / "runs/gte-small",
  "bge": W / "runs/bge-small-en-v1.5",
  "int8s": W / "runs-gm465/jina-v2-base-code-int8-structured",
}
O = {k: q.load_arm(p, EV) for k, p in arms.items()}
F = {k: q.fit_floors(v)[0] for k, v in O.items()}
print("floors", json.dumps(F))
# counts (authored only)
ref = O["fp32"]
print("\n## counts (authored)")
print("lang | pos all | abs all | pos held-out | abs held-out | pos FIT | rank1 all fp32 | rank1 ho fp32 | rank1 ho int8s")
cnt = {}
for l in LANGS:
    a = [o for o in ref if o["language"] == l and not o["mechanical"]]
    pa = [o for o in a if o["positive"]]; aa = [o for o in a if not o["positive"]]
    pho = [o for o in pa if o["held_out"]]; aho = [o for o in aa if o["held_out"]]
    r1 = sum(o["rank"] == 1 for o in pa); r1h = sum(o["rank"] == 1 for o in pho)
    s = [o for o in O["int8s"] if o["language"] == l and not o["mechanical"] and o["positive"] and o["held_out"]]
    r1s = sum(o["rank"] == 1 for o in s)
    cnt[l] = dict(pos=len(pa), abs=len(aa), pho=len(pho), aho=len(aho), r1=r1, r1h=r1h, r1s=r1s)
    print(l, len(pa), len(aa), len(pho), len(aho), len(pa) - len(pho), r1, r1h, r1s, sep=" | ")
pairs = [("int8", "fp32"), ("int8s", "int8"), ("gte", "fp32"), ("bge", "fp32")]
def stats(d):
    n = len(d)
    if n == 0: return (0, 0, 0, 0, 0)
    m = sum(d) / n; v = sum((x - m) ** 2 for x in d) / (n - 1) if n > 1 else 0.0
    return (n, m, math.sqrt(v), sum(x > 0 for x in d), sum(x < 0 for x in d))
res = {}
print("\n## per-query deltas, held-out, each arm at its own D6 floors")
print("pair | metric | lang | n | mean pts | sd | +/- | 95% half-width pts (1.96 sd/sqrt n)")
for c, r in pairs:
    g5 = q.paired(O[r], F[r], O[c], F[c])
    g4 = q.paired_ind(O[r], F[r], O[c], F[c], q.cw_indicator)
    for name, g in (("Q5", g5), ("Q4", g4)):
        for l in LANGS:
            n, m, sd, up, dn = stats(g.get(l, []))
            hw = 1.96 * sd / math.sqrt(n) * 100 if n else float("nan")
            res[(c, r, name, l)] = (n, m, sd)
            print(f"{c} vs {r} | {name} | {l} | {n} | {100*m:+.1f} | {sd:.3f} | +{up}/-{dn} | {hw:.1f}")

def ho(i): return hashlib.sha256(i.encode()).digest()[0] % 2 == 1
def load(d):
    out = []
    for f in sorted((EV / d).glob("*.jsonl")):
        for line in f.read_text().splitlines():
            if line.strip(): out.append(json.loads(line))
    return out
old, conf = load("queries"), load("confirm/queries")
print("\n## counts by set, language, kind, side (held-out = odd first byte of sha256(id))")
print("set | lang | pos FIT | pos held-out | abs FIT | abs held-out")
tot = {}
for name, qs in (("GM-398", old), ("GM-422 confirm", conf)):
    for l in LANGS:
        c = {(k, s): 0 for k in ("positive", "absent") for s in (False, True)}
        for x in qs:
            if x["language"] == l: c[(x["kind"], ho(x["id"]))] += 1
        for k, v in c.items(): tot[(l,) + k] = tot.get((l,) + k, 0) + v
        print(name, l, c[("positive", False)], c[("positive", True)], c[("absent", False)], c[("absent", True)], sep=" | ")
for l in LANGS:
    print("merged", l, *(tot[(l, k, s)] for k, s in (("positive", False), ("positive", True), ("absent", False), ("absent", True))), sep=" | ")
# leakage: held-out queries sharing an expected symbol with a FIT query
print("\n## cross-side expected-symbol overlap (merged set)")
key = lambda x, e: (x["corpus"], e["filePath"], e["qualifiedName"])
fit_syms = {}
for x in old + conf:
    if x["kind"] == "positive" and not ho(x["id"]):
        for e in x["expected"]: fit_syms.setdefault(key(x, e), []).append(x["id"])
leak = [(x["id"], sorted({i for e in x["expected"] for i in fit_syms.get(key(x, e), [])}))
        for x in old + conf if x["kind"] == "positive" and ho(x["id"])
        and any(key(x, e) in fit_syms for e in x["expected"])]
print(f"held-out positives sharing an expected symbol with a FIT positive: {len(leak)}")
for i, f in leak: print("  ", i, "<->", ", ".join(f))
# sizing
SD5, SD4, Z = 0.45, 0.55, 1.96
PAIR = {"go": (16 + 48) / (61 + 200), "python": (20 + 34) / (47 + 150), "rust": (6 + 24) / (47 + 150), "typescript": (25 + 55) / (60 + 150)}
LEARNED = 60 / 70  # GM-443 S8: rerank-vs-int8 pairs / int8 held-out rank-1 (13+17+7+23)/(17+20+7+26)
print("\n## planning: pair rate r (int8 vs fp32 Q5 pairs / held-out positives, GM-398 + GM-422 confirm), x", round(LEARNED, 3))
R = {l: PAIR[l] * LEARNED for l in LANGS}
for l in LANGS: print(f"{l}: pair rate {PAIR[l]:.3f} -> planning r {R[l]:.3f}")
print("\n## n positives per language (both halves; held-out = half) for a 95% half-width h (pts); absent = pos/4")
print("h | Q5 pairs needed | Q5 go | Q5 python | Q5 rust | Q5 ts | Q4 held-out rows needed | Q4 pos per lang")
for h in (20, 15, 12.5, 10, 7.5, 5):
    m5 = math.ceil((Z * SD5 / (h / 100)) ** 2); m4 = math.ceil((Z * SD4 / (h / 100)) ** 2)
    q5 = [math.ceil(2 * m5 / R[l]) for l in LANGS]
    q4 = math.ceil(2 * m4 / 1.25)
    print(h, m5, *q5, m4, q4, sep=" | ")
def hw(sd, n): return 100 * Z * sd / math.sqrt(n) if n else float("nan")
print("\n## half-width (pts) bought at a given total per language (pos, abs = pos/4), held-out = half")
print("pos/lang | Q4 rows ho | Q4 h | Q5 h go | python | rust | ts | pooled Q5 h | pooled Q4 h")
for P in (100, 250, 300, 330, 400, 500):
    rows = P / 2 * 1.25
    h5 = [hw(SD5, R[l] * P / 2) for l in LANGS]
    pooled5 = 100 * Z * math.sqrt(sum(SD5 ** 2 / (R[l] * P / 2) for l in LANGS)) / 4
    pooled4 = 100 * Z * math.sqrt(4 * SD4 ** 2 / rows) / 4
    print(P, round(rows), f"{hw(SD4, rows):.1f}", *(f"{x:.1f}" for x in h5), f"{pooled5:.1f}", f"{pooled4:.1f}", sep=" | ")
