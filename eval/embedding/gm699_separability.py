"""GM-699/S1: how well does the top-1 score separate "top row right" from "top row wrong or no answer"?

On the v2 NL FIT split (authored queries, harness held_out == False; positives and absent queries),
per language and pooled: AUROC (+ paired bootstrap 95% CI over queries) of three signals of the
reranked top-1 row: cosine, CE logit (Rust-scored quint8, cache of ce-minilm-quint8-rust), and the
blend logit + 40 * cosine. Setup of the shipped path: ship text (E+P+B48), K=30, BETA 40, exact-name
skip rule (rerank.rs `confident`), one pair per run. An absent query counts as "wrong".

  python3 gm699_separability.py --work <v2 work> [--out results.json] [--resamples 2000]

Reads the v2 work dir and its Rust logit cache; writes nothing there.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm463_bakeoff as B  # noqa: E402
import rerank_eval as R  # noqa: E402

RUST, PY, K, BETA, SEED = "ce-minilm-quint8-rust", "ce-minilm-quint8", 30, 40.0, 699
SHIPPED_SHA = "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9"
SIGNALS = ("cosine", "logit", "blend")
LANGS = R.LANGS


def auroc(score, right):
    """P(score of a right row > score of a wrong row), ties 0.5. nan if a class is empty."""
    score, right = np.asarray(score, float), np.asarray(right, bool)
    n1, n0 = int(right.sum()), int((~right).sum())
    if n1 == 0 or n0 == 0:
        return float("nan")
    order = np.argsort(score, kind="mergesort")
    s = score[order]
    ranks = np.empty(len(s))
    i = 0
    while i < len(s):  # average rank over ties
        j = i
        while j + 1 < len(s) and s[j + 1] == s[i]:
            j += 1
        ranks[i:j + 1] = (i + j) / 2 + 1
        i = j + 1
    r = np.empty(len(s))
    r[order] = ranks
    return float((r[right].sum() - n1 * (n1 + 1) / 2) / (n1 * n0))


def boot(idx_sets, feats, right, resamples, rng):
    """Paired bootstrap: one resample of query indices serves all signals. Returns per-signal
    (lo, hi) of AUROC and of the difference to cosine; degenerate resamples are dropped."""
    n = len(idx_sets)
    vals = {s: [] for s in SIGNALS}
    for _ in range(resamples):
        ix = idx_sets[rng.integers(0, n, n)]
        r = right[ix]
        if r.all() or not r.any():
            continue
        for s in SIGNALS:
            vals[s].append(auroc(feats[s][ix], r))
    v = {s: np.array(x) for s, x in vals.items()}
    ci = {s: (float(np.quantile(v[s], 0.025)), float(np.quantile(v[s], 0.975))) for s in SIGNALS}
    diff = {s: (float(np.quantile(v[s] - v["cosine"], 0.025)), float(np.quantile(v[s] - v["cosine"], 0.975)))
            for s in ("logit", "blend")}
    return ci, diff, len(v["cosine"])


def collect(cs, sc, flags, keep):
    rows = []
    for c in cs:
        for q in c.qids:
            qq = c.queries[q]
            if qq["mechanical"] or not keep(qq):
                continue
            lst = c.lists[B.ST][q]
            n = min(K, len(lst))
            if n == 0:
                rows.append(dict(lang=qq["language"], pos=qq["positive"], empty=True))
                continue
            cos = np.array([x for _, x in lst[:n]])
            logit = np.array([sc(c, q, nid) for nid, _ in lst[:n]])
            blend = logit + BETA * cos
            skipped = bool(flags[(c.name, q)])
            s_ = -np.arange(n, dtype=np.float64) if skipped else blend
            t = int(np.lexsort((np.arange(n), -s_))[0])
            exp = c.expected[q]
            cos_right = lst[0][0] in exp
            tl = c.node_lang.get(lst[0][0])
            rows.append(dict(lang=qq["language"], pos=qq["positive"], empty=False, skipped=skipped,
                             right=lst[t][0] in exp, cos_right=cos_right, cos1=float(lst[0][1]), top_lang=tl,
                             cosine=float(cos[t]), logit=float(logit[t]), blend=float(blend[t])))
    return rows


def analyse(rows, label, resamples, shuffle_rng=None):
    out = {}
    rs = [r for r in rows if not r["empty"]]
    if shuffle_rng is not None:  # control: permute the right/wrong labels over all pooled queries
        perm = shuffle_rng.permutation(len(rs))
        lab = np.array([r["right"] for r in rs])[perm]
        rs = [dict(r, right=bool(l)) for r, l in zip(rs, lab)]
    for lang in LANGS + ["pooled"]:
        sub = [r for r in rs if lang == "pooled" or r["lang"] == lang]
        right = np.array([r["right"] for r in sub], bool)
        feats = {s: np.array([r[s] for r in sub]) for s in SIGNALS}
        rng = np.random.default_rng(SEED)
        ci, diff, used = boot(np.arange(len(sub)), feats, right, resamples, rng)
        out[lang] = {"n": len(sub), "right": int(right.sum()),
                     "wrong": int(sum(1 for r, ok in zip(sub, right) if not ok and r["pos"])),
                     "absent": int(sum(1 for r in sub if not r["pos"])),
                     "auroc": {s: auroc(feats[s], right) for s in SIGNALS}, "ci": ci, "diff_ci": diff,
                     "boots_used": used}
    return out


def shuffle_many(rows, runs, resamples):
    """Control over many label permutations (one permutation alone can land 2 sd off 0.5 by chance):
    per language x signal, the mean AUROC, its 2.5/97.5% range over permutations, and the share of
    permutations whose bootstrap CI contains 0.5."""
    acc = {}
    for i in range(runs):
        r = analyse(rows, "FIT", resamples, np.random.default_rng(SEED + 100 + i))
        for lang, v in r.items():
            for s in SIGNALS:
                acc.setdefault((lang, s), []).append((v["auroc"][s], v["ci"][s][0] <= 0.5 <= v["ci"][s][1]))
    out = {}
    for (lang, s), xs in acc.items():
        a_ = np.array([x[0] for x in xs])
        out.setdefault(lang, {})[s] = {"mean": float(a_.mean()), "lo": float(np.quantile(a_, 0.025)),
                                       "hi": float(np.quantile(a_, 0.975)), "ci_contains_half": float(np.mean([x[1] for x in xs]))}
    out["runs"], out["resamples"] = runs, resamples
    return out


def cross_check(cs, shipped, keep):
    """Cosine order (int8-structured, no rerank), shipped floors, the way gm697_floor_refit.stats counts
    NL confident-wrong: top clears the floor (>=) and its row is wrong / the query is absent."""
    pos, ab = [0, 0], [0, 0]
    right = [0, 0]
    for c in cs:
        for q in c.qids:
            qq = c.queries[q]
            lst = c.lists[B.ST][q]
            if qq["mechanical"] or not keep(qq) or not lst:
                continue
            f = shipped.get(c.node_lang.get(lst[0][0]))
            if f is None:
                continue
            clears = lst[0][1] >= f
            ok = lst[0][0] in c.expected[q]
            if qq["positive"]:
                pos[1] += 1
                pos[0] += (not ok) and clears
                right[1] += 1
                right[0] += ok
            else:
                ab[1] += 1
                ab[0] += clears
    return pos, ab, right


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--cache-dir", type=Path)
    ap.add_argument("--resamples", type=int, default=2000)
    ap.add_argument("--shuffles", type=int, default=100)
    ap.add_argument("--shuffle-resamples", type=int, default=300)
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()
    sha = R.sha256_file(a.work / "models" / "ms-marco-MiniLM-L6-v2-quint8" / "model.onnx")
    if sha != SHIPPED_SHA:
        sys.exit(f"STOP: model.onnx sha256 {sha} != shipped {SHIPPED_SHA}")
    ns = argparse.Namespace(work=a.work, eval_dir=HERE / "v2", st_runs=f"runs/{B.ST}", corpora=None)
    st_dir, corpora_dir = B.load_setup(ns)
    cs = B.load_corpora(a.work, ns.eval_dir, st_dir, corpora_dir, K)
    B.build_model_texts(cs, a.work, PY, ["ship"])
    B.use_texts(cs, PY)
    for c in cs:
        c.atext_by[RUST] = c.atext_by[PY]
    cache = B.open_cache(a.cache_dir or a.work / "rerank_cache_gm700_rust", cs, a.work, RUST, B.file_shas(a.work, PY))
    # every pair of the K window must be cached (sc raises KeyError otherwise)
    sc = B.score_of_for(cache, RUST, "ship")
    flags = B.skip_flags(cs, True, None)
    shipped = R.shipped_floors(HERE.parents[1])
    res = {"sha256": sha, "cache_key": cache.key, "resamples": a.resamples, "seed": SEED,
           "queries_total": sum(len(c.qids) for c in cs)}
    for split, keep in (("FIT", lambda q: not q["held_out"]), ("HELD-OUT", lambda q: q["held_out"])):
        rows = collect(cs, sc, flags, keep)
        nonempty = [r for r in rows if not r["empty"]]
        res[split] = {"empty_windows": len(rows) - len(nonempty),
                      "skip_fired": sum(r["skipped"] for r in nonempty), "queries": len(nonempty),
                      "skip_fired_by_lang": {l: sum(r["skipped"] for r in nonempty if r["lang"] == l) for l in LANGS},
                      "right_cosine_order": sum(r["cos_right"] for r in nonempty),
                      "right_reranked": sum(r["right"] for r in nonempty),
                      "auroc": analyse(rows, split, a.resamples),
                      "cross_check": cross_check(cs, shipped, keep)}
        # supplementary: the cosine-order top-1 (what the verdict judges): cosine of that row vs cos_right
        sup = [dict(r, right=r["cos_right"], cosine=r["cos1"]) for r in nonempty]
        res[split]["cosine_order_top1_cosine"] = analyse(
            [dict(r, logit=r["cos1"], blend=r["cos1"]) for r in sup], split, a.resamples)
        if split == "FIT":
            res["control_shuffle"] = analyse(rows, split, a.resamples, np.random.default_rng(SEED + 1))
            res["control_shuffle_many"] = shuffle_many(rows, a.shuffles, a.shuffle_resamples)
    text = json.dumps(res, indent=1, default=lambda o: o.item() if hasattr(o, "item") else str(o))
    if a.out:
        a.out.write_text(text)
    print(text)


if __name__ == "__main__":
    main()
