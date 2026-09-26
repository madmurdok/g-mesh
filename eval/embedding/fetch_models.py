#!/usr/bin/env python3
"""Downloads each model variant of variants.toml at its pinned revision into
work/models/<name>/ (model.onnx + tokenizer.json), the layout
EmbeddingModel::load_with_spec reads. Variants with an explicit model_dir
(the reference, which production already fetched) are skipped.

The g-mesh crate itself never downloads anything outside `g-mesh model
fetch`, so this lives here as a script.

Usage: fetch_models.py [variant ...]   (all model variants when omitted)
"""

import hashlib
import os
import re
import sys
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))


def variants():
    """Flat `[[variant]]` tables of variants.toml; enough TOML for that file."""
    rows, current = [], None
    for line in open(os.path.join(HERE, "variants.toml")):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[variant]]":
            current = {}
            rows.append(current)
        elif line.startswith("["):
            current = None
        elif current is not None and "=" in line:
            key, value = (s.strip() for s in line.split("=", 1))
            m = re.fullmatch(r'"(.*)"', value)
            current[key] = m.group(1) if m else value
    return rows


def fetch(url, dest):
    tmp = dest + ".part"
    with urllib.request.urlopen(url) as r, open(tmp, "wb") as f:
        while chunk := r.read(1 << 20):
            f.write(chunk)
    os.replace(tmp, dest)


def main():
    wanted = set(sys.argv[1:])
    for v in variants():
        if v.get("arm") != "model" or (wanted and v["name"] not in wanted):
            continue
        if "model_dir" in v:
            print(f"{v['name']}: uses {v['model_dir']}, not fetched")
            continue
        out = os.path.join(HERE, "work", "models", v["name"])
        os.makedirs(out, exist_ok=True)
        base = f"https://huggingface.co/{v['hf_repo']}/resolve/{v['revision']}"
        for remote, local in ((v["onnx_file"], "model.onnx"), ("tokenizer.json", "tokenizer.json")):
            dest = os.path.join(out, local)
            if not os.path.exists(dest):
                fetch(f"{base}/{remote}", dest)
            digest = hashlib.sha256(open(dest, "rb").read()).hexdigest()
            print(f"{v['name']}: {local} {os.path.getsize(dest)} bytes sha256 {digest}")


if __name__ == "__main__":
    main()
