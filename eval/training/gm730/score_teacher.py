#!/usr/bin/env python3
"""Job RR-T: gte-mb (gte-reranker-modernbert-base) fp32 logits for every window pair
(docs/design/GM-730-gpu-training-routes.md, sections 1.1, 2 C-T and 7).

score (the job step): for each split (train, dev) the pairs (query text, node ship
text) of windows.jsonl, flattened in file order, scored in chunks of --chunk pairs
(each chunk run in length-sorted batches of --batch). Outputs in --out:
    <split>.f32    little-endian float32, one logit per pair, windows order
    teacher.json   {model, maxLen, splits: {split: {file, sha256, pairs, pairsSha256}}, ct, determinism, timing}
    ct.json        C-T: these logits vs the laptop's on $GM730_CT/ct_pairs.jsonl, max |diff| <= 0.05
checkpoint.pt holds the split cursor and the partial arrays (GM-457 contract,
trainlib.Checkpointer); a chunk's scores do not depend on the chunks before it,
so a resumed run writes the same arrays.

make-ct (laptop, before the job): --n pairs sampled with the seed from the dev
windows, scored on this CPU in fp32, written as ct_pairs.jsonl {query, text, logit}
(the C-T reference; it is uploaded as the gm730-ct-pairs data set).

usage: score_teacher.py --out DIR [--device cuda] [--limit N] [--no-ct]
       score_teacher.py make-ct --out-dir DIR [--n 1000]
"""

import argparse
import json
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import trainlib as L  # noqa: E402

SPLITS = ("train", "dev")
SNAPSHOT = "gte-mb"
MAX_LEN = 512   # the GM-463 scoring truncation (rerank_eval.MAX_TOKENS)
CT_TOL = 0.05   # GM-463 C7 tolerance, GPU vs CPU fp32
CT_FILE = "ct_pairs.jsonl"


def load_model(device):
    import torch
    from transformers import AutoModelForSequenceClassification
    # reference_compile=False: ModernBERT would otherwise torch.compile itself on CUDA
    m = AutoModelForSequenceClassification.from_pretrained(str(L.S.path_of(SNAPSHOT)), torch_dtype=torch.float32,
                                                           reference_compile=False)
    return m.to(device).eval()


def scorer(model, tok, device, batch):
    """pairs [(query, text)] -> float32 logits, length-sorted batches, fp32, no grad."""
    import torch
    pad = L.pad_id(tok)

    def f(pairs):
        encs = tok.encode_batch(list(pairs))
        with torch.no_grad():
            out = L.forward_sorted(lambda feed: model(**feed).logits[:, 0].float(), encs, batch, pad, device)
        return out.cpu().numpy().astype(np.float32)
    return f


def split_pairs(windows, texts):
    out = []
    for w in windows:
        out += [(w["text"], texts[k]["ship"]) for k in w["keys"]]
    return out


def ct_check(score, root):
    """C-T on $GM730_CT/ct_pairs.jsonl."""
    p = Path(root) / CT_FILE
    rows = L.read_jsonl(p)
    got = score([(r["query"], r["text"]) for r in rows])
    ref = np.array([r["logit"] for r in rows], np.float64)
    d = float(np.abs(got.astype(np.float64) - ref).max()) if rows else float("nan")
    return {"check": "C-T", "file": str(p), "sha256": L.S.sha256_file(p), "n": len(rows), "maxAbsDiff": d,
            "tol": CT_TOL, "pass": bool(rows) and d <= CT_TOL}


def score_main(a):
    import os
    cfg, cfg_sha = L.config_of(a)
    device = L.setup_torch(a)
    out = Path(a.out)
    data = {}
    for split in SPLITS:
        windows, texts, man = L.load_split(split)
        windows = windows[:a.limit] if a.limit else windows
        missing = sorted({k for w in windows for k in w["keys"]} - set(texts))
        if missing:
            raise SystemExit(f"{split}: {len(missing)} window keys have no text (first {missing[0]})")
        data[split] = (windows, split_pairs(windows, texts))
    total = sum(len(p) for _, p in data.values())
    tok = L.tokenizer(L.S.path_of(SNAPSHOT), a.max_len)
    model = load_model(device)
    score = scorer(model, tok, device, a.batch)

    with L.Checkpointer(a, cfg_sha) as ck:
        st = ck.load() or {"done": {s: 0 for s in SPLITS}, "scores": {s: np.zeros(0, np.float32) for s in SPLITS},
                           "chunks": 0, "seconds": 0.0}
        done_before = sum(st["done"].values())
        if done_before:
            L.progress(f"{out}: resumed at {done_before}/{total} pairs")
        probe = L.Probe.of("rr-teacher", -(-total // a.chunk), a)
        t_run = time.monotonic()
        for split in SPLITS:
            _, pairs = data[split]
            while st["done"][split] < len(pairs):
                s = st["done"][split]
                part = pairs[s:s + a.chunk]
                st["scores"][split] = np.concatenate([st["scores"][split], score(part)])
                st["done"][split] = s + len(part)
                st["chunks"] += 1
                probe.tick(st["chunks"])
                ck.at_step(st["chunks"], lambda: dict(st, seconds=st["seconds"] + time.monotonic() - t_run))
        st["seconds"] += time.monotonic() - t_run
        ck.save(st)

    meta = {"model": {"snapshot": SNAPSHOT, "repo": L.S.PINS[SNAPSHOT][0], "revision": L.S.PINS[SNAPSHOT][1]},
            "maxLen": a.max_len, "limit": a.limit, "splits": {}, "configSha256": cfg_sha,
            "determinism": L.determinism_note(a),
            "timing": {"seconds": st["seconds"], "pairs": total,
                       "pairsPerSecond": total / st["seconds"] if st["seconds"] else None}}
    for split in SPLITS:
        windows, pairs = data[split]
        arr = np.asarray(st["scores"][split], "<f4")
        if len(arr) != len(pairs):
            raise SystemExit(f"{split}: {len(arr)} scores for {len(pairs)} pairs")
        f = out / f"{split}.f32"
        tmp = Path(str(f) + ".tmp")
        arr.tofile(tmp)
        os.replace(tmp, f)
        meta["splits"][split] = {"file": f.name, "sha256": L.S.sha256_file(f), "pairs": len(pairs),
                                 "pairsSha256": L.pairs_sha256(windows)}
    if a.no_ct:
        meta["ct"] = {"check": "C-T", "skipped": "--no-ct"}
    else:
        ct_root = os.environ.get("GM730_CT") or str(L.S.data_root() / "ct")
        meta["ct"] = ct_check(score, ct_root)
        L.write_json(out / "ct.json", meta["ct"])
    L.write_json(out / "teacher.json", meta)
    ct = meta["ct"]
    L.progress(f"{out}: {total} pairs in {st['seconds']:.0f} s; C-T "
               + ("skipped" if "skipped" in ct else f"max |diff| {ct['maxAbsDiff']:.4g} -> {'pass' if ct['pass'] else 'FAIL'}"))
    return 0 if ("skipped" in ct or ct["pass"]) else 1


def make_ct(a):
    """Sample a.n dev window pairs with the seed and score them here (CPU, fp32)."""
    a.device = "cpu"
    L.setup_torch(a)
    windows, texts, _ = L.load_split("dev")
    pairs = split_pairs(windows, texts)
    pick = sorted(np.random.default_rng(a.seed).choice(len(pairs), size=min(a.n, len(pairs)), replace=False).tolist())
    import torch
    score = scorer(load_model(torch.device("cpu")), L.tokenizer(L.S.path_of(SNAPSHOT), MAX_LEN), torch.device("cpu"), a.batch)
    chosen = [pairs[i] for i in pick]
    t = time.monotonic()
    logits = score(chosen)
    dt = time.monotonic() - t
    out = Path(a.out_dir)
    out.mkdir(parents=True, exist_ok=True)
    with open(out / CT_FILE, "w") as f:
        for (q, x), z, i in zip(chosen, logits, pick):
            f.write(json.dumps({"pair": i, "query": q, "text": x, "logit": float(z)}, sort_keys=True) + "\n")
    print(f"{out / CT_FILE}: {len(chosen)} dev pairs, {dt:.1f} s on CPU ({len(chosen) / dt:.1f} pairs/s)")
    return 0


def parse(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv[:1] == ["make-ct"]:
        ap = argparse.ArgumentParser(description="C-T reference pairs (laptop)")
        ap.add_argument("--out-dir", required=True)
        ap.add_argument("--n", type=int, default=1000)
        ap.add_argument("--batch", type=int, default=16)
        ap.add_argument("--seed", type=int, default=730)
        ap.add_argument("--threads", type=int, default=4)
        a = ap.parse_args(argv[1:])
        a.cmd = "make-ct"
        return a
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    L.add_common_args(ap)
    ap.add_argument("--max-len", type=int, default=MAX_LEN)
    ap.add_argument("--batch", type=int, default=64, help="pairs per forward pass")
    ap.add_argument("--chunk", type=int, default=4096, help="pairs per checkpointable unit")
    ap.add_argument("--limit", type=int, help="first N windows of each split (smoke)")
    ap.add_argument("--no-ct", action="store_true", help="skip C-T (no $GM730_CT)")
    a = ap.parse_args(argv)
    a.cmd = "score"
    return a


def main(argv=None):
    a = parse(argv)
    return make_ct(a) if a.cmd == "make-ct" else score_main(a)


if __name__ == "__main__":
    sys.exit(main())
