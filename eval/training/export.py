#!/usr/bin/env python3
"""Export of a GM-456 training-data version
(docs/design/GM-456-training-data.md, section 5).

From `<version>/queries.judged.jsonl`, or with `--filtered` from the
`leak_guard.py --filter` output `queries.judged.filtered.jsonl` (refused
unless `filter.guard.json` names this judged file's and this filtered file's
sha256; its drop counts go to the card and manifest):
  1. refuses any unjudged negative (`unjudged`);
  2. refuses a texts folder rendered by other code (`render.texts_for`);
  3. sets each row's `split` from corpora.toml, by repo, and refuses a repo
     on both sides (`split_rows`);
  4. removes the previous export (queries.jsonl, sample.md, card.md,
     manifest.json, guard.json, leaks.jsonl), writes the rows to
     `queries.export.jsonl` and runs `leak_guard.py --check --dataset` on it;
     a hit or a guard STOP fails the export, leaving only the staging file,
     guard.json and leaks.jsonl (when the guard got that far);
  5. on a clean guard whose guard.json names this staging file's sha256,
     renames it to `queries.jsonl` and writes `sample.md` (SAMPLE rows,
     seeded), `card.md` and
  6. `manifest.json`: pins, seeds, the generation and judging records
     (`claude_cli.run_record`: driver, Claude Code version, models, effort,
     prompt and schema sha256s, argv template, pack size, blindness probe,
     sha256 of every stored CLI response), renderIds, the guard result, per-language counts and the sha256
     of every file of the version.
`--repo-copy` also copies card.md, manifest.json and sample.md into
eval/training/ (`card/<version>.md`, `manifest/<version>.json`,
`sample/<version>.md`) for committing.

usage: export.py --version v0.0-stub [--filtered]
"""

import argparse
import json
import random
import shutil
import subprocess
import sys
from collections import Counter, defaultdict
from pathlib import Path

import claude_cli
import common as C
import generate
import judge
import render
import sample

SAMPLE = 100


def unjudged(rows):
    """[(query id, candidate key)] of negatives with no judge verdict."""
    return [(r["id"], c["key"]) for r in rows for c in r["negatives"] if not c.get("judge")]


def split_rows(rows, repos):
    """Rows with `split` set from their repo's corpora.toml `split`; exits when
    a repo would land on both sides or has no split."""
    out, sides = [], defaultdict(set)
    for r in rows:
        s = repos.get(r["repo"], {}).get("split")
        if s not in ("train", "dev"):
            sys.exit(f"STOP: repo {r['repo']} has no train/dev split in corpora.toml")
        sides[r["repo"]].add(s)
        out.append(dict(r, split=s))
    both = sorted(repo for repo, s in sides.items() if len(s) > 1)
    if both:
        sys.exit(f"STOP: repos on both sides of the split: {both}")
    return out


def sample_md(rows, nodes, version, k=SAMPLE, seed=sample.SEED):
    pick = sorted(random.Random(seed).sample(range(len(rows)), min(k, len(rows))))
    out = [f"# GM-456 training data {version}: {len(pick)} sample rows (seed {seed})", ""]
    for i in pick:
        r = rows[i]
        t = nodes[r["target"]]
        stub = " (STUB)" if (r["gen"] or {}).get("stub") else ""
        out += [f"## {r['id']}{stub}", "",
                f"- query ({r['shape']}): {r['text']}",
                f"- split: {r['split']}; overlap: {r['overlap']}; docEcho: {r['docEcho']}",
                f"- target: `{t['filePath']}` `{t['qualifiedName']}` ({t['kind']})",
                f"- positives: {len(r['positives'])}; negatives: {len(r['negatives'])}"]
        for c in r["negatives"][:5]:
            n = nodes[c["key"]]
            j = c["judge"] or {}
            out.append(f"  - `{n['qualifiedName']}` [{', '.join(c['sources'])}] sameFile={c['sameFile']} "
                       f"judge={j.get('verdict')}{' (stub)' if j.get('stub') else ''}")
        out.append("")
    return "\n".join(out)


def filter_record(vdir):
    """The `filter` block of filter.guard.json, checked to be the filter run
    of this queries.judged.jsonl that wrote this queries.judged.filtered.jsonl;
    exits otherwise."""
    judged, filtered = vdir / "queries.judged.jsonl", vdir / "queries.judged.filtered.jsonl"
    path = vdir / "filter.guard.json"
    rep = json.load(open(path)) if path.exists() else None
    if not rep or not filtered.exists() or "filter" not in rep:
        sys.exit(f"STOP: --filtered needs {filtered.name} and {path.name} (run leak_guard.py --filter --dataset "
                 f"{judged})")
    if rep.get("datasetSha256") != C.sha256_file(judged) or rep["filter"].get("outputSha256") != C.sha256_file(
            filtered):
        sys.exit(f"STOP: {path.name} is not the filter run of this {judged.name} and {filtered.name}; "
                 f"rerun the filter stage")
    return dict(rep["filter"], hitsPerCheck=rep["hitsPerCheck"])


def card_md(version, rows, repos, counts, guard, stub, filt=None):
    langs = counts["queriesPerLanguage"]
    lines = [f"# GM-456 training data {version}", ""]
    if stub:
        lines += ["**STUB DATA.** Queries come from `generate.py --stub` and verdicts from",
                  "`judge.py --dry-run --stub-verdict`. For pipeline tests only; never train on it.", ""]
    lines += [
        "Source: pinned public repos (`eval/training/corpora.toml`), none of them an eval corpus.",
        f"Size: {len(rows)} queries over {counts['targets']} targets.",
        f"Language mix: {json.dumps(langs)}.",
        f"Split (by repo): {json.dumps(counts['queriesPerSplit'])}.",
        f"Leak guard: {guard['result']} ({json.dumps(guard['hitsPerCheck'])}).",
        (f"Leak filter: dropped {filt['rowsDropped']} of {filt['rowsIn']} rows and {filt['negativesDropped']} "
         f"negatives of kept rows ({json.dumps(filt['hitsPerCheck'])}; filter.leaks.jsonl)." if filt else
         "Leak filter: not run."),
        "",
        "Known biases: queries are LLM-written (cleaner than agent queries; see `docEcho` and the",
        "`overlap = false` share); hard negatives are what BM25 and the shipped jina int8 rank high,",
        "plus same-parent, same-file, trait-impl and name-sibling nodes.",
        "",
        f"`overlap = false` share per language: {json.dumps(counts['noOverlapShare'])}.",
        f"Mean docEcho per language: {json.dumps(counts['docEchoMean'])}.",
        f"Negatives per source: {json.dumps(counts['negativesPerSource'])}; same-file share "
        f"{counts['negativeSameFileShare']}.",
        "",
        "S7 rates (query correctness, false-negative rate): not measured yet.",
    ]
    return "\n".join(lines) + "\n"


def file_hashes(vdir, skip=("manifest.json",)):
    out = {}
    for p in sorted(Path(vdir).rglob("*")):
        rel = str(p.relative_to(vdir))
        if p.is_file() and not p.is_symlink() and rel not in skip and not rel.startswith("mine/work"):
            out[rel] = C.sha256_file(p)
    return out


def step_record(vdir, step, prompt, pack_size, stub):
    """The manifest block of a step: its CLI run record, or, for a step that
    never ran on the CLI, the prompt sha256 and pack size with driver `stub`
    (stub data) or `none`."""
    rec = claude_cli.run_record(Path(vdir) / "responses", step)
    if rec is not None:
        return rec
    return {"driver": "stub" if stub else "none", "promptSha256": C.sha256_file(prompt), "packSize": pack_size}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo-copy", action="store_true")
    ap.add_argument("--eval-work", default=None)
    ap.add_argument("--filtered", action="store_true",
                    help="export queries.judged.filtered.jsonl (the pipeline's filter stage)")
    args = ap.parse_args()
    vdir = Path(C.version_dir(args.version))
    filt = filter_record(vdir) if args.filtered else None
    rows = C.read_jsonl(vdir / ("queries.judged.filtered.jsonl" if args.filtered else "queries.judged.jsonl"))
    missing = unjudged(rows)
    if missing:
        sys.exit(f"STOP: {len(missing)} unjudged negatives, e.g. {missing[:5]}")
    repos = C.corpora()
    rows = split_rows(rows, repos)
    names = sorted({r["repo"] for r in rows})
    render_ids = {}
    for form in render.FORMS:
        for repo in names:
            render.texts_for(vdir, form, repo)
        render_ids[form] = render.render_id(form)

    nodes = {}
    for repo in names:
        for n in C.load_nodes(vdir, repo):
            nodes[n["key"]] = n
    # Nothing of a previous export may survive a failed guard, and the card
    # may only quote this run's guard.json.
    for name in ("queries.jsonl", "sample.md", "card.md", "manifest.json", "guard.json", "leaks.jsonl"):
        (vdir / name).unlink(missing_ok=True)
    staging = vdir / "queries.export.jsonl"
    C.write_jsonl(staging, rows)
    staging_sha = C.sha256_file(staging)

    by_lang = defaultdict(list)
    for r in rows:
        by_lang[r["language"]].append(r)
    negs = [c for r in rows for c in r["negatives"]]
    counts = {
        "targets": len({r["target"] for r in rows}),
        "queriesPerLanguage": {l: len(v) for l, v in sorted(by_lang.items())},
        "queriesPerRepo": dict(Counter(r["repo"] for r in rows)),
        "queriesPerSplit": dict(Counter(r["split"] for r in rows)),
        "noOverlapShare": {l: round(sum(not r["overlap"] for r in v) / len(v), 4) for l, v in by_lang.items()},
        "docEchoMean": {l: round(sum(r["docEcho"] for r in v) / len(v), 4) for l, v in by_lang.items()},
        "negativesPerSource": dict(Counter(s for c in negs for s in c["sources"])),
        "negativeSameFileShare": round(sum(c["sameFile"] for c in negs) / len(negs), 4) if negs else None,
        "positivesPerQuery": C.quantiles([len(r["positives"]) for r in rows]),
    }
    stub = any((r["gen"] or {}).get("stub") for r in rows) or any((c["judge"] or {}).get("stub") for c in negs)

    cmd = [sys.executable, str(C.HERE / "leak_guard.py"), "--version", args.version, "--check",
           "--dataset", str(staging)]
    if args.eval_work:
        cmd += ["--eval-work", args.eval_work]
    guard_rc = subprocess.run(cmd).returncode
    if guard_rc != 0:
        sys.exit(f"STOP: the leak guard exited {guard_rc}; nothing exported. See its output, guard.json and "
                 f"leaks.jsonl (if written); the rows are in {staging.name}")
    guard_path = vdir / "guard.json"
    guard = json.load(open(guard_path)) if guard_path.exists() else None
    if not guard or guard.get("result") != "clean" or guard.get("datasetSha256") != staging_sha:
        sys.exit(f"STOP: guard.json is missing, not clean or not this run's (datasetSha256 "
                 f"{(guard or {}).get('datasetSha256')}, staging {staging_sha}); nothing exported")
    staging.rename(vdir / "queries.jsonl")
    guard["dataset"] = str(vdir / "queries.jsonl")
    C.write_json(guard_path, guard)
    (vdir / "sample.md").write_text(sample_md(rows, nodes, args.version))
    (vdir / "card.md").write_text(card_md(args.version, rows, repos, counts, guard, stub, filt))

    targets_meta = json.load(open(vdir / "targets.meta.json"))
    manifest = {
        "version": args.version,
        "stub": stub,
        "pins": {r: {k: repos[r].get(k) for k in ("url", "ref", "revision", "licence", "split")} for r in names},
        "licenceFiles": {r: (C.work_dir() / "licences" / f"{r}.tsv").read_text().splitlines()
                         for r in names if (C.work_dir() / "licences" / f"{r}.tsv").exists()},
        "seeds": {"sample": targets_meta["seed"]},
        "generation": step_record(vdir, "gen", generate.PROMPT, generate.PER_REQUEST, stub),
        "judging": step_record(vdir, "judge", judge.PROMPT, judge.PER_REQUEST, stub),
        "renderIds": render_ids,
        "guard": {k: guard[k] for k in ("result", "hitsPerCheck", "rowsFlaggedShare", "thresholds", "pinsVerified")},
        "filter": filt,
        "counts": counts,
        "files": file_hashes(vdir),
    }
    C.write_json(vdir / "manifest.json", manifest)
    print("export:", json.dumps({"rows": len(rows), "guard": guard["result"],
                                 "filterDropped": filt and filt["rowsDropped"], **counts}))
    if args.repo_copy:
        for src, dst in (("card.md", f"card/{args.version}.md"), ("manifest.json", f"manifest/{args.version}.json"),
                         ("sample.md", f"sample/{args.version}.md")):
            (C.HERE / dst).parent.mkdir(exist_ok=True)
            shutil.copyfile(vdir / src, C.HERE / dst)


if __name__ == "__main__":
    main()
