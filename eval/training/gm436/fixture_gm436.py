"""A tiny GM-456 version dir, snapshot and Rust run dir for the GM-436 tests.

Everything lives under one temp dir: $GM456_DATA (work/<repo>.sqlite,
work/runs/<variant>/<repo>/, <version>/{queries.jsonl, nodes/, mine/}) and
$GM436_DATA (the cache). The caller sets both variables (see `env`).

Vectors are seeded: node vectors are Gaussian; a query of target t is
R c_t + noise for one fixed rotation R, so an adapter has something to learn.
A run's node order is a seeded permutation of the snapshot rows, so a join
that ignores node_ids.txt is visible. rankings.jsonl is written the way
`debug-embed-eval run` writes it: f32 cosine, ties to the lower candidate.
"""

import hashlib
import json
import sqlite3
from pathlib import Path

import numpy as np

VERSION = "v0.1"
VARIANT = "fx-structured"
PREFIX = "q: "
DIM = 8


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def key_of(repo, i):
    return f"{repo}:src/f{i}.rs#m::f{i}@{i + 1}"


class Fixture:
    def __init__(self, root, repos=("alpha",), nodes=40, train_targets=10, dev_targets=4, per_target=3, seed=436):
        self.root = Path(root)
        self.gm456 = self.root / "gm456"
        self.gm436 = self.root / "gm436"
        self.work = self.gm456 / "work"
        self.vdir = self.gm456 / VERSION
        self.mine = self.vdir / "mine"
        self.repos = list(repos)
        self.n = nodes
        rng = np.random.default_rng(seed)
        self.rotation = np.linalg.qr(rng.normal(size=(DIM, DIM)))[0]
        self.node_vec, self.query_vec, self.queries = {}, {}, []
        for d in (self.work, self.vdir / "nodes", self.mine / "queries"):
            d.mkdir(parents=True, exist_ok=True)
        (self.mine / "variants.toml").write_text(
            f'[[variant]]\nname = "{VARIANT}"\narm = "model"\ntext = "structured"\nquery_prefix = "{PREFIX}"\n')
        for repo in self.repos:
            self._repo(repo, rng, train_targets, dev_targets, per_target)
        with open(self.vdir / "queries.jsonl", "w") as f:
            for q in self.queries:
                f.write(json.dumps(q) + "\n")

    def env(self):
        return {"GM456_DATA": str(self.gm456), "GM436_DATA": str(self.gm436)}

    def _repo(self, repo, rng, train_targets, dev_targets, per_target):
        db = self.work / f"{repo}.sqlite"
        conn = sqlite3.connect(db)
        conn.execute("CREATE TABLE nodes (id TEXT, filePath TEXT, qualifiedName TEXT, startLine INTEGER)")
        conn.executemany("INSERT INTO nodes VALUES (?, ?, ?, ?)",
                         [(f"{repo}-n{i}", f"src/f{i}.rs", f"m::f{i}", i + 1) for i in range(self.n)])
        conn.commit()
        conn.close()
        # Every other node is a frame node with a span hash.
        with open(self.vdir / "nodes" / f"{repo}.jsonl", "w") as f:
            for i in range(0, self.n, 2):
                f.write(json.dumps({"key": key_of(repo, i), "spanSha256": hashlib.sha256(f"span {repo} {i}".encode()).hexdigest()}) + "\n")
        vec = rng.normal(size=(self.n, DIM))
        self.node_vec[repo] = vec
        targets = rng.permutation(self.n)[:train_targets + dev_targets]
        rows = []
        for t_i, t in enumerate(targets):
            split = "train" if t_i < train_targets else "dev"
            others = [int(x) for x in rng.permutation(self.n) if x != t][:4]
            for j in range(per_target):
                qid = f"{repo}-t{t}-q{j}"
                q = {"id": qid, "split": split, "repo": repo, "language": "rust", "target": key_of(repo, int(t)),
                     "text": f"find f{t} variant {j}", "positives": [key_of(repo, int(t))],
                     "answersAlso": [key_of(repo, others[3])] if j == 0 else [],
                     "negatives": [{"key": key_of(repo, o)} for o in others[:3]]}
                rows.append(q)
                self.query_vec[qid] = self.rotation.T @ vec[t] + 0.3 * rng.normal(size=DIM)
        self.queries += rows
        with open(self.mine / "queries" / f"{repo}.jsonl", "w") as f:
            for q in rows:
                f.write(json.dumps({"id": q["id"], "text": q["text"]}) + "\n")
        self.write_run(repo)

    def run_dir(self, repo, variant=VARIANT):
        return self.work / "runs" / variant / repo

    def write_run(self, repo, order_seed=1, gmesh_version="0.0.0", snapshot_sha=None):
        """(Re)writes the Rust run dir of `repo`; another `order_seed` or
        `gmesh_version` is another run (another manifest sha256)."""
        run = self.run_dir(repo)
        run.mkdir(parents=True, exist_ok=True)
        order = np.random.default_rng(order_seed).permutation(self.n)
        node_ids = [f"{repo}-n{i}" for i in order]
        nvec = self.node_vec[repo][order].astype("<f4")
        qids = [q["id"] for q in self.queries if q["repo"] == repo]
        qvec = np.stack([self.query_vec[i] for i in qids]).astype("<f4")
        (run / "node_ids.txt").write_text("\n".join(node_ids) + "\n")
        (run / "vectors.bin").write_bytes(nvec.tobytes())
        (run / "query_vectors.bin").write_bytes(qvec.tobytes())
        qfile = f"queries/{repo}.jsonl"
        manifest = {"variant": VARIANT, "variantFingerprint": "f" * 64, "corpus": repo, "revision": "rev",
                    "snapshotSha256": snapshot_sha or sha256_file(self.work / f"{repo}.sqlite"),
                    "nodeIdsSha256": hashlib.sha256("\n".join(node_ids).encode()).hexdigest(), "nodeCount": self.n, "dimension": DIM, "queryIds": qids,
                    "queryFiles": [[qfile, sha256_file(self.mine / qfile)]],
                    "modelFiles": [["model.onnx", "ab" * 32, 1]], "gmeshVersion": gmesh_version}
        (run / "manifest.json").write_text(json.dumps(manifest, indent=1))
        nn = nvec / np.linalg.norm(nvec, axis=1, keepdims=True)
        qn = qvec / np.linalg.norm(qvec, axis=1, keepdims=True)
        scores = (qn @ nn.T).astype(np.float32)
        with open(run / "rankings.jsonl", "w") as f:
            for qid, s in zip(qids, scores):
                top = sorted(range(len(s)), key=lambda j: (-s[j], j))[:100]
                f.write(json.dumps({"id": qid, "hits": [[node_ids[j], float(s[j])] for j in top]}) + "\n")
        return run
