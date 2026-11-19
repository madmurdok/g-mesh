#!/usr/bin/env python3
"""Stand-in trainer for the launcher's end-to-end and terminate scenarios (stdlib only).

Follows the runner's trainer contract: writes --out/checkpoint.pt atomically,
resumes from it when present, exits 0 when done, 1 on failure.
  --mode ok    two epochs of --seconds each, a checkpoint after each, result.json
  --mode fail  exit 1 after the first checkpoint
  --mode hang  sleep forever (the lifetime layers must stop the instance)
"""

import argparse
import json
import os
import sys
import time
from pathlib import Path


def save(path, obj):
    tmp = path.with_name(path.name + ".tmp")
    with open(tmp, "w") as f:
        json.dump(obj, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--mode", choices=["ok", "fail", "hang"], default="ok")
    ap.add_argument("--seconds", type=float, default=30)
    ap.add_argument("--epochs", type=int, default=2)
    ap.add_argument("--device", default="cpu")
    ap.add_argument("--out", required=True)
    a = ap.parse_args(argv)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    ckpt = out / "checkpoint.pt"
    state = {"epoch": 0}
    if ckpt.exists():
        state = json.loads(ckpt.read_text())
        print(f"{out}: resumed after epoch {state['epoch']}", flush=True)
    if a.mode == "hang":
        print(f"{out}: hanging", flush=True)
        while True:
            time.sleep(3600)
    for epoch in range(state["epoch"] + 1, a.epochs + 1):
        time.sleep(a.seconds)
        state["epoch"] = epoch
        save(ckpt, state)
        print(f"{out}: epoch {epoch}/{a.epochs}", flush=True)
        if a.mode == "fail":
            print(f"{out}: failing on purpose", flush=True)
            return 1
    (out / "result.json").write_text(json.dumps({"epochs": state["epoch"], "device": a.device}) + "\n")
    print(f"{out}: done", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
