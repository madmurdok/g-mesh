#!/usr/bin/env python3
"""One MCP round trip through an installed g-mesh, standard library only.

Usage: mcp-smoke.py <g-mesh-exe> <project-dir>

Spawns `<g-mesh-exe> mcp-shim` with <project-dir> as its working directory
and speaks newline-delimited JSON-RPC over its stdio, the way an MCP client
does: `initialize`, `notifications/initialized`, `tools/list`, then one
`find_callers` for `add` (one call per declaration when the name is
ambiguous). <project-dir> is expected to be a
copy of scripts/release-smoke-fixture (README.md removed) that the same
binary has already indexed under the same G_MESH_HOME. Each of its four
files defines `add` and a `double` that calls it, so the answer must name
`double` as a caller in hello.go, hello.py, lib.rs and hello.ts: one CALLS
edge per bundled language, read back through the daemon.

Afterwards it closes the shim's stdin, waits for the shim to exit and runs
`<g-mesh-exe> stop` in <project-dir>, so no daemon outlives the caller and
keeps files open under the install or G_MESH_HOME.

The whole exchange has one deadline (DEADLINE_SECS): the first call starts
the daemon and waits for it to load the index. On a timeout, a missing tool,
an error result or a missing caller it prints every frame it received and
the shim's stderr, kills the shim and exits 1. Exit 0 means every check
passed.

CLAUDE_PROJECT_DIR is removed from the shim's environment so the project is
the working directory and nothing else.
"""

import json
import os
import queue
import subprocess
import sys
import threading
import time

DEADLINE_SECS = 180
PROTOCOL_VERSION = "2025-03-26"
REQUIRED_TOOLS = ("find_callers", "find_definition")
# fixture file -> language, for the PASS line and failure messages.
EXPECTED_FILES = {
    "hello.go": "go",
    "hello.py": "python",
    "lib.rs": "rust",
    "hello.ts": "typescript",
}
CALLER = "double"
CALLEE = "add"


class SmokeFailure(Exception):
    pass


class Shim:
    def __init__(self, exe, project):
        env = dict(os.environ)
        env.pop("CLAUDE_PROJECT_DIR", None)
        self.proc = subprocess.Popen(
            [exe, "mcp-shim"],
            cwd=project,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.deadline = time.monotonic() + DEADLINE_SECS
        self.frames = []
        self.stderr = []
        self.lines = queue.Queue()
        self.next_id = 1
        # Daemon threads: a detached daemon process may inherit a pipe end
        # and keep it open after the shim exits; these must not block exit.
        threading.Thread(target=self._pump_stdout, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()

    def _pump_stdout(self):
        for raw in iter(self.proc.stdout.readline, b""):
            self.lines.put(raw)
        self.lines.put(None)

    def _pump_stderr(self):
        for raw in iter(self.proc.stderr.readline, b""):
            self.stderr.append(raw.decode("utf-8", "replace").rstrip("\r\n"))

    def send(self, message):
        data = (json.dumps(message) + "\n").encode("utf-8")
        try:
            self.proc.stdin.write(data)
            self.proc.stdin.flush()
        except OSError as err:
            raise SmokeFailure("could not write to the shim's stdin: %s" % err)

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            message = self._read()
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise SmokeFailure("%s answered with a JSON-RPC error: %s" % (method, json.dumps(message["error"])))
                return message.get("result")
            if "method" in message and "id" in message:
                # A request from the server (ping, or anything this client
                # did not advertise): answer it so the server is not left
                # waiting, then keep reading for our own response.
                if message["method"] == "ping":
                    self.send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
                else:
                    self.send(
                        {
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "error": {"code": -32601, "message": "not supported by mcp-smoke.py"},
                        }
                    )
            # Notifications (progress, logging) are recorded and skipped.

    def _read(self):
        while True:
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeFailure("no answer within %d s" % DEADLINE_SECS)
            try:
                raw = self.lines.get(timeout=remaining)
            except queue.Empty:
                raise SmokeFailure("no answer within %d s" % DEADLINE_SECS)
            if raw is None:
                raise SmokeFailure("the shim closed its stdout (exit status %s)" % self.proc.poll())
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            self.frames.append(line)
            try:
                message = json.loads(line)
            except ValueError:
                raise SmokeFailure("the shim wrote a line that is not JSON: %r" % line)
            if isinstance(message, dict):
                return message

    def close(self):
        """Closes stdin and waits for the shim; True if it exited on its own."""
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=30)
            return True
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
            return False

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()

    def dump(self):
        print("--- frames received from the shim (%d) ---" % len(self.frames))
        for frame in self.frames:
            print(frame)
        # Give the stderr pump a moment to drain what the shim wrote last.
        time.sleep(0.5)
        print("--- shim stderr (%d lines) ---" % len(self.stderr))
        for line in self.stderr:
            print(line)
        print("--- end ---")


def result_text(result):
    """The concatenated text content of a tools/call result."""
    parts = []
    for item in (result or {}).get("content") or []:
        if isinstance(item, dict) and item.get("type") == "text":
            parts.append(item.get("text", ""))
    return "\n".join(parts)


def result_rows(result):
    """The `results` rows of a tools/call result whose text is g-mesh's JSON."""
    text = result_text(result)
    try:
        parsed = json.loads(text)
    except ValueError:
        raise SmokeFailure("tool answer is not JSON:\n%s" % text)
    rows = parsed.get("results") if isinstance(parsed, dict) else None
    if not isinstance(rows, list):
        raise SmokeFailure("tool answer has no `results` list:\n%s" % text)
    return parsed, [row for row in rows if isinstance(row, dict)]


def expected_file(row):
    """The fixture file name a row's filePath ends in, or None."""
    path = str(row.get("filePath", "")).replace("\\", "/")
    for name in EXPECTED_FILES:
        if path == name or path.endswith("/" + name):
            return name
    return None


def call_tool(shim, name, arguments):
    result = shim.request("tools/call", {"name": name, "arguments": arguments})
    if (result or {}).get("isError"):
        raise SmokeFailure("%s %s returned isError: %s" % (name, json.dumps(arguments), result_text(result)))
    return result


def callers_by_file(shim):
    """Fixture file -> the caller names find_callers reports for `add` there.

    `add` is declared in all four files, so the name alone is ambiguous: the
    server answers with one candidate per declaration, and each candidate is
    then asked by its `id`. A server that resolves the name directly instead
    is accepted too.
    """
    parsed, rows = result_rows(call_tool(shim, "find_callers", {"symbol_name": CALLEE}))
    found = {name: set() for name in EXPECTED_FILES}
    if not parsed.get("ambiguous"):
        for row in rows:
            name = expected_file(row)
            if name:
                found[name].add(str(row.get("qualifiedName") or row.get("name")))
        return found
    candidates = {}
    for row in rows:
        name = expected_file(row)
        if name and row.get("id"):
            candidates[name] = row["id"]
    absent = [name for name in EXPECTED_FILES if name not in candidates]
    if absent:
        raise SmokeFailure(
            "find_callers %s lists no declaration of %s in %s (candidates: %s)"
            % (CALLEE, CALLEE, ", ".join(absent), json.dumps(rows))
        )
    print("==> mcp-smoke: find_callers %s is ambiguous across %d declarations; asking each by id" % (CALLEE, len(rows)))
    for name, symbol_id in candidates.items():
        _, caller_rows = result_rows(call_tool(shim, "find_callers", {"symbol_id": symbol_id}))
        for row in caller_rows:
            if expected_file(row) == name:
                found[name].add(str(row.get("qualifiedName") or row.get("name")))
    return found


def run(exe, project):
    shim = Shim(exe, project)
    try:
        init = shim.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "g-mesh-mcp-smoke", "version": "1"},
            },
        )
        server = (init or {}).get("serverInfo", {})
        print("==> mcp-smoke: initialize answered by %s %s" % (server.get("name"), server.get("version")))
        shim.notify("notifications/initialized")

        tools = shim.request("tools/list", {})
        names = {tool.get("name") for tool in (tools or {}).get("tools") or []}
        absent = [name for name in REQUIRED_TOOLS if name not in names]
        if absent:
            raise SmokeFailure("tools/list lacks %s (has: %s)" % (", ".join(absent), ", ".join(sorted(n for n in names if n))))
        print("==> mcp-smoke: tools/list has %d tools, including %s" % (len(names), ", ".join(REQUIRED_TOOLS)))

        found = callers_by_file(shim)
        missing = [name for name, callers in found.items() if not any(c.split("::")[-1].split(".")[-1] == CALLER for c in callers)]
        if missing:
            raise SmokeFailure(
                "find_callers %s does not name %s as a caller in %s (callers seen: %s)"
                % (
                    CALLEE,
                    CALLER,
                    ", ".join("%s (%s)" % (m, EXPECTED_FILES[m]) for m in missing),
                    json.dumps({k: sorted(v) for k, v in found.items()}),
                )
            )
        print("==> mcp-smoke: find_callers %s names %s in %s" % (CALLEE, CALLER, ", ".join(EXPECTED_FILES)))
    except SmokeFailure:
        shim.kill()
        shim.dump()
        raise
    if not shim.close():
        shim.dump()
        raise SmokeFailure("the shim did not exit within 30 s of its stdin closing")


def stop_daemon(exe, project):
    env = dict(os.environ)
    env.pop("CLAUDE_PROJECT_DIR", None)
    try:
        done = subprocess.run(
            [exe, "stop"], cwd=project, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=60
        )
    except subprocess.TimeoutExpired:
        raise SmokeFailure("'g-mesh stop' did not finish within 60 s")
    output = done.stdout.decode("utf-8", "replace").strip()
    print("==> mcp-smoke: g-mesh stop: %s" % (output or "(no output)"))
    if done.returncode != 0:
        raise SmokeFailure("'g-mesh stop' exited %d" % done.returncode)


def main(argv):
    if len(argv) != 3:
        print("usage: mcp-smoke.py <g-mesh-exe> <project-dir>", file=sys.stderr)
        return 2
    exe, project = os.path.abspath(argv[1]), os.path.abspath(argv[2])
    if not os.path.isfile(exe):
        print("mcp-smoke: not a file: %s" % exe, file=sys.stderr)
        return 2
    if not os.path.isdir(project):
        print("mcp-smoke: not a directory: %s" % project, file=sys.stderr)
        return 2
    failure = None
    try:
        run(exe, project)
    except SmokeFailure as err:
        failure = err
    # Stopped on success and on failure alike: a daemon left running holds
    # files the caller is about to delete.
    try:
        stop_daemon(exe, project)
    except SmokeFailure as err:
        failure = failure or err
    if failure:
        print("mcp-smoke: FAIL: %s" % failure, file=sys.stderr)
        return 1
    print(
        "PASS: g-mesh answered find_callers over MCP with a caller in every language (%s)"
        % ", ".join(sorted(EXPECTED_FILES.values()))
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
