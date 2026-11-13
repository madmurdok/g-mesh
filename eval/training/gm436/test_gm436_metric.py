"""Tests of the GM-436 dev metric (dev_metric.py; design note
docs/design/GM-436-laptop-training.md, section 4 "Learning curve", section 8
behaviours 13-14) on planted fixtures. stdlib + numpy.

Run: python -m unittest discover -s eval/training/gm436 -p 'test_gm436_*.py'
(the GM-436 venv's python: dev_metric imports cache, which reads TOML).
"""

import math
import sys
import unittest
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import dev_metric as M  # noqa: E402

RESAMPLES = 2000


def per_target(values, n=6):
    """Each target's value repeated for its n queries, and the target of each."""
    return np.repeat(values, n), np.repeat([f"t{i}" for i in range(len(values))], n)


class Bootstrap(unittest.TestCase):
    """Behaviour 13: the CI resamples targets, not queries."""

    def test_clustered_ci_is_wider_than_a_query_level_one_on_identical_queries(self):
        rng = np.random.default_rng(1)
        values, clusters = per_target(rng.integers(0, 2, size=40).astype(float))
        clustered = M.cluster_bootstrap(values, clusters, RESAMPLES)
        query_level = M.cluster_bootstrap(values, None, RESAMPLES)
        self.assertAlmostEqual(clustered["point"], query_level["point"])
        self.assertLessEqual(clustered["lower"], clustered["point"])
        self.assertGreaterEqual(clustered["upper"], clustered["point"])
        width = lambda ci: ci["upper"] - ci["lower"]  # noqa: E731
        # Six identical values per target: the query-level width is about 1/sqrt(6) of the clustered one.
        self.assertLess(width(query_level), 0.6 * width(clustered), (clustered, query_level))

    def test_bootstrap_is_seeded(self):
        values, clusters = per_target(np.linspace(0, 1, 20))
        self.assertEqual(M.cluster_bootstrap(values, clusters, RESAMPLES),
                         M.cluster_bootstrap(values, clusters, RESAMPLES))

    def test_unequal_cluster_sizes_weight_queries_not_targets(self):
        # One target of 9 queries at 1.0, nine targets of 1 query at 0.0: query mean 0.5.
        values = np.array([1.0] * 9 + [0.0] * 9)
        clusters = ["big"] * 9 + [f"s{i}" for i in range(9)]
        ci = M.cluster_bootstrap(values, clusters, RESAMPLES)
        self.assertAlmostEqual(ci["point"], 0.5)
        self.assertLess(ci["lower"], 0.5)
        self.assertGreater(ci["upper"], 0.5)


class CurveRule(unittest.TestCase):
    """Behaviour 14: a 50->100 gain whose CI excludes 0 recommends more data."""

    def setUp(self):
        rng = np.random.default_rng(2)
        base, self.clusters = per_target(rng.uniform(0.2, 0.6, size=30))
        self.base = base

    def test_a_planted_gain_produces_the_more_data_line(self):
        noise = np.repeat(np.random.default_rng(3).normal(0, 0.02, size=30), 6)
        points = {0.25: self.base - 0.1, 0.5: self.base, 1.0: self.base + 0.1 + noise}
        out = M.curve_rule(points, self.clusters, RESAMPLES)
        self.assertGreater(out["gain50to100"]["lower"], 0)
        self.assertTrue(out["line"].startswith("Generate more GM-456 data"), out["line"])
        self.assertIn("not a measurement", out["line"])
        # Log-linear fit over 25/50/100: +0.1 MRR per doubling.
        self.assertAlmostEqual(out["fit"]["mrrPerDoubling"], 0.1, delta=0.01)
        self.assertAlmostEqual(out["fit"]["extrapolatedMrrAt2x"], out["fit"]["mrrAt100"] + out["fit"]["mrrPerDoubling"])

    def test_a_gain_whose_ci_includes_zero_recommends_nothing(self):
        swing = np.repeat(np.where(np.arange(30) % 2 == 0, 0.05, -0.05), 6)
        points = {0.25: self.base, 0.5: self.base, 1.0: self.base + swing}
        out = M.curve_rule(points, self.clusters, RESAMPLES)
        self.assertLessEqual(out["gain50to100"]["lower"], 0)
        self.assertGreaterEqual(out["gain50to100"]["upper"], 0)
        self.assertTrue(out["line"].startswith("No data recommendation"), out["line"])

    def test_the_gain_is_paired_over_the_same_queries(self):
        with self.assertRaises(ValueError):
            M.paired_bootstrap(self.base, self.base[:-1], self.clusters, RESAMPLES)


class Ranks(unittest.TestCase):
    def test_first_hit_rank_ties_go_to_the_lower_node(self):
        scores = np.array([[0.5, 0.9, 0.9, 0.1]] * 4)
        ranks = M.first_hit_ranks(scores, [[1], [2], [3, 2], []])
        self.assertEqual(ranks[:3].tolist(), [1, 2, 2])
        self.assertTrue(math.isinf(ranks[3]))

    def test_metrics_are_mrr_and_recall_at_10(self):
        m = M.metrics([1, 2, 11, math.inf])
        self.assertAlmostEqual(m["mrr"], (1 + 0.5 + 1 / 11) / 4)
        self.assertAlmostEqual(m["recall@10"], 0.5)


class C0bCompare(unittest.TestCase):
    """The C0b comparator: order inside a tie group is free; anything else is a mismatch."""

    def rust(self, hits):
        return {"q": {"hits": [list(h) for h in hits]}}

    def test_tie_groups_may_reorder(self):
        rust = self.rust([("a", 0.9), ("b", 0.5), ("c", 0.5 + 1e-6), ("d", 0.1)])
        py = {"q": [("a", 0.9), ("c", 0.5), ("b", 0.5), ("d", 0.1)]}
        self.assertEqual(M.c0b_compare(py, rust), [])

    def test_a_swapped_pair_outside_a_tie_fails(self):
        rust = self.rust([("a", 0.9), ("b", 0.5), ("c", 0.3), ("d", 0.1)])
        py = {"q": [("a", 0.9), ("c", 0.5), ("b", 0.3), ("d", 0.1)]}
        self.assertEqual(M.c0b_compare(py, rust), ["q"])

    def test_the_last_group_only_needs_tied_scores(self):
        rust = self.rust([("a", 0.9), ("b", 0.5)])
        self.assertEqual(M.c0b_compare({"q": [("a", 0.9), ("z", 0.5)]}, rust), [])
        self.assertEqual(M.c0b_compare({"q": [("a", 0.9), ("z", 0.4)]}, rust), ["q"])


if __name__ == "__main__":
    unittest.main()
