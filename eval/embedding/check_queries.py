#!/usr/bin/env python3
"""Authoring-time check of eval/embedding/queries/<corpus>.jsonl against D3
of docs/architecture/embedding-eval.md.

Checks the record format, that every expected file exists at the pinned
checkout, that each derivation names its sampled target, the shape
alternation, the positive/absent counts, and the overlap share (at least
half of the positives with overlap = false). The Rust harness repeats the
overlap computation and additionally resolves every expected entry against
the snapshot; this script is the author's fast loop, not the gate.

Usage: check_queries.py <corpus> [--checkout DIR] [--positives N] [--absent N]
                        [--eval-dir DIR] [--seed N] [--disjoint-from DIR]

--eval-dir reads <DIR>/queries/<corpus>.jsonl (the GM-422 confirmatory
study uses eval/embedding/confirm); --seed is the sampling seed each
positive's derivation must name (398 for GM-398, 4222 for GM-422);
--disjoint-from fails any query whose text, lower-cased and stripped, equals
an authored query text of the eval dir DIR (any corpus).
"""

import argparse
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
KINDS = {"Function", "Type", "Variable", "Module", "File"}
PREFIX = {
    "ripgrep": "rg",
    "g-mesh": "gm",
    "gin": "gin",
    "py-requests": "req",
    "task-tracker-mcp": "ttm",
    "excalidraw": "exc",
}
LANGUAGE = {
    "ripgrep": "rust",
    "g-mesh": "rust",
    "gin": "go",
    "py-requests": "python",
    "task-tracker-mcp": "typescript",
    "excalidraw": "typescript",
}


def sub_tokens(identifier):
    """Camel/snake split of an identifier, lower-cased, length >= 4."""
    parts = re.split(r"[^A-Za-z0-9]+", identifier)
    out = set()
    for part in parts:
        for piece in re.findall(r"[A-Z]+(?=[A-Z][a-z])|[A-Z]?[a-z]+|[A-Z]+|[0-9]+", part):
            if len(piece) >= 4:
                out.add(piece.lower())
    return out


def symbol_name(qualified_name):
    """Last segment of a qualified name, as the harness takes it."""
    return re.split(r"::|[.#/]", qualified_name)[-1]


def query_words(text):
    return {w.lower() for w in re.findall(r"[A-Za-z0-9]+", text)}


def overlap(text, expected):
    words = query_words(text)
    return any(words & sub_tokens(symbol_name(e["qualifiedName"])) for e in expected)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("corpus")
    ap.add_argument("--checkout")
    ap.add_argument("--positives", type=int)
    ap.add_argument("--absent", type=int)
    ap.add_argument("--eval-dir", default=HERE)
    ap.add_argument("--seed", type=int, default=398)
    ap.add_argument("--disjoint-from")
    args = ap.parse_args()
    earlier = set()
    if args.disjoint_from:
        qdir = os.path.join(args.disjoint_from, "queries")
        for name in sorted(os.listdir(qdir)):
            if name.endswith(".jsonl"):
                for line in open(os.path.join(qdir, name)):
                    if line.strip():
                        earlier.add(json.loads(line)["text"].strip().lower())

    corpus = args.corpus
    checkout = args.checkout or os.path.join(HERE, "work", "corpora", corpus)
    path = os.path.join(args.eval_dir, "queries", f"{corpus}.jsonl")
    errors = []
    ids = set()
    positives = absent = no_overlap = 0
    phrases = sentences = 0
    targets_used = set()
    for lineno, line in enumerate(open(path), 1):
        if not line.strip():
            continue
        q = json.loads(line)
        where = f"{path}:{lineno} ({q.get('id')})"
        for key in ("id", "corpus", "language", "kind", "shape", "text", "expected", "derivation", "author"):
            if key not in q:
                errors.append(f"{where}: missing {key}")
        if q.get("id") in ids:
            errors.append(f"{where}: duplicate id")
        ids.add(q.get("id"))
        if not str(q.get("id", "")).startswith(PREFIX[corpus] + "-"):
            errors.append(f"{where}: id must start with {PREFIX[corpus]}-")
        if q.get("corpus") != corpus or q.get("language") != LANGUAGE[corpus]:
            errors.append(f"{where}: corpus/language mismatch")
        if q.get("text", "").strip().lower() in earlier:
            errors.append(f"{where}: text repeats a query of {args.disjoint_from}")
        words = len(q.get("text", "").split())
        if q.get("shape") == "phrase":
            phrases += 1
            if not 3 <= words <= 7:
                errors.append(f"{where}: phrase has {words} words, want 3-7")
        elif q.get("shape") == "sentence":
            sentences += 1
            if not 10 <= words <= 25:
                errors.append(f"{where}: sentence has {words} words, want 10-25")
        else:
            errors.append(f"{where}: shape must be phrase or sentence")
        expected = q.get("expected", [])
        if q.get("kind") == "positive":
            positives += 1
            if not 1 <= len(expected) <= 3:
                errors.append(f"{where}: positive needs 1-3 expected symbols")
            m = re.search(rf"Target sampled \(seed {args.seed}, #(\d+)\)", q.get("derivation", ""))
            if not m:
                errors.append(f"{where}: derivation must start with 'Target sampled (seed {args.seed}, #N)'")
            elif m.group(1) in targets_used:
                errors.append(f"{where}: target #{m.group(1)} used twice")
            else:
                targets_used.add(m.group(1))
            if not overlap(q.get("text", ""), expected):
                no_overlap += 1
        elif q.get("kind") == "absent":
            absent += 1
            if expected:
                errors.append(f"{where}: absent query must have an empty expected set")
            if "absent" not in q.get("derivation", "").lower():
                errors.append(f"{where}: absent derivation must say how absence was proven")
        else:
            errors.append(f"{where}: kind must be positive or absent")
        for e in expected:
            if set(e) != {"filePath", "qualifiedName", "kind"}:
                errors.append(f"{where}: expected entry keys must be filePath, qualifiedName, kind")
            if e.get("kind") not in KINDS:
                errors.append(f"{where}: expected kind {e.get('kind')} not in {sorted(KINDS)}")
            if not os.path.isfile(os.path.join(checkout, e.get("filePath", ""))):
                errors.append(f"{where}: {e.get('filePath')} not in the pinned checkout")

    if args.positives is not None and positives != args.positives:
        errors.append(f"{positives} positives, want {args.positives}")
    if args.absent is not None and absent != args.absent:
        errors.append(f"{absent} absent, want {args.absent}")
    if positives and no_overlap * 2 < positives:
        errors.append(f"only {no_overlap}/{positives} positives have overlap = false; D3 needs at least half")
    print(
        json.dumps(
            {
                "corpus": corpus,
                "positives": positives,
                "absent": absent,
                "overlapFalse": no_overlap,
                "phrases": phrases,
                "sentences": sentences,
                "errors": len(errors),
            }
        )
    )
    for e in errors:
        print("  " + e)
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
