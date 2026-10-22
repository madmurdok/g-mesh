"""Unit tests for gm453_summary.py (GM-453 S14 fixes L1, L2, L5).

Run: python3 -I eval/embedding/test_gm453_summary.py
"""

import importlib.util
import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("gm453_summary", HERE / "gm453_summary.py")
summary = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(summary)


def write_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


class QualityTest(unittest.TestCase):
    """L1: recall@10 and MRR over the authored positive queries only."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.queries = root / "queries"
        self.saved = summary.QUERIES
        summary.QUERIES = self.queries
        write_jsonl(self.queries / "gin.jsonl", [
            {"id": "q1", "kind": "positive", "text": "a"},
            {"id": "q2", "kind": "positive", "text": "b"},
            {"id": "q3", "kind": "positive", "text": "c"},
            {"id": "q4", "kind": "negative", "text": "d"},
        ])
        self.run_dir = root / "A" / "r1" / "A0" / "variant" / "gin"
        hits = [[f"n{i}", 1.0 - i / 100] for i in range(1, 16)]
        write_jsonl(self.run_dir / "rankings.jsonl", [
            {"id": "q1", "expected": ["n1"], "hits": hits},            # rank 1
            {"id": "q2", "expected": ["n12", "n4"], "hits": hits},     # first expected at rank 4
            {"id": "q3", "expected": ["n12"], "hits": hits},           # rank 12: outside @10
            {"id": "q4", "expected": ["n1"], "hits": hits},            # negative: not scored
            {"id": "q9", "expected": ["n1"], "hits": hits},            # not authored: not scored
        ])

    def tearDown(self):
        summary.QUERIES = self.saved
        self.tmp.cleanup()

    def test_recall_and_mrr_count_only_authored_positives(self):
        recall, mrr, count = summary.quality(self.run_dir)
        self.assertEqual(count, 3)
        self.assertAlmostEqual(recall, 2 / 3)
        self.assertAlmostEqual(mrr, (1 + 1 / 4 + 1 / 12) / 3)

    def test_a_query_without_an_expected_hit_scores_zero(self):
        write_jsonl(self.run_dir / "rankings.jsonl", [
            {"id": "q1", "expected": ["missing"], "hits": [["n1", 0.9]]},
            {"id": "q2", "expected": ["n1"], "hits": [["n1", 0.9]]},
        ])
        recall, mrr, count = summary.quality(self.run_dir)
        self.assertEqual((recall, mrr, count), (0.5, 0.5, 2))

    def test_no_rankings_gives_none(self):
        self.assertIsNone(summary.quality(self.run_dir.parent / "absent"))


def compare_json(compared=10, bit=10, max_l2=0.0, min_cos=1.0, over=0):
    return {"compared": compared, "bitIdentical": bit, "maxL2": max_l2, "minCos": min_cos,
            "overTolerance": over}


class ParityTest(unittest.TestCase):
    """L2: a compare JSON with null maxL2/minCos (a NaN) is a failure, not a crash."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        (self.out / "parity").mkdir()

    def tearDown(self):
        self.tmp.cleanup()

    def put(self, name, data):
        (self.out / "parity" / f"{name}.json").write_text(json.dumps(data))

    def test_null_distances_count_as_a_parity_failure(self):
        self.put("A-r1-A9-gin", compare_json(bit=0, max_l2=None, min_cos=None, over=0))
        self.put("A-r1-A9-py-requests", compare_json(max_l2=1e-6, min_cos=0.99))
        par = summary.parity(self.out)
        merged = par[("A", "1", "A9")]
        self.assertEqual(merged["maxL2"], float("inf"))
        self.assertEqual(merged["minCos"], float("-inf"))
        self.assertGreater(merged["over"], 0)
        self.assertFalse(summary.arm_passes(par, "A", "A9"))

    def test_passing_comparisons_merge_over_corpora(self):
        self.put("A-r1-A3-gin", compare_json(compared=4, bit=4))
        self.put("A-r1-A3-py-requests", compare_json(compared=6, bit=5, max_l2=3e-5, min_cos=0.9999))
        merged = summary.parity(self.out)[("A", "1", "A3")]
        self.assertEqual((merged["compared"], merged["bit"], merged["over"]), (10, 9, 0))
        self.assertEqual(merged["maxL2"], 3e-5)
        self.assertTrue(summary.arm_passes(summary.parity(self.out), "A", "A3"))


class GateTest(unittest.TestCase):
    """L5: no phase-A A0 timing exits non-zero with a message."""

    def test_gate_without_an_a0_timing_exits_with_a_message(self):
        with tempfile.TemporaryDirectory() as out:
            with self.assertRaises(SystemExit) as raised:
                summary.gate(out)
            self.assertIsInstance(raised.exception.code, str)
            self.assertIn("A0", raised.exception.code)

    def test_gate_with_an_a0_timing_prints_a_verdict(self):
        with tempfile.TemporaryDirectory() as out:
            rows = ["phase\tround\tarm\tembed_ms", "A\t1\tA0\t1000", "A\t1\tA3\t900"]
            (Path(out) / "timing.tsv").write_text("\n".join(rows) + "\n")
            buffer = io.StringIO()
            with redirect_stdout(buffer):
                summary.gate(out)
            self.assertTrue(buffer.getvalue().startswith("go"), buffer.getvalue())


if __name__ == "__main__":
    unittest.main()
