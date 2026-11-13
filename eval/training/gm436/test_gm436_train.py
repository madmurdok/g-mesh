"""Tests of the GM-436 adapter trainer (train_adapter.py; design note
docs/design/GM-436-laptop-training.md, section 4 R1/R2, section 8 behaviour
12) on the tiny fixture of fixture_gm436.py. Needs torch (the GM-436 venv).

Kill-and-resume runs the trainer as separate processes: `--stop-after-epochs`
exits 75 right after an epoch's checkpoint, and a fresh process resumes.

Run: python -m unittest discover -s eval/training/gm436 -p 'test_gm436_*.py'
"""

import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cache as K  # noqa: E402
import data as D  # noqa: E402
from fixture_gm436 import DIM, VARIANT, VERSION, Fixture  # noqa: E402

try:
    import torch  # noqa: F401
    import train_adapter as T
except ImportError:  # pragma: no cover
    T = None

COMMON = ["--version", VERSION, "--base", VARIANT, "--batch", "8", "--patience", "99", "--resamples", "100",
          "--threads", "1", "--no-train-metric"]
IDENTITY_SHA = None if T is None else T.w_sha256(np.eye(DIM, dtype=np.float32))


@unittest.skipIf(T is None, "torch is not installed (GM-436 venv)")
class TrainCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.fx = Fixture(self.tmp.name)
        env = mock.patch.dict(os.environ, self.fx.env())
        env.start()
        self.addCleanup(env.stop)
        K.import_run(self.fx.mine, self.fx.vdir, VARIANT, "alpha")
        self.root = Path(self.tmp.name)

    def run_trainer(self, out, *args):
        return subprocess.run([sys.executable, str(HERE / "train_adapter.py"), *COMMON, "--out", str(out), *args],
                              env={**os.environ, **self.fx.env()}, capture_output=True, text=True, timeout=600)

    def result(self, out):
        return json.loads((Path(out) / "result.json").read_text())


class KillAndResume(TrainCase):
    """Behaviour 12: a run stopped at a checkpoint and resumed ends with the same W."""

    def check_form(self, *form):
        whole, cut = self.root / "whole", self.root / "cut"
        p = self.run_trainer(whole, *form, "--epochs", "5")
        self.assertEqual(p.returncode, 0, p.stderr)
        p = self.run_trainer(cut, *form, "--epochs", "5", "--stop-after-epochs", "2")
        self.assertEqual(p.returncode, T.KILLED_EXIT, p.stderr)
        self.assertFalse((cut / "result.json").exists())
        p = self.run_trainer(cut, *form, "--epochs", "5")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("resumed after epoch 2", p.stdout)
        a, b = self.result(whole), self.result(cut)
        self.assertEqual(len(a["history"]), 6)
        self.assertNotEqual(a["lastWSha256"], IDENTITY_SHA, "training did not move W")
        self.assertEqual(b["lastWSha256"], a["lastWSha256"])
        self.assertEqual(b["wSha256"], a["wSha256"])
        self.assertEqual(b["history"], a["history"])

    def test_full_form(self):
        self.check_form("--form", "full", "--lam", "0.001")

    def test_lowrank_form(self):
        self.check_form("--form", "lowrank", "--rank", "4")

    def test_a_checkpoint_of_another_config_is_refused(self):
        out = self.root / "other"
        p = self.run_trainer(out, "--form", "full", "--epochs", "4", "--stop-after-epochs", "1")
        self.assertEqual(p.returncode, T.KILLED_EXIT, p.stderr)
        p = self.run_trainer(out, "--form", "full", "--epochs", "4", "--lr", "0.01")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("written for config", p.stderr)


class R2Temperatures(TrainCase):
    """Section 4 R2: the KL term runs at tau_s = tau_t = 0.05 whatever --tau is;
    --tau drives only the alpha * InfoNCE term, which alpha = 0 removes."""

    def setUp(self):
        super().setUp()
        nt, qt = K.tables_for(self.fx.mine, VARIANT, "alpha")
        queries = D.load_queries(self.fx.vdir)
        self.teacher = K.build_teacher(nt, qt, queries, VERSION, "q: ", self.root / "teacher", k=5)

    def train(self, name, *args):
        a = T.parse([*COMMON, "--route", "r2", "--alpha", "0", "--teacher-dir", str(self.teacher), "--form", "full",
                     "--epochs", "3", "--out", str(self.root / name), *args])
        with contextlib.redirect_stdout(io.StringIO()):
            return T.train(a)

    def test_tau_does_not_change_an_alpha_0_r2_run(self):
        a = self.train("tau02", "--tau", "0.02")
        b = self.train("tau05", "--tau", "0.05")
        self.assertEqual(a["config"]["tau_s"], 0.05)
        self.assertNotEqual(a["lastWSha256"], IDENTITY_SHA, "training did not move W")
        self.assertEqual(a["lastWSha256"], b["lastWSha256"])
        # The KL temperature itself does change the run, so the equality above is not vacuous.
        c = self.train("taus02", "--tau", "0.05", "--tau-s", "0.02")
        self.assertNotEqual(c["lastWSha256"], b["lastWSha256"])


if __name__ == "__main__":
    unittest.main()
