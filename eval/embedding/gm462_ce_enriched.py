#!/usr/bin/env python3
"""GM-462: F4 with the cross-encoder fed an enriched text (gm462_ce_text.build).

Setup A (the shipped F4): int8-structured's top-K (K=30) reordered by
ce(text) + beta * cos, verdict = int8-structured's own top-1 cosine at the
shipped floors. Only the CE text changes between arms:

  off    base text only (= F4 today; its scores come from the gm464 cache)
  all    E I P B, every query
  -E -I -P -B    leave-one-out, authored NL queries only
  all-b48        E I P B with a 48-token body, authored NL only (on request)

Outside an arm's query scope (name queries for the authored-NL-only arms) the
arm keeps the off order. Every arm is reported at beta 80 and at its
FIT-tuned beta. The design (fields, arms, controls, verdict rule) is
docs/design/GM-462-richer-reranker-input.md.

Scores are cached per (query, sha16(text)) in
<cache-dir>/gm462-ce-minilm.<corpus>.<key>.json; a lookup falls back to the
gm464 cache file, which this script only reads.

Controls (any failure is a STOP):
  C1  build(all off) == gm464's structured text, every window node;
  C2  the gm464 cache holds every (query, window node) off pair;
  C3  200 off pairs rescored through score_texts agree with the cache (<= 1e-5);
  C4  the off row reproduces the published F4 row (rendered lines compared):
      on the current set GM-464 S1's row; on another eval dir (v2) GM-460's
      F4 K=30 row, and the off arm restricted to the GM-398 query ids
      reproduces GM-464 S1's row;
  C5  rerank disabled == int8-structured on every outcome and cell;
  C6  the all arm's text differs from base on every window node, and its row
      differs from the off row.

Progress of the long stages goes to ~/.claude/progress/GM-462-measure.txt.

usage: gm462_ce_enriched.py --work <eval/embedding/work> [--eval-dir eval/embedding]
       [--st-runs runs-gm465/jina-v2-base-code-int8-structured] [--k 30]
       [--arms off,all,-E,-I,-P,-B] [--body-budget 96] [--dry-run CORPUS]
       [--table out.md] [--summary summary.txt]
"""
import argparse
import hashlib
import json
import os
import random
import re
import sqlite3
import statistics
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402
import gm462_ce_text as T  # noqa: E402
from gm423_token_lengths import form_text, structured_doc  # noqa: E402
from gm464_ce_structured import c1_fixture, flips, render  # noqa: E402
from rerank_q5_diag import fixed_floors_gates  # noqa: E402

NAME = "ce-minilm"
ST = "jina-v2-base-code-int8-structured"
SHIPPED_BETA = 80.0
PROGRESS = Path.home() / ".claude/progress/GM-462-measure.txt"
C3_PAIRS, C3_MAX_DIFF = 200, 1e-5
C4_DOC = HERE.parents[1] / "docs/results/gm-443-recall-rerank.md"
C4_SECTION = "## GM-464 S1: F4 K=30 on the structured text"
C4_LABEL = "F4s K=30 beta=80 (FIT-tuned), CE on structured text"
V2_C4_DOC = HERE.parents[1] / "docs/results/gm-460-enlarged-eval.md"
V2_C4_LABEL = re.compile(r"^\| (F4s K=30 beta=80(?: \(FIT-tuned\))?, CE on structured text) \|")
LOGIT_LOW = -8.0
ARMS = {  # name -> (switches, body budget or None for --body-budget, authored NL only)
    "off": (frozenset(), None, False),
    "all": (frozenset("EIPB"), None, False),
    "-E": (frozenset("IPB"), None, True),
    "-I": (frozenset("EPB"), None, True),
    "-P": (frozenset("EIB"), None, True),
    "-B": (frozenset("EIP"), None, True),
    "all-b48": (frozenset("EIPB"), 48, True),
}
DEFAULT_ARMS = "off,all,-E,-I,-P,-B"
LOO_ARMS = ("-E", "-I", "-P", "-B")
NODE_COLS = ("id", "kind", "nativeKind", "name", "qualifiedName", "filePath", "startLine", "endLine",
             "signature", "docComment", "language")


class Progress:
    """One overwritten line per stage: at start, at every 10% of `total`, at
    least every 5 minutes, and at the end. `on_write` runs at every write."""

    def __init__(self, stage, total, on_write=None):
        self.stage, self.total, self.on_write = stage, total, on_write
        self.t0 = self.last = time.time()
        self.step = max(1, total // 10)
        self.next = self.step
        self.first_eta = None
        self.write(0)

    def write(self, done):
        el = time.time() - self.t0
        pct = 100.0 * done / self.total if self.total else 100.0
        eta = el / done * (self.total - done) if done else float("nan")
        if done and self.first_eta is None and done >= self.step:
            self.first_eta = el + eta
        la = " ".join(f"{x:.2f}" for x in os.getloadavg())
        PROGRESS.parent.mkdir(parents=True, exist_ok=True)
        PROGRESS.write_text(f"GM-462 {self.stage}: {done}/{self.total} ({pct:.0f}%) elapsed {el:.0f}s "
                            f"eta {eta:.0f}s load {la}\n")
        if self.on_write:
            self.on_write()
        self.last = time.time()

    def tick(self, done):
        if done >= self.next or time.time() - self.last > 300:
            self.write(done)
            while self.next <= done:
                self.next += self.step


def tsha(t):
    return hashlib.sha256(t.encode()).hexdigest()[:16]


# ---- copied from gm464_ce_structured.py (add_structured with the runs root as a
# parameter; main()'s nested f4, row, sig and fit_obj at module level) ----------
def add_structured(c, st_dir):
    """int8-structured lists and the structured text of every node."""
    d = st_dir / c.name
    man = json.loads((d / "manifest.json").read_text())
    for key, want in (("snapshotSha256", c.snapshot_sha), ("nodeIdsSha256", c.node_ids_sha), ("queryIds", c.all_qids)):
        if man[key] != want:
            sys.exit(f"STOP: {ST}/{c.name} {key} differs from the fp32 run")
    lst = {}
    for line in (d / "rankings.jsonl").read_text().splitlines():
        r = json.loads(line)
        if set(r["expected"]) != c.expected[r["id"]]:
            sys.exit(f"STOP: {ST} {r['id']} expected set differs")
        lst[r["id"]] = [(h[0], h[1]) for h in r["hits"]]
    c.lists[ST] = lst
    con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
    c.stext = {}
    for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes ORDER BY id"):
        if nid in c.text:
            c.stext[nid] = form_text("structured", doc, sig, c.text[nid])
    con.close()


def f4(cs, k, beta, score_of=None, rerank=True):
    """ST's top-k in the order score_of(c, q, nid) + beta * cos; verdict on ST's own top-1 cosine."""
    outs = []
    for c in cs:
        for q in c.qids:
            lst = c.lists[ST][q]
            n = min(k, len(lst))
            if rerank:
                s = np.array([score_of(c, q, nid) for nid, _ in lst[:n]]) + beta * np.array([x for _, x in lst[:n]])
            else:
                s = -np.arange(n, dtype=np.float64)
            hits, _ = R.reranked(c, q, k, s, ST)
            o = R.outcome(c, q, hits, lst[0][1] if lst else None)
            o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
            outs.append(o)
    return outs


def row(label, outs, cf, base, rf32, ref8, rf8):
    s = R.summarize(outs)
    return {"label": label, "floors": cf,
            "r10": R.bootstrap(R.pooled(outs, R.hit10, R.ho)[1]), "mrr": R.bootstrap(R.pooled(outs, R.rr, R.ho)[1]),
            "name": (s["name"]["r10"], s["name"]["mrr"]),
            "vs": {"fp32": fixed_floors_gates(base, outs, rf32, cf),
                   "int8": fixed_floors_gates(ref8, outs, rf8, cf)},
            "gm434": {k_: R.gm434(outs, cf, f) for k_, f in (("nl", R.nl_ho_all), ("name", R.name_all))},
            "gm434_lang": {l: R.gm434(outs, cf, R.nl_ho_all, l) for l in R.LANGS}}


def sig(r):
    return json.dumps({k_: r[k_] for k_ in ("r10", "mrr", "name", "gm434", "gm434_lang")}, default=str) + json.dumps(
        {t: {x: g[x] for x in ("r10", "mrr", "cw", "fa", "fa_lang", "pass")} for t, g in r["vs"].items()}, default=str)


def fit_obj(fit_keys, k, score_of, b):
    g10, gm = {}, {}
    for c, q in fit_keys:
        lst = c.lists[ST][q]
        n = min(k, len(lst))
        s = np.array([score_of(c, q, nid) for nid, _ in lst[:n]]) + b * np.array([x for _, x in lst[:n]])
        hits, _ = R.reranked(c, q, k, s, ST)
        r = R.first_rank(c, q, hits)
        lang = c.queries[q]["language"]
        g10.setdefault(lang, []).append(1.0 if r and r <= 10 else 0.0)
        gm.setdefault(lang, []).append(1.0 / r if r else 0.0)
    return round(R.pooled_mean(g10), 9), round(R.pooled_mean(gm), 9)
# ---- end of the gm464 copies --------------------------------------------------


class EnrichedCache:
    """Logits per corpus, {qid: {sha16(text): logit}}. Lookups try this script's
    file first, then the gm464 file; only this script's file is ever written."""

    def __init__(self, cache_dir, cs, model_sha):
        self.dir = cache_dir
        self.key = hashlib.sha256(json.dumps([model_sha, R.MAX_TOKENS, "gm462 text-keyed v1"]).encode()).hexdigest()[:16]
        # gm464_ce_structured.Cache's key formula.
        self.base_key = hashlib.sha256(json.dumps([model_sha, R.MAX_TOKENS, "gm464 text-keyed v1"]).encode()).hexdigest()[:16]
        self.d, self.base = {}, {}
        for c in cs:
            f, b = self.file(c), self.base_file(c)
            self.d[c.name] = json.loads(f.read_text()) if f.exists() else {}
            self.base[c.name] = json.loads(b.read_text()) if b.exists() else {}

    def file(self, c):
        return self.dir / f"gm462-{NAME}.{c.name}.{self.key}.json"

    def base_file(self, c):
        return self.dir / f"gm464-{NAME}.{c.name}.{self.base_key}.json"

    def has(self, c, q, h):
        return h in self.d[c.name].get(q, {}) or h in self.base[c.name].get(q, {})

    def get(self, c, q, text):
        h = tsha(text)
        own = self.d[c.name].get(q, {})
        return own[h] if h in own else self.base[c.name][q][h]

    def put(self, cn, q, hs, scores):
        self.d[cn].setdefault(q, {}).update(zip(hs, map(float, scores)))

    def save(self, c):
        tmp = self.file(c).with_suffix(".tmp")
        tmp.write_text(json.dumps(self.d[c.name]))
        tmp.replace(self.file(c))


def score_texts(ce, query, texts):
    """The one scoring path: every new score and C3's rescoring go through here."""
    return ce.score(query, texts)


def load_nodes(c, corpora_dir, k):
    """Eval DB rows and source lines of every node in a top-k window."""
    win = {nid for q in c.all_qids for nid, _ in c.lists[ST][q][:k]}
    con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
    c.rows, c.trimmed_doc = {}, {}
    for r in con.execute(f"SELECT {', '.join(NODE_COLS)} FROM nodes"):
        d = dict(zip(NODE_COLS, r))
        c.trimmed_doc[d["id"]] = bool(d["docComment"] and structured_doc(d["docComment"]).strip())
        if d["id"] in win:
            c.rows[d["id"]] = d
    con.close()
    c.src, missing = {}, 0
    for d in c.rows.values():
        fp = d["filePath"]
        if fp not in c.src:
            p = corpora_dir / c.name / fp
            c.src[fp] = p.read_text(errors="replace").split("\n") if p.exists() else None
            missing += c.src[fp] is None
    return missing


def build_texts(c, arms, tok, body_budget):
    """c.atext[arm][nid] for every window node."""
    c.atext = {}
    for a in arms:
        sw, bb, _ = ARMS[a]
        c.atext[a] = {nid: T.build(d, c.src[d["filePath"]], sw, tok, bb or body_budget) for nid, d in c.rows.items()}


def in_scope(arm, c, q):
    return not (ARMS[arm][2] and c.queries[q]["mechanical"])


def text_of(arm, c, q, nid):
    return c.atext[arm if in_scope(arm, c, q) else "off"][nid]


def need_pairs(cs, cache, arm, k, seen):
    """{(corpus, qid): {sha16: text}} the caches lack for `arm`, minus pairs in `seen`."""
    need = {}
    for c in cs:
        for q in c.qids:
            if not in_scope(arm, c, q):
                continue
            for nid, _ in c.lists[ST][q][:k]:
                t = c.atext[arm][nid]
                h = tsha(t)
                if (c.name, q, h) in seen or cache.has(c, q, h):
                    continue
                seen.add((c.name, q, h))
                need.setdefault((c.name, q), {})[h] = t
    return need


def score_missing(cs, cache, need, ce, stage, limit=None):
    """Scores `need` (at most `limit` pairs), saving the cache at each progress write.
    Returns (pairs scored, seconds, the ETA estimated at the first 10%)."""
    byname = {c.name: c for c in cs}
    total = sum(len(v) for v in need.values())
    if limit is not None:
        total = min(total, limit)
    dirty = set()

    def flush():
        for cn in dirty:
            cache.save(byname[cn])
        dirty.clear()

    p = Progress(stage, total, flush)
    t0, done = time.time(), 0
    for (cn, q), m in need.items():
        if done >= total:
            break
        hs, ts = list(m)[:total - done], list(m.values())[:total - done]
        cache.put(cn, q, hs, score_texts(ce, byname[cn].queries[q]["text"], ts))
        dirty.add(cn)
        done += len(ts)
        p.tick(done)
    p.write(done)
    return done, time.time() - t0, p.first_eta


# ---- controls ------------------------------------------------------------------
def window_pairs(cs, k):
    return [(c, q, nid) for c in cs for q in c.qids for nid, _ in c.lists[ST][q][:k]]


def c1_off_is_base(cs):
    nodes = [(c, nid) for c in cs for nid in c.rows]
    bad = [(c.name, nid) for c, nid in nodes if c.atext["off"][nid] != c.stext[nid]]
    print(f"C1 build(all off) == structured text: {len(nodes) - len(bad)}/{len(nodes)} window nodes")
    if bad:
        sys.exit(f"STOP: C1 failed, first {bad[:5]}")


def c2_cache_covers_off(cs, cache, k, may_score):
    pairs = window_pairs(cs, k)
    miss = {}
    for c, q, nid in pairs:
        t = c.atext["off"][nid]
        if tsha(t) not in cache.base[c.name].get(q, {}):
            miss.setdefault((c.name, q), {})[tsha(t)] = t
    n = sum(len(v) for v in miss.values())
    print(f"C2 gm464 cache ({cache.base_key}) holds the off pair: {len(pairs) - n}/{len(pairs)}")
    if n and not may_score:
        sys.exit("STOP: C2 failed on the current eval set")
    return miss


def c3_rescore(cs, cache, ce, k):
    """Rescores C3_PAIRS off pairs grouped by query, as score_missing does. Returns ms per pair."""
    pool = window_pairs(cs, k)
    sample = random.Random(462).sample(pool, min(C3_PAIRS, len(pool)))
    byq = {}
    for c, q, nid in sample:
        byq.setdefault((c, q), []).append(c.atext["off"][nid])
    diffs, t0 = [], time.time()
    p = Progress("C3 rescoring", len(sample))
    for (c, q), ts in byq.items():
        got = score_texts(ce, c.queries[q]["text"], ts)
        diffs += [abs(float(g) - cache.get(c, q, t)) for g, t in zip(got, ts)]
        p.tick(len(diffs))
    p.write(len(diffs))
    ms = 1000.0 * (time.time() - t0) / len(diffs)
    import onnxruntime
    import tokenizers
    m = max(diffs)
    print(f"C3 {len(diffs)} off pairs rescored: max|diff| {m:.3e} (onnxruntime {onnxruntime.__version__}, "
          f"tokenizers {tokenizers.__version__}, model sha256 {ce.files['model.onnx']}); {ms:.2f} ms/pair")
    if m > C3_MAX_DIFF:
        sys.exit(f"STOP: C3 max|diff| {m:.3e} > {C3_MAX_DIFF}")
    return m, ms


def c6_texts_differ(cs):
    nodes = [(c, nid) for c in cs for nid in c.rows]
    n = sum(c.atext["all"][nid] != c.atext["off"][nid] for c, nid in nodes)
    print(f"C6 all-arm text != base: {n}/{len(nodes)} window nodes")
    if n != len(nodes):
        sys.exit("STOP: C6 failed (the all arm leaves some base text unchanged)")
    return n, len(nodes)


def c4_target(doc, section, label):
    """(label, the published lines of that label in order) from `doc`, inside
    `section` when one is given. `label` is a string or a compiled pattern
    whose group 1 is the label; a pattern must match one label only."""
    if not doc.exists():
        sys.exit(f"STOP: C4 target {doc} does not exist")
    text = doc.read_text()
    if section:
        if section + "\n" not in text:
            sys.exit(f"STOP: C4 section '{section}' not in {doc}")
        i = text.index(section + "\n")
        j = text.find("\n## ", i + len(section))
        text = text[i:j if j >= 0 else None]
    lines = text.splitlines()
    if isinstance(label, str):
        return label, [l for l in lines if l.startswith(f"| {label} |")]
    found = {m.group(1) for m in map(label.match, lines) if m}
    if len(found) != 1:
        sys.exit(f"STOP: C4 wants one row matching {label.pattern} in {doc}, found {sorted(found) or 'none'}")
    lab = found.pop()
    return lab, [l for l in lines if l.startswith(f"| {lab} |")]


def gm398_ids(cs):
    """Query ids of the current (GM-398) eval set, authored and mechanical."""
    return {q for c in cs for q in R.load_queries(HERE, c.name)[0]}


# ---- metrics -------------------------------------------------------------------
def paired(a, b, keep):
    """b minus a over `keep`: (Δr@10, ΔMRR) bootstraps, pooled by language."""
    by = {o["id"]: o for o in b}
    r10, mrr = {}, {}
    for o in a:
        if keep(o):
            x = by[o["id"]]
            r10.setdefault(o["language"], []).append(R.hit10(x) - R.hit10(o))
            mrr.setdefault(o["language"], []).append(R.rr(x) - R.rr(o))
    return R.bootstrap(r10), R.bootstrap(mrr)


def fmt_d(t, mrr=False):
    if t is None:
        return "-"
    if mrr:
        return f"{t[0]:+.3f} [{t[1]:+.3f}, {t[2]:+.3f}]"
    return f"{R.pts(t[0])} [{R.pts(t[1])}, {R.pts(t[2])}]"


def d9_pass(r):
    return all(r["vs"][t]["pass"][q] for t in ("int8", "fp32") for q in ("Q1", "Q2", "Q3", "Q4", "Q5"))


def verdict(d10, dmrr, d9):
    """Section 6's rule over the paired all-minus-off bootstraps."""
    def up(t):
        return t[1] >= 0 and t[0] > 0

    if ((up(dmrr) and d10[0] >= 0) or (up(d10) and dmrr[0] >= 0)) and d9:
        return "helps"
    if d10[2] < 0 or dmrr[2] < 0:
        return "hurts"
    return "no effect"


def bucket_of(c, q):
    docs = [c.trimmed_doc.get(n, False) for n in c.expected[q]]
    return "documented" if all(docs) else "signature-only" if not any(docs) else "mixed"


def window_logits(cs, arm, beta, k, score_of):
    """Per query: (logit of the best-ranked expected node in the window, logits of
    every expected node in the window), or None when no expected node is there."""
    out = {}
    for c in cs:
        for q in c.qids:
            lst = c.lists[ST][q][:k]
            exp = [(i, nid, x) for i, (nid, x) in enumerate(lst) if nid in c.expected[q]]
            if not exp:
                continue
            lg = [score_of(c, q, nid) for _, nid, _ in exp]
            best = max(range(len(exp)), key=lambda j: (lg[j] + beta * exp[j][2], -exp[j][0]))
            out[q] = (lg[best], lg)
    return out


def bucket_table(cs, outs_off, outs_all, lg_off, lg_all):
    L = ["", "Buckets of held-out NL positives by the expected symbols' doc comments (off and all at their tuned betas)",
         "r@10 and MRR are pooled over languages (the mean of per-language means), as the Δ columns are", "",
         "| bucket | scope | n | off r@10 / MRR | all r@10 / MRR | Δr@10 [lo, hi] | ΔMRR [lo, hi] | "
             "median logit off / all | expected nodes logit < -8 off / all |",
         "|---|---|---|---|---|---|---|---|---|"]
    lab = {(c.name, q): bucket_of(c, q) for c in cs for q in c.qids}
    by_all = {o["id"]: o for o in outs_all}
    for b in ("documented", "signature-only", "mixed"):
        for scope in ("all languages", "typescript"):
            keep = lambda o: (R.ho(o) and o["positive"] and lab[(o["corpus"], o["id"])] == b
                              and (scope == "all languages" or o["language"] == "typescript"))
            sel = [o for o in outs_off if keep(o)]
            if not sel:
                L.append(f"| {b} | {scope} | 0 | - | - | - | - | - | - |")
                continue
            m = lambda xs, f: R.pooled(xs, f, lambda o: True)[0]
            sa = [by_all[o["id"]] for o in sel]
            d10, dmrr = paired(outs_off, outs_all, keep)
            lo = [lg_off[o["id"]] for o in sel if o["id"] in lg_off]
            la = [lg_all[o["id"]] for o in sel if o["id"] in lg_all]
            med = lambda v: f"{statistics.median(x[0] for x in v):.2f}" if v else "-"
            low = lambda v: sum(sum(l < LOGIT_LOW for l in x[1]) for x in v)
            L.append(f"| {b} | {scope} | {len(sel)} | {m(sel, R.hit10):.3f} / {m(sel, R.rr):.3f} | "
                     f"{m(sa, R.hit10):.3f} / {m(sa, R.rr):.3f} | {fmt_d(d10)} | {fmt_d(dmrr, True)} | "
                     f"{med(lo)} / {med(la)} | {low(lo)} / {low(la)} |")
    return L


def strict_ho(cs):
    """Held-out NL queries that share no expected symbol with a FIT query."""
    drop = set()
    for c in cs:
        fit = set().union(*([c.expected[q] for q in c.qids
                             if not c.queries[q]["mechanical"] and not c.queries[q]["held_out"]] or [set()]))
        drop |= {q for q in c.qids if c.queries[q]["held_out"] and c.expected[q] & fit}
    return lambda o: R.ho(o) and o["id"] not in drop


# ---- main ----------------------------------------------------------------------
def parse_args(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--eval-dir", type=Path, default=HERE)
    ap.add_argument("--st-runs", default=f"runs-gm465/{ST}",
                    help="int8-structured runs root, relative to --work unless absolute")
    ap.add_argument("--corpora", type=Path, help="corpus checkouts (default <work>/corpora)")
    ap.add_argument("--cache-dir", type=Path, help="score caches (default <work>/rerank_cache)")
    ap.add_argument("--k", type=int, default=30)
    ap.add_argument("--setup", choices=("shipped", "gm443-k50"), default="shipped")
    ap.add_argument("--arms", default=DEFAULT_ARMS)
    ap.add_argument("--body-budget", type=int, default=T.BODY_BUDGET)
    ap.add_argument("--dry-run", metavar="CORPUS",
                    help="controls and the pair count on every corpus, then time new pairs of CORPUS and stop")
    ap.add_argument("--dry-run-pairs", type=int, default=200,
                    help="new pairs the dry run scores to time the arms' text (0: none)")
    ap.add_argument("--c4-doc", type=Path, help="C4 target doc (default: GM-464 S1's on the current set, "
                                                "GM-460's results on another eval dir)")
    ap.add_argument("--c4-section")
    ap.add_argument("--c4-label")
    ap.add_argument("--table", type=Path)
    ap.add_argument("--summary", type=Path)
    a = ap.parse_args(argv)
    a.arms = [x for x in a.arms.split(",") if x]
    unknown = [x for x in a.arms if x not in ARMS]
    if unknown:
        ap.error(f"unknown arms {unknown}; known: {list(ARMS)}")
    if "off" not in a.arms:
        a.arms.insert(0, "off")
    return a


def main(argv=None):
    a = parse_args(argv)
    if a.setup == "gm443-k50":
        raise NotImplementedError("--setup gm443-k50 (design option B) is not built: the owner chose setup A")
    work = a.work
    st_dir = Path(a.st_runs) if Path(a.st_runs).is_absolute() else work / a.st_runs
    corpora_dir = a.corpora or work / "corpora"
    cache_dir = a.cache_dir or work / "rerank_cache"
    current_set = a.eval_dir.resolve() == HERE.resolve()
    toml = ((a.eval_dir / "variants.toml") if (a.eval_dir / "variants.toml").exists() else HERE / "variants.toml").read_text()
    for key in ("bootstrap_seed", "bootstrap_resamples"):
        R.SETTINGS[key] = int(re.search(rf"^{key}\s*=\s*(\d+)", toml, re.M).group(1))
    K = a.k
    S = [f"GM-462 setup A (shipped F4): K={K}, eval set {'current' if current_set else a.eval_dir}, "
         f"st-runs {st_dir}, arms {','.join(a.arms)}, body budget {a.body_budget}"]
    print(S[0])
    c1_fixture()

    corpora = [p.name for p in sorted((work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    if a.dry_run and a.dry_run not in corpora:
        sys.exit(f"STOP: --dry-run {a.dry_run} is not one of {corpora}")
    cs = [R.Corpus(work, a.eval_dir, c, 0) for c in corpora]
    ce = R.CrossEncoder(work, NAME)
    from tokenizers import Tokenizer
    tok = T.token_counter(Tokenizer.from_file(str(work / "models" / R.CE_DIR[NAME] / "tokenizer.json")))
    for c in cs:
        add_structured(c, st_dir)
        missing = load_nodes(c, corpora_dir, K)
        if missing:
            sys.exit(f"STOP: {missing} source files of {c.name} missing under {corpora_dir}")
        build_texts(c, a.arms, tok, a.body_budget)

    c1_off_is_base(cs)
    S.append("C1 pass")
    cache = EnrichedCache(cache_dir, cs, ce.files["model.onnx"])
    miss = c2_cache_covers_off(cs, cache, K, may_score=not current_set)
    if miss:
        n, _, _ = score_missing(cs, cache, miss, ce, "C2 off pairs")
        print(f"C2 covered after scoring {n}")
        S.append(f"C2 covered after scoring {n}")
    else:
        S.append("C2 pass (100%)")
    c3, ms_base = c3_rescore(cs, cache, ce, K)
    S.append(f"C3 max|diff| {c3:.3e}, {ms_base:.2f} ms/pair (base text)")
    if "all" in a.arms:
        S.append("C6 texts differ on {}/{} window nodes".format(*c6_texts_differ(cs)))

    seen, needs = set(), {}
    for arm in a.arms:
        needs[arm] = need_pairs(cs, cache, arm, K, seen)
        print(f"pairs to score for {arm}: {sum(len(v) for v in needs[arm].values())}")
    total = sum(len(v) for m in needs.values() for v in m.values())
    print(f"pairs to score, all arms: {total}; threads {ce.threads}; load {os.getloadavg()}")

    if a.dry_run:
        ms, src = ms_base, "C3, base text"
        arm = next((x for x in a.arms if any(cn == a.dry_run for cn, _ in needs[x])), None)
        if arm and a.dry_run_pairs:
            own = {key: v for key, v in needs[arm].items() if key[0] == a.dry_run}
            n, sec, _ = score_missing(cs, cache, own, ce, f"dry run {arm}", a.dry_run_pairs)
            ms, src = 1000.0 * sec / n, f"{n} new {arm} pairs of {a.dry_run}"
            print(f"dry run: {n} new {arm} pairs of {a.dry_run} scored, {ms:.2f} ms/pair")
        print(f"dry run ETA, every corpus: {total} pairs x {ms:.2f} ms ({src}) = {total * ms / 1000:.0f}s; "
              f"load {os.getloadavg()}")
        PROGRESS.write_text("GM-462 dry run done\n")
        print("dry run OK")
        return

    timing = []
    for arm in a.arms:
        if needs[arm]:
            n, sec, eta = score_missing(cs, cache, needs[arm], ce, f"CE scoring {arm}")
            timing.append(f"{arm}: {n} pairs in {sec:.0f}s ({1000 * sec / n:.2f} ms/pair), ETA at 10% {eta or 0:.0f}s")
            print("scored", timing[-1])
    S += timing

    p = Progress("metrics", len(a.arms))
    shipped = R.shipped_floors(HERE.parents[1])
    base = {arm: [R.outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                  for c in cs for q in c.qids] for arm in (R.REF, ST)}
    rf32 = R.fit_floors(base[R.REF])
    mkrow = lambda label, outs: row(label, outs, shipped, base[R.REF], rf32, base[ST], shipped)
    r32 = row("jina fp32, full text (baseline, D6 floors)", base[R.REF], rf32, base[R.REF], rf32, base[ST], shipped)
    r8s = mkrow("int8-structured (no rerank, shipped floors)", base[ST])

    ctl = f4(cs, K, SHIPPED_BETA, rerank=False)
    ok = ctl == base[ST] and sig(mkrow("off", ctl)) == sig(r8s)
    print(f"C5 rerank disabled == int8-structured on every outcome and cell: {ok}")
    if not ok:
        sys.exit("STOP: C5 failed")
    S.append("C5 pass")

    score_of = {arm: (lambda arm_: lambda c, q, nid: cache.get(c, q, text_of(arm_, c, q, nid)))(arm) for arm in a.arms}
    fit_keys = [(c, q) for c in cs for q in c.qids if R.fit_nl(c.queries[q])]
    tuned, outs, rows, variants = {}, {}, [r32, r8s], []
    sweep = []
    for i, arm in enumerate(a.arms):
        bb = None
        for b in R.BETAS:
            key = fit_obj(fit_keys, K, score_of[arm], b)
            if bb is None or key + (-b,) > bb[0]:
                bb = (key + (-b,), b)
            if arm == "all":
                s = R.summarize(f4(cs, K, b, score_of[arm]))
                sweep.append(f"| {b:g} | {key[0]:.3f} | {key[1]:.3f} | {s['nl_heldout']['r10']:.3f} | "
                             f"{s['nl_heldout']['mrr']:.3f} | {s['name']['r10']:.3f} / {s['name']['mrr']:.3f} |")
        tuned[arm] = bb[1]
        scope = " (name queries: off order)" if ARMS[arm][2] else ""
        for b in sorted({SHIPPED_BETA, tuned[arm]}):
            tag = " (FIT-tuned)" if b == tuned[arm] else ""
            lab = f"{arm} K={K} beta={b:g}{tag}{scope}"
            o = f4(cs, K, b, score_of[arm])
            outs[(arm, b)] = o
            rows.append(mkrow(lab, o))
            variants.append((lab, o))
        print(f"arm {arm}: FIT-tuned beta {tuned[arm]:g} (FIT r@10 {bb[0][0]:.3f}, MRR {bb[0][1]:.3f})")
        p.write(i + 1)
    S.append("FIT-tuned beta: " + ", ".join(f"{x} {b:g}" for x, b in tuned.items()))
    by8 = {o_["id"]: o_ for o_ in base[ST]}
    rowby = {r["label"]: r for r in rows}

    # ---- C4: the off row at beta 80 reproduces the published F4 row ----
    off80 = outs[("off", SHIPPED_BETA)]

    def c4(tag, target, ids=None):
        """Renders the off row at beta 80 (restricted to `ids` when given) under the
        target's label and compares it with the published lines. A "(FIT-tuned)"
        label also requires the off arm's FIT-tuned beta, on the same queries, to be 80."""
        label, want = target
        sel = (lambda xs: xs) if ids is None else (lambda xs: [o_ for o_ in xs if o_["id"] in ids])
        b32, b8, o80 = sel(base[R.REF]), sel(base[ST]), sel(off80)
        r = row(label, o80, shipped, b32, R.fit_floors(b32), b8, shipped)
        b8by = {o_["id"]: o_ for o_ in b8}
        got = [l for l in render([r]) + flips([(label, o80)], b8by, shipped) if l.startswith(f"| {label} |")]
        beta = tuned["off"]
        if ids is not None:
            keys = [(c, q) for c, q in fit_keys if q in ids]
            beta = max(R.BETAS, key=lambda b: fit_obj(keys, K, score_of["off"], b) + (-b,))
        ok = bool(want) and got == want and ("(FIT-tuned)" not in label or beta == SHIPPED_BETA)
        print(f"C4{tag} off row reproduces '{label}': {'MATCH' if ok else 'MISMATCH'} "
              f"({len(want)} published lines); off FIT-tuned beta {beta:g}")
        if not ok:
            for g_, w_ in zip(got + [""] * len(want), want + [""] * len(got)):
                if g_ != w_:
                    print(f"   got  {g_}\n   want {w_}")
            sys.exit(f"STOP: C4{tag} failed")
        S.append(f"C4{tag} pass ('{label}')")

    if current_set or a.c4_doc:
        c4("", c4_target(a.c4_doc or C4_DOC, a.c4_section if a.c4_doc else C4_SECTION,
                         a.c4_label or (C4_LABEL if not a.c4_doc else V2_C4_LABEL)))
    else:
        c4("a", c4_target(V2_C4_DOC, a.c4_section, V2_C4_LABEL))
    if not current_set:
        c4("b (GM-398 ids)", c4_target(C4_DOC, C4_SECTION, C4_LABEL), gm398_ids(cs))

    L = render(rows)
    L += flips(variants, by8, shipped)
    mean_tok = {arm: statistics.mean(tok(c.atext[arm][n]) for c in cs for n in c.rows) for arm in a.arms}
    L += ["", "Mean text tokens per window node (no special tokens)", "", "| arm | tokens |", "|---|---|"]
    L += [f"| {arm} | {mean_tok[arm]:.1f} |" for arm in a.arms]

    off_t = outs[("off", tuned["off"])]
    if "all" in a.arms:
        all_t = outs[("all", tuned["all"])]
        all_row = rowby[f"all K={K} beta={tuned['all']:g} (FIT-tuned)"]
        differs = sig(all_row) != sig(rowby[f"off K={K} beta={tuned['off']:g} (FIT-tuned)"])
        print(f"C6 all row differs from the off row: {differs}")
        if not differs:
            sys.exit("STOP: C6 failed (the all row equals the off row)")
        S.append("C6 rows differ")
        L += ["", f"Beta sweep, all arm, K={K}: FIT-half objective and held-out point estimates", "",
              "| beta | FIT NL r@10 | FIT NL MRR | held-out NL r@10 | held-out NL MRR | name r@10 / MRR |",
              "|---|---|---|---|---|---|"] + sweep
        d10, dmrr = paired(off_t, all_t, R.ho)
        g = R.gates(off_t, all_t, R.ho, rf=shipped)
        v = verdict(g["r10"], g["mrr"], d9_pass(all_row))
        L += ["", "Paired, held-out NL, at each arm's FIT-tuned beta (b minus a)", "",
              "| a | b | Δr@10 [lo, hi] | ΔMRR [lo, hi] |", "|---|---|---|---|",
              f"| off | all | {fmt_d(g['r10'])} | {fmt_d(g['mrr'], True)} |"]
        loo = []
        for arm in a.arms:
            if arm in ("off", "all"):
                continue
            x10, xmrr = paired(all_t, outs[(arm, tuned[arm])], R.ho)
            L.append(f"| all | {arm} | {fmt_d(x10)} | {fmt_d(xmrr, True)} |")
            if arm in LOO_ARMS and x10[1] <= 0 <= x10[2] and xmrr[1] <= 0 <= xmrr[2]:
                loo.append(arm)
        if not current_set:
            s10, smrr = paired(off_t, all_t, strict_ho(cs))
            L += [f"| off | all (strict held-out) | {fmt_d(s10)} | {fmt_d(smrr, True)} |"]
        lg_off = window_logits(cs, "off", tuned["off"], K, score_of["off"])
        lg_all = window_logits(cs, "all", tuned["all"], K, score_of["all"])
        L += bucket_table(cs, off_t, all_t, lg_off, lg_all)
        cheap = min(loo, key=lambda x: mean_tok[x]) if loo and v == "helps" else None
        vline = (f"VERDICT all vs off: {v} (Δr@10 {fmt_d(g['r10'])}, ΔMRR {fmt_d(g['mrr'], True)}, "
                 f"D9 vs int8-structured and fp32 {'pass' if d9_pass(all_row) else 'FAIL'})")
        L += ["", vline, f"LOO arms within noise of all: {loo or 'none'}; cheapest: {cheap or '-'}"]
        S += [vline, f"LOO within noise of all: {loo or 'none'}; cheapest {cheap or '-'}"]
        assert (d10, dmrr) == (g["r10"], g["mrr"]), "paired() disagrees with R.gates"
    p.write(len(a.arms))

    print("\n" + "\n".join(L))
    if a.table:
        a.table.write_text("\n".join(L) + "\n")
    if a.summary:
        a.summary.write_text("\n".join(S) + "\n")
    PROGRESS.write_text("GM-462 measure done\n")


if __name__ == "__main__":
    main()
