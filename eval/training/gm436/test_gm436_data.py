"""Tests of the GM-436 training data helpers (data.py; design note
docs/design/GM-436-laptop-training.md, section 4 "Masks", "Learning curve",
section 6 C1/C2, section 8 behaviours 9-11). stdlib + numpy.

Run: python3 -m unittest discover -s eval/training/gm436 -p 'test_gm436_*.py'
"""

import sys
import unittest
from collections import Counter
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import data as D  # noqa: E402


def q(qid, target, positives, negatives=(), answers_also=()):
    return {"id": qid, "target": target, "positives": list(positives), "negatives": list(negatives),
            "answersAlso": list(answers_also)}


class Masks(unittest.TestCase):
    """Behaviour 9: no positive, answersAlso or same-target positive is a negative."""

    def setUp(self):
        # a1, a2: one target (shared positive P); b: positive Q whose hard
        # negative is P; a1's answersAlso X is b's hard negative; c has two
        # positives, one of them a1's hard negative.
        self.batch = [
            q("a1", "A", ["P"], negatives=["N1", "C2"], answers_also=["X"]),
            q("a2", "A", ["P"], negatives=["N2"]),
            q("b", "B", ["Q"], negatives=["P", "X"]),
            q("c", "C", ["C1", "C2"], negatives=["N1"]),
        ]
        self.cols, self.pos, self.valid = D.batch_columns(self.batch)
        self.col = {k: j for j, k in enumerate(self.cols)}

    def negatives_of(self, i):
        return {k for k, j in self.col.items() if self.valid[i, j] and not self.pos[i, j]}

    def test_columns_are_every_in_batch_positive_and_hard_negative_once(self):
        self.assertEqual(sorted(self.cols), sorted({"P", "N1", "C2", "N2", "Q", "X", "C1"}))
        self.assertEqual(len(self.cols), len(set(self.cols)))

    def test_a_positive_is_never_a_negative_whoever_brought_it_in(self):
        for i, query in enumerate(self.batch):
            for k in query["positives"]:
                self.assertTrue(self.pos[i, self.col[k]], (query["id"], k))
                self.assertNotIn(k, self.negatives_of(i))
        # b brought P in as a hard negative; it stays a positive of a1 and a2.
        self.assertTrue(self.pos[0, self.col["P"]] and self.pos[1, self.col["P"]])

    def test_same_target_queries_share_their_positives(self):
        self.assertTrue((self.pos[0] == self.pos[1]).all())

    def test_answers_also_is_neither_positive_nor_negative(self):
        j = self.col["X"]
        self.assertFalse(self.valid[0, j])
        self.assertFalse(self.pos[0, j])
        self.assertNotIn("X", self.negatives_of(0))
        # For b, X is an ordinary hard negative.
        self.assertIn("X", self.negatives_of(2))

    def test_a_multi_positive_query_marks_every_positive(self):
        self.assertTrue(self.pos[3, self.col["C1"]] and self.pos[3, self.col["C2"]])
        # C2 is a1's hard negative, and still c's positive.
        self.assertIn("C2", self.negatives_of(0))

    def test_cross_batch_hard_negatives_enter_every_row(self):
        self.assertIn("N2", self.negatives_of(0))
        self.assertIn("Q", self.negatives_of(0))


class NestedSubsets(unittest.TestCase):
    """Behaviour 10: 25% within 50% within 100%, by target."""

    def setUp(self):
        self.queries = [q(f"t{t}-q{j}", f"t{t}", [f"t{t}"]) for t in range(40) for j in range(6)]
        self.targets = {x["target"] for x in self.queries}

    def test_subsets_are_nested_prefixes_of_the_targets(self):
        s = D.nested_subsets([x["target"] for x in self.queries])
        self.assertEqual(len(s[0.25]), 10)
        self.assertEqual(len(s[0.5]), 20)
        self.assertEqual(s[1.0], self.targets)
        self.assertTrue(s[0.25] < s[0.5] < s[1.0])

    def test_a_targets_queries_go_together(self):
        for f in (0.25, 0.5, 1.0):
            kept = D.subset_queries(self.queries, f)
            per = Counter(x["target"] for x in kept)
            self.assertTrue(all(n == 6 for n in per.values()), f)
            self.assertEqual(len(per), int(round(f * 40)))
        q25 = {x["id"] for x in D.subset_queries(self.queries, 0.25)}
        q50 = {x["id"] for x in D.subset_queries(self.queries, 0.5)}
        self.assertTrue(q25 < q50)

    def test_subsets_do_not_depend_on_query_order(self):
        shuffled = [self.queries[i] for i in np.random.default_rng(7).permutation(len(self.queries))]
        self.assertEqual({x["id"] for x in D.subset_queries(shuffled, 0.5)},
                         {x["id"] for x in D.subset_queries(self.queries, 0.5)})

    def test_subsets_are_seeded(self):
        a = D.nested_subsets(sorted(self.targets), seed=436)[0.5]
        self.assertEqual(a, D.nested_subsets(sorted(self.targets), seed=436)[0.5])
        self.assertNotEqual(a, D.nested_subsets(sorted(self.targets), seed=1)[0.5])


class Controls(unittest.TestCase):
    """Behaviour 11: the C1 derangement has no fixed point; C2 keeps each list's multiset."""

    def test_derangement_has_no_fixed_point(self):
        for n in (2, 3, 5, 17, 100):
            for seed in range(20):
                p = D.derangement(n, seed)
                self.assertEqual(sorted(p), list(range(n)))
                self.assertTrue(all(p[i] != i for i in range(n)), (n, seed, p))
        with self.assertRaises(ValueError):
            D.derangement(1)

    def test_shuffled_pairs_give_every_query_another_targets_labels(self):
        queries = []
        for t in range(12):
            for j in range(3):
                queries.append(q(f"t{t}-q{j}", f"t{t}", [f"P{t}"], negatives=[f"N{t}"], answers_also=[f"X{t}"]))
        out = D.shuffle_pairs(queries)
        self.assertEqual([x["id"] for x in out], [x["id"] for x in queries])
        by_target = {}
        for x in out:
            self.assertNotEqual(x["labelsOf"], x["target"])
            self.assertNotEqual(x["positives"], [f"P{x['target'][1:]}"])
            src = x["labelsOf"][1:]
            self.assertEqual((x["positives"], x["negatives"], x["answersAlso"]), ([f"P{src}"], [f"N{src}"], [f"X{src}"]))
            by_target.setdefault(x["target"], set()).add(x["labelsOf"])
        # A target's queries keep one partner, and the partners are a permutation.
        self.assertTrue(all(len(v) == 1 for v in by_target.values()))
        self.assertEqual(sorted(next(iter(v)) for v in by_target.values()), sorted(by_target))

    def test_permute_within_keeps_each_lists_multiset(self):
        rng = np.random.default_rng(3)
        scores = {f"q{i}": rng.normal(size=int(rng.integers(5, 60))).round(3) for i in range(30)}
        scores["dup"] = np.array([0.5, 0.5, 0.1, 0.9, 0.1])
        out = D.permute_within(scores)
        self.assertEqual(set(out), set(scores))
        moved = 0
        for qid, s in scores.items():
            self.assertEqual(sorted(out[qid].tolist()), sorted(s.tolist()), qid)
            moved += not np.array_equal(out[qid], s)
        self.assertGreater(moved, 20)


if __name__ == "__main__":
    unittest.main()
