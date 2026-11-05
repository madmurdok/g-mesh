#!/usr/bin/env python3
"""GM-698: the shared fixture of the product's cross-encoder text builder.

Writes core/src/embedding/testdata/ce_text.json: cases of
`gm462_ce_text.build(row, src, "EPB", tok, 48)`, the text search_code's
rerank reads (core/src/embedding/ce_text.rs), each built twice:

  words   tok = whitespace words (`len(s.split())`); the Rust always-run
          test builds every case with a whitespace counter and compares;
  minilm  tok = the ce-minilm tokenizer (the #[ignore]d Rust test).

Cases: the synthetic rules of test_gm462_ce_text.py, edge cases of the port
(CRLF, lone CR, invalid UTF-8, NBSP and \\x1f in a line, empty nativeKind,
empty name, a span past EOF), and real nodes sampled from the eval DBs.

A case's `srcLines` is the source the builder reads. A real node's lines are
its span from startLine, rebased to 0 and cut after the last line either
counter reads (`span_for`); the script asserts the cut cannot change either
text. A case with `srcBytes` (base64) is read from a file of those bytes the
way the eval reads a source file (`read_text(errors="replace")`, universal
newlines, split on "\\n"); its `srcLines` is that result.

usage: gm698_ce_text_fixture.py --work <eval/embedding/work> [--out path]
"""

import argparse
import base64
import json
import random
import sqlite3
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm462_ce_text as T  # noqa: E402

SWITCHES = frozenset("EPB")
BODY_BUDGET = 48
PER_LANGUAGE = 15
SEED = 698
NODE_COLS = ("kind", "nativeKind", "name", "qualifiedName", "filePath", "startLine", "endLine", "language",
             "signature", "docComment")
CORPUS_LANGUAGE = {"gin": "go", "py-requests": "python", "ripgrep": "rust", "g-mesh": "rust",
                   "excalidraw": "typescript", "task-tracker-mcp": "typescript"}
TOKENIZER = Path("models/ms-marco-MiniLM-L6-v2/tokenizer.json")


def words(s):
    return len(s.split())


def build(row, src, tok):
    return T.build(row, src, SWITCHES, tok, BODY_BUDGET)


def minilm_counter(work):
    from tokenizers import Tokenizer
    tk = Tokenizer.from_file(str(work / TOKENIZER))
    tk.no_truncation()
    return T.token_counter(tk)


def read_lines(path):
    """A source file's lines exactly as the eval reads them."""
    return path.read_text(errors="replace").split("\n")


def lines_of_bytes(data):
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "src"
        p.write_bytes(data)
        return read_lines(p)


def last_read(row, lines, tok):
    """The last source index the body field reads under `tok` (the line it
    stops at, when it stops), or the signature's last line without a body."""
    start, end = row["startLine"], row["endLine"]
    if row["kind"] not in T.BODY_KINDS or not lines:
        return start
    se = T.signature_end(lines, start, end, row["language"])
    idx = list(range(se + 1, min(end, len(lines) - 1) + 1))
    if row["language"] == "python":
        skip = T._skip_docstring([lines[i] for i in idx])
        if skip > len(idx):
            return idx[-1] if idx else se
        idx = idx[skip:]
    n, last = 0, se
    for i in idx:
        last = i
        if T._PUNCT_ONLY.match(lines[i]):
            continue
        n += tok(" ".join(lines[i].split())[:T.LINE_CHARS])
        if n > BODY_BUDGET:
            break
    return last


def span_for(row, lines, toks):
    """`row` rebased to line 0 and its span cut after the last line any
    counter in `toks` reads; asserts the cut changes no text."""
    if lines is None:
        return dict(row), None
    start = row["startLine"]
    cut = max(last_read(row, lines, tok) for tok in toks)
    span = lines[start:cut + 1]
    rebased = dict(row, startLine=0, endLine=row["endLine"] - start)
    for tok in toks:
        assert build(rebased, span, tok) == build(row, lines, tok), (row, "the cut changed the text")
    return rebased, span


def node(kind="Function", native_kind="function", name="f", qualified_name="f", file_path="src/a.rs",
         start=0, end=0, language="rust", signature=None, doc=None):
    return {"kind": kind, "nativeKind": native_kind, "name": name, "qualifiedName": qualified_name,
            "filePath": file_path, "startLine": start, "endLine": end, "language": language,
            "signature": signature, "docComment": doc}


def synthetic():
    """(label, row, src lines or None, src bytes or None)."""
    c = []

    def add(label, row, src=None, data=None):
        c.append((label, row, src, data))

    # split_words, through the name, qualified name and path fields
    for qn in ("HTTPServer::serve", "safeInt8::run", "send_quit::go", "parseURL2Fast::x", "ABC::x",
               "aB::x", "Ünïcode_naïve::x", "x9Y::x", "a__b--c::x"):
        add(f"words of {qn}", node(name="x", qualified_name=qn, file_path=qn.replace("::", "/")))
    # enclosing
    add("go method", node("Method", "method", "Next", "Context.Next", "context.go", language="go"))
    add("rust trait impl method", node("Method", "method", "send_quit", "walk::Worker::send_quit"))
    add("python method, no native kind", node("Method", None, "send", "requests.sessions.Session.send",
                                              "requests/sessions.py", language="python"))
    add("ts constructor", node("Method", "constructor", "constructor", "TaskConflictError#constructor",
                               "src/errors.ts", language="typescript"))
    add("top-level function is kind only", node("Function", "function", "requireTask", "requireTask"))
    add("qualified name not ending in name", node("Type", "struct", "Other", "pkg::Thing"))
    add("empty native kind falls back to kind", node("Type", "", "T", "m::T"))
    add("empty name keeps the whole qualified name", node("Function", "function", "", "m::f"))
    add("no name, no qualified name, no path", node("Function", None, None, None, None))
    add("qualified name equal to name", node("Function", "function", "f", "f"))
    # body lines
    add("python one-line docstring", node(language="python", end=2, file_path="a.py"),
        ["def f():", '    """Doc."""', "    return 1"])
    add("python multi-line docstring", node(language="python", end=6, file_path="a.py"),
        ["def f():", '    """Doc', "", "    more.", '    """', "    x = 1", "    return x"])
    add("python raw docstring", node(language="python", end=3, file_path="a.py"),
        ["def f():", '    r"""Doc \\d', '    """', "    return 1"])
    add("python unclosed docstring", node(language="python", end=3, file_path="a.py"),
        ["def f():", '    """Doc', "    more", "    return 1"])
    add("python three-letter prefix is no docstring", node(language="python", end=2, file_path="a.py"),
        ["def f():", '    rbu"""x"""', "    return 1"])
    add("no docstring keeps first line", node(language="python", end=2, file_path="a.py"),
        ["def f():", "    x = '''not a docstring'''", "    return x"])
    add("punctuation dropped, whitespace collapsed, 200-char cut", node(end=5),
        ["fn f() {", "    let   a  =  1;", "    });", "", "    " + "x" * 250, "}"])
    add("200-char cut counts code points", node(end=2), ["fn f() {", "    " + "é" * 250, "}"])
    add("only lines up to end", node(end=3),
        ["fn f() {", "    a();", "    b();", "}", "fn g() {", "    c();", "}"])
    # signature end
    add("python decorators and multi-line header", node(language="python", end=5, file_path="a.py"),
        ["@decorator", "@other(x=1)", "def f(a,", "      b) -> int:", '    """Doc."""', "    return a"])
    add("python colon in a comment or string", node(language="python", end=3, file_path="a.py"),
        ["def f(a='x:'):  # note:", "    return a", "", ""])
    add("rust multi-line parameters", node(end=4), ["pub fn f(", "    a: u8,", ") -> u8 {", "    a + 1", "}"])
    add("rust lifetime is not a string", node(end=2), ["fn f<'a>(x: &'a str) -> &'a str {", "    x", "}"])
    add("ts arrow ends the signature", node(language="typescript", end=2, file_path="a.ts"),
        ["export const f = (a: number,", "  b: number) =>", "  a + b;"])
    add("go brace inside parameters", node(language="go", end=2, file_path="a.go"),
        ["func f(opts struct{ A int }, s string) {", "    return", "}"])
    add("rust brace inside a string", node(end=3), ['fn f(s: &str = "{", t: u8,', "  u: u8) {", "  g()", "}"])
    add("escaped quote inside a string", node(end=2), ['fn f(s = "a\\"{") {', "  g()", "}"])
    add("line comment hides a brace", node(end=2), ["fn f() // {", "{ g()", "  h()"])
    add("backtick string spans lines", node(language="typescript", end=3, file_path="a.ts"),
        ["const f = `a", "{ b`;", "  c()", "  d()"])
    add("no signature line starts after start", node("Type", "type", "X", "X", language="typescript", end=2,
                                                     file_path="a.ts"), ["type X =", "  | A", "  | B"])
    add("start is honoured", node(language="python", start=2, end=3, file_path="a.py"),
        ["import x", "", "def f():", "    return 1"])
    # body budget and kinds
    many = ["fn f() {"] + ["    " + " ".join(f"w{i}_{j}" for j in range(7)) for i in range(10)] + ["}"]
    add("stops at the first line that does not fit", node(end=len(many) - 1), many)
    exact = ["fn f() {"] + ["    " + " ".join(["w"] * 8) for _ in range(6)] + ["    tail", "}"]
    add("exact budget fits", node(end=len(exact) - 1), exact)
    add("a long first line gives no body", node(end=2), ["fn f() {", "    " + " ".join(["w"] * 49), "}"])
    for kind in ("File", "Module", "Variable", "Method", "Field"):
        add(f"no body for {kind}", node(kind, kind.lower(), end=2), ["fn f() {", "    a();", "}"])
    add("type body", node("Type", "struct", "S", "m::S", end=3), ["struct S {", "    a: u8,", "    b: u8,", "}"])
    add("no source", node(end=3, signature="fn f()", doc="Does f."), None)
    add("empty source", node(end=3), [])
    add("span past EOF", node(start=1, end=40), ["// x", "fn f() {", "    a();"])
    add("start past EOF", node(start=9, end=12), ["fn f() {", "    a();", "}"])
    add("start after end", node(start=2, end=1), ["fn f() {", "    a();", "}"])
    add("nbsp and unit separator in a line", node(end=2),
        ["fn f() {", "    a b\x1fc\x1c d", "}"])
    add("unit separator only line is dropped", node(end=3), ["fn f() {", "\x1f\x1e", "    a();", "}"])
    add("unicode line separator inside a line", node(end=2), ["fn f() {", "    a b　c", "}"])
    # base text
    add("doc and signature", node(end=1, signature="pub fn f(x: u8)", doc="Does it.\n\nMore."),
        ["pub fn f(x: u8) {", "}"])
    add("doc only", node(doc="Does it.", end=0), ["fn f() {}"])
    # file reading
    add("crlf", node(end=3), data=b"fn f() {\r\n    a();\r\n    b();\r\n}\r\n")
    add("lone cr", node(end=3), data=b"fn f() {\r    a();\r    b();\r}")
    add("mixed newlines", node(end=3), data=b"fn f() {\r\n    a();\r    b();\n}")
    add("invalid utf-8", node(end=3), data=b"fn f() {\n    a(\"\xff\xfe\");\n    b(\"\xe2\x82\");\n}\n")
    add("bom and surrogate bytes", node(end=2), data=b"\xef\xbb\xbffn f() {\n    a(\"\xed\xa0\x80\");\n}")
    return c


def sample_real(work, rng):
    """(label, row, lines) for PER_LANGUAGE nodes per language: two thirds
    Function/Type, the rest other kinds."""
    by_lang = {}
    for corpus, lang in CORPUS_LANGUAGE.items():
        db = work / f"{corpus}.sqlite"
        if not db.exists():
            continue
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        rows = [dict(zip(("id",) + NODE_COLS, r)) for r in
                con.execute(f"SELECT id, {', '.join(NODE_COLS)} FROM nodes WHERE language = ? ORDER BY id", (lang,))]
        con.close()
        by_lang.setdefault(lang, []).extend((corpus, r) for r in rows)
    out = []
    for lang, rows in sorted(by_lang.items()):
        bodied = [x for x in rows if x[1]["kind"] in T.BODY_KINDS]
        other = [x for x in rows if x[1]["kind"] not in T.BODY_KINDS]
        n_body = PER_LANGUAGE * 2 // 3
        picks = rng.sample(bodied, min(n_body, len(bodied))) + rng.sample(other, min(PER_LANGUAGE - n_body, len(other)))
        for corpus, r in picks:
            p = work / "corpora" / corpus / r["filePath"]
            lines = read_lines(p) if p.is_file() else None
            out.append((f"{corpus} {r['kind']} {r['qualifiedName']}", {k: r[k] for k in NODE_COLS}, lines))
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--out", type=Path, default=HERE.parents[1] / "core/src/embedding/testdata/ce_text.json")
    a = ap.parse_args()
    minilm = minilm_counter(a.work)
    toks = (words, minilm)

    cases = []
    for label, row, src, data in synthetic():
        case = {"label": label, "node": row}
        if data is not None:
            src = lines_of_bytes(data)
            case["srcBytes"] = base64.b64encode(data).decode()
        case["srcLines"] = src
        case["words"], case["minilm"] = build(row, src, words), build(row, src, minilm)
        cases.append(case)
    real = sample_real(a.work, random.Random(SEED))
    for label, row, lines in real:
        rebased, span = span_for(row, lines, toks)
        cases.append({"label": label, "node": rebased, "srcLines": span,
                      "words": build(rebased, span, words), "minilm": build(rebased, span, minilm)})

    stops = sum(c["words"] != c["minilm"] for c in cases)
    print(f"{len(cases)} cases ({len(real)} real); words and minilm texts differ on {stops}")
    if not stops:
        sys.exit("STOP: no case tells the two counters apart")
    doc = {
        "about": "GM-698 CE text fixture, written by eval/embedding/gm698_ce_text_fixture.py",
        "bodyBudget": BODY_BUDGET,
        "cases": cases,
    }
    a.out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
