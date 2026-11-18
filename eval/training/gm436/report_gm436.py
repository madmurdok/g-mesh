#!/usr/bin/env python3
"""GM-436 results table (docs/design/GM-436-laptop-training.md, section 5 item 3).

From the same v2 run dirs, per arm:
- `debug-embed-eval report --json` over all arms (floors refit per arm on v2
  FIT, splits, validity, gates);
- shipped_floor_rates.py at that arm's refit floors (`--custom`): misled %,
  confident-wrong positives/absent;
- r@1..100 and MRR, pooled over languages (recall_at_k.py's definitions), and
  r@10 per corpus;
- the GM-398 subset and the confirm subset (gm460_subset.py), scored the same
  way at the v2-FIT floors;
- the words-shuffled gap: r@10 of an arm minus r@10 of its twin on the
  words-shuffled base (`--ws-pair ARM=TWIN`).

Writes <out>/results.json and <out>/results.md.

usage: report_gm436.py --bin <g-mesh> --runs <v2 runs dir> --arm A [--arm B ...]
                       [--ws-pair ARM=TWIN ...] --out <dir>
"""

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
EMB = REPO / "eval" / "embedding"
sys.path.insert(0, str(EMB))
import recall_at_k as RK  # noqa: E402

KS = [1, 5, 10, 20, 50, 100]


def run(cmd):
    print("+", " ".join(map(str, cmd)), flush=True)
    subprocess.run([str(c) for c in cmd], check=True)


def scores(eval_dir, run_dir):
    rows, _ = RK.load_arm(Path(eval_dir), Path(run_dir))
    nl = [r for r in rows if r["stratum"] == "nl"]
    out = {f"r@{k}": RK.recall(nl, k) for k in KS}
    out["mrr"] = RK.pooled(RK.by_lang(nl, RK.rr))
    corp = {}
    for r in nl:
        corp.setdefault(r["corpus"], []).append(RK.hit(10)(r))
    out["r@10ByCorpus"] = {c: sum(v) / len(v) for c, v in sorted(corp.items())}
    return out


def floor_rates(run_dir, eval_dir, floors, out_json):
    custom = ",".join(f"{k}={v}" for k, v in sorted(floors.items()))
    run([sys.executable, EMB / "shipped_floor_rates.py", "--run", run_dir, "--eval-dir", eval_dir,
         "--custom", custom, "--json", out_json])
    return json.loads(Path(out_json).read_text())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--runs", required=True, type=Path)
    ap.add_argument("--eval-dir", type=Path, default=EMB / "v2")
    ap.add_argument("--gm398-dir", type=Path, default=EMB)
    ap.add_argument("--confirm-dir", type=Path, default=EMB / "confirm")
    ap.add_argument("--arm", action="append", required=True)
    ap.add_argument("--ws-pair", action="append", default=[])
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)

    report_json = a.out / "report.json"
    run([a.bin, "debug-embed-eval", "report", "--eval-dir", a.eval_dir, "--json", report_json]
        + [a.runs / arm for arm in a.arm])
    report = json.loads(report_json.read_text())

    subsets = {"gm398": a.gm398_dir, "confirm": a.confirm_dir}
    for name, d in subsets.items():
        run([sys.executable, EMB / "gm460_subset.py", "--runs", a.runs, "--eval-dir", d,
             "--out", a.out / f"subset-{name}"] + sum((["--arm", arm] for arm in a.arm), []))

    res = {"validity": report.get("validity"), "reference": report.get("reference"), "arms": {}}
    for arm in a.arm:
        rep = report["arms"][arm]
        floors = rep["floors"]["floors"]
        row = {"report": {k: rep.get(k) for k in ("mrr", "recall@10", "recall@10ByCorpus", "splits", "verdict",
                                                    "gates", "discordanceVsReference@10")},
               "floors": floors, "v2": scores(a.eval_dir, a.runs / arm)}
        if floors:
            row["floorRates"] = floor_rates(a.runs / arm, a.eval_dir, floors, a.out / f"floors-{arm}.json")
        for name, d in subsets.items():
            sub = a.out / f"subset-{name}" / arm
            row[name] = scores(d, sub)
            if floors:
                row[name]["floorRates"] = floor_rates(sub, d, floors, a.out / f"floors-{name}-{arm}.json")
        res["arms"][arm] = row
    for pair in a.ws_pair:
        arm, twin = pair.split("=", 1)
        res["arms"][arm]["wordsShuffledGap@10"] = res["arms"][arm]["v2"]["r@10"] - scores(a.eval_dir, a.runs / twin)["r@10"]

    (a.out / "results.json").write_text(json.dumps(res, indent=1, default=str) + "\n")
    lines = ["| arm | MRR | r@1 | r@10 | r@100 | r@10 GM-398 | r@10 confirm | WS gap@10 | " +
             " | ".join(sorted(next(iter(res["arms"].values()))["v2"]["r@10ByCorpus"])) + " |"]
    lines.append("|" + "---|" * (8 + len(next(iter(res["arms"].values()))["v2"]["r@10ByCorpus"])))
    for arm, r in res["arms"].items():
        v = r["v2"]
        gap = r.get("wordsShuffledGap@10")
        lines.append(f"| {arm} | {v['mrr']:.3f} | {v['r@1']:.3f} | {v['r@10']:.3f} | {v['r@100']:.3f} | "
                     f"{r['gm398']['r@10']:.3f} | {r['confirm']['r@10']:.3f} | {'' if gap is None else f'{gap:+.3f}'} | "
                     + " | ".join(f"{x:.2f}" for _, x in sorted(v["r@10ByCorpus"].items())) + " |")
    (a.out / "results.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()
