#!/usr/bin/env python3
"""GM-692 summary: one row per edit. usage: summarize.py raw.jsonl [daemon-log ...]"""
import json, sys, re
rows = [json.loads(l) for l in open(sys.argv[1])]
logs = "".join(open(p, errors="replace").read() for p in sys.argv[2:])
reps = [r for r in rows if r.get("kind") == "rep"]
out = []
out.append("rerank-unavailable log lines: %d" % len(re.findall(r"rerank model .* is not available", logs)))
out.append("")
out.append("| sess | arm | rep | first query | rank (limit 10) | top-10 hit | alpha cosine | cutoff10 (cosine) | rank@60 | rerank evidence (page not in cosine order / rerank: cursor) | first search s | uptime load1 | real s | daemon cpu s |")
out.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
for r in reps:
    fd = r.get("found_page") or r.get("first_page") or {}
    d = r.get("diag") or {}; late = d.get("late60") or {}
    ev = "%s / %s" % ("yes" if fd.get("monotone") is False else "no", (fd.get("extra") or {}).get("cursor_prefix") or late.get("cursor_prefix") or "-")
    hit = "no" if r["miss"] else "yes"
    ld = r["machine_before"]["load"][0]
    out.append("| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |" % (
        r["session"], r["arm"], r["rep"], "Y" if r["first_query"] else "", "absent" if r["miss"] else fd.get("rank"), hit,
        fd.get("score"), late.get("cutoff10"), late.get("rank"), ev, r.get("first_search_t"), ld, r.get("real_s"), r.get("daemon_cpu_s")))
    if r["miss"]:
        out.append("MISS detail: %s" % json.dumps({"names": (r.get("miss_page") or {}).get("names"), "diag": d})[:1500])
    out.append("<!-- uptime %s -->" % r.get("uptime"))
def tally(rs):
    return "%d/%d" % (sum(1 for r in rs if r["miss"]), len(rs))
out.append("")
out.append("misses (all edits): %s" % tally(reps))
out.append("misses (session 1, the 19 GM-690 edits): %s" % tally([r for r in reps if r["session"] == 1]))
out.append("misses (first-query reps): %s" % tally([r for r in reps if r["first_query"]]))
out.append("misses (non-first-query reps): %s" % tally([r for r in reps if not r["first_query"]]))
out.append("reps with rerank evidence (non-cosine order on the page): %d/%d" % (
    sum(1 for r in reps if (r.get("found_page") or r.get("first_page") or {}).get("monotone") is False), len(reps)))
for r in rows:
    if r.get("kind") in ("warm", "stopped", "trace", "end"):
        out.append("<!-- %s -->" % json.dumps({k: v for k, v in r.items() if k != "status"})[:500])
print("\n".join(out))
