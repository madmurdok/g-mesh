#!/usr/bin/env python3
"""GM-447 S6: a stdio pass-through between Claude Code and `g-mesh mcp-shim`.

Logs every newline-delimited JSON-RPC frame in both directions, with a
timestamp, to a JSONL file, so the run-set script can tell from the wire
whether a call was answered by another agent's project.
Usage: tap.py <log.jsonl> <g-mesh binary> [args...]
"""
import json
import subprocess
import sys
import threading
import time

log_path, cmd = sys.argv[1], sys.argv[2:]
log = open(log_path, "a", buffering=1)
lock = threading.Lock()
child = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
with lock:
    log.write(json.dumps({"t": time.time(), "dir": "meta", "shim_pid": child.pid}) + "\n")


def record(direction, raw):
    try:
        frame = json.loads(raw)
    except ValueError:
        frame = {"unparsed": raw.decode("utf-8", "replace")}
    with lock:
        log.write(json.dumps({"t": time.time(), "dir": direction, "frame": frame}) + "\n")


def discover_probe_id(line):
    """The JSON-RPC id of a `server/discover` request, else None.

    Claude Code 2.1.286 probes a fresh stdio server with `server/discover`
    before `initialize`. g-mesh's front daemon rejects any request before
    `initialize` and closes, the shim answers -32603, and Claude Code then
    marks the server failed. -32601 is the answer an older server gives, so
    the tap gives it and the client falls back to `initialize`.
    """
    try:
        frame = json.loads(line)
    except ValueError:
        return None
    if isinstance(frame, dict) and frame.get("method") == "server/discover" and "id" in frame:
        return frame["id"]
    return None


def pump_in():
    for line in sys.stdin.buffer:
        record("c2s", line)
        probe = discover_probe_id(line)
        if probe is not None:
            reply = json.dumps({"jsonrpc": "2.0", "id": probe,
                                "error": {"code": -32601, "message": "Method not found"}}) + "\n"
            with lock:
                log.write(json.dumps({"t": time.time(), "dir": "tap-s2c", "frame": json.loads(reply)}) + "\n")
                sys.stdout.buffer.write(reply.encode())
                sys.stdout.buffer.flush()
            continue
        try:
            child.stdin.write(line)
            child.stdin.flush()
        except BrokenPipeError:
            break
    try:
        child.stdin.close()
    except BrokenPipeError:
        pass


threading.Thread(target=pump_in, daemon=True).start()
for line in child.stdout:
    record("s2c", line)
    with lock:
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()
sys.exit(child.wait())
