#!/usr/bin/env python3
"""Ranking windows and pair texts for the GM-730 routes
(docs/design/GM-730-gpu-training-routes.md, section 1.1, M6 and M7 of 9.1).

- `build_windows(split)`: for every GM-456 query of the split, the top K (30)
  nodes of its repo by int8-structured cosine from the GM-436 vector cache
  (`gm436/cache.py` tables; ties to the earlier node of the run, as
  `embed_eval.rs::top_hits`). The query's target is appended when it is not in
  the top K (`forced`, so the window has K + 1 entries).
- `join_check(windows)`: the non-forced part of every window equals the GM-456
  mine run's int8 `rankings.jsonl` top K (the same ids in the same order, or
  tie groups equal as sets, `gm436/dev_metric.c0b_compare`). `--plant-offset` shifts the key -> vector
  join by one row, which the check must catch.
- `pair_texts(repos)`: every embeddable node of the repos (the run's
  node_ids) with its ship text (E + P + B48, `gm462_ce_text.build`, MiniLM
  token counter; the text ship-L6 and gte-mb are judged on) and its structured
  text (`gm462_ce_text.base_text`, the text the embedders see).

Outputs under <out>/<split>/:
    windows.jsonl  {id, repo, split, target, text, keys, cos, forced, gold, positives, answersAlso}
    texts.jsonl    {key, repo, nodeId, ship, structured}
    manifest.json  {split, k, variant, counts, files: {name: sha256}, pairsSha256, joinCheck, structuredCheck}
pairsSha256 is the sha256 of "<query id>\\t<node key>\\n" over every window
entry in file order: the order of the teacher's score array.

usage: windows.py build --split train|dev [--out DIR] [--k 30] [--plant-offset]
"""

import argparse
import hashlib
import json
import os
import sqlite3
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "gm436"))
sys.path.insert(0, str(HERE.parent.parent / "embedding"))
import cache as K  # noqa: E402  (gm436; imports eval/training/common.py as K.C)
import data as D  # noqa: E402
import dev_metric as DM  # noqa: E402
import gm462_ce_text as T  # noqa: E402

C = K.C
VARIANT = "jina-v2-base-code-int8-structured"
VERSION = "v0.4-full"
K_WINDOW = 30
SHIP_SWITCHES = "EPB"
SHIP_BODY = 48
L6_TOKENIZER = "ms-marco-MiniLM-L6-v2@233902d25c440f23af6f7d6e94d2946bac0bee0a"
COLS = ("id", "kind", "nativeKind", "name", "qualifiedName", "filePath", "startLine", "endLine",
        "signature", "docComment", "language")


def data_root():
    return Path(os.environ.get("GM730_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm730"))


def sha256_file(p):
    return C.sha256_file(p)


# ---- windows -------------------------------------------------------------------------
class Vectors:
    """int8-structured node and query vectors of some repos from the GM-436 cache. Every
    node of the run is ranked, as the run did; nodes whose frame text changed since are counted."""

    def __init__(self, vdir, repos, plant_offset=False):
        mine = vdir / "mine"
        nt, qt = K.tables_for(mine, VARIANT, repos[0])
        self.nrows, nmat = nt.load(repos)
        self.qrows, qmat = qt.load(repos)
        loaded = {r["repo"] for r in self.nrows}
        if set(repos) - loaded:
            raise SystemExit(f"{VARIANT}: no imported vectors for {sorted(set(repos) - loaded)}; run gm436/cache.py import")
        if plant_offset:
            nmat = np.roll(nmat, 1, axis=0)
        self.nn = K.normalise(nmat)
        self.qn = K.normalise(qmat)
        self.prefix = K.variant_row(mine, VARIANT).get("query_prefix", "")
        spans = {}
        for repo in repos:
            spans.update(K.frame_spans(vdir, repo))
        self.stale_nodes = sum(1 for r in self.nrows if r["key"] in spans and r["textSha256"] != spans[r["key"]])
        self.query = {r["key"]: (i, r["textSha256"]) for i, r in enumerate(self.qrows)}


def top_k(scores, k):
    """Indices of the k highest scores; ties to the lower index."""
    return np.lexsort((np.arange(len(scores)), -scores))[:k]


def build_windows(split, k=K_WINDOW, plant_offset=False, version=VERSION):
    """(windows, counts) for every query of `split`, in queries.jsonl order within each repo, repos sorted."""
    vdir = C.version_dir(version)
    queries = D.load_queries(vdir, split)
    repos = sorted({q["repo"] for q in queries})
    V = Vectors(vdir, repos, plant_offset)
    counts = {"queries": len(queries), "windows": 0, "forced": 0, "droppedQueryVector": 0, "droppedNoGold": 0,
              "staleNodes": V.stale_nodes}
    out = []
    for repo in repos:
        idx = [i for i, r in enumerate(V.nrows) if r["repo"] == repo]
        keys = [V.nrows[i]["key"] for i in idx]
        col = {key: c for c, key in enumerate(keys)}
        nn = V.nn[idx]
        for q in (q for q in queries if q["repo"] == repo):
            hit = V.query.get(q["id"])
            if hit is None or hit[1] != K.query_text_sha(V.prefix, q["text"]):
                counts["droppedQueryVector"] += 1
                continue
            if q["target"] not in col:
                counts["droppedNoGold"] += 1
                continue
            s = V.qn[hit[0]] @ nn.T
            top = [int(j) for j in top_k(s, k)]
            forced = col[q["target"]] not in top
            if forced:
                top.append(col[q["target"]])
                counts["forced"] += 1
            wkeys = [keys[j] for j in top]
            pos, also = set(q["positives"]), set(q["answersAlso"])
            out.append({"id": q["id"], "repo": repo, "split": q["split"], "target": q["target"], "text": q["text"],
                        "keys": wkeys, "cos": [float(s[j]) for j in top], "forced": forced,
                        "gold": wkeys.index(q["target"]),
                        "positives": [i for i, key in enumerate(wkeys) if key in pos],
                        "answersAlso": [i for i, key in enumerate(wkeys) if key in also]})
    counts["windows"] = len(out)
    return out, counts


def join_check(windows, k=K_WINDOW):
    """{repo: {queries, mismatches, missing, first}}: each window's top k vs the run's rankings.jsonl."""
    by_repo = {}
    for w in windows:
        by_repo.setdefault(w["repo"], []).append(w)
    out = {}
    for repo, ws in sorted(by_repo.items()):
        run_dir = C.work_dir() / "runs" / VARIANT / repo
        id_of = {key: nid for nid, key in K.snapshot_keys(repo).items()}
        rust = {}
        with open(run_dir / "rankings.jsonl") as f:
            for line in f:
                r = json.loads(line)
                rust[r["id"]] = {"hits": r["hits"][:k]}
        missing = [w["id"] for w in ws if w["id"] not in rust]
        py_top = {w["id"]: [(id_of[key], c) for key, c in list(zip(w["keys"], w["cos"]))[:k]]
                  for w in ws if w["id"] in rust}
        # A query whose ids equal the run's, in order, passes even when f64-vs-f32 rounding
        # splits c0b_compare's last tie group.
        bad = [qid for qid in DM.c0b_compare(py_top, rust)
               if [n for n, _ in py_top[qid]] != [h[0] for h in rust[qid]["hits"]]]
        out[repo] = {"queries": len(py_top), "mismatches": len(bad), "missing": len(missing), "first": (bad + missing)[:3]}
    return out


def pairs_sha256(windows):
    h = hashlib.sha256()
    for w in windows:
        for key in w["keys"]:
            h.update(f"{w['id']}\t{key}\n".encode())
    return h.hexdigest()


# ---- texts ---------------------------------------------------------------------------
def l6_counter():
    from tokenizers import Tokenizer
    d = Path(os.environ.get("GM730_HF", data_root() / "hf")) / L6_TOKENIZER
    tk = Tokenizer.from_file(str(d / "tokenizer.json"))
    tk.no_truncation()
    tk.no_padding()
    return T.token_counter(tk)


def pair_texts(repos):
    """(rows, counts): {key, repo, nodeId, ship, structured} for every embeddable node of the repos."""
    work = C.work_dir()
    tok = l6_counter()
    rows, counts = [], {"nodes": 0, "missingFiles": 0}
    for repo in repos:
        con = sqlite3.connect(f"file:{work / (repo + '.sqlite')}?mode=ro", uri=True)
        db = {r[0]: dict(zip(COLS, r)) for r in con.execute(f"SELECT {', '.join(COLS)} FROM nodes")}
        con.close()
        ids = (work / "runs" / VARIANT / repo / "node_ids.txt").read_text().split()
        src = {}
        for nid in ids:
            r = db[nid]
            fp = r["filePath"]
            if fp not in src:
                p = work / "corpora" / repo / fp
                src[fp] = p.read_text(errors="replace").split("\n") if p.exists() else None
                counts["missingFiles"] += src[fp] is None
            rows.append({"key": C.key_of(repo, fp, r["qualifiedName"], r["startLine"]), "repo": repo, "nodeId": nid,
                         "ship": T.build(r, src[fp], SHIP_SWITCHES, tok, SHIP_BODY), "structured": T.base_text(r)})
        counts["nodes"] += len(ids)
    return rows, counts


def structured_check(rows, version=VERSION):
    """Structured texts vs GM-456's embed-structured render where the key is there."""
    d = sorted((C.version_dir(version) / "texts").glob("embed-structured@*"))[-1]
    render = {}
    for repo in sorted({r["repo"] for r in rows}):
        p = d / f"{repo}.jsonl"
        if p.exists():
            render.update({x["key"]: x["text"] for x in C.read_jsonl(p)})
    compared = [r for r in rows if r["key"] in render]
    diff = [r["key"] for r in compared if render[r["key"]] != r["structured"]]
    return {"render": d.name, "compared": len(compared), "differ": len(diff), "first": diff[:3]}


def write_jsonl(path, rows):
    tmp = Path(str(path) + ".tmp")
    with open(tmp, "w") as f:
        for r in rows:
            f.write(json.dumps(r, sort_keys=True) + "\n")
    os.replace(tmp, path)
    return sha256_file(path)


def build(split, out_root, k=K_WINDOW, plant_offset=False):
    """Write <out_root>/<split>/{windows,texts}.jsonl and manifest.json; returns the manifest."""
    windows, counts = build_windows(split, k, plant_offset)
    jc = join_check(windows, k)
    repos = sorted({w["repo"] for w in windows})
    texts, tcounts = pair_texts(repos)
    have = {t["key"] for t in texts}
    missing_text = sorted({key for w in windows for key in w["keys"]} - have)
    out = Path(out_root) / split
    out.mkdir(parents=True, exist_ok=True)
    files = {"windows.jsonl": write_jsonl(out / "windows.jsonl", windows),
             "texts.jsonl": write_jsonl(out / "texts.jsonl", texts)}
    man = {"split": split, "k": k, "variant": VARIANT, "version": VERSION, "plantOffset": plant_offset,
           "counts": dict(counts, pairs=sum(len(w["keys"]) for w in windows), **tcounts,
                          windowKeysWithoutText=len(missing_text)),
           "files": files, "pairsSha256": pairs_sha256(windows), "joinCheck": jc,
           "structuredCheck": structured_check(texts)}
    man["joinCheckPass"] = all(v["mismatches"] == 0 and v["missing"] == 0 for v in jc.values())
    C.write_json(out / "manifest.json", man)
    return man


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--split", required=True, choices=["train", "dev"])
    b.add_argument("--out", type=Path, default=data_root() / "windows")
    b.add_argument("--k", type=int, default=K_WINDOW)
    b.add_argument("--plant-offset", action="store_true", help="C-F: key -> vector join off by one row (the join check must fail)")
    a = ap.parse_args(argv)
    man = build(a.split, a.out, a.k, a.plant_offset)
    print(json.dumps({k: v for k, v in man.items() if k != "joinCheck"}, indent=1, sort_keys=True))
    bad = {r: v for r, v in man["joinCheck"].items() if v["mismatches"] or v["missing"]}
    print(f"join check: {'pass' if man['joinCheckPass'] else 'FAIL ' + json.dumps(bad)}")
    return 0 if man["joinCheckPass"] and not man["counts"]["missingFiles"] and not man["counts"]["windowKeysWithoutText"] else 1


if __name__ == "__main__":
    sys.exit(main())
