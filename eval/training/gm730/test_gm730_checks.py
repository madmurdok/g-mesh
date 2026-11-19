"""Tests of budget.py (the pre-launch spend check, exit 3) and eval_checks.py (the pass rules
of C-0, C-S on the RR table, S5 and C-P2; each prints `FAILED` and exits 1 on a failure).
docs/design/GM-730-gpu-training-routes.md sections 2, 5 and 6. stdlib only (eval_checks
imports eval/embedding/recall_at_k.py).

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_checks.py'
"""

import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import budget as B  # noqa: E402
import eval_checks as E  # noqa: E402

CFG = {"max_attempts": 5, "root_volume_gb": 100, "ebs_gp3_usd_gb_month": 0.073}
RATE = 0.70


def worst(max_minutes, attempts=5):
    return (max_minutes + 5 * attempts) / 60 * (RATE + 100 * 0.073 / 730)


class BudgetCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runs = self.root / "runs"
        self.runs.mkdir()

    def job(self, name, minutes=120, scope='max_minutes_scope = "job"\n'):
        p = self.root / f"{name}.toml"
        p.write_text(f'name = "{name}"\nmax_minutes = {minutes}\n{scope}')
        return p

    def spend(self, run, usd):
        d = self.runs / run
        d.mkdir()
        (d / "cost.json").write_text(json.dumps({"totalUsd": usd}))

    def check(self, job):
        return B.check(job, self.runs, RATE, CFG)

    def main(self, job):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = B.main(["check", "--job", str(job), "--runs", str(self.runs), "--usd-per-hour", str(RATE)])
        return code, out.getvalue()


class Refusals(BudgetCase):
    def test_worst_case_is_the_lifetime_plus_boot_per_attempt_at_the_hourly_rate_and_ebs(self):
        self.assertAlmostEqual(B.worst_usd({"max_minutes": 120}, CFG, RATE), worst(120), places=9)
        self.assertAlmostEqual(B.worst_usd({"max_minutes": 120, "max_attempts": 2}, CFG, RATE), worst(120, 2), places=9)

    def test_within_every_cap_is_ok_and_exits_zero(self):
        self.spend("gm730-rr-teacher-1", 1.0)
        code, out = self.main(self.job("gm730-rr-grid"))
        self.assertEqual(code, 0, out)
        self.assertIn("ok", out.splitlines()[-1])

    def test_a_job_scope_other_than_job_is_refused(self):
        problems, info = self.check(self.job("gm730-rr-grid", scope=""))
        self.assertTrue(problems)
        self.assertIsNone(info)
        code, out = self.main(self.job("gm730-rr-grid", scope='max_minutes_scope = "attempt"\n'))
        self.assertEqual(code, B.REFUSED)
        self.assertEqual(B.REFUSED, 3)

    def test_route_cap(self):
        self.spend("gm730-rr-teacher-1", 4.0 - worst(120) + 0.01)
        problems, _ = self.check(self.job("gm730-rr-grid"))
        self.assertEqual(len(problems), 1)
        self.assertIn("route rr", problems[0])
        self.assertEqual(self.main(self.job("gm730-rr-grid"))[0], 3)

    def test_route_cap_is_met_exactly(self):
        self.spend("gm730-rr-teacher-1", 4.0 - worst(120))
        self.assertEqual(self.check(self.job("gm730-rr-grid"))[0], [])

    def test_total_cap_refuses_a_job_even_when_its_route_is_within_its_own(self):
        self.spend("gm730-rr-grid-1", 19.0)
        problems, _ = self.check(self.job("gm730-gte-grid", 60))
        self.assertEqual(len(problems), 2)
        self.assertTrue(any("S3 no new job" in p for p in problems))
        self.assertFalse(any("route gte" in p for p in problems))

    def test_gte_is_refused_over_fifteen_dollars_total_but_jina_is_not(self):
        self.spend("gm730-rr-grid-1", 14.5)
        gte, _ = self.check(self.job("gm730-gte-grid", 60))
        self.assertEqual([("R3" in p) for p in gte], [True])
        jina, _ = self.check(self.job("gm730-jina-grid", 60))
        self.assertEqual(jina, [])

    def test_a_run_without_cost_json_is_refused_as_unknown_spend(self):
        (self.runs / "gm730-rr-teacher-1").mkdir()
        problems, _ = self.check(self.job("gm730-rr-grid"))
        self.assertTrue(any("no cost.json" in p for p in problems))
        self.assertEqual(self.main(self.job("gm730-rr-grid"))[0], 3)

    def test_other_runs_are_not_counted(self):
        self.spend("gm730-rr-teacher-1", 1.0)
        (self.runs / "other-run-1").mkdir()
        by, total, problems = B.spent(self.runs)
        self.assertEqual((by["rr"], total, problems), (1.0, 1.0, []))

    def test_a_name_that_is_not_gm730_is_refused(self):
        with self.assertRaises(SystemExit):
            B.route_of("gm436-grid")
        with self.assertRaises(SystemExit):
            B.route_of("gm730-xx-grid")


def run_main(argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = E.main(argv)
    return code, out.getvalue()


class EvalChecksCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def write(self, name, obj):
        p = self.root / name
        p.write_text(obj if isinstance(obj, str) else json.dumps(obj))
        return p


class C0(EvalChecksCase):
    def report(self, a=0.608, b=0.380, c=0.655, d=0.423):
        arm = lambda r, m: {"recall@10": {"point": r}, "mrr": {"point": m}}
        return self.write("r.json", {"arms": {"jina-v2-base-code-int8-structured": arm(a, b),
                                              "int8-structured-adapter-full": arm(c, d)}})

    def test_reproduced_numbers_pass(self):
        code, out = run_main(["c0", "--report", str(self.report())])
        self.assertEqual(code, 0, out)
        self.assertNotIn("FAILED", out)

    def test_a_planted_offset_of_one_thousandth_fails(self):
        code, out = run_main(["c0", "--report", str(self.report(a=0.610))])
        self.assertEqual(code, 1)
        self.assertIn("FAILED", out)

    def test_a_missing_arm_fails(self):
        p = self.write("r.json", {"arms": {}})
        self.assertEqual(run_main(["c0", "--report", str(p)])[0], 1)


class CsRr(EvalChecksCase):
    def table(self, r10="-1.00 [-3.00, +0.50]", mrr="-0.010 [-0.030, +0.005]", arm="ce-gm730-rr-shuf", k=30):
        return self.write("t.md", "\n".join([
            f"## K={k}", "", "| b | a | subset | r@10 | MRR |", "|---|---|---|---|---|",
            f"| {arm} | ship-L6 | held-out NL | {r10} | {mrr} |",
            f"| {arm} | ship-L6 | other | +9.00 [+8.00, +9.50] | +0.9 [+0.8, +0.95] |"]) + "\n")

    def test_lower_bounds_at_or_below_zero_pass(self):
        code, out = run_main(["cs-rr", "--table", str(self.table()), "--arm", "ce-gm730-rr-shuf"])
        self.assertEqual(code, 0, out)

    def test_a_positive_lower_bound_fails_the_control(self):
        code, out = run_main(["cs-rr", "--table", str(self.table(mrr="+0.050 [+0.020, +0.080]")), "--arm", "ce-gm730-rr-shuf"])
        self.assertEqual(code, 1)
        self.assertIn("C-S rr MRR: FAILED", out)
        self.assertIn("C-S rr r@10: pass", out)

    def test_r10_is_read_in_points(self):
        code, out = run_main(["cs-rr", "--table", str(self.table(r10="+2.00 [+1.00, +3.00]")), "--arm", "ce-gm730-rr-shuf"])
        self.assertEqual(code, 1)
        self.assertIn("C-S rr r@10: FAILED", out)

    def test_no_row_for_the_arm_at_the_k_fails(self):
        code, out = run_main(["cs-rr", "--table", str(self.table()), "--arm", "nope"])
        self.assertEqual(code, 1)
        self.assertIn("no paired row", out)
        self.assertEqual(run_main(["cs-rr", "--table", str(self.table(k=10)), "--arm", "ce-gm730-rr-shuf"])[0], 1)

    def test_an_unparsable_cell_fails(self):
        code, out = run_main(["cs-rr", "--table", str(self.table(mrr="n/a")), "--arm", "ce-gm730-rr-shuf"])
        self.assertEqual(code, 1)
        self.assertIn("cannot parse", out)


class S5AndCp2(EvalChecksCase):
    def test_s5_passes_when_the_gain_lower_bound_is_not_positive(self):
        ok = self.write("a.json", {"gain": {"mrr": 0.01, "lower": 0.0, "upper": 0.02}})
        bad = self.write("b.json", {"gain": {"mrr": 0.05, "lower": 0.01, "upper": 0.09}})
        self.assertEqual(run_main(["s5", "--result", str(ok)])[0], 0)
        code, out = run_main(["s5", "--result", str(bad)])
        self.assertEqual(code, 1)
        self.assertIn("S5 dev: FAILED", out)

    def test_cp2_needs_every_export_to_record_a_pass(self):
        ok = self.write("a.json", {"cp2": {"check": "C-P2", "pass": True, "floor": 0.1}})
        bad = self.write("b.json", {"cp2": {"check": "C-P2", "pass": False}})
        none = self.write("c.json", {})
        self.assertEqual(run_main(["cp2", "--export", str(ok)])[0], 0)
        self.assertEqual(run_main(["cp2", "--export", str(ok), str(bad)])[0], 1)
        code, out = run_main(["cp2", "--export", str(none)])
        self.assertEqual(code, 1)
        self.assertIn("no cp2 record", out)

    def test_a_non_boolean_pass_is_not_a_pass(self):
        odd = self.write("a.json", {"cp2": {"pass": "yes"}})
        self.assertEqual(run_main(["cp2", "--export", str(odd)])[0], 1)


if __name__ == "__main__":
    unittest.main()
