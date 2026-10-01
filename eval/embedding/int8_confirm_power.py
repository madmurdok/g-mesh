#!/usr/bin/env python3
"""Power of a confirmatory int8-vs-fp32 study on NEW queries (GM-422/S6).

Design support for docs/architecture/embedding-eval-int8-confirm.md. Reads the
stored GM-398 rankings only; embeds nothing and writes no queries.

A simulated study draws, per language, n_pos positive and n_abs absent queries
with replacement from the language's pool of GM-398 authored queries (both
halves), with both arms' outcomes attached. Every drawn query is judged as
held-out at FROZEN floors (GM-398's fitted floors, which the protocol freezes),
and D9's Q1-Q5 are computed as `debug-embed-eval report` computes them:
paired deltas, languages weighted equally, one-sided 95% percentile bootstrap
stratified by language.

Scenarios:
  measured  the int8 records as they are, int8 at its floors, fp32 at its own.
            Its true delta is the pool's own delta (printed).
  equal     candidate = fp32's record; both arms' top scores get independent
            N(0, sd/sqrt 2) jitter; both judged at fp32's floors. True Q4/Q5 = 0.
  harm5     `equal`, plus a share of the candidate's queries made to fail the
            gate so that the true delta is +5 points in every language
            (Q5: right-first positives pushed below every floor; Q4: held-out
            rows turned into clearing wrong answers).

CI options:
  fixed     floors are constants; bootstrap over the new queries only.
  joint     O4 of GM-422/S2 applied to the frozen fit set: each resample also
            refits both arms' floors on a resample of GM-398's fit half (paired
            queries), so the floor-fit noise that the frozen floors carry is in
            the bound. New queries cannot shrink that part.

Rules: A = D9 (point <= 0 and upper <= +5), B = upper <= +5 only.

Control (the run stops on failure): floors re-fitted from the stored rankings
equal GM-398's; the SplitMix `Boot` reproduces int8's Q5 +2.1 [-1.4, +6.2];
the record pools reproduce S1's held-out Q5 pairs (n and discordant counts).

usage: int8_confirm_power.py <runs dir> <eval dir> [--reps N] [--resamples N]
       [--grid 50,100,...] [--sd 0.012] [--seed 4226]
"""
import math
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import q5_floor_sensitivity as q5s  # noqa: E402

import numpy as np  # noqa: E402

LANGS = q5s.LANGS
FROZEN = {q5s.REF: [0.56, 0.58, 0.56, 0.55], q5s.CAND: [0.57, 0.57, 0.55, 0.53]}
MARGIN = 0.05
EPS = q5s.GATE_EPS


def arg(name, default, cast):
    return cast(sys.argv[sys.argv.index(name) + 1]) if name in sys.argv else default


def lang_idx(tl):
    return LANGS.index(tl) if tl in LANGS else -1


def pools(ref, cand):
    """Per language: positive and absent records of the authored queries, both arms."""
    by_id = {o["id"]: o for o in cand}
    out = {l: {"pos": [], "abs": []} for l in LANGS}
    for r in ref:
        if r["mechanical"]:
            continue
        c = by_id[r["id"]]
        rec = []
        for o in (r, c):
            rec += [o["rank"] if o["rank"] is not None else 10**6,
                    o["top"] if o["top"] is not None else np.nan, lang_idx(o["top_language"])]
        rec.append(1.0 if r["held_out"] else 0.0)
        out[r["language"]]["pos" if r["positive"] else "abs"].append(rec)
    return {l: {k: np.array(v, dtype=float) for k, v in d.items()} for l, d in out.items()}


# record columns
RK_R, TOP_R, TL_R, RK_C, TOP_C, TL_C, HELD = range(7)


def floor_of(F, tl):
    """F: (4,) or (B, 4) floors; tl: (m,) language index (-1 = unknown -> NaN floor)."""
    Fp = np.concatenate([F, np.full(F.shape[:-1] + (1,), np.nan)], axis=-1)
    return Fp[..., tl.astype(int)]


def fa(top, tl, F):
    fl = floor_of(F, tl)
    return (top < fl).astype(float)


def cw(top, tl, rank, positive, F):
    clears = top >= floor_of(F, tl)
    wrong = clears & (rank != 1) if positive else clears
    return wrong.astype(float)


def boot_bound(rng, groups, resamples):
    """groups: {lang: values (m,)}, or {lang: (B, m)} with one row of values per resample.
    Returns (point, lower, upper) as metrics.rs `bootstrap` does (percentile, 5% tails)."""
    keys = [k for k in LANGS if k in groups and groups[k].shape[-1] > 0]
    if not keys:
        return None
    point = np.mean([np.mean(groups[k]) for k in keys])
    stats = 0.0
    for k in keys:
        v = groups[k]
        m = v.shape[-1]
        idx = rng.integers(0, m, size=(resamples, m))
        if v.ndim == 1:
            stats = stats + v[idx].mean(axis=1)
        else:
            stats = stats + np.take_along_axis(v, idx, axis=1).mean(axis=1)
    stats = np.sort(stats / len(keys))
    tail = math.floor(0.05 * resamples)
    return (float(point), float(stats[tail]), float(stats[resamples - 1 - tail]))


def study(rng, P, n_pos, n_abs, scenario, sd, h5, h4, Fr, Fc):
    """Draw one study; return per-language arrays for every gate plus top/lang rows for joint CI."""
    g = {"q1": {}, "q2": {}, "q4": {}, "q5": {}, "q5rows": {}, "q4rows": {}}
    for li, l in enumerate(LANGS):
        pos = P[l]["pos"][rng.integers(0, len(P[l]["pos"]), n_pos[l])].copy()
        ab = P[l]["abs"][rng.integers(0, len(P[l]["abs"]), n_abs[l])].copy()
        if scenario != "measured":
            for rows in (pos, ab):
                rows[:, RK_C], rows[:, TL_C] = rows[:, RK_R], rows[:, TL_R]
                base = rows[:, TOP_R].copy()
                rows[:, TOP_R] = base + rng.normal(0, sd / math.sqrt(2), len(rows))
                rows[:, TOP_C] = base + rng.normal(0, sd / math.sqrt(2), len(rows))
        if scenario == "harm5":
            both1 = (pos[:, RK_R] == 1) & (pos[:, RK_C] == 1)
            pos[:, TOP_C] = np.where(both1 & (rng.random(len(pos)) < h5[l]), -1.0, pos[:, TOP_C])
        rr = lambda k: np.where(k <= 100, 1.0 / k, 0.0)
        g["q1"][l] = (pos[:, RK_C] <= 10).astype(float) - (pos[:, RK_R] <= 10).astype(float)
        g["q2"][l] = rr(pos[:, RK_C]) - rr(pos[:, RK_R])
        # Q5: pairs both arms rank first, known top language
        m5 = (pos[:, RK_R] == 1) & (pos[:, RK_C] == 1) & (pos[:, TL_R] >= 0) & (pos[:, TL_C] >= 0)
        p5 = pos[m5]
        g["q5rows"][l] = p5
        g["q5"][l] = fa(p5[:, TOP_C], p5[:, TL_C], Fc) - fa(p5[:, TOP_R], p5[:, TL_R], Fr)
        # Q4: every held-out row with a known top language, positives and absent
        mp = (pos[:, TL_R] >= 0) & (pos[:, TL_C] >= 0) & ~np.isnan(pos[:, TOP_R]) & ~np.isnan(pos[:, TOP_C])
        ma = (ab[:, TL_R] >= 0) & (ab[:, TL_C] >= 0) & ~np.isnan(ab[:, TOP_R]) & ~np.isnan(ab[:, TOP_C])
        pp, aa = pos[mp], ab[ma]
        c_pos = cw(pp[:, TOP_C], pp[:, TL_C], pp[:, RK_C], True, Fc)
        c_abs = cw(aa[:, TOP_C], aa[:, TL_C], None, False, Fc)
        f_pos, f_abs = np.zeros(len(c_pos)), np.zeros(len(c_abs))
        if scenario == "harm5":
            f_pos = (rng.random(len(c_pos)) < h4[l]).astype(float)
            f_abs = (rng.random(len(c_abs)) < h4[l]).astype(float)
            c_pos, c_abs = np.maximum(c_pos, f_pos), np.maximum(c_abs, f_abs)
        r_pos = cw(pp[:, TOP_R], pp[:, TL_R], pp[:, RK_R], True, Fr)
        r_abs = cw(aa[:, TOP_R], aa[:, TL_R], None, False, Fr)
        g["q4"][l] = np.concatenate([c_pos - r_pos, c_abs - r_abs])
        g["q4rows"][l] = (pp, aa, f_pos, f_abs)
    return g


def joint(rng, g, resamples, fit_r, fit_c, paired, kind):
    """Joint CI: per resample, refit both arms' floors on a resample of the frozen fit set."""
    Br = np.empty((resamples, 4))
    Bc = np.empty((resamples, 4))
    for j, l in enumerate(LANGS):
        idx = rng.integers(0, len(fit_r[l]), size=(resamples, len(fit_r[l])))
        Br[:, j] = q5s.resampled_floors(np, rng, fit_r[l], resamples, 0.01, idx)
        Bc[:, j] = q5s.resampled_floors(np, rng, fit_c[l], resamples, 0.01, idx if paired else None)
    D = {}
    for l in LANGS:
        if kind == "q5":
            p5 = g["q5rows"][l]
            forced = p5[:, TOP_C] < 0
            dc = np.maximum(fa(p5[:, TOP_C], p5[:, TL_C], Bc), forced[None, :])
            D[l] = dc - fa(p5[:, TOP_R], p5[:, TL_R], Br)
        else:
            pp, aa, f_pos, f_abs = g["q4rows"][l]
            # rows forced wrong by harm5 stay wrong under any floor
            cpos = np.maximum(cw(pp[:, TOP_C], pp[:, TL_C], pp[:, RK_C], True, Bc), f_pos[None, :])
            cabs = np.maximum(cw(aa[:, TOP_C], aa[:, TL_C], None, False, Bc), f_abs[None, :])
            rpos = cw(pp[:, TOP_R], pp[:, TL_R], pp[:, RK_R], True, Br)
            rabs = cw(aa[:, TOP_R], aa[:, TL_R], None, False, Br)
            D[l] = np.concatenate([cpos - rpos, cabs - rabs], axis=1)
    b = boot_bound(rng, D, resamples)
    return (float(np.mean([g[kind][l].mean() for l in LANGS if len(g[kind][l])])), b[1], b[2])


def passes(rule, b):
    if b is None or any(math.isnan(x) for x in b):
        return False
    if rule == "A":
        return b[0] <= EPS and b[2] <= MARGIN + EPS
    return b[2] <= MARGIN + EPS


def main():
    runs, eval_dir = Path(sys.argv[1]), Path(sys.argv[2])
    reps = arg("--reps", 400, int)
    resamples = arg("--resamples", 2000, int)
    grid = [int(x) for x in arg("--grid", "25,50,100,150,200,300,400", str).split(",")]
    sd = arg("--sd", 0.012, float)
    seed = arg("--seed", 4226, int)

    ref = q5s.load_arm(runs / q5s.REF, eval_dir)
    cand = q5s.load_arm(runs / q5s.CAND, eval_dir)

    # --- control -----------------------------------------------------------
    bad = []
    for name, arm in ((q5s.REF, ref), (q5s.CAND, cand)):
        fl = q5s.fit_floors_step(arm, 0.01)
        if [fl[l] for l in LANGS] != FROZEN[name]:
            bad.append(f"{name} floors {fl} != {FROZEN[name]}")
    Fr, Fc = np.array(FROZEN[q5s.REF]), np.array(FROZEN[q5s.CAND])
    boot = q5s.Boot(np, 10000, 398)
    rfl = dict(zip(LANGS, FROZEN[q5s.REF]))
    cfl = dict(zip(LANGS, FROZEN[q5s.CAND]))
    b5 = boot.bound(q5s.paired_ind(ref, rfl, cand, cfl, q5s.fa_indicator))
    got = tuple(round(100 * x, 1) + 0.0 for x in b5)
    if got != (2.1, -1.4, 6.2):
        bad.append(f"Q5 {got} != (2.1, -1.4, 6.2)")
    print("== control ==")
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("floors fp32", FROZEN[q5s.REF], "int8", FROZEN[q5s.CAND], "| Boot Q5", got, "OK")

    P = pools(ref, cand)

    # --- empirical inputs ------------------------------------------------------
    print("\n== inputs per language, frozen floors (held-out half | all authored) ==")
    print("lang  pos abs | pair-rate(both r1)  Q5 +w/-b of pairs  | Q4 rows  +w/-b | Q1 +w/-b")
    inputs = {}
    for li, l in enumerate(LANGS):
        row = {}
        for label, sel in (("held", 1.0), ("all", None)):
            pos = P[l]["pos"] if sel is None else P[l]["pos"][P[l]["pos"][:, HELD] == sel]
            ab = P[l]["abs"] if sel is None else P[l]["abs"][P[l]["abs"][:, HELD] == sel]
            both1 = (pos[:, RK_R] == 1) & (pos[:, RK_C] == 1)
            p5 = pos[both1]
            d5 = fa(p5[:, TOP_C], p5[:, TL_C], Fc) - fa(p5[:, TOP_R], p5[:, TL_R], Fr)
            d4 = np.concatenate([
                cw(pos[:, TOP_C], pos[:, TL_C], pos[:, RK_C], True, Fc) - cw(pos[:, TOP_R], pos[:, TL_R], pos[:, RK_R], True, Fr),
                cw(ab[:, TOP_C], ab[:, TL_C], None, False, Fc) - cw(ab[:, TOP_R], ab[:, TL_R], None, False, Fr)])
            d1 = (pos[:, RK_C] <= 10).astype(float) - (pos[:, RK_R] <= 10).astype(float)
            fa_r = fa(p5[:, TOP_R], p5[:, TL_R], Fr).mean() if len(p5) else float("nan")
            cw_r = np.concatenate([cw(pos[:, TOP_R], pos[:, TL_R], pos[:, RK_R], True, Fr),
                                   cw(ab[:, TOP_R], ab[:, TL_R], None, False, Fr)]).mean()
            row[label] = dict(npos=len(pos), nabs=len(ab), pair=both1.mean(), n5=len(p5),
                              w5=int((d5 > 0).sum()), b5=int((d5 < 0).sum()), d5=d5.mean() if len(d5) else 0,
                              n4=len(d4), w4=int((d4 > 0).sum()), b4=int((d4 < 0).sum()), d4=d4.mean(),
                              w1=int((d1 > 0).sum()), b1=int((d1 < 0).sum()), fa_r=fa_r, cw_r=cw_r)
            r = row[label]
            print(f"{l[:4]:4s} {label:4s} {r['npos']:3d} {r['nabs']:3d} | {r['pair']:.2f} (n={r['n5']:3d})  "
                  f"+{r['w5']}/-{r['b5']} d5 {100 * r['d5']:+.1f} faR {100 * fa_r:.1f}% | "
                  f"{r['n4']:3d} +{r['w4']}/-{r['b4']} d4 {100 * r['d4']:+.1f} cwR {100 * cw_r:.1f}% | "
                  f"+{r['w1']}/-{r['b1']}")
        inputs[l] = row
    # control: the pools reproduce S1's held-out Q5 pairs (n and discordant counts)
    want = {"go": (16, 2, 0), "python": (20, 0, 0), "rust": (6, 0, 0), "typescript": (25, 0, 1)}
    got = {l: (inputs[l]["held"]["n5"], inputs[l]["held"]["w5"], inputs[l]["held"]["b5"]) for l in LANGS}
    if got != want:
        print(f"CONTROL FAILED: pools' held-out Q5 pairs {got} != {want}")
        sys.exit(2)
    print("pools reproduce S1's held-out Q5 pairs and discordant counts: OK")

    # harm5 shares: forcing a share h of the candidate's rows adds h * (1 - candidate rate) to the
    # delta in expectation, so h = 0.05 / (1 - rate) makes the true delta +5 in every language.
    # The rates are those of `equal` (jittered fp32 at fp32's floors), read off one large draw.
    crng = np.random.default_rng(seed + 1)
    # absent rows are drawn at the study's 4:1 ratio, since their rate differs from the positives'
    big = {l: 200000 for l in LANGS}
    e = study(crng, P, big, {l: 50000 for l in LANGS}, "equal", sd, None, None, Fr, Fr)
    h5, h4 = {}, {}
    for li, l in enumerate(LANGS):
        p5 = e["q5rows"][l]
        h5[l] = MARGIN / (1 - fa(p5[:, TOP_C], p5[:, TL_C], Fr).mean())
        pp, aa, _, _ = e["q4rows"][l]
        rate = np.concatenate([cw(pp[:, TOP_C], pp[:, TL_C], pp[:, RK_C], True, Fr),
                               cw(aa[:, TOP_C], aa[:, TL_C], None, False, Fr)]).mean()
        h4[l] = MARGIN / (1 - rate)
    print("\nharm5 shares h5", {l: round(float(v), 3) for l, v in h5.items()},
          "h4", {l: round(float(v), 3) for l, v in h4.items()})

    # frozen fit sets for the joint CI
    fr = q5s.fit_arrays(np, ref, cand)
    fit_r = {l: fr[l][0] for l in LANGS}
    fit_c_meas = {l: fr[l][1] for l in LANGS}

    # --- control 2: a study equal to the pool reproduces the pool's point -----------
    rng = np.random.default_rng(seed)
    print("\n== simulation: reps", reps, "resamples", resamples, "sd", sd, "seed", seed, "==")
    print("cells: share of reps PASSING (equal, measured: want high; harm5: want low = false pass)")
    hdr = "n_pos/lang n_abs | scen     | Q5 mean  A   B   | Q4 mean  A   B   | Q1  Q2  Q3 | all-A all-B | Q5 joint B  Q4 joint B | mean n5 (go)"
    print(hdr)
    for n in grid:
        n_pos = {l: n for l in LANGS}
        n_abs = {l: max(1, round(n / 4)) for l in LANGS}
        for scen in ("equal", "measured", "harm5"):
            Frs, Fcs = (Fr, Fc) if scen == "measured" else (Fr, Fr)
            cnt = dict(q5A=0, q5B=0, q4A=0, q4B=0, q1=0, q2=0, q3=0, allA=0, allB=0, j5=0, j4=0)
            m5 = m4 = 0.0
            n5 = n5go = 0
            jreps = min(reps, 100)
            for rep in range(reps):
                g = study(rng, P, n_pos, n_abs, scen, sd, h5, h4, Frs, Fcs)
                b5 = boot_bound(rng, g["q5"], resamples)
                b4 = boot_bound(rng, g["q4"], resamples)
                b1 = boot_bound(rng, g["q1"], resamples)
                b2 = boot_bound(rng, g["q2"], resamples)
                m5 += b5[0] if b5 else 0
                m4 += b4[0]
                n5 += sum(len(v) for v in g["q5"].values())
                n5go += len(g["q5"]["go"])
                p1 = b1[0] >= -0.02 - EPS and b1[1] >= -0.05 - EPS
                p2 = b2[0] >= -0.02 - EPS and b2[1] >= -0.05 - EPS
                p3 = all(g["q1"][l].mean() >= -0.10 - EPS for l in LANGS)
                a5, bb5, a4, bb4 = passes("A", b5), passes("B", b5), passes("A", b4), passes("B", b4)
                cnt["q5A"] += a5
                cnt["q5B"] += bb5
                cnt["q4A"] += a4
                cnt["q4B"] += bb4
                cnt["q1"] += p1
                cnt["q2"] += p2
                cnt["q3"] += p3
                cnt["allA"] += p1 and p2 and p3 and a4 and a5
                cnt["allB"] += p1 and p2 and p3 and bb4 and bb5
                if rep < jreps:
                    if scen == "measured":
                        fc = fit_c_meas
                    else:
                        fc = {l: fit_r[l] + rng.normal(0, sd, len(fit_r[l])) for l in LANGS}
                    j5 = joint(rng, g, resamples, fit_r, fc, True, "q5")
                    j4 = joint(rng, g, resamples, fit_r, fc, True, "q4")
                    cnt["j5"] += passes("B", j5)
                    cnt["j4"] += passes("B", j4)
            pc = lambda k, d=reps: f"{100 * cnt[k] / d:3.0f}"
            print(f"{n:5d} {n_abs['go']:5d}      | {scen:8s} | {100 * m5 / reps:+5.1f} {pc('q5A')} {pc('q5B')} | "
                  f"{100 * m4 / reps:+5.1f} {pc('q4A')} {pc('q4B')} | {pc('q1')} {pc('q2')} {pc('q3')} | "
                  f"{pc('allA')}   {pc('allB')}   | {pc('j5', jreps)}        {pc('j4', jreps)}        | "
                  f"{n5 / reps:.0f} ({n5go / reps:.0f})", flush=True)


if __name__ == "__main__":
    main()
