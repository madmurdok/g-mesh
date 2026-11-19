#!/usr/bin/env python3
"""ONNX export, re-quantisation and parity checks for the GM-730 routes
(docs/design/GM-730-gpu-training-routes.md, sections 2 and 4, M2 of 9.1).

- `export_onnx`: torch -> ONNX, opset 17, dynamic (batch, sequence) axes, the
  base's own input names, output `last_hidden_state` (embedders; pooling stays
  in Rust) or `logits` (cross-encoder).
- `export_dir`: export_onnx plus the snapshot's tokenizer files, copied
  unchanged (their sha256 must equal the snapshot's).
- `quantize(recipe, src, dst)`:
  jina: `quantize_dynamic(QInt8, per_channel, reduce_range)`, which reproduces
  the official `model_quantized.onnx` from the official fp32 file bit for bit.
  l6: the ORT graph of `quantize_dynamic(QUInt8, per_channel, reduce_range=False,
  WeightSymmetric, MatMulConstBOnly=False)`, then every `<w>_quantized`,
  `_scale` and `_zero_point` rewritten with the closed form of the official
  `model_quint8_avx2.onnx`: per output channel s = maxabs / 127,
  q = rint(w / s) + 127, zero point 127 (ORT's own symmetric uint8 uses zero
  point 128 and scale 2 maxabs / 255, so it never matches).
- `parity_embed` / `parity_ce` (C-P): torch vs exported ONNX, mean-pooled cosine
  distance or logit difference.
- `compare_quantized` (C-R): op counts, quantised weights matched by node,
  multisets of the non-float initializers and of the scales, byte for byte.

usage: export.py recipe-control --model jina|gte|l6 [--n 1000] [--out DIR] [--plant flip-per-channel|flip-reduce-range]
       export.py plant-row --src model.onnx --dst planted.onnx [--factor 1.01]   (C-F input for C-P)
"""

import argparse
import collections
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import snapshots as S  # noqa: E402

OPSET = 17
MAX_TOKENS = 512
# truncation of the parity checks: the eval's max_tokens of the model's row (jina 1024), else MAX_TOKENS
PARITY_TOKENS = {"jina": 1024, "gte": 512, "l6": MAX_TOKENS}
# model -> (snapshot name, kind, ONNX input names, ONNX output name)
MODELS = {
    "jina": ("jina", "embed", ["input_ids", "attention_mask"], "last_hidden_state"),
    "gte": ("gte-small", "embed", ["input_ids", "attention_mask", "token_type_ids"], "last_hidden_state"),
    "l6": ("l6", "ce", ["input_ids", "attention_mask", "token_type_ids"], "logits"),
}
TOKENIZER_FILES = ("tokenizer.json", "config.json")
RECIPES = {
    "jina": {"weight_type": "QInt8", "per_channel": True, "reduce_range": True, "extra": {}, "closed_form": False},
    "l6": {"weight_type": "QUInt8", "per_channel": True, "reduce_range": False,
           "extra": {"WeightSymmetric": True, "MatMulConstBOnly": False}, "closed_form": True},
}
PLANTS = {"flip-per-channel": "per_channel", "flip-reduce-range": "reduce_range"}
# C-R and C-P pass rules (section 2)
TOL_EMBED = 1e-4       # max cosine distance, export vs reference fp32
TOL_CE = 1e-4          # max |logit diff|, export vs reference fp32
TOL_QUANT_EMBED = 1e-5  # max cosine distance, our int8 vs official int8
TOL_QUANT_CE = 1e-5    # max |logit diff| at chunk 1, our quint8 vs official quint8
CLOSED_ZP = 127
CLOSED_QMAX = 127.0


def sha256_file(p):
    return S.sha256_file(p)


# ---- torch side ------------------------------------------------------------------
def load_torch(model, snap_dir=None):
    """The base model of `model` from its snapshot, fp32, eval mode, offline."""
    import torch
    from transformers import AutoModel, AutoModelForSequenceClassification
    snap, kind, _, _ = MODELS[model]
    d = str(snap_dir or S.path_of(snap))
    cls = AutoModelForSequenceClassification if kind == "ce" else AutoModel
    return cls.from_pretrained(d, trust_remote_code=(model == "jina"), torch_dtype=torch.float32).eval()


class Wrap:
    """forward(*inputs in `names` order) -> logits or last_hidden_state; built lazily so
    importing this module needs no torch."""

    def __new__(cls, model, kind, names):
        import torch

        class _W(torch.nn.Module):
            def __init__(self):
                super().__init__()
                self.m = model

            def forward(self, *args):
                out = self.m(**dict(zip(names, args)))
                return out.logits if kind == "ce" else out.last_hidden_state

        return _W()


def export_onnx(model, kind, names, output, dst, opset=OPSET):
    """torch.onnx.export of `model` to dst; returns the file's sha256."""
    import torch
    w = Wrap(model, kind, names).eval()
    dummy = tuple(torch.ones((2, 8), dtype=torch.long) if n != "token_type_ids" else torch.zeros((2, 8), dtype=torch.long)
                  for n in names)
    axes = {n: {0: "batch", 1: "sequence"} for n in names}
    axes[output] = {0: "batch"} if kind == "ce" else {0: "batch", 1: "sequence"}
    Path(dst).parent.mkdir(parents=True, exist_ok=True)
    with torch.no_grad():
        torch.onnx.export(w, dummy, str(dst), input_names=list(names), output_names=[output],
                          dynamic_axes=axes, opset_version=opset, do_constant_folding=True)
    return sha256_file(dst)


def copy_tokenizer(snap_dir, out_dir, files=TOKENIZER_FILES):
    """Copy the tokenizer files unchanged; {file: sha256}, each equal to the snapshot's."""
    out = {}
    for f in files:
        src = Path(snap_dir) / f
        if not src.exists():
            continue
        shutil.copyfile(src, Path(out_dir) / f)
        a, b = sha256_file(src), sha256_file(Path(out_dir) / f)
        if a != b:
            raise SystemExit(f"{f}: copy sha256 {b} != snapshot {a}")
        out[f] = a
    return out


def export_dir(torch_model, model, out_dir, snap_dir=None):
    """out_dir/model.onnx + tokenizer files; returns {file: sha256}."""
    snap, kind, names, output = MODELS[model]
    snap_dir = Path(snap_dir or S.path_of(snap))
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    files = {"model.onnx": export_onnx(torch_model, kind, names, output, out_dir / "model.onnx")}
    files.update(copy_tokenizer(snap_dir, out_dir))
    return files


# ---- quantisation ----------------------------------------------------------------
def quantize(recipe, src, dst, plant=None):
    """Quantise fp32 `src` to `dst` with RECIPES[recipe]; `plant` flips one flag (C-F)."""
    from onnxruntime.quantization import QuantType, quantize_dynamic
    r = dict(RECIPES[recipe])
    if plant:
        key = PLANTS[plant]
        r[key] = not r[key]
    Path(dst).parent.mkdir(parents=True, exist_ok=True)
    quantize_dynamic(str(src), str(dst), weight_type=getattr(QuantType, r["weight_type"]),
                     per_channel=r["per_channel"], reduce_range=r["reduce_range"], extra_options=dict(r["extra"]))
    if r["closed_form"]:
        rewrite_closed_form(src, dst)
    return sha256_file(dst)


def _per_channel(w, s, axis):
    """s broadcast against w along `axis` (None = per tensor)."""
    if axis is None:
        return s.reshape(())
    shape = [1] * w.ndim
    shape[axis] = -1
    return s.reshape(shape)


def closed_form(w, axis):
    """(q uint8, s float32, zp uint8) of the official quint8 file for fp32 weight w."""
    w = np.asarray(w, np.float32)
    if axis is None:
        maxabs = np.abs(w).max().reshape(1)
    else:
        maxabs = np.abs(w).max(axis=tuple(i for i in range(w.ndim) if i != axis))
    s = (maxabs / np.float32(CLOSED_QMAX)).astype(np.float32)
    s = np.where(s == 0, np.float32(1.0), s).astype(np.float32)
    q = np.clip(np.rint(w / _per_channel(w, s, axis)) + CLOSED_ZP, 0, 255).astype(np.uint8)
    return q, s, np.full(s.shape, CLOSED_ZP, np.uint8)


def _layout(w0, q, s, z):
    """The (fp32 weight in q's layout, channel axis) whose ORT dequantisation is closest
    to q: ORT stores Gemm(transB) weights transposed and a square weight fits both ways."""
    cands = [w0] + ([w0.T] if w0.ndim == 2 else [])
    best = None
    for w in cands:
        if w.shape != q.shape:
            continue
        axes = [None] if s.size == 1 else [a for a in range(q.ndim) if q.shape[a] == s.size]
        for axis in axes:
            deq = (q.astype(np.float32) - _per_channel(q, z.astype(np.float32), axis)) * _per_channel(q, s, axis)
            err = float(np.abs(deq - w).max())
            if best is None or err < best[0]:
                best = (err, w, axis)
    if best is None:
        raise SystemExit(f"no layout of fp32 {w0.shape} fits quantised {q.shape}")
    return best[1], best[2]


def rewrite_closed_form(fp32_path, q_path):
    """Replace every `<w>_quantized/_scale/_zero_point` of q_path with closed_form(w)."""
    import onnx
    from onnx import numpy_helper
    fp = {i.name: numpy_helper.to_array(i) for i in onnx.load(str(fp32_path)).graph.initializer}
    m = onnx.load(str(q_path))
    inits = {i.name: i for i in m.graph.initializer}
    n = 0
    for name in list(inits):
        if not name.endswith("_quantized"):
            continue
        base = name[: -len("_quantized")]
        if base not in fp:
            raise SystemExit(f"{q_path}: {name} has no fp32 tensor {base} in {fp32_path}")
        q0 = numpy_helper.to_array(inits[name])
        s0 = numpy_helper.to_array(inits[base + "_scale"])
        z0 = numpy_helper.to_array(inits[base + "_zero_point"])
        w, axis = _layout(fp[base].astype(np.float32), q0, s0.astype(np.float32).reshape(-1), z0.reshape(-1))
        q, s, z = closed_form(w, axis)
        for key, arr, like in ((name, q, q0), (base + "_scale", s.reshape(s0.shape), s0),
                               (base + "_zero_point", z.reshape(z0.shape), z0)):
            inits[key].CopyFrom(numpy_helper.from_array(arr.astype(like.dtype), key))
        n += 1
    onnx.save(m, str(q_path))
    return n


def _graph_summary(path):
    import onnx
    from onnx import numpy_helper
    g = onnx.load(str(path)).graph
    inits = {i.name: i for i in g.initializer}
    ops = collections.Counter(n.op_type for n in g.node)
    by_node = {}
    for n in g.node:
        if n.op_type == "MatMulInteger" and len(n.input) > 1 and n.input[1] in inits:
            by_node[n.name] = numpy_helper.to_array(inits[n.input[1]])
        if n.op_type == "Gather" and n.input[0] in inits and inits[n.input[0]].data_type != onnx.TensorProto.FLOAT:
            by_node[n.name] = numpy_helper.to_array(inits[n.input[0]])

    def digest(i):
        a = numpy_helper.to_array(i)
        return hashlib.sha256(f"{a.dtype}|{a.shape}|".encode() + a.tobytes()).hexdigest()

    nonfloat = collections.Counter(digest(i) for i in g.initializer if i.data_type != onnx.TensorProto.FLOAT)
    scales = collections.Counter(digest(i) for i in g.initializer if i.name.endswith("_scale"))
    return ops, by_node, nonfloat, scales


def compare_quantized(ours, official):
    """C-R on quantised graphs: op counts, quantised weights by node, and the multisets
    of non-float initializers and scales, byte for byte."""
    o_ops, o_q, o_nf, o_sc = _graph_summary(official)
    ops, q, nf, sc = _graph_summary(ours)
    common = sorted(set(q) & set(o_q))
    equal = [k for k in common if q[k].dtype == o_q[k].dtype and q[k].shape == o_q[k].shape
             and np.array_equal(q[k], o_q[k])]
    diffs = {}
    for k in common:
        if k in equal:
            continue
        a, b = q[k], o_q[k]
        diffs[k] = ([int(np.abs(a.astype(np.int32) - b.astype(np.int32)).max())] if a.shape == b.shape
                    else [list(a.shape), list(b.shape)])
    res = {"opsEqual": ops == o_ops,
           "opsDiff": {k: [ops.get(k, 0), o_ops.get(k, 0)] for k in sorted(set(ops) | set(o_ops)) if ops.get(k, 0) != o_ops.get(k, 0)},
           "quantTensors": [len(q), len(o_q)], "matchedByNode": len(common), "bitEqual": len(equal),
           "nonFloatMultisetEqual": nf == o_nf, "scaleMultisetEqual": sc == o_sc,
           "firstDiffs": dict(list(diffs.items())[:6])}
    res["pass"] = (res["opsEqual"] and len(q) == len(o_q) == len(common) == len(equal)
                   and res["nonFloatMultisetEqual"] and res["scaleMultisetEqual"])
    return res


# ---- inference and parity ------------------------------------------------------------
def tokenizer(snap_dir, max_len=MAX_TOKENS):
    from tokenizers import Tokenizer
    tok = Tokenizer.from_file(str(Path(snap_dir) / "tokenizer.json"))
    tok.enable_truncation(max_len)
    tok.no_padding()
    return tok


def batches(tok, items, batch):
    """Feeds of `batch` items each (texts, or (query, text) pairs), padded per batch with
    the tokenizer's pad id and masked."""
    pad = next((i for i in (tok.token_to_id(t) for t in ("[PAD]", "<pad>")) if i is not None), 0)
    enc = tok.encode_batch(list(items))
    for s in range(0, len(enc), batch):
        part = enc[s:s + batch]
        L = max(len(e.ids) for e in part)
        ids = np.full((len(part), L), pad, np.int64)
        mask = np.zeros((len(part), L), np.int64)
        tt = np.zeros((len(part), L), np.int64)
        for r, e in enumerate(part):
            n = len(e.ids)
            ids[r, :n], mask[r, :n], tt[r, :n] = e.ids, 1, e.type_ids
        yield {"input_ids": ids, "attention_mask": mask, "token_type_ids": tt}


def ort_session(path, threads=0):
    import onnxruntime as ort
    so = ort.SessionOptions()
    so.intra_op_num_threads = threads
    return ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])


def ort_forward(sess):
    names = [i.name for i in sess.get_inputs()]
    return lambda feed: sess.run(None, {k: feed[k] for k in names})[0]


def torch_forward(model, model_name):
    import torch
    _, kind, names, _ = MODELS[model_name]
    dev = next(model.parameters()).device

    def f(feed):
        with torch.no_grad():
            out = model(**{k: torch.from_numpy(feed[k]).to(dev) for k in names})
        return (out.logits if kind == "ce" else out.last_hidden_state).float().cpu().numpy()
    return f


def mean_pool(h, mask):
    m = mask[..., None].astype(np.float64)
    return (h.astype(np.float64) * m).sum(1) / np.maximum(m.sum(1), 1.0)


def max_cos_dist(fa, fb, tok, texts, batch=16):
    worst = 0.0
    for feed in batches(tok, texts, batch):
        a, b = mean_pool(fa(feed), feed["attention_mask"]), mean_pool(fb(feed), feed["attention_mask"])
        cos = (a * b).sum(1) / (np.linalg.norm(a, axis=1) * np.linalg.norm(b, axis=1))
        worst = max(worst, float((1.0 - cos).max()))
    return worst


def max_logit_diff(fa, fb, tok, pairs, batch=16):
    worst = 0.0
    for feed in batches(tok, pairs, batch):
        worst = max(worst, float(np.abs(fa(feed).reshape(-1) - fb(feed).reshape(-1)).max()))
    return worst


def parity_embed(torch_model, model_name, onnx_path, tok, texts, batch=16):
    """C-P for an embedder: max cosine distance of mean-pooled torch vs ONNX vectors."""
    d = max_cos_dist(torch_forward(torch_model, model_name), ort_forward(ort_session(onnx_path)), tok, texts, batch)
    return {"check": "C-P", "n": len(texts), "maxCosDist": d, "tol": TOL_EMBED, "pass": d <= TOL_EMBED}


def parity_ce(torch_model, onnx_path, tok, pairs, batch=16):
    """C-P for a cross-encoder: max |logit| difference torch vs ONNX."""
    d = max_logit_diff(torch_forward(torch_model, "l6"), ort_forward(ort_session(onnx_path)), tok, pairs, batch)
    return {"check": "C-P", "n": len(pairs), "maxAbsDiff": d, "tol": TOL_CE, "pass": d <= TOL_CE}


def plant_row(src, dst, factor=1.01):
    """C-F for C-P: row 0 of the largest 2-D float MatMul/Gemm weight scaled by `factor`
    (an embedding table's row 0 is the pad token, which no output reads)."""
    import onnx
    from onnx import numpy_helper
    m = onnx.load(str(src))
    used = {x for n in m.graph.node if n.op_type in ("MatMul", "Gemm") for x in n.input}
    cand = [i for i in m.graph.initializer
            if i.data_type == onnx.TensorProto.FLOAT and len(i.dims) == 2 and i.name in used]
    init = max(cand, key=lambda i: int(np.prod(i.dims)))
    a = numpy_helper.to_array(init).copy()
    a[0] *= np.float32(factor)
    init.CopyFrom(numpy_helper.from_array(a, init.name))
    onnx.save(m, str(dst))
    return init.name


# ---- C-R inputs ---------------------------------------------------------------------
def gm456_root():
    return Path(os.environ.get("GM456_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm456"))


def default_texts(n):
    """n structured node texts, round robin over the GM-456 repos' embed-structured render."""
    d = sorted((gm456_root() / "v0.4-full" / "texts").glob("embed-structured@*"))[-1]
    per = [[json.loads(x)["text"] for x in open(p) if x.strip()] for p in sorted(d.glob("*.jsonl"))]
    out, i = [], 0
    while len(out) < n and any(i < len(p) for p in per):
        out += [p[i] for p in per if i < len(p)]
        i += 1
    return out[:n]


def default_pairs(n):
    """n (query, structured text) pairs: GM-456 dev queries with texts of their repo, round robin."""
    d = sorted((gm456_root() / "v0.4-full" / "texts").glob("embed-structured@*"))[-1]
    qs = [json.loads(x) for x in open(gm456_root() / "v0.4-full" / "queries.jsonl") if x.strip()]
    texts = {p.stem: [json.loads(x)["text"] for x in open(p) if x.strip()] for p in d.glob("*.jsonl")}
    out = []
    for i, q in enumerate(q for q in qs if q["split"] == "dev"):
        t = texts.get(q["repo"]) or []
        if t:
            out.append((q["text"], t[i % len(t)]))
        if len(out) == n:
            break
    return out


def official_int8_jina():
    return Path(os.environ.get("GM730_JINA_INT8", Path.home() / "Projects/ClaudeProjects/g-mesh-eval-data/v2/work/models/jina-v2-base-code-int8/model.onnx"))


def recipe_control(model, n, out, plant=None, texts=None, pairs=None):
    """C-R for one model; one result dict per check, each with `pass`."""
    snap_name, kind, _, _ = MODELS[model]
    snap = S.path_of(snap_name)
    bad = S.verify(snap_name)
    if bad:
        raise SystemExit(f"snapshot {snap_name}: {bad}")
    out = Path(out) / model
    tok = tokenizer(snap, PARITY_TOKENS[model])
    res = []
    if plant is None:
        tm = load_torch(model, snap)
        files = export_dir(tm, model, out / "fp32", snap)
        del tm
        ours = ort_forward(ort_session(out / "fp32" / "model.onnx"))
        ref = ort_forward(ort_session(snap / "onnx" / "model.onnx"))
        if kind == "embed":
            items = texts or default_texts(n)
            d = max_cos_dist(ours, ref, tok, items)
            res.append({"check": "C-R export", "model": model, "n": len(items), "maxCosDist": d, "tol": TOL_EMBED,
                        "pass": d <= TOL_EMBED, "files": files})
        else:
            items = pairs or default_pairs(n)
            d = max_logit_diff(ours, ref, tok, items)
            res.append({"check": "C-R export", "model": model, "n": len(items), "maxAbsDiff": d, "tol": TOL_CE,
                        "pass": d <= TOL_CE, "files": files})
    if model in RECIPES:
        official = official_int8_jina() if model == "jina" else snap / "onnx" / "model_quint8_avx2.onnx"
        dst = out / ("int8" if plant is None else f"int8-{plant}") / "model.onnx"
        sha = quantize(model, snap / "onnx" / "model.onnx", dst, plant)
        copy_tokenizer(snap, dst.parent)
        r = dict(compare_quantized(dst, official), check="C-R quant", model=model, plant=plant, sha256=sha,
                 officialSha256=sha256_file(official))
        a, b = ort_forward(ort_session(dst)), ort_forward(ort_session(official))
        if kind == "embed":
            items = texts or default_texts(n)
            r["maxCosDist"] = max_cos_dist(a, b, tok, items)
            r["pass"] = r["pass"] and r["maxCosDist"] <= TOL_QUANT_EMBED
        else:
            items = pairs or default_pairs(n)
            r["maxAbsDiff"] = max_logit_diff(a, b, tok, items, batch=1)
            r["pass"] = r["pass"] and r["maxAbsDiff"] <= TOL_QUANT_CE
        r["n"] = len(items)
        res.append(r)
    return res


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("recipe-control", help="C-R on the laptop")
    r.add_argument("--model", required=True, choices=sorted(MODELS))
    r.add_argument("--n", type=int, default=1000)
    r.add_argument("--out", type=Path, default=S.data_root() / "cr")
    r.add_argument("--plant", choices=sorted(PLANTS), help="C-F: quantise with this flag flipped (must fail); skips the export")
    r.add_argument("--json", type=Path, help="append the result lines here")
    p = sub.add_parser("plant-row", help="C-F input for C-P")
    p.add_argument("--src", required=True, type=Path)
    p.add_argument("--dst", required=True, type=Path)
    p.add_argument("--factor", type=float, default=1.01)
    a = ap.parse_args(argv)
    if a.cmd == "plant-row":
        print(plant_row(a.src, a.dst, a.factor))
        return 0
    res = recipe_control(a.model, a.n, a.out, a.plant)
    for x in res:
        line = json.dumps(x, sort_keys=True)
        print(line, flush=True)
        if a.json:
            with open(a.json, "a") as f:
                f.write(line + "\n")
    ok = all(x["pass"] for x in res)
    print(f"C-R {a.model}{' plant ' + a.plant if a.plant else ''}: {'pass' if ok else 'FAIL'}", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
