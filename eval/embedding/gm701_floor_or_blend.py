"""GM-701/S2: does "GM-697 floor OR per-language blend threshold" catch more wrong/absent tops than the floor alone?

Per query (rerank-on path, setup of gm699_separability.collect: ship text, K=30, BETA 40, exact-name skip):
  right   = the reranked top-1 row is an expected row; wrong/absent = everything else (absent queries are wrong)
  floor   = shipped GM-697 floor on the COSINE-order page: cosine of the cosine top-1 < floor[language of that row]
  blend   = logit + 40 * cosine of the reranked top-1 row
Arms (flag = page refused / soft-noted):
  A  floor alone
  B  floor OR blend < t[lang], t = GM-699 cut (keeps 90% of right tops on FIT by blend alone), per language
  B2 floor OR blend < t2[lang], t2 = largest cut for which the OR rule itself keeps >= 90% of FIT right tops
     (t2 = -inf, i.e. B2 == A, in a language where the floor alone already keeps < 90%)
  C  blend alone at the GM-699 cut (context)
Populations: NL = authored (non-mechanical) queries, split by harness held_out; NAME = mechanical queries,
split by sha256(id)[0] odd = held-out (mirror of queries.rs::is_held_out_id). Thresholds are fitted on FIT of
each population separately, then applied unchanged to its held-out half.
Paired bootstrap: resample held-out queries, caught(arm) - caught(A) on the resampled wrong/absent set.
Controls: (a) right labels permuted within language on FIT before fitting (held-out labels real);
(b) number of held-out queries where B differs from A.

  python3 gm701_floor_or_blend.py --work <v2 work> [--resamples 10000] [--shuffles 200]
"""
import argparse
import hashlib
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm699_separability as S  # noqa: E402
import gm699_operating_point as O  # noqa: E402
import shipped_floor_rates as SFR  # noqa: E402

B, R = S.B, S.R
SEED, KEEP = 701, 0.90


def name_held_out(qid):
    return hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def setup(work, cache_dir):
    sha = R.sha256_file(work / "models" / "ms-marco-MiniLM-L6-v2-quint8" / "model.onnx")
    if sha != S.SHIPPED_SHA:
        sys.exit(f"STOP: model.onnx sha256 {sha} != shipped {S.SHIPPED_SHA}")
    ns = argparse.Namespace(work=work, eval_dir=HERE / "v2", st_runs=f"runs/{B.ST}", corpora=None)
    st_dir, corpora_dir = B.load_setup(ns)
    cs = B.load_corpora(work, ns.eval_dir, st_dir, corpora_dir, S.K)
    B.build_model_texts(cs, work, S.PY, ["ship"])
    B.use_texts(cs, S.PY)
    for c in cs:
        c.atext_by[S.RUST] = c.atext_by[S.PY]
    cache = B.open_cache(cache_dir or work / "rerank_cache_gm700_rust", cs, work, S.RUST, B.file_shas(work, S.PY))
    return cs, B.score_of_for(cache, S.RUST, "ship"), B.skip_flags(cs, True, None), sha


def collect(cs, sc, flags, floors):
    rows = []
    for c in cs:
        for q in c.qids:
            qq = c.queries[q]
            lst = c.lists[B.ST][q]
            n = min(S.K, len(lst))
            if n == 0:
                continue
            mech = bool(qq["mechanical"])
            cos = np.array([x for _, x in lst[:n]])
            logit = np.array([sc(c, q, nid) for nid, _ in lst[:n]])
            blend = logit + S.BETA * cos
            s_ = -np.arange(n, dtype=np.float64) if flags[(c.name, q)] else blend
            t = int(np.lexsort((np.arange(n), -s_))[0])
            f = floors.get(c.node_lang.get(lst[0][0]))
            rows.append(dict(pop="NAME" if mech else "NL", ho=name_held_out(q) if mech else bool(qq["held_out"]),
                             lang=qq["language"], pos=bool(qq["positive"]), right=lst[t][0] in c.expected[q],
                             floor=bool(f is not None and lst[0][1] < f), blend=float(blend[t])))
    return rows


def arrays(rows):
    return dict(lang=np.array([S.LANGS.index(r["lang"]) for r in rows]), right=np.array([r["right"] for r in rows]),
                floor=np.array([r["floor"] for r in rows]), blend=np.array([r["blend"] for r in rows]))


def fit(fa, keep=KEEP):
    """-> (t, t2) per language index."""
    t, t2 = np.zeros(len(S.LANGS)), np.full(len(S.LANGS), -np.inf)
    for i in range(len(S.LANGS)):
        m = (fa["lang"] == i) & fa["right"]
        b, fl = fa["blend"][m], fa["floor"][m]
        t[i] = O.thr_for(b, keep)
        for cand in np.sort(b)[::-1]:  # largest cut first
            if (~(fl | (b < cand))).mean() >= keep:
                t2[i] = cand
                break
    return t, t2


def flags(a, t, t2):
    fl, b, l = a["floor"], a["blend"], a["lang"]
    return dict(A=fl, B=fl | (b < t[l]), B2=fl | (b < t2[l]), C=b < t[l])


def point(a, fg):
    r = a["right"]
    return {k: dict(kept=float((~v[r]).mean()), caught=float(v[~r].mean()), kept_k=[int((~v[r]).sum()), int(r.sum())],
                    caught_k=[int(v[~r].sum()), int((~r).sum())]) for k, v in fg.items()}


def boot(a, fg, resamples):
    rng = np.random.default_rng(SEED)
    n = len(a["right"])
    r = a["right"]
    d = {k: [] for k in fg if k != "A"}
    dk = {k: [] for k in fg if k != "A"}
    for _ in range(resamples):
        i = rng.integers(0, n, n)
        ri = r[i]
        ca = fg["A"][i][~ri].mean()
        ka = (~fg["A"][i][ri]).mean()
        for k in d:
            d[k].append(fg[k][i][~ri].mean() - ca)
            dk[k].append((~fg[k][i][ri]).mean() - ka)
    pc = lambda x: [float(np.percentile(x, 2.5)), float(np.percentile(x, 97.5))]
    return {k: dict(caught_delta_ci=pc(d[k]), kept_delta_ci=pc(dk[k])) for k in d}


def shuffled(fit_rows, ho_a, resh, rng):
    out = []
    for _ in range(resh):
        fa = arrays(fit_rows)
        right = fa["right"].copy()
        for i in range(len(S.LANGS)):
            idx = np.where(fa["lang"] == i)[0]
            right[idx] = right[rng.permutation(idx)]
        fa["right"] = right
        t, t2 = fit(fa)
        fg = flags(ho_a, t, t2)
        p = point(ho_a, fg)
        out.append({k: p[k]["caught"] - p["A"]["caught"] for k in ("B", "B2")})
    return {k: dict(mean=float(np.mean([o[k] for o in out])), max=float(np.max([o[k] for o in out])),
                    p95=float(np.percentile([o[k] for o in out], 95))) for k in ("B", "B2")}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--cache-dir", type=Path)
    ap.add_argument("--resamples", type=int, default=10000)
    ap.add_argument("--shuffles", type=int, default=200)
    ap.add_argument("--out", type=Path)
    a = ap.parse_args()
    cs, sc, sk, sha = setup(a.work, a.cache_dir)
    floors = SFR.structured_floors()
    rows = collect(cs, sc, sk, floors)
    res = {"sha256": sha, "seed": SEED, "resamples": a.resamples, "shuffles": a.shuffles, "floors": floors}
    for pop in ("NL", "NAME"):
        fit_rows = [r for r in rows if r["pop"] == pop and not r["ho"]]
        ho_rows = [r for r in rows if r["pop"] == pop and r["ho"]]
        fa, ha = arrays(fit_rows), arrays(ho_rows)
        t, t2 = fit(fa)
        fgf, fgh = flags(fa, t, t2), flags(ha, t, t2)
        o = dict(n_fit=len(fit_rows), n_ho=len(ho_rows), t=dict(zip(S.LANGS, map(float, t))),
                 t2=dict(zip(S.LANGS, map(float, t2))), fit=point(fa, fgf), ho=point(ha, fgh),
                 boot=boot(ha, fgh, a.resamples))
        o["ho_differ_B_vs_A"] = int((fgh["B"] != fgh["A"]).sum())
        o["ho_differ_B2_vs_A"] = int((fgh["B2"] != fgh["A"]).sum())
        o["ho_differ_wrong_B_vs_A"] = int(((fgh["B"] != fgh["A"]) & ~ha["right"]).sum())
        o["control_shuffle"] = shuffled(fit_rows, ha, a.shuffles, np.random.default_rng(SEED + 1))
        res[pop] = o
    text = json.dumps(res, indent=1)
    if a.out:
        a.out.write_text(text)
    print(text)


if __name__ == "__main__":
    main()
