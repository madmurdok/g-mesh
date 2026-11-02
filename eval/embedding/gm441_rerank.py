#!/usr/bin/env python3
"""Applies F4 to the 100% checkpoint rankings written by `debug-embed-eval schedule`.

F4 is the shipped rerank: the top K=30 of the 100 kept hits, reordered by
`ce-minilm logit + BETA * cosine` (stable), the rest unchanged. It reuses
gm464_ce_structured.py's structured-text port and logit cache, and
rerank_eval.py's reranking, so the numbers come from the same scorer as
the ADR 0016 ones. It never scores a pair: when the cache lacks pairs of the
top-K windows it prints how many and stops.

For every <schedule-root>/<arm>/<corpus>/ it writes rankings.reranked.jsonl
and adds `recall10Reranked` / `mrrReranked` to the last checkpoint of
schedule.json (the checkpoint with `reranked: true`). It then checks that the
reranked rankings of all arms are identical per corpus.

usage: gm441_rerank.py --work <eval/embedding/work> --schedule-root <dir>
       [--arm A ...] [--corpus C ...]
"""
import argparse
import json
import sqlite3
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm464_ce_structured as G  # noqa: E402
import rerank_eval as R  # noqa: E402
from gm423_token_lengths import form_text  # noqa: E402

K = 30
BETA = 80.0
ARM = "schedule"


def add_structured_text(c):
    """c.stext: the structured text of every node with text, as the Rust port gives it."""
    con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
    c.stext = {}
    for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes ORDER BY id"):
        if nid in c.text:
            c.stext[nid] = form_text("structured", doc, sig, c.text[nid])
    con.close()


def read_lines(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def first_rank(hits, expected):
    exp = set(expected)
    return next((i + 1 for i, h in enumerate(hits) if h[0] in exp), None)


def recall_mrr(c, lines, hits_of):
    """Pooled recall@10 and MRR over the scored positives (language-pooled, as metrics.rs)."""
    r10, mrr = {}, {}
    for line in lines:
        q = c.queries[line["id"]]
        if not (q["positive"] and not q["mechanical"]):
            continue
        rank = first_rank(hits_of(line), line["expected"])
        r10.setdefault(q["language"], []).append(1.0 if rank is not None and rank <= 10 else 0.0)
        mrr.setdefault(q["language"], []).append(1.0 / rank if rank is not None else 0.0)
    return R.pooled_mean(r10) if r10 else None, R.pooled_mean(mrr) if mrr else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--schedule-root", required=True, type=Path)
    ap.add_argument("--arm", action="append")
    ap.add_argument("--corpus", action="append")
    a = ap.parse_args()
    arms = a.arm or sorted(p.name for p in a.schedule_root.iterdir() if p.is_dir())
    corpora = a.corpus or sorted(p.name for p in (a.schedule_root / arms[0]).iterdir() if p.is_dir())
    model_sha = R.sha256_file(a.work / "models" / R.CE_DIR[G.NAME] / "model.onnx")

    plans, missing = [], 0
    for corpus in corpora:
        c = R.Corpus(a.work, HERE, corpus, 0)
        add_structured_text(c)
        cache = G.Cache(a.work, [c], model_sha)
        for arm in arms:
            d = a.schedule_root / arm / corpus
            lines = read_lines(d / "rankings.jsonl")
            for line in lines:
                have = cache.d[c.name].get(line["id"], {})
                missing += sum(1 for h in line["hits"][:K] if G.tsha(c.stext[h[0]]) not in have)
            plans.append((arm, corpus, c, cache, d, lines))
    if missing:
        sys.exit(f"STOP: the logit cache lacks {missing} (query, node) pairs of the top-{K} windows; not scoring them")

    rerankings = {}
    for arm, corpus, c, cache, d, lines in plans:
        c.lists[ARM] = {line["id"]: [(h[0], h[1]) for h in line["hits"]] for line in lines}
        out = []
        for line in lines:
            q = line["id"]
            window = c.lists[ARM][q][:K]
            ce = np.array([cache.get(c, q, c.stext[n]) for n, _ in window])
            score = ce + BETA * np.array([cos for _, cos in window])
            hits, _ = R.reranked(c, q, K, score, ARM)
            out.append({**line, "hits": [[n, s] for n, s in hits]})
        (d / "rankings.reranked.jsonl").write_text("".join(json.dumps(x) + "\n" for x in out))
        by_id = {x["id"]: x["hits"] for x in out}
        rerankings.setdefault(corpus, {})[arm] = [(x["id"], [h[0] for h in x["hits"]]) for x in out]

        r10, mrr = recall_mrr(c, lines, lambda line: line["hits"])
        r10r, mrrr = recall_mrr(c, lines, lambda line: by_id[line["id"]])
        sched = json.loads((d / "schedule.json").read_text())
        last = sched["checkpoints"][-1]
        if not last["reranked"] or last["embedded"] != sched["nodes"]:
            sys.exit(f"STOP: {d}/schedule.json does not end in a reranked 100% checkpoint")
        for key, mine in (("recall10", r10), ("mrr", mrr)):
            if abs(last[key] - mine) > 1e-12:
                sys.exit(f"STOP: {d} {key} {last[key]} (schedule.json) != {mine} (rankings.jsonl)")
        last["recall10Reranked"], last["mrrReranked"] = r10r, mrrr
        (d / "schedule.json").write_text(json.dumps(sched, indent=2) + "\n")
        print(f"{corpus:18} {arm:14} recall@10 {r10:.6f} -> {r10r:.6f}   MRR {mrr:.6f} -> {mrrr:.6f}")

    for corpus, by_arm in rerankings.items():
        names = sorted(by_arm)
        same = all(by_arm[n] == by_arm[names[0]] for n in names)
        print(f"{corpus}: reranked rankings identical across {names}: {same}")
        if not same:
            sys.exit("STOP: reranked rankings differ between arms at 100%")


if __name__ == "__main__":
    main()
