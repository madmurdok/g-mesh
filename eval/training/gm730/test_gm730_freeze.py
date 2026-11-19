"""Tests of freeze.py (the dev winner, the S4 gates, the frozen final_configs.json and its
sha256 sidecar) and make_jobs.py (the grids and the final job's refusals; docs/design/
GM-730-gpu-training-routes.md sections 1, 6 and 8). stdlib only.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_freeze.py'
"""

import hashlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parents[1] / "gpu"))
import freeze as F  # noqa: E402
import make_jobs as MJ  # noqa: E402


def result(args, mrr, base=0.30, lower=0.01):
    return {"route": "x", "args": list(args), "best": {"epoch": 1, "mrr": mrr, "recall@10": 0.5},
            "base": {"mrr": base, "recall@10": 0.4}, "gain": {"mrr": mrr - base, "lower": lower, "upper": 0.1}}


class RunDir:
    """A pulled grid run: out/<config>/result.json for every config of the route's grid."""

    def __init__(self, root, route, mrr, teacher=True, **kw):
        self.dir = Path(root) / "run"
        for name, args in MJ.grid(route, teacher):
            p = self.dir / "out" / name / "result.json"
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(json.dumps(result(args, mrr.get(name, 0.2), **kw)))

    def write(self, name, obj):
        p = self.dir / "out" / name / "result.json"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(json.dumps(obj))


class Gates(unittest.TestCase):
    def test_best_is_the_highest_mrr_and_ties_go_to_the_name_sorting_first(self):
        runs = [{"name": "b", "mrr": 0.4}, {"name": "a", "mrr": 0.4}, {"name": "c", "mrr": 0.5}]
        self.assertEqual(F.best(runs)["name"], "c")
        self.assertEqual(F.best(runs[:2])["name"], "a")

    def test_s4_rules_per_route(self):
        r = result([], 0.5, lower=0.0)
        self.assertFalse(F.s4("rr", r)["pass"])
        self.assertTrue(F.s4("rr", dict(r, gain={"mrr": 0, "lower": 1e-6, "upper": 1}))["pass"])
        self.assertTrue(F.s4("jina", result([], 0.634, base=0.600))["pass"])
        self.assertFalse(F.s4("jina", result([], 0.633, base=0.600))["pass"])
        self.assertTrue(F.s4("gte", result([], 0.41))["pass"])
        self.assertFalse(F.s4("gte", result([], 0.409))["pass"])


class Select(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def test_picks_the_winner_with_the_grids_args(self):
        names = [n for n, _ in MJ.grid("gte")]
        run = RunDir(self.root, "gte", {names[3]: 0.45})
        e = F.select("gte", run.dir)
        self.assertEqual(e["name"], names[3])
        self.assertEqual(e["args"], dict(MJ.grid("gte"))[names[3]])
        self.assertTrue(e["s4"]["pass"])
        self.assertEqual([g["name"] for g in e["grid"]], sorted(names))

    def test_refuses_a_missing_config(self):
        run = RunDir(self.root, "gte", {})
        (run.dir / "out" / MJ.grid("gte")[0][0] / "result.json").unlink()
        with self.assertRaises(SystemExit) as cm:
            F.select("gte", run.dir)
        self.assertIn("incomplete or foreign", str(cm.exception))

    def test_refuses_an_extra_config(self):
        run = RunDir(self.root, "gte", {})
        run.write("lr9-a9", result(["--lr", "9"], 0.9))
        with self.assertRaises(SystemExit) as cm:
            F.select("gte", run.dir)
        self.assertIn("lr9-a9", str(cm.exception))

    def test_refuses_args_that_differ_from_the_grid(self):
        run = RunDir(self.root, "gte", {})
        name, args = MJ.grid("gte")[0]
        run.write(name, result(args + ["--bf16"], 0.3))
        with self.assertRaises(SystemExit) as cm:
            F.select("gte", run.dir)
        self.assertIn("differ from the grid", str(cm.exception))


class Freeze(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.final = self.root / "final_configs.json"

    def test_writes_final_configs_and_a_matching_sha256_sidecar(self):
        run = RunDir(self.root, "gte", {MJ.grid("gte")[0][0]: 0.5})
        entry, sha = F.freeze("gte", run.dir, self.final)
        self.assertEqual(sha, hashlib.sha256(self.final.read_bytes()).hexdigest())
        self.assertEqual(self.final.with_suffix(".sha256").read_text(), f"{sha}  final_configs.json\n")
        self.assertEqual(json.loads(self.final.read_text())["gte"]["name"], entry["name"])
        again = F.freeze("jina", RunDir(self.root / "j", "jina", {}).dir, self.final)[1]
        self.assertNotEqual(again, sha)
        self.assertEqual(set(json.loads(self.final.read_text())), {"gte", "jina"})

    def test_the_file_is_stable_for_the_same_input(self):
        run = RunDir(self.root, "gte", {MJ.grid("gte")[1][0]: 0.5})
        a = F.freeze("gte", run.dir, self.final)[1]
        self.final.unlink()
        self.final.with_suffix(".sha256").unlink()
        self.assertEqual(F.freeze("gte", run.dir, self.final)[1], a)

    def test_a_frozen_route_is_never_replaced(self):
        run = RunDir(self.root, "gte", {})
        F.freeze("gte", run.dir, self.final)
        before = self.final.read_bytes()
        with self.assertRaises(SystemExit) as cm:
            F.freeze("gte", run.dir, self.final)
        self.assertIn("already frozen", str(cm.exception))
        self.assertEqual(self.final.read_bytes(), before)


class Grids(unittest.TestCase):
    def test_sizes(self):
        self.assertEqual(len(MJ.grid("rr")), 8)
        g = MJ.grid("jina")
        self.assertEqual((len(g), sum(n.startswith("full") for n, _ in g), sum(n.startswith("lora") for n, _ in g)), (8, 4, 4))
        self.assertEqual(len(MJ.grid("gte")), 6)

    def test_lora_configs_carry_the_six_target_names(self):
        for name, args in MJ.grid("jina"):
            if name.startswith("lora"):
                self.assertEqual(args[args.index("--lora-targets") + 1].split(","),
                                 ["query", "key", "value", "dense", "up_gated_layer", "down_layer"])

    def test_without_the_teacher_every_config_has_alpha_zero(self):
        for route in ("jina", "gte"):
            g = MJ.grid(route, teacher=False)
            self.assertTrue(g)
            self.assertTrue(all(a[a.index("--alpha") + 1] == "0" for _, a in g))
        self.assertEqual(len(MJ.grid("gte", teacher=False)), 3)

    def test_rr_has_no_teacherless_form(self):
        with self.assertRaises(SystemExit):
            MJ.grid("rr", teacher=False)

    def test_unknown_route_is_refused(self):
        with self.assertRaises(SystemExit):
            MJ.grid("zz")


class FinalJob(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.final = self.root / "final_configs.json"

    def frozen(self, mrr):
        run = RunDir(self.root, "gte", {MJ.grid("gte")[0][0]: mrr})
        F.freeze("gte", run.dir, self.final)

    def test_final_job_names_the_frozen_config_and_loads_as_a_job(self):
        self.frozen(0.5)
        text = MJ.final_job("gte", self.final)
        path = MJ.write("gte-final", text, self.root / "jobs")
        import gpu_train
        job = gpu_train.load_job(path)
        self.assertEqual([s["name"] for s in job["steps"]], ["final", "shuffled"])
        self.assertEqual(job["max_minutes_scope"], "job")
        names = [d["env"] for d in job["data"]]
        self.assertEqual(len(names), len(set(names)))

    def test_refuses_a_file_whose_sha256_is_not_the_sidecars(self):
        self.frozen(0.5)
        self.final.write_text(self.final.read_text() + " ")
        with self.assertRaises(SystemExit) as cm:
            MJ.final_job("gte", self.final)
        self.assertIn("sha256", str(cm.exception))

    def test_refuses_a_route_that_is_not_frozen(self):
        self.frozen(0.5)
        with self.assertRaises(SystemExit) as cm:
            MJ.final_job("jina", self.final)
        self.assertIn("no frozen config", str(cm.exception))

    def test_refuses_a_route_that_failed_s4(self):
        self.frozen(0.2)
        with self.assertRaises(SystemExit) as cm:
            MJ.final_job("gte", self.final)
        self.assertIn("S4", str(cm.exception))

    def test_without_the_teacher_the_job_is_named_nt_and_has_no_teacher_data(self):
        for route in ("jina", "gte"):
            text = MJ.grid_job(route, teacher=False)
            self.assertIn(f'name = "gm730-{route}-grid-nt"', text)
            self.assertNotIn("GM730_TEACHER", text)
            self.assertIn("GM730_TEACHER", MJ.grid_job(route, teacher=True))

    def test_every_step_carries_the_job_steps_left_and_the_eta_limit(self):
        text = MJ.grid_job("gte")
        n = len(MJ.grid("gte"))
        self.assertEqual(text.count("--job-steps"), n)
        self.assertEqual(text.count("--eta-limit-minutes"), n)
        self.assertIn(f'"--job-steps", "{n}"', text)
        self.assertIn('"--job-steps", "1"', text)


if __name__ == "__main__":
    unittest.main()
