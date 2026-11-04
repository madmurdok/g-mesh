#!/usr/bin/env python3
"""GM-462/S1 design probe: ms-marco-MiniLM token lengths of the enriched
cross-encoder input fields, on the real rerank candidates.

Candidates: the union of int8-structured's top-30 (runs-gm465, the shipped
F4 window) and int8 full-text's top-50 (GM-443's K=50 setup), every query.
Reads stored rankings, the snapshot DBs, the corpus checkouts and the
tokenizer only; nothing is scored. The field rules are the design note's
draft (docs/design/GM-462-richer-reranker-input.md), not the final builder.

usage: gm462_token_lengths.py <main checkout>/eval/embedding/work
"""
import json
import re
import sqlite3
import statistics
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402
from gm423_token_lengths import form_text  # noqa: E402
from tokenizers import Tokenizer  # noqa: E402

WORK = Path(sys.argv[1])
TOK = Tokenizer.from_file(str(WORK / "models/ms-marco-MiniLM-L6-v2/tokenizer.json"))
CORPORA = ["excalidraw", "g-mesh", "gin", "py-requests", "ripgrep", "task-tracker-mcp"]
WORD = re.compile(r"[A-Z]+(?=[A-Z][a-z])|[A-Z]?[a-z]+|[A-Z]+|[0-9]+")
KW = {"pub", "crate", "super", "in", "fn", "mod", "const", "static", "struct", "enum", "func", "def", "async",
      "let", "export", "self", "mut", "var", "type", "impl", "unsafe", "extern"}
PUNCT = re.compile(r"^[\s{}()\[\];,]*$")
FIELDS = ("base", "E", "I", "P", "B5", "B10", "B48", "B96")


def nt(s):
    return len(TOK.encode(s, add_special_tokens=False).ids) if s else 0


def words(s):
    return " ".join(w.lower() for w in WORD.findall(s or ""))


def sig_idents(sig, name):
    seen, out = {name}, []
    for m in re.finditer(r"[A-Za-z_][A-Za-z0-9_]*", sig or ""):
        w = m.group(0)
        if w.lower() not in KW and w not in seen:
            seen.add(w)
            out.append(w)
    return out


def body_lines(src, start, end, lang):
    """Source lines after the declaration's first line (startLine is 0-based),
    a leading Python docstring skipped, punctuation-only lines dropped,
    whitespace collapsed, each line cut at 200 chars."""
    ls, i = src[start + 1:end + 1], 0
    if lang == "python":
        while i < len(ls) and not ls[i].strip():
            i += 1
        head = ls[i].strip().lstrip("rRbBuU") if i < len(ls) else ""
        if head[:3] in ('"""', "'''"):
            q = head[:3]
            i += 1
            if q not in head[3:]:
                while i < len(ls) and q not in ls[i]:
                    i += 1
                i += 1
    return [" ".join(l.split())[:200] for l in ls[i:] if not PUNCT.match(l)]


def capped(lines, budget):
    tot = 0
    for l in lines:
        n = nt(l) + (1 if tot else 0)
        if tot + n > budget:
            break
        tot += n
    return tot


def stat(v):
    v = sorted(v)
    n = len(v)
    p = lambda q: v[min(n - 1, int(q * n))]
    return f"n={n} p50={p(.5)} p90={p(.9)} p99={p(.99)} max={v[-1]} mean={statistics.mean(v):.1f}"


def main():
    recs, qlens = [], []
    for cn in CORPORA:
        qs, _ = R.load_queries(HERE, cn)
        qlens += [nt(q["text"]) for q in qs.values()]
        cand = {}
        for arm, sub, k in (("st30", "runs-gm465/jina-v2-base-code-int8-structured", 30),
                            ("i50", "runs/jina-v2-base-code-int8", 50)):
            for line in (WORK / sub / cn / "rankings.jsonl").read_text().splitlines():
                for h in json.loads(line)["hits"][:k]:
                    cand.setdefault(h[0], set()).add(arm)
        con = sqlite3.connect(f"file:{WORK / (cn + '.sqlite')}?mode=ro", uri=True)
        srcs = {}
        for nid, kind, nk, name, qn, fp, sl, el, sig, doc, lang in con.execute(
                "SELECT id, kind, nativeKind, name, qualifiedName, filePath, startLine, endLine, signature, "
                "docComment, language FROM nodes"):
            if nid not in cand:
                continue
            base = form_text("structured", doc, sig, R.text_to_embed(doc, sig))
            prefix = qn[:-len(name)] if qn.endswith(name) else qn
            if fp not in srcs:
                p = WORK / "corpora" / cn / fp
                srcs[fp] = p.read_text(errors="replace").split("\n") if p.exists() else None
            B = body_lines(srcs[fp], sl, el, lang) if srcs[fp] and kind in ("Function", "Type") else []
            recs.append(dict(
                arms=cand[nid], lang=lang, kind=kind, missing=srcs[fp] is None,
                documented=bool(doc and form_text("structured", doc, None, None)),
                base=nt(base), E=nt(" ".join(x for x in (words(nk or kind), words(prefix)) if x)),
                I=nt(" ".join([words(name)] + [words(w) for w in sig_idents(sig, name)])), P=nt(words(fp)),
                B5=nt("\n".join(B[:5])), B10=nt("\n".join(B[:10])), B48=capped(B, 48), B96=capped(B, 96)))
        con.close()
    print("query tokens:", stat(qlens))
    print("candidate nodes:", len(recs), "source file missing:", sum(r["missing"] for r in recs))
    qmax = max(qlens)
    for arm in ("st30", "i50"):
        rs = [r for r in recs if arm in r["arms"]]
        print(f"\n== {arm}: {len(rs)} nodes, documented {sum(r['documented'] for r in rs)}, "
              f"kinds {sorted((k, sum(r['kind'] == k for r in rs)) for k in {r['kind'] for r in rs})}")
        for f in FIELDS:
            print(f"  {f:10s}", stat([r[f] for r in rs]))
        hdr = lambda r: r["E"] + r["I"] + r["P"] + 3
        for lab, fn in (("header", hdr),
                        ("all-on B48", lambda r: hdr(r) + r["base"] + r["B48"] + 1),
                        ("all-on B96", lambda r: hdr(r) + r["base"] + r["B96"] + 1)):
            print(f"  {lab:10s}", stat([fn(r) for r in rs]),
                  f"| pairs > 512 with the longest query: {sum(fn(r) + qmax + 3 > 512 for r in rs)}")
        print(f"  {'base':10s} pairs > 512 with the longest query: {sum(r['base'] + qmax + 3 > 512 for r in rs)}")
        for lang in R.LANGS:
            s = [r for r in rs if r["lang"] == lang]
            print(f"  {lang}: n={len(s)} signature-only {sum(not r['documented'] for r in s)}")


if __name__ == "__main__":
    main()
