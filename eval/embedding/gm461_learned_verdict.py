"""GM-461: does a small classifier over several signals catch more wrong/absent tops than the shipped floor
at the same share of right tops kept? (design: docs/design/GM-461-learned-no-match-verdict.md)

Setup = GM-701 (gm701_floor_or_blend.setup): v2 eval, int8-structured top-30, ce-minilm quint8 logits from
the Rust cache, blend = logit + 40 * cosine, exact-name skip rule. One row per query with a non-empty window.
Label "right" = the reranked top-1 (the row shown) is an expected row; absent queries are never right.
Populations: NL (prose, soft verdict) and NAME (mechanical, hard verdict), one model each. Side: sha256(id)[0]
odd = held-out (rerank_eval.held_out for NL, gm701.name_held_out for names). Everything fitted (standardisation,
L2 strength by 5-fold CV, coefficients, thresholds) comes from FIT only.

Arms (page / verdict):
  int8   cosine order / cos1 vs shipped floor of the cosine top's language
  F4     blend order  / cos1 vs shipped floor (= GM-701 arm A, the baseline)
  C0     blend order  / logistic on cos1 + cosine-top language; thresholds z(shipped floor, language): == F4
  C0-m   blend order  / the C0 model at one matched threshold (informative)
  L-m    blend order  / full model, threshold keeps as many FIT right tops as F4 keeps on FIT
  L-d6   blend order  / full model, D6 rule (3% of FIT right tops below) on z
  L-noCE cosine order / model without CE inputs, label = cosine top right, matched to int8 (informative)

Controls: 1 F4/int8 reproduce GM-701/GM-697; 2 C0 exact (a: verdict, b: D6 rule); 3 shuffled FIT labels;
4 fitter (gradient here; scikit-learn via gm461_sklearn_check.py on --dump-fit); 5 differ count.
Stop rules (design): control 1-2 mismatch or a non-positive net cosine (cos1 + cos_t) or logit coefficient of a
deciding model (full, C0) -> no tables, exit 2; an informative model (L-noCE) failing it is reported as stopped
by rule with its coefficients and no metrics
(--ignore-stops builds the tables anyway for diagnosis and still exits 2).

  python3 gm461_learned_verdict.py --work <v2 work> --out <scratch>/gm461.json [--md <path>]
      [--resamples 10000] [--shuffles 200] [--coef-boot 1000] [--dump-fit <npz>]
"""
import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm699_separability as S  # noqa: E402
import gm701_floor_or_blend as G  # noqa: E402
import rerank_q5_diag as Q5  # noqa: E402
import shipped_floor_rates as SFR  # noqa: E402

B, R = S.B, S.R
LANGS = R.LANGS
SEED, SHUFFLE_SEED = 461, 462
LAMBDAS = (0.01, 0.1, 1.0, 10.0)
FOLDS = 5
MAX_MISS = 0.03  # rerank_eval.MAX_FALSE_ALARM / GM-697 D2 name budget
STRICT_DROP = ("req-c025", "rg-042")  # gm-460-enlarged-eval.md: held-out NL sharing an expected symbol with FIT

# Control 1 constants (copied from the results docs named; a mismatch stops the run).
GM701_A = {  # gm-701-floor-or-blend.md arm A, held-out: (kept, right), (caught, wrong-or-absent)
    "NL": {"kept_k": [186, 247], "caught_k": [265, 598]},
    "NAME": {"kept_k": [329, 339], "caught_k": [330, 499]},
}
GM697_INT8_NL_HO = {"misled": (40, 184), "cw_pos": (338, 681), "cw_abs": (37, 164)}  # gm-697-floor-refit.md C2
V2_D6_INT8 = {"go": 0.51, "python": 0.47, "rust": 0.57, "typescript": 0.46}  # gm-460-enlarged-eval.md:282

ONEHOT = [l for l in LANGS if l != "go"]  # go = reference level
FULL = ["cos1", "cos_t", "logit_t", "gap_cos", "gap_logit", "has_doc"] + [f"lang:{l}" for l in ONEHOT]
C0_COLS = ["cos1"] + [f"clang:{l}" for l in ONEHOT]
NOCE = ["cos1", "gap_cos", "doc0"] + [f"clang:{l}" for l in ONEHOT]
# Terms whose coefficient sum must be positive: the net cosine effect (cos1 and cos_t are collinear, so
# only their sum is checked) and the CE logit.
POSITIVE_COEF = {"full": (("cos1", "cos_t"), ("logit_t",)), "C0": (("cos1",),), "noCE": (("cos1",),)}


# Informative (non-deciding) models: a stop rule failure stops only that arm; it is reported with its
# coefficients and no metrics. A deciding model (full, C0) failing stops the whole run.
INFORMATIVE = {"noCE"}


def sign_stops(pop, kind, coef):
    """Stop reasons for a non-positive net cosine or logit coefficient (standardised)."""
    out = []
    for terms in POSITIVE_COEF[kind]:
        v = sum(coef[t] for t in terms)
        if v <= 0:
            out.append(f"{pop} {kind}: coefficient of {' + '.join(terms)} = {v:.4g} <= 0")
    return out


def cols_for(kind, pop):
    base = {"full": FULL, "C0": C0_COLS, "noCE": NOCE}[kind]
    return base + (["exact"] if pop == "NAME" and kind in ("full", "noCE") else [])


# ---- rows --------------------------------------------------------------------------------------
def features(cs, sc, flags, floors):
    """One row per query with a non-empty window: f1-f10, labels, population, side, languages."""
    rows, disagree = [], []
    for c in cs:
        for q in c.qids:
            qq = c.queries[q]
            lst = c.lists[B.ST][q]
            n = min(S.K, len(lst))
            if n == 0:
                continue
            mech = bool(qq["mechanical"])
            prose = SFR.rust_is_prose(qq["text"])
            if prose == mech:
                disagree.append(q)
            side = G.name_held_out(q)
            if not mech and side != bool(qq["held_out"]):
                sys.exit(f"STOP: {q}: harness held_out {qq['held_out']} != sha256 side {side}")
            cos = np.array([x for _, x in lst[:n]], dtype=np.float64)
            logit = np.array([sc(c, q, nid) for nid, _ in lst[:n]], dtype=np.float64)
            blend = logit + S.BETA * cos
            skip = bool(flags[(c.name, q)])
            s_ = -np.arange(n, dtype=np.float64) if skip else blend
            order = np.lexsort((np.arange(n), -s_))  # rerank order, ties by cosine position (R.reranked)
            t = int(order[0])
            exp = c.expected[q]
            clang = c.node_lang.get(lst[0][0])
            tlang = c.node_lang.get(lst[t][0])
            f = floors.get(clang)
            r = dict(key=(c.name, q), id=q, corpus=c.name, pop="NL" if prose else "NAME", mechanical=mech,
                     ho=side, qlang=qq["language"], pos=bool(qq["positive"]),
                     right=lst[t][0] in exp, cos_right=lst[0][0] in exp, clang=clang, tlang=tlang,
                     floor_known=f is not None, floor=bool(f is not None and lst[0][1] < f), floor_value=f,
                     cos1=float(lst[0][1]), cos_t=float(cos[t]), logit_t=float(logit[t]),
                     gap_cos=float(lst[0][1] - lst[1][1]) if n > 1 else 0.0,
                     gap_logit=float(logit[t] - logit[int(order[1])]) if n > 1 else 0.0,
                     has_doc=float(bool(c.trimmed_doc.get(lst[t][0], False))),
                     doc0=float(bool(c.trimmed_doc.get(lst[0][0], False))), exact=float(skip))
            for l in ONEHOT:
                r[f"lang:{l}"] = float(tlang == l)
                r[f"clang:{l}"] = float(clang == l)
            rows.append(r)
    return rows, disagree


def matrix(rows, cols):
    return np.array([[r[k] for k in cols] for r in rows], dtype=np.float64).reshape(len(rows), len(cols))


def arm_outs(cs, sc, flags, pop_of):
    """int8 outs (cosine order) and F4 outs (blend order, skip rule), as rerank_eval.outcome dicts.
    `mechanical`/`held_out` follow the population split (rust_is_prose) and the sha256 side."""
    int8 = [R.outcome(c, q, c.lists[B.ST][q], c.lists[B.ST][q][0][1] if c.lists[B.ST][q] else None)
            for c in cs for q in c.qids]
    f4 = B.f4_skip(cs, S.K, S.BETA, sc, flags)
    for outs in (int8, f4):
        for o in outs:
            pop = pop_of.get((o["corpus"], o["id"]))
            if pop is not None and (pop == "NAME") != bool(o["mechanical"]):
                o["mechanical"] = pop == "NAME"
                o["held_out"] = (not o["mechanical"]) and G.name_held_out(o["id"])
    return int8, f4


# ---- logistic regression (numpy, IRLS with L2; intercept unpenalised) ------------------------
def sigmoid(z):
    return 0.5 * (1.0 + np.tanh(0.5 * np.asarray(z, dtype=np.float64)))


def log_loss(z, y):
    """Mean log-loss of log-odds z for labels y."""
    z, y = np.asarray(z, dtype=np.float64), np.asarray(y, dtype=np.float64)
    return float(np.mean(np.logaddexp(0.0, z) - y * z)) if len(z) else float("nan")


def standardise(X_fit):
    """FIT means and SDs (an all-constant column keeps SD 1)."""
    mu = X_fit.mean(axis=0)
    sd = X_fit.std(axis=0)
    sd = np.where(sd > 0, sd, 1.0)
    return mu, sd


def gradient(A, y, w, lam):
    """Gradient of sum log-loss + lam/2 * ||w[1:]||^2; A carries the intercept column first."""
    pen = np.full(len(w), lam)
    pen[0] = 0.0
    return A.T @ (sigmoid(A @ w) - y) + pen * w


def fit_logreg(X, y, lam, iters=100, tol=1e-12):
    """Newton/IRLS on standardised X (no intercept column). Returns w with w[0] the intercept."""
    n, p = X.shape
    A = np.hstack([np.ones((n, 1)), X])
    y = np.asarray(y, dtype=np.float64)
    w = np.zeros(p + 1)
    pen = np.full(p + 1, lam)
    pen[0] = 0.0
    for _ in range(iters):
        m = sigmoid(A @ w)
        g = A.T @ (m - y) + pen * w
        H = A.T @ (A * (m * (1.0 - m))[:, None]) + np.diag(pen)
        step = np.linalg.solve(H, g)
        w = w - step
        if np.max(np.abs(step)) < tol:
            break
    return w


class Model:
    def __init__(self, cols, X_fit, y_fit, lam):
        self.cols, self.lam = list(cols), lam
        self.mu, self.sd = standardise(X_fit)
        self.Z_fit = (X_fit - self.mu) / self.sd
        self.w = fit_logreg(self.Z_fit, y_fit, lam)
        A = np.hstack([np.ones((len(X_fit), 1)), self.Z_fit])
        self.grad_norm = float(np.linalg.norm(gradient(A, np.asarray(y_fit, dtype=np.float64), self.w, lam)))

    def z(self, X):
        return ((X - self.mu) / self.sd) @ self.w[1:] + self.w[0]

    def coef(self):
        return dict(zip(["intercept"] + self.cols, map(float, self.w)))


def folds_of(y, k, seed):
    """Stratified fold index per row: each class shuffled, then dealt round-robin."""
    rng = np.random.default_rng(seed)
    f = np.empty(len(y), dtype=int)
    for cls in (False, True):
        idx = np.where(np.asarray(y, dtype=bool) == cls)[0]
        f[rng.permutation(idx)] = np.arange(len(idx)) % k
    return f


def cv_lambda(X, y, grid=LAMBDAS, seed=SEED, k=FOLDS):
    """Lambda with the lowest mean held-in-fold log-loss (standardised inside each fold); ties -> larger."""
    f = folds_of(y, k, seed)
    curve = {}
    for lam in grid:
        losses = []
        for i in range(k):
            tr, va = f != i, f == i
            m = Model([], X[tr], y[tr], lam)
            losses.append(log_loss(m.z(X[va]), y[va]))
        curve[lam] = float(np.mean(losses))
    best = min(grid, key=lambda l: (curve[l], -l))
    return best, curve


# ---- thresholds on z (FIT only) ---------------------------------------------------------------
def matched_threshold(z_right, kept_target):
    """Largest t with #(z_right >= t) >= kept_target (refuse when z < t)."""
    s = np.sort(np.asarray(z_right, dtype=np.float64))[::-1]
    if kept_target <= 0:
        return float("inf")
    if kept_target > len(s):
        return float("-inf")
    return float(s[kept_target - 1])


def d6_threshold(z_right, max_rate=MAX_MISS):
    """rerank_eval.fit_floors' rule on z, without the 2-dp rounding."""
    s = np.sort(np.asarray(z_right, dtype=np.float64))
    if len(s) == 0:
        return float("-inf")
    k = math.floor(max_rate * len(s) + 1e-9)
    return float(s[min(k, len(s) - 1)])


def as_verdict(outs, z, floors):
    """Copy of `outs` whose verdict is the classifier's: top = z, top_language = the classifier key.
    z: (corpus, id) -> (key, value); a query without z gets no verdict (top None), as an empty window does.
    Returns (outs, floors) for R.clears/R.gm434/R.gates."""
    res = []
    for o in outs:
        o2 = dict(o)
        k = z.get((o["corpus"], o["id"]))
        o2["top_language"], o2["top"] = (k[0], float(k[1])) if k is not None else (None, None)
        res.append(o2)
    return res, dict(floors)


# ---- scoring helpers ---------------------------------------------------------------------------
def point(right, refused):
    right, refused = np.asarray(right, bool), np.asarray(refused, bool)
    kr, kw = int((~refused & right).sum()), int((refused & ~right).sum())
    nr, nw = int(right.sum()), int((~right).sum())
    return dict(kept=kr / nr if nr else float("nan"), caught=kw / nw if nw else float("nan"),
                kept_k=[kr, nr], caught_k=[kw, nw])


def boot_vs(right, fg, ref, resamples):
    """gm701.boot with seed 461 and `ref` as the reference arm."""
    fg = dict(fg)
    fg = {("A" if k == ref else k): v for k, v in fg.items()}
    saved = G.SEED
    G.SEED = SEED
    try:
        return G.boot({"right": np.asarray(right, bool)}, fg, resamples)
    finally:
        G.SEED = saved


def wilson_hi(k, n, z=1.96):
    """gm468_name_floors.wilson_hi."""
    if n == 0:
        return float("nan")
    ph = k / n
    return (ph + z * z / (2 * n) + z * math.sqrt(ph * (1 - ph) / n + z * z / (4 * n * n))) / (1 + z * z / n)


def nl_ho_all(o):
    return R.nl_ho_all(o)


def name_ho_all(o):
    return bool(o["mechanical"]) and G.name_held_out(o["id"])


def name_ho_pos(o):
    return name_ho_all(o) and o["positive"]


def gm434_cols(outs, floors):
    nl, nm = R.gm434(outs, floors, nl_ho_all), R.gm434(outs, floors, name_ho_all)
    return {"nl_" + k: v for k, v in nl.items()} | {"name_" + k: v for k, v in nm.items()}


def per_lang(outs, floors, keep):
    return {l: R.gm434(outs, floors, keep, l) for l in LANGS}


def quality(outs):
    nl = {"r10": R.bootstrap(R.pooled(outs, R.hit10, R.ho)[1]), "mrr": R.bootstrap(R.pooled(outs, R.rr, R.ho)[1])}
    nm = {"r10": R.bootstrap(R.pooled(outs, R.hit10, name_ho_pos)[1]),
          "mrr": R.bootstrap(R.pooled(outs, R.rr, name_ho_pos)[1])}
    return {"nl": nl, "name": nm}


def strip_floors(g):
    return {k: v for k, v in g.items() if k != "floors"}


# ---- per-population fit ----------------------------------------------------------------------
def fit_population(rows, pop, resamples, shuffles, coef_boot, stops):
    fit_rows = [r for r in rows if r["pop"] == pop and not r["ho"]]
    ho_rows = [r for r in rows if r["pop"] == pop and r["ho"]]
    yf = np.array([r["right"] for r in fit_rows], dtype=bool)
    yh = np.array([r["right"] for r in ho_rows], dtype=bool)
    cyf = np.array([r["cos_right"] for r in fit_rows], dtype=bool)
    cyh = np.array([r["cos_right"] for r in ho_rows], dtype=bool)
    f4f = np.array([r["floor"] for r in fit_rows], dtype=bool)
    f4h = np.array([r["floor"] for r in ho_rows], dtype=bool)
    out = dict(n_fit=len(fit_rows), n_ho=len(ho_rows), right_fit=int(yf.sum()), right_ho=int(yh.sum()),
               stopped_arms={})

    models, zs = {}, {}
    for kind, lab_f in (("full", yf), ("C0", yf), ("noCE", cyf)):
        cols = cols_for(kind, pop)
        Xf, Xh = matrix(fit_rows, cols), matrix(ho_rows, cols)
        lam, curve = cv_lambda(Xf, lab_f.astype(np.float64))
        m = Model(cols, Xf, lab_f.astype(np.float64), lam)
        models[kind] = m
        zs[kind] = (m.z(Xf), m.z(Xh))
        out[f"model_{kind}"] = dict(cols=cols, lam=lam, cv_logloss={str(k): v for k, v in curve.items()},
                                    coef_std=m.coef(), grad_norm=m.grad_norm, mu=m.mu.tolist(), sd=m.sd.tolist())
        if m.grad_norm >= 1e-8:
            stops.append(f"{pop} {kind}: gradient norm {m.grad_norm:.3g} >= 1e-8")
        reasons = sign_stops(pop, kind, m.coef())
        if reasons and kind in INFORMATIVE:
            out["stopped_arms"][kind] = reasons
        else:
            stops.extend(reasons)
    noce = "noCE" not in out["stopped_arms"]

    # thresholds (FIT only)
    kept_f4 = int((yf & ~f4f).sum())
    kept_int8 = int((cyf & ~f4f).sum())
    zf, zh = zs["full"]
    th = {"L-m": matched_threshold(zf[yf], kept_f4), "L-d6": d6_threshold(zf[yf]),
          "C0-m": matched_threshold(zs["C0"][0][yf], kept_f4)}
    if noce:
        th["L-noCE"] = matched_threshold(zs["noCE"][0][cyf], kept_int8)
    out["thresholds"] = th
    out["kept_target_fit"] = {"F4": kept_f4, "int8": kept_int8}

    # C0 shifted to the shipped floor per language: z(x) - z(floor of x's cosine-top language)
    c0 = models["C0"]
    shift = {}
    for side_rows in (fit_rows, ho_rows):
        X = matrix(side_rows, c0.cols)
        Xfl = X.copy()
        Xfl[:, 0] = [r["floor_value"] if r["floor_known"] else np.nan for r in side_rows]
        d = c0.z(X) - c0.z(Xfl)
        for r, v in zip(side_rows, d):
            if r["floor_known"]:
                shift[r["key"]] = float(v)
    c0a_bad = [r["id"] for r in fit_rows + ho_rows if r["floor_known"] and (shift[r["key"]] >= 0) != (r["cos1"] >= r["floor_value"])]
    out["control2a_rows_checked"] = len(shift)
    out["control2a_mismatch"] = c0a_bad[:20]
    out["control2a_mismatch_n"] = len(c0a_bad)
    if c0a_bad:
        stops.append(f"{pop} control 2a: {len(c0a_bad)} queries where C0 at z(shipped floor) != shipped verdict")

    # row-level refusals per arm
    ref_f = {"F4": f4f, "C0": np.array([not (shift.get(r["key"], 0.0) >= 0) if r["floor_known"] else False
                                        for r in fit_rows], dtype=bool),
             "C0-m": zs["C0"][0] < th["C0-m"], "L-m": zf < th["L-m"], "L-d6": zf < th["L-d6"]}
    ref_h = {"F4": f4h, "C0": np.array([not (shift.get(r["key"], 0.0) >= 0) if r["floor_known"] else False
                                        for r in ho_rows], dtype=bool),
             "C0-m": zs["C0"][1] < th["C0-m"], "L-m": zh < th["L-m"], "L-d6": zh < th["L-d6"]}
    out["fit"] = {k: point(yf, v) for k, v in ref_f.items()}
    out["ho"] = {k: point(yh, v) for k, v in ref_h.items()}
    out["fit"]["int8"], out["ho"]["int8"] = point(cyf, f4f), point(cyh, f4h)
    out["boot_vs_F4"] = boot_vs(yh, ref_h, "F4", resamples)
    if noce:
        out["fit"]["L-noCE"] = point(cyf, zs["noCE"][0] < th["L-noCE"])
        out["ho"]["L-noCE"] = point(cyh, zs["noCE"][1] < th["L-noCE"])
        out["boot_noCE_vs_int8"] = boot_vs(cyh, {"int8": f4h, "L-noCE": zs["noCE"][1] < th["L-noCE"]}, "int8",
                                           resamples)
    out["differ_vs_F4"] = {k: dict(n=int((v != f4h).sum()), right=int(((v != f4h) & yh).sum()))
                           for k, v in ref_h.items() if k != "F4"}

    # T9 overfitting: AUROC, log-loss, kept/caught FIT vs held-out
    ov = {}
    for kind, (yf_, yh_) in (("full", (yf, yh)), ("C0", (yf, yh)), ("noCE", (cyf, cyh))):
        if kind in out["stopped_arms"]:
            continue
        a_f, a_h = S.auroc(zs[kind][0], yf_), S.auroc(zs[kind][1], yh_)
        ov[kind] = dict(auroc_fit=a_f, auroc_ho=a_h, auroc_gap=a_f - a_h,
                        logloss_fit=log_loss(zs[kind][0], yf_), logloss_ho=log_loss(zs[kind][1], yh_))
    for arm in ("L-m", "L-d6", "C0-m", "L-noCE"):
        if arm not in th:
            continue
        ov[arm] = dict(kept_fit=out["fit"][arm]["kept"], kept_ho=out["ho"][arm]["kept"],
                       caught_fit=out["fit"][arm]["caught"], caught_ho=out["ho"][arm]["caught"])
    ov["overfit_full"] = bool(ov["full"]["auroc_gap"] > 0.05 or ov["L-m"]["kept_ho"] < ov["L-m"]["kept_fit"] - 0.05)
    out["overfit"] = ov

    # T10 coefficient bootstrap over FIT queries (fixed FIT standardisation and lambda)
    rng = np.random.default_rng(SEED)
    m = models["full"]
    W = []
    for _ in range(coef_boot):
        i = rng.integers(0, len(yf), len(yf))
        W.append(fit_logreg(m.Z_fit[i], yf[i].astype(np.float64), m.lam))
    W = np.array(W).reshape(coef_boot, len(m.cols) + 1)
    out["coef_boot"] = {c: [float(np.percentile(W[:, j], 2.5)), float(np.percentile(W[:, j], 97.5))]
                        for j, c in enumerate(["intercept"] + m.cols)} if coef_boot else {}

    # control 3: FIT labels permuted within query language, model + matched threshold refitted
    rng = np.random.default_rng(SHUFFLE_SEED)
    lang_f = np.array([r["qlang"] for r in fit_rows])
    Xh_full = matrix(ho_rows, m.cols)
    base_h = point(yh, f4h)
    deltas, kdeltas = [], []
    for _ in range(shuffles):
        ys = yf.copy()
        for l in set(lang_f):
            idx = np.where(lang_f == l)[0]
            ys[idx] = ys[rng.permutation(idx)]
        ms = Model(m.cols, matrix(fit_rows, m.cols), ys.astype(np.float64), m.lam)
        t = matched_threshold(ms.z(matrix(fit_rows, m.cols))[ys], int((ys & ~f4f).sum()))
        p = point(yh, ms.z(Xh_full) < t)
        deltas.append(p["caught"] - base_h["caught"])
        kdeltas.append(p["kept"] - base_h["kept"])
    obs = out["ho"]["L-m"]["caught"] - base_h["caught"]
    out["control_shuffle"] = dict(
        shuffles=shuffles, observed_caught_delta=obs,
        caught_delta=dict(mean=float(np.mean(deltas)), p95=float(np.percentile(deltas, 95)), max=float(np.max(deltas)))
        if shuffles else {},
        kept_delta=dict(mean=float(np.mean(kdeltas)), p5=float(np.percentile(kdeltas, 5)), min=float(np.min(kdeltas)))
        if shuffles else {},
        share_ge_observed=float(np.mean(np.array(deltas) >= obs)) if shuffles else None)

    zmaps = {"full": {}, "C0": {}, "noCE": {}}
    for kind in [k for k in zmaps if k not in out["stopped_arms"]]:
        for side_rows, zv in zip((fit_rows, ho_rows), zs[kind]):
            for r, v in zip(side_rows, zv):
                zmaps[kind][r["key"]] = float(v)
    zmaps["C0shift"] = shift
    dump = {kind: dict(Z=models[kind].Z_fit, y=(cyf if kind == "noCE" else yf).astype(np.float64),
                       lam=models[kind].lam, w=models[kind].w) for kind in models}
    return out, zmaps, dump, models


def c0_d6_check(rows, int8_outs, model_nl):
    """Control 2b: the D6 rule on C0's z per query language, over rerank_eval.fit_floors' rows of the
    int8 outs; the implied cosine cut, rounded down to 2 dp, must equal R.fit_floors and the v2 refit."""
    by = {r["key"]: r for r in rows}
    groups = {}
    for o in int8_outs:
        if o["positive"] and (o["mechanical"] or not o["held_out"]) and o["rank"] == 1 and o["top"] is not None:
            groups.setdefault(o["language"], []).append(by[(o["corpus"], o["id"])])
    implied = {}
    for l, rs in groups.items():
        z = model_nl.z(matrix(rs, model_nl.cols))
        k = math.floor(MAX_MISS * len(rs) + 1e-9)
        i = int(np.argsort(z, kind="mergesort")[min(k, len(rs) - 1)])
        implied[l] = R.round_down_2(rs[i]["cos1"])
    ff = R.fit_floors(int8_outs)
    return dict(implied=implied, fit_floors=ff, v2_refit=V2_D6_INT8,
                ok=implied == ff and all(implied.get(l) == v for l, v in V2_D6_INT8.items()))


# ---- markdown ----------------------------------------------------------------------------------
def pct(x):
    return "-" if x is None or (isinstance(x, float) and math.isnan(x)) else f"{100 * x:.1f}%"


def kn(t):
    k, n = t
    return f"{pct(k / n) if n else '-'} ({k}/{n})"


def ci3(b, scale=100.0):
    if b is None:
        return "-"
    return f"{scale * b[0]:+.2f} [{scale * b[1]:+.2f}, {scale * b[2]:+.2f}]"


def markdown(res):
    L = [f"# GM-461 learned verdict (scratch tables; results doc is the measure slice's)", "",
         f"sha256 {res['sha256']}, floors {res['floors']}, resamples {res['resamples']}, shuffles {res['shuffles']}", ""]
    L += ["## T1 quality (held-out)", "", "| arm | NL r@10 | NL MRR | name r@10 | name MRR |", "|---|---|---|---|---|"]
    for arm, q in res["T1"].items():
        L.append(f"| {arm} | {ci3(q['nl']['r10'])} | {ci3(q['nl']['mrr'])} | {ci3(q['name']['r10'])} | "
                 f"{ci3(q['name']['mrr'])} |")
    for tname, key in (("T2 D9 gates vs F4", "T2"), ("T3 D9 gates vs int8", "T3")):
        L += ["", f"## {tname} (held-out NL, shipped floors as reference)", "",
              "| arm | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language |",
              "|---|---|---|---|---|---|---|---|"]
        for arm, g in res[key].items():
            fails = ",".join(k for k in ("Q1", "Q2", "Q3", "Q4", "Q5") if not g["pass"][k]) or "none"
            q5l = "; ".join(f"{l} {100 * d:+.1f} (n={n})" for l, (d, n) in g["fa_lang"].items())
            w = g["q3_worst"]
            L.append(f"| {arm} | {100 * g['r10'][0]:+.2f} [{100 * g['r10'][1]:+.2f}] | "
                     f"{g['mrr'][0]:+.4f} [{g['mrr'][1]:+.4f}] | {w[0]} {100 * w[1]:+.1f} | "
                     f"{ci3(g['cw']) if g['cw'] else '-'} | {ci3(g['fa']) if g['fa'] else '-'} | {fails} | {q5l} |")
    L += ["", "## T4 GM-434 columns (held-out, own thresholds)", "",
          "| arm | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |",
          "|---|---|---|---|---|---|---|"]
    for arm, c in res["T4"].items():
        L.append(f"| {arm} | {kn(c['nl_misled'])} | {kn(c['nl_cw_pos'])} | {kn(c['nl_cw_abs'])} | "
                 f"{kn(c['name_misled'])} | {kn(c['name_cw_pos'])} | {kn(c['name_cw_abs'])} |")
    for tname, key in (("T5 per language, held-out NL", "T5"), ("T6 per language, held-out name", "T6")):
        L += ["", f"## {tname}", "", "| arm | language | misled | CW pos | CW absent | misled Wilson hi |",
              "|---|---|---|---|---|---|"]
        for arm, pl in res[key].items():
            for l, c in pl.items():
                L.append(f"| {arm} | {l} | {kn(c['misled'])} | {kn(c['cw_pos'])} | {kn(c['cw_abs'])} | "
                         f"{pct(wilson_hi(*c['misled']))} |")
    L += ["", "## T7 paired deltas vs F4 (held-out, 95% CI)", "",
          "| pop | arm | kept | caught | caught Δ CI | kept Δ CI | differ (right) |", "|---|---|---|---|---|---|---|"]
    for pop in ("NL", "NAME"):
        p = res[pop]
        for arm in ("F4", "C0", "C0-m", "L-m", "L-d6"):
            b = p["boot_vs_F4"].get(arm)
            d = p["differ_vs_F4"].get(arm)
            L.append(f"| {pop} | {arm} | {kn(p['ho'][arm]['kept_k'])} | {kn(p['ho'][arm]['caught_k'])} | "
                     f"{'-' if not b else '[%+.1f, %+.1f]' % tuple(100 * x for x in b['caught_delta_ci'])} | "
                     f"{'-' if not b else '[%+.1f, %+.1f]' % tuple(100 * x for x in b['kept_delta_ci'])} | "
                     f"{'-' if not d else '%d (%d)' % (d['n'], d['right'])} |")
        if "L-noCE" not in p["ho"]:
            L.append(f"| {pop} | L-noCE vs int8 | stopped by rule | - | - | - | - |")
            continue
        b = p["boot_noCE_vs_int8"]["L-noCE"]
        L.append(f"| {pop} | L-noCE vs int8 | {kn(p['ho']['L-noCE']['kept_k'])} | {kn(p['ho']['L-noCE']['caught_k'])} | "
                 f"[{100 * b['caught_delta_ci'][0]:+.1f}, {100 * b['caught_delta_ci'][1]:+.1f}] | "
                 f"[{100 * b['kept_delta_ci'][0]:+.1f}, {100 * b['kept_delta_ci'][1]:+.1f}] | - |")
    L += ["", "## T8 sensitivity (held-out NL, GM-434 columns)", "", "| view | arm | misled | CW pos | CW absent |",
          "|---|---|---|---|---|"]
    for view, arms in res["T8"].items():
        for arm, c in arms.items():
            L.append(f"| {view} | {arm} | {kn(c['misled'])} | {kn(c['cw_pos'])} | {kn(c['cw_abs'])} |")
    L += ["", "## T9 overfitting (FIT vs held-out)", "", "| pop | model | AUROC FIT | AUROC HO | gap | log-loss FIT | log-loss HO |",
          "|---|---|---|---|---|---|---|"]
    for pop in ("NL", "NAME"):
        ov = res[pop]["overfit"]
        for kind in [k for k in ("full", "C0", "noCE") if k in ov]:
            o = ov[kind]
            L.append(f"| {pop} | {kind} | {o['auroc_fit']:.3f} | {o['auroc_ho']:.3f} | {o['auroc_gap']:+.3f} | "
                     f"{o['logloss_fit']:.3f} | {o['logloss_ho']:.3f} |")
    L += ["", "| pop | arm | kept FIT | kept HO | caught FIT | caught HO |", "|---|---|---|---|---|---|"]
    for pop in ("NL", "NAME"):
        ov = res[pop]["overfit"]
        for arm in [k for k in ("L-m", "L-d6", "C0-m", "L-noCE") if k in ov]:
            o = ov[arm]
            L.append(f"| {pop} | {arm} | {pct(o['kept_fit'])} | {pct(o['kept_ho'])} | {pct(o['caught_fit'])} | "
                     f"{pct(o['caught_ho'])} |")
    L += ["", "## T10 model (standardised coefficients, 95% bootstrap over FIT queries)", ""]
    for pop in ("NL", "NAME"):
        m = res[pop]["model_full"]
        L += [f"{pop}: lambda {m['lam']}, CV log-loss {m['cv_logloss']}, gradient norm {m['grad_norm']:.2e}", "",
              "| term | coef | 95% CI |", "|---|---|---|"]
        for c, v in m["coef_std"].items():
            ci = res[pop]["coef_boot"].get(c)
            L.append(f"| {c} | {v:+.3f} | {'-' if not ci else '[%+.3f, %+.3f]' % tuple(ci)} |")
        L.append("")
    L += ["## Arms stopped by rule (informative models: coefficients only, no metrics)", ""]
    for pop in ("NL", "NAME"):
        for kind, why in res[pop]["stopped_arms"].items():
            L.append(f"- {pop} {kind}: {'; '.join(why)}; coefficients {json.dumps(res[pop][f'model_{kind}']['coef_std'])}")
    L += ["", "## Controls", ""]
    for k, v in res["controls"].items():
        L.append(f"- {k}: {json.dumps(v, default=str)}")
    L += ["", "## Verdict inputs (the measure slice decides)", "", f"```\n{json.dumps(res['verdict_inputs'], indent=1)}\n```"]
    return "\n".join(L) + "\n"


# ---- main ----------------------------------------------------------------------------------------
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--cache-dir", type=Path)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--md", type=Path, help="markdown tables (default: --out with .md)")
    ap.add_argument("--resamples", type=int, default=10000)
    ap.add_argument("--shuffles", type=int, default=200)
    ap.add_argument("--coef-boot", type=int, default=1000)
    ap.add_argument("--dump-fit", type=Path, help="write the FIT design matrices + fits as .npz (control 4)")
    ap.add_argument("--ignore-stops", action="store_true",
                    help="diagnostic only: build the tables despite a stop rule (still exits 2); not for the measure run")
    a = ap.parse_args()

    cs, sc, flags, sha = G.setup(a.work, a.cache_dir)
    floors = SFR.structured_floors()
    rows, disagree = features(cs, sc, flags, floors)
    pop_of = {r["key"]: r["pop"] for r in rows}
    int8, f4 = arm_outs(cs, sc, flags, pop_of)
    stops = []
    res = {"sha256": sha, "seed": SEED, "resamples": a.resamples, "shuffles": a.shuffles, "floors": floors,
           "rows": len(rows), "prose_vs_mechanical_disagree": len(disagree), "disagree_ids": disagree[:20],
           "floor_unknown_rows": sum(not r["floor_known"] for r in rows)}

    zmaps, dumps, models = {}, {}, {}
    for pop in ("NL", "NAME"):
        res[pop], zmaps[pop], dumps[pop], models[pop] = fit_population(rows, pop, a.resamples, a.shuffles, a.coef_boot, stops)

    # control 1: F4 reproduces GM-701 arm A; int8 reproduces GM-697/GM-460
    c1 = {pop: {"got": {k: res[pop]["ho"]["F4"][k] for k in ("kept_k", "caught_k")}, "want": GM701_A[pop]}
          for pop in ("NL", "NAME")}
    int8_nl = R.gm434(int8, floors, nl_ho_all)
    c1["int8_nl_ho"] = {"got": int8_nl, "want": GM697_INT8_NL_HO}
    c1_ok = all(c1[p]["got"] == c1[p]["want"] for p in ("NL", "NAME")) and \
        all(tuple(int8_nl[k]) == v for k, v in GM697_INT8_NL_HO.items())
    if not c1_ok:
        stops.append("control 1: F4/int8 do not reproduce GM-701/GM-697")

    # arms as rerank_eval outcome lists
    key = lambda pop, z: {k: (f"clf:{pop}", v) for k, v in z.items()}
    zf = {k: v for pop in ("NL", "NAME") for k, v in key(pop, zmaps[pop]["full"]).items()}
    zc0 = {k: v for pop in ("NL", "NAME") for k, v in key(pop, zmaps[pop]["C0shift"]).items()}
    zc0m = {k: v for pop in ("NL", "NAME") for k, v in key(pop, zmaps[pop]["C0"]).items()}
    znc = {k: v for pop in ("NL", "NAME") for k, v in key(pop, zmaps[pop]["noCE"]).items()}
    th = lambda arm: {f"clf:{p}": res[p]["thresholds"][arm] for p in ("NL", "NAME") if arm in res[p]["thresholds"]}
    arms = {"int8": (int8, floors), "F4": (f4, floors), "C0": as_verdict(f4, zc0, {"clf:NL": 0.0, "clf:NAME": 0.0}),
            "C0-m": as_verdict(f4, zc0m, th("C0-m")), "L-m": as_verdict(f4, zf, th("L-m")),
            "L-d6": as_verdict(f4, zf, th("L-d6"))}
    if th("L-noCE"):  # rows of a population whose L-noCE was stopped get no verdict (empty cells)
        arms["L-noCE"] = as_verdict(int8, znc, th("L-noCE"))
    res["stopped_arms"] = {p: res[p]["stopped_arms"] for p in ("NL", "NAME")}

    # control 2: C0 exact
    c2b = c0_d6_check(rows, int8, models["NL"]["C0"])
    if not c2b["ok"]:
        stops.append("control 2b: D6 rule on C0's z does not give the v2 int8 refit floors")
    g_f4_f4 = Q5.fixed_floors_gates(f4, f4, floors, floors)
    g_c0_f4 = Q5.fixed_floors_gates(f4, arms["C0"][0], floors, arms["C0"][1])
    c0_cells = (gm434_cols(*arms["C0"]) == gm434_cols(f4, floors)
                and per_lang(*arms["C0"], nl_ho_all) == per_lang(f4, floors, nl_ho_all)
                and per_lang(*arms["C0"], name_ho_all) == per_lang(f4, floors, name_ho_all)
                and strip_floors(g_c0_f4) == strip_floors(g_f4_f4))
    if not c0_cells:
        stops.append("control 2a: C0's T2/T4-T6 cells differ from F4's")
    res["controls"] = {
        "1_reproduce": dict(c1, ok=c1_ok),
        "2a_C0_verdict": dict(rows_checked={p: res[p]["control2a_rows_checked"] for p in ("NL", "NAME")},
                              mismatches={p: res[p]["control2a_mismatch_n"] for p in ("NL", "NAME")},
                              cells_equal_F4=c0_cells),
        "2b_C0_d6": c2b,
        "3_shuffle": {p: res[p]["control_shuffle"] for p in ("NL", "NAME")},
        "4_grad_norm": {p: {k: res[p][f"model_{k}"]["grad_norm"] for k in ("full", "C0", "noCE")} for p in ("NL", "NAME")},
        "5_differ_L-m_vs_F4": {p: res[p]["differ_vs_F4"]["L-m"] for p in ("NL", "NAME")},
    }
    res["stops"] = stops

    if a.dump_fit:
        flat = {}
        for pop, d in dumps.items():
            for kind, v in d.items():
                for k, x in v.items():
                    flat[f"{pop}_{kind}_{k}"] = np.asarray(x)
        np.savez(a.dump_fit, **flat)

    if stops and not a.ignore_stops:
        text = json.dumps(res, indent=1, default=str)
        if a.out:
            a.out.write_text(text)
        print(text)
        print("STOP: " + "; ".join(stops), file=sys.stderr)
        sys.exit(2)

    res["T1"] = {arm: quality(arms[arm][0]) for arm in ("int8", "F4", "L-m")}
    res["T2"] = {arm: Q5.fixed_floors_gates(f4, o, floors, fl) for arm, (o, fl) in arms.items()
                 if arm in ("int8", "F4", "C0", "C0-m", "L-m", "L-d6")}
    res["T3"] = {arm: Q5.fixed_floors_gates(int8, o, floors, fl) for arm, (o, fl) in arms.items()
                 if arm in ("int8", "F4", "C0", "C0-m", "L-m", "L-d6")}
    res["T4"] = {arm: gm434_cols(o, fl) for arm, (o, fl) in arms.items()}
    res["T5"] = {arm: per_lang(o, fl, nl_ho_all) for arm, (o, fl) in arms.items()}
    res["T6"] = {arm: per_lang(o, fl, name_ho_all) for arm, (o, fl) in arms.items()}
    v1 = {json.loads(line)["id"] for p in sorted((HERE / "queries").glob("*.jsonl")) for line in p.open() if line.strip()}
    res["T8"] = {
        "v1 subset": {arm: R.gm434(arms[arm][0], arms[arm][1], lambda o: nl_ho_all(o) and o["id"] in v1)
                      for arm in ("F4", "L-m")},
        "strict held-out": {arm: R.gm434(arms[arm][0], arms[arm][1], lambda o: nl_ho_all(o) and o["id"] not in STRICT_DROP)
                            for arm in ("F4", "L-m")},
    }

    # verdict inputs (rule fixed in the design; the measure slice reads and records the verdict)
    nl_g = res["T2"]["L-m"]
    name_lang = {l: dict(misled=c["misled"], wilson_hi=wilson_hi(*c["misled"]),
                         ok=c["misled"][1] > 0 and c["misled"][0] / c["misled"][1] <= MAX_MISS)
                 for l, c in res["T6"]["L-d6"].items()}
    res["verdict_inputs"] = {
        "NL": dict(q4_cw=nl_g["cw"], q4_superior=bool(nl_g["cw"] and nl_g["cw"][0] < 0 and nl_g["cw"][2] < 0),
                   q5_pass=nl_g["pass"]["Q5"], overfit=res["NL"]["overfit"]["overfit_full"],
                   readable=res["NL"]["ho"]["F4"]["caught_k"][1] - res["NL"]["ho"]["F4"]["caught_k"][0] >= 50
                   and res["NL"]["ho"]["F4"]["kept_k"][0] >= 100),
        "NAME": dict(misled_per_lang=name_lang, misled_ok_all=all(v["ok"] for v in name_lang.values()),
                     caught_delta_ci_L_d6=res["NAME"]["boot_vs_F4"]["L-d6"]["caught_delta_ci"],
                     overfit=res["NAME"]["overfit"]["overfit_full"],
                     readable=res["NAME"]["ho"]["F4"]["caught_k"][1] - res["NAME"]["ho"]["F4"]["caught_k"][0] >= 50
                     and res["NAME"]["ho"]["F4"]["kept_k"][0] >= 100),
    }

    text = json.dumps(res, indent=1, default=lambda o: o.item() if hasattr(o, "item") else str(o))
    if a.out:
        a.out.write_text(text)
        (a.md or a.out.with_suffix(".md")).write_text(markdown(json.loads(text)))
    elif a.md:
        a.md.write_text(markdown(json.loads(text)))
    print(text)
    if stops:
        print("STOP (tables built with --ignore-stops): " + "; ".join(stops), file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
