"""Checkpoint helpers shared by the training scripts.

A checkpoint holds only CPU tensors, so one written on a GPU host loads on a
CPU-only laptop; loads map every tensor to the CPU. The file is replaced
atomically (tmp + fsync + replace): a reader never sees a partial file.

Exit codes of a trainer: 0 done, KILLED_EXIT stopped resumably (a rerun with
the same --out resumes from checkpoint.pt), anything else a failure.
"""

import os
import random
import signal
from pathlib import Path

import numpy as np
import torch

KILLED_EXIT = 75


def to_cpu(obj):
    """A copy of obj with every tensor inside dicts, lists and tuples moved to the CPU."""
    if isinstance(obj, torch.Tensor):
        return obj.detach().cpu()
    if isinstance(obj, dict):
        out = obj.copy()
        for k, v in obj.items():
            out[k] = to_cpu(v)
        return out
    if isinstance(obj, list):
        return [to_cpu(v) for v in obj]
    if isinstance(obj, tuple):
        return tuple(to_cpu(v) for v in obj)
    return obj


def save_ckpt(path, obj):
    tmp = Path(str(path) + ".tmp")
    torch.save(to_cpu(obj), tmp)
    with open(tmp, "rb+") as f:
        os.fsync(f.fileno())
    os.replace(tmp, path)


def load_ckpt(path):
    return torch.load(path, map_location="cpu", weights_only=False)


def rng_state(cuda=None):
    """Python, NumPy and torch CPU generator states, plus every CUDA generator's
    under the key `cuda` when cuda is true (default: CUDA was used by this process)."""
    s = {"python": random.getstate(), "numpy": np.random.get_state(), "torch": torch.get_rng_state()}
    if cuda is None:
        cuda = torch.cuda.is_available() and torch.cuda.is_initialized()
    if cuda:
        s["cuda"] = torch.cuda.get_rng_state_all()
    return s


def set_rng_state(s, allow_device_change=False):
    """Restore rng_state()'s result. A state with CUDA generators on a host
    without CUDA raises ValueError unless allow_device_change."""
    if "cuda" in s and not torch.cuda.is_available() and not allow_device_change:
        raise ValueError("the RNG state holds CUDA generators and this host has no CUDA (--allow-device-change to drop them)")
    random.setstate(s["python"])
    np.random.set_state(s["numpy"])
    torch.set_rng_state(s["torch"])
    if "cuda" in s and torch.cuda.is_available():
        for i, g in enumerate(s["cuda"][:torch.cuda.device_count()]):
            torch.cuda.set_rng_state(g, i)


class StopOnSignal:
    """While entered, the given signals (default SIGTERM) set `requested`
    instead of killing the process; the training loop checks it at a step
    boundary, saves a checkpoint and exits KILLED_EXIT. Previous handlers are
    restored on exit."""

    def __init__(self, signals=(signal.SIGTERM,)):
        self.signals = tuple(signals)
        self.requested = False
        self._previous = {}

    def _handle(self, signum, frame):
        self.requested = True

    def __enter__(self):
        for sig in self.signals:
            self._previous[sig] = signal.signal(sig, self._handle)
        return self

    def __exit__(self, *exc):
        for sig, handler in self._previous.items():
            signal.signal(sig, handler)
        self._previous.clear()
        return False
