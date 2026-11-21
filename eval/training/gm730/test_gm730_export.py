"""Tests of export.py (docs/design/GM-730-gpu-training-routes.md sections 2 and 4): the
closed-form quint8 recipe, C-R on quantised graphs and its planted failures, C-P and its
planted failure, the device-correct trace inputs, the per-batch padding. Tiny random BERT
snapshots (gm730_fixture.py); the closed form and padding tests need numpy only, the rest
torch, transformers, onnx and onnxruntime and skip without them.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_export.py'
"""

import contextlib
import logging
import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import export as X  # noqa: E402
import gm730_fixture as GF  # noqa: E402

try:
    import onnx
    import onnxruntime  # noqa: F401
    import tokenizers  # noqa: F401
    import torch
    import transformers
except ImportError:  # pragma: no cover
    torch = None

# the tiny models have 16 channels, so a 1% row scale stays below the 1e-4 tolerance
PLANT_FACTOR = 100.0

logging.getLogger().setLevel(logging.ERROR)
needs_stack = unittest.skipIf(torch is None, "torch, transformers, onnx or onnxruntime is not installed (GM-730 venv)")


class ClosedForm(unittest.TestCase):
    def test_per_channel_scale_is_maxabs_over_127_and_zero_point_127(self):
        w = np.array([[1.0, -2.0, 0.5], [0.0, 0.0, 0.0], [127.0, 3.0, -64.0]], np.float32)
        q, s, z = X.closed_form(w, 0)
        np.testing.assert_array_equal(s, np.array([2.0 / 127, 1.0, 1.0], np.float32))
        self.assertEqual(z.tolist(), [127, 127, 127])
        self.assertEqual(q.dtype, np.uint8)
        np.testing.assert_array_equal(q[0], np.clip(np.rint(w[0] / s[0]) + 127, 0, 255).astype(np.uint8))
        self.assertEqual(q[2].tolist(), [254, 130, 127 - 64])
        self.assertEqual(q[1].tolist(), [127, 127, 127])

    def test_the_other_channel_axis_and_per_tensor(self):
        w = np.array([[1.0, -4.0], [2.0, 0.5]], np.float32)
        _, s1, _ = X.closed_form(w, 1)
        np.testing.assert_array_equal(s1, np.array([2.0 / 127, 4.0 / 127], np.float32))
        q, s, z = X.closed_form(w, None)
        self.assertEqual(s.shape, (1,))
        self.assertEqual(float(s[0]), np.float32(4.0 / 127))
        self.assertEqual(int(q.min()), 0)
        self.assertEqual(z.tolist(), [127])


class Tok:
    """token_to_id / encode_batch of the two pad names, with ids of known lengths."""

    class Enc:
        def __init__(self, n):
            self.ids, self.type_ids = list(range(10, 10 + n)), [0] * n

    def __init__(self, pad):
        self.pad = pad

    def token_to_id(self, t):
        return self.pad if t == "[PAD]" else None

    def encode_batch(self, items):
        return [self.Enc(len(str(i))) for i in items]


class Padding(unittest.TestCase):
    def test_batches_pad_per_batch_with_the_pad_id_and_mask_it(self):
        feeds = list(X.batches(Tok(7), ["aaa", "b", "cc", "dddddd"], 2))
        self.assertEqual([f["input_ids"].shape for f in feeds], [(2, 3), (2, 6)])
        f0 = feeds[0]
        self.assertEqual(f0["input_ids"][1].tolist(), [10, 7, 7])
        self.assertEqual(f0["attention_mask"].tolist(), [[1, 1, 1], [1, 0, 0]])
        self.assertEqual(f0["token_type_ids"].sum(), 0)

    def test_mean_pool_ignores_padding(self):
        h = np.array([[[1.0, 1.0], [3.0, 5.0], [100.0, 100.0]]])
        m = np.array([[1, 1, 0]])
        np.testing.assert_allclose(X.mean_pool(h, m), [[2.0, 3.0]])
        np.testing.assert_allclose(X.mean_pool(h, np.zeros((1, 3), int)), [[0.0, 0.0]])


@needs_stack
class Tiny(unittest.TestCase):
    """fp32 ONNX exports of a tiny cross-encoder (`l6`) and a tiny embedder (`gte`)."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.tmp.cleanup)
        cls.root = Path(cls.tmp.name)
        cls.world = GF.World(cls.root, nodes=12)
        cls.world.write_snapshots(("l6", "gte-small"))
        cls.snap = {"l6": cls.world.hf_root / f"ms-marco-MiniLM-L6-v2@{X.S.PINS['l6'][1]}",
                    "gte": cls.world.hf_root / f"gte-small@{X.S.PINS['gte-small'][1]}"}
        cls.models = {m: X.load_torch(m, cls.snap[m]) for m in ("l6", "gte")}
        cls.fp32 = {m: cls.root / m / "fp32" for m in cls.models}
        for m in cls.models:
            X.export_dir(cls.models[m], m, cls.fp32[m], cls.snap[m])
        cls.tok = X.tokenizer(cls.snap["l6"], 32)
        cls.texts = [f"f{i} src f{i}" for i in range(12)]
        cls.pairs = [(f"find f{i} variant {i % 3}", f"f{i} body") for i in range(12)]

    def quant(self, model, name, plant=None):
        dst = self.root / model / name / "model.onnx"
        X.quantize("l6" if model == "l6" else "jina", self.fp32[model] / "model.onnx", dst, plant)
        return dst


@needs_stack
class RecipeControl(Tiny):
    def test_the_same_recipe_twice_passes_c_r_on_both_recipes(self):
        for model, rec in (("l6", "l6"), ("gte", "jina")):
            a, b = (self.root / model / n / "model.onnx" for n in ("same-a", "same-b"))
            X.quantize(rec, self.fp32[model] / "model.onnx", a)
            X.quantize(rec, self.fp32[model] / "model.onnx", b)
            r = X.compare_quantized(a, b)
            self.assertTrue(r["pass"], (rec, r))
            self.assertGreater(r["matchedByNode"], 0)

    def test_planted_per_channel_flip_fails_c_r_on_l6(self):
        good = self.quant("l6", "official")
        bad = self.quant("l6", "plant-pc", "flip-per-channel")
        r = X.compare_quantized(bad, good)
        self.assertFalse(r["pass"], r)

    def test_planted_flips_fail_c_r_on_the_jina_recipe(self):
        good = self.root / "gte" / "jina-good" / "model.onnx"
        X.quantize("jina", self.fp32["gte"] / "model.onnx", good)
        for plant in X.PLANTS:
            bad = self.root / "gte" / f"jina-{plant}" / "model.onnx"
            X.quantize("jina", self.fp32["gte"] / "model.onnx", bad, plant)
            self.assertFalse(X.compare_quantized(bad, good)["pass"], plant)

    @contextlib.contextmanager
    def fake_quantizer(self, seen):
        with mock.patch("onnxruntime.quantization.quantize_dynamic", lambda *a, **kw: seen.update(kw)), \
                mock.patch.object(X, "sha256_file", lambda p: "0" * 64):
            yield

    def test_a_plant_flips_exactly_one_recipe_flag(self):
        for plant, key in X.PLANTS.items():
            seen = {}
            with self.fake_quantizer(seen):
                X.quantize("jina", "src", self.root / "x.onnx", plant)
            base = X.RECIPES["jina"]
            self.assertEqual({k for k in ("per_channel", "reduce_range") if seen[k] != base[k]}, {key})
        seen = {}
        with self.fake_quantizer(seen):
            X.quantize("jina", "src", self.root / "x.onnx")
        self.assertEqual((seen["per_channel"], seen["reduce_range"]), (True, True))

    def test_the_closed_form_rewrite_sets_zero_point_127_and_is_idempotent(self):
        q = self.quant("l6", "closed")
        m = onnx.load(str(q))
        zps = [onnx.numpy_helper.to_array(i) for i in m.graph.initializer if i.name.endswith("_zero_point")]
        self.assertTrue(zps)
        self.assertTrue(all((z == 127).all() for z in zps))
        before = q.read_bytes()
        X.rewrite_closed_form(self.fp32["l6"] / "model.onnx", q)
        self.assertEqual(X.compare_quantized(q, q)["pass"], True)
        again = self.root / "l6" / "closed2" / "model.onnx"
        again.parent.mkdir()
        again.write_bytes(before)
        X.rewrite_closed_form(self.fp32["l6"] / "model.onnx", again)
        self.assertTrue(X.compare_quantized(again, q)["pass"])

    def test_ours_vs_official_is_a_non_gating_row_with_its_own_int8_and_tokenizer(self):
        out = self.root / "gte" / "ovo"
        (out / "fp32").mkdir(parents=True)
        (out / "fp32" / "model.onnx").write_bytes((self.fp32["gte"] / "model.onnx").read_bytes())
        official = self.root / "gte" / "ovo-official.onnx"
        X.quantize("jina", self.fp32["gte"] / "model.onnx", official)
        r = X.ours_vs_official("jina", out, self.snap["gte"], official, self.tok, self.texts)
        self.assertIs(r["gating"], False)
        self.assertEqual(r["check"], "C-R ours-int8")
        self.assertTrue((out / "int8-ours" / "model.onnx").exists())
        self.assertTrue((out / "int8-ours" / "tokenizer.json").exists())
        self.assertLessEqual(r["maxCosDist"], 1e-9)

    def test_main_ignores_non_gating_rows(self):
        rows = [{"check": "a", "pass": True}, {"check": "b", "pass": False, "gating": False}]
        with mock.patch.object(X, "recipe_control", return_value=rows), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(X.main(["recipe-control", "--model", "gte", "--out", str(self.root / "o")]), 0)
        rows[0]["pass"] = False
        with mock.patch.object(X, "recipe_control", return_value=rows), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(X.main(["recipe-control", "--model", "gte", "--out", str(self.root / "o")]), 1)


@needs_stack
class Parity(Tiny):
    def test_the_export_passes_c_p_and_a_planted_row_fails_it(self):
        ce = X.parity_ce(self.models["l6"], self.fp32["l6"] / "model.onnx", self.tok, self.pairs)
        self.assertTrue(ce["pass"], ce)
        planted = self.root / "l6" / "planted.onnx"
        X.plant_row(self.fp32["l6"] / "model.onnx", planted, PLANT_FACTOR)
        bad = X.parity_ce(self.models["l6"], planted, self.tok, self.pairs)
        self.assertFalse(bad["pass"], bad)
        self.assertGreater(bad["maxAbsDiff"], X.TOL_CE)

    def test_embedder_c_p_and_its_planted_failure(self):
        ok = X.parity_embed(self.models["gte"], "gte", self.fp32["gte"] / "model.onnx", self.tok, self.texts)
        self.assertTrue(ok["pass"], ok)
        planted = self.root / "gte" / "planted.onnx"
        X.plant_row(self.fp32["gte"] / "model.onnx", planted, PLANT_FACTOR)
        bad = X.parity_embed(self.models["gte"], "gte", planted, self.tok, self.texts)
        self.assertFalse(bad["pass"], bad)

    def test_plant_row_changes_one_weight_matrix_row_by_the_factor(self):
        src = self.fp32["l6"] / "model.onnx"
        dst = self.root / "l6" / "planted2.onnx"
        name = X.plant_row(src, dst, 1.5)
        a = {i.name: onnx.numpy_helper.to_array(i) for i in onnx.load(str(src)).graph.initializer}
        b = {i.name: onnx.numpy_helper.to_array(i) for i in onnx.load(str(dst)).graph.initializer}
        np.testing.assert_allclose(b[name][0], a[name][0] * np.float32(1.5))
        np.testing.assert_array_equal(b[name][1:], a[name][1:])
        self.assertEqual([k for k in a if not np.array_equal(a[k], b[k])], [name])


@needs_stack
class DeviceAndTokenizer(Tiny):
    def test_dummy_inputs_are_on_the_models_parameter_device(self):
        with torch.device("meta"):
            meta = transformers.BertModel(self.models["gte"].config)
        names = X.MODELS["gte"][2]
        feeds = X.dummy_inputs(meta, names)
        self.assertEqual([t.device.type for t in feeds], ["meta"] * 3)
        cpu = X.dummy_inputs(self.models["gte"], names)
        self.assertEqual([t.device.type for t in cpu], ["cpu"] * 3)
        self.assertEqual([int(t.sum()) for t in cpu], [16, 16, 0])
        self.assertEqual({tuple(t.shape) for t in cpu}, {(2, 8)})

    def test_copy_tokenizer_errors_when_the_copys_sha_differs(self):
        out = self.root / "ct"
        out.mkdir()
        got = X.copy_tokenizer(self.snap["l6"], out)
        self.assertEqual(set(got), {"tokenizer.json", "config.json"})
        shas = iter(["a" * 64, "b" * 64])
        with mock.patch.object(X, "sha256_file", lambda p: next(shas)):
            with self.assertRaises(SystemExit):
                X.copy_tokenizer(self.snap["l6"], out)

    def test_export_matches_torch_on_fresh_inputs(self):
        sess = X.ort_forward(X.ort_session(self.fp32["gte"] / "model.onnx"))
        feed = next(X.batches(self.tok, ["f1 src f1", "f2"], 2))
        want = X.torch_forward(self.models["gte"], "gte")(feed)
        np.testing.assert_allclose(sess(feed)[:, :, :], want, atol=1e-5)


if __name__ == "__main__":
    unittest.main()
