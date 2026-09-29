#!/usr/bin/env python3
"""GM-443: recall with a cheap stage, then rerank its top-K. Does it reach jina?

Two recall stages, each from stored data: gte-small and jina-v2-base-code int8
(the shipped model, ADR 0011) rankings that GM-398's `g-mesh embed-eval run`
wrote (top 100 per query), plus the per-corpus g-mesh indexes
(`<corpus>.sqlite`) for symbol text and graph edges. The rerankers run here,
on CPU.

Rerankers, each over the recall arm's top-K (K = 50, and 100 as a
sensitivity); the reranked top-K is followed by the arm's hits K+1..100 in
the arm's order:
- identity: the arm's cosine itself (control: must give the arm's GM-398 numbers);
- shuffled: seeded random scores (control: must fail);
- cross-encoders on (query text, symbol text), symbol text being exactly what
  the embedders saw (pipeline.rs `text_to_embed`: doc + "\n\n" + signature);
- cross-encoder + arm: ce_logit + beta * arm_cosine, beta tuned on the FIT half;
- graph: arm_cosine + a*log1p(in-degree) + b*log1p(#top-K graph neighbours)
  + c*log1p(#top-10 hits in the same file), (a, b, c) tuned on the FIT half.
Tuned variants are gated on the held-out NL half only.

Every variant is gated (D9) against two baselines: jina fp32 at its D6 floors,
and jina int8 at the floors `core/src/mcp/similarity.rs::floor` ships (read
from the source; on the full query set they must equal int8's D6 fit).

Loaders, metrics.rs floors / false alarm / confident wrong / SplitMix64
bootstrap and decision.rs D9 gates are copied from GM-444's
eval/embedding/fusion_eval.py (commit ffed83c), which took them from
eval/embedding/q5_floor_sensitivity.py. The GM-434 misled / confident-wrong
columns follow eval/embedding/shipped_floor_rates.py, option "a" (every page
below the floor is noMatch), at each variant's own D6-fitted floors.

Cross-encoder logits are cached per pair under <work>/rerank_cache/.

usage: rerank_eval.py --work <eval/embedding/work> [--corpora a,b --max-queries N]
                      [--table out.md] [--json out.json]
       rerank_eval.py --work <eval/embedding/work> --latency N   (K=50 timing only)
"""
import argparse
import hashlib
import json
import math
import os
import random
import re
import sqlite3
import statistics
import sys
import time
from pathlib import Path

import numpy as np

KEPT_HITS = 100
MAX_FALSE_ALARM = 0.03
EPS = 1e-12
REF = "jina-v2-base-code-fp32"
GTE = "gte-small"
INT8 = "jina-v2-base-code-int8"
RECALL = (GTE, INT8)  # recall stages; INT8 is also the shipped model (ADR 0011)
SHORT = {REF: "fp32", GTE: "gte", INT8: "int8"}
LANGS = ["go", "python", "rust", "typescript"]
KS = (50, 100)
CES = {  # name -> (Hugging Face id, pinned revision); files under <work>/models/<name>/
    "ce-minilm": ("cross-encoder/ms-marco-MiniLM-L6-v2", "233902d25c440f23af6f7d6e94d2946bac0bee0a"),
    "ce-jina-tiny": ("jinaai/jina-reranker-v1-tiny-en", "aca45de6945b5dc6399abcd2a9c55ded5dc9111f"),
}
CE_DIR = {"ce-minilm": "ms-marco-MiniLM-L6-v2", "ce-jina-tiny": "jina-reranker-v1-tiny-en"}
MAX_TOKENS = 512
BETAS = [0.0, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0]
GRID_A = [-0.01, -0.005, 0.0, 0.005, 0.01, 0.02]
GRID_B = [0.0, 0.005, 0.01, 0.02, 0.04]
GRID_C = [0.0, 0.005, 0.01, 0.02]
GRAPH_KINDS = ("CALLS", "REFERENCES", "SUPERTYPE_OF")
INDEG_KINDS = ("CALLS", "REFERENCES")


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


# --- loading (fusion_eval.py; recall_at_k.py; embed_eval.rs load_nodes) --------
def held_out(qid, mechanical):
    return not mechanical and hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load_queries(eval_dir, corpus):
    out, hashes = {}, []
    for rel, mech in [(f"queries/{corpus}.jsonl", False), (f"queries/mechanical/{corpus}.jsonl", True)]:
        p = eval_dir / rel
        if not p.exists():
            continue
        data = p.read_bytes()
        hashes.append([rel, hashlib.sha256(data).hexdigest()])
        for line in data.decode().splitlines():
            if line.strip():
                q = json.loads(line)
                q["mechanical"] = mech
                q["positive"] = q["kind"] == "positive"
                q["held_out"] = held_out(q["id"], mech)
                out[q["id"]] = q
    return out, hashes


def text_to_embed(doc, sig):
    """pipeline.rs text_to_embed."""
    doc = (doc or "").strip() or None
    sig = (sig or "").strip() or None
    if doc and sig:
        return f"{doc}\n\n{sig}"
    return doc or sig


class Corpus:
    def __init__(self, work, eval_dir, corpus, max_queries):
        self.name = corpus
        self.lists, self.expected, mans = {}, {}, {}
        for arm in (REF,) + RECALL:
            d = work / "runs" / arm / corpus
            man = json.loads((d / "manifest.json").read_text())
            mans[arm] = man
            if arm == REF:
                self.queries, hashes = load_queries(eval_dir, corpus)
                if hashes != man["queryFiles"]:
                    sys.exit(f"STOP: query files of {corpus} changed since {d} was run (D3 freeze)")
            lst = {}
            for line in (d / "rankings.jsonl").read_text().splitlines():
                r = json.loads(line)
                lst[r["id"]] = [(h[0], h[1]) for h in r["hits"]]
                self.expected.setdefault(r["id"], set(r["expected"]))
                if set(r["expected"]) != self.expected[r["id"]]:
                    sys.exit(f"STOP: {arm} {r['id']} expected set differs between arms")
            self.lists[arm] = lst
        for arm in RECALL:
            for key in ("snapshotSha256", "nodeIdsSha256", "queryFiles", "queryIds"):
                if mans[arm][key] != mans[REF][key]:
                    sys.exit(f"STOP: {arm}/{corpus} {key} differs from {REF}")
        self.snapshot_sha, self.node_ids_sha = mans[REF]["snapshotSha256"], mans[REF]["nodeIdsSha256"]
        self.all_qids = mans[REF]["queryIds"]
        self.db = work / f"{corpus}.sqlite"
        con = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True)
        ids, self.node_lang, self.text, self.file = [], {}, {}, {}
        for nid, language, doc, sig, fp in con.execute(
                "SELECT id, language, docComment, signature, filePath FROM nodes ORDER BY id"):
            self.node_lang[nid] = language
            self.file[nid] = fp
            t = text_to_embed(doc, sig)
            if t is not None:
                ids.append(nid)
                self.text[nid] = t
        h = hashlib.sha256()
        for nid in ids:
            h.update(nid.encode() + b"\n")
        if h.hexdigest() != mans[REF]["nodeIdsSha256"]:
            sys.exit(f"STOP: {corpus} candidate ids do not hash to the manifest's nodeIdsSha256")
        self.index = {nid: i for i, nid in enumerate(ids)}
        self.qids = mans[REF]["queryIds"][:max_queries] if max_queries else mans[REF]["queryIds"]
        self.gte_rankings_sha = sha256_file(work / "runs" / GTE / corpus / "rankings.jsonl")
        con.close()


# --- metrics.rs / decision.rs (copied from fusion_eval.py) --------------------
def outcome(c, qid, hits, top_score):
    q = c.queries[qid]
    exp = c.expected[qid]
    rank = next((i + 1 for i, (nid, _) in enumerate(hits[:KEPT_HITS]) if nid in exp), None)
    return {"id": qid, "corpus": c.name, "language": q["language"], "positive": q["positive"],
            "mechanical": q["mechanical"], "held_out": q["held_out"], "rank": rank,
            "top": top_score, "top_language": c.node_lang.get(hits[0][0]) if hits else None}


def pooled_mean(groups):
    means = [sum(v) / len(v) for _, v in sorted(groups.items()) if v]
    return sum(means) / len(means) if means else None


GAMMA = np.uint64(0x9E3779B97F4A7C15)


def splitmix_stream(seed, count):
    with np.errstate(over="ignore"):
        z = np.uint64(seed) + np.arange(1, count + 1, dtype=np.uint64) * GAMMA
        z = (z ^ (z >> np.uint64(30))) * np.uint64(0xBF58476D1CE4E5B9)
        z = (z ^ (z >> np.uint64(27))) * np.uint64(0x94D049BB133111EB)
        return z ^ (z >> np.uint64(31))


def below(x, n):
    n = np.uint64(n)
    hi, lo = x >> np.uint64(32), x & np.uint64(0xFFFFFFFF)
    with np.errstate(over="ignore"):
        return ((hi * n + ((lo * n) >> np.uint64(32))) >> np.uint64(32)).astype(np.int64)


SETTINGS = {}


def bootstrap(groups):
    point = pooled_mean(groups)
    if point is None:
        return None
    gs = [np.array(v, dtype=np.float64) for _, v in sorted(groups.items()) if v]
    R, seed = SETTINGS["bootstrap_resamples"], SETTINGS["bootstrap_seed"]
    N = sum(len(g) for g in gs)
    draws = splitmix_stream(seed, R * N).reshape(R, N)
    s = np.zeros(R)
    off = 0
    for g in gs:
        n = len(g)
        s += g[below(draws[:, off:off + n], n)].sum(axis=1) / n
        off += n
    stats = np.sort(s / len(gs))
    tail = math.floor(0.05 * R)
    return (point, float(stats[tail]), float(stats[R - 1 - tail]))


def round_down_2(x):
    return math.floor(x * 100.0 + 1e-9) / 100.0


def fit_floors(outs):
    scores = {}
    for o in outs:
        if o["positive"] and (o["mechanical"] or not o["held_out"]) and o["rank"] == 1 and o["top"] is not None:
            scores.setdefault(o["language"], []).append(o["top"])
    floors = {}
    for l, s in scores.items():
        s = sorted(s)
        k = math.floor(MAX_FALSE_ALARM * len(s) + 1e-9)
        floors[l] = round_down_2(s[min(k, len(s) - 1)])
    return floors


def clears(o, floors):
    if o["top"] is None or o["top_language"] not in floors:
        return None
    return o["top"] >= floors[o["top_language"]]


def fa_indicator(o, floors):
    if not (o["positive"] and not o["mechanical"] and o["held_out"] and o["rank"] == 1):
        return None
    c = clears(o, floors)
    return None if c is None else (0.0 if c else 1.0)


def cw_indicator(o, floors):
    if o["mechanical"] or not o["held_out"]:
        return None
    c = clears(o, floors)
    if c is None:
        return None
    wrong = (c and o["rank"] != 1) if o["positive"] else c
    return 1.0 if wrong else 0.0


def rate(outs, fn, keep=lambda o: True):
    v = [fn(o) for o in outs if keep(o)]
    v = [x for x in v if x is not None]
    return (sum(v) / len(v)) if v else None


def paired_own(ref, rf, cand, cf, ind):
    by = {o["id"]: ind(o, rf) for o in ref}
    g = {}
    for o in cand:
        r, c = by.get(o["id"]), ind(o, cf)
        if r is not None and c is not None:
            g.setdefault(o["language"], []).append(c - r)
    return g


def hit_at(k):
    return lambda o: 1.0 if o["rank"] is not None and o["rank"] <= k else 0.0


hit10 = hit_at(10)


def rr(o):
    return 1.0 / o["rank"] if o["rank"] is not None and o["rank"] <= KEPT_HITS else 0.0


def scored(o):
    return o["positive"] and not o["mechanical"]


def name_q(o):
    return o["positive"] and o["mechanical"]


def ho(o):
    return scored(o) and o["held_out"]


def fit_nl(o):
    return scored(o) and not o["held_out"]


def pooled(outs, fn, keep):
    g = {}
    for o in outs:
        if keep(o):
            g.setdefault(o["language"], []).append(fn(o))
    return pooled_mean(g), g


def gates(ref, cand, keep=scored, rf=None):
    by = {o["id"]: o for o in cand}
    r10, mrr = {}, {}
    for o in ref:
        if keep(o):
            c = by[o["id"]]
            r10.setdefault(o["language"], []).append(hit10(c) - hit10(o))
            mrr.setdefault(o["language"], []).append(rr(c) - rr(o))
    rf, cf = rf or fit_floors(ref), fit_floors(cand)
    b = {"r10": bootstrap(r10), "mrr": bootstrap(mrr),
         "cw": bootstrap(paired_own(ref, rf, cand, cf, cw_indicator)),
         "fa": bootstrap(paired_own(ref, rf, cand, cf, fa_indicator))}
    fa_g = paired_own(ref, rf, cand, cf, fa_indicator)
    b["fa_lang"] = {l: (sum(v) / len(v), len(v)) for l, v in sorted(fa_g.items())}
    b["q3"] = {l: sum(v) / len(v) for l, v in sorted(r10.items())}
    b["q3_worst"] = min(b["q3"].items(), key=lambda kv: kv[1])
    nan = (float("nan"),) * 3
    cw, fa = b["cw"] or nan, b["fa"] or nan
    b["pass"] = {
        "Q1": b["r10"][0] >= -0.02 - EPS and b["r10"][1] >= -0.05 - EPS,
        "Q2": b["mrr"][0] >= -0.02 - EPS and b["mrr"][1] >= -0.05 - EPS,
        "Q3": all(d >= -0.10 - EPS for d in b["q3"].values()),
        "Q4": cw[0] <= EPS and cw[2] <= 0.05 + EPS,
        "Q5": fa[0] <= EPS and fa[2] <= 0.05 + EPS,
        "Q1sup": b["r10"][1] > 0.0, "Q2sup": b["mrr"][1] > 0.0,
    }
    b["floors"] = cf
    return b


# --- GM-434 columns (shipped_floor_rates.py, option a, at own fitted floors) ---
def gm434(outs, floors, keep, group=None):
    """misled = below floor with the right top row (of rank-1 positives);
    confident wrong = clears the floor with a wrong top (positives) / at all (absent)."""
    sel = [o for o in outs if keep(o) and (group is None or o["language"] == group)]
    fa = [o for o in sel if o["positive"] and o["rank"] == 1 and clears(o, floors) is not None]
    cwp = [o for o in sel if o["positive"] and clears(o, floors) is not None]
    cwa = [o for o in sel if not o["positive"] and clears(o, floors) is not None]
    return {"misled": (sum(not clears(o, floors) for o in fa), len(fa)),
            "cw_pos": (sum(clears(o, floors) and o["rank"] != 1 for o in cwp), len(cwp)),
            "cw_abs": (sum(clears(o, floors) for o in cwa), len(cwa))}


def nl_ho_all(o):  # GM-434 "NL held-out": positives and absent
    return not o["mechanical"] and o["held_out"]


def name_all(o):
    return o["mechanical"]


def shipped_floors(repo):
    """similarity.rs::floor's per-language constants, read from the source."""
    src = (repo / "core/src/mcp/similarity.rs").read_text()
    body = src[src.index("pub(crate) fn floor("):]
    body = body[:body.index("\n}\n")]
    return {l: float(v) for l, v in re.findall(r'"(\w+)" => ([0-9.]+)', body)}


# --- rerankers ----------------------------------------------------------------
class CrossEncoder:
    def __init__(self, work, name):
        import onnxruntime as ort
        from tokenizers import Tokenizer
        d = work / "models" / CE_DIR[name]
        self.files = {f: sha256_file(d / f) for f in ("model.onnx", "tokenizer.json", "config.json")}
        self.tok = Tokenizer.from_file(str(d / "tokenizer.json"))
        self.tok.enable_truncation(MAX_TOKENS)
        self.pad_id = next(i for i in (self.tok.token_to_id(t) for t in ("[PAD]", "<pad>")) if i is not None)
        assert self.pad_id is not None
        so = ort.SessionOptions()
        self.sess = ort.InferenceSession(str(d / "model.onnx"), so, providers=["CPUExecutionProvider"])
        self.names = {i.name for i in self.sess.get_inputs()}
        self.threads = so.intra_op_num_threads

    def score(self, query, texts, chunk=16):
        """Logits for (query, text) pairs, run in length-sorted chunks so one long doc
        comment does not pad the whole batch to 512 tokens (padding is masked)."""
        enc = self.tok.encode_batch([(query, t) for t in texts])
        order = sorted(range(len(enc)), key=lambda i: len(enc[i].ids))
        out = np.zeros(len(enc))
        for s in range(0, len(order), chunk):
            idx = order[s:s + chunk]
            L = max(len(enc[i].ids) for i in idx)
            ids = np.full((len(idx), L), self.pad_id, dtype=np.int64)
            mask = np.zeros((len(idx), L), dtype=np.int64)
            tt = np.zeros((len(idx), L), dtype=np.int64)
            for r, i in enumerate(idx):
                n = len(enc[i].ids)
                ids[r, :n], mask[r, :n], tt[r, :n] = enc[i].ids, 1, enc[i].type_ids
            feed = {"input_ids": ids, "attention_mask": mask}
            if "token_type_ids" in self.names:
                feed["token_type_ids"] = tt
            out[idx] = self.sess.run(None, feed)[0].reshape(-1)
        return out


def ce_scores(work, cs, name, cache_dir):
    """Per corpus: qid -> {node id: logit} over the union of every recall arm's
    top-100. Cached per (model file, snapshot, node ids, MAX_TOKENS); pairs the
    cache lacks are scored and added. A pair's logit does not depend on the other
    pairs in its batch (padding is masked)."""
    ce = CrossEncoder(work, name)
    out, scored_pairs = {}, 0
    for c in cs:
        key = hashlib.sha256(json.dumps([ce.files["model.onnx"], c.snapshot_sha, c.node_ids_sha, MAX_TOKENS]).encode()).hexdigest()[:16]
        f = cache_dir / f"{name}.{c.name}.pairs.{key}.json"
        d = json.loads(f.read_text()) if f.exists() else {}
        # the first cache format: a list aligned with gte's stored top-100, whole query set
        old_key = hashlib.sha256(json.dumps([ce.files["model.onnx"], c.gte_rankings_sha, c.all_qids, MAX_TOKENS]).encode()).hexdigest()[:16]
        old = cache_dir / f"{name}.{c.name}.{old_key}.json"
        if not d and old.exists():
            for q, v in json.loads(old.read_text()).items():
                d[q] = {n: s_ for (n, _), s_ in zip(c.lists[GTE][q], v["s"])}
        new = 0
        for q in c.qids:
            have = d.setdefault(q, {})
            need = list(dict.fromkeys(n for arm in RECALL for n, _ in c.lists[arm][q] if n not in have))
            if need:
                have.update(zip(need, map(float, ce.score(c.queries[q]["text"], [c.text[n] for n in need]))))
                new += len(need)
        if new or not f.exists():
            f.write_text(json.dumps(d))
        scored_pairs += new
        out[c.name] = d
    return out, ce.files, ce.threads, scored_pairs


def graph_features(c, qids, arm):
    """Per query: features over the recall arm's top-K, read from the corpus index with SQL
    per query (the path a server would run), and per-query seconds for the top-50."""
    con = sqlite3.connect(f"file:{c.db}?mode=ro", uri=True)
    feats, secs = {}, []
    for q in qids:
        ids = [n for n, _ in c.lists[arm][q]]
        per_k = {}
        for K in KS:
            t0 = time.perf_counter()
            top = ids[:K]
            ph = ",".join("?" * len(top))
            indeg = dict(con.execute(
                f"SELECT toId, count(*) FROM edges WHERE toId IN ({ph}) AND kind IN ({','.join('?' * len(INDEG_KINDS))}) GROUP BY toId",
                top + list(INDEG_KINDS)).fetchall())
            nb = {n: set() for n in top}
            for a, b in con.execute(
                    f"SELECT fromId, toId FROM edges WHERE fromId IN ({ph}) AND toId IN ({ph}) "
                    f"AND kind IN ({','.join('?' * len(GRAPH_KINDS))})", top + top + list(GRAPH_KINDS)):
                if a != b:
                    nb[a].add(b)
                    nb[b].add(a)
            files = dict(con.execute(f"SELECT id, filePath FROM nodes WHERE id IN ({ph})", top).fetchall())
            top10_files = [files.get(n) for n in top[:10]]
            f = np.zeros((len(top), 3))
            for i, n in enumerate(top):
                same = sum(1 for j, fp in enumerate(top10_files) if fp == files.get(n) and j != i)
                f[i] = (math.log1p(indeg.get(n, 0)), math.log1p(len(nb[n])), math.log1p(same))
            dt = time.perf_counter() - t0
            per_k[K] = f
            if K == 50:
                secs.append(dt)
        feats[q] = per_k
    con.close()
    return feats, secs


def reranked(c, q, K, score, arm):
    """Rerank the arm's top-K by `score` (array over those K), ties by the arm's
    position; hits K+1..100 follow in the arm's order. Returns (hits, own top score)."""
    hits = c.lists[arm][q]
    k = min(K, len(hits))
    s = np.asarray(score[:k], dtype=np.float64)
    order = np.lexsort((np.arange(k), -s))
    new = [(hits[i][0], float(s[i])) for i in order] + list(hits[k:])
    return new, float(s[order[0]]) if k else None


def first_rank(c, q, hits):
    exp = c.expected[q]
    return next((i + 1 for i, (n, _) in enumerate(hits) if n in exp), None)


# --- report helpers ---------------------------------------------------------------
def pts(x):
    return f"{100 * x:+.1f}"


def f3(x):
    return "-" if x is None else f"{x:.3f}"


def pct(t):
    return f"{100 * t[0] / t[1]:.1f}% ({t[0]}/{t[1]})" if t[1] else "-"


def pctl(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, max(0, math.ceil(p * len(v)) - 1))]


def summarize(outs):
    s = {}
    for label, keep in [("nl", scored), ("nl_heldout", ho), ("nl_fit", fit_nl), ("name", name_q)]:
        s[label] = {"r1": pooled(outs, hit_at(1), keep)[0], "r5": pooled(outs, hit_at(5), keep)[0],
                    "r10": pooled(outs, hit10, keep)[0], "mrr": pooled(outs, rr, keep)[0],
                    "n": sum(1 for o in outs if keep(o))}
    return s


def latency_only(work, cs, n):
    """Per-query wall time of the K=50 rerank (tokenize + model, or SQL features),
    on a seeded sample of all queries, for each recall arm, after one warm-up query
    per reranker. Excludes the recall stage (query embedding + vector search)."""
    pool = [(c, q) for c in cs for q in c.qids]
    sample = random.Random(50).sample(pool, min(n, len(pool)))
    res = {}
    for name in CES:
        ce = CrossEncoder(work, name)
        c0, q0 = sample[0]
        ce.score(c0.queries[q0]["text"], [c0.text[x] for x, _ in c0.lists[GTE][q0][:50]])
        for arm in RECALL:
            ts = []
            for c, q in sample:
                texts = [c.text[x] for x, _ in c.lists[arm][q][:50]]
                t0 = time.perf_counter()
                ce.score(c.queries[q]["text"], texts)
                ts.append(time.perf_counter() - t0)
            res[f"{name} over {SHORT[arm]}"] = ts
            print(f"{name} over {SHORT[arm]} K=50: n={len(ts)} p50 {1000 * statistics.median(ts):.0f} ms, "
                  f"p95 {1000 * pctl(ts, 0.95):.0f} ms, max {1000 * max(ts):.0f} ms; "
                  f"ort intra-op threads {ce.threads} (0=default); {os.popen('uptime').read().strip()}", flush=True)
    for arm in RECALL:
        ts = []
        for c in cs:
            qs = [q for cc, q in sample if cc is c]
            ts += graph_features(c, qs, arm)[1]
        res[f"graph over {SHORT[arm]}"] = ts
        print(f"graph over {SHORT[arm]} K=50: n={len(ts)} p50 {1000 * statistics.median(ts):.1f} ms, "
              f"p95 {1000 * pctl(ts, 0.95):.1f} ms; {os.popen('uptime').read().strip()}", flush=True)
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=Path)
    ap.add_argument("--eval-dir", type=Path, default=Path(__file__).resolve().parent)
    ap.add_argument("--corpora", default="")
    ap.add_argument("--max-queries", type=int, default=0)
    ap.add_argument("--json", type=Path)
    ap.add_argument("--table", type=Path, help="write the compact markdown summary tables here")
    ap.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[2])
    ap.add_argument("--latency", type=int, default=0,
                    help="only time the K=50 rerank of N seeded-sampled queries per reranker (no cache)")
    a = ap.parse_args()
    toml = (a.eval_dir / "variants.toml").read_text()
    for k in ("bootstrap_seed", "bootstrap_resamples"):
        SETTINGS[k] = int(re.search(rf"^{k}\s*=\s*(\d+)", toml, re.M).group(1))
    print(f"start: {os.popen('uptime').read().strip()}")
    corpora = [p.name for p in sorted((a.work / "runs" / REF).iterdir()) if (p / "rankings.jsonl").exists()]
    dry = bool(a.corpora) or bool(a.max_queries)
    if a.corpora:
        corpora = a.corpora.split(",")
    cs = [Corpus(a.work, a.eval_dir, c, a.max_queries) for c in corpora]
    if a.latency:
        return latency_only(a.work, cs, a.latency)
    print(f"corpora: {', '.join(corpora)}; queries {sum(len(c.qids) for c in cs)}; bootstrap seed "
          f"{SETTINGS['bootstrap_seed']}, resamples {SETTINGS['bootstrap_resamples']}")
    base = {arm: [outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                  for c in cs for q in c.qids] for arm in (REF,) + RECALL}
    cache_dir = a.work / "rerank_cache"
    cache_dir.mkdir(exist_ok=True)
    bad = []
    shipped = shipped_floors(a.repo)
    int8_fit = fit_floors(base[INT8])
    print(f"shipped floors (similarity.rs): {shipped}; int8 D6-fitted here: {int8_fit}")
    if not dry and int8_fit != shipped:
        bad.append("int8's D6-fitted floors differ from the shipped floors")

    variants = {}  # name -> {"arm", "K", "outs", "judge", "tuned", "meta"}

    def build(name, arm, K, score_fn, tuned=False, meta=None):
        outs, judge = [], []
        for c in cs:
            for q in c.qids:
                hits, top = reranked(c, q, K, score_fn(c, q), arm)
                outs.append(outcome(c, q, hits, top))
                judge.append(outcome(c, q, hits, dict(c.lists[arm][q])[hits[0][0]] if hits else None))
        variants[name] = {"arm": arm, "K": K, "outs": outs, "judge": judge, "tuned": tuned, "meta": meta or {}}
        return outs

    def cos(arm):
        return lambda c, q: [s for _, s in c.lists[arm][q]]

    # ---------------- 0. controls ----------------
    print("\n== 0. controls ==")
    want = {  # docs/results/gm-398-model-comparison.md: r@1, r@5, r@10 [lo, hi], MRR [lo, hi], CW comb/pos/abs
        REF: (0.335, 0.542, 0.633, 0.595, 0.670, 0.427, 0.395, 0.460, 47.9, 52.6, 26.1),
        GTE: (0.275, 0.478, 0.580, 0.540, 0.617, 0.373, 0.340, 0.406, 59.8, 61.9, 50.0),
        INT8: (0.328, 0.532, 0.637, 0.600, 0.675, 0.424, 0.392, 0.457, 46.4, 51.6, 21.7),
    }
    want_fl = {REF: ([.56, .58, .56, .55], [18.8, 9.5, 14.3, 14.8]),
               GTE: ([.86, .86, .85, .84], [25.0, 7.7, 0.0, 5.0]),
               INT8: ([.57, .57, .55, .53], [29.4, 10.0, 14.3, 7.7])}
    want_v = {GTE: (-5.3, -9.2, -0.055, -0.086, ("typescript", -9.0), 11.1, 16.7, -10.3, 0.0),
              INT8: (0.5, -1.0, -0.003, -0.012, ("go", -1.0), -1.4, 1.0, 2.1, 6.2)}

    def row398(o):
        r1 = pooled(o, hit_at(1), scored)[0]
        r5 = pooled(o, hit_at(5), scored)[0]
        b10 = bootstrap(pooled(o, hit10, scored)[1])
        bm = bootstrap(pooled(o, rr, scored)[1])
        fl = fit_floors(o)
        cwr = [100 * rate(o, lambda x: cw_indicator(x, fl), k)
               for k in (lambda x: True, lambda x: x["positive"], lambda x: not x["positive"])]
        got = (round(r1, 3), round(r5, 3), round(b10[0], 3), round(b10[1], 3), round(b10[2], 3),
               round(bm[0], 3), round(bm[1], 3), round(bm[2], 3), *[round(x, 1) for x in cwr])
        ff = [fl.get(l) for l in LANGS]
        fa = [round(100 * (rate(o, lambda x: fa_indicator(x, fl), lambda x, l=l: x["language"] == l) or 0), 1)
              for l in LANGS]
        return got, (ff, fa)

    def vrow(o):
        g = gates(base[REF], o)
        return (round(100 * g["r10"][0], 1), round(100 * g["r10"][1], 1), round(g["mrr"][0], 3),
                round(g["mrr"][1], 3), (g["q3_worst"][0], round(100 * g["q3_worst"][1], 1)),
                round(100 * g["cw"][0], 1), round(100 * g["cw"][2], 1),
                *((round(100 * g["fa"][0], 1), round(100 * g["fa"][2], 1) + 0.0) if g["fa"] else (None, None)))

    ident, shuf = {}, {}
    for arm in RECALL:
        for K in KS:  # identity must equal the stored arm query by query, on any subset
            ident[arm, K] = build(f"identity over {SHORT[arm]} K={K}", arm, K, cos(arm))
            diff = sum(1 for x, y in zip(ident[arm, K], base[arm])
                       if (x["rank"], x["top"], x["top_language"]) != (y["rank"], y["top"], y["top_language"]))
            print(f"0a identity over {SHORT[arm]} K={K}: (rank, top score, top language) differs from stored on "
                  f"{diff}/{len(ident[arm, K])} queries")
            if diff:
                bad.append(f"0a identity {arm} K={K}")
        rng = random.Random(443)
        shuf[arm] = build(f"shuffled over {SHORT[arm]} K=50", arm, 50, lambda c, q, rng=rng: [rng.random() for _ in c.lists[arm][q]])
    if not dry:
        rows = [("jina fp32 (stored)", base[REF], REF)]
        for arm in RECALL:
            rows += [(f"{SHORT[arm]} (stored)", base[arm], arm)] + [(f"identity over {SHORT[arm]} K={K}", ident[arm, K], arm) for K in KS]
        for label, o, arm in rows:
            got, flfa = row398(o)
            ok = got == want[arm] and flfa == want_fl[arm]
            print(f"0b {label}: r@1/r@5/r@10[lo,hi]/MRR[lo,hi]/CW comb,pos,abs {got}; floors/FA {flfa}: "
                  f"{'MATCH' if ok else 'MISMATCH ' + str((want[arm], want_fl[arm]))}")
            if not ok:
                bad.append(f"0b {label}")
            if arm != REF:
                gv = vrow(o)
                ok = gv == want_v[arm]
                print(f"0c {label} D9 row vs fp32 {gv}: {'MATCH' if ok else 'MISMATCH ' + str(want_v[arm])}")
                if not ok:
                    bad.append(f"0c {label}")
        for arm in RECALL:
            got, _ = row398(shuf[arm])
            g, gi = gates(base[REF], shuf[arm]), gates(base[INT8], shuf[arm], rf=shipped)
            fails_ = got != want[arm] and not g["pass"]["Q1"] and not gi["pass"]["Q1"]
            print(f"0d shuffled over {SHORT[arm]} K=50: {got}; D9 vs fp32 {vrow(shuf[arm])}; Q1 pass vs fp32 "
                  f"{g['pass']['Q1']}, vs int8 {gi['pass']['Q1']}: {'FAILS as it must' if fails_ else 'DID NOT FAIL'}")
            if not fails_:
                bad.append(f"0d shuffled over {arm} did not fail")
    else:
        for arm in RECALL:
            print(f"0d shuffled over {SHORT[arm]} (dry): NL r@10 {f3(summarize(shuf[arm])['nl']['r10'])} "
                  f"vs stored {f3(summarize(base[arm])['nl']['r10'])}")
    if bad:
        print("CONTROL FAILED:\n  " + "\n  ".join(bad))
        sys.exit(2)
    print("CONTROL OK" + (" (dry run: GM-398 values not checked on a subset)" if dry else ""), flush=True)

    # ---------------- 1. rerankers ----------------
    fit_keys = [(c, q) for c in cs for q in c.qids if fit_nl(c.queries[q])]

    def tune(score_of, params, K, arm):
        """Objective on the FIT half of NL positives: pooled r@10, then MRR; ties -> smaller params."""
        best = None
        for p in params:
            g10, gm = {}, {}
            for c, q in fit_keys:
                hits, _ = reranked(c, q, K, score_of(c, q, p), arm)
                r = first_rank(c, q, hits)
                lang = c.queries[q]["language"]
                g10.setdefault(lang, []).append(1.0 if r and r <= 10 else 0.0)
                gm.setdefault(lang, []).append(1.0 / r if r else 0.0)
            key = (round(pooled_mean(g10), 9), round(pooled_mean(gm), 9), -sum(abs(x) for x in np.atleast_1d(p)))
            if best is None or key > best[0]:
                best = (key, p)
        return best[1], best[0]

    ce_meta = {}
    for name in CES:
        t0 = time.time()
        sc, files, threads, npairs = ce_scores(a.work, cs, name, cache_dir)
        ce_meta[name] = {"hf": CES[name][0], "revision": CES[name][1], "sha256": files,
                         "ort_intra_op_threads(0=default)": threads}
        print(f"{name}: {npairs} uncached pairs scored in {time.time() - t0:.0f}s", flush=True)
        for arm in RECALL:
            S = lambda c, q, sc=sc, arm=arm: np.array([sc[c.name][q][n] for n, _ in c.lists[arm][q]])
            G = lambda c, q, arm=arm: np.array(cos(arm)(c, q))
            for K in KS:
                build(f"{name} over {SHORT[arm]} K={K}", arm, K, S)
                beta, key = tune(lambda c, q, b, S=S, G=G: S(c, q) + b * G(c, q), BETAS, K, arm)
                build(f"{name}+{SHORT[arm]} over {SHORT[arm]} K={K} (beta={beta:g})", arm, K,
                      lambda c, q, b=beta, S=S, G=G: S(c, q) + b * G(c, q), tuned=True, meta={"beta": beta, "fit": key})
                print(f"  {name}+{SHORT[arm]} K={K}: beta {beta:g} (fit r@10 {key[0]:.3f}, MRR {key[1]:.3f})")

    grid = [np.array([x, y, z]) for x in GRID_A for y in GRID_B for z in GRID_C]
    for arm in RECALL:
        feats = {c.name: graph_features(c, c.qids, arm)[0] for c in cs}
        for K in KS:
            GF = lambda c, q, p, K=K, arm=arm, feats=feats: np.array(cos(arm)(c, q)[:K]) + feats[c.name][q][K] @ p
            w, key = tune(GF, grid, K, arm)
            build(f"graph over {SHORT[arm]} K={K} (a,b,c={w[0]:g},{w[1]:g},{w[2]:g})", arm, K,
                  lambda c, q, w=w, GF=GF: GF(c, q, w), tuned=True, meta={"w": [float(x) for x in w], "fit": key})
            print(f"  graph over {SHORT[arm]} K={K}: weights {[float(x) for x in w]} (fit r@10 {key[0]:.3f}, MRR {key[1]:.3f})")

    # ---------------- 2. report ----------------
    res = {"models": ce_meta, "shipped_floors": shipped, "base": {}, "variants": {}}

    def evaluate(outs, tuned, arm=None, own_floors=None):
        keep = ho if tuned else scored
        s = summarize(outs)
        fl = own_floors or fit_floors(outs)
        e = {"summary": s, "keep": "held-out NL" if tuned else "all NL", "floors": fl,
             "r10_ci": bootstrap(pooled(outs, hit10, keep)[1]), "mrr_ci": bootstrap(pooled(outs, rr, keep)[1]),
             "gm434": {k: gm434(outs, fl, f) for k, f in (("nl_heldout", nl_ho_all), ("name", name_all))},
             "gm434_lang": {l: {k: gm434(outs, fl, f, l) for k, f in (("nl_heldout", nl_ho_all), ("name", name_all))}
                            for l in LANGS}}
        if arm is not None:  # share of the arm's top-K answers the reranker keeps in its top 10
            e["ceiling"] = {K: pooled(base[arm], hit_at(K), keep)[0] for K in KS}
        e["vs"] = {}
        if outs is not base[REF]:
            e["vs"]["fp32"] = gates(base[REF], outs, keep)
        if outs is not base[INT8]:
            e["vs"]["int8"] = gates(base[INT8], outs, keep, rf=shipped)
        return e

    rows = [("jina fp32 (baseline)", None, "-", evaluate(base[REF], False)),
            ("jina int8 (baseline, shipped floors)", None, "-", evaluate(base[INT8], False, own_floors=shipped)),
            ("gte (stored)", None, "-", evaluate(base[GTE], False))]
    for arm, (label, _, _, e) in zip((REF, INT8, GTE), rows):
        res["base"][arm] = e
    for name, v in variants.items():
        e = evaluate(v["outs"], v["tuned"], v["arm"])
        e["judge_arm_cosine_floor"] = {k: {x: g[x] for x in ("cw", "fa", "floors", "pass")}
                                       for k, g in (("fp32", gates(base[REF], v["judge"], ho if v["tuned"] else scored)),)}
        e["meta"] = v["meta"]
        res["variants"][name] = e
        rows.append((name, v["arm"], v["K"], e))

    def ci(t, scale=1.0, d=3):
        return "-" if t is None else f"{scale * t[0]:.{d}f} [{scale * t[1]:.{d}f}, {scale * t[2]:.{d}f}]"

    def gate_cells(g):
        if g is None:
            return ["-"] * 6
        fails = [q for q in ("Q1", "Q2", "Q3", "Q4", "Q5") if not g["pass"][q]]
        up = lambda t: "-" if t is None else f"{pts(t[0])} [{pts(t[2])}]"
        return [f"{pts(g['r10'][0])} [{pts(g['r10'][1])}]", f"{g['mrr'][0]:+.3f} [{g['mrr'][1]:+.3f}]",
                f"{g['q3_worst'][0]} {pts(g['q3_worst'][1])}", up(g["cw"]), up(g["fa"]),
                "pass" if not fails else "**" + ",".join(fails) + "**"]

    def c434(t):
        return "-" if not t[1] else f"{100 * t[0] / t[1]:.0f}% ({t[0]}/{t[1]})"

    L = []
    L.append(f"Queries: {sum(len(c.qids) for c in cs)} over {', '.join(corpora)}. Tuned variants (beta, graph weights) "
             "are scored and gated on the held-out NL half only; the others on all NL positives. "
             "Bounds are one-sided 95% (SplitMix64 bootstrap, languages weighted equally).\n")
    L.append("### Quality\n")
    L.append("| variant | K | on | NL r@10 [lo, hi] | NL MRR [lo, hi] | kept of recall@K | held-out r@10 / MRR (n) | name r@10 / MRR |")
    L.append("|---|---|---|---|---|---|---|---|")
    for label, arm, K, e in rows:
        s, key = e["summary"], "nl_heldout" if e["keep"] == "held-out NL" else "nl"
        kept = "-" if arm is None else f"{s[key]['r10'] / e['ceiling'][K]:.2f} of {e['ceiling'][K]:.3f}"
        L.append(f"| {label} | {K} | {e['keep']} | {ci(e['r10_ci'])} | {ci(e['mrr_ci'])} | {kept} | "
                 f"{f3(s['nl_heldout']['r10'])} / {f3(s['nl_heldout']['mrr'])} ({s['nl_heldout']['n']}) | "
                 f"{f3(s['name']['r10'])} / {f3(s['name']['mrr'])} |")
    for tag, title in (("fp32", "jina fp32 at its D6 floors"), ("int8", "shipped jina int8 at its shipped floors")):
        L.append(f"\n### D9 gates vs {title}\n")
        L.append("Δ = variant - baseline, points (MRR absolute). Q1 Δr@10 [lo], Q2 ΔMRR [lo], Q3 worst language, "
                 "Q4 Δ confident-wrong [up], Q5 Δ false alarm [up] (each arm at its own floors; paired held-out rank-1 positives).\n")
        L.append("| variant | Q1 Δr@10 [lo] | Q2 ΔMRR [lo] | Q3 worst | Q4 ΔCW [up] | Q5 ΔFA [up] | fails | Q5 per language (Δ, n) |")
        L.append("|---|---|---|---|---|---|---|---|")
        for label, arm, K, e in rows:
            g = e["vs"].get(tag)
            fl = "-" if g is None else ", ".join(f"{l[:2]} {pts(d)} n={n}" for l, (d, n) in g["fa_lang"].items())
            L.append(f"| {label} | " + " | ".join(gate_cells(g)) + f" | {fl} |")
    L.append("\n### GM-434 columns (option a: below the floor is noMatch), at each row's own floors\n")
    L.append("misled = rank-1 positives the floor calls no-match; CW = clears the floor with a wrong top (positives) / at all (absent). "
             "NL = held-out NL queries; name = mechanical name queries.\n")
    L.append("| variant | floors go/py/rs/ts | NL misled | NL CW pos | NL CW absent | name misled | name CW pos | name CW absent |")
    L.append("|---|---|---|---|---|---|---|---|")
    for label, arm, K, e in rows:
        fl, g = e["floors"], e["gm434"]
        L.append(f"| {label} | {' / '.join(f'{fl[l]:.2f}' if l in fl else '-' for l in LANGS)} | "
                 f"{c434(g['nl_heldout']['misled'])} | {c434(g['nl_heldout']['cw_pos'])} | {c434(g['nl_heldout']['cw_abs'])} | "
                 f"{c434(g['name']['misled'])} | {c434(g['name']['cw_pos'])} | {c434(g['name']['cw_abs'])} |")
    for key, title in (("nl_heldout", "held-out NL"), ("name", "name")):
        L.append(f"\n### GM-434 per language, {title} (misled / CW pos / CW absent)\n")
        L.append("| variant | " + " | ".join(LANGS) + " |")
        L.append("|---|" + "---|" * len(LANGS))
        for label, arm, K, e in rows:
            gl = e["gm434_lang"]
            L.append(f"| {label} | " + " | ".join(
                f"{c434(gl[l][key]['misled'])} / {c434(gl[l][key]['cw_pos'])} / {c434(gl[l][key]['cw_abs'])}" for l in LANGS) + " |")
    table = "\n".join(L)
    print("\n" + table)
    print(f"\nend: {os.popen('uptime').read().strip()}")
    print("models: " + json.dumps(ce_meta))
    if a.table:
        a.table.write_text(table + "\n")
    if a.json:
        a.json.write_text(json.dumps(res, indent=1, default=str))


if __name__ == "__main__":
    main()
