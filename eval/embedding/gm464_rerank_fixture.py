#!/usr/bin/env python3
"""GM-464: the order-parity fixture for search_code's cross-encoder rerank.

Writes core/src/embedding/testdata/rerank_parity.json: for a few NL and name
queries per eval corpus, int8-structured's top-30 (the node's columns,
cosine), the text search_code's rerank reads for each node
(gm462_ce_text.build(row, src, "EPB", ce-minilm tokenizer, 48), GM-698), the
logits rerank_eval.CrossEncoder.score gives each (query, text) pair under the
shipped weights (ce-minilm-quint8, the official quint8 AVX2 export, one pair per run,
GM-700), and the F4 order (ce + BETA * cosine, ties in int8 order).

Each row also carries `srcLines`, its source span from startLine, cut after
the last line the body field reads (gm698_ce_text_fixture.span_for), and
`wordsText`, the text built with a whitespace-word counter. The handler test
writes every row's span into one file per filePath (rows of one file share
it) and compares the texts a whitespace-counting stub was handed with
`wordsText`; the script asserts that file gives every row's texts.

The Rust tests read it: `embedding::rerank`'s blend test reorders the stored
logits (no model needed), and its #[ignore]d parity test builds the texts
with the real tokenizer, scores them with the real model and compares logits
and order.

The fixture must tell its controls apart, so this script refuses to write it
unless the order differs from F4 under beta = 80 and under the structured
text alone (the text before GM-698) on at least one query each.

usage: gm464_rerank_fixture.py --work <eval/embedding/v2/work> [--corpora dir] [--eval-dir dir]
                               [--st-runs dir] [--out path]
"""

import argparse
import json
import sqlite3
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402
import gm698_ce_text_fixture as F  # noqa: E402
from gm464_ce_structured import ST, add_structured  # noqa: E402

K = 30
BETA = 40.0
CE = "ce-minilm-quint8"  # rerank_eval scores it one pair per run (core's CHUNK = 1)
NL_PER_CORPUS = 2
NAME_PER_CORPUS = 1


def f4_order(logits, cosines, beta):
    s = np.asarray(logits, dtype=np.float64) + beta * np.asarray(cosines, dtype=np.float64)
    return [int(i) for i in np.lexsort((np.arange(len(s)), -s))]


def merged_files(rows):
    """One file's lines per filePath: each row's span written at its
    startLine, the lines no span covers left empty."""
    files = {}
    for r in rows:
        if r["srcLines"] is None:
            continue
        lines = files.setdefault(r["filePath"], [])
        top = r["startLine"] + len(r["srcLines"])
        lines.extend([""] * (top - len(lines)))
        lines[r["startLine"]:top] = r["srcLines"]
    return files


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--corpora", type=Path, help="corpus checkouts (default <work>/corpora)")
    ap.add_argument("--eval-dir", type=Path, default=HERE / "v2", help="holds queries/ (default eval/embedding/v2)")
    ap.add_argument("--st-runs", type=Path, help="int8-structured runs root (default <work>/runs-gm465/<ST>)")
    ap.add_argument("--out", type=Path, default=HERE.parents[1] / "core/src/embedding/testdata/rerank_parity.json")
    a = ap.parse_args()

    corpora = [p.name for p in sorted((a.work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    corpora_dir = a.corpora or a.work / "corpora"
    ce = R.CrossEncoder(a.work, CE)
    assert ce.chunk == 1, ce.chunk
    minilm = F.minilm_counter(a.work)
    toks = (F.words, minilm)
    cases, differs_beta, differs_base = [], 0, 0
    for name in corpora:
        c = R.Corpus(a.work, a.eval_dir, name, 0)
        add_structured(c, a.work, a.st_runs)
        con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
        con_rows = {r[0]: dict(zip(F.NODE_COLS, r[1:]))
                    for r in con.execute(f"SELECT id, {', '.join(F.NODE_COLS)} FROM nodes")}
        con.close()
        src = {}
        nl = [q for q in c.qids if not c.queries[q]["mechanical"] and c.queries[q]["positive"]][:NL_PER_CORPUS]
        names = [q for q in c.qids if c.queries[q]["mechanical"] and c.queries[q]["positive"]][:NAME_PER_CORPUS]
        for q in nl + names:
            hits = c.lists[ST][q][:K]
            query = c.queries[q]["text"]
            ids = [nid for nid, _ in hits]
            cosines = [float(x) for _, x in hits]
            rows = []
            for n, cos in zip(ids, cosines):
                row = con_rows[n]
                fp = row["filePath"]
                if fp not in src:
                    p = corpora_dir / name / fp
                    src[fp] = F.read_lines(p) if p.is_file() else None
                rebased, span = F.span_for(row, src[fp], toks)
                rows.append(dict(row, id=n, srcLines=span, cosine=cos,
                                 text=F.build(row, src[fp], minilm), wordsText=F.build(row, src[fp], F.words)))
            files = merged_files(rows)
            for r in rows:
                merged = files.get(r["filePath"])
                for tok, key in ((minilm, "text"), (F.words, "wordsText")):
                    assert F.build(r, merged, tok) == r[key], (q, r["id"], key, "the merged file changes the text")
            logits = [float(x) for x in ce.score(query, [r["text"] for r in rows])]
            base_logits = [float(x) for x in ce.score(query, [c.stext[n] for n in ids])]
            order = f4_order(logits, cosines, BETA)
            differs_beta += order != f4_order(logits, cosines, 80.0)
            differs_base += order != f4_order(base_logits, cosines, BETA)
            for r, lg in zip(rows, logits):
                r["logit"] = lg
            cases.append({
                "corpus": name,
                "queryId": q,
                "kind": "name" if c.queries[q]["mechanical"] else "nl",
                "query": query,
                "rows": rows,
                "order": [ids[i] for i in order],
            })
            print(f"{name} {q}: {len(ids)} rows, moved {sum(i != j for i, j in enumerate(order))}", flush=True)

    print(f"{len(cases)} queries; order differs under beta 80 on {differs_beta}, "
          f"under the structured text on {differs_base}")
    if not differs_beta or not differs_base:
        sys.exit("STOP: the fixture cannot tell a control apart")
    doc = {
        "about": "GM-464 rerank parity fixture, written by eval/embedding/gm464_rerank_fixture.py",
        "model": {"files": ce.files, "maxTokens": R.MAX_TOKENS},
        "k": K,
        "beta": BETA,
        "bodyBudget": F.BODY_BUDGET,
        "cases": cases,
    }
    a.out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
