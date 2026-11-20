"""GM-703 export tests (docs/design/GM-703-coderankembed.md, section 9: T1-T4, Q-F).

Run: <gm703 venv>/bin/python -m unittest discover -s eval/embedding/gm703 -p 'test_gm703_*.py'

The export tests use a 2-layer random-init NomicBERT of the pinned config. They
need the pinned revision's *code and config* from the local HF cache
(g-mesh-data/gm703/hf; a few KB), never the 547 MB weights, and skip with a
message when the cache or torch/onnx is missing. Offline: nothing is downloaded.
"""

import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

os.environ.setdefault("HF_HOME", str(Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm703/hf"))
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

import export_coderankembed as E  # noqa: E402


def _deps_or_skip():
    try:
        import numpy  # noqa: F401
        import onnx  # noqa: F401
        import onnxruntime  # noqa: F401
        import torch  # noqa: F401
        import transformers  # noqa: F401
    except ImportError as e:
        raise unittest.SkipTest(f"needs the gm703 venv (torch, onnx, onnxruntime, transformers): {e}")
    try:
        return E.snapshot_dir()
    except Exception as e:  # noqa: BLE001 - any cache miss means no code to load
        raise unittest.SkipTest(f"pinned CodeRankEmbed code not in the local HF cache ({os.environ['HF_HOME']}): {e}")


class TinyExport(unittest.TestCase):
    """T1-T3: one 2-layer export and its int8 quantization, shared by the tests."""

    @classmethod
    def setUpClass(cls):
        cls.snapshot = _deps_or_skip()
        import onnxruntime as ort

        cls.tmp = tempfile.mkdtemp(prefix="gm703-tiny-")
        cls.model = E.load_model(cls.snapshot, n_layer=2)
        cls.fp32 = Path(cls.tmp) / "fp32.onnx"
        cls.int8 = Path(cls.tmp) / "int8.onnx"
        E.export_onnx(cls.model, cls.fp32)
        E.quantize_int8(cls.fp32, cls.int8)
        so = ort.SessionOptions()
        so.log_severity_level = 3
        cls.sess = ort.InferenceSession(str(cls.fp32), so, providers=["CPUExecutionProvider"])

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def _onnx(self, ids, mask):
        import numpy as np

        return self.sess.run(None, {"input_ids": np.asarray(ids, np.int64), "attention_mask": np.asarray(mask, np.int64)})[0]

    def _torch(self, ids, mask):
        import torch

        with torch.no_grad():
            return self.model(input_ids=torch.tensor(ids), attention_mask=torch.tensor(mask)).last_hidden_state.numpy()

    def _ids(self, n, seed):
        import numpy as np

        return np.random.default_rng(seed).integers(1000, 20000, size=n).tolist()

    def test_t1_lengths_beyond_the_trace_length_match_torch(self):
        """T1: the graph was traced at length 16, yet lengths 7 and 300 match
        torch, because the rotary tables were pre-built for 2048 positions."""
        import numpy as np

        for n in (7, 300):
            ids = [self._ids(n, n)]
            mask = [[1] * n]
            got, want = self._onnx(ids, mask), self._torch(ids, mask)
            self.assertEqual(got.shape, (1, n, E.HIDDEN))
            self.assertLess(float(np.abs(got - want).max()), 1e-4, f"length {n}")

    def test_t2_a_padded_batch_equals_batch_of_one(self):
        """T2: dynamic batch and sequence axes - a batch of 3 with padding
        gives each row the vectors it gets alone, at its real positions."""
        import numpy as np

        lengths = [5, 12, 9]
        rows = [self._ids(n, 40 + n) for n in lengths]
        width = max(lengths)
        ids = [r + [0] * (width - len(r)) for r in rows]
        mask = [[1] * len(r) + [0] * (width - len(r)) for r in rows]
        batch = self._onnx(ids, mask)
        self.assertEqual(batch.shape, (3, width, E.HIDDEN))
        for i, r in enumerate(rows):
            alone = self._onnx([r], [[1] * len(r)])
            self.assertLess(float(np.abs(batch[i, : len(r)] - alone[0]).max()), 1e-4, f"row {i}")

    def test_t3_the_int8_file_has_the_shipped_recipe(self):
        """T3 (Q-F): reduce_range keeps 8-bit weights inside [-64, 64]; the
        quantized file is per-channel integer matmuls with a quantized Gather."""
        fp = E.int8_fingerprint(self.int8)
        self.assertGreaterEqual(fp["int8_min"], -64, fp)
        self.assertLessEqual(fp["int8_max"], 64, fp)
        self.assertGreater(fp["matmul_integer"], 0, fp)
        self.assertGreaterEqual(fp["gather_quantized"], 1, fp)
        self.assertEqual(fp["fp32_matmul_on_big_weights"], 0, fp)
        self.assertTrue(fp["ok"], fp)

    def test_the_fp32_file_is_not_mistaken_for_int8(self):
        fp = E.int8_fingerprint(self.fp32)
        self.assertFalse(fp["ok"], fp)
        self.assertEqual(fp["matmul_integer"], 0)


class QuantizedGatherDetection(unittest.TestCase):
    """T7: an embedding table counts as quantized when its Gather reads an
    8-bit initializer (the DequantizeLinear follows the Gather)."""

    def _graph(self, path, table_type):
        import numpy as np
        import onnx
        from onnx import TensorProto as TP
        from onnx import helper, numpy_helper

        np_type = {TP.UINT8: np.uint8, TP.FLOAT: np.float32}[table_type]
        table = numpy_helper.from_array(np.zeros((6, 4), np_type), "table")
        scale = numpy_helper.from_array(np.ones((), np.float32), "scale")
        nodes = [helper.make_node("Gather", ["table", "ids"], ["rows"])]
        if table_type == TP.UINT8:
            nodes.append(helper.make_node("DequantizeLinear", ["rows", "scale"], ["out"]))
        else:
            nodes.append(helper.make_node("Identity", ["rows"], ["out"]))
        graph = helper.make_graph(
            nodes,
            "g",
            [helper.make_tensor_value_info("ids", TP.INT64, [2])],
            [helper.make_tensor_value_info("out", TP.FLOAT, [2, 4])],
            initializer=[table, scale],
        )
        onnx.save(helper.make_model(graph, opset_imports=[helper.make_opsetid("", 17)]), str(path))

    def setUp(self):
        _deps_or_skip_libs()
        self.tmp = Path(tempfile.mkdtemp(prefix="gm703-gather-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def test_gather_on_an_8_bit_table_is_counted(self):
        from onnx import TensorProto as TP

        self._graph(self.tmp / "q.onnx", TP.UINT8)
        self.assertEqual(E.int8_fingerprint(self.tmp / "q.onnx")["gather_quantized"], 1)

    def test_gather_on_a_float_table_is_not(self):
        from onnx import TensorProto as TP

        self._graph(self.tmp / "f.onnx", TP.FLOAT)
        self.assertEqual(E.int8_fingerprint(self.tmp / "f.onnx")["gather_quantized"], 0)


def _deps_or_skip_libs():
    try:
        import numpy  # noqa: F401
        import onnx  # noqa: F401
    except ImportError as e:
        raise unittest.SkipTest(f"needs the gm703 venv (numpy, onnx): {e}")


class CustomCodeGuard(unittest.TestCase):
    """T4: remote modeling code runs only when its sha256 is the pinned one."""

    def _write(self, d, name, data):
        (Path(d) / name).write_bytes(data)

    def test_refuses_changed_and_missing_files(self):
        with tempfile.TemporaryDirectory() as d:
            for name, body in (("a.py", b"print(1)\n"), ("b.py", b"print(2)\n")):
                self._write(d, name, body)
            expected = {n: E.sha256(Path(d) / n) for n in ("a.py", "b.py")}
            E.verify_custom_code(d, expected)  # unchanged: accepted
            self._write(d, "b.py", b"print(2)\n#")  # one byte more
            with self.assertRaises(E.CodeMismatch):
                E.verify_custom_code(d, expected)
            (Path(d) / "b.py").unlink()
            with self.assertRaises(E.CodeMismatch):
                E.verify_custom_code(d, expected)

    def test_load_model_refuses_code_that_is_not_the_pinned_code(self):
        """The guard must sit in front of the loader: load_model on a directory
        whose files are not the pinned ones raises CodeMismatch before any code
        in it is imported."""
        try:
            import torch  # noqa: F401
            import transformers  # noqa: F401
        except ImportError as e:
            self.skipTest(f"needs the gm703 venv: {e}")
        with tempfile.TemporaryDirectory() as d:
            marker = Path(d) / "ran"
            for name in E.CODE_SHA256:
                self._write(d, name, f"open({str(marker)!r}, 'w').write('x')\n".encode())
            self._write(d, "config.json", b"{}")
            with self.assertRaises(E.CodeMismatch):
                E.load_model(d)
            self.assertFalse(marker.exists(), "unpinned code was executed")

    def test_the_pinned_files_in_the_cache_pass_and_a_one_byte_change_does_not(self):
        snap = _deps_or_skip()
        with tempfile.TemporaryDirectory() as d:
            for name in E.CODE_SHA256:
                shutil.copyfile(snap / name, Path(d) / name)
            E.verify_custom_code(d)
            with open(Path(d) / "modeling_hf_nomic_bert.py", "ab") as f:
                f.write(b"#")
            with self.assertRaises(E.CodeMismatch):
                E.verify_custom_code(d)


if __name__ == "__main__":
    unittest.main()
