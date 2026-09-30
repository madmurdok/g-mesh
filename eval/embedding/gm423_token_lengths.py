#!/usr/bin/env python3
"""GM-423: token counts of every embedded node text, under the model's tokenizer.

Reads each corpus snapshot (work/<corpus>.sqlite), rebuilds the text
`text_to_embed` produces (doc comment, blank line, signature; both trimmed,
empty ones dropped) and the GM-423 "first-paragraph" form (the doc cut at its
first blank line), and tokenizes it with work/models/<model>/tokenizer.json,
special tokens included and no truncation - what `EmbeddingModel` counts
against `max_sequence_length`.

Control: the share above 512 and 1024 for the full form must equal the
`tokenShareOver512`/`tokenShareOver1024` the harness wrote into the stored
int8 run's manifests (it tokenizes the same texts in Rust); the script prints
both side by side.

Usage: python3 eval/embedding/gm423_token_lengths.py [--model jina-v2-base-code-int8] [--json out.json]
"""
import argparse
import json
import sqlite3
from pathlib import Path

from tokenizers import Tokenizer

EVAL = Path(__file__).resolve().parent
CORPORA = ["task-tracker-mcp", "excalidraw", "ripgrep", "g-mesh", "gin", "py-requests"]


def first_paragraph(doc: str) -> str:
    doc = doc.strip()
    out = []
    for line in doc.split("\n"):
        if line.strip() == "":
            break
        out.append(line)
    return "\n".join(out).rstrip()


def text_to_embed(doc, sig):
    doc = doc.strip() if doc else ""
    sig = sig.strip() if sig else ""
    if doc and sig:
        return f"{doc}\n\n{sig}"
    return doc or sig or None


def percentile(sorted_vals, p):
    # nearest-rank
    if not sorted_vals:
        return None
    k = max(0, min(len(sorted_vals) - 1, int(-(-p * len(sorted_vals) // 100)) - 1))
    return sorted_vals[k]


def stats(counts):
    s = sorted(counts)
    n = len(s)
    return {
        "n": n,
        "p50": percentile(s, 50),
        "p90": percentile(s, 90),
        "p95": percentile(s, 95),
        "p99": percentile(s, 99),
        "max": s[-1],
        "over256": sum(c > 256 for c in s) / n,
        "over512": sum(c > 512 for c in s) / n,
        "over1024": sum(c > 1024 for c in s) / n,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="jina-v2-base-code-int8")
    ap.add_argument("--json")
    a = ap.parse_args()
    tok = Tokenizer.from_file(str(EVAL / "work" / "models" / a.model / "tokenizer.json"))
    tok.no_padding()
    tok.no_truncation()
    result = {}
    pooled = {"full": [], "first-paragraph": []}
    for corpus in CORPORA:
        rows = sqlite3.connect(EVAL / "work" / f"{corpus}.sqlite").execute(
            "SELECT docComment, signature FROM nodes ORDER BY id"
        ).fetchall()
        full, cut = [], []
        for doc, sig in rows:
            t = text_to_embed(doc, sig)
            if t is None:
                continue
            full.append(t)
            cut.append(text_to_embed(first_paragraph(doc) if doc else None, sig))
        counts = {
            "full": [len(e.ids) for e in tok.encode_batch(full, add_special_tokens=True)],
            "first-paragraph": [len(e.ids) for e in tok.encode_batch(cut, add_special_tokens=True)],
        }
        result[corpus] = {form: stats(c) for form, c in counts.items()}
        for form, c in counts.items():
            pooled[form].extend(c)
        manifest = EVAL / "work" / "runs" / a.model / corpus / "manifest.json"
        if manifest.exists():
            m = json.loads(manifest.read_text())
            result[corpus]["harness"] = {
                "n": m["nodeCount"],
                "over512": m["tokenShareOver512"],
                "over1024": m["tokenShareOver1024"],
            }
    result["pooled"] = {form: stats(c) for form, c in pooled.items()}

    print("| corpus | form | n | p50 | p90 | p95 | p99 | max | >256 | >512 | >1024 | harness n / >512 / >1024 |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|")
    for corpus in CORPORA + ["pooled"]:
        for form in ("full", "first-paragraph"):
            s = result[corpus][form]
            h = result[corpus].get("harness") if form == "full" else None
            hs = f"{h['n']} / {100*h['over512']:.2f}% / {100*h['over1024']:.2f}%" if h else "-"
            print(
                f"| {corpus} | {form} | {s['n']} | {s['p50']} | {s['p90']} | {s['p95']} | {s['p99']} | {s['max']} "
                f"| {100*s['over256']:.2f}% | {100*s['over512']:.2f}% | {100*s['over1024']:.2f}% | {hs} |"
            )
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
