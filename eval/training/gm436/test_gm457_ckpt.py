"""Tests of the device-portable checkpoints of the adapter trainer (GM-457;
design note docs/design/GM-457-spot-gpu-training.md, section 4 behaviours
13-18, D5): eval/training/ckpt.py, train_adapter.py --device /
--allow-device-change, export_adapter.py. Needs torch (the GM-436 venv); every
test assumes a host without CUDA and skips on one with it.

The SIGTERM test runs the trainer as separate processes. GM436_PROGRESS points
at a FIFO the test reads: once the test stops reading, the trainer blocks in its
next progress() call (right after an epoch's checkpoint), where it is killed
by SIGTERM's default action; a fresh process resumes.

Run (GM-436 venv, from the repo root):
  ~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -m unittest discover \
      -s eval/training/gm436 -p 'test_gm457_*.py'
"""

import contextlib
import io
import json
import os
import pickle
import random
import signal
import subprocess
import sys
import tempfile
import threading
import unittest
from collections import OrderedDict
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent))
import cache as K  # noqa: E402
from fixture_gm436 import VARIANT, VERSION, Fixture  # noqa: E402

try:
    import torch
    import ckpt as CK
    import export_adapter as E
    import train_adapter as T
except ImportError:  # pragma: no cover
    torch = None

COMMON = ["--version", VERSION, "--base", VARIANT, "--batch", "8", "--patience", "99", "--resamples", "100",
          "--threads", "1", "--no-train-metric", "--form", "full", "--lam", "0.001"]
NO_TORCH = torch is None
HAS_CUDA = (not NO_TORCH) and torch.cuda.is_available()
HAS_MPS = (not NO_TORCH) and torch.backends.mps.is_available()


@unittest.skipIf(NO_TORCH, "torch is not installed (GM-436 venv)")
class CkptHelpers(unittest.TestCase):
    """Behaviours 14, 14b, 15, 15b, 21', 22'."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    @unittest.skipUnless(HAS_MPS, "needs a non-CPU torch device (MPS)")
    def test_save_ckpt_writes_only_cpu_tensors_and_keeps_containers(self):
        dev = torch.device("mps")
        obj = OrderedDict(a=torch.ones(2, device=dev), b=[torch.zeros(1, device=dev), (torch.arange(3, device=dev), 7)],
                          c={"d": torch.full((2, 2), 3.0, device=dev)}, e="text")
        p = self.root / "c.pt"
        CK.save_ckpt(p, obj)
        raw = torch.load(p, weights_only=False)  # no map_location: the devices as written
        self.assertIsInstance(raw, OrderedDict)
        self.assertEqual(list(raw), ["a", "b", "c", "e"])
        self.assertIsInstance(raw["b"], list)
        self.assertIsInstance(raw["b"][1], tuple)
        self.assertEqual(raw["b"][1][1], 7)
        self.assertEqual(raw["e"], "text")
        for t in (raw["a"], raw["b"][0], raw["b"][1][0], raw["c"]["d"]):
            self.assertEqual(t.device.type, "cpu")
        self.assertTrue(torch.equal(raw["c"]["d"], torch.full((2, 2), 3.0)))
        self.assertEqual(obj["a"].device.type, "mps", "save_ckpt must not move the caller's tensors")

    def test_save_ckpt_moves_every_tensor_to_cpu(self):
        moved = []
        orig = torch.Tensor.cpu

        def spy(self, *a, **k):
            moved.append(tuple(self.shape))
            return orig(self, *a, **k)

        obj = {"x": torch.ones(1), "y": [torch.ones(2), (torch.ones(3),)], "z": OrderedDict(w=torch.ones(4))}
        with mock.patch.object(torch.Tensor, "cpu", spy):
            CK.save_ckpt(self.root / "s.pt", obj)
        self.assertEqual(sorted(moved), [(1,), (2,), (3,), (4,)])

    def test_to_cpu_moves_every_nested_tensor(self):
        moved = []
        orig = torch.Tensor.cpu

        def spy(self, *a, **k):
            moved.append(tuple(self.shape))
            return orig(self, *a, **k)

        obj = {"x": torch.ones(1), "y": [torch.ones(2), (torch.ones(3),)], "z": OrderedDict(w=torch.ones(4))}
        with mock.patch.object(torch.Tensor, "cpu", spy):
            out = CK.to_cpu(obj)
        self.assertEqual(sorted(moved), [(1,), (2,), (3,), (4,)])
        self.assertIsInstance(out["z"], OrderedDict)
        self.assertIsNot(out, obj)

    def test_load_ckpt_maps_to_cpu(self):
        p = self.root / "c.pt"
        CK.save_ckpt(p, {"w": torch.ones(2)})
        calls = []
        real = torch.load

        def spy(*a, **k):
            calls.append(k)
            return real(*a, **k)

        with mock.patch.object(torch, "load", spy):
            CK.load_ckpt(p)
        self.assertEqual(calls[0].get("map_location"), "cpu")

    def test_save_ckpt_is_atomic(self):
        p = self.root / "c.pt"
        CK.save_ckpt(p, {"v": 1})
        self.assertEqual(sorted(x.name for x in self.root.iterdir()), ["c.pt"])
        before = p.read_bytes()

        def boom(obj, f, *a, **k):
            Path(f).write_bytes(b"partial")
            raise OSError("disk full")

        with mock.patch.object(torch, "save", boom):
            with self.assertRaises(OSError):
                CK.save_ckpt(p, {"v": 2})
        self.assertEqual(p.read_bytes(), before)
        self.assertEqual(CK.load_ckpt(p), {"v": 1})

    def test_rng_state_round_trip(self):
        random.seed(1)
        np.random.seed(1)
        torch.manual_seed(1)
        s = CK.rng_state()
        a = (random.random(), np.random.rand(), torch.rand(1).item())
        random.seed(99)
        np.random.seed(99)
        torch.manual_seed(99)
        CK.set_rng_state(pickle.loads(pickle.dumps(s)))
        self.assertEqual((random.random(), np.random.rand(), torch.rand(1).item()), a)

    @unittest.skipIf(HAS_CUDA, "needs a host without CUDA")
    def test_cuda_key_only_when_asked_or_initialised(self):
        self.assertNotIn("cuda", CK.rng_state())
        self.assertNotIn("cuda", CK.rng_state(cuda=False))

    @unittest.skipIf(HAS_CUDA, "needs a host without CUDA")
    def test_cuda_state_on_cuda_less_host(self):
        s = CK.rng_state()
        s["cuda"] = [torch.zeros(16, dtype=torch.uint8)]
        with self.assertRaises(ValueError):
            CK.set_rng_state(s)
        torch.manual_seed(5)
        x = torch.rand(1).item()
        torch.manual_seed(5)
        s = CK.rng_state()
        s["cuda"] = [torch.zeros(16, dtype=torch.uint8)]
        torch.manual_seed(77)
        CK.set_rng_state(s, allow_device_change=True)
        self.assertEqual(torch.rand(1).item(), x)

    def test_stop_on_signal(self):
        prev = signal.getsignal(signal.SIGTERM)
        with CK.StopOnSignal() as stop:
            self.assertFalse(stop.requested)
            os.kill(os.getpid(), signal.SIGTERM)
            self.assertTrue(stop.requested)
        self.assertIs(signal.getsignal(signal.SIGTERM), prev)

    def test_killed_exit_importable_from_trainer(self):
        self.assertEqual(T.KILLED_EXIT, 75)


@unittest.skipIf(NO_TORCH, "torch is not installed (GM-436 venv)")
class TrainCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.fx = Fixture(self.tmp.name)
        env = mock.patch.dict(os.environ, self.fx.env())
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop("GM436_PROGRESS", None)
        K.import_run(self.fx.mine, self.fx.vdir, VARIANT, "alpha")
        self.root = Path(self.tmp.name)

    def env(self, **extra):
        e = {**os.environ, **self.fx.env(), **extra}
        if "GM436_PROGRESS" not in extra:
            e.pop("GM436_PROGRESS", None)
        return e

    def cmd(self, out, *args):
        return [sys.executable, str(HERE / "train_adapter.py"), *COMMON, "--out", str(out), *args]

    def run_trainer(self, out, *args):
        return subprocess.run(self.cmd(out, *args), env=self.env(), capture_output=True, text=True, timeout=600)

    def result(self, out):
        return json.loads((Path(out) / "result.json").read_text())

    def rewrite_ckpt(self, out, fn):
        p = Path(out) / "checkpoint.pt"
        state = torch.load(p, weights_only=False)
        fn(state)
        torch.save(state, p)


class SigtermAndResume(TrainCase):
    """Behaviour 13: a trainer killed by SIGTERM (no handler: the signal's default
    action) and rerun with the same --out ends bit-identical to an uninterrupted run."""

    def test_sigterm_then_resume_is_bit_identical(self):
        epochs = ["--epochs", "6"]
        whole, cut = self.root / "whole", self.root / "cut"
        p = self.run_trainer(whole, *epochs)
        self.assertEqual(p.returncode, 0, p.stderr)
        fifo = self.root / "progress.fifo"
        os.mkfifo(fifo)
        proc = subprocess.Popen(self.cmd(cut, *epochs), env=self.env(GM436_PROGRESS=str(fifo)),
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        lines = []

        def reader():
            while True:
                with open(fifo) as f:
                    text = f.read()
                lines.append(text)
                if "epoch 2/6" in text or proc.poll() is not None:
                    return

        t = threading.Thread(target=reader, daemon=True)
        t.start()
        t.join(300)
        self.assertFalse(t.is_alive(), "trainer never reported epoch 2")
        # The trainer is now blocked opening the FIFO for its next progress line,
        # after epoch 3's checkpoint at the latest.
        self.assertIsNone(proc.poll(), "".join(lines))
        proc.send_signal(signal.SIGTERM)
        _, err = proc.communicate(timeout=60)
        self.assertEqual(proc.returncode, -signal.SIGTERM, err)
        self.assertFalse((cut / "result.json").exists())
        self.assertEqual([x.name for x in cut.iterdir() if x.name.endswith(".tmp")], [])
        state = torch.load(cut / "checkpoint.pt", weights_only=False)
        self.assertGreaterEqual(state["epoch"], 2)
        self.assertLess(state["epoch"], 6)
        p = self.run_trainer(cut, *epochs)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(f"resumed after epoch {state['epoch']}", p.stdout)
        a, b = self.result(whole), self.result(cut)
        self.assertEqual(len(a["history"]), 7)
        self.assertEqual(b["wSha256"], a["wSha256"])
        self.assertEqual(b["lastWSha256"], a["lastWSha256"])
        self.assertEqual(b["history"], a["history"])


class DeviceGuard(TrainCase):
    """Behaviours 15b, 16 (C7), 16b, 17, 20'."""

    def stopped(self, name):
        out = self.root / name
        p = self.run_trainer(out, "--epochs", "4", "--stop-after-epochs", "1")
        self.assertEqual(p.returncode, T.KILLED_EXIT, p.stderr)
        return out

    def test_checkpoint_device_is_recorded(self):
        out = self.stopped("rec")
        self.assertEqual(torch.load(out / "checkpoint.pt", weights_only=False)["device"], "cpu")

    def test_checkpoint_of_another_device_is_refused(self):
        out = self.stopped("c7")
        self.rewrite_ckpt(out, lambda s: s.update(device="cuda"))
        p = self.run_trainer(out, "--epochs", "4")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("written on device cuda", p.stderr)
        self.assertFalse((out / "result.json").exists())

    def test_allow_device_change_resumes_and_records_new_device(self):
        whole = self.root / "whole"
        p = self.run_trainer(whole, "--epochs", "4")
        self.assertEqual(p.returncode, 0, p.stderr)
        out = self.stopped("moved")
        self.rewrite_ckpt(out, lambda s: s.update(device="cuda"))
        p = self.run_trainer(out, "--epochs", "4", "--allow-device-change", "--stop-after-epochs", "2")
        self.assertEqual(p.returncode, T.KILLED_EXIT, p.stderr)
        self.assertIn("resumed after epoch 1", p.stdout)
        self.assertEqual(torch.load(out / "checkpoint.pt", weights_only=False)["device"], "cpu")
        p = self.run_trainer(out, "--epochs", "4")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.result(out)["wSha256"], self.result(whole)["wSha256"])

    def test_checkpoint_without_device_counts_as_cpu(self):
        out = self.stopped("old")
        self.rewrite_ckpt(out, lambda s: s.pop("device"))
        p = self.run_trainer(out, "--epochs", "4")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("resumed after epoch 1", p.stdout)
        p2 = self.stopped("old2")
        self.rewrite_ckpt(p2, lambda s: s.pop("device"))
        p = self.run_trainer(p2, "--epochs", "4", "--device", "cuda")
        self.assertNotEqual(p.returncode, 0)

    @unittest.skipIf(HAS_CUDA, "needs a host without CUDA")
    def test_cuda_rng_state_on_cuda_less_host(self):
        out = self.stopped("rng")
        self.rewrite_ckpt(out, lambda s: s["rng"].update(cuda=[torch.zeros(16, dtype=torch.uint8)]))
        p = self.run_trainer(out, "--epochs", "4")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn(f"{out / 'checkpoint.pt'}: ", p.stderr)
        self.assertIn("CUDA", p.stderr)
        p = self.run_trainer(out, "--epochs", "4", "--allow-device-change")
        self.assertEqual(p.returncode, 0, p.stderr)

    @unittest.skipIf(HAS_CUDA, "needs a host without CUDA")
    def test_device_cuda_without_cuda_exits(self):
        p = self.run_trainer(self.root / "nocuda", "--epochs", "2", "--device", "cuda")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("CUDA is not available", p.stderr)

    def test_device_flags_do_not_change_config_sha(self):
        base = [*COMMON, "--out", str(self.root / "x")]
        _, a = T.config_of(T.parse(base))
        _, b = T.config_of(T.parse([*base, "--device", "cuda", "--allow-device-change"]))
        _, c = T.config_of(T.parse([*base, "--lr", "0.5"]))
        self.assertEqual(a, b)
        self.assertNotEqual(a, c)


class ExportLoadsOnCpu(TrainCase):
    """Behaviour 18: export_adapter reads the checkpoint with map_location="cpu"."""

    def test_export_maps_checkpoint_to_cpu(self):
        out = self.root / "run"
        p = self.run_trainer(out, "--epochs", "2")
        self.assertEqual(p.returncode, 0, p.stderr)
        calls = []
        real = torch.load

        def spy(*a, **k):
            calls.append(k)
            return real(*a, **k)

        argv = ["export_adapter.py", "--run", str(out), "--name", "t", "--reference", "ref",
                "--dest", str(self.root / "adapters" / "t")]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(torch, "load", spy), \
                contextlib.redirect_stdout(io.StringIO()):
            E.main()
        self.assertTrue(calls)
        self.assertTrue(all(k.get("map_location") == "cpu" for k in calls), calls)


if __name__ == "__main__":
    unittest.main()
