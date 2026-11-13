"""Tests of the GM-456 leak guard (leak_guard.py; design note
docs/design/GM-456-training-data.md, section 4).

Synthetic (no work dir, no model): eval-file pins, the snapshot sqlite pin,
the few-shot block, and the code checks (exact, containment) on made-up spans.

Real data (skipped with a reason when absent, e.g. on a fresh checkout): the
plant control. A copy of the v0.0-stub dataset gets one plant per leak kind,
and `run_guard` must name every plant, by the expected check, and nothing
else; the unplanted copy must come back clean. It needs the main checkout's
eval/embedding/work (six snapshots, checkouts and the jina int8 model),
$GM456_DATA/v0.0-stub and $GM456_DATA/work/qvec-cache (copied to a temp dir,
so the shared cache is not written).

Run: python3 eval/training/test_gm456_leak_guard.py
"""

import hashlib
import json
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import common as C  # noqa: E402
import leak_guard as G  # noqa: E402
from sample_targets import exclusion  # noqa: E402


def write_jsonl(path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def pins_of(root):
    root = Path(root)
    return {str(p.relative_to(root)): sha(p) for p in sorted(root.rglob("*.jsonl"))}


class FakeEncoder:
    """Stands in for the jina encoder: a fixed random unit vector per text, so
    distinct texts are near-orthogonal (cosine far below COSINE)."""

    def __init__(self, *_args, **_kwargs):
        import numpy as np
        self.np = np

    def embed(self, texts):
        np = self.np
        out = []
        for t in texts:
            seed = int(hashlib.sha256(t.encode()).hexdigest()[:8], 16)
            v = np.random.default_rng(seed).standard_normal(256).astype(np.float32)
            out.append(v / np.linalg.norm(v))
        return np.stack(out) if out else np.zeros((0, 256), np.float32)


def numpy_available():
    try:
        import numpy  # noqa: F401
        return True
    except ImportError:
        return False


# --- eval file pins (behaviour 6) -------------------------------------------

class EvalFilePins(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        write_jsonl(self.root / "queries" / "gin.jsonl", [{"id": "g-1", "text": "route a request to its handler"}])
        write_jsonl(self.root / "queries" / "mechanical" / "gin.jsonl",
                    [{"id": "m-1", "text": "Engine", "expected": [{"filePath": "gin.go", "qualifiedName": "Engine"}]}])
        write_jsonl(self.root / "targets" / "gin.jsonl", [{"n": 1, "filePath": "gin.go", "qualifiedName": "New"}])
        self.pins = pins_of(self.root)

    def tearDown(self):
        self.tmp.cleanup()

    def test_matching_pins_pass(self):
        self.assertEqual(G.verify_pins(self.root, self.pins), [])

    def test_changed_query_file_is_refused(self):
        p = self.root / "queries" / "gin.jsonl"
        p.write_text(p.read_text().replace("handler", "handlers"))
        errors = G.verify_pins(self.root, self.pins)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("queries/gin.jsonl: sha256", errors[0])

    def test_changed_target_file_is_refused(self):
        p = self.root / "targets" / "gin.jsonl"
        p.write_text(p.read_text() + "\n")
        errors = G.verify_pins(self.root, self.pins)
        self.assertTrue(any(e.startswith("targets/gin.jsonl: sha256") for e in errors), errors)

    def test_missing_pinned_file_is_refused(self):
        (self.root / "queries" / "mechanical" / "gin.jsonl").unlink()
        self.assertIn("queries/mechanical/gin.jsonl: missing", G.verify_pins(self.root, self.pins))

    def test_unpinned_eval_file_is_refused(self):
        write_jsonl(self.root / "confirm" / "queries" / "gin.jsonl", [{"id": "c-1", "text": "x"}])
        self.assertIn("confirm/queries/gin.jsonl: not pinned", G.verify_pins(self.root, self.pins))

    def test_run_guard_stops_before_any_check_on_a_changed_file(self):
        p = self.root / "queries" / "gin.jsonl"
        p.write_text(p.read_text().replace("route", "routes"))
        with mock.patch.object(G, "eval_queries", side_effect=AssertionError("read past a failed pin")):
            with self.assertRaises(SystemExit) as cm:
                G.run_guard([], {}, {}, self.root, self.root, "unused", "unused", pins=self.pins)
        self.assertIn("differ from their pins", str(cm.exception.code))


# --- eval snapshot sqlite pin (behaviour 6) ---------------------------------

class SnapshotSqlitePin(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.work = Path(self.tmp.name)
        checkout = self.work / "corpora" / "gin"
        checkout.mkdir(parents=True)
        (checkout / "gin.go").write_text("package gin\n\nfunc New() *Engine {\n\treturn &Engine{}\n}\n")
        env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@t", "PATH": "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"}
        for cmd in (["init", "-q"], ["add", "gin.go"], ["commit", "-q", "-m", "init"]):
            subprocess.run(["git", "-C", str(checkout), *cmd], check=True, env=env, capture_output=True)
        self.db = self.work / "gin.sqlite"
        conn = sqlite3.connect(self.db)
        conn.execute("CREATE TABLE nodes (filePath TEXT, qualifiedName TEXT, startLine INT, endLine INT)")
        conn.execute("INSERT INTO nodes VALUES ('gin.go', 'New', 2, 4)")
        conn.commit()
        conn.close()
        self.db_sha = sha(self.db)
        self.revision = G.git(checkout, "rev-parse", "HEAD")
        self.write_record(self.db_sha)
        self.keys = {"gin": {("gin.go", "New")}}

    def tearDown(self):
        self.tmp.cleanup()

    def write_record(self, db_sha):
        (self.work / "gin.snapshot.json").write_text(json.dumps({"revision": self.revision, "sha256": db_sha}))

    def test_pinned_snapshot_gives_spans(self):
        with mock.patch.dict(G.SQLITE_PINS, {"gin": self.db_sha}):
            spans = G.eval_spans(self.work, self.keys)
        self.assertEqual(spans, [("gin:gin.go#New@2", ["func New() *Engine {", "\treturn &Engine{}", "}"])])

    def test_sqlite_differing_from_its_pin_is_refused(self):
        with mock.patch.dict(G.SQLITE_PINS, {"gin": "0" * 64}):
            with self.assertRaises(SystemExit) as cm:
                G.eval_spans(self.work, self.keys)
        self.assertIn("STOP: eval snapshot", str(cm.exception.code))

    def test_sqlite_changed_after_pin_and_record_is_refused(self):
        conn = sqlite3.connect(self.db)
        conn.execute("INSERT INTO nodes VALUES ('gin.go', 'Other', 0, 0)")
        conn.commit()
        conn.close()
        self.assertNotEqual(sha(self.db), self.db_sha)
        with mock.patch.dict(G.SQLITE_PINS, {"gin": self.db_sha}):
            with self.assertRaises(SystemExit):
                G.eval_spans(self.work, self.keys)

    def test_snapshot_record_disagreeing_with_pin_is_refused(self):
        self.write_record("f" * 64)
        with mock.patch.dict(G.SQLITE_PINS, {"gin": self.db_sha}):
            with self.assertRaises(SystemExit):
                G.eval_spans(self.work, self.keys)


# --- a synthetic run_guard environment ----------------------------------------

EVAL_QUERY = "route an incoming request to its registered handler function"


@unittest.skipUnless(numpy_available(), "numpy does not import")
class FewshotBlock(unittest.TestCase):
    """run_guard on a synthetic eval root (one query file, no targets), a
    fake encoder and one clean row: only the prompt varies."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        t = Path(self.tmp.name)
        self.root = t / "eval"
        write_jsonl(self.root / "queries" / "gin.jsonl", [{"id": "g-1", "text": EVAL_QUERY}])
        self.pins = pins_of(self.root)
        self.toml = t / "corpora.toml"
        self.toml.write_text('[[corpus]]\nid = "gin"\nsource = "https://github.com/gin-gonic/gin.git"\n')
        self.work = t / "work"
        self.work.mkdir()
        self.prompt = t / "generate.md"
        self.rows = [{"id": "r-1", "repo": "train-a", "text": "count the bytes of a buffer",
                      "positives": [], "negatives": []}]
        self.repos = {"train-a": {"url": "https://example.org/train-a.git"}}

    def tearDown(self):
        self.tmp.cleanup()

    def run_guard(self, prompt_text):
        self.prompt.write_text(prompt_text)
        with mock.patch.object(G, "Encoder", FakeEncoder):
            return G.run_guard(self.rows, {}, self.repos, self.root, self.work, self.prompt, self.work / "cache",
                               pins=self.pins, eval_corpora_toml=self.toml)

    def test_prompt_without_fewshot_block_stops(self):
        with self.assertRaises(SystemExit) as cm:
            self.run_guard("# prompt\n\nno examples here\n")
        self.assertIn("fewshot", str(cm.exception.code))

    def test_prompt_with_empty_fewshot_block_stops(self):
        with self.assertRaises(SystemExit):
            self.run_guard("# prompt\n\n```fewshot\n\n```\n")

    def test_clean_fewshot_block_passes(self):
        hits, report = self.run_guard("```fewshot\nsum the sizes of open files\nlist pending timers\n```\n")
        self.assertEqual(hits, [])
        self.assertEqual(report["result"], "clean")
        self.assertEqual(report["fewshotTexts"], 2)

    def test_fewshot_text_near_an_eval_query_is_a_hit(self):
        hits, _ = self.run_guard(f"```fewshot\nsum the sizes of open files\n{EVAL_QUERY}\n```\n")
        self.assertEqual([(h["check"], h.get("prompt"), h["eval"]) for h in hits],
                         [("query-jaccard", EVAL_QUERY, "gm398:g-1")])


# --- code checks on synthetic spans ---------------------------------------------

def code_lines(prefix, n):
    """n distinct lines of >= 5 tokens each, unique to `prefix`."""
    return [f"let {prefix}value{i} = compute{prefix}item(alpha{i}, beta{i}, gamma{i});" for i in range(n)]


class CodeChecks(unittest.TestCase):
    def setUp(self):
        self.eval_span = code_lines("eval", 12)
        self.ev = [("gin:x.go#F@3", self.eval_span)]

    def test_exact_copy_is_exact_code(self):
        spaced = ["    " + l + "   " for l in self.eval_span]
        hits = G.code_hits({"k"}, {"k": {"srcLines": spaced}}, self.ev)
        self.assertEqual(hits, [("k", "exact-code", "gin:x.go#F@3", 1.0)])

    def test_eval_span_inside_a_larger_node_is_containment(self):
        wrapper = code_lines("traina", 40) + self.eval_span + code_lines("trainb", 40)
        ev_sh, tr_sh = G.shingles(self.eval_span), G.shingles(wrapper)
        jac = len(ev_sh & tr_sh) / len(ev_sh | tr_sh)
        self.assertLess(jac, G.CODE_JACCARD, "precondition: Jaccard alone must not fire")
        hits = G.code_hits({"k"}, {"k": {"srcLines": wrapper}}, self.ev)
        self.assertEqual([(k, c, l) for k, c, l, _ in hits], [("k", "code-neardup", "gin:x.go#F@3")])
        self.assertGreaterEqual(hits[0][3], G.CODE_JACCARD)

    def test_unrelated_code_is_clean(self):
        hits = G.code_hits({"k"}, {"k": {"srcLines": code_lines("other", 30)}}, self.ev)
        self.assertEqual(hits, [])


# --- the plant control on real data (behaviour 1) -------------------------------

STUB = C.data_root() / "v0.0-stub"
COSINE_PLANT = "enabling TCP keep-alive for pooled socket connections"
COSINE_PLANT_EVAL = "v2/new:req-e083"  # "enable TCP keepalive on pooled sockets"


def real_data_missing():
    try:
        import numpy  # noqa: F401
        import onnxruntime  # noqa: F401
        import tokenizers  # noqa: F401
    except ImportError as e:
        return f"python deps missing: {e}"
    if not (STUB / "queries.jsonl").exists() or not (STUB / "nodes" / "mini-redis.jsonl").exists():
        return f"no v0.0-stub dataset at {STUB}"
    if not (C.work_dir() / "qvec-cache").is_dir():
        return f"no query-vector cache at {C.work_dir() / 'qvec-cache'}"
    try:
        ew = G.default_eval_work()
    except Exception as e:  # noqa: BLE001
        return f"cannot locate the main checkout: {e}"
    if not (ew / G.MODEL_DIR / "model.onnx").exists():
        return f"no jina int8 model under {ew}"
    for c in G.EVAL_CORPORA:
        if not (ew / f"{c}.sqlite").exists() or not (ew / "corpora" / c / ".git").exists():
            return f"no eval snapshot or checkout of {c} under {ew}"
    return None


SKIP_REAL = real_data_missing()


@unittest.skipIf(SKIP_REAL, SKIP_REAL or "")
class RealPlantControl(unittest.TestCase):
    """Eight plants on a copy of v0.0-stub: each must be named by the check
    that catches it, no clean row may be named, and the unplanted copy is
    clean (design note section 4, "Control (S4/S5)", plus GM-456/S15)."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cache = Path(cls.tmp.name) / "qvec-cache"
        shutil.copytree(C.work_dir() / "qvec-cache", cache)
        cls.eval_work = G.default_eval_work()
        rows = C.read_jsonl(STUB / "queries.jsonl")
        nodes = {n["key"]: n for n in C.load_nodes(STUB, "mini-redis")}
        repos = dict(C.corpora())
        ev = G.eval_queries(C.EVAL)
        cls.clean_ids = {r["id"] for r in rows}

        def eval_span(corpus, key):
            spans = G.eval_spans(cls.eval_work, {corpus: {key}})
            assert spans, f"no eval span for {corpus} {key}"
            return spans[0]

        # Authored ripgrep keys, and ripgrep mechanical keys outside them.
        authored = set()
        for study in G.STUDIES:
            d = C.EVAL / study if study else C.EVAL
            authored |= exclusion(str(d), "ripgrep")[0]
        mech = sorted({(e["filePath"], e["qualifiedName"])
                       for q in C.read_jsonl(C.EVAL / "queries" / "mechanical" / "ripgrep.jsonl")
                       for e in q.get("expected", [])} - authored)
        cls.mech_key = mech[0]
        authored_sorted = sorted(authored)

        def long_span(keys, skip=()):
            for k in keys:
                if k in skip:
                    continue
                spans = G.eval_spans(cls.eval_work, {"ripgrep": {k}})
                if spans and G.shingles(spans[0][1]) and len(G.terms("\n".join(spans[0][1]))) >= 60:
                    return k, spans[0]
            raise AssertionError("no eval span long enough")

        target_key, (cls.target_label, target_span) = long_span(authored_sorted)
        _, (cls.contain_label, contain_span) = long_span(authored_sorted, skip={target_key})
        cls.mech_label, mech_span = eval_span("ripgrep", cls.mech_key)

        # A wrapper of clean mini-redis code around the eval span: containment
        # 1.0, Jaccard below CODE_JACCARD.
        big = sorted(nodes.values(), key=lambda n: -len(n["srcLines"]))
        wrapper = big[0]["srcLines"] + contain_span + big[1]["srcLines"]
        ev_sh, tr_sh = G.shingles(contain_span), G.shingles(wrapper)
        cls.contain_jaccard = len(ev_sh & tr_sh) / len(ev_sh | tr_sh)

        gm398 = next((i, t) for i, t in ev if i.startswith("gm398:") and len(C.words(t)) >= 5)
        confirm = next((i, t) for i, t in ev if i.startswith("confirm:") and len(t.split()) >= 8
                       and len(C.words(t)) == len(t.split()))
        w = confirm[1].split()
        confirm_text = " ".join(["zqxwv", "vwxqz"] + w[2:])
        cls.confirm_jaccard = C.jaccard(C.words(confirm_text), C.words(confirm[1]))
        others = {t for i, t in ev if not i.startswith("v2/new:")}
        v2new = next((i, t) for i, t in ev if i.startswith("v2/new:") and t not in others)

        template = rows[0]

        def plant(pid, **kw):
            return dict(template, id=pid, **kw)

        def node(key, lines):
            nodes[key] = dict(nodes[template["target"]], key=key, srcLines=lines)
            return key

        planted = [
            plant("plant-gm398", text=gm398[1]),
            plant("plant-confirm", text=confirm_text),
            plant("plant-v2new", text=v2new[1]),
            plant("plant-cosine", text=COSINE_PLANT),
            plant("plant-target", positives=[node("mini-redis:plant.rs#target@1", target_span)]),
            plant("plant-contain", positives=[node("mini-redis:plant.rs#contain@1", wrapper)]),
            plant("plant-mechanical", positives=[node("mini-redis:plant.rs#mech@1", mech_span)]),
            plant("plant-fork", repo="gin-fork"),
        ]
        repos["gin-fork"] = {"id": "gin-fork", "url": "https://github.com/gin-gonic/gin.git", "split": "train"}
        cls.gm398, cls.confirm, cls.v2new = gm398, confirm, v2new

        prompt = HERE / "prompts" / "generate.md"
        cls.hits, cls.report = G.run_guard(rows + planted, nodes, repos, C.EVAL, cls.eval_work, prompt, cache)
        cls.clean_hits, cls.clean_report = G.run_guard(rows, nodes, repos, C.EVAL, cls.eval_work, prompt, cache)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def checks_naming(self, pid):
        return sorted(h["check"] for h in self.hits if pid in h["rows"])

    def hit(self, pid, check):
        found = [h for h in self.hits if pid in h["rows"] and h["check"] == check]
        self.assertEqual(len(found), 1, f"{pid}: {self.hits}")
        return found[0]

    def test_unplanted_copy_is_clean(self):
        self.assertEqual(self.clean_hits, [])
        self.assertEqual(self.clean_report["result"], "clean")

    def test_no_clean_row_is_named(self):
        named = {i for h in self.hits for i in h["rows"]} | {i for h in self.hits for i in h.get("negativeRows", ())}
        self.assertEqual(named & self.clean_ids, set())
        self.assertEqual(self.report["result"], "hits")

    def test_gm398_query_verbatim(self):
        h = self.hit("plant-gm398", "query-jaccard")
        self.assertEqual(h["score"], 1.0)

    def test_confirm_query_two_words_changed(self):
        self.assertGreaterEqual(self.confirm_jaccard, G.QUERY_JACCARD)
        self.assertLess(self.confirm_jaccard, 1.0)
        h = self.hit("plant-confirm", "query-jaccard")
        self.assertEqual(h["eval"], self.confirm[0])

    def test_gm460_v2_new_query(self):
        h = self.hit("plant-v2new", "query-jaccard")
        self.assertEqual(h["score"], 1.0)
        self.assertTrue(h["eval"].startswith("v2/new:"), h)

    def test_paraphrase_clearing_cosine_fires_query_embed_alone(self):
        self.assertEqual(self.checks_naming("plant-cosine"), ["query-embed"])
        h = self.hit("plant-cosine", "query-embed")
        self.assertEqual(h["eval"], COSINE_PLANT_EVAL)
        self.assertGreaterEqual(h["score"], G.COSINE)

    def test_eval_target_as_positive(self):
        h = self.hit("plant-target", "exact-code")
        self.assertEqual(h["eval"], self.target_label)

    def test_eval_span_inside_a_larger_node(self):
        self.assertLess(self.contain_jaccard, G.CODE_JACCARD, "precondition: Jaccard alone must not fire")
        h = self.hit("plant-contain", "code-neardup")
        self.assertEqual(h["eval"], self.contain_label)

    def test_mechanical_expected_symbol(self):
        h = self.hit("plant-mechanical", "exact-code")
        fp, qn = self.mech_key
        self.assertTrue(h["eval"].startswith(f"ripgrep:{fp}#{qn}@"), h)

    def test_fork_url(self):
        self.assertEqual(self.checks_naming("plant-fork"), ["repo-identity"])

    def test_every_plant_named(self):
        named = {i for h in self.hits for i in h["rows"]}
        self.assertEqual(named, {"plant-gm398", "plant-confirm", "plant-v2new", "plant-cosine", "plant-target",
                                 "plant-contain", "plant-mechanical", "plant-fork"})


if __name__ == "__main__":
    unittest.main()
