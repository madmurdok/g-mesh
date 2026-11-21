"""Tests of the GM-730 windows (windows.py; docs/design/GM-730-gpu-training-routes.md section 8):
the top-k tie rule, the forced gold, the join check against the Rust run's rankings, and
the order-sensitive pairs sha256. A tiny GM-456 version dir and Rust run (gm730_fixture.py);
numpy only.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_windows.py'
"""

import copy
import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm730_fixture as GF  # noqa: E402
import windows as W  # noqa: E402


class WindowsCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.world = GF.World(self.tmp.name)
        self.world.activate(self)
        self.world.import_vectors()

    def windows(self, split="train", k=GF.K_WINDOW, plant=False):
        return W.build_windows(split, k, plant, GF.VERSION)[0]


class TopK(unittest.TestCase):
    def test_ties_go_to_the_lower_index(self):
        s = np.array([0.5, 0.9, 0.5, 0.9, 0.1, 0.5])
        self.assertEqual(W.top_k(s, 4).tolist(), [1, 3, 0, 2])
        self.assertEqual(W.top_k(s, 6).tolist(), [1, 3, 0, 2, 5, 4])


class BuildWindows(WindowsCase):
    def test_gold_is_forced_as_the_last_entry_when_outside_the_top_k(self):
        ws, counts = W.build_windows("train", 2, False, GF.VERSION)
        forced = [w for w in ws if w["forced"]]
        self.assertTrue(forced, "the fixture must have a query whose target is outside its top 2")
        self.assertEqual(counts["forced"], len(forced))
        for w in ws:
            self.assertEqual(w["keys"][w["gold"]], w["target"])
            self.assertEqual(len(w["keys"]), 3 if w["forced"] else 2)
            if w["forced"]:
                self.assertEqual(w["gold"], len(w["keys"]) - 1)
            else:
                self.assertLess(w["gold"], 2)

    def test_labels_are_window_indexes_of_the_queries_sets(self):
        queries = {json.loads(x)["id"]: json.loads(x) for x in (self.world.fx.vdir / "queries.jsonl").read_text().splitlines()}
        ws = self.windows()
        self.assertTrue(any(w["answersAlso"] for w in ws))
        for w in ws:
            q = queries[w["id"]]
            self.assertEqual([w["keys"][i] for i in w["positives"]], [k for k in w["keys"] if k in set(q["positives"])])
            self.assertEqual([w["keys"][i] for i in w["answersAlso"]], [k for k in w["keys"] if k in set(q["answersAlso"])])
            self.assertEqual(len(set(w["keys"])), len(w["keys"]))

    def test_cos_is_descending_over_the_ranked_entries(self):
        for w in self.windows(k=5):
            ranked = w["cos"][:5]
            self.assertEqual(ranked, sorted(ranked, reverse=True))

    def test_query_whose_vector_text_changed_is_dropped(self):
        before = self.windows()
        p = self.world.fx.vdir / "queries.jsonl"
        rows = [json.loads(x) for x in p.read_text().splitlines()]
        victim = next(r["id"] for r in rows if r["split"] == "train")
        for r in rows:
            if r["id"] == victim:
                r["text"] += " changed"
        p.write_text("".join(json.dumps(r) + "\n" for r in rows))
        ws, counts = W.build_windows("train", GF.K_WINDOW, False, GF.VERSION)
        self.assertEqual(counts["droppedQueryVector"], 1)
        self.assertEqual({w["id"] for w in before} - {w["id"] for w in ws}, {victim})


class JoinCheck(WindowsCase):
    def test_windows_equal_to_the_runs_rankings_pass(self):
        res = W.join_check(self.windows(), GF.K_WINDOW)
        self.assertEqual({r: (v["mismatches"], v["missing"]) for r, v in res.items()}, {"alpha": (0, 0)})
        self.assertGreater(res["alpha"]["queries"], 0)

    def test_planted_offset_fails(self):
        res = W.join_check(self.windows(plant=True), GF.K_WINDOW)
        self.assertGreater(res["alpha"]["mismatches"], 0)
        self.assertTrue(res["alpha"]["first"])

    def test_window_without_a_ranking_is_counted_missing(self):
        ws = self.windows()
        ghost = dict(copy.deepcopy(ws[0]), id="no-such-query")
        res = W.join_check(ws + [ghost], GF.K_WINDOW)
        self.assertEqual(res["alpha"]["missing"], 1)
        self.assertIn("no-such-query", res["alpha"]["first"])


class PairsSha(WindowsCase):
    def test_is_the_sha256_of_qid_tab_key_lines_in_order(self):
        ws = self.windows()
        want = hashlib.sha256("".join(f"{w['id']}\t{k}\n" for w in ws for k in w["keys"]).encode()).hexdigest()
        self.assertEqual(W.pairs_sha256(ws), want)

    def test_is_order_sensitive(self):
        ws = self.windows()
        swapped = [ws[1], ws[0]] + ws[2:]
        self.assertNotEqual(W.pairs_sha256(ws), W.pairs_sha256(swapped))
        w0 = dict(ws[0], keys=list(reversed(ws[0]["keys"])))
        self.assertNotEqual(W.pairs_sha256(ws), W.pairs_sha256([w0] + ws[1:]))


if __name__ == "__main__":
    unittest.main()
