#!/usr/bin/env python3
"""Judging of mined candidates (docs/design/GM-456-training-data.md, section 3
"The judge").

One request per target: the target's code, its queries and its candidates'
code heads (HEAD_LINES lines). The judge returns a verdict per (candidate,
query) pair, applied by `apply_verdicts`:
  answers     the candidate moves to that query's `positives`;
  partial     dropped as ambiguous;
  non-answer  kept as a hard negative, with its verdict recorded.
A pair the judge left out stays unjudged; export refuses unjudged negatives.

Modes:
  --dry-run   build `judge/judge-requests.jsonl` and print its size. With
              --stub-verdict, every pair also gets the verdict `non-answer`
              from model "stub" (`judge.stub = true`), for pipeline tests only.
  --submit / --collect   as generate.py.

Output: `<version>/queries.judged.jsonl`.

usage: judge.py --version v0.0-stub --dry-run --stub-verdict
"""

import argparse
import json
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

import common as C
from generate import API_VERSION, client, custom_id

MODEL = "claude-sonnet-5-5"
EFFORT = "low"
HEAD_LINES = 40
MAX_TOKENS = 16000
PROMPT = C.HERE / "prompts" / "judge.md"
VERDICTS = ("answers", "partial", "non-answer")

SCHEMA = {
    "type": "object",
    "properties": {
        "verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "c": {"type": "integer"},
                    "q": {"type": "integer"},
                    "verdict": {"type": "string", "enum": list(VERDICTS)},
                },
                "required": ["c", "q", "verdict"],
                "additionalProperties": False,
            },
        }
    },
    "required": ["verdicts"],
    "additionalProperties": False,
}


def code_head(node, lines=HEAD_LINES):
    span = node["srcLines"][:lines]
    more = len(node["srcLines"]) - len(span)
    return "\n".join(span) + (f"\n... ({more} more lines)" if more else "")


def by_target(rows):
    out = defaultdict(list)
    for r in rows:
        out[r["target"]].append(r)
    return out


def build_requests(rows, nodes, prompt, model=MODEL, effort=EFFORT):
    out = []
    for tkey, qs in by_target(rows).items():
        cands = qs[0]["negatives"]
        if not cands:
            continue
        t = nodes[tkey]
        qtext = "\n".join(f"q{i}: {q['text']}" for i, q in enumerate(qs, 1))
        ctext = "\n\n".join(f"c{i}: {nodes[c['key']]['filePath']} {nodes[c['key']]['qualifiedName']}\n"
                            f"{code_head(nodes[c['key']])}" for i, c in enumerate(cands, 1))
        n = int(qs[0]["id"].split("-")[-2])
        out.append({
            "custom_id": custom_id("judge", qs[0]["repo"], n),
            "target": tkey,
            "queries": [q["id"] for q in qs],
            "candidates": [c["key"] for c in cands],
            "params": {
                "model": model,
                "max_tokens": MAX_TOKENS,
                "system": [{"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}],
                "messages": [{"role": "user", "content":
                              f"Target: {t['filePath']} {t['qualifiedName']}\n{code_head(t, 150)}\n\n"
                              f"Queries:\n{qtext}\n\nCandidates:\n\n{ctext}"}],
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": effort, "format": {"type": "json_schema", "schema": SCHEMA}},
            },
        })
    return out


def apply_verdicts(rows, verdicts):
    """`verdicts`: {(query id, candidate key): judge dict with `verdict`}.
    Returns new rows: `answers` candidates moved to positives, `partial`
    dropped, `non-answer` kept with the judge dict; unlisted pairs unjudged."""
    out = []
    for r in rows:
        positives, negatives = list(r["positives"]), []
        for c in r["negatives"]:
            j = verdicts.get((r["id"], c["key"]))
            if j is None:
                negatives.append(dict(c, judge=None))
            elif j["verdict"] == "answers":
                if c["key"] not in positives:
                    positives.append(c["key"])
            elif j["verdict"] == "partial":
                continue
            elif j["verdict"] == "non-answer":
                negatives.append(dict(c, judge=dict(j)))
            else:
                raise ValueError(f"{r['id']}: unknown verdict {j['verdict']!r}")
        out.append(dict(r, positives=positives, negatives=negatives))
    return out


def parse_message(request, message, prompt_sha):
    if message.get("stop_reason") == "refusal":
        return None, f"refusal {message.get('stop_details')}"
    text = next((b["text"] for b in message["content"] if b.get("type") == "text"), None)
    if text is None:
        return None, f"no text block (stop_reason {message.get('stop_reason')})"
    out, problems = {}, []
    for v in json.loads(text)["verdicts"]:
        if not (1 <= v["c"] <= len(request["candidates"]) and 1 <= v["q"] <= len(request["queries"])):
            problems.append(f"pair out of range {v}")
            continue
        out[(request["queries"][v["q"] - 1], request["candidates"][v["c"] - 1])] = {
            "verdict": v["verdict"], "model": message.get("model", MODEL), "promptSha256": prompt_sha,
            "customId": request["custom_id"]}
    return out, problems


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--submit", action="store_true")
    mode.add_argument("--collect", action="store_true")
    ap.add_argument("--stub-verdict", action="store_true")
    args = ap.parse_args()
    if args.stub_verdict and not args.dry_run:
        sys.exit("--stub-verdict goes with --dry-run")
    vdir = Path(C.version_dir(args.version))
    rows = C.read_jsonl(vdir / "queries.mined.jsonl")
    nodes = {}
    for repo in sorted({r["repo"] for r in rows}):
        for n in C.load_nodes(vdir, repo):
            nodes[n["key"]] = n
    prompt = PROMPT.read_text()
    prompt_sha = C.sha256_file(PROMPT)
    requests = build_requests(rows, nodes, prompt)
    C.write_jsonl(vdir / "judge" / "judge-requests.jsonl", requests)
    chars = sum(len(json.dumps(r["params"])) for r in requests)
    pairs = sum(len(r["queries"]) * len(r["candidates"]) for r in requests)
    print(f"judge: {len(requests)} requests, {pairs} (query, candidate) pairs, {chars} request chars "
          f"(~{chars // 4} input tokens before caching), prompt sha256 {prompt_sha}")

    if args.dry_run:
        if args.stub_verdict:
            stub = {"verdict": "non-answer", "model": "stub", "promptSha256": None, "customId": None, "stub": True}
            verdicts = {(q, c): stub for r in requests for q in r["queries"] for c in r["candidates"]}
            judged = apply_verdicts(rows, verdicts)
            C.write_jsonl(vdir / "queries.judged.jsonl", judged)
            print(f"judge --stub-verdict: {len(verdicts)} stub verdicts (non-answer)")
        return

    resp = vdir / "judge"
    if args.submit:
        batch = client().messages.batches.create(
            requests=[{"custom_id": r["custom_id"], "params": r["params"]} for r in requests])
        C.write_json(resp / "judge-batch.json", {"batchId": batch.id, "apiVersion": API_VERSION,
                                                 "submitted": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
                                                 "requests": len(requests), "promptSha256": prompt_sha})
        print(f"submitted batch {batch.id}")
        return

    batch_id = json.load(open(resp / "judge-batch.json"))["batchId"]
    c = client()
    status = c.messages.batches.retrieve(batch_id).processing_status
    if status != "ended":
        sys.exit(f"batch {batch_id} is {status}; collect again later")
    results = [r.to_dict() if hasattr(r, "to_dict") else r.model_dump() for r in c.messages.batches.results(batch_id)]
    C.write_jsonl(resp / "judge-results.jsonl", results)
    by_id = {r["custom_id"]: r for r in requests}
    verdicts, skipped = {}, []
    for res in results:
        if res["result"]["type"] != "succeeded":
            skipped.append({"customId": res["custom_id"], "reason": res["result"]["type"]})
            continue
        got, problems = parse_message(by_id[res["custom_id"]], res["result"]["message"], prompt_sha)
        if got is None:
            skipped.append({"customId": res["custom_id"], "reason": problems})
            continue
        verdicts.update(got)
        skipped += [{"customId": res["custom_id"], "reason": p} for p in problems]
    judged = apply_verdicts(rows, verdicts)
    C.write_jsonl(vdir / "queries.judged.jsonl", judged)
    C.write_jsonl(resp / "judge-skipped.jsonl", skipped)
    print(f"collect: {len(verdicts)} verdicts {dict(Counter(v['verdict'] for v in verdicts.values()))}, "
          f"{len(skipped)} skipped")


if __name__ == "__main__":
    main()
