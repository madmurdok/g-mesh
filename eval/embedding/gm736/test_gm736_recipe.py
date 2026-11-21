"""GM-736 recipe tests (docs/design/GM-736-cre-int8.md, section 5.1: T1-T4, T6, T7).

Run: <gm703 venv>/bin/python -m unittest discover -s eval/embedding/gm736 -p 'test_gm736_*.py'

Two fixtures, no real CodeRankEmbed weights:
- a hand-built ONNX graph (Gather -> two MatMuls sharing one input -> MatMul) with a
  planted outlier channel, for the smoothing mechanics (T2, T3);
- GM-703's 2-layer random-init NomicBERT (`load_model(snapshot, n_layer=2)`), exported
  once per module, for identity (T1), both int8 forms (T4) and the recipe CLI (T6, T7).
  It needs the pinned code and config in the local HF cache and the CRE tokenizer.json,
  and skips with a message when either is missing.
The gate tests stub the CLS vectors (no jina model is read).
"""

import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "gm703"))
sys.path.insert(0, str(HERE))

os.environ.setdefault("HF_HOME", str(Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm703/hf"))
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ.setdefault("TOKENIZERS_PARALLELISM", "false")

try:
    import numpy as np
    import onnx
    import onnxruntime as ort
    from onnx import TensorProto, helper, numpy_helper

    import export_coderankembed as E
    import parity as P
    import recipe as R
    import smooth as S
except ImportError as e:  # pragma: no cover - outside the gm703 venv
    raise unittest.SkipTest(f"needs the gm703 venv (numpy, onnx, onnxruntime, torch): {e}")


def run(path, ids):
    so = ort.SessionOptions()
    so.log_severity_level = 3
    sess = ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])
    r = np.asarray([ids], dtype=np.int64)
    return sess.run(None, {"input_ids": r, "attention_mask": np.ones_like(r)})[0]


# --- a hand-built graph ---------------------------------------------------------------

V, D, H, OUTLIER = 64, 8, 6, 3


def tiny_graph(path):
    """input_ids -> Gather(table) = x; y = x@W1 + x@W2 (one input, two consumers); x@W1+x@W2 = z;
    h = z@W3; out = h * mask. Channel OUTLIER of x is 40x the others."""
    rng = np.random.default_rng(736)
    table = rng.normal(size=(V, D)).astype(np.float32)
    table[:, OUTLIER] *= 40.0
    inits = [
        numpy_helper.from_array(table, "table"),
        numpy_helper.from_array(rng.uniform(-1, 1, (D, H)).astype(np.float32), "W1"),
        numpy_helper.from_array(rng.uniform(-1, 1, (D, H)).astype(np.float32), "W2"),
        numpy_helper.from_array(rng.uniform(-1, 1, (H, H)).astype(np.float32), "W3"),
        numpy_helper.from_array(np.asarray([-1], dtype=np.int64), "axes"),
    ]
    nodes = [
        helper.make_node("Gather", ["table", "input_ids"], ["x"], name="gather"),
        helper.make_node("MatMul", ["x", "W1"], ["y1"], name="fc11"),
        helper.make_node("MatMul", ["x", "W2"], ["y2"], name="fc12"),
        helper.make_node("Add", ["y1", "y2"], ["z"], name="add"),
        helper.make_node("MatMul", ["z", "W3"], ["h"], name="fc2"),
        helper.make_node("Cast", ["attention_mask"], ["mf"], to=TensorProto.FLOAT, name="cast"),
        helper.make_node("Unsqueeze", ["mf", "axes"], ["m3"], name="unsq"),
        helper.make_node("Mul", ["h", "m3"], ["last_hidden_state"], name="mask"),
    ]
    g = helper.make_graph(
        nodes,
        "gm736-tiny",
        [helper.make_tensor_value_info("input_ids", TensorProto.INT64, [1, "L"]),
         helper.make_tensor_value_info("attention_mask", TensorProto.INT64, [1, "L"])],
        [helper.make_tensor_value_info("last_hidden_state", TensorProto.FLOAT, [1, "L", H])],
        inits,
    )
    m = helper.make_model(g, opset_imports=[helper.make_opsetid("", 17)])
    m.ir_version = 8
    onnx.checker.check_model(m)
    onnx.save(m, str(path))


def ratio(a):
    a = np.asarray(a, dtype=np.float64)
    return float(a.max() / np.median(a))


class SmoothingFormula(unittest.TestCase):
    """T2: s_j = a_j^alpha / w_j^(1-alpha), w_j the max over every consumer of the input."""

    def test_w_is_the_max_over_all_consumers(self):
        a = np.asarray([4.0, 1.0, 9.0], dtype=np.float32)
        # row maxima: W1 [1, 2, 0.5], W2 [3, 1, 0.25]; over both [3, 2, 0.5]
        w1 = np.asarray([[1.0, -0.5], [0.0, -2.0], [0.5, 0.1]], dtype=np.float32)
        w2 = np.asarray([[-3.0, 1.0], [1.0, 0.5], [0.25, 0.0]], dtype=np.float32)
        w = np.asarray([3.0, 2.0, 0.5])
        for alpha in (0.3, 0.5, 0.7):
            s = S.smoothing_factors({"x": a}, {"x": [w1, w2]}, alpha)["x"]
            np.testing.assert_allclose(s, a ** alpha / w ** (1 - alpha), rtol=1e-6, err_msg=f"alpha {alpha}")

    def test_the_factors_sha_follows_the_factors(self):
        f = {"x": np.asarray([1.0, 2.0], np.float32), "z": np.asarray([3.0], np.float32)}
        g = {"x": np.asarray([1.0, 2.0], np.float32), "z": np.asarray([3.5], np.float32)}
        self.assertEqual(S.factors_sha256(f), S.factors_sha256({k: v.copy() for k, v in f.items()}))
        self.assertNotEqual(S.factors_sha256(f), S.factors_sha256(g))


class TinyGraph(unittest.TestCase):
    """T2, T3 on the hand-built graph."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="gm736-tiny-"))
        cls.fp32 = cls.tmp / "fp32.onnx"
        tiny_graph(cls.fp32)
        rng = np.random.default_rng(1)
        cls.rows = [rng.integers(0, V, size=n).tolist() for n in (5, 9, 17)]
        cls.absmax = S.collect_absmax(cls.fp32, cls.rows)
        cls.smoothed = cls.tmp / "smoothed.onnx"
        factors = S.smoothing_factors(cls.absmax, S.weights_by_input(onnx.load(str(cls.fp32))), 0.5)
        cls.muls = S.apply_smoothing(cls.fp32, cls.smoothed, factors)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_one_mul_per_distinct_matmul_input(self):
        self.assertEqual(sorted(S.const_matmuls(onnx.load(str(self.fp32)))), ["x", "z"])
        self.assertEqual(self.muls, 2)
        g = onnx.load(str(self.smoothed))
        self.assertEqual(sum(n.op_type == "Mul" for n in g.graph.node), 2 + 1)  # + the mask Mul
        reads = {n.name: n.input[0] for n in g.graph.node if n.op_type == "MatMul"}
        self.assertEqual(reads, {"fc11": "x_sq", "fc12": "x_sq", "fc2": "z_sq"})
        onnx.checker.check_model(g)

    def test_the_smoothed_graph_is_the_identity(self):
        for ids in self.rows:
            np.testing.assert_allclose(run(self.smoothed, ids), run(self.fp32, ids), atol=1e-4)

    def test_smoothing_lowers_the_range_ratio_the_quantizer_sees(self):
        before = self.absmax["x"]
        self.assertEqual(int(np.argmax(before)), OUTLIER)
        after = S.collect_absmax(self.smoothed, self.rows)["x_sq"]
        self.assertLess(ratio(after), ratio(before) / 2, (before, after))

    def test_factors_for_another_graph_are_refused(self):
        with self.assertRaises(ValueError):
            S.apply_smoothing(self.fp32, self.tmp / "bad.onnx", {"x": np.ones(D, np.float32)})


# --- GM-703's 2-layer NomicBERT, exported once ---------------------------------------

_FIX = {}


def fixture():
    """{tmp, fp32_dir, fp32, calib}; raises SkipTest without the HF cache or the tokenizer."""
    if "skip" in _FIX:
        raise unittest.SkipTest(_FIX["skip"])
    if _FIX:
        return _FIX
    tok = E.DEFAULT_OUT / "coderankembed-fp32" / "tokenizer.json"
    try:
        import torch  # noqa: F401
        import transformers  # noqa: F401

        snapshot = E.snapshot_dir()
    except Exception as e:  # noqa: BLE001 - any cache miss means no code to load
        _FIX["skip"] = f"pinned CodeRankEmbed code not in the local HF cache ({os.environ['HF_HOME']}): {e}"
        raise unittest.SkipTest(_FIX["skip"])
    if not tok.exists():
        _FIX["skip"] = f"no CRE tokenizer at {tok}"
        raise unittest.SkipTest(_FIX["skip"])
    tmp = Path(tempfile.mkdtemp(prefix="gm736-fixture-"))
    d = tmp / "fp32"
    d.mkdir()
    E.export_onnx(E.load_model(snapshot, n_layer=2), d / "model.onnx")
    shutil.copyfile(tok, d / "tokenizer.json")
    (d / "EXPORT.json").write_text(json.dumps({"precision": "fp32", "fixture": "2-layer random init"}) + "\n")
    calib = tmp / "calib.jsonl"
    calib.write_text("".join(json.dumps({"text": t}) + "\n" for t in (
        "def read_config(path: str) -> dict", "fn parse(input: &str) -> Result<Ast, Error>",
        "Represent this query for searching relevant code: open a file and read every line")))
    jina = tmp / "jina"
    jina.mkdir()
    shutil.copyfile(tok, jina / "tokenizer.json")
    _FIX.update(tmp=tmp, fp32_dir=d, fp32=d / "model.onnx", calib=calib, jina=jina)
    return _FIX


def tearDownModule():
    if "tmp" in _FIX:
        shutil.rmtree(_FIX["tmp"], ignore_errors=True)


def recipe_main(*args, work, models):
    f = fixture()
    argv = [*args, "--fp32-dir", str(f["fp32_dir"]), "--models", str(models), "--work", str(work),
            "--calib", str(f["calib"]), "--select", str(f["calib"]),
            "--jina-fp32", str(f["jina"]), "--jina-int8", str(f["jina"])]
    with contextlib.redirect_stdout(io.StringIO()):
        return R.main(argv)


class Identity(unittest.TestCase):
    """T1, T2 on the NomicBERT fixture."""

    @classmethod
    def setUpClass(cls):
        f = fixture()
        cls.fp32 = f["fp32"]
        rng = np.random.default_rng(736)
        cls.ids = {n: rng.integers(1000, 20000, size=n).tolist() for n in (7, 300)}
        absmax = S.collect_absmax(cls.fp32, [cls.ids[7], rng.integers(1000, 20000, size=40).tolist()])
        cls.alpha = float(rng.uniform(0.3, 0.7))
        model = onnx.load(str(cls.fp32))
        cls.users = S.const_matmuls(model)
        factors = S.smoothing_factors(absmax, S.weights_by_input(model), cls.alpha)
        cls.smoothed = f["tmp"] / "identity-smoothed.onnx"
        cls.muls = S.apply_smoothing(cls.fp32, cls.smoothed, factors)

    def test_t1_smoothing_is_the_identity_in_fp32(self):
        for n, ids in self.ids.items():
            a, b = run(self.fp32, ids), run(self.smoothed, ids)
            self.assertLessEqual(float(np.abs(a - b).max()), 1e-4, f"length {n}, alpha {self.alpha}")

    def test_t2_one_mul_per_distinct_input_not_per_matmul(self):
        self.assertEqual(self.muls, len(self.users))
        self.assertLess(self.muls, sum(len(ns) for ns in self.users.values()))  # fc11 and fc12 share one
        g = onnx.load(str(self.smoothed))
        self.assertEqual(sum(n.name.endswith("_sq_mul") for n in g.graph.node), self.muls)
        self.assertTrue(all(n.input[0].endswith("_sq") for ns in S.const_matmuls(g).values() for n in ns))


class Exports(unittest.TestCase):
    """T4 (Q-F of both weight forms, cross-refusal, R2's allowed_fp32), T7 (EXPORT.json,
    a re-export is byte-identical), through `recipe.py --export`."""

    @classmethod
    def setUpClass(cls):
        f = fixture()
        cls.tmp = f["tmp"]
        w1, w2 = cls.tmp / "work1", cls.tmp / "work2"
        cls.m = {k: cls.tmp / k for k in ("a", "b", "x")}
        for weights in ("s7", "u8"):
            assert recipe_main("--export", "--alpha", "0.5", "--weights", weights, work=w1, models=cls.m["a"]) == 0
        # the same inputs again, from a fresh work dir (no absmax cache)
        assert recipe_main("--export", "--alpha", "0.5", "--weights", "s7", work=w2, models=cls.m["b"]) == 0
        # R2: one big MatMul kept fp32
        model = onnx.load(str(f["fp32"]))
        inits = {t.name: tuple(t.dims) for t in model.graph.initializer}
        cls.excluded = sorted(n.name for ns in S.const_matmuls(model).values() for n in ns
                              if inits[n.input[1]] == (E.HIDDEN, 3 * E.HIDDEN))[0]
        ex = cls.tmp / "exclude.json"
        ex.write_text(json.dumps([cls.excluded]))
        assert recipe_main("--export", "--alpha", "0.5", "--weights", "s7", "--exclude-file", str(ex),
                           work=w1, models=cls.m["x"]) == 0
        cls.s7 = R.model_dir(cls.m["a"], "s7") / "model.onnx"
        cls.u8 = R.model_dir(cls.m["a"], "u8") / "model.onnx"
        cls.x = R.model_dir(cls.m["x"], "s7") / "model.onnx"

    def record(self, models, weights):
        return json.loads((R.model_dir(models, weights) / "EXPORT.json").read_text())

    def test_t4a_the_s7_file_keeps_the_shipped_fingerprint(self):
        fp = E.int8_fingerprint(self.s7, weights="s7")
        self.assertTrue(fp["ok"], fp)
        self.assertGreaterEqual(fp["int8_min"], -64, fp)
        self.assertLessEqual(fp["int8_max"], 64, fp)
        self.assertGreaterEqual(fp["gather_quantized"], 1, fp)
        self.assertEqual(fp["fp32_matmul_on_big_weights"], 0, fp)

    def test_t4b_the_u8_file_has_unsigned_symmetric_per_channel_weights(self):
        fp = E.int8_fingerprint(self.u8, weights="u8")
        self.assertTrue(fp["ok"], fp)
        self.assertEqual(fp["int8_weights"], 0, fp)
        self.assertGreater(fp["uint8_weights"], 0, fp)
        self.assertEqual(fp["uint8_zero_point_values"], [E.U8_ZERO_POINT], fp)
        self.assertGreaterEqual(fp["uint8_per_channel_zero_points"], fp["uint8_weights"] - fp["gather_quantized"], fp)
        self.assertEqual(fp["fp32_matmul_on_big_weights"], 0, fp)

    def test_t4c_neither_form_passes_the_others_fingerprint(self):
        self.assertFalse(E.int8_fingerprint(self.s7, weights="u8")["ok"])
        self.assertFalse(E.int8_fingerprint(self.u8, weights="s7")["ok"])
        with self.assertRaises(ValueError):
            E.int8_fingerprint(self.s7, weights="u7")

    def test_t4d_an_excluded_matmul_is_allowed_only_when_counted(self):
        self.assertEqual(self.record(self.m["x"], "s7")["excluded"], [self.excluded])
        fp = E.int8_fingerprint(self.x, allowed_fp32=1)
        self.assertEqual(fp["fp32_matmul_on_big_weights"], 1, fp)
        self.assertTrue(fp["ok"], fp)
        self.assertFalse(E.int8_fingerprint(self.x, allowed_fp32=0)["ok"])

    def test_t7_export_json_records_the_recipe_inputs(self):
        f = fixture()
        for weights, path in (("s7", self.s7), ("u8", self.u8)):
            rec = self.record(self.m["a"], weights)
            self.assertEqual(rec["recipe"], "smooth+dynamic")
            self.assertEqual(rec["precision"], "int8")
            self.assertEqual(rec["alpha"], 0.5)
            self.assertEqual(rec["weights"], weights)
            self.assertEqual(rec["excluded"], [])
            self.assertEqual(rec["calib_sample_sha256"], P.sha256_file(f["calib"]))
            self.assertEqual(rec["fp32_model_onnx_sha256"], E.sha256(f["fp32"]))
            self.assertRegex(rec["smoothing_sha256"], "^[0-9a-f]{64}$")
            self.assertEqual(rec["model_onnx_sha256"], E.sha256(path))
            self.assertEqual(rec["model_onnx_bytes"], path.stat().st_size)
            self.assertEqual(rec["quantize"]["reduce_range"], weights == "s7")

    def test_t7_the_same_inputs_give_the_same_file(self):
        a, b = self.record(self.m["a"], "s7"), self.record(self.m["b"], "s7")
        self.assertEqual(a["smoothing_sha256"], b["smoothing_sha256"])
        self.assertEqual(a["model_onnx_sha256"], b["model_onnx_sha256"])

    # --- T6: the gate ---

    def gate(self, models, cosine, sample=P.SAMPLE):
        """`recipe.py --gate` with stubbed CLS vectors: every int8 text at `cosine` to its fp32 vector,
        jina at 1.0 (so the p1 rule never decides)."""
        f = fixture()
        e0 = np.zeros(E.HIDDEN)
        e0[0] = 1.0
        v = e0.copy()
        v[:2] = [cosine, np.sqrt(1 - cosine ** 2)]

        def cls_vectors(sess, ids):
            return [e0 if Path(sess) == f["fp32"] else v for _ in ids]

        with mock.patch.object(P, "session", side_effect=lambda p: str(p)), \
                mock.patch.object(P, "cls_vectors", side_effect=cls_vectors), \
                mock.patch.object(P, "mean_vectors", side_effect=lambda s, ids: [e0 for _ in ids]):
            rc = recipe_main("--gate", "--weights", "s7", "--gate-sample", str(sample), "--limit", "5",
                             work=self.tmp / "gate-work", models=models)
        out = self.tmp / "gate-work" / "gate-s7-limit5.json"
        return rc, (json.loads(out.read_text()) if rc != 2 else None)

    def test_t6_a_median_just_under_0_99_misses_the_gate(self):
        rc, rep = self.gate(self.m["a"], 0.9899)
        self.assertEqual(rc, 3, rep)
        self.assertFalse(rep["Q-D"]["ok"], rep)
        self.assertEqual(rep["Q-D"]["min_median"], 0.99)
        self.assertTrue(rep["Q-F"]["ok"] and rep["size"]["ok"], rep)

    def test_t6_a_median_at_0_9901_passes(self):
        rc, rep = self.gate(self.m["a"], 0.9901)
        self.assertEqual(rc, 0, rep)
        self.assertTrue(rep["ok"], rep)
        self.assertEqual(rep["sample_sha256"], R.FROZEN_SAMPLE_SHA256)

    def test_t6_the_gate_counts_the_recorded_exclusions(self):
        rc, rep = self.gate(self.m["x"], 0.995)
        self.assertEqual(rc, 0, rep)
        self.assertEqual(rep["Q-F"]["allowed_fp32"], 1, rep)

    def test_t6_a_sample_that_is_not_the_frozen_one_is_refused(self):
        other = self.tmp / "not-frozen.jsonl"
        other.write_text("".join(P.SAMPLE.read_text().splitlines(keepends=True)[:-1]))
        rc, _ = self.gate(self.m["a"], 0.9999, sample=other)
        self.assertEqual(rc, 2)

    def test_t6_a_file_over_the_size_budget_misses_the_gate(self):
        big = self.tmp / "big"
        d = R.model_dir(big, "s7")
        d.mkdir(parents=True)
        os.link(self.s7, d / "model.onnx")
        rec = self.record(self.m["a"], "s7")
        rec["model_onnx_bytes"] = R.SIZE_BUDGET + 1
        (d / "EXPORT.json").write_text(json.dumps(rec))
        rc, rep = self.gate(big, 0.9999)
        self.assertEqual(rc, 3, rep)
        self.assertFalse(rep["size"]["ok"], rep)
        self.assertEqual(rep["size"]["budget"], 161_895_621)


if __name__ == "__main__":
    unittest.main()
