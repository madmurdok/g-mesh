"""GM-703: export nomic-ai/CodeRankEmbed to fp32 and int8 ONNX.

docs/design/GM-703-coderankembed.md, sections 3.2 and 3.4. The model has no
upstream ONNX, so it is traced here with `torch.onnx.export` from the pinned
revision. The repository's custom modeling code is run only after its sha256
matches the one pinned below.

    export_coderankembed.py --fetch          # download the pinned files, check the code
    export_coderankembed.py [--out DIR]      # fp32 + int8 export, EXPORT.json

Runs with HF_HOME pointing at the gm703 data dir; after --fetch every step is
offline (HF_HUB_OFFLINE=1). Output, per precision:
    <out>/coderankembed-fp32/{model.onnx,tokenizer.json,EXPORT.json}
    <out>/coderankembed-int8/{model.onnx,tokenizer.json,EXPORT.json}
"""

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

REPO = "nomic-ai/CodeRankEmbed"
REVISION = "3c4b60807d71f79b43f3c4363786d9493691f8b1"
# The repository's own code, run with trust_remote_code (design note, section 2).
CODE_SHA256 = {
    "modeling_hf_nomic_bert.py": "502ccfb9c2d5dac976109ac2f04dc3125d8540329e5d4af92bc587d6dc65edcd",
    "configuration_hf_nomic_bert.py": "8632792e922e62ab1a6feaab15baf406e223c0547a21de23ab4f520b2b36d674",
}
TOKENIZER_SHA256 = "91f1def9b9391fdabe028cd3f3fcc4efd34e5d1f08c3bf2de513ebb5911a1854"
QUERY_PREFIX = "Represent this query for searching relevant code: "
# Rotary cos/sin tables are built to this many positions before tracing, so
# the traced graph slices them by the runtime length (section 3.2, step 2).
ROTARY_POSITIONS = 2048
OPSET = 17
INPUT_NAMES = ["input_ids", "attention_mask"]
OUTPUT_NAME = "last_hidden_state"
HIDDEN = 768
# The shipped jina int8 file's recipe (section 3.4).
QUANTIZE_ARGS = {"weight_type": "QInt8", "per_channel": True, "reduce_range": True}
# ORT's zero point for symmetric QUInt8 weights (WeightSymmetric), every channel.
U8_ZERO_POINT = 128
DEFAULT_OUT = Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm703/models"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class CodeMismatch(Exception):
    pass


def verify_custom_code(snapshot, expected=CODE_SHA256):
    """Refuse to run remote code whose sha256 is not the pinned one."""
    for name, want in expected.items():
        path = Path(snapshot) / name
        if not path.exists():
            raise CodeMismatch(f"{name} missing in {snapshot}")
        got = sha256(path)
        if got != want:
            raise CodeMismatch(f"{name}: sha256 {got}, pinned {want}")


def snapshot_dir(fetch=False):
    from huggingface_hub import snapshot_download

    return Path(
        snapshot_download(REPO, revision=REVISION, local_files_only=not fetch, ignore_patterns=["*.bin", "*.onnx"])
    )


def load_model(snapshot, n_layer=None):
    """The pinned model (or, with n_layer, a random-init one of the same config),
    loaded only after the custom-code check."""
    import torch
    from transformers import AutoConfig, AutoModel

    verify_custom_code(snapshot)
    if n_layer is None:
        model = AutoModel.from_pretrained(str(snapshot), trust_remote_code=True, add_pooling_layer=False)
    else:
        config = AutoConfig.from_pretrained(str(snapshot), trust_remote_code=True)
        config.n_layer = n_layer
        torch.manual_seed(703)
        model = AutoModel.from_config(config, trust_remote_code=True, add_pooling_layer=False)
    return model.eval().float()


def prebuild_rotary(model, positions=ROTARY_POSITIONS):
    """Build every layer's rotary tables for `positions` before tracing; returns
    how many tables were built."""
    import torch

    built = 0
    for module in model.modules():
        if hasattr(module, "_update_cos_sin_cache") and hasattr(module, "_cos_cached"):
            module._update_cos_sin_cache(positions, device=torch.device("cpu"), dtype=torch.float32)
            built += 1
    if built == 0:
        raise RuntimeError("no rotary embedding found to pre-build")
    return built


def _wrapper(model):
    import torch

    class Encoder(torch.nn.Module):
        def __init__(self, inner):
            super().__init__()
            self.inner = inner

        def forward(self, input_ids, attention_mask):
            # No token_type_ids: the model fills zeros, as the Rust loader's
            # token_type_ids = false path expects.
            return self.inner(input_ids=input_ids, attention_mask=attention_mask).last_hidden_state

    return Encoder(model).eval()


def export_onnx(model, path, opset=OPSET, dynamic=True, prebuild=True):
    """Steps 2-5 of section 3.2. The dummy batch is 2 x 16 with one padded row."""
    import onnx
    import torch

    if prebuild:
        prebuild_rotary(model)
    ids = torch.randint(1000, 2000, (2, 16), generator=torch.Generator().manual_seed(703))
    ids[:, 0] = 101
    mask = torch.ones_like(ids)
    mask[1, 10:] = 0
    ids[1, 10:] = 0
    axes = {0: "batch", 1: "sequence"}
    with torch.no_grad():
        torch.onnx.export(
            _wrapper(model),
            (ids, mask),
            str(path),
            opset_version=opset,
            input_names=INPUT_NAMES,
            output_names=[OUTPUT_NAME],
            dynamic_axes={n: axes for n in INPUT_NAMES + [OUTPUT_NAME]} if dynamic else None,
            do_constant_folding=True,
        )
    proto = onnx.load(str(path))
    onnx.checker.check_model(proto)
    inputs = [i.name for i in proto.graph.input]
    outputs = [o.name for o in proto.graph.output]
    if inputs != INPUT_NAMES or outputs != [OUTPUT_NAME]:
        raise RuntimeError(f"graph io {inputs} -> {outputs}")
    last = proto.graph.output[0].type.tensor_type.shape.dim[-1]
    width = model.config.n_embd
    if last.dim_value != width:
        raise RuntimeError(f"output trailing dimension {last}, expected {width}")


def quantize_int8(fp32_path, int8_path, nodes_to_exclude=()):
    """The shipped recipe; `nodes_to_exclude` (node names) stay fp32."""
    from onnxruntime.quantization import QuantType, quantize_dynamic

    quantize_dynamic(
        str(fp32_path),
        str(int8_path),
        weight_type=getattr(QuantType, QUANTIZE_ARGS["weight_type"]),
        per_channel=QUANTIZE_ARGS["per_channel"],
        reduce_range=QUANTIZE_ARGS["reduce_range"],
        nodes_to_exclude=list(nodes_to_exclude) or None,
    )


def int8_fingerprint(path, hidden=HIDDEN, allowed_fp32=0, weights="s7"):
    """Q-F (section 3.4): the shipped jina recipe's marks. Returns a dict with
    `ok` and the counts behind it.

    weights="s7": signed 8-bit tables holding 7-bit values (reduce_range).
    weights="u8": unsigned 8-bit tables whose zero points all equal U8_ZERO_POINT,
    and no signed 8-bit table (the AVX2 U8S8 kernel saturates on signed 8-bit weights).
    `allowed_fp32` is the number of big MatMuls deliberately left fp32."""
    if weights not in ("s7", "u8"):
        raise ValueError(f"weights {weights!r}: s7 or u8")
    import numpy as np
    import onnx
    from onnx import numpy_helper

    proto = onnx.load(str(path))
    graph = proto.graph
    inits = {t.name: t for t in graph.initializer}
    ops = [n.op_type for n in graph.node]
    int8_min, int8_max, per_channel_zp, int8_weights = 0, 0, 0, 0
    uint8_weights, uint8_min, uint8_max, uint8_per_channel_zp = 0, 255, 0, 0
    uint8_zero_points = set()
    for t in graph.initializer:
        if t.data_type == onnx.TensorProto.INT8:
            a = numpy_helper.to_array(t)
            if a.ndim == 2:
                int8_weights += 1
                int8_min = min(int8_min, int(a.min()))
                int8_max = max(int8_max, int(a.max()))
            elif a.ndim == 1 and a.shape[0] > 1:
                per_channel_zp += 1
        elif t.data_type == onnx.TensorProto.UINT8:
            a = numpy_helper.to_array(t)
            if a.ndim == 2:
                uint8_weights += 1
                uint8_min = min(uint8_min, int(a.min()))
                uint8_max = max(uint8_max, int(a.max()))
            elif t.name.endswith("_zero_point"):
                uint8_zero_points.update(int(v) for v in a.reshape(-1))
                if a.ndim == 1 and a.shape[0] > 1:
                    uint8_per_channel_zp += 1
    # An embedding table is quantized when its Gather reads an 8-bit table
    # (a DequantizeLinear follows the Gather).
    gather_quantized = 0
    for n in graph.node:
        if n.op_type == "Gather" and n.input[0] in inits:
            t = inits[n.input[0]]
            if t.data_type in (onnx.TensorProto.UINT8, onnx.TensorProto.INT8) and len(t.dims) == 2:
                gather_quantized += 1
    big_shapes = {(hidden, hidden), (hidden, 3 * hidden), (hidden, 4 * hidden), (4 * hidden, hidden)}
    fp32_matmul = 0
    for n in graph.node:
        if n.op_type == "MatMul":
            for i in n.input:
                t = inits.get(i)
                if t is not None and t.data_type == onnx.TensorProto.FLOAT and tuple(t.dims) in big_shapes:
                    fp32_matmul += 1
    result = {
        "matmul_integer": ops.count("MatMulInteger"),
        "dynamic_quantize_linear": ops.count("DynamicQuantizeLinear"),
        "int8_weights": int8_weights,
        "int8_min": int8_min,
        "int8_max": int8_max,
        "per_channel_zero_points": per_channel_zp,
        "gather_quantized": gather_quantized,
        "fp32_matmul_on_big_weights": fp32_matmul,
        "allowed_fp32": allowed_fp32,
        "weights": weights,
        "opset": [{"domain": o.domain, "version": o.version} for o in proto.opset_import],
    }
    common = result["matmul_integer"] > 0 and gather_quantized >= 1 and fp32_matmul == allowed_fp32
    if weights == "s7":
        result["ok"] = bool(
            common and int8_weights > 0 and -64 <= int8_min and int8_max <= 64 and per_channel_zp >= int8_weights
        )
    else:
        result.update(
            uint8_weights=uint8_weights,
            uint8_min=uint8_min,
            uint8_max=uint8_max,
            uint8_per_channel_zero_points=uint8_per_channel_zp,
            uint8_zero_point_values=sorted(uint8_zero_points),
        )
        result["ok"] = bool(
            common
            and int8_weights == 0
            and uint8_weights > 0
            and uint8_per_channel_zp >= uint8_weights - gather_quantized
            and uint8_zero_points == {U8_ZERO_POINT}
        )
    return result


def versions():
    import onnx
    import onnxruntime
    import torch
    import transformers

    return {
        "torch": torch.__version__,
        "onnx": onnx.__version__,
        "onnxruntime": onnxruntime.__version__,
        "transformers": transformers.__version__,
        "python": sys.version.split()[0],
    }


def write_export_json(out_dir, record):
    with open(Path(out_dir) / "EXPORT.json", "w") as f:
        json.dump(record, f, indent=2, sort_keys=True)
        f.write("\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fetch", action="store_true", help="download the pinned files and check the code, then stop")
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--opset", type=int, default=OPSET)
    args = ap.parse_args()

    snapshot = snapshot_dir(fetch=args.fetch)
    try:
        verify_custom_code(snapshot)
    except CodeMismatch as e:
        sys.exit(f"custom code check failed: {e}")
    tok = snapshot / "tokenizer.json"
    if sha256(tok) != TOKENIZER_SHA256:
        sys.exit(f"tokenizer.json sha256 {sha256(tok)}, pinned {TOKENIZER_SHA256}")
    weights = snapshot / "model.safetensors"
    if args.fetch:
        print(f"snapshot {snapshot}")
        print(f"model.safetensors {weights.stat().st_size} bytes sha256 {sha256(weights)}")
        return

    fp32_dir = args.out / "coderankembed-fp32"
    int8_dir = args.out / "coderankembed-int8"
    for d in (fp32_dir, int8_dir):
        d.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(tok, d / "tokenizer.json")

    model = load_model(snapshot)
    fp32 = fp32_dir / "model.onnx"
    export_onnx(model, fp32, opset=args.opset)
    int8 = int8_dir / "model.onnx"
    quantize_int8(fp32, int8)

    common = {
        "repo": REPO,
        "revision": REVISION,
        "code_sha256": CODE_SHA256,
        "model_safetensors_sha256": sha256(weights),
        "tokenizer_sha256": TOKENIZER_SHA256,
        "versions": versions(),
        "opset": args.opset,
        "rotary_positions": ROTARY_POSITIONS,
        "inputs": INPUT_NAMES,
        "output": OUTPUT_NAME,
        "pooling": "cls",
        "query_prefix": QUERY_PREFIX,
    }
    fp32_sha = sha256(fp32)
    write_export_json(
        fp32_dir,
        {**common, "precision": "fp32", "model_onnx_sha256": fp32_sha, "model_onnx_bytes": fp32.stat().st_size},
    )
    write_export_json(
        int8_dir,
        {
            **common,
            "precision": "int8",
            "quantize": {"function": "onnxruntime.quantization.quantize_dynamic", **QUANTIZE_ARGS},
            "fp32_model_onnx_sha256": fp32_sha,
            "model_onnx_sha256": sha256(int8),
            "model_onnx_bytes": int8.stat().st_size,
        },
    )
    for d in (fp32_dir, int8_dir):
        rec = json.loads((d / "EXPORT.json").read_text())
        print(f"{d.name}: model.onnx {rec['model_onnx_bytes']} bytes sha256 {rec['model_onnx_sha256']}")


if __name__ == "__main__":
    os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")
    main()
