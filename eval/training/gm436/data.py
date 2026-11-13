"""GM-456 training data for the GM-436 adapters (docs/design/GM-436-laptop-training.md,
sections 4 and 6): loader, InfoNCE masks, target-nested subsets, the C1
derangement and the C2 within-list permutation. stdlib + numpy only.
"""

import json
from pathlib import Path

import numpy as np

SEED = 436


def load_queries(vdir, split=None, repos=None, exclude_languages=()):
    """queries.jsonl rows reduced to what training reads; negatives as keys."""
    out = []
    with open(Path(vdir) / "queries.jsonl") as f:
        for line in f:
            if not line.strip():
                continue
            q = json.loads(line)
            if split and q["split"] != split:
                continue
            if repos is not None and q["repo"] not in repos:
                continue
            if q["language"] in exclude_languages:
                continue
            out.append({"id": q["id"], "split": q["split"], "repo": q["repo"], "language": q["language"],
                        "target": q["target"], "text": q["text"], "positives": list(q["positives"]),
                        "answersAlso": list(q.get("answersAlso") or []),
                        "negatives": [n["key"] for n in q.get("negatives") or []]})
    return out


def batch_columns(batch):
    """Columns and masks of one R1 batch.

    Columns are the unique keys of every in-batch query's positives and hard
    negatives (cross-batch hard negatives). For query i:
      pos[i, j]   = column j is one of i's positives;
      valid[i, j] = column j enters i's softmax: every column except i's
                    `answersAlso` that are not also i's positives.
    A column that is one of i's positives is never a negative for i, whichever
    query brought it in; the queries of one target share positives, so a
    same-target positive is a positive too.
    """
    cols, col = [], {}
    for q in batch:
        for k in q["positives"] + q["negatives"]:
            if k not in col:
                col[k] = len(cols)
                cols.append(k)
    pos = np.zeros((len(batch), len(cols)), bool)
    valid = np.ones((len(batch), len(cols)), bool)
    for i, q in enumerate(batch):
        for k in q["answersAlso"]:
            if k in col:
                valid[i, col[k]] = False
        for k in q["positives"]:
            pos[i, col[k]] = True
            valid[i, col[k]] = True
    return cols, pos, valid


def nested_subsets(targets, fractions=(0.25, 0.5, 1.0), seed=SEED):
    """{fraction: set of targets}: prefixes of one seeded permutation of the sorted unique targets."""
    uniq = sorted(set(targets))
    perm = [uniq[i] for i in np.random.default_rng(seed).permutation(len(uniq))]
    return {f: set(perm[:int(round(f * len(uniq)))]) for f in fractions}


def subset_queries(queries, fraction, seed=SEED):
    keep = nested_subsets([q["target"] for q in queries], seed=seed)[fraction] if fraction < 1 else None
    return [q for q in queries if keep is None or q["target"] in keep]


def derangement(n, seed=SEED):
    """A permutation of range(n) with no fixed point (Sattolo: one n-cycle), n >= 2."""
    if n < 2:
        raise ValueError("a derangement needs n >= 2")
    rng = np.random.default_rng(seed)
    p = list(range(n))
    for i in range(n - 1, 0, -1):
        j = int(rng.integers(0, i))
        p[i], p[j] = p[j], p[i]
    return p


def shuffle_pairs(queries, seed=SEED):
    """C1: every query gets the labels (positives, answersAlso, negatives) of another target."""
    targets = sorted({q["target"] for q in queries})
    first = {}
    for q in queries:
        first.setdefault(q["target"], q)
    perm = derangement(len(targets), seed)
    partner = {t: first[targets[perm[i]]] for i, t in enumerate(targets)}
    out = []
    for q in queries:
        p = partner[q["target"]]
        out.append(dict(q, positives=p["positives"], answersAlso=p["answersAlso"], negatives=p["negatives"],
                        labelsOf=p["target"]))
    return out


def permute_within(scores, seed=SEED):
    """C2: each query's teacher scores permuted within its own candidate list (multiset kept).
    `scores` is {qid: 1-d array}; qids are visited in sorted order."""
    rng = np.random.default_rng(seed)
    return {qid: np.asarray(scores[qid])[rng.permutation(len(scores[qid]))] for qid in sorted(scores)}
