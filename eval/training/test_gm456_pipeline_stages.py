"""GM-456 pipeline stages added by 937c5dbe: the leak-guard `filter` stage,
`export.py --filtered` and its filter.guard.json consistency check, exit-3
propagation in run_pipeline.sh, and `licence_file` in setup_work.sh.

Nothing here calls a real `claude`, the network or the real eval data:
leak_guard's checks are mocked, run_pipeline.sh runs against stub step
scripts, setup_work.sh against a local git repo.

run with: uv run --with pytest --with tokenizers pytest eval/training/test_gm456_pipeline_stages.py
"""

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import claude_cli as CC  # noqa: E402
import common as C  # noqa: E402
import export as X  # noqa: E402
import leak_guard as LG  # noqa: E402
import test_gm456_claude_cli as CLI  # noqa: E402
import test_gm456_render_export as RE  # noqa: E402

VERSION = RE.VERSION
PIPELINE = HERE / "run_pipeline.sh"
SETUP = HERE / "setup_work.sh"


def two_neg_row(i, repo="mini-redis"):
    r = RE.row(i, repo)
    r["negatives"].append(dict(r["negatives"][0], key=f"{repo}:m.rs#m{i}@1"))
    return r


# --- leak_guard --filter and the export that follows -----------------------------

class FilterStage(RE.ExportBase):
    """ExportBase supplies the temp $GM456_DATA, nodes and texts."""

    def setUp(self):
        super().setUp()
        self.rows = [two_neg_row(i) for i in range(1, 5)]
        self.write_judged(self.rows)
        nodes = [{"key": c["key"], "filePath": c["key"].split(":", 1)[1].split("#")[0],
                  "qualifiedName": c["key"].split("#")[1].split("@")[0], "kind": "Function"}
                 for r in self.rows for c in r["negatives"] + [{"key": r["target"]}]]
        nodes += C.read_jsonl(self.vdir / "nodes" / "mini-redis.jsonl")
        C.write_jsonl(self.vdir / "nodes" / "mini-redis.jsonl", {n["key"]: n for n in nodes}.values())
        self.judged = self.vdir / "queries.judged.jsonl"
        r2, r3 = self.rows[1], self.rows[2]
        # row 2: a query hit, drops the row (and both its negatives with it);
        # row 3: a negative-code hit, drops only that one negative.
        self.hits = [{"check": "query-jaccard", "key": "q", "rows": [r2["id"]]},
                     {"check": "negative-code", "key": r3["negatives"][1]["key"], "rows": [],
                      "negativeRows": [r3["id"]]}]
        self.report = {"result": "hits", "rows": 4, "hitsPerCheck": {"query-jaccard": 1, "negative-code": 1},
                       "rowsFlaggedShare": 0.25, "nearestEvalCosine": {}, "nearestEvalJaccard": {}}

    def run_filter(self):
        argv = ["leak_guard.py", "--filter", "--dataset", str(self.judged), "--eval-work", str(self.vdir)]
        with mock.patch.object(LG, "run_guard", return_value=(self.hits, dict(self.report))), \
                mock.patch.object(sys, "argv", argv):
            LG.main()

    def export_filtered(self, guard):
        with mock.patch.object(X.subprocess, "run", guard), \
                mock.patch.object(sys, "argv", ["export.py", "--version", VERSION, "--filtered"]):
            X.main()

    def test_filter_drops_hit_rows_and_records_them_under_filter_names(self):
        (self.vdir / "guard.json").write_text("previous guard\n")
        (self.vdir / "leaks.jsonl").write_text("previous leaks\n")
        self.run_filter()
        kept = C.read_jsonl(self.vdir / "queries.judged.filtered.jsonl")
        self.assertEqual([r["id"] for r in kept], [self.rows[0]["id"], self.rows[2]["id"], self.rows[3]["id"]])
        self.assertEqual([c["key"] for c in kept[1]["negatives"]], [self.rows[2]["negatives"][0]["key"]])
        self.assertEqual(C.read_jsonl(self.vdir / "filter.leaks.jsonl"), self.hits)
        # --filter leaves the --check records alone.
        self.assertEqual((self.vdir / "guard.json").read_text(), "previous guard\n")
        self.assertEqual((self.vdir / "leaks.jsonl").read_text(), "previous leaks\n")
        rep = json.load(open(self.vdir / "filter.guard.json"))
        self.assertEqual(rep["datasetSha256"], C.sha256_file(self.judged))
        f = rep["filter"]
        self.assertEqual((f["rowsIn"], f["rowsKept"], f["rowsDropped"], f["hits"]), (4, 3, 1, 2))
        self.assertEqual(f["outputSha256"], C.sha256_file(self.vdir / "queries.judged.filtered.jsonl"))

    def test_negatives_dropped_counts_only_negatives_of_kept_rows(self):
        self.run_filter()
        # Row 2 is dropped with its two negatives; only row 3 lost a negative.
        self.assertEqual(json.load(open(self.vdir / "filter.guard.json"))["filter"]["negativesDropped"], 1)

    def test_export_of_the_filtered_rows_succeeds_and_keeps_the_filter_records(self):
        self.run_filter()
        self.export_filtered(self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d))))
        for name in RE.EXPORT_FILES:
            self.assertTrue((self.vdir / name).exists(), name)
        exported = (self.vdir / "queries.jsonl").read_text()
        self.assertNotIn(self.rows[1]["id"], exported)
        for i in (0, 2, 3):
            self.assertIn(self.rows[i]["id"], exported)
        self.assertEqual(len(self.guard_calls), 1)
        for name in ("filter.leaks.jsonl", "filter.guard.json", "queries.judged.filtered.jsonl"):
            self.assertTrue((self.vdir / name).exists(), f"export deleted {name}")
        manifest = json.load(open(self.vdir / "manifest.json"))
        self.assertIn("filter.leaks.jsonl", manifest["files"])
        self.assertIn("filter.guard.json", manifest["files"])
        self.assertEqual(manifest["filter"]["rowsDropped"], 1)
        self.assertEqual(manifest["filter"]["negativesDropped"], 1)
        self.assertEqual(manifest["filter"]["hitsPerCheck"], self.report["hitsPerCheck"])
        card = (self.vdir / "card.md").read_text()
        self.assertIn("Leak filter: dropped 1 of 4 rows and 1 negatives of kept rows", card)

    def test_export_without_filtered_says_filter_not_run(self):
        self.export(self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d))))
        self.assertIn("Leak filter: not run.", (self.vdir / "card.md").read_text())
        self.assertIsNone(json.load(open(self.vdir / "manifest.json"))["filter"])

    def test_export_check_is_still_the_last_gate_for_filtered_rows(self):
        self.run_filter()
        self.plant_previous_export()
        with self.assertRaises(SystemExit):
            self.export_filtered(self.fake_guard(1, lambda d: self.guard_report("hits", C.sha256_file(d))))
        self.assertEqual(len(self.guard_calls), 1)
        self.assert_nothing_exported()


class FilteredExportConsistency(RE.ExportBase):
    """A filter.guard.json that is not the filter run of the current judged
    and filtered files stops the export before it deletes anything."""

    def setUp(self):
        super().setUp()
        self.write_judged(self.rows)
        C.write_jsonl(self.vdir / "queries.judged.filtered.jsonl", self.rows[:2])
        self.rep = {"result": "hits", "datasetSha256": C.sha256_file(self.vdir / "queries.judged.jsonl"),
                    "hitsPerCheck": {"query-jaccard": 1},
                    "filter": {"outputSha256": C.sha256_file(self.vdir / "queries.judged.filtered.jsonl"),
                               "rowsIn": 3, "rowsKept": 2, "rowsDropped": 1, "negativesDropped": 0}}
        self.plant_previous_export()

    def put_record(self, rep):
        C.write_json(self.vdir / "filter.guard.json", rep)

    def run_filtered_export(self):
        guard = self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d)))
        with mock.patch.object(X.subprocess, "run", guard), \
                mock.patch.object(sys, "argv", ["export.py", "--version", VERSION, "--filtered"]):
            X.main()

    def assert_stops_untouched(self, needle):
        with self.assertRaises(SystemExit) as cm:
            self.run_filtered_export()
        self.assertIn(needle, str(cm.exception.code))
        self.assertEqual(self.guard_calls, [])
        for name in RE.EXPORT_FILES:
            self.assertEqual((self.vdir / name).read_text(), "previous export, Leak guard: clean\n", name)
        self.assertTrue((self.vdir / "guard.json").exists())
        self.assertTrue((self.vdir / "leaks.jsonl").exists())

    def test_matching_record_exports(self):
        self.put_record(self.rep)
        self.run_filtered_export()
        self.assertEqual(json.load(open(self.vdir / "manifest.json"))["filter"]["rowsDropped"], 1)

    def test_missing_record_stops(self):
        self.assert_stops_untouched("--filtered needs")

    def test_record_without_a_filter_block_stops(self):
        # A --check report is not a filter run.
        self.put_record({k: v for k, v in self.rep.items() if k != "filter"})
        self.assert_stops_untouched("--filtered needs")

    def test_missing_filtered_file_stops(self):
        self.put_record(self.rep)
        (self.vdir / "queries.judged.filtered.jsonl").unlink()
        self.assert_stops_untouched("--filtered needs")

    def test_judged_rows_changed_since_the_filter_ran_stops(self):
        self.put_record(self.rep)
        self.write_judged(self.rows[:2] + [RE.row(9, "mini-redis")])  # re-judged
        self.assert_stops_untouched("not the filter run")

    def test_filtered_file_changed_since_the_filter_ran_stops(self):
        self.put_record(self.rep)
        C.write_jsonl(self.vdir / "queries.judged.filtered.jsonl", self.rows[:1])
        self.assert_stops_untouched("not the filter run")

    def test_record_without_checksums_stops(self):
        rep = json.loads(json.dumps(self.rep))
        del rep["datasetSha256"]
        self.put_record(rep)
        self.assert_stops_untouched("not the filter run")


# --- run_pipeline.sh ---------------------------------------------------------------

STUB = """#!/usr/bin/env python3
import os, sys
name = os.path.basename(__file__)[:-3]
with open(os.path.join(os.environ["GM456_DATA"], "calls.log"), "a") as f:
    f.write(name + " " + " ".join(sys.argv[1:]) + "\\n")
print("filter: kept 1 of 2 rows" if name == "leak_guard" else name + ": ok")
sys.exit(int(os.environ.get("RC_" + name.upper(), "0")))
"""


class Pipeline(unittest.TestCase):
    """The real run_pipeline.sh, in a temp repo whose step scripts are stubs
    that log their arguments and exit with $RC_<NAME>."""

    def setUp(self):
        if shutil.which("taskpolicy") is None:
            self.skipTest("taskpolicy (macOS) not available")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = Path(self.tmp.name) / "repo"
        self.data = Path(self.tmp.name) / "data"
        tdir = self.cwd / "eval" / "training"
        tdir.mkdir(parents=True)
        self.data.mkdir()
        shutil.copy(PIPELINE, tdir / "run_pipeline.sh")
        for name in ("nodes", "sample", "render", "generate", "mine", "judge", "leak_guard", "export"):
            (tdir / f"{name}.py").write_text(STUB)
        self.vdir = self.data / VERSION

    def run_pipeline(self, *args, **rc):
        env = dict(os.environ, GM456_DATA=str(self.data), **{f"RC_{k.upper()}": str(v) for k, v in rc.items()})
        return subprocess.run(["bash", "eval/training/run_pipeline.sh", "--version", VERSION, *args], cwd=self.cwd,
                              env=env, capture_output=True, text=True, timeout=120)

    def calls(self):
        f = self.data / "calls.log"
        return f.read_text().splitlines() if f.exists() else []

    def summary(self):
        return (self.vdir / "summary.txt").read_text()

    def test_filter_runs_leak_guard_filter_then_export_filtered(self):
        p = self.run_pipeline("--from", "filter")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.calls(), [
            f"leak_guard --version {VERSION} --filter --dataset {self.vdir}/queries.judged.jsonl",
            f"export --version {VERSION} --filtered"])
        self.assertIn("filter: kept 1 of 2 rows", self.summary())
        self.assertIn(f"OK {VERSION}", self.summary())

    def test_from_export_skips_the_filter(self):
        p = self.run_pipeline("--from", "export")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.calls(), [f"export --version {VERSION} --filtered"])

    def test_unknown_from_exits_2_before_touching_anything(self):
        self.vdir.mkdir()
        (self.vdir / "summary.txt").write_text("earlier summary\n")
        p = self.run_pipeline("--from", "bogus")
        self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
        self.assertIn("--from: one of", p.stderr)
        self.assertEqual(self.summary(), "earlier summary\n")
        self.assertEqual(self.calls(), [])

    def test_from_naming_more_than_one_stage_or_none_exits_2(self):
        for value in ("judge filter", "", " export"):
            with self.subTest(value=value):
                p = self.run_pipeline("--from", value)
                self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
                self.assertIn("--from: one of", p.stderr)
                self.assertFalse((self.vdir / "summary.txt").exists())
                self.assertEqual(self.calls(), [])

    def test_step_exit_3_makes_the_pipeline_exit_3_with_a_stop_line(self):
        p = self.run_pipeline("--from", "filter", leak_guard=3)
        self.assertEqual(p.returncode, 3, p.stdout + p.stderr)
        self.assertIn("STOP: filter stopped, resumable", self.summary())
        self.assertNotIn("FAILED", self.summary())
        self.assertNotIn("OK", self.summary())
        self.assertEqual(len(self.calls()), 1)  # export did not run

    def test_exit_3_of_a_later_step_propagates_too(self):
        p = self.run_pipeline("--from", "filter", export=3)
        self.assertEqual(p.returncode, 3, p.stdout + p.stderr)
        self.assertIn("STOP: export stopped, resumable", self.summary())

    def test_other_failures_exit_1_with_the_code_in_the_failed_line(self):
        for code in (1, 2, 7):
            with self.subTest(code=code):
                p = self.run_pipeline("--from", "filter", leak_guard=code)
                self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
                self.assertIn(f"FAILED: filter (exit {code};", self.summary())
                self.assertNotIn("STOP:", self.summary())


# --- config STOPs stay exit 1 --------------------------------------------------------

class ConfigStopsAreNotResumable(CLI.FakeBase):
    def assert_config_stop(self, cm, needle):
        # sys.exit(<message>) is exit status 1; a resumable stop is the int 3.
        self.assertIsInstance(cm.exception.code, str)
        self.assertNotEqual(cm.exception.code, CC.STOP_EXIT)
        self.assertIn(needle, cm.exception.code)

    def test_unknown_retry_pack_is_a_config_stop(self):
        reqs = [CLI.mk_request("a-1", "u")]
        self.run_step(reqs)
        with self.assertRaises(SystemExit) as cm:
            self.run_step(reqs, retry=("nope",))
        self.assert_config_stop(cm, "unknown packs")

    def test_responses_of_another_prompt_are_a_config_stop(self):
        reqs = [CLI.mk_request("a-1", "u")]
        self.run_step(reqs, prompt_sha="p1")
        with self.assertRaises(SystemExit) as cm:
            self.run_step(reqs, prompt_sha="p2")
        self.assert_config_stop(cm, "another prompt")


# --- setup_work.sh licence lookup -------------------------------------------------------

def sha256(text):
    return hashlib.sha256(text.encode()).hexdigest()


class LicenceLookup(unittest.TestCase):
    """The real setup_work.sh against a local git repo and a stub
    make_snapshot.sh."""

    def setUp(self):
        for tool in ("git", "shasum"):
            if shutil.which(tool) is None:
                self.skipTest(f"{tool} not available")
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.cwd = self.root / "repo"
        self.data = self.root / "data"
        (self.cwd / "eval/training").mkdir(parents=True)
        (self.cwd / "eval/embedding").mkdir(parents=True)
        shutil.copy(SETUP, self.cwd / "eval/training/setup_work.sh")
        snap = self.cwd / "eval/embedding/make_snapshot.sh"
        snap.write_text("#!/usr/bin/env bash\n"
                        "printf '{\\n  \"embeddable\": 1\\n}\\n' > \"$GM456_DATA/work/$1.snapshot.json\"\n")
        snap.chmod(snap.stat().st_mode | stat.S_IXUSR)
        self.eval_work = self.root / "evalwork"
        (self.eval_work / "models/jina-v2-base-code-int8").mkdir(parents=True)
        self.src = self.root / "src"
        self.src.mkdir()

    def commit_source(self, files):
        for rel, text in files.items():
            p = self.src / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        git = ["git", "-C", str(self.src), "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"]
        subprocess.run([*git, "init", "-q"], check=True)
        subprocess.run([*git, "add", "."], check=True)
        subprocess.run([*git, "commit", "-q", "-m", "x"], check=True)
        return subprocess.run(["git", "-C", str(self.src), "rev-parse", "HEAD"], check=True, capture_output=True,
                              text=True).stdout.strip()

    def setup_repo(self, files, licence_file=None):
        rev = self.commit_source(files)
        extra = f'licence_file = "{licence_file}"\n' if licence_file else ""
        (self.cwd / "eval/training/corpora.toml").write_text(
            f'[[corpus]]\nid = "demo"\nurl = "file://{self.src}"\nrevision = "{rev}"\nref = "HEAD"\n'
            f'checkout = "work/corpora/demo"\nlicence = "MIT"\n{extra}split = "train"\n')
        env = dict(os.environ, GM456_DATA=str(self.data))
        return subprocess.run(["bash", "eval/training/setup_work.sh", "--bin", "x", "--eval-work",
                               str(self.eval_work), "demo"], cwd=self.cwd, env=env, capture_output=True,
                              text=True, timeout=120)

    def tsv(self):
        return (self.data / "work/licences/demo.tsv").read_text().splitlines()

    def test_licence_file_names_exactly_that_path_and_its_hash(self):
        p = self.setup_repo({"sub/LICENSE": "sub licence\n", "LICENSE-DECOY": "root decoy\n", "a.rs": "x\n"},
                            licence_file="sub/LICENSE")
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.tsv(), [f"sub/LICENSE\t{sha256('sub licence' + chr(10))}"])

    def test_missing_licence_file_exits_1(self):
        p = self.setup_repo({"LICENSE": "root\n"}, licence_file="sub/LICENSE")
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertIn("licence_file sub/LICENSE not found", p.stderr)

    def test_without_the_key_the_root_licences_are_used_by_basename(self):
        p = self.setup_repo({"LICENSE": "mit\n", "COPYING": "gpl\n", "sub/LICENSE": "ignored\n"})
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(sorted(self.tsv()),
                         sorted([f"LICENSE\t{sha256('mit' + chr(10))}", f"COPYING\t{sha256('gpl' + chr(10))}"]))

    def test_without_the_key_and_without_a_root_licence_exits_1(self):
        p = self.setup_repo({"sub/LICENSE": "not at the root\n"})
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertIn("no licence file", p.stderr)

    def test_axum_names_its_licence_in_the_shipped_corpora_toml(self):
        self.assertEqual(C.corpora()["axum"]["licence_file"], "axum/LICENSE")


if __name__ == "__main__":
    unittest.main()
