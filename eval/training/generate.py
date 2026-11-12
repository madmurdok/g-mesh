#!/usr/bin/env python3
"""Query generation for the GM-456 training data
(docs/design/GM-456-training-data.md, section 2).

Modes (one per run):
  --dry-run   build the requests (`responses/gen-requests.jsonl`)
              and print their size; no API call.
  --stub      write clearly-labelled stub queries (`gen.model = "stub"`): the
              doc's first sentence as the sentence and its first 7 words as
              the phrase; without a doc, "placeholder stub ..." texts around
              the name's words (bare name words would match the eval's
              mechanical name queries). For pipeline tests only.
  --run [--jobs N] [--retry ID...]
              one headless `claude -p` per request (`claude_cli`, section 2b),
              raw CLI results stored as `responses/gen/<custom_id>.json` (the
              artefact of record), failures as `<custom_id>.err.json`; a pack
              with either for the same request is not rerun unless `--retry`
              names it; files for a changed request are moved to
              `responses/gen/stale/` and the pack reruns. Then every
              stored response is parsed; parse problems go to
              `responses/gen-skipped.jsonl`. Exits 3 on a usage limit.

Each request carries up to PER_REQUEST targets after the system prompt.
The model sees the span (capped at SPAN_CAP lines), the untrimmed doc, the
file path and the parent; never a trimmed form. A refusal
(`stop_reason == "refusal"`) is logged and its targets skipped.

Output: `<version>/queries.gen.jsonl`, one row per query, with `overlap`
(`check_queries.overlap`, D3 rule 6) and `docEcho` (word Jaccard with the doc's
first paragraph).

usage: generate.py --version v0.0-stub --stub
"""

import argparse
import json
import re
import sys
from pathlib import Path

import claude_cli
import common as C
from check_queries import overlap
from gm423_token_lengths import first_paragraph

MODEL = "claude-opus-5-5"
EFFORT = "low"
PER_REQUEST = 10
SPAN_CAP = 150
MAX_TOKENS = 16000
PROMPT = C.HERE / "prompts" / "generate.md"
SUFFIXES = ("p1", "p2", "s1", "s2", "s1p", "s2p")

SCHEMA = {
    "type": "object",
    "properties": {
        "targets": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "n": {"type": "integer"},
                    "phrases": {"type": "array", "items": {"type": "string"}},
                    "sentences": {"type": "array", "items": {"type": "string"}},
                    "paraphrases": {"type": "array", "items": {"type": "string"}},
                    "answersAlso": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["n", "phrases", "sentences", "paraphrases", "answersAlso"],
                "additionalProperties": False,
            },
        }
    },
    "required": ["targets"],
    "additionalProperties": False,
}


def target_block(t, node):
    span = node["srcLines"][:SPAN_CAP]
    cut = len(node["srcLines"]) - len(span)
    code = "\n".join(span) + (f"\n... ({cut} more lines)" if cut else "")
    doc = (node["docComment"] or "").strip() or "(none)"
    return (f"n: {t['n']}\npath: {node['filePath']}\nparent: {node['parent'] or '(none)'}\n"
            f"doc: {doc}\ncode:\n{code}")


def custom_id(prefix, repo, first_n):
    cid = f"{prefix}-{repo}-{first_n:04d}"
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", cid):
        sys.exit(f"STOP: custom_id {cid!r} is not valid")
    return cid


def build_requests(targets, nodes, prompt, model=MODEL, effort=EFFORT, per_request=PER_REQUEST):
    """Batch requests: targets grouped per repo in order, `per_request` each."""
    out, by_repo = [], {}
    for t in targets:
        by_repo.setdefault(t["repo"], []).append(t)
    for repo, ts in by_repo.items():
        for i in range(0, len(ts), per_request):
            group = ts[i:i + per_request]
            body = "\n\n---\n\n".join(target_block(t, nodes[t["key"]]) for t in group)
            out.append({
                "custom_id": custom_id("gen", repo, group[0]["n"]),
                "targets": [t["key"] for t in group],
                "params": {
                    "model": model,
                    "max_tokens": MAX_TOKENS,
                    "system": [{"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}],
                    "messages": [{"role": "user", "content": f"Symbols ({len(group)}):\n\n{body}"}],
                    "thinking": {"type": "adaptive"},
                    "output_config": {"effort": effort,
                                      "format": {"type": "json_schema", "schema": SCHEMA}},
                },
            })
    return out


def query_row(t, node, suffix, shape, text, gen, paraphrase_of=None, answers_also=None):
    doc_words = C.words(first_paragraph(node["docComment"]) if node["docComment"] else "")
    return {
        "id": f"{t['repo']}-{t['n']:04d}-{suffix}",
        "split": None,
        "repo": t["repo"],
        "commit": node["commit"],
        "language": node["language"],
        "shape": shape,
        "paraphraseOf": paraphrase_of,
        "text": text,
        "overlap": overlap(text, [{"qualifiedName": node["qualifiedName"]}]),
        "docEcho": round(C.jaccard(C.words(text), doc_words), 4),
        "target": t["key"],
        "positives": [t["key"]],
        "answersAlso": answers_also or [],
        "negatives": [],
        "gen": gen,
    }


def name_words(node):
    from gm462_ce_text import split_words
    return split_words(node["name"]) or node["name"]


def first_sentence(doc):
    if not doc:
        return None
    para = " ".join(first_paragraph(doc).split())
    m = re.match(r"(.+?[.!?])(\s|$)", para)
    s = (m.group(1) if m else para).strip()
    return s if len(s.split()) >= 3 else None


def stub_queries(t, node):
    gen = {"model": "stub", "effort": None, "promptSha256": None, "customId": None, "stub": True}
    words = name_words(node)
    sentence = first_sentence(node["docComment"])
    phrase = " ".join(sentence.split()[:7]) if sentence else f"placeholder stub query for {words}"
    sentence = sentence or f"placeholder stub sentence about the code of {words} for pipeline tests"
    return [
        query_row(t, node, "p1", "phrase", phrase, gen),
        query_row(t, node, "s1", "sentence", sentence, gen),
    ]


def parse_message(request, message, nodes, targets_by_key, prompt_sha):
    """Query rows from one stored response's message (`claude_cli.to_message`), or
    (None, reason) when it cannot be used."""
    if message.get("stop_reason") == "refusal":
        return None, f"refusal {message.get('stop_details')}"
    text = next((b["text"] for b in message["content"] if b.get("type") == "text"), None)
    if text is None:
        return None, f"no text block (stop_reason {message.get('stop_reason')})"
    data = json.loads(text)
    gen = {"model": message.get("model", MODEL), "effort": request["params"]["output_config"]["effort"],
           "promptSha256": prompt_sha, "customId": request["custom_id"], "usage": message.get("usage")}
    by_n = {targets_by_key[k]["n"]: targets_by_key[k] for k in request["targets"]}
    rows, problems = [], []
    for entry in data["targets"]:
        t = by_n.get(entry["n"])
        if t is None:
            problems.append(f"unknown n {entry['n']}")
            continue
        node = nodes[t["key"]]
        if len(entry["phrases"]) != 2 or len(entry["sentences"]) != 2 or len(entry["paraphrases"]) != 2:
            problems.append(f"n {entry['n']}: wrong counts")
            continue
        p, s, pp = entry["phrases"], entry["sentences"], entry["paraphrases"]
        base = f"{t['repo']}-{t['n']:04d}"
        also = entry["answersAlso"]
        rows += [
            query_row(t, node, "p1", "phrase", p[0], gen, answers_also=also),
            query_row(t, node, "p2", "phrase", p[1], gen, answers_also=also),
            query_row(t, node, "s1", "sentence", s[0], gen, answers_also=also),
            query_row(t, node, "s2", "sentence", s[1], gen, answers_also=also),
            query_row(t, node, "s1p", "sentence", pp[0], gen, paraphrase_of=f"{base}-s1", answers_also=also),
            query_row(t, node, "s2p", "sentence", pp[1], gen, paraphrase_of=f"{base}-s2", answers_also=also),
        ]
    missing = sorted(set(by_n) - {e["n"] for e in data["targets"]})
    if missing:
        problems.append(f"no entry for targets n {missing}")
    return rows, problems


def load_targets(vdir):
    targets = C.read_jsonl(Path(vdir) / "targets.jsonl")
    nodes = {}
    for repo in sorted({t["repo"] for t in targets}):
        for n in C.load_nodes(vdir, repo):
            nodes[n["key"]] = n
    return targets, nodes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--stub", action="store_true")
    mode.add_argument("--run", action="store_true")
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--effort", default=EFFORT)
    claude_cli.add_run_args(ap)
    args = ap.parse_args()
    vdir = C.version_dir(args.version)
    targets, nodes = load_targets(vdir)
    prompt = PROMPT.read_text()
    prompt_sha = C.sha256_file(PROMPT)
    resp = Path(vdir) / "responses"

    if args.stub:
        rows = [q for t in targets for q in stub_queries(t, nodes[t["key"]])]
        C.write_jsonl(Path(vdir) / "queries.gen.jsonl", rows)
        print(f"generate --stub: {len(rows)} stub queries for {len(targets)} targets")
        return

    requests = build_requests(targets, nodes, prompt, args.model, args.effort)
    C.write_jsonl(resp / "gen-requests.jsonl", requests)
    chars = sum(len(json.dumps(r["params"])) for r in requests)
    print(f"generate: {len(requests)} requests for {len(targets)} targets, {chars} request chars "
          f"(~{chars // 4} input tokens before caching), prompt sha256 {prompt_sha}")
    if args.dry_run:
        return

    claude_cli.run_step("gen", requests, resp, args, SCHEMA, prompt, prompt_sha, args.model, args.effort,
                        PER_REQUEST, args.version)
    targets_by_key = {t["key"]: t for t in targets}
    rows, skipped = [], []
    for req in requests:
        cid = req["custom_id"]
        cli, why = claude_cli.stored(req, resp / "gen")
        if cli is None:
            skipped.append({"customId": cid, "reason": why})
            continue
        got, problems = parse_message(req, claude_cli.to_message(cli), nodes, targets_by_key,
                                      prompt_sha)
        if got is None:
            skipped.append({"customId": cid, "reason": problems})
            continue
        rows += got
        skipped += [{"customId": cid, "reason": p} for p in problems]
    C.write_jsonl(Path(vdir) / "queries.gen.jsonl", rows)
    C.write_jsonl(resp / "gen-skipped.jsonl", skipped)
    print(f"generate --run: {len(rows)} queries, {len(skipped)} skipped")


if __name__ == "__main__":
    main()
