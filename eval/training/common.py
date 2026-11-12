"""Shared paths and helpers of the GM-456 training-data pipeline
(docs/design/GM-456-training-data.md).

Layout: `$GM456_DATA` (default ~/Projects/ClaudeProjects/g-mesh-data/gm456)
holds `work/` (checkouts, snapshots, encoder runs, caches; eval/training/work
links to it) and one folder per dataset version, `v<major>.<minor>[-tag]/`.
"""

import hashlib
import json
import os
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
EVAL = REPO / "eval" / "embedding"
sys.path.insert(0, str(EVAL))

try:
    import tomllib as _toml  # Python 3.11+

    def _load_toml(path):
        with open(path, "rb") as f:
            return _toml.load(f)
except ImportError:  # Python 3.9/3.10: the `toml` package
    import toml as _toml

    def _load_toml(path):
        with open(path) as f:
            return _toml.load(f)


def data_root():
    return Path(os.environ.get("GM456_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm456"))


def work_dir():
    return data_root() / "work"


def version_dir(version):
    if not re.fullmatch(r"v\d+\.\d+(-[A-Za-z0-9]+)?", version):
        sys.exit(f"STOP: version {version!r} is not v<major>.<minor>[-tag]")
    return data_root() / version


def corpora(path=None):
    """corpora.toml's repos, by id, in file order."""
    rows = _load_toml(path or HERE / "corpora.toml")["corpus"]
    return {r["id"]: r for r in rows}


def key_of(repo, file_path, qualified_name, start_line):
    """A node's stable key; it does not use `nodes.id`, which an indexer
    version can change."""
    return f"{repo}:{file_path}#{qualified_name}@{start_line}"


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def normalised_span_sha256(lines):
    """sha256 of a code span with all whitespace runs collapsed to one space."""
    return sha256_bytes(" ".join("\n".join(lines).split()).encode())


def read_jsonl(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def write_jsonl(path, rows):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with open(tmp, "w") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False, sort_keys=False) + "\n")
    os.replace(tmp, path)


def write_json(path, obj):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with open(tmp, "w") as f:
        json.dump(obj, f, indent=2, ensure_ascii=False)
        f.write("\n")
    os.replace(tmp, path)


def load_nodes(vdir, repo):
    return read_jsonl(Path(vdir) / "nodes" / f"{repo}.jsonl")


def node_repos(vdir):
    return sorted(p.stem for p in (Path(vdir) / "nodes").glob("*.jsonl"))


def words(text):
    """Word set of the GM-460 near-duplicate review (`v2/neardup.py`)."""
    return {w.lower() for w in re.findall(r"[A-Za-z0-9]+", text)}


def jaccard(a, b):
    if not a and not b:
        return 0.0
    return len(a & b) / len(a | b)


def quantiles(values, qs=(0.0, 0.5, 0.9, 0.95, 0.99, 1.0)):
    v = sorted(values)
    if not v:
        return {}
    out = {}
    for q in qs:
        i = min(len(v) - 1, max(0, int(round(q * (len(v) - 1)))))
        out[f"p{int(q * 100)}"] = round(v[i], 4)
    return out


def histogram(values, edges):
    """Counts per [edges[i], edges[i+1]) bucket, the last bucket closed."""
    counts = [0] * (len(edges) - 1)
    for x in values:
        for i in range(len(edges) - 1):
            last = i == len(edges) - 2
            if edges[i] <= x < edges[i + 1] or (last and x == edges[-1]):
                counts[i] += 1
                break
    return {f"[{edges[i]:.2f},{edges[i + 1]:.2f}{']' if i == len(edges) - 2 else ')'}": c
            for i, c in enumerate(counts)}
