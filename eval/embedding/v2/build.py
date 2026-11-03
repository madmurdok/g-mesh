#!/usr/bin/env python3
"""Build or check the v2 authored query files (docs/design/GM-460-enlarged-nl-eval.md,
sections 4 and 6).

v2/queries/<corpus>.jsonl is the bytes of three sources, verbatim and in
this order:
  1. eval/embedding/queries/<corpus>.jsonl          (GM-398, sha256 pinned)
  2. eval/embedding/confirm/queries/<corpus>.jsonl  (GM-422 confirm, sha256 pinned)
  3. eval/embedding/v2/new/queries/<corpus>.jsonl   (GM-460 top-up; empty if absent)

The pinned sha256 are the GM-398 run manifests' `queryFiles` and the Files
table of docs/results/gm-422-int8-confirm.md. A pinned source that differs
stops the build: stored runs and both old studies depend on those bytes.

Without --check, writes the v2 files. With --check, writes nothing and fails
unless each v2 file equals the concatenation, its ids are unique, and every
line ends in "\\n". Both modes print per-language, per-side counts (the split
is `is_held_out_id`: sha256(id)[0] odd) and the cross-side overlap list: each
held-out positive whose expected symbol is also expected by a FIT positive.

usage: build.py [--check] [--json OUT]
"""
import argparse
import hashlib
import json
import os
import re
import sys

V2 = os.path.dirname(os.path.abspath(__file__))
EVAL = os.path.dirname(V2)

CORPORA = ["gin", "py-requests", "ripgrep", "g-mesh", "excalidraw", "task-tracker-mcp"]
LANGUAGE = {
    "gin": "go",
    "py-requests": "python",
    "ripgrep": "rust",
    "g-mesh": "rust",
    "excalidraw": "typescript",
    "task-tracker-mcp": "typescript",
}
PREFIX = {
    "gin": "gin",
    "py-requests": "req",
    "ripgrep": "rg",
    "g-mesh": "gm",
    "excalidraw": "exc",
    "task-tracker-mcp": "ttm",
}
# (GM-398 queries/<corpus>.jsonl, confirm/queries/<corpus>.jsonl)
PINNED = {
    "gin": (
        "8e4bc84b1c5bf8479c5296b832fe856b42d8e91bc468ac8100ca675d33b177a7",
        "50757662698de019c9450615dee79b48196b1373ff828bcbf25e4ed03718ba3b",
    ),
    "py-requests": (
        "17b75b266e218961abebdd08a74d912b62b01e978f60010b742e6d50b5505e0e",
        "7fedbf196f826e17776f0a330e11d9cd50f8b227159880b149fc693cd81670ce",
    ),
    "ripgrep": (
        "cfb3c016bee4adacb1302fb31f10d50912e650d2b1c7bc9ac82e2d0b21ef083c",
        "51b126d72394ddc8b1076697bb077052e0e63b1d37a9414cd00ceecc3705e3cc",
    ),
    "g-mesh": (
        "4e290fb888704a604e6efe003e5c2e1b7564518131ca4eb97c95087a078fa133",
        "39a6d97af21589eaa76fb1d98681bb90f9aa8784597d106e5fe64ee3d51d26b1",
    ),
    "excalidraw": (
        "9cbdd1dc78bed9e941f149f1aceb3f8b836e13d7bec3a7b2bbbfb46705de6782",
        "0e703f65f9a616028c9b9cf56cc8c89635119a9e7e69280538cbd9537da1eb41",
    ),
    "task-tracker-mcp": (
        "16dc63f7b43e2dad09ba5492520ebac148d0727168f4ac49d2b3d6128b4c20d3",
        "c698ace39f4a2c337702e197cd022c0e5a1541665f226cef4a43d043e25a8ee2",
    ),
}
SOURCES = ("gm398", "confirm", "new")


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def is_held_out_id(qid):
    """Mirror of core/src/cli/embed_eval/queries.rs::is_held_out_id."""
    return hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def read(path):
    with open(path, "rb") as f:
        return f.read()


def source_paths(corpus):
    return [
        os.path.join(EVAL, "queries", f"{corpus}.jsonl"),
        os.path.join(EVAL, "confirm", "queries", f"{corpus}.jsonl"),
        os.path.join(V2, "new", "queries", f"{corpus}.jsonl"),
    ]


def rel(path):
    return os.path.relpath(path, os.path.dirname(os.path.dirname(EVAL)))


def lines_of(data, where, errors):
    """Parsed records of a JSONL byte string; every line must end in \\n and be non-blank."""
    if not data:
        return []
    if not data.endswith(b"\n"):
        errors.append(f"{where}: last line does not end in \\n")
    out = []
    lines = data.split(b"\n")
    for n, line in enumerate(lines[:-1] if data.endswith(b"\n") else lines, 1):
        if not line.strip():
            errors.append(f"{where}:{n}: blank line")
            continue
        if line.endswith(b"\r"):
            errors.append(f"{where}:{n}: line ends in \\r\\n")
        try:
            out.append(json.loads(line))
        except json.JSONDecodeError as e:
            errors.append(f"{where}:{n}: not JSON ({e})")
    return out


def build_corpus(corpus, check, errors):
    """Checks one corpus and returns (records tagged with their source, file report)."""
    errors_before = len(errors)
    parts = []
    for i, path in enumerate(source_paths(corpus)):
        data = read(path) if (i < 2 or os.path.exists(path)) else b""
        if i < 2 and sha256(data) != PINNED[corpus][i]:
            errors.append(f"{rel(path)}: sha256 {sha256(data)}, pinned {PINNED[corpus][i]}")
        lines_of(data, rel(path), errors)
        parts.append(data)
    merged = b"".join(parts)
    out = os.path.join(V2, "queries", f"{corpus}.jsonl")
    if check:
        if not os.path.exists(out):
            errors.append(f"{rel(out)}: missing; run build.py")
        else:
            have = read(out)
            offset = 0
            for name, part in zip(SOURCES, parts):
                if have[offset : offset + len(part)] != part:
                    errors.append(f"{rel(out)}: bytes {offset}..{offset + len(part)} differ from the {name} source")
                offset += len(part)
            if len(have) != offset:
                errors.append(f"{rel(out)}: {len(have)} bytes, sources total {offset}")
            merged = have

    records = []
    for name, part in zip(SOURCES, parts):
        for q in lines_of(part, "", []):
            records.append((name, q))
    lines_of(merged, rel(out), errors)

    ids = {}
    new_id = re.compile(rf"^{re.escape(PREFIX[corpus])}-e\d{{3}}$")
    for name, q in records:
        qid = q.get("id")
        if qid in ids:
            errors.append(f"{rel(out)}: id {qid} in {ids[qid]} and {name}")
        ids[qid] = name
        if q.get("corpus") != corpus or q.get("language") != LANGUAGE[corpus]:
            errors.append(f"{rel(out)}: {qid} corpus/language mismatch")
        if name == "new" and not new_id.match(str(qid)):
            errors.append(f"{rel(out)}: new id {qid} must match {PREFIX[corpus]}-eNNN")
    if not check:
        if len(errors) == errors_before:
            os.makedirs(os.path.dirname(out), exist_ok=True)
            with open(out, "wb") as f:
                f.write(merged)
        else:
            errors.append(f"{rel(out)}: not written")
    report = {
        "file": rel(out),
        "sha256": sha256(merged),
        "bytes": len(merged),
        "queries": {s: sum(1 for n, _ in records if n == s) for s in SOURCES},
    }
    return records, report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="verify the v2 files instead of writing them")
    ap.add_argument("--json", help="also write the full report as JSON to this path")
    args = ap.parse_args()

    errors = []
    files = {}
    rows = []  # (corpus, source, record)
    for corpus in CORPORA:
        records, report = build_corpus(corpus, args.check, errors)
        files[corpus] = report
        rows.extend((corpus, s, q) for s, q in records)

    counts = {}
    for corpus, source, q in rows:
        lang = LANGUAGE[corpus]
        side = "held-out" if is_held_out_id(q["id"]) else "FIT"
        for key in ((lang, q["kind"], side), (lang, q["kind"], "all")):
            counts[key] = counts.get(key, 0) + 1

    # Cross-side overlap: held-out positive sharing an expected symbol with a FIT positive.
    fit_by_symbol = {}
    for corpus, _, q in rows:
        if q["kind"] == "positive" and not is_held_out_id(q["id"]):
            for e in q["expected"]:
                fit_by_symbol.setdefault((corpus, e["filePath"], e["qualifiedName"]), []).append(q["id"])
    overlap = []
    for corpus, _, q in rows:
        if q["kind"] == "positive" and is_held_out_id(q["id"]):
            for e in q["expected"]:
                for fit_id in fit_by_symbol.get((corpus, e["filePath"], e["qualifiedName"]), []):
                    overlap.append(
                        {"heldOut": q["id"], "fit": fit_id, "filePath": e["filePath"], "qualifiedName": e["qualifiedName"]}
                    )

    print("mode:", "check" if args.check else "build")
    print("\nfiles (queries by source gm398 / confirm / new):")
    for corpus, r in files.items():
        q = r["queries"]
        print(f"  {r['file']}  {q['gm398']} / {q['confirm']} / {q['new']}  sha256 {r['sha256']}")
    print("\nper language (FIT / held-out / all; held-out share; +-15% of half):")
    print("  language    kind      FIT  held-out  all   share   within")
    balance = []
    for lang in sorted(set(LANGUAGE.values())):
        for kind in ("positive", "absent"):
            fit = counts.get((lang, kind, "FIT"), 0)
            held = counts.get((lang, kind, "held-out"), 0)
            total = fit + held
            within = total == 0 or abs(held - total / 2) <= 0.15 * total / 2
            balance.append({"language": lang, "kind": kind, "fit": fit, "heldOut": held, "within15pct": within})
            share = held / total if total else 0.0
            print(f"  {lang:<11} {kind:<9} {fit:>4} {held:>9} {total:>4}  {share:6.3f}   {'yes' if within else 'NO'}")
    print(f"\ncross-side overlap (held-out positive sharing an expected symbol with a FIT positive): {len(overlap)}")
    for o in overlap:
        print(f"  {o['heldOut']} (held-out) ~ {o['fit']} (FIT): {o['filePath']} {o['qualifiedName']}")

    if args.json:
        with open(args.json, "w") as f:
            json.dump({"files": files, "balance": balance, "overlap": overlap, "errors": errors}, f, indent=1)
            f.write("\n")
    print(f"\nerrors: {len(errors)}")
    for e in errors:
        print("  " + e)
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
