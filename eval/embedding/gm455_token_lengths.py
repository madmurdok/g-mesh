#!/usr/bin/env python3
"""GM-455: token counts of the structural-context texts, header included.

Rebuilds every embedded text of a (form, context) variant from the corpus
snapshot (work/<corpus>.sqlite): `gm423_token_lengths.form_text` for the
body, then the unlabelled header of core/src/cli/embed_eval/context.rs
(file path, then the parent line, joined by "\\n", then "\\n\\n"), and
tokenizes it as gm423_token_lengths.py does (special tokens, no truncation).

The shuffled arm is not ported (its derangement is Rust's seeded RNG); its
header has the same shape and length distribution as path-parent's.

Controls, per corpus and variant:
  - sha256 of every rebuilt text against `debug-embed-eval churn --dump`
    (`<dump>/<corpus>/<variant>.tsv`, id<TAB>sha256 of the harness's text):
    all must match, else the Python port is not the text the harness embeds;
  - the shares above 512/1024 against the variant's run manifest
    (`tokenShareOver512/1024`, computed in Rust).

Usage: gm455_token_lengths.py --out <runs dir> --dump <dump dir>
       --variants fp-ctx-none:first-paragraph:none,fp-path:first-paragraph:path,... [--json f]
"""
import argparse
import hashlib
import json
import sqlite3
import sys
from pathlib import Path

from tokenizers import Tokenizer

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gm423_token_lengths as t  # noqa: E402

EVAL = t.EVAL


def owner_of(qn, name):
    if not name or not qn.endswith(name):
        return None
    rest = qn[: len(qn) - len(name)]
    for sep in ("::", ".", "#"):
        if rest.endswith(sep):
            r = rest[: len(rest) - len(sep)]
            return r or None
    return None


def trait_impl_segment(owner):
    if not owner.endswith(">"):
        return None
    depth, open_ = 0, None
    for i in range(len(owner) - 1, -1, -1):
        c = owner[i]
        if c == ">":
            depth += 1
        elif c == "<":
            depth -= 1
            if depth == 0:
                open_ = i
                break
    if open_ is None:
        return None
    if open_ != 0 and not owner[:open_].endswith("::"):
        return None
    inner = owner[open_ + 1 : -1]
    depth = 0
    for i, c in enumerate(inner):
        if c == "<":
            depth += 1
        elif c == ">":
            depth -= 1
        elif c == " " and depth == 0 and inner[i:].startswith(" as "):
            x, tr = inner[:i].strip(), inner[i + 4 :].strip()
            return (x, tr) if x and tr else None
    return None


def parent_lines(nodes):
    types = {}
    for i, n in enumerate(nodes):
        if n["kind"] == "Type":
            types.setdefault((n["language"], n["qualifiedName"]), []).append(i)
    out = []
    for n in nodes:
        owner = owner_of(n["qualifiedName"], n["name"])
        line = None
        if owner is not None:
            ti = trait_impl_segment(owner) if n["language"] == "rust" else None
            if ti:
                line = f"impl {ti[1]} for {ti[0]}"
            else:
                found = types.get((n["language"], owner))
                if found:
                    pick = next((j for j in found if nodes[j]["filePath"] == n["filePath"]), found[0])
                    p = nodes[pick]
                    line = f"{p['nativeKind']} {p['name']}" if p["nativeKind"] else p["name"]
        out.append(line)
    return out


def texts(corpus, form, context):
    con = sqlite3.connect(EVAL / "work" / f"{corpus}.sqlite")
    con.row_factory = sqlite3.Row
    nodes = [dict(r) for r in con.execute(
        "SELECT id, kind, qualifiedName, filePath, language, docComment, signature, name, nativeKind "
        "FROM nodes ORDER BY id"
    )]
    parents = parent_lines(nodes) if "parent" in context else [None] * len(nodes)
    res = []
    for n, par in zip(nodes, parents):
        full = t.text_to_embed(n["docComment"], n["signature"])
        if full is None:
            continue
        body = t.form_text(form, n["docComment"], n["signature"], full)
        if body is None:
            continue
        header = []
        if "path" in context:
            header.append(n["filePath"])
        if par is not None:
            header.append(par)
        res.append((n["id"], body, "\n".join(header) + "\n\n" + body if header else body))
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="jina-v2-base-code-int8")
    ap.add_argument("--out", required=True)
    ap.add_argument("--dump", required=True)
    ap.add_argument("--variants", required=True)
    ap.add_argument("--corpora", default=",".join(t.CORPORA))
    ap.add_argument("--json")
    a = ap.parse_args()
    tok = Tokenizer.from_file(str(EVAL / "work" / "models" / a.model / "tokenizer.json"))
    tok.no_padding()
    tok.no_truncation()
    variants = [v.split(":") for v in a.variants.split(",")]
    result, pooled, all_ok = {}, {}, True
    rows = []
    for corpus in a.corpora.split(","):
        for name, form, context in variants:
            tx = texts(corpus, form, context)
            dump = Path(a.dump) / corpus / f"{name}.tsv"
            sha_ok = sha_n = None
            if dump.exists():
                want = dict(l.split("\t") for l in dump.read_text().splitlines() if l)
                got = {i: hashlib.sha256(s.encode()).hexdigest() for i, _, s in tx}
                sha_n = len(want)
                sha_ok = sum(got.get(i) == h for i, h in want.items())
                all_ok &= sha_ok == sha_n and len(got) == len(want)
            full = [len(e.ids) for e in tok.encode_batch([s for _, _, s in tx], add_special_tokens=True)]
            base = [len(e.ids) for e in tok.encode_batch([b for _, b, _ in tx], add_special_tokens=True)]
            added = [f - b for f, b in zip(full, base)]
            s = t.stats(full)
            s["headerMean"] = sum(added) / len(added)
            s["headerP50"] = t.percentile(sorted(added), 50)
            s["headerMax"] = max(added)
            s["shaMatch"], s["shaN"] = sha_ok, sha_n
            m = Path(a.out) / name / corpus / "manifest.json"
            if m.exists():
                mj = json.loads(m.read_text())
                s["harness"] = [mj["nodeCount"], mj["tokenShareOver512"], mj["tokenShareOver1024"]]
            result.setdefault(corpus, {})[name] = s
            p = pooled.setdefault(name, ([], []))
            p[0].extend(full)
            p[1].extend(added)
            rows.append((corpus, name, s))
    for name, (full, added) in pooled.items():
        s = t.stats(full)
        s["headerMean"] = sum(added) / len(added)
        s["headerP50"] = t.percentile(sorted(added), 50)
        s["headerMax"] = max(added)
        result.setdefault("pooled", {})[name] = s
        rows.append(("pooled", name, s))
    print("| corpus | variant | n | p50 | p90 | p99 | max | >512 | >1024 | header tok mean / p50 / max | sha256 match | harness n / >512 / >1024 |")
    print("|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|---|")
    for corpus, name, s in rows:
        sha = f"{s['shaMatch']}/{s['shaN']}" if s.get("shaN") is not None else "-"
        h = s.get("harness")
        hs = f"{h[0]} / {100*h[1]:.2f}% / {100*h[2]:.2f}%" if h else "-"
        print(
            f"| {corpus} | {name} | {s['n']} | {s['p50']} | {s['p90']} | {s['p99']} | {s['max']} "
            f"| {100*s['over512']:.2f}% | {100*s['over1024']:.2f}% "
            f"| {s['headerMean']:.1f} / {s['headerP50']} / {s['headerMax']} | {sha} | {hs} |"
        )
    print("\nSHA CONTROL", "PASS" if all_ok else "DIFFERS")
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
