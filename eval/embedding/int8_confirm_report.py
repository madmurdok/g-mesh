#!/usr/bin/env python3
"""Gated verdict of the GM-422 confirmatory int8 study at FROZEN floors.

Implements section 4 ("Analysis tooling") and section 5 ("Controls") of
docs/architecture/embedding-eval-int8-confirm.md. `g-mesh debug-embed-eval
report` refits floors, so the gated numbers come from here. Reads stored
rankings only; embeds nothing.

- Floors are GM-398's fitted values, as constants (fp32 go/python/rust/ts
  0.56/0.58/0.56/0.55, int8 0.57/0.57/0.55/0.53). Never refit.
- Every `-c` query is held-out; the sha256(id) parity split is not used.
- Q1-Q3 as D9 (point and bound), Q4 and Q5 under rule B: one-sided 95% upper
  bound <= +5 points only. Cost is GM-398's (int8 passes on size and RSS).
- Bootstrap as the harness: SplitMix64, seed and resamples from
  variants.toml (398, 10,000), percentile, 5% tails, languages weighted
  equally, a fresh generator per bound (q5_floor_sensitivity.Boot, checked
  here against the scalar Rng.below port).

Loaders, outcome fields and indicators are q5_floor_sensitivity.py's ports of
embed_eval.rs / metrics.rs; this script adds only the frozen floors, the
all-held-out treatment, D7's broken-arm check and the planted-harm arms.

Controls (any failure: no verdict, exit 2, fp32 stays):
  A  on the OLD GM-398 runs, parity split: refitted floors equal the frozen
     ones; int8's Q1/Q2/Q4/Q5 equal GM-398's (Q5 +2.1 [-1.4, +6.2]); D7
     reproduces (gaps 62.8/61.8, ratios 0.008/0.024, random 0.005 vs chance
     0.015).
  2  each confirm run's vectors.bin sha256 equals the GM-398 run's (fp32,
     int8, random, shuffled).
  3  D7 on the new queries: fp32 and int8 each clearly above random and
     shuffled (gap >= 20 points, ratio <= 0.25, bounds separate); random at
     chance (<= 3x chance + 2 points); and random/shuffled as candidates fail.
  4  planted harm, seed 4223: int8-harm10 (int8_confirm_power.py's harm5
     shares doubled) must fail Q4 and Q5; int8-harm5 is reported.
  5  null: fp32 against itself is exactly 0 on every gate and passes.

usage:
  int8_confirm_report.py --old-runs DIR --old-eval-dir DIR \\
      --runs DIR --eval-dir DIR [--json OUT]          the confirm verdict
  int8_confirm_report.py --old-runs DIR --old-eval-dir DIR --rehearse [--json OUT]
      dry run before the new runs exist: the OLD runs play the confirm runs,
      every authored GM-398 query treated as held-out. Exercises controls 2-5
      and the verdict path; its verdict means nothing.
  --controls-only (with --runs/--eval-dir): controls A and 2-5 only, blind to
      int8. The int8 verdict arm, the int8-at-fp32-floors view and the verdict
      line are skipped, and arms derived from int8 (control 3's int8 lines,
      control 4's planted-harm arms) print pass/fail without figures. Exit 2
      on a control failure, else 0. The embedding slice (GM-422/S7) runs this;
      the verdict run (S9) runs without it.
"""
import hashlib
import json
import math
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import q5_floor_sensitivity as q5s  # noqa: E402
import int8_confirm_power as icp  # noqa: E402

import numpy as np  # noqa: E402

LANGS = q5s.LANGS
REF, CAND = q5s.REF, q5s.CAND
RANDOM, SHUFFLED = "random", "shuffled"
FROZEN = {REF: dict(zip(LANGS, icp.FROZEN[REF])), CAND: dict(zip(LANGS, icp.FROZEN[CAND]))}
EPS = q5s.GATE_EPS
MARGIN = 0.05
COST_PASS_INT8 = True  # GM-398: size 0.26x (<= 0.50) and RSS 0.48x (<= 0.70); pass time 0.69x misses
HARM_SEED = 4223
POWER_SEED = 4226  # int8_confirm_power.py's default --seed; its shares use seed + 1
POWER_SD = 0.012


def arg(name):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else None


def settings_of(eval_dir):
    toml = (eval_dir / "variants.toml").read_text()  # python 3.9: no tomllib
    return {k: int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))
            for k in ("bootstrap_seed", "bootstrap_resamples")}


def all_held_out(outs):
    """Section 3 step 7: every authored query is held-out."""
    return [dict(o, held_out=not o["mechanical"]) for o in outs]


def chance(run, eval_dir):
    """D7 chance recall@10: pooled mean of min(1, 10|E|/N) over scored positives (embed_eval.rs)."""
    groups = {}
    for d in sorted(p for p in run.iterdir() if (p / "rankings.jsonl").exists()):
        manifest = json.loads((d / "manifest.json").read_text())
        qs = q5s.load_queries(eval_dir, manifest["corpus"])
        n = manifest["nodeCount"]
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            q = qs[r["id"]]
            if q["kind"] == "positive" and not q["mechanical"]:
                groups.setdefault(q["language"], []).append(min(1.0, 10 * len(r["expected"]) / n))
    return q5s.pooled_mean(groups)


def r10_groups(outs):
    g = {}
    for o in outs:
        if o["positive"] and not o["mechanical"]:
            g.setdefault(o["language"], []).append(q5s.hit10(o))
    return g


def broken_arm(boot, reference, broken):
    """metrics.rs `broken_arm_check` on pooled recall@10 bounds."""
    r, b = boot.bound(r10_groups(reference)), boot.bound(r10_groups(broken))
    gap = r[0] - b[0]
    ratio = b[0] / r[0] if r[0] > 0 else math.inf
    sep = r[1] > b[2]
    return {"gap": 100 * gap, "ratio": ratio, "separate": sep, "ref": r, "broken": b,
            "passes": gap >= 0.20 - EPS and ratio <= 0.25 + EPS and sep}


def gates(boot, ref, rf, cand, cf):
    """D9 Q1-Q3 and rule-B Q4/Q5 of `cand` against `ref`, each at the given floors."""
    q1 = boot.bound(q5s.paired_value(ref, cand, q5s.hit10))
    q2 = boot.bound(q5s.paired_value(ref, cand, q5s.rr))
    q3 = {l: sum(v) / len(v) for l, v in q5s.paired_value(ref, cand, q5s.hit10).items()}
    g4 = q5s.paired_ind(ref, rf, cand, cf, q5s.cw_indicator)
    g5 = q5s.paired_ind(ref, rf, cand, cf, q5s.fa_indicator)
    q4, q5 = boot.bound(g4), boot.bound(g5)
    by_lang = {}
    for l in LANGS:
        if g5.get(l):
            v = g5[l]
            by_lang[l] = (boot.bound({l: v}), len(v), sum(x > 0 for x in v), sum(x < 0 for x in v))
    p = {
        "Q1": q1 is not None and q1[0] >= -0.02 - EPS and q1[1] >= -0.05 - EPS,
        "Q2": q2 is not None and q2[0] >= -0.02 - EPS and q2[1] >= -0.05 - EPS,
        "Q3": bool(q3) and all(d >= -0.10 - EPS for d in q3.values()),
        "Q4": q4 is not None and q4[2] <= MARGIN + EPS,
        "Q5": q5 is not None and q5[2] <= MARGIN + EPS,
    }
    return {"Q1": q1, "Q2": q2, "Q3": q3, "Q4": q4, "Q5": q5, "Q5_by_language": by_lang,
            "n4": sum(len(v) for v in g4.values()), "n5": sum(len(v) for v in g5.values()),
            "passes": p, "quality_pass": all(p.values())}


def show(label, g):
    p = g["passes"]
    ok = lambda k: "ok" if p[k] else "FAIL"
    print(f"{label:14s} Q1 {q5s.pb(g['Q1'])} {ok('Q1')} | Q2 {g['Q2'][0]:+.3f} [{g['Q2'][1]:+.3f}] {ok('Q2')} | "
          f"Q3 min {q5s.pts(min(g['Q3'].values()))} {ok('Q3')} | Q4 {q5s.pb(g['Q4'])} n={g['n4']} {ok('Q4')} | "
          f"Q5 {q5s.pb(g['Q5'])} n={g['n5']} {ok('Q5')}")


def harm_shares(ref_old, cand_old):
    """int8_confirm_power.py's harm5 shares (a +5 true delta), computed exactly as its main does."""
    P = icp.pools(ref_old, cand_old)
    Fr = np.array(icp.FROZEN[REF])
    crng = np.random.default_rng(POWER_SEED + 1)
    big = {l: 200000 for l in LANGS}
    e = icp.study(crng, P, big, {l: 50000 for l in LANGS}, "equal", POWER_SD, None, None, Fr, Fr)
    h5, h4 = {}, {}
    for l in LANGS:
        p5 = e["q5rows"][l]
        h5[l] = MARGIN / (1 - icp.fa(p5[:, icp.TOP_C], p5[:, icp.TL_C], Fr).mean())
        pp, aa, _, _ = e["q4rows"][l]
        rate = np.concatenate([icp.cw(pp[:, icp.TOP_C], pp[:, icp.TL_C], pp[:, icp.RK_C], True, Fr),
                               icp.cw(aa[:, icp.TOP_C], aa[:, icp.TL_C], None, False, Fr)]).mean()
        h4[l] = MARGIN / (1 - rate)
    return {l: float(v) for l, v in h5.items()}, {l: float(v) for l, v in h4.items()}


def plant_harm(outs, h5, h4, factor, seed):
    """A derived arm: int8's real outcomes with harm planted (section 5, control 4).

    Q4: each held-out row with a known top language becomes a clearing wrong
    answer with probability factor*h4 (top score 1.0, a right-first positive
    demoted to rank 2, which keeps recall@10). Q5: each remaining right-first
    held-out positive is pushed below every floor (top -1.0) with probability
    factor*h5. One draw per row per harm, in file order."""
    rng = np.random.default_rng(seed)
    out = []
    for o in outs:
        c = dict(o)
        if c["held_out"] and not c["mechanical"] and c["top"] is not None and c["top_language"] in LANGS:
            u4, u5 = rng.random(), rng.random()
            lang = c["language"]
            if u4 < factor * h4[lang]:
                c["top"] = max(c["top"], 1.0)
                if c["positive"] and c["rank"] == 1:
                    c["rank"] = 2
            elif c["positive"] and c["rank"] == 1 and u5 < factor * h5[lang]:
                c["top"] = -1.0
        out.append(c)
    return out


def vectors_sha(run):
    return {d.name: hashlib.sha256((d / "vectors.bin").read_bytes()).hexdigest()
            for d in sorted(run.iterdir()) if (d / "vectors.bin").exists()}


def control_old(boot, old_runs, old_eval, settings):
    """Control A: the frozen floors and GM-398's int8 gates reproduce from the stored runs."""
    bad = []
    ref = q5s.load_arm(old_runs / REF, old_eval)
    cand = q5s.load_arm(old_runs / CAND, old_eval)
    for name, arm in ((REF, ref), (CAND, cand)):
        fl = q5s.fit_floors_step(arm, 0.01)
        if fl != FROZEN[name]:
            bad.append(f"{name} refitted floors {fl} != frozen {FROZEN[name]}")
    g = gates(boot, ref, FROZEN[REF], cand, FROZEN[CAND])
    r1 = lambda x: round(100 * x, 1) + 0.0
    got = {"Q1": (r1(g["Q1"][0]), r1(g["Q1"][1])), "Q2": (round(g["Q2"][0], 3), round(g["Q2"][1], 3)),
           "Q4": (r1(g["Q4"][0]), r1(g["Q4"][2])), "Q5": tuple(r1(x) for x in g["Q5"])}
    want = {"Q1": (0.5, -1.0), "Q2": (-0.003, -0.012), "Q4": (-1.4, 1.0), "Q5": (2.1, -1.4, 6.2)}
    for k in want:
        if tuple(x + 0.0 for x in got[k]) != want[k]:
            bad.append(f"int8 {k} {got[k]} != GM-398 {want[k]}")
    arms = {a: q5s.load_arm(old_runs / a, old_eval) for a in (RANDOM, SHUFFLED)}
    d7 = {a: broken_arm(boot, ref, arms[a]) for a in arms}
    got7 = {a: (round(d7[a]["gap"], 1), round(d7[a]["ratio"], 3), d7[a]["separate"]) for a in d7}
    want7 = {RANDOM: (62.8, 0.008, True), SHUFFLED: (61.8, 0.024, True)}
    if got7 != want7:
        bad.append(f"D7 {got7} != GM-398 {want7}")
    obs = q5s.pooled_mean(r10_groups(arms[RANDOM]))
    ch = chance(old_runs / RANDOM, old_eval)
    if (round(obs, 3), round(ch, 3)) != (0.005, 0.015):
        bad.append(f"random {obs:.3f} vs chance {ch:.3f} != GM-398 0.005 vs 0.015")
    print("== control A: OLD GM-398 runs (parity split), frozen floors ==")
    print(f"refitted floors fp32 {q5s.fit_floors_step(ref, 0.01)} int8 {q5s.fit_floors_step(cand, 0.01)}")
    show("int8 (GM-398)", g)
    print(f"D7 {got7}; random {obs:.3f} vs chance {ch:.3f}")
    return bad, ref, cand


def main():
    old_runs, old_eval = Path(arg("--old-runs")), Path(arg("--old-eval-dir"))
    rehearse = "--rehearse" in sys.argv
    blind = "--controls-only" in sys.argv
    runs = old_runs if rehearse else Path(arg("--runs"))
    eval_dir = old_eval if rehearse else Path(arg("--eval-dir"))
    settings = settings_of(eval_dir)
    if settings != {"bootstrap_seed": 398, "bootstrap_resamples": 10000}:
        sys.exit(f"STOP: bootstrap settings {settings} are not the protocol's (seed 398, 10,000)")
    boot = q5s.Boot(np, settings["bootstrap_resamples"], settings["bootstrap_seed"])
    result = {"mode": "rehearse (old runs as confirm runs; verdict meaningless)" if rehearse else "confirm",
              "floors": FROZEN, "settings": settings}
    bad = []

    # vectorised SplitMix draws equal the scalar port of rng.rs
    rng0, idx0 = q5s.Rng(settings["bootstrap_seed"]), boot.indices((3, 7))
    check = [[rng0.below(n) for n in (3,) * 3 + (7,) * 7] for _ in range(2)]
    if check != [list(idx0[0][r]) + list(idx0[1][r]) for r in range(2)]:
        bad.append("vectorised SplitMix != Rng.below")

    bad_a, ref_old, cand_old = control_old(boot, old_runs, old_eval, settings)
    bad += bad_a

    # --- confirm runs ---------------------------------------------------------
    arms = {a: all_held_out(q5s.load_arm(runs / a, eval_dir)) for a in (REF, CAND, RANDOM, SHUFFLED)}
    empty = [a for a in arms if not arms[a]]
    if empty:
        sys.exit(f"STOP: no rankings under {runs} for {empty}")
    ids = {a: sorted(o["id"] for o in arms[a]) for a in arms}
    if any(ids[a] != ids[REF] for a in arms):
        bad.append("the arms do not hold the same queries")
    if not rehearse:
        # mechanical queries ride along for the harness's secondary refit report; they never enter a gate
        if any(not o["mechanical"] and not re.search(r"-c\d{3}$", o["id"]) for o in arms[REF]):
            bad.append("a confirm run holds an authored query that is not a new -cNNN query")
    n_pos = {l: sum(1 for o in arms[REF] if o["language"] == l and o["positive"] and not o["mechanical"]) for l in LANGS}
    n_abs = {l: sum(1 for o in arms[REF] if o["language"] == l and not o["positive"] and not o["mechanical"]) for l in LANGS}
    print(f"\n== confirm queries ({result['mode']}): positives {n_pos} absent {n_abs} ==")

    # control 2: node vectors unchanged
    print("\n== control 2: vectors.bin sha256, confirm run == GM-398 run ==")
    vec = {}
    for a in (REF, CAND, RANDOM, SHUFFLED):
        new, old = vectors_sha(runs / a), vectors_sha(old_runs / a)
        same = bool(old) and new == old
        vec[a] = same
        if not same:
            diff = sorted(c for c in set(new) | set(old) if new.get(c) != old.get(c))
            bad.append(f"{a} vectors.bin differ from GM-398's in {diff}")
    print(str({a: "same" if s else "DIFFER" for a, s in vec.items()})
          + ("  (trivial in --rehearse: the same files)" if rehearse else ""))

    # control 3: D7 broken arms on the new queries
    print("\n== control 3: D7 broken arms on the confirm queries ==")
    d7 = {}
    for good in (REF, CAND):
        for broken in (RANDOM, SHUFFLED):
            c = broken_arm(boot, arms[good], arms[broken])
            d7[f"{good} vs {broken}"] = c
            if blind and good == CAND:
                print(f"{good:24s} vs {broken:8s} (figures withheld, --controls-only): "
                      f"{'pass' if c['passes'] else 'FAIL'}")
            else:
                print(f"{good:24s} vs {broken:8s} gap {c['gap']:.1f} ratio {c['ratio']:.3f} "
                      f"bounds separate {c['separate']}: {'pass' if c['passes'] else 'FAIL'}")
            if not c["passes"]:
                bad.append(f"{good} is not clearly above {broken}")
    obs = q5s.pooled_mean(r10_groups(arms[RANDOM]))
    ch = chance(runs / RANDOM, eval_dir)
    at_chance = obs <= 3 * ch + 0.02 + 1e-12
    print(f"random recall@10 {obs:.3f} vs chance {ch:.4f}: {'pass' if at_chance else 'FAIL'} (<= 3x chance + 0.02)")
    if not at_chance:
        bad.append("random arm above chance: the harness leaks order")
    rf = FROZEN[REF]
    for broken in (RANDOM, SHUFFLED):
        g = gates(boot, arms[REF], rf, arms[broken], rf)
        show(f"{broken} as cand", g)
        if g["quality_pass"]:
            bad.append(f"the broken arm {broken} passes the gates")
    result["d7"] = {k: {kk: vv for kk, vv in v.items()} for k, v in d7.items()}
    result["random_chance"] = {"observed": obs, "chance": ch, "passes": at_chance}

    # control 5: null arm
    print("\n== control 5: null arm, fp32 vs fp32 at fp32's floors ==")
    null = gates(boot, arms[REF], rf, arms[REF], rf)
    show("fp32 vs fp32", null)
    zero = all(null[k] is not None and all(abs(x) < 1e-15 for x in null[k]) for k in ("Q1", "Q2", "Q4", "Q5"))
    zero = zero and all(abs(v) < 1e-15 for v in null["Q3"].values())
    if not (zero and null["quality_pass"]):
        bad.append("null arm is not exactly 0 and passing")

    # the verdict arm
    if not blind:
        print("\n== int8 vs fp32, frozen floors, rule B ==")
        g8 = gates(boot, arms[REF], rf, arms[CAND], FROZEN[CAND])
        show("int8", g8)
        for l, (b, n, w, bt) in g8["Q5_by_language"].items():
            print(f"   Q5 {l:10s} {q5s.pb(b)} n={n} +{w}/-{bt} (reported, not gated)")

    # control 4: planted harm
    h5, h4 = harm_shares(ref_old, cand_old)
    print(f"\n== control 4: planted harm, seed {HARM_SEED}; harm5 shares h5 "
          f"{ {l: round(v, 3) for l, v in h5.items()} } h4 { {l: round(v, 3) for l, v in h4.items()} } ==")
    if not all(0.05 <= 2 * v <= 0.13 for v in h5.values()) or not all(0.14 <= 2 * v <= 0.37 for v in h4.values()):
        bad.append("harm10 shares outside the protocol's 11-12% (Q5) / 16-36% (Q4)")
    harm = {}
    for factor, name in ((2, "int8-harm10"), (1, "int8-harm5")):
        arm = plant_harm(arms[CAND], h5, h4, factor, HARM_SEED)
        g = gates(boot, arms[REF], rf, arm, FROZEN[CAND])
        harm[name] = {"passes": g["passes"]} if blind else g
        if not blind:
            show(name, g)
        print(f"   {name}: Q4 {'fails' if not g['passes']['Q4'] else 'PASSES'}, "
              f"Q5 {'fails' if not g['passes']['Q5'] else 'PASSES'} under rule B")
    if harm["int8-harm10"]["passes"]["Q4"] or harm["int8-harm10"]["passes"]["Q5"]:
        bad.append("int8-harm10 passes Q4 or Q5: the sample cannot see harm, the study is void")

    if blind:
        result.update({"mode": "controls-only (int8 figures withheld)", "null": null, "harm": harm,
                       "vectors_same": vec, "control_failures": bad})
        result["d7"] = {k: ({"passes": v["passes"]} if k.startswith(CAND) else v) for k, v in result["d7"].items()}
        print("\n== controls ==")
        print("CONTROL FAILED:\n  " + "\n  ".join(bad) if bad else "controls A, 2, 3, 4, 5: OK (no verdict computed)")
        if arg("--json"):
            Path(arg("--json")).write_text(json.dumps(result, indent=1, default=str))
        sys.exit(2 if bad else 0)

    # secondary, not gated: both arms at fp32's floors (S1's shared-floor view)
    shared = gates(boot, arms[REF], rf, arms[CAND], rf)
    print("\n== secondary, not gated: int8 at fp32's floors ==")
    show("int8 @ fp32 fl", shared)

    verdict_pass = g8["quality_pass"] and COST_PASS_INT8
    result.update({"int8": g8, "null": null, "harm": harm, "shared_floors": shared,
                   "harm_shares": {"h5": h5, "h4": h4}, "vectors_same": vec,
                   "control_failures": bad})
    print("\n== verdict ==")
    if bad:
        print("CONTROL FAILED (no verdict; fp32 stays):\n  " + "\n  ".join(bad))
        result["verdict"] = None
    else:
        print("controls A, 2, 3, 4, 5: OK")
        result["verdict"] = "switch to int8" if verdict_pass else "keep fp32"
        print(("REHEARSAL (meaningless): " if rehearse else "") + f"int8 {'PASSES' if verdict_pass else 'FAILS'}"
              f" Q1-Q5 (rule B) and cost -> {result['verdict']}")
    if arg("--json"):
        Path(arg("--json")).write_text(json.dumps(result, indent=1, default=str))
    sys.exit(2 if bad else 0)


if __name__ == "__main__":
    main()
