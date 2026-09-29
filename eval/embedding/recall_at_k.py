#!/usr/bin/env python3
"""GM-443 S1: recall@K of stored embedding runs, for a recall-then-rerank stage.

Reads the rankings `g-mesh embed-eval run` already wrote (no new embedding) and
scores them with the same definitions `embed_eval.rs` uses (D5):

- a query's outcome is the 1-based rank of the first expected node id among the
  kept hits (`KEPT_HITS` = 100, so K cannot exceed 100);
- recall@K = 1 if that rank <= K, over scored positives (positive and not
  mechanical); pooled = mean of per-language means (languages weighted equally);
- MRR = mean of 1/rank, 0 outside the kept hits.

Before reporting anything it reproduces GM-398's pooled r@1/r@5/r@10/MRR and
per-language r@10 for every arm, parsed from docs/results/gm-398-model-comparison.md,
and exits non-zero on any mismatch at the table's precision.

Beyond GM-398 it reports the mechanical name queries (D6: positive, `shape`
"name") as a separate stratum, "name"; GM-398 never scores them.

Usage: recall_at_k.py --runs <eval/embedding/work/runs> [--eval-dir eval/embedding]
"""

import argparse
import hashlib
import json
import random
import re
import sys
from pathlib import Path

KEPT_HITS = 100
KS = [1, 5, 10, 20, 50, 100]
SMALL = ["gte-small", "bge-small-en-v1.5"]
REF = "jina-v2-base-code-fp32"
ARMS = SMALL + [REF]
GM398_ROW = {REF: "jina fp32 (R)", "gte-small": "gte-small", "bge-small-en-v1.5": "bge-small-en-v1.5"}
LANG_COLS = {"go": "go", "python": "python", "rust": "rust", "typescript": "ts"}
SEED, RESAMPLES = 398, 10000  # variants.toml's bootstrap settings


def load_queries(eval_dir, corpus):
    """queries.rs `load`: authored file, then mechanical; returns id -> query, file hashes."""
    out, hashes = {}, []
    for rel, mech in [(f"queries/{corpus}.jsonl", False), (f"queries/mechanical/{corpus}.jsonl", True)]:
        p = eval_dir / rel
        if not p.exists():
            if not mech:
                sys.exit(f"missing {p}")
            continue
        data = p.read_bytes()
        hashes.append([rel, hashlib.sha256(data).hexdigest()])
        for line in data.decode().splitlines():
            if line.strip():
                q = json.loads(line)
                q["mechanical"] = mech
                out[q["id"]] = q
    return out, hashes


def load_arm(eval_dir, run_dir):
    """Per query: dict(corpus, language, stratum, rank, hits, expected)."""
    rows = []
    manifests = {}
    for d in sorted(p for p in run_dir.iterdir() if (p / "rankings.jsonl").exists()):
        man = json.loads((d / "manifest.json").read_text())
        queries, hashes = load_queries(eval_dir, man["corpus"])
        if hashes != man["queryFiles"]:
            sys.exit(f"query files of {man['corpus']} changed since {d} was run (D3 freeze)")
        manifests[man["corpus"]] = man
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            q = queries[r["id"]]
            if q["kind"] != "positive":
                continue
            hits = [h[0] for h in r["hits"]]
            if len(hits) > KEPT_HITS:
                sys.exit(f"{d}: {r['id']} keeps {len(hits)} hits > {KEPT_HITS}")
            exp = set(r["expected"])
            rank = next((i + 1 for i, h in enumerate(hits) if h in exp), None)
            rows.append({
                "id": r["id"], "corpus": q["corpus"], "language": q["language"],
                "stratum": "name" if q["mechanical"] else "nl",
                "rank": rank, "hits": hits, "expected": exp, "nhits": len(hits),
            })
    return rows, manifests


def by_lang(rows, value):
    g = {}
    for r in rows:
        g.setdefault(r["language"], []).append(value(r))
    return g


def pooled(groups):
    # Language order as Rust's BTreeMap: float summation order decides ties at 3 decimals.
    means = [sum(v) / len(v) for _, v in sorted(groups.items()) if v]
    return sum(means) / len(means) if means else None


def hit(k):
    return lambda r: 1.0 if r["rank"] is not None and r["rank"] <= k else 0.0


def rr(r):
    return 1.0 / r["rank"] if r["rank"] is not None and r["rank"] <= KEPT_HITS else 0.0


def recall(rows, k):
    return pooled(by_lang(rows, hit(k)))


def paired_lower(a_rows, b_rows, fa, fb):
    """One-sided 95% lower bound of pooled(fa(a)) - pooled(fb(b)), paired by query id,
    bootstrap stratified by language (D5's method; Python RNG, so not bit-identical
    to the Rust bootstrap)."""
    b_by = {r["id"]: r for r in b_rows}
    groups = {}
    for r in a_rows:
        groups.setdefault(r["language"], []).append(fa(r) - fb(b_by[r["id"]]))
    point = pooled(groups)
    rng = random.Random(SEED)
    stats = []
    for _ in range(RESAMPLES):
        means = []
        for v in groups.values():
            n = len(v)
            means.append(sum(v[rng.randrange(n)] for _ in range(n)) / n)
        stats.append(sum(means) / len(means))
    stats.sort()
    return point, stats[int(0.05 * RESAMPLES)]


def parse_gm398(doc):
    """Pooled (r1, r5, r10, mrr) and per-language r@10 per arm, from the GM-398 tables."""
    pooled_rows, lang_rows, header = {}, {}, None
    for line in doc.read_text().splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip().strip("*") for c in line.strip("|").split("|")]
        if cells[0] == "arm":
            header = cells
            continue
        if set(cells[0]) <= set("-"):
            continue
        nums = [re.match(r"[0-9.]+", c) for c in cells[1:5]]
        if header and header[1] == "r@1" and all(nums):
            pooled_rows[cells[0]] = [float(m.group(0)) for m in nums]
        elif header and header[1] == "go" and re.fullmatch(r"[0-9.]+", cells[1]):
            lang_rows[cells[0]] = {h: float(c) for h, c in zip(header[1:], cells[1:]) if c}
    return pooled_rows, lang_rows


def fmt(x, nd=3):
    return "-" if x is None else f"{x:.{nd}f}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", required=True, type=Path)
    ap.add_argument("--eval-dir", type=Path, default=Path(__file__).resolve().parent)
    ap.add_argument("--gm398", type=Path,
                    default=Path(__file__).resolve().parents[2] / "docs/results/gm-398-model-comparison.md")
    a = ap.parse_args()

    arms, mans = {}, {}
    for name in ARMS:
        arms[name], mans[name] = load_arm(a.eval_dir, a.runs / name)
    # Same snapshot and node ids in every arm, so hit lists are comparable (union).
    for c in mans[REF]:
        for name in SMALL:
            for key in ("snapshotSha256", "nodeIdsSha256"):
                if mans[name][c][key] != mans[REF][c][key]:
                    sys.exit(f"{name}/{c} {key} differs from {REF}")
    ids = {name: sorted(r["id"] for r in rows) for name, rows in arms.items()}
    assert ids[SMALL[0]] == ids[SMALL[1]] == ids[REF], "arms score different queries"

    # --- control: reproduce GM-398 ---
    g_pooled, g_lang = parse_gm398(a.gm398)
    print("## Control: reproduce GM-398 (NL = scored positives)\n")
    ok = True
    for name in ARMS:
        nl = [r for r in arms[name] if r["stratum"] == "nl"]
        mine = [recall(nl, 1), recall(nl, 5), recall(nl, 10), pooled(by_lang(nl, rr))]
        want = g_pooled[GM398_ROW[name]]
        match = all(round(m, 3) == w for m, w in zip(mine, want))
        lang_mine = {LANG_COLS[l]: sum(v) / len(v) for l, v in by_lang(nl, hit(10)).items()}
        want_l = g_lang[GM398_ROW[name]]
        match_l = all(round(lang_mine[l], 2) == want_l[l] for l in lang_mine)
        ok &= match and match_l
        print(f"- {name}: r@1/r@5/r@10/MRR {' / '.join(fmt(x) for x in mine)} vs GM-398 "
              f"{' / '.join(f'{x:.3f}' for x in want)}; per-language r@10 "
              f"{', '.join(f'{l} {v:.2f}' for l, v in sorted(lang_mine.items()))}: "
              f"{'MATCH' if match and match_l else 'MISMATCH'}")
    if not ok:
        sys.exit("\nGM-398 not reproduced; stopping")
    print()

    nhits = {r["nhits"] for rows in arms.values() for r in rows}
    print(f"Kept hits per query: min {min(nhits)}, max {max(nhits)} (KEPT_HITS {KEPT_HITS}); max K = {min(nhits)}\n")

    langs = sorted({r["language"] for r in arms[REF]})
    for stratum, label in [("nl", "NL (authored; GM-398's scored set)"), ("name", "name (mechanical, positives)")]:
        n = sum(1 for r in arms[REF] if r["stratum"] == stratum)
        nl_by = {l: sum(1 for r in arms[REF] if r["stratum"] == stratum and r["language"] == l) for l in langs}
        print(f"## recall@K, {label}, n={n} ({', '.join(f'{l} {c}' for l, c in nl_by.items())})\n")
        print("| arm | scope | " + " | ".join(f"@{k}" for k in KS) + " |")
        print("|---|---|" + "---|" * len(KS))
        for name in ARMS:
            rows = [r for r in arms[name] if r["stratum"] == stratum]
            print(f"| {name} | pooled | " + " | ".join(fmt(recall(rows, k)) for k in KS) + " |")
            for l in langs:
                lr = [r for r in rows if r["language"] == l]
                print(f"| {name} | {l} | " + " | ".join(fmt(sum(map(hit(k), lr)) / len(lr)) for k in KS) + " |")
        print()

    # --- gate ---
    nl = {name: [r for r in arms[name] if r["stratum"] == "nl"] for name in ARMS}
    ref10 = recall(nl[REF], 10)
    print(f"## Gate: small recall@50 >= jina fp32 recall@10 ({ref10:.3f}), NL pooled\n")
    for name in SMALL:
        r50 = recall(nl[name], 50)
        first = next((k for k in range(1, KEPT_HITS + 1) if recall(nl[name], k) >= ref10), None)
        d, lo = paired_lower(nl[name], nl[REF], hit(50), hit(10))
        print(f"- {name}: r@50 {r50:.3f}, delta vs jina r@10 {d:+.3f} (one-sided 95% lower {lo:+.3f}); "
              f"first K reaching {ref10:.3f}: {first}; {'GO' if r50 >= ref10 else 'NO-GO'}")
    # Per language: does each language's r@50 reach jina's r@10 in that language?
    for name in SMALL:
        cells = []
        for l in langs:
            s = [r for r in nl[name] if r["language"] == l]
            j = [r for r in nl[REF] if r["language"] == l]
            cells.append(f"{l} {sum(map(hit(50), s)) / len(s):.3f} vs {sum(map(hit(10), j)) / len(j):.3f}")
        print(f"  - {name} per language r@50 vs jina r@10: {'; '.join(cells)}")
    print()

    # --- union aside ---
    print("## Aside: union of gte and bge top-K (candidate count up to 2K)\n")
    g_by = {r["id"]: r for r in arms["gte-small"]}
    b_by = {r["id"]: r for r in arms["bge-small-en-v1.5"]}
    for stratum in ("nl", "name"):
        for k in (10, 20, 50):
            groups, sizes = {}, []
            for r in arms[REF]:
                if r["stratum"] != stratum:
                    continue
                u = set(g_by[r["id"]]["hits"][:k]) | set(b_by[r["id"]]["hits"][:k])
                sizes.append(len(u))
                groups.setdefault(r["language"], []).append(1.0 if u & r["expected"] else 0.0)
            print(f"- {stratum} union top-{k}: recall {pooled(groups):.3f}, "
                  f"mean candidates {sum(sizes) / len(sizes):.1f}")
    print()

    # --- ceiling for S2 ---
    print("## Ceiling for S2: a perfect reranker over the small model's top-K\n")
    print("A reranker only reorders the candidate set, so its recall@10 (and recall@1) is at most the set's recall@K.")
    print("Headroom = ceiling - jina r@10; also the current small r@10 for contrast.\n")
    print("| arm | K | ceiling (NL) | vs jina r@10 | small r@10 today | ceiling (name) |")
    print("|---|---|---|---|---|---|")
    for name in SMALL:
        nm = [r for r in arms[name] if r["stratum"] == "name"]
        for k in (10, 20, 50, 100):
            c = recall(nl[name], k)
            print(f"| {name} | {k} | {c:.3f} | {c - ref10:+.3f} | {recall(nl[name], 10):.3f} | {recall(nm, k):.3f} |")
    # Queries no candidate set can fix: expected outside every arm's top-100.
    miss = [r["id"] for r in nl[REF]
            if r["rank"] is None and g_by[r["id"]]["rank"] is None and b_by[r["id"]]["rank"] is None]
    only_jina = sum(1 for r in nl[REF] if r["rank"] is not None and r["rank"] <= 10
                    and not (g_by[r["id"]]["rank"] or 999) <= 50)
    print(f"\nNL queries outside all three arms' top-100: {len(miss)} of {len(nl[REF])}")
    print(f"NL queries jina ranks in its top 10 that gte's top 50 misses: {only_jina}")


if __name__ == "__main__":
    main()
