#!/usr/bin/env python3
"""Renders node texts to `<version>/texts/<renderId>/<repo>.jsonl`
(docs/design/GM-456-training-data.md, section 5 "Render forms").

Forms:
  embed-structured  the embedder's text, Rust `text_to_embed`
                    (`gm462_ce_text.base_text`: `gm423_token_lengths`'
                    structured doc, then the signature); parity fixture
                    core/src/embedding/testdata/structured_text.json
  ce-epb48          the cross-encoder's text, E P B48
                    (`gm698_ce_text_fixture.build` with the MiniLM tokenizer)
                    over the node's rebased `srcLines`; parity fixture
                    core/src/embedding/testdata/ce_text.json

A `renderId` is `<form>@<sha256[:12]>` of the form's renderer files (this
file, the reused builders and, for ce-epb48, the tokenizer). `texts_for`
refuses a texts folder whose recorded id differs from the current one, so a
consumer never mixes forms.

usage: render.py --version v0.0-stub [--form embed-structured --form ce-epb48]
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

import common as C

FORMS = ("embed-structured", "ce-epb48")
TOKENIZER = Path("models/ms-marco-MiniLM-L6-v2/tokenizer.json")


def renderer_files(form, work=None):
    files = [Path(__file__).resolve(), C.EVAL / "gm423_token_lengths.py", C.EVAL / "gm462_ce_text.py"]
    if form == "ce-epb48":
        files += [C.EVAL / "gm698_ce_text_fixture.py", Path(work or C.work_dir()) / TOKENIZER]
    elif form != "embed-structured":
        raise ValueError(f"unknown form {form}")
    return files


def render_id(form, work=None):
    h = hashlib.sha256()
    for f in renderer_files(form, work):
        h.update(f.name.encode() + b"\0" + C.sha256_file(f).encode() + b"\0")
    return f"{form}@{h.hexdigest()[:12]}"


def embed_text(node):
    import gm462_ce_text as T
    return T.base_text(node)


def ce_row(node):
    """The builder's row: the node rebased to its span's first line."""
    return {
        "kind": node["kind"], "nativeKind": node["nativeKind"], "name": node["name"],
        "qualifiedName": node["qualifiedName"], "filePath": node["filePath"],
        "startLine": 0, "endLine": node["endLine"] - node["startLine"],
        "language": node["language"], "signature": node["signature"], "docComment": node["docComment"],
    }


def ce_text(node, tok):
    import gm698_ce_text_fixture as F
    return F.build(ce_row(node), node["srcLines"], tok)


def minilm(work=None):
    import gm698_ce_text_fixture as F
    return F.minilm_counter(Path(work or C.work_dir()))


def texts_dir(vdir, form, work=None):
    return Path(vdir) / "texts" / render_id(form, work)


def texts_for(vdir, form, repo, work=None):
    """{key: text} of a rendered form; refuses a folder rendered by other code."""
    want = render_id(form, work)
    folders = sorted((Path(vdir) / "texts").glob(f"{form}@*"))
    if not folders:
        sys.exit(f"STOP: no {form} texts under {vdir}/texts; run render.py")
    for d in folders:
        rid = json.load(open(d / "render.json"))["renderId"]
        if rid != d.name:
            sys.exit(f"STOP: {d} records renderId {rid}")
        if rid != want:
            sys.exit(f"STOP: {d.name} was rendered by other code than this checkout's ({want}); re-render")
    return {r["key"]: r["text"] for r in C.read_jsonl(folders[0] / f"{repo}.jsonl")}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--form", action="append", choices=FORMS)
    args = ap.parse_args()
    vdir = C.version_dir(args.version)
    forms = args.form or list(FORMS)
    tok = minilm() if "ce-epb48" in forms else None
    for form in forms:
        rid = render_id(form)
        out = Path(vdir) / "texts" / rid
        for stale in (Path(vdir) / "texts").glob(f"{form}@*"):
            if stale != out:
                sys.exit(f"STOP: {stale} holds another render of {form}; remove it to re-render")
        counts = {}
        for repo in C.node_repos(vdir):
            nodes = C.load_nodes(vdir, repo)
            if form == "embed-structured":
                rows = [{"key": n["key"], "text": embed_text(n)} for n in nodes]
            else:
                rows = [{"key": n["key"], "text": ce_text(n, tok)} for n in nodes]
            C.write_jsonl(out / f"{repo}.jsonl", rows)
            counts[repo] = len(rows)
        C.write_json(out / "render.json", {
            "renderId": rid,
            "files": {f.name: C.sha256_file(f) for f in renderer_files(form)},
            "counts": counts,
        })
        print(f"render {rid}: {counts}")


if __name__ == "__main__":
    main()
