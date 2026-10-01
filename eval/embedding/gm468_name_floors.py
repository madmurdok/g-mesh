#!/usr/bin/env python3
"""GM-468: name-query floors for find_definition's semantic-neighbours rung.

`find_definition::by_semantic_neighbours` embeds a *symbol name*, takes the top
SEMANTIC_CANDIDATES (3) hits of `search_code::search`, keeps the hits scoring
at or above `similarity::floor(language)`, and refuses (falls through to the
terse "not found") when none is left. The floors it reads were fitted on
free-text queries for `search_code`; this script re-judges them on the name
arm of the GM-398 query set, from STORED rankings (no re-embedding).

Per language, over the mechanical `shape == "name"` queries:

- page positive: a positive query whose top-3 holds an expected node (the
  page the rung would show holds the right answer). `--strict` counts only
  pages whose rank-1 hit is expected.
- false refusal: a page positive whose top-1 score is below the floor, so the
  rung returns nothing although the right answer was on its page. The
  expensive error (GM-234 existed to stop refusals).
- answer dropped: a page positive where no expected hit in the top 3 reaches
  the floor (a superset of false refusals: the rung may still offer others).
- hopeless offer: an absent query (its name exists nowhere in the corpus)
  whose top-1 reaches the floor, so the rung offers labelled "did you mean"
  candidates. The cheap error.

The 3% floor is the largest floor whose false-refusal rate is <= 3%: with n
page positives and k = floor(0.03 n), it is the (k+1)-th smallest top-1 score
(a floor equal to it leaves exactly k scores strictly below).

Cross-check: `--run <runs/jina-v2-base-code-fp32> --corpora gin py-requests
ripgrep excalidraw task-tracker-mcp` is GM-381's corpus set on the fp32 model.
It does NOT reproduce the fp32 numbers quoted above by_semantic_neighbours
(0.648 / 0.628 / 0.555 / 0.538 on 149/145/143/291 positives): GM-381's query
set is not GM-398's (see docs/results/gm-468-name-query-floors.md).

Usage:
  python3 eval/embedding/gm468_name_floors.py --run <run dir> [--corpora ...] [--strict] [--json out]
"""
import argparse
import hashlib
import json
import math
from collections import defaultdict
from pathlib import Path

EVAL = Path(__file__).resolve().parent
TOP = 3  # find_definition.rs::SEMANTIC_CANDIDATES (read, not remembered)
BUDGET = 0.03  # the find_definition comment's "false refusals at or under 3%"
LANGS = ("go", "python", "rust", "typescript")
# core/src/mcp/similarity.rs::floor on release-3.18.0 (read, not remembered).
SHIPPED = {"go": 0.57, "python": 0.59, "rust": 0.57, "typescript": 0.53}
# The fp32 name-query floors quoted above by_semantic_neighbours.
FP32_NAME = {"go": 0.648, "python": 0.628, "rust": 0.555, "typescript": 0.538}


def load_queries(corpus):
    p = EVAL / "queries" / "mechanical" / f"{corpus}.jsonl"
    return {q["id"]: q for q in (json.loads(l) for l in p.read_text().splitlines() if l.strip())}


def collect(run, corpora, strict):
    pos, neg = defaultdict(list), defaultdict(list)  # lang -> [(top1, [expected-hit scores in top3])], [top1]
    for corpus in corpora:
        d = Path(run) / corpus
        man = json.loads((d / "manifest.json").read_text())
        for rel, sha in man["queryFiles"]:
            got = hashlib.sha256((EVAL / rel).read_bytes()).hexdigest()
            if got != sha:
                raise SystemExit(f"{rel} changed since the run ({got} != {sha})")
        queries = load_queries(corpus)
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            q = queries.get(r["id"])
            if q is None or q.get("shape") != "name":
                continue
            hits = r["hits"][:TOP]
            scores = [s for _, s in hits]
            assert scores == sorted(scores, reverse=True), r["id"]
            if q["kind"] == "absent":
                neg[q["language"]].append(scores[0])
                continue
            want = set(r["expected"])
            exp = [s for n, s in hits if n in want]
            on_page = (hits[0][0] in want) if strict else bool(exp)
            if on_page:
                pos[q["language"]].append((scores[0], exp))
    return pos, neg


def rates(p, n, f):
    fr = sum(t < f for t, _ in p)
    dropped = sum(not any(s >= f for s in e) for _, e in p)
    offer = sum(t >= f for t in n)
    return fr, dropped, offer


def wilson_hi(k, n, z=1.96):
    if n == 0:
        return float("nan")
    ph = k / n
    c = (ph + z * z / (2 * n) + z * math.sqrt(ph * (1 - ph) / n + z * z / (4 * n * n))) / (1 + z * z / n)
    return c


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--corpora", nargs="+",
                    default=["gin", "py-requests", "ripgrep", "g-mesh", "excalidraw", "task-tracker-mcp"])
    ap.add_argument("--strict", action="store_true", help="page positive = rank-1 hit expected")
    ap.add_argument("--json")
    a = ap.parse_args()
    pos, neg = collect(a.run, a.corpora, a.strict)

    out = {"run": a.run, "corpora": a.corpora, "strict": a.strict, "languages": {}}
    pct = lambda k, n: f"{100 * k / n:.1f}%" if n else "-"
    print(f"run {a.run}  corpora {' '.join(a.corpora)}  {'strict (rank 1)' if a.strict else 'page (top 3)'}")
    print("| lang | pos | neg | 3% floor (exact) | 2dp down | FR@2dp | drop@2dp | offer@2dp |"
          " shipped | FR@shipped (k/n, 95% hi) | drop@shipped | offer@shipped | FR@fp32-name | offer@fp32-name |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for lang in LANGS:
        p, n = pos.get(lang, []), neg.get(lang, [])
        if not p:
            continue
        tops = sorted(t for t, _ in p)
        k = math.floor(BUDGET * len(p))
        exact = tops[k]
        down = math.floor(exact * 100) / 100
        row = {"positives": len(p), "negatives": len(n), "floor3pct": exact, "floor3pct2dp": down}
        cells = [lang, str(len(p)), str(len(n)), f"{exact:.3f}", f"{down:.2f}"]
        fr, dr, of = rates(p, n, down)
        cells += [pct(fr, len(p)), pct(dr, len(p)), pct(of, len(n))]
        row["at2dp"] = {"falseRefusal": fr, "answerDropped": dr, "hopelessOffer": of}
        s = SHIPPED[lang]
        fr, dr, of = rates(p, n, s)
        cells += [f"{s:.2f}", f"{pct(fr, len(p))} ({fr}/{len(p)}, <={100 * wilson_hi(fr, len(p)):.1f}%)",
                  pct(dr, len(p)), pct(of, len(n))]
        row["atShipped"] = {"floor": s, "falseRefusal": fr, "answerDropped": dr, "hopelessOffer": of}
        fr, dr, of = rates(p, n, FP32_NAME[lang])
        cells += [pct(fr, len(p)), pct(of, len(n))]
        row["atFp32Name"] = {"floor": FP32_NAME[lang], "falseRefusal": fr, "answerDropped": dr, "hopelessOffer": of}
        # the curve, for the doc: every 0.01 from 0.45 to 0.70
        row["curve"] = [{"floor": f / 100, **dict(zip(("falseRefusal", "answerDropped", "hopelessOffer"),
                                                       rates(p, n, f / 100)))} for f in range(45, 71)]
        out["languages"][lang] = row
        print("| " + " | ".join(cells) + " |")
    if a.json:
        Path(a.json).write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
