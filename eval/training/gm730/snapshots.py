#!/usr/bin/env python3
"""Pinned Hugging Face snapshots for the GM-730 routes
(docs/design/GM-730-gpu-training-routes.md, gap G3 and section 9.1 M9).

Each snapshot is plain files at one revision under

    $GM730_DATA/hf/<repo name>@<revision>/
        <files of PINS[name]>
        MANIFEST.sha256.txt   "<path> <size> <sha256>" per file, sorted by path
        snapshot.json         {name, repo, revision, files: {path: sha256}, code?}

and is uploaded as a `[[data]]` set, so the instance loads every model with
HF_HUB_OFFLINE=1. LFS files are checked against the sha256 pinned below
(the Hub's LFS oid at that revision).

jina's config.json maps `auto_map` to the remote repo
jinaai/jina-bert-v2-qk-post-norm without a revision, which an offline load
cannot resolve. `fetch("jina")` copies that repo's two code files, pinned by
revision and sha256, into the snapshot and rewrites `auto_map` to the local
modules; snapshot.json records the code files' sha256 and the published
config.json's sha256.

usage: snapshots.py fetch <name>... [--with-onnx]   (names: jina, jina-code, gte-mb, gte-small, l6)
       snapshots.py verify <name>...                 (offline: files vs MANIFEST and pins)
       snapshots.py path <name>
"""

import argparse
import hashlib
import json
import os
import shutil
import sys
from pathlib import Path

LFS = "lfs"
# name -> repo, revision, {path: pinned sha256 or None}, {optional path: pinned sha256}
PINS = {
    "jina": ("jinaai/jina-embeddings-v2-base-code", "516f4baf13dec4ddddda8631e019b5737c8bc250",
             {"config.json": None, "tokenizer.json": None, "tokenizer_config.json": None,
              "special_tokens_map.json": None,
              "model.safetensors": "8b53bfd4ae2cd586004a6ca4a16551b630a2a1b1d655ff1ee9be1286a1781c5b"},
             {"onnx/model.onnx": "63363fc178428b74620c6f3780cbc7191883fa5c7f84c0945c45eb5c4256733b"}),
    "jina-code": ("jinaai/jina-bert-v2-qk-post-norm", "3baf9e3ac750e76e8edd3019170176884695fb94",
                  {"configuration_bert.py": "d018de11bcb62cedf436c8876459e5cd119d1faf72bc107338576a6d1b8eeea5",
                   "modeling_bert.py": "8f96ef0576401f6b996f9b72ad34e13190139eec1195767516485876434bb612"},
                  {}),
    "gte-mb": ("Alibaba-NLP/gte-reranker-modernbert-base", "f7481e6055501a30fb19d090657df9ec1f79ab2c",
               {"config.json": None, "tokenizer.json": None, "tokenizer_config.json": None,
                "special_tokens_map.json": None,
                "model.safetensors": "13c533d902d6a48fa97375c6856c3e60854f9228ea760e3fc18ebde057e0cf31"},
               {}),
    "gte-small": ("thenlper/gte-small", "17e1f347d17fe144873b1201da91788898c639cd",
                  {"config.json": None, "tokenizer.json": None, "tokenizer_config.json": None,
                   "special_tokens_map.json": None, "vocab.txt": None,
                   "model.safetensors": "9a1eb90bbac323ea08aa5629b624fe6ae75db121b904799c2266a1e2c2de22d2"},
                  {"onnx/model.onnx": "0b01312b59bec0a2558a626f2937be4cbe4bb16d1511560153f598cec488f1f8"}),
    "l6": ("cross-encoder/ms-marco-MiniLM-L6-v2", "233902d25c440f23af6f7d6e94d2946bac0bee0a",
           {"config.json": None, "tokenizer.json": None, "tokenizer_config.json": None,
            "special_tokens_map.json": None, "vocab.txt": None,
            "model.safetensors": "821d1aa69520101d6e0737f78a042ae25b19e5cb9160701909d10434f4aeb0ae"},
           {"onnx/model.onnx": "5d3e70fd0c9ff14b9b5169a51e957b7a9c74897afd0a35ce4bd318150c1d4d4a",
            "onnx/model_quint8_avx2.onnx": "c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9"}),
}
# snapshot name -> the code snapshot whose files are copied into it
CODE = {"jina": "jina-code"}
# sha256 of the published config.json of a snapshot whose auto_map is rewritten
PUBLISHED_CONFIG = {"jina": "e426aa684c7f9a95c5f020aa855faf93a24f065f5fad0c9e17b124670cabdea6"}
MANIFEST = "MANIFEST.sha256.txt"
META = "snapshot.json"


def data_root():
    return Path(os.environ.get("GM730_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm730"))


def hf_root():
    """The snapshots' parent: $GM730_HF when set (a `[[data]]` set on the instance), else <GM730_DATA>/hf."""
    return Path(os.environ["GM730_HF"]) if os.environ.get("GM730_HF") else data_root() / "hf"


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def path_of(name):
    repo, rev, _, _ = PINS[name]
    return hf_root() / f"{repo.split('/')[-1]}@{rev}"


def local_auto_map(auto_map):
    """`<repo>--<module>.<Class>` -> `<module>.<Class>`; a value without `--` is kept."""
    return {k: v.split("--", 1)[1] if "--" in v else v for k, v in auto_map.items()}


def write_manifest(dest):
    """MANIFEST.sha256.txt over every file under dest except the manifest and snapshot.json."""
    rows = []
    for p in sorted(dest.rglob("*")):
        rel = p.relative_to(dest).as_posix()
        if p.is_file() and p.name not in (MANIFEST, META) and not rel.startswith(".cache/"):
            rows.append(f"{rel} {p.stat().st_size} {sha256_file(p)}")
    (dest / MANIFEST).write_text("\n".join(rows) + "\n")
    return {r.split(" ")[0]: r.split(" ")[2] for r in rows}


def check_pins(dest, pins):
    bad = []
    for rel, want in pins.items():
        p = dest / rel
        if not p.is_file():
            bad.append(f"{rel}: missing")
        elif want is not None and sha256_file(p) != want:
            bad.append(f"{rel}: sha256 {sha256_file(p)} != pinned {want}")
    return bad


def download(repo, rev, files, dest):
    from huggingface_hub import hf_hub_download
    for rel in files:
        if not (dest / rel).is_file():
            hf_hub_download(repo, rel, revision=rev, local_dir=str(dest))


def add_code(dest, name):
    """Copy the pinned code files into dest and point auto_map at them. A config.json
    still naming the remote repo must be the published one. Returns the snapshot.json `code` entry."""
    code_name = CODE[name]
    crepo, crev, cpins, _ = PINS[code_name]
    src = path_of(code_name)
    bad = check_pins(src, cpins)
    if bad:
        raise SystemExit(f"{src}: {bad}")
    for rel in cpins:
        shutil.copyfile(src / rel, dest / rel)
    cfg_path = dest / "config.json"
    cfg = json.loads(cfg_path.read_text())
    if any("--" in v for v in cfg["auto_map"].values()) and sha256_file(cfg_path) != PUBLISHED_CONFIG[name]:
        raise SystemExit(f"{cfg_path}: sha256 {sha256_file(cfg_path)} != published {PUBLISHED_CONFIG[name]}")
    cfg["auto_map"] = local_auto_map(cfg["auto_map"])
    cfg_path.write_text(json.dumps(cfg, indent=2) + "\n")
    return {"repo": crepo, "revision": crev, "files": dict(cpins), "publishedConfigSha256": PUBLISHED_CONFIG[name],
            "autoMap": cfg["auto_map"]}


def fetch(name, with_onnx=False):
    """Download PINS[name] at its revision (files already present are kept), check the
    pins, add the code files for jina, write MANIFEST and snapshot.json. Returns the dir."""
    repo, rev, pins, optional = PINS[name]
    dest = path_of(name)
    dest.mkdir(parents=True, exist_ok=True)
    want = dict(pins, **(optional if with_onnx else {}))
    download(repo, rev, list(want), dest)
    meta = {"name": name, "repo": repo, "revision": rev}
    if name in CODE:
        fetch(CODE[name])
        meta["code"] = add_code(dest, name)
    bad = check_pins(dest, {k: v for k, v in want.items()})
    if bad:
        raise SystemExit(f"{dest}: {bad}")
    meta["files"] = write_manifest(dest)
    (dest / META).write_text(json.dumps(meta, indent=1, sort_keys=True) + "\n")
    shutil.rmtree(dest / ".cache", ignore_errors=True)
    return dest


def verify(name):
    """Offline: every MANIFEST row matches its file, the pins hold, and a code snapshot's
    files equal the pinned code. Returns a list of problems (empty = ok)."""
    dest = path_of(name)
    if not (dest / MANIFEST).is_file():
        return [f"{dest}: no {MANIFEST}"]
    bad = []
    for line in (dest / MANIFEST).read_text().splitlines():
        rel, size, sha = line.split(" ")
        p = dest / rel
        if not p.is_file():
            bad.append(f"{rel}: missing")
        elif p.stat().st_size != int(size) or sha256_file(p) != sha:
            bad.append(f"{rel}: differs from {MANIFEST}")
    _, _, pins, optional = PINS[name]
    bad += check_pins(dest, pins)
    bad += check_pins(dest, {k: v for k, v in optional.items() if (dest / k).exists()})
    if name in CODE:
        cpins = PINS[CODE[name]][2]
        bad += check_pins(dest, cpins)
        cfg = json.loads((dest / "config.json").read_text())
        if any("--" in v for v in cfg.get("auto_map", {}).values()):
            bad.append("config.json: auto_map still names a remote repo")
    return bad


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    f = sub.add_parser("fetch")
    f.add_argument("names", nargs="+", choices=sorted(PINS))
    f.add_argument("--with-onnx", action="store_true", help="also the official ONNX files (C-R inputs)")
    v = sub.add_parser("verify")
    v.add_argument("names", nargs="+", choices=sorted(PINS))
    p = sub.add_parser("path")
    p.add_argument("name", choices=sorted(PINS))
    a = ap.parse_args(argv)
    if a.cmd == "path":
        print(path_of(a.name))
        return 0
    rc = 0
    for name in a.names:
        if a.cmd == "fetch":
            print(f"{name}: {fetch(name, a.with_onnx)}", flush=True)
        bad = verify(name)
        print(f"{name}: verify {'ok' if not bad else 'FAILED ' + '; '.join(bad)}", flush=True)
        rc |= bool(bad)
    return rc


if __name__ == "__main__":
    sys.exit(main())
