"""Tests of the GM-436 adapter export and the measure pipeline's wiring
(export_adapter.py, grid.py, run_measure.sh; design note
docs/design/GM-436-laptop-training.md, section 4 R2, section 8 behaviour 7).

The fixture in fixtures/w_roundtrip is what `write_w` and `apply` produce for
the W and queries below; core/src/embedding/adapter.rs reads the same files
and must reproduce `expected.f32` (the Rust half of the round trip).

Run: python -m unittest discover -s eval/training/gm436 -p 'test_gm436_*.py'
"""

import hashlib
import re
import sys
import tempfile
import unittest
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import export_adapter as E  # noqa: E402
import grid as G  # noqa: E402

FIXTURE = HERE / "fixtures" / "w_roundtrip"
W = np.array([[0.5, -1.25, 2.0], [0.75, 0.0, -0.5], [1.5, 0.25, -2.0]], np.float32)
Q = np.array([[1.0, 0.0, 0.0], [0.1, -0.2, 0.3], [-0.7, 0.4, 0.9]], np.float32)


class RoundTrip(unittest.TestCase):
    """Behaviour 7, Python half."""

    def test_write_w_produces_the_fixture_rust_reads(self):
        with tempfile.TemporaryDirectory() as tmp:
            path, sha = E.write_w(W, tmp)
            self.assertEqual(path.read_bytes(), (FIXTURE / "W.f32").read_bytes())
            self.assertEqual(sha, hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual((Path(tmp) / "W.f32.sha256").read_text(), (FIXTURE / "W.f32.sha256").read_text())

    def test_the_file_is_row_major_little_endian(self):
        data = (FIXTURE / "W.f32").read_bytes()
        self.assertEqual(len(data), 9 * 4)
        self.assertEqual(np.frombuffer(data, "<f4")[:3].tolist(), W[0].tolist())

    def test_apply_is_w_times_q_and_matches_the_fixture(self):
        expected = np.frombuffer((FIXTURE / "expected.f32").read_bytes(), "<f4").reshape(3, 3)
        self.assertEqual(np.frombuffer((FIXTURE / "q.f32").read_bytes(), "<f4").reshape(3, 3).tolist(), Q.tolist())
        for q, want in zip(Q, expected):
            got = E.apply(W, q)
            np.testing.assert_allclose(got, want, atol=1e-6)
            np.testing.assert_allclose(got, [sum(float(W[i, j]) * float(q[j]) for j in range(3)) for i in range(3)], atol=1e-6)

    def test_load_w_reads_back_and_refuses_a_wrong_sha_or_size(self):
        sha = (FIXTURE / "W.f32.sha256").read_text().split()[0]
        self.assertEqual(E.load_w(FIXTURE / "W.f32", sha).tolist(), W.tolist())
        with self.assertRaisesRegex(ValueError, "sha256"):
            E.load_w(FIXTURE / "W.f32", "0" * 64)
        with tempfile.TemporaryDirectory() as tmp:
            bad = Path(tmp) / "W.f32"
            for n in (3, 5):
                bad.write_bytes(np.zeros(n, "<f4").tobytes())
                with self.assertRaisesRegex(ValueError, "not a square"):
                    E.load_w(bad)
        with self.assertRaises(ValueError):
            E.w_bytes(np.zeros((2, 3)))


class MeasureWiring(unittest.TestCase):
    def test_r2_runs_fix_both_kl_temperatures_at_0_05(self):
        # Note section 4 R2: tau_t = tau_s = 0.05 whatever --tau the R1 form carries.
        form = ["--form", "full", "--tau", "0.02", "--lam", "0.0"]
        for name, args in G.r2_grid(form):
            line = " ".join(args)
            self.assertIn("--tau-s 0.05", line, name)
            self.assertIn("--tau-t 0.05", line, name)

    def test_the_curve_100_point_is_the_final_frozen_run(self):
        # Section 4 "Learning curve": the same frozen config at every point; the
        # 100% point is the final (C5) run beside curve-0.25 and curve-0.5.
        text = (HERE / "run_measure.sh").read_text()
        calls = re.findall(r'dev_metric\.py" curve --r25 "\$OUT/final/\$v/curve-0\.25".*?--r100 "([^"]+)"', text, re.S)
        self.assertEqual(calls, ["$OUT/final/$v/c5"])


if __name__ == "__main__":
    unittest.main()
