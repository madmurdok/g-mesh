#!/usr/bin/env python3
"""Judging of mined candidates (docs/design/GM-456-training-data.md, section 3
"The judge").

One request per PER_REQUEST targets of one repo: per target (`t`), its code,
its queries and its candidates' code heads (HEAD_LINES lines). The judge
returns, per (target `t`, candidate `c`), one verdict per query in query
order; `parse_message` records every left-out or extra entry as a problem.
Verdicts are applied by `apply_verdicts`:
  answers     the candidate moves to that query's `positives`;
  partial     dropped as ambiguous;
  non-answer  kept as a hard negative, with its verdict recorded.
A pair the judge left out stays unjudged; export refuses unjudged negatives.

Modes:
  --dry-run   build `judge/judge-requests.jsonl` and print its size. With
              --stub-verdict, every pair also gets the verdict `non-answer`
              from model "stub" (`judge.stub = true`), for pipeline tests only.
  --run [--jobs N] [--retry ID...]   as generate.py; responses in
              `responses/judge/`, parse problems in `judge/judge-skipped.jsonl`.

Output: `<version>/queries.judged.jsonl`.

usage: judge.py --version v0.0-stub --dry-run --stub-verdict
"""

import argparse
import json
import sys
from collections import Counter, defaultdict
from pathlib import Path

import claude_cli
import common as C
from generate import custom_id

MODEL = "claude-sonnet-5-5"
EFFORT = "low"
HEAD_LINES = 40
PER_REQUEST = 5
MAX_TOKENS = 16000
PROMPT = C.HERE / "prompts" / "judge.md"
VERDICTS = ("answers", "partial", "non-answer")

# One item per (target, candidate): `verdicts[k]` is the verdict for query
# q = k + 1, so a left-out pair is a short list, never a silent omission.
SCHEMA = {
    "type": "object",
    "properties": {
        "verdicts": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "t": {"type": "integer"},
                    "c": {"type": "integer"},
                    "verdicts": {"type": "array", "items": {"type": "string", "enum": list(VERDICTS)}},
                },
                "required": ["t", "c", "verdicts"],
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


def target_block(i, tkey, qs, cands, nodes):
    t = nodes[tkey]
    qtext = "\n".join(f"q{k}: {q['text']}" for k, q in enumerate(qs, 1))
    ctext = "\n\n".join(f"c{k}: {nodes[c['key']]['filePath']} {nodes[c['key']]['qualifiedName']}\n"
                        f"{code_head(nodes[c['key']])}" for k, c in enumerate(cands, 1))
    return (f"t{i}: Target: {t['filePath']} {t['qualifiedName']}\n{code_head(t, 150)}\n\n"
            f"Queries of t{i} ({len(qs)}):\n{qtext}\n\nCandidates of t{i} ({len(cands)}):\n\n{ctext}")


def build_requests(rows, nodes, prompt, model=MODEL, effort=EFFORT, per_request=PER_REQUEST):
    """Requests of up to `per_request` targets of one repo each, in row order;
    targets without candidates are left out. `targets[i]` is the pack's
    target `t = i + 1`."""
    by_repo = defaultdict(list)
    for tkey, qs in by_target(rows).items():
        if qs[0]["negatives"]:
            by_repo[qs[0]["repo"]].append((tkey, qs))
    out = []
    for repo, ts in by_repo.items():
        for i in range(0, len(ts), per_request):
            group = ts[i:i + per_request]
            packs = [{"target": tkey, "queries": [q["id"] for q in qs],
                      "candidates": [c["key"] for c in qs[0]["negatives"]]} for tkey, qs in group]
            body = "\n\n---\n\n".join(target_block(k, tkey, qs, qs[0]["negatives"], nodes)
                                       for k, (tkey, qs) in enumerate(group, 1))
            n = int(group[0][1][0]["id"].split("-")[-2])
            out.append({
                "custom_id": custom_id("judge", repo, n),
                "targets": packs,
                "params": {
                    "model": model,
                    "max_tokens": MAX_TOKENS,
                    "system": [{"type": "text", "text": prompt, "cache_control": {"type": "ephemeral"}}],
                    "messages": [{"role": "user", "content": f"Targets ({len(group)}):\n\n{body}"}],
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
    seen = set()
    for v in json.loads(text)["verdicts"]:
        if not 1 <= v["t"] <= len(request["targets"]):
            problems.append(f"target out of range t{v['t']} c{v['c']}")
            continue
        t = request["targets"][v["t"] - 1]
        if not 1 <= v["c"] <= len(t["candidates"]):
            problems.append(f"candidate out of range t{v['t']} c{v['c']}")
            continue
        if (v["t"], v["c"]) in seen:
            problems.append(f"duplicate t{v['t']} c{v['c']}")
            continue
        seen.add((v["t"], v["c"]))
        if len(v["verdicts"]) != len(t["queries"]):
            # Alignment to q1..qn is unknown: no pair of this entry is judged, so export stops on them.
            problems.append(f"t{v['t']} c{v['c']}: {len(v['verdicts'])} verdicts for {len(t['queries'])} queries;"
                            " entry dropped")
            continue
        for q, verdict in zip(t["queries"], v["verdicts"]):
            out[(q, t["candidates"][v["c"] - 1])] = {
                "verdict": verdict, "model": message.get("model", MODEL), "promptSha256": prompt_sha,
                "customId": request["custom_id"]}
    for ti, t in enumerate(request["targets"], 1):
        missing = [ci for ci in range(1, len(t["candidates"]) + 1) if (ti, ci) not in seen]
        if missing:
            problems.append(f"t{ti}: no entry for candidates {missing}")
    return out, problems


def unjudged_pairs(request, got):
    """(query, candidate) pairs of `request` that `got` gives no verdict."""
    return sum(1 for t in request["targets"] for q in t["queries"] for c in t["candidates"]
               if (q, c) not in (got or {}))


def collect(requests, step_dir, prompt_sha):
    """Parses every stored response. Returns (verdicts, skipped rows,
    {custom_id: unjudged pair count} of the packs with unjudged pairs,
    custom_ids of the packs with no stored response)."""
    verdicts, skipped, short, absent = {}, [], {}, set()
    for req in requests:
        cid = req["custom_id"]
        cli, why = claude_cli.stored(req, step_dir)
        if cli is None:
            skipped.append({"customId": cid, "reason": why})
            short[cid] = unjudged_pairs(req, None)
            absent.add(cid)
            continue
        got, problems = parse_message(req, claude_cli.to_message(cli), prompt_sha)
        n = unjudged_pairs(req, got)
        if n:
            short[cid] = n
        if got is None:
            skipped.append({"customId": cid, "reason": problems})
            continue
        verdicts.update(got)
        skipped += [{"customId": cid, "reason": p} for p in problems]
    return verdicts, skipped, short, absent


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--run", action="store_true")
    ap.add_argument("--stub-verdict", action="store_true")
    claude_cli.add_run_args(ap)
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
    pairs = sum(len(t["queries"]) * len(t["candidates"]) for r in requests for t in r["targets"])
    print(f"judge: {len(requests)} requests, {pairs} (query, candidate) pairs, {chars} request chars "
          f"(~{chars // 4} input tokens before caching), prompt sha256 {prompt_sha}")

    if args.dry_run:
        if args.stub_verdict:
            stub = {"verdict": "non-answer", "model": "stub", "promptSha256": None, "customId": None, "stub": True}
            verdicts = {(q, c): stub for r in requests for t in r["targets"]
                        for q in t["queries"] for c in t["candidates"]}
            judged = apply_verdicts(rows, verdicts)
            C.write_jsonl(vdir / "queries.judged.jsonl", judged)
            print(f"judge --stub-verdict: {len(verdicts)} stub verdicts (non-answer)")
        return

    resp = vdir / "responses"
    claude_cli.run_step("judge", requests, resp, args, SCHEMA, prompt, prompt_sha, MODEL, EFFORT, PER_REQUEST,
                        args.version)
    verdicts, skipped, unjudged, absent = collect(requests, resp / "judge", prompt_sha)
    # A stored response that leaves pairs unjudged is re-sent once per invocation; a recorded
    # failure is not (--retry re-sends it).
    short = {cid: n for cid, n in unjudged.items() if cid not in absent}
    resent = []
    if short:
        print(f"judge --run: re-sending {len(short)} packs with unjudged pairs once: {sorted(short)}", flush=True)
        claude_cli.run_step("judge", requests, resp, argparse.Namespace(**dict(vars(args), retry=sorted(short))),
                            SCHEMA, prompt, prompt_sha, MODEL, EFFORT, PER_REQUEST, args.version)
        verdicts, skipped, after, _ = collect(requests, resp / "judge", prompt_sha)
        resent = [{"customId": cid, "unjudgedBefore": short[cid], "unjudgedAfter": after.get(cid, 0)}
                  for cid in sorted(short)]
        print(f"judge --run: re-sent {len(resent)} packs, {sum(1 for r in resent if r['unjudgedAfter'])} "
              f"still with unjudged pairs", flush=True)
    C.write_jsonl(vdir / "judge" / "judge-resent.jsonl", resent)
    judged = apply_verdicts(rows, verdicts)
    C.write_jsonl(vdir / "queries.judged.jsonl", judged)
    C.write_jsonl(vdir / "judge" / "judge-skipped.jsonl", skipped)
    print(f"judge --run: {len(verdicts)} verdicts {dict(Counter(v['verdict'] for v in verdicts.values()))}, "
          f"{len(skipped)} skipped")


if __name__ == "__main__":
    main()
