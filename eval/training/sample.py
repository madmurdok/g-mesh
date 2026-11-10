#!/usr/bin/env python3
"""Seeded, stratified target order with per-repo quotas
(docs/design/GM-456-training-data.md, section 1 "Counts").

Per repo, the frame is its `nodes/<repo>.jsonl` rows in the repo's language,
less the native kinds that are references rather than declarations
(SKIP_NATIVE). Each stratum (`sample_targets.stratum`) is shuffled with
`random.Random(f"{seed}:{repo}")`, then picked in `STRATA_PATTERN` order
(6 fn / 3 type / 1 other per ten; an exhausted stratum hands its slot to the
next non-empty one in fn, type, other order, as `sample_targets.py` does).

Quotas: `--count N` gives every named repo N targets (the S2 pilot). Without
it, `--per-language` totals are split across a language's repos in proportion
to their frames, each capped at `--cap` of the language total, and the excess
goes to the uncapped repos (`quotas`).

usage: sample.py --version v0.0-stub --count 50 [--repo mini-redis]
       sample.py --version v1.0 [--per-language rust=1600,go=800,python=800,typescript=800]
"""

import argparse
import json
import random
from collections import Counter

import common as C
from sample_targets import STRATA_PATTERN, stratum

SEED = 456
SKIP_NATIVE = {"pending_symbol", "external_module", "resolved_module", "reexport"}
DEFAULT_PER_LANGUAGE = {"rust": 1600, "go": 800, "python": 800, "typescript": 800}


def target_frame(nodes, language):
    return [n for n in nodes if n["language"] == language and n["nativeKind"] not in SKIP_NATIVE]


def order(frame, seed, repo):
    """The frame in authoring order: per-stratum seeded shuffle, then the
    6/3/1 pattern."""
    rng = random.Random(f"{seed}:{repo}")
    strata = {"fn": [], "type": [], "other": []}
    for n in sorted(frame, key=lambda n: n["key"]):
        strata[stratum(n["kind"])].append(n)
    for s in ("fn", "type", "other"):
        rng.shuffle(strata[s])
    out, cursor, i = [], Counter(), 0
    total = sum(len(v) for v in strata.values())
    while len(out) < total:
        want = STRATA_PATTERN[i % len(STRATA_PATTERN)]
        i += 1
        for s in [want, "fn", "type", "other"]:
            if cursor[s] < len(strata[s]):
                out.append((s, strata[s][cursor[s]]))
                cursor[s] += 1
                break
    return out


def quotas(frames, total, cap):
    """Split `total` over repos in proportion to `frames` ({repo: size}),
    no repo above `cap * total` or its own frame; largest-remainder rounding.
    Returns {repo: quota}; the sum is `total` unless the frames are too small."""
    limit = {r: min(int(cap * total), f) for r, f in frames.items()}
    quota = {r: 0 for r in frames}
    left, open_ = total, set(r for r in frames if limit[r] > 0)
    while left > 0 and open_:
        weight = sum(frames[r] for r in open_)
        share = {r: left * frames[r] / weight for r in open_}
        floor = {r: min(int(share[r]), limit[r] - quota[r]) for r in open_}
        given = sum(floor.values())
        if given == 0:
            for r in sorted(open_, key=lambda r: (-(share[r] - int(share[r])), r)):
                if left == 0:
                    break
                if quota[r] < limit[r]:
                    quota[r] += 1
                    left -= 1
        else:
            for r in open_:
                quota[r] += floor[r]
            left -= given
        open_ = set(r for r in open_ if quota[r] < limit[r])
    return quota


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo", action="append", default=[])
    ap.add_argument("--seed", type=int, default=SEED)
    ap.add_argument("--count", type=int, help="targets per repo (overrides the quotas)")
    ap.add_argument("--per-language", help="e.g. rust=1600,go=800")
    ap.add_argument("--cap", type=float, default=0.30)
    args = ap.parse_args()
    vdir = C.version_dir(args.version)
    repos = C.corpora()
    names = args.repo or C.node_repos(vdir)

    ordered, frames = {}, {}
    for repo in names:
        f = target_frame(C.load_nodes(vdir, repo), repos[repo]["language"])
        frames[repo] = len(f)
        ordered[repo] = order(f, args.seed, repo)

    if args.count is not None:
        quota = {r: min(args.count, frames[r]) for r in names}
    else:
        per = dict(DEFAULT_PER_LANGUAGE)
        if args.per_language:
            per.update({k: int(v) for k, v in (x.split("=") for x in args.per_language.split(","))})
        quota = {}
        for lang, total in per.items():
            fr = {r: frames[r] for r in names if repos[r]["language"] == lang}
            if fr:
                quota.update(quotas(fr, total, args.cap))

    rows = []
    for repo in names:
        for n, (s, node) in enumerate(ordered[repo][: quota[repo]], 1):
            rows.append({"n": n, "repo": repo, "stratum": s, "key": node["key"]})
    C.write_jsonl(vdir / "targets.jsonl", rows)
    meta = {
        "seed": args.seed,
        "frames": frames,
        "quotas": quota,
        "strata": {r: dict(Counter(x["stratum"] for x in rows if x["repo"] == r)) for r in names},
        "skipNative": sorted(SKIP_NATIVE),
    }
    C.write_json(vdir / "targets.meta.json", meta)
    print("sample:", json.dumps(meta))


if __name__ == "__main__":
    main()
