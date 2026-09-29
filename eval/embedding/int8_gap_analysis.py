#!/usr/bin/env python3
"""Per-query gap analysis, jina-v2-base-code int8 vs fp32 (GM-422/S8).

Descriptive only: no gate, no verdict. The gated verdict is
int8_confirm_report.py's (GM-422/S9), which this script never runs.

Reads stored rankings (no embedding) of both sets:
- OLD: GM-398's runs and queries (authored held-out split by sha256 parity);
- NEW: the frozen GM-422 confirmatory set (every authored query held-out).
Per query it pairs fp32 and int8 and computes int8 - fp32 of
- the gold's score (its best-ranked expected node in each arm's top 100),
- the gold's rank (censored at 101 when outside the top 100),
- the top-1 score,
and whether the top-1 score crosses the frozen floor (GM-398's, per top
language: fp32 0.56/0.58/0.56/0.55, int8 0.57/0.57/0.55/0.53) downward or
upward. Deltas are broken down by set, language/corpus, target node kind,
embedded text length (jina tokens, special tokens included, as the harness's
token_shares; 1024 is the model's input cap), doc comment present, query
shape, NL vs mechanical, lexical-overlap stratum (check_queries.overlap).

Concentration: per stratum a sign test (exact binomial) of the score delta
and a bootstrap 95% CI of its median; per factor a permutation test (numpy,
fixed seed) of the between-stratum spread of mean deltas. p-values are
reported raw with the Bonferroni threshold over the tests printed.

Control, before anything else (exit 2 on mismatch): the S1 score-shift table
of docs/results/gm-422-int8-q5.md on the OLD set, computed with
q5_floor_sensitivity's own loaders and indicator (go n=16 median -0.0001
mean abs 0.0097, and the other three rows), and this script's pairing must
give the same multiset of shifts on that subset.

usage: int8_gap_analysis.py --old-runs DIR --old-eval-dir DIR --runs DIR \\
           --eval-dir DIR --snapshots DIR --tokenizer FILE [--json OUT]
"""
import json
import math
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import q5_floor_sensitivity as q5s  # noqa: E402
import int8_confirm_power as icp  # noqa: E402
from check_queries import overlap  # noqa: E402

import numpy as np  # noqa: E402

LANGS = q5s.LANGS
REF, CAND = q5s.REF, q5s.CAND
FROZEN = {REF: dict(zip(LANGS, icp.FROZEN[REF])), CAND: dict(zip(LANGS, icp.FROZEN[CAND]))}
CENSOR = 101
SEED = 4228
PERMS = 10000
BOOTS = 2000
CAP = 1024

# S1 (docs/results/gm-422-int8-q5.md, "How far quantization moves scores")
S1_TABLE = {"go": (16, -0.0001, -0.0146, +0.0294, 0.0097),
            "python": (20, -0.0039, -0.0269, +0.0128, 0.0086),
            "rust": (6, +0.0005, -0.0194, +0.0026, 0.0088),
            "typescript": (25, -0.0035, -0.0195, +0.0292, 0.0097)}


def arg(name):
    return sys.argv[sys.argv.index(name) + 1] if name in sys.argv else None


# --- control ---------------------------------------------------------------
def s1_shifts(ref, cand):
    """q5_floor_sensitivity.main's score-shift block, verbatim logic."""
    rf, _ = q5s.fit_floors(ref)
    cf, _ = q5s.fit_floors(cand)
    by_id = {o["id"]: o for o in ref}
    shifts = {}
    for o in cand:
        r = by_id[o["id"]]
        if q5s.fa_indicator(r, rf) is not None and q5s.fa_indicator(o, cf) is not None:
            shifts.setdefault(o["language"], []).append((o["id"], o["top"] - r["top"]))
    return rf, cf, shifts


def control(ref, cand, rows_old):
    rf, cf, shifts = s1_shifts(ref, cand)
    bad = []
    if rf != FROZEN[REF] or cf != FROZEN[CAND]:
        bad.append(f"refitted floors {rf} / {cf} != frozen {FROZEN}")
    print("| language | n | median | min | max | mean abs |\n|---|---|---|---|---|---|")
    for l in LANGS:
        s = sorted(d for _, d in shifts.get(l, []))
        got = (len(s), round(s[len(s) // 2], 4), round(s[0], 4), round(s[-1], 4),
               round(sum(map(abs, s)) / len(s), 4))
        print(f"| {l} | {got[0]} | {got[1]:+.4f} | {got[2]:+.4f} | {got[3]:+.4f} | {got[4]:.4f} |")
        want = S1_TABLE[l]
        if got[0] != want[0] or any(abs(a - b) > 5e-5 for a, b in zip(got[1:], want[1:])):
            bad.append(f"{l}: {got} != S1 {want}")
    # this script's pairing gives the same shifts on that subset
    mine = {r["id"]: r["d_top"] for r in rows_old}
    for l in LANGS:
        for qid, d in shifts.get(l, []):
            if qid not in mine or abs(mine[qid] - d) > 1e-12:
                bad.append(f"pairing mismatch on {qid}")
                break
    return bad


# --- loading ---------------------------------------------------------------
class Nodes:
    def __init__(self, snap_dir, tokenizer_path):
        from tokenizers import Tokenizer
        self.tok = Tokenizer.from_file(str(tokenizer_path))
        self.tok.no_padding()
        self.tok.no_truncation()
        self.snap_dir = snap_dir
        self.cache = {}

    def get(self, corpus, nid):
        key = (corpus, nid)
        if key not in self.cache:
            con = sqlite3.connect(f"file:{self.snap_dir / (corpus + '.sqlite')}?mode=ro", uri=True)
            row = con.execute("SELECT kind, nativeKind, docComment, signature FROM nodes WHERE id = ?",
                              (nid,)).fetchone()
            con.close()
            kind, native, doc, sig = row
            doc = (doc or "").strip()
            sig = (sig or "").strip()
            text = f"{doc}\n\n{sig}" if doc and sig else (doc or sig)  # pipeline.rs text_to_embed
            self.cache[key] = {"kind": kind, "native": native or "", "has_doc": bool(doc),
                               "chars": len(text), "tokens": len(self.tok.encode(text, add_special_tokens=True).ids)}
        return self.cache[key]


def raw_rankings(run):
    out = {}
    for d in sorted(p for p in run.iterdir() if (p / "rankings.jsonl").exists()):
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            out[r["id"]] = r
    return out


def tok_bucket(n):
    for hi, name in [(64, "<64"), (128, "64-127"), (256, "128-255"), (512, "256-511"), (CAP, "512-1023")]:
        if n < hi:
            return name
    return ">=1024 (truncated)"


def rows_of(set_name, runs, eval_dir, nodes, held_out_all):
    ref = q5s.load_arm(runs / REF, eval_dir)
    cand = q5s.load_arm(runs / CAND, eval_dir)
    raw_r, raw_c = raw_rankings(runs / REF), raw_rankings(runs / CAND)
    qs = {}
    for corpus in sorted({o["corpus"] for o in ref}):
        qs.update(q5s.load_queries(eval_dir, corpus))
    by_c = {o["id"]: o for o in cand}
    rows = []
    for r in ref:
        c = by_c[r["id"]]
        q = qs[r["id"]]
        row = {"set": set_name, "id": r["id"], "corpus": r["corpus"], "language": r["language"],
               "positive": r["positive"], "mechanical": r["mechanical"],
               "held_out": (not r["mechanical"]) if held_out_all else r["held_out"],
               "shape": q.get("shape", "?"), "text": r["text"], "gold": r["gold"],
               "top_f": r["top"], "top_i": c["top"], "d_top": c["top"] - r["top"],
               "tl_f": r["top_language"], "tl_i": c["top_language"]}
        if r["positive"]:
            row["overlap"] = "overlap" if overlap(q["text"], q["expected"]) else "no overlap"
            # the gold node: fp32's best-ranked expected node, else the first expected
            hits_f = raw_r[r["id"]]["hits"]
            gid = next((nid for nid, _ in hits_f if nid in raw_r[r["id"]]["expected"]),
                       raw_r[r["id"]]["expected"][0])
            n = nodes.get(r["corpus"], gid)
            row.update({"kind": f"{n['kind']}/{n['native']}", "has_doc": "doc" if n["has_doc"] else "signature only",
                        "chars": n["chars"], "tokens": n["tokens"], "tok_bucket": tok_bucket(n["tokens"]),
                        "rank_f": r["rank"], "rank_i": c["rank"],
                        "g_f": r["expected_score"], "g_i": c["expected_score"]})
            row["d_rank"] = (c["rank"] or CENSOR) - (r["rank"] or CENSOR)
            # margin: gold score minus the best non-gold score in the top 100 (> 0 = gold first)
            margins = []
            for raw in (raw_r[r["id"]], raw_c[r["id"]]):
                g = next((sc for nid, sc in raw["hits"] if nid in raw["expected"]), None)
                o = next((sc for nid, sc in raw["hits"] if nid not in raw["expected"]), None)
                margins.append(g - o if g is not None and o is not None else None)
            row["d_margin"] = margins[1] - margins[0] if None not in margins else None
            row["d_gold"] = (c["expected_score"] - r["expected_score"]
                             if r["expected_score"] is not None and c["expected_score"] is not None else None)
        # frozen-floor crossing of the top-1 score, each arm at its own floor (the operative setting)
        cf = r["top_language"] in FROZEN[REF] and r["top"] is not None and r["top"] >= FROZEN[REF][r["top_language"]]
        ci = c["top_language"] in FROZEN[CAND] and c["top"] is not None and c["top"] >= FROZEN[CAND][c["top_language"]]
        # same floor for both (fp32's): the score effect alone
        ci_same = c["top_language"] in FROZEN[REF] and c["top"] is not None and c["top"] >= FROZEN[REF][c["top_language"]]
        row.update({"clears_f": cf, "clears_i": ci, "clears_i_at_fp32": ci_same,
                    "dist_f": (r["top"] - FROZEN[REF][r["top_language"]]) if r["top_language"] in FROZEN[REF] else None})
        rows.append(row)
    return ref, cand, rows


# --- statistics --------------------------------------------------------------
def median(v):
    return float(np.median(v)) if len(v) else float("nan")


def sign_p(neg, pos):
    """Two-sided exact binomial (p = 1/2) on the non-zero signs."""
    n = neg + pos
    if n == 0:
        return 1.0
    k = min(neg, pos)
    p = 2 * sum(math.comb(n, i) for i in range(k + 1)) / 2 ** n
    return min(1.0, p)


def boot_median_ci(rng, v):
    v = np.asarray(v)
    if len(v) < 2:
        return (float("nan"), float("nan"))
    m = np.median(v[rng.integers(0, len(v), size=(BOOTS, len(v)))], axis=1)
    return (float(np.percentile(m, 2.5)), float(np.percentile(m, 97.5)))


def perm_p(rng, values, labels):
    """Permutation test of sum_g n_g (mean_g - mean)^2 (one-way between-group spread)."""
    v = np.asarray(values, dtype=float)
    groups = sorted(set(labels))
    if len(groups) < 2:
        return float("nan")
    codes = np.array([groups.index(x) for x in labels])
    k = len(groups)

    def stat(c):
        sums = np.bincount(c, weights=v, minlength=k)
        ns = np.bincount(c, minlength=k)
        return float(np.sum(sums ** 2 / np.maximum(ns, 1)))  # equivalent to the spread up to constants

    obs = stat(codes)
    perms = np.array([stat(rng.permutation(codes)) for _ in range(PERMS)])
    return float((1 + np.sum(perms >= obs - 1e-12)) / (PERMS + 1))


def spearman(x, y):
    x, y = np.asarray(x, float), np.asarray(y, float)
    rx = np.argsort(np.argsort(x)).astype(float)
    ry = np.argsort(np.argsort(y)).astype(float)
    return float(np.corrcoef(rx, ry)[0, 1])


FACTORS = [("set", "set"), ("language", "language"), ("corpus", "corpus"), ("kind", "target node kind"),
           ("tok_bucket", "embedded tokens (jina)"), ("has_doc", "embedded text"),
           ("shape", "query shape"), ("nlmech", "NL vs mechanical"), ("overlap", "lexical overlap")]


def breakdown(rng, rows, metric, tests):
    out = {}
    for key, label in FACTORS:
        vals = [(r[key], r[metric]) for r in rows if r.get(key) is not None and r.get(metric) is not None]
        if not vals:
            continue
        strata = {}
        for k, v in vals:
            strata.setdefault(k, []).append(v)
        p_het = perm_p(rng, [v for _, v in vals], [k for k, _ in vals])
        tests.append((f"{metric} heterogeneity by {label}", p_het))
        tab = []
        for k in sorted(strata, key=str):
            v = strata[k]
            neg, pos = sum(1 for x in v if x < 0), sum(1 for x in v if x > 0)
            p = sign_p(neg, pos)
            tests.append((f"{metric} sign {label}={k}", p))
            lo, hi = boot_median_ci(rng, v)
            tab.append({"stratum": k, "n": len(v), "median": median(v), "ci": (lo, hi),
                        "mean": float(np.mean(v)), "neg": neg, "pos": pos, "sign_p": p})
        out[label] = {"p_heterogeneity": p_het, "strata": tab}
    return out


def crossings(rows, pred):
    sel = [r for r in rows if pred(r)]
    down = sum(1 for r in sel if r["clears_f"] and not r["clears_i"])
    up = sum(1 for r in sel if not r["clears_f"] and r["clears_i"])
    down_s = sum(1 for r in sel if r["clears_f"] and not r["clears_i_at_fp32"])
    up_s = sum(1 for r in sel if not r["clears_f"] and r["clears_i_at_fp32"])
    return {"n": len(sel), "down": down, "up": up, "sign_p": sign_p(down, up),
            "down_at_fp32_floor": down_s, "up_at_fp32_floor": up_s, "sign_p_at_fp32_floor": sign_p(down_s, up_s)}


def fmt_s(x, d=4):
    return "-" if x is None or (isinstance(x, float) and math.isnan(x)) else f"{x:+.{d}f}"


def print_breakdown(title, b):
    print(f"\n### {title}")
    for label, t in b.items():
        print(f"\n{label} (permutation p = {t['p_heterogeneity']:.4f})\n")
        print("| stratum | n | median | 95% CI of median | mean | int8 lower / higher | sign p |")
        print("|---|---|---|---|---|---|---|")
        for s in t["strata"]:
            print(f"| {s['stratum']} | {s['n']} | {fmt_s(s['median'])} | [{fmt_s(s['ci'][0])}, {fmt_s(s['ci'][1])}] "
                  f"| {fmt_s(s['mean'])} | {s['neg']} / {s['pos']} | {s['sign_p']:.4f} |")


def main():
    old_runs, old_eval = Path(arg("--old-runs")), Path(arg("--old-eval-dir"))
    runs, eval_dir = Path(arg("--runs")), Path(arg("--eval-dir"))
    nodes = Nodes(Path(arg("--snapshots")), Path(arg("--tokenizer")))
    rng = np.random.default_rng(SEED)

    ref_old, cand_old, rows_old = rows_of("old", old_runs, old_eval, nodes, held_out_all=False)
    print("## 0. control: S1's score-shift table on the OLD set (paired held-out right-first)\n")
    bad = control(ref_old, cand_old, rows_old)
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("\nCONTROL OK: S1 table reproduced (n, median, min, max, mean abs; 4 dp), refitted floors = frozen,"
          " and this script's pairing gives the same shifts")

    _, _, rows_new = rows_of("new", runs, eval_dir, nodes, held_out_all=True)
    # the confirm runs carry GM-398's mechanical queries unchanged (same files, same vectors):
    # count them once, and check that both runs scored them identically
    old_mech = {r["id"]: r for r in rows_old if r["mechanical"]}
    new_mech = [r for r in rows_new if r["mechanical"]]
    diff = [r["id"] for r in new_mech if r["id"] not in old_mech
            or (r["top_f"], r["top_i"], r.get("rank_f"), r.get("rank_i"))
            != (old_mech[r["id"]]["top_f"], old_mech[r["id"]]["top_i"],
                old_mech[r["id"]].get("rank_f"), old_mech[r["id"]].get("rank_i"))]
    if diff or len(new_mech) != len(old_mech):
        print(f"STOP: mechanical queries differ between the sets: {diff[:5]} ({len(new_mech)} vs {len(old_mech)})")
        sys.exit(2)
    print(f"mechanical queries: {len(old_mech)} shared by both sets, scored identically; counted once (set 'mech')")
    for r in rows_old:
        if r["mechanical"]:
            r["set"] = "mech"
    rows = rows_old + [r for r in rows_new if not r["mechanical"]]
    for r in rows:
        r["nlmech"] = "mechanical" if r["mechanical"] else "NL (authored)"
    pos = [r for r in rows if r["positive"]]
    authored = [r for r in pos if not r["mechanical"]]
    tests, result = [], {"floors": FROZEN}

    # 1. overall shifts
    print("\n## 1. overall int8 - fp32 (positives)\n")
    print("| subset | n | gold score median | mean | lower/higher | sign p | top-1 median | rank worse/better/same | sign p | margin median | margin lower/higher | sign p |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for name, sel in [("old authored", [r for r in authored if r["set"] == "old"]),
                      ("new authored", [r for r in authored if r["set"] == "new"]),
                      ("mechanical (shared)", [r for r in pos if r["mechanical"]]),
                      ("all positives", pos)]:
        g = [r["d_gold"] for r in sel if r["d_gold"] is not None]
        t = [r["d_top"] for r in sel]
        gn, gp = sum(x < 0 for x in g), sum(x > 0 for x in g)
        rw = sum(r["d_rank"] > 0 for r in sel)
        rb = sum(r["d_rank"] < 0 for r in sel)
        mg = [r["d_margin"] for r in sel if r["d_margin"] is not None]
        mn, mp = sum(x < 0 for x in mg), sum(x > 0 for x in mg)
        print(f"| {name} | {len(sel)} | {fmt_s(median(g))} | {fmt_s(float(np.mean(g)))} | {gn}/{gp} | {sign_p(gn, gp):.4f} "
              f"| {fmt_s(median(t))} | {rw}/{rb}/{len(sel) - rw - rb} | {sign_p(rb, rw):.4f} "
              f"| {fmt_s(median(mg))} | {mn}/{mp} | {sign_p(mn, mp):.4f} |")
        tests.append((f"overall margin sign {name}", sign_p(mn, mp)))
        tests.append((f"overall gold sign {name}", sign_p(gn, gp)))
        tests.append((f"overall rank sign {name}", sign_p(rb, rw)))

    # 2. breakdowns
    print("\n## 2. breakdowns, authored + mechanical positives, both sets")
    result["gold"] = breakdown(rng, pos, "d_gold", tests)
    print_breakdown("gold score delta (int8 - fp32)", result["gold"])
    result["top"] = breakdown(rng, pos, "d_top", tests)
    print_breakdown("top-1 score delta (int8 - fp32)", result["top"])
    result["rank"] = breakdown(rng, pos, "d_rank", tests)
    print_breakdown("gold rank delta (int8 - fp32; >0 = int8 worse; censored at 101)", result["rank"])
    result["margin"] = breakdown(rng, pos, "d_margin", tests)
    print_breakdown("gold margin delta (int8 - fp32 of gold score - best non-gold score; <0 = int8 closer to losing)",
                    result["margin"])
    print("\n## 2b. authored (NL) positives only: does a factor hold without the mechanical queries?")
    result["gold_nl"] = breakdown(rng, authored, "d_gold", tests)
    print_breakdown("gold score delta, NL only", result["gold_nl"])
    result["margin_nl"] = breakdown(rng, authored, "d_margin", tests)
    print_breakdown("gold margin delta, NL only", result["margin_nl"])

    # 3. frozen-floor crossings of the top-1 score
    print("\n## 3. top-1 score crossing the frozen floor (fp32 at its floor vs int8 at its floor;"
          " and int8 at fp32's floor)\n")
    print("| subset | n | down (fp32 clears, int8 not) | up | sign p | down @fp32 floor | up @fp32 floor | sign p |")
    print("|---|---|---|---|---|---|---|---|")
    result["crossings"] = {}
    for st in ("old", "new"):
        for lang in LANGS + [None]:
            for name, pred in [
                    ("authored positives right-first in both",
                     lambda r: r["positive"] and not r["mechanical"] and r["rank_f"] == 1 and r["rank_i"] == 1),
                    ("authored positives", lambda r: r["positive"] and not r["mechanical"]),
                    ("absent", lambda r: not r["positive"])]:
                sel = [r for r in rows if r["set"] == st and (lang is None or r["language"] == lang) and r["held_out"]]
                c = crossings(sel, pred)
                label = f"{st} {lang or 'all'} {name}"
                result["crossings"][label] = c
                print(f"| {label} | {c['n']} | {c['down']} | {c['up']} | {c['sign_p']:.4f} | "
                      f"{c['down_at_fp32_floor']} | {c['up_at_fp32_floor']} | {c['sign_p_at_fp32_floor']:.4f} |")
                if lang is None:
                    tests.append((f"crossing {label}", c["sign_p"]))
    print("\n(held-out only: OLD by sha256 parity, NEW all authored. 'down' on right-first = a new false alarm;"
          " 'up' on absent = a new confident-wrong.)")

    # floor gap vs median shift per language (does per-model floor calibration absorb the shift?)
    print("\n### median top-1 shift of held-out right-first-in-both authored positives vs frozen floor gap\n")
    print("| set | language | n | median top-1 shift | int8 floor - fp32 floor | fp32 within 0.01 above its floor |")
    print("|---|---|---|---|---|---|")
    for st in ("old", "new"):
        for l in LANGS:
            sel = [r for r in authored if r["set"] == st and r["language"] == l and r["held_out"]
                   and r["rank_f"] == 1 and r["rank_i"] == 1]
            near = sum(1 for r in sel if r["dist_f"] is not None and 0 <= r["dist_f"] < 0.01)
            print(f"| {st} | {l} | {len(sel)} | {fmt_s(median([r['d_top'] for r in sel]))} | "
                  f"{FROZEN[CAND][l] - FROZEN[REF][l]:+.2f} | {near} |")

    # 4. length and rerank-recoverability
    print("\n## 4. length and recoverability\n")
    for st in ("old", "new", "mech", None):
        sel = [r for r in pos if (st is None or r["set"] == st) and r["d_gold"] is not None]
        print(f"Spearman(tokens, gold delta) {st or 'both'}: {spearman([r['tokens'] for r in sel], [r['d_gold'] for r in sel]):+.3f}"
              f" (n={len(sel)}); Spearman(tokens, |gold delta|): "
              f"{spearman([r['tokens'] for r in sel], [abs(r['d_gold']) for r in sel]):+.3f}")
    over = [r for r in pos if r["tokens"] >= CAP]
    print(f"gold texts at or over the {CAP}-token cap: {len(over)} of {len(pos)} positives "
          f"({', '.join(r['id'] for r in over) or 'none'})")
    lost1 = [r for r in authored if r["rank_f"] == 1 and (r["rank_i"] or CENSOR) > 1]
    won1 = [r for r in authored if r["rank_i"] == 1 and (r["rank_f"] or CENSOR) > 1]
    print(f"authored positives fp32 rank 1 -> int8 rank > 1: {len(lost1)}; the reverse: {len(won1)}")
    for k in (2, 3, 5, 10, 20):
        print(f"  lost rank 1 with int8 gold rank <= {k}: {sum(1 for r in lost1 if (r['rank_i'] or CENSOR) <= k)}")
    lost10 = [r for r in authored if (r["rank_f"] or CENSOR) <= 10 < (r["rank_i"] or CENSOR)]
    won10 = [r for r in authored if (r["rank_i"] or CENSOR) <= 10 < (r["rank_f"] or CENSOR)]
    print(f"authored positives leaving the top 10 under int8: {len(lost10)}; entering: {len(won10)}")

    # 5. largest losses
    print("\n## 5. largest losses (authored + mechanical positives)\n")
    print("| id | set | lang | shape | overlap | kind | tokens | gold fp32 -> int8 (rank) | Δ gold | Δ top-1 | text |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    worst = sorted((r for r in pos if r["d_gold"] is not None), key=lambda r: r["d_gold"])[:15]
    worst_rank = sorted(pos, key=lambda r: -r["d_rank"])[:10]
    for r in worst + [r for r in worst_rank if r not in worst]:
        print(f"| {r['id']} | {r['set']} | {r['language']} | {r['shape']} | {r['overlap']} | {r['kind']} | {r['tokens']} "
              f"| {fmt_s(r['g_f'], 3)} ({r['rank_f']}) -> {fmt_s(r['g_i'], 3)} ({r['rank_i']}) "
              f"| {fmt_s(r['d_gold'])} | {fmt_s(r['d_top'])} | {r['text'][:70]} |")

    # tests
    alpha = 0.05 / len(tests)
    sig = [(n, p) for n, p in tests if p < alpha]
    print(f"\n## 6. tests: {len(tests)} run; Bonferroni threshold {alpha:.2e}; significant after it: {len(sig)}")
    for n, p in sorted(sig, key=lambda x: x[1]):
        print(f"  {p:.2e}  {n}")
    print("  smallest raw p (not corrected):")
    for n, p in sorted(tests, key=lambda x: x[1])[:12]:
        print(f"  {p:.2e}  {n}")
    result["tests"] = tests
    result["rows"] = rows

    if arg("--json"):
        Path(arg("--json")).write_text(json.dumps(result, indent=1, default=str))


if __name__ == "__main__":
    main()
