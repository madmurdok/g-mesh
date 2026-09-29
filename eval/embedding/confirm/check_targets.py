#!/usr/bin/env python3
"""Control for the GM-422 confirmatory target lists (section 3 of
docs/architecture/embedding-eval-int8-confirm.md).

Independently of sample_targets.py, checks for every corpus that
confirm/targets/<corpus>.jsonl
- shares no (filePath, qualifiedName) with any GM-398 target row (the whole
  list, not only the consumed prefix) nor with any expected symbol of a
  GM-398 authored query (eval/embedding/queries);
- has seed 4222, the protocol's snapshot sha256, and consecutive numbers;
- keeps the 6/3/1 strata pattern until a stratum runs out;
- has no duplicate target.
Also reports how many targets overlap GM-398's *unconsumed* rows (allowed).

usage: check_targets.py [targets dir]   exit 1 on any failure
"""
import json
import os
import re
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
EVAL = os.path.dirname(HERE)
SHA = {
    "gin": "1e013b7630a1f154dbe946ca243ee881d5dfe0bea9861816a100c74b4e068c62",
    "py-requests": "d24c70266c36a27af0d923c8fc6ab65d37cdad3c0a71d8c7c7c38de3e8afe7fd",
    "ripgrep": "d00f8a256d17c0cda5fe81c1a0c744a697d8c2cce9d5a6075e11f9ff3954aca0",
    "g-mesh": "1fd9a1563cd55afa2eb9be061735dc56394027ad061bfac64e2ac0c65d03fd02",
    "excalidraw": "3c7bcb4601c64f93bf8b6b043f8140f8b0f9781a6e292e0b236fdfd2102dca4d",
    "task-tracker-mcp": "6e2381ba9123712a3114c5a6127fe4f29e0a27e372147ac392c8ad2ce74041ff",
}
PATTERN = ["fn", "fn", "type", "fn", "fn", "type", "fn", "type", "fn", "other"]


def jsonl(path):
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def main():
    tdir = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "targets")
    failures = 0
    for corpus, sha in SHA.items():
        rows = jsonl(os.path.join(tdir, f"{corpus}.jsonl"))
        meta, targets = rows[0]["meta"], rows[1:]
        old_rows = [r for r in jsonl(os.path.join(EVAL, "targets", f"{corpus}.jsonl")) if "meta" not in r]
        used = set()
        for q in jsonl(os.path.join(EVAL, "queries", f"{corpus}.jsonl")):
            m = re.search(r"Target sampled \(seed 398, #(\d+)\)", q["derivation"])
            if m:
                used.add(int(m.group(1)))
        skips = {s["n"] for s in jsonl(os.path.join(EVAL, "targets", f"{corpus}.skips.jsonl"))}
        k = max(used | skips)
        consumed = {(r["filePath"], r["qualifiedName"]) for r in old_rows if r["n"] <= k}
        unconsumed = {(r["filePath"], r["qualifiedName"]) for r in old_rows if r["n"] > k}
        expected = {
            (e["filePath"], e["qualifiedName"])
            for q in jsonl(os.path.join(EVAL, "queries", f"{corpus}.jsonl"))
            for e in q["expected"]
        }
        keys = [(t["filePath"], t["qualifiedName"]) for t in targets]
        errs = []
        if meta["seed"] != 4222:
            errs.append(f"seed {meta['seed']}")
        if meta["frameSha256"] != sha:
            errs.append("frame sha256 differs from the protocol's snapshot")
        if [t["n"] for t in targets] != list(range(1, len(targets) + 1)):
            errs.append("numbers not consecutive")
        if len(set(keys)) != len(keys):
            errs.append("duplicate targets")
        hit_c = [k_ for k_ in keys if k_ in consumed]
        hit_e = [k_ for k_ in keys if k_ in expected]
        if hit_c:
            errs.append(f"{len(hit_c)} GM-398 consumed targets: {hit_c[:3]}")
        if hit_e:
            errs.append(f"{len(hit_e)} GM-398 expected symbols: {hit_e[:3]}")
        # strata: slot i wants PATTERN[i % 10] unless that stratum is exhausted
        sizes, taken = meta["frameSizes"], {"fn": 0, "type": 0, "other": 0}
        for i, t in enumerate(targets):
            want = PATTERN[i % 10]
            if taken[want] < sizes[want] and t["stratum"] != want:
                errs.append(f"#{t['n']} is {t['stratum']}, slot wants {want} with rows left")
                break
            taken[t["stratum"]] += 1
        strata = {s: sum(1 for t in targets if t["stratum"] == s) for s in taken}
        print(json.dumps({
            "corpus": corpus, "targets": len(targets), "strata": strata, "frameSizes": sizes,
            "gm398ConsumedPrefix": k, "overlapConsumed": len(hit_c), "overlapExpected": len(hit_e),
            "overlapUnconsumedGm398Rows": sum(1 for k_ in keys if k_ in unconsumed),
            "errors": errs,
        }))
        failures += bool(errs)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
