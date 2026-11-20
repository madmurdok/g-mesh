"""GM-703: parity of the exported CodeRankEmbed ONNX files.

docs/design/GM-703-coderankembed.md, sections 3.3 and 3.4.

    parity.py --build-sample                 # writes parity_sample.jsonl (seed 703)
    parity.py --out DIR                      # P1, P1b, P1c, P1d, Q-F, Q-D -> DIR/parity.json
    parity.py --rust-queries RUN --model-dir DIR --eval-dir EVAL --out DIR
                                             # P3: the harness's query_vectors.bin vs onnxruntime

Exit status: 0 all checks pass; 2 stop rule S1 (P1, P1b, P1c or P1d);
3 stop rule S2 (Q-F or Q-D); 4 stop rule S4 (P3).
"""

import argparse
import hashlib
import json
import random
import sqlite3
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import export_coderankembed as E  # noqa: E402

HERE = Path(__file__).resolve().parent
SAMPLE = HERE / "parity_sample.jsonl"
V2 = HERE.parent / "v2"
WORK = Path.home() / "Projects/ClaudeProjects/g-mesh-eval-data/v2/work"
CORPORA = ["gin", "py-requests", "ripgrep", "g-mesh", "excalidraw", "task-tracker-mcp"]
SEED = 703
MAX_TOKENS = 1024
# (bin, lowest token count, highest token count or None, how many); counts include [CLS]/[SEP]
BINS = [("short", 1, 32, 40), ("medium", 33, 256, 40), ("long", 257, 1023, 40), ("over", 1025, None, 20)]
QUERIES = 20
SEVEN_TOKEN_CANDIDATES = ["read the whole file now", "open the file and read", "close every open file now"]

# Tolerances fixed before any run (section 3.3); a stop rule, never loosened.
P1_MAX_ABS = 1e-4
P1_MIN_COS = 0.99999
P1B_MAX_ABS = 1e-5
P1C_MIN_COS = 0.99999
P1D_MAX_ABS = 1e-3
P1D_LENGTHS = [1, 7, 512, 1024]
QD_MIN_MEDIAN = 0.98
QD_MAX_P1_GAP = 0.02
P3_MIN_COS = 0.99999
BATCH = 8


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def tokenizer(path, max_tokens=MAX_TOKENS):
    """The Rust loader's tokenizer setup: truncation at the spec's cap, no padding."""
    from tokenizers import Tokenizer

    tok = Tokenizer.from_file(str(path))
    tok.no_padding()
    if max_tokens is None:
        tok.no_truncation()
    else:
        tok.enable_truncation(max_tokens)
    return tok


def text_to_embed(doc, sig):
    """pipeline.rs text_to_embed (doc, blank line, signature)."""
    doc = (doc or "").strip() or None
    sig = (sig or "").strip() or None
    if doc and sig:
        return f"{doc}\n\n{sig}"
    return doc or sig


def query_texts(eval_dir, corpus):
    out = []
    for rel in (f"queries/{corpus}.jsonl", f"queries/mechanical/{corpus}.jsonl"):
        p = Path(eval_dir) / rel
        if p.exists():
            out += [json.loads(line) for line in p.read_text().splitlines() if line.strip()]
    return out


def build_sample(work=WORK, eval_dir=V2):
    """160 texts by seed 703 from the v2 snapshots' node texts and query files,
    plus the hand-made texts of 2 tokens ("") and 7 tokens."""
    tok = tokenizer(E.snapshot_dir() / "tokenizer.json", None)
    rng = random.Random(SEED)
    nodes = []
    for corpus in CORPORA:
        con = sqlite3.connect(f"file:{Path(work) / (corpus + '.sqlite')}?mode=ro", uri=True)
        for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes ORDER BY id"):
            t = text_to_embed(doc, sig)
            if t is not None:
                nodes.append((corpus, nid, t))
        con.close()
    counts = [len(tok.encode(t).ids) for _, _, t in nodes]
    rows = []
    for name, lo, hi, n in BINS:
        pool = [i for i, c in enumerate(counts) if c >= lo and (hi is None or c <= hi)]
        if len(pool) < n:
            sys.exit(f"bin {name}: {len(pool)} candidates, {n} needed")
        for i in sorted(rng.sample(pool, n), key=lambda i: (nodes[i][0], nodes[i][1])):
            corpus, nid, t = nodes[i]
            rows.append({"id": f"{name}:{corpus}:{nid}", "bin": name, "tokens": counts[i], "text": t})
    queries = []
    for corpus in CORPORA:
        queries += [(corpus, q["id"], q["text"]) for q in query_texts(eval_dir, corpus)]
    for corpus, qid, t in sorted(rng.sample(queries, QUERIES), key=lambda q: (q[0], q[1])):
        text = E.QUERY_PREFIX + t
        rows.append({"id": f"query:{corpus}:{qid}", "bin": "query", "tokens": len(tok.encode(text).ids), "text": text})
    seven = next(t for t in SEVEN_TOKEN_CANDIDATES if len(tok.encode(t).ids) == 7)
    rows.append({"id": "hand:empty", "bin": "hand", "tokens": len(tok.encode("").ids), "text": ""})
    rows.append({"id": "hand:seven", "bin": "hand", "tokens": 7, "text": seven})
    with open(SAMPLE, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    return rows


def load_sample(path=SAMPLE):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def normalize(v):
    v = np.asarray(v, dtype=np.float64)
    return v / np.linalg.norm(v)


def cosine(a, b):
    return float(np.dot(normalize(a), normalize(b)))


def session(path):
    import onnxruntime as ort

    return ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])


def run_onnx(sess, ids_rows):
    """Right-padded batch with [PAD] = 0 and mask 0; returns last_hidden_state."""
    width = max(len(r) for r in ids_rows)
    ids = np.zeros((len(ids_rows), width), dtype=np.int64)
    mask = np.zeros_like(ids)
    for i, r in enumerate(ids_rows):
        ids[i, : len(r)] = r
        mask[i, : len(r)] = 1
    feeds = {"input_ids": ids, "attention_mask": mask}
    names = [i.name for i in sess.get_inputs()]
    if "token_type_ids" in names:
        feeds["token_type_ids"] = np.zeros_like(ids)
    return sess.run(None, feeds)[0], mask


def cls_vectors(sess, ids_list):
    return [normalize(run_onnx(sess, [ids])[0][0, 0]) for ids in ids_list]


def mean_vectors(sess, ids_list):
    out = []
    for ids in ids_list:
        h, mask = run_onnx(sess, [ids])
        out.append(normalize((h[0] * mask[0][:, None]).sum(0) / mask[0].sum()))
    return out


def torch_hidden(model, ids):
    import torch

    t = torch.tensor([ids], dtype=torch.long)
    with torch.no_grad():
        return model(input_ids=t, attention_mask=torch.ones_like(t)).last_hidden_state[0].numpy()


def summary(values):
    a = np.asarray(values, dtype=np.float64)
    return {"min": float(a.min()), "p1": float(np.percentile(a, 1)), "median": float(np.median(a)), "max": float(a.max())}


def check_p1(model, fp32, ids_list):
    ref = [normalize(torch_hidden(model, ids)[0]) for ids in ids_list]
    onnx = cls_vectors(fp32, ids_list)
    max_abs = [float(np.abs(r - o).max()) for r, o in zip(ref, onnx)]
    cos = [float(np.dot(r, o)) for r, o in zip(ref, onnx)]
    ok = max(max_abs) <= P1_MAX_ABS and min(cos) >= P1_MIN_COS
    return ref, onnx, {"ok": ok, "max_abs": summary(max_abs), "cosine": summary(cos), "worst": int(np.argmax(max_abs))}


def check_p1b(fp32, ids_list, single):
    rng = random.Random(SEED)
    order = list(range(len(ids_list)))
    rng.shuffle(order)
    worst = 0.0
    for start in range(0, len(order), BATCH):
        chunk = order[start : start + BATCH]
        h, _ = run_onnx(fp32, [ids_list[i] for i in chunk])
        for row, i in enumerate(chunk):
            worst = max(worst, float(np.abs(normalize(h[row, 0]) - single[i]).max()))
    return {"ok": worst <= P1B_MAX_ABS, "max_abs": worst, "batch": BATCH}


def check_p1c(snapshot, sample, ref):
    from sentence_transformers import SentenceTransformer

    st = SentenceTransformer(str(snapshot), trust_remote_code=True, device="cpu")
    picks = [i for i, r in enumerate(sample) if 2 < r["tokens"] <= 256][:10]
    vecs = st.encode([sample[i]["text"] for i in picks], normalize_embeddings=True, convert_to_numpy=True)
    cos = [float(np.dot(normalize(v), ref[i])) for v, i in zip(vecs, picks)]
    return {"ok": min(cos) >= P1C_MIN_COS, "cosine": summary(cos), "n": len(picks)}


def check_p1d(model, fp32, sample, tok):
    seven = next(r for r in sample if r["id"] == "hand:seven")
    longest = max(sample, key=lambda r: r["tokens"])
    long_ids = tokenizer_ids(tok, longest["text"], None)
    cls_id, sep_id = long_ids[0], long_ids[-1]
    cases = {
        1: [cls_id],
        7: tokenizer_ids(tok, seven["text"], MAX_TOKENS),
        512: long_ids[:511] + [sep_id],
        1024: long_ids[:1023] + [sep_id],
    }
    out = {}
    for length in P1D_LENGTHS:
        ids = cases[length]
        assert len(ids) == length, (length, len(ids))
        ref = torch_hidden(model, ids)
        h, _ = run_onnx(fp32, [ids])
        out[str(length)] = float(np.abs(ref - h[0]).max())
    return {"ok": max(out.values()) <= P1D_MAX_ABS, "max_abs": out}


def tokenizer_ids(tok, text, max_tokens):
    if max_tokens is None:
        tok.no_truncation()
    else:
        tok.enable_truncation(max_tokens)
    ids = tok.encode(text).ids
    tok.enable_truncation(MAX_TOKENS)
    return ids


def check_qd(int8, fp32_vecs, ids_list, sample, jina_fp32_dir, jina_int8_dir):
    int8_vecs = cls_vectors(int8, ids_list)
    cre = [float(np.dot(a, b)) for a, b in zip(int8_vecs, fp32_vecs)]
    jtok = tokenizer(Path(jina_fp32_dir) / "tokenizer.json")
    jids = [jtok.encode(r["text"]).ids for r in sample]
    jf = mean_vectors(session(Path(jina_fp32_dir) / "model.onnx"), jids)
    ji = mean_vectors(session(Path(jina_int8_dir) / "model.onnx"), jids)
    jina = [float(np.dot(a, b)) for a, b in zip(ji, jf)]
    cs, js = summary(cre), summary(jina)
    ok = cs["median"] >= QD_MIN_MEDIAN and cs["p1"] >= js["p1"] - QD_MAX_P1_GAP
    return {"ok": ok, "coderankembed": cs, "jina": js}


def run_checks(out_dir, models, jina_fp32, jina_int8):
    snapshot = E.snapshot_dir()
    sample = load_sample()
    tok = tokenizer(Path(models) / "coderankembed-fp32" / "tokenizer.json")
    ids_list = [tok.encode(r["text"]).ids for r in sample]
    model = E.load_model(snapshot)
    fp32 = session(Path(models) / "coderankembed-fp32" / "model.onnx")
    int8 = session(Path(models) / "coderankembed-int8" / "model.onnx")
    report = {"n": len(sample), "sample_sha256": sha256_file(SAMPLE)}
    ref, single, report["P1"] = check_p1(model, fp32, ids_list)
    report["P1"]["worst_id"] = sample[report["P1"].pop("worst")]["id"]
    report["P1b"] = check_p1b(fp32, ids_list, single)
    report["P1c"] = check_p1c(snapshot, sample, ref)
    report["P1d"] = check_p1d(model, fp32, sample, tok)
    report["Q-F"] = E.int8_fingerprint(Path(models) / "coderankembed-int8" / "model.onnx")
    report["Q-D"] = check_qd(int8, single, ids_list, sample, jina_fp32, jina_int8)
    write(out_dir, "parity.json", report)
    if not all(report[k]["ok"] for k in ("P1", "P1b", "P1c", "P1d")):
        return 2
    if not (report["Q-F"]["ok"] and report["Q-D"]["ok"]):
        return 3
    return 0


def read_vectors(path, rows, dim):
    a = np.fromfile(path, dtype="<f4")
    if a.size != rows * dim:
        raise ValueError(f"{path}: {a.size} floats, expected {rows} x {dim}")
    return a.reshape(rows, dim)


def check_p3(run_dir, model_dir, eval_dir, prefix=E.QUERY_PREFIX, min_cos=P3_MIN_COS):
    """The harness's query vectors against onnxruntime-python on the same
    prefixed query texts (section 3.3, P3)."""
    run_dir, eval_dir = Path(run_dir), Path(eval_dir)
    man = json.loads((run_dir / "manifest.json").read_text())
    texts = {}
    for rel, digest in man["queryFiles"]:
        p = eval_dir / rel
        if sha256_file(p) != digest:
            raise ValueError(f"{p} changed since the run (sha256)")
        for line in p.read_text().splitlines():
            if line.strip():
                q = json.loads(line)
                texts[q["id"]] = q["text"]
    qids = man["queryIds"]
    rust = read_vectors(run_dir / "query_vectors.bin", len(qids), man["dimension"])
    tok = tokenizer(Path(model_dir) / "tokenizer.json")
    sess = session(Path(model_dir) / "model.onnx")
    py = cls_vectors(sess, [tok.encode(prefix + texts[q]).ids for q in qids])
    cos = [cosine(r, p) for r, p in zip(rust, py)]
    max_abs = [float(np.abs(normalize(r) - p).max()) for r, p in zip(rust, py)]
    worst = int(np.argmin(cos))
    return {
        "ok": min(cos) >= min_cos,
        "min_cosine_rule": min_cos,
        "run": str(run_dir),
        "model_dir": str(model_dir),
        "n": len(qids),
        "cosine": summary(cos),
        "max_abs": summary(max_abs),
        "exactly_equal": sum(1 for m in max_abs if m == 0.0),
        "worst_id": qids[worst],
    }


def write(out_dir, name, obj):
    Path(out_dir).mkdir(parents=True, exist_ok=True)
    (Path(out_dir) / name).write_text(json.dumps(obj, indent=2, sort_keys=True) + "\n")
    print(json.dumps(obj, indent=2, sort_keys=True))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--build-sample", action="store_true")
    ap.add_argument("--out", type=Path)
    ap.add_argument("--models", type=Path, default=E.DEFAULT_OUT)
    ap.add_argument("--jina-fp32", type=Path, default=WORK / "models" / "jina-v2-base-code-fp32-r")
    ap.add_argument("--jina-int8", type=Path, default=WORK / "models" / "jina-v2-base-code-int8")
    ap.add_argument("--rust-queries", type=Path, help="a harness run dir (manifest.json, query_vectors.bin)")
    ap.add_argument("--model-dir", type=Path, help="P3: the variant's model dir")
    ap.add_argument("--eval-dir", type=Path, default=V2)
    ap.add_argument("--name", default="p3.json", help="P3: output file name")
    ap.add_argument("--min-cosine", type=float, default=P3_MIN_COS)
    args = ap.parse_args()

    if args.build_sample:
        rows = build_sample(eval_dir=args.eval_dir)
        print(f"{len(rows)} texts -> {SAMPLE}")
        return 0
    if args.out is None:
        ap.error("--out is required")
    if args.rust_queries:
        if args.model_dir is None:
            ap.error("--rust-queries needs --model-dir")
        r = check_p3(args.rust_queries, args.model_dir, args.eval_dir, min_cos=args.min_cosine)
        write(args.out, args.name, r)
        return 0 if r["ok"] else 4
    return run_checks(args.out, args.models, args.jina_fp32, args.jina_int8)


if __name__ == "__main__":
    sys.exit(main())
