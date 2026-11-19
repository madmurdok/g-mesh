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


if __name__ == "__main__":
    unittest.main()
