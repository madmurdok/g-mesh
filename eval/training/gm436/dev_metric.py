#!/usr/bin/env python3
"""GM-436 dev metric (docs/design/GM-436-laptop-training.md, section 4 "Learning
curve" and section 5 item 5).

- Each query is ranked against every node of its repo in the mining run (the
  harness's candidate set) by cosine(W q, c); a hit is a positive or an
  `answersAlso` node. Ties go to the earlier node (the run's node order), as
  `embed_eval.rs::top_hits` orders them.
- MRR (primary) and recall@10; 95% percentile bootstrap clustered by target,
  10,000 resamples, seed 436. The 50% -> 100% gain is a paired bootstrap over
  the same targets.
- Curve rule: if the 50 -> 100 gain's CI lies above 0, the report recommends
  more GM-456 data, with a log-linear extrapolation labelled as such.
- C0b: identity-W Python ranking vs the mine run's `rankings.jsonl`.

usage: dev_metric.py c0b --variant jina-v2-base-code-int8-structured --repos <dev repos> [--plant-offset]
"""

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cache as K  # noqa: E402
import data as D  # noqa: E402

RESAMPLES = 10_000
TOP = 100
TIE_EPS = 2e-6


def normalise(m):
    return K.normalise(m)


def adapt(qmat, W):
    """q' = W q for every row q of qmat (W row-major, d x d)."""
    q = np.asarray(qmat, np.float64)
    return q if W is None else q @ np.asarray(W, np.float64).T


def first_hit_ranks(scores, relevant):
    """1-based rank of each row's best relevant column (ties to the lower column index); inf if none."""
    out = np.full(len(scores), np.inf)
    for i, (s, rel) in enumerate(zip(scores, relevant)):
        best = math.inf
        for j in rel:
            r = int(np.count_nonzero(s > s[j]) + np.count_nonzero(s[:j] == s[j]) + 1)
            best = min(best, r)
        out[i] = best
    return out


def repo_eval(W, queries, qrows, qmat, nrows, nmat):
    """Per-query (id, target, 1-based first-hit rank) over the nodes of each query's repo."""
    q_index = {r["key"]: i for i, r in enumerate(qrows)}
    out, missing = [], []
    by_repo = {}
    for q in queries:
        by_repo.setdefault(q["repo"], []).append(q)
    for repo, qs in sorted(by_repo.items()):
        idx = [i for i, r in enumerate(nrows) if r["repo"] == repo]
        col = {nrows[i]["key"]: c for c, i in enumerate(idx)}
        nn = normalise(nmat[idx])
        have = [q for q in qs if q["id"] in q_index]
        missing += [q["id"] for q in qs if q["id"] not in q_index]
        if not have:
            continue
        qa = normalise(adapt(qmat[[q_index[q["id"]] for q in have]], W))
        scores = qa @ nn.T
        rel = [[col[k] for k in q["positives"] + q["answersAlso"] if k in col] for q in have]
        ranks = first_hit_ranks(scores, rel)
        out += [(q["id"], q["target"], r) for q, r in zip(have, ranks)]
    return out, missing


def metrics(ranks):
    r = np.asarray(ranks, float)
    rr = np.where(np.isfinite(r), 1.0 / r, 0.0)
    return {"mrr": float(rr.mean()) if len(rr) else 0.0, "recall@10": float((r <= 10).mean()) if len(r) else 0.0}


def rr_values(evals):
    return np.array([1.0 / r if math.isfinite(r) else 0.0 for _, _, r in evals])


def cluster_bootstrap(values, clusters, resamples=RESAMPLES, seed=D.SEED, alpha=0.05):
    """Percentile CI of the mean, resampling whole clusters with replacement.
    clusters=None resamples single values (the query-level contrast)."""
    values = np.asarray(values, float)
    if clusters is None:
        clusters = np.arange(len(values))
    names, inv = np.unique(np.asarray(clusters), return_inverse=True)
    sums = np.bincount(inv, weights=values, minlength=len(names))
    counts = np.bincount(inv, minlength=len(names)).astype(float)
    rng = np.random.default_rng(seed)
    pick = rng.integers(0, len(names), size=(resamples, len(names)))
    means = sums[pick].sum(1) / counts[pick].sum(1)
    lo, hi = np.quantile(means, [alpha / 2, 1 - alpha / 2])
    return {"point": float(values.mean()), "lower": float(lo), "upper": float(hi)}


def paired_bootstrap(a, b, clusters, resamples=RESAMPLES, seed=D.SEED, alpha=0.05):
    """CI of mean(b) - mean(a) over the same items, resampling clusters (targets)."""
    a, b = np.asarray(a, float), np.asarray(b, float)
    if a.shape != b.shape:
        raise ValueError("paired bootstrap needs the same items in both arms")
    return cluster_bootstrap(b - a, clusters, resamples, seed, alpha)


def curve_rule(points, clusters, resamples=RESAMPLES, seed=D.SEED):
    """points: {fraction: per-query RR array over the same dev queries}, fractions 0.25/0.5/1.0.
    Returns the 50->100 gain CI, the log-linear fit and the report line."""
    gain = paired_bootstrap(points[0.5], points[1.0], clusters, resamples, seed)
    fr = sorted(points)
    x = np.log2(np.array(fr))
    y = np.array([float(np.mean(points[f])) for f in fr])
    slope, icept = np.polyfit(x, y, 1)
    out = {"gain50to100": gain, "fit": {"mrrPerDoubling": float(slope), "mrrAt100": float(icept),
                                        "extrapolatedMrrAt2x": float(icept + slope),
                                        "extrapolatedMrrAt4x": float(icept + 2 * slope)}}
    if gain["lower"] > 0:
        out["line"] = (f"Generate more GM-456 data: the 50%->100% dev MRR gain is {gain['point']:+.4f} "
                       f"(95% CI {gain['lower']:+.4f}..{gain['upper']:+.4f}, excludes 0). "
                       f"Extrapolation (log-linear fit over 25/50/100%, not a measurement): "
                       f"2x data -> MRR {icept + slope:.4f}, 4x -> {icept + 2 * slope:.4f}.")
    else:
        out["line"] = (f"No data recommendation: the 50%->100% dev MRR gain is {gain['point']:+.4f} "
                       f"(95% CI {gain['lower']:+.4f}..{gain['upper']:+.4f}, includes 0).")
    return out


# --- C0b ---------------------------------------------------------------------

def c0b_compare(py_top, rust_rows, tie_eps=TIE_EPS):
    """Compare Python's top-TOP [(node id, score)] with the run's hits.

    Ids are grouped by runs of Rust scores within tie_eps; each group must hold
    the same ids (order inside a group is free, f32 vs f64 rounding). The last
    group may be cut by TOP differently on each side, so there every Python id
    only has to score within tie_eps of the group. Returns mismatching query ids.
    """
    bad = []
    for qid, top in py_top.items():
        hits = rust_rows[qid]["hits"][:TOP]
        if len(hits) != len(top):
            bad.append(qid)
            continue
        i, ok = 0, True
        while ok and i < len(hits):
            j = i
            while j + 1 < len(hits) and abs(hits[j + 1][1] - hits[i][1]) <= tie_eps:
                j += 1
            mine = top[i:j + 1]
            if j + 1 == len(hits):
                ok = all(abs(s - hits[i][1]) <= tie_eps for _, s in mine)
            else:
                ok = {n for n, _ in mine} == {h[0] for h in hits[i:j + 1]}
            i = j + 1
        if not ok:
            bad.append(qid)
    return bad


def c0b(mine_dir, variant, repos, plant_offset=False, runs_dir=None):
    """Identity-W Python ranking from the cache tables vs the run's rankings.jsonl."""
    out = {}
    for repo in repos:
        nt, qt = K.tables_for(mine_dir, variant, repo, runs_dir)
        run = K.Run((Path(runs_dir) if runs_dir else K.C.work_dir() / "runs") / variant / repo)
        nrows, nmat = nt.load([repo])
        qrows, qmat = qt.load([repo])
        # Node order must be the run's node order for the tie rule.
        key_to_id = {v: k for k, v in K.snapshot_keys(repo).items()}
        ids = [key_to_id[r["key"]] for r in nrows]
        if ids != run.node_ids:
            raise SystemExit(f"{repo}: cache node order differs from the run's node_ids.txt")
        if plant_offset:
            # Planted fault: the key -> vector join is off by one row.
            nmat = np.roll(nmat, 1, axis=0)
        scores = normalise(qmat) @ normalise(nmat).T
        rust = {}
        with open(run.dir / "rankings.jsonl") as f:
            for line in f:
                r = json.loads(line)
                rust[r["id"]] = r
        py_top = {}
        for qi, r in enumerate(qrows):
            s = scores[qi]
            order = np.lexsort((np.arange(len(s)), -s))[:TOP]
            py_top[r["key"]] = [(ids[j], float(s[j])) for j in order]
        bad = c0b_compare(py_top, rust)
        out[repo] = {"queries": len(py_top), "mismatches": len(bad), "first": bad[:3]}
    return out


def load_dev_ranks(run_dir):
    rows = K.C.read_jsonl(Path(run_dir) / "dev_ranks.jsonl")
    return {r["id"]: (r["target"], 1.0 / r["rank"] if r["rank"] else 0.0) for r in rows}


def curve_main(a):
    runs = {0.25: load_dev_ranks(a.r25), 0.5: load_dev_ranks(a.r50), 1.0: load_dev_ranks(a.r100)}
    ids = sorted(runs[1.0])
    if any(sorted(r) != ids for r in runs.values()):
        sys.exit("curve: the three runs ranked different dev queries")
    clusters = [runs[1.0][i][0] for i in ids]
    res = curve_rule({f: np.array([r[i][1] for i in ids]) for f, r in runs.items()}, clusters)
    if a.json:
        K.C.write_json(a.json, res)
    print(res["line"])


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    a1 = sub.add_parser("c0b")
    a1.add_argument("--version", default="v0.4-full")
    a1.add_argument("--variant", default="jina-v2-base-code-int8-structured")
    a1.add_argument("--repos", nargs="+", required=True)
    a1.add_argument("--plant-offset", action="store_true", help="dry run: shift the join one row; must fail")
    a1.add_argument("--json")
    a2 = sub.add_parser("curve", help="learning-curve rule over three train_adapter runs' dev_ranks.jsonl")
    a2.add_argument("--r25", required=True)
    a2.add_argument("--r50", required=True)
    a2.add_argument("--r100", required=True)
    a2.add_argument("--json")
    a = ap.parse_args()
    if a.cmd == "curve":
        return curve_main(a)
    vdir = K.C.version_dir(a.version)
    res = c0b(vdir / "mine", a.variant, a.repos, a.plant_offset)
    total = sum(r["mismatches"] for r in res.values())
    verdict = "PASS" if total == 0 else "FAIL"
    if a.json:
        K.C.write_json(a.json, {"repos": res, "verdict": verdict, "plantOffset": a.plant_offset})
    print(json.dumps(res))
    print(f"C0b {'(planted offset) ' if a.plant_offset else ''}{verdict}: {total} mismatching queries")
    sys.exit(0 if total == 0 else 1)


if __name__ == "__main__":
    main()
