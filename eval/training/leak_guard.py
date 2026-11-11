#!/usr/bin/env python3
"""Leak guard of the GM-456 training data
(docs/design/GM-456-training-data.md, section 4).

Eval inputs, from all three studies (GM-398 `eval/embedding`, GM-422
`confirm`, GM-460 `v2/new`), each file checked against its sha256 in PINS
before use; `v2/queries/<c>.jsonl` must equal the concatenation of the three
studies' authored files and `v2/queries/mechanical/<c>.jsonl` the GM-398
mechanical file:
  - query texts, positive and absent, mechanical included;
  - symbol keys per study and corpus, `sample_targets.exclusion` (consumed
    targets plus every expected symbol of the authored queries) plus every
    expected symbol of the mechanical queries, with their code read from the
    eval snapshots (each sqlite checked against SQLITE_PINS and its
    snapshot.json) and checkouts under `--eval-work`.

Checks, each naming the rows it fires on:
  1 repo-identity   a training repo's normalised URL or root commit equals an
                    eval corpus's (catches forks);
  2 exact-code      a positive or negative whose whitespace-normalised span
                    sha256 equals an eval key's (catches vendored copies);
  3 code-neardup    shingle Jaccard or containment of the eval span in the
                    training span >= CODE_JACCARD (SHINGLE-grams of
                    `bm25::terms` tokens, spans of >= MIN_TOKENS tokens);
                    containment catches an eval function copied inside a
                    larger Type/Module/File node;
  4 query-jaccard   word Jaccard >= QUERY_JACCARD with any eval query text
                    (`v2/neardup.py` tokens); also run on the generator
                    prompt's few-shot texts (a prompt with no ```fewshot
                    block is an error);
  5 query-embed     jina int8 cosine >= COSINE with any eval query text
                    (Python onnxruntime, mean pooling, L2; vectors cached by
                    text sha256 under work/qvec-cache).

  --check    exit 1 on any hit (export runs this last);
  --filter   write the dataset minus the hits (rows of a hit query, repo or
             positive; the single negative of a negative-code hit) as
             <stem>.filtered.jsonl, the hits as filter.leaks.jsonl and the
             report, with kept/dropped counts and the output's sha256, as
             filter.guard.json (export deletes guard.json and leaks.jsonl);
  --pooling-check RUN_DIR   compare Python query vectors with a Rust run's
             query_vectors.bin on its first N queries; exit 1 below
             POOLING_BAR.

Every --check run writes guard.json and leaks.jsonl beside the dataset:
per-check hit counts and the nearest-eval cosine and Jaccard distributions.

usage: leak_guard.py --version v0.0-stub --check [--dataset queries.jsonl]
"""

import argparse
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

import common as C
from sample_targets import exclusion

CODE_JACCARD = 0.7
QUERY_JACCARD = 0.4
COSINE = 0.90
SHINGLE = 5
MIN_TOKENS = 20
STUDIES = ("", "confirm", "v2/new")
EVAL_CORPORA = ("gin", "py-requests", "ripgrep", "g-mesh", "excalidraw", "task-tracker-mcp")
MODEL_DIR = "models/jina-v2-base-code-int8"
MAX_TOKENS = 1024
# Python vs Rust query-vector cosine floor (GM-456/S9 L1): vectors agree to
# cos 1.000000 up to 15 tokens and drift from 16 (min 0.99478 over 2279
# queries; likely the int8 GEMM kernels of the two ORT builds); 0 verdict
# flips at COSINE over 2.6M pairs.
POOLING_BAR = 0.994

# sha256 of each eval snapshot sqlite, equal to its <c>.snapshot.json record.
SQLITE_PINS = {
    "excalidraw": "3c7bcb4601c64f93bf8b6b043f8140f8b0f9781a6e292e0b236fdfd2102dca4d",
    "g-mesh": "1fd9a1563cd55afa2eb9be061735dc56394027ad061bfac64e2ac0c65d03fd02",
    "gin": "1e013b7630a1f154dbe946ca243ee881d5dfe0bea9861816a100c74b4e068c62",
    "py-requests": "d24c70266c36a27af0d923c8fc6ab65d37cdad3c0a71d8c7c7c38de3e8afe7fd",
    "ripgrep": "d00f8a256d17c0cda5fe81c1a0c744a697d8c2cce9d5a6075e11f9ff3954aca0",
    "task-tracker-mcp": "6e2381ba9123712a3114c5a6127fe4f29e0a27e372147ac392c8ad2ce74041ff",
}

# sha256 of every eval file the guard reads, relative to eval/embedding.
# GM-398 and GM-422 authored files agree with v2/build.py's PINNED.
PINS = {
    "queries/excalidraw.jsonl": "9cbdd1dc78bed9e941f149f1aceb3f8b836e13d7bec3a7b2bbbfb46705de6782",
    "queries/g-mesh.jsonl": "4e290fb888704a604e6efe003e5c2e1b7564518131ca4eb97c95087a078fa133",
    "queries/gin.jsonl": "8e4bc84b1c5bf8479c5296b832fe856b42d8e91bc468ac8100ca675d33b177a7",
    "queries/py-requests.jsonl": "17b75b266e218961abebdd08a74d912b62b01e978f60010b742e6d50b5505e0e",
    "queries/ripgrep.jsonl": "cfb3c016bee4adacb1302fb31f10d50912e650d2b1c7bc9ac82e2d0b21ef083c",
    "queries/task-tracker-mcp.jsonl": "16dc63f7b43e2dad09ba5492520ebac148d0727168f4ac49d2b3d6128b4c20d3",
    "queries/mechanical/excalidraw.jsonl": "170e8e9a2ae13cac484847c7243ed426dca767aec776f05e47a9afa78d1f9611",
    "queries/mechanical/g-mesh.jsonl": "fb4f49abd6d67c85c4a12239833c98d5fdb12f964a1766803244453ff6c5ec47",
    "queries/mechanical/gin.jsonl": "db6b1a740e3bbd7671f00ec225cb9bdc5bd6eee7872f9c1a1eeaf7f91815cf4c",
    "queries/mechanical/py-requests.jsonl": "c6433cd01bd839dd55215da2d3e17318f162687d04bfe26e4fc8b568c72655f9",
    "queries/mechanical/ripgrep.jsonl": "0131dd68c2eba18ed06cdebfe8c7f4ec55cee9d1fac1cf8890d685ef5545962a",
    "queries/mechanical/task-tracker-mcp.jsonl": "6298ecc050539b13d7f7093d20c1ad7f448f7cfd6bebc0bdc64e26da3ef2dcfa",
    "confirm/queries/excalidraw.jsonl": "0e703f65f9a616028c9b9cf56cc8c89635119a9e7e69280538cbd9537da1eb41",
    "confirm/queries/g-mesh.jsonl": "39a6d97af21589eaa76fb1d98681bb90f9aa8784597d106e5fe64ee3d51d26b1",
    "confirm/queries/gin.jsonl": "50757662698de019c9450615dee79b48196b1373ff828bcbf25e4ed03718ba3b",
    "confirm/queries/py-requests.jsonl": "7fedbf196f826e17776f0a330e11d9cd50f8b227159880b149fc693cd81670ce",
    "confirm/queries/ripgrep.jsonl": "51b126d72394ddc8b1076697bb077052e0e63b1d37a9414cd00ceecc3705e3cc",
    "confirm/queries/task-tracker-mcp.jsonl": "c698ace39f4a2c337702e197cd022c0e5a1541665f226cef4a43d043e25a8ee2",
    "confirm/queries/mechanical/excalidraw.jsonl": "170e8e9a2ae13cac484847c7243ed426dca767aec776f05e47a9afa78d1f9611",
    "confirm/queries/mechanical/g-mesh.jsonl": "fb4f49abd6d67c85c4a12239833c98d5fdb12f964a1766803244453ff6c5ec47",
    "confirm/queries/mechanical/gin.jsonl": "db6b1a740e3bbd7671f00ec225cb9bdc5bd6eee7872f9c1a1eeaf7f91815cf4c",
    "confirm/queries/mechanical/py-requests.jsonl": "c6433cd01bd839dd55215da2d3e17318f162687d04bfe26e4fc8b568c72655f9",
    "confirm/queries/mechanical/ripgrep.jsonl": "0131dd68c2eba18ed06cdebfe8c7f4ec55cee9d1fac1cf8890d685ef5545962a",
    "confirm/queries/mechanical/task-tracker-mcp.jsonl": "6298ecc050539b13d7f7093d20c1ad7f448f7cfd6bebc0bdc64e26da3ef2dcfa",
    "v2/new/queries/excalidraw.jsonl": "4d939369998278e51e6f613eee12dfe9f5e93c6a51847664a6855d893e231e7d",
    "v2/new/queries/g-mesh.jsonl": "feb653245ab856f6b5726c486416e7f3d8d08656191eef3608185f32a3e41d6a",
    "v2/new/queries/gin.jsonl": "8f30231a453b9e8c8dc1052f4ec2461311ba662d08e7f453ed6c31172ab96547",
    "v2/new/queries/py-requests.jsonl": "dc1a2cfe9009a97fc73421661585be968e4bb60d941c01b7df0f4815e16744c9",
    "v2/new/queries/ripgrep.jsonl": "ab3e0019a75a5ffc7743d445c4bdadd63ea954c7e70e5aaa07260e468a706a8a",
    "v2/new/queries/task-tracker-mcp.jsonl": "c3460a45aab0c5d7c1c87445b7602136f9ae19cdcb45062f7786c6d51584eadf",
    "v2/queries/excalidraw.jsonl": "f545e5553c566835a781721d9a6daea5c5244262b7d3141dfe8bfd3cfed89f78",
    "v2/queries/g-mesh.jsonl": "e551b3461e3375080d8e6f91f6eb3efd0911a384acbaad3846e46dbb06f106a7",
    "v2/queries/gin.jsonl": "71fa5b134a68242b6f45862d6b19a407cf1642310e30036624ac4bf5e7da8306",
    "v2/queries/py-requests.jsonl": "ce3a068fca2a3f5a87af76756440a7498bcd8a1dde8add5697bbbfdc352e599a",
    "v2/queries/ripgrep.jsonl": "b498fd50fe91e0fbd59fc4de96d48e800c37da5cdb246e91aaf4f7481a5b228c",
    "v2/queries/task-tracker-mcp.jsonl": "570ec16dfcbe85b12605cd3fd2c65139ab45c0b9210cc02bd807443044e4463b",
    "v2/queries/mechanical/excalidraw.jsonl": "170e8e9a2ae13cac484847c7243ed426dca767aec776f05e47a9afa78d1f9611",
    "v2/queries/mechanical/g-mesh.jsonl": "fb4f49abd6d67c85c4a12239833c98d5fdb12f964a1766803244453ff6c5ec47",
    "v2/queries/mechanical/gin.jsonl": "db6b1a740e3bbd7671f00ec225cb9bdc5bd6eee7872f9c1a1eeaf7f91815cf4c",
    "v2/queries/mechanical/py-requests.jsonl": "c6433cd01bd839dd55215da2d3e17318f162687d04bfe26e4fc8b568c72655f9",
    "v2/queries/mechanical/ripgrep.jsonl": "0131dd68c2eba18ed06cdebfe8c7f4ec55cee9d1fac1cf8890d685ef5545962a",
    "v2/queries/mechanical/task-tracker-mcp.jsonl": "6298ecc050539b13d7f7093d20c1ad7f448f7cfd6bebc0bdc64e26da3ef2dcfa",
    "targets/excalidraw.jsonl": "406dea7b95bc3b8e3f057fbc5bb9dee810f75ab78e5aa4b8cd5357c88940d7ab",
    "targets/excalidraw.skips.jsonl": "3bf9485a9f12ec622d0db2e472ff64d2bf552dffc920b5dac2066c918d7232c3",
    "targets/g-mesh.jsonl": "a462d85aed0182eb679b41333aebbf29bc3e168264c3fa237bf2f752674152f8",
    "targets/g-mesh.skips.jsonl": "ec5730c51a510b0ce50cbdf6782f36ab0aa73001c4546bf133dddc52149ee093",
    "targets/gin.jsonl": "337ee35a49867054b19554e18526f54e6be41ab4c5422c60ac20718e13f721c8",
    "targets/gin.skips.jsonl": "dcf597ecb2346ad4ca2fbef8c0fa939704b6957bba393fa3a50a1b7c21bdb608",
    "targets/py-requests.jsonl": "633cdedd97e49765170c9b54984716a7531a14f0b582678be3f4f95463df0622",
    "targets/py-requests.skips.jsonl": "d7ff51e55b62f6a025696df667702bc2e2a79e19d65c44aa34c852606442a9db",
    "targets/ripgrep.jsonl": "ef423c60d8480ca704414de4aac727c635965daafe63c55a64527ad9c41d2dbd",
    "targets/ripgrep.skips.jsonl": "a3b711699a3052fa0156abf287c32d149d766bf2380b0292409bf1bdaf224676",
    "targets/task-tracker-mcp.jsonl": "547baef4d8729ba150bc8aa02fb60d542648d5caf7b6ab0937c992ad65032a31",
    "targets/task-tracker-mcp.skips.jsonl": "36c887424b82bdf4f7d41c3d6b133128d695e69c69039c5339141b54deffbbe4",
    "confirm/targets/excalidraw.jsonl": "26d921b7ec533257f1badb1ddaebc1a3972dd5736301ebb85039f00c542165cd",
    "confirm/targets/excalidraw.skips.jsonl": "0dcd7f6431df4ae3f27b46d5a883ec1fee4e016d0891e2967f7d8e4ad3f9f03b",
    "confirm/targets/g-mesh.jsonl": "335686f6f7c7b179a3b26bd2ac88bd92ac5be0bd4027275d357783a8a288209b",
    "confirm/targets/g-mesh.skips.jsonl": "aca1291deabf1582a21edd565d39da791b50bf21444d801bb8a87a0382e25419",
    "confirm/targets/gin.jsonl": "eb7431e8a1c4099e02ed65aaceca8964d29f3053955dd58c79c92837e1ec5bdc",
    "confirm/targets/gin.skips.jsonl": "73ebbd12a59d4fd996bb52ffaa7c734a557cb222eb920302ba5c7bf91e11c84b",
    "confirm/targets/py-requests.jsonl": "e081b4e753ad5867a488920d45d0f072a1b9f5ad6cda7f548608059b136eab4b",
    "confirm/targets/py-requests.skips.jsonl": "edef9b788cc8f3f98f9345cdb224e8430071e3f507bb60013fc139e9024170ed",
    "confirm/targets/ripgrep.jsonl": "ac59dc6e2c9b533b8b8b21e995581e02d1fbc8af2e135fdbb804e9780ed28eff",
    "confirm/targets/ripgrep.skips.jsonl": "7991b3a66ec07047831617d7b8b54c75a670174ada4f45ca35d90d34fc23039e",
    "confirm/targets/task-tracker-mcp.jsonl": "cdb94cad47f05efbffba072764a97b946bef32297b25d26549fd152cfc2cbb57",
    "confirm/targets/task-tracker-mcp.skips.jsonl": "99d074b9d146e34c58abc4ded8f76d0048e4f86a4a04cc56ab82c971fe3b6e48",
    "v2/new/targets/excalidraw.jsonl": "c495f82eac093ed66d514a0defb668d1cb40e4b397d7b049e69c046409b14ef5",
    "v2/new/targets/excalidraw.skips.jsonl": "03cf436a6af8fba02a1af647a33af34527e3c8709e9f96de3098e92c6e4c3744",
    "v2/new/targets/g-mesh.jsonl": "79bf08a4a74d7b20d8c4d371ba26552720ba04ff31d3aae8f61fa98382d4b951",
    "v2/new/targets/g-mesh.skips.jsonl": "82601f66abf9725b8fe12f060c17156279566f99f62544fa9b67318b64d693ec",
    "v2/new/targets/gin.jsonl": "ab357c5063e2626ba649d4f169ef47bbc6b4f96c098488d61c073cc22975b1b1",
    "v2/new/targets/gin.skips.jsonl": "e321ca1ba6f2cfa7630561a4f991f9f5b247c1fa55658cf86279140d95fbb875",
    "v2/new/targets/py-requests.jsonl": "6adee6145f9a3223b571d06c2fa01b37c35761adc2893b04c79ba8e49fa4ef70",
    "v2/new/targets/py-requests.skips.jsonl": "a6caa5b43d4d80d237b83a1a89cdd913d07a8e257072294f2b94980100092eb4",
    "v2/new/targets/ripgrep.jsonl": "40f9db102c6d6da96d20c16bf4b044c7a2161e4f3099990311f31aa8cfa5394f",
    "v2/new/targets/ripgrep.skips.jsonl": "949ecb227665f2efa3b1ea0b6fde45e58395205a4088c6cef2cb482861cd436e",
    "v2/new/targets/task-tracker-mcp.jsonl": "61a9b9b8f2134af994213eeeb661608da2c857e1093e770127717984c05b6f50",
    "v2/new/targets/task-tracker-mcp.skips.jsonl": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
}


# --- eval inputs ----------------------------------------------------------

def default_eval_work():
    out = subprocess.run(["git", "-C", str(C.REPO), "worktree", "list", "--porcelain"],
                         capture_output=True, text=True, check=True).stdout
    main = out.splitlines()[0].split(" ", 1)[1]
    return Path(main) / "eval" / "embedding" / "work"


def verify_pins(eval_root, pins=None):
    """Errors for a pinned file that is missing or changed, an eval query or
    target file with no pin, and a v2 merged file that is not the
    concatenation of its parts."""
    pins = PINS if pins is None else pins
    root = Path(eval_root)
    errors = []
    for rel, sha in sorted(pins.items()):
        p = root / rel
        if not p.is_file():
            errors.append(f"{rel}: missing")
        elif C.sha256_file(p) != sha:
            errors.append(f"{rel}: sha256 {C.sha256_file(p)}, pinned {sha}")
    for study in STUDIES + ("v2",):
        base = root / study if study else root
        for sub in ("queries", "queries/mechanical", "targets"):
            for p in sorted((base / sub).glob("*.jsonl")):
                rel = str(p.relative_to(root))
                if rel not in pins:
                    errors.append(f"{rel}: not pinned")
    for c in EVAL_CORPORA:
        parts = b"".join((root / s / "queries" / f"{c}.jsonl").read_bytes() if (root / s / "queries" / f"{c}.jsonl").exists() else b""
                         for s in STUDIES)
        merged = root / "v2" / "queries" / f"{c}.jsonl"
        if merged.exists() and merged.read_bytes() != parts:
            errors.append(f"v2/queries/{c}.jsonl is not the concatenation of the three studies")
        mech = root / "v2" / "queries" / "mechanical" / f"{c}.jsonl"
        if mech.exists() and mech.read_bytes() != (root / "queries" / "mechanical" / f"{c}.jsonl").read_bytes():
            errors.append(f"v2/queries/mechanical/{c}.jsonl differs from queries/mechanical/{c}.jsonl")
    return errors


def eval_queries(eval_root):
    """[(id, text)] of every eval query of the three studies (v2/queries is
    their concatenation, so it is not read twice)."""
    out = []
    root = Path(eval_root)
    for study in STUDIES:
        base = root / study if study else root
        for sub in ("queries", "queries/mechanical"):
            for p in sorted((base / sub).glob("*.jsonl")):
                for q in C.read_jsonl(p):
                    out.append((f"{study or 'gm398'}:{q['id']}", q["text"]))
    return out


def eval_keys(eval_root):
    """{corpus: {(filePath, qualifiedName)}} over the three studies: the
    `exclusion` keys plus the expected symbols of the mechanical queries."""
    root = Path(eval_root)
    keys = defaultdict(set)
    for study in STUDIES:
        d = root / study if study else root
        for c in EVAL_CORPORA:
            if (d / "targets" / f"{c}.jsonl").exists():
                k, _ = exclusion(str(d), c)
                keys[c] |= k
            mech = d / "queries" / "mechanical" / f"{c}.jsonl"
            if mech.exists():
                keys[c] |= {(e["filePath"], e["qualifiedName"]) for q in C.read_jsonl(mech)
                            for e in q.get("expected", [])}
    return keys


def git(path, *args):
    return subprocess.run(["git", "-C", str(path), *args], capture_output=True, text=True, check=True).stdout.strip()


def eval_spans(eval_work, keys):
    """[(label, span lines)] of every eval key, read from the eval snapshot's
    lines and the checkout at the snapshot's revision."""
    import sqlite3
    work = Path(eval_work)
    out = []
    for c, ks in sorted(keys.items()):
        record = json.load(open(work / f"{c}.snapshot.json"))
        checkout = work / "corpora" / c
        head = git(checkout, "rev-parse", "HEAD")
        if head != record["revision"]:
            sys.exit(f"STOP: eval checkout {checkout} is at {head}, its snapshot at {record['revision']}")
        db_sha = C.sha256_file(work / f"{c}.sqlite")
        if c not in SQLITE_PINS or db_sha != SQLITE_PINS[c] or db_sha != record["sha256"]:
            sys.exit(f"STOP: eval snapshot {work / f'{c}.sqlite'} sha256 {db_sha}, "
                     f"pinned {SQLITE_PINS.get(c)}, snapshot.json {record['sha256']}")
        conn = sqlite3.connect(f"file:{work / f'{c}.sqlite'}?mode=ro", uri=True)
        rows = conn.execute("SELECT filePath, qualifiedName, startLine, endLine FROM nodes").fetchall()
        conn.close()
        files = {}
        for f, q, s, e in rows:
            if (f, q) not in ks:
                continue
            if f not in files:
                p = checkout / f
                files[f] = p.read_text(errors="replace").split("\n") if p.is_file() else None
            if files[f] is not None:
                out.append((f"{c}:{f}#{q}@{s}", files[f][s:e + 1]))
    return out


# --- tokens ---------------------------------------------------------------

def terms(text):
    """A port of `bm25::terms` (core/src/cli/embed_eval/bm25.rs)."""
    out = []
    for word in re.split(r"[^A-Za-z0-9]+", text):
        if not word:
            continue
        start = 0
        n = len(word)
        for i in range(1, n + 1):
            if i == n:
                boundary = True
            else:
                prev, cur = word[i - 1], word[i]
                next_lower = i + 1 < n and word[i + 1].islower()
                boundary = ((prev.islower() and cur.isupper())
                            or (prev.isupper() and cur.isupper() and next_lower)
                            or (prev.isalpha() != cur.isalpha()))
            if boundary:
                piece = word[start:i].lower()
                if len(piece) >= 2:
                    out.append(piece)
                start = i
    return out


def shingles(lines):
    toks = terms("\n".join(lines))
    if len(toks) < MIN_TOKENS:
        return None
    return {" ".join(toks[i:i + SHINGLE]) for i in range(len(toks) - SHINGLE + 1)}


def fewshot_texts(prompt_text):
    m = re.search(r"```fewshot\n(.*?)```", prompt_text, re.S)
    return [l.strip() for l in m.group(1).splitlines() if l.strip()] if m else []


# --- encoder ----------------------------------------------------------------

class Encoder:
    """jina-v2-base-code int8: tokenizer truncated to MAX_TOKENS, no padding,
    one text per run, mean pooling over the mask, L2 normalisation (as
    `EmbeddingModel::embed`). Vectors are cached by text sha256 in chunk files."""

    def __init__(self, model_dir, cache_dir, threads=2):
        import numpy as np
        import onnxruntime as ort
        from tokenizers import Tokenizer
        self.np = np
        self.tok = Tokenizer.from_file(str(Path(model_dir) / "tokenizer.json"))
        self.tok.enable_truncation(MAX_TOKENS)
        self.tok.no_padding()
        so = ort.SessionOptions()
        so.intra_op_num_threads = threads
        so.inter_op_num_threads = 1
        self.sess = ort.InferenceSession(str(Path(model_dir) / "model.onnx"), so, providers=["CPUExecutionProvider"])
        self.inputs = [i.name for i in self.sess.get_inputs()]
        tag = C.sha256_file(Path(model_dir) / "model.onnx")[:12]
        self.cache_dir = Path(cache_dir) / tag
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.cache = {}
        for f in sorted(self.cache_dir.glob("chunk-*.npz")):
            z = np.load(f)
            for k, v in zip(z["keys"], z["vecs"]):
                self.cache[str(k)] = v

    def embed_one(self, text):
        np = self.np
        enc = self.tok.encode(text)
        ids = np.array([enc.ids], dtype=np.int64)
        mask = np.array([enc.attention_mask], dtype=np.int64)
        feed = {"input_ids": ids, "attention_mask": mask}
        if "token_type_ids" in self.inputs:
            feed["token_type_ids"] = np.zeros_like(ids)
        hidden = self.sess.run(None, feed)[0][0]
        m = mask[0].astype(np.float32)[:, None]
        v = (hidden * m).sum(0) / m.sum()
        n = np.linalg.norm(v)
        return (v / n if n > 0 else v).astype(np.float32)

    def embed(self, texts, chunk=256):
        np = self.np
        shas = [C.sha256_bytes(t.encode()) for t in texts]
        todo = [(s, t) for s, t in dict(zip(shas, texts)).items() if s not in self.cache]
        for i in range(0, len(todo), chunk):
            part = todo[i:i + chunk]
            vecs = [self.embed_one(t) for _, t in part]
            n = len(list(self.cache_dir.glob("chunk-*.npz")))
            np.savez(self.cache_dir / f"chunk-{n:05d}.npz", keys=np.array([s for s, _ in part]), vecs=np.stack(vecs))
            for (s, _), v in zip(part, vecs):
                self.cache[s] = v
        return np.stack([self.cache[s] for s in shas]) if shas else np.zeros((0, 768), np.float32)


# --- checks -----------------------------------------------------------------

def normalise_url(url):
    u = url.strip().lower()
    u = re.sub(r"^git@([^:]+):", r"\1/", u)
    u = re.sub(r"^[a-z+]+://", "", u)
    u = re.sub(r"^[^@/]+@", "", u)
    u = re.sub(r"\.git$", "", u.rstrip("/"))
    return u


def repo_identity(train_repos, eval_work, eval_corpora_toml):
    """[(repo, reason)] for training repos that are an eval corpus or a fork."""
    work = Path(eval_work)
    ev_urls, ev_roots = {}, {}
    for row in C._load_toml(eval_corpora_toml)["corpus"]:
        c = row["id"]
        ev_urls[normalise_url(row["source"])] = c
        checkout = work / "corpora" / c
        if (checkout / ".git").exists():
            for remote in git(checkout, "remote").split():
                ev_urls[normalise_url(git(checkout, "remote", "get-url", remote))] = c
            for root in git(checkout, "rev-list", "--max-parents=0", "HEAD").split():
                ev_roots[root] = c
    hits = []
    for repo, meta in train_repos.items():
        for key in ("url", "source"):
            if meta.get(key) and normalise_url(meta[key]) in ev_urls:
                hits.append((repo, f"{key} {meta[key]} is eval corpus {ev_urls[normalise_url(meta[key])]}"))
        checkout = C.work_dir() / "corpora" / repo
        if (checkout / ".git").exists():
            for root in git(checkout, "rev-list", "--max-parents=0", "HEAD").split():
                if root in ev_roots:
                    hits.append((repo, f"root commit {root} is eval corpus {ev_roots[root]}'s"))
    return hits


def code_hits(dataset_keys, nodes, ev_spans):
    """[(key, check, eval label, score)] for exact and near-duplicate code."""
    exact = {C.normalised_span_sha256(lines): label for label, lines in ev_spans}
    ev_sh, index = [], defaultdict(list)
    for label, lines in ev_spans:
        sh = shingles(lines)
        if sh:
            j = len(ev_sh)
            ev_sh.append((label, sh))
            for s in sh:
                index[s].append(j)
    hits = []
    for k in sorted(dataset_keys):
        lines = nodes[k]["srcLines"]
        sha = C.normalised_span_sha256(lines)
        if sha in exact:
            hits.append((k, "exact-code", exact[sha], 1.0))
            continue
        sh = shingles(lines)
        if not sh:
            continue
        shared = defaultdict(int)
        for s in sh:
            for j in index.get(s, ()):
                shared[j] += 1
        # max of Jaccard and containment |A & B| / |B_eval| (an eval span
        # copied inside a larger training node)
        best = max(((max(n / (len(sh) + len(ev_sh[j][1]) - n), n / len(ev_sh[j][1])), j)
                    for j, n in shared.items()), default=(0.0, None))
        if best[0] >= CODE_JACCARD:
            hits.append((k, "code-neardup", ev_sh[best[1]][0], round(best[0], 4)))
    return hits


def nearest_jaccard(texts, ev_texts):
    """For each text, (best word Jaccard, eval id)."""
    ev_words = [(i, C.words(t)) for i, t in ev_texts]
    index = defaultdict(list)
    for j, (_, w) in enumerate(ev_words):
        for x in w:
            index[x].append(j)
    out = []
    for t in texts:
        a = C.words(t)
        shared = defaultdict(int)
        for x in a:
            for j in index.get(x, ()):
                shared[j] += 1
        best = max(((n / (len(a) + len(ev_words[j][1]) - n), j) for j, n in shared.items()), default=(0.0, None))
        out.append((best[0], ev_words[best[1]][0] if best[1] is not None else None))
    return out


def nearest_cosine(encoder, texts, ev_texts):
    if not texts:
        return []
    q = encoder.embed(texts)
    e = encoder.embed([t for _, t in ev_texts])
    sims = q @ e.T
    best = sims.argmax(1)
    return [(float(sims[i, b]), ev_texts[b][0]) for i, b in enumerate(best)]


def run_guard(rows, nodes, train_repos, eval_root, eval_work, prompt_path, cache_dir, threads=2,
              pins=None, eval_corpora_toml=None):
    """All five checks. Returns (hits, report): hits are dicts with `check`,
    `rows` (query ids), and the evidence."""
    errors = verify_pins(eval_root, pins)
    if errors:
        sys.exit("STOP: eval files differ from their pins:\n  " + "\n  ".join(errors))
    ev_texts = eval_queries(eval_root)
    hits = []
    by_repo = defaultdict(list)
    for r in rows:
        by_repo[r["repo"]].append(r["id"])

    for repo, reason in repo_identity({r: train_repos[r] for r in by_repo}, eval_work,
                                      eval_corpora_toml or Path(eval_root) / "corpora.toml"):
        hits.append({"check": "repo-identity", "repo": repo, "reason": reason, "rows": by_repo[repo]})

    used = defaultdict(lambda: {"pos": [], "neg": []})
    for r in rows:
        for k in r["positives"]:
            used[k]["pos"].append(r["id"])
        for c in r["negatives"]:
            used[c["key"]]["neg"].append(r["id"])
    ev_spans = eval_spans(eval_work, eval_keys(eval_root))
    for key, check, label, score in code_hits(set(used), nodes, ev_spans):
        hits.append({"check": check, "key": key, "eval": label, "score": score,
                     "rows": used[key]["pos"], "negativeRows": used[key]["neg"]})

    texts = [r["text"] for r in rows]
    jac = nearest_jaccard(texts, ev_texts)
    for r, (j, eid) in zip(rows, jac):
        if j >= QUERY_JACCARD:
            hits.append({"check": "query-jaccard", "rows": [r["id"]], "eval": eid, "score": round(j, 4)})
    fewshot = fewshot_texts(Path(prompt_path).read_text())
    if not fewshot:
        sys.exit(f"STOP: {prompt_path} has no ```fewshot block (or it is empty)")
    for t, (j, eid) in zip(fewshot, nearest_jaccard(fewshot, ev_texts)):
        if j >= QUERY_JACCARD:
            hits.append({"check": "query-jaccard", "rows": [], "prompt": t, "eval": eid, "score": round(j, 4)})

    encoder = Encoder(Path(eval_work) / MODEL_DIR, cache_dir, threads)
    cos = nearest_cosine(encoder, texts, ev_texts)
    for r, (c, eid) in zip(rows, cos):
        if c >= COSINE:
            hits.append({"check": "query-embed", "rows": [r["id"]], "eval": eid, "score": round(c, 4)})

    counts = defaultdict(int)
    for h in hits:
        counts[h["check"]] += 1
    flagged = {i for h in hits for i in h["rows"]}
    report = {
        "result": "clean" if not hits else "hits",
        "rows": len(rows),
        "hitsPerCheck": {c: counts.get(c, 0) for c in
                         ("repo-identity", "exact-code", "code-neardup", "query-jaccard", "query-embed")},
        "rowsFlagged": len(flagged),
        "rowsFlaggedShare": round(len(flagged) / len(rows), 4) if rows else 0.0,
        "evalQueries": len(ev_texts),
        "evalKeySpans": len(ev_spans),
        "fewshotTexts": len(fewshot),
        "thresholds": {"codeJaccard": CODE_JACCARD, "codeMeasure": "max(jaccard, containment)",
                       "queryJaccard": QUERY_JACCARD, "cosine": COSINE,
                       "shingle": SHINGLE, "minTokens": MIN_TOKENS},
        "nearestEvalCosine": {"quantiles": C.quantiles([c for c, _ in cos]),
                              "histogram": C.histogram([c for c, _ in cos],
                                                       [-1.0, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0])},
        "nearestEvalJaccard": {"quantiles": C.quantiles([j for j, _ in jac]),
                               "histogram": C.histogram([j for j, _ in jac], [0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 1.0])},
        "pinsVerified": len(pins if pins is not None else PINS),
    }
    return hits, report


def filter_rows(rows, hits):
    """The dataset minus the hits."""
    drop = {i for h in hits for i in h["rows"]}
    neg_drop = defaultdict(set)
    for h in hits:
        for i in h.get("negativeRows", ()):
            neg_drop[i].add(h["key"])
    out = []
    for r in rows:
        if r["id"] in drop:
            continue
        if r["id"] in neg_drop:
            r = dict(r, negatives=[c for c in r["negatives"] if c["key"] not in neg_drop[r["id"]]])
        out.append(r)
    return out


def pooling_check(run_dir, eval_dir, eval_work, cache_dir, count, threads):
    import numpy as np
    run = Path(run_dir)
    m = json.load(open(run / "manifest.json"))
    texts = {}
    for rel, _ in m["queryFiles"]:
        for q in C.read_jsonl(Path(eval_dir) / rel):
            texts[q["id"]] = q["text"]
    ids = m["queryIds"][:count]
    rust = np.fromfile(run / "query_vectors.bin", dtype="<f4").reshape(len(m["queryIds"]), m["dimension"])[:count]
    enc = Encoder(Path(eval_work) / MODEL_DIR, cache_dir, threads)
    py = enc.embed([texts[i] for i in ids])
    cos = (py * rust).sum(1) / (np.linalg.norm(py, axis=1) * np.linalg.norm(rust, axis=1))
    return {"run": str(run), "queries": len(ids), "minCosine": float(cos.min()), "meanCosine": float(cos.mean()),
            "perQuery": {i: round(float(c), 6) for i, c in zip(ids, cos)}}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version")
    ap.add_argument("--dataset", help="query rows (default <version>/queries.jsonl)")
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--filter", action="store_true")
    mode.add_argument("--pooling-check", metavar="RUN_DIR")
    ap.add_argument("--corpora", default=str(C.HERE / "corpora.toml"))
    ap.add_argument("--eval-root", default=str(C.EVAL))
    ap.add_argument("--eval-work", default=None)
    ap.add_argument("--prompt", default=str(C.HERE / "prompts" / "generate.md"))
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--count", type=int, default=20)
    args = ap.parse_args()
    eval_work = Path(args.eval_work) if args.eval_work else default_eval_work()
    cache = C.work_dir() / "qvec-cache"

    if args.pooling_check:
        res = pooling_check(args.pooling_check, args.eval_root, eval_work, cache, args.count, args.threads)
        print(json.dumps(res, indent=2))
        sys.exit(0 if res["minCosine"] >= POOLING_BAR else 1)

    if not args.version and not args.dataset:
        ap.error("--version or --dataset")
    vdir = C.version_dir(args.version) if args.version else Path(args.dataset).parent
    dataset = Path(args.dataset) if args.dataset else Path(vdir) / "queries.jsonl"
    rows = C.read_jsonl(dataset)
    nodes = {}
    for repo in sorted({r["repo"] for r in rows}):
        for n in C.load_nodes(vdir, repo):
            nodes[n["key"]] = n
    hits, report = run_guard(rows, nodes, C.corpora(args.corpora), args.eval_root, eval_work, args.prompt,
                             cache, args.threads)
    report["dataset"] = str(dataset)
    report["datasetSha256"] = C.sha256_file(dataset)
    # --filter records under filter.* so that export, which deletes and
    # rewrites guard.json and leaks.jsonl, keeps what the filter dropped.
    prefix = "filter." if args.filter else ""
    if not args.filter:
        C.write_json(dataset.parent / "guard.json", report)
    C.write_jsonl(dataset.parent / f"{prefix}leaks.jsonl", hits)
    print("leak_guard:", json.dumps({k: report[k] for k in ("result", "rows", "hitsPerCheck", "rowsFlaggedShare")}))
    print("  nearest-eval cosine:", json.dumps(report["nearestEvalCosine"]))
    print("  nearest-eval Jaccard:", json.dumps(report["nearestEvalJaccard"]))
    for h in hits[:50]:
        print("  HIT", json.dumps(h)[:400])
    if args.filter:
        kept = filter_rows(rows, hits)
        out = dataset.with_name(dataset.stem + ".filtered.jsonl")
        C.write_jsonl(out, kept)
        # Negatives removed from kept rows (a negative-code hit); a dropped
        # row's negatives go with it and are not counted here.
        kept_ids = {r["id"] for r in kept}
        negs_before = sum(len(r["negatives"]) for r in rows if r["id"] in kept_ids)
        negs_after = sum(len(r["negatives"]) for r in kept)
        report["filter"] = {"output": str(out), "outputSha256": C.sha256_file(out), "rowsIn": len(rows),
                            "rowsKept": len(kept), "rowsDropped": len(rows) - len(kept),
                            "negativesDropped": negs_before - negs_after, "hits": len(hits)}
        C.write_json(dataset.parent / "filter.guard.json", report)
        print(f"filter: kept {len(kept)} of {len(rows)} rows, dropped {len(rows) - len(kept)} rows and "
              f"{negs_before - negs_after} negatives of kept rows, {len(hits)} hits -> {out}")
        return
    sys.exit(1 if hits else 0)


if __name__ == "__main__":
    main()
