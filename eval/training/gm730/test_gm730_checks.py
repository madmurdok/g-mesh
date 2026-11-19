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


class FamilyPricing(BudgetCase):
    CFGF = dict(CFG, instance_types=["g5.xlarge", "g5.2xlarge"])

    def fjob(self, name, types, minutes=100):
        p = self.root / f"{name}.toml"
        t = "" if types is None else "instance_types = [" + ", ".join(f'"{x}"' for x in types) + "]\n"
        p.write_text(f'name = "{name}"\nmax_minutes = {minutes}\nmax_minutes_scope = "job"\n{t}')
        return p

    def worst(self, name, types, minutes=100):
        problems, info = B.check(self.fjob(name, types, minutes), self.runs, None, self.CFGF)
        return problems, info

    def test_planning_prices_per_family(self):
        self.assertEqual(B.USD_PER_HOUR_BY_FAMILY, {"g5": 0.75, "g6e": 2.0, "g6": 0.70})

    def test_a_g6e_job_costs_more_than_the_same_g5_job(self):
        _, g5 = self.worst("gm730-gte-grid", ["g5.xlarge"])
        _, g6e = self.worst("gm730-gte-grid-g6e", ["g6e.xlarge", "g6e.2xlarge"])
        self.assertEqual((g5["family"], g5["usdPerHour"]), ("g5", 0.75))
        self.assertEqual((g6e["family"], g6e["usdPerHour"]), ("g6e", 2.0))
        self.assertAlmostEqual(g6e["worstUsd"], B.worst_usd({"max_minutes": 100}, self.CFGF, 2.0), places=9)
        self.assertGreater(g6e["worstUsd"], g5["worstUsd"] * 2)

    def test_a_job_without_instance_types_is_priced_as_the_config_family(self):
        _, none = self.worst("gm730-gte-grid", None)
        _, g5 = self.worst("gm730-gte-grid", ["g5.xlarge"])
        self.assertEqual(none["family"], "g5")
        self.assertAlmostEqual(none["worstUsd"], g5["worstUsd"], places=9)

    def test_usd_per_hour_overrides_the_family_price(self):
        j = self.fjob("gm730-gte-grid-g6e", ["g6e.xlarge"])
        _, info = B.check(j, self.runs, 0.5, self.CFGF)
        self.assertIsNone(info["family"])
        self.assertAlmostEqual(info["worstUsd"], B.worst_usd({"max_minutes": 100}, self.CFGF, 0.5), places=9)

    def test_route_cap_refuses_rr_grid_on_g6e_at_zero_spend(self):
        # rr-grid on g6e: 114 min + 5 x 5 boot at $2/h + EBS = $4.66 > the $4 rr cap
        problems, info = self.worst("gm730-rr-grid-g6e", ["g6e.xlarge", "g6e.2xlarge"], 114)
        self.assertEqual(info["spentTotal"], 0)
        self.assertAlmostEqual(info["worstUsd"], 4.656, places=2)
        self.assertEqual(len(problems), 1)
        self.assertIn("route rr", problems[0])
        # the same job on g5 and on g6 stays within the cap
        for name, types, minutes in (("gm730-rr-grid", ["g5.xlarge"], 190), ("gm730-rr-grid-g6", ["g6.xlarge"], 304)):
            problems, _ = self.worst(name, types, minutes)
            self.assertEqual(problems, [], name)

    def test_a_job_mixing_families_is_refused(self):
        with self.assertRaises(SystemExit) as cm:
            self.worst("gm730-gte-grid", ["g5.xlarge", "g6e.xlarge"])
        self.assertIn("one GPU family", str(cm.exception))

    def test_an_unpriced_family_is_refused_unless_a_price_is_given(self):
        j = self.fjob("gm730-gte-grid", ["g7.xlarge"])
        problems, info = B.check(j, self.runs, None, self.CFGF)
        self.assertIsNone(info)
        self.assertIn("no planning price for GPU family g7", problems[0])
        problems, info = B.check(j, self.runs, 1.0, self.CFGF)
        self.assertEqual(problems, [])


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


class V2Runs(EvalChecksCase):
    """Fake v2 run dirs: runs/<arm>/<corpus>/{manifest.json, rankings.jsonl} over authored
    positive queries of one corpus (the `nl` stratum), as recall_at_k.load_arm reads them."""

    N = 40

    def setUp(self):
        super().setUp()
        self.eval = self.root / "eval"
        (self.eval / "queries").mkdir(parents=True)
        rows = "".join(json.dumps({"id": f"q{i}", "kind": "positive", "corpus": "c", "language": "rust"}) + "\n"
                       for i in range(self.N))
        (self.eval / "queries" / "c.jsonl").write_text(rows)
        sha = __import__("hashlib").sha256(rows.encode()).hexdigest()
        self.manifest = {"corpus": "c", "queryFiles": [["queries/c.jsonl", sha]]}
        self.runs = self.root / "runs"

    def arm(self, name, rank=lambda i: 1, order=lambda i: 0, queries=None):
        """rank(i): 1-based rank of query i's expected hit; order(i): a rotation of the other hits."""
        d = self.runs / name / "c"
        d.mkdir(parents=True)
        (d / "manifest.json").write_text(json.dumps(self.manifest))
        lines = []
        for i in range(self.N) if queries is None else queries:
            r = rank(i)
            others = [f"x{(j + order(i)) % 10}" for j in range(11)]
            hits = others[:r - 1] + ["want"] + others[r - 1:]
            lines.append(json.dumps({"id": f"q{i}", "hits": [[h, 1.0] for h in hits], "expected": ["want"]}))
        (d / "rankings.jsonl").write_text("\n".join(lines) + "\n")

    def check(self, cmd, arm, other):
        return run_main([cmd, "--runs", str(self.runs), "--eval-dir", str(self.eval), "--arm", arm,
                         "--official" if cmd == "cr2" else "--base", other])


class CsRule(V2Runs):
    def test_a_shuffled_arm_that_beats_its_base_fails(self):
        self.arm("shuf", rank=lambda i: 1)
        self.arm("base", rank=lambda i: 5)
        code, out = self.check("cs", "shuf", "base")
        self.assertEqual(code, 1)
        self.assertIn("C-S MRR: FAILED", out)
        self.assertIn("C-S r@10: pass", out)  # both reach r@10 = 1
        self.assertIn("FAILED: cs", out)

    def test_a_shuffled_arm_that_beats_the_base_in_recall_fails_that_metric(self):
        self.arm("shuf", rank=lambda i: 3)
        self.arm("base", rank=lambda i: 20)
        code, out = self.check("cs", "shuf", "base")
        self.assertEqual(code, 1)
        self.assertIn("C-S r@10: FAILED", out)

    def test_an_equal_or_worse_arm_passes(self):
        self.arm("same", rank=lambda i: 2)
        self.arm("base", rank=lambda i: 2)
        self.arm("worse", rank=lambda i: 9)
        self.assertEqual(self.check("cs", "same", "base")[0], 0)
        code, out = self.check("cs", "worse", "base")
        self.assertEqual(code, 0, out)
        self.assertNotIn("FAILED", out)

    def test_arms_over_different_queries_fail(self):
        self.arm("shuf", queries=range(self.N - 1))
        self.arm("base")
        code, out = self.check("cs", "shuf", "base")
        self.assertEqual(code, 1)
        self.assertIn("different queries", out)


class Cr2Rule(V2Runs):
    def test_an_identical_requant_passes_all_three_rules(self):
        self.arm("ours")
        self.arm("official")
        code, out = self.check("cr2", "ours", "official")
        self.assertEqual(code, 0, out)
        self.assertEqual(out.count(": pass"), 3)

    def test_metrics_outside_the_tolerance_fail(self):
        self.arm("ours", rank=lambda i: 2)
        self.arm("official", rank=lambda i: 1)
        code, out = self.check("cr2", "ours", "official")
        self.assertEqual(code, 1)
        self.assertIn("C-R2 MRR: FAILED", out)
        self.assertIn("FAILED: cr2", out)

    def test_one_query_moving_from_rank_1_to_2_exceeds_the_mrr_tolerance(self):
        # |d MRR| = 0.5 / 40 = 0.0125 > 0.002
        self.arm("ours", rank=lambda i: 2 if i == 0 else 1)
        self.arm("official")
        self.assertIn("C-R2 MRR: FAILED", self.check("cr2", "ours", "official")[1])

    def test_equal_metrics_but_other_top_10_lists_fail_the_top_10_rule(self):
        self.arm("ours", order=lambda i: 3)
        self.arm("official", order=lambda i: 0)
        code, out = self.check("cr2", "ours", "official")
        self.assertEqual(code, 1)
        self.assertIn("C-R2 r@10: pass", out)
        self.assertIn("C-R2 MRR: pass", out)
        self.assertIn("C-R2 top-10: FAILED", out)

    def test_top_10_may_differ_on_one_percent_of_the_queries(self):
        self.N = 100
        self.setUp()
        self.arm("ours", order=lambda i: 3 if i == 0 else 0)
        self.arm("official")
        code, out = self.check("cr2", "ours", "official")
        self.assertEqual(code, 0, out)
        self.assertIn("C-R2 top-10: pass", out)

    def test_different_query_sets_fail(self):
        self.arm("ours", queries=range(self.N - 1))
        self.arm("official")
        code, out = self.check("cr2", "ours", "official")
        self.assertEqual(code, 1)
        self.assertIn("query sets differ", out)


if __name__ == "__main__":
    unittest.main()
