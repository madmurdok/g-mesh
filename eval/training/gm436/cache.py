#!/usr/bin/env python3
"""GM-436 vector and teacher-score cache (docs/design/GM-436-laptop-training.md,
section 3).

Encoder passes are Rust `debug-embed-eval run`s over the GM-456 mine dir
(owner D4); Python never embeds. This module imports a run's `vectors.bin` and
`query_vectors.bin` into keyed tables:

    $GM436_DATA/cache/vec/<modelKey>/<side>/
        index.jsonl      {key, textSha256, chunk, row, repo}
        chunk-NNNNN.npy  float32 [<=4096, d], raw harness vectors
        manifest.json    {modelKey, side, dimension, sourceRuns, chunks}

- modelKey = <variant>@<sha256(model.onnx)[:12]> (run manifest `modelFiles`).
- side = node@<renderId> | query@<sha256(query_prefix)[:12]>, where
  renderId = embed-<text>@<variantFingerprint[:12]>. The variant fingerprint
  covers the text form, so a re-render lands in a new side.
- Node textSha256: the GM-456 `spanSha256` of a frame node (nodes/<repo>.jsonl);
  for a non-frame node, sha256("snapshot:<snapshotSha256>:<key>"). The
  embed-structured render itself is not stored by GM-456, so the side carries
  the render and textSha256 the content.
- Query textSha256 = sha256(prefix + text), text from the mine dir's query file
  (the text the harness embedded).
- Lookup is by (key, textSha256): an edited query or node misses.
- Resumable: every chunk goes to *.tmp, is fsynced and renamed; manifest and
  index are rewritten atomically after each chunk; a listed chunk whose sha256
  matches is skipped on restart.

Teacher scores (R2, owner D2: jina int8):

    $GM436_DATA/cache/teacher/<teacherModelKey>/<renderId>/<candSetId>/
        chunk-NNNNN.npz  qids int32 [n], cand int32 [n, K] (-1 pad), score float32 [n, K] (NaN pad)
        manifest.json    {queryIds, nodeTable, rule, k, chunks}

candSetId = sha256(rule|K|GM-456 version)[:12]; rule = GM-456 positives +
negatives + answersAlso, union teacher top-K over the query's repo nodes;
`cand` indexes the teacher node table's rows.
"""

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
import common as C  # noqa: E402  (eval/training/common.py)

CHUNK_ROWS = 4096
TEACHER_CHUNK = 4096
TEACHER_K = 50
TEACHER_RULE = "gm456-pos+neg+answersAlso|teacher-topK-repo"


def gm436_root():
    return Path(os.environ.get("GM436_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm436"))


def sha256_text(s):
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


def _fsync_replace(tmp, dst):
    with open(tmp, "rb+") as f:
        os.fsync(f.fileno())
    os.replace(tmp, dst)


def _write_json_atomic(path, obj):
    tmp = Path(str(path) + ".tmp")
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=1)
        f.write("\n")
    _fsync_replace(tmp, path)


# --- Rust run dirs -----------------------------------------------------------

class Run:
    """One `debug-embed-eval run` dir: raw f32 little-endian row-major files."""

    def __init__(self, run_dir):
        self.dir = Path(run_dir)
        self.manifest_bytes = (self.dir / "manifest.json").read_bytes()
        self.manifest = json.loads(self.manifest_bytes)
        self.manifest_sha256 = C.sha256_bytes(self.manifest_bytes)
        self.dim = int(self.manifest["dimension"])
        self.node_ids = (self.dir / "node_ids.txt").read_text().split()
        if len(self.node_ids) != self.manifest["nodeCount"]:
            raise SystemExit(f"{self.dir}: node_ids.txt has {len(self.node_ids)} ids, manifest says {self.manifest['nodeCount']}")
        self.query_ids = list(self.manifest["queryIds"])

    def _matrix(self, name, rows):
        path = self.dir / name
        size = path.stat().st_size
        if size != rows * self.dim * 4:
            raise SystemExit(f"{path}: {size} bytes, expected {rows} x {self.dim} f32")
        return np.memmap(path, dtype="<f4", mode="r", shape=(rows, self.dim))

    def node_vectors(self):
        return self._matrix("vectors.bin", len(self.node_ids))

    def query_vectors(self):
        return self._matrix("query_vectors.bin", len(self.query_ids))

    def model_key(self):
        onnx = [f for f in self.manifest.get("modelFiles", []) if f[0] == "model.onnx"]
        if not onnx:
            raise SystemExit(f"{self.dir}: manifest has no model.onnx in modelFiles")
        return f"{self.manifest['variant']}@{onnx[0][1][:12]}"


def variant_row(eval_dir, name):
    rows = C._load_toml(Path(eval_dir) / "variants.toml")["variant"]
    for r in rows:
        if r["name"] == name:
            return r
    raise SystemExit(f"no variant {name} in {eval_dir}/variants.toml")


def render_id(row, manifest):
    return f"embed-{row.get('text', 'full')}@{manifest['variantFingerprint'][:12]}"


def node_side(row, manifest):
    return f"node@{render_id(row, manifest)}"


def query_side(row):
    return f"query@{sha256_text(row.get('query_prefix', ''))[:12]}"


def node_text_sha(key, frame_span, snapshot_sha):
    span = frame_span.get(key)
    return span if span else sha256_text(f"snapshot:{snapshot_sha}:{key}")


def query_text_sha(prefix, text):
    return sha256_text(prefix + text)


# --- keyed vector table ------------------------------------------------------

class VecTable:
    def __init__(self, path, model_key=None, side=None, dimension=None):
        self.dir = Path(path)
        mpath = self.dir / "manifest.json"
        if mpath.exists():
            self.manifest = json.loads(mpath.read_text())
        else:
            if model_key is None:
                raise SystemExit(f"{self.dir}: no table here")
            self.manifest = {"modelKey": model_key, "side": side, "dimension": dimension,
                             "sourceRuns": [], "chunks": []}
        ipath = self.dir / "index.jsonl"
        self.index = C.read_jsonl(ipath) if ipath.exists() and mpath.exists() else []
        # Index rows of chunks not in the manifest (a crash between files) are dropped.
        listed = {c["file"] for c in self.manifest["chunks"]}
        self.index = [r for r in self.index if r["chunk"] in listed]

    @property
    def dim(self):
        return self.manifest["dimension"]

    def _save(self):
        self.dir.mkdir(parents=True, exist_ok=True)
        tmp = self.dir / "index.jsonl.tmp"
        with open(tmp, "w") as f:
            for r in self.index:
                f.write(json.dumps(r) + "\n")
        _fsync_replace(tmp, self.dir / "index.jsonl")
        _write_json_atomic(self.dir / "manifest.json", self.manifest)

    def has_source(self, manifest_sha):
        return any(s["manifestSha256"] == manifest_sha and s.get("complete") for s in self.manifest["sourceRuns"])

    def add(self, source, manifest_sha, repo, keys, shas, vectors):
        """Append one source run's rows, chunk by chunk, resumably."""
        if self.has_source(manifest_sha):
            return 0
        vectors = np.asarray(vectors, dtype="<f4")
        if vectors.shape != (len(keys), self.dim):
            raise SystemExit(f"{source}: {vectors.shape} vectors for {len(keys)} keys, dim {self.dim}")
        srcs = self.manifest["sourceRuns"]
        entry = next((s for s in srcs if s["manifestSha256"] == manifest_sha), None)
        if entry is None:
            entry = {"dir": str(source), "manifestSha256": manifest_sha, "repo": repo, "rows": len(keys), "complete": False}
            srcs.append(entry)
        written = 0
        for part, start in enumerate(range(0, len(keys), CHUNK_ROWS)):
            name = f"chunk-{manifest_sha[:12]}-{part:03d}.npy"
            block = np.ascontiguousarray(vectors[start:start + CHUNK_ROWS])
            listed = next((c for c in self.manifest["chunks"] if c["file"] == name), None)
            if listed and (self.dir / name).exists() and C.sha256_file(self.dir / name) == listed["sha256"]:
                continue
            self.dir.mkdir(parents=True, exist_ok=True)
            tmp = self.dir / (name + ".tmp")
            with open(tmp, "wb") as f:
                np.save(f, block)
            _fsync_replace(tmp, self.dir / name)
            self.manifest["chunks"] = [c for c in self.manifest["chunks"] if c["file"] != name]
            self.manifest["chunks"].append({"file": name, "rows": len(block), "sha256": C.sha256_file(self.dir / name)})
            self.index = [r for r in self.index if r["chunk"] != name]
            self.index += [{"key": k, "textSha256": s, "chunk": name, "row": i, "repo": repo}
                           for i, (k, s) in enumerate(zip(keys[start:start + CHUNK_ROWS], shas[start:start + CHUNK_ROWS]))]
            self._save()
            written += len(block)
        entry["complete"] = True
        self._save()
        return written

    def load(self, repos=None, verify=True):
        """(rows, matrix): index rows (optionally of some repos) and their vectors, in index order."""
        want = None if repos is None else set(repos)
        chunks = {}
        rows, out = [], []
        for r in self.index:
            if want is not None and r["repo"] not in want:
                continue
            name = r["chunk"]
            if name not in chunks:
                path = self.dir / name
                if verify:
                    listed = next(c for c in self.manifest["chunks"] if c["file"] == name)
                    if C.sha256_file(path) != listed["sha256"]:
                        raise SystemExit(f"{path}: sha256 differs from the table manifest")
                chunks[name] = np.load(path)
            rows.append(r)
            out.append(chunks[name][r["row"]])
        mat = np.stack(out).astype(np.float32) if out else np.zeros((0, self.dim), np.float32)
        return rows, mat


def table_dir(model_key, side):
    return gm436_root() / "cache" / "vec" / model_key / side


# --- import ------------------------------------------------------------------

def frame_spans(vdir, repo):
    return {n["key"]: n["spanSha256"] for n in C.load_nodes(vdir, repo)}


def snapshot_keys(repo):
    """Snapshot node id -> GM-456 key (mine.py::snapshot_maps, without the edges)."""
    import sqlite3
    db = C.work_dir() / f"{repo}.sqlite"
    conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    key = {i: C.key_of(repo, f, q, s) for i, f, q, s in
           conn.execute("SELECT id, filePath, qualifiedName, startLine FROM nodes")}
    conn.close()
    return key


def import_run(mine_dir, vdir, variant, repo, runs_dir=None):
    """Import one (variant, repo) run into its node and query tables. Returns (node table dir, query table dir, rows written)."""
    mine_dir = Path(mine_dir)
    run = Run((Path(runs_dir) if runs_dir else C.work_dir() / "runs") / variant / repo)
    row = variant_row(mine_dir, variant)
    mkey = run.model_key()
    # Node side.
    id_to_key = snapshot_keys(repo)
    missing = [i for i in run.node_ids if i not in id_to_key]
    if missing:
        raise SystemExit(f"{run.dir}: {len(missing)} node ids not in the {repo} snapshot (first {missing[0]})")
    keys = [id_to_key[i] for i in run.node_ids]
    spans = frame_spans(vdir, repo)
    shas = [node_text_sha(k, spans, run.manifest["snapshotSha256"]) for k in keys]
    nt = VecTable(table_dir(mkey, node_side(row, run.manifest)), mkey, node_side(row, run.manifest), run.dim)
    n = nt.add(run.dir, run.manifest_sha256, repo, keys, shas, run.node_vectors())
    # Query side: the texts the harness embedded, from the run's own query file.
    prefix = row.get("query_prefix", "")
    texts = {}
    for rel, sha in run.manifest["queryFiles"]:
        path = mine_dir / rel
        if C.sha256_file(path) != sha:
            raise SystemExit(f"{path} changed since the run")
        texts.update({q["id"]: q["text"] for q in C.read_jsonl(path)})
    qshas = [query_text_sha(prefix, texts[q]) for q in run.query_ids]
    qt = VecTable(table_dir(mkey, query_side(row)), mkey, query_side(row), run.dim)
    m = qt.add(run.dir, run.manifest_sha256, repo, run.query_ids, qshas, run.query_vectors())
    return nt.dir, qt.dir, n + m


def tables_for(mine_dir, variant, repo, runs_dir=None):
    """(node table, query table) a run was (or will be) imported into."""
    run = Run((Path(runs_dir) if runs_dir else C.work_dir() / "runs") / variant / repo)
    row = variant_row(mine_dir, variant)
    mkey = run.model_key()
    return (VecTable(table_dir(mkey, node_side(row, run.manifest)), mkey, node_side(row, run.manifest), run.dim),
            VecTable(table_dir(mkey, query_side(row)), mkey, query_side(row), run.dim))


# --- teacher scores ----------------------------------------------------------

def cand_set_id(version, k=TEACHER_K, rule=TEACHER_RULE):
    return sha256_text(f"{rule}|{k}|{version}")[:12]


def normalise(m):
    m = np.asarray(m, dtype=np.float64)
    n = np.linalg.norm(m, axis=1, keepdims=True)
    n[n == 0] = 1.0
    return m / n


def build_teacher(node_table, query_table, queries, version, out_root=None, k=TEACHER_K):
    """Teacher cosine scores over each query's candidate set; resumable by chunk."""
    nrows, nmat = node_table.load()
    qrows, qmat = query_table.load()
    key_row = {r["key"]: i for i, r in enumerate(nrows)}
    q_row = {r["key"]: i for i, r in enumerate(qrows)}
    by_repo = {}
    for i, r in enumerate(nrows):
        by_repo.setdefault(r["repo"], []).append(i)
    nn = normalise(nmat)
    qn = normalise(qmat)
    side = node_table.manifest["side"].split("@", 1)[1]
    out = Path(out_root or gm436_root() / "cache" / "teacher") / node_table.manifest["modelKey"] / side / cand_set_id(version, k)
    out.mkdir(parents=True, exist_ok=True)
    mpath = out / "manifest.json"
    qs = [q for q in queries if q["id"] in q_row]
    man = json.loads(mpath.read_text()) if mpath.exists() else {
        "queryIds": [q["id"] for q in qs], "nodeTable": str(node_table.dir), "queryTable": str(query_table.dir),
        "rule": TEACHER_RULE, "k": k, "version": version, "chunks": []}
    if man["queryIds"] != [q["id"] for q in qs]:
        raise SystemExit(f"{out}: query list differs from the stored manifest; remove it to rebuild")
    for c0 in range(0, len(qs), TEACHER_CHUNK):
        name = f"chunk-{c0 // TEACHER_CHUNK:05d}.npz"
        listed = next((c for c in man["chunks"] if c["file"] == name), None)
        if listed and (out / name).exists() and C.sha256_file(out / name) == listed["sha256"]:
            continue
        part = qs[c0:c0 + TEACHER_CHUNK]
        lists = []
        for q in part:
            idx = np.array(by_repo[q["repo"]])
            s = nn[idx] @ qn[q_row[q["id"]]]
            top = idx[np.argsort(-s, kind="stable")[:k]]
            labelled = [key_row[x] for x in q["positives"] + q["negatives"] + q["answersAlso"] if x in key_row]
            cand = list(dict.fromkeys(labelled + top.tolist()))
            lists.append(cand)
        width = max(len(c) for c in lists)
        cand = np.full((len(part), width), -1, np.int32)
        score = np.full((len(part), width), np.nan, np.float32)
        for i, (q, cl) in enumerate(zip(part, lists)):
            cand[i, :len(cl)] = cl
            score[i, :len(cl)] = nn[cl] @ qn[q_row[q["id"]]]
        tmp = out / (name + ".tmp")
        with open(tmp, "wb") as f:
            np.savez(f, qids=np.arange(c0, c0 + len(part), dtype=np.int32), cand=cand, score=score)
        _fsync_replace(tmp, out / name)
        man["chunks"] = [c for c in man["chunks"] if c["file"] != name] + [{"file": name, "rows": len(part), "sha256": C.sha256_file(out / name)}]
        _write_json_atomic(mpath, man)
    return out


def load_teacher(tdir):
    """{query id: (candidate node-table rows, teacher scores)} and the node table's keys."""
    tdir = Path(tdir)
    man = json.loads((tdir / "manifest.json").read_text())
    nkeys = [r["key"] for r in VecTable(man["nodeTable"]).index]
    out = {}
    for c in sorted(man["chunks"], key=lambda c: c["file"]):
        if C.sha256_file(tdir / c["file"]) != c["sha256"]:
            raise SystemExit(f"{tdir / c['file']}: sha256 differs from the manifest")
        z = np.load(tdir / c["file"])
        for qi, cand, score in zip(z["qids"], z["cand"], z["score"]):
            keep = cand >= 0
            out[man["queryIds"][qi]] = ([nkeys[j] for j in cand[keep]], score[keep].astype(np.float64))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    a1 = sub.add_parser("import", help="import (variant, repo) runs from the mine dir")
    a1.add_argument("--version", default="v0.4-full")
    a1.add_argument("--variant", required=True)
    a1.add_argument("--repos", nargs="*", help="default: every repo of queries.jsonl")
    a2 = sub.add_parser("teacher", help="teacher scores for every train+dev query")
    a2.add_argument("--version", default="v0.4-full")
    a2.add_argument("--variant", default="jina-v2-base-code-int8-structured")
    a2.add_argument("--repos", nargs="*")
    a2.add_argument("--k", type=int, default=TEACHER_K)
    a2.add_argument("--out-root", help="default: $GM436_DATA/cache/teacher")
    a = ap.parse_args()
    vdir = C.version_dir(a.version)
    mine = vdir / "mine"
    import data as D
    repos = a.repos or sorted({q["repo"] for q in D.load_queries(vdir)})
    if a.cmd == "import":
        for repo in repos:
            nd, qd, n = import_run(mine, vdir, a.variant, repo)
            print(f"imported {a.variant}/{repo}: {n} rows -> {nd.name}, {qd.name}", flush=True)
    else:
        nt, qt = tables_for(mine, a.variant, repos[0])
        qs = [q for q in D.load_queries(vdir) if q["repo"] in set(repos)]
        print(build_teacher(nt, qt, qs, a.version, out_root=a.out_root, k=a.k))


if __name__ == "__main__":
    main()
