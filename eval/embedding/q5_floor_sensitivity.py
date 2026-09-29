#!/usr/bin/env python3
"""Q5 (held-out false alarm, D9) of jina int8 vs fp32 under changed floors.

Reads stored rankings only (no embedding). Ports, line for line, the code
paths `g-mesh debug-embed-eval report` computes Q5 with:

- outcomes:        core/src/cli/embed_eval.rs `load_arm`
- held-out split:  core/src/cli/embed_eval/queries.rs `is_held_out_id`
- floors:          metrics.rs `fit_floors` / `fit_floor` / `round_down_2`
- Q5 indicator:    metrics.rs `false_alarm_indicator`, `paired_at_own_floors`
- bounds:          metrics.rs `bootstrap` with rng.rs SplitMix64 + Lemire
                   `below`, seed and resamples from variants.toml,
                   a fresh generator per bound as in `bound_of`.

Step 0 is a control: the fitted floors, held-out false-alarm rates and the
Q5 table must reproduce the values in docs/results/gm-398-model-comparison.md;
on any mismatch the script stops. Then: the discordant queries, Q5 with the
floors swapped or shared, and Q5 with each floor moved by +-0.01.

usage: q5_floor_sensitivity.py <runs dir> <eval dir> [--json out.json]
       q5_floor_sensitivity.py <runs dir> <eval dir> --proposal

--proposal (GM-422/S2) runs instead: a control against every GM-398 arm, then
Q4/Q5 of every arm under floor-fit and Q5-rule options, and an R-vs-R
self-test of each rule's false-fail rate from floor-fit noise. Needs numpy.
"""
import hashlib
import json
import math
import re
import sys
from pathlib import Path

MASK = (1 << 64) - 1
MAX_FALSE_ALARM = 0.03
LANGS = ["go", "python", "rust", "typescript"]
REF, CAND = "jina-v2-base-code-fp32", "jina-v2-base-code-int8"


# --- rng.rs --------------------------------------------------------------
class Rng:
    def __init__(self, seed):
        self.state = seed & MASK

    def next_u64(self):
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)

    def below(self, n):
        return (self.next_u64() * n) >> 64


# --- metrics.rs ----------------------------------------------------------
def pooled_mean(groups):
    means = [sum(v) / len(v) for _, v in sorted(groups.items()) if v]
    return sum(means) / len(means) if means else None


def bootstrap(groups, resamples, seed):
    point = pooled_mean(groups)
    if point is None:
        return None
    gs = [v for _, v in sorted(groups.items()) if v]  # BTreeMap order
    rng = Rng(seed)
    stats = []
    for _ in range(resamples):
        s = 0.0
        for values in gs:
            n = len(values)
            total = 0.0
            for _ in range(n):
                total += values[rng.below(n)]
            s += total / n
        stats.append(s / len(gs))
    stats.sort()
    tail = math.floor(0.05 * len(stats))
    return (point, stats[tail], stats[len(stats) - 1 - tail])


def round_down_2(x):
    return math.floor(x * 100.0 + 1e-9) / 100.0


def fit_floor_raw(scores):
    s = sorted(scores)
    k = math.floor(MAX_FALSE_ALARM * len(s) + 1e-9)
    return s[min(k, len(s) - 1)], k, len(s)


def fit_floors(outcomes):
    scores = {}
    for o in outcomes:
        in_fit = o["mechanical"] or not o["held_out"]
        if o["positive"] and in_fit and o["rank"] == 1 and o["top"] is not None:
            scores.setdefault(o["language"], []).append(o["top"])
    raw = {l: fit_floor_raw(s) for l, s in scores.items()}
    return {l: round_down_2(r[0]) for l, r in raw.items()}, raw


def fa_indicator(o, floors):
    if not (o["positive"] and not o["mechanical"] and o["held_out"] and o["rank"] == 1):
        return None
    if o["top"] is None or o["top_language"] is None or o["top_language"] not in floors:
        return None
    return 0.0 if o["top"] >= floors[o["top_language"]] else 1.0


def false_alarm_rate(outcomes, floors, lang):
    vals = [fa_indicator(o, floors) for o in outcomes if o["language"] == lang]
    vals = [v for v in vals if v is not None]
    return (sum(vals), len(vals))


def paired(ref, ref_floors, cand, cand_floors):
    by_id = {o["id"]: fa_indicator(o, ref_floors) for o in ref}
    groups = {}
    for o in cand:
        r = by_id.get(o["id"])
        c = fa_indicator(o, cand_floors)
        if r is not None and c is not None:
            groups.setdefault(o["language"], []).append(c - r)
    return groups


# --- loading (embed_eval.rs load_arm, queries.rs load) --------------------
def held_out(qid, mechanical):
    return not mechanical and hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load_queries(eval_dir, corpus):
    qs = {}
    for path, mech in [(eval_dir / "queries" / f"{corpus}.jsonl", False),
                       (eval_dir / "queries" / "mechanical" / f"{corpus}.jsonl", True)]:
        if not path.exists():
            continue
        for line in path.read_text().splitlines():
            if line.strip():
                q = json.loads(line)
                q["mechanical"] = mech or q.get("mechanical", False)
                qs[q["id"]] = q
    return qs


def load_arm(run, eval_dir):
    outs = []
    for d in sorted(p for p in run.iterdir() if (p / "rankings.jsonl").exists()):
        manifest = json.loads((d / "manifest.json").read_text())
        for name, sha in manifest["queryFiles"]:
            actual = hashlib.sha256((eval_dir / name).read_bytes()).hexdigest()
            if actual != sha:
                sys.exit(f"STOP: {name} changed since {d} was run")
        qs = load_queries(eval_dir, manifest["corpus"])
        with open(d / "rankings.jsonl") as f:
            for line in f:
                r = json.loads(line)
                q = qs[r["id"]]
                rank = next((i + 1 for i, (nid, _) in enumerate(r["hits"]) if nid in r["expected"]), None)
                outs.append({
                    "id": q["id"], "corpus": q["corpus"], "language": q["language"],
                    "positive": q["kind"] == "positive", "mechanical": q["mechanical"],
                    "held_out": held_out(q["id"], q["mechanical"]), "rank": rank,
                    "top": r["hits"][0][1] if r["hits"] else None,
                    "top_language": r.get("top_language", r.get("topLanguage")),
                    "text": q["text"],
                    "gold": [e["qualifiedName"] for e in q["expected"]],
                    "expected_score": next((s for nid, s in r["hits"] if nid in r["expected"]), None),
                })
    return outs


# --- analysis --------------------------------------------------------------
def q5(ref, rf, cand, cf, settings):
    g = paired(ref, rf, cand, cf)
    res, seed = settings["bootstrap_resamples"], settings["bootstrap_seed"]
    out = {"pooled": bootstrap(g, res, seed)}
    for l in LANGS:
        if l in g:
            b = bootstrap({l: g[l]}, res, seed)
            out[l] = (*b, len(g[l]), sum(1 for x in g[l] if x > 0), sum(1 for x in g[l] if x < 0))
    return out


def pts(x):
    return f"{100 * x:+.1f}" if abs(100 * x) >= 0.05 else "0.0"


def fmt(q):
    p = q["pooled"]
    s = f"pooled {pts(p[0])} [{pts(p[1])}, {pts(p[2])}]"
    for l in LANGS:
        if l in q:
            pt, lo, up, n, worse, better = q[l]
            s += f" | {l} {pts(pt)} [{pts(lo)}, {pts(up)}] n={n} +{worse}/-{better}"
    return s


# --- --proposal: floor-fit and Q5 options on every GM-398 arm --------------
# Needs numpy; the default run above does not.
ARMS = ["jina-v2-base-code-int8", "gte-small", "bge-small-en-v1.5", "snowflake-arctic-embed-s",
        "random", "shuffled", "words-shuffled"]
CONTROLS = {"random", "shuffled", "words-shuffled"}
SHORT = {"jina-v2-base-code-int8": "int8", "gte-small": "gte", "bge-small-en-v1.5": "bge",
         "snowflake-arctic-embed-s": "snow", "random": "random", "shuffled": "shuffled",
         "words-shuffled": "words-shuf"}
GATE_EPS = 1e-12  # decision.rs EPS
# Cost gates are timing/size measurements from GM-398, independent of floors.
COST_PASS = {"jina-v2-base-code-int8": True, "gte-small": True, "bge-small-en-v1.5": True,
             "snowflake-arctic-embed-s": True}
# Q5 rules compared: current D9, bound only, and a Q1-shaped point tolerance.
RULES = {
    "A": lambda b: b[0] <= GATE_EPS and b[2] <= 0.05 + GATE_EPS,
    "B": lambda b: b[2] <= 0.05 + GATE_EPS,
    "C": lambda b: b[0] <= 0.02 + GATE_EPS and b[2] <= 0.05 + GATE_EPS,
}


def rounder(step):
    if step is None:
        return lambda x: x
    m = round(1 / step)
    return lambda x: math.floor(x * m + 1e-9) / m


def fit_scores(outcomes):
    scores = {}
    for o in outcomes:
        in_fit = o["mechanical"] or not o["held_out"]
        if o["positive"] and in_fit and o["rank"] == 1 and o["top"] is not None:
            scores.setdefault(o["language"], []).append(o["top"])
    return scores


def fit_floors_step(outcomes, step):
    r = rounder(step)
    return {l: r(fit_floor_raw(s)[0]) for l, s in fit_scores(outcomes).items()}


def cw_indicator(o, floors):
    """metrics.rs `confident_wrong`."""
    if o["mechanical"] or not o["held_out"]:
        return None
    if o["top"] is None or o["top_language"] is None or o["top_language"] not in floors:
        return None
    clears = o["top"] >= floors[o["top_language"]]
    wrong = (clears and o["rank"] != 1) if o["positive"] else clears
    return 1.0 if wrong else 0.0


def paired_ind(ref, rf, cand, cf, ind):
    by_id = {o["id"]: ind(o, rf) for o in ref}
    groups = {}
    for o in cand:
        r = by_id.get(o["id"])
        c = ind(o, cf)
        if r is not None and c is not None:
            groups.setdefault(o["language"], []).append(c - r)
    return groups


def paired_value(ref, cand, value):
    """metrics.rs `paired_deltas` over authored positives, grouped by the reference's language."""
    by_id = {o["id"]: o for o in cand}
    groups = {}
    for r in ref:
        if r["positive"] and not r["mechanical"]:
            groups.setdefault(r["language"], []).append(value(by_id[r["id"]]) - value(r))
    return groups


def hit10(o):
    return 1.0 if o["rank"] is not None and o["rank"] <= 10 else 0.0


def rr(o):
    return 1.0 / o["rank"] if o["rank"] is not None and o["rank"] <= 100 else 0.0


class Boot:
    """`bootstrap` vectorised: SplitMix64 output k is mix(seed + k*gamma), drawn
    resample-major, group by group in sorted order, so index draws depend only
    on the group sizes and are cached per size tuple."""

    def __init__(self, np, resamples, seed):
        self.np, self.resamples, self.seed, self.cache = np, resamples, seed, {}

    def indices(self, sizes):
        if sizes in self.cache:
            return self.cache[sizes]
        np = self.np
        u = np.uint64
        total = sum(sizes)
        k = np.arange(1, self.resamples * total + 1, dtype=np.uint64)
        with np.errstate(over="ignore"):
            z = u(self.seed) + k * u(0x9E3779B97F4A7C15)
            z = (z ^ (z >> u(30))) * u(0xBF58476D1CE4E5B9)
            z = (z ^ (z >> u(27))) * u(0x94D049BB133111EB)
            z = z ^ (z >> u(31))
        z = z.reshape(self.resamples, total)
        out, off = [], 0
        for n in sizes:
            zz = z[:, off:off + n]
            hi, lo = zz >> u(32), zz & u(0xFFFFFFFF)
            idx = (hi * u(n) + ((lo * u(n)) >> u(32))) >> u(32)  # (z * n) >> 64, exact
            out.append(idx.astype(np.int64))
            off += n
        self.cache[sizes] = out
        return out

    def bound_from_stats(self, point, stats):
        stats = self.np.sort(stats)
        tail = math.floor(0.05 * self.resamples)
        return (point, float(stats[tail]), float(stats[self.resamples - 1 - tail]))

    def bound(self, groups):
        np = self.np
        keys = [k for k in sorted(groups) if groups[k]]
        if not keys:
            return None
        vals = [np.asarray(groups[k], dtype=float) for k in keys]
        point = pooled_mean({k: groups[k] for k in keys})
        idx = self.indices(tuple(len(v) for v in vals))
        stats = sum(v[i].mean(axis=1) for v, i in zip(vals, idx)) / len(vals)
        return self.bound_from_stats(point, stats)

    def bound_matrix(self, point, D, cols_by_group):
        """Bound when each resample b has its own per-query values D[b, :]."""
        np = self.np
        keys = sorted(cols_by_group)
        idx = self.indices(tuple(len(cols_by_group[k]) for k in keys))
        stats = 0.0
        for k, i in zip(keys, idx):
            sub = D[:, cols_by_group[k]]
            stats = stats + np.take_along_axis(sub, i, axis=1).mean(axis=1)
        return self.bound_from_stats(point, stats / len(keys))


def resampled_floors(np, rng, arr, resamples, step, idx=None):
    """Raw fit_floor of each bootstrap resample of `arr` (NaN = not right first), rounded."""
    m = len(arr)
    if idx is None:
        idx = rng.integers(0, m, size=(resamples, m))
    s = np.sort(arr[idx], axis=1)
    n = (~np.isnan(s)).sum(axis=1)
    k = np.floor(0.03 * n + 1e-9).astype(np.int64)
    raw = s[np.arange(resamples), np.maximum(np.minimum(k, n - 1), 0)]
    raw = np.where(n > 0, raw, np.nan)
    if step is None:
        return raw
    mult = round(1 / step)
    return np.floor(raw * mult + 1e-9) / mult


def fit_arrays(np, ref, cand):
    """Per language, both arms' fit-half right-first scores aligned by query (NaN = not right first)."""
    by_id = {o["id"]: o for o in cand}
    out = {}
    for r in ref:
        if r["positive"] and (r["mechanical"] or not r["held_out"]):
            c = by_id[r["id"]]
            out.setdefault(r["language"], ([], []))
            out[r["language"]][0].append(r["top"] if r["rank"] == 1 and r["top"] is not None else np.nan)
            out[r["language"]][1].append(c["top"] if c["rank"] == 1 and c["top"] is not None else np.nan)
    return {l: (np.array(a, dtype=float), np.array(b, dtype=float)) for l, (a, b) in out.items()}


def pair_rows(ref, cand, langs, kind):
    """Held-out pairs Q5 ("fa") or Q4 ("cw") judges, as
    (top_r, lang_r, flag_r, top_c, lang_c, flag_c, group); flag = a clearing top hit is wrong."""
    by_id = {o["id"]: o for o in cand}
    rows = []
    for r in ref:
        c = by_id[r["id"]]
        if r["mechanical"] or not r["held_out"]:
            continue
        if kind == "fa" and not (r["positive"] and r["rank"] == 1 and c["rank"] == 1):
            continue
        if any(o["top"] is None or o["top_language"] not in langs for o in (r, c)):
            continue
        rows.append(tuple(x for o in (r, c) for x in (o["top"], langs.index(o["top_language"]),
                                                       (not o["positive"]) or o["rank"] != 1))
                    + (c["language"],))
    return rows


def joint_bound(np, boot, rng, fits_ref, fits_cand, paired_fit, rows, step, point, kind="fa"):
    """Bound with floor-fit uncertainty: each resample refits both arms' floors on a
    bootstrap resample of the fit half (the same queries for both arms when
    `paired_fit`), then judges a resample of the held-out pairs."""
    langs = sorted(fits_ref)
    R = boot.resamples
    Fr = np.empty((R, len(langs)))
    Fc = np.empty((R, len(langs)))
    for j, l in enumerate(langs):
        a, b = fits_ref[l], fits_cand[l]
        idx = rng.integers(0, len(a), size=(R, len(a)))
        Fr[:, j] = resampled_floors(np, rng, a, R, step, idx)
        Fc[:, j] = resampled_floors(np, rng, b, R, step, idx if paired_fit else None)
    col = lambda i: np.array([x[i] for x in rows])

    def ind(top, lang, flag, F):
        if kind == "fa":
            return (top[None, :] < F[:, lang]).astype(float)
        return ((top[None, :] >= F[:, lang]) & flag[None, :]).astype(float)

    D = ind(col(3), col(4), col(5), Fc) - ind(col(0), col(1), col(2), Fr)
    cols = {}
    for i, x in enumerate(rows):
        cols.setdefault(x[6], []).append(i)
    return boot.bound_matrix(point, D, {k: np.array(v) for k, v in cols.items()})


def pb(b):
    return "-" if b is None else f"{pts(b[0])} [{pts(b[1])}, {pts(b[2])}]"


def proposal(runs, eval_dir, settings):
    import numpy as np
    boot = Boot(np, settings["bootstrap_resamples"], settings["bootstrap_seed"])
    # control: the vectorised draws equal Rng.below
    rng0, idx0 = Rng(settings["bootstrap_seed"]), boot.indices((3, 7))
    check = [[rng0.below(n) for n in (3,) * 3 + (7,) * 7] for _ in range(2)]
    got = [list(idx0[0][r]) + list(idx0[1][r]) for r in range(2)]
    if check != got:
        sys.exit(f"CONTROL FAILED: vectorised SplitMix {got} != {check}")

    ref = load_arm(runs / REF, eval_dir)
    arms = {a: load_arm(runs / a, eval_dir) for a in ARMS}
    langs_all = LANGS

    # 0. control against gm-398-model-comparison.md
    want_floors = {
        REF: [.56, .58, .56, .55], "jina-v2-base-code-int8": [.57, .57, .55, .53],
        "gte-small": [.86, .86, .85, .84], "bge-small-en-v1.5": [.69, .71, .70, .67],
        "snowflake-arctic-embed-s": [.57, .57, .59, .58]}
    want = {  # Q1 (pt, lower), Q2 (pt, lower), Q4 (pt, upper), Q5 (pt, lo, up)
        "jina-v2-base-code-int8": ((0.5, -1.0), (-0.003, -0.012), (-1.4, 1.0), (2.1, -1.4, 6.2)),
        "gte-small": ((-5.3, -9.2), (-0.055, -0.086), (11.1, 16.7), (-10.3, -22.3, 0.0)),
        "bge-small-en-v1.5": ((-7.5, -11.8), (-0.068, -0.100), (14.8, 20.1), (-12.1, -25.0, -0.2)),
        "snowflake-arctic-embed-s": ((-8.2, -12.5), (-0.058, -0.092), (24.6, 30.3), (-10.8, -21.6, -2.3))}
    bad = []
    rf = fit_floors_step(ref, 0.01)
    for name, arm in [(REF, ref)] + list(arms.items()):
        if name in want_floors:
            fl = fit_floors_step(arm, 0.01)
            if [fl.get(l) for l in LANGS] != want_floors[name]:
                bad.append(f"{name} floors {fl}")
    base = {}
    for a in ARMS:
        arm = arms[a]
        cf = fit_floors_step(arm, 0.01)
        q1 = boot.bound(paired_value(ref, arm, hit10))
        q2 = boot.bound(paired_value(ref, arm, rr))
        q3 = {l: sum(v) / len(v) for l, v in paired_value(ref, arm, hit10).items()}
        q4 = boot.bound(paired_ind(ref, rf, arm, cf, cw_indicator))
        q5b = boot.bound(paired_ind(ref, rf, arm, cf, fa_indicator))
        base[a] = (q1, q2, q3, q4)
        if a in want:
            w1, w2, w4, w5 = want[a]
            g1 = (round(100 * q1[0], 1), round(100 * q1[1], 1))
            g2 = (round(q2[0], 3), round(q2[1], 3))
            g4 = (round(100 * q4[0], 1), round(100 * q4[2], 1))
            g5 = tuple(round(100 * x, 1) + 0.0 for x in q5b)
            for lab, g, w in [("Q1", g1, w1), ("Q2", g2, w2), ("Q4", g4, w4), ("Q5", g5, w5)]:
                if tuple(x + 0.0 for x in g) != tuple(x + 0.0 for x in w):
                    bad.append(f"{a} {lab} {g} != {w}")
    print("== 0. control: floors, Q1, Q2, Q4, Q5 of every candidate vs gm-398-model-comparison.md ==")
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("CONTROL OK (vectorised SplitMix == Rng.below; 5 arms' floors; 4 candidates' Q1/Q2/Q4/Q5)")

    # floor-independent gates
    print("\n== floor-independent gates Q1-Q3 (and cost from GM-398) ==")
    q123 = {}
    for a in ARMS:
        q1, q2, q3, _ = base[a]
        p1 = q1[0] >= -0.02 - GATE_EPS and q1[1] >= -0.05 - GATE_EPS
        p2 = q2[0] >= -0.02 - GATE_EPS and q2[1] >= -0.05 - GATE_EPS
        p3 = bool(q3) and all(d >= -0.10 - GATE_EPS for d in q3.values())
        q123[a] = p1 and p2 and p3
        print(f"{SHORT[a]:10s} Q1 {pb(q1)} {'ok' if p1 else 'FAIL'}  Q2 {q2[0]:+.3f} [{q2[1]:+.3f}] "
              f"{'ok' if p2 else 'FAIL'}  Q3 min {pts(min(q3.values()))} {'ok' if p3 else 'FAIL'}  "
              f"cost {COST_PASS.get(a, 'n/a')}")

    # fit-half noise of the raw floor
    rng = np.random.default_rng(422)
    print("\n== fit noise: raw floor (4th-ish lowest right-first fit score) and its bootstrap SD ==")
    for name, arm in [(REF, ref)] + list(arms.items()):
        sc = fit_scores(arm)
        parts = []
        for l in LANGS:
            if l not in sc:
                parts.append(f"{l} -")
                continue
            a = np.array(sc[l])
            sd = np.nanstd(resampled_floors(np, rng, a, 2000, None))
            parts.append(f"{l} {fit_floor_raw(sc[l])[0]:.4f}±{sd:.4f} (n={len(a)})")
        print(f"{SHORT.get(name, 'fp32'):10s} " + "  ".join(parts))

    # options
    options = [  # label, step, floors mode, ci
        ("O0 baseline: own floors, 0.01, fixed CI", 0.01, "own", "fixed"),
        ("O1 own floors, step 0.001", 0.001, "own", "fixed"),
        ("O2 own floors, unrounded", None, "own", "fixed"),
        ("O3 shared: Q5 with both arms at R's 0.01 floors", 0.01, "shared", "fixed"),
        ("O4 own 0.01 floors, joint CI (floors refit per resample)", 0.01, "own", "joint"),
        ("O5 own unrounded floors, joint CI", None, "own", "joint"),
    ]
    print("\n== options: Q4 pt [lo, up] (D9 rule A) | Q5 pt [lo, up] n, "
          "pass under rules A (D9: pt<=0 & up<=5) / B (up<=5) / C (pt<=+2 & up<=5); "
          "overall = Q1-Q3 & Q4 & Q5 & cost, with Q4 and Q5 under the same rule ==")
    summary = {}
    for label, step, mode, ci in options:
        print(f"\n-- {label}")
        for a in ARMS:
            q4, q5, n = evaluate(np, boot, ref, arms[a], step, mode, ci)
            passes = {r: (q5 is not None and f(q5)) for r, f in RULES.items()}
            q4p = {r: (q4 is not None and f(q4)) for r, f in RULES.items()}
            verdict = {r: q123[a] and q4p[r] and passes[r] and COST_PASS.get(a, False) for r in RULES}
            summary[(label, a)] = (q4, q5, n, passes, verdict)
            print(f"{SHORT[a]:10s} Q4 {pb(q4)} A {'ok' if q4p['A'] else 'FAIL'} B {'ok' if q4p['B'] else 'FAIL'}"
                  f" | Q5 {pb(q5)} n={n} "
                  f"A {'ok' if passes['A'] else 'FAIL'} B {'ok' if passes['B'] else 'FAIL'} "
                  f"C {'ok' if passes['C'] else 'FAIL'} | overall A/B/C "
                  + "/".join("PASS" if verdict[r] else "fail" for r in RULES))
        winners = {r: [SHORT[a] for a in ARMS if a not in CONTROLS and summary[(label, a)][4][r]] for r in RULES}
        print("   selection: " + "  ".join(f"{r}: {', '.join(w) if w else 'keep fp32'}" for r, w in winners.items()))

    # Simulated pairs built from R, same rankings, top scores jittered. "equal": both arms
    # get independent jitter of sd/sqrt(2), so they are equal in expectation (true delta 0).
    # "noisier copy": only the candidate is jittered, so it is R plus noise. "harm": on top
    # of "equal", a share h of the candidate's held-out right-first queries is forced below
    # every floor (true Q5 delta about +h * (1 - R's rate)).
    print("\n== simulation (floors refit on the same fit queries; reps 200 fixed CI / 100 joint CI). "
          "equal/noisier: share of reps FAILING; harm: share PASSING ==")
    sims = [("equal, 0.006 apart", 0.006, True, 0.0), ("equal, 0.012 apart", 0.012, True, 0.0),
            ("noisier copy, 0.012", 0.012, False, 0.0),
            ("harm h=0.06, equal 0.012", 0.012, True, 0.06), ("harm h=0.12, equal 0.012", 0.012, True, 0.12)]
    sim_opts = [o for o in options if o[0][:2] in ("O0", "O3", "O4")]

    def jitter(outcomes, sd, h, rng):
        out = []
        for o in outcomes:
            c = dict(o)
            if c["top"] is not None:
                c["top"] = c["top"] + rng.normal(0.0, sd)
                if (h and c["positive"] and not c["mechanical"] and c["held_out"]
                        and c["rank"] == 1 and rng.random() < h):
                    c["top"] = -1.0
            out.append(c)
        return out

    for slabel, sd, sym, h in sims:
        for label, step, mode, ci in sim_opts:
            reps = 100 if ci == "joint" else 200
            rng = np.random.default_rng(4221)
            f5 = {r: 0 for r in RULES}
            f4 = {r: 0 for r in RULES}
            pt5 = pt4 = 0.0
            for _ in range(reps):
                r_arm = jitter(ref, sd / math.sqrt(2), 0.0, rng) if sym else ref
                c_arm = jitter(ref, sd / math.sqrt(2) if sym else sd, h, rng)
                b4, b5, _ = evaluate(np, boot, r_arm, c_arm, step, mode, ci, seed=int(rng.integers(1 << 31)))
                pt5 += b5[0]
                pt4 += b4[0]
                for r, f in RULES.items():
                    f5[r] += not f(b5)
                    f4[r] += not f(b4)
            q5s = " ".join(f"{r} {100 * (reps - f5[r] if h else f5[r]) / reps:.0f}%" for r in RULES)
            q4s = " ".join(f"{r} {100 * f4[r] / reps:.0f}%" for r in "AB")
            print(f"{slabel:26s} {label[:2]}: Q5 mean {pts(pt5 / reps)}, {'pass' if h else 'fail'} {q5s} | "
                  f"Q4 mean {pts(pt4 / reps)}, fail {q4s}")


def evaluate(np, boot, ref, arm, step, mode, ci, seed=398, with_q4=True):
    """Q4 and Q5 bounds of `arm` against `ref` under one floor/CI option, and Q5's n."""
    rfs, cfs = fit_floors_step(ref, step), fit_floors_step(arm, step)
    q4 = boot.bound(paired_ind(ref, rfs, arm, cfs, cw_indicator)) if with_q4 else None
    g = paired_ind(ref, rfs, arm, rfs if mode == "shared" else cfs, fa_indicator)
    n = sum(len(v) for v in g.values())
    q5 = boot.bound(g)
    if ci == "joint":
        fr = fit_arrays(np, ref, arm)
        if min(np.sum(~np.isnan(x)) for pair in fr.values() for x in pair) < 10:
            return None, None, n  # too few right-first fit scores to refit
        langs = sorted(fr)
        fits_r, fits_c = {l: v[0] for l, v in fr.items()}, {l: v[1] for l, v in fr.items()}
        if q5 is not None:
            q5 = joint_bound(np, boot, np.random.default_rng(seed), fits_r, fits_c, True,
                             pair_rows(ref, arm, langs, "fa"), step, q5[0], "fa")
        if q4 is not None:
            q4 = joint_bound(np, boot, np.random.default_rng(seed), fits_r, fits_c, True,
                             pair_rows(ref, arm, langs, "cw"), step, q4[0], "cw")
    return q4, q5, n


def main():
    runs, eval_dir = Path(sys.argv[1]), Path(sys.argv[2])
    if "--proposal" in sys.argv:
        toml = (eval_dir / "variants.toml").read_text()
        return proposal(runs, eval_dir, {k: int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))
                                         for k in ("bootstrap_seed", "bootstrap_resamples")})
    toml = (eval_dir / "variants.toml").read_text()  # python 3.9: no tomllib
    settings = {k: int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))
                for k in ("bootstrap_seed", "bootstrap_resamples")}
    ref = load_arm(runs / REF, eval_dir)
    cand = load_arm(runs / CAND, eval_dir)
    rf, rraw = fit_floors(ref)
    cf, craw = fit_floors(cand)
    result = {}

    # 0. control
    print("== 0. control: reproduce gm-398-model-comparison.md ==")
    want_floors = {REF: {"go": .56, "python": .58, "rust": .56, "typescript": .55},
                   CAND: {"go": .57, "python": .57, "rust": .55, "typescript": .53}}
    want_fa = {REF: [18.8, 9.5, 14.3, 14.8], CAND: [29.4, 10.0, 14.3, 7.7]}
    want_q5 = {"pooled": (2.1, -1.4, 6.2), "go": (12.5, 0.0, 25.0, 16),
               "python": (0.0, 0.0, 0.0, 20), "rust": (0.0, 0.0, 0.0, 6),
               "typescript": (-4.0, -12.0, 0.0, 25)}
    bad = []
    for name, arm, fl in [(REF, ref, rf), (CAND, cand, cf)]:
        if fl != want_floors[name]:
            bad.append(f"{name} floors {fl} != {want_floors[name]}")
        got = [round(100 * e / t, 1) for e, t in (false_alarm_rate(arm, fl, l) for l in LANGS)]
        if got != want_fa[name]:
            bad.append(f"{name} held-out FA {got} != {want_fa[name]}")
        print(f"{name}: floors {fl}  held-out FA % {got}")
    base = q5(ref, rf, cand, cf, settings)
    for k, w in want_q5.items():
        g = base[k]
        got = tuple(round(100 * x, 1) + 0.0 for x in g[:3]) + ((g[3],) if k != "pooled" else ())
        if got != w:
            bad.append(f"Q5 {k} {got} != {w}")
    print("Q5 fitted:", fmt(base))
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("CONTROL OK: floors, held-out FA and Q5 table match gm-398-model-comparison.md at 0.1-pt precision")
    result["fitted"] = {"floors": {REF: rf, CAND: cf}, "q5": base}

    # fit detail: raw k-th score before rounding
    print("\n== fit detail (raw floor before round-down, k, n fit-half right-first) ==")
    for l in LANGS:
        print(f"{l:10s} fp32 raw {rraw[l][0]:.4f} (k={rraw[l][1]}, n={rraw[l][2]}) -> {rf[l]:.2f}   "
              f"int8 raw {craw[l][0]:.4f} (k={craw[l][1]}, n={craw[l][2]}) -> {cf[l]:.2f}")
    result["raw_floors"] = {REF: rraw, CAND: craw}

    # 1. the discordant queries
    print("\n== 1. discordant paired queries (int8 FA=1, fp32 FA=0 or reverse) ==")
    by_id = {o["id"]: o for o in ref}
    disc = []
    for o in cand:
        r = by_id[o["id"]]
        a, b = fa_indicator(r, rf), fa_indicator(o, cf)
        if a is not None and b is not None and a != b:
            row = {"id": o["id"], "language": o["language"], "text": o["text"], "gold": o["gold"],
                   "fp32": {"rank": r["rank"], "top": r["top"], "fa": a},
                   "int8": {"rank": o["rank"], "top": o["top"], "fa": b}}
            disc.append(row)
            print(f"{o['id']} [{o['language']}] int8-fp32 FA {b - a:+.0f}  gold={o['gold']}\n"
                  f"   text: {o['text']}\n"
                  f"   fp32 rank {r['rank']} top {r['top']:.4f} (vs fp32 floor {rf[o['language']]:.2f}, "
                  f"int8 floor {cf[o['language']]:.2f})\n"
                  f"   int8 rank {o['rank']} top {o['top']:.4f} (vs int8 floor {cf[o['language']]:.2f}, "
                  f"fp32 floor {rf[o['language']]:.2f})   Δscore {o['top'] - r['top']:+.4f}")
    result["discordant"] = disc

    # score shift on all paired held-out right-first queries
    print("\n== score shift int8 - fp32 on paired held-out right-first queries ==")
    shifts = {}
    for o in cand:
        r = by_id[o["id"]]
        if fa_indicator(r, rf) is not None and fa_indicator(o, cf) is not None:
            shifts.setdefault(o["language"], []).append(o["top"] - r["top"])
    for l in LANGS:
        s = sorted(shifts.get(l, []))
        if s:
            print(f"{l:10s} n={len(s)} median {s[len(s)//2]:+.4f} min {s[0]:+.4f} max {s[-1]:+.4f} "
                  f"mean|d| {sum(map(abs, s))/len(s):.4f}")
    result["score_shift"] = shifts

    # 2. swapped floors and common floors
    print("\n== 2. floors swapped / common ==")
    variants = {
        "swapped (int8 @ fp32 floors, fp32 @ int8 floors)": (cf, rf),
        "both @ fp32 floors": (rf, rf),
        "both @ int8 floors": (cf, cf),
    }
    result["floor_variants"] = {}
    for label, (r_fl, c_fl) in variants.items():
        q = q5(ref, r_fl, cand, c_fl, settings)
        result["floor_variants"][label] = q
        print(f"{label}:\n   {fmt(q)}")

    # 3. sensitivity +-0.01 per arm per language
    print("\n== 3. sensitivity: one arm's floor in one language moved by +-0.01 ==")
    result["sensitivity"] = {}
    for arm in (REF, CAND):
        for l in LANGS:
            for d in (-0.01, +0.01):
                r_fl, c_fl = dict(rf), dict(cf)
                target = r_fl if arm == REF else c_fl
                target[l] = round(target[l] + d, 2)
                q = q5(ref, r_fl, cand, c_fl, settings)
                own = cand if arm == CAND else ref
                e, t = false_alarm_rate(own, target, l)
                key = f"{arm.split('-')[-1]} {l} {d:+.2f} -> {target[l]:.2f}"
                result["sensitivity"][key] = {"q5": q, "own_fa": (e, t)}
                p = q["pooled"]
                lq = q.get(l)
                lang_s = f"{l} {pts(lq[0])} [{pts(lq[1])}, {pts(lq[2])}]" if lq else f"{l} -"
                gate = "pass" if p[0] <= 1e-9 and p[2] <= 0.05 + 1e-9 else "FAIL"
                print(f"{key:28s} own FA {int(e)}/{t}  pooled {pts(p[0])} [{pts(p[1])}, {pts(p[2])}] {gate}  | {lang_s}")

    if "--json" in sys.argv:
        Path(sys.argv[sys.argv.index("--json") + 1]).write_text(json.dumps(result, indent=1, default=str))


if __name__ == "__main__":
    main()
