"""Tests of the GM-436 vector and teacher cache (cache.py) and the C0b join
(dev_metric.c0b); design note docs/design/GM-436-laptop-training.md,
sections 3 and 5 item 5, section 8 behaviour 8. A tiny GM-456 version dir,
snapshot and Rust run (fixture_gm436.py) under a temp dir; numpy only.

Run: python -m unittest discover -s eval/training/gm436 -p 'test_gm436_*.py'
(the GM-436 venv's python: cache.py reads TOML).
"""

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cache as K  # noqa: E402
import data as D  # noqa: E402
import dev_metric as M  # noqa: E402
from fixture_gm436 import VARIANT, Fixture, key_of  # noqa: E402


class CacheCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.fx = Fixture(self.tmp.name, repos=("alpha", "beta"))
        env = mock.patch.dict(os.environ, self.fx.env())
        env.start()
        self.addCleanup(env.stop)
        self.addCleanup(self.tmp.cleanup)

    def import_(self, repo):
        nt, qt, _ = K.import_run(self.fx.mine, self.fx.vdir, VARIANT, repo)
        return K.VecTable(nt), K.VecTable(qt)

    def ranks(self, repo):
        nt, qt = K.tables_for(self.fx.mine, VARIANT, repo)
        nrows, nmat = nt.load()
        qrows, qmat = qt.load()
        queries = [q for q in D.load_queries(self.fx.vdir) if q["repo"] == repo]
        evals, missing = M.repo_eval(None, queries, qrows, qmat, nrows, nmat)
        self.assertEqual(missing, [])
        return {qid: r for qid, _, r in evals}


class OneSourceRunPerRepo(CacheCase):
    """A repo's rows come from one run: a new run of the repo replaces them."""

    def assert_one_run(self, table, repo, manifest_sha, rows):
        self.assertEqual(sum(r["repo"] == repo for r in table.index), rows)
        srcs = [s for s in table.manifest["sourceRuns"] if s["repo"] == repo]
        self.assertEqual([s["manifestSha256"] for s in srcs], [manifest_sha])
        on_disk = {p.name for p in table.dir.glob("chunk-*.npy")}
        self.assertEqual(on_disk, {c["file"] for c in table.manifest["chunks"]})

    def test_reimporting_a_repo_from_another_run_replaces_its_rows(self):
        self.import_("alpha")
        self.import_("beta")
        before = self.ranks("alpha")
        old = K.Run(self.fx.run_dir("alpha")).manifest_sha256
        self.fx.write_run("alpha", gmesh_version="0.0.1")
        new = K.Run(self.fx.run_dir("alpha")).manifest_sha256
        self.assertNotEqual(old, new)
        nt, qt = self.import_("alpha")
        n_queries = sum(q["repo"] == "alpha" for q in self.fx.queries)
        self.assert_one_run(nt, "alpha", new, self.fx.n)
        self.assert_one_run(qt, "alpha", new, n_queries)
        self.assertFalse(list(nt.dir.glob(f"chunk-{old[:12]}-*")))
        self.assertEqual(sum(r["repo"] == "beta" for r in nt.index), self.fx.n)
        self.assertEqual(self.ranks("alpha"), before)

    def test_a_table_holding_two_runs_of_a_repo_collapses_to_the_reimported_one(self):
        nt, qt = self.import_("alpha")
        saved = {t: Path(self.tmp.name) / f"saved-{i}" for i, t in enumerate((nt.dir, qt.dir))}
        for src, dst in saved.items():
            shutil.copytree(src, dst)
        self.fx.write_run("alpha", gmesh_version="0.0.1")
        new = K.Run(self.fx.run_dir("alpha")).manifest_sha256
        self.import_("alpha")
        # Plant the old run's rows back beside the new ones (two runs of one repo).
        for live, old in saved.items():
            om = json.loads((old / "manifest.json").read_text())
            lm = json.loads((live / "manifest.json").read_text())
            lm["sourceRuns"] += om["sourceRuns"]
            lm["chunks"] += om["chunks"]
            for c in om["chunks"]:
                shutil.copy(old / c["file"], live / c["file"])
            (live / "manifest.json").write_text(json.dumps(lm))
            with open(live / "index.jsonl", "a") as f:
                f.write((old / "index.jsonl").read_text())
        self.assertEqual(len(K.VecTable(nt.dir).manifest["sourceRuns"]), 2)
        nt, qt = self.import_("alpha")
        self.assert_one_run(nt, "alpha", new, self.fx.n)
        self.assert_one_run(qt, "alpha", new, sum(q["repo"] == "alpha" for q in self.fx.queries))


class SnapshotPin(CacheCase):
    def test_a_run_of_another_snapshot_is_refused_and_nothing_is_written(self):
        self.fx.write_run("alpha", snapshot_sha="1" * 64)
        with self.assertRaises(SystemExit) as cm:
            K.import_run(self.fx.mine, self.fx.vdir, VARIANT, "alpha")
        self.assertIn("is not the snapshot the run embedded", str(cm.exception))
        self.assertFalse((self.fx.gm436 / "cache").exists())


class C0b(CacheCase):
    """Behaviour 8: the key <-> vector join reproduces the run's rankings; a one-row offset fails."""

    def test_identity_ranking_from_the_cache_equals_the_runs_rankings(self):
        self.import_("alpha")
        self.import_("beta")
        res = M.c0b(self.fx.mine, VARIANT, ["alpha", "beta"])
        for repo in ("alpha", "beta"):
            self.assertEqual(res[repo]["mismatches"], 0, res)
            self.assertEqual(res[repo]["queries"], sum(q["repo"] == repo for q in self.fx.queries))

    def test_a_planted_one_row_offset_fails(self):
        self.import_("alpha")
        res = M.c0b(self.fx.mine, VARIANT, ["alpha"], plant_offset=True)
        self.assertGreater(res["alpha"]["mismatches"], 0, res)


class Teacher(CacheCase):
    def setUp(self):
        super().setUp()
        self.nt, self.qt = self.import_("alpha")
        self.queries = [q for q in D.load_queries(self.fx.vdir) if q["repo"] == "alpha"]
        self.out = Path(self.tmp.name) / "teacher"

    def build(self, queries=None):
        nt, qt = K.tables_for(self.fx.mine, VARIANT, "alpha")
        return K.build_teacher(nt, qt, queries or self.queries, "v0.1", "q: ", self.out, k=5)

    def test_a_query_whose_text_changed_gets_no_teacher_scores(self):
        edited = [dict(q, text=q["text"] + " (edited)") if i == 0 else q for i, q in enumerate(self.queries)]
        tdir = self.build(edited)
        ids = json.loads((tdir / "manifest.json").read_text())["queryIds"]
        self.assertNotIn(self.queries[0]["id"], ids)
        self.assertEqual(ids, [q["id"] for q in self.queries[1:]])

    def test_scores_built_before_a_reimport_are_refused_then_rebuilt(self):
        tdir = self.build()
        K.load_teacher(tdir)
        self.fx.write_run("alpha", order_seed=2)
        self.import_("alpha")
        with self.assertRaises(SystemExit) as cm:
            K.load_teacher(tdir)
        self.assertIn("node table changed", str(cm.exception))
        self.assertEqual(self.build(), tdir)
        scores = K.load_teacher(tdir)
        self.assertEqual(set(scores), {q["id"] for q in self.queries})
        index = {key_of("alpha", i): i for i in range(self.fx.n)}
        nv = self.fx.node_vec["alpha"]
        for q in self.queries:
            keys, s = scores[q["id"]]
            labelled = q["positives"] + q["negatives"] + q["answersAlso"]
            self.assertEqual(keys[:len(labelled)], labelled)
            qv = self.fx.query_vec[q["id"]]
            for k, got in zip(keys, s):
                c = nv[index[k]]
                want = float(c @ qv / np.linalg.norm(c) / np.linalg.norm(qv))
                self.assertAlmostEqual(got, want, delta=1e-5, msg=(q["id"], k))


if __name__ == "__main__":
    unittest.main()
