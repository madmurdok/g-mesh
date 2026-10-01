#!/usr/bin/env python3
"""GM-466: refit calibrated cost curves, build control curves, and validate
`cost predict` ratios against every clean measured pass-time round.

Usage:
  gm466_validate.py curves OUT_DIR CURVE...
      Refits each calibration's per-point medians (relative, absolute,
      linear-only), prints coefficients, residuals and per-point differences
      against the first calibration, and writes curve files for `cost
      predict`, fitted to the per-point median of all calibrations:
      <OUT_DIR>/{rel,abs,linear,flat}.curve.json.
  gm466_validate.py validate OUT_DIR
      Reads <OUT_DIR>/predict-{rel,abs,linear,flat}.json and prints the
      prediction and validation tables.

The fit replicates core/src/cli/embed_eval/cost.rs::fit_quadratic (weighted
least squares, relative weights 1/t^2); `curves` checks that its relative
refit matches the coefficients `calibrate` stored.
"""
import json
import statistics
import sys
from pathlib import Path

import numpy as np

INT8 = "jina-v2-base-code-int8"
FP = "jina-v2-base-code-int8-first-paragraph"
ST = "jina-v2-base-code-int8-structured"
S512 = "jina-v2-base-code-int8-seq512"
S256 = "jina-v2-base-code-int8-seq256"

# Measured embedNodesMs (s), from the result docs; ratios only within a round.
# (round, corpus, arm, baseline arm, measured arm s, measured baseline s)
MEASURED = [
    # GM-423 docs/results/gm-423-sequence-length.md "Embedding time"
    ("GM-423 r1", "g-mesh", S512, INT8, 345.1, 400.1),
    ("GM-423 r1", "g-mesh", S256, INT8, 302.1, 400.1),
    ("GM-423 r1", "g-mesh", FP, INT8, 225.8, 400.1),
    ("GM-423 r2", "g-mesh", S512, INT8, 345.0, 384.0),
    ("GM-423 r2", "g-mesh", S256, INT8, 303.5, 384.0),
    ("GM-423 r2", "g-mesh", FP, INT8, 227.9, 384.0),
    ("GM-423 r1 all", "pooled", S512, INT8, 563.3, 622.5),
    ("GM-423 r1 all", "pooled", S256, INT8, 505.3, 622.5),
    ("GM-423 r1 all", "pooled", FP, INT8, 404.6, 622.5),
    # GM-465 round 1, runs-gm465/timing.tsv
    ("GM-465 r1", "g-mesh", FP, INT8, 363.889, 539.792),
    ("GM-465 r1", "g-mesh", ST, INT8, 386.549, 539.792),
    ("GM-465 r1", "g-mesh", ST, FP, 386.549, 363.889),
    # GM-465 round 2 + GM-455 round 1: one gm455_measure.sh window, valid rows
    # only (runs-gm455/timing.tsv). int8 and fp-ctx-none have no valid run, so
    # the baseline is first-paragraph (same texts as fp-ctx-none, C0).
    ("GM-455/465 window", "g-mesh", ST, FP, 345.961, 335.564),
    ("GM-455/465 window", "g-mesh", "fp-path", FP, 386.115, 335.564),
    ("GM-455/465 window", "g-mesh", "fp-parent", FP, 324.665, 335.564),
    ("GM-455/465 window", "g-mesh", "fp-path-parent", FP, 401.110, 335.564),
]

SHORT = {INT8: "int8 1024", S512: "seq512", S256: "seq256", FP: "first-paragraph", ST: "structured"}


def short(v):
    return SHORT.get(v, v)


def fit(points, mode, degree=2):
    ns = np.array([p[0] for p in points], float)
    ts = np.array([p[1] for p in points], float)
    scale = ns.max()
    u = ns / scale
    basis = np.vstack([u**k for k in range(degree + 1)]).T
    w = 1.0 / ts**2 if mode == "relative" else np.ones_like(ts)
    m = basis.T @ (basis * w[:, None])
    rhs = basis.T @ (w * ts)
    beta = np.linalg.solve(m, rhs)
    coef = [beta[k] / scale**k for k in range(degree + 1)] + [0.0] * (2 - degree)
    return coef


def at(coef, n):
    return coef[0] + coef[1] * n + coef[2] * n * n


def describe_fits(label, pts):
    for mode, deg in (("relative", 2), ("absolute", 2), ("relative", 1)):
        coef = fit(pts, mode, deg)
        res = [(at(coef, n) - t) / t for n, t in pts]
        name = f"{mode}{'' if deg == 2 else ' linear'}"
        print(f"  {label} {name:<16} a={coef[0]:8.4f} b={coef[1]:.6f} c={coef[2]:.4e}  "
              f"t(16)={at(coef,16):6.2f} t(256)={at(coef,256):7.2f} t(1024)={at(coef,1024):8.2f}  "
              f"max|res| {max(abs(r) for r in res)*100:5.1f}%  "
              f"res: {' '.join(f'{r*100:+.1f}' for r in res)}")


def curves(out_dir, *paths):
    """Fits per run, run-to-run differences, and the combined curve (per-point
    median of the runs' medians) that `predict` uses."""
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    runs = [json.loads(Path(p).read_text()) for p in paths]
    names = [Path(p).name.split(".")[0] for p in paths]
    for name, c in zip(names, runs):
        pts = [(p["n"], p["medianMs"]) for p in c["points"]]
        stored = c["fit"]
        mine = fit(pts, "relative")
        print(f"{name}: stored relative fit a={stored['aMs']:.4f} b={stored['bMsPerToken']:.6f} "
              f"c={stored['cMsPerToken2']:.4e}; refit a={mine[0]:.4f} b={mine[1]:.6f} c={mine[2]:.4e}")
        describe_fits(name, pts)
    ns = [p["n"] for p in runs[0]["points"]]
    for c in runs[1:]:
        assert [p["n"] for p in c["points"]] == ns
    print("\nper point: n | medians | (run_k - run_0)/run_0 | pmset speed before->after | load1 before")
    diffs = {k: [] for k in range(1, len(runs))}
    for i, n in enumerate(ns):
        pts = [c["points"][i] for c in runs]
        base = pts[0]["medianMs"]
        rel = []
        for k in range(1, len(runs)):
            d = (pts[k]["medianMs"] - base) / base
            diffs[k].append(abs(d))
            rel.append(f"{d*100:+6.1f}%")
        print(f"  {n:>5} | " + " ".join(f"{p['medianMs']:8.2f}" for p in pts) + " | " + " ".join(rel) + " | "
              + " ".join(f"{p['thermalBefore']['cpuSpeedLimit']}->{p['thermalAfter']['cpuSpeedLimit']}" for p in pts)
              + " | " + " ".join(str(p["thermalBefore"]["load1"]) for p in pts))
    for k, d in diffs.items():
        print(f"  {names[k]} vs {names[0]}: max |diff| {max(d)*100:.1f}%, median |diff| {statistics.median(d)*100:.1f}%")

    combined = dict(runs[0])
    combined["points"] = []
    for i, n in enumerate(ns):
        p = dict(runs[0]["points"][i])
        p["medianMs"] = statistics.median(c["points"][i]["medianMs"] for c in runs)
        p["latenciesMs"] = [x for c in runs for x in c["points"][i]["latenciesMs"]]
        combined["points"].append(p)
    pts = [(p["n"], p["medianMs"]) for p in combined["points"]]
    print("\ncombined (per-point median of " + ", ".join(names) + "):")
    print("  medians: " + " ".join(f"{n}:{t:.2f}" for n, t in pts))
    describe_fits("combined", pts)
    variants = {
        "rel": fit(pts, "relative"),
        "abs": fit(pts, "absolute"),
        "linear": fit(pts, "relative", 1),
        "flat": [1.0, 0.0, 0.0],
    }
    for key, coef in variants.items():
        c = dict(combined)
        c["fit"] = {"mode": "absolute" if key == "abs" else "relative",
                    "aMs": coef[0], "bMsPerToken": coef[1], "cMsPerToken2": coef[2]}
        c["combinedFrom"] = [str(p) for p in paths]
        (out / f"{key}.curve.json").write_text(json.dumps(c, indent=2))
    print(f"wrote {', '.join(f'{k}.curve.json' for k in variants)} to {out}")


def validate(out_dir):
    out = Path(out_dir)
    preds = {k: json.loads((out / f"predict-{k}.json").read_text())["variants"]
             for k in ("rel", "abs", "linear", "flat")}

    def ratio(model, corpus, arm, base):
        v = preds[model]
        return v[arm][corpus]["predictedS"] / v[base][corpus]["predictedS"]

    print("## prediction (relative fit), ratio vs int8 1024")
    print("| arm | g-mesh | pooled | g-mesh predicted s | g-mesh tokens |")
    print("|---|---:|---:|---:|---:|")
    rel = preds["rel"]
    for arm in rel:
        g = rel[arm]["g-mesh"]
        print(f"| {short(arm)} | {g['ratio']:.3f}x | {rel[arm]['pooled']['ratio']:.3f}x | "
              f"{g['predictedS']:.1f} | {g['tokens']} |")

    print("\n## validation")
    print("| round | corpus | ratio | measured | rel fit | abs fit | linear | flat |")
    print("|---|---|---|---:|---:|---:|---:|---:|")
    dev = {k: [] for k in preds}
    dev_g = {k: [] for k in preds}
    for rnd, corpus, arm, base, s_arm, s_base in MEASURED:
        meas = s_arm / s_base
        cells = []
        for k in ("rel", "abs", "linear", "flat"):
            p = ratio(k, corpus, arm, base)
            d = p - meas
            dev[k].append(abs(d))
            if corpus == "g-mesh":
                dev_g[k].append(abs(d))
            cells.append(f"{p:.3f} ({d:+.3f})")
        print(f"| {rnd} | {corpus} | {short(arm)} / {short(base)} | {meas:.3f} | " + " | ".join(cells) + " |")
    for label, dd in (("all pairs", dev), ("g-mesh pairs", dev_g)):
        print(f"\n{label} (n={len(dd['rel'])}): |predicted - measured| max / median")
        for k, v in dd.items():
            print(f"  {k:<7} max {max(v):.3f}  median {statistics.median(v):.3f}")


if __name__ == "__main__":
    if sys.argv[1] == "curves":
        curves(sys.argv[2], *sys.argv[3:])
    elif sys.argv[1] == "validate":
        validate(sys.argv[2])
    else:
        sys.exit(__doc__)
