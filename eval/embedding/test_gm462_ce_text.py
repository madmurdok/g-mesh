"""Unit tests for gm462_ce_text.py (GM-462 C0) and the pure helpers of
gm462_ce_enriched.py (verdict, buckets, C4 targets, GM-398 ids).

The tokenizer is a stand-in that counts whitespace words, the rows are
synthetic and the C4 docs are temp files, so the tests need no work dir or
model; numpy must import (user site).

Run: python3 eval/embedding/test_gm462_ce_text.py
"""

import json
import re
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import gm462_ce_text as T  # noqa: E402
import gm462_ce_enriched as E  # noqa: E402


def tok(s):
    return len(s.split())


class SplitWords(unittest.TestCase):
    def test_spec_cases(self):
        self.assertEqual(T.split_words("HTTPServer"), "http server")
        self.assertEqual(T.split_words("safeInt8"), "safe int 8")
        self.assertEqual(T.split_words("send_quit"), "send quit")
        self.assertEqual(T.split_words("parseURL2Fast"), "parse url 2 fast")

    def test_path_and_empty(self):
        self.assertEqual(T.split_words("src/walk/mod.rs"), "src walk mod rs")
        self.assertEqual(T.split_words(""), "")
        self.assertEqual(T.split_words(None), "")


class Enclosing(unittest.TestCase):
    def test_go_method(self):
        self.assertEqual(T.enclosing("Method", "method", "Next", "Context.Next"), "method context")

    def test_rust_trait_impl_method(self):
        self.assertEqual(T.enclosing("Method", "method", "send_quit", "walk::Worker::send_quit"),
                         "method walk worker")

    def test_python_method(self):
        self.assertEqual(T.enclosing("Method", None, "send", "requests.sessions.Session.send"),
                         "method requests sessions session")

    def test_ts_constructor(self):
        self.assertEqual(T.enclosing("Method", "constructor", "constructor", "TaskConflictError#constructor"),
                         "constructor task conflict error")

    def test_top_level_function_is_kind_only(self):
        self.assertEqual(T.enclosing("Function", "function", "requireTask", "requireTask"), "function")

    def test_qualified_name_not_ending_in_name_is_kept_whole(self):
        self.assertEqual(T.enclosing("Type", "struct", "Other", "pkg::Thing"), "struct pkg thing")


class Identifiers(unittest.TestCase):
    def test_keywords_name_and_repeats_dropped_in_source_order(self):
        sig = "pub fn send_quit(&self, tx: Sender<Msg>, rx: Sender<Msg>) -> Result<Self>"
        self.assertEqual(T.identifiers("send_quit", sig, tok), "send quit tx sender msg rx result")

    def test_no_signature_gives_name_words(self):
        self.assertEqual(T.identifiers("safeInt8", None, tok), "safe int 8")

    def test_cap_at_48_tokens(self):
        sig = "def f(" + ", ".join(f"p{chr(97 + i // 26)}{chr(97 + i % 26)}" for i in range(60)) + ")"
        out = T.identifiers("f", sig, tok).split()
        self.assertEqual(T.I_CAP, 48)
        self.assertEqual(len(out), 48)
        self.assertEqual(out[:3], ["f", "paa", "pab"])

    def test_cap_on_word_boundary(self):
        # Each word costs 2 tokens: with cap 5 only two whole words fit.
        self.assertEqual(T.identifiers("a", "def a(b, c, d)", lambda s: 2, cap=5), "a b")


class BodyLines(unittest.TestCase):
    def test_python_one_line_docstring_skipped(self):
        lines = ["def f():", '    """Doc."""', "    return 1"]
        self.assertEqual(T.body_lines(lines, 0, 2, "python"), ["return 1"])

    def test_python_multi_line_docstring_skipped(self):
        lines = ["def f():", '    """Doc', "", "    more.", '    """', "    x = 1", "    return x"]
        self.assertEqual(T.body_lines(lines, 0, 6, "python"), ["x = 1", "return x"])

    def test_python_raw_docstring_skipped(self):
        lines = ["def f():", '    r"""Doc \\d', '    """', "    return 1"]
        self.assertEqual(T.body_lines(lines, 0, 3, "python"), ["return 1"])

    def test_no_docstring_keeps_first_line(self):
        lines = ["def f():", "    x = '''not a docstring'''", "    return x"]
        self.assertEqual(T.body_lines(lines, 0, 2, "python"), ["x = '''not a docstring'''", "return x"])

    def test_punctuation_lines_dropped_whitespace_collapsed_200_char_cut(self):
        long = "x" * 250
        lines = ["fn f() {", "    let   a  =  1;", "    });", "", f"    {long}", "}"]
        out = T.body_lines(lines, 0, 5, "rust")
        self.assertEqual(out, ["let a = 1;", "x" * 200])

    def test_only_lines_up_to_end(self):
        lines = ["fn f() {", "    a();", "    b();", "}", "fn g() {", "    c();", "}"]
        self.assertEqual(T.body_lines(lines, 0, 3, "rust"), ["a();", "b();"])


class SignatureEnd(unittest.TestCase):
    def test_python_decorators_and_multi_line_header(self):
        lines = ["@decorator", "@other(x=1)", "def f(a,", "      b) -> int:", '    """Doc."""', "    return a"]
        self.assertEqual(T.signature_end(lines, 0, 5, "python"), 3)
        self.assertEqual(T.body_lines(lines, 0, 5, "python"), ["return a"])

    def test_rust_multi_line_parameters(self):
        lines = ["pub fn f(", "    a: u8,", ") -> u8 {", "    a + 1", "}"]
        self.assertEqual(T.signature_end(lines, 0, 4, "rust"), 2)
        self.assertEqual(T.body_lines(lines, 0, 4, "rust"), ["a + 1"])

    def test_rust_lifetime_is_not_a_string(self):
        lines = ["fn f<'a>(x: &'a str) -> &'a str {", "    x", "}"]
        self.assertEqual(T.signature_end(lines, 0, 2, "rust"), 0)

    def test_ts_arrow_ends_the_signature(self):
        lines = ["export const f = (a: number,", "  b: number) =>", "  a + b;"]
        self.assertEqual(T.signature_end(lines, 0, 2, "typescript"), 1)
        self.assertEqual(T.body_lines(lines, 0, 2, "typescript"), ["a + b;"])

    def test_brace_inside_parameters_or_string_does_not_end_it(self):
        lines = ['func f(opts struct{ A int }, s string) {', "    return", "}"]
        self.assertEqual(T.signature_end(lines, 0, 2, "go"), 0)
        lines = ['fn f(s: &str = "{", t: u8,', "  u: u8) {", "  g()", "}"]
        self.assertEqual(T.signature_end(lines, 0, 3, "rust"), 1)

    def test_no_signature_line_starts_after_start(self):
        lines = ["type X =", "  | A", "  | B"]
        self.assertEqual(T.signature_end(lines, 0, 2, "typescript"), 0)
        self.assertEqual(T.body_lines(lines, 0, 2, "typescript"), ["| A", "| B"])

    def test_start_is_honoured(self):
        lines = ["import x", "", "def f():", "    return 1"]
        self.assertEqual(T.signature_end(lines, 2, 3, "python"), 2)


class Body(unittest.TestCase):
    LINES = ["fn f() {", "    a b c", "    d e f g", "    h", "    i j", "}"]

    def test_default_budget_is_96(self):
        self.assertEqual(T.BODY_BUDGET, 96)

    def test_stops_at_first_line_that_does_not_fit(self):
        # Line costs 3, 4, 1, 2. At budget 6 "h" alone would still fit after
        # "a b c", but the field stops at "d e f g".
        self.assertEqual(T.body(self.LINES, 0, 5, "Function", "rust", tok, budget=8), "a b c\nd e f g\nh")
        self.assertEqual(T.body(self.LINES, 0, 5, "Function", "rust", tok, budget=6), "a b c")

    def test_exact_budget_fits(self):
        self.assertEqual(T.body(self.LINES, 0, 5, "Type", "rust", tok, budget=7), "a b c\nd e f g")

    def test_no_body_for_other_kinds(self):
        for kind in ("File", "Module", "Variable", "Method"):
            with self.subTest(kind=kind):
                self.assertEqual(T.body(self.LINES, 0, 5, kind, "rust", tok), "")

    def test_no_source_lines(self):
        self.assertEqual(T.body(None, 0, 5, "Function", "rust", tok), "")
        self.assertEqual(T.body([], 0, 5, "Function", "rust", tok), "")


ROW = {
    "kind": "Function", "nativeKind": "function", "name": "send_quit", "qualifiedName": "walk::send_quit",
    "filePath": "crates/ignore/src/walk.rs", "startLine": 0, "endLine": 3,
    "signature": "pub fn send_quit(tx: Sender) {", "docComment": "Sends the quit signal.", "language": "rust",
}
SRC = ["pub fn send_quit(tx: Sender) {", "    tx.send(Quit);", "    log()", "}"]
FIELDS = {
    "E": "function walk",
    "I": "send quit tx sender",
    "P": "crates ignore src walk rs",
    "base": "Sends the quit signal.\n\npub fn send_quit(tx: Sender) {",
    "B": "tx.send(Quit);\nlog()",
}


class Build(unittest.TestCase):
    def test_all_on_order_e_i_p_base_b(self):
        self.assertEqual(T.build(ROW, SRC, frozenset("EIPB"), tok),
                         "\n".join(FIELDS[k] for k in ("E", "I", "P", "base", "B")))

    def test_each_switch_off_omits_exactly_its_field(self):
        for off in "EIPB":
            with self.subTest(off=off):
                keep = [k for k in ("E", "I", "P", "base", "B") if k != off]
                self.assertEqual(T.build(ROW, SRC, frozenset("EIPB") - {off}, tok),
                                 "\n".join(FIELDS[k] for k in keep))

    def test_all_off_is_base_byte_for_byte(self):
        cases = {
            "documented": ("Sends it.", "fn f()", "Sends it.\n\nfn f()"),
            "signature-only": (None, "fn f()", "fn f()"),
            "doc-only": ("Sends it.", None, "Sends it."),
        }
        for label, (doc, sig, want) in cases.items():
            with self.subTest(label):
                row = dict(ROW, docComment=doc, signature=sig)
                self.assertEqual(T.build(row, SRC, frozenset(), tok), want)
                self.assertEqual(T.build(row, SRC, frozenset(), tok), T.base_text(row))

    def test_empty_field_omitted_with_its_newline(self):
        row = dict(ROW, filePath="", kind="Variable")
        self.assertEqual(T.build(row, SRC, frozenset("EIPB"), tok),
                         "\n".join([FIELDS["E"], FIELDS["I"], FIELDS["base"]]))
        self.assertNotIn("\n\n\n", T.build(row, None, frozenset("EIPB"), tok))

    def test_body_budget_passed_through(self):
        self.assertEqual(T.build(ROW, SRC, frozenset("B"), tok, body_budget=0), FIELDS["base"])
        self.assertEqual(T.build(ROW, SRC, frozenset("B"), tok, body_budget=1),
                         FIELDS["base"] + "\ntx.send(Quit);")

    def test_unknown_switch_rejected(self):
        with self.assertRaises(ValueError):
            T.build(ROW, SRC, frozenset("EX"), tok)


class Verdict(unittest.TestCase):
    # (point, lo, hi)
    def test_helps_via_mrr(self):
        self.assertEqual(E.verdict((0.0, -0.02, 0.02), (0.01, 0.0, 0.03), True), "helps")

    def test_helps_via_r10(self):
        self.assertEqual(E.verdict((0.02, 0.0, 0.04), (0.0, -0.01, 0.01), True), "helps")

    def test_d9_fail_blocks_helps(self):
        self.assertEqual(E.verdict((0.02, 0.01, 0.04), (0.02, 0.01, 0.03), False), "no effect")

    def test_other_point_negative_blocks_helps(self):
        self.assertEqual(E.verdict((-0.01, -0.03, 0.01), (0.02, 0.01, 0.03), True), "no effect")

    def test_lower_bound_below_zero_is_not_helps(self):
        self.assertEqual(E.verdict((0.02, -0.01, 0.04), (0.02, -0.001, 0.03), True), "no effect")

    def test_zero_point_is_not_helps(self):
        self.assertEqual(E.verdict((0.0, 0.0, 0.0), (0.0, 0.0, 0.0), True), "no effect")

    def test_hurts_when_either_upper_bound_below_zero(self):
        self.assertEqual(E.verdict((-0.03, -0.05, -0.01), (0.0, -0.01, 0.01), True), "hurts")
        self.assertEqual(E.verdict((0.0, -0.01, 0.01), (-0.03, -0.05, -0.001), False), "hurts")

    def test_negative_point_with_upper_bound_at_zero_is_no_effect(self):
        self.assertEqual(E.verdict((-0.02, -0.05, 0.0), (-0.01, -0.03, 0.0), True), "no effect")


class Buckets(unittest.TestCase):
    def test_labels(self):
        c = SimpleNamespace(trimmed_doc={"a": True, "b": True, "c": False},
                            expected={"q1": ["a", "b"], "q2": ["c", "d"], "q3": ["a", "c"]})
        self.assertEqual(E.bucket_of(c, "q1"), "documented")
        self.assertEqual(E.bucket_of(c, "q2"), "signature-only")
        self.assertEqual(E.bucket_of(c, "q3"), "mixed")


class C4Target(unittest.TestCase):
    LABEL = "F4s K=30 beta=80 (FIT-tuned), CE on structured text"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def doc(self, text):
        p = self.dir / "doc.md"
        p.write_text(text)
        return p

    def test_current_set_label_inside_section(self):
        p = self.doc("## Other\n\n"
                     f"| {self.LABEL} | 0.1 |\n\n"
                     "## GM-464 S1: F4 K=30 on the structured text\n\n"
                     "| row | r@10 |\n"
                     f"| {self.LABEL} | 0.676 |\n"
                     f"| {self.LABEL} | vs int8 +3.7 |\n"
                     "| fp32 | 0.6 |\n\n"
                     "## Later\n\n"
                     f"| {self.LABEL} | 0.9 |\n")
        lab, lines = E.c4_target(p, "## GM-464 S1: F4 K=30 on the structured text", self.LABEL)
        self.assertEqual(lab, self.LABEL)
        self.assertEqual(lines, [f"| {self.LABEL} | 0.676 |", f"| {self.LABEL} | vs int8 +3.7 |"])

    def test_v2_pattern_matches_with_or_without_fit_tuned(self):
        for label in ("F4s K=30 beta=80, CE on structured text", self.LABEL):
            with self.subTest(label=label):
                p = self.doc(f"| row | r@10 |\n| {label} | 0.7 |\n| F4s K=50 beta=40, CE on full text | 0.6 |\n")
                self.assertEqual(E.c4_target(p, None, E.V2_C4_LABEL), (label, [f"| {label} | 0.7 |"]))

    def test_v2_pattern_with_two_labels_stops(self):
        p = self.doc(f"| F4s K=30 beta=80, CE on structured text | 0.7 |\n| {self.LABEL} | 0.7 |\n")
        with self.assertRaises(SystemExit) as stop:
            E.c4_target(p, None, E.V2_C4_LABEL)
        self.assertIn("STOP", str(stop.exception.code))

    def test_v2_pattern_with_no_label_stops(self):
        p = self.doc("| fp32 | 0.6 |\n")
        with self.assertRaises(SystemExit) as stop:
            E.c4_target(p, None, E.V2_C4_LABEL)
        self.assertIn("none", str(stop.exception.code))

    def test_missing_doc_stops_clearly(self):
        missing = self.dir / "gm-460-enlarged-eval.md"
        with self.assertRaises(SystemExit) as stop:
            E.c4_target(missing, None, E.V2_C4_LABEL)
        self.assertIn("STOP", str(stop.exception.code))
        self.assertIn(str(missing), str(stop.exception.code))

    def test_missing_section_stops(self):
        p = self.doc(f"| {self.LABEL} | 0.7 |\n")
        with self.assertRaises(SystemExit) as stop:
            E.c4_target(p, "## GM-464 S1", self.LABEL)
        self.assertIn("STOP", str(stop.exception.code))

    def test_targets_point_at_the_published_docs(self):
        self.assertEqual(E.C4_DOC.name, "gm-443-recall-rerank.md")
        self.assertEqual(E.V2_C4_DOC.name, "gm-460-enlarged-eval.md")
        self.assertTrue(E.V2_C4_LABEL.match(f"| {self.LABEL} |"))


class GM398Ids(unittest.TestCase):
    def test_ids_of_current_set_authored_and_mechanical(self):
        want = set()
        for rel in ("queries/gin.jsonl", "queries/mechanical/gin.jsonl"):
            for line in (HERE / rel).read_text().splitlines():
                if line.strip():
                    want.add(json.loads(line)["id"])
        got = E.gm398_ids([SimpleNamespace(name="gin")])
        self.assertEqual(got, want)
        mech = {json.loads(l)["id"] for l in (HERE / "queries/mechanical/gin.jsonl").read_text().splitlines()
                if l.strip()}
        self.assertTrue(mech and mech <= got)

    def test_unknown_corpus_gives_no_ids(self):
        self.assertEqual(E.gm398_ids([SimpleNamespace(name="no-such-corpus")]), set())


if __name__ == "__main__":
    unittest.main()
