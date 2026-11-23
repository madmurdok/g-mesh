"""GM-736: the calibration and selection samples (docs/design/GM-736-cre-int8.md, section 1.1).

Both come from the GM-456 training repos (eval/training/corpora.toml), never from
an eval v2 corpus: calibration from the `train` repos, selection from the `dev`
repos, so the two are disjoint by repo and neither touches the gate sample.

    samples.py [--out-dir DIR]    # writes calib_sample.jsonl and select_sample.jsonl, prints their sha256
"""

import argparse
import json
import random
import sqlite3
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "gm703"))

import export_coderankembed as E  # noqa: E402
import parity as P  # noqa: E402

REPO = HERE.parents[2]
CORPORA_TOML = REPO / "eval" / "training" / "corpora.toml"
GM456 = Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm456"
TOKENIZER = E.DEFAULT_OUT / "coderankembed-fp32" / "tokenizer.json"
SEED = 736
CALIB = HERE / "calib_sample.jsonl"
SELECT = HERE / "select_sample.jsonl"
# (bins as parity.BINS with the counts replaced, queries, hand-made rows, fill a short bin)
CALIB_SHAPE = ({"short": 64, "medium": 64, "long": 64, "over": 32}, 32, False, False)
# The dev repos hold too few texts over 1024 tokens: the selection sample's
# "over" bin takes all of them and fills up with the longest remaining texts.
SELECT_SHAPE = ({name: n for name, _, _, n in P.BINS}, P.QUERIES, True, True)


def split_repos(split, corpora_toml=CORPORA_TOML):
    """The repo ids of `split`; refuses any eval v2 corpus name."""
    rows = tomllib.loads(Path(corpora_toml).read_text()).get("corpus", [])
    repos = sorted(r["id"] for r in rows if r.get("split") == split)
    leaked = sorted(set(repos) & set(P.CORPORA))
    if leaked:
        raise SystemExit(f"{corpora_toml}: split {split} lists eval v2 corpora {leaked}")
    if not repos:
        raise SystemExit(f"{corpora_toml}: no repo in split {split}")
    return repos


def node_texts(repos, work):
    out = []
    for repo in repos:
        con = sqlite3.connect(f"file:{Path(work) / (repo + '.sqlite')}?mode=ro", uri=True)
        for nid, doc, sig in con.execute("SELECT id, docComment, signature FROM nodes ORDER BY id"):
            t = P.text_to_embed(doc, sig)
            if t is not None:
                out.append((repo, nid, t))
        con.close()
    return out


def query_rows(repos, queries_path):
    keep = set(repos)
    out = []
    for line in Path(queries_path).read_text().splitlines():
        if line.strip():
            q = json.loads(line)
            if q["repo"] in keep:
                out.append((q["repo"], q["id"], q["text"]))
    return out


def build(split, shape, gm456=GM456, corpora_toml=CORPORA_TOML, tokenizer=TOKENIZER, seed=SEED):
    bins, n_queries, hand, fill = shape
    repos = split_repos(split, corpora_toml)
    tok = P.tokenizer(tokenizer, None)
    rng = random.Random(seed)
    nodes = node_texts(repos, Path(gm456) / "work")
    counts = [len(e.ids) for e in tok.encode_batch([t for _, _, t in nodes])]
    rows, taken = [], set()
    for name, lo, hi, _ in P.BINS:
        n = bins[name]
        pool = [i for i, c in enumerate(counts) if c >= lo and (hi is None or c <= hi) and i not in taken]
        filled = []
        if len(pool) < n:
            if not fill:
                raise SystemExit(f"split {split} bin {name}: {len(pool)} candidates, {n} needed")
            rest = sorted((i for i in range(len(nodes)) if i not in taken and i not in pool),
                          key=lambda i: (-counts[i], nodes[i][0], nodes[i][1]))
            filled = rest[: n - len(pool)]
        picked = rng.sample(pool, min(n, len(pool))) + filled
        for i in sorted(picked, key=lambda i: (nodes[i][0], nodes[i][1])):
            repo, nid, t = nodes[i]
            taken.add(i)
            rows.append({"id": f"{name}:{repo}:{nid}", "bin": name, "tokens": counts[i], "text": t,
                         **({"filled": True} if i in filled else {})})
    queries = query_rows(repos, Path(gm456) / "v0.4-full" / "queries.jsonl")
    for repo, qid, t in sorted(rng.sample(queries, n_queries), key=lambda q: (q[0], q[1])):
        text = E.QUERY_PREFIX + t
        rows.append({"id": f"query:{repo}:{qid}", "bin": "query", "tokens": len(tok.encode(text).ids), "text": text})
    if hand:
        seven = next(t for t in P.SEVEN_TOKEN_CANDIDATES if len(tok.encode(t).ids) == 7)
        rows.append({"id": "hand:empty", "bin": "hand", "tokens": len(tok.encode("").ids), "text": ""})
        rows.append({"id": "hand:seven", "bin": "hand", "tokens": 7, "text": seven})
    return rows


def build_calib_sample(**kw):
    return build("train", CALIB_SHAPE, **kw)


def build_select_sample(gate_sample=None, **kw):
    """The selection draw minus every text the frozen gate sample holds (the two hand rows)."""
    gate = {r["text"] for r in P.load_sample(gate_sample or P.SAMPLE)}
    return [r for r in build("dev", SELECT_SHAPE, **kw) if r["text"] not in gate]


def write(rows, path):
    with open(path, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    return P.sha256_file(path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out-dir", type=Path, default=HERE)
    a = ap.parse_args()
    for name, rows in (("calib_sample.jsonl", build_calib_sample()), ("select_sample.jsonl", build_select_sample())):
        path = a.out_dir / name
        print(f"{path}: {len(rows)} texts sha256 {write(rows, path)}")


if __name__ == "__main__":
    main()
