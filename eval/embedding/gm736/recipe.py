"""GM-736: the CodeRankEmbed int8 recipe with smoothing (docs/design/GM-736-cre-int8.md, sections 1-3).

    recipe.py --sweep 0.3,0.4,0.5,0.6,0.7 --weights s7   # R1: selection-sample Q-D per alpha -> sweep-s7.json
    recipe.py --export --alpha A --weights s7 [--exclude-file F]
                                                          # <models>/coderankembed-int8-sq-s7/{model.onnx,tokenizer.json,EXPORT.json}
    recipe.py --sensitivity --alpha A --weights s7        # R2: per-MatMul gain over R1 -> sensitivity-s7.json
    recipe.py --greedy --alpha A --weights s7             # R2: exclusion list -> exclude-s7.json
    recipe.py --gate --weights s7                         # Q-D on the frozen parity sample, Q-F, size -> gate-s7.json

--weights s7 is the shipped recipe (QInt8, 7-bit weights); u8 is QUInt8 symmetric 8-bit weights.
alpha and the R2 exclusions are chosen on the selection sample only; the gate
sample is read by --gate alone. --limit N (smoke runs) takes the first N rows of
every sample and is recorded in each output.

Exit status: 0 ok; 3 the gate (or R2's greedy) missed; 2 bad input.
"""

import argparse
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "gm703"))
sys.path.insert(0, str(HERE))

import export_coderankembed as E  # noqa: E402
import parity as P  # noqa: E402
import smooth as S  # noqa: E402

DATA = Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm736"
FP32_DIR = E.DEFAULT_OUT / "coderankembed-fp32"
CALIB = HERE / "calib_sample.jsonl"
SELECT = HERE / "select_sample.jsonl"
FROZEN_SAMPLE_SHA256 = "48908b01922fc50fee5ae523101da2d128b131a9f298c245918212f6a77e98a9"
GATE_MIN_MEDIAN = 0.99
R2_TARGET_MEDIAN = GATE_MIN_MEDIAN + 0.002
SIZE_BUDGET = 162_000_000


def model_dir(models, weights):
    return Path(models) / f"coderankembed-int8-sq-{weights}"


def load_ids(path, tok, limit=None):
    rows = P.load_sample(path)[:limit]
    return rows, [tok.encode(r["text"]).ids for r in rows]


class Ctx:
    """Paths, the tokenizer and cached calibration / reference results."""

    def __init__(self, a):
        self.a = a
        self.fp32 = Path(a.fp32_dir) / "model.onnx"
        self.work = Path(a.work)
        self.work.mkdir(parents=True, exist_ok=True)
        self.tok = P.tokenizer(Path(a.fp32_dir) / "tokenizer.json")
        self.suffix = f"-limit{a.limit}" if a.limit else ""
        self.fp32_sha = E.sha256(self.fp32)
        self.calib_sha = P.sha256_file(a.calib)

    def absmax(self):
        cache = self.work / f"absmax-{self.fp32_sha[:16]}-{self.calib_sha[:16]}{self.suffix}.npz"
        if cache.exists():
            return dict(np.load(cache))
        _, ids = load_ids(self.a.calib, self.tok, self.a.limit)
        amax = S.collect_absmax(self.fp32, ids)
        np.savez(cache, **amax)
        return amax

    def select(self):
        """(ids, fp32 CLS vectors) of the selection sample."""
        _, ids = load_ids(self.a.select, self.tok, self.a.limit)
        cache = self.work / f"fp32-select-{self.fp32_sha[:16]}-{P.sha256_file(self.a.select)[:16]}{self.suffix}.npy"
        if cache.exists():
            return ids, np.load(cache)
        ref = np.asarray(P.cls_vectors(P.session(self.fp32), ids))
        np.save(cache, ref)
        return ids, ref

    def common(self):
        return {"limit": self.a.limit, "calib_sample_sha256": self.calib_sha, "fp32_model_onnx_sha256": self.fp32_sha}


def qd(int8_path, ids, ref):
    t = time.time()
    vecs = P.cls_vectors(P.session(int8_path), ids)
    cos = [float(np.dot(a, b)) for a, b in zip(vecs, ref)]
    return {**P.summary(cos), "bytes": Path(int8_path).stat().st_size, "run_s": round(time.time() - t, 1)}


def write_json(path, obj):
    Path(path).write_text(json.dumps(obj, indent=2, sort_keys=True) + "\n")
    print(f"{path}: {json.dumps(obj, sort_keys=True)[:400]}")


def best_alpha(rows):
    """Highest selection median; a tie goes to the smaller alpha."""
    return min(rows, key=lambda r: (-r["median"], r["alpha"]))["alpha"]


def cmd_sweep(c, alphas, weights):
    amax = c.absmax()
    ids, ref = c.select()
    rows = []
    with tempfile.TemporaryDirectory(prefix="gm736-sweep-") as tmp:
        for alpha in alphas:
            q = Path(tmp) / "int8.onnx"
            rec = S.smoothed_int8(c.fp32, q, alpha, amax, weights)
            rows.append({**rec, **qd(q, ids, ref)})
            print(json.dumps(rows[-1]), flush=True)
    out = {**c.common(), "weights": weights, "rows": rows, "best_alpha": best_alpha(rows)}
    write_json(c.work / f"sweep-{weights}{c.suffix}.json", out)
    return 0


def cmd_export(c, alpha, weights, exclude):
    amax = c.absmax()
    d = model_dir(c.a.models, weights)
    d.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(Path(c.a.fp32_dir) / "tokenizer.json", d / "tokenizer.json")
    q = d / "model.onnx"
    rec = S.smoothed_int8(c.fp32, q, alpha, amax, weights, exclude)
    record = {
        **json.loads((Path(c.a.fp32_dir) / "EXPORT.json").read_text()),
        **c.common(),
        "precision": "int8",
        "recipe": "smooth+dynamic",
        **rec,
        "quantize": {"function": "onnxruntime.quantization.quantize_dynamic", **S.WEIGHT_RECIPES[weights]},
        "versions": E.versions(),
        "model_onnx_sha256": E.sha256(q),
        "model_onnx_bytes": q.stat().st_size,
    }
    E.write_export_json(d, record)
    print(f"{d}: model.onnx {record['model_onnx_bytes']} bytes sha256 {record['model_onnx_sha256']}")
    return 0


def matmul_names(fp32):
    import onnx

    return sorted(n.name for ns in S.const_matmuls(onnx.load(str(fp32))).values() for n in ns)


def cmd_sensitivity(c, alpha, weights):
    import onnx

    amax = c.absmax()
    ids, ref = c.select()
    factors = S.smoothing_factors(amax, S.weights_by_input(onnx.load(str(c.fp32))), alpha)
    rows = []
    with tempfile.TemporaryDirectory(prefix="gm736-sens-") as tmp:
        smoothed, q = Path(tmp) / "smoothed.onnx", Path(tmp) / "int8.onnx"
        S.apply_smoothing(c.fp32, smoothed, factors)
        S.quantize(smoothed, q, weights)
        base = qd(q, ids, ref)
        for name in matmul_names(c.fp32):
            S.quantize(smoothed, q, weights, [name])
            r = qd(q, ids, ref)
            rows.append({"node": name, "gain": r["median"] - base["median"], **r})
            print(json.dumps(rows[-1]), flush=True)
    rows.sort(key=lambda r: (-r["gain"], r["node"]))
    out = {**c.common(), "weights": weights, "alpha": alpha, "base": base, "rows": rows}
    write_json(c.work / f"sensitivity-{weights}{c.suffix}.json", out)
    return 0


def cmd_greedy(c, alpha, weights):
    import onnx

    sens = json.loads((c.work / f"sensitivity-{weights}{c.suffix}.json").read_text())
    if sens["alpha"] != alpha:
        raise SystemExit(f"sensitivity was measured at alpha {sens['alpha']}, not {alpha}")
    amax = c.absmax()
    ids, ref = c.select()
    factors = S.smoothing_factors(amax, S.weights_by_input(onnx.load(str(c.fp32))), alpha)
    excluded, steps, reached = [], [], False
    with tempfile.TemporaryDirectory(prefix="gm736-greedy-") as tmp:
        smoothed, q = Path(tmp) / "smoothed.onnx", Path(tmp) / "int8.onnx"
        S.apply_smoothing(c.fp32, smoothed, factors)
        for row in sens["rows"]:
            if row["gain"] <= 0:
                break
            S.quantize(smoothed, q, weights, excluded + [row["node"]])
            r = qd(q, ids, ref)
            if r["bytes"] > SIZE_BUDGET:
                steps.append({"node": row["node"], "over_budget": True, **r})
                break
            excluded.append(row["node"])
            steps.append({"node": row["node"], **r})
            print(json.dumps(steps[-1]), flush=True)
            if r["median"] >= R2_TARGET_MEDIAN:
                reached = True
                break
    out = {**c.common(), "weights": weights, "alpha": alpha, "target": R2_TARGET_MEDIAN,
           "size_budget": SIZE_BUDGET, "reached": reached, "excluded": excluded, "steps": steps}
    write_json(c.work / f"greedy-{weights}{c.suffix}.json", out)
    write_json(c.work / f"exclude-{weights}{c.suffix}.json", excluded)
    return 0 if reached else 3


def cmd_gate(c, weights):
    if P.sha256_file(c.a.gate_sample) != FROZEN_SAMPLE_SHA256:
        print(f"{c.a.gate_sample}: sha256 is not the frozen parity sample's {FROZEN_SAMPLE_SHA256}", file=sys.stderr)
        return 2
    d = model_dir(c.a.models, weights)
    rec = json.loads((d / "EXPORT.json").read_text())
    if rec["model_onnx_sha256"] != E.sha256(d / "model.onnx"):
        print(f"{d}/model.onnx is not the file EXPORT.json records", file=sys.stderr)
        return 2
    sample, ids = load_ids(c.a.gate_sample, c.tok, c.a.limit)
    ref = P.cls_vectors(P.session(c.fp32), ids)
    report = {
        "limit": c.a.limit,
        "n": len(sample),
        "sample_sha256": FROZEN_SAMPLE_SHA256,
        "export": {k: rec.get(k) for k in ("alpha", "weights", "excluded", "smoothing_sha256", "calib_sample_sha256",
                                           "model_onnx_sha256", "model_onnx_bytes")},
        "Q-D": P.check_qd(P.session(d / "model.onnx"), ref, ids, sample, c.a.jina_fp32, c.a.jina_int8,
                          min_median=GATE_MIN_MEDIAN),
        "Q-F": E.int8_fingerprint(d / "model.onnx", allowed_fp32=len(rec["excluded"]), weights=rec["weights"]),
        "size": {"bytes": rec["model_onnx_bytes"], "budget": SIZE_BUDGET, "ok": rec["model_onnx_bytes"] <= SIZE_BUDGET},
    }
    report["ok"] = bool(report["Q-D"]["ok"] and report["Q-F"]["ok"] and report["size"]["ok"])
    write_json(c.work / f"gate-{weights}{c.suffix}.json", report)
    return 0 if report["ok"] else 3


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    m = ap.add_mutually_exclusive_group(required=True)
    m.add_argument("--sweep", type=lambda s: [float(x) for x in s.split(",")])
    m.add_argument("--export", action="store_true")
    m.add_argument("--sensitivity", action="store_true")
    m.add_argument("--greedy", action="store_true")
    m.add_argument("--gate", action="store_true")
    ap.add_argument("--weights", choices=sorted(S.WEIGHT_RECIPES), required=True)
    ap.add_argument("--alpha", type=float)
    ap.add_argument("--exclude-file", type=Path, help="JSON list of MatMul node names kept fp32 (R2)")
    ap.add_argument("--fp32-dir", type=Path, default=FP32_DIR)
    ap.add_argument("--models", type=Path, default=DATA / "models")
    ap.add_argument("--work", type=Path, default=DATA / "work")
    ap.add_argument("--calib", type=Path, default=CALIB)
    ap.add_argument("--select", type=Path, default=SELECT)
    ap.add_argument("--gate-sample", type=Path, default=P.SAMPLE)
    ap.add_argument("--jina-fp32", type=Path, default=P.WORK / "models" / "jina-v2-base-code-fp32-r")
    ap.add_argument("--jina-int8", type=Path, default=P.WORK / "models" / "jina-v2-base-code-int8")
    ap.add_argument("--limit", type=int, help="smoke runs only: the first N rows of each sample")
    a = ap.parse_args(argv)
    if (a.export or a.sensitivity or a.greedy) and a.alpha is None:
        ap.error("--alpha is required")
    c = Ctx(a)
    if a.sweep:
        return cmd_sweep(c, a.sweep, a.weights)
    if a.export:
        exclude = json.loads(a.exclude_file.read_text()) if a.exclude_file else []
        return cmd_export(c, a.alpha, a.weights, exclude)
    if a.sensitivity:
        return cmd_sensitivity(c, a.alpha, a.weights)
    if a.greedy:
        return cmd_greedy(c, a.alpha, a.weights)
    return cmd_gate(c, a.weights)


if __name__ == "__main__":
    os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
    sys.exit(main())
