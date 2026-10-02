#!/usr/bin/env python3
"""GM-464 S8: product-path latency of search_code's cross-encoder rerank.

Drives the real product path - `g-mesh mcp-shim` over stdio, which bootstraps
the per-project daemon - on g-mesh's own index, and times `search_code` calls
end to end (JSON-RPC request written -> response read).

Arms (each runs in a fresh daemon, so the first search pays the model load):
  off  rerank disabled (G_MESH_RERANK=off), the 4-thread binary
  t4   rerank on, the branch's binary: intra-op threads min(4, physical cores)
  t1   rerank on, a binary built from the same commit with MAX_THREADS = 1
       (core/src/embedding/rerank.rs) in a throwaway worktree - the design has
       no runtime thread knob

Per arm: start the shim, outline one file (starts the walk), wait for
index.phase == ready, then gate (1-min load < GATE_LOAD held GATE_HOLD s,
total waiting across the run capped at GATE_CAP s; past the cap the arm runs
anyway and is marked), then: one first call (timed alone), one untimed warm-up
pass over the query set, PASSES timed passes. The daemon's CPU time over the
timed passes (ps) separates 1 from 4 threads; the share of pages whose scores
are not in descending cosine order separates rerank on from off.

Modes:
  gm464_latency.py run      every arm (ARMS env, default "off t4 t1 t1 t4 off"),
                            each under /usr/bin/time -p, then the summary table
  gm464_latency.py arm NAME one arm (internal; prints a JSON line)

Env: OUT (results dir), H (isolated G_MESH_HOME; short path - the daemon
socket lives under it), BIN_T4, BIN_T1 (dirs holding g-mesh + plugins),
NQ (queries, default 30), PASSES (default 3), GATE=0 (dry run: no wait),
GATE_LOAD (4), GATE_HOLD (30), GATE_CAP (3600), PROGRESS (progress file).
"""

import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent  # the checkout being indexed (g-mesh itself)
OUT = Path(os.environ.get("OUT", HERE / "work" / "runs-gm464-latency"))
H = Path(os.environ.get("H", "/private/tmp/claude-502/gm464s8/home"))
BINS = {
    "off": Path(os.environ.get("BIN_T4", "/private/tmp/claude-502/gm464s8/t4")),
    "t4": Path(os.environ.get("BIN_T4", "/private/tmp/claude-502/gm464s8/t4")),
    "t1": Path(os.environ.get("BIN_T1", "/private/tmp/claude-502/gm464s8/t1")),
}
RERANK_DIR = os.environ.get(
    "G_MESH_RERANK_MODEL_DIR",
    str(ROOT.parent / "g-mesh" / "eval" / "embedding" / "work" / "models" / "ms-marco-MiniLM-L6-v2"),
)
NQ = int(os.environ.get("NQ", "30"))
PASSES = int(os.environ.get("PASSES", "3"))
GATE = os.environ.get("GATE", "1") == "1"
GATE_LOAD = float(os.environ.get("GATE_LOAD", "4"))
GATE_HOLD = int(os.environ.get("GATE_HOLD", "30"))
GATE_CAP = int(os.environ.get("GATE_CAP", "3600"))
PROGRESS = Path(os.environ.get("PROGRESS", str(Path.home() / ".claude" / "progress" / "GM-464-S8.txt")))
WAITED_FILE = OUT / "gate_waited_s"
T0 = float(os.environ.get("RUN_T0", time.time()))


def load1():
    return os.getloadavg()[0]


def uptime():
    return subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()


def therm():
    out = subprocess.run(["pmset", "-g", "therm"], capture_output=True, text=True).stdout
    s = re.search(r"CPU_Speed_Limit\s*=\s*(\d+)", out)
    c = re.search(r"CPU_Scheduler_Limit\s*=\s*(\d+)", out)
    return f"{s.group(1) if s else 'na'}/{c.group(1) if c else 'na'}"


def progress(stage, done, total, eta=None):
    el = time.time() - T0
    pct = 100.0 * done / total if total else 0.0
    eta_s = f"{eta:.0f}s" if eta is not None else "?"
    line = f"{stage} {done}/{total} {pct:.0f}% elapsed {el:.0f}s eta {eta_s} load {load1():.2f}\n"
    try:
        PROGRESS.parent.mkdir(parents=True, exist_ok=True)
        tmp = PROGRESS.with_suffix(".tmp")
        tmp.write_text(line)
        tmp.replace(PROGRESS)
    except OSError:
        pass


def queries():
    rows = [json.loads(line) for line in (HERE / "queries" / "g-mesh.jsonl").open()]
    return [r["text"] for r in rows if r["kind"] == "positive"][:NQ]


class Shim:
    def __init__(self, binary, env):
        self.p = subprocess.Popen(
            [str(binary), "mcp-shim"], cwd=ROOT, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self.next_id = 0

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


def state_dirs():
    return [d for d in (H / "projects").glob("*") if d.is_dir()] if (H / "projects").exists() else []


def phase():
    for d in state_dirs():
        f = d / "index.phase"
        if f.exists():
            return f.read_text().strip()
    return None


def pid_files():
    out = []
    for d in state_dirs():
        out += [d / "daemon.pid"] + sorted(d.glob("plugin-*.pid"))
    return out


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


def cpu_seconds(pid):
    """Total CPU (user+sys) of pid in seconds, from ps's cumulative time."""
    out = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    parts = out.replace("-", ":").split(":")
    secs = 0.0
    for p in parts:
        secs = secs * 60 + float(p)
    return secs


def env_for(arm):
    env = dict(os.environ)
    env["G_MESH_HOME"] = str(H)
    env["G_MESH_RERANK_MODEL_DIR"] = RERANK_DIR
    env["G_MESH_PLUGIN_ROOTS_OVERRIDE"] = str(ROOT / "plugins")
    env.pop("G_MESH_RERANK", None)
    if arm == "off":
        env["G_MESH_RERANK"] = "off"
    return env


def stop_daemon(binary, env):
    """`g-mesh stop`, then SIGKILL any recorded pid that survived (leaked
    daemons have been seen to outlive SIGTERM)."""
    pids = [p for p in (read_pid(f) for f in pid_files()) if p]
    subprocess.run([str(binary / "g-mesh"), "stop"], cwd=ROOT, env=env, capture_output=True, timeout=60)
    deadline = time.time() + 15
    while time.time() < deadline and any(alive(p) for p in pids):
        time.sleep(0.5)
    killed = []
    for p in pids:
        if alive(p):
            os.kill(p, signal.SIGKILL)
            killed.append(p)
    return pids, killed


def gate():
    waited = float(WAITED_FILE.read_text()) if WAITED_FILE.exists() else 0.0
    if not GATE:
        return {"gate": "off", "waited_s": 0}
    held = 0
    start = time.time()
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
            status = "closed (cap reached, measured anyway)"
            break
        progress(f"gate-wait load<{GATE_LOAD} held {held}s waited {spent:.0f}/{GATE_CAP}s", 0, 1)
        time.sleep(5)
    mine = time.time() - start
    WAITED_FILE.write_text(f"{waited + mine:.0f}")
    return {"gate": status, "waited_s": round(mine)}


def nonmonotonic(resp):
    """True when the page's rows are not in descending score order, i.e. the
    rerank reordered them (the score shown stays the cosine)."""
    text = json.dumps(resp)
    scores = [float(s) for s in re.findall(r'\\"score\\":\s*(-?[0-9.eE+-]+)', text)]
    if not scores:
        scores = [float(s) for s in re.findall(r'"score":\s*(-?[0-9.eE+-]+)', text)]
    return any(b > a + 1e-9 for a, b in zip(scores, scores[1:])), len(scores)


def timed(shim, q):
    t = time.perf_counter()
    resp = shim.call("search_code", {"query": q})
    ms = (time.perf_counter() - t) * 1000
    res = resp.get("result", {})
    if "error" in resp or res.get("isError"):
        raise RuntimeError(f"search_code failed: {json.dumps(resp)[:400]}")
    if '\\"partial\\":' in json.dumps(res):
        raise RuntimeError("partial page during timing (embedding pass not done)")
    return ms, res


def run_arm(arm, label):
    qs = queries()
    binary = BINS[arm]
    env = env_for(arm)
    stop_daemon(binary, env)  # never reuse another arm's daemon
    shim = Shim(binary / "g-mesh", env)
    rec = {"arm": arm, "label": label, "bin": str(binary), "nq": len(qs), "passes": PASSES}
    try:
        shim.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "gm464-latency", "version": "0"}})
        shim.notify("notifications/initialized")
        t = time.time()
        while True:
            r = shim.call("get_file_outline", {"file_path": "core/src/main.rs"})
            if not r.get("result", {}).get("isError") and "error" not in r:
                break
            if time.time() - t > 3600:
                raise RuntimeError("structural index never answered")
            time.sleep(5)
        while phase() != "ready":
            if time.time() - t > 3600:
                raise RuntimeError(f"index never ready (phase {phase()})")
            progress(f"{label} indexing phase={phase()}", 0, 1)
            time.sleep(3)
        rec["ready_s"] = round(time.time() - t, 1)
        dpid = read_pid(next(d / "daemon.pid" for d in state_dirs()))
        rec["daemon_pid"] = dpid
        rec.update(gate())
        rec["uptime_start"] = uptime()
        rec["therm_start"] = therm()
        rec["load_start"] = round(load1(), 2)
        ms, res = timed(shim, qs[0])
        rec["first_ms"] = round(ms, 1)
        reordered = 0
        for i, q in enumerate(qs):  # warm-up pass, untimed
            _, res = timed(shim, q)
            progress(f"{label} warmup", i + 1, len(qs))
        cpu0, w0 = cpu_seconds(dpid), time.time()
        samples = []
        total = PASSES * len(qs)
        last = time.time()
        for p in range(PASSES):
            for i, q in enumerate(qs):
                ms, res = timed(shim, q)
                samples.append(round(ms, 2))
                nm, nrows = nonmonotonic(res)
                reordered += nm
                done = len(samples)
                if time.time() - last > 300 or done % max(1, total // 10) == 0:
                    rate = (time.time() - w0) / done
                    progress(f"{label} timed", done, total, rate * (total - done))
                    last = time.time()
        wall = time.time() - w0
        cpu1 = cpu_seconds(dpid)
        rec["samples_ms"] = samples
        rec["reordered_pages"] = reordered
        rec["timed_wall_s"] = round(wall, 2)
        rec["daemon_cpu_s"] = round(cpu1 - cpu0, 2) if cpu0 is not None and cpu1 is not None else None
        rec["load_end"] = round(load1(), 2)
        rec["therm_end"] = therm()
        rec["uptime_end"] = uptime()
    finally:
        shim.close()
        pids, killed = stop_daemon(binary, env)
        rec["stopped_pids"] = pids
        rec["sigkilled"] = killed
    return rec


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    k = (len(xs) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def main_run():
    OUT.mkdir(parents=True, exist_ok=True)
    if WAITED_FILE.exists():
        WAITED_FILE.unlink()
    arms = os.environ.get("ARMS", "off t4 t1 t1 t4 off").split()
    env = dict(os.environ, RUN_T0=str(T0), OUT=str(OUT))
    recs = []
    for n, arm in enumerate(arms):
        label = f"{arm}#{n + 1}"
        progress(f"arm {label} start", n, len(arms))
        p = subprocess.run(["/usr/bin/time", "-p", sys.executable, __file__, "arm", arm, label],
                           env=env, capture_output=True, text=True)
        tp = dict(re.findall(r"^(real|user|sys)\s+([0-9.]+)", p.stderr, re.M))
        lines = [l for l in p.stdout.splitlines() if l.startswith("{")]
        if p.returncode != 0 or not lines:
            rec = {"arm": arm, "label": label, "error": p.stderr[-1500:]}
        else:
            rec = json.loads(lines[-1])
        rec["time_p"] = tp
        recs.append(rec)
        (OUT / "arms.jsonl").open("a").write(json.dumps(rec) + "\n")
    write_summary(recs)
    progress("done", len(arms), len(arms))


def write_summary(recs):
    rows = ["| arm | n | p50 ms | p95 ms | first-call ms | reordered | daemon cpu/wall | gate | load start/end | therm | time -p real/user/sys |",
            "|---|---|---|---|---|---|---|---|---|---|---|"]
    pooled = {}
    for r in recs:
        tp = r.get("time_p", {})
        tps = f"{tp.get('real', '?')}/{tp.get('user', '?')}/{tp.get('sys', '?')}"
        if "error" in r:
            rows.append(f"| {r['label']} | ERROR | | | | | | | | | {tps} |")
            continue
        s = r["samples_ms"]
        pooled.setdefault(r["arm"], []).extend(s)
        cw = f"{r['daemon_cpu_s'] / r['timed_wall_s']:.2f}" if r.get("daemon_cpu_s") is not None else "?"
        rows.append(f"| {r['label']} | {len(s)} | {pct(s, .5):.0f} | {pct(s, .95):.0f} | {r['first_ms']:.0f} | "
                    f"{r['reordered_pages']}/{len(s)} | {cw} | {r['gate']} | {r['load_start']}/{r['load_end']} | "
                    f"{r['therm_start']}/{r['therm_end']} | {tps} |")
    rows += ["", "| arm (pooled) | n | p50 ms | p95 ms |", "|---|---|---|---|"]
    for arm, s in pooled.items():
        rows.append(f"| {arm} | {len(s)} | {pct(s, .5):.0f} | {pct(s, .95):.0f} |")
    rows += ["", "uptime per arm (start -> end):"]
    for r in recs:
        if "error" not in r:
            rows.append(f"- {r['label']}: {r['uptime_start']} -> {r['uptime_end']}")
        else:
            rows.append(f"- {r['label']}: error: {r['error'][-300:]!r}")
    (OUT / "summary.md").write_text("\n".join(rows) + "\n")


if __name__ == "__main__":
    if len(sys.argv) >= 2 and sys.argv[1] == "arm":
        print(json.dumps(run_arm(sys.argv[2], sys.argv[3])))
    elif len(sys.argv) >= 2 and sys.argv[1] == "run":
        main_run()
    else:
        sys.exit(__doc__)
