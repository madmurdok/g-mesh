"""Unit tests for gm461_learned_verdict.py (GM-461 learned no-match verdict).

Synthetic fixtures only: tiny fake corpora for feature extraction and generated
rows for the fit; the held-out split and the eval data are never read. numpy
must import.

Run: ~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -I eval/embedding/test_gm461_learned_verdict.py
"""

import copy
import math
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import gm461_learned_verdict as V  # noqa: E402


# ---- feature extraction fixtures ---------------------------------------------------------------
def fake_corpus(lists, expected, logits, docs, qtext="find the thing", lang="go"):
    """One corpus, one prose query "q1" (held_out follows the sha256 side, as features() requires)."""
    q = "q1"
    queries = {q: dict(text=qtext, mechanical=False, language=lang, positive=True,
                       held_out=V.G.name_held_out(q))}
    node_lang = {nid: "go" for nid, _ in lists}
    c = SimpleNamespace(name="c", qids=[q], queries=queries, lists={V.B.ST: {q: lists}},
                        expected={q: set(expected)}, node_lang=node_lang, trimmed_doc=docs)
    sc = lambda c_, q_, nid: logits[nid]  # noqa: E731
    return c, sc


def one_row(lists, expected, logits, docs, skip=False, floors=None):
    c, sc = fake_corpus(lists, expected, logits, docs)
    rows, _ = V.features([c], sc, {("c", "q1"): skip}, floors or {"go": 0.5})
    assert len(rows) == 1
    return rows[0]


class FeatureTests(unittest.TestCase):
    # a: cos 0.9, b: 0.8, c: 0.7; the CE logits promote b to the reranked top, a is second.
    LISTS = [("a", 0.9), ("b", 0.8), ("c", 0.7)]
    LOGITS = {"a": 0.0, "b": 10.0, "c": -100.0}

    def test_gap_logit_in_rerank_order_gap_cos_in_cosine_order(self):
        r = one_row(self.LISTS, ["b"], self.LOGITS, {})
        # reranked order is b, a, c: gap_logit = logit(b) - logit(a), not cosine-order logit[0]-logit[1] (-10)
        self.assertEqual(r["gap_logit"], 10.0)
        self.assertAlmostEqual(r["gap_cos"], 0.1)
        self.assertEqual((r["cos1"], r["cos_t"], r["logit_t"]), (0.9, 0.8, 10.0))
        self.assertTrue(r["right"])
        self.assertFalse(r["cos_right"])

    def test_has_doc_reads_shown_row_doc0_reads_cosine_top(self):
        r = one_row(self.LISTS, ["b"], self.LOGITS, {"a": False, "b": True})
        self.assertEqual((r["has_doc"], r["doc0"]), (1.0, 0.0))
        r = one_row(self.LISTS, ["b"], self.LOGITS, {"a": True, "b": False})
        self.assertEqual((r["has_doc"], r["doc0"]), (0.0, 1.0))

    def test_skip_keeps_cosine_top_and_flags_exact(self):
        r = one_row(self.LISTS, ["a"], self.LOGITS, {}, skip=True)
        self.assertEqual(r["exact"], 1.0)
        self.assertEqual(r["cos_t"], r["cos1"])
        self.assertTrue(r["right"])

    def test_floor_flag_and_single_hit_gaps(self):
        r = one_row([("a", 0.4)], ["a"], {"a": 1.0}, {}, floors={"go": 0.5})
        self.assertTrue(r["floor"])  # cos1 < shipped floor of the cosine top's language
        self.assertEqual((r["gap_cos"], r["gap_logit"]), (0.0, 0.0))
        r = one_row([("a", 0.5)], ["a"], {"a": 1.0}, {}, floors={"go": 0.5})
        self.assertFalse(r["floor"])  # boundary clears (>=)

    def test_language_one_hots_use_reference_level_go(self):
        r = one_row([("a", 0.9)], ["a"], {"a": 0.0}, {})
        self.assertNotIn("lang:go", r)
        self.assertEqual([r[f"lang:{l}"] for l in V.ONEHOT], [0.0, 0.0, 0.0])

    def test_empty_window_has_no_row(self):
        c, sc = fake_corpus([], [], {}, {})
        rows, _ = V.features([c], sc, {("c", "q1"): False}, {"go": 0.5})
        self.assertEqual(rows, [])


# ---- logistic fit ------------------------------------------------------------------------------
def objective(X, y, w, lam):
    A = np.hstack([np.ones((len(X), 1)), X])
    z = A @ w
    return float(np.sum(np.logaddexp(0.0, z) - y * z) + 0.5 * lam * np.sum(w[1:] ** 2))


def kkt_gradient(X, y, w, lam):
    """Independent gradient of the penalised objective (intercept unpenalised)."""
    A = np.hstack([np.ones((len(X), 1)), X])
    p = 1.0 / (1.0 + np.exp(-(A @ w)))
    g = A.T @ (p - y)
    g[1:] += lam * w[1:]
    return g


def make_xy(n=120, seed=7):
    rng = np.random.default_rng(seed)
    X = rng.normal(size=(n, 3))
    y = (rng.random(n) < 1.0 / (1.0 + np.exp(-(0.8 * X[:, 0] - 0.5 * X[:, 1] + 0.2)))).astype(np.float64)
    return X, y


class LogregTests(unittest.TestCase):
    def test_converges_to_penalised_optimum(self):
        X, y = make_xy()
        for lam in (0.01, 1.0, 10.0):
            w = V.fit_logreg(X, y, lam)
            self.assertLess(np.max(np.abs(kkt_gradient(X, y, w, lam))), 1e-8, lam)
            rng = np.random.default_rng(1)
            for _ in range(20):  # strictly convex: every perturbation is worse
                self.assertGreater(objective(X, y, w + 1e-3 * rng.normal(size=len(w)), lam),
                                   objective(X, y, w, lam))

    def test_zero_feature_closed_form(self):
        # a constant (zero) column: slope stays 0, intercept = logit(mean y), whatever lambda is
        y = np.array([1.0] * 3 + [0.0] * 9)
        for lam in (0.01, 10.0):
            w = V.fit_logreg(np.zeros((12, 1)), y, lam)
            self.assertAlmostEqual(w[1], 0.0, places=12)
            self.assertAlmostEqual(w[0], math.log(3 / 9), places=9)

    def test_penalty_shrinks_slopes_not_intercept_term(self):
        X, y = make_xy()
        small, big = V.fit_logreg(X, y, 0.01), V.fit_logreg(X, y, 100.0)
        self.assertLess(np.linalg.norm(big[1:]), np.linalg.norm(small[1:]))

    def test_model_gradient_norm_matches_independent_gradient(self):
        X, y = make_xy()
        m = V.Model(["a", "b", "c"], X, y, 1.0)
        self.assertLess(m.grad_norm, 1e-8)
        self.assertLess(np.max(np.abs(kkt_gradient(m.Z_fit, y, m.w, 1.0))), 1e-8)

    def test_standardisation_uses_fit_statistics(self):
        X, y = make_xy()
        m = V.Model(["a", "b", "c"], X, y, 1.0)
        np.testing.assert_allclose(m.mu, X.mean(axis=0))
        np.testing.assert_allclose(m.sd, X.std(axis=0))
        Xh = X[:10] * 5.0 + 3.0  # held-out with other statistics
        want = ((Xh - X.mean(axis=0)) / X.std(axis=0)) @ m.w[1:] + m.w[0]
        np.testing.assert_allclose(m.z(Xh), want)
        # a constant column keeps SD 1
        self.assertEqual(V.standardise(np.ones((4, 1)))[1][0], 1.0)

    def test_cv_folds_are_stratified(self):
        y = np.array([True] * 10 + [False] * 40)
        f = V.folds_of(y, 5, 461)
        for i in range(5):
            self.assertEqual(int(y[f == i].sum()), 2)
            self.assertEqual(int((~y[f == i]).sum()), 8)

    def test_cv_lambda_tie_goes_to_larger_lambda(self):
        y = np.array([1.0] * 10 + [0.0] * 40)
        best, curve = V.cv_lambda(np.zeros((50, 1)), y)  # lambda cannot matter with a zero column
        self.assertEqual(len(set(curve.values())), 1)
        self.assertEqual(best, max(V.LAMBDAS))


# ---- thresholds and verdict --------------------------------------------------------------------
class ThresholdTests(unittest.TestCase):
    def test_matched_threshold_is_largest_cut_keeping_target(self):
        z = [1.0, 5.0, 3.0, 2.0, 4.0]
        t = V.matched_threshold(z, 3)
        self.assertEqual(t, 3.0)
        self.assertGreaterEqual(sum(v >= t for v in z), 3)  # boundary row kept
        self.assertLess(sum(v >= np.nextafter(t, np.inf) for v in z), 3)  # nothing larger keeps 3

    def test_matched_threshold_edges(self):
        self.assertEqual(V.matched_threshold([1.0, 2.0], 0), float("inf"))
        self.assertEqual(V.matched_threshold([1.0, 2.0], 3), float("-inf"))
        self.assertEqual(V.matched_threshold([1.0, 2.0], 2), 1.0)
        self.assertEqual(V.matched_threshold([2.0, 2.0, 2.0], 2), 2.0)  # ties at the cut are all kept

    def test_d6_threshold_index_and_no_rounding(self):
        z = [0.123456 + 0.0137 * i for i in range(100)]
        self.assertEqual(V.d6_threshold(z), z[3])  # floor(0.03 * 100) = 3rd order statistic, unrounded
        self.assertNotEqual(V.d6_threshold(z), math.floor(z[3] * 100) / 100)
        z = [float(i) for i in range(150)]
        self.assertEqual(V.d6_threshold(z), 4.0)  # floor(4.5), not ceil
        self.assertEqual(V.d6_threshold([]), float("-inf"))

    def test_d6_threshold_matches_fit_floors_rule_after_rounding(self):
        cos = [0.3 + 0.0071 * i for i in range(120)]
        outs = [dict(positive=True, mechanical=False, held_out=False, rank=1, top=c, language="go")
                for c in cos]
        want = V.R.fit_floors(outs)["go"]
        self.assertEqual(V.R.round_down_2(V.d6_threshold(cos)), want)

    def test_as_verdict_reproduces_floor_verdict_with_classifier_key(self):
        floors = {"go": 0.5, "python": 0.4}
        outs = [dict(corpus="c", id=f"q{i}", positive=True, mechanical=False, held_out=True, rank=1,
                     language=l, top=t, top_language=l)
                for i, (l, t) in enumerate([("go", 0.6), ("go", 0.4), ("python", 0.4), ("python", 0.39),
                                            ("go", 0.5)])]
        z = {("c", o["id"]): ("clf", o["top"] - floors[o["top_language"]]) for o in outs}  # 1-D monotone
        outs2, fl2 = V.as_verdict(outs, z, {"clf": 0.0})
        self.assertEqual(fl2, {"clf": 0.0})
        for o, o2 in zip(outs, outs2):
            self.assertEqual(V.R.clears(o2, fl2), V.R.clears(o, floors), o["id"])
            self.assertEqual(o2["top_language"], "clf")
        self.assertEqual(outs[0]["top_language"], "go")  # input not mutated

    def test_as_verdict_without_z_has_no_verdict(self):
        outs2, fl2 = V.as_verdict([dict(corpus="c", id="x", top=0.9, top_language="go")], {}, {"clf": 0.0})
        self.assertIsNone(outs2[0]["top"])
        self.assertIsNone(V.R.clears(outs2[0], fl2))


# ---- stop rule ---------------------------------------------------------------------------------
def coef(**kw):
    c = dict(cos1=0.0, cos_t=0.0, logit_t=0.0)
    c.update(kw)
    return c


class SignStopTests(unittest.TestCase):
    def test_full_checks_net_cosine_not_each_cosine(self):
        # the S2 NL case: cos1 negative, cos_t positive, net positive -> no stop
        self.assertEqual(V.sign_stops("NL", "full", coef(cos1=-1.061, cos_t=1.347, logit_t=0.5)), [])
        self.assertEqual(V.sign_stops("NAME", "full", coef(cos1=0.879, cos_t=-0.298, logit_t=2.0)), [])

    def test_full_stops_on_non_positive_net_cosine_or_logit(self):
        s = V.sign_stops("NL", "full", coef(cos1=-1.0, cos_t=0.5, logit_t=0.5))
        self.assertEqual(len(s), 1)
        self.assertIn("cos1 + cos_t", s[0])
        self.assertEqual(len(V.sign_stops("NL", "full", coef(cos1=1.0, cos_t=1.0, logit_t=-0.1))), 1)
        self.assertEqual(len(V.sign_stops("NL", "full", coef(cos1=0.5, cos_t=-0.5, logit_t=1.0))), 1)  # zero
        self.assertEqual(len(V.sign_stops("NL", "full", coef(cos1=-1.0, cos_t=-1.0, logit_t=-1.0))), 2)

    def test_c0_and_noce_check_cos1_alone(self):
        for kind in ("C0", "noCE"):
            self.assertEqual(V.sign_stops("NAME", kind, coef(cos1=0.2)), [])
            self.assertEqual(len(V.sign_stops("NAME", kind, coef(cos1=-0.3037))), 1)
            self.assertEqual(len(V.sign_stops("NAME", kind, coef(cos1=0.0))), 1)

    def test_only_noce_is_informative(self):
        self.assertEqual(V.INFORMATIVE, {"noCE"})


# ---- fit_population on generated rows ----------------------------------------------------------
def gen_rows(n_fit=240, n_ho=100, seed=3, cos_sign_right=1.0, cos_sign_cosright=1.0, ho_shift=False, split=False):
    """NL rows: right depends on cos (sign cos_sign_right) and the CE logit; cos_right on cos (sign
    cos_sign_cosright). cos_t == cos1. Shipped floor 0.5 on every row (F4 refusal = cos1 < 0.5)."""
    rng = np.random.default_rng(seed)
    rows = []
    for i in range(n_fit + n_ho):
        ho = i >= n_fit
        cos1 = float(rng.uniform(0.2, 0.9))
        logit = float(rng.normal())
        cos_t = cos1 + float(rng.normal(0, 0.1)) if split else cos1
        eff = 6.0 * (cos_t - 0.55) - 3.0 * (cos1 - 0.55) if split else 3.0 * cos_sign_right * (cos1 - 0.55)
        right = bool(rng.random() < 1 / (1 + math.exp(-(eff + 1.5 * logit))))
        cos_right = bool(rng.random() < 1 / (1 + math.exp(-(6.0 * cos_sign_cosright * (cos1 - 0.55)))))
        if ho and ho_shift:
            cos1, logit, right, cos_right = 0.99 - cos1 / 10, -logit * 7, not right, not cos_right
        r = dict(key=("c", f"q{i}"), id=f"q{i}", corpus="c", pop="NL", mechanical=False, ho=ho, qlang="go",
                 pos=True, right=right, cos_right=cos_right, clang="go", tlang="go", floor_known=True,
                 floor=cos1 < 0.5, floor_value=0.5, cos1=cos1, cos_t=cos_t, logit_t=logit,
                 gap_cos=float(rng.uniform(0, 0.2)), gap_logit=float(rng.normal()),
                 has_doc=float(rng.random() < 0.5), doc0=float(rng.random() < 0.5))
        for l in V.ONEHOT:
            r[f"lang:{l}"] = 0.0
            r[f"clang:{l}"] = 0.0
        rows.append(r)
    return rows


def run_fp(rows, pop="NL"):
    stops = []
    res = V.fit_population(rows, pop, resamples=20, shuffles=2, coef_boot=5, stops=stops)
    return res, stops


class FitPopulationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.rows = gen_rows()
        cls.res, cls.stops = run_fp(cls.rows)

    def test_healthy_fixture_has_no_stops_and_all_arms(self):
        out = self.res[0]
        self.assertEqual(self.stops, [])
        self.assertEqual(out["stopped_arms"], {})
        for arm in ("L-m", "L-d6", "C0-m", "L-noCE"):
            self.assertIn(arm, out["thresholds"])
            self.assertIn(arm, out["ho"])
        for kind in ("full", "C0", "noCE"):
            self.assertLess(out[f"model_{kind}"]["grad_norm"], 1e-8)

    def test_counts_use_fit_and_heldout_sides_separately(self):
        out = self.res[0]
        self.assertEqual((out["n_fit"], out["n_ho"]), (240, 100))
        self.assertEqual(out["right_fit"], sum(r["right"] for r in self.rows if not r["ho"]))
        self.assertEqual(out["right_ho"], sum(r["right"] for r in self.rows if r["ho"]))

    def test_c0_at_shipped_floor_reproduces_shipped_verdict(self):
        out = self.res[0]
        self.assertEqual(out["control2a_mismatch_n"], 0)
        self.assertEqual(out["control2a_rows_checked"], 340)
        self.assertEqual(out["fit"]["C0"], out["fit"]["F4"])
        self.assertEqual(out["ho"]["C0"], out["ho"]["F4"])

    def test_matched_threshold_keeps_f4_kept_count_on_fit(self):
        out = self.res[0]
        self.assertGreaterEqual(out["fit"]["L-m"]["kept_k"][0], out["kept_target_fit"]["F4"])
        self.assertEqual(out["fit"]["L-m"]["kept_k"][1], out["fit"]["F4"]["kept_k"][1])

    def test_fit_uses_fit_rows_only(self):
        # wild held-out rows must change nothing the FIT determines: coefficients, means, thresholds
        base = self.res[0]
        other, _ = run_fp(gen_rows(ho_shift=True))
        o = other[0]
        for kind in ("full", "C0", "noCE"):
            self.assertEqual(o[f"model_{kind}"]["coef_std"], base[f"model_{kind}"]["coef_std"])
            self.assertEqual(o[f"model_{kind}"]["mu"], base[f"model_{kind}"]["mu"])
            self.assertEqual(o[f"model_{kind}"]["sd"], base[f"model_{kind}"]["sd"])
        self.assertEqual(o["thresholds"], base["thresholds"])
        self.assertEqual(o["fit"], base["fit"])
        self.assertNotEqual(o["ho"]["L-m"], base["ho"]["L-m"])  # the held-out side did move
        full = base["model_full"]
        fit_X = V.matrix([r for r in self.rows if not r["ho"]], full["cols"])
        np.testing.assert_allclose(full["mu"], fit_X.mean(axis=0))

    def test_noce_failure_stops_only_that_arm(self):
        rows = gen_rows(cos_sign_cosright=-1.0)  # cos_right falls with cos1: noCE cos1 < 0; full/C0 healthy
        (out, zmaps, _, _), stops = run_fp(rows)
        self.assertEqual(stops, [])
        self.assertEqual(list(out["stopped_arms"]), ["noCE"])
        self.assertIn("noCE", out["stopped_arms"]["noCE"][0])
        for d in (out["thresholds"], out["ho"], out["fit"]):
            self.assertNotIn("L-noCE", d)
        self.assertNotIn("noCE", out["overfit"])
        self.assertEqual(zmaps["noCE"], {})
        for arm in ("L-m", "L-d6", "C0-m"):  # the deciding arms are still measured
            self.assertIn(arm, out["ho"])

    def test_deciding_model_failure_is_a_run_stop(self):
        rows = gen_rows(cos_sign_right=-1.0, cos_sign_cosright=-1.0)  # right falls with cos1
        (out, _, _, _), stops = run_fp(rows)
        self.assertTrue(any(s.startswith("NL full:") and "cos1 + cos_t" in s for s in stops), stops)
        self.assertTrue(any(s.startswith("NL C0:") for s in stops), stops)
        self.assertEqual(list(out["stopped_arms"]), ["noCE"])  # informative arm still goes to its own list

    def test_net_cosine_not_cos1_alone_decides_full_stop(self):
        # cos_t carries the cosine effect and cos1 gets a negative coefficient, the net effect is positive:
        # the full model must not stop
        (out, _, _, _), stops = run_fp(gen_rows(split=True))
        c = out["model_full"]["coef_std"]
        self.assertLess(c["cos1"], 0)
        self.assertGreater(c["cos1"] + c["cos_t"], 0)
        self.assertEqual([s for s in stops if "full" in s], [], stops)


if __name__ == "__main__":
    unittest.main()
