"""GM-736: per-channel smoothing before CodeRankEmbed's dynamic int8 quantization.

docs/design/GM-736-cre-int8.md, section 1.1. For every distinct input X of a
MatMul whose B is a 2-D constant, and every input channel j:
    a_j = max |X_j| over the calibration texts,
    w_j = max |W[j, :]| over every MatMul that reads X,
    s_j = a_j^alpha / w_j^(1 - alpha).
The graph gets X' = X * (1/s) (one Mul per X) and each consumer's W[j, :] *= s_j,
which is the identity in fp32 up to rounding; then the int8 recipe runs on it.
"""

import hashlib
import sys
import tempfile
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "gm703"))

import export_coderankembed as E  # noqa: E402

FLOOR = 1e-5
WEIGHT_RECIPES = {
    "s7": dict(E.QUANTIZE_ARGS),
    "u8": {"weight_type": "QUInt8", "per_channel": True, "reduce_range": False, "extra_options": {"WeightSymmetric": True}},
}


def const_matmuls(model):
    """{input tensor name: [MatMul nodes]} for every MatMul whose B is a 2-D initializer."""
    inits = {t.name: t for t in model.graph.initializer}
    users = {}
    for n in model.graph.node:
        if n.op_type == "MatMul" and n.input[1] in inits and len(inits[n.input[1]].dims) == 2:
            users.setdefault(n.input[0], []).append(n)
    return users


def weights_by_input(model):
    from onnx import numpy_helper

    inits = {t.name: t for t in model.graph.initializer}
    return {x: [numpy_helper.to_array(inits[n.input[1]]) for n in ns] for x, ns in const_matmuls(model).items()}


def collect_absmax(fp32_path, ids_rows):
    """{tensor: per-channel max |x|} over every token of `ids_rows`, one text per run (no padding)."""
    import onnx
    import onnxruntime as ort

    model = onnx.load(str(fp32_path))
    names = sorted(const_matmuls(model))
    for x in names:
        model.graph.output.append(onnx.helper.make_tensor_value_info(x, onnx.TensorProto.FLOAT, None))
    so = ort.SessionOptions()
    so.log_severity_level = 3
    sess = ort.InferenceSession(model.SerializeToString(), so, providers=["CPUExecutionProvider"])
    del model
    amax = {}
    for ids in ids_rows:
        r = np.asarray([ids], dtype=np.int64)
        for x, v in zip(names, sess.run(names, {"input_ids": r, "attention_mask": np.ones_like(r)})):
            m = np.abs(v.reshape(-1, v.shape[-1])).max(0).astype(np.float32)
            amax[x] = m if x not in amax else np.maximum(amax[x], m)
    return amax


def smoothing_factors(absmax, weights, alpha):
    """{tensor: s} with s_j = a_j^alpha / w_j^(1-alpha); w_j is the max over all of the tensor's consumers."""
    out = {}
    for x, ws in weights.items():
        w = np.max([np.abs(np.asarray(W)).max(1) for W in ws], axis=0)
        a = np.asarray(absmax[x], dtype=np.float64)
        s = np.maximum(a, FLOOR) ** alpha / np.maximum(w.astype(np.float64), FLOOR) ** (1 - alpha)
        out[x] = s.astype(np.float32)
    return out


def factors_sha256(factors):
    h = hashlib.sha256()
    for x in sorted(factors):
        h.update(x.encode() + b"\0")
        h.update(np.ascontiguousarray(factors[x], dtype="<f4").tobytes())
    return h.hexdigest()


def apply_smoothing(fp32_path, out_path, factors):
    """Writes the smoothed fp32 graph; returns the number of Mul nodes added."""
    import onnx
    from onnx import helper, numpy_helper

    g = onnx.load(str(fp32_path))
    inits = {t.name: t for t in g.graph.initializer}
    users = const_matmuls(g)
    if set(users) != set(factors):
        raise ValueError(f"factors for {sorted(set(factors) ^ set(users))[:3]}... do not match the graph's MatMul inputs")
    muls = []
    for x, ns in users.items():
        s = np.asarray(factors[x], dtype=np.float32)
        inv, out = x + "_sq_inv", x + "_sq"
        g.graph.initializer.append(numpy_helper.from_array((1.0 / s).astype(np.float32), inv))
        muls.append((x, helper.make_node("Mul", [x, inv], [out], name=x + "_sq_mul")))
        for n in ns:
            w = numpy_helper.to_array(inits[n.input[1]])
            inits[n.input[1]].CopyFrom(numpy_helper.from_array((w * s[:, None]).astype(np.float32), n.input[1]))
            n.input[0] = out
    # Each Mul goes right after its producer, so the node list stays topologically sorted.
    nodes = list(g.graph.node)
    produced = {o: i for i, n in enumerate(nodes) for o in n.output}
    for x, mul in sorted(muls, key=lambda m: -produced.get(m[0], -1)):
        nodes.insert(produced.get(x, -1) + 1, mul)
    del g.graph.node[:]
    g.graph.node.extend(nodes)
    onnx.save(g, str(out_path))
    return len(muls)


def quantize(fp32_path, int8_path, weights="s7", nodes_to_exclude=()):
    if weights == "s7":
        E.quantize_int8(fp32_path, int8_path, nodes_to_exclude)
        return
    from onnxruntime.quantization import QuantType, quantize_dynamic

    r = WEIGHT_RECIPES[weights]
    quantize_dynamic(
        str(fp32_path),
        str(int8_path),
        weight_type=getattr(QuantType, r["weight_type"]),
        per_channel=r["per_channel"],
        reduce_range=r["reduce_range"],
        extra_options=dict(r["extra_options"]),
        nodes_to_exclude=list(nodes_to_exclude) or None,
    )


def smoothed_int8(fp32_path, int8_path, alpha, absmax, weights="s7", exclude=()):
    """Smoothing (alpha, calibration `absmax`) + the int8 recipe of `weights`.
    Returns {"alpha", "weights", "smoothing_sha256", "mul_nodes", "excluded"}."""
    import onnx

    if weights not in WEIGHT_RECIPES:
        raise ValueError(f"weights {weights!r}: one of {sorted(WEIGHT_RECIPES)}")
    factors = smoothing_factors(absmax, weights_by_input(onnx.load(str(fp32_path))), alpha)
    with tempfile.TemporaryDirectory(prefix="gm736-sq-") as tmp:
        smoothed = Path(tmp) / "smoothed.onnx"
        muls = apply_smoothing(fp32_path, smoothed, factors)
        quantize(smoothed, int8_path, weights, exclude)
    return {
        "alpha": alpha,
        "weights": weights,
        "smoothing_sha256": factors_sha256(factors),
        "mul_nodes": muls,
        "excluded": sorted(exclude),
    }
