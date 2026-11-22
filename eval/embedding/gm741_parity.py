"""GM-741 S8: Rust vs Python ORT parity of the QAT quint8 reranker (design section 4).

  sample  --work W --cache-dir C --out F [--windows N]
      draws 15 NL + 15 name windows per language (seed 741) from the v2 K=30 window set, scores every pair
      with Python ORT (rerank_eval.CrossEncoder, one pair per run) and writes F; the Python logits also go
      into the python arm's cache under C (so gm700_rust_pairs.py import has pairs to compare).
  compare --work W --dir D --sample F --beta B --out R
      D = the gm700_rust_pairs.py dump dir holding keys.json + logits.jsonl (the Rust logits of every pair).
      Per-pair |d|, and per-window top-1 / top-3 of the blend logit + B * cosine, Rust vs Python.

  top     --work W --cache-base C0 --cache-qat C1 --beta B --out F
      held-out positives: queries whose F4 top row differs between today's stack and the candidate, MRR and
      r@10 of the shown order within the K=30 window (rank of the first expected row; none = 0).

Tolerance (design section 4): mean |d| <= 0.01 and <= 0.005 x the sample's Python logit sd; max |d| <= 0.20;
blend top-1 identical in >= 119 of 120 windows; no window's top-3 set differs. Exit 1 when a bound fails.
"""
import argparse
import json
import random
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm463_bakeoff as B  # noqa: E402
import gm462_ce_enriched as E  # noqa: E402
import gm700_rust_pairs as P  # noqa: E402
import rerank_eval as R  # noqa: E402

SEED, PER_CELL = 741, 15


def draw(cs, need, per_cell):
    byname = {c.name: c for c in cs}
    cells = {}
    for (cn, q) in sorted(need):
        qq = byname[cn].queries[q]
        cells.setdefault(("name" if qq["mechanical"] else "nl", qq["language"]), []).append((cn, q))
    rng = random.Random(SEED)
    out = []
    for cell in sorted(cells):
        c = cells[cell]
        if len(c) < per_cell:
            sys.exit(f"STOP: cell {cell} has {len(c)} windows < {per_cell}")
        out += [(cell, w) for w in rng.sample(c, per_cell)]
    return out


def sample(a):
    ns = argparse.Namespace(work=a.work, ce="qat")
    cs, need = P.setup(ns)
    byname = {c.name: c for c in cs}
    drawn = draw(cs, need, a.per_cell)
    ce = R.CrossEncoder(a.work, R.SHIPPED_CE["qat"]["py"])
    py_arm, _ = P.arms(ns)
    cache = B.open_cache(a.cache_dir, cs, a.work, py_arm, B.file_shas(a.work, py_arm))
    windows, logit = [], {}
    for (pop, lang), (cn, q) in drawn:
        c = byname[cn]
        m = need[(cn, q)]
        hs = list(m)
        v = ce.score(c.queries[q]["text"], [m[h] for h in hs])
        cos = {}
        for nid, x in c.lists[B.ST][q][:P.K]:
            cos.setdefault(E.tsha(c.atext["ship"][nid]), float(x))
        windows.append({"cn": cn, "q": q, "pop": pop, "lang": lang, "hs": hs, "cos": [cos[h] for h in hs]})
        for h, x in zip(hs, v):
            logit[f"{cn}|{q}|{h}"] = float(x)
            cache.put(cn, q, [h], [float(x)])
    for c in cs:
        cache.save(c)
    a.out.write_text(json.dumps({"windows": windows, "py": logit}))
    print(f"sampled {len(windows)} windows, {len(logit)} pairs")


def compare(a):
    s = json.loads(a.sample.read_text())
    keys = json.loads((a.dir / "keys.json").read_text())
    rl = [json.loads(l)["logit"] for l in (a.dir / "logits.jsonl").read_text().splitlines()]
    if len(rl) != len(keys):
        sys.exit(f"STOP: {len(rl)} logits for {len(keys)} pairs")
    rust = {f"{cn}|{q}|{h}": v for (cn, q, h), v in zip(keys, rl)}
    d, pyv, per_win = [], [], []
    top1_same = top3_same = 0
    for w in s["windows"]:
        ks = [f"{w['cn']}|{w['q']}|{h}" for h in w["hs"]]
        p = np.array([s["py"][k] for k in ks])
        r = np.array([rust[k] for k in ks])
        d += list(np.abs(r - p))
        pyv += list(p)
        cos = np.array(w["cos"])
        bp, br = p + a.beta * cos, r + a.beta * cos
        t1 = int(np.argsort(-bp, kind="stable")[0]) == int(np.argsort(-br, kind="stable")[0])
        t3 = set(np.argsort(-bp, kind="stable")[:3]) == set(np.argsort(-br, kind="stable")[:3])
        top1_same += t1
        top3_same += t3
        per_win.append({"cn": w["cn"], "q": w["q"], "top1_same": t1, "top3_same": t3,
                        "max_abs_d": float(np.abs(r - p).max())})
    d, pyv = np.array(d), np.array(pyv)
    n = len(s["windows"])
    sd = float(pyv.std())
    rep = {"windows": n, "pairs": int(len(d)), "beta": a.beta, "mean_abs_d": float(d.mean()), "max_abs_d": float(d.max()),
           "p99_abs_d": float(np.quantile(d, 0.99)), "pairs_d_gt_0": int((d > 0).sum()),
           "py_logit_mean": float(pyv.mean()), "py_logit_sd": sd, "bound_mean_scaled": 0.005 * sd,
           "top1_same": top1_same, "top3_same": top3_same, "worst_windows": sorted(per_win, key=lambda x: -x["max_abs_d"])[:3]}
    rep["checks"] = {"mean<=0.01": rep["mean_abs_d"] <= 0.01, "mean<=0.005sd": rep["mean_abs_d"] <= 0.005 * sd,
                     "max<=0.20": rep["max_abs_d"] <= 0.20, "top1>=n-1": top1_same >= n - 1, "top3_all_same": top3_same == n}
    rep["pass"] = all(rep["checks"].values())
    a.out.write_text(json.dumps(rep, indent=1))
    print(json.dumps(rep))
    sys.exit(0 if rep["pass"] else 1)


def f4_order(c, q, sc, flags, beta):
    """Row order of the shown page: cosine order when the exact-name skip fires, else logit + beta * cosine."""
    import gm699_separability as S  # noqa: E402
    lst = c.lists[B.ST][q]
    n = min(S.K, len(lst))
    cos = np.array([x for _, x in lst[:n]])
    logit = np.array([sc(c, q, nid) for nid, _ in lst[:n]])
    s_ = -np.arange(n, dtype=np.float64) if flags[(c.name, q)] else logit + beta * cos
    return [lst[i][0] for i in np.lexsort((np.arange(n), -s_))]


def top(a):
    """Held-out queries whose top row differs between the arms, and held-out MRR / r@10 of each arm."""
    import gm701_floor_or_blend as G  # noqa: E402
    arms = {}
    for name, ce, cache, beta in (("base", "l6", a.cache_base, 40.0), ("cand", "qat", a.cache_qat, a.beta)):
        cs, sc, flags, _ = G.setup(a.work, cache, None, ce)
        arms[name] = (cs, sc, flags, beta)
    out = {"differ": {}, "n_ho": {}, "mrr": {"base": {}, "cand": {}}, "r10": {"base": {}, "cand": {}}}
    acc = {}
    for ci, c in enumerate(arms["base"][0]):
        c2 = arms["cand"][0][ci]
        for q in c.qids:
            qq = c.queries[q]
            mech = bool(qq["mechanical"])
            ho = G.name_held_out(q) if mech else bool(qq["held_out"])
            if not ho or not qq["positive"] or not c.lists[B.ST][q]:
                continue
            pop = "NAME" if mech else "NL"
            o = {}
            for nm, cc in (("base", c), ("cand", c2)):
                _, sc, flags, beta = arms[nm]
                o[nm] = f4_order(cc, q, sc, flags, beta)
            d = acc.setdefault(pop, {"n": 0, "differ": 0, "base": [], "cand": []})
            d["n"] += 1
            d["differ"] += o["base"][0] != o["cand"][0]
            for nm in ("base", "cand"):
                r = next((i + 1 for i, nid in enumerate(o[nm]) if nid in c.expected[q]), None)
                d[nm].append(r)
    for pop, d in acc.items():
        out["n_ho"][pop], out["differ"][pop] = d["n"], d["differ"]
        for nm in ("base", "cand"):
            rs = d[nm]
            out["mrr"][nm][pop] = float(np.mean([1 / r if r else 0 for r in rs]))
            out["r10"][nm][pop] = float(np.mean([bool(r and r <= 10) for r in rs]))
    out["differ_total"] = sum(out["differ"].values())
    a.out.write_text(json.dumps(out, indent=1))
    print(json.dumps(out))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)
    s = sub.add_parser("sample")
    s.add_argument("--work", type=Path, required=True)
    s.add_argument("--cache-dir", type=Path, required=True)
    s.add_argument("--out", type=Path, required=True)
    s.add_argument("--per-cell", type=int, default=PER_CELL)
    c = sub.add_parser("compare")
    c.add_argument("--dir", type=Path, required=True)
    c.add_argument("--sample", type=Path, required=True)
    c.add_argument("--beta", type=float, required=True)
    c.add_argument("--out", type=Path, required=True)
    t = sub.add_parser("top")
    t.add_argument("--work", type=Path, required=True)
    t.add_argument("--cache-base", type=Path, required=True)
    t.add_argument("--cache-qat", type=Path, required=True)
    t.add_argument("--beta", type=float, required=True)
    t.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    {"sample": sample, "compare": compare, "top": top}[a.mode](a)


if __name__ == "__main__":
    main()
