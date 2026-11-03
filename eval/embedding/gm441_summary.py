#!/usr/bin/env python3
"""GM-441 S5: compact summary of the schedule replay, the 100% rerank and the daemon confirm runs.

usage: gm441_summary.py --sched DIR --ctl DIR --work DIR [--confirm DIR] --out FILE.json

Reads <sched>/<arm>/<corpus>/schedule.json (+ rankings.reranked.jsonl at 100%),
<ctl> (an `--filter none` replay of arm id, compared with the stored run) and
<confirm>/<arm>.json (gm441_confirm.py). Prints a plain-text table and writes
the same numbers as JSON.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402

ARMS = ["id", "tier", "oracle", "tier-reversed"]
LABEL = {"id": "A0 id", "tier": "A1 tier", "oracle": "C oracle", "tier-reversed": "ctl tier-reversed"}
SECONDS = (20, 60, 120)
SEED, RESAMPLES = 398, 10000


def load(path):
    return json.loads(Path(path).read_text())


def first_t(cps, pred):
    for c in cps:
        if pred(c):
            return c["t"]
    return None


def share_at(cps, total, s):
    best = 0
    for c in cps:
        if c["t"] <= s:
            best = c["gold_embedded"]
    return best / total if total else None


def auc(cps, key):
    xs = [0.0] + [c["frac"] for c in cps]
    ys = [0.0] + [c[key] or 0.0 for c in cps]
    return float(sum((xs[i + 1] - xs[i]) * (ys[i + 1] + ys[i]) / 2 for i in range(len(xs) - 1)))


def arm_corpus(d):
    s = load(d / "schedule.json")
    cps = s["checkpoints"]
    last = cps[-1]
    r_full = last["recall10"]
    pass_s = s["passS"]
    row = {
        "nodes": s["nodes"], "goldTotal": s["goldTotal"], "passS": pass_s, "measuredPassS": s["measuredPassS"],
        "recall10": last["recall10"], "mrr": last["mrr"],
        "recall10Reranked": last.get("recall10Reranked"), "mrrReranked": last.get("mrrReranked"),
        "tFullGold": first_t(cps, lambda c: c["gold_embedded"] >= s["goldTotal"]),
        "t90": first_t(cps, lambda c: c["recall10"] is not None and c["recall10"] >= 0.9 * r_full - 1e-12),
        "t100": first_t(cps, lambda c: c["recall10"] is not None and c["recall10"] >= r_full - 1e-12),
        "goldShare": {str(x): share_at(cps, s["goldTotal"], x) for x in SECONDS},
        "tierEnds": {k: {"t": v["t"], "frac": v["frac"], "nodes": v["nodes"]} for k, v in s["tierEnds"].items()},
        "aucRecall": auc(cps, "recall10"),
        "checkpoints": len(cps),
    }
    for k in ("tFullGold", "t90", "t100"):
        row[k + "Frac"] = row[k] / pass_s if row[k] is not None else None
    return row


def per_query_hits(sched_dir, arm, corpus, target_frac, queries):
    """Scored-positive recall@10 hits at the first checkpoint reaching target_frac."""
    d = sched_dir / arm / corpus
    cps = load(d / "schedule.json")["checkpoints"]
    cp = next(c for c in cps if c["frac"] >= target_frac - 1e-9)
    path = d / "checkpoints" / f"{cp['embedded']:06d}" /"rankings.jsonl"
    out = {}
    for line in path.read_text().splitlines():
        r = json.loads(line)
        q = queries[r["id"]]
        if not (q["positive"] and not q["mechanical"]):
            continue
        exp = set(r["expected"])
        rank = next((i + 1 for i, h in enumerate(r["hits"][:10]) if h[0] in exp), None)
        out[r["id"]] = 1.0 if rank else 0.0
    return cp["frac"], out


def bootstrap(sched_dir, corpora, qmaps, a1, a0, target):
    rng = np.random.default_rng(SEED)
    diffs = {}
    fracs = {}
    for c in corpora:
        f1, h1 = per_query_hits(sched_dir, a1, c, target, qmaps[c])
        f0, h0 = per_query_hits(sched_dir, a0, c, target, qmaps[c])
        ids = sorted(h1)
        diffs[c] = np.array([h1[i] - h0[i] for i in ids])
        fracs[c] = (f1, f0)
    point = float(np.mean([d.mean() for d in diffs.values()]))
    boots = np.zeros(RESAMPLES)
    for c, d in diffs.items():
        idx = rng.integers(0, len(d), size=(RESAMPLES, len(d)))
        boots += d[idx].mean(axis=1)
    boots /= len(diffs)
    lo, hi = np.percentile(boots, [2.5, 97.5])
    return {"target": target, "diff": point, "ci95": [float(lo), float(hi)]}


def stored_equals(ctl, work, corpora, variant_dir):
    res = {}
    for c in corpora:
        a = [json.loads(x) for x in (ctl / "id" / c / "rankings.jsonl").read_text().splitlines()]
        b = [json.loads(x) for x in (Path(variant_dir) / c / "rankings.jsonl").read_text().splitlines()]
        sa = {r["id"]: r["hits"] for r in a}
        sb = {r["id"]: r["hits"] for r in b}
        same_ids = set(sa) == set(sb) and all([h[0] for h in sa[k]] == [h[0] for h in sb[k]] for k in sb)
        dmax = max((abs(x[1] - y[1]) for k in sb for x, y in zip(sa[k], sb[k])), default=0.0)
        res[c] = {"sameOrder": bool(same_ids), "maxScoreDiff": dmax, "queries": len(sb)}
    return res


def reranked_identical(sched, corpora):
    res = {}
    for c in corpora:
        texts = {a: (sched / a / c / "rankings.reranked.jsonl").read_text() for a in ARMS}
        res[c] = len(set(texts.values())) == 1
    return res


def confirm_row(path):
    d = load(path)
    tl = d["timeline"]
    emb = next((r for r in tl if r["phase"] == "embedding"), None)
    return {
        "readyT": d.get("ready_t"), "passS": d.get("pass_s"), "embeddingStartT": d.get("embedding_start_t"),
        "tiers": d.get("tiers"), "daemonPid": d.get("daemonPid"), "daemonCpuTime": d.get("daemonCpuTime"),
        "searches": d.get("searches"), "embedTotal": emb["total"] if emb else None,
        "timelinePoints": len(tl),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sched", required=True, type=Path)
    ap.add_argument("--ctl", type=Path)
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--eval-dir", required=True, type=Path)
    ap.add_argument("--confirm", type=Path)
    ap.add_argument("--variant-dir", type=Path)
    ap.add_argument("--confirm-corpus", default="g-mesh")
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()

    corpora = sorted(p.name for p in (a.sched / "id").iterdir() if p.is_dir())
    rows = {arm: {c: arm_corpus(a.sched / arm / c) for c in corpora} for arm in ARMS}
    res = {"corpora": corpora, "arms": rows}

    pooled = {}
    for arm in ARMS:
        r = rows[arm]
        tot = sum(r[c]["passS"] for c in corpora)

        def sec(k):
            return sum(r[c][k] for c in corpora) if all(r[c][k] is not None for c in corpora) else None

        pooled[arm] = {
            "passSSum": tot,
            "tFullGoldSumS": sec("tFullGold"), "t90SumS": sec("t90"), "t100SumS": sec("t100"),
            "tFullGoldSecShare": sec("tFullGold") / tot if sec("tFullGold") is not None else None,
            "t90SecShare": sec("t90") / tot if sec("t90") is not None else None,
            "t100SecShare": sec("t100") / tot if sec("t100") is not None else None,
            "tFullGoldMacroFrac": float(np.mean([r[c]["tFullGoldFrac"] for c in corpora])),
            "t90MacroFrac": float(np.mean([r[c]["t90Frac"] for c in corpora])),
            "t100MacroFrac": float(np.mean([r[c]["t100Frac"] for c in corpora])),
            "aucMacro": float(np.mean([r[c]["aucRecall"] for c in corpora])),
            "recall10Macro": float(np.mean([r[c]["recall10"] for c in corpora])),
            "recall10RerankedMacro": float(np.mean([r[c]["recall10Reranked"] for c in corpora])),
            "mrrMacro": float(np.mean([r[c]["mrr"] for c in corpora])),
            "mrrRerankedMacro": float(np.mean([r[c]["mrrReranked"] for c in corpora])),
        }
    res["pooled"] = pooled

    qmaps = {c: R.load_queries(a.eval_dir, c)[0] for c in corpora}
    res["bootstrapA1vsA0"] = [bootstrap(a.sched, corpora, qmaps, "tier", "id", t) for t in (0.25, 0.5, 0.75)]

    res["controls"] = {
        "tierReversedT90LaterThanA0": {c: rows["tier-reversed"][c]["t90"] > rows["id"][c]["t90"] for c in corpora},
        "tierReversedT90PooledSecShare": [pooled["tier-reversed"]["t90SecShare"], pooled["id"]["t90SecShare"]],
        "tierReversedAucBelowA0": {c: rows["tier-reversed"][c]["aucRecall"] < rows["id"][c]["aucRecall"] for c in corpora},
        "rerankedRankingsIdenticalAcrossArms": reranked_identical(a.sched, corpora),
    }
    if a.ctl and a.variant_dir:
        res["controls"]["maskOffEqualsStoredRankings"] = stored_equals(a.ctl, a.work, corpora, a.variant_dir)

    if a.confirm and a.confirm.exists():
        conf = {p.stem: confirm_row(p) for p in sorted(a.confirm.glob("*.json"))}
        res["confirm"] = conf
        tier = rows["tier"].get(a.confirm_corpus)
        a1 = conf.get("A1")
        if a1 and a1.get("tiers") and tier and a1.get("passS"):
            tot_s = sum(t["s"] for t in a1["tiers"])
            cum, out = 0.0, {}
            for t in a1["tiers"]:
                cum += t["s"]
                pred = tier["tierEnds"].get(t["tier"], {}).get("frac")
                out[t["tier"]] = {"measuredFrac": cum / tot_s, "predictedFrac": pred,
                                  "diffPct": (cum / tot_s - pred) * 100 if pred is not None else None}
            res["confirmTierShares"] = out

    a.out.write_text(json.dumps(res, indent=1))

    # compact text
    print(f"corpora: {', '.join(corpora)}")
    hdr = f"{'arm':18} {'corpus':17} {'pass_s':>7} {'full-gold s (%)':>17} {'t90 s (%)':>14} {'t100 s (%)':>14} {'R@10 100%':>10} {'->rerank':>9} {'MRR':>7} {'->rerank':>9}"
    print(hdr)
    for arm in ARMS:
        for c in corpora:
            x = rows[arm][c]
            print(f"{LABEL[arm]:18} {c:17} {x['passS']:7.1f} {x['tFullGold']:8.1f} ({100*x['tFullGoldFrac']:5.1f}%) "
                  f"{x['t90']:7.1f} ({100*x['t90Frac']:4.1f}%) {x['t100']:7.1f} ({100*x['t100Frac']:4.1f}%) "
                  f"{x['recall10']:10.4f} {x['recall10Reranked']:9.4f} {x['mrr']:7.4f} {x['mrrReranked']:9.4f}")
    print("pooled:")
    for arm in ARMS:
        p = pooled[arm]
        print(f"  {LABEL[arm]:18} full-gold {p['tFullGoldSumS']:.0f}/{p['passSSum']:.0f}s = {100*p['tFullGoldSecShare']:.1f}% (macro {100*p['tFullGoldMacroFrac']:.1f}%), "
              f"t90 {100*p['t90SecShare']:.1f}% (macro {100*p['t90MacroFrac']:.1f}%), t100 {100*p['t100SecShare']:.1f}% (macro {100*p['t100MacroFrac']:.1f}%), "
              f"AUC {p['aucMacro']:.4f}, R@10 {p['recall10Macro']:.4f}->{p['recall10RerankedMacro']:.4f}, MRR {p['mrrMacro']:.4f}->{p['mrrRerankedMacro']:.4f}")
    print("bootstrap A1-A0 recall@10 (seed 398, 10000):")
    for b in res["bootstrapA1vsA0"]:
        print(f"  {int(b['target']*100)}%: {b['diff']:+.4f}  CI95 [{b['ci95'][0]:+.4f}, {b['ci95'][1]:+.4f}]")
    print("controls:", json.dumps(res["controls"]))
    print("A1 tier ends (g-mesh):", json.dumps(rows["tier"].get(a.confirm_corpus, {}).get("tierEnds")))
    if "confirm" in res:
        for k, v in res["confirm"].items():
            print(f"confirm {k}: ready_t={v['readyT']} pass_s={v['passS']} tiers={v['tiers']} cpu={v['daemonCpuTime']}")
        print("confirmTierShares:", json.dumps(res.get("confirmTierShares")))


if __name__ == "__main__":
    main()
