#!/usr/bin/env python3
"""Do fused retrievers match or beat jina-v2-base-code fp32?

Stored data only: the rankings, vectors and snapshots GM-398's
`g-mesh embed-eval run` wrote. Nothing is embedded again.

Ported, not reimplemented (sources named per function):
- loading and recall/MRR: eval/embedding/recall_at_k.py,
  itself `embed_eval.rs` `load_arm` and metrics.rs `hit_at`/`reciprocal_rank`;
- floors, false alarm, SplitMix64 bootstrap: eval/embedding/q5_floor_sensitivity.py,
  itself metrics.rs `fit_floors`, `false_alarm_indicator`,
  `paired_at_own_floors`, `bootstrap`; the bootstrap here is the same generator
  vectorised (SplitMix64's n-th output is a closed form of n);
- confident-wrong and the D9 gates: metrics.rs `confident_wrong`,
  decision.rs `quality_gates`.

Step 0 (control) reproduces GM-398's pooled r@1/r@5/r@10 [bounds]/MRR [bounds],
confident-wrong, floors, held-out false alarm and the D9 verdict rows for
jina fp32, gte-small, bge-small and bm25, plus int8's Q5 row (bootstrap check),
and three fusion-code controls; any mismatch stops the script.

usage: fusion_eval.py --runs <eval/embedding/work/runs> [--work <eval/embedding/work>]
                      [--corpora a,b] [--json out.json]
"""
import argparse
import hashlib
import json
import math
import re
import sqlite3
import sys
from pathlib import Path

import numpy as np

KEPT_HITS = 100
MAX_FALSE_ALARM = 0.03
EPS = 1e-12
REF = "jina-v2-base-code-fp32"
GTE, BGE, BM25, INT8 = "gte-small", "bge-small-en-v1.5", "bm25", "jina-v2-base-code-int8"
EMBEDDERS = [REF, GTE, BGE]
LANGS = ["go", "python", "rust", "typescript"]
SHORT = {REF: "jina", GTE: "gte", BGE: "bge", BM25: "bm25"}
WEIGHTS = [round(0.05 * i, 2) for i in range(1, 20)]  # embedder weight grid, fit half only


# --- loading (recall_at_k.py load_queries / load_arm; embed_eval.rs load_nodes) ---
def held_out(qid, mechanical):
    return not mechanical and hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load_queries(eval_dir, corpus):
    out, hashes = {}, []
    for rel, mech in [(f"queries/{corpus}.jsonl", False), (f"queries/mechanical/{corpus}.jsonl", True)]:
        p = eval_dir / rel
        if not p.exists():
            continue
        data = p.read_bytes()
        hashes.append([rel, hashlib.sha256(data).hexdigest()])
        for line in data.decode().splitlines():
            if line.strip():
                q = json.loads(line)
                q["mechanical"] = mech
                q["positive"] = q["kind"] == "positive"
                q["held_out"] = held_out(q["id"], mech)
                out[q["id"]] = q
    return out, hashes


def load_nodes(work, corpus, want_sha):
    """Candidate node ids in vectors.bin order (nodes with embeddable text, ORDER BY id)."""
    con = sqlite3.connect(f"file:{work / (corpus + '.sqlite')}?mode=ro", uri=True)
    ids, lang = [], {}
    for nid, language, doc, sig in con.execute(
            "SELECT id, language, docComment, signature FROM nodes ORDER BY id"):
        lang[nid] = language
        if (doc or "").strip() or (sig or "").strip():  # pipeline.rs text_to_embed is Some
            ids.append(nid)
    h = hashlib.sha256()
    for nid in ids:
        h.update(nid.encode() + b"\n")
    if h.hexdigest() != want_sha:
        sys.exit(f"STOP: {corpus} candidate ids do not hash to the manifest's nodeIdsSha256")
    return ids, lang


def read_vectors(path, count, dim):
    a = np.fromfile(path, dtype="<f4")
    if a.size != count * dim:
        sys.exit(f"STOP: {path} holds {a.size} floats, expected {count} x {dim}")
    return a.reshape(count, dim)


def unit(m):
    m = m.astype(np.float64)
    n = np.linalg.norm(m, axis=1, keepdims=True)
    n[n == 0] = 1.0
    return m / n


class Corpus:
    def __init__(self, runs, work, eval_dir, corpus, arms):
        self.name = corpus
        self.lists = {}  # arm -> qid -> [(id, score)]
        self.expected = {}
        mans = {}
        for arm in arms:
            d = runs / arm / corpus
            man = json.loads((d / "manifest.json").read_text())
            mans[arm] = man
            if arm == REF:
                self.queries, hashes = load_queries(eval_dir, corpus)
                if hashes != man["queryFiles"]:
                    sys.exit(f"STOP: query files of {corpus} changed since {d} was run (D3 freeze)")
            lst = {}
            for line in (d / "rankings.jsonl").read_text().splitlines():
                r = json.loads(line)
                lst[r["id"]] = [(h[0], h[1]) for h in r["hits"]]
                self.expected.setdefault(r["id"], set(r["expected"]))
                if set(r["expected"]) != self.expected[r["id"]]:
                    sys.exit(f"STOP: {arm} {r['id']} expected set differs between arms")
            self.lists[arm] = lst
        for arm, man in mans.items():
            for key in ("snapshotSha256", "nodeIdsSha256", "queryFiles"):
                if man[key] != mans[REF][key]:
                    sys.exit(f"STOP: {arm}/{corpus} {key} differs from {REF}")
        self.ids, self.node_lang = load_nodes(work, corpus, mans[REF]["nodeIdsSha256"])
        self.index = {nid: i for i, nid in enumerate(self.ids)}
        self.qids = mans[REF]["queryIds"]
        # Full cosine matrices (queries x nodes) of the embedders, float64 of unit rows.
        self.cos = {}
        for arm in arms:
            if arm in EMBEDDERS:
                man = mans[arm]
                nv = unit(read_vectors(runs / arm / corpus / "vectors.bin", man["nodeCount"], man["dimension"]))
                qv = unit(read_vectors(runs / arm / corpus / "query_vectors.bin", len(self.qids), man["dimension"]))
                self.cos[arm] = qv @ nv.T
        self.qrow = {q: i for i, q in enumerate(self.qids)}


# --- ranking helpers ---------------------------------------------------------
def top_from_scores(scores, ids):
    """embed_eval.rs top_hits: score desc, then candidate order; keep 100."""
    order = np.lexsort((np.arange(len(scores)), -scores))[:KEPT_HITS]
    return [(ids[i], float(scores[i])) for i in order]


def top_from_dict(d, index):
    """Rank a {node id: score} map like top_hits (ties by candidate order)."""
    return sorted(d.items(), key=lambda kv: (-kv[1], index[kv[0]]))[:KEPT_HITS]


def rrf(lists, k, index):
    s = {}
    for lst in lists:
        for r, (nid, _) in enumerate(lst, 1):
            s[nid] = s.get(nid, 0.0) + 1.0 / (k + r)
    return top_from_dict(s, index)


def minmax(lst):
    hi, lo = lst[0][1], lst[-1][1]
    span = hi - lo
    return {nid: (1.0 if span == 0 else (sc - lo) / span) for nid, sc in lst}


def weighted(la, lb, w, index):
    a, b = minmax(la), minmax(lb)
    s = {nid: w * a.get(nid, 0.0) + (1 - w) * b.get(nid, 0.0) for nid in set(a) | set(b)}
    return top_from_dict(s, index)


def full_rank_list(c, arm, qid):
    """An embedder's ranking over every candidate (no top-100 cut)."""
    sc = c.cos[arm][c.qrow[qid]]
    order = np.lexsort((np.arange(len(sc)), -sc))
    return [(c.ids[i], float(sc[i])) for i in order]


# --- outcomes -----------------------------------------------------------------
def outcome(c, qid, hits, top_score):
    q = c.queries[qid]
    exp = c.expected[qid]
    rank = next((i + 1 for i, (nid, _) in enumerate(hits[:KEPT_HITS]) if nid in exp), None)
    return {"id": qid, "corpus": c.name, "language": q["language"], "positive": q["positive"],
            "mechanical": q["mechanical"], "held_out": q["held_out"], "rank": rank,
            "top": top_score, "top_language": c.node_lang.get(hits[0][0]) if hits else None}


def judge_score(c, arms, qid, nid):
    """Mean over `arms` of the embedder cosine of node `nid`: stored score when the node is
    in that arm's stored top-100, else the recomputed cosine."""
    vals = []
    for arm in arms:
        stored = dict(c.lists[arm][qid])
        vals.append(stored[nid] if nid in stored else float(c.cos[arm][c.qrow[qid], c.index[nid]]))
    return sum(vals) / len(vals)


# --- metrics.rs --------------------------------------------------------------
def pooled_mean(groups):
    means = [sum(v) / len(v) for _, v in sorted(groups.items()) if v]
    return sum(means) / len(means) if means else None


GAMMA = np.uint64(0x9E3779B97F4A7C15)


def splitmix_stream(seed, count):
    """rng.rs SplitMix64 next_u64(), outputs 1..count, vectorised."""
    with np.errstate(over="ignore"):
        z = np.uint64(seed) + np.arange(1, count + 1, dtype=np.uint64) * GAMMA
        z = (z ^ (z >> np.uint64(30))) * np.uint64(0xBF58476D1CE4E5B9)
        z = (z ^ (z >> np.uint64(27))) * np.uint64(0x94D049BB133111EB)
        return z ^ (z >> np.uint64(31))


def below(x, n):
    """(u128(x) * n) >> 64 for n < 2^32, in u64 arithmetic."""
    n = np.uint64(n)
    hi, lo = x >> np.uint64(32), x & np.uint64(0xFFFFFFFF)
    with np.errstate(over="ignore"):
        return ((hi * n + ((lo * n) >> np.uint64(32))) >> np.uint64(32)).astype(np.int64)


SETTINGS = {}


def bootstrap(groups):
    point = pooled_mean(groups)
    if point is None:
        return None
    gs = [np.array(v, dtype=np.float64) for _, v in sorted(groups.items()) if v]
    R, seed = SETTINGS["bootstrap_resamples"], SETTINGS["bootstrap_seed"]
    N = sum(len(g) for g in gs)
    draws = splitmix_stream(seed, R * N).reshape(R, N)
    s = np.zeros(R)
    off = 0
    for g in gs:
        n = len(g)
        s += g[below(draws[:, off:off + n], n)].sum(axis=1) / n
        off += n
    stats = np.sort(s / len(gs))
    tail = math.floor(0.05 * R)
    return (point, float(stats[tail]), float(stats[R - 1 - tail]))


def round_down_2(x):
    return math.floor(x * 100.0 + 1e-9) / 100.0


def fit_floors(outs):
    scores = {}
    for o in outs:
        if o["positive"] and (o["mechanical"] or not o["held_out"]) and o["rank"] == 1 and o["top"] is not None:
            scores.setdefault(o["language"], []).append(o["top"])
    floors = {}
    for l, s in scores.items():
        s = sorted(s)
        k = math.floor(MAX_FALSE_ALARM * len(s) + 1e-9)
        floors[l] = round_down_2(s[min(k, len(s) - 1)])
    return floors


def clears(o, floors):
    if o["top"] is None or o["top_language"] not in floors:
        return None
    return o["top"] >= floors[o["top_language"]]


def fa_indicator(o, floors):
    if not (o["positive"] and not o["mechanical"] and o["held_out"] and o["rank"] == 1):
        return None
    c = clears(o, floors)
    return None if c is None else (0.0 if c else 1.0)


def cw_indicator(o, floors):
    if o["mechanical"] or not o["held_out"]:
        return None
    c = clears(o, floors)
    if c is None:
        return None
    wrong = (c and o["rank"] != 1) if o["positive"] else c
    return 1.0 if wrong else 0.0


def rate(outs, fn, keep=lambda o: True):
    v = [fn(o) for o in outs if keep(o)]
    v = [x for x in v if x is not None]
    return (sum(v) / len(v)) if v else None


def paired_own(ref, rf, cand, cf, ind):
    by = {o["id"]: ind(o, rf) for o in ref}
    g = {}
    for o in cand:
        r, c = by.get(o["id"]), ind(o, cf)
        if r is not None and c is not None:
            g.setdefault(o["language"], []).append(c - r)
    return g


def hit10(o):
    return 1.0 if o["rank"] is not None and o["rank"] <= 10 else 0.0


def rr(o):
    return 1.0 / o["rank"] if o["rank"] is not None and o["rank"] <= KEPT_HITS else 0.0


def scored(o):
    return o["positive"] and not o["mechanical"]


def name_q(o):
    return o["positive"] and o["mechanical"]


def ho(o):
    return scored(o) and o["held_out"]


def fit_nl(o):
    return scored(o) and not o["held_out"]


def pooled(outs, fn, keep):
    g = {}
    for o in outs:
        if keep(o):
            g.setdefault(o["language"], []).append(fn(o))
    return pooled_mean(g), g


def gates(ref, cand, keep=scored, superiority=False):
    """decision.rs quality_gates (Cost role; Quality role's Q1/Q2 reported beside)."""
    by = {o["id"]: o for o in cand}
    r10, mrr = {}, {}
    for o in ref:
        if keep(o):
            c = by[o["id"]]
            r10.setdefault(o["language"], []).append(hit10(c) - hit10(o))
            mrr.setdefault(o["language"], []).append(rr(c) - rr(o))
    rf, cf = fit_floors(ref), fit_floors(cand)
    b = {"r10": bootstrap(r10), "mrr": bootstrap(mrr),
         "cw": bootstrap(paired_own(ref, rf, cand, cf, cw_indicator)),
         "fa": bootstrap(paired_own(ref, rf, cand, cf, fa_indicator))}
    fa_g = paired_own(ref, rf, cand, cf, fa_indicator)
    b["fa_lang"] = {l: (sum(v) / len(v), len(v)) for l, v in sorted(fa_g.items())}
    b["q3"] = {l: sum(v) / len(v) for l, v in sorted(r10.items())}
    worst = min(b["q3"].items(), key=lambda kv: kv[1])
    b["q3_worst"] = worst
    nan = (float("nan"),) * 3
    cw, fa = b["cw"] or nan, b["fa"] or nan
    b["pass"] = {
        "Q1": b["r10"][0] >= -0.02 - EPS and b["r10"][1] >= -0.05 - EPS,
        "Q2": b["mrr"][0] >= -0.02 - EPS and b["mrr"][1] >= -0.05 - EPS,
        "Q3": all(d >= -0.10 - EPS for d in b["q3"].values()),
        "Q4": cw[0] <= EPS and cw[2] <= 0.05 + EPS,
        "Q5": fa[0] <= EPS and fa[2] <= 0.05 + EPS,
        "Q1sup": b["r10"][1] > 0.0, "Q2sup": b["mrr"][1] > 0.0,
    }
    b["floors"] = cf
    return b


# --- report helpers -----------------------------------------------------------
def pts(x):
    return f"{100 * x:+.1f}"


def floors_s(fl):
    return " / ".join(f"{fl[l]:.2f}" if l in fl else "-" for l in LANGS)


def f3(x):
    return "-" if x is None else f"{x:.3f}"


def summarize(outs):
    s = {}
    for label, keep in [("nl", scored), ("nl_heldout", ho), ("nl_fit", fit_nl), ("name", name_q)]:
        s[label] = {"r10": pooled(outs, hit10, keep)[0], "mrr": pooled(outs, rr, keep)[0],
                    "n": sum(1 for o in outs if keep(o))}
    return s


# --- main ---------------------------------------------------------------------
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", required=True, type=Path)
    ap.add_argument("--work", type=Path)
    ap.add_argument("--eval-dir", type=Path, default=Path(__file__).resolve().parent)
    ap.add_argument("--corpora", default="")
    ap.add_argument("--json", type=Path)
    a = ap.parse_args()
    work = a.work or a.runs.parent
    toml = (a.eval_dir / "variants.toml").read_text()
    for k in ("bootstrap_seed", "bootstrap_resamples"):
        SETTINGS[k] = int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))

    corpora = [p.name for p in sorted((a.runs / REF).iterdir()) if (p / "rankings.jsonl").exists()]
    dry = bool(a.corpora)
    if dry:
        corpora = a.corpora.split(",")
    arms = EMBEDDERS + [BM25, INT8]
    cs = [Corpus(a.runs, work, a.eval_dir, c, arms) for c in corpora]
    print(f"corpora: {', '.join(corpora)}; bootstrap seed {SETTINGS['bootstrap_seed']}, "
          f"resamples {SETTINGS['bootstrap_resamples']}")

    # Unfused arms, straight from the stored lists.
    base = {arm: [outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                  for c in cs for q in c.qids] for arm in arms}

    # ---------------- 0. controls ----------------
    bad = []
    print("\n== 0. controls ==")
    # 0a. vectors reproduce the stored rankings (vector loading and node order are right).
    for arm in EMBEDDERS:
        same10 = tot = 0
        maxdiff = 0.0
        for c in cs:
            for q in c.qids:
                mine = top_from_scores(c.cos[arm][c.qrow[q]], c.ids)
                st = c.lists[arm][q]
                tot += 1
                same10 += [n for n, _ in mine[:10]] == [n for n, _ in st[:10]]
                maxdiff = max(maxdiff, max(abs(x[1] - y[1]) for x, y in zip(mine, st)))
        print(f"0a {arm}: recomputed top-10 order identical on {same10}/{tot} queries; "
              f"max |score diff| over top-100 {maxdiff:.2e}")
        if maxdiff > 1e-5 or same10 < 0.98 * tot:
            bad.append(f"0a {arm} vectors do not reproduce rankings")
    if not dry:
        want = {  # docs/results/gm-398-model-comparison.md, "Quality, pooled"
            REF: (0.335, 0.542, 0.633, 0.595, 0.670, 0.427, 0.395, 0.460, 47.9, 52.6, 26.1),
            GTE: (0.275, 0.478, 0.580, 0.540, 0.617, 0.373, 0.340, 0.406, 59.8, 61.9, 50.0),
            BGE: (0.258, 0.485, 0.557, 0.518, 0.595, 0.360, 0.327, 0.392, 64.0, 66.5, 52.2),
            BM25: (0.245, 0.400, 0.448, 0.407, 0.488, 0.320, None, None, 73.9, 72.6, 80.4),
        }
        want_fl = {REF: ([.56, .58, .56, .55], [18.8, 9.5, 14.3, 14.8]),
                   GTE: ([.86, .86, .85, .84], [25.0, 7.7, 0.0, 5.0]),
                   BGE: ([.69, .71, .70, .67], [8.3, 0.0, 0.0, 11.8])}
        # "Verdicts under D9": dr10, lower, dmrr, lower, q3 worst (lang, pts), dcw, upper, dfa, upper
        want_v = {GTE: (-5.3, -9.2, -0.055, -0.086, ("typescript", -9.0), 11.1, 16.7, -10.3, 0.0),
                  BGE: (-7.5, -11.8, -0.068, -0.100, ("python", -10.0), 14.8, 20.1, -12.1, -0.2)}
        for arm, w in want.items():
            o = base[arm]
            r1 = pooled(o, lambda x: 1.0 if x["rank"] is not None and x["rank"] <= 1 else 0.0, scored)[0]
            r5 = pooled(o, lambda x: 1.0 if x["rank"] is not None and x["rank"] <= 5 else 0.0, scored)[0]
            b10 = bootstrap(pooled(o, hit10, scored)[1])
            bm = bootstrap(pooled(o, rr, scored)[1])
            fl = fit_floors(o)
            cwr = [100 * rate(o, lambda x: cw_indicator(x, fl), k)
                   for k in (lambda x: True, lambda x: x["positive"], lambda x: not x["positive"])]
            got = (round(r1, 3), round(r5, 3), round(b10[0], 3), round(b10[1], 3), round(b10[2], 3),
                   round(bm[0], 3), round(bm[1], 3) if w[6] is not None else None,
                   round(bm[2], 3) if w[7] is not None else None, *[round(x, 1) for x in cwr])
            ok = got == w
            print(f"0b {arm}: r@1/r@5/r@10[lo,hi]/MRR[lo,hi]/CW comb,pos,abs {got}: {'MATCH' if ok else 'MISMATCH ' + str(w)}")
            if not ok:
                bad.append(f"0b {arm} {got} != {w}")
            if arm in want_fl:
                ff = [fl[l] for l in LANGS]
                fa = [round(100 * rate(o, lambda x: fa_indicator(x, fl), lambda x, l=l: x["language"] == l), 1)
                      for l in LANGS]
                ok = (ff, fa) == want_fl[arm]
                print(f"0c {arm}: floors {ff} held-out FA {fa}: {'MATCH' if ok else 'MISMATCH'}")
                if not ok:
                    bad.append(f"0c {arm} floors/FA")
        for arm, w in want_v.items():
            g = gates(base[REF], base[arm])
            got = (round(100 * g["r10"][0], 1), round(100 * g["r10"][1], 1), round(g["mrr"][0], 3),
                   round(g["mrr"][1], 3), (g["q3_worst"][0], round(100 * g["q3_worst"][1], 1)),
                   round(100 * g["cw"][0], 1), round(100 * g["cw"][2], 1),
                   round(100 * g["fa"][0], 1), round(100 * g["fa"][2], 1) + 0.0)
            ok = got == w
            print(f"0d {arm} D9 row {got}: {'MATCH' if ok else 'MISMATCH ' + str(w)}")
            if not ok:
                bad.append(f"0d {arm} verdict row")
        g = gates(base[REF], base[INT8])
        got = tuple(round(100 * x, 1) + 0.0 for x in g["fa"])
        ok = got == (2.1, -1.4, 6.2)
        print(f"0e int8 Q5 pooled {got}: {'MATCH' if ok else 'MISMATCH (2.1, -1.4, 6.2)'}")
        if not ok:
            bad.append("0e int8 Q5")
    # 0f. fusion code: fusing jina with itself must give jina back.
    for label, fn in [("RRF k=60", lambda c, q: rrf([c.lists[REF][q]] * 2, 60, c.index)),
                      ("weighted w=0.5", lambda c, q: weighted(c.lists[REF][q], c.lists[REF][q], 0.5, c.index))]:
        o = [outcome(c, q, fn(c, q), None) for c in cs for q in c.qids]
        diff = sum(1 for x, y in zip(o, base[REF]) if x["rank"] != y["rank"])
        print(f"0f {label} of jina with itself: first-expected rank differs on {diff} queries")
        if diff:
            bad.append(f"0f self-fusion {label}")
    # 0g. concat of gte with itself == gte from vectors.
    o = [outcome(c, q, top_from_scores((c.cos[GTE][c.qrow[q]] * 2) / 2, c.ids), None) for c in cs for q in c.qids]
    diff = sum(1 for x, y in zip(o, base[GTE]) if x["rank"] != y["rank"])
    print(f"0g concat(gte, gte): first-expected rank differs from stored gte on {diff} queries")
    if diff:
        bad.append("0g concat self")
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("CONTROL OK" + (" (dry run: GM-398 values not checked on a subset)" if dry else ""))

    # ---------------- 1. variants ----------------
    variants = {}  # name -> dict(outs_judge, outs_own, meta)

    def build(name, fused_fn, judge_arms, own_norm, meta):
        judge, own = [], []
        for c in cs:
            for q in c.qids:
                hits = fused_fn(c, q)
                top = hits[0][0] if hits else None
                judge.append(outcome(c, q, hits, judge_score(c, judge_arms, q, top) if top else None))
                own.append(outcome(c, q, hits, own_norm(hits[0][1]) if hits else None))
        variants[name] = {"judge": judge, "own": own, "meta": meta}

    for k in (60, 10):
        build(f"gte+bge RRF k={k}", lambda c, q, k=k: rrf([c.lists[GTE][q], c.lists[BGE][q]], k, c.index),
              [GTE, BGE], lambda s, k=k: s / (2.0 / (k + 1)), {"tuned": False, "rrf": True})
    build("gte+bge score (min-max, w=0.5)",
          lambda c, q: weighted(c.lists[GTE][q], c.lists[BGE][q], 0.5, c.index),
          [GTE, BGE], lambda s: s, {"tuned": False, "rrf": False})
    build("gte+bge concat (mean cosine)",
          lambda c, q: top_from_scores((c.cos[GTE][c.qrow[q]] + c.cos[BGE][c.qrow[q]]) / 2, c.ids),
          [GTE, BGE], lambda s: s, {"tuned": False, "rrf": False, "own_is_judge": True})

    tuning = {}
    for emb in EMBEDDERS:
        for k in (60, 10):
            build(f"{SHORT[emb]}+bm25 RRF k={k}", lambda c, q, e=emb, k=k: rrf([c.lists[e][q], c.lists[BM25][q]], k, c.index),
                  [emb], lambda s, k=k: s / (2.0 / (k + 1)), {"tuned": False, "rrf": True})
        # weight: fit half of the authored NL positives only (objective r@10, then MRR)
        curve = []
        fit_ids = {(c.name, q) for c in cs for q in c.qids
                   if c.queries[q]["positive"] and not c.queries[q]["mechanical"] and not c.queries[q]["held_out"]}
        for w in WEIGHTS:
            outs = [outcome(c, q, weighted(c.lists[emb][q], c.lists[BM25][q], w, c.index), None)
                    for c in cs for q in c.qids if (c.name, q) in fit_ids]
            curve.append((w, pooled(outs, hit10, scored)[0], pooled(outs, rr, scored)[0]))
        best = max(curve, key=lambda t: (round(t[1], 9), round(t[2], 9), -abs(t[0] - 0.5)))
        tuning[emb] = {"curve": curve, "w": best[0]}
        build(f"{SHORT[emb]}+bm25 score (min-max, w={best[0]:.2f})",
              lambda c, q, e=emb, w=best[0]: weighted(c.lists[e][q], c.lists[BM25][q], w, c.index),
              [emb], lambda s: s, {"tuned": True, "rrf": False, "w": best[0]})

    # Truncation: untruncated RRF k=60 (embedders' full cosine rankings; bm25 stays top-100).
    untrunc = {}
    for label, parts in [("gte+bge RRF k=60", [GTE, BGE]), ("jina+bm25 RRF k=60", [REF, BM25]),
                         ("gte+bm25 RRF k=60", [GTE, BM25]), ("bge+bm25 RRF k=60", [BGE, BM25])]:
        outs = []
        for c in cs:
            for q in c.qids:
                ls = [full_rank_list(c, p, q) if p in EMBEDDERS else c.lists[p][q] for p in parts]
                outs.append(outcome(c, q, rrf(ls, 60, c.index), None))
        untrunc[label] = summarize(outs)
    # How often the expected answer sits in only one of the two top-100 lists.
    one_list = {}
    for label, (x, y) in {"gte+bge": (GTE, BGE), "jina+bm25": (REF, BM25),
                          "gte+bm25": (GTE, BM25), "bge+bm25": (BGE, BM25)}.items():
        n = only = none = 0
        for c in cs:
            for q in c.qids:
                qq = c.queries[q]
                if not (qq["positive"] and not qq["mechanical"]):
                    continue
                e = c.expected[q]
                inx = any(nid in e for nid, _ in c.lists[x][q])
                iny = any(nid in e for nid, _ in c.lists[y][q])
                n += 1
                only += inx != iny
                none += not (inx or iny)
        one_list[label] = (only, none, n)

    # ---------------- 2. report ----------------
    res = {"controls": "ok", "base": {a_: summarize(base[a_]) for a_ in [REF, GTE, BGE, BM25]},
           "variants": {}, "tuning": tuning, "untruncated": untrunc, "one_list": one_list}
    print("\n== 1. unfused ==")
    for arm in [REF, GTE, BGE, BM25]:
        s = res["base"][arm]
        print(f"{arm}: NL r@10 {f3(s['nl']['r10'])} MRR {f3(s['nl']['mrr'])} | held-out NL r@10 "
              f"{f3(s['nl_heldout']['r10'])} MRR {f3(s['nl_heldout']['mrr'])} (n={s['nl_heldout']['n']}) | "
              f"name r@10 {f3(s['name']['r10'])} MRR {f3(s['name']['mrr'])} (n={s['name']['n']})")
    print("\n== 2. weight tuning (fit half NL; embedder weight w) ==")
    for emb, t in tuning.items():
        print(f"{SHORT[emb]}+bm25: chosen w={t['w']:.2f}; curve " +
              " ".join(f"{w:.2f}:{r:.3f}/{m:.3f}" for w, r, m in t["curve"]))
    print("\n== 3. variants (gates vs jina fp32; judge = embedder cosine of the fused top hit) ==")
    for name, v in variants.items():
        s = summarize(v["judge"])
        g_all = gates(base[REF], v["judge"], scored)
        g_ho = gates(base[REF], v["judge"], ho)
        g_own = gates(base[REF], v["own"], scored)
        res["variants"][name] = {"summary": s, "gates_all": g_all, "gates_heldout": g_ho,
                                 "gates_own_score": {k_: g_own[k_] for k_ in ("cw", "fa", "fa_lang", "floors", "pass")},
                                 "meta": v["meta"]}
        g = g_ho if v["meta"]["tuned"] else g_all
        p = g["pass"]
        fails = [q for q in ("Q1", "Q2", "Q3", "Q4", "Q5") if not p[q]]
        print(f"\n{name}  [gates on {'held-out NL' if v['meta']['tuned'] else 'all NL'}]")
        print(f"  NL r@10 {f3(s['nl']['r10'])} MRR {f3(s['nl']['mrr'])} | held-out r@10 {f3(s['nl_heldout']['r10'])} "
              f"MRR {f3(s['nl_heldout']['mrr'])} | name r@10 {f3(s['name']['r10'])} MRR {f3(s['name']['mrr'])}")
        print(f"  Q1 dr@10 {pts(g['r10'][0])} [lo {pts(g['r10'][1])}, hi {pts(g['r10'][2])}] | "
              f"Q2 dMRR {g['mrr'][0]:+.3f} [lo {g['mrr'][1]:+.3f}, hi {g['mrr'][2]:+.3f}] | "
              f"Q3 worst {g['q3_worst'][0]} {pts(g['q3_worst'][1])} ({', '.join(f'{l} {pts(d)}' for l, d in g['q3'].items())})")
        print(f"  Q4 dCW {pts(g['cw'][0])} [up {pts(g['cw'][2])}] | Q5 dFA {pts(g['fa'][0])} [up {pts(g['fa'][2])}] "
              f"({', '.join(f'{l} {pts(d)} n={n}' for l, (d, n) in g['fa_lang'].items())}) | floors "
              f"{floors_s(g['floors'])}")
        go_ = g_own
        print(f"  own-score Q4 dCW {pts(go_['cw'][0])} [up {pts(go_['cw'][2])}] Q5 dFA {pts(go_['fa'][0])} "
              f"[up {pts(go_['fa'][2])}] floors {floors_s(go_['floors'])}")
        print(f"  non-inferiority (cost role) fails: {fails or 'none'}; superiority Q1 lower>0 {p['Q1sup']}, "
              f"Q2 lower>0 {p['Q2sup']}")
    print("\n== 4. truncation ==")
    for label, s in untrunc.items():
        print(f"{label} untruncated embedder lists: NL r@10 {f3(s['nl']['r10'])} MRR {f3(s['nl']['mrr'])} "
              f"held-out r@10 {f3(s['nl_heldout']['r10'])} name r@10 {f3(s['name']['r10'])}")
    for label, (only, none, n) in one_list.items():
        print(f"{label}: expected in only one top-100 on {only}/{n} NL queries; in neither on {none}")
    if a.json:
        a.json.write_text(json.dumps(res, indent=1, default=str))


if __name__ == "__main__":
    main()
