#!/usr/bin/env python3
"""GM-463: off-the-shelf cross-encoder bake-off, quality x offline CPU latency.

Setup as GM-698's ship arm: int8-structured's top-K reordered by
ce(text) + beta * cos, verdict on int8-structured's own top-1 cosine at the
shipped floors. Every candidate reads the ship text (E+P+B48, gm462_ce_text)
with its 48-token body budget counted by the candidate's own tokenizer.
MiniLM-L6 (`ce-minilm`) is the reference: its off text at beta 80 and its
ship text at its FIT-tuned beta, read from the GM-698 caches. The design is
docs/design/GM-463-reranker-bakeoff.md.

Modes:
  quality      score the missing ship pairs of each --models entry, run the
               controls, tune beta on FIT NL, report held-out NL rows, paired
               deltas vs ship-L6 and off, the doc split, flips, the window
               ceiling and mean pair tokens at every --report-k.
               --dry-run-pairs N: controls C1/C8/C7, then N new ship pairs per
               model, ms/pair and the ETA of the model's missing pairs; stop.
               --controls-only: C1, C5, C8, C7, C3' per model; stop.
  latency      every --models arm in a fresh process under /usr/bin/time -p,
               in mirrored order (a b c c b a), each behind the load gate and
               pmset CPU_Speed_Limit == 100; writes arms.jsonl + summary.md.
  latency-arm  one model (internal; prints one JSON line).

Scores are cached per (query, sha16(text)) in
<cache-dir>/gm463-<model>.<corpus>.<key>.json; `ce-minilm` reads the GM-698
file (gm462-ce-minilm.*) as its base and writes only its gm463 file.

Controls (a failure is a STOP):
  C0   (ce-minilm in --models, K=30) reproduces gm-698-ship-arm.md's rows;
  C1   build(off) == structured text on every window node, and the Rust fixture;
  C3'  200 cached ship pairs rescored in other batches agree (1e-5 ce-minilm, 1e-4 others);
  C5   rerank disabled == int8-structured on every outcome and cell;
  C7   the model card's example pairs give the card's logits (jina: rank only);
  C8   ONNX input names, the pair template, no pad ids unpadded, 512-token truncation.

usage: gm463_bakeoff.py quality --work W --eval-dir eval/embedding/v2
         --st-runs runs/jina-v2-base-code-int8-structured --models ce-minilm,ce-gte-mb
         [--k 30] [--report-k 20,30] [--scope ce-bge-m3=nl] [--dry-run-pairs N]
         [--controls-only] [--table out.md] [--summary out.txt]
       gm463_bakeoff.py latency --work W --eval-dir eval/embedding/v2 --models ce-minilm,ce-minilm-l12
         [--ks 20,30,50] [--passes 3] [--out DIR]
       gm463_bakeoff.py latency-arm --work W --eval-dir eval/embedding/v2 --model X [--ks ..] [--passes N]
"""
import argparse
import hashlib
import json
import os
import random
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import rerank_eval as R  # noqa: E402
import gm462_ce_text as T  # noqa: E402
import gm462_ce_enriched as E  # noqa: E402
import gm464_latency as LAT  # noqa: E402
from gm464_ce_structured import c1_fixture, flips, render  # noqa: E402

# E.Progress reads E.PROGRESS when it writes, so this redirects every stage line.
E.PROGRESS = Path(os.environ.get("GM_PROGRESS", Path.home() / ".claude/progress/GM-463-S4.txt"))
LAT.PROGRESS = Path(os.environ.get("PROGRESS", Path.home() / ".claude/progress/GM-463-S5.txt"))

ST = E.ST
REF = "ce-minilm"
SHIP_BETA = 40.0  # GM-698's FIT-tuned beta of the shipped arm
OFF_BETA = E.SHIPPED_BETA
MODELS = [REF] + list(R.CE_BAKEOFF)
RUST_SCORED = {"ce-minilm-quint8-rust"}  # logits come from the Rust dump (gm700_rust_pairs.py), never from ORT here
NOT_SHIPPABLE = {"ce-jina-v2": "CC-BY-NC-4.0, reference only"}
SCOPE_DEFAULT = {"ce-bge-m3": "nl"}
PASSES_DEFAULT = {"ce-bge-m3": 1}
BETA_MAX, BETA_MIN = 2560.0, 0.125
# Section 3.5's latency order L6 L12 turbo gte jv2 bge; the arms run it, then mirrored.
LAT_ORDER = ["ce-minilm", "ce-minilm-int8", "ce-minilm-quint8", "ce-minilm-quint8-rust", "ce-minilm-l12", "ce-jina-turbo", "ce-gte-mb", "ce-gte-mb-int8", "ce-jina-v2", "ce-bge-m3"]
L6_PAIR_TOKENS = 101.9  # gm-698-latency.md: enr mean pair tokens, same 30 queries, K=30, one decimal
assert sorted(MODELS) == sorted(LAT_ORDER), (MODELS, LAT_ORDER)
C3_PAIRS = 200
C3_TOL = {REF: 1e-5, "ce-minilm-int8": 1e-5, "ce-minilm-quint8": 1e-5, "ce-gte-mb-int8": 1e-5}  # int8 at chunk 1: exact
C3_TOL_DEFAULT = 1e-4
BERT_PAIR = "[CLS] A [SEP] B [SEP]"
ROBERTA_PAIR = "<s> A </s> </s> B </s>"
BERT_INPUTS = {"input_ids", "attention_mask", "token_type_ids"}
MASK_INPUTS = {"input_ids", "attention_mask"}
C8_SPEC = {  # name -> (ONNX input names, special-token layout of an encoded pair)
    "ce-minilm": (BERT_INPUTS, BERT_PAIR),
    "ce-minilm-l12": (BERT_INPUTS, BERT_PAIR),
    "ce-minilm-int8": (BERT_INPUTS, BERT_PAIR),
    "ce-minilm-quint8": (BERT_INPUTS, BERT_PAIR),
    "ce-minilm-quint8-rust": (BERT_INPUTS, BERT_PAIR),
    "ce-jina-turbo": (MASK_INPUTS, ROBERTA_PAIR),
    "ce-jina-v2": (MASK_INPUTS, ROBERTA_PAIR),
    "ce-gte-mb": (MASK_INPUTS, BERT_PAIR),
    "ce-gte-mb-int8": (MASK_INPUTS, BERT_PAIR),
    "ce-bge-m3": (MASK_INPUTS, ROBERTA_PAIR),
}
_SKIN_Q = "Organic skincare products for sensitive skin"
# name -> ([(query, doc)], printed logits or None, tolerance, indices that must take the top ranks when logits
# are None: jina-v2's are the same skincare document in five languages, the others are makeup trends).
# Pairs and logits are the model cards' usage examples at the pinned revisions.
C7_CARDS = {
    "ce-minilm-l12": ([("How many people live in Berlin?",
                        "Berlin had a population of 3,520,031 registered inhabitants in an area of 891.82 square kilometers."),
                       ("How many people live in Berlin?", "Berlin is well known for its museums.")],
                      [9.218911, -4.0780287], 1e-3, None),
    "ce-gte-mb": ([("what is the capital of China?", "Beijing"),
                   ("how to implement quick sort in python?", "Introduction of quick sort"),
                   ("how to implement quick sort in python?", "The weather is nice today")],
                  [2.1387, 2.4609, -1.6729], 0.05, None),
    # int8: the card logits' 0.05 tolerance is fp32's, so rank only (the card's best document is index 1)
    "ce-gte-mb-int8": ([("what is the capital of China?", "Beijing"),
                        ("how to implement quick sort in python?", "Introduction of quick sort"),
                        ("how to implement quick sort in python?", "The weather is nice today")],
                       None, None, {0, 1}),
    "ce-bge-m3": ([("what is panda?", "hi"),
                   ("what is panda?", "The giant panda (Ailuropoda melanoleuca), sometimes called a panda bear or "
                                      "simply panda, is a bear species endemic to China."),
                   ("query", "passage")],
                  [-8.1875, 5.26171875, -5.65234375], 0.05, None),
    "ce-jina-turbo": ([(_SKIN_Q, d) for d in (
        "Eco-friendly kitchenware for modern homes", "Biodegradable cleaning supplies for eco-conscious consumers",
        "Organic cotton baby clothes for sensitive skin", "Natural organic skincare range for sensitive skin",
        "Tech gadgets for smart homes: 2024 edition", "Sustainable gardening tools and compost solutions",
        "Sensitive skin-friendly facial cleansers and toners", "Organic food wraps and storage solutions",
        "All-natural pet food for dogs with allergies", "Yoga mats made from recycled materials")],
        None, None, {3}),
    "ce-jina-v2": ([(_SKIN_Q, d) for d in (
        "Organic skincare for sensitive skin with aloe vera and chamomile.",
        "New makeup trends focus on bold colors and innovative techniques",
        "Bio-Hautpflege für empfindliche Haut mit Aloe Vera und Kamille",
        "Neue Make-up-Trends setzen auf kräftige Farben und innovative Techniken",
        "Cuidado de la piel orgánico para piel sensible con aloe vera y manzanilla",
        "Las nuevas tendencias de maquillaje se centran en colores vivos y técnicas innovadoras",
        "针对敏感肌专门设计的天然有机护肤产品", "新的化妆趋势注重鲜艳的颜色和创新的技巧",
        "敏感肌のために特別に設計された天然有機スキンケア製品",
        "新しいメイクのトレンドは鮮やかな色と革新的な技術に焦点を当てています")],
        None, None, {0, 2, 4, 6, 8}),
}


def sha16(x):
    return hashlib.sha256(json.dumps(x).encode()).hexdigest()[:16]


def model_dir(work, name):
    return work / "models" / R.CE_DIR[name]


def counter_tokenizer(work, name):
    """The model's own tokenizer for the B48 budget: no truncation and no padding,
    so counts above 512 are not capped (T.token_counter's precondition)."""
    from tokenizers import Tokenizer
    tk = Tokenizer.from_file(str(model_dir(work, name) / "tokenizer.json"))
    tk.no_truncation()
    tk.no_padding()
    return tk


# ---- cache ---------------------------------------------------------------------
class ModelCache(E.EnrichedCache):
    """Logits per corpus, {qid: {sha16(text): logit}} in gm463-<name>.<corpus>.<key>.json.
    Lookups try this file, then `base` (an E.EnrichedCache, for ce-minilm only); only
    this file is ever written."""

    def __init__(self, cache_dir, cs, name, files, text_tok_sha, base=None):
        self.dir, self.name, self.base_cache = cache_dir, name, base
        ids = [files["model.onnx"]] + ([files["model.onnx_data"]] if "model.onnx_data" in files else [])
        self.key = sha16(ids + [files["tokenizer.json"], R.MAX_TOKENS, text_tok_sha, "gm463 text-keyed v1"])
        self.d = {}
        for c in cs:
            f = self.file(c)
            self.d[c.name] = json.loads(f.read_text()) if f.exists() else {}

    def file(self, c):
        return self.dir / f"gm463-{self.name}.{c.name}.{self.key}.json"

    def has(self, c, q, h):
        return h in self.d[c.name].get(q, {}) or (self.base_cache is not None and self.base_cache.has(c, q, h))

    def get(self, c, q, text):
        own = self.d[c.name].get(q, {})
        h = E.tsha(text)
        if h in own:
            return own[h]
        if self.base_cache is None:
            raise KeyError((c.name, q, h))
        return self.base_cache.get(c, q, text)


def file_shas(work, name):
    d = model_dir(work, name)
    out = {f: R.sha256_file(d / f) for f in ("model.onnx", "tokenizer.json")}
    if (d / "model.onnx_data").exists():
        out["model.onnx_data"] = R.sha256_file(d / "model.onnx_data")
    return out


def open_cache(cache_dir, cs, work, name, files):
    base = E.EnrichedCache(cache_dir, cs, files["model.onnx"]) if name == REF else None
    return ModelCache(cache_dir, cs, name, files, files["tokenizer.json"], base)


# ---- texts and pairs -----------------------------------------------------------
def build_model_texts(cs, work, name, arms):
    """c.atext_by[name][arm][nid], counted with the model's own tokenizer."""
    tok = T.token_counter(counter_tokenizer(work, name))
    for c in cs:
        E.build_texts(c, arms, tok, T.BODY_BUDGET)
        c.atext_by = getattr(c, "atext_by", {})
        c.atext_by[name] = c.atext
    return tok


def use_texts(cs, name):
    for c in cs:
        c.atext = c.atext_by[name]


def nl_only(c, q):
    return not c.queries[q]["mechanical"]


def in_model_scope(scope, c, q):
    return scope == "all" or nl_only(c, q)


def model_need(cs, cache, name, arm, k, scope):
    use_texts(cs, name)
    need = E.need_pairs(cs, cache, arm, k, set())
    byname = {c.name: c for c in cs}
    return {key: v for key, v in need.items() if in_model_scope(scope, byname[key[0]], key[1])}


def score_of_for(cache, name, arm):
    return lambda c, q, nid: cache.get(c, q, c.atext_by[name][arm][nid])


def f4s(cs, k, beta, score_of, scope="all"):
    """E.f4; with scope nl, name queries keep int8-structured's order."""
    if scope == "all":
        return E.f4(cs, k, beta, score_of)
    outs = []
    for c in cs:
        for q in c.qids:
            lst = c.lists[ST][q]
            n = min(k, len(lst))
            if nl_only(c, q):
                s = np.array([score_of(c, q, nid) for nid, _ in lst[:n]]) + beta * np.array([x for _, x in lst[:n]])
            else:
                s = -np.arange(n, dtype=np.float64)
            hits, _ = R.reranked(c, q, k, s, ST)
            o = R.outcome(c, q, hits, lst[0][1] if lst else None)
            o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
            outs.append(o)
    return outs


def tune_beta(fit_keys, k, score_of):
    """FIT-optimal beta over R.BETAS (best FIT NL r@10, then MRR, then the smaller
    beta), with the edge rule: an optimum at the grid's top doubles the top up to
    BETA_MAX; an optimum at 0 halves the smallest positive beta down to BETA_MIN.
    Returns (beta, (FIT r@10, FIT MRR), the added betas)."""
    memo = {}

    def key(b):
        if b not in memo:
            memo[b] = E.fit_obj(fit_keys, k, score_of, b) + (-b,)
        return memo[b]

    grid, added = list(R.BETAS), []
    best = max(grid, key=key)
    while best == max(grid) and max(grid) < BETA_MAX:
        grid.append(max(grid) * 2)
        added.append(grid[-1])
        best = max(grid, key=key)
    while best == min(grid) == 0.0 and min(b for b in grid if b > 0) > BETA_MIN:
        grid.append(min(b for b in grid if b > 0) / 2)
        added.append(grid[-1])
        best = max(grid, key=key)
    return best, key(best)[:2], added


# ---- controls ------------------------------------------------------------------
def c8_input_format(ce, name):
    """ONNX input names; special-token layout of one pair; no pad id in unpadded
    encodings of unequal length; a 700-token pair truncated to R.MAX_TOKENS."""
    want_inputs, want_layout = C8_SPEC[name]
    got_inputs = {i.name for i in ce.sess.get_inputs()}
    enc = ce.tok.encode("where is the config parsed", "fn parse_config(path: &Path) -> Config")
    layout, prev = [], None
    for tid, tokstr, sid, special in zip(enc.ids, enc.tokens, enc.sequence_ids, enc.special_tokens_mask):
        part = tokstr if special else "AB"[sid]
        if special or part != prev:
            layout.append(part)
        prev = part
    layout = " ".join(layout)
    short, long_ = ce.tok.encode_batch([("q", "a"), ("a longer query here", "a much longer document text for the pair")])
    no_pad = len(short.ids) != len(long_.ids) and ce.pad_id not in short.ids and ce.pad_id not in long_.ids
    big = ce.tok.encode("query words", " ".join(["token"] * 700))
    trunc = len(big.ids) == R.MAX_TOKENS and big.special_tokens_mask[-1] == 1
    ok = got_inputs == want_inputs and layout == want_layout and no_pad and trunc
    print(f"C8 {name}: inputs {sorted(got_inputs)} (want {sorted(want_inputs)}); layout '{layout}' "
          f"(want '{want_layout}'); unpadded {no_pad}; 700-token pair -> {len(big.ids)} ids, last special {trunc}: "
          f"{'pass' if ok else 'FAIL'}", flush=True)
    return ok


C7_BATCH_TOL = 1e-4


def c7_batch_agreement(ce, name, pairs):
    """Each pair scored alone (no padding) vs every query's pairs as one batch
    (unequal lengths: padded and masked). Fails if no batch was padded."""
    single = [float(E.score_texts(ce, q, [d])[0]) for q, d in pairs]
    byq = {}
    for i, (q, d) in enumerate(pairs):
        byq.setdefault(q, []).append(i)
    batch, padded = [0.0] * len(pairs), 0
    for q, idx in byq.items():
        docs = [pairs[i][1] for i in idx]
        lens = {len(e.ids) for e in ce.tok.encode_batch([(q, d) for d in docs])}
        padded += len(idx) if len(lens) > 1 else 0
        for i, s in zip(idx, E.score_texts(ce, q, docs)):
            batch[i] = float(s)
    diff = max(abs(s - b) for s, b in zip(single, batch))
    ok = padded > 0 and diff <= C7_BATCH_TOL
    print(f"C7 {name} batch: {len(pairs)} pairs alone vs one batch per query ({padded} in padded batches): "
          f"max|diff| {diff:.3e} (tol {C7_BATCH_TOL}): {'pass' if ok else 'FAIL'}", flush=True)
    return ok, single, diff


def c7_known_answer(ce, name):
    # The shipped model has no card scores; its batch check uses jina-turbo's card pairs (English, 10 unequal docs).
    pairs = C7_CARDS[name if name in C7_CARDS else "ce-jina-turbo"][0]
    okb, got, db = c7_batch_agreement(ce, name, pairs)
    bs = f"batch max|diff| {db:.1e}"
    if name not in C7_CARDS:
        print(f"C7 {name}: n/a (no card scores; C0 covers the shipped model)", flush=True)
        return okb, f"n/a; {bs}"
    pairs, want, tol, first = C7_CARDS[name]
    if want is not None:
        diff = max(abs(g - w) for g, w in zip(got, want))
        ok = diff <= tol
        print(f"C7 {name}: got {[round(x, 5) for x in got]} card {want} max|diff| {diff:.4g} (tol {tol}): "
              f"{'pass' if ok else 'FAIL'}", flush=True)
        return ok and okb, f"max|diff| {diff:.4g} <= {tol}; {bs}"
    order = sorted(range(len(got)), key=lambda i: -got[i])
    ok = set(order[:len(first)]) == first
    print(f"C7 {name} (weaker: rank only): ranking {order}, logits {[round(x, 3) for x in got]}; "
          f"docs {sorted(first)} on top: {'pass' if ok else 'FAIL'}", flush=True)
    return ok and okb, f"rank only, docs {sorted(first)} on top; {bs}"


def c3p_rescore(cs, cache, ce, name, k, scope):
    """C3': C3_PAIRS seeded ship pairs; any the cache lacks are scored first, each
    query's missing pairs in one batch padded out with up to 15 other texts of the
    query's window, longest first (so it differs from every rescoring batch). Then
    every query's sample is rescored in two halves (other chunk composition and
    padding) and compared with the cached logit. A fresh pair whose query window
    has no other text is scored in the same batch twice and counted as untested."""
    pool = [(c, q, nid) for c in cs for q in c.qids if in_model_scope(scope, c, q) for nid, _ in c.lists[ST][q][:k]]
    sample = random.Random(463).sample(pool, min(C3_PAIRS, len(pool)))
    byq = {}
    for c, q, nid in sample:
        byq.setdefault((c, q), []).append(c.atext_by[name]["ship"][nid])
    fresh = untested = 0
    for (c, q), ts in byq.items():
        miss = [t for t in dict.fromkeys(ts) if not cache.has(c, q, E.tsha(t))]
        if miss:
            qtext = c.queries[q]["text"]
            win = [t for t in dict.fromkeys(c.atext_by[name]["ship"][nid] for nid, _ in c.lists[ST][q][:k]) if t not in miss]
            ntok = [len(e.ids) for e in ce.tok.encode_batch([(qtext, t) for t in win])]
            fill = [t for _, t in sorted(zip(ntok, win), key=lambda x: -x[0])][:max(0, 16 - len(miss))]
            cache.put(c.name, q, [E.tsha(t) for t in miss], E.score_texts(ce, qtext, miss + fill)[:len(miss)])
            cache.save(c)
            fresh += len(miss)
            untested += len(miss) if not fill else 0
    diffs, t0 = [], time.time()
    for (c, q), ts in byq.items():
        h = (len(ts) + 1) // 2
        for part in (ts[:h], ts[h:]):
            if part:
                got = E.score_texts(ce, c.queries[q]["text"], part)
                diffs += [abs(float(g) - cache.get(c, q, t)) for g, t in zip(got, part)]
    ms = 1000.0 * (time.time() - t0) / len(diffs)
    m, tol = max(diffs), C3_TOL.get(name, C3_TOL_DEFAULT)
    print(f"C3' {name}: {len(diffs)} ship pairs ({fresh} scored for the control in padded window batches, "
          f"{untested} of them untested: no other window text) rescored in halves: "
          f"max|diff| {m:.3e} (tol {tol}); {ms:.2f} ms/pair", flush=True)
    return m <= tol and untested < len(diffs), m, ms


def c16_control(cs, cache, ce, name, k, scope):
    """GM-700 batching control: the C3' sample rescored at chunk 16 (the query's sample
    plus up to 16 longest window texts) must DISAGREE with the cached chunk-1 logits
    (> 1e-3), or the chunk-1 default tests nothing. Returns (max|diff|, pairs)."""
    pool = [(c, q, nid) for c in cs for q in c.qids if in_model_scope(scope, c, q) for nid, _ in c.lists[ST][q][:k]]
    sample = random.Random(463).sample(pool, min(C3_PAIRS, len(pool)))
    byq = {}
    for c, q, nid in sample:
        byq.setdefault((c, q), []).append(c.atext_by[name]["ship"][nid])
    diffs = []
    for (c, q), ts in byq.items():
        qtext = c.queries[q]["text"]
        ts = list(dict.fromkeys(ts))
        win = [t for t in dict.fromkeys(c.atext_by[name]["ship"][nid] for nid, _ in c.lists[ST][q][:k]) if t not in ts]
        got = ce.score(qtext, ts + win[:max(0, 16 - len(ts))], chunk=16)
        diffs += [abs(float(g) - cache.get(c, q, t)) for g, t in zip(got, ts)]
    m = max(diffs)
    print(f"C3'-16 {name}: {len(diffs)} sample pairs rescored at chunk 16 vs cached chunk 1: max|diff| {m:.3e} "
          f"(must exceed 1e-3): {'pass' if m > 1e-3 else 'FAIL'}", flush=True)
    return m, len(diffs)


def top10_ids(cs, k, beta, sc):
    out = {}
    for c in cs:
        for q in c.qids:
            lst = c.lists[ST][q]
            n = min(k, len(lst))
            s_ = np.array([sc(c, q, nid) for nid, _ in lst[:n]]) + beta * np.array([x for _, x in lst[:n]])
            out[(c.name, q)] = tuple(h[0] for h in R.reranked(c, q, k, s_, ST)[0][:10])
    return out


def n_top10_diff(cs, a, b, keep):
    return sum(1 for c in cs for q in c.qids if keep(c, q) and a[(c.name, q)] != b[(c.name, q)])


def ho_nl_q(c, q):
    return nl_only(c, q) and bool(c.queries[q]["held_out"])


SKIP_GRID = [0.005, 0.01, 0.015, 0.02, 0.03, 0.04, 0.05, 0.06, 0.08, 0.10, 0.125, 0.15, 0.20, 0.25, 0.30]
SKIP_FIT_LOSS = 0.005  # design GM-700 2.2: FIT MRR loss <= 0.005
_QN_SEP = re.compile(r"::|[./#\\]")


def q2_rule(cs, k, m, scope, ref40, o40, r40, off, off_row, sc, sc_ref):
    """GM-700 Q2/Q5 ship rule for model m at beta 40 (lines of a markdown table) and the
    control that the int8 and fp32 reranked top-10 differ for some query."""
    d10, dmrr = E.paired(ref40, o40, R.ho)
    dn = E.paired(ref40, o40, R.name_q)[1]
    bars = {"held-out NL r@10 >= 0.647": r40["r10"][0] >= 0.647, "held-out NL MRR >= 0.425": r40["mrr"][0] >= 0.425,
            "D9 Q1-Q5 vs int8-structured": all(r40["vs"]["int8"]["pass"][q] for q in ("Q1", "Q2", "Q3", "Q4", "Q5")),
            f"name MRR {r40['name'][1]:.3f} >= GM-463 off {off_row['name'][1]:.3f}": r40["name"][1] >= off_row["name"][1],
            f"dMRR CI lo {dmrr[1]:+.4f} >= -0.015": dmrr[1] >= -0.015,
            f"dr@10 CI lo {100 * d10[1]:+.2f} pt >= -1.5": d10[1] >= -0.015}
    t_a, t_b = top10_ids(cs, k, SHIP_BETA, sc_ref), top10_ids(cs, k, SHIP_BETA, sc)
    n_ho, n_all = n_top10_diff(cs, t_a, t_b, ho_nl_q), n_top10_diff(cs, t_a, t_b, lambda c, q: True)
    allq = sum(len(c.qids) for c in cs)
    ctl = n_all > 0
    out = [f"{m} @ beta {SHIP_BETA:g}: r@10 {ci3(r40['r10'])}, MRR {ci3(r40['mrr'])}, name r@10 {r40['name'][0]:.3f} / MRR "
           f"{r40['name'][1]:.3f}; paired vs fp32: dr@10 {E.fmt_d(d10)}, dMRR {E.fmt_d(dmrr, True)}, name dMRR "
           f"{dn[0]:+.3f} [{dn[1]:+.3f}, {dn[2]:+.3f}]", "", "| check | result |", "|---|---|"]
    out += [f"| {k_} | {'pass' if v else 'FAIL'} |" for k_, v in bars.items()]
    out += [f"| **all** | **{'SHIP RULE PASSES' if all(bars.values()) else 'SHIP RULE FAILS'}** |", "",
            f"Control: queries whose reranked top-10 differs fp32 vs {m} at beta {SHIP_BETA:g}, K={k}: "
            f"{n_all}/{allq} (held-out NL {n_ho}); must be > 0: {'pass' if ctl else 'FAIL'}"]
    print(out[0], flush=True)
    return out


def skip_flags(cs, name_rule, m):
    """query -> skip? Rule (a): split_words(query) == split_words(top-1's last qualified-name
    segment). Rule (b): cos(top-1) - cos(top-2) >= m (m None: off)."""
    flags = {}
    for c in cs:
        for q in c.qids:
            lst, f = c.lists[ST][q], False
            if lst:
                if name_rule:
                    last = _QN_SEP.split(c.rows[lst[0][0]]["qualifiedName"] or "")[-1]
                    w = T.split_words(last)
                    f = bool(w) and T.split_words(c.queries[q]["text"]) == w
                if not f and m is not None and len(lst) > 1 and lst[0][1] - lst[1][1] >= m:
                    f = True
            flags[(c.name, q)] = f
    return flags


def f4_skip(cs, k, beta, sc, flags, keys=None):
    """E.f4 with the skipped queries left in int8-structured's order."""
    outs = []
    for c in cs:
        for q in c.qids:
            if keys is not None and (c, q) not in keys:
                continue
            lst = c.lists[ST][q]
            n = min(k, len(lst))
            if flags[(c.name, q)]:
                s_ = -np.arange(n, dtype=np.float64)
            else:
                s_ = np.array([sc(c, q, nid) for nid, _ in lst[:n]]) + beta * np.array([x for _, x in lst[:n]])
            hits, _ = R.reranked(c, q, k, s_, ST)
            o = R.outcome(c, q, hits, lst[0][1] if lst else None)
            o["top_language"] = c.node_lang.get(lst[0][0]) if lst else None
            outs.append(o)
    return outs


def fit_rr(outs):
    g10, gm = {}, {}
    for o in outs:
        g10.setdefault(o["language"], []).append(R.hit10(o))
        gm.setdefault(o["language"], []).append(R.rr(o))
    return R.pooled_mean(g10), R.pooled_mean(gm)


def skip_sim(cs, K, beta, sc, fit_keys, mkrow, base_st, r8s, label):
    """GM-700 4.3: skip -> int8-structured order, else blend. m picked on FIT NL, reported held-out."""
    L, S = [f"## Confident-skip simulation, {label}, K={K}, beta {beta:g}", ""], []
    fk = set(fit_keys)
    nofl = skip_flags(cs, False, None)
    fit0 = fit_rr(f4_skip(cs, K, beta, sc, nofl, fk))
    L += [f"FIT NL queries {len(fit_keys)}; no-skip FIT r@10 {fit0[0]:.4f}, MRR {fit0[1]:.4f}", "",
          "| m | FIT skip share | FIT r@10 | FIT MRR | loss | ok |", "|---|---|---|---|---|---|"]
    best = None
    for m in SKIP_GRID:
        fl = skip_flags(cs, False, m)
        share = sum(fl[(c.name, q)] for c, q in fit_keys) / len(fit_keys)
        r10, mrr = fit_rr(f4_skip(cs, K, beta, sc, fl, fk))
        ok = fit0[1] - mrr <= SKIP_FIT_LOSS and share > 0
        L.append(f"| {m:g} | {share:.3f} | {r10:.4f} | {mrr:.4f} | {fit0[1] - mrr:+.4f} | {'yes' if ok else 'no'} |")
        if ok and (best is None or share > best[1]):
            best = (m, share)
    m_star = best[0] if best else None
    L += ["", f"FIT-picked m = {m_star} (largest FIT skip share with MRR loss <= {SKIP_FIT_LOSS}); "
          f"grid {SKIP_GRID}", ""]
    S.append(f"skip-sim {label} K={K}: FIT-picked m {m_star}")
    arms = [("no skip (blend)", nofl), ("(a) exact-name only", skip_flags(cs, True, None)),
            (f"(b) margin m={m_star} only", skip_flags(cs, False, m_star) if m_star is not None else nofl),
            ("(a)+(b)", skip_flags(cs, True, m_star)),
            ("skip all (= int8-structured)", {k_: True for k_ in nofl})]
    outs = {n: f4_skip(cs, K, beta, sc, fl) for n, fl in arms}
    rows = [mkrow(n, outs[n]) for n, _ in arms]
    # controls: no rule fired == no skip; every query skipped == int8-structured (C5's twin)
    ctl1 = E.sig(mkrow("c", f4_skip(cs, K, beta, sc, skip_flags(cs, False, float("inf"))))) == E.sig(rows[0])
    ctl2 = outs[arms[-1][0]] == base_st and E.sig(rows[-1]) == E.sig(r8s)
    L += [f"Control m=+inf, name rule off == no-skip on every cell: {'pass' if ctl1 else 'FAIL'}",
          f"Control every query skipped == int8-structured on every outcome and cell: {'pass' if ctl2 else 'FAIL'}", ""]
    S.append(f"skip-sim controls: {ctl1} {ctl2}")
    if not (ctl1 and ctl2):
        print("\n".join(L))
        sys.exit("STOP: skip simulation controls failed")
    L += render(rows)
    L += ["", "| arm | skip share held-out NL | skip share name queries | skip share all queries | "
          "dr@10 vs no-skip, held-out NL | dMRR | name dMRR |", "|---|---|---|---|---|---|---|"]
    for n, fl in arms:
        def share(keep):
            v = [fl[(c.name, q)] for c in cs for q in c.qids if keep(c, q)]
            return f"{sum(v) / len(v):.3f} ({sum(v)}/{len(v)})"
        d10, dmrr = E.paired(outs[arms[0][0]], outs[n], R.ho)
        dn = E.paired(outs[arms[0][0]], outs[n], R.name_q)[1]
        L.append(f"| {n} | {share(ho_nl_q)} | {share(lambda c, q: not nl_only(c, q))} | {share(lambda c, q: True)} | "
                 f"{E.fmt_d(d10)} | {E.fmt_d(dmrr, True)} | {dn[0]:+.3f} [{dn[1]:+.3f}, {dn[2]:+.3f}] |")
    return L, S


# ---- report helpers ------------------------------------------------------------
def window_ceiling(cs, ks=(20, 30, 50)):
    """Share of held-out NL positives with an expected node in int8-structured's top K, pooled by language."""
    out = {}
    for K in ks:
        g = {}
        for c in cs:
            for q in c.qids:
                qq = c.queries[q]
                if qq["held_out"] and qq["positive"] and not qq["mechanical"]:
                    hit = any(nid in c.expected[q] for nid, _ in c.lists[ST][q][:K])
                    g.setdefault(qq["language"], []).append(1.0 if hit else 0.0)
        out[K] = (R.pooled_mean(g), sum(len(v) for v in g.values()))
    return out


def mean_pair_tokens(cs, ce, name, k, scope):
    n = []
    for c in cs:
        for q in c.qids:
            if in_model_scope(scope, c, q):
                ts = [c.atext_by[name]["ship"][nid] for nid, _ in c.lists[ST][q][:k]]
                n += [len(e.ids) for e in ce.tok.encode_batch([(c.queries[q]["text"], t) for t in ts])]
    return statistics.mean(n) if n else float("nan")


def ci3(t):
    return f"{t[0]:.3f} [{t[1]:.3f}, {t[2]:.3f}]"


def split_table(cs, ref_outs, arms):
    """Doc split of held-out NL positives: n, r@10 / MRR per arm, paired delta vs the reference arm."""
    lab = {(c.name, q): E.bucket_of(c, q) for c in cs for q in c.qids}
    L = ["| bucket | n | " + " | ".join(f"{a} r@10 / MRR" for a, _ in arms) + " | "
         + " | ".join(f"{a} - ship-L6 Δr@10 / ΔMRR" for a, _ in arms[1:]) + " |",
         "|---|---|" + "---|" * (2 * len(arms) - 1)]
    for b in ("documented", "signature-only", "mixed"):
        keep = lambda o, b=b: R.ho(o) and o["positive"] and lab[(o["corpus"], o["id"])] == b
        sel_ids = {o["id"] for o in ref_outs if keep(o)}
        if not sel_ids:
            L.append(f"| {b} | 0 |" + " - |" * (2 * len(arms) - 1))
            continue
        cells = []
        for _, outs in arms:
            s = [o for o in outs if o["id"] in sel_ids]
            cells.append(f"{R.pooled(s, R.hit10, lambda o: True)[0]:.3f} / {R.pooled(s, R.rr, lambda o: True)[0]:.3f}")
        ds = []
        for _, outs in arms[1:]:
            d10, dmrr = E.paired(ref_outs, outs, keep)
            ds.append(f"{E.fmt_d(d10)} / {E.fmt_d(dmrr, True)}")
        L.append(f"| {b} | {len(sel_ids)} | " + " | ".join(cells) + " | " + " | ".join(ds) + " |")
    return L


# ---- quality -------------------------------------------------------------------
def load_setup(a):
    work = a.work
    st_dir = Path(a.st_runs) if Path(a.st_runs).is_absolute() else work / a.st_runs
    corpora_dir = a.corpora or work / "corpora"
    toml = ((a.eval_dir / "variants.toml") if (a.eval_dir / "variants.toml").exists() else HERE / "variants.toml").read_text()
    for key in ("bootstrap_seed", "bootstrap_resamples"):
        R.SETTINGS[key] = int(re.search(rf"^{key}\s*=\s*(\d+)", toml, re.M).group(1))
    return st_dir, corpora_dir


def load_corpora(work, eval_dir, st_dir, corpora_dir, k, only=None):
    corpora = [p.name for p in sorted((work / "runs" / R.REF).iterdir()) if (p / "rankings.jsonl").exists()]
    cs = [R.Corpus(work, eval_dir, c, 0) for c in corpora if only is None or c in only]
    for c in cs:
        E.add_structured(c, st_dir)
        missing = E.load_nodes(c, corpora_dir, k)
        if missing:
            sys.exit(f"STOP: {missing} source files of {c.name} missing under {corpora_dir}")
    return cs


def quality(a):
    st_dir, corpora_dir = load_setup(a)
    cache_dir = a.cache_dir or a.work / "rerank_cache"
    K = a.k
    report_k = sorted({x for x in a.report_k if x <= K} | {K})
    S = [f"GM-463 quality: K={K}, report K {report_k}, eval dir {a.eval_dir}, st-runs {st_dir}, models {a.models}, "
         f"scope {a.scope}"]
    print(S[0], flush=True)
    c1_fixture()
    cs = load_corpora(a.work, a.eval_dir, st_dir, corpora_dir, K)

    # Reference (ce-minilm): off + ship texts, the GM-698 cache as base.
    build_model_texts(cs, a.work, REF, ["off", "ship"])
    use_texts(cs, REF)
    E.c1_off_is_base(cs)
    S.append("C1 pass")
    caches = {REF: open_cache(cache_dir, cs, a.work, REF, file_shas(a.work, REF))}
    ref_need = {arm: model_need(cs, caches[REF], REF, arm, K, "all") for arm in ("off", "ship")}
    nref = {arm: sum(len(v) for v in m.values()) for arm, m in ref_need.items()}
    print(f"ce-minilm reference pairs missing at K={K}: off {nref['off']}, ship {nref['ship']}", flush=True)
    if any(nref.values()) and REF not in a.models:
        sys.exit("STOP: the ce-minilm reference lacks pairs; add ce-minilm to --models to score them")

    ces, failed, ctl = {}, {}, {}
    for m in a.models:
        scope = a.scope[m]
        try:
            ce = R.CrossEncoder(a.work, m)
        except Exception as e:  # noqa: BLE001 - R5: a model ORT cannot load is dropped and quoted
            failed[m] = f"{type(e).__name__}: {e}"
            print(f"LOAD FAIL {m}: {failed[m]}", flush=True)
            S.append(f"LOAD FAIL {m}: {failed[m][:300]}")
            continue
        ces[m] = ce
        ok8 = c8_input_format(ce, m)
        ok7, c7s = c7_known_answer(ce, m)
        if not (ok8 and ok7):
            sys.exit(f"STOP: C8/C7 failed for {m}")
        S.append(f"{m}: C8 pass, C7 pass ({c7s})")
        if m != REF:
            build_model_texts(cs, a.work, m, ["ship"])
            caches[m] = open_cache(cache_dir, cs, a.work, m, ce.files)
        need = model_need(cs, caches[m], m, "ship", K, scope)
        if m == REF:
            for key, v in ref_need["off"].items():
                need.setdefault(key, {}).update(v)
        total = sum(len(v) for v in need.values())
        print(f"{m}: scope {scope}, ship pairs to score at K={K}: {total}; threads {ce.threads}; "
              f"load {os.getloadavg()}", flush=True)
        if a.dry_run_pairs is not None:
            if total and a.dry_run_pairs:
                n, sec, _ = E.score_missing(cs, caches[m], need, ce, f"dry run {m}", a.dry_run_pairs)
                ms = 1000.0 * sec / n
                line = (f"DRY {m}: {n} new ship pairs, {ms:.2f} ms/pair; ETA {total} pairs x {ms:.2f} ms = "
                        f"{total * ms / 1000:.0f}s ({total * ms / 3.6e6:.2f} h); load {os.getloadavg()}")
            else:
                line = f"DRY {m}: 0 pairs to score"
            print(line, flush=True)
            S.append(line)
            continue
        if m in RUST_SCORED:
            if total:
                sys.exit(f"STOP: {m} cache lacks {total} pairs; run gm700_rust_pairs.py import")
            S.append(f"{m}: cache complete (Rust-scored); C3'/C3'-16 not applicable (rescoring would be Python)")
            continue
        if total and not a.controls_only:
            n, sec, eta = E.score_missing(cs, caches[m], need, ce, f"CE scoring {m}")
            line = f"{m}: scored {n} pairs in {sec:.0f}s ({1000 * sec / n:.2f} ms/pair), ETA at 10% {eta or 0:.0f}s"
            print(line, flush=True)
            S.append(line)
        ok3, d3, ms3 = c3p_rescore(cs, caches[m], ce, m, K, scope)
        if not ok3:
            sys.exit(f"STOP: C3' failed for {m}: max|diff| {d3:.3e}")
        S.append(f"{m}: C3' max|diff| {d3:.3e}, {ms3:.2f} ms/pair")
        ctl[m] = ms3
        if m in R.INT8_MODELS:
            d16, n16 = c16_control(cs, caches[m], ce, m, K, scope)
            S.append(f"{m}: C3'-16 chunk 16 vs chunk 1 on {n16} pairs: max|diff| {d16:.3e} "
                     f"({'differs, control ok' if d16 > 1e-3 else 'DOES NOT DIFFER, control FAILED'})")

    shipped = R.shipped_floors(HERE.parents[1])
    base = {arm: [R.outcome(c, q, c.lists[arm][q], c.lists[arm][q][0][1] if c.lists[arm][q] else None)
                  for c in cs for q in c.qids] for arm in (R.REF, ST)}
    rf32 = R.fit_floors(base[R.REF])
    mkrow = lambda label, outs: E.row(label, outs, shipped, base[R.REF], rf32, base[ST], shipped)
    r8s = mkrow("int8-structured (no rerank, shipped floors)", base[ST])
    disabled = E.f4(cs, K, OFF_BETA, rerank=False)
    ok5 = disabled == base[ST] and E.sig(mkrow("off", disabled)) == E.sig(r8s)
    print(f"C5 rerank disabled == int8-structured on every outcome and cell: {ok5}", flush=True)
    if not ok5:
        sys.exit("STOP: C5 failed")
    S.append("C5 pass")
    if a.dry_run_pairs is not None or a.controls_only:
        S.append("DRY RUN / CONTROLS ONLY: stop before metrics")
        finish(a, S, [])
        return

    ceil = window_ceiling(cs)
    fit_keys = [(c, q) for c in cs for q in c.qids if R.fit_nl(c.queries[q])]
    strict = E.strict_ho(cs)
    sc_ref = {arm: score_of_for(caches[REF], REF, arm) for arm in ("off", "ship")}
    L = ["Window ceiling: held-out NL positives with an expected node in int8-structured's top K (pooled by language)", "",
         "| K | share | n |", "|---|---|---|"] + [f"| {k_} | {v:.3f} | {n} |" for k_, (v, n) in ceil.items()]
    p = E.Progress("metrics", len(report_k) * (len(ces) + 1))
    step = 0
    for k_ in report_k:
        rows, variants = [r8s], []
        off = f4s(cs, k_, OFF_BETA, sc_ref["off"])
        rows.append(mkrow(f"off (L6) K={k_} beta={OFF_BETA:g}", off))
        bref, fref, xref = tune_beta(fit_keys, k_, sc_ref["ship"])
        ship = f4s(cs, k_, bref, sc_ref["ship"])
        rows.append(mkrow(f"ship-L6 K={k_} beta={bref:g} (FIT-tuned)", ship))
        variants.append((f"ship-L6 K={k_} beta={bref:g}", ship))
        if bref != SHIP_BETA:
            rows.append(mkrow(f"ship-L6 K={k_} beta={SHIP_BETA:g}", f4s(cs, k_, SHIP_BETA, sc_ref["ship"])))
        S.append(f"K={k_}: ship-L6 FIT-tuned beta {bref:g} (FIT r@10 {fref[0]:.3f}, MRR {fref[1]:.3f}){' extended ' + str(xref) if xref else ''}")
        step += 1
        p.write(step)
        tok_lines = [f"| ce-minilm (ship-L6) | all | {mean_pair_tokens(cs, ces[REF], REF, k_, 'all'):.1f} |"] if REF in ces else []
        paired_lines, rule_lines, split_arms = [], [], [("ship-L6", ship), ("off", off)]
        ship_rule_lines = []
        off_row = rows[1]
        ship40 = ship if bref == SHIP_BETA else f4s(cs, k_, SHIP_BETA, sc_ref["ship"])
        for m in ces:
            if m == REF:
                step += 1
                continue
            scope = a.scope[m]
            sc = score_of_for(caches[m], m, "ship")
            bm, fm, xm = tune_beta(fit_keys, k_, sc)
            tag = (" (NL only; name queries: int8-structured order)" if scope == "nl" else "") + \
                  (f" [not shippable: {NOT_SHIPPABLE[m]}]" if m in NOT_SHIPPABLE else "")
            om = f4s(cs, k_, bm, sc, scope)
            rm = mkrow(f"{m} K={k_} beta={bm:g} (FIT-tuned){tag}", om)
            rows.append(rm)
            variants.append((f"{m} K={k_} beta={bm:g}", om))
            if bm != SHIP_BETA:
                o40 = f4s(cs, k_, SHIP_BETA, sc, scope)
                r40 = mkrow(f"{m} K={k_} beta={SHIP_BETA:g}{tag}", o40)
                rows.append(r40)
            else:
                o40, r40 = om, rm
            if m != REF:
                ship_rule_lines.append(q2_rule(cs, k_, m, scope, ship40, o40, r40, off, off_row, sc, sc_ref["ship"]))
            S.append(f"K={k_}: {m} FIT-tuned beta {bm:g} (FIT r@10 {fm[0]:.3f}, MRR {fm[1]:.3f})"
                     + (f"; grid extended by {xm}" + (" (optimum still at the edge)" if bm in (0.0, BETA_MAX) else "") if xm else ""))
            subsets = [("held-out NL", R.ho), ("strict held-out NL", strict)] + ([("name queries", R.name_q)] if scope == "all" else [])
            for ref_lab, ref_outs in (("ship-L6", ship), ("off", off)):
                for sub, keep in subsets:
                    d10, dmrr = E.paired(ref_outs, om, keep)
                    paired_lines.append(f"| {m} | {ref_lab} | {sub} | {E.fmt_d(d10)} | {E.fmt_d(dmrr, True)} |")
            d10, dmrr = E.paired(ship, om, R.ho)
            r1, r2 = dmrr[1] > 0, d10[1] >= -0.010
            r3 = all(rm["vs"]["int8"]["pass"][q] for q in ("Q1", "Q2", "Q3", "Q4", "Q5")) if scope == "all" else None
            r4 = E.paired(ship, om, R.name_q)[1][1] >= -0.010 if scope == "all" else None
            yn = lambda x: "n/a" if x is None else ("pass" if x else "FAIL")
            rule_lines.append(f"| {m} | {yn(r1)} | {yn(r2)} | {yn(r3)} | {yn(r4)} |")
            split_arms.append((m, om))
            tok_lines.append(f"| {m} | {scope} | {mean_pair_tokens(cs, ces[m], m, k_, scope):.1f} |")
            step += 1
            p.write(step)
        by8 = {o["id"]: o for o in base[ST]}
        L += ["", f"## K={k_}", ""] + render(rows) + flips(variants, by8, shipped)
        L += ["", f"Paired b minus a, K={k_}, each candidate at its FIT-tuned beta (ship-L6 at beta {bref:g}, off at {OFF_BETA:g})", "",
              "| b | a | subset | Δr@10 [lo, hi] | ΔMRR [lo, hi] |", "|---|---|---|---|---|"] + paired_lines
        if ship_rule_lines:
            L += ["", f"GM-700 Q2 ship rule at beta {SHIP_BETA:g}, K={k_}; paired against fp32 ship-L6 at beta {SHIP_BETA:g}; "
                  f"name floor = GM-463 off (L6, structured text, beta {OFF_BETA:g}) at this K", ""] + [x for t in ship_rule_lines for x in t]
        L += ["", f"Rules 1-4 of section 3.6 at K={k_} (rule 5 is the latency run's)", "",
              "| model | 1 ΔMRR lo > 0 | 2 Δr@10 lo >= -1.0 pt | 3 D9 vs int8-structured | 4 name ΔMRR lo >= -0.010 |",
              "|---|---|---|---|---|"] + rule_lines
        L += ["", f"Doc split, held-out NL positives, K={k_}", ""] + split_table(cs, ship, split_arms)
        L += ["", f"Mean pair tokens (query + text + specials, model's tokenizer, truncated at {R.MAX_TOKENS}), K={k_} window", "",
              "| model | scope | tokens |", "|---|---|---|"] + tok_lines
        if k_ == 30 and REF in a.models:
            S += c0_check(rows, bref, nref)
    p.write(step)
    if a.skip_sim:
        m_ = a.skip_sim
        sl, ss = skip_sim(cs, K, SHIP_BETA, score_of_for(caches[m_], m_, "ship"), fit_keys, mkrow, base[ST], r8s, m_)
        L += [""] + sl
        S += ss
    if failed:
        L += ["", "Models that did not load:"] + [f"- {m}: {e}" for m, e in failed.items()]
    finish(a, S, L)


C0_WANT = {"beta": 40.0, "r10": "0.670 [0.641, 0.700]", "mrr": "0.465 [0.439, 0.493]", "name": "0.971 / 0.889",
           "off": "0.649 / 0.427"}  # gm-698-ship-arm.md


def c0_check(rows, bref, nref):
    """Also requires that the reference scored 0 pairs (every pair came from the GM-698 cache)."""
    by = {r["label"]: r for r in rows}
    sh = by[f"ship-L6 K=30 beta={bref:g} (FIT-tuned)"]
    off = by[f"off (L6) K=30 beta={OFF_BETA:g}"]
    got = {"beta": bref, "r10": ci3(sh["r10"]), "mrr": ci3(sh["mrr"]), "name": f"{sh['name'][0]:.3f} / {sh['name'][1]:.3f}",
           "off": f"{off['r10'][0]:.3f} / {off['mrr'][0]:.3f}"}
    ok = got == C0_WANT and nref == {"off": 0, "ship": 0}
    line = (f"C0 ship-L6 reproduces GM-698: {'pass' if ok else 'MISMATCH'}; got {got}; want {C0_WANT}; "
            f"pairs scored {nref} (want 0)")
    print(line, flush=True)
    if not ok:
        sys.exit("STOP: " + line)
    return [line]


def finish(a, S, L):
    if L:
        print("\n" + "\n".join(L))
    if a.table and L:
        a.table.write_text("\n".join(L) + "\n")
    if a.summary:
        a.summary.write_text("\n".join(S) + "\n")
    E.PROGRESS.write_text("GM-463 done\n")


# ---- latency -------------------------------------------------------------------
def physical_cores():
    try:
        return int(subprocess.run(["sysctl", "-n", "hw.physicalcpu"], capture_output=True, text=True).stdout.strip())
    except ValueError:
        return os.cpu_count() or 1


def latency_queries(c):
    """gm464_latency.queries() (the GM-698 latency set), as qids of corpus c."""
    by_text = {}
    for q, qq in c.queries.items():
        if not qq["mechanical"]:
            by_text.setdefault(qq["text"], []).append(q)
    out = []
    for t in LAT.queries():
        ids = by_text.get(t, [])
        if len(ids) != 1:
            sys.exit(f"STOP: latency query {t!r} matches {len(ids)} NL queries of {c.name}")
        out.append(ids[0])
    return out


def latency_arm(a):
    """One model in this process: load (timed with the first call), one untimed
    warm-up pass at the largest K, then `passes` timed passes per K. The timed op
    is latency_f4's: tokenize + ORT + blend + sort over int8-structured's top K."""
    st_dir, corpora_dir = load_setup(a)
    ks = sorted(a.ks)
    c = load_corpora(a.work, a.eval_dir, st_dir, corpora_dir, max(ks), only={"g-mesh"})[0]
    build_model_texts([c], a.work, a.model, ["ship"])
    qids = latency_queries(c)
    texts = c.atext_by[a.model]["ship"]
    rec = {"model": a.model, "label": a.label, "ks": ks, "passes": a.passes, "nq": len(qids),
           "uptime_start": LAT.uptime(), "therm_start": LAT.therm(), "load_start": round(LAT.load1(), 2)}
    threads = min(4, physical_cores())

    def f4(ce, q, K):
        hits = c.lists[ST][q][:K]
        t0 = time.perf_counter()
        s = ce.score(c.queries[q]["text"], [texts[x] for x, _ in hits])
        s = s + SHIP_BETA * np.array([v for _, v in hits])
        order = np.lexsort((np.arange(len(hits)), -s))
        _ = [hits[i][0] for i in order]
        return time.perf_counter() - t0, len(hits)

    t0 = time.perf_counter()
    ce = R.CrossEncoder(a.work, a.model, threads, rust_like=True, chunk=a.chunk)
    rec["init_s"] = round(time.perf_counter() - t0, 3)
    first, _ = f4(ce, qids[0], 30 if 30 in ks else ks[-1])
    rec["first_call_ms"] = round(1000 * (time.perf_counter() - t0), 1)
    rec["first_inference_ms"] = round(1000 * first, 1)
    rec["threads"] = ce.threads
    rec["chunk"] = ce.chunk
    for q in qids:
        f4(ce, q, ks[-1])
    rec["per_k"] = {}
    for K in ks:
        ts, n = [], []
        for _ in range(a.passes):
            for q in qids:
                dt, w = f4(ce, q, K)
                ts.append(1000 * dt)
                n.append(w)
        rec["per_k"][K] = {"n": len(ts), "p50_ms": round(LAT.pct(ts, .5), 2), "p95_ms": round(LAT.pct(ts, .95), 2),
                           "mean_ms": round(statistics.mean(ts), 2), "ms_per_pair": round(sum(ts) / sum(n), 3)}
    k_tok = 30 if 30 in ks else ks[-1]
    rec["mean_pair_tokens_k"] = k_tok
    rec["mean_pair_tokens"] = round(statistics.mean(
        len(e.ids) for q in qids for e in ce.tok.encode_batch([(c.queries[q]["text"], texts[x]) for x, _ in c.lists[ST][q][:k_tok]])), 1)
    rec.update(uptime_end=LAT.uptime(), therm_end=LAT.therm(), load_end=round(LAT.load1(), 2), files=ce.files)
    print(json.dumps(rec), flush=True)


def speed_limit():
    out = subprocess.run(["pmset", "-g", "therm"], capture_output=True, text=True).stdout
    s = re.search(r"CPU_Speed_Limit\s*=\s*(\d+)", out)
    return int(s.group(1)) if s else None


def wait_speed_limit(cap_s):
    """Waits until pmset reports CPU_Speed_Limit == 100 (no limit printed counts as 100),
    at most cap_s: the caller passes what is left of the run's shared gate budget."""
    t0 = time.time()
    while True:
        v = speed_limit()
        if v is None or v == 100:
            return "100" if v else "na", round(time.time() - t0)
        if time.time() - t0 >= cap_s:
            return f"{v} (cap reached, measured anyway)", round(time.time() - t0)
        LAT.progress(f"therm-wait CPU_Speed_Limit {v}", 0, 1)
        time.sleep(5)


def latency(a):
    out = a.out or a.work / "gm463-latency"
    out.mkdir(parents=True, exist_ok=True)
    LAT.WAITED_FILE = out / "gate_waited_s"
    LAT.GATE_CAP = a.gate_cap
    if LAT.WAITED_FILE.exists():
        LAT.WAITED_FILE.unlink()
    order = [m for m in LAT_ORDER if m in a.models]
    if a.models != order:
        sys.exit(f"STOP: --models {a.models} is not in section 3.5's order {order}")
    arms = order + order[::-1]
    if order != LAT_ORDER:
        print(f"latency: subset of section 3.5's arms, missing {[m for m in LAT_ORDER if m not in order]}", flush=True)
    recs = []
    for i, m in enumerate(arms):
        label = f"{m}#{i + 1}"
        LAT.progress(f"arm {label} gate", i, len(arms))
        g = LAT.gate()
        waited = float(LAT.WAITED_FILE.read_text()) if LAT.WAITED_FILE.exists() else 0.0
        th, th_wait = wait_speed_limit(max(0.0, a.gate_cap - waited))
        LAT.WAITED_FILE.write_text(f"{waited + th_wait:.0f}")
        LAT.progress(f"arm {label} run", i, len(arms))
        passes = a.passes if a.passes is not None else PASSES_DEFAULT.get(m, 3)
        cmd = ["/usr/bin/time", "-p", sys.executable, __file__, "latency-arm", "--work", str(a.work),
               "--eval-dir", str(a.eval_dir), "--st-runs", a.st_runs, "--model", m, "--label", label,
               "--ks", ",".join(map(str, a.ks)), "--passes", str(passes)]
        if a.corpora:
            cmd += ["--corpora", str(a.corpora)]
        if a.chunk:
            cmd += ["--chunk", str(a.chunk)]
        p = subprocess.run(cmd, capture_output=True, text=True)
        tp = dict(re.findall(r"^(real|user|sys)\s+([0-9.]+)", p.stderr, re.M))
        lines = [l for l in p.stdout.splitlines() if l.startswith("{")]
        rec = json.loads(lines[-1]) if p.returncode == 0 and lines else {"model": m, "label": label, "error": p.stderr[-1500:]}
        rec.update(time_p=tp, gate=g, speed_limit=th, speed_wait_s=th_wait)
        if tp.get("real") and tp.get("user"):
            rec["user_per_real"] = round(float(tp["user"]) / max(float(tp["real"]), 1e-9), 2)
            rec["waiting"] = rec["user_per_real"] < 1.0
        recs.append(rec)
        with (out / "arms.jsonl").open("a") as f:
            f.write(json.dumps(rec) + "\n")
    write_latency_summary(out, recs, a.ks)
    LAT.progress("done", len(arms), len(arms))


def write_latency_summary(out, recs, ks):
    rows = ["| arm | K | n | p50 ms | p95 ms | mean ms | ms/pair | first call ms (load+1st) | pair tokens | gate | "
            "speed limit | load start/end | therm | time -p real/user/sys | user/real |",
            "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
    pooled = {}
    for r in recs:
        tp = r.get("time_p", {})
        tps = f"{tp.get('real', '?')}/{tp.get('user', '?')}/{tp.get('sys', '?')}"
        if "error" in r:
            rows.append(f"| {r['label']} | ERROR | | | | | | | | | | | | {tps} | |")
            continue
        for K in ks:
            s = r["per_k"][str(K)] if str(K) in r["per_k"] else r["per_k"][K]
            pooled.setdefault((r["model"], K), []).append(s["p50_ms"])
            rows.append(f"| {r['label']} | {K} | {s['n']} | {s['p50_ms']:.1f} | {s['p95_ms']:.1f} | {s['mean_ms']:.1f} | "
                        f"{s['ms_per_pair']:.2f} | {r['first_call_ms']:.0f} | {r['mean_pair_tokens']} | "
                        f"{r['gate']['gate']} | {r['speed_limit']} | {r['load_start']}/{r['load_end']} | "
                        f"{r['therm_start']}/{r['therm_end']} | {tps} | {r.get('user_per_real', '?')}"
                        f"{' WAITING' if r.get('waiting') else ''} |")
    l6 = {K: statistics.mean(v) for (m, K), v in pooled.items() if m == REF}
    rows += ["", "| model | K | mean of arm p50s ms | x MiniLM-L6 |", "|---|---|---|---|"]
    for (m, K), v in pooled.items():
        x = f"{statistics.mean(v) / l6[K]:.2f}" if K in l6 else "-"
        rows.append(f"| {m} | {K} | {statistics.mean(v):.1f} | {x} |")
    rows += ["", "| arm | chunk | model.onnx sha256 |", "|---|---|---|"]
    rows += [f"| {r['label']} | {r.get('chunk')} | {r.get('files', {}).get('model.onnx', '?')} |" for r in recs if "error" not in r]
    rows += ["", "uptime per arm (start -> end):"]
    rows += [f"- {r['label']}: " + (f"{r['uptime_start']} -> {r['uptime_end']}" if "error" not in r else f"error {r['error'][-300:]!r}")
             for r in recs]
    rows += ["", l6_tokens_control(recs)]
    print(rows[-1], flush=True)
    (out / "summary.md").write_text("\n".join(rows) + "\n")


def l6_tokens_control(recs):
    """Each ce-minilm arm's mean pair tokens at K=30 must equal gm-698-latency.md's value (one decimal)."""
    got = [(r["label"], r["mean_pair_tokens_k"], r["mean_pair_tokens"]) for r in recs if r["model"] == REF and "error" not in r]
    if not got:
        return "Control L6 mean pair tokens: not checked (no ce-minilm arm)"
    bad = [g for g in got if g[1] != 30 or abs(g[2] - L6_PAIR_TOKENS) > 0.05]
    return (f"Control L6 mean pair tokens at K=30 = {L6_PAIR_TOKENS} (gm-698-latency.md): "
            f"{'pass' if not bad else 'FAIL ' + str(bad)}; arms {got}")


# ---- cli -----------------------------------------------------------------------
def csv_ints(s):
    return [int(x) for x in s.split(",") if x]


def csv_models(s):
    ms = [x for x in s.split(",") if x]
    bad = [m for m in ms if m not in MODELS]
    if bad:
        raise argparse.ArgumentTypeError(f"unknown models {bad}; known: {MODELS}")
    return ms


def parse_args(argv=None):
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)

    def common(p):
        p.add_argument("--work", required=True, type=Path)
        p.add_argument("--eval-dir", type=Path, default=HERE / "v2")
        p.add_argument("--st-runs", default=f"runs/{ST}", help="int8-structured runs root, relative to --work unless absolute")
        p.add_argument("--corpora", type=Path, help="corpus checkouts (default <work>/corpora)")

    q = sub.add_parser("quality")
    common(q)
    q.add_argument("--models", type=csv_models, required=True)
    q.add_argument("--k", type=int, default=30, help="scoring window")
    q.add_argument("--report-k", type=csv_ints, default=[20, 30], help="windows reported from the same scores (<= --k)")
    q.add_argument("--scope", action="append", default=[], metavar="MODEL=all|nl",
                   help=f"query scope per model (default all; {SCOPE_DEFAULT})")
    q.add_argument("--cache-dir", type=Path, help="score caches (default <work>/rerank_cache)")
    q.add_argument("--dry-run-pairs", type=int, help="controls, then N new ship pairs per model with ms/pair and ETA; stop")
    q.add_argument("--controls-only", action="store_true", help="C1 C5 C8 C7 C3' per model; stop")
    q.add_argument("--skip-sim", choices=MODELS, help="GM-700: confident-skip simulation on this model's scores at beta 40")
    q.add_argument("--table", type=Path)
    q.add_argument("--summary", type=Path)

    lat = sub.add_parser("latency")
    common(lat)
    lat.add_argument("--models", type=csv_models, required=True)
    lat.add_argument("--ks", type=csv_ints, default=[20, 30, 50])
    lat.add_argument("--passes", type=int, help=f"timed passes per K (default 3; {PASSES_DEFAULT})")
    lat.add_argument("--chunk", type=int, help="pairs per ORT run for every arm (default: the model's own, 1 for int8, else 16)")
    lat.add_argument("--gate-cap", type=int, default=1800, help="seconds the load/therm gates may wait in total")
    lat.add_argument("--out", type=Path)

    arm = sub.add_parser("latency-arm")
    common(arm)
    arm.add_argument("--model", required=True, choices=MODELS)
    arm.add_argument("--label", default="")
    arm.add_argument("--ks", type=csv_ints, default=[20, 30, 50])
    arm.add_argument("--passes", type=int, default=3)
    arm.add_argument("--chunk", type=int, help="pairs per ORT run (default: the model's own)")

    a = ap.parse_args(argv)
    if a.mode == "quality":
        scope = dict(SCOPE_DEFAULT)
        for s in a.scope:
            m, _, v = s.partition("=")
            if m not in MODELS or v not in ("all", "nl"):
                ap.error(f"--scope {s}: want MODEL=all|nl with MODEL in {MODELS}")
            scope[m] = v
        a.scope = {m: scope.get(m, "all") for m in MODELS}
        if a.scope[REF] != "all":
            ap.error("ce-minilm is the reference and is scored on every query")
    return a


def main(argv=None):
    a = parse_args(argv)
    {"quality": quality, "latency": latency, "latency-arm": latency_arm}[a.mode](a)


if __name__ == "__main__":
    main()
