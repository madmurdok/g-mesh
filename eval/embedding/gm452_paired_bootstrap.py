#!/usr/bin/env python3
"""GM-452/S2: paired per-language bootstrap of base vs candidate arm deltas.

Reads stored rankings of two arms (no re-embedding). For each population
(NL held-out authored queries; NAME held-out mechanical queries) and each
language, resamples query ids with replacement, the SAME ids for both arms,
and reports delta = candidate - base of:
  misled  = rank-1 positives whose top row is below the floor / rank-1 positives
  cw_pos  = positives with a wrong top row that clears the floor / positives
  cw_abs  = absent-answer pages whose top row clears the floor / absent pages
Floors are fixed per arm (--floors d6: each arm's own D6 refit at equal false
alarm, q5.fit_floors on the full arm; shipped: the structured floors of
similarity.rs), not refitted per resample. The pooled row resamples within
each language (stratified) and pools the counts. Percentile 95% interval,
default seed 398 and 10,000 resamples (variants.toml). Needs numpy.

Stack mode (--base-stack FILE --stack FILE): the two files are the --per-query-out JSONL of
gm461_learned_verdict.py run on each recall arm (e.g. base = today's stack, candidate = J + L6); the arm
named by --base-arm/--stack-arm (default F4: blend order, shipped floors) is compared, paired by query id
on the held-out half of each population, same metrics, stratification and seed.
Resamples where a metric's denominator is empty (rate NaN, dropped by nanpercentile) are counted:
stderr line and "nan_resamples" in --out.

usage: gm452_paired_bootstrap.py --base-run <runs/base> --run <runs/candidate>
           --eval-dir eval/embedding/v2 [--floors d6|shipped] [--out out.json]
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm697_floor_refit as G  # noqa: E402
import q5_floor_sensitivity as q5  # noqa: E402
import shipped_floor_rates as sfr  # noqa: E402

SEED, RESAMPLES = 398, 10000
LANGS = G.LANGS
METRICS = ("misled", "cw_pos", "cw_abs")


def arm_floors(outs, kind):
    return sfr.structured_floors() if kind == "shipped" else q5.fit_floors(outs)[0]


def indicators(outs, floors, pop):
    """id -> (language, [num, den] x METRICS) for the population's usable rows."""
    res = {}
    for o in outs:
        if o["top"] is None:
            continue
        f = G.fl(floors, o)
        if f is None:
            continue
        if pop == "NAME":
            if not (o["mechanical"] and o["name_ho"]):
                continue
        elif o["mechanical"] or not o["held_out"]:
            continue
        below = o["top"] < f
        res[o["id"]] = (o["language"], vector(o["positive"], o["rank1"], below))
    return res


def vector(positive, rank1, below):
    """[num, den] x METRICS for one query."""
    v = np.zeros(6)
    if positive:
        if rank1:
            v[0], v[1] = below, 1
        v[2], v[3] = (not rank1) and not below, 1
    else:
        v[4], v[5] = not below, 1
    return v


def stack_indicators(path, arm, pop):
    """id -> (language, vector) from a gm461 --per-query-out JSONL: held-out queries of `pop`, rows of `arm`."""
    res = {}
    for line in Path(path).read_text().splitlines():
        r = json.loads(line)
        if r["arm"] == arm and r["pop"] == pop and r["ho"]:
            res[r["id"]] = (r["language"], vector(r["positive"], r["rank1"], r["below"]))
    return res


def paired(base, cand):
    """language -> (ids sorted, base matrix n x 6, cand matrix n x 6) over ids in both arms."""
    ids = sorted(set(base) & set(cand))
    out = {}
    for l in LANGS:
        sel = [i for i in ids if base[i][0] == l]
        if sel:
            out[l] = (np.array([base[i][1] for i in sel]), np.array([cand[i][1] for i in sel]))
    return out, len(base), len(cand), len(ids)


def rates(sums):
    return [sums[2 * m] / sums[2 * m + 1] if sums[2 * m + 1] else np.nan for m in range(3)]


def delta(b, c):
    return [x - y for x, y in zip(rates(c), rates(b))]


def run(groups, rng, resamples, stats=None):
    """groups: [(Bmat, Cmat)] strata. Returns point, lo, hi arrays (3,) for the pooled strata.
    stats (dict, optional): gets "nan_resamples", per metric the resamples nanpercentile ignored (empty denominator)."""
    point = np.array(delta(sum(g[0].sum(0) for g in groups), sum(g[1].sum(0) for g in groups)))
    draws = np.empty((resamples, 3))
    for r in range(resamples):
        bs, cs = np.zeros(6), np.zeros(6)
        for B, C in groups:
            idx = rng.integers(0, len(B), len(B))
            bs += B[idx].sum(0)
            cs += C[idx].sum(0)
        draws[r] = delta(bs, cs)
    lo, hi = np.nanpercentile(draws, [2.5, 97.5], axis=0)
    if stats is not None:
        stats["nan_resamples"] = [int(x) for x in np.isnan(draws).sum(0)]
    return point, lo, hi


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base-run")
    ap.add_argument("--run", help="candidate arm runs dir")
    ap.add_argument("--eval-dir")
    ap.add_argument("--base-stack", help="stack mode: base per-query JSONL (gm461 --per-query-out)")
    ap.add_argument("--stack", help="stack mode: candidate per-query JSONL")
    ap.add_argument("--base-arm", default="F4")
    ap.add_argument("--stack-arm", default="F4")
    ap.add_argument("--floors", choices=["d6", "shipped"], default="d6")
    ap.add_argument("--seed", type=int, default=SEED)
    ap.add_argument("--resamples", type=int, default=RESAMPLES)
    ap.add_argument("--out")
    a = ap.parse_args()
    stack = bool(a.base_stack or a.stack)
    if stack:
        if not (a.base_stack and a.stack):
            ap.error("stack mode needs both --base-stack and --stack")
        if a.base_run or a.run:
            ap.error("--base-stack/--stack exclude --base-run/--run")
        res = {"seed": a.seed, "resamples": a.resamples, "mode": "stack", "base_arm": a.base_arm, "stack_arm": a.stack_arm, "rows": {}}
        print(f"stack: base {a.base_stack}[{a.base_arm}] candidate {a.stack}[{a.stack_arm}]")
        get = lambda pop: (stack_indicators(a.base_stack, a.base_arm, pop), stack_indicators(a.stack, a.stack_arm, pop))
    else:
        if not (a.base_run and a.run and a.eval_dir):
            ap.error("--base-run, --run and --eval-dir are required (or --base-stack and --stack)")
        bo, co = G.load(a.base_run, a.eval_dir), G.load(a.run, a.eval_dir)
        bf, cf = arm_floors(bo, a.floors), arm_floors(co, a.floors)
        res = {"seed": a.seed, "resamples": a.resamples, "floors_kind": a.floors, "base_floors": bf, "cand_floors": cf, "rows": {}}
        print(f"floors {a.floors}: base {bf} candidate {cf}")
        get = lambda pop: (indicators(bo, bf, pop), indicators(co, cf, pop))
    print("| population | language | n paired | d misled (pts) | d CW pos (pts) | d CW absent (pts) |")
    print("|---|---|---|---|---|---|")
    nan_total = [0, 0, 0]
    for pop in ("NL", "NAME"):
        pr, nb, nc, npair = paired(*get(pop))
        if npair != nb or npair != nc:
            print(f"note: {pop}: ids differ between arms (base {nb}, candidate {nc}, paired {npair}); unpaired ids dropped")
        for lang in list(LANGS) + ["pooled"]:
            groups = [pr[l] for l in LANGS if l in pr] if lang == "pooled" else ([pr[lang]] if lang in pr else [])
            if not groups:
                continue
            rng = np.random.default_rng(a.seed)
            st = {}
            p, lo, hi = run(groups, rng, a.resamples, st)
            nan_total = [x + y for x, y in zip(nan_total, st["nan_resamples"])]
            n = sum(len(g[0]) for g in groups)
            cells = [f"{100*p[m]:+.1f} [{100*lo[m]:+.1f}, {100*hi[m]:+.1f}]" for m in range(3)]
            print(f"| {pop} | {lang} | {n} | " + " | ".join(cells) + " |")
            res["rows"][f"{pop}/{lang}"] = {"n": n, "nan_resamples": st["nan_resamples"], **{m: {"point": float(p[i]), "lo": float(lo[i]), "hi": float(hi[i])}
                                                       for i, m in enumerate(METRICS)}}
    res["nan_resamples_total"] = nan_total
    print(f"nan resamples dropped by nanpercentile (misled, cw_pos, cw_abs; summed over rows): {nan_total}", file=sys.stderr)
    if a.out:
        Path(a.out).write_text(json.dumps(res, indent=1))


if __name__ == "__main__":
    main()
