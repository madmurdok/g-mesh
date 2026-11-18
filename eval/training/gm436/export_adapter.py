#!/usr/bin/env python3
"""Export a trained adapter for the Rust `adapter` arm
(docs/design/GM-436-laptop-training.md, section 5 item 2).

Format (core/src/embedding/adapter.rs `QueryAdapter::load`): raw
little-endian f32, row-major d x d, q' = W q, i.e. q'[i] = sum_j W[i*d + j] q[j].
Writes <dest>/W.f32 and W.f32.sha256, prints the `[[variant]]` row, and with
--append adds it to a variants.toml (refusing a name that is already there).

usage: export_adapter.py --run <train_adapter --out dir> --name <arm> --reference <base arm>
                         [--role cost|quality|control|informational] [--dest DIR] [--append variants.toml]
"""

import argparse
import hashlib
import re
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cache as K  # noqa: E402


def w_bytes(W):
    W = np.asarray(W)
    if W.ndim != 2 or W.shape[0] != W.shape[1]:
        raise ValueError(f"W must be square, got {W.shape}")
    return np.ascontiguousarray(W, dtype="<f4").tobytes(order="C")


def write_w(W, dest):
    dest = Path(dest)
    dest.mkdir(parents=True, exist_ok=True)
    data = w_bytes(W)
    sha = hashlib.sha256(data).hexdigest()
    tmp = dest / "W.f32.tmp"
    tmp.write_bytes(data)
    tmp.replace(dest / "W.f32")
    (dest / "W.f32.sha256").write_text(f"{sha}  W.f32\n")
    return dest / "W.f32", sha


def load_w(path, expected_sha256=None):
    """Read a W.f32 the way Rust does: size must be 4*d*d, sha256 must match."""
    data = Path(path).read_bytes()
    if expected_sha256 is not None and hashlib.sha256(data).hexdigest() != expected_sha256:
        raise ValueError(f"adapter {path}: sha256 mismatch")
    n = len(data) // 4
    d = int(round(n ** 0.5))
    if len(data) % 4 or d * d != n:
        raise ValueError(f"adapter {path} holds {len(data)} bytes, not a square d x d f32 matrix")
    return np.frombuffer(data, dtype="<f4").reshape(d, d)


def apply(W, q):
    """q' = W q in f32, the reference result for the Rust round-trip fixture."""
    return (np.asarray(W, np.float32) @ np.asarray(q, np.float32)).astype(np.float32)


def home_path(p):
    p = Path(p).resolve()
    try:
        return "~/" + str(p.relative_to(Path.home()))
    except ValueError:
        return str(p)


def variant_row(name, reference, path, sha, role):
    return (f'[[variant]]\nname = "{name}"\narm = "adapter"\nrole = "{role}"\n'
            f'reference = "{reference}"\nadapter = "{home_path(path)}"\nadapter_sha256 = "{sha}"\n')


def append_row(variants_toml, name, row):
    text = Path(variants_toml).read_text()
    if re.search(rf'^name = "{re.escape(name)}"$', text, re.M):
        raise SystemExit(f"{variants_toml} already has a variant {name}; rows are appended, never edited")
    with open(variants_toml, "a") as f:
        f.write("\n" + row)


def main():
    ap = argparse.ArgumentParser()
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--run", help="train_adapter.py --out dir")
    src.add_argument("--identity", type=int, metavar="D", help="W = I of size D (control C0)")
    ap.add_argument("--name", required=True)
    ap.add_argument("--reference", required=True)
    ap.add_argument("--role", default="informational")
    ap.add_argument("--dest", help="default: $GM436_DATA/adapters/<name>")
    ap.add_argument("--append")
    a = ap.parse_args()
    if a.identity:
        W = np.eye(a.identity, dtype=np.float32)
    else:
        import torch
        state = torch.load(Path(a.run) / "checkpoint.pt", map_location="cpu", weights_only=False)
        if not state["done"]:
            sys.exit(f"{a.run}: training not finished")
        W = np.asarray(state["bestW"], np.float32)
    path, sha = write_w(W, a.dest or K.gm436_root() / "adapters" / a.name)
    if not np.array_equal(load_w(path, sha), W):
        sys.exit(f"{path}: read-back differs from the checkpoint")
    row = variant_row(a.name, a.reference, path, sha, a.role)
    if a.append:
        append_row(a.append, a.name, row)
    (Path(path).parent / "variant.toml").write_text(row)
    print(row, end="")


if __name__ == "__main__":
    main()
