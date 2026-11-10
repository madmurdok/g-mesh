"""GM-699: refusal operating points of the reranked top-1 signals on v2 NL.

For each signal (cosine, CE logit, blend) the threshold keeps a given share of right top rows on FIT
(one pooled threshold, or one per language); "caught" is the share of wrong-or-absent tops below it,
on FIT and with the same threshold on held-out. Then a paired bootstrap over FIT queries (threshold
refitted per resample) of caught(blend) - caught(cosine). Rows come from gm699_separability.collect.

  python3 gm699_operating_point.py --work <v2 work> [--resamples 2000]

Reads the v2 work dir and its Rust logit cache; writes nothing there.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm699_separability as S  # noqa: E402

B, R = S.B, S.R
KEEPS = (0.95, 0.90, 0.80)
BOOT_KEEPS = (0.90, 0.80)


def load_rows(work, cache_dir):
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
    sc = B.score_of_for(cache, S.RUST, "ship")
    flags = B.skip_flags(cs, True, None)
    out = {}
    for split, keep in (("FIT", lambda q: not q["held_out"]), ("HO", lambda q: q["held_out"])):
        out[split] = [r for r in S.collect(cs, sc, flags, keep) if not r["empty"]]
    return out


def thr_for(scores_right, keep):
    """Threshold t such that a share >= keep of the right rows score >= t."""
    s = np.sort(scores_right)
    return s[int(np.floor((1 - keep) * len(s)))]


def op(sub, sig, t):
    right = np.array([r["right"] for r in sub])
    x = np.array([r[sig] for r in sub])
    refused = x < t
    return dict(kept=float((x[right] >= t).mean()), caught=float((x[~right] < t).mean()),
                refused=float(refused.mean()),
                refused_wrong=float((~right[refused]).mean()) if refused.any() else float("nan"),
                n_right=int(right.sum()), n_bad=int((~right).sum()))


def by_lang(rows, lang):
    return [r for r in rows if lang == "pooled" or r["lang"] == lang]


def caught_at(sub, sig, keep, perlang):
    groups = [by_lang(sub, l) for l in S.LANGS] if perlang else [sub]
    c = b = 0
    for g in groups:
        t = thr_for(np.array([r[sig] for r in g if r["right"]]), keep)
        x = np.array([r[sig] for r in g if not r["right"]])
        c += int((x < t).sum())
        b += len(x)
    return c / b


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--cache-dir", type=Path)
    ap.add_argument("--resamples", type=int, default=2000)
    a = ap.parse_args()
    rows = load_rows(a.work, a.cache_dir)
    fit, ho = rows["FIT"], rows["HO"]
    res = {}
    for keep in KEEPS:
        for sig in S.SIGNALS:
            for lang in S.LANGS + ["pooled"]:
                f, h = by_lang(fit, lang), by_lang(ho, lang)
                t = thr_for(np.array([r[sig] for r in f if r["right"]]), keep)
                res[f"{keep}|{sig}|{lang}"] = dict(t=float(t), fit=op(f, sig, t), ho=op(h, sig, t))
            # one threshold per language, outcome summed over languages: [kept, right, caught, bad]
            agg = {"fit": [0, 0, 0, 0], "ho": [0, 0, 0, 0]}
            for lang in S.LANGS:
                f, h = by_lang(fit, lang), by_lang(ho, lang)
                t = thr_for(np.array([r[sig] for r in f if r["right"]]), keep)
                for nm, sub in (("fit", f), ("ho", h)):
                    right = np.array([r["right"] for r in sub])
                    x = np.array([r[sig] for r in sub])
                    g = agg[nm]
                    g[0] += int((x[right] >= t).sum())
                    g[1] += int(right.sum())
                    g[2] += int((x[~right] < t).sum())
                    g[3] += int((~right).sum())
            res[f"{keep}|{sig}|perlang"] = {k: dict(kept=v[0] / v[1], caught=v[2] / v[3], k=v) for k, v in agg.items()}
    for keep in BOOT_KEEPS:
        for perlang in (False, True):
            rng = np.random.default_rng(S.SEED)
            d, n = [], len(fit)
            for _ in range(a.resamples):
                sub = [fit[i] for i in rng.integers(0, n, n)]
                d.append(caught_at(sub, "blend", keep, perlang) - caught_at(sub, "cosine", keep, perlang))
            res[f"boot|{keep}|{'perlang' if perlang else 'pooled'}"] = [float(np.percentile(d, 2.5)),
                                                                         float(np.percentile(d, 97.5))]
    print(json.dumps(res, indent=1))


if __name__ == "__main__":
    main()
