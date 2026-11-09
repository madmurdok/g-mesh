"""GM-700 S22: dump the (query, CE text) pairs the quint8 cross-encoder scored, and load the logits
the Rust CrossEncoder produced for them as the score cache of ce-minilm-quint8-rust.

  dump   --work W --out DIR            DIR/pairs.jsonl ({"query","text"} per row) + DIR/keys.json
  import --work W --dir DIR --cache-dir C   DIR/logits.jsonl -> C/gm463-ce-minilm-quint8-rust.<corpus>.<key>.json,
                                       and the Rust-vs-Python agreement stats (DIR/agreement.json)

Pairs are the K=30 window of every query (scope all) with the model's own ship texts, the set
gm463_bakeoff.py quality scores for ce-minilm-quint8.
"""
import argparse
import json
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gm463_bakeoff as B  # noqa: E402
import gm462_ce_enriched as E  # noqa: E402
import rerank_eval as R  # noqa: E402

PY, RUST, K = "ce-minilm-quint8", "ce-minilm-quint8-rust", 30


class NoCache:
    def has(self, c, q, h):
        return False


def setup(a):
    ns = argparse.Namespace(work=a.work, eval_dir=HERE / "v2", st_runs=f"runs/{B.ST}", corpora=None)
    st_dir, corpora_dir = B.load_setup(ns)
    cs = B.load_corpora(a.work, ns.eval_dir, st_dir, corpora_dir, K)
    B.build_model_texts(cs, a.work, PY, ["ship"])
    need = B.model_need(cs, NoCache(), PY, "ship", K, "all")
    return cs, need


def dump(a):
    cs, need = setup(a)
    a.out.mkdir(parents=True, exist_ok=True)
    byname = {c.name: c for c in cs}
    keys = []
    with open(a.out / "pairs.jsonl", "w") as f:
        for (cn, q), m in need.items():
            qt = byname[cn].queries[q]["text"]
            for h, t in m.items():
                f.write(json.dumps({"query": qt, "text": t}) + "\n")
                keys.append([cn, q, h])
    (a.out / "keys.json").write_text(json.dumps(keys))
    print(f"dumped {len(keys)} pairs over {len(need)} queries")


def import_(a):
    cs, need = setup(a)
    keys = json.loads((a.dir / "keys.json").read_text())
    logits = [json.loads(l)["logit"] for l in (a.dir / "logits.jsonl").read_text().splitlines()]
    if len(logits) != len(keys):
        sys.exit(f"STOP: {len(logits)} logits for {len(keys)} pairs")
    files = B.file_shas(a.work, PY)
    py = B.open_cache(a.cache_dir, cs, a.work, PY, files)
    rs = B.open_cache(a.cache_dir, cs, a.work, RUST, files)
    for (cn, q, h), v in zip(keys, logits):
        if not np.isfinite(v):
            sys.exit(f"STOP: non-finite logit for {cn} {q}")
        rs.put(cn, q, [h], [v])
    d, per_q = [], {}
    for (cn, q, h), v in zip(keys, logits):
        p = py.d[cn][q][h]
        d.append(abs(v - p))
        per_q.setdefault((cn, q), []).append((v, p))
    agree = []
    for rows in per_q.values():
        r, p = np.array([x[0] for x in rows]), np.array([x[1] for x in rows])
        n = min(10, len(rows))
        agree.append(set(np.argsort(-r, kind="stable")[:n]) == set(np.argsort(-p, kind="stable")[:n]))
    d = np.array(d)
    stats = {"pairs": len(d), "queries": len(per_q), "mean_abs_d": float(d.mean()), "max_abs_d": float(d.max()),
             "p99_abs_d": float(np.quantile(d, 0.99)), "pairs_differing": int((d > 0).sum()),
             "pairs_differing_gt_1e-3": int((d > 1e-3).sum()),
             "top10_set_agreement": float(np.mean(agree)), "top10_set_differs_queries": int(len(agree) - sum(agree))}
    (a.dir / "agreement.json").write_text(json.dumps(stats, indent=1))
    print(json.dumps(stats))
    if stats["pairs_differing"] == 0:
        sys.exit("STOP: control failed, the Rust variant equals the Python quint8 on every pair")
    for c in cs:
        rs.save(c)
    print("cache written", [str(rs.file(c)) for c in cs][:1])


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="mode", required=True)
    d = sub.add_parser("dump")
    d.add_argument("--work", type=Path, required=True)
    d.add_argument("--out", type=Path, required=True)
    i = sub.add_parser("import")
    i.add_argument("--work", type=Path, required=True)
    i.add_argument("--dir", type=Path, required=True)
    i.add_argument("--cache-dir", type=Path, required=True)
    a = ap.parse_args()
    {"dump": dump, "import": import_}[a.mode](a)


if __name__ == "__main__":
    main()
