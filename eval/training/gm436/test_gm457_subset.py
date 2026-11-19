"""The adapter job's data subset is sufficient (GM-457 design note
docs/design/GM-457-spot-gpu-training.md, section 4 behaviour 7, D4): the
trainer runs on a directory holding only the files the launcher's own
selection (gpu_train.data_manifest) picks for the job's [[data]] includes.

The include globs name the production version and variant; they are mapped
to the fixture's names. The fixture has no file of some kinds the production
version has (FIXTURE_LACKS); those globs are skipped, and the test fails if
the set of skipped globs changes. Needs torch (the GM-436 venv).

Run (GM-436 venv, from the repo root):
  ~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -m unittest discover \
      -s eval/training/gm436 -p 'test_gm457_*.py'
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent.parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(REPO / "eval" / "gpu"))
import gpu_train as G  # noqa: E402
from fixture_gm436 import VARIANT, VERSION, Fixture  # noqa: E402

try:
    import torch  # noqa: F401
    import cache as K
except ImportError:  # pragma: no cover
    torch = None

JOBS = [REPO / "eval/gpu/jobs/gm436-parity.toml", REPO / "eval/gpu/jobs/gm436-resume.toml"]
PROD_VERSION = "v0.4-full"
PROD_VARIANT = "gte-small-structured"
FIXTURE_LACKS = {"v0.4-full/manifest.json", "v0.4-full/targets.jsonl", "v0.4-full/texts/**"}
SMALL = ["--version", VERSION, "--base", VARIANT, "--epochs", "1", "--batch", "8", "--resamples", "100",
         "--threads", "1"]


def to_fixture(pattern):
    return pattern.replace(PROD_VERSION, VERSION).replace(PROD_VARIANT, VARIANT)


def select(root, include):
    """The launcher's file selection per glob; globs the fixture has no file for
    are returned separately (data_manifest refuses a glob that matches nothing)."""
    paths, skipped = set(), set()
    for pattern in include:
        try:
            m = G.data_manifest(root, [to_fixture(pattern)])
        except SystemExit:
            skipped.add(pattern)
            continue
        paths.update(f["path"] for f in m["files"])
    return paths, skipped


def trainer_step(job):
    """The job's first trainer step; the test runs its args on the CPU."""
    return next(s for s in job["steps"] if s["entry"].endswith("train_adapter.py"))


@unittest.skipIf(torch is None, "torch is not installed (GM-436 venv)")
class SubsetIsSufficient(unittest.TestCase):
    def check(self, job_path):
        job = G.load_job(job_path)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        src, dst = Path(tmp.name) / "full", Path(tmp.name) / "subset"
        fx = Fixture(src)
        env = {**os.environ, **fx.env()}
        old = {k: os.environ.get(k) for k in fx.env()}
        os.environ.update(fx.env())
        try:
            K.import_run(fx.mine, fx.vdir, VARIANT, "alpha")
        finally:
            for k, v in old.items():
                os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v)
        roots = {"GM456_DATA": fx.gm456, "GM436_DATA": fx.gm436}
        self.assertEqual({d["env"] for d in job["data"]}, set(roots))
        skipped_all = set()
        for d in job["data"]:
            paths, skipped = select(roots[d["env"]], d["include"])
            self.assertTrue(paths, d["name"])
            skipped_all |= skipped
            for rel in paths:
                out = dst / d["env"] / rel
                out.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(roots[d["env"]] / rel, out)
            env[d["env"]] = str(dst / d["env"])
        self.assertEqual(skipped_all, FIXTURE_LACKS)
        # The copy is not the whole fixture, and the source is gone.
        self.assertFalse((dst / "GM456_DATA" / "work" / "alpha.sqlite").exists())
        self.assertFalse(list((dst / "GM456_DATA").rglob("rankings.jsonl")))
        shutil.rmtree(src)
        env.pop("GM436_PROGRESS", None)
        out = Path(tmp.name) / "out"
        step = trainer_step(job)
        cmd = [sys.executable, str(REPO / step["entry"]), *step["args"], *SMALL, "--device", "cpu",
               "--out", str(out)]
        p = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=600)
        self.assertEqual(p.returncode, 0, p.stderr[-4000:])
        self.assertTrue((out / "result.json").is_file())

    def test_parity_job_subset(self):
        self.check(JOBS[0])

    def test_resume_job_subset(self):
        self.check(JOBS[1])


if __name__ == "__main__":
    unittest.main()
