#!/usr/bin/env python3
"""GM-423: token counts of every embedded node text, under the model's tokenizer.

Reads each corpus snapshot (work/<corpus>.sqlite), rebuilds the text
`text_to_embed` produces (doc comment, blank line, signature; both trimmed,
empty ones dropped), the GM-423 "first-paragraph" form (the doc cut at its
first blank line) and the GM-465 "structured" form (`structured_doc` below, a
line-for-line port of core/src/cli/embed_eval/structured.rs), and tokenizes it with work/models/<model>/tokenizer.json,
special tokens included and no truncation - what `EmbeddingModel` counts
against `max_sequence_length`.

Control: for each form, the share above 512 and 1024 must equal the
`tokenShareOver512`/`tokenShareOver1024` the harness wrote into that form's
stored run manifests (runs/<model>[-<form>]/<corpus>/manifest.json; the
harness tokenizes the same texts in Rust); the script prints both side by
side, "-" where no run is stored. For "structured" this is also the check
that the Python port agrees with the Rust rules.

Usage: python3 eval/embedding/gm423_token_lengths.py [--model jina-v2-base-code-int8]
       [--forms full,first-paragraph,structured] [--json out.json]
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


# --- "structured": keep in step with core/src/cli/embed_eval/structured.rs ---

SHORT_PARAGRAPH_CHARS = 200
DROPPED_SECTIONS = {
    "arguments", "args", "parameters", "params", "returns", "return", "errors",
    "panics", "raises", "throws", "yields", "examples", "example", "usage",
}


def _lines(text):
    # Rust's str::lines: split on \n, strip one trailing \r, no final empty line.
    out = text.split("\n")
    if out and out[-1] == "":
        out.pop()
    return [l[:-1] if l.endswith("\r") else l for l in out]


def _heading_level(line):
    level = len(line) - len(line.lstrip("#"))
    rest = line[level:]
    return level if 1 <= level <= 6 and rest.startswith(" ") and rest.strip() else None


def _is_dropped_name(name):
    return name.strip().rstrip(":").strip().lower() in DROPPED_SECTIONS


def _is_label_line(line):
    line = line.strip()
    return line.endswith(":") and _is_dropped_name(line)


def _is_list_item(line):
    t = line.lstrip()
    if t.startswith(("- ", "* ", "+ ")):
        return True
    digits = len(t) - len(t.lstrip("0123456789"))
    return digits > 0 and t[digits:].startswith((". ", ") "))


def _is_code(block):
    if block[0].lstrip().startswith(">>>"):
        return True
    return not _is_list_item(block[0]) and all(
        l.startswith(("\t", "    ")) for l in block if l.strip()
    )


def _is_ascii_alpha(c):
    return c.isascii() and c.isalpha()


def _is_tag_line(line):
    t = line.lstrip()
    if t.startswith("@"):
        return len(t) > 1 and _is_ascii_alpha(t[1])
    if t.startswith(":"):
        rest = t[1:]
        name = 0
        while name < len(rest) and _is_ascii_alpha(rest[name]):
            name += 1
        return name > 0 and ":" in rest[name:]
    return False


def _is_link_only(line):
    t = line.strip()
    if t.startswith("[") and "]:" in t:
        end = t.index("]:")
        target = t[end + 2:].strip()
        return end > 1 and target != "" and not any(c.isspace() for c in target)
    if t.startswith("See:"):
        t = t[4:]
    elif t.startswith("See "):
        t = t[4:]
    t = t.strip()
    if t.startswith("<") and t.endswith(">") and len(t) >= 2:
        t = t[1:-1]
    return t.startswith(("https://", "http://")) and not any(c.isspace() for c in t)


def _paragraphs(doc):
    out, current, fence = [], [], None
    for line in _lines(doc):
        trimmed = line.lstrip()
        if fence is not None:
            if trimmed.startswith(fence):
                fence = None
            continue
        marker = next((m for m in ("```", "~~~") if trimmed.startswith(m)), None)
        heading = _heading_level(line) is not None
        if marker or line.strip() == "" or heading:
            if current:
                out.append(current)
                current = []
            if marker:
                fence = marker
            elif heading:
                out.append([line])
            continue
        current.append(line)
    if current:
        out.append(current)
    return out


def structured_doc(doc):
    kept, have_summary, skipping = [], False, None
    for block in _paragraphs(doc.strip()):
        level = _heading_level(block[0])
        if level is not None:
            if skipping is not None and level > skipping:
                continue
            skipping = None
            if _is_dropped_name(block[0].lstrip("#").strip()):
                skipping = level
            else:
                kept.append(block[0].strip())
            continue
        if skipping is not None or _is_label_line(block[0]) or _is_code(block):
            continue
        lines = []
        for l in block:
            if _is_tag_line(l):
                break
            if not _is_link_only(l):
                lines.append(l)
        text = "\n".join(lines).strip()
        if not text:
            continue
        if not have_summary or len(text) <= SHORT_PARAGRAPH_CHARS:
            kept.append(text)
        have_summary = True
    return "\n\n".join(kept)


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


FORMS = ["full", "first-paragraph", "structured"]


def form_text(form, doc, sig, full):
    if form == "full":
        return full
    if form == "first-paragraph":
        return text_to_embed(first_paragraph(doc) if doc else None, sig)
    # The harness falls back to the full text when the trimmed doc and the
    # signature are both empty, so the candidate set stays the same.
    return text_to_embed(structured_doc(doc) if doc else None, sig) or full


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="jina-v2-base-code-int8")
    ap.add_argument("--forms", default=",".join(FORMS), help="comma-separated subset of " + ",".join(FORMS))
    ap.add_argument("--json")
    a = ap.parse_args()
    forms = a.forms.split(",")
    unknown = [f for f in forms if f not in FORMS]
    if unknown:
        ap.error(f"unknown form(s): {', '.join(unknown)}")
    tok = Tokenizer.from_file(str(EVAL / "work" / "models" / a.model / "tokenizer.json"))
    tok.no_padding()
    tok.no_truncation()
    result = {}
    pooled = {form: [] for form in forms}
    for corpus in CORPORA:
        rows = sqlite3.connect(EVAL / "work" / f"{corpus}.sqlite").execute(
            "SELECT docComment, signature FROM nodes ORDER BY id"
        ).fetchall()
        texts = {form: [] for form in forms}
        for doc, sig in rows:
            t = text_to_embed(doc, sig)
            if t is None:
                continue
            for form in forms:
                texts[form].append(form_text(form, doc, sig, t))
        counts = {
            form: [len(e.ids) for e in tok.encode_batch(texts[form], add_special_tokens=True)] for form in forms
        }
        result[corpus] = {form: stats(c) for form, c in counts.items()}
        for form, c in counts.items():
            pooled[form].extend(c)
        harness = {}
        for form in forms:
            run = a.model if form == "full" else f"{a.model}-{form}"
            manifest = EVAL / "work" / "runs" / run / corpus / "manifest.json"
            if manifest.exists():
                m = json.loads(manifest.read_text())
                harness[form] = {
                    "n": m["nodeCount"],
                    "over512": m["tokenShareOver512"],
                    "over1024": m["tokenShareOver1024"],
                }
        if harness:
            result[corpus]["harness"] = harness
    result["pooled"] = {form: stats(c) for form, c in pooled.items()}

    print("| corpus | form | n | p50 | p90 | p95 | p99 | max | >256 | >512 | >1024 | harness n / >512 / >1024 |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|")
    for corpus in CORPORA + ["pooled"]:
        for form in forms:
            s = result[corpus][form]
            h = result[corpus].get("harness", {}).get(form)
            hs = f"{h['n']} / {100*h['over512']:.2f}% / {100*h['over1024']:.2f}%" if h else "-"
            print(
                f"| {corpus} | {form} | {s['n']} | {s['p50']} | {s['p90']} | {s['p95']} | {s['p99']} | {s['max']} "
                f"| {100*s['over256']:.2f}% | {100*s['over512']:.2f}% | {100*s['over1024']:.2f}% | {hs} |"
            )
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
