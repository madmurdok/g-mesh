"""GM-703 measure pipeline tests (docs/design/GM-703-coderankembed.md, sections 7, 9, 11, 11.2):
measure.sh, measure_costs.sh and report_gm436.py, run against fakes and dry runs.
No model, no cargo build, no eval run: every "binary" is a stub script.

Run: python -m unittest discover -s eval/embedding/gm703 -p 'test_gm703_measure.py'
"""

import hashlib
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
MEASURE = HERE / "measure.sh"
COSTS = REPO / "eval" / "embedding" / "measure_costs.sh"
FP32 = "coderankembed-fp32-structured"
INT8 = "coderankembed-int8-structured"


def stub(path, body):
    Path(path).write_text("#!/bin/bash\n" + body)
    Path(path).chmod(Path(path).stat().st_mode | stat.S_IXUSR)
    return str(path)


class Sandbox(unittest.TestCase):
    """measure.sh copied into a throwaway repo layout, so its `v2/work` symlink
    and everything else it writes stay out of the checkout."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="gm703-measure-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        gm = self.tmp / "repo" / "eval" / "embedding" / "gm703"
        gm.mkdir(parents=True)
        (self.tmp / "repo" / "eval" / "embedding" / "v2").mkdir()
        (self.tmp / "eval" / "work").mkdir(parents=True)  # EVAL_DATA/work, target of the v2/work link
        subprocess.run(["git", "init", "-q", str(self.tmp / "repo")], check=True)
        shutil.copyfile(MEASURE, gm / "measure.sh")
        self.script = gm / "measure.sh"
        self.marker = self.tmp / "ran"
        # a "python" and a "g-mesh" that only record that they were run
        self.py = stub(self.tmp / "py", f'echo "py $*" >> {self.marker}\nexit "${{PY_RC:-0}}"\n')
        self.bin = stub(self.tmp / "gmesh", f'echo "bin $*" >> {self.marker}\n')
        self.out = self.tmp / "out"

    def run_measure(self, *args, **env):
        e = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.tmp),
            "GM703_CAFFEINATED": "1",
            "GM703_DATA": str(self.tmp / "data"),
            "EVAL_DATA": str(self.tmp / "eval"),
            "OUT": str(self.out),
            "PY": self.py,
            "GMESH_BIN": self.bin,
        }
        e.update(env)
        return subprocess.run(["bash", str(self.script), *args], env=e, capture_output=True, text=True, timeout=120)

    def summary(self):
        return (self.out / "summary.txt").read_text()


class CheckExport(Sandbox):
    """T6: model.onnx must be the file EXPORT.json records."""

    def model_dir(self, recorded):
        d = self.tmp / "model"
        d.mkdir()
        (d / "model.onnx").write_bytes(b"not really onnx")
        (d / "EXPORT.json").write_text('{\n  "model_onnx_sha256": "%s",\n  "precision": "fp32"\n}\n' % recorded)
        return d

    def test_accepts_the_recorded_sha(self):
        d = self.model_dir(hashlib.sha256(b"not really onnx").hexdigest())
        r = self.run_measure("--check-export", str(d))
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_refuses_a_different_sha(self):
        d = self.model_dir(hashlib.sha256(b"an older export").hexdigest())
        r = self.run_measure("--check-export", str(d))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("sha256", r.stderr)

    def test_refuses_missing_files_and_an_empty_record(self):
        d = self.model_dir("")
        self.assertNotEqual(self.run_measure("--check-export", str(d)).returncode, 0)
        (d / "EXPORT.json").unlink()
        self.assertNotEqual(self.run_measure("--check-export", str(d)).returncode, 0)

    def test_step_4_runs_no_eval_on_a_stale_export(self):
        """The check guards the runs: a mismatched model dir stops step 4 before any g-mesh run."""
        models = self.tmp / "data" / "models" / "coderankembed-fp32"
        models.mkdir(parents=True)
        (models / "model.onnx").write_bytes(b"new")
        (models / "EXPORT.json").write_text('{"model_onnx_sha256": "%s"}\n' % ("0" * 64))
        r = self.run_measure("--from", "4", "--until", "4")
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(self.marker.exists() and "debug-embed-eval run" in self.marker.read_text())


class Arms(Sandbox):
    """T9, T10: which arm a parity failure stops, and which arms exist."""

    def test_t10_an_unknown_arm_is_refused(self):
        r = self.run_measure(ARM="jina-v2-base-code-int8-structured")
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertFalse(self.marker.exists())

    def test_both_coderankembed_arms_are_accepted(self):
        for arm in (FP32, INT8):
            r = self.run_measure("--from", "9", "--until", "9", ARM=arm, DRY="1")
            self.assertEqual(r.returncode, 0, (arm, r.stderr))

    def test_t9_qf_qd_failure_stops_the_int8_arm(self):
        r = self.run_measure("--from", "2", "--until", "2", ARM=INT8, PY_RC="3")
        self.assertEqual(r.returncode, 3, r.stderr)
        self.assertIn("STOP S2", self.summary())

    def test_t9_qf_qd_failure_does_not_stop_the_fp32_arm(self):
        r = self.run_measure("--from", "2", "--until", "2", ARM=FP32, PY_RC="3")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertNotIn("STOP", self.summary())

    def test_a_p1_failure_stops_the_fp32_arm(self):
        r = self.run_measure("--from", "2", "--until", "2", ARM=FP32, PY_RC="2")
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("STOP S1", self.summary())

    def test_a_p1_failure_stops_the_int8_arm(self):
        r = self.run_measure("--from", "2", "--until", "2", ARM=INT8, PY_RC="2")
        self.assertEqual(r.returncode, 2, r.stderr)
        self.assertIn("STOP S1", self.summary())


class DryRun(Sandbox):
    def test_t11_dry_runs_log_every_step_and_run_nothing(self):
        # step 4 still checks the export on disk (a check, not a command)
        models = self.tmp / "data" / "models" / "coderankembed-fp32"
        models.mkdir(parents=True)
        (models / "model.onnx").write_bytes(b"x")
        (models / "EXPORT.json").write_text('{"model_onnx_sha256": "%s"}\n' % hashlib.sha256(b"x").hexdigest())
        r = self.run_measure(DRY="1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(self.marker.exists(), "DRY=1 ran a command: " + (self.marker.read_text() if self.marker.exists() else ""))
        log = self.summary()
        for step in ("s0-fetch", "s1-export", "s2-parity", "s3-build", "s4-run-excalidraw", "s5-p3-gin", "s6-report", "s8-costs", "s9-report"):
            self.assertIn(f"DRY {step}", log, step)

    def test_the_cost_pass_is_given_the_arms_binary(self):
        """measure_costs.sh would otherwise time target/release/g-mesh of the checkout."""
        r = self.run_measure("--from", "8", "--until", "8", DRY="1", GMESH_BIN="/some/where/g-mesh")
        self.assertEqual(r.returncode, 0, r.stderr)
        line = next(x for x in self.summary().splitlines() if "DRY s8-costs" in x)
        self.assertIn("GMESH_BIN=/some/where/g-mesh", line)
        self.assertIn(f"VARIANTS={FP32}", line)
        self.assertIn("REF=jina-v2-base-code-int8-structured", line)


class CostsScript(unittest.TestCase):
    """measure_costs.sh with a stub binary: which binary it runs, and when it
    scores against another reference."""

    def run_costs(self, **env):
        tmp = Path(tempfile.mkdtemp(prefix="gm703-costs-"))
        self.addCleanup(shutil.rmtree, tmp, True)
        calls = tmp / "calls"
        fake = stub(tmp / "gmesh", f'echo "$*" >> {calls}\n')
        runs = tmp / "runs"
        ref = env.get("REF", "jina-v2-base-code-fp32")
        for v in {ref, "cand"}:
            d = runs / v / "g-mesh"
            d.mkdir(parents=True)
            (d / "manifest.json").write_text("{}")
            (d / "vectors.bin").write_bytes(b"")
        e = {
            "PATH": os.environ["PATH"],
            "HOME": str(tmp),
            "GMESH_BIN": fake,
            "OUT": str(tmp / "out"),
            "RUNS": str(runs),
            "VARIANTS": "cand",
            "SKIP_QUALITY": "1",
            "NOWAIT": "1",
            "ROUNDS": "1",
            "CONTROLS": "random",
        }
        e.update(env)
        subprocess.run(["bash", str(COSTS)], env=e, capture_output=True, text=True, timeout=120, cwd=tmp)
        log = tmp / "out" / "log.txt"
        self.log = log.read_text() if log.exists() else ""
        return calls.read_text().splitlines() if calls.exists() else []

    def report_line(self, calls):
        reports = [c for c in calls if c.startswith("debug-embed-eval report")]
        self.assertEqual(len(reports), 1, calls)
        return reports[0]

    def test_it_runs_the_binary_it_is_given(self):
        calls = self.run_costs()
        self.assertTrue(any(c.startswith("debug-embed-eval run") for c in calls), calls)
        self.assertTrue(any(c.startswith("debug-embed-eval report") for c in calls), calls)

    def test_t8_a_non_default_reference_is_passed_to_the_report(self):
        line = self.report_line(self.run_costs(REF="jina-int8-structured"))
        self.assertIn("--reference jina-int8-structured", line)

    def test_t8_the_default_reference_is_not_passed(self):
        line = self.report_line(self.run_costs())
        self.assertNotIn("--reference", line)

    def test_eval_dir_and_controls_reach_the_report_and_the_runs(self):
        calls = self.run_costs(EVAL_DIR="eval/embedding/v2")
        self.assertTrue(all("--eval-dir eval/embedding/v2" in c for c in calls), calls)
        line = self.report_line(calls)
        self.assertIn("/random ", line + " ")
        self.assertNotIn("bm25", line)

    def test_skip_quality_skips_the_quality_runs(self):
        self.run_costs()
        self.assertIn("quality runs skipped", self.log)
        self.assertNotIn("quality cand", self.log)


class ReportGm436(unittest.TestCase):
    """--reference reaches the report command, ahead of the arm run dirs."""

    def first_command(self, *extra):
        sys.path.insert(0, str(REPO / "eval" / "training" / "gm436"))
        try:
            import report_gm436 as R
        except ImportError as e:  # numpy via recall_at_k
            self.skipTest(f"needs the gm703 venv: {e}")
        seen = []

        class Stop(Exception):
            pass

        def fake_run(cmd):
            seen.append([str(c) for c in cmd])
            raise Stop

        old_run, old_argv = R.run, sys.argv
        R.run = fake_run
        out = tempfile.mkdtemp(prefix="gm703-report-")
        self.addCleanup(shutil.rmtree, out, True)
        sys.argv = ["report_gm436.py", "--bin", "/b/g-mesh", "--runs", "/r", "--arm", "a1", "--arm", "a2", "--out", out, *extra]
        try:
            with self.assertRaises(Stop):
                R.main()
        finally:
            R.run, sys.argv = old_run, old_argv
        return seen[0]

    def test_reference_is_passed_through(self):
        cmd = self.first_command("--reference", "jina-int8")
        i = cmd.index("--reference")
        self.assertEqual(cmd[i + 1], "jina-int8")
        self.assertLess(i, cmd.index("/r/a1"))

    def test_without_it_the_evals_own_reference_is_used(self):
        self.assertNotIn("--reference", self.first_command())


if __name__ == "__main__":
    unittest.main()
