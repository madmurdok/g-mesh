#!/usr/bin/env python3
"""GM-473 S1: how long each MCP tool handler holds the index store lock, and
how long a concurrent call on another session waits for it (or for a worker).

Needs a g-mesh binary built with the throwaway lock-log instrumentation (not
committed; see docs/results/gm-473-store-lock-hold.md, "Instrumentation"):
with G_MESH_LOCKLOG=<file> set, every IndexStore hold appends

    HOLD <t_acq_unix_us> <wait_us> <hold_us> <caller file:line> <thread>
    EMBED <t_end_unix_us> 0 <embed_us> find_definition::by_semantic_neighbours <thread>

`read()` is #[track_caller], so a handler's hold is attributed to its own
`store.read()` line; holds whose site is outside core/src/mcp/ are writers
(indexing, embedding backfill, semantic pass).

Drives the real product path: `g-mesh mcp-shim` over stdio (one shim = one
session), which bootstraps the per-project daemon. Per corpus:

  1. index: session A outlines a file, waits for index.phase == ready, then for
     the store to go quiet (no writer hold for QUIET_S seconds).
  2. restart: stop the daemon, start a fresh one (so the embedding model is not
     loaded), wait ready + quiet again.
  3. cold: probes B (get_file_outline of a small file, every PACE_MS) and C
     (tools/list, no store access: a pure worker-availability probe) run while
     A sends the daemon's first find_definition that falls to the semantic
     rung (model load + inference under the store lock).
  4. solo arms, one per handler: A alone, N calls, each call's summed hold.
  5. baseline: B and C alone. Then contention arms: A loops one handler while
     B and C probe; B's lock wait (log) and B/C latency over baseline.

Each arm in 3-5 is gated: 1-min load < GATE_LOAD held GATE_HOLD s, total
waiting across the run capped at GATE_CAP s; past the cap the arm runs anyway
and is marked ungated.

Modes:
  gm473_lock_hold.py run          every corpus (CORPORA), each under
                                  /usr/bin/time -p, then OUT/summary.md
  gm473_lock_hold.py corpus NAME  one corpus (internal; writes OUT/NAME.json)

Env: OUT (results dir), HBASE (short dir: one G_MESH_HOME per corpus under
it), BIN (dir holding the instrumented g-mesh + plugin binaries), PLUGINS
(plugin roots, with plugins/typescript built), CORPORA_DIR, CORPORA (default
"g-mesh excalidraw"), N (calls per arm, default 40), HANDLERS (subset),
DRY=1 (gate off, N=3), GATE_LOAD (4), GATE_HOLD (30), GATE_CAP (3600),
QUIET_S (30), QUIET_CAP (1800), PACE_MS (20), PROGRESS (progress file).
"""

import json
import os
import random
import re
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = Path(os.environ.get("OUT", "/private/tmp/claude-502/gm473/out"))
HBASE = Path(os.environ.get("HBASE", "/private/tmp/claude-502/gm473"))
BIN = Path(os.environ.get("BIN", "/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm473-instr/target/release"))
PLUGINS = Path(os.environ.get("PLUGINS", "/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm473-instr/plugins"))
CORPORA_DIR = Path(os.environ.get(
    "CORPORA_DIR", "/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh/eval/embedding/work/corpora"))
QUERIES_DIR = HERE / "embedding" / "queries"
DRY = os.environ.get("DRY") == "1"
CORPORA = os.environ.get("CORPORA", "g-mesh" if DRY else "g-mesh excalidraw").split()
N = int(os.environ.get("N", "3" if DRY else "40"))
ALL_HANDLERS = ["get_file_outline", "get_dependencies", "find_definition", "find_references",
                "find_callers", "find_callees", "find_implementations", "find_definition_semantic"]
HANDLERS = os.environ.get("HANDLERS", "find_references find_definition_semantic" if DRY else " ".join(ALL_HANDLERS)).split()
GATE = not DRY and os.environ.get("GATE", "1") == "1"
GATE_LOAD = float(os.environ.get("GATE_LOAD", "4"))
GATE_HOLD = int(os.environ.get("GATE_HOLD", "30"))
GATE_CAP = int(os.environ.get("GATE_CAP", "3600"))
QUIET_S = int(os.environ.get("QUIET_S", "30"))
QUIET_CAP = int(os.environ.get("QUIET_CAP", "1800"))
PACE = int(os.environ.get("PACE_MS", "20")) / 1000
PROGRESS = Path(os.environ.get("PROGRESS", str(Path.home() / ".claude" / "progress" / "GM-473-S1.txt")))
WAITED_FILE = OUT / "gate_waited_s"
T0 = float(os.environ.get("RUN_T0", time.time()))
EXTS = {"rust": (".rs",), "typescript": (".ts", ".tsx")}
LANG = {"g-mesh": "rust", "excalidraw": "typescript", "ripgrep": "rust"}
SITE_HANDLER = {  # caller file of store.read() -> handler
    "get_file_outline.rs": "get_file_outline", "get_dependencies.rs": "get_dependencies",
    "find_definition.rs": "find_definition", "find_references.rs": "find_references",
    "find_implementations.rs": "find_implementations", "search_code.rs": "search_code",
}

STAGE = {"name": "start", "done": 0, "total": 1, "last": 0.0}


def load1():
    return os.getloadavg()[0]


def uptime():
    return subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()


def progress(stage=None, done=None, total=None, force=False):
    if stage is not None:
        STAGE.update(name=stage, done=done or 0, total=total or 1)
    if done is not None:
        STAGE["done"] = done
    now = time.time()
    total = STAGE["total"]
    step = max(1, total // 10)
    if not force and STAGE["done"] % step and now - STAGE["last"] < 300:
        return
    STAGE["last"] = now
    el = now - T0
    pct = 100.0 * STAGE["done"] / total if total else 0
    rate = el / max(STAGE["done"], 1)
    line = (f"{STAGE['name']} {STAGE['done']}/{total} {pct:.0f}% elapsed {el:.0f}s "
            f"eta(stage) {rate * (total - STAGE['done']):.0f}s load {load1():.2f}\n")
    try:
        PROGRESS.parent.mkdir(parents=True, exist_ok=True)
        tmp = PROGRESS.with_suffix(".tmp")
        tmp.write_text(line)
        tmp.replace(PROGRESS)
    except OSError:
        pass


def now_us():
    return int(time.time() * 1e6)


class Shim:
    def __init__(self, cwd, env, name):
        self.p = subprocess.Popen([str(BIN / "g-mesh"), "mcp-shim"], cwd=cwd, env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, bufsize=1)
        self.next_id = 0
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": f"gm473-{name}", "version": "0"}})
        self.notify("notifications/initialized")

    def request(self, method, params):
        self.next_id += 1
        rid = self.next_id
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError(f"shim closed while waiting for {method}")
            msg = json.loads(line)
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                return msg

    def notify(self, method, params=None):
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}) + "\n")
        self.p.stdin.flush()

    def call(self, name, args):
        return self.request("tools/call", {"name": name, "arguments": args})

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:
            self.p.kill()


def text_of(resp):
    res = resp.get("result", {})
    if "error" in resp or res.get("isError"):
        return None
    return "".join(c.get("text", "") for c in res.get("content", []))


# ---- lock log ---------------------------------------------------------------

class LockLog:
    def __init__(self, path):
        self.path = path
        self.off = 0
        self.rows = []  # (kind, t_us, wait_us, dur_us, site, thread)

    def poll(self):
        if not self.path.exists():
            return
        with self.path.open() as f:
            f.seek(self.off)
            data = f.read()
            # keep a partial last line for the next poll
            cut = data.rfind("\n") + 1
            self.off += len(data[:cut].encode())
        for line in data[:cut].splitlines():
            p = line.split()
            if len(p) >= 5 and p[0] in ("HOLD", "EMBED"):
                self.rows.append((p[0], int(p[1]), int(p[2]), int(p[3]), p[4], p[5] if len(p) > 5 else "?"))

    def holds(self, t0, t1, mcp=None):
        out = []
        for r in self.rows:
            if r[0] != "HOLD" or not (t0 <= r[1] <= t1):
                continue
            if mcp is None or mcp == is_request(r[4]):
                out.append(r)
        return out

    def last_writer_us(self):
        ws = [r[1] + r[3] for r in self.rows if r[0] == "HOLD" and not is_request(r[4])]
        return max(ws) if ws else 0


def is_request(site):
    """A hold taken on a tool call's own path: the handler's read(), mark_used
    (mcp/mod.rs) and ensure_fresh's staleness check (daemon/registry.rs:1207,
    reached from get_file_outline, find_definition and get_dependencies)."""
    return "/mcp/" in site or site.endswith("daemon/registry.rs:1207")


def handler_of(site):
    f = site.split("/")[-1].split(":")[0]
    if f == "find_callers_callees.rs":
        line = int(site.split(":")[-1])
        return "find_callers" if line < 400 else "find_callees"
    return SITE_HANDLER.get(f, f)


# ---- daemon lifecycle -------------------------------------------------------

def state_dirs(h):
    p = h / "projects"
    return [d for d in p.glob("*") if d.is_dir()] if p.exists() else []


def phase(h):
    for d in state_dirs(h):
        f = d / "index.phase"
        if f.exists():
            return f.read_text().strip()
    return None


def read_pid(f):
    try:
        return int(f.read_text().split()[0])
    except (OSError, ValueError, IndexError):
        return None


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def pids(h):
    out = []
    for d in state_dirs(h):
        for f in [d / "daemon.pid"] + sorted(d.glob("plugin-*.pid")):
            p = read_pid(f)
            if p:
                out.append(p)
    return out


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    secs = 0.0
    for part in out.replace("-", ":").split(":"):
        secs = secs * 60 + float(part)
    return secs


def stop_daemon(h, cwd, env):
    """`g-mesh stop`, then SIGKILL only the recorded pids that survived."""
    ps = pids(h)
    subprocess.run([str(BIN / "g-mesh"), "stop"], cwd=cwd, env=env, capture_output=True, timeout=60)
    deadline = time.time() + 15
    while time.time() < deadline and any(alive(p) for p in ps):
        time.sleep(0.5)
    killed = [p for p in ps if alive(p)]
    for p in killed:
        os.kill(p, signal.SIGKILL)
    return ps, killed


def env_for(h, log):
    env = dict(os.environ)
    env["G_MESH_HOME"] = str(h)
    env["G_MESH_PLUGIN_ROOTS_OVERRIDE"] = str(PLUGINS)
    env["G_MESH_LOCKLOG"] = str(log)
    return env


def prepare_home(h):
    h.mkdir(parents=True, exist_ok=True)
    src = Path.home() / ".g-mesh"
    m = h / "models"
    if not m.exists():
        m.symlink_to(src / "models")
    c = h / "embedding-cache"
    if not c.exists() and (src / "embedding-cache").exists():
        subprocess.run(["cp", "-R", str(src / "embedding-cache"), str(c)], check=True)


def wait_ready(shim, h, log, probe_file, label):
    t = time.time()
    while True:
        r = shim.call("get_file_outline", {"file_path": probe_file})
        if text_of(r) is not None:
            break
        if time.time() - t > 3600:
            raise RuntimeError("structural index never answered")
        progress(f"{label} walking", 0, 1, force=True)
        time.sleep(5)
    while phase(h) != "ready":
        if time.time() - t > 3600:
            raise RuntimeError(f"index never ready (phase {phase(h)})")
        progress(f"{label} indexing phase={phase(h)}", 0, 1)
        time.sleep(3)
    ready_s = time.time() - t
    tq = time.time()
    while True:
        log.poll()
        quiet = (now_us() - log.last_writer_us()) / 1e6
        if quiet >= QUIET_S:
            status = "quiet"
            break
        if time.time() - tq > QUIET_CAP:
            status = "writers still active (cap)"
            break
        progress(f"{label} waiting for writers to stop ({quiet:.0f}s quiet)", 0, 1)
        time.sleep(3)
    return {"ready_s": round(ready_s, 1), "quiet_wait_s": round(time.time() - tq, 1), "quiet": status}


def gate():
    waited = float(WAITED_FILE.read_text()) if WAITED_FILE.exists() else 0.0
    rec = {"load_before_gate": round(load1(), 2)}
    if not GATE:
        rec.update(gate="off", waited_s=0)
        return rec
    held, start = 0, time.time()
    while True:
        if load1() < GATE_LOAD:
            held += 5
            if held >= GATE_HOLD:
                status = "open"
                break
        else:
            held = 0
        spent = waited + (time.time() - start)
        if spent >= GATE_CAP:
            status = "closed"
            break
        progress(f"gate-wait load<{GATE_LOAD} held {held}s waited {spent:.0f}/{GATE_CAP}s", 0, 1)
        time.sleep(5)
    mine = time.time() - start
    WAITED_FILE.write_text(f"{waited + mine:.0f}")
    rec.update(gate=status, waited_s=round(mine))
    return rec


# ---- queries ----------------------------------------------------------------

def collect_symbols(obj, out):
    if isinstance(obj, dict):
        if isinstance(obj.get("name"), str) and isinstance(obj.get("kind"), str):
            out.append((obj["name"], obj["kind"]))
        for v in obj.values():
            collect_symbols(v, out)
    elif isinstance(obj, list):
        for v in obj:
            collect_symbols(v, out)


def camel(text):
    stop = {"a", "an", "the", "of", "for", "to", "in", "on", "and", "or", "by", "with", "its", "is", "that", "from"}
    words = [w for w in re.findall(r"[A-Za-z]+", text) if w.lower() not in stop][:4]
    return words[0].lower() + "".join(w.capitalize() for w in words[1:]) if words else "unknownThing"


def build_queries(shim, corpus, root):
    rnd = random.Random(473)
    files = sorted(str(p.relative_to(root)) for p in root.rglob("*")
                   if p.suffix in EXTS[LANG[corpus]] and p.is_file()
                   and not any(x in p.parts for x in ("node_modules", "target", "dist", ".git")))
    rnd.shuffle(files)
    files = files[: max(N, 40)]
    sizes, syms = {}, []
    for f in files:
        t = text_of(shim.call("get_file_outline", {"file_path": f, "limit": 200}))
        if t is None:
            continue
        s = []
        try:
            collect_symbols(json.loads(t), s)
        except json.JSONDecodeError:
            pass
        sizes[f] = len(s)
        syms += s
    files = [f for f in files if sizes.get(f)]
    probe = min(files, key=lambda f: sizes[f])
    names = sorted({n for n, _ in syms if len(n) > 2})
    rnd.shuffle(names)
    fn_names = sorted({n for n, k in syms if k in ("Function", "Method") and len(n) > 2})
    rnd.shuffle(fn_names)
    type_names = sorted({n for n, k in syms if k not in ("Function", "Method", "Variable", "Module", "Field",
                                                         "Property", "Constant", "Import") and len(n) > 2})
    rnd.shuffle(type_names)
    rows = [json.loads(line) for line in (QUERIES_DIR / f"{corpus}.jsonl").open()]
    misses = [camel(r["text"]) for r in rows if r["shape"] == "phrase"]
    rnd.shuffle(misses)
    cycle = lambda xs: (xs * (N // max(len(xs), 1) + 1))[:N] if xs else []
    q = {
        "get_file_outline": [{"file_path": f, "limit": 200} for f in cycle(files)],
        "get_dependencies": [{"file_path": f, "direction": d} for f, d in
                             zip(cycle(files), ["Incoming", "Outgoing"] * N)],
        "find_definition": [{"symbol_name": n} for n in cycle(names)],
        "find_references": [{"symbol_name": n, "limit": 200} for n in cycle(names)],
        "find_callers": [{"symbol_name": n, "limit": 200} for n in cycle(fn_names)],
        "find_callees": [{"symbol_name": n, "limit": 200} for n in cycle(fn_names)],
        "find_implementations": [{"symbol_name": n, "limit": 200} for n in cycle(type_names or names)],
        "find_definition_semantic": [{"symbol_name": n} for n in cycle(misses)],
    }
    kinds = {}
    for _, k in syms:
        kinds[k] = kinds.get(k, 0) + 1
    return q, probe, {"files": len(files), "names": len(names), "fn": len(fn_names),
                      "types": len(type_names), "misses": len(misses), "kinds": kinds}


def tool_of(h):
    return "find_definition" if h == "find_definition_semantic" else h


# ---- probes -----------------------------------------------------------------

class Probe(threading.Thread):
    def __init__(self, shim, kind, probe_file):
        super().__init__(daemon=True)
        self.shim, self.kind, self.probe_file = shim, kind, probe_file
        self.stop = threading.Event()
        self.samples = []  # (t_start_us, t_end_us)
        self.err = None

    def run(self):
        try:
            while not self.stop.is_set():
                t0 = now_us()
                if self.kind == "B":
                    self.shim.call("get_file_outline", {"file_path": self.probe_file})
                else:
                    self.shim.request("tools/list", {})
                self.samples.append((t0, now_us()))
                time.sleep(PACE)
        except Exception as e:  # noqa: BLE001
            self.err = repr(e)


def lat_ms(samples, t0=None, t1=None):
    return [(b - a) / 1000 for a, b in samples if (t0 is None or a >= t0) and (t1 is None or a <= t1)]


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return round(xs[lo] + (xs[hi] - xs[lo]) * (k - lo), 2)


def stats(xs):
    return {"n": len(xs), "p50": pct(xs, 0.5), "p95": pct(xs, 0.95), "max": round(max(xs), 2) if xs else None}


# ---- arms -------------------------------------------------------------------

def timed_calls(shim, log, handler, qs, label):
    calls = []
    progress(label, 0, len(qs), force=True)
    for i, a in enumerate(qs):
        t0 = now_us()
        r = shim.call(tool_of(handler), a)
        t1 = now_us()
        t = text_of(r)
        calls.append({"t0": t0, "t1": t1, "ok": t is not None,
                      "semantic": bool(t and "semanticNeighbours" in t)})
        progress(None, i + 1)
    time.sleep(0.2)
    log.poll()
    for c in calls:
        hs = log.holds(c["t0"], c["t1"], mcp=True)
        own = [h for h in hs if handler_of(h[4]) == tool_of(handler)]
        c["hold_ms"] = sum(h[3] for h in own) / 1000  # the handler's own store.read() guard(s)
        c["total_hold_ms"] = sum(h[3] for h in hs) / 1000  # + mark_used, ensure_fresh
        c["max_hold_ms"] = max((h[3] for h in hs), default=0) / 1000
        c["n_holds"] = len(hs)
        c["sites"] = sorted({handler_of(h[4]) for h in hs})
        c["embed_ms"] = [r[3] / 1000 for r in log.rows if r[0] == "EMBED" and c["t0"] <= r[1] <= c["t1"]]
    return calls


def arm_meta(log, t0, t1):
    ws = log.holds(t0, t1, mcp=False)
    return {"writer_holds": len(ws), "writer_hold_ms_sum": round(sum(w[3] for w in ws) / 1000, 1),
            "load_end": round(load1(), 2)}


def solo_arm(shim, log, handler, qs):
    g = gate()
    g["uptime"] = uptime()
    t0 = now_us()
    calls = timed_calls(shim, log, handler, qs, f"solo {handler}")
    t1 = now_us()
    rec = {"arm": "solo", "handler": handler, **g, **arm_meta(log, t0, t1)}
    rec["hold_ms"] = stats([c["hold_ms"] for c in calls])
    rec["total_hold_ms"] = stats([c["total_hold_ms"] for c in calls])
    rec["latency_ms"] = stats(lat_ms([(c["t0"], c["t1"]) for c in calls]))
    rec["ok"] = sum(c["ok"] for c in calls)
    rec["semantic_hits"] = sum(c["semantic"] for c in calls)
    rec["embed_ms"] = stats([e for c in calls for e in c["embed_ms"]])
    rec["sites"] = sorted({s for c in calls for s in c["sites"]})
    rec["calls"] = calls
    return rec


def probe_window(shim_b, shim_c, probe_file, body):
    b = Probe(shim_b, "B", probe_file)
    c = Probe(shim_c, "C", probe_file)
    b.start()
    c.start()
    time.sleep(0.5)
    t0 = now_us()
    try:
        out = body()
    finally:
        t1 = now_us()
        b.stop.set()
        c.stop.set()
        b.join(30)
        c.join(30)
    return out, t0, t1, b, c


def probe_stats(log, b, c, t0, t1, base):
    log.poll()
    bl = lat_ms(b.samples, t0, t1)
    cl = lat_ms(c.samples, t0, t1)
    bwait = [h[2] / 1000 for h in log.rows if h[0] == "HOLD" and is_request(h[4])
             and any(s <= h[1] - h[2] <= e for s, e in b.samples if s >= t0 - 1_000_000 and s <= t1)
             and handler_of(h[4]) == "get_file_outline"]
    rec = {"B_latency_ms": stats(bl), "C_latency_ms": stats(cl), "B_lock_wait_ms": stats(bwait),
           "probe_err": [x for x in (b.err, c.err) if x]}
    if base:
        rec["B_excess_ms"] = stats([x - base["B_p50"] for x in bl])
        rec["C_excess_ms"] = stats([x - base["C_p50"] for x in cl])
    return rec


def baseline_arm(shim_b, shim_c, log, probe_file, secs):
    g = gate()
    _, t0, t1, b, c = probe_window(shim_b, shim_c, probe_file, lambda: time.sleep(secs))
    rec = {"arm": "baseline", "handler": "-", **g, **arm_meta(log, t0, t1),
           **probe_stats(log, b, c, t0, t1, None)}
    rec["B_p50"] = rec["B_latency_ms"]["p50"] or 0
    rec["C_p50"] = rec["C_latency_ms"]["p50"] or 0
    rec["uptime"] = uptime()
    return rec


def contention_arm(shim_a, shim_b, shim_c, log, handler, qs, probe_file, base):
    g = gate()
    g["uptime"] = uptime()
    calls, t0, t1, b, c = probe_window(
        shim_b, shim_c, probe_file, lambda: timed_calls(shim_a, log, handler, qs, f"contention {handler}"))
    rec = {"arm": "contention", "handler": handler, **g, **arm_meta(log, t0, t1),
           **probe_stats(log, b, c, t0, t1, base)}
    rec["A_hold_ms"] = stats([x["hold_ms"] for x in calls])
    rec["A_latency_ms"] = stats(lat_ms([(x["t0"], x["t1"]) for x in calls]))
    rec["B_wait_attributable"] = handler != "get_file_outline"
    return rec


def cold_arm(shim_a, shim_b, shim_c, log, q, probe_file):
    g = gate()
    g["uptime"] = uptime()
    log.poll()
    embeds_before = sum(1 for r in log.rows if r[0] == "EMBED")

    def body():
        t0 = now_us()
        r = shim_a.call("find_definition", q)
        return t0, now_us(), text_of(r)

    (a0, a1, t), w0, w1, b, c = probe_window(shim_b, shim_c, probe_file, body)
    time.sleep(0.2)
    log.poll()
    holds = log.holds(a0, a1, mcp=True)
    a_holds = [h for h in holds if handler_of(h[4]) == "find_definition"]
    embeds = [r for r in log.rows if r[0] == "EMBED"]
    rec = {"arm": "cold", "handler": "find_definition_semantic", **g, **arm_meta(log, w0, w1),
           "query": q, "semantic": bool(t and "semanticNeighbours" in t),
           "embeds_before": embeds_before,
           "A_latency_ms": round((a1 - a0) / 1000, 1),
           "A_hold_ms": round(sum(h[3] for h in a_holds) / 1000, 1),
           "first_embed_ms": round(embeds[embeds_before][3] / 1000, 1) if len(embeds) > embeds_before else None,
           **probe_stats(log, b, c, a0, a1, None)}
    # B calls that were in flight across A's call, not only those that started in it
    rec["B_max_overlapping_ms"] = max(((e - s) / 1000 for s, e in b.samples if e >= a0 and s <= a1), default=None)
    rec["C_max_overlapping_ms"] = max(((e - s) / 1000 for s, e in c.samples if e >= a0 and s <= a1), default=None)
    return rec


def run_corpus(corpus):
    root = CORPORA_DIR / corpus
    h = HBASE / ("h" + corpus[:2])
    prepare_home(h)
    OUT.mkdir(parents=True, exist_ok=True)
    recs, meta = [], {"corpus": corpus, "root": str(root), "N": N, "dry": DRY, "uptime_start": uptime()}
    shims = []

    def start(tag):
        log = OUT / f"{corpus}-{tag}.locklog"
        if log.exists():
            log.unlink()
        env = env_for(h, log)
        stop_daemon(h, root, env)
        return env, LockLog(log)

    env, log = start("index")
    try:
        a = Shim(root, env, "A")
        shims = [a]
        first_file = next(str(p.relative_to(root)) for p in sorted(root.rglob("*"))
                          if p.suffix in EXTS[LANG[corpus]] and "node_modules" not in p.parts)
        meta["index"] = wait_ready(a, h, log, first_file, f"{corpus} index")
        a.close()
        shims = []
        env, log = start("measure")  # fresh daemon: model not loaded
        a = Shim(root, env, "A")
        shims = [a]
        meta["restart"] = wait_ready(a, h, log, first_file, f"{corpus} restart")
        dpids = pids(h)
        meta["daemon_pid"] = dpids[0] if dpids else None
        cpu0 = cpu_seconds(meta["daemon_pid"]) if meta["daemon_pid"] else None
        # Query building only outlines (no semantic rung), so the model stays unloaded.
        qs, probe, meta["query_pool"] = build_queries(a, corpus, root)
        meta["probe_file"] = probe
        b = Shim(root, env, "B")
        c = Shim(root, env, "C")
        shims = [a, b, c]
        if "find_definition_semantic" in HANDLERS:
            recs.append(cold_arm(a, b, c, log, qs["find_definition_semantic"][0], probe))
        for hd in HANDLERS:
            recs.append(solo_arm(a, log, hd, qs[hd]))
        base = baseline_arm(b, c, log, probe, 10 if DRY else 20)
        recs.append(base)
        for hd in HANDLERS:
            recs.append(contention_arm(a, b, c, log, hd, qs[hd], probe, base))
        meta["daemon_cpu_s"] = (round(cpu_seconds(meta["daemon_pid"]) - cpu0, 1)
                                if cpu0 is not None and cpu_seconds(meta["daemon_pid"]) is not None else None)
        log.poll()
        meta["writer_holds_total"] = sum(1 for r in log.rows if r[0] == "HOLD" and not is_request(r[4]))
    finally:
        for s in shims:
            s.close()
        ps, killed = stop_daemon(h, root, env)
        meta["stopped_pids"], meta["sigkilled"] = ps, killed
        meta["left_alive"] = [p for p in ps if alive(p)]
        meta["uptime_end"] = uptime()
    (OUT / f"{corpus}.json").write_text(json.dumps({"meta": meta, "arms": recs}, indent=1))


def fmt(s, k):
    v = (s or {}).get(k)
    return "-" if v is None else f"{v:.1f}" if v >= 10 else f"{v:.2f}"


def write_summary(times):
    lines = ["# GM-473 S1 summary", ""]
    for corpus in CORPORA:
        f = OUT / f"{corpus}.json"
        if not f.exists():
            lines += [f"## {corpus}: no result", ""]
            continue
        d = json.loads(f.read_text())
        m = d["meta"]
        lines += [f"## {corpus} ({m['query_pool']}, probe {m['probe_file']})",
                  f"uptime start: {m['uptime_start']}  |  end: {m['uptime_end']}",
                  f"time -p: {times.get(corpus)}  |  daemon cpu over arms: {m.get('daemon_cpu_s')} s"
                  f"  |  index {m['index']}  restart {m['restart']}  |  left alive: {m['left_alive']}", ""]
        for r in d["arms"]:
            if r["arm"] == "cold":
                lines.append(
                    f"cold semantic find_definition ({r['gate']}, load {r['load_before_gate']}): A {r['A_latency_ms']} ms, "
                    f"hold {r['A_hold_ms']} ms, first embed {r['first_embed_ms']} ms (embeds before {r['embeds_before']}), "
                    f"semantic={r['semantic']}; B max overlapping {r['B_max_overlapping_ms']} ms, "
                    f"C (tools/list) max overlapping {r['C_max_overlapping_ms']} ms")
        base = next((r for r in d["arms"] if r["arm"] == "baseline"), None)
        if base:
            lines.append(f"baseline ({base['gate']}): B outline p50/p95 {fmt(base['B_latency_ms'], 'p50')}/"
                         f"{fmt(base['B_latency_ms'], 'p95')} ms, C tools/list p50/p95 "
                         f"{fmt(base['C_latency_ms'], 'p50')}/{fmt(base['C_latency_ms'], 'p95')} ms")
        lines += ["", "| handler | n | hold p50 | hold p95 | hold max | +req holds p95 | latency p50 | "
                      "B wait p50 | B wait p95 | B wait max | C wait p95 | C max | gate (solo/cont) | load (solo/cont) | writers |",
                  "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"]
        solo = {r["handler"]: r for r in d["arms"] if r["arm"] == "solo"}
        cont = {r["handler"]: r for r in d["arms"] if r["arm"] == "contention"}
        for hd in HANDLERS:
            s, c = solo.get(hd, {}), cont.get(hd, {})
            hs = s.get("hold_ms")
            name = hd + (f" ({s.get('semantic_hits')}/{s.get('ok')} sem, embed p50 {fmt(s.get('embed_ms'), 'p50')})"
                         if hd == "find_definition_semantic" else "")
            lines.append(
                f"| {name} | {(hs or {}).get('n', '-')} | {fmt(hs, 'p50')} | {fmt(hs, 'p95')} | {fmt(hs, 'max')} | "
                f"{fmt(s.get('total_hold_ms'), 'p95')} | {fmt(s.get('latency_ms'), 'p50')} | "
                f"{fmt(c.get('B_excess_ms'), 'p50')} | {fmt(c.get('B_excess_ms'), 'p95')} | "
                f"{fmt(c.get('B_excess_ms'), 'max')} | {fmt(c.get('C_excess_ms'), 'p95')} | "
                f"{fmt(c.get('C_latency_ms'), 'max')} | {s.get('gate', '-')}/{c.get('gate', '-')} | "
                f"{s.get('load_end', '-')}/{c.get('load_end', '-')} | "
                f"{s.get('writer_holds', '-')}/{c.get('writer_holds', '-')} |")
        lines.append("")
    lines.append("All times ms. hold = the handler's own store.read() guard per call, A alone (solo arm); +req holds "
                 "adds mark_used and ensure_fresh. Wait columns are the contention arm (A loops the handler): "
                 "B = get_file_outline probe on another session, C = tools/list on a third session (takes no "
                 "store lock, so its wait is for a worker); wait = probe latency minus its baseline p50. "
                 "writers = non-request store holds during the arm (solo/contention).")
    (OUT / "summary.md").write_text("\n".join(lines) + "\n")


def main_run():
    OUT.mkdir(parents=True, exist_ok=True)
    if WAITED_FILE.exists():
        WAITED_FILE.unlink()
    times = {}
    env = dict(os.environ, RUN_T0=str(T0))
    for corpus in CORPORA:
        r = subprocess.run(["/usr/bin/time", "-p", sys.executable, __file__, "corpus", corpus],
                           env=env, stderr=subprocess.PIPE, text=True)
        tl = [ln for ln in r.stderr.splitlines() if re.match(r"^(real|user|sys) ", ln)]
        times[corpus] = " ".join(tl) + ("" if r.returncode == 0 else f" EXIT {r.returncode}")
        if r.returncode:
            (OUT / f"{corpus}.stderr").write_text(r.stderr[-20000:])
    write_summary(times)
    progress("done", 1, 1, force=True)


if __name__ == "__main__":
    if sys.argv[1:2] == ["run"]:
        main_run()
    elif sys.argv[1:2] == ["corpus"]:
        run_corpus(sys.argv[2])
    elif sys.argv[1:2] == ["summary"]:
        write_summary({})
    else:
        sys.exit(__doc__)
