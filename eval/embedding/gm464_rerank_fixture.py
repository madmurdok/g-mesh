#!/usr/bin/env python3
"""GM-464: the order-parity fixture for search_code's cross-encoder rerank.

Writes core/src/embedding/testdata/rerank_parity.json: for a few NL and name
queries per eval corpus, int8-structured's top-30 (node id, doc comment,
signature, cosine), the ce-minilm logits rerank_eval.CrossEncoder.score gives
each (query, structured text) pair, and the F4 order (ce + 80 * cosine,
ties in int8 order), exactly as gm464_ce_structured.py computes it.

The Rust tests read it: `embedding::rerank`'s blend test reorders the stored
logits (no model needed), and its #[ignore]d parity test scores the pairs with
the real model and compares logits and order.

The fixture must tell its controls apart, so this script refuses to write it
unless the order differs from F4 under beta = 40 and under the full
(untrimmed) text on at least one query each.

usage: gm464_rerank_fixture.py --work <eval/embedding/work> [--out path]
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
from gm464_ce_structured import ST, add_structured  # noqa: E402

K = 30
BETA = 80.0
NL_PER_CORPUS = 2
NAME_PER_CORPUS = 1


def f4_order(logits, cosines, beta):
    s = np.asarray(logits, dtype=np.float64) + beta * np.asarray(cosines, dtype=np.float64)
    return [int(i) for i in np.lexsort((np.arange(len(s)), -s))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--out", type=Path, default=HERE.parents[1] / "core/src/embedding/testdata/rerank_parity.json")
    a = ap.parse_args()

    corpora = [p.name for p in sorted((a.work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    ce = R.CrossEncoder(a.work, "ce-minilm")
    cases, differs_beta, differs_full = [], 0, 0
    for name in corpora:
        c = R.Corpus(a.work, HERE, name, 0)
        add_structured(c, a.work)
        con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
        con_rows = {nid: (doc, sig) for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes")}
        con.close()
        nl = [q for q in c.qids if not c.queries[q]["mechanical"] and c.queries[q]["positive"]][:NL_PER_CORPUS]
        names = [q for q in c.qids if c.queries[q]["mechanical"] and c.queries[q]["positive"]][:NAME_PER_CORPUS]
        for q in nl + names:
            hits = c.lists[ST][q][:K]
            query = c.queries[q]["text"]
            ids = [nid for nid, _ in hits]
            cosines = [float(x) for _, x in hits]
            logits = [float(x) for x in ce.score(query, [c.stext[n] for n in ids])]
            full_logits = [float(x) for x in ce.score(query, [c.text[n] for n in ids])]
            order = f4_order(logits, cosines, BETA)
            differs_beta += order != f4_order(logits, cosines, 40.0)
            differs_full += order != f4_order(full_logits, cosines, BETA)
            cases.append({
                "corpus": name,
                "queryId": q,
                "kind": "name" if c.queries[q]["mechanical"] else "nl",
                "query": query,
                "rows": [
                    {"id": n, "docComment": con_rows[n][0], "signature": con_rows[n][1], "text": c.stext[n],
                     "cosine": cos, "logit": lg}
                    for n, cos, lg in zip(ids, cosines, logits)
                ],
                "order": [ids[i] for i in order],
            })
            print(f"{name} {q}: {len(ids)} rows, moved {sum(i != j for i, j in enumerate(order))}", flush=True)

    print(f"{len(cases)} queries; order differs under beta 40 on {differs_beta}, under the full text on {differs_full}")
    if not differs_beta or not differs_full:
        sys.exit("STOP: the fixture cannot tell a control apart")
    doc = {
        "about": "GM-464 rerank parity fixture, written by eval/embedding/gm464_rerank_fixture.py",
        "model": {"files": ce.files, "maxTokens": R.MAX_TOKENS},
        "k": K,
        "beta": BETA,
        "cases": cases,
    }
    a.out.write_text(json.dumps(doc, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
