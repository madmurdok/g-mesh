#!/usr/bin/env python3
"""Register exported GM-730 models as eval arms (docs/design/GM-730-gpu-training-routes.md,
sections 3.1, 3.2 and M4 of 9.1).

Embedding arms (`embed`): the export's model.onnx and tokenizer.json are copied
into $GM730_DATA/models/<arm>@<sha16>/ (sha16 = the first 16 hex digits of
model.onnx's sha256) and a row is appended to eval/embedding/v2/variants.toml:
`arm = "model"`, `model_dir` = that directory, the encoder spec copied from the
base row, and a `# gm730 pins:` comment line above it holding both files'
sha256. A model row has no sha256 field and `debug-embed-eval run` checks no
pin, so:
  - `verify` re-hashes each arm's files against its pins (and the dir name)
    before `run`;
  - `check-run` compares every run manifest's `modelFiles` with the pins after it; a
    words-shuffled arm (no `modelFiles` of its own) through its reference's pins and runs.
Rows are appended, never edited; no row gets `role = "control"`.

Cross-encoder arms (`ce`): the same copy, and an entry in
eval/embedding/ce_local.json ({name: {dir, files: {file: sha256}, chunk1}})
that rerank_eval.CrossEncoder and gm463_bakeoff.py load without Hugging Face;
`chunk1` marks a dynamically quantised model (one pair per ORT run).

usage: register_arms.py embed --name gm730-... --base jina|gte --src DIR --role quality|cost|informational [--words-shuffled]
       register_arms.py ce --name ce-gm730-... --src DIR [--chunk1]
       register_arms.py verify [--names a,b]
       register_arms.py check-run --runs <v2 work>/runs [--names a,b]
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
VARIANTS = REPO / "eval" / "embedding" / "v2" / "variants.toml"
CE_LOCAL = REPO / "eval" / "embedding" / "ce_local.json"
BASE_ROW = {"jina": "jina-v2-base-code-int8-structured", "gte": "gte-small-structured"}
ROLES = ("quality", "cost", "informational")
SPEC_SKIP = {"name", "arm", "role", "hf_repo", "revision", "onnx_file", "model_dir", "reference"}
EMBED_FILES = ("model.onnx", "tokenizer.json")
CE_FILES = ("model.onnx", "tokenizer.json", "config.json")
PIN_RE = re.compile(r"^# gm730 pins: (.*)$")


def data_root():
    return Path(os.environ.get("GM730_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm730"))


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def home_path(p):
    p = Path(p).resolve()
    try:
        return "~/" + str(p.relative_to(Path.home()))
    except ValueError:
        return str(p)


def expand(p, base=VARIANTS.parent):
    p = os.path.expanduser(p)
    return Path(p) if os.path.isabs(p) else base / p


def copy_model(name, src, files):
    """Copy `files` (+ model.onnx_data when present) from src into <models>/<name>@<sha16>/.
    Returns (dir, {file: sha256}); refuses a dir that holds other bytes."""
    src = Path(src)
    pins = {f: sha256_file(src / f) for f in files}
    if (src / "model.onnx_data").exists():
        pins["model.onnx_data"] = sha256_file(src / "model.onnx_data")
    dest = data_root() / "models" / f"{name}@{pins['model.onnx'][:16]}"
    dest.mkdir(parents=True, exist_ok=True)
    for f, sha in pins.items():
        if not (dest / f).exists():
            shutil.copyfile(src / f, dest / f)
        if sha256_file(dest / f) != sha:
            raise SystemExit(f"{dest / f}: sha256 differs from {src / f}")
    return dest, pins


def rows(text):
    return tomllib.loads(text).get("variant", [])


def row_of(text, name):
    for r in rows(text):
        if r["name"] == name:
            return r
    raise SystemExit(f"{VARIANTS}: no variant {name}")


def toml_value(v):
    return json.dumps(v) if not isinstance(v, bool) else ("true" if v else "false")


def pins_line(pins):
    return "# gm730 pins: " + " ".join(f"{f}={s}" for f, s in sorted(pins.items()))


def embed_row(name, role, model_dir, base_row, pins):
    spec = {k: v for k, v in base_row.items() if k not in SPEC_SKIP}
    lines = [pins_line(pins), "[[variant]]", f"name = {json.dumps(name)}", 'arm = "model"', f"role = {json.dumps(role)}",
             f"model_dir = {json.dumps(home_path(model_dir))}"] + [f"{k} = {toml_value(v)}" for k, v in spec.items()]
    return "\n".join(lines) + "\n"


def words_shuffled_row(name):
    return (f'[[variant]]\nname = {json.dumps(name + "-words-shuffled")}\narm = "words-shuffled"\n'
            f'role = "informational"\nreference = {json.dumps(name)}\ntext = "structured"\n')


def append(variants, text, new_rows):
    """Append rows; refuses a name that already exists and checks the file still parses."""
    have = {r["name"] for r in rows(text)}
    for r in rows("\n".join(new_rows)):
        if r["name"] in have:
            raise SystemExit(f"{variants} already has a variant {r['name']}; rows are appended, never edited")
        if r.get("role") == "control":
            raise SystemExit(f"{r['name']}: GM-730 arms are never role = control")
    out = text.rstrip("\n") + "\n" + "".join("\n" + r for r in new_rows)
    rows(out)
    tmp = Path(str(variants) + ".tmp")
    tmp.write_text(out)
    tmp.replace(variants)


def register_embed(name, base, src, role, words_shuffled=False, variants=VARIANTS):
    if not name.startswith("gm730-"):
        raise SystemExit(f"{name}: GM-730 arm names start with gm730-")
    if role not in ROLES:
        raise SystemExit(f"role {role}: one of {ROLES}")
    text = Path(variants).read_text()
    base_row = row_of(text, BASE_ROW[base])
    base_tok = expand(base_row["model_dir"], Path(variants).parent) / "tokenizer.json"
    if base_tok.exists() and sha256_file(base_tok) != sha256_file(Path(src) / "tokenizer.json"):
        raise SystemExit(f"{src}/tokenizer.json differs from the base row's {base_tok}")
    dest, pins = copy_model(name, src, EMBED_FILES)
    new = [embed_row(name, role, dest, base_row, pins)] + ([words_shuffled_row(name)] if words_shuffled else [])
    append(variants, text, new)
    return dest, pins


def register_ce(name, src, chunk1=False, registry=CE_LOCAL):
    if not name.startswith("ce-gm730-"):
        raise SystemExit(f"{name}: GM-730 cross-encoder names start with ce-gm730-")
    reg = json.loads(Path(registry).read_text()) if Path(registry).exists() else {}
    if name in reg:
        raise SystemExit(f"{registry} already has {name}; entries are added, never edited")
    dest, pins = copy_model(name, src, CE_FILES)
    reg[name] = {"dir": home_path(dest), "files": pins, "chunk1": bool(chunk1)}
    tmp = Path(str(registry) + ".tmp")
    tmp.write_text(json.dumps(reg, indent=1, sort_keys=True) + "\n")
    tmp.replace(registry)
    return dest, pins


def pinned(variants=VARIANTS):
    """{arm name: (model_dir, {file: sha256})} of every row with a `# gm730 pins:` line."""
    out, pending = {}, None
    for line in Path(variants).read_text().splitlines():
        m = PIN_RE.match(line)
        if m:
            pending = dict(kv.split("=", 1) for kv in m.group(1).split())
            continue
        nm = re.match(r'^name = "(.*)"$', line)
        if nm and pending is not None:
            out[nm.group(1)] = pending
            pending = None
    text = Path(variants).read_text()
    return {n: (expand(row_of(text, n)["model_dir"], Path(variants).parent), p) for n, p in out.items()}


def verify(names=None, variants=VARIANTS):
    """Problems (empty = ok): each pinned arm's files vs its pins and its dir's @sha16."""
    bad = []
    arms = pinned(variants)
    for n in names or sorted(arms):
        if n not in arms:
            bad.append(f"{n}: no gm730 pins in {variants}")
            continue
        d, pins = arms[n]
        if not d.name.endswith("@" + pins["model.onnx"][:16]):
            bad.append(f"{n}: dir {d.name} does not carry model.onnx's sha16 {pins['model.onnx'][:16]}")
        for f, sha in pins.items():
            if not (d / f).exists():
                bad.append(f"{n}: {d / f} missing")
            elif sha256_file(d / f) != sha:
                bad.append(f"{n}: {d / f} sha256 differs from its pin")
    return bad


def words_shuffled_of(variants=VARIANTS):
    """{arm name: its reference} of every words-shuffled row whose reference is a pinned arm."""
    arms = pinned(variants)
    return {r["name"]: r["reference"] for r in rows(Path(variants).read_text())
            if r.get("arm") == "words-shuffled" and r.get("reference") in arms}


def check_run(runs_dir, names=None, variants=VARIANTS):
    """Problems (empty = ok): every <runs>/<arm>/<corpus>/manifest.json `modelFiles` vs the pins.
    A words-shuffled arm encodes with its reference's model_dir and records no modelFiles, so
    it is checked through its reference: the reference's files match its pins now, every
    corpus the arm ran has a reference run whose modelFiles are the pins, and any modelFiles
    the arm itself records are the pins too."""
    bad = []
    arms = pinned(variants)
    ws = words_shuffled_of(variants)

    def files_vs_pins(mp, pins, allow_empty=False):
        got = {f[0]: f[1] for f in json.loads(mp.read_text()).get("modelFiles", [])}
        if allow_empty and not got:
            return
        for f in EMBED_FILES:
            if got.get(f) != pins.get(f):
                bad.append(f"{mp}: modelFiles {f} {got.get(f)} != pin {pins.get(f)}")

    for n in names or sorted(arms) + sorted(ws):
        ref = ws.get(n)
        if ref is None and n not in arms:
            bad.append(f"{n}: no gm730 pins in {variants}")
            continue
        pins = arms[ref or n][1]
        mans = sorted((Path(runs_dir) / n).glob("*/manifest.json"))
        if not mans:
            bad.append(f"{n}: no run manifests under {runs_dir}/{n}")
        if ref is not None:
            bad += verify([ref], variants)
        for mp in mans:
            files_vs_pins(mp, pins, allow_empty=ref is not None)
            if ref is not None:
                rp = Path(runs_dir) / ref / mp.parent.name / "manifest.json"
                if not rp.exists():
                    bad.append(f"{mp}: no reference run {rp}")
                else:
                    files_vs_pins(rp, pins)
    return bad


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("embed")
    e.add_argument("--name", required=True)
    e.add_argument("--base", required=True, choices=sorted(BASE_ROW))
    e.add_argument("--src", required=True, type=Path)
    e.add_argument("--role", required=True, choices=ROLES)
    e.add_argument("--words-shuffled", action="store_true")
    c = sub.add_parser("ce")
    c.add_argument("--name", required=True)
    c.add_argument("--src", required=True, type=Path)
    c.add_argument("--chunk1", action="store_true", help="dynamic int8: one pair per ORT run")
    v = sub.add_parser("verify")
    r = sub.add_parser("check-run")
    r.add_argument("--runs", required=True, type=Path)
    for p in (e, c, v, r):
        p.add_argument("--variants", type=Path, default=VARIANTS)
    c.add_argument("--registry", type=Path, default=CE_LOCAL)
    for p in (v, r):
        p.add_argument("--names", type=lambda s: [x for x in s.split(",") if x])
    a = ap.parse_args(argv)
    if a.cmd == "embed":
        d, pins = register_embed(a.name, a.base, a.src, a.role, a.words_shuffled, a.variants)
        print(f"{a.name}: {d} {pins}")
        return 0
    if a.cmd == "ce":
        d, pins = register_ce(a.name, a.src, a.chunk1, a.registry)
        print(f"{a.name}: {d} {pins}")
        return 0
    bad = verify(a.names, a.variants) if a.cmd == "verify" else check_run(a.runs, a.names, a.variants)
    print("\n".join(bad) if bad else f"{a.cmd}: ok")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
