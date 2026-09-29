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


def main():
    runs, eval_dir = Path(sys.argv[1]), Path(sys.argv[2])
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
