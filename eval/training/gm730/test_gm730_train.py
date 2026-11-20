"""Tests of the GM-730 trainers and the teacher scorer (train_ce.py, train_encoder.py,
score_teacher.py; docs/design/GM-730-gpu-training-routes.md sections 1, 2 and 8) on tiny
random BERT snapshots and a tiny GM-456 world (gm730_fixture.py): kill + resume equals an
uninterrupted run, the shuffled controls (derangement, teacher permutation), the planted
C-P failure, C-T, C-P2, the LoRA merge before export. No download; CPU only. Needs torch,
transformers, tokenizers, onnx, onnxruntime and peft; skipped without them.

The resume tests run the trainers as separate processes (`--stop-after-steps` exits 75
right after a checkpoint, a fresh process resumes); everything else runs in-process.

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_train.py'
"""

import contextlib
import io
import itertools
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm730_fixture as GF  # noqa: E402

try:
    import onnx
    import onnxruntime  # noqa: F401
    import peft  # noqa: F401
    import tokenizers  # noqa: F401
    import torch
    import transformers
    import score_teacher as ST
    import train_ce as TC
    import train_encoder as TE
    import trainlib as L
except ImportError:  # pragma: no cover
    torch = None

needs_stack = unittest.skipIf(torch is None, "torch, transformers, peft, onnx or onnxruntime is not installed (GM-730 venv)")
COMMON = ["--lr", "1e-3", "--epochs", "2", "--batch", "6", "--seed", "1", "--resamples", "50", "--eval-batch", "16",
          "--ckpt-minutes", "60", "--threads", "1"]
CE = COMMON + ["--max-len", "32", "--micro-queries", "3", "--micro-batch", "16", "--eval-queries", "4"]
ENC = COMMON + ["--docs-per-query", "4", "--max-len-query", "32", "--max-len-doc", "32"]
STOP = 75
RUN_NUMBER = itertools.count()


@needs_stack
class TinyWorld(unittest.TestCase):
    """One tiny world per class: windows, teacher and snapshots under a temp dir."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.tmp.cleanup)
        cls.root = Path(cls.tmp.name)
        cls.world = GF.World(cls.root / "w")
        with mock.patch.dict(os.environ, cls.world.env()), mock.patch.object(GF.W, "VARIANT", GF.FX.VARIANT):
            cls.splits = cls.world.write_splits()
            cls.world.write_teacher(cls.splits)
        cls.world.write_snapshots()

    def setUp(self):
        env = mock.patch.dict(os.environ, self.world.env())
        env.start()
        self.addCleanup(env.stop)

    def out(self, *parts):
        return self.root / "runs" / f"{next(RUN_NUMBER)}" / Path(*parts)

    def spawn(self, script, args, out, expect=0):
        r = subprocess.run([sys.executable, str(HERE / script)] + args + ["--out", str(out)], env=self.world.child_env(),
                           capture_output=True, text=True, timeout=900)
        self.assertEqual(r.returncode, expect, r.stdout[-1500:] + r.stderr[-3000:])
        return r

    def read(self, out, name):
        return json.loads((Path(out) / name).read_text())

    def resume_equal(self, script, args, name):
        """Kill after step 4 (epoch 2, step 1; 3 steps per epoch), resume, and compare with a
        run that was never stopped. Both runs have the same --out basename."""
        a, b = self.out("a", name), self.out("b", name)
        self.spawn(script, args, a)
        self.spawn(script, args + ["--stop-after-steps", "4"], b, expect=STOP)
        self.assertTrue((b / "checkpoint.pt").exists())
        self.assertFalse((b / "result.json").exists())
        r = self.spawn(script, args, b)
        self.assertIn("resumed at epoch 2 step 1", r.stdout)
        ta, tb = self.read(a, "train.json"), self.read(b, "train.json")
        self.assertEqual(ta["lastParamsSha256"], tb["lastParamsSha256"])
        key = lambda h: [(x["epoch"], x["mrr"], x["recall@10"], x.get("loss")) for x in h]  # noqa: E731
        self.assertEqual(key(ta["history"]), key(tb["history"]))
        self.assertEqual(self.read(a, "result.json"), self.read(b, "result.json"))
        return a, b, ta, tb

    def best_is_first_maximum(self, out):
        t, r = self.read(out, "train.json"), self.read(out, "result.json")
        mrrs = [h["mrr"] for h in t["history"]]
        self.assertEqual(r["best"]["epoch"], mrrs.index(max(mrrs)))
        self.assertEqual(t["history"][0]["epoch"], 0)
        self.assertEqual(r["base"]["mrr"], mrrs[0])


class Resume(TinyWorld):
    def test_ce_kill_and_resume_equals_uninterrupted(self):
        a, b, ta, tb = self.resume_equal("train_ce.py", ["--stage", "final"] + CE, "step")
        self.assertEqual(ta["bestParamsSha256"], tb["bestParamsSha256"])
        self.best_is_first_maximum(a)
        for d in (a, b):
            for f in ("onnx-fp32/model.onnx", "onnx-fp32/tokenizer.json", "onnx-fp32/config.json",
                      "onnx-quint8/model.onnx", "onnx-quint8/tokenizer.json", "export.json"):
                self.assertTrue((d / f).exists(), f)
            ex = self.read(d, "export.json")
            self.assertTrue(ex["cp"]["pass"])
            self.assertTrue(ex["cp2"]["pass"])
            self.assertFalse((d / "_base_tmp").exists())

    def test_encoder_full_kill_and_resume_equals_uninterrupted(self):
        a, b, _, _ = self.resume_equal("train_encoder.py", ["--model", "gte-small", "--stage", "grid", "--alpha", "0.5"] + ENC, "step")
        self.best_is_first_maximum(a)
        self.assertFalse((a / "onnx-fp32").exists())

    def test_encoder_lora_kill_and_resume_equals_uninterrupted_and_the_export_is_merged(self):
        args = ["--model", "jina", "--mode", "lora", "--rank", "4", "--lora-targets", "query,key,value,dense",
                "--stage", "final"] + ENC
        a, b, ta, tb = self.resume_equal("train_encoder.py", args, "step")
        self.assertEqual(ta["bestParamsSha256"], tb["bestParamsSha256"])
        self.assertLess(ta["params"]["trainable"], ta["params"]["total"])
        for d in (a, b):
            ex = self.read(d, "export.json")
            self.assertTrue(ex["cp"]["pass"])
            self.assertIn("cp2", ex)
            self.assertTrue((d / "onnx-int8" / "model.onnx").exists())
            self.assertFalse((d / "_base_tmp").exists())
            names = [i.name for i in onnx.load(str(d / "onnx-fp32" / "model.onnx")).graph.initializer]
            self.assertFalse([n for n in names if "lora" in n.lower()], "adapters were exported, not merged")


class Controls(TinyWorld):
    def enc_args(self, **extra):
        argv = ["--model", "gte-small", "--stage", "grid", "--alpha", "0.5", "--out", str(self.out("c"))] + ENC
        for k, v in extra.items():
            argv += [f"--{k.replace('_', '-')}", v]
        return TE.parse(argv)

    def test_shuffled_encoder_control_deranges_the_labels_and_permutes_the_teacher_within_windows(self):
        plain = TE.Data(self.enc_args())
        shuf = TE.Data(self.enc_args(control="shuffled"))
        targets = sorted({q["target"] for q in plain.train_q})
        self.assertGreater(len(targets), 2)
        self.assertTrue(all(q["gold"] == q["target"] for q in plain.train_q))
        self.assertTrue(all(q["gold"] != q["target"] for q in shuf.train_q), "a query kept its own target's labels")
        partner = {}
        for q in shuf.train_q:
            self.assertEqual(partner.setdefault(q["target"], q["gold"]), q["gold"])
        self.assertEqual(sorted(partner.values()), targets, "the labels are not a permutation of the targets")
        self.assertEqual([q["id"] for q in shuf.train_q], [q["id"] for q in plain.train_q])
        self.assertEqual([q["text"] for q in shuf.train_q], [q["text"] for q in plain.train_q])
        # the teacher: same keys, same multiset per window, different order somewhere
        self.assertEqual(set(shuf.windows), set(plain.windows))
        moved = 0
        for qid, (keys, t) in shuf.windows.items():
            self.assertEqual(keys, plain.windows[qid][0])
            np.testing.assert_array_equal(np.sort(t), np.sort(plain.windows[qid][1]))
            moved += not np.array_equal(t, plain.windows[qid][1])
        self.assertGreater(moved, 0)
        again = TE.Data(self.enc_args(control="shuffled"))
        self.assertEqual([q["gold"] for q in again.train_q], [q["gold"] for q in shuf.train_q])
        for qid in shuf.windows:
            np.testing.assert_array_equal(again.windows[qid][1], shuf.windows[qid][1])

    def test_alpha_zero_loads_no_teacher(self):
        a = TE.parse(["--model", "gte-small", "--stage", "grid", "--out", str(self.out("c"))] + ENC)
        self.assertEqual(TE.Data(a).windows, {})

    def test_shuffled_teacher_control_permutes_each_window_and_keeps_the_gold(self):
        base = ["--stage", "grid", "--out", str(self.out("c"))] + CE
        plain = TC.Data(TC.parse(base))
        shuf = TC.Data(TC.parse(base + ["--control", "shuffled-teacher"]))
        self.assertEqual([(w["id"], w["gold"], w["keys"]) for w in shuf.train], [(w["id"], w["gold"], w["keys"]) for w in plain.train])
        moved = 0
        for w in plain.train:
            np.testing.assert_array_equal(np.sort(shuf.teacher[w["id"]]), np.sort(plain.teacher[w["id"]]))
            moved += not np.array_equal(shuf.teacher[w["id"]], plain.teacher[w["id"]])
        self.assertGreater(moved, 0)

    def test_batch_columns_mark_gold_and_positives_and_mask_answers_also(self):
        batch = [{"positives": ["p"], "gold": "g", "answersAlso": ["a", "p"]}, {"positives": [], "gold": "a", "answersAlso": ["g"]}]
        cols, pos, valid = TE.batch_columns(batch, [["g", "n", "a"], ["a", "p"]])
        self.assertEqual(cols, ["g", "n", "a", "p"])
        self.assertEqual(pos.tolist(), [[True, False, False, True], [False, False, True, False]])
        self.assertEqual(valid.tolist(), [[True, True, False, True], [False, True, True, True]])

    def test_sample_negatives_is_bounded_and_deterministic_in_seed_epoch_and_index(self):
        negs = [f"n{i}" for i in range(20)]
        q = {"negatives": negs}
        f = lambda *a: TE.Data.sample_negatives(None, *a)  # noqa: E731
        self.assertEqual(f(0, {"negatives": negs[:3]}, 5, 1, 1), negs[:3])
        a = f(2, q, 5, 1, 1)
        self.assertEqual(len(a), 5)
        self.assertEqual(a, f(2, q, 5, 1, 1))
        self.assertTrue(set(a) <= set(negs))
        self.assertTrue(any(f(2, q, 5, 1, e) != a for e in range(2, 8)))
        self.assertTrue(any(f(i, q, 5, 1, 1) != a for i in range(3, 8)))

    def test_lora_is_refused_for_gte_small_and_trains_only_adapters_for_jina(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            TE.parse(["--model", "gte-small", "--mode", "lora", "--stage", "grid", "--out", "x"] + ENC)
        a = TE.parse(["--model", "jina", "--mode", "lora", "--rank", "4", "--lora-targets", "query,key,value,dense",
                      "--stage", "grid", "--out", "x"] + ENC)
        m = TE.load_model("jina", a, torch.device("cpu"))
        names = [n for n, p in m.named_parameters() if p.requires_grad]
        self.assertTrue(names)
        self.assertTrue(all("lora_" in n for n in names))
        self.assertEqual(set(L.trainable_state(m)), set(names))


class Accumulation(TinyWorld):
    def test_micro_query_accumulation_gives_the_gradient_of_one_full_batch_pass(self):
        grads, losses = {}, {}
        for micro in (1, 2, 6):
            a = TC.parse(["--stage", "grid", "--out", str(self.out("c")), "--alpha", "0.5"] + CE + ["--micro-queries", str(micro)])
            L.setup_torch(a)
            data = TC.Data(a)
            tok = L.tokenizer(L.S.path_of(TC.SNAPSHOT), a.max_len)
            model = TC.load_model(torch.device("cpu"))
            model.eval()  # no dropout: the passes must see the same graph
            opt = torch.optim.AdamW(model.parameters(), lr=0.0)
            sched = torch.optim.lr_scheduler.LambdaLR(opt, lambda s: 1.0)
            ws = data.train[:6]
            losses[micro] = TC.train_step(model, opt, sched, tok, data, ws, torch.device("cpu"), a, L.autocast(a, torch.device("cpu")))
            grads[micro] = {n: p.grad.clone() for n, p in model.named_parameters() if p.grad is not None}
        for micro in (1, 2):
            self.assertAlmostEqual(losses[micro], losses[6], places=5)
            self.assertEqual(set(grads[micro]), set(grads[6]))
            self.assertGreater(len(grads[6]), 0)
            for n in grads[6]:
                torch.testing.assert_close(grads[micro][n], grads[6][n], rtol=1e-4, atol=1e-6)


def plant_after_export(case):
    """export_dir of gm730/export.py with one weight row of its model.onnx scaled (C-F)."""
    X = L.export_module()
    real = X.export_dir

    def planted(torch_model, model, out_dir, snap_dir=None):
        files = real(torch_model, model, out_dir, snap_dir)
        X.plant_row(Path(out_dir) / "model.onnx", Path(out_dir) / "model.onnx", 100.0)
        return files

    p = mock.patch.object(X, "export_dir", planted)
    p.start()
    case.addCleanup(p.stop)


class PlantedParity(TinyWorld):
    def test_ce_final_step_exits_1_when_the_exported_model_is_perturbed(self):
        argv = ["--stage", "final", "--out", str(self.out("p"))] + CE + ["--epochs", "1"]
        out = Path(argv[argv.index("--out") + 1])
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(TC.run(TC.parse(argv), argv), 0)
        clean = self.read(out, "export.json")
        self.assertTrue(clean["cp"]["pass"])
        plant_after_export(self)
        argv2 = ["--stage", "final", "--out", str(self.out("p2"))] + CE + ["--epochs", "1"]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(TC.run(TC.parse(argv2), argv2), 1)
        planted = self.read(Path(argv2[argv2.index("--out") + 1]), "export.json")
        self.assertFalse(planted["cp"]["pass"])
        self.assertGreater(planted["cp"]["maxAbsDiff"], 1e-4)

    def test_encoder_final_step_exits_1_when_the_exported_model_is_perturbed(self):
        argv = ["--model", "gte-small", "--stage", "final", "--out", str(self.out("p"))] + ENC + ["--epochs", "1"]
        plant_after_export(self)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(TE.run(TE.parse(argv), argv), 1)
        self.assertFalse(self.read(Path(argv[argv.index("--out") + 1]), "export.json")["cp"]["pass"])


class CP2Rule(TinyWorld):
    """C-P2 of the jina int8: dev MRR (int8) >= dev MRR (fp32) - (base gap + 0.01)."""

    def export(self, int8_rank):
        argv = ["--model", "jina", "--stage", "final", "--out", str(self.out("cp2"))] + ENC
        a = TE.parse(argv)
        data = TE.Data(a)
        toks = (L.tokenizer(L.S.path_of("jina"), 32), L.tokenizer(L.S.path_of("jina"), 32))
        model = TE.load_model("jina", a, torch.device("cpu"))
        out = Path(a.out)
        out.mkdir(parents=True)
        evals = lambda rank: [(q["id"], q["target"], rank) for q in data.dev_q]  # noqa: E731
        calls = iter([evals(1.0), evals(int8_rank)])  # the base's int8, then the trained model's
        with mock.patch.object(TE, "ort_dev_eval", lambda *x: next(calls)), contextlib.redirect_stdout(io.StringIO()):
            res, ok = TE.export_final(model, 0.5, 0.5, toks, data, a, out)
        return res, ok

    def test_int8_below_the_floor_fails(self):
        res, ok = self.export(10.0)
        self.assertFalse(ok)
        c = res["cp2"]
        self.assertFalse(c["pass"])
        self.assertAlmostEqual(c["floor"], 0.5 - (-0.5 + 0.01))
        self.assertTrue(res["cp"]["pass"])

    def test_int8_at_the_floor_passes(self):
        res, ok = self.export(1.0)
        self.assertTrue(ok)
        self.assertTrue(res["cp2"]["pass"])


class Teacher(TinyWorld):
    def setUp(self):
        super().setUp()
        load = lambda device: transformers.AutoModelForSequenceClassification.from_pretrained(  # noqa: E731
            str(L.S.path_of(ST.SNAPSHOT))).to(device).eval()
        p = mock.patch.object(ST, "load_model", load)
        p.start()
        self.addCleanup(p.stop)
        self.ct = self.out("ct")
        e = mock.patch.dict(os.environ, {"GM730_CT": str(self.ct)})
        e.start()
        self.addCleanup(e.stop)

    def score_args(self, out, *extra):
        return ST.parse(["--out", str(out), "--chunk", "40", "--batch", "8", "--threads", "1", "--max-len", "32",
                         "--ckpt-minutes", "60"] + list(extra))

    def make_ct(self, n=20, seed=3):
        with contextlib.redirect_stdout(io.StringIO()):
            ST.main(["make-ct", "--out-dir", str(self.ct), "--n", str(n), "--seed", str(seed), "--threads", "1"])
        return L.read_jsonl(self.ct / ST.CT_FILE)

    def score(self, out, *extra):
        with contextlib.redirect_stdout(io.StringIO()):
            return ST.score_main(self.score_args(out, *extra))

    def test_kill_and_resume_gives_byte_equal_arrays_and_the_windows_pair_order(self):
        self.make_ct()
        a, b = self.out("a"), self.out("b")
        self.assertEqual(self.score(a), 0)
        with self.assertRaises(SystemExit) as cm:
            self.score(b, "--stop-after-steps", "2")
        self.assertEqual(cm.exception.code, STOP)
        self.assertFalse((b / "teacher.json").exists())
        self.assertEqual(self.score(b), 0)
        for s in ST.SPLITS:
            self.assertEqual((a / f"{s}.f32").read_bytes(), (b / f"{s}.f32").read_bytes())
        meta = self.read(b, "teacher.json")
        for s in ST.SPLITS:
            windows, _, _ = L.load_split(s)
            sp = meta["splits"][s]
            self.assertEqual(sp["pairsSha256"], L.pairs_sha256(windows))
            self.assertEqual(sp["pairs"], sum(len(w["keys"]) for w in windows))
            self.assertEqual(sp["sha256"], L.S.sha256_file(b / sp["file"]))
        self.assertTrue(meta["ct"]["pass"])
        with mock.patch.dict(os.environ, {"GM730_TEACHER": str(b)}):
            windows, _, _ = L.load_split("train")
            self.assertEqual(len(L.load_teacher("train", windows)), len(windows))

    def test_c_t_fails_above_tolerance_on_a_planted_logit_offset(self):
        rows = self.make_ct()
        for r in rows:
            r["logit"] += 0.2
        (self.ct / ST.CT_FILE).write_text("".join(json.dumps(r) + "\n" for r in rows))
        out = self.out("bad")
        self.assertEqual(self.score(out), 1)
        ct = self.read(out, "teacher.json")["ct"]
        self.assertFalse(ct["pass"])
        self.assertGreater(ct["maxAbsDiff"], ST.CT_TOL)
        self.assertEqual(self.read(out, "ct.json")["pass"], False)

    def test_c_t_passes_on_the_unplanted_reference(self):
        self.make_ct()
        out = self.out("ok")
        self.assertEqual(self.score(out), 0)
        self.assertLess(self.read(out, "teacher.json")["ct"]["maxAbsDiff"], ST.CT_TOL)

    def test_no_ct_skips_the_check_and_load_teacher_then_refuses(self):
        out = self.out("nt")
        self.assertEqual(self.score(out, "--no-ct"), 0)
        self.assertEqual(self.read(out, "teacher.json")["ct"], {"check": "C-T", "skipped": "--no-ct"})
        self.assertFalse((out / "ct.json").exists())
        windows, _, _ = L.load_split("train")
        with mock.patch.dict(os.environ, {"GM730_TEACHER": str(out)}), self.assertRaises(SystemExit):
            L.load_teacher("train", windows)

    def test_make_ct_samples_n_dev_pairs_deterministically_by_seed(self):
        a = self.make_ct(10, 3)
        self.assertEqual(len(a), 10)
        self.assertEqual(set(a[0]), {"pair", "query", "text", "logit"})
        self.assertEqual([r["pair"] for r in a], sorted(r["pair"] for r in a))
        b = self.make_ct(10, 3)
        self.assertEqual([(r["pair"], r["query"], r["text"]) for r in a], [(r["pair"], r["query"], r["text"]) for r in b])
        c = self.make_ct(10, 4)
        self.assertNotEqual([r["pair"] for r in a], [r["pair"] for r in c])
        windows, texts, _ = L.load_split("dev")
        pairs = ST.split_pairs(windows, texts)
        for r in a:
            self.assertEqual((r["query"], r["text"]), pairs[r["pair"]])


    # --- GM-730/S26: non-finite teacher logits (CUDA sdpa NaN on fully masked rows) ---

    def nan_scorer(self, bad, calls, when=lambda pairs: True):
        """A scorer wrapping the real one; call number i in `bad` (and when(pairs)) gets a NaN at position 0."""
        real = ST.scorer

        def make(model, tok, device, batch):
            inner = real(model, tok, device, batch)

            def f(pairs):
                out = inner(pairs)
                calls.append(len(pairs))
                if when(pairs) and (len(calls) - 1) in bad:
                    out = out.copy()
                    out[0] = np.nan
                return out
            return f
        return mock.patch.object(ST, "scorer", make)

    def written(self, out):
        return sorted(p.name for p in Path(out).glob("*") if p.name.endswith((".f32", ".json")))

    def test_a_nan_chunk_stops_the_job_naming_the_pair_and_writes_no_output(self):
        self.make_ct()
        out = self.out("nan")
        calls = []
        with self.nan_scorer({1}, calls), self.assertRaises(SystemExit) as cm:
            self.score(out)
        msg = str(cm.exception.code)
        self.assertTrue(msg.startswith("non-finite teacher logits: "), msg)
        self.assertIn("(first at pair 40)", msg)  # chunk 1 of 40 pairs, NaN at its position 0
        self.assertEqual(self.written(out), [])

    def test_the_first_bad_chunk_stops_the_run_before_the_next_chunk_is_scored(self):
        self.make_ct()
        calls = []
        with self.nan_scorer({0}, calls), self.assertRaises(SystemExit):
            self.score(self.out("ff"))
        self.assertEqual(len(calls), 1)

    def test_a_resumed_checkpoint_holding_nan_is_refused_before_any_output(self):
        self.make_ct()
        out = self.out("old")
        calls = []
        # an older run without the per-chunk guard: NaN gets into the checkpoint
        with self.nan_scorer({0}, calls), mock.patch.object(ST, "require_finite", lambda what, x, offset=0: x), \
                self.assertRaises(SystemExit) as cm:
            self.score(out, "--stop-after-steps", "2")
        self.assertEqual(cm.exception.code, STOP)
        with self.assertRaises(SystemExit) as cm:
            self.score(out)
        self.assertTrue(str(cm.exception.code).startswith("non-finite teacher logits: "), cm.exception.code)
        self.assertEqual(self.written(out), [])

    def test_c_t_scoring_a_nan_fails_the_job_instead_of_reporting_a_nan_diff(self):
        rows = self.make_ct()
        ct_pairs = {(r["query"], r["text"]) for r in rows}
        out = self.out("ctnan")
        calls = []
        with self.nan_scorer(set(range(1000)), calls, when=lambda pairs: set(pairs) == ct_pairs), \
                self.assertRaises(SystemExit) as cm:
            self.score(out)
        self.assertTrue(str(cm.exception.code).startswith("non-finite teacher logits: C-T pairs"), cm.exception.code)
        self.assertFalse((out / "ct.json").exists())
        self.assertFalse((out / "teacher.json").exists())

    def test_make_ct_refuses_to_write_a_non_finite_reference_logit(self):
        ct = self.out("ctref")
        calls = []
        with self.nan_scorer({0}, calls), self.assertRaises(SystemExit) as cm, contextlib.redirect_stdout(io.StringIO()):
            ST.main(["make-ct", "--out-dir", str(ct), "--n", "10", "--seed", "3", "--threads", "1"])
        self.assertTrue(str(cm.exception.code).startswith("non-finite teacher logits: make-ct pairs"), cm.exception.code)
        self.assertFalse((ct / ST.CT_FILE).exists())


class TeacherModel(TinyWorld):
    def test_the_teacher_loads_with_eager_attention_and_stays_finite_on_over_padded_rows(self):
        cfg = transformers.ModernBertConfig(vocab_size=64, hidden_size=16, intermediate_size=32, num_hidden_layers=2,
                                            num_attention_heads=2, local_attention=4, global_attn_every_n_layers=2,
                                            max_position_embeddings=64, pad_token_id=0, num_labels=1)
        snap = self.root / "modernbert"
        torch.manual_seed(0)
        transformers.ModernBertForSequenceClassification(cfg).save_pretrained(snap)
        with mock.patch.object(L.S, "path_of", lambda name: snap):
            model = ST.load_model(torch.device("cpu"))
        self.assertEqual(model.config._attn_implementation, "eager")
        # CPU sdpa does not NaN; emulate the CUDA memory-efficient kernel on all-masked rows (pytorch#110213)
        real = torch.nn.functional.scaled_dot_product_attention

        def cuda_like(q, k, v, attn_mask=None, **kw):
            out = real(q, k, v, attn_mask=attn_mask, **kw)
            if attn_mask is not None:
                dead = (attn_mask <= torch.finfo(attn_mask.dtype).min).all(-1, keepdim=True).expand_as(out)
                out = out.masked_fill(dead, float("nan"))
            return out

        ids = torch.randint(1, 64, (2, 24))
        mask = torch.ones(2, 24, dtype=torch.long)
        mask[1, 6:] = 0  # 18 padding positions > local_attention // 2 == 2 past the last real token
        with mock.patch.object(torch.nn.functional, "scaled_dot_product_attention", cuda_like), torch.no_grad():
            logits = model(input_ids=ids, attention_mask=mask).logits
        self.assertTrue(bool(torch.isfinite(logits).all()), logits)


if __name__ == "__main__":
    unittest.main()


def tiny_bert(seed=0):
    cfg = transformers.BertConfig(vocab_size=50, hidden_size=16, num_hidden_layers=2, num_attention_heads=2,
                                  intermediate_size=32, max_position_embeddings=32, pad_token_id=0)
    torch.manual_seed(seed)
    return transformers.BertModel(cfg)


@needs_stack
class GradCheckpoint(unittest.TestCase):
    """--grad-checkpoint changes memory and time, never the loss or the gradients (dropout 0.1
    is active; the recompute replays the RNG). In-process, a 2-layer random BERT."""

    def loss_and_grads(self, lora, ckpt):
        m = tiny_bert()
        if lora:
            from peft import LoraConfig, get_peft_model
            m = get_peft_model(m, LoraConfig(r=4, lora_alpha=8, lora_dropout=0.0, bias="none",
                                             target_modules=["query", "key", "value", "dense"]))
        TE.grad_checkpoint(m, on=ckpt)
        m.train()
        ids = torch.randint(1, 50, (3, 9), generator=torch.Generator().manual_seed(5))
        mask = torch.ones_like(ids)
        torch.manual_seed(11)  # the dropout masks
        loss = m(input_ids=ids, attention_mask=mask).last_hidden_state.pow(2).mean()
        loss.backward()
        return loss.item(), {n: p.grad.clone() for n, p in m.named_parameters() if p.grad is not None}

    def same(self, lora):
        l0, g0 = self.loss_and_grads(lora, False)
        l1, g1 = self.loss_and_grads(lora, True)
        self.assertEqual(l0, l1)
        self.assertGreater(len(g0), 0)
        self.assertEqual(set(g0), set(g1))
        for n in g0:
            torch.testing.assert_close(g1[n], g0[n], rtol=0, atol=1e-6)
        if lora:
            self.assertTrue(all("lora_" in n for n in g1), "only adapters train")
        return g1

    def test_full_training_gives_the_same_loss_and_gradients_on_and_off(self):
        self.same(lora=False)

    def test_lora_behind_frozen_embeddings_gets_the_same_gradients_on_and_off(self):
        g = self.same(lora=True)
        self.assertTrue(any(p.abs().sum() > 0 for p in g.values()))

    def test_only_a_training_forward_with_grad_is_checkpointed_and_off_restores_the_layer(self):
        from torch.utils import checkpoint as C
        m = tiny_bert()
        keys = list(m.state_dict())
        ids = torch.randint(1, 50, (2, 6))
        with mock.patch.object(C, "checkpoint", wraps=C.checkpoint) as ck:
            TE.grad_checkpoint(m, on=True)
            self.assertEqual(list(m.state_dict()), keys)
            m.train()
            m(input_ids=ids)
            self.assertEqual(ck.call_count, 2)  # one per layer
            m.eval()
            m(input_ids=ids)
            with torch.no_grad():
                m.train()
                m(input_ids=ids)
            self.assertEqual(ck.call_count, 2)
            TE.grad_checkpoint(m, on=False)
            m.train()
            m(input_ids=ids)
            self.assertEqual(ck.call_count, 2)
        self.assertTrue(all("forward" not in layer.__dict__ for layer in TE.encoder_layers(m)))

    def test_the_flag_is_not_part_of_the_config_and_its_sha256(self):
        base = ["--model", "gte-small", "--stage", "grid", "--out", "x", "--alpha", "0.5"] + ENC
        off, on = TE.parse(base), TE.parse(base + ["--grad-checkpoint"])
        self.assertFalse(off.grad_checkpoint)
        self.assertTrue(on.grad_checkpoint)
        self.assertEqual(L.config_of(off), L.config_of(on))
        self.assertNotIn("grad_checkpoint", L.config_of(on)[0])


class GradCheckpointRun(TinyWorld):
    def test_a_run_with_the_flag_ends_in_the_same_weights_and_config_and_records_the_flag(self):
        args = ["--model", "gte-small", "--stage", "grid", "--alpha", "0.5"] + ENC
        a, b = self.out("off", "step"), self.out("on", "step")
        self.spawn("train_encoder.py", args, a)
        self.spawn("train_encoder.py", args + ["--grad-checkpoint"], b)
        ta, tb = self.read(a, "train.json"), self.read(b, "train.json")
        self.assertFalse(ta["gradCheckpoint"])
        self.assertTrue(tb["gradCheckpoint"])
        self.assertEqual(ta["configSha256"], tb["configSha256"])
        self.assertEqual(ta["lastParamsSha256"], tb["lastParamsSha256"])

    def test_the_flag_turns_checkpointing_on_for_training_and_off_before_the_export(self):
        calls = []
        real = TE.grad_checkpoint

        def spy(model, on=True):
            calls.append(on)
            return real(model, on)
        args = ["--model", "gte-small", "--stage", "final", "--alpha", "0.5", "--grad-checkpoint"] + ENC
        with mock.patch.object(TE, "grad_checkpoint", spy):
            TE.main(args + ["--out", str(self.out("spy", "step"))])
        self.assertEqual(calls, [True, False])
        calls.clear()
        with mock.patch.object(TE, "grad_checkpoint", spy):
            TE.main([x for x in args if x != "--grad-checkpoint"] + ["--out", str(self.out("spy2", "step"))])
        self.assertEqual(calls, [False])
