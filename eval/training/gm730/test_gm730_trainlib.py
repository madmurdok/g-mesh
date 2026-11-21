"""Tests of the shared trainer code (trainlib.py; docs/design/GM-730-gpu-training-routes.md
section 8): the windows and teacher loaders' refusals, the run identity, the batch and
warm-up rules, the dev ranks, result.json, the checkpoint contract and the job ETA gate.
Tiny fixtures (gm730_fixture.py), no download. Needs torch (trainlib imports the GM-457
checkpoint module); skipped without it.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_trainlib.py'
"""

import argparse
import hashlib
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
try:
    import torch
    import trainlib as L
except ImportError:  # pragma: no cover
    torch = L = None
import gm730_fixture as GF  # noqa: E402
import make_jobs as MJ  # noqa: E402
import windows as W  # noqa: E402

needs_torch = unittest.skipIf(L is None, "torch is not installed (GM-730 venv)")


@needs_torch
class WorldCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.world = GF.World(self.tmp.name)
        self.world.activate(self)
        self.splits = self.world.write_splits()
        self.world.write_teacher(self.splits)
        self.train = self.splits["train"]

    def split_dir(self, split="train"):
        return self.world.windows_root / split

    def edit_manifest(self, split="train", **kw):
        p = self.split_dir(split) / "manifest.json"
        m = json.loads(p.read_text())
        m.update(kw)
        p.write_text(json.dumps(m))
        return m

    def teacher_meta(self):
        return json.loads((self.world.teacher_root / "teacher.json").read_text())

    def edit_teacher(self, fn):
        p = self.world.teacher_root / "teacher.json"
        m = json.loads(p.read_text())
        fn(m)
        p.write_text(json.dumps(m))


class PairsSha(WorldCase):
    def test_equals_windows_py_over_the_same_windows(self):
        self.assertEqual(L.pairs_sha256(self.train), W.pairs_sha256(self.train))
        self.assertNotEqual(L.pairs_sha256(self.train), L.pairs_sha256(self.train[::-1]))


class LoadSplit(WorldCase):
    def test_loads_windows_texts_and_manifest(self):
        ws, texts, man = L.load_split("train")
        self.assertEqual([w["id"] for w in ws], [w["id"] for w in self.train])
        self.assertEqual(len(texts), self.world.nodes)
        self.assertTrue(man["joinCheckPass"])

    def test_refuses_a_file_whose_sha256_differs_from_the_manifest(self):
        p = self.split_dir() / "texts.jsonl"
        p.write_text(p.read_text() + "\n")
        with self.assertRaises(SystemExit) as cm:
            L.load_split("train")
        self.assertIn("sha256", str(cm.exception))

    def test_refuses_windows_whose_pair_order_differs_from_pairs_sha256(self):
        d = self.split_dir()
        rows = (d / "windows.jsonl").read_text().splitlines()
        rows[0], rows[1] = rows[1], rows[0]
        (d / "windows.jsonl").write_text("\n".join(rows) + "\n")
        man = json.loads((d / "manifest.json").read_text())
        man["files"]["windows.jsonl"] = hashlib.sha256((d / "windows.jsonl").read_bytes()).hexdigest()
        (d / "manifest.json").write_text(json.dumps(man))
        with self.assertRaises(SystemExit) as cm:
            L.load_split("train")
        self.assertIn("pair order", str(cm.exception))

    def test_refuses_a_manifest_whose_join_check_did_not_pass(self):
        self.edit_manifest(joinCheckPass=False)
        with self.assertRaises(SystemExit):
            L.load_split("train")

    def test_refuses_a_manifest_built_with_the_planted_offset(self):
        self.edit_manifest(plantOffset=True)
        with self.assertRaises(SystemExit):
            L.load_split("train")

    def test_a_manifest_without_the_join_check_keys_is_refused(self):
        p = self.split_dir() / "manifest.json"
        m = json.loads(p.read_text())
        del m["joinCheckPass"]
        p.write_text(json.dumps(m))
        with self.assertRaises(SystemExit):
            L.load_split("train")

    def test_a_smoke_manifest_needs_no_join_check(self):
        p = self.split_dir() / "manifest.json"
        m = json.loads(p.read_text())
        del m["joinCheckPass"], m["plantOffset"]
        m["smoke"] = True
        p.write_text(json.dumps(m))
        self.assertEqual(len(L.load_split("train")[0]), len(self.train))


class LoadTeacher(WorldCase):
    def test_returns_each_windows_slice_of_the_array_in_windows_order(self):
        arr = np.fromfile(self.world.teacher_root / "train.f32", dtype="<f4")
        got = L.load_teacher("train", self.train)
        self.assertEqual(list(got), [w["id"] for w in self.train])
        s = 0
        for w in self.train:
            n = len(w["keys"])
            np.testing.assert_array_equal(got[w["id"]], arr[s:s + n])
            self.assertEqual(got[w["id"]].dtype, np.float32)
            s += n
        self.assertEqual(s, len(arr))

    def test_permute_keeps_each_windows_multiset_and_is_deterministic_per_seed(self):
        plain = L.load_teacher("train", self.train)
        a = L.load_teacher("train", self.train, permute_seed=5)
        b = L.load_teacher("train", self.train, permute_seed=5)
        c = L.load_teacher("train", self.train, permute_seed=6)
        for w in self.train:
            k = w["id"]
            np.testing.assert_array_equal(np.sort(a[k]), np.sort(plain[k]))
            np.testing.assert_array_equal(a[k], b[k])
            self.assertEqual(a[k].dtype, np.float32)
        self.assertTrue(any(not np.array_equal(a[k], plain[k]) for k in plain))
        self.assertTrue(any(not np.array_equal(a[k], c[k]) for k in plain))

    def test_refuses_an_array_whose_sha256_differs(self):
        p = self.world.teacher_root / "train.f32"
        raw = bytearray(p.read_bytes())
        raw[0] ^= 1
        p.write_bytes(bytes(raw))
        with self.assertRaises(SystemExit) as cm:
            L.load_teacher("train", self.train)
        self.assertIn("sha256", str(cm.exception))

    def test_refuses_another_pairs_sha256(self):
        self.edit_teacher(lambda m: m["splits"]["train"].update(pairsSha256="0" * 64))
        with self.assertRaises(SystemExit) as cm:
            L.load_teacher("train", self.train)
        self.assertIn("pair order", str(cm.exception))

    def test_refuses_another_length(self):
        p = self.world.teacher_root / "train.f32"
        p.write_bytes(p.read_bytes()[:-4])
        sha = hashlib.sha256(p.read_bytes()).hexdigest()
        self.edit_teacher(lambda m: m["splits"]["train"].update(sha256=sha))
        with self.assertRaises(SystemExit) as cm:
            L.load_teacher("train", self.train)
        self.assertIn("pair order", str(cm.exception))

    def test_refuses_a_teacher_whose_c_t_did_not_pass(self):
        self.edit_teacher(lambda m: m.update(ct={"check": "C-T", "pass": False, "maxAbsDiff": 0.3}))
        with self.assertRaises(SystemExit) as cm:
            L.load_teacher("train", self.train)
        self.assertIn("did not pass", str(cm.exception))

    def test_refuses_a_teacher_whose_c_t_was_skipped_unless_the_env_allows_it(self):
        self.edit_teacher(lambda m: m.update(ct={"check": "C-T", "skipped": "--no-ct"}))
        with self.assertRaises(SystemExit) as cm:
            L.load_teacher("train", self.train)
        self.assertIn("skipped", str(cm.exception))
        with mock.patch.dict(os.environ, {L.ALLOW_UNCHECKED_TEACHER: "1"}):
            self.assertEqual(len(L.load_teacher("train", self.train)), len(self.train))
        with mock.patch.dict(os.environ, {L.ALLOW_UNCHECKED_TEACHER: "0"}):
            with self.assertRaises(SystemExit):
                L.load_teacher("train", self.train)

    def test_a_teacher_without_a_ct_record_is_refused(self):
        self.edit_teacher(lambda m: m.pop("ct"))
        with self.assertRaises(SystemExit):
            L.load_teacher("train", self.train)


@needs_torch
class RunIdentity(unittest.TestCase):
    def ns(self, **kw):
        base = dict(lr=1e-4, alpha=0.5, out="/x/a", device="cpu", threads=4, ckpt_minutes=5.0, stop_after_steps=None,
                    allow_device_change=False, job_steps=1, eta_limit_minutes=None)
        base.update(kw)
        return argparse.Namespace(**base)

    def test_run_plumbing_args_do_not_change_the_sha(self):
        ref = L.config_of(self.ns())[1]
        for kw in ({"out": "/y"}, {"device": "cuda"}, {"threads": 1}, {"ckpt_minutes": 1.0}, {"stop_after_steps": 3},
                   {"allow_device_change": True}, {"job_steps": 4}, {"eta_limit_minutes": 9.0}):
            self.assertEqual(L.config_of(self.ns(**kw))[1], ref, kw)

    def test_any_other_arg_changes_the_sha(self):
        ref = L.config_of(self.ns())[1]
        for kw in ({"lr": 2e-4}, {"alpha": 0.0}, {"extra": 1}):
            self.assertNotEqual(L.config_of(self.ns(**kw))[1], ref, kw)

    def test_grid_identity_of_every_grid_step(self):
        for route in MJ.ROUTES:
            for teacher in (True, False):
                try:
                    configs = MJ.grid(route, teacher)
                except SystemExit:
                    continue
                for name, args in configs:
                    argv = ["--stage", "grid"] + MJ.COMMON[route] + args + ["--out", f"/work/out/{name}", "--device", "cuda"]
                    self.assertEqual(L.grid_identity(route, argv, f"/work/out/{name}"), (name, args), (route, teacher, name))

    def test_grid_identity_of_a_final_step_carrying_the_grid_args(self):
        name, args = MJ.grid("gte")[2]
        argv = ["--stage", "final", "--control", "shuffled"] + MJ.COMMON["gte"] + args + ["--out", "/w/out/shuffled"]
        self.assertEqual(L.grid_identity("gte", argv, "/w/out/shuffled"), ("shuffled", args))

    def test_without_a_grid_config_it_is_argv_minus_out_and_device(self):
        argv = ["--lr", "0.1", "--out", "/w/o/x", "--device", "cpu", "--seed", "1"]
        self.assertEqual(L.grid_identity("rr", argv, "/w/o/x"), ("x", ["--lr", "0.1", "--seed", "1"]))

    def test_export_module_is_this_directorys_export_py(self):
        self.assertEqual(Path(L.export_module().__file__).resolve(), HERE / "export.py")


@needs_torch
class Schedules(unittest.TestCase):
    def test_linear_warmup_rises_to_one_over_ceil_five_percent_then_decays_to_zero(self):
        f = L.linear_warmup(40)
        self.assertEqual([f(i) for i in range(2)], [0.5, 1.0])
        self.assertEqual(f(2), 1.0)
        self.assertAlmostEqual(f(40), 0.0)
        tail = [f(i) for i in range(2, 41)]
        self.assertEqual(tail, sorted(tail, reverse=True))
        self.assertEqual([L.linear_warmup(21)(i) for i in range(2)], [0.5, 1.0])

    def test_bucketed_batches_cover_every_index_once_and_are_seeded(self):
        lengths = np.random.default_rng(0).integers(1, 50, 137)
        a = L.bucketed_batches(lengths, 8, 3, 1)
        flat = sorted(int(i) for b in a for i in b)
        self.assertEqual(flat, list(range(137)))
        self.assertLessEqual(max(len(b) for b in a), 8)
        b = L.bucketed_batches(lengths, 8, 3, 1)
        self.assertEqual([x.tolist() for x in a], [x.tolist() for x in b])
        c = L.bucketed_batches(lengths, 8, 3, 2)
        self.assertNotEqual([x.tolist() for x in a], [x.tolist() for x in c])
        self.assertEqual(sorted(int(i) for x in c for i in x), list(range(137)))

    def test_window_ranks_first_hit_over_positives_and_answers_also(self):
        w = {"positives": [2], "answersAlso": [4]}
        self.assertEqual(L.window_ranks([0.1, 0.9, 0.5, 0.3, 0.2], w), 2.0)
        self.assertEqual(L.window_ranks([0.1, 0.2, 0.5, 0.3, 0.9], w), 1.0)
        self.assertEqual(L.window_ranks([0.1, 0.2, 0.3, 0.3, 0.2], w), 1.0)
        tie = {"positives": [1, 3], "answersAlso": []}
        self.assertEqual(L.window_ranks([0.0, 0.5, 0.0, 0.5], tie), 1.0)
        self.assertEqual(L.window_ranks([0.5, 0.5, 0.0], {"positives": [1], "answersAlso": []}), 2.0)
        self.assertEqual(L.window_ranks([0.5, 0.4], {"positives": [], "answersAlso": []}), float("inf"))


@needs_torch
class ResultJson(unittest.TestCase):
    def evals(self, ranks):
        return [(f"q{i}", f"t{i % 3}", r) for i, r in enumerate(ranks)]

    def test_keys_and_the_paired_gain(self):
        base, best = self.evals([4.0, 3.0, 5.0, 2.0, 6.0, 4.0]), self.evals([1.0, 1.0, 2.0, 1.0, 2.0, 1.0])
        r = L.result_json("gte", "lr1-a0", ["--lr", "1"], 2, base, best, 730, 200)
        self.assertEqual(set(r), {"route", "name", "args", "best", "base", "gain"})
        self.assertEqual(r["best"]["epoch"], 2)
        self.assertEqual(set(r["gain"]), {"mrr", "lower", "upper"})
        self.assertAlmostEqual(r["gain"]["mrr"], r["best"]["mrr"] - r["base"]["mrr"])
        self.assertGreater(r["gain"]["lower"], 0)
        self.assertEqual(r, L.result_json("gte", "lr1-a0", ["--lr", "1"], 2, base, best, 730, 200))

    def test_refuses_base_and_best_evals_over_different_queries(self):
        base = self.evals([1.0, 2.0])
        best = [("other", "t0", 1.0), ("q1", "t1", 1.0)]
        with self.assertRaises(SystemExit):
            L.result_json("gte", "n", [], 1, base, best, 1, 10)


@needs_torch
class TorchHelpers(unittest.TestCase):
    def tiny(self, seed=0):
        torch.manual_seed(seed)
        return torch.nn.Sequential(torch.nn.Linear(3, 4), torch.nn.Linear(4, 2))

    def test_trainable_state_round_trip_and_name_mismatch_refused(self):
        m, other = self.tiny(0), self.tiny(1)
        L.load_trainable(other, L.trainable_state(m))
        for a, b in zip(m.parameters(), other.parameters()):
            self.assertTrue(torch.equal(a, b))
        for p in other[0].parameters():
            p.requires_grad_(False)
        with self.assertRaises(SystemExit) as cm:
            L.load_trainable(other, L.trainable_state(m))
        self.assertIn("differ", str(cm.exception))
        self.assertEqual(set(L.trainable_state(other)), {"1.weight", "1.bias"})

    def test_forward_sorted_returns_rows_in_input_order(self):
        class Enc:
            def __init__(self, n):
                self.ids, self.type_ids = list(range(1, n + 1)), [0] * n

        encs = [Enc(n) for n in (5, 2, 7, 3, 1, 6)]

        def fn(feed):
            return (feed["input_ids"].float() * feed["attention_mask"]).sum(1, keepdim=True)

        got = L.forward_sorted(fn, encs, 2, 0, torch.device("cpu"))
        want = torch.tensor([[sum(e.ids)] for e in encs], dtype=torch.float32)
        self.assertTrue(torch.equal(got, want))


@needs_torch
class CheckpointContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def ns(self, **kw):
        base = dict(out=str(Path(self.tmp.name) / "o"), ckpt_minutes=60.0, stop_after_steps=None, device="cpu",
                    allow_device_change=False)
        base.update(kw)
        return argparse.Namespace(**base)

    def test_load_refuses_another_config_sha(self):
        with L.Checkpointer(self.ns(), "a" * 64) as ck:
            ck.save({"x": 1})
        with L.Checkpointer(self.ns(), "b" * 64) as ck:
            with self.assertRaises(SystemExit) as cm:
                ck.load()
        self.assertIn("written for config", str(cm.exception))

    def test_load_refuses_another_device_unless_allowed(self):
        with L.Checkpointer(self.ns(device="cuda"), "a" * 64) as ck:
            ck.save({"x": 1})
        with L.Checkpointer(self.ns(), "a" * 64) as ck:
            with self.assertRaises(SystemExit) as cm:
                ck.load()
        self.assertIn("--allow-device-change", str(cm.exception))

    def test_load_restores_the_rng_state(self):
        with L.Checkpointer(self.ns(), "a" * 64) as ck:
            ck.save({"x": 1})
            want = (torch.rand(3), np.random.rand(3).tolist(), __import__("random").random())
        torch.rand(10)
        np.random.rand(10)
        __import__("random").random()
        with L.Checkpointer(self.ns(), "a" * 64) as ck:
            st = ck.load()
            self.assertEqual(st["x"], 1)
            got = (torch.rand(3), np.random.rand(3).tolist(), __import__("random").random())
        self.assertTrue(torch.equal(got[0], want[0]))
        self.assertEqual(got[1:], want[1:])

    def test_load_without_a_file_is_none(self):
        with L.Checkpointer(self.ns(), "a" * 64) as ck:
            self.assertIsNone(ck.load())

    def test_stop_after_steps_saves_then_exits_75(self):
        with L.Checkpointer(self.ns(stop_after_steps=3), "a" * 64) as ck:
            ck.at_step(1, lambda: {"s": 1})
            self.assertFalse(ck.path.exists())
            with self.assertRaises(SystemExit) as cm:
                ck.at_step(3, lambda: {"s": 3})
            self.assertEqual(cm.exception.code, 75)
            self.assertEqual(ck.load()["s"], 3)

    def test_saves_without_exiting_once_the_interval_has_passed(self):
        with L.Checkpointer(self.ns(ckpt_minutes=0.0), "a" * 64) as ck:
            ck.at_step(1, lambda: {"s": 1})
            self.assertEqual(ck.load()["s"], 1)


@needs_torch
class ProbeGate(unittest.TestCase):
    def run_probe(self, **kw):
        t = iter(range(0, 10_000, 60))  # every clock read is 60 s after the last
        with mock.patch.object(L.time, "monotonic", lambda: next(t)):
            p = L.Probe("x", total_steps=10, **kw)
            for i in range(1, 5):
                p.tick(i)
        return p

    def test_job_eta_is_the_step_eta_plus_the_rate_times_the_later_steps(self):
        with mock.patch.object(L, "progress") as pr:
            p = self.run_probe(job_steps=3)
        self.assertEqual(p.rate, 60.0)
        self.assertIn("job ETA 26.0 min over 3 step(s)", pr.call_args_list[0][0][0])

    def test_exits_76_when_the_job_eta_is_over_the_limit(self):
        with mock.patch.object(L, "progress"):
            with self.assertRaises(SystemExit) as cm:
                self.run_probe(job_steps=3, limit_minutes=25.9)
            self.assertEqual(cm.exception.code, L.ETA_EXIT)
            self.assertEqual(L.ETA_EXIT, 76)
            self.run_probe(job_steps=3, limit_minutes=26.0)

    def test_no_limit_never_exits(self):
        with mock.patch.object(L, "progress"):
            self.run_probe(job_steps=50)


if __name__ == "__main__":
    unittest.main()
