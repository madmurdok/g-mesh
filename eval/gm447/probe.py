#!/usr/bin/env python3
"""GM-447 S6: drive one g-mesh shim over stdio without an LLM.

Checks the fixture routes as expected: select p1, look up retry_limit,
select p2, look up retry_limit again. Prints each answer's text items.
Usage: probe.py <g-mesh binary> <fixture dir> <G_MESH_HOME>
"""
import json
import os
import subprocess
import sys

binary, fixture, home = sys.argv[1:4]
env = dict(os.environ, G_MESH_HOME=home)
proc = subprocess.Popen([binary, "mcp-shim"], cwd=fixture, env=env,
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
print("shim pid", proc.pid, flush=True)
next_id = 0


def call(method, params=None, notify=False):
    global next_id
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        msg["params"] = params
    if not notify:
        next_id += 1
        msg["id"] = next_id
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    if notify:
        return None
    while True:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit("shim closed stdout")
        frame = json.loads(line)
        if frame.get("id") == msg["id"]:
            return frame


def tool(name, args):
    frame = call("tools/call", {"name": name, "arguments": args})
    texts = [c.get("text", "") for c in frame.get("result", {}).get("content", [])]
    print(f"--- {name} {args} isError={frame.get('result', {}).get('isError')}")
    for t in texts:
        print("   ", t[:600].replace("\n", "\n    "))


call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "gm447-probe", "version": "0"}})
call("notifications/initialized", notify=True)
tools = call("tools/list", {})["result"]["tools"]
for t in tools:
    if t["name"] in ("select_project", "find_definition"):
        print(t["name"], json.dumps(t["inputSchema"].get("properties"))[:300])
tool("select_project", {"project": "p1"})
tool("find_definition", {"symbol_name": "retry_limit", "include_source": True})
tool("select_project", {"project": "p2"})
tool("find_definition", {"symbol_name": "retry_limit", "include_source": True})
proc.stdin.close()
proc.wait(timeout=30)
