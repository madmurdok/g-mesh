"""Tests of GM-456 rendering and export (render.py, export.py; design note
docs/design/GM-456-training-data.md, section 5).

- Render parity with the shipped fixtures: `embed_text` against
  core/src/embedding/testdata/structured_text.json; `ce_text` against
  core/src/embedding/testdata/ce_text.json (`words` always; `minilm` needs
  $GM456_DATA/work/models/ms-marco-MiniLM-L6-v2/tokenizer.json and is
  skipped without it).
- `texts_for` refuses a texts folder of another renderId.
- `split_rows` sets the split by repo only.
- Unjudged negatives stop the export before anything is written.
- `export.main` writes queries.jsonl, sample.md, card.md and manifest.json
  only on a clean guard.json of this run (the guard subprocess is faked; the
  data dir is a temp $GM456_DATA).

Run: python3 eval/training/test_gm456_render_export.py
"""

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import common as C  # noqa: E402
import export as X  # noqa: E402
import render as R  # noqa: E402
import gm698_ce_text_fixture as F  # noqa: E402

STRUCTURED = C.REPO / "core/src/embedding/testdata/structured_text.json"
CE_TEXT = C.REPO / "core/src/embedding/testdata/ce_text.json"
# Fixture cases whose srcLines is a whole file with a non-zero start; a node
# row's srcLines is always its rebased span, so render never sees this shape.
WHOLE_FILE_CASES = {"start is honoured", "start past EOF"}
MINILM = C.work_dir() / R.TOKENIZER


# --- render parity (behaviour 2) ---------------------------------------------

class RenderParity(unittest.TestCase):
    def test_embed_text_equals_structured_text_fixture(self):
        cases = json.loads(STRUCTURED.read_text())
        self.assertGreaterEqual(len(cases), 20)
        for c in cases:
            with self.subTest(c["rule"]):
                self.assertEqual(R.embed_text({"docComment": c["doc"], "signature": c["signature"]}), c["expected"])

    def ce_cases(self):
        doc = json.loads(CE_TEXT.read_text())
        cases = [c for c in doc["cases"] if c["label"] not in WHOLE_FILE_CASES]
        self.assertEqual(len(doc["cases"]) - len(cases), len(WHOLE_FILE_CASES))
        self.assertGreaterEqual(len(cases), 100)
        return cases

    def test_ce_text_words_equals_ce_text_fixture(self):
        for c in self.ce_cases():
            with self.subTest(c["label"]):
                self.assertEqual(R.ce_text(dict(c["node"], srcLines=c["srcLines"]), F.words), c["words"])

    @unittest.skipUnless(MINILM.exists(), f"no MiniLM tokenizer at {MINILM}")
    def test_ce_text_minilm_equals_ce_text_fixture(self):
        tok = R.minilm()
        for c in self.ce_cases():
            with self.subTest(c["label"]):
                self.assertEqual(R.ce_text(dict(c["node"], srcLines=c["srcLines"]), tok), c["minilm"])

    def test_ce_row_rebases_a_span_node(self):
        node = dict(kind="Function", nativeKind="function", name="f", qualifiedName="m::f", filePath="src/m.rs",
                    startLine=40, endLine=43, language="rust", signature="fn f()", docComment=None)
        row = R.ce_row(node)
        self.assertEqual((row["startLine"], row["endLine"]), (0, 3))


# --- renderId (behaviour 5) ----------------------------------------------------

class RenderIdRefusal(unittest.TestCase):
    form = "embed-structured"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.vdir = Path(self.tmp.name)
        self.rid = R.render_id(self.form)

    def tearDown(self):
        self.tmp.cleanup()

    def folder(self, name, recorded):
        d = self.vdir / "texts" / name
        C.write_jsonl(d / "mini-redis.jsonl", [{"key": "k1", "text": "t1"}])
        C.write_json(d / "render.json", {"renderId": recorded})

    def test_current_render_is_read(self):
        self.folder(self.rid, self.rid)
        self.assertEqual(R.texts_for(self.vdir, self.form, "mini-redis"), {"k1": "t1"})

    def test_render_of_other_code_is_refused(self):
        stale = f"{self.form}@000000000000"
        self.assertNotEqual(stale, self.rid)
        self.folder(stale, stale)
        with self.assertRaises(SystemExit) as cm:
            R.texts_for(self.vdir, self.form, "mini-redis")
        self.assertIn("other code", str(cm.exception.code))

    def test_stale_render_beside_current_is_refused(self):
        self.folder(self.rid, self.rid)
        self.folder(f"{self.form}@000000000000", f"{self.form}@000000000000")
        with self.assertRaises(SystemExit):
            R.texts_for(self.vdir, self.form, "mini-redis")

    def test_folder_whose_record_disagrees_with_its_name_is_refused(self):
        self.folder(self.rid, f"{self.form}@ffffffffffff")
        with self.assertRaises(SystemExit):
            R.texts_for(self.vdir, self.form, "mini-redis")

    def test_missing_texts_are_refused(self):
        with self.assertRaises(SystemExit):
            R.texts_for(self.vdir, self.form, "mini-redis")

    def test_changed_renderer_file_changes_the_id(self):
        f = self.vdir / "renderer.py"
        f.write_text("a = 1\n")
        with mock.patch.object(R, "renderer_files", lambda form, work=None: [f]):
            before = R.render_id(self.form)
            f.write_text("a = 2\n")
            after = R.render_id(self.form)
        self.assertNotEqual(before, after)
        self.assertTrue(before.startswith(self.form + "@"))


# --- split (behaviour 3) -------------------------------------------------------

def row(i, repo, split=None, judged=True):
    neg = {"key": f"{repo}:n.rs#n{i}@1", "sources": ["embed"], "sameFile": False,
           "mining": {"bm25Rank": None, "embedRank": 3, "embedCos": 0.7}}
    if judged:
        neg["judge"] = {"verdict": "non-answer", "model": "m"}
    r = {"id": f"{repo}-{i:04d}-p1", "repo": repo, "commit": "c", "language": "rust", "shape": "phrase",
         "paraphraseOf": None, "text": f"query {i}", "overlap": True, "docEcho": 0.5,
         "target": f"{repo}:t.rs#t{i}@1", "positives": [f"{repo}:t.rs#t{i}@1"], "answersAlso": [],
         "negatives": [neg], "gen": {"model": "stub", "stub": True}}
    if split is not None:
        r["split"] = split
    return r


class Split(unittest.TestCase):
    repos = {"a": {"split": "train"}, "b": {"split": "dev"}}

    def test_split_follows_the_repo_not_the_row(self):
        rows = [row(1, "a", "dev"), row(2, "a", "train"), row(3, "b", "train"), row(4, "b")]
        out = X.split_rows(rows, self.repos)
        self.assertEqual([(r["repo"], r["split"]) for r in out],
                         [("a", "train"), ("a", "train"), ("b", "dev"), ("b", "dev")])

    def test_no_repo_on_both_sides(self):
        rows = [row(i, repo, split) for i, (repo, split) in
                enumerate([("a", "dev"), ("b", "train"), ("a", "train"), ("b", "dev"), ("a", None)])]
        out = X.split_rows(rows, self.repos)
        sides = {}
        for r in out:
            sides.setdefault(r["repo"], set()).add(r["split"])
        self.assertEqual(sides, {"a": {"train"}, "b": {"dev"}})

    def test_repo_without_split_stops(self):
        for repos in ({"a": {"split": "train"}}, {"a": {"split": "train"}, "c": {"split": "test"}}):
            with self.subTest(repos=repos), self.assertRaises(SystemExit):
                X.split_rows([row(1, "a"), row(2, "c")], repos)

    def test_shipped_corpora_have_disjoint_sides(self):
        repos = C.corpora()
        rows = [row(i, r) for i, r in enumerate(repos)]
        out = X.split_rows(rows, repos)
        train = {r["repo"] for r in out if r["split"] == "train"}
        dev = {r["repo"] for r in out if r["split"] == "dev"}
        self.assertTrue(train and dev)
        self.assertEqual(train & dev, set())


# --- export (behaviours 4 and 7) -------------------------------------------------

VERSION = "v9.9-test"
EXPORT_FILES = ("queries.jsonl", "sample.md", "card.md", "manifest.json")


class ExportBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        data = Path(self.tmp.name)
        env = mock.patch.dict(os.environ, {"GM456_DATA": str(data)})
        env.start()
        self.addCleanup(env.stop)
        self.vdir = data / VERSION
        # A stand-in tokenizer file: render_id only hashes it.
        tok = C.work_dir() / R.TOKENIZER
        tok.parent.mkdir(parents=True)
        tok.write_text("{}")
        self.rows = [row(i, "mini-redis") for i in range(1, 4)]
        nodes = []
        for r in self.rows:
            for k in [r["target"]] + [c["key"] for c in r["negatives"]]:
                fp, rest = k.split(":", 1)[1].split("#")
                nodes.append({"key": k, "filePath": fp, "qualifiedName": rest.split("@")[0], "kind": "Function"})
        C.write_jsonl(self.vdir / "nodes" / "mini-redis.jsonl", nodes)
        for form in R.FORMS:
            rid = R.render_id(form)
            C.write_jsonl(self.vdir / "texts" / rid / "mini-redis.jsonl", [{"key": n["key"], "text": "x"} for n in nodes])
            C.write_json(self.vdir / "texts" / rid / "render.json", {"renderId": rid})
        C.write_json(self.vdir / "targets.meta.json", {"seed": 456})
        self.guard_calls = []

    def tearDown(self):
        self.tmp.cleanup()

    def write_judged(self, rows):
        C.write_jsonl(self.vdir / "queries.judged.jsonl", rows)

    def plant_previous_export(self, dataset_sha="0" * 64):
        """A complete earlier export whose card and guard.json say clean."""
        for name in EXPORT_FILES:
            (self.vdir / name).write_text("previous export, Leak guard: clean\n")
        C.write_json(self.vdir / "guard.json", self.guard_report("clean", dataset_sha))
        (self.vdir / "leaks.jsonl").write_text("")

    @staticmethod
    def guard_report(result, dataset_sha):
        return {"result": result, "rows": 3, "datasetSha256": dataset_sha,
                "hitsPerCheck": {"repo-identity": 0, "exact-code": 0, "code-neardup": 0,
                                 "query-jaccard": 0, "query-embed": 0 if result == "clean" else 1},
                "rowsFlaggedShare": 0.0, "thresholds": {}, "pinsVerified": 0}

    def staging_sha(self):
        """sha256 of the staging file export.main will write for self.rows."""
        p = Path(self.tmp.name) / "expected.jsonl"
        C.write_jsonl(p, X.split_rows(self.rows, C.corpora()))
        return C.sha256_file(p)

    def fake_guard(self, rc, write=None):
        """export's subprocess.run: records the call, optionally writes
        guard.json beside the --dataset file, returns `rc`."""
        def run(cmd, *a, **kw):
            dataset = Path(cmd[cmd.index("--dataset") + 1])
            self.guard_calls.append(dataset)
            if write is not None:
                C.write_json(dataset.parent / "guard.json", write(dataset))
            return SimpleNamespace(returncode=rc)
        return run

    def export(self, guard):
        with mock.patch.object(X.subprocess, "run", guard), \
                mock.patch.object(sys, "argv", ["export.py", "--version", VERSION]):
            X.main()

    def assert_nothing_exported(self):
        for name in EXPORT_FILES:
            self.assertFalse((self.vdir / name).exists(), f"{name} survived a failed export")


class Unjudged(ExportBase):
    def test_unjudged_lists_negatives_without_a_verdict(self):
        rows = [row(1, "a"), row(2, "a", judged=False), row(3, "a")]
        rows[2]["negatives"][0]["judge"] = None
        self.assertEqual(X.unjudged(rows), [("a-0002-p1", "a:n.rs#n2@1"), ("a-0003-p1", "a:n.rs#n3@1")])
        self.assertEqual(X.unjudged([row(1, "a")]), [])

    def test_unjudged_negative_stops_the_export_before_the_guard(self):
        self.rows[1] = row(2, "mini-redis", judged=False)
        self.write_judged(self.rows)
        with self.assertRaises(SystemExit) as cm:
            self.export(self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d))))
        self.assertIn("unjudged", str(cm.exception.code))
        self.assertEqual(self.guard_calls, [])
        self.assertFalse((self.vdir / "queries.export.jsonl").exists())
        self.assert_nothing_exported()


class ExportGuard(ExportBase):
    def setUp(self):
        super().setUp()
        self.write_judged(self.rows)

    def test_clean_guard_of_this_run_exports(self):
        self.plant_previous_export()
        self.export(self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d))))
        for name in EXPORT_FILES:
            self.assertTrue((self.vdir / name).exists(), name)
        self.assertFalse((self.vdir / "queries.export.jsonl").exists())
        self.assertIn("Leak guard: clean", (self.vdir / "card.md").read_text())
        self.assertEqual(C.sha256_file(self.vdir / "queries.jsonl"), self.staging_sha())
        self.assertEqual(json.load(open(self.vdir / "manifest.json"))["guard"]["result"], "clean")

    def test_failing_guard_exports_nothing(self):
        self.plant_previous_export()
        with self.assertRaises(SystemExit):
            self.export(self.fake_guard(1, lambda d: self.guard_report("hits", C.sha256_file(d))))
        self.assertEqual(len(self.guard_calls), 1)
        self.assert_nothing_exported()

    def test_guard_stop_without_guard_json_exports_nothing(self):
        self.plant_previous_export()
        with self.assertRaises(SystemExit):
            self.export(self.fake_guard(1))
        self.assert_nothing_exported()

    def test_stale_clean_guard_json_cannot_stand_in_for_this_run(self):
        # The earlier guard.json is clean and even names this run's staging
        # sha256; the guard exits 0 but writes nothing.
        self.plant_previous_export(self.staging_sha())
        with self.assertRaises(SystemExit):
            self.export(self.fake_guard(0))
        self.assert_nothing_exported()

    def test_clean_guard_json_of_another_dataset_is_refused(self):
        self.plant_previous_export()
        with self.assertRaises(SystemExit):
            self.export(self.fake_guard(0, lambda d: self.guard_report("clean", "f" * 64)))
        self.assert_nothing_exported()

    def test_guard_json_with_hits_is_refused_even_at_rc_0(self):
        self.plant_previous_export()
        with self.assertRaises(SystemExit):
            self.export(self.fake_guard(0, lambda d: self.guard_report("hits", C.sha256_file(d))))
        self.assert_nothing_exported()


if __name__ == "__main__":
    unittest.main()
