#!/usr/bin/env python3
"""GM-447 S6: do real Claude Code subagents notice the answering-project stamp?

Each run starts one headless `claude -p` session whose only MCP server is
g-mesh (the binary under test, behind tap.py). The parent launches two
subagents in parallel on the one shared connection:

  A: select_project p1, signal a.ready, wait for b.ready, then look up
     retry_limit and report the integer it returns (p1: 41).
  B: wait for a.ready, select_project p2, signal b.ready, then look up
     export_format (exists only in p2).

The markers force B's select between A's select and A's lookup, so A's
lookup is routed to p2 (retry_limit returns 97 there) unless A re-selects
on its own. Whether that misroute happened is read from the wire log, not
assumed: a run with no misrouted A lookup is excluded, not counted.

Usage: run_set.py --arm NAME=BINARY [--arm ...] --runs N --fixture DIR
                  --home-root DIR --out DIR [--model sonnet]
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
PROGRESS = Path.home() / ".claude" / "progress" / "GM-447-S6.txt"
RUN_TIMEOUT_S = 900

PARENT = """You coordinate two subagents. In ONE message, launch both with the Agent tool \
(subagent_type general-purpose) so they run in parallel, passing each brief below verbatim as its prompt. \
Do not do any of their work yourself. When both have finished, reply with exactly two lines: \
`A: <subagent A's final answer>` and `B: <subagent B's final answer>`.

=== Brief for subagent A ===
{a}

=== Brief for subagent B ===
{b}"""

BRIEF_A = """You are working on the project p1 in the current folder. Inspect code only with the g-mesh MCP tools \
(mcp__g-mesh__*); do not read source files with Bash or any other tool.
1. Call mcp__g-mesh__select_project with project "p1".
2. Run this Bash command: `touch {a_ready}`
3. The p1 build is still running; wait for it by running this Bash command (timeout 600000 ms): \
`for i in $(seq 240); do [ -f {b_ready} ] && break; sleep 1; done`
4. Find the definition of the function retry_limit in p1 with the g-mesh tools and determine the integer it returns.
Final answer: one line `RETRY_LIMIT=<integer>`, followed by at most two sentences if anything about the lookup needs saying."""

BRIEF_B = """You are working on the project p2 in the current folder. Inspect code only with the g-mesh MCP tools \
(mcp__g-mesh__*); do not read source files with Bash or any other tool.
1. Wait until another job is ready by running this Bash command (timeout 600000 ms): \
`for i in $(seq 240); do [ -f {a_ready} ] && break; sleep 1; done`
2. Call mcp__g-mesh__select_project with project "p2".
3. Run this Bash command: `touch {b_ready}`
4. Find the definition of the function export_format in p2 with the g-mesh tools and determine the string it returns.
Final answer: one line `EXPORT_FORMAT=<string>`."""


def write_progress(line):
    PROGRESS.parent.mkdir(parents=True, exist_ok=True)
    tmp = PROGRESS.with_suffix(".tmp")
    tmp.write_text(line + "\n")
    tmp.replace(PROGRESS)


def load1():
    out = subprocess.run(["uptime"], capture_output=True, text=True).stdout
    m = re.search(r"load averages?: ([\d.]+)", out)
    return m.group(1) if m else "?"


def kill_ours(binary_prefixes, pid_log):
    """kill -9 every process running one of OUR binaries (by exact path)."""
    killed = []
    for prefix in binary_prefixes:
        out = subprocess.run(["pgrep", "-f", prefix], capture_output=True, text=True).stdout
        for pid in out.split():
            if int(pid) == os.getpid():
                continue
            cmd = subprocess.run(["ps", "-o", "command=", "-p", pid], capture_output=True, text=True).stdout.strip()
            if not cmd.startswith(prefix):
                continue
            try:
                os.kill(int(pid), signal.SIGKILL)
                killed.append(f"{pid} {cmd}")
            except ProcessLookupError:
                pass
    with open(pid_log, "a") as f:
        for k in killed:
            f.write(f"killed {k}\n")
    return len(killed)


def texts(resp):
    return [c.get("text", "") for c in (resp or {}).get("result", {}).get("content", []) if c.get("type") == "text"]


def analyse(wire_path):
    reqs, resps = {}, {}
    for line in open(wire_path):
        rec = json.loads(line)
        fr = rec.get("frame") or {}
        if rec["dir"] == "c2s" and fr.get("method") == "tools/call":
            reqs[fr["id"]] = (rec["t"], fr["params"])
        elif rec["dir"] == "s2c" and "id" in fr and ("result" in fr or "error" in fr):
            resps[fr["id"]] = (rec["t"], fr)
    calls = []
    for rid, (t, params) in reqs.items():
        rt, resp = resps.get(rid, (None, None))
        body = "\n".join(texts(resp))
        calls.append({"t": t, "rt": rt, "name": params.get("name"), "args": params.get("arguments") or {},
                      "texts": texts(resp), "body": body})
    calls.sort(key=lambda c: c["t"])
    b_sel = next((c for c in calls if c["name"] == "select_project" and c["args"].get("project") == "p2"), None)
    a_queries = [c for c in calls if c["name"] != "select_project" and "retry_limit" in json.dumps(c["args"])]
    for c in a_queries:
        if "return 97" in c["body"]:
            c["from"] = "p2"
        elif "return 41" in c["body"]:
            c["from"] = "p1"
        else:
            m = re.match(r"g-mesh: answered from project (\S+)\.", c["texts"][0] if c["texts"] else "")
            c["from"] = m.group(1) if m else "?"
    mis = next((c for c in a_queries if b_sel and c["t"] > b_sel["t"] and c["from"] == "p2"), None)
    out = {
        "calls": len(calls),
        "a_queries": [(c["name"], c["from"]) for c in a_queries],
        "misroute": mis is not None,
        "stamp_seen": bool(mis and mis["texts"] and mis["texts"][0].startswith("g-mesh: answered from project")),
        "stamp_text": (mis["texts"][0][:60] if mis and mis["texts"] else ""),
        "reselected": False,
        "requery_from": "",
    }
    if mis:
        later_sel = [c for c in calls if c["name"] == "select_project" and c["t"] > mis["rt"]
                     and str(c["args"].get("project") or "").rstrip("/").split("/")[-1] == "p1"]
        out["reselected"] = bool(later_sel)
        later_q = [c for c in a_queries if c["t"] > mis["rt"]]
        out["requery_from"] = ",".join(c["from"] for c in later_q)
    return out


def final_answer(stream_path):
    result = None
    tools = None
    for line in open(stream_path):
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            tools = ev.get("tools")
        if ev.get("type") == "result":
            result = ev
    return result, tools


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", action="append", required=True, help="NAME=BINARY")
    ap.add_argument("--runs", type=int, required=True)
    ap.add_argument("--fixture", required=True)
    ap.add_argument("--home-root", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--model", default="sonnet")
    ap.add_argument("--tools", default="Bash,Agent")
    args = ap.parse_args()

    arms = [a.split("=", 1) for a in args.arm]
    prefixes = [b for _, b in arms] + [str(Path(b).parent / "g-mesh-plugin-") for _, b in arms]
    out = Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    pid_log = out / "pids.log"
    summary = out / "summary.tsv"
    cols = ["arm", "run", "misroute", "stamp_seen", "reselected", "requery_from", "final", "outcome",
            "wall_s", "user_s", "inner_tokens", "cost_usd", "load1", "a_queries", "answer"]
    if not summary.exists():
        summary.write_text("\t".join(cols) + "\n")
    total = len(arms) * args.runs
    done, misroutes, started = 0, 0, time.time()
    kill_ours(prefixes, pid_log)
    for arm, binary in arms:
        for i in range(1, args.runs + 1):
            eta = "?" if done == 0 else f"{(time.time() - started) / done * (total - done) / 60:.0f}m"
            write_progress(f"arm {arm} run {i}/{args.runs} ({100 * done // total}% of {total}) "
                           f"misroutes {misroutes} eta {eta} load {load1()}")
            tag = f"{arm}-{i}"
            home = Path(args.home_root) / tag
            home.mkdir(parents=True, exist_ok=False)
            rundir = out / tag
            rundir.mkdir()
            wire = rundir / "wire.jsonl"
            cfg = rundir / "mcp.json"
            cfg.write_text(json.dumps({"mcpServers": {"g-mesh": {
                "type": "stdio", "command": sys.executable,
                "args": [str(HERE / "tap.py"), str(wire), binary, "mcp-shim"],
                "env": {"G_MESH_HOME": str(home / "gm")}}}}))
            fmt = {"a_ready": home / "a.ready", "b_ready": home / "b.ready"}
            prompt = PARENT.format(a=BRIEF_A.format(**fmt), b=BRIEF_B.format(**fmt))
            (rundir / "prompt.txt").write_text(prompt)
            env = dict(os.environ, CLAUDE_CODE_DISABLE_CLAUDE_MDS="1", CLAUDE_CODE_DISABLE_AUTO_MEMORY="1")
            cmd = ["/usr/bin/time", "-p", "claude", "-p", prompt, "--model", args.model,
                   "--output-format", "stream-json", "--verbose",
                   "--mcp-config", str(cfg), "--strict-mcp-config", "--setting-sources", "",
                   "--tools", args.tools, "--dangerously-skip-permissions",
                   "--no-session-persistence", "--max-budget-usd", "3"]
            up = subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()
            (rundir / "uptime.txt").write_text(up + "\n")
            t0 = time.time()
            with open(rundir / "stream.jsonl", "w") as so, open(rundir / "stderr.txt", "w") as se:
                p = subprocess.Popen(cmd, cwd=args.fixture, env=env, stdout=so, stderr=se,
                                     stdin=subprocess.DEVNULL, start_new_session=True)
                with open(pid_log, "a") as f:
                    f.write(f"{tag} claude(time) pid {p.pid} pgid {p.pid}\n")
                try:
                    p.wait(timeout=RUN_TIMEOUT_S)
                except subprocess.TimeoutExpired:
                    os.killpg(p.pid, signal.SIGKILL)
                    p.wait()
                    (rundir / "TIMEOUT").write_text("")
            wall = time.time() - t0
            n_killed = kill_ours(prefixes, pid_log)
            err = (rundir / "stderr.txt").read_text()
            m = re.search(r"^user\s+([\d.]+)", err, re.M)
            user_s = m.group(1) if m else "?"
            result, tools = final_answer(rundir / "stream.jsonl")
            (rundir / "init_tools.json").write_text(json.dumps(tools))
            answer = (result or {}).get("result", "") or ""
            fm = re.search(r"RETRY_LIMIT\s*=\s*`?(\d+)", answer)
            final = fm.group(1) if fm else ""
            outcome = {"41": "correct", "97": "wrong"}.get(final, "other")
            tok = 0
            for mu in ((result or {}).get("modelUsage") or {}).values():
                tok += sum(mu.get(k, 0) for k in ("inputTokens", "outputTokens",
                                                   "cacheReadInputTokens", "cacheCreationInputTokens"))
            cost = (result or {}).get("total_cost_usd", "")
            an = analyse(wire) if wire.exists() else {"misroute": False, "stamp_seen": False, "reselected": False,
                                                      "requery_from": "", "a_queries": "nowire"}
            (rundir / "analysis.json").write_text(json.dumps({**an, "killed_after": n_killed,
                                                              "modelUsage": (result or {}).get("modelUsage")},
                                                             indent=1))
            misroutes += int(an["misroute"])
            row = [arm, str(i), str(an["misroute"]), str(an["stamp_seen"]), str(an["reselected"]),
                   an["requery_from"] or "-", final or "-", outcome, f"{wall:.0f}", user_s, str(tok), str(cost),
                   up.split("load averages:")[-1].split()[0] if "load averages:" in up else "?",
                   json.dumps(an["a_queries"]), answer.replace("\n", " | ")[:300]]
            with open(summary, "a") as f:
                f.write("\t".join(row) + "\n")
            done += 1
    write_progress(f"done {done}/{total} runs, misroutes {misroutes}, {(time.time() - started) / 60:.0f}m "
                   f"load {load1()}")


if __name__ == "__main__":
    main()
