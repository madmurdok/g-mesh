#!/usr/bin/env python3
"""GM-423: the fixture that pins production's embedded text to the Python port.

Writes core/src/embedding/testdata/structured_text.json: real doc comments
from the g-mesh snapshot (work/g-mesh.sqlite) that exercise the structured
rules (fenced code, indented code, headings, a long later paragraph), plus
hand-written docs for the rules g-mesh's own code does not use (Google-style
labels, JSDoc tags, reST fields, doctests, link-only lines, the fallback).
Each case's `expected` is computed here, by the Python port in
gm423_token_lengths.py, an implementation independent of the Rust one; the
Rust test `embedding::text::tests::matches_the_python_port_on_the_fixture`
asserts `text_to_embed` reproduces every one.

Selection is deterministic (ORDER BY id, first N per rule), so a re-run on the
same snapshot writes the same file.

Usage: python3 eval/embedding/gm423_text_fixture.py
"""
import json
import sqlite3
from pathlib import Path

from gm423_token_lengths import structured_doc, text_to_embed

EVAL = Path(__file__).resolve().parent
OUT = EVAL.parent.parent / "core/src/embedding/testdata/structured_text.json"
PER_RULE = 3
MAX_DOC_CHARS = 2500

# (rule, SQL predicate over docComment) - real g-mesh nodes.
REAL = [
    ("fenced code", "docComment LIKE '%```%'"),
    ("indented code", "docComment LIKE '%' || char(10) || '    %' AND docComment NOT LIKE '%```%'"),
    ("headings", "docComment LIKE '%' || char(10) || '# %' AND docComment NOT LIKE '%```%'"),
    (
        "long later paragraph",
        "length(docComment) > 600 AND docComment LIKE '%' || char(10) || char(10) || '%' "
        "AND docComment NOT LIKE '%```%' AND docComment NOT LIKE '%' || char(10) || '    %'",
    ),
]

# Rules g-mesh's own code does not exercise; written for this fixture.
WRITTEN = [
    ("google labels", "Parses the text.\n\nArgs:\n    text: the input.\n\nReturns:\n    The tree.\n\nRaises:\n    ValueError: when bad.", "def parse(text)"),
    ("jsdoc tags", "Calculates the deltas.\n\n@param prev - Previous.\n@param next - Next.\n@returns The delta.", "function deltas(prev, next)"),
    ("rest fields", "Sends the request.\n\n:param request: The request.\n:param timeout: How long\n    to wait.\n:rtype: Response", "def send(self, request, timeout=None)"),
    ("doctest and usage", "The adapter.\n\nUsage::\n\n  >>> import requests\n  >>> s = requests.Session()\n\n>>> s.mount('https://', a)", "class HTTPAdapter(BaseAdapter)"),
    ("link-only lines", "Reads it.\n\nSee: <https://example.com/wsl>\n\nMore in [`None`].\n[`None`]: https://example.com/option\nhttps://example.com/x", "fn read()"),
    ("code only, no signature", "```\nfn main() {}\n```", None),
    ("code only, with signature", "```\nfn main() {}\n```", "fn main()"),
    ("signature only", None, "fn f()"),
]


def main():
    cases = []
    conn = sqlite3.connect(f"file:{EVAL / 'work/g-mesh.sqlite'}?mode=ro", uri=True)
    seen = set()
    for rule, predicate in REAL:
        rows = conn.execute(
            f"SELECT id, docComment, signature FROM nodes WHERE docComment IS NOT NULL "
            f"AND length(docComment) <= {MAX_DOC_CHARS} AND ({predicate}) ORDER BY id"
        ).fetchall()
        taken = 0
        for node_id, doc, sig in rows:
            if node_id in seen or structured_doc(doc).strip() == doc.strip():
                continue  # only docs the rule actually trims
            seen.add(node_id)
            cases.append({"rule": rule, "source": f"g-mesh {node_id}", "doc": doc, "signature": sig})
            taken += 1
            if taken == PER_RULE:
                break
    for rule, doc, sig in WRITTEN:
        cases.append({"rule": rule, "source": "written", "doc": doc, "signature": sig})
    for case in cases:
        doc, sig = case["doc"], case["signature"]
        full = text_to_embed(doc, sig)
        case["expected"] = text_to_embed(structured_doc(doc) if doc else None, sig) or full
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(cases, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {len(cases)} cases to {OUT}")


if __name__ == "__main__":
    main()
