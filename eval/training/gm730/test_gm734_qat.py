"""Tests of the RR quantization-aware fine-tune (docs/design/GM-734-rr-qat.md section 6):
qat.py's fake quantisers against export.closed_form and ORT DynamicQuantizeLinear, the
masked per-pair ranges and the mask plumbing, emulation vs the ORT quint8 export, the STE
gradients, wrap / unwrap / bypass, trainlib.init_from_onnx, train_ce.py --stage qat (the
selection rule, the C-P2 margin, a checkpoint after every dev row, the S2 probe's whole-
process ETA, the zero-step export, kill + resume) and make_jobs.py qat. Tiny random BERT
snapshots and a tiny GM-456 world (gm730_fixture.py); CPU only, nothing downloaded. Needs
torch, transformers, tokenizers, onnx and onnxruntime; skipped without them.

The resume test runs train_ce.py as separate processes; everything else runs in-process.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm734_qat.py'
"""

import contextlib
import hashlib
import io
import itertools
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import types
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm730_fixture as GF  # noqa: E402

try:
    import onnx
    import onnxruntime as ort
    import tokenizers  # noqa: F401
    import torch
    import transformers
    import budget as B
    import make_jobs as MJ
    import qat as Q
    import train_ce as TC
    import trainlib as L
except ImportError:  # pragma: no cover
    torch = None

needs_stack = unittest.skipIf(torch is None, "torch, transformers, onnx or onnxruntime is not installed (GM-730 venv)")
STOP = 75
RUN_NUMBER = itertools.count()
# a tiny qat run: 3 batches per epoch, so 4 steps cross into epoch 2; dev rows at 0, 2, 4
QAT = ["--stage", "qat", "--steps", "4", "--eval-every", "2", "--lr", "1e-3", "--batch", "6", "--seed", "1",
       "--resamples", "50", "--eval-batch", "16", "--ckpt-minutes", "60", "--threads", "1", "--max-len", "32",
       "--micro-queries", "3", "--micro-batch", "16", "--eval-queries", "4"]


def tiny_ce(seed=0, attn="eager"):
    """A 2-layer random BERT cross-encoder, eval mode."""
    cfg = transformers.BertConfig(vocab_size=40, hidden_size=16, num_hidden_layers=2, num_attention_heads=2,
                                  intermediate_size=32, max_position_embeddings=32, pad_token_id=0, num_labels=1,
                                  attn_implementation=attn)
    torch.manual_seed(seed)
    m = transformers.BertForSequenceClassification(cfg)
    return m.eval()


def padded_batch():
    """input_ids / attention_mask of 3 pairs of lengths 7, 4 and 2 padded to 7."""
    lens = [7, 4, 2]
    g = torch.Generator().manual_seed(9)
    ids = torch.randint(1, 40, (3, 7), generator=g)
    mask = torch.zeros(3, 7, dtype=torch.long)
    for r, n in enumerate(lens):
        mask[r, :n] = 1
        ids[r, n:] = 0
    return ids, mask


def dql_session():
    """ORT session of one DynamicQuantizeLinear node: x -> (y uint8, scale, zero point)."""
    from onnx import TensorProto, helper
    node = helper.make_node("DynamicQuantizeLinear", ["x"], ["y", "s", "z"])
    g = helper.make_graph([node], "dql", [helper.make_tensor_value_info("x", TensorProto.FLOAT, None)],
                          [helper.make_tensor_value_info("y", TensorProto.UINT8, None),
                           helper.make_tensor_value_info("s", TensorProto.FLOAT, None),
                           helper.make_tensor_value_info("z", TensorProto.UINT8, None)])
    model = helper.make_model(g, opset_imports=[helper.make_opsetid("", 13)])
    return ort.InferenceSession(model.SerializeToString(), providers=["CPUExecutionProvider"])


# ---- quantisers ------------------------------------------------------------------------
@needs_stack
class FakeQuantWeight(unittest.TestCase):
    """fq_weight is export.closed_form dequantised, bit for bit."""

    def closed(self, w, axis):
        X = L.export_module()
        q, s, z = X.closed_form(w, axis)
        shape = [1] * w.ndim
        if axis is not None:
            shape[axis] = -1
        return (q.astype(np.float32) - z.astype(np.float32).reshape(shape)) * s.reshape(shape)

    def weights(self):
        rng = np.random.default_rng(4)
        w = (rng.normal(size=(6, 9)) * rng.uniform(0.01, 3, size=(6, 1))).astype(np.float32)
        w[2] = 0.0  # a zero channel: scale 1
        return w

    def test_per_output_channel_equals_the_closed_form_dequantised(self):
        w = self.weights()
        got = Q.fq_weight(torch.from_numpy(w), 0).numpy()
        np.testing.assert_array_equal(got, self.closed(w, 0))
        self.assertTrue((got[2] == 0).all())

    def test_per_tensor_equals_the_closed_form_dequantised(self):
        w = self.weights()
        np.testing.assert_array_equal(Q.fq_weight(torch.from_numpy(w), None).numpy(), self.closed(w, None))


@needs_stack
class FakeQuantAct(unittest.TestCase):
    def test_each_row_equals_ort_dynamic_quantize_linear_dequantised(self):
        sess = dql_session()
        rng = np.random.default_rng(0)
        rows = [rng.normal(size=(5, 8)) * 3, np.abs(rng.normal(size=(5, 8))) + 0.5,
                -np.abs(rng.normal(size=(5, 8))) - 0.5, rng.normal(size=(5, 8)) * 0.01]
        x = np.stack(rows).astype(np.float32)  # [4, 5, 8]: mixed, all positive, all negative, small
        got = Q.fq_act(torch.from_numpy(x)).numpy()
        for r in range(len(x)):
            y, s, z = sess.run(None, {"x": x[r]})
            np.testing.assert_array_equal(got[r], (y.astype(np.float32) - np.float32(z)) * s, f"row {r}")

    def test_an_all_positive_row_keeps_zero_in_its_range(self):
        x = torch.tensor([[1.0, 2.0, 3.0]])
        # range [0, 3]: s = 3 / 255, zero point 0, so 0 is representable and 1.0 is not exact
        np.testing.assert_array_equal(Q.fq_act(x).numpy(),
                                      (np.rint(x.numpy() / np.float32(3 / 255)) * np.float32(3 / 255)).astype(np.float32))

    def test_masked_range_of_a_padded_row_equals_the_unpadded_row(self):
        g = torch.Generator().manual_seed(2)
        x = torch.randn(3, 6, 4, generator=g)
        lens = [6, 3, 1]
        mask = torch.zeros(3, 6)
        for r, n in enumerate(lens):
            mask[r, :n] = 1
            x[r, n:] = 50.0 * (r + 1)  # padded positions far outside the row's own range
        got = Q.fq_act(x, mask[:, :, None])
        for r, n in enumerate(lens):
            alone = Q.fq_act(x[r:r + 1, :n])
            self.assertTrue(torch.equal(got[r:r + 1, :n], alone), f"row {r}")

    def test_the_mask_broadcasts_over_heads(self):
        g = torch.Generator().manual_seed(3)
        x = torch.randn(2, 2, 5, 3, generator=g)  # [B, heads, S, d]
        mask = torch.tensor([[1, 1, 1, 1, 1], [1, 1, 0, 0, 0]], dtype=torch.float32)
        x[1, :, 2:] = -40.0
        got = Q.fq_act(x, mask[:, None, :, None])
        self.assertTrue(torch.equal(got[1:, :, :2], Q.fq_act(x[1:, :, :2])))


@needs_stack
class StraightThrough(unittest.TestCase):
    def test_weight_and_input_gradients_equal_the_plain_linear_on_the_quantised_values(self):
        torch.manual_seed(1)
        lin = torch.nn.Linear(6, 4)
        fq = Q.FakeQuantLinear(lin, Q._State())
        x = torch.randn(3, 5, 6, requires_grad=True)
        c = torch.randn(3, 5, 4)
        (fq(x) * c).sum().backward()
        gw, gb, gx = lin.weight.grad.clone(), lin.bias.grad.clone(), x.grad.clone()
        self.assertGreater(float(gw.abs().sum()), 0)

        xq = Q.fq_act(x.detach()).requires_grad_(True)
        ref = torch.nn.Linear(6, 4)
        with torch.no_grad():
            ref.weight.copy_(Q.fq_weight(lin.weight.detach(), 0))
            ref.bias.copy_(lin.bias.detach())
        (ref(xq) * c).sum().backward()
        torch.testing.assert_close(gw, ref.weight.grad, rtol=1e-6, atol=1e-6)
        torch.testing.assert_close(gb, ref.bias.grad, rtol=1e-6, atol=1e-6)
        torch.testing.assert_close(gx, xq.grad, rtol=1e-6, atol=1e-6)

    def test_forward_values_are_the_quantised_ones(self):
        torch.manual_seed(2)
        lin = torch.nn.Linear(6, 4)
        x = torch.randn(2, 3, 6)
        want = torch.nn.functional.linear(Q.fq_act(x), Q.fq_weight(lin.weight.detach(), 0), lin.bias.detach())
        self.assertTrue(torch.equal(Q.FakeQuantLinear(lin, Q._State())(x), want))


# ---- wrap / unwrap / bypass / mask plumbing ----------------------------------------------
@needs_stack
class Wrapping(unittest.TestCase):
    def test_wrap_keeps_parameter_names_objects_and_their_sha_and_unwrap_restores_the_modules(self):
        m = tiny_ce()
        before = dict(m.named_parameters())
        sha = L.params_sha256(L.trainable_state(m))
        Q.wrap(m)
        self.assertTrue(Q.is_wrapped(m))
        after = dict(m.named_parameters())
        self.assertEqual(list(after), list(before))
        self.assertTrue(all(after[n] is before[n] for n in before))
        self.assertEqual(L.params_sha256(L.trainable_state(m)), sha)
        lin, tables, attn = Q.targets(m)
        self.assertEqual(len(lin), 6 * 2 + 2)
        self.assertTrue(all(isinstance(m.get_submodule(p), Q.FakeQuantLinear) for p in lin))
        self.assertTrue(all(isinstance(m.get_submodule(p), Q.FakeQuantEmbedding) for p in tables))
        with self.assertRaises(RuntimeError):
            Q.wrap(m)
        Q.unwrap(m)
        self.assertFalse(Q.is_wrapped(m))
        self.assertTrue(all(type(m.get_submodule(p)) is torch.nn.Linear for p in lin))
        self.assertTrue(all(type(m.get_submodule(p)) is torch.nn.Embedding for p in tables))
        self.assertTrue(all("forward" not in vars(m.get_submodule(p)) for p in attn))
        self.assertTrue(all(p is before[n] for n, p in m.named_parameters()))
        self.assertEqual(list(dict(m.named_parameters())), list(before))

    def test_bypass_is_the_plain_eager_model_exactly(self):
        ids, mask = padded_batch()
        plain = tiny_ce(seed=5)(input_ids=ids, attention_mask=mask).logits
        m = Q.wrap(tiny_ce(seed=5))
        with torch.no_grad():
            on = m(input_ids=ids, attention_mask=mask).logits
            with Q.bypass(m):
                off = m(input_ids=ids, attention_mask=mask).logits
            again = m(input_ids=ids, attention_mask=mask).logits
        self.assertTrue(torch.equal(off, plain))
        self.assertFalse(torch.equal(on, plain))
        self.assertTrue(torch.equal(again, on))
        Q.enabled(m, False)
        with torch.no_grad():
            self.assertTrue(torch.equal(m(input_ids=ids, attention_mask=mask).logits, plain))

    def test_the_attention_mask_keyword_reaches_every_sequence_quantiser(self):
        ids, mask = padded_batch()
        m = Q.wrap(tiny_ce())
        calls = []
        real = Q.fq_act

        def spy(x, mask=None):
            calls.append((x.dim(), None if mask is None else mask.clone()))
            return real(x, mask)

        with mock.patch.object(Q, "fq_act", spy), torch.no_grad():
            m(input_ids=ids, attention_mask=mask)
        dims = [d for d, _ in calls]
        # per layer: 6 Linear inputs [B, S, H] and Q, K, V, probabilities [B, heads, S, d]; then pooler, classifier
        self.assertEqual(dims.count(3), 6 * 2)
        self.assertEqual(dims.count(4), 4 * 2)
        self.assertEqual(dims.count(2), 2)
        for d, mk in calls:
            if d == 3:
                self.assertTrue(torch.equal(mk, mask[:, :, None]))
            elif d == 4:
                self.assertTrue(torch.equal(mk, mask[:, None, :, None]))
            else:
                self.assertIsNone(mk)
        self.assertIsNone(m._qat_state.mask)

    def test_mask_context_sets_the_mask_for_a_call_without_the_keyword(self):
        ids, mask = padded_batch()
        m = Q.wrap(tiny_ce())
        with torch.no_grad():
            kw = m(input_ids=ids, attention_mask=mask).logits
            with Q.mask_context(m, mask):
                ctx = m(ids, mask).logits
        self.assertTrue(torch.equal(ctx, kw))
        self.assertIsNone(m._qat_state.mask)


# ---- helpers of train_ce.py --stage qat --------------------------------------------------
@needs_stack
class Selection(unittest.TestCase):
    def row(self, step, eligible, mrr):
        return {"step": step, "eligible": eligible, "quint8Emu": {"mrr": mrr}}

    def test_the_first_maximum_among_eligible_rows(self):
        rows = [self.row(0, True, 0.40), self.row(100, False, 0.50), self.row(200, True, 0.45),
                self.row(300, True, 0.45), self.row(400, False, 0.60)]
        self.assertEqual(TC.select_row(rows), 2)

    def test_without_an_eligible_row_the_first_highest_mrr(self):
        rows = [self.row(0, False, 0.40), self.row(100, False, 0.47), self.row(200, False, 0.47)]
        self.assertEqual(TC.select_row(rows), 1)

    def test_step_zero_is_a_candidate_and_empty_is_none(self):
        self.assertEqual(TC.select_row([self.row(0, True, 0.5), self.row(100, True, 0.5)]), 0)
        self.assertIsNone(TC.select_row([]))

    def test_predicted_margin_is_q_minus_the_floor_minus_the_safety(self):
        a = types.SimpleNamespace(cp2_allowance=0.0023, cp2_safety=0.005)
        self.assertAlmostEqual(TC.predicted_margin(0.50, 0.51, a), 0.50 - (0.51 - 0.0023) - 0.005, places=12)
        self.assertLess(TC.predicted_margin(0.51, 0.51, types.SimpleNamespace(cp2_allowance=0.0, cp2_safety=0.001)), 0)
        self.assertGreater(TC.predicted_margin(0.51, 0.51, types.SimpleNamespace(cp2_allowance=0.002, cp2_safety=0.001)), 0)


@needs_stack
class QatArgs(unittest.TestCase):
    BASE = ["--lr", "1e-3", "--out", "/nonexistent/x", "--max-len", "32"]

    def test_grid_and_final_configs_do_not_depend_on_the_qat_options(self):
        plain = TC.parse(["--stage", "final"] + self.BASE)
        extra = TC.parse(["--stage", "final"] + self.BASE + ["--steps", "4", "--eval-every", "2", "--cp2-safety", "0.1",
                                                            "--export-minutes", "3", "--init-onnx", "x.onnx"])
        self.assertEqual(L.config_of(plain), L.config_of(extra))
        for k in TC.QAT_ARGS:
            self.assertFalse(hasattr(extra, k), k)

    def test_export_minutes_is_not_part_of_the_qat_config_and_the_safety_is(self):
        q = ["--stage", "qat", "--init-onnx", "x.onnx", "--steps", "4"] + self.BASE
        a, b = TC.parse(q + ["--export-minutes", "1"]), TC.parse(q + ["--export-minutes", "30"])
        self.assertEqual(L.config_of(a), L.config_of(b))
        self.assertNotEqual(L.config_of(a)[1], L.config_of(TC.parse(q + ["--cp2-safety", "0.01"]))[1])

    def test_init_path_expands_variables_and_refuses_an_unset_one(self):
        with mock.patch.dict(os.environ, {"GM734_T_INIT": "/data/init"}):
            self.assertEqual(TC.init_path("$GM734_T_INIT/model.onnx"), Path("/data/init/model.onnx"))
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("GM734_T_UNSET", None)
            with self.assertRaises(SystemExit):
                TC.init_path("$GM734_T_UNSET/model.onnx")


class ProbeExtra(unittest.TestCase):
    """trainlib.Probe adds extra(done_steps) seconds to the job ETA, and the limit applies to the sum."""

    def run_probe(self, **kw):
        t = iter(range(0, 10_000, 60))  # every clock read is 60 s after the last
        with mock.patch.object(L.time, "monotonic", lambda: next(t)):
            p = L.Probe("x", total_steps=10, **kw)
            for i in range(1, 5):
                p.tick(i)
        return p

    @needs_stack
    def test_extra_is_added_to_the_job_eta_with_the_steps_done(self):
        seen = []
        with mock.patch.object(L, "progress") as pr:
            self.run_probe(job_steps=3, extra=lambda d: seen.append(d) or 120.0)
        self.assertEqual(seen, [4])
        self.assertIn("job ETA 28.0 min over 3 step(s) (incl. 2.0 min outside training steps)", pr.call_args_list[0][0][0])

    @needs_stack
    def test_the_limit_applies_to_the_eta_with_the_extra(self):
        with mock.patch.object(L, "progress"):
            with self.assertRaises(SystemExit) as cm:
                self.run_probe(job_steps=3, limit_minutes=27.9, extra=lambda d: 120.0)
            self.assertEqual(cm.exception.code, L.ETA_EXIT)
            self.run_probe(job_steps=3, limit_minutes=28.0, extra=lambda d: 120.0)


# ---- the tiny world: init export, emulation fidelity, init_from_onnx, runs ---------------
def sha256_file(p):
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


@needs_stack
class QatWorld(unittest.TestCase):
    """One tiny world per class, and an "RR final" init: the l6 snapshot with seeded noise
    added to every parameter (so it differs from the base and its logits are O(1)), exported
    through export_dir as <init>/model.onnx."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.tmp.cleanup)
        cls.root = Path(cls.tmp.name)
        cls.world = GF.World(cls.root / "w")
        with mock.patch.dict(os.environ, cls.world.env()), mock.patch.object(GF.W, "VARIANT", GF.FX.VARIANT):
            cls.splits = cls.world.write_splits()
            cls.world.write_teacher(cls.splits)
        cls.world.write_snapshots(("l6",))
        with mock.patch.dict(os.environ, cls.world.env()), contextlib.redirect_stdout(io.StringIO()):
            cls.X = L.export_module()
            cls.snap = L.S.path_of(TC.SNAPSHOT)
            m = TC.load_model(torch.device("cpu"))
            cls.base_sha = L.params_sha256(L.trainable_state(m))
            g = torch.Generator().manual_seed(3)
            with torch.no_grad():
                for _, p in m.named_parameters():
                    p.add_(torch.randn(p.shape, generator=g) * 0.3)
            cls.init_sha = L.params_sha256(L.trainable_state(m))
            cls.init_dir = cls.root / "init"
            cls.X.export_dir(m.eval(), "l6", cls.init_dir, cls.snap)
            cls.init_onnx = cls.init_dir / "model.onnx"
            cls.setUpWorld()

    @classmethod
    def setUpWorld(cls):
        pass

    def setUp(self):
        env = mock.patch.dict(os.environ, self.world.env())
        env.start()
        self.addCleanup(env.stop)

    @classmethod
    def out(cls, *parts):
        return cls.root / "runs" / f"{next(RUN_NUMBER)}" / Path(*parts)

    @classmethod
    def qat_argv(cls, out, *extra):
        return QAT + ["--init-onnx", str(cls.init_onnx), "--init-params-sha256", cls.init_sha, "--out", str(out)] + list(extra)

    @classmethod
    def run_qat(cls, out, *extra):
        argv = cls.qat_argv(out, *extra)
        with contextlib.redirect_stdout(io.StringIO()):
            return TC.run(TC.parse(argv), argv)

    def read(self, out, name):
        return json.loads((Path(out) / name).read_text())


class InitFromOnnx(QatWorld):
    def test_reproduces_the_exported_parameters_and_their_sha(self):
        self.assertNotEqual(self.init_sha, self.base_sha)
        m = TC.load_model(torch.device("cpu"), "eager")
        self.assertEqual(L.init_from_onnx(m, self.init_onnx, self.init_sha), self.init_sha)
        self.assertEqual(L.params_sha256(L.trainable_state(m)), self.init_sha)
        names = {n.name for n in onnx.load(str(self.init_onnx)).graph.node if n.op_type == "MatMul"}
        self.assertIn("/m/bert/encoder/layer.0/attention/self/query/MatMul", names)  # a transposed MatMul weight

    def test_a_wrong_sha_is_refused_and_leaves_the_model_unchanged(self):
        m = TC.load_model(torch.device("cpu"), "eager")
        with self.assertRaises(SystemExit) as cm:
            L.init_from_onnx(m, self.init_onnx, "0" * 64)
        self.assertIn("--init-params-sha256", str(cm.exception.code))
        self.assertEqual(L.params_sha256(L.trainable_state(m)), self.base_sha)


class EmulationVsOrt(QatWorld):
    """The wrapped torch model reproduces the ORT quint8 file of the same parameters (export_dir
    + quantize("l6")) at chunk 1 far below the fp32-vs-quint8 gap."""

    def test_emulated_logits_match_the_ort_quint8_export_at_chunk_1(self):
        qpath = self.root / "emu-q" / "model.onnx"
        self.X.quantize("l6", self.init_onnx, qpath)
        m = TC.load_model(torch.device("cpu"), "eager")
        L.init_from_onnx(m, self.init_onnx, self.init_sha)
        Q.wrap(m).eval()
        ptok = self.X.tokenizer(self.snap, 32)
        pairs = [(f"find f{i} variant {i % 3}", f"f{i} body src f{(i * 7) % 24}") for i in range(24)]
        fq = self.X.ort_forward(self.X.ort_session(qpath))
        ff = self.X.ort_forward(self.X.ort_session(self.init_onnx))
        ft = self.X.torch_forward(m, "l6")
        emu = max(float(np.abs(ft(f) - fq(f)).max()) for f in self.X.batches(ptok, pairs, 1))
        gap = max(float(np.abs(ff(f) - fq(f)).max()) for f in self.X.batches(ptok, pairs, 1))
        self.assertGreater(gap, 1e-2)  # quantisation visibly moves these logits
        self.assertLess(emu, 1e-3)
        self.assertLess(emu, gap / 50)


class QatRun(QatWorld):
    """One in-process --stage qat run (4 steps, dev rows at 0, 2, 4) with spies on the
    checkpoint saves and on the S2 probe's extra term."""

    @classmethod
    def setUpWorld(cls):
        cls.saves = []
        cls.extra = []
        real_save, real_of = L.Checkpointer.save, L.Probe.of

        def save(ck, state):
            cls.saves.append((state["step"], [h["step"] for h in state["history"]]))
            return real_save(ck, state)

        def of(label, total_steps, a, extra=None):
            cls.extra.append(extra)
            return real_of(label, total_steps, a, extra=extra)

        cls.export_minutes = 1000.0
        cls.dir = cls.out("qat")
        t0 = time.monotonic()
        with mock.patch.object(L.Checkpointer, "save", save), mock.patch.object(L.Probe, "of", of):
            try:
                cls.code = cls.run_qat(cls.dir, "--export-minutes", str(cls.export_minutes))
            except SystemExit as e:  # unittest lets a SystemExit out of setUpClass end the whole run
                raise RuntimeError(f"the qat run exited: {e.code}") from e
        cls.elapsed = time.monotonic() - t0

    def test_the_checkpoint_is_saved_after_every_dev_row_including_step_0(self):
        self.assertEqual(self.saves, [(0, [0]), (2, [0, 2]), (4, [0, 2, 4])])

    def test_the_probe_extra_adds_the_remaining_dev_rows_at_the_step_0_cost(self):
        self.assertEqual(len(self.extra), 1)
        f = self.extra[0]
        cost = self.read(self.dir, "train.json")["history"][0]["devSeconds"]
        self.assertGreater(cost, 0)
        with mock.patch.object(TC.time, "monotonic", return_value=1e9):
            at = {d: f(d) for d in range(5)}
        # dev rows still due after d steps: steps 2 and 4
        for d, rows in {0: 2, 1: 2, 2: 1, 3: 1, 4: 0}.items():
            self.assertAlmostEqual(at[d] - at[4], rows * cost, delta=1e-6, msg=f"done {d}")

    def test_the_probe_extra_adds_the_export_minutes_and_the_elapsed_time(self):
        e = self.extra[0](4)  # nothing left to evaluate: elapsed + export
        self.assertGreaterEqual(e, self.export_minutes * 60)
        self.assertLessEqual(e, self.export_minutes * 60 + self.elapsed + 60)

    def test_history_rows_selection_and_outputs(self):
        self.assertIn(self.code, (0, 1))
        t, r = self.read(self.dir, "train.json"), self.read(self.dir, "result.json")
        hist = t["history"]
        self.assertEqual([h["step"] for h in hist], [0, 2, 4])
        for h in hist:
            self.assertAlmostEqual(h["margin"], h["quint8EmuSub"]["mrr"] - (h["fp32Sub"]["mrr"] - 0.0023) - 0.005,
                                   places=12)
            self.assertEqual(h["eligible"], h["margin"] >= 0)
        self.assertEqual(t["selectedStep"], hist[TC.select_row(hist)]["step"])
        self.assertEqual(r["best"]["step"], t["selectedStep"])
        self.assertEqual(t["initParamsSha256"], self.init_sha)
        self.assertNotEqual(t["lastParamsSha256"], self.init_sha)
        self.assertEqual(r["base"]["mrr"], t["baseRows"]["quint8Emu"]["mrr"])
        ex = self.read(self.dir, "export.json")
        self.assertEqual(ex["qatFidelity"]["chunk"], 1)
        self.assertGreater(ex["qatFidelity"]["n"], 0)

    def test_eta_over_the_limit_exits_76(self):
        out = self.out("eta")
        with self.assertRaises(SystemExit) as cm:
            self.run_qat(out, "--export-minutes", "30", "--eta-limit-minutes", "20")
        self.assertEqual(cm.exception.code, L.ETA_EXIT)
        self.assertFalse((out / "export.json").exists())


class ZeroSteps(QatWorld):
    def test_the_zero_step_export_is_byte_identical_to_the_plain_export_of_the_init(self):
        out = self.out("zero")
        self.run_qat(out, "--steps", "0")
        ex = self.read(out, "export.json")
        self.assertEqual(ex["fp32"]["files"]["model.onnx"], sha256_file(self.init_onnx))
        self.assertEqual(sha256_file(out / "onnx-fp32" / "model.onnx"), sha256_file(self.init_onnx))
        q = self.X.quantize("l6", self.init_onnx, self.root / "zero-q" / "model.onnx")
        self.assertEqual(ex["quint8"]["files"]["model.onnx"], q)
        t = self.read(out, "train.json")
        self.assertEqual([h["step"] for h in t["history"]], [0])
        self.assertEqual(t["bestParamsSha256"], self.init_sha)


class QatResume(QatWorld):
    def spawn(self, out, *extra, expect=None):
        r = subprocess.run([sys.executable, str(HERE / "train_ce.py")] + self.qat_argv(out, *extra),
                           env=self.world.child_env(), capture_output=True, text=True, timeout=900)
        if expect is not None:
            self.assertEqual(r.returncode, expect, r.stdout[-1500:] + r.stderr[-3000:])
        else:
            self.assertIn(r.returncode, (0, 1), r.stdout[-1500:] + r.stderr[-3000:])
        return r

    def test_kill_at_step_2_and_resume_equals_uninterrupted(self):
        a, b = self.out("a", "qat"), self.out("b", "qat")
        ra = self.spawn(a)
        self.spawn(b, "--stop-after-steps", "2", expect=STOP)
        self.assertTrue((b / "checkpoint.pt").exists())
        self.assertFalse((b / "result.json").exists())
        rb = self.spawn(b)
        self.assertEqual(ra.returncode, rb.returncode)
        self.assertIn("resumed at step 2", rb.stdout)
        ta, tb = self.read(a, "train.json"), self.read(b, "train.json")
        for k in ("lastParamsSha256", "bestParamsSha256", "selectedStep"):
            self.assertEqual(ta[k], tb[k], k)
        key = lambda h: [(x["step"], x["quint8Emu"]["mrr"], x["fp32Sub"]["mrr"], x["loss"]) for x in h]  # noqa: E731
        self.assertEqual(key(ta["history"]), key(tb["history"]))
        self.assertEqual(self.read(a, "result.json"), self.read(b, "result.json"))


class MakeQatJob(QatWorld):
    def init_run(self, best_sha=None, recorded=None):
        run = self.out("rr-final-run")
        d = run / "out" / "final" / "onnx-fp32"
        d.mkdir(parents=True)
        shutil.copyfile(self.init_onnx, d / "model.onnx")
        (d.parent / "train.json").write_text(json.dumps({"bestParamsSha256": best_sha or self.init_sha}))
        (d.parent / "export.json").write_text(
            json.dumps({"fp32": {"files": {"model.onnx": recorded or sha256_file(self.init_onnx)}}}))
        return run

    def test_the_job_starts_from_the_init_export_and_loads_as_a_job(self):
        run = self.init_run()
        with contextlib.redirect_stdout(io.StringIO()):
            path = MJ.write("rr-qat", MJ.qat_job(run), self.root / "jobs")
        import gpu_train
        job = gpu_train.load_job(path)
        self.assertEqual([s["name"] for s in job["steps"]], ["qat"])
        args = job["steps"][0]["args"]
        self.assertEqual(args[args.index("--init-params-sha256") + 1], self.init_sha)
        self.assertEqual(args[args.index("--stage") + 1], "qat")
        self.assertNotIn("--bf16", args)
        init = [d for d in job["data"] if d["env"] == "GM730_INIT"]
        self.assertEqual(len(init), 1)
        self.assertEqual(Path(init[0]["local_root"]).expanduser().resolve(),
                         (run / "out" / "final" / "onnx-fp32").resolve())
        self.assertEqual(B.route_of("gm730-rr-qat"), "rr")

    def test_refuses_an_init_export_whose_parameters_do_not_hash_to_best_params_sha256(self):
        run = self.init_run(best_sha=self.base_sha)
        with self.assertRaises(SystemExit) as cm:
            MJ.qat_job(run)
        self.assertIn("bestParamsSha256", str(cm.exception.code))

    def test_refuses_a_file_that_is_not_the_one_export_json_recorded(self):
        run = self.init_run(recorded="0" * 64)
        with self.assertRaises(SystemExit) as cm:
            MJ.qat_job(run)
        self.assertIn("export.json", str(cm.exception.code))


if __name__ == "__main__":
    unittest.main()
