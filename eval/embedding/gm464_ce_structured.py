#!/usr/bin/env python3
"""GM-464 S1: F4 (order-only ce-minilm rerank of int8's top-K) on the structured text.

GM-443 S14 measured F4 at K=30, beta=80 with the cross-encoder fed the old
full text (doc + signature) and int8's top-30 from the full-text vectors.
3.17.0 embeds the structured text (ADR 0012), so both inputs changed. This
re-runs F4 on the stored int8-structured runs (<work>/runs-gm465/...) with
the cross-encoder fed the structured text, at the shipped (structured) floors.

Cross-encoder scores are cached per (query, node text) in
<work>/rerank_cache/gm464-ce-minilm.<corpus>.<key>.json ({qid: {sha16(text): logit}}),
seeded from rerank_eval.py's full-text pair cache (a pair's logit depends
only on the query and the text). Pairs the cache lacks are scored.

Arms:
  - fp32 (full text) at its D6 floors, int8 full text at the old floors,
    int8-structured (no rerank) at the shipped floors: baselines;
  - F4s: int8-structured top-K in the order ce(structured text) + beta * cos,
    verdict = int8-structured's own top-1 cosine at the shipped floors;
  - F4s-full: same top-K, but the cross-encoder fed the full text.

Controls (any failure is a STOP):
  C1  the Python structured-text port reproduces the Rust fixture
      (core/src/embedding/testdata/structured_text.json);
  C2  seeded cache values: a sample of pairs is rescored and must agree;
  C3  this script's F4 path on the old int8 lists, the old cache and the old
      floors reproduces GM-443 S14's F4 K=30 beta=80 row;
  C4  F4s with the rerank disabled equals int8-structured on every outcome
      and every reported cell.

usage: gm464_ce_structured.py --work <eval/embedding/work> [--k 30] [--table out.md]
       [--dry-run CORPUS]  (one corpus, no controls that need all corpora)
"""
import argparse
import hashlib
import json
import math
import os
import random
import re
import sqlite3
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402
from gm423_token_lengths import form_text  # noqa: E402
from rerank_q5_diag import fixed_floors_gates  # noqa: E402

NAME = "ce-minilm"
ST = "jina-v2-base-code-int8-structured"
OLD_FLOORS = {"go": 0.57, "python": 0.57, "rust": 0.55, "typescript": 0.53}  # similarity.rs before 3.17.0
PROGRESS = Path.home() / ".claude/progress/GM-464-S1.txt"
T0 = time.time()


def progress(stage, done, total):
    el = time.time() - T0
    pct = 100.0 * done / total if total else 100.0
    eta = el / done * (total - done) if done else float("nan")
    la = " ".join(f"{x:.2f}" for x in os.getloadavg())
    PROGRESS.parent.mkdir(parents=True, exist_ok=True)
    PROGRESS.write_text(f"GM-464 S1 {stage}: {done}/{total} ({pct:.0f}%) elapsed {el:.0f}s eta {eta:.0f}s load {la}\n")


def tsha(t):
    return hashlib.sha256(t.encode()).hexdigest()[:16]


def c1_fixture():
    cases = json.loads((HERE.parents[1] / "core/src/embedding/testdata/structured_text.json").read_text())
    bad = [c["rule"] for c in cases
           if form_text("structured", c["doc"], c["signature"], R.text_to_embed(c["doc"], c["signature"])) != c["expected"]]
    print(f"C1 Python structured port vs Rust fixture: {len(cases) - len(bad)}/{len(cases)} equal")
    if bad:
        sys.exit(f"STOP: C1 failed on {bad}")


def add_structured(c, work):
    """int8-structured lists and the structured text of every node."""
    d = work / "runs-gm465" / ST / c.name
    man = json.loads((d / "manifest.json").read_text())
    for key, want in (("snapshotSha256", c.snapshot_sha), ("nodeIdsSha256", c.node_ids_sha), ("queryIds", c.all_qids)):
        if man[key] != want:
            sys.exit(f"STOP: {ST}/{c.name} {key} differs from the fp32 run")
    lst = {}
    for line in (d / "rankings.jsonl").read_text().splitlines():
        r = json.loads(line)
        if set(r["expected"]) != c.expected[r["id"]]:
            sys.exit(f"STOP: {ST} {r['id']} expected set differs")
        lst[r["id"]] = [(h[0], h[1]) for h in r["hits"]]
    c.lists[ST] = lst
    con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
    c.stext = {}
    for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes ORDER BY id"):
        if nid in c.text:
            c.stext[nid] = form_text("structured", doc, sig, c.text[nid])
    con.close()


class Cache:
    def __init__(self, work, cs, model_sha):
        self.work, self.model_sha = work, model_sha
        self.key = hashlib.sha256(json.dumps([model_sha, R.MAX_TOKENS, "gm464 text-keyed v1"]).encode()).hexdigest()[:16]
        self.d, self.seeded = {}, {}
        for c in cs:
            f = self.file(c)
            d = json.loads(f.read_text()) if f.exists() else {}
            old_key = hashlib.sha256(json.dumps([model_sha, c.snapshot_sha, c.node_ids_sha, R.MAX_TOKENS]).encode()).hexdigest()[:16]
            old = json.loads((work / "rerank_cache" / f"{NAME}.{c.name}.pairs.{old_key}.json").read_text())
            seeded = []
            for q, m in old.items():
                have = d.setdefault(q, {})
                for n, s in m.items():
                    h = tsha(c.text[n])
                    if h not in have:
                        have[h] = s
                        seeded.append((q, n))
            self.d[c.name], self.seeded[c.name] = d, seeded
            self.old = getattr(self, "old", {})
            self.old[c.name] = old

    def file(self, c):
        return self.work / "rerank_cache" / f"gm464-{NAME}.{c.name}.{self.key}.json"

    def save(self, c):
        tmp = self.file(c).with_suffix(".tmp")
        tmp.write_text(json.dumps(self.d[c.name]))
        tmp.replace(self.file(c))

    def get(self, c, q, text):
        return self.d[c.name][q][tsha(text)]


def need_pairs(cs, cache, k):
    """(corpus, qid, text) pairs the cache lacks for ST's top-k, structured and full text."""
    need = {}
    for c in cs:
        for q in c.qids:
            have = cache.d[c.name].setdefault(q, {})
            for n, _ in c.lists[ST][q][:k]:
                for t in (c.stext[n], c.text[n]):
                    if tsha(t) not in have:
                        need.setdefault((c.name, q), {})[tsha(t)] = t
    return need


def score_missing(cs, cache, need, ce):
    byname = {c.name: c for c in cs}
    total = sum(len(v) for v in need.values())
    done, last, step = 0, time.time(), max(1, total // 10)
    nextmark = step
    progress("CE scoring", 0, total)
    dirty = set()
    for (cn, q), m in need.items():
        c = byname[cn]
        hs, ts = list(m), list(m.values())
        s = ce.score(c.queries[q]["text"], ts)
        cache.d[cn][q].update(zip(hs, map(float, s)))
        dirty.add(cn)
        done += len(ts)
        if done >= nextmark or time.time() - last > 300:
            progress("CE scoring", done, total)
            for d_ in dirty:
                cache.save(byname[d_])
            dirty.clear()
            last = time.time()
            while nextmark <= done:
                nextmark += step
    for d_ in dirty:
        cache.save(byname[d_])
    progress("CE scoring", done, total)
    return total


def c2_seeded(cs, cache, ce, n=120):
    rng = random.Random(464)
    pool = [(c, q, nid) for c in cs for q, nid in cache.seeded[c.name] if q in c.queries]
    if not pool:
        print("C2 seeded pairs rescored: none seeded (cache already existed)")
        pool = [(c, q, nid) for c in cs for q in c.qids for nid in list(cache.old[c.name].get(q, {}))[:3]]
    sample = rng.sample(pool, min(n, len(pool)))
    diffs = []
    for c, q, nid in sample:
        got = float(ce.score(c.queries[q]["text"], [c.text[nid]])[0])
        diffs.append(abs(got - cache.old[c.name][q][nid]))
    m = max(diffs)
    print(f"C2 {len(sample)} cached full-text pairs rescored: max|diff| {m:.2e}")
    if m > 1e-3:
        sys.exit("STOP: C2 rescored pairs disagree with the GM-443 cache")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--k", type=int, default=30)
    ap.add_argument("--table", type=Path)
    ap.add_argument("--dry-run", metavar="CORPUS")
    a = ap.parse_args()
    toml = (HERE / "variants.toml").read_text()
    for key in ("bootstrap_seed", "bootstrap_resamples"):
        R.SETTINGS[key] = int(re.search(rf"^{key}\s*=\s*(\d+)", toml, re.M).group(1))
    K = a.k
    c1_fixture()
    corpora = [p.name for p in sorted((a.work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    if a.dry_run:
        corpora = [a.dry_run]
    cs = [R.Corpus(a.work, HERE, c, 3 if a.dry_run else 0) for c in corpora]
    for c in cs:
        add_structured(c, a.work)
    n_changed = sum(sum(c.stext[n] != c.text[n] for n in c.stext) for c in cs)
    print(f"nodes whose structured text differs from the full text: {n_changed}/{sum(len(c.stext) for c in cs)}")

    ce = R.CrossEncoder(a.work, NAME)
    cache = Cache(a.work, cs, ce.files["model.onnx"])
    need = need_pairs(cs, cache, K)
    print(f"pairs to score for {ST} top-{K} (structured + full text): {sum(len(v) for v in need.values())} "
          f"over {len(need)} queries; threads {ce.threads}")
    c2_seeded(cs, cache, ce)
    score_missing(cs, cache, need, ce)
    if a.dry_run:
        q = cs[0].qids[0]
        print("dry run OK:", q, [round(cache.get(cs[0], q, cs[0].stext[n]), 3) for n, _ in cs[0].lists[ST][q][:5]])
        PROGRESS.write_text("GM-464 S1 dry run done\n")
        return

    shipped = R.shipped_floors(HERE.parents[1])
    print(f"shipped floors (structured, similarity.rs): {shipped}; old floors {OLD_FLOORS}")

    def base(arm):
        return [R.outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                for c in cs for q in c.qids]

    B = {arm: base(arm) for arm in (R.REF, R.INT8, ST)}
    rf32 = R.fit_floors(B[R.REF])

    def f4(arm, k, beta, text=None, rerank=True):
        """arm's top-k in the order ce(text) + beta * cos; verdict on arm's own top-1 cosine."""
        outs = []
        for c in cs:
            for q in c.qids:
                lst = c.lists[arm][q]
                n = min(k, len(lst))
                if rerank:
                    if text is None:  # GM-443's pair cache, keyed by node id
                        ce_ = np.array([cache.old[c.name][q][nid] for nid, _ in lst[:n]])
                    else:
                        ce_ = np.array([cache.get(c, q, text(c)[nid]) for nid, _ in lst[:n]])
                    s = ce_ + beta * np.array([x for _, x in lst[:n]])
                else:
                    s = -np.arange(n, dtype=np.float64)
                hits, _ = R.reranked(c, q, k, s, arm)
                o = R.outcome(c, q, hits, lst[0][1] if lst else None)
                o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
                outs.append(o)
        return outs

    def row(label, outs, cf, ref8, rf8):
        s = R.summarize(outs)
        return {"label": label, "floors": cf,
                "r10": R.bootstrap(R.pooled(outs, R.hit10, R.ho)[1]), "mrr": R.bootstrap(R.pooled(outs, R.rr, R.ho)[1]),
                "name": (s["name"]["r10"], s["name"]["mrr"]),
                "vs": {"fp32": fixed_floors_gates(B[R.REF], outs, rf32, cf),
                       "int8": fixed_floors_gates(ref8, outs, rf8, cf)},
                "gm434": {k_: R.gm434(outs, cf, f) for k_, f in (("nl", R.nl_ho_all), ("name", R.name_all))},
                "gm434_lang": {l: R.gm434(outs, cf, R.nl_ho_all, l) for l in R.LANGS}}

    def sig(r):
        return json.dumps({k_: r[k_] for k_ in ("r10", "mrr", "name", "gm434", "gm434_lang")}, default=str) + json.dumps(
            {t: {x: g[x] for x in ("r10", "mrr", "cw", "fa", "fa_lang", "pass")} for t, g in r["vs"].items()}, default=str)

    def cells(r):
        g8, g32 = r["vs"]["int8"], r["vs"]["fp32"]
        return (round(r["r10"][0], 3), round(r["mrr"][0], 3), R.pts(g8["r10"][0]), R.pts(g8["r10"][1]),
                f"{g8['mrr'][0]:+.3f}", f"{g8['mrr'][1]:+.3f}", R.pts(g32["r10"][0]), R.pts(g32["r10"][1]),
                f"{g32['mrr'][0]:+.3f}", f"{g32['mrr'][1]:+.3f}")

    # ---- C3: GM-443 S14's F4 K=30 beta=80 through this path (old int8, old cache, old floors) ----
    old = f4(R.INT8, 30, 80.0)
    got = cells(row("C3", old, OLD_FLOORS, B[R.INT8], OLD_FLOORS))
    want = (0.691, 0.465, "+4.6", "+2.1", "+0.046", "+0.021", "+5.8", "+2.4", "+0.042", "+0.014")
    print(f"C3 GM-443 S14 F4 K=30 beta=80 reproduced: {got}\n   {'MATCH' if got == want else 'MISMATCH, want ' + str(want)}")
    if got != want:
        sys.exit("STOP: C3 failed")

    # ---- C4: rerank disabled == int8-structured ----
    r8s = row("int8-structured (no rerank, shipped floors)", B[ST], shipped, B[ST], shipped)
    ctl = f4(ST, K, 80.0, rerank=False)
    ok = ctl == B[ST] and sig(row("off", ctl, shipped, B[ST], shipped)) == sig(r8s)
    print(f"C4 F4s rerank disabled == int8-structured on every outcome and cell: {ok}")
    if not ok:
        sys.exit("STOP: C4 failed")

    stx = lambda c: c.stext
    ftx = lambda c: c.text
    # ---- beta: FIT-half tuning (S4's objective) and a held-out sweep ----
    fit_keys = [(c, q) for c in cs for q in c.qids if R.fit_nl(c.queries[q])]

    def fit_obj(text, b):
        g10, gm = {}, {}
        for c, q in fit_keys:
            lst = c.lists[ST][q]
            n = min(K, len(lst))
            s = np.array([cache.get(c, q, text(c)[nid]) for nid, _ in lst[:n]]) + b * np.array([x for _, x in lst[:n]])
            hits, _ = R.reranked(c, q, K, s, ST)
            r = R.first_rank(c, q, hits)
            lang = c.queries[q]["language"]
            g10.setdefault(lang, []).append(1.0 if r and r <= 10 else 0.0)
            gm.setdefault(lang, []).append(1.0 / r if r else 0.0)
        return round(R.pooled_mean(g10), 9), round(R.pooled_mean(gm), 9)

    sweep = ["", f"Beta sweep at K={K} (CE on structured text): FIT-half objective and held-out point estimates", "",
             "| beta | FIT NL r@10 | FIT NL MRR | held-out NL r@10 | held-out NL MRR | name r@10 / MRR |",
             "|---|---|---|---|---|---|"]
    best = {}
    for text, tag in ((stx, "structured"), (ftx, "full")):
        bb = None
        for b in R.BETAS:
            key = fit_obj(text, b)
            if bb is None or key + (-b,) > bb[0]:
                bb = (key + (-b,), b)
            if tag == "structured":
                s = R.summarize(f4(ST, K, b, text))
                sweep.append(f"| {b:g} | {key[0]:.3f} | {key[1]:.3f} | {s['nl_heldout']['r10']:.3f} | "
                             f"{s['nl_heldout']['mrr']:.3f} | {s['name']['r10']:.3f} / {s['name']['mrr']:.3f} |")
        best[tag] = bb[1]
        print(f"FIT-tuned beta, CE on {tag} text: {bb[1]:g} (FIT r@10 {bb[0][0]:.3f}, MRR {bb[0][1]:.3f})")

    r32 = row("jina fp32, full text (baseline, D6 floors)", B[R.REF], rf32, B[ST], shipped)
    r8 = row("jina int8, full text (pre-3.17, old floors)", B[R.INT8], OLD_FLOORS, B[ST], shipped)
    rows, variants = [r32, r8, r8s], []
    betas = sorted({40.0, 80.0, best["structured"]})
    for b in betas:
        tag = " (FIT-tuned)" if b == best["structured"] else ""
        lab = f"F4s K={K} beta={b:g}{tag}, CE on structured text"
        o = f4(ST, K, b, stx)
        rows.append(row(lab, o, shipped, B[ST], shipped))
        variants.append((lab, o))
    lab = f"F4s K={K} beta=80, CE on full text"
    o = f4(ST, K, 80.0, ftx)
    rows.append(row(lab, o, shipped, B[ST], shipped))
    variants.append((lab, o))
    lab = "GM-443 F4 K=30 beta=80 (full-text int8 + full-text CE, old floors; C3)"
    rows.append(row(lab, old, OLD_FLOORS, B[ST], shipped))

    by8 = {o_["id"]: o_ for o_ in B[ST]}
    vf = all(R.clears(o_, shipped) == R.clears(by8[o_["id"]], shipped) for _, v in variants for o_ in v)
    print(f"F4s verdict == int8-structured verdict on every query: {vf}")

    L = render(rows)
    L += ["", "int8-structured recall@K on held-out NL (the ceiling of a K-row rerank), vs int8 full text", "",
          "| arm | K | pooled | " + " | ".join(R.LANGS) + " |", "|---|---|---|" + "---|" * len(R.LANGS)]
    for arm, tag in ((R.INT8, "int8 full"), (ST, "int8 structured")):
        m, g = R.pooled(B[arm], R.hit_at(K), R.ho)
        L.append(f"| {tag} | {K} | {m:.3f} | " + " | ".join(
            f"{sum(g[l]) / len(g[l]):.3f} ({sum(g[l]):g}/{len(g[l])})" for l in R.LANGS) + " |")
    L += flips(variants, by8, shipped)
    L += sweep
    # paired: CE on full text minus CE on structured text, same top-30, beta 80
    vs = dict(variants)
    a_ = vs[f"F4s K={K} beta=80{' (FIT-tuned)' if best['structured'] == 80.0 else ''}, CE on structured text"]
    b_ = vs[f"F4s K={K} beta=80, CE on full text"]
    g = R.gates(a_, b_, R.ho, rf=shipped)
    L += ["", f"Paired, held-out NL: CE on full text minus CE on structured text (K={K}, beta 80, same top-K)", "",
          "| Δr@10 [lo, hi] | ΔMRR [lo, hi] |", "|---|---|",
          f"| {R.pts(g['r10'][0])} [{R.pts(g['r10'][1])}, {R.pts(g['r10'][2])}] | "
          f"{g['mrr'][0]:+.3f} [{g['mrr'][1]:+.3f}, {g['mrr'][2]:+.3f}] |"]
    print("\n" + "\n".join(L))
    if a.table:
        a.table.write_text("\n".join(L) + "\n")
    PROGRESS.write_text(f"GM-464 S1 done in {time.time() - T0:.0f}s\n")


def flips(variants, by8, floors):
    FL = ["", "Flips among positives the verdict clears (int8-structured top right -> reranked top wrong / "
          "wrong -> right; n = cleared positives)", "",
          "| variant | queries | " + " | ".join(R.LANGS) + " | total |", "|---|---|" + "---|" * (len(R.LANGS) + 1)]
    for lab, outs in variants:
        for grp, keep in (("NL held-out", lambda o: R.nl_ho_all(o) and o["positive"]),
                          ("name", lambda o: R.name_all(o) and o["positive"])):
            d = {}
            for o in outs:
                if not keep(o) or not R.clears(o, floors):
                    continue
                was, now = by8[o["id"]]["rank"] == 1, o["rank"] == 1
                t = d.setdefault(o["language"], [0, 0, 0])
                t[0] += was and not now
                t[1] += now and not was
                t[2] += 1
            tot = [sum(d.get(l, [0, 0, 0])[j] for l in R.LANGS) for j in range(3)]
            FL.append(f"| {lab} | {grp} | " + " | ".join(
                "{} / {} (n={})".format(*d.get(l, [0, 0, 0])) for l in R.LANGS) + " | {} / {} (n={}) |".format(*tot))
    return FL


def render(allr):
    ci = lambda t: f"{t[0]:.3f} [{t[1]:.3f}, {t[2]:.3f}]"
    L = ["| variant | held-out NL r@10 [lo, hi] | held-out NL MRR [lo, hi] | name r@10 / MRR |", "|---|---|---|---|"]
    for r in allr:
        L.append(f"| {r['label']} | {ci(r['r10'])} | {ci(r['mrr'])} | {r['name'][0]:.3f} / {r['name'][1]:.3f} |")
    for tag, title in (("fp32", "jina fp32 (full text) at its D6 floors"),
                       ("int8", "int8-structured (shipped) at the shipped floors")):
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
    return L


if __name__ == "__main__":
    main()
