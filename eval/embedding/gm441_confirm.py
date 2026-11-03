#!/usr/bin/env python3
"""GM-441 S5 daemon confirm: one cold embedding pass through a real daemon.

Starts `<bin> mcp-shim` in a copy of the corpus checkout with an isolated
G_MESH_HOME, polls `index.progress` once a second, issues one `search_code`
at every tier boundary (a new "finished the <tier> tier" daemon log line) and
once at Ready, and writes a JSON timeline. The daemon's pid is recorded and
killed at the end.

usage: gm441_confirm.py --bin B --project DIR --home DIR --out FILE [--timeout S]
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

QUERIES = [
    "parse a configuration file",
    "compute the embedding of a node text",
    "find the nodes that still need a vector",
    "report indexing progress to the status command",
    "load the language plugin process",
]
TIER_RE = re.compile(r"finished the (\w+) tier: (\d+) candidates in ([\d.]+)s")


class Shim:
    def __init__(self, cmd, cwd, env):
        self.p = subprocess.Popen(cmd, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.next_id = 1
        self.pending = {}
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if "id" in msg and ("result" in msg or "error" in msg):
                with self.lock:
                    ev = self.pending.pop(msg["id"], None)
                if ev:
                    ev[1].append(msg)
                    ev[0].set()

    def notify(self, method, params=None):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()

    def call(self, method, params, timeout=120):
        with self.lock:
            rid = self.next_id
            self.next_id += 1
            ev = (threading.Event(), [])
            self.pending[rid] = ev
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        if not ev[0].wait(timeout):
            return None
        return ev[1][0]


def search(shim, query):
    t = time.monotonic()
    resp = shim.call("tools/call", {"name": "search_code", "arguments": {"query": query, "limit": 30}})
    took = time.monotonic() - t
    if not resp or "result" not in resp:
        return {"query": query, "error": str(resp)[:200], "took_s": took}
    text = "".join(c.get("text", "") for c in resp["result"].get("content", []))
    partial = "embedding pass" in text and "still running" in text or "has not started" in text
    scores = [float(m) for m in re.findall(r'"score"\s*:\s*(-?[0-9.]+)', text)]
    if not scores:
        sc = resp["result"].get("structuredContent") or {}
        scores = [r.get("score") for r in sc.get("results", []) if isinstance(r.get("score"), (int, float))]
    mono = all(a >= b - 1e-12 for a, b in zip(scores, scores[1:]))
    return {"query": query, "took_s": round(took, 3), "partial": bool(partial), "rows": len(scores),
            "score_monotone": mono}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--project", required=True)
    ap.add_argument("--home", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--timeout", type=float, default=1500)
    a = ap.parse_args()

    home = Path(a.home)
    home.mkdir(parents=True, exist_ok=True)
    log = home / "daemon.log"
    env = dict(os.environ, G_MESH_HOME=str(home), G_MESH_EMBEDDING_CACHE="off", G_MESH_DAEMON_LOG=str(log),
               G_MESH_SEARCH_EMBEDDING_WAIT_MS="200")
    env.pop("CLAUDE_PROJECT_DIR", None)
    t0 = time.monotonic()
    shim = Shim([a.bin, "mcp-shim"], a.project, env)
    out = {"bin": a.bin, "project": a.project, "timeline": [], "searches": [], "tiers": [], "daemonPid": None}
    init = shim.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                    "clientInfo": {"name": "gm441-confirm", "version": "1"}}, timeout=60)
    shim.notify("notifications/initialized")
    out["initialize"] = bool(init and "result" in init)

    # Indexing is lazy: the first tool call starts the walk and the passes.
    threading.Thread(target=lambda: shim.call("tools/call", {"name": "search_code", "arguments": {
        "query": "start indexing", "limit": 5}}, timeout=a.timeout), daemon=True).start()
    seen_tiers = 0
    last = None
    ready_t = None
    deadline = t0 + a.timeout
    state_dir = None
    while time.monotonic() < deadline:
        if state_dir is None:
            dirs = list((home / "projects").glob("*/")) if (home / "projects").exists() else []
            state_dir = dirs[0] if dirs else None
        row = None
        if state_dir:
            pid_file = state_dir / "daemon.pid"
            if out["daemonPid"] is None and pid_file.exists():
                try:
                    out["daemonPid"] = int(pid_file.read_text().strip())
                except ValueError:
                    pass
            try:
                prog = json.loads((state_dir / "index.progress").read_text())
                row = (prog["phase"], prog["embeddings"]["done"], prog["embeddings"]["total"])
            except (OSError, ValueError, KeyError):
                row = None
        t = round(time.monotonic() - t0, 2)
        if row and row != last:
            out["timeline"].append({"t": t, "phase": row[0], "done": row[1], "total": row[2]})
            last = row
        tiers = TIER_RE.findall(log.read_text()) if log.exists() else []
        if len(tiers) > seen_tiers:
            for tier, n, secs in tiers[seen_tiers:]:
                out["tiers"].append({"tier": tier, "candidates": int(n), "s": float(secs), "seen_t": t})
            seen_tiers = len(tiers)
            if not (row and row[0] == "ready"):
                s = search(shim, QUERIES[len(out["searches"]) % len(QUERIES)])
                s.update(at="tier-" + tiers[-1][0], t=t)
                out["searches"].append(s)
        if row and row[0] == "ready":
            ready_t = t
            break
        if row and row[0] == "failed":
            break
        time.sleep(1)
    out["ready_t"] = ready_t
    if ready_t is not None:
        for q in QUERIES:
            s = search(shim, q)
            s.update(at="ready", t=round(time.monotonic() - t0, 2))
            out["searches"].append(s)
    emb = [r for r in out["timeline"] if r["phase"] == "embedding"]
    out["embedding_start_t"] = emb[0]["t"] if emb else None
    out["pass_s"] = round(ready_t - emb[0]["t"], 2) if emb and ready_t else None

    pid = out["daemonPid"]
    if pid:
        r = subprocess.run(["ps", "-o", "cputime=", "-p", str(pid)], capture_output=True, text=True)
        out["daemonCpuTime"] = r.stdout.strip()
    try:
        shim.p.stdin.close()
    except OSError:
        pass
    shim.p.terminate()
    subprocess.run([a.bin, "stop"], cwd=a.project, env=env, capture_output=True, timeout=60)
    if pid:
        time.sleep(1)
        try:
            os.kill(pid, 0)
            os.kill(pid, signal.SIGKILL)
            out["daemonKilled"] = True
        except ProcessLookupError:
            out["daemonKilled"] = False
    Path(a.out).write_text(json.dumps(out, indent=1))
    print(json.dumps({k: out[k] for k in ("ready_t", "pass_s", "tiers", "daemonPid")}))
    sys.exit(0 if ready_t is not None else 1)


if __name__ == "__main__":
    main()
