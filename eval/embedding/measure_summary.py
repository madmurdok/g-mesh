#!/usr/bin/env python3
"""GM-398 S14 summary: `costs OUT REF V...` writes costs.toml; `table OUT REF V...` a compact markdown summary."""
import glob, json, os, re, statistics, sys

WT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
TCORPUS = os.environ.get("TCORPUS", "g-mesh")


def model_dir(v, ref):
    if v == ref:
        return os.path.expanduser("~/.g-mesh/models/jina-embeddings-v2-base-code")
    return os.path.join(WT, "eval/embedding/work/models", v)


def model_bytes(v, ref):
    d = model_dir(v, ref)
    return sum(os.path.getsize(os.path.join(d, f)) for f in ("model.onnx", "tokenizer.json"))


def timed(out, v):
    rows = []
    for rdir in sorted(glob.glob(os.path.join(out, "timed", "r*"))):
        f = os.path.join(rdir, v + ".time")
        t = os.path.join(rdir, v, TCORPUS, "timings.json")
        if not (os.path.exists(f) and os.path.exists(t)):
            continue
        text = open(f).read()
        g = lambda k: float(re.search(rf"^{k} +([\d.]+)", text, re.M).group(1))
        rss = int(re.search(r"(\d+)\s+maximum resident set size", text).group(1))
        la = lambda k: re.search(rf"^{k}:.*load averages?: ([\d.]+)", text, re.M).group(1)
        att = re.search(r"^attempt: (\d+)", text, re.M).group(1)
        d = json.load(open(t))
        rows.append(dict(round=os.path.basename(rdir), pass_s=d["embedNodesMs"] / 1000, nodes=d["nodeCount"],
                         real=g("real"), user=g("user"), sys=g("sys"), rss=rss,
                         load_before=la("before"), load_after=la("after"), attempt=att))
    return rows


def qlat(out, v):
    t = os.path.join(out, "qlat", v, TCORPUS, "timings.json")
    d = json.load(open(t))
    return d["queryEmbedMsMedian"], len(d["queryEmbedMs"])


def costs(out, ref, vs):
    print("# GM-398 S14 D11 measurements: medians of the timed rounds; query latency from qlat/")
    for v in [ref] + vs:
        rows = timed(out, v)
        print("[[variant]]")
        print(f'name = "{v}"')
        print(f"pass_seconds = {statistics.median(r['pass_s'] for r in rows):.3f}")
        print(f"max_rss_bytes = {statistics.median(r['rss'] for r in rows):.0f}")
        print(f"model_bytes = {model_bytes(v, ref)}")
        print(f"query_latency_ms = {qlat(out, v)[0]:.3f}")
        print()


def pct(x):
    return "-" if x is None else f"{100 * x:.1f}"


def table(out, ref, vs):
    print("## Timed passes (corpus %s)\n" % TCORPUS)
    print("| variant | round | attempt | pass s | real | user | sys | user/real | max RSS MB | load before | load after |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for v in [ref] + vs:
        for r in timed(out, v):
            print(f"| {v} | {r['round']} | {r['attempt']} | {r['pass_s']:.1f} | {r['real']:.1f} | {r['user']:.1f} | {r['sys']:.1f} "
                  f"| {r['user'] / r['real']:.2f} | {r['rss'] / 1e6:.0f} | {r['load_before']} | {r['load_after']} |")
    print()
    print("## Cost medians and ratios to R\n")
    print("| variant | pass s | x R | max RSS MB | x R | size MB | x R | query ms (n) | x R |")
    print("|---|---|---|---|---|---|---|---|---|")
    c = {}
    for v in [ref] + vs:
        rows = timed(out, v)
        q, n = qlat(out, v)
        c[v] = (statistics.median(r["pass_s"] for r in rows), statistics.median(r["rss"] for r in rows),
                model_bytes(v, ref), q, n)
    for v, (p, rss, size, q, n) in c.items():
        R = c[ref]
        print(f"| {v} | {p:.1f} | {p / R[0]:.2f} | {rss / 1e6:.0f} | {rss / R[1]:.2f} | {size / 1e6:.1f} | {size / R[2]:.2f} "
              f"| {q:.2f} ({n}) | {q / R[3]:.2f} |")
    print()

    rp = os.path.join(out, "report.json")
    if not os.path.exists(rp):
        print("no report.json")
        return
    rep = json.load(open(rp))
    print("## Validity\n")
    print("```\n" + json.dumps(rep["validity"], indent=1) + "\n```\n")
    arms = rep["arms"]
    langs = sorted(arms[ref]["recall@10ByLanguage"])
    corpora = sorted(arms[ref]["recall@10ByCorpus"])
    print("## Quality, pooled (languages weighted equally)\n")
    print("| arm | r@1 | r@5 | r@10 [lo, hi] | MRR [lo, hi] | CW comb % | CW pos % | CW abs % | disc@10 vs R |")
    print("|---|---|---|---|---|---|---|---|---|")
    for a, e in arms.items():
        fl = e["floors"]
        print(f"| {a} | {e['recall@1']:.3f} | {e['recall@5']:.3f} | {e['recall@10']['point']:.3f} [{e['recall@10']['lower']:.3f}, {e['recall@10']['upper']:.3f}] "
              f"| {e['mrr']['point']:.3f} [{e['mrr']['lower']:.3f}, {e['mrr']['upper']:.3f}] "
              f"| {pct(fl['confidentWrongCombined']['rate'])} | {pct(fl['confidentWrongPositives']['rate'])} | {pct(fl['confidentWrongAbsent']['rate'])} "
              f"| {e['discordanceVsReference@10']:.3f} |")
    print()
    print("## recall@10 by language / corpus; floors; own held-out false alarm %\n")
    print("| arm | " + " | ".join(langs) + " | " + " | ".join(corpora) + " | floors " + "/".join(langs) + " | FA% " + "/".join(langs) + " | >512 tok g-mesh |")
    print("|---|" + "---|" * (len(langs) + len(corpora) + 3))
    for a, e in arms.items():
        fl = e["floors"]
        floors = "/".join(str(fl["floors"].get(l, "-")) for l in langs)
        fa = "/".join(pct(fl["falseAlarmHeldOut"].get(l)) for l in langs)
        ts = e["tokenShareOver512"].get("g-mesh")
        print(f"| {a} | " + " | ".join(f"{e['recall@10ByLanguage'][l]:.2f}" for l in langs) + " | "
              + " | ".join(f"{e['recall@10ByCorpus'][c]:.2f}" for c in corpora)
              + f" | {floors} | {fa} | {pct(ts)} |")
    print()
    print("## Candidates vs R: gates\n")
    for a, e in arms.items():
        if "gates" not in e:
            continue
        print(f"### {a}: verdict {e['verdict']}\n")
        for g in e["gates"]:
            print(f"- {g['id']} {'PASS' if g['passed'] else 'FAIL'}: {g['detail']}")
        fd = e.get("falseAlarmDelta")
        if fd:
            print(f"- Q5 pooled delta {pct(fd['point'])} pts [lower {pct(fd['lower'])}, upper {pct(fd['upper'])}]")
        for l, b in (e.get("falseAlarmDeltaByLanguage") or {}).items():
            print(f"  - Q5 {l}: n={b['n']} delta {pct(b['point'])} pts [lower {pct(b['lower'])}, upper {pct(b['upper'])}]")
        print()
    print(f"winner: {rep['winner']}")


if __name__ == "__main__":
    mode, out, ref, *vs = sys.argv[1:]
    {"costs": costs, "table": table}[mode](out, ref, vs)
