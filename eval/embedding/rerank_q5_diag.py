#!/usr/bin/env python3
"""GM-443 S8: why does ce-minilm+int8 (K=50, beta=40) fail Q5, and can a floor rule fix it?

Reads only stored data: the GM-398 rankings, the corpus indexes and
rerank_eval.py's per-pair cross-encoder cache (<work>/rerank_cache/). It never
scores a pair: a pair missing from the cache is a STOP.

Stages:
  0. control: rebuild S4's ce-minilm+int8 K=50 blend (beta re-tuned on the FIT
     half, must be 40) and check its S4 row (held-out r@10 / MRR, D9 row vs
     int8 and fp32, TS false-alarm delta, floors, GM-434 NL misled);
  1. diagnosis: the paired Q5 TypeScript queries, which flip, what they hit,
     the floor's fitting population, and where the extra misled share comes from;
  2. fixes: each candidate floor rule is tuned on the FIT half only and gated on
     the held-out half; the "revert" of each must reproduce S4's row.

usage: rerank_q5_diag.py --work <eval/embedding/work> [--fixes [--table out.md]]
"""
import argparse
import hashlib
import json
import math
import re
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rerank_eval as R  # noqa: E402

NAME = "ce-minilm"
ARM = R.INT8
K = 50


def ce_cache(work, cs):
    """rerank_eval.ce_scores' cache, read only; STOP on any missing pair."""
    d = work / "models" / R.CE_DIR[NAME]
    model_sha = R.sha256_file(d / "model.onnx")
    out = {}
    for c in cs:
        key = hashlib.sha256(json.dumps([model_sha, c.snapshot_sha, c.node_ids_sha, R.MAX_TOKENS]).encode()).hexdigest()[:16]
        f = work / "rerank_cache" / f"{NAME}.{c.name}.pairs.{key}.json"
        if not f.exists():
            sys.exit(f"STOP: no cache {f}")
        s = json.loads(f.read_text())
        for q in c.qids:
            miss = [n for n, _ in c.lists[ARM][q][:K] if n not in s.get(q, {})]
            if miss:
                sys.exit(f"STOP: {c.name}/{q}: {len(miss)} pairs not cached")
        out[c.name] = s
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--fixes", action="store_true")
    ap.add_argument("--table", type=Path, help="write the fix tables here")
    ap.add_argument("--table14", type=Path, help="write the S14 tables (F4 at K=20/30) here")
    a = ap.parse_args()
    eval_dir = Path(R.__file__).resolve().parent
    toml = (eval_dir / "variants.toml").read_text()
    for k in ("bootstrap_seed", "bootstrap_resamples"):
        R.SETTINGS[k] = int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))
    corpora = [p.name for p in sorted((a.work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    cs = [R.Corpus(a.work, eval_dir, c, 0) for c in corpora]
    byname = {c.name: c for c in cs}
    base = {arm: [R.outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                  for c in cs for q in c.qids] for arm in (R.REF, R.INT8)}
    shipped = R.shipped_floors(eval_dir.parents[1])
    sc = ce_cache(a.work, cs)
    CE = lambda c, q: np.array([sc[c.name][q][n] for n, _ in c.lists[ARM][q][:K]])
    COS = lambda c, q: np.array([s for _, s in c.lists[ARM][q][:K]])
    fit_keys = [(c, q) for c in cs for q in c.qids if R.fit_nl(c.queries[q])]

    def fit_objective(score_of, keys=fit_keys):
        g10, gm = {}, {}
        for c, q in keys:
            hits, _ = R.reranked(c, q, K, score_of(c, q), ARM)
            r = R.first_rank(c, q, hits)
            lang = c.queries[q]["language"]
            g10.setdefault(lang, []).append(1.0 if r and r <= 10 else 0.0)
            gm.setdefault(lang, []).append(1.0 / r if r else 0.0)
        return round(R.pooled_mean(g10), 9), round(R.pooled_mean(gm), 9)

    def tune_beta(keys=fit_keys):
        best = None
        for b in R.BETAS:
            key = fit_objective(lambda c, q: CE(c, q) + b * COS(c, q), keys) + (-b,)
            if best is None or key > best[0]:
                best = (key, b)
        return best[1], best[0]

    beta, key = tune_beta()
    print(f"beta re-tuned on FIT: {beta:g} (fit r@10 {key[0]:.3f}, MRR {key[1]:.3f})")

    def build(beta_of, floor_score="blend"):
        """Rerank int8's top-K by CE + beta*cos; `top` = the floored score of the new top row.
        Also records per query the parts of the top row for the diagnosis."""
        outs, info = [], {}
        for c in cs:
            for q in c.qids:
                b = beta_of(c, q)
                ce, co = CE(c, q), COS(c, q)
                s = ce + b * co
                hits, top = R.reranked(c, q, K, s, ARM)
                i = [n for n, _ in c.lists[ARM][q][:K]].index(hits[0][0])
                ftop = {"blend": top, "cos": float(co[i]), "ce": float(ce[i])}[floor_score]
                o = R.outcome(c, q, hits, ftop)
                outs.append(o)
                info[q] = {"corpus": c.name, "node": hits[0][0], "ce": float(ce[i]), "cos": float(co[i]),
                           "blend": float(s[i]), "beta": b}
        return outs, info

    s4, s4info = build(lambda c, q: beta)

    # ---------------- 0. control ----------------
    s = R.summarize(s4)["nl_heldout"]
    g8 = R.gates(base[R.INT8], s4, R.ho, rf=shipped)
    g32 = R.gates(base[R.REF], s4, R.ho)
    fl = R.fit_floors(s4)
    mis = R.gm434(s4, fl, R.nl_ho_all)["misled"]
    got = (round(s["r10"], 3), round(s["mrr"], 3),
           R.pts(g8["r10"][0]), R.pts(g8["r10"][1]), f"{g8['mrr'][0]:+.3f}", f"{g8['mrr'][1]:+.3f}",
           R.pts(g8["fa"][0]), R.pts(g8["fa"][2]), R.pts(g8["fa_lang"]["typescript"][0]), g8["fa_lang"]["typescript"][1],
           R.pts(g32["fa"][0]), R.pts(g32["fa"][2]), R.pts(g32["fa_lang"]["typescript"][0]), g32["fa_lang"]["typescript"][1],
           tuple(f"{fl[l]:.2f}" for l in R.LANGS), mis)
    want = (0.693, 0.477, "+4.8", "+1.2", "+0.058", "+0.027", "+6.5", "+12.7", "+26.1", 23,
            "+5.7", "+12.1", "+22.7", 22, ("19.48", "17.02", "24.91", "17.10"), (20, 80))
    ok = beta == 40.0 and got == want
    print(f"0 control S4 ce-minilm+int8 K=50: {got}\n  {'MATCH' if ok else 'MISMATCH, want ' + str(want)}")
    if not ok:
        sys.exit(2)

    # ---------------- 1. diagnosis ----------------
    def node(cname, nid):
        con = byname[cname]
        import sqlite3
        db = sqlite3.connect(f"file:{con.db}?mode=ro", uri=True)
        r = db.execute("SELECT kind, name, filePath, startLine FROM nodes WHERE id=?", (nid,)).fetchone()
        db.close()
        return r

    int8o = {o["id"]: o for o in base[R.INT8]}
    s4o = {o["id"]: o for o in s4}
    print("\n== 1a. paired Q5 (held-out NL rank-1 positives in both arms), vs int8 at shipped floors ==")
    res = {"per_lang": {}}
    for lang in R.LANGS:
        rows = []
        for o in s4:
            if o["language"] != lang:
                continue
            r_ = R.fa_indicator(int8o[o["id"]], shipped)
            c_ = R.fa_indicator(o, fl)
            if r_ is not None and c_ is not None:
                rows.append((o["id"], r_, c_))
        up = sum(1 for _, r_, c_ in rows if c_ > r_)
        down = sum(1 for _, r_, c_ in rows if c_ < r_)
        both = sum(1 for _, r_, c_ in rows if c_ == 1 and r_ == 1)
        ci = R.bootstrap({lang: [c_ - r_ for _, r_, c_ in rows]})
        print(f"  {lang}: n={len(rows)} int8 FA {sum(r_ for _, r_, _ in rows):.0f}, blend FA {sum(c_ for *_, c_ in rows):.0f}; "
              f"0->1 {up}, 1->0 {down}, both {both}; delta {R.pts(ci[0])} [lo {R.pts(ci[1])}, up {R.pts(ci[2])}]")
        res["per_lang"][lang] = {"rows": rows, "ci": ci}

    # Q5 sensitivity: move k TS flips (0->1 to 0->0), or add k
    g = R.paired_own(base[R.INT8], shipped, s4, fl, R.fa_indicator)
    print("  Q5 pooled (vs int8) if the TS group had k fewer / more 0->1 flips:")
    for k in (-6, -5, -4, -3, -2, -1, 0, 1):
        gg = {l: list(v) for l, v in g.items()}
        ts = gg["typescript"]
        if k < 0:
            idx = [i for i, x in enumerate(ts) if x == 1.0][:(-k)]
            for i in idx:
                ts[i] = 0.0
        else:
            idx = [i for i, x in enumerate(ts) if x == 0.0][:k]
            for i in idx:
                ts[i] = 1.0
        b = R.bootstrap(gg)
        passes = b[0] <= R.EPS and b[2] <= 0.05 + R.EPS
        print(f"    k={k:+d}: TS {R.pts(sum(ts) / len(ts))}, pooled {R.pts(b[0])} [up {R.pts(b[2])}] -> {'pass' if passes else 'FAIL'}")
    # and with TS removed entirely
    b = R.bootstrap({l: v for l, v in g.items() if l != "typescript"})
    print(f"    TS removed: pooled {R.pts(b[0])} [up {R.pts(b[2])}]")

    print("\n== 1b. TS queries that flip to a false alarm (correct top, below the blend's TS floor) ==")
    print(f"  blend floors {fl}; int8 shipped floors {shipped}")
    for qid, r_, c_ in res["per_lang"]["typescript"]["rows"]:
        if c_ > r_ or (c_ == 1):
            inf = s4info[qid]
            k_, nm, fp, ln = node(inf["corpus"], inf["node"])
            c = byname[inf["corpus"]]
            txt = c.text[inf["node"]].replace("\n", " ")[:90]
            print(f"  {qid} [{'flip' if c_ > r_ else 'both'}] q=\"{c.queries[qid]['text'][:80]}\"\n"
                  f"     top {k_} {nm} ({fp}:{ln}) top_lang {s4o[qid]['top_language']}\n"
                  f"     text \"{txt}\"\n"
                  f"     ce {inf['ce']:.2f}, int8 cos {inf['cos']:.3f} (shipped floor {shipped.get(s4o[qid]['top_language'])}), "
                  f"blend {inf['blend']:.2f} (floor {fl.get(s4o[qid]['top_language'])}); int8's own top score {int8o[qid]['top']:.3f}")

    print("\n== 1b'. top row has a doc comment? (held-out NL rank-1 positives, blend) ==")
    import sqlite3
    for lang in R.LANGS:
        st = {True: [], False: []}
        for o in s4:
            if R.ho(o) and o["rank"] == 1 and o["language"] == lang:
                inf = s4info[o["id"]]
                db = sqlite3.connect(f"file:{byname[inf['corpus']].db}?mode=ro", uri=True)
                doc = (db.execute("SELECT docComment FROM nodes WHERE id=?", (inf["node"],)).fetchone()[0] or "").strip()
                db.close()
                st[bool(doc)].append((inf["ce"], R.clears(o, fl)))
        for has in (True, False):
            v = st[has]
            if v:
                ces = sorted(x for x, _ in v)
                print(f"  {lang:10s} doc={'yes' if has else 'no '}: n={len(v)}, median ce {ces[len(ces) // 2]:.2f}, "
                      f"below blend floor {sum(1 for _, c in v if c is False)}")

    print("\n== 1c. the floor's fitting population (D6: rank-1 positives, all name + FIT-half NL, 3% quantile) ==")
    for lang in R.LANGS:
        pops = {"name": [], "nl_fit": [], "nl_ho": []}
        for o in s4:
            if o["positive"] and o["rank"] == 1 and o["language"] == lang:
                pops["name" if o["mechanical"] else ("nl_ho" if o["held_out"] else "nl_fit")].append(o)
        def q(v, p):
            v = sorted(v)
            return v[min(len(v) - 1, math.floor(p * len(v) + 1e-9))] if v else float("nan")
        def desc(key, sc_key):
            v = [s4info[o["id"]][sc_key] for o in pops[key]]
            return f"{key} n={len(v)} p3 {q(v, .03):.2f} p10 {q(v, .10):.2f} med {q(v, .5):.2f}"
        nl_fit_only = R.round_down_2(q([o["top"] for o in pops["nl_fit"]], 0.03)) if pops["nl_fit"] else None
        name_only = R.round_down_2(q([o["top"] for o in pops["name"]], 0.03)) if pops["name"] else None
        print(f"  {lang}: floor {fl[lang]:.2f}; floor from NL-FIT only {nl_fit_only}, from names only {name_only}")
        for sk in ("blend", "ce", "cos"):
            print(f"     {sk:5s}: " + "; ".join(desc(k_, sk) for k_ in ("name", "nl_fit", "nl_ho")))
        # the same for int8 cosine at rank 1 (int8's own ranking)
        ip = {"name": [], "nl_fit": [], "nl_ho": []}
        for o in base[R.INT8]:
            if o["positive"] and o["rank"] == 1 and o["language"] == lang:
                ip["name" if o["mechanical"] else ("nl_ho" if o["held_out"] else "nl_fit")].append(o["top"])
        print(f"     int8 own cos: " + "; ".join(f"{k_} n={len(v)} p3 {q(v, .03):.3f} med {q(v, .5):.3f}" for k_, v in ip.items()))

    print("\n== 1d. GM-434 misled (rank-1 positives the floor calls no-match), by language and query type ==")
    for key, keep in (("NL held-out", R.nl_ho_all), ("name", R.name_all)):
        for lang in R.LANGS:
            m8 = R.gm434(base[R.INT8], shipped, keep, lang)["misled"]
            mb = R.gm434(s4, fl, keep, lang)["misled"]
            print(f"  {key:11s} {lang:10s}: int8 {m8[0]}/{m8[1]}, blend {mb[0]}/{mb[1]}")
    # attribute the misled delta: queries rank-1 in both / only in blend
    for lang in R.LANGS:
        extra_new, extra_same = 0, 0
        for o in s4:
            if not R.nl_ho_all(o) or o["language"] != lang or not o["positive"] or o["rank"] != 1:
                continue
            if R.clears(o, fl) is False:
                i8 = int8o[o["id"]]
                if i8["rank"] != 1:
                    extra_new += 1
                elif R.clears(i8, shipped) is True:
                    extra_same += 1
        print(f"  NL held-out {lang}: blend-misled that int8 got wrong at rank 1 {extra_new}; "
              f"that int8 got right and cleared {extra_same}")

    print("\n== 1e. operating points: every floor shifted by d, held-out NL and name (misled / CW pos / CW absent) ==")
    fmt = lambda t: f"{100 * t[0] / t[1]:.0f}% ({t[0]}/{t[1]})"
    for label, outs, fl0, ds in (("int8", base[R.INT8], shipped, [-0.04, -0.02, 0.0, 0.02, 0.04, 0.06, 0.08, 0.10]),
                                 ("blend", s4, fl, [-8.0, -6.0, -4.0, -2.0, 0.0, 2.0])):
        for d in ds:
            f_ = {l: v + d for l, v in fl0.items()}
            n_, m_ = R.gm434(outs, f_, R.nl_ho_all), R.gm434(outs, f_, R.name_all)
            print(f"  {label:5s} d={d:+.2f}: NL {fmt(n_['misled'])} / {fmt(n_['cw_pos'])} / {fmt(n_['cw_abs'])}; "
                  f"name {fmt(m_['misled'])} / {fmt(m_['cw_pos'])} / {fmt(m_['cw_abs'])}")

    if not a.fixes:
        return
    fixes(a, cs, base, shipped, s4, beta, CE, COS)


def fixed_floors_gates(ref, cand, rf, cf):
    """R.gates with the candidate's floors given instead of D6-fitted from its `top`."""
    orig = R.fit_floors
    R.fit_floors = lambda outs: cf if outs is cand else orig(outs)
    try:
        return R.gates(ref, cand, R.ho, rf=rf)
    finally:
        R.fit_floors = orig


def fit_fa(outs, floors):
    """FIT-half false alarm: NL FIT rank-1 positives below the floor, languages weighted equally."""
    g = {}
    for o in outs:
        if R.fit_nl(o) and o["rank"] == 1:
            c = R.clears(o, floors)
            if c is not None:
                g.setdefault(o["language"], []).append(0.0 if c else 1.0)
    return R.pooled_mean(g)


def fixes(a, cs, base, shipped, s4, beta, CE, COS):
    rf32 = R.fit_floors(base[R.REF])

    def build(c0=-math.inf, floor_on="blend"):
        """Rerank int8's top-K by max(ce, c0) + beta*cos; `top` is the floored score of the new top row.
        Returns outs and, per query, (blend, cos) of the new top row."""
        outs, parts = [], {}
        for c in cs:
            for q in c.qids:
                ce, co = CE(c, q), COS(c, q)
                s = np.maximum(ce, c0) + beta * co
                hits, top = R.reranked(c, q, K, s, ARM)
                i = [n for n, _ in c.lists[ARM][q][:K]].index(hits[0][0])
                outs.append(R.outcome(c, q, hits, top if floor_on == "blend" else float(co[i])))
                parts[q] = (float(s[i]), float(co[i]))
        return outs, parts

    def joint(outs, parts, fb, fc):
        """Clears iff blend >= fb[lang] or cos >= fc[lang]: `top` becomes the larger margin, floors 0."""
        new = []
        for o in outs:
            tl, (bl, co) = o["top_language"], parts[o["id"]]
            o = dict(o)
            if tl in fb and tl in fc:
                o["top"] = max(bl - fb[tl], 100.0 * (co - fc[tl]))
            new.append(o)
        return new, {l: 0.0 for l in fb}

    def row(label, outs, cf, note=""):
        s = R.summarize(outs)
        g32 = fixed_floors_gates(base[R.REF], outs, rf32, cf)
        g8 = fixed_floors_gates(base[R.INT8], outs, shipped, cf)
        return {"label": label, "note": note, "floors": cf,
                "r10": R.bootstrap(R.pooled(outs, R.hit10, R.ho)[1]), "mrr": R.bootstrap(R.pooled(outs, R.rr, R.ho)[1]),
                "name": (s["name"]["r10"], s["name"]["mrr"]), "n": s["nl_heldout"]["n"],
                "vs": {"fp32": g32, "int8": g8},
                "gm434": {k: R.gm434(outs, cf, f) for k, f in (("nl", R.nl_ho_all), ("name", R.name_all))},
                "gm434_lang": {l: R.gm434(outs, cf, R.nl_ho_all, l) for l in R.LANGS}}

    def sig(r):
        """What a revert must reproduce: every reported cell."""
        return json.dumps({k: r[k] for k in ("r10", "mrr", "name", "gm434", "gm434_lang")}, default=str) + json.dumps(
            {t: {x: g[x] for x in ("r10", "mrr", "cw", "fa", "fa_lang", "pass")} for t, g in r["vs"].items()}, default=str)

    s4_fl = R.fit_floors(s4)
    s4row = row("S4 ce-minilm+int8 K=50 (beta=40), floor on blend", s4, s4_fl)
    rows = []

    # F1: rank by the blend, floor on the int8 cosine of the new top row (D6-fitted on it)
    f1, parts = build(floor_on="cos")
    f1_fl = R.fit_floors(f1)
    rows.append(row("F1 floor on int8 cosine of the reranked top", f1, f1_fl, f"cos floors D6-fitted {f1_fl}"))
    rev = row("F1 revert", *(lambda o: (o, R.fit_floors(o)))(build(floor_on="blend")[0]))
    print(f"F1 revert == S4: {sig(rev) == sig(s4row)}")

    # F2: blend floor OR the cosine floor (both D6-fitted), no tuning
    b, bparts = build()
    j, jf = joint(b, bparts, s4_fl, f1_fl)
    rows.append(row("F2 clears if blend >= its floor OR cosine >= its floor", j, jf))
    j0, jf0 = joint(b, bparts, s4_fl, {l: math.inf for l in s4_fl})
    print(f"F2 revert (cos floor = inf) == S4: {sig(row('F2 revert', j0, jf0)) == sig(s4row)}")

    # F3: CE minimum c0, tuned on the FIT half: best FIT r@10, MRR with FIT-half FA <= int8's at shipped floors
    fa8 = fit_fa(base[R.INT8], shipped)
    print(f"\nF3 tuning on FIT (int8 FIT-half FA at shipped floors {100 * fa8:.1f}%):")
    grid = []
    for c0 in (-math.inf, -10.0, -8.0, -6.0, -4.0, -2.0, 0.0, 2.0):
        o, _ = build(c0)
        fl_ = R.fit_floors(o)
        s = R.summarize(o)["nl_fit"]
        fa = fit_fa(o, fl_)
        grid.append((c0, s["r10"], s["mrr"], fa, fl_))
        print(f"  c0 {c0:>5}: FIT r@10 {s['r10']:.3f}, MRR {s['mrr']:.3f}, FIT FA {100 * fa:.1f}%, floors "
              f"{' / '.join(f'{fl_[l]:.2f}' for l in R.LANGS)}")
    ok = [g for g in grid if g[3] <= fa8 + R.EPS]
    pick = max(ok, key=lambda g: (round(g[1], 9), round(g[2], 9), -g[0])) if ok else min(grid, key=lambda g: g[3])
    c0 = pick[0]
    print(f"  picked c0 {c0}{'' if ok else ' (no c0 meets the FA bound; lowest FIT FA)'}")
    o3, _ = build(c0)
    rows.append(row(f"F3 CE minimum c0={c0:g}", o3, R.fit_floors(o3)))
    o3r, _ = build(-math.inf)
    print(f"F3 revert (c0 = -inf) == S4: {sig(row('F3 revert', o3r, R.fit_floors(o3r))) == sig(s4row)}")

    # ---------------- S11: order-only rerank (F4, F4') ----------------
    int8row = row("jina int8 (baseline, shipped floors)", base[R.INT8], shipped)

    def build_f4(verdict, rerank=True):
        """Rows shown in the blend's order (int8's own order if not rerank), shipped floors, no refit.
        verdict "int8": top/top_language of int8's own un-reranked top-1 (the shipped verdict).
        verdict "reranked": int8 cosine and language of the reranked top-1."""
        outs = []
        for c in cs:
            for q in c.qids:
                lst = c.lists[ARM][q]
                k = min(K, len(lst))
                s = CE(c, q) + beta * COS(c, q) if rerank else -np.arange(k, dtype=np.float64)
                hits, _ = R.reranked(c, q, K, s, ARM)
                if verdict == "int8":
                    o = R.outcome(c, q, hits, lst[0][1] if lst else None)
                    o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
                else:
                    i = [n for n, _ in lst[:K]].index(hits[0][0])
                    o = R.outcome(c, q, hits, float(COS(c, q)[i]))
                outs.append(o)
        return outs

    f4 = build_f4("int8")
    f4p = build_f4("reranked")
    for lab, v in (("F4", "int8"), ("F4'", "reranked")):
        ctl = build_f4(v, rerank=False)
        same_outs = ctl == base[R.INT8]
        same_row = sig(row(f"{lab} rerank disabled", ctl, shipped)) == sig(int8row)
        print(f"{lab} rerank disabled == int8 baseline: outcomes {same_outs}, row {same_row}")
        if not (same_outs and same_row):
            sys.exit(f"STOP: {lab} control failed")
    by8 = {o["id"]: o for o in base[R.INT8]}
    print(f"F4 verdict == int8 verdict on every query: "
          f"{all(R.clears(o, shipped) == R.clears(by8[o['id']], shipped) for o in f4)}")
    rows.append(row("F4 blend order, verdict on int8's own top-1 cosine (shipped floors)", f4, shipped))
    rows.append(row("F4' blend order, verdict on int8 cosine of the reranked top (shipped floors)", f4p, shipped))

    # flips among queries whose verdict clears the floor: int8 top right -> reranked wrong, and the reverse
    def flip_table(title, variants):
        """Per language: int8 top right -> reranked top wrong / wrong -> right, among cleared positives."""
        flips = {}
        for lab, outs in variants:
            for grp, keep in (("NL held-out", lambda o: R.nl_ho_all(o) and o["positive"]),
                              ("name", lambda o: R.name_all(o) and o["positive"])):
                d = {}
                for o in outs:
                    if not keep(o) or not R.clears(o, shipped):
                        continue
                    b8 = by8[o["id"]]
                    was, now = b8["rank"] == 1, o["rank"] == 1
                    l = o["language"]
                    t = d.setdefault(l, [0, 0, 0])
                    t[0] += was and not now
                    t[1] += now and not was
                    t[2] += 1
                flips[(lab, grp)] = d
        FL = ["", title, "",
              "| variant | queries | " + " | ".join(R.LANGS) + " | total |", "|---|---|" + "---|" * (len(R.LANGS) + 1)]
        for (lab, grp), d in flips.items():
            tot = [sum(d.get(l, [0, 0, 0])[j] for l in R.LANGS) for j in range(3)]
            FL.append(f"| {lab} | {grp} | " + " | ".join(
                "{} / {} (n={})".format(*d.get(l, [0, 0, 0])) for l in R.LANGS) + " | {} / {} (n={}) |".format(*tot))
        return FL

    FL = flip_table("S11 flips among positives the verdict clears (int8 top right -> reranked top wrong / "
                    "wrong -> right; n = cleared positives)", (("F4", f4), ("F4'", f4p)))
    print("\n".join(FL))

    # ---------------- tables ----------------
    def render(allr):
        ci = lambda t: f"{t[0]:.3f} [{t[1]:.3f}, {t[2]:.3f}]"
        L = ["| variant | held-out NL r@10 [lo, hi] | held-out NL MRR [lo, hi] | name r@10 / MRR |", "|---|---|---|---|"]
        for r in allr:
            L.append(f"| {r['label']} | {ci(r['r10'])} | {ci(r['mrr'])} | {r['name'][0]:.3f} / {r['name'][1]:.3f} |")
        for tag, title in (("fp32", "jina fp32 at its D6 floors"), ("int8", "shipped jina int8 at its shipped floors")):
            L += ["", f"D9 gates vs {title}", "",
                  "| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |",
                  "|---|---|---|---|---|---|---|---|"]
            for r in allr:
                g = r["vs"][tag]
                fails = [q for q in ("Q1", "Q2", "Q3", "Q4", "Q5") if not g["pass"][q]]
                up = lambda t: "-" if t is None else f"{R.pts(t[0])} [{R.pts(t[2])}]"
                L.append(f"| {r['label']} | {R.pts(g['r10'][0])} [{R.pts(g['r10'][1])}] | {g['mrr'][0]:+.3f} [{g['mrr'][1]:+.3f}] | "
                         f"{g['q3_worst'][0]} {R.pts(g['q3_worst'][1])} | {up(g['cw'])} | {up(g['fa'])} | "
                         f"{'pass' if not fails else '**' + ','.join(fails) + '**'} | "
                         + ", ".join(f"{l[:2]} {R.pts(d)} n={n}" for l, (d, n) in g["fa_lang"].items()) + " |")
        c434 = lambda t: "-" if not t[1] else f"{100 * t[0] / t[1]:.0f}% ({t[0]}/{t[1]})"
        L += ["", "GM-434 columns (option a), at each row's own floors", "",
              "| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |",
              "|---|---|---|---|---|---|---|---|"]
        for r in allr:
            fl, g = r["floors"], r["gm434"]
            L.append(f"| {r['label']} | {' / '.join(f'{fl[l]:.2f}' for l in R.LANGS)} | {c434(g['nl']['misled'])} | "
                     f"{c434(g['nl']['cw_pos'])} | {c434(g['nl']['cw_abs'])} | {c434(g['name']['misled'])} | "
                     f"{c434(g['name']['cw_pos'])} | {c434(g['name']['cw_abs'])} |")
        L += ["", "GM-434 per language, held-out NL (misled / CW pos / CW absent)", "",
              "| variant | " + " | ".join(R.LANGS) + " |", "|---|" + "---|" * len(R.LANGS)]
        for r in allr:
            gl = r["gm434_lang"]
            L.append(f"| {r['label']} | " + " | ".join(
                f"{c434(gl[l]['misled'])} / {c434(gl[l]['cw_pos'])} / {c434(gl[l]['cw_abs'])}" for l in R.LANGS) + " |")
        return L

    base_rows = [row("jina fp32 (baseline)", base[R.REF], rf32), int8row]
    allr = base_rows + [s4row] + rows
    L = render(allr)
    L += FL
    print("\n" + "\n".join(L))
    if a.table:
        a.table.write_text("\n".join(L) + "\n")

    # ---------------- S14: F4 at smaller K ----------------
    # CE(c, q) and COS(c, q) cover int8's top-50 (all cached); a smaller K is their prefix.
    fit_keys = [(c, q) for c in cs for q in c.qids if R.fit_nl(c.queries[q])]

    def blend(c, q, k, b):
        return CE(c, q)[:k] + b * COS(c, q)[:k]

    def tune_beta_at(k):
        """S4's objective at K=k: pooled FIT-half NL r@10, then MRR, then the smaller beta."""
        best = None
        for b in R.BETAS:
            g10, gm = {}, {}
            for c, q in fit_keys:
                hits, _ = R.reranked(c, q, k, blend(c, q, k, b), ARM)
                r = R.first_rank(c, q, hits)
                lang = c.queries[q]["language"]
                g10.setdefault(lang, []).append(1.0 if r and r <= 10 else 0.0)
                gm.setdefault(lang, []).append(1.0 / r if r else 0.0)
            key = (round(R.pooled_mean(g10), 9), round(R.pooled_mean(gm), 9), -b)
            if best is None or key > best[0]:
                best = (key, b)
        return best[1], best[0]

    def f4_at(k, b, rerank=True):
        """F4 at K=k: int8's top-k in the blend's order, verdict on int8's own top-1 cosine."""
        outs = []
        for c in cs:
            for q in c.qids:
                lst = c.lists[ARM][q]
                n = min(k, len(lst))
                sc_ = blend(c, q, n, b) if rerank else -np.arange(n, dtype=np.float64)
                hits, _ = R.reranked(c, q, k, sc_, ARM)
                o = R.outcome(c, q, hits, lst[0][1] if lst else None)
                o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
                outs.append(o)
        return outs

    print("\n== S14: F4 at K=20 / 30 / 50 ==")
    f4row = next(r for r in rows if r["label"].startswith("F4 blend order"))
    c50 = f4_at(50, 40.0)
    ok50 = c50 == f4 and sig(row("F4 K=50 beta=40", c50, shipped)) == sig(f4row)
    print(f"control: F4 at K=50 beta=40 == S11 F4 row: outcomes {c50 == f4}, row {ok50}")
    if not ok50:
        sys.exit("STOP: S14 K=50 control failed")
    rows14, var14, ceil = [], [], {}
    for k in (20, 30, 50):
        ceil[k] = R.pooled(base[R.INT8], R.hit_at(k), R.ho)
        n_in = sum(sum(v) for v in ceil[k][1].values())
        n_all = sum(len(v) for v in ceil[k][1].values())
        print(f"int8 recall@{k} held-out NL (pooled): {ceil[k][0]:.3f} ({n_in:g}/{n_all}); per language "
              + ", ".join(f"{l} {sum(v) / len(v):.3f}" for l, v in sorted(ceil[k][1].items())))
        bk, key = tune_beta_at(k)
        print(f"K={k}: beta re-tuned on FIT {bk:g} (fit r@10 {key[0]:.3f}, MRR {key[1]:.3f})")
        for b in sorted({bk, 40.0}):
            ctl = f4_at(k, b, rerank=False)
            same = ctl == base[R.INT8] and sig(row("off", ctl, shipped)) == sig(int8row)
            print(f"  control K={k} beta={b:g} rerank disabled == int8: {same}")
            if not same:
                sys.exit(f"STOP: S14 K={k} rerank-disabled control failed")
            outs = c50 if (k, b) == (50, 40.0) else f4_at(k, b)
            tag = "FIT-tuned" if b == bk else "fixed"
            if b == bk and b == 40.0:
                tag = "FIT-tuned = fixed"
            lab = f"F4 K={k} beta={b:g} ({tag})"
            rows14.append(row(lab, outs, shipped))
            var14.append((lab, outs))
    L14 = render(base_rows + rows14)
    L14 += ["", "int8 recall@K on held-out NL (the ceiling a K-row rerank can reach for r@10 and MRR), pooled over "
            "languages", "", "| K | pooled | " + " | ".join(R.LANGS) + " |", "|---|---|" + "---|" * len(R.LANGS)]
    for k, (m, g) in ceil.items():
        L14.append(f"| {k} | {m:.3f} | " + " | ".join(
            f"{sum(g[l]) / len(g[l]):.3f} ({sum(g[l]):g}/{len(g[l])})" for l in R.LANGS) + " |")
    L14 += flip_table("S14 flips among positives the verdict clears (int8 top right -> reranked top wrong / "
                      "wrong -> right; n = cleared positives)", var14)
    print("\n" + "\n".join(L14))
    if a.table14:
        a.table14.write_text("\n".join(L14) + "\n")

if __name__ == "__main__":
    main()
