"""GM-456 S68: `nodes.frame` drops every node of a file whose longest line is
over MAX_LINE_CHARS (2000), counted under dropped["minified_file"].

Small temp checkouts only; no real data, claude or network.

run with: uv run --with pytest --with tokenizers pytest eval/training/test_gm456_nodes_minified.py
"""

import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import common as C  # noqa: E402
import generate as G  # noqa: E402
import judge as J  # noqa: E402
import claude_cli as CC  # noqa: E402
import nodes as N  # noqa: E402
import sample as S  # noqa: E402

META = {"revision": "abc123"}


def line_of(n):
    """A comment line of exactly n chars."""
    return "//" + "x" * (n - 2)


def node_row(path, name, start, end=None, doc="docs", sig="fn f()", qname=None):
    return {"id": f"id-{path}-{name}", "kind": "function", "nativeKind": "function_item", "name": name,
            "qualifiedName": qname or name, "filePath": path, "startLine": start,
            "endLine": start if end is None else end, "signature": sig, "docComment": doc,
            "language": "rust"}


class Base(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.co = Path(self._tmp.name) / "co"
        self.co.mkdir()

    def write(self, path, lines):
        p = self.co / path
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("\n".join(lines))

    def frame(self, rows, repo="r"):
        return N.frame(repo, META, rows, self.co)


class MinifiedFile(Base):
    def test_constant_is_2000(self):
        self.assertEqual(N.MAX_LINE_CHARS, 2000)

    def test_line_over_limit_drops_all_nodes_and_counts_them(self):
        self.write("m.rs", ["fn a() {}", line_of(2001), "fn b() {}"])
        out, dropped = self.frame([node_row("m.rs", "a", 0), node_row("m.rs", "b", 2)])
        self.assertEqual(out, [])
        self.assertEqual(dict(dropped), {"minified_file": 2})

    def test_line_of_exactly_2000_is_kept(self):
        self.write("k.rs", ["fn a() {}", line_of(2000), "fn b() {}"])
        out, dropped = self.frame([node_row("k.rs", "a", 0), node_row("k.rs", "b", 2)])
        self.assertEqual([n["name"] for n in out], ["a", "b"])
        self.assertEqual(dict(dropped), {})

    def test_boundary_2000_kept_2001_dropped_in_one_call(self):
        self.write("k.rs", [line_of(2000), "fn a() {}"])
        self.write("m.rs", [line_of(2001), "fn a() {}"])
        out, dropped = self.frame([node_row("k.rs", "a", 1), node_row("m.rs", "a", 1)])
        self.assertEqual([n["filePath"] for n in out], ["k.rs"])
        self.assertEqual(dict(dropped), {"minified_file": 1})

    def test_decision_is_per_file(self):
        self.write("m.rs", ["fn short() {}", line_of(2500)])
        self.write("ok.rs", ["fn fine() {}"])
        out, dropped = self.frame([node_row("m.rs", "short", 0), node_row("ok.rs", "fine", 0)])
        self.assertEqual([n["name"] for n in out], ["fine"])
        self.assertEqual(dict(dropped), {"minified_file": 1})

    def test_short_symbol_span_in_minified_file_is_dropped(self):
        # the span (line 0 only) is short; the file's long line is elsewhere
        self.write("m.rs", ["fn short() {}", "", line_of(5000)])
        out, dropped = self.frame([node_row("m.rs", "short", 0, 0)])
        self.assertEqual(out, [])
        self.assertEqual(dropped["minified_file"], 1)

    def test_empty_file_is_not_minified(self):
        self.write("e.rs", [])
        out, dropped = self.frame([node_row("e.rs", "a", 0)])
        self.assertEqual(len(out), 1)
        self.assertEqual(dict(dropped), {})


class OtherDrops(Base):
    def test_each_other_drop_is_counted_and_minified_is_not_double_counted(self):
        self.write("m.rs", ["fn a() {}", line_of(3000)])
        self.write("ok.rs", ["fn a() {}", "fn dup() {}"])
        self.write("tests/t.rs", ["fn a() {}", line_of(3000)])
        rows = [
            node_row("ok.rs", "kept", 0),
            node_row("ok.rs", "dup", 1),
            node_row("ok.rs", "dup", 1),                      # same key -> duplicate_key
            node_row("ok.rs", "blank", 0, doc="  ", sig=""),  # not embeddable
            node_row("tests/t.rs", "a", 0),                   # test path, even in a long-line file
            node_row("gone.rs", "a", 0),                      # no source file
            node_row("m.rs", "a", 0),                         # minified
            node_row("m.rs", "b", 1),                         # minified
            node_row("m.rs", "blank", 0, doc="", sig=""),     # not embeddable wins over minified
        ]
        out, dropped = self.frame(rows)
        self.assertEqual(sorted(n["name"] for n in out), ["dup", "kept"])
        self.assertEqual(dict(dropped), {"duplicate_key": 1, "not_embeddable": 2, "test_or_fixture": 1,
                                         "no_source_file": 1, "minified_file": 2})

    def test_missing_file_is_no_source_file_not_minified(self):
        out, dropped = self.frame([node_row("gone.rs", "a", 0)])
        self.assertEqual(out, [])
        self.assertEqual(dict(dropped), {"no_source_file": 1})


# --- sample: per-repo seed -------------------------------------------------------

def sample_node(repo, i):
    kind = ("function", "struct", "const")[i % 3]
    return {"key": f"{repo}:f.rs#s{i}@{i}", "repo": repo, "language": "rust", "kind": kind,
            "nativeKind": "x"}


class SamplePerRepo(unittest.TestCase):
    def run_sample(self, repos):
        with tempfile.TemporaryDirectory() as tmp:
            vdir = Path(tmp)
            (vdir / "nodes").mkdir()
            for repo, nodes in repos.items():
                C.write_jsonl(vdir / "nodes" / f"{repo}.jsonl", nodes)
            corpora = {r: {"language": "rust"} for r in repos}
            argv = ["sample.py", "--version", "v0.0-stub", "--count", "12"]
            with mock.patch.object(C, "version_dir", return_value=vdir), \
                    mock.patch.object(C, "corpora", return_value=corpora), \
                    mock.patch.object(sys, "argv", argv), mock.patch("builtins.print"):
                S.main()
            return C.read_jsonl(vdir / "targets.jsonl")

    def test_removing_one_repos_nodes_leaves_other_targets_identical(self):
        full = {r: [sample_node(r, i) for i in range(40)] for r in ("aa", "bb", "cc")}
        before = self.run_sample(full)
        without = {r: v for r, v in full.items() if r != "bb"}
        after = self.run_sample(without)
        for repo in ("aa", "cc"):
            self.assertEqual([x for x in before if x["repo"] == repo],
                             [x for x in after if x["repo"] == repo])
        self.assertEqual({x["repo"] for x in after}, {"aa", "cc"})

    def test_same_input_twice_gives_same_targets(self):
        full = {r: [sample_node(r, i) for i in range(40)] for r in ("aa", "bb")}
        self.assertEqual(self.run_sample(full), self.run_sample(full))


# --- request_sha of an unchanged repo --------------------------------------------

class RequestShaUnchanged(Base):
    """A repo with no long line: the rule must not alter any kept node, so the
    requests built from `frame` hash the same as requests built from the nodes
    written out by hand (what the pre-rule `frame` produced)."""

    def setUp(self):
        super().setUp()
        self.write("a.rs", ["/// doc a", "fn a() {", "    1", "}", "fn b() {}"])
        self.rows = [node_row("a.rs", "a", 1, 3, doc="doc a"), node_row("a.rs", "b", 4, 4, doc="doc b")]

    def hand_nodes(self):
        a = {"key": "r:a.rs#a@1", "filePath": "a.rs", "qualifiedName": "a", "parent": None,
             "docComment": "doc a", "srcLines": ["fn a() {", "    1", "}"]}
        b = {"key": "r:a.rs#b@4", "filePath": "a.rs", "qualifiedName": "b", "parent": None,
             "docComment": "doc b", "srcLines": ["fn b() {}"]}
        return {a["key"]: a, b["key"]: b}

    def gen_shas(self, nodes):
        targets = [{"n": 1, "repo": "r", "key": "r:a.rs#a@1"}, {"n": 2, "repo": "r", "key": "r:a.rs#b@4"}]
        return [CC.request_sha(q) for q in G.build_requests(targets, nodes, "PROMPT")]

    def judge_shas(self, nodes):
        rows = []
        for k, tkey in enumerate(nodes, 1):
            other = [x for x in nodes if x != tkey][0]
            rows.append({"id": f"r-{k:04d}-p1", "repo": "r", "target": tkey, "text": f"q{k}",
                         "negatives": [{"key": other}]})
        return [CC.request_sha(q) for q in J.build_requests(rows, nodes, "PROMPT")]

    def test_generate_and_judge_request_sha_equal_hand_built(self):
        out, dropped = self.frame(self.rows)
        self.assertEqual(dict(dropped), {})
        framed = {n["key"]: n for n in out}
        hand = self.hand_nodes()
        self.assertEqual(set(framed), set(hand))
        self.assertEqual(self.gen_shas(framed), self.gen_shas(hand))
        self.assertEqual(self.judge_shas(framed), self.judge_shas(hand))
        for k, h in hand.items():
            self.assertEqual(framed[k]["srcLines"], h["srcLines"])

    def test_minified_file_beside_it_does_not_change_kept_requests(self):
        self.write("m.rs", ["fn z() {}", line_of(4000)])
        out, dropped = self.frame(self.rows + [node_row("m.rs", "z", 0)])
        self.assertEqual(dict(dropped), {"minified_file": 1})
        framed = {n["key"]: n for n in out}
        self.assertEqual(self.gen_shas(framed), self.gen_shas(self.hand_nodes()))


if __name__ == "__main__":
    unittest.main()
