"""Tests of register_arms.py (docs/design/GM-730-gpu-training-routes.md section 8): the pins
of a registered arm, verify (tampered file or dir refused), check_run (a run whose
modelFiles are not the pins refused) and the words-shuffled arm checked through its
reference arm's pins and runs. A temp variants.toml and models dir; stdlib only (Python 3.11).

Run: python -m unittest discover -s eval/training/gm730 -p 'test_gm730_register.py'
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import register_arms as R  # noqa: E402

BASE = "jina-v2-base-code-int8-structured"
ARM = "gm730-test-arm"
WS = ARM + "-words-shuffled"


class Case(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        env = mock.patch.dict(os.environ, {"GM730_DATA": str(self.root / "data")})
        env.start()
        self.addCleanup(env.stop)
        base_dir = self.root / "base"
        base_dir.mkdir()
        self.variants = self.root / "variants.toml"
        self.variants.write_text(f'[[variant]]\nname = "{BASE}"\narm = "model"\nrole = "control"\n'
                                 f'model_dir = "{base_dir}"\ntext = "structured"\n')
        src = self.root / "src"
        src.mkdir()
        (src / "model.onnx").write_bytes(b"onnx bytes")
        (src / "tokenizer.json").write_bytes(b"{}")
        self.src = src

    def register(self, **kw):
        return R.register_embed(ARM, "jina", self.src, "quality", variants=self.variants, **kw)

    def run_dir(self, arm, corpus="c1", files=None, empty=False):
        d = self.root / "runs" / arm / corpus
        d.mkdir(parents=True, exist_ok=True)
        files = {} if empty else (files if files is not None else self.pins)
        (d / "manifest.json").write_text(json.dumps({"modelFiles": [[f, s, 1] for f, s in sorted(files.items())]}))
        return self.root / "runs"


class Verify(Case):
    def test_a_registered_arm_verifies_and_its_pins_are_the_files_sha256(self):
        dest, pins = self.register()
        self.assertEqual(pins["model.onnx"], R.sha256_file(self.src / "model.onnx"))
        self.assertTrue(dest.name.endswith("@" + pins["model.onnx"][:16]))
        self.assertEqual(R.verify(variants=self.variants), [])
        self.assertEqual(R.verify([ARM], variants=self.variants), [])

    def test_a_tampered_file_is_refused(self):
        dest, _ = self.register()
        (dest / "tokenizer.json").write_bytes(b"{ }")
        bad = R.verify(variants=self.variants)
        self.assertEqual(len(bad), 1)
        self.assertIn("sha256 differs from its pin", bad[0])

    def test_a_missing_file_and_a_foreign_dir_name_are_refused(self):
        dest, _ = self.register()
        (dest / "tokenizer.json").unlink()
        self.assertTrue(any("missing" in b for b in R.verify(variants=self.variants)))
        moved = dest.with_name(ARM + "@0000000000000000")
        dest.rename(moved)
        self.variants.write_text(self.variants.read_text().replace(str(dest), str(moved)))
        bad = R.verify(variants=self.variants)
        self.assertTrue(any("does not carry model.onnx's sha16" in b for b in bad))

    def test_an_unpinned_name_is_reported(self):
        self.register()
        self.assertTrue(R.verify(["nope"], variants=self.variants))

    def test_registering_the_same_arm_again_is_a_no_op(self):
        self.register()
        before = self.variants.read_text()
        self.register()
        self.assertEqual(self.variants.read_text(), before)
        self.assertEqual(before.count(f'name = "{ARM}"'), 1)
        self.assertEqual(R.verify(variants=self.variants), [])

    def test_a_different_row_under_an_existing_name_is_refused(self):
        self.register()
        before = self.variants.read_text()
        (self.src / "model.onnx").write_bytes(b"other onnx bytes")
        with self.assertRaises(SystemExit) as cm:
            self.register()
        self.assertIn("different variant", str(cm.exception))
        self.assertEqual(self.variants.read_text(), before)

    def test_a_symlinked_variants_file_stays_a_symlink(self):
        real = self.root / "real-variants.toml"
        self.variants.rename(real)
        self.variants.symlink_to(real)
        self.register()
        self.assertTrue(self.variants.is_symlink())
        self.assertIn(f'name = "{ARM}"', real.read_text())
        self.assertEqual(R.verify(variants=self.variants), [])


class CheckRun(Case):
    def setUp(self):
        super().setUp()
        _, self.pins = self.register()

    def test_modelfiles_equal_to_the_pins_pass(self):
        runs = self.run_dir(ARM)
        self.assertEqual(R.check_run(runs, [ARM], self.variants), [])

    def test_modelfiles_that_differ_from_the_pins_are_refused(self):
        runs = self.run_dir(ARM, files=dict(self.pins, **{"model.onnx": "0" * 64}))
        bad = R.check_run(runs, [ARM], self.variants)
        self.assertEqual(len(bad), 1)
        self.assertIn("modelFiles model.onnx", bad[0])

    def test_a_run_that_recorded_no_model_files_is_refused(self):
        runs = self.run_dir(ARM, empty=True)
        self.assertEqual(len(R.check_run(runs, [ARM], self.variants)), len(R.EMBED_FILES))

    def test_no_manifests_is_refused(self):
        bad = R.check_run(self.root / "runs", [ARM], self.variants)
        self.assertTrue(any("no run manifests" in b for b in bad))

    def test_an_unpinned_arm_is_refused(self):
        self.assertTrue(any("no gm730 pins" in b for b in R.check_run(self.root / "runs", ["other"], self.variants)))


class WordsShuffled(Case):
    def setUp(self):
        super().setUp()
        self.dest, self.pins = self.register(words_shuffled=True)

    def test_the_arm_is_registered_with_its_reference(self):
        self.assertEqual(R.words_shuffled_of(self.variants), {WS: ARM})

    def test_its_empty_modelfiles_pass_when_the_reference_run_has_the_pins(self):
        self.run_dir(WS, empty=True)
        runs = self.run_dir(ARM)
        self.assertEqual(R.check_run(runs, [WS], self.variants), [])

    def test_a_reference_run_with_other_modelfiles_is_refused(self):
        self.run_dir(WS, empty=True)
        runs = self.run_dir(ARM, files=dict(self.pins, **{"model.onnx": "1" * 64}))
        bad = R.check_run(runs, [WS], self.variants)
        self.assertEqual(len(bad), 1)
        self.assertIn(f"runs/{ARM}/c1/manifest.json", bad[0])

    def test_a_corpus_without_a_reference_run_is_refused(self):
        self.run_dir(WS, corpus="c2", empty=True)
        runs = self.run_dir(ARM, corpus="c1")
        bad = R.check_run(runs, [WS], self.variants)
        self.assertTrue(any("no reference run" in b for b in bad))

    def test_a_changed_reference_file_is_refused_through_verify(self):
        self.run_dir(WS, empty=True)
        runs = self.run_dir(ARM)
        (self.dest / "model.onnx").write_bytes(b"changed")
        self.assertTrue(any("sha256 differs from its pin" in b for b in R.check_run(runs, [WS], self.variants)))

    def test_modelfiles_the_arm_records_must_be_the_pins_too(self):
        runs = self.run_dir(ARM)
        self.run_dir(WS, files=dict(self.pins, **{"tokenizer.json": "2" * 64}))
        bad = R.check_run(runs, [WS], self.variants)
        self.assertEqual(len(bad), 1)
        self.assertIn(f"runs/{WS}/c1/manifest.json", bad[0])

    def test_the_default_names_include_the_words_shuffled_arm(self):
        self.run_dir(WS, empty=True)
        runs = self.run_dir(ARM)
        self.assertEqual(R.check_run(runs, None, self.variants), [])
        (self.root / "runs" / WS / "c1" / "manifest.json").unlink()
        self.assertTrue(any("no run manifests" in b for b in R.check_run(runs, None, self.variants)))


if __name__ == "__main__":
    unittest.main()
