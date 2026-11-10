#!/usr/bin/env python3
"""Hard-negative mining for the GM-456 training data
(docs/design/GM-456-training-data.md, section 3).

1. Writes a mining eval dir `<version>/mine/` (corpora.toml and variants.toml
   linked from eval/training, `work` linked to the data dir's work, and
   `queries/<repo>.jsonl` in the D3 record format, `expected` = the positive),
   so `debug-embed-eval run` checks every positive against the snapshot and
   fails on one that does not resolve.
2. Runs `debug-embed-eval run` for the `bm25` and
   `jina-v2-base-code-int8-structured` arms under `taskpolicy -b` with
   G_MESH_EMBED_INTRA_THREADS threads. Run dirs are in `work/runs`, shared
   across versions, so node vectors are reused while the snapshot and the
   variant are unchanged; each run's rankings.jsonl is copied to
   `mine/rankings/<variant>/<repo>.jsonl`.
3. Builds one candidate list per target, shared by its queries, capped at CAP,
   each candidate labelled with every source that found it:
     embed / bm25   top TOP_K hits of any of the target's queries
     same-parent    same owner (`nodes.owner_of`; same file too for Python and
                    TypeScript, whose qualified names carry no module)
     trait-impl     the same method name under another `<X as T>` segment,
                    or another implementer of a supertype the target has
                    (`edges.kind = 'SUPERTYPE_OF'`)
     name-sibling   a name sharing at least half the target name's sub-tokens
                    (`check_queries.sub_tokens`)
     same-file      the same file (also the `sameFile` flag on every candidate)
   Candidates are frame nodes only (embeddable, non-test, `nodes/<repo>.jsonl`)
   that are not a positive and not the target's own (filePath, qualifiedName).
   Graph sources consider Function, Type and Variable nodes only. Order before
   the cap: more sources first, then the best mined rank, then graph source
   priority, then line distance in the same file, then key.

Output: `<version>/queries.mined.jsonl` (each query row with `negatives`, every
`judge` null) and `mine/summary.json` (candidate counts per source).

usage: mine.py --version v0.0-stub [--bin target/release/g-mesh] [--threads 2]
"""

import argparse
import json
import os
import shutil
import sqlite3
import subprocess
from collections import Counter, defaultdict
from functools import lru_cache
from pathlib import Path

import common as C
from check_queries import sub_tokens as _sub_tokens
from nodes import trait_impl_segment as _trait_impl_segment

sub_tokens = lru_cache(maxsize=None)(lambda name: frozenset(_sub_tokens(name)))
trait_impl_segment = lru_cache(maxsize=None)(_trait_impl_segment)

VARIANTS = {"embed": "jina-v2-base-code-int8-structured", "bm25": "bm25"}
TOP_K = 20
CAP = 12
GRAPH_KINDS = {"Function", "Type", "Variable"}
GRAPH_PRIORITY = ["same-parent", "trait-impl", "name-sibling", "same-file"]
SOURCES = ["same-parent", "same-file", "trait-impl", "name-sibling", "bm25", "embed"]
NO_MODULE_LANGUAGES = {"python", "typescript"}


def d3_record(q, node, corpus):
    return {
        "id": q["id"], "corpus": corpus, "language": q["language"], "kind": "positive",
        "shape": q["shape"], "text": q["text"],
        "expected": [{"filePath": node["filePath"], "qualifiedName": node["qualifiedName"], "kind": node["kind"]}],
        "derivation": f"GM-456 training query for {q['target']}", "author": (q["gen"] or {}).get("model") or "stub",
    }


def link(src, dst):
    dst = Path(dst)
    if dst.is_symlink() or dst.exists():
        if dst.is_symlink() and os.readlink(dst) == str(src):
            return
        dst.unlink()
    dst.symlink_to(src)


def prepare_eval_dir(vdir, queries, nodes):
    mine = Path(vdir) / "mine"
    (mine / "queries").mkdir(parents=True, exist_ok=True)
    link(C.HERE / "corpora.toml", mine / "corpora.toml")
    link(C.HERE / "variants.toml", mine / "variants.toml")
    link(C.work_dir(), mine / "work")
    by_repo = defaultdict(list)
    for q in queries:
        by_repo[q["repo"]].append(d3_record(q, nodes[q["target"]], q["repo"]))
    for repo, rows in by_repo.items():
        C.write_jsonl(mine / "queries" / f"{repo}.jsonl", rows)
    return mine, sorted(by_repo)


def run_arms(mine, repos, binary, threads):
    env = dict(os.environ, G_MESH_EMBED_INTRA_THREADS=str(threads), G_MESH_EMBED_INTER_THREADS="1")
    out = C.work_dir() / "runs"
    for source, variant in VARIANTS.items():
        for repo in repos:
            cmd = ["taskpolicy", "-b", str(binary), "debug-embed-eval", "run", "--eval-dir", str(mine),
                   "--variant", variant, "--corpus", repo, "--out", str(out)]
            print("+", " ".join(cmd), flush=True)
            subprocess.run(cmd, env=env, check=True)
            dst = mine / "rankings" / variant / f"{repo}.jsonl"
            dst.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(out / variant / repo / "rankings.jsonl", dst)
            for name in ("manifest.json", "timings.json"):
                shutil.copyfile(out / variant / repo / name, dst.with_name(f"{repo}.{name}"))


def snapshot_maps(repo):
    """Snapshot node id -> key, and SUPERTYPE_OF edges as (from key, to key)."""
    db = C.work_dir() / f"{repo}.sqlite"
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    key = {i: C.key_of(repo, f, q, s) for i, f, q, s in
           conn.execute("SELECT id, filePath, qualifiedName, startLine FROM nodes")}
    edges = [(key[a], key[b]) for a, b in
             conn.execute("SELECT fromId, toId FROM edges WHERE kind = 'SUPERTYPE_OF'") if a in key and b in key]
    conn.close()
    return key, edges


def graph_candidates(target, frame, supers, subs):
    """{key: set(sources)} from the snapshot structure alone."""
    out = defaultdict(set)
    t = target
    t_tokens = sub_tokens(t["name"])
    t_seg = trait_impl_segment(t["parent"]) if t["language"] == "rust" and t["parent"] else None
    for n in frame:
        if n["kind"] not in GRAPH_KINDS or n["key"] == t["key"]:
            continue
        if n["filePath"] == t["filePath"]:
            out[n["key"]].add("same-file")
        if t["parent"] and n["parent"] == t["parent"] and n["language"] == t["language"]:
            if t["language"] not in NO_MODULE_LANGUAGES or n["filePath"] == t["filePath"]:
                out[n["key"]].add("same-parent")
        if t_seg and n["name"] == t["name"] and n["parent"]:
            seg = trait_impl_segment(n["parent"])
            if seg and seg[1] == t_seg[1] and seg[0] != t_seg[0]:
                out[n["key"]].add("trait-impl")
        if t_tokens and n["language"] == t["language"]:
            shared = t_tokens & sub_tokens(n["name"])
            if 2 * len(shared) >= len(t_tokens):
                out[n["key"]].add("name-sibling")
    for sup in supers.get(t["key"], ()):
        for other in subs.get(sup, ()):
            if other != t["key"]:
                out[other].add("trait-impl")
    return out


def mined_candidates(query_ids, rankings, id_to_key):
    """{key: (best 1-based rank, best score)} over the queries' top TOP_K hits."""
    best = {}
    for qid in query_ids:
        for rank, (nid, score) in enumerate(rankings[qid]["hits"][:TOP_K], 1):
            k = id_to_key.get(nid)
            if k is None:
                continue
            r, s = best.get(k, (None, None))
            best[k] = (rank if r is None else min(r, rank), score if s is None else max(s, score))
    return best


def candidate_list(target, positives, frame_by_key, graph, mined, cap=CAP):
    """The target's capped, ordered candidate list, and the uncapped one."""
    own = (target["filePath"], target["qualifiedName"])
    sources = defaultdict(set)
    for k, s in graph.items():
        sources[k] |= s
    for src, found in mined.items():
        for k in found:
            sources[k].add(src)
    rows = []
    for k, s in sources.items():
        n = frame_by_key.get(k)
        if n is None or k in positives or (n["filePath"], n["qualifiedName"]) == own:
            continue
        b = mined.get("bm25", {}).get(k)
        e = mined.get("embed", {}).get(k)
        ranks = [x[0] for x in (b, e) if x]
        prio = min([GRAPH_PRIORITY.index(x) for x in s if x in GRAPH_PRIORITY] or [len(GRAPH_PRIORITY)])
        same_file = n["filePath"] == target["filePath"]
        dist = abs(n["startLine"] - target["startLine"]) if same_file else 10 ** 9
        rows.append(((-len(s), min(ranks) if ranks else 10 ** 9, prio, dist, k), {
            "key": k,
            "sources": [x for x in SOURCES if x in s],
            "sameFile": same_file,
            "mining": {"bm25Rank": b[0] if b else None, "embedRank": e[0] if e else None,
                       "embedCos": round(e[1], 4) if e else None},
            "judge": None,
        }))
    rows.sort(key=lambda r: r[0])
    full = [r[1] for r in rows]
    return full[:cap], full


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--bin", default=str(C.REPO / "target/release/g-mesh"))
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--skip-run", action="store_true", help="reuse mine/rankings from an earlier run")
    args = ap.parse_args()
    vdir = C.version_dir(args.version)
    queries = C.read_jsonl(Path(vdir) / "queries.gen.jsonl")
    repos_needed = sorted({q["repo"] for q in queries})
    nodes, frames = {}, {}
    for repo in repos_needed:
        frames[repo] = C.load_nodes(vdir, repo)
        for n in frames[repo]:
            nodes[n["key"]] = n
    mine, repos = prepare_eval_dir(vdir, queries, nodes)
    if not args.skip_run:
        run_arms(mine, repos, args.bin, args.threads)

    by_target = defaultdict(list)
    for q in queries:
        by_target[q["target"]].append(q)
    pre, post, sizes, same_file_share = Counter(), Counter(), [], []
    out = []
    for repo in repos:
        id_to_key, edges = snapshot_maps(repo)
        frame_by_key = {n["key"]: n for n in frames[repo]}
        id_to_frame = {i: k for i, k in id_to_key.items() if k in frame_by_key}
        supers, subs = defaultdict(set), defaultdict(set)
        for a, b in edges:
            supers[a].add(b)
            subs[b].add(a)
        rankings = {}
        for variant in VARIANTS.values():
            for r in C.read_jsonl(mine / "rankings" / variant / f"{repo}.jsonl"):
                rankings.setdefault(variant, {})[r["id"]] = r
        for tkey, qs in by_target.items():
            if qs[0]["repo"] != repo:
                continue
            target = nodes[tkey]
            positives = set(qs[0]["positives"])
            qids = [q["id"] for q in qs]
            graph = graph_candidates(target, frames[repo], supers, subs)
            mined = {src: mined_candidates(qids, rankings[v], id_to_frame) for src, v in VARIANTS.items()}
            capped, full = candidate_list(target, positives, frame_by_key, graph, mined)
            for c in full:
                for s in c["sources"]:
                    pre[s] += 1
            for c in capped:
                for s in c["sources"]:
                    post[s] += 1
            sizes.append(len(capped))
            if capped:
                same_file_share.append(sum(c["sameFile"] for c in capped) / len(capped))
            for q in qs:
                out.append(dict(q, negatives=[dict(c) for c in capped]))
    C.write_jsonl(Path(vdir) / "queries.mined.jsonl", out)
    summary = {
        "targets": len(by_target),
        "queries": len(out),
        "candidatesPerSourceUncapped": {s: pre[s] for s in SOURCES},
        "candidatesPerSourceCapped": {s: post[s] for s in SOURCES},
        "listSize": C.quantiles(sizes),
        "targetsWithEmptyList": sum(1 for s in sizes if s == 0),
        "sameFileShareMean": round(sum(same_file_share) / len(same_file_share), 4) if same_file_share else None,
        "cap": CAP, "topK": TOP_K,
    }
    C.write_json(mine / "summary.json", summary)
    print("mine:", json.dumps(summary))


if __name__ == "__main__":
    main()
