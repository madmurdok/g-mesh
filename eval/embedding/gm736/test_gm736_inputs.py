"""GM-736 input and threshold tests (docs/design/GM-736-cre-int8.md, section 5.1: T5, T6;
the review fixes: a selection sample disjoint from the gate sample, the jina int8 size budget).
No ONNX model is run.

Run: <gm703 venv>/bin/python -m unittest discover -s eval/embedding/gm736 -p 'test_gm736_*.py'
"""

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "gm703"))
sys.path.insert(0, str(HERE))

try:
    import numpy as np

    import parity as P
    import recipe as R
    import samples as SM
except ImportError as e:  # pragma: no cover - outside the gm703 venv
    raise unittest.SkipTest(f"needs the gm703 venv: {e}")


def corpora(tmp, rows):
    p = Path(tmp) / "corpora.toml"
    p.write_text("".join(f'[[corpus]]\nid = "{i}"\nsplit = "{s}"\n\n' for i, s in rows))
    return p


def repo_of(row):
    return row["id"].split(":")[1]


class Splits(unittest.TestCase):
    """T5: calibration reads only `train` repos, selection only `dev`, neither an eval v2 corpus."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def test_each_split_reads_only_its_own_repos(self):
        p = corpora(self.tmp.name, [("serde-json", "train"), ("axum", "train"), ("attrs", "dev")])
        self.assertEqual(SM.split_repos("train", p), ["axum", "serde-json"])
        self.assertEqual(SM.split_repos("dev", p), ["attrs"])

    def test_an_eval_v2_corpus_in_either_split_is_refused(self):
        for split, leaked in (("train", "gin"), ("dev", "ripgrep")):
            p = corpora(self.tmp.name, [("serde-json", "train"), ("attrs", "dev"), (leaked, split)])
            with self.assertRaises(SystemExit) as cm:
                SM.split_repos(split, p)
            self.assertIn(leaked, str(cm.exception))

    def test_the_calibration_build_refuses_before_reading_any_snapshot(self):
        p = corpora(self.tmp.name, [("serde-json", "train"), ("g-mesh", "train")])
        with mock.patch.object(SM, "node_texts", side_effect=AssertionError("read a snapshot")):
            with self.assertRaises(SystemExit):
                SM.build_calib_sample(corpora_toml=p, gm456=Path(self.tmp.name) / "none")

    def test_the_committed_corpora_file_passes(self):
        train, dev = SM.split_repos("train"), SM.split_repos("dev")
        self.assertFalse(set(train) & set(dev))
        self.assertFalse(set(train + dev) & set(P.CORPORA))


class SelectSample(unittest.TestCase):
    """The selection sample never holds a text of the frozen gate sample."""

    def test_rows_whose_text_is_in_the_gate_sample_are_dropped(self):
        gate = [r["text"] for r in P.load_sample(P.SAMPLE)]
        drawn = [{"id": "short:attrs:1", "text": "def fresh_text(self)"},
                 {"id": "hand:empty", "text": ""},
                 {"id": "short:attrs:2", "text": gate[0]},
                 {"id": "hand:seven", "text": gate[-1]},
                 {"id": "short:attrs:3", "text": "def other_fresh(self, o)"}]
        self.assertIn("", gate)
        with mock.patch.object(SM, "build", return_value=drawn) as build:
            out = SM.build_select_sample(gate_sample=P.SAMPLE)
        self.assertEqual(build.call_args.args[0], "dev")
        self.assertEqual([r["id"] for r in out], ["short:attrs:1", "short:attrs:3"])

    def test_the_committed_samples_are_disjoint(self):
        gate = {r["text"] for r in P.load_sample(P.SAMPLE)}
        select, calib = P.load_sample(SM.SELECT), P.load_sample(SM.CALIB)
        self.assertEqual(len(select), 160)
        self.assertEqual(len(calib), 256)
        self.assertFalse(gate & {r["text"] for r in select})
        self.assertFalse(gate & {r["text"] for r in calib})
        self.assertFalse({repo_of(r) for r in select} & {repo_of(r) for r in calib})
        self.assertTrue({repo_of(r) for r in select} <= set(SM.split_repos("dev")))
        self.assertTrue({repo_of(r) for r in calib} <= set(SM.split_repos("train")))


class Gate(unittest.TestCase):
    """T6: the gate's median floor is 0.99; GM-703's check keeps 0.98."""

    def check(self, cosine, **kw):
        e0 = np.zeros(4)
        e0[0] = 1.0
        v = np.asarray([cosine, np.sqrt(1 - cosine ** 2), 0, 0])
        ids = [[1, 2, 3]] * 5
        sample = [{"text": "x"}] * 5
        with mock.patch.object(P, "cls_vectors", return_value=[v] * 5), \
                mock.patch.object(P, "tokenizer"), mock.patch.object(P, "session"), \
                mock.patch.object(P, "mean_vectors", return_value=[e0] * 5):
            return P.check_qd(None, [e0] * 5, ids, sample, "jf", "ji", **kw)

    def test_a_0_9899_median_misses_the_gm736_gate(self):
        self.assertEqual(R.GATE_MIN_MEDIAN, 0.99)
        self.assertFalse(self.check(0.9899, min_median=R.GATE_MIN_MEDIAN)["ok"])
        self.assertTrue(self.check(0.9901, min_median=R.GATE_MIN_MEDIAN)["ok"])

    def test_gm703s_default_stays_0_98(self):
        self.assertEqual(P.QD_MIN_MEDIAN, 0.98)
        r = self.check(0.9899)
        self.assertTrue(r["ok"], r)
        self.assertEqual(r["min_median"], 0.98)

    def test_r2_aims_above_the_gate(self):
        self.assertAlmostEqual(R.R2_TARGET_MEDIAN, 0.992)


class Budget(unittest.TestCase):
    JINA_INT8 = P.WORK / "models" / "jina-v2-base-code-int8" / "model.onnx"

    def test_the_size_budget_is_the_jina_int8_bytes(self):
        self.assertEqual(R.SIZE_BUDGET, 161_895_621)

    @unittest.skipUnless(JINA_INT8.exists(), "no jina int8 model in the eval data dir")
    def test_the_budget_matches_the_file_on_disk(self):
        self.assertEqual(R.SIZE_BUDGET, self.JINA_INT8.stat().st_size)


class BestAlpha(unittest.TestCase):
    def test_the_highest_median_wins_and_a_tie_goes_to_the_smaller_alpha(self):
        rows = [{"alpha": 0.5, "median": 0.990}, {"alpha": 0.6, "median": 0.991}, {"alpha": 0.4, "median": 0.991}]
        self.assertEqual(R.best_alpha(rows), 0.4)
        self.assertEqual(R.best_alpha(rows[:2]), 0.6)


if __name__ == "__main__":
    unittest.main()
