#!/usr/bin/env python3
"""GM-435 S10 measure harness (stdlib only, python 3.9). See run.sh."""
import argparse, glob, hashlib, json, os, queue, re, signal, sqlite3, statistics, subprocess, sys, threading, time

PROTOCOL = "2025-03-26"
TIER_TIMEOUT = 60.0
QUERY = "tally lantern shipments waiting in quartz harbour ledger"

ARMS = {
    "rust": {
        "root": "P",
        "A": dict(file="core/src/daemon/identity.rs", removed=["project_hash", "record_project_root"], style="rs", callee="drain_ready", label="A: identity.rs (public, imported by name)"),
        "B": dict(file="core/src/cli/status/json.rs", removed=["languages", "outcome"], style="rs", callee="drain_ready", label="B: status/json.rs (private)"),
    },
    "ts": {
        "root": "TS",
        "T": dict(file="src/domain/taskStatus.ts", removed=["findUnblockedDependents", "displayStatus"], style="ts", callee="estimateTokens", label="TS: taskStatus.ts"),
    },
    "go": {
        "root": "GO",
        "G": dict(file="internal/swarm/files.go", removed=["SelectVideos", "looksLikeSample"], style="go", callee="Name", label="Go: swarm/files.go"),
    },
}

NEW_NAMES = {
    "rs": ["edit_probe_alpha", "edit_probe_beta", "edit_probe_gamma"],
    "ts": ["editProbeAlpha", "editProbeBeta", "editProbeGamma"],
    "go": ["EditProbeAlpha", "EditProbeBeta", "EditProbeGamma"],
}

def new_code(style, salt):
    if style == "rs":
        return f'''/// Tallies the lantern shipments waiting in the quartz harbour ledger. {salt}
pub fn edit_probe_alpha(counts: &[u32]) -> u32 {{
    let mut total = 0;
    for count in counts {{
        if *count > 3 {{
            total += *count * 2;
        }} else if *count == 0 {{
            total += 1;
        }} else {{
            total += *count;
        }}
    }}
    if total > 1000 {{
        total = 1000;
    }}
    total
}}

/// Renders a comma separated manifest of crate labels. {salt}
pub fn edit_probe_beta(items: &[String]) -> String {{
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {{
        if index > 0 {{
            out.push_str(", ");
        }}
        out.push_str(item.trim());
        if item.is_empty() {{
            out.push_str("(blank)");
        }}
    }}
    if out.len() > 200 {{
        out.truncate(200);
    }}
    out
}}

/// Polls a fresh debouncer once and counts what was ready. {salt}
pub fn edit_probe_gamma() -> usize {{
    let mut debouncer = crate::watcher::debounce::Debouncer::new(std::time::Duration::from_millis(1));
    let ready = debouncer.drain_ready();
    ready.len()
}}
'''
    if style == "ts":
        return f'''/** Tallies the lantern shipments waiting in the quartz harbour ledger. {salt} */
export function editProbeAlpha(counts: number[]): number {{
  let total = 0;
  for (const count of counts) {{
    if (count > 3) {{
      total += count * 2;
    }} else if (count === 0) {{
      total += 1;
    }} else {{
      total += count;
    }}
  }}
  if (total > 1000) {{
    total = 1000;
  }}
  return total;
}}

/** Renders a comma separated manifest of crate labels. {salt} */
export function editProbeBeta(items: string[]): string {{
  let out = "";
  items.forEach((item, index) => {{
    if (index > 0) {{
      out += ", ";
    }}
    out += item.trim();
    if (item.length === 0) {{
      out += "(blank)";
    }}
  }});
  return out.length > 200 ? out.slice(0, 200) : out;
}}

/** Asks a fresh estimator for the size of nothing. {salt} */
export function editProbeGamma(): number {{
  const estimator = new HeuristicBudgetEstimator();
  const tokens = estimator.estimateTokens(".", []);
  return tokens;
}}
'''
    return f'''
// EditProbeAlpha tallies the lantern shipments waiting in the quartz harbour ledger. {salt}
func EditProbeAlpha(counts []int) int {{
	total := 0
	for _, count := range counts {{
		if count > 3 {{
			total += count * 2
		}} else if count == 0 {{
			total++
		}} else {{
			total += count
		}}
	}}
	if total > 1000 {{
		total = 1000
	}}
	return total
}}

// EditProbeBeta renders a comma separated manifest of crate labels. {salt}
func EditProbeBeta(items []string) string {{
	out := ""
	for index, item := range items {{
		if index > 0 {{
			out += ", "
		}}
		out += strings.TrimSpace(item)
		if item == "" {{
			out += "(blank)"
		}}
	}}
	if len(out) > 200 {{
		out = out[:200]
	}}
	return out
}}

// EditProbeGamma asks a file entry for its base name. {salt}
func EditProbeGamma() string {{
	entry := FileInfo{{Index: 1, Path: "a/b.mkv"}}
	name := entry.Name()
	return name
}}
'''

def remove_fn(text, name, style):
    lines = text.split("\n")
    if style == "rs":
        pat = re.compile(r"^(pub(\([a-z]+\))? )?(async )?fn %s\b" % re.escape(name))
    elif style == "ts":
        pat = re.compile(r"^export (async )?function %s\b" % re.escape(name))
    else:
        pat = re.compile(r"^func %s\b" % re.escape(name))
    idx = [i for i, l in enumerate(lines) if pat.match(l)]
    assert len(idx) == 1, "fn %s: %d matches" % (name, len(idx))
    i = idx[0]
    start = i
    while start > 0 and lines[start - 1].lstrip().startswith(("//", "/**", "*", "#[")):
        start -= 1
    j = i
    while lines[j] != "}":
        j += 1
    end = j + 1
    if end < len(lines) and lines[end] == "":
        end += 1
    return "\n".join(lines[:start] + lines[end:])

def make_edit(orig, cfg, salt):
    style = cfg["style"]
    text = orig
    for n in cfg["removed"]:
        text = remove_fn(text, n, style)
    code = new_code(style, salt)
    if style == "rs":
        m = re.search(r"^#\[cfg\(test\)\]", text, re.M)
        if m:
            text = text[: m.start()] + code + "\n" + text[m.start():]
        else:
            text = text.rstrip("\n") + "\n\n" + code
    elif style == "ts":
        text = 'import { HeuristicBudgetEstimator } from "./budget.js";\n' + text.rstrip("\n") + "\n\n" + code
    else:
        text = text.rstrip("\n") + "\n" + code
    return text

# ---------------------------------------------------------------- MCP shim
class Shim:
    def __init__(self, exe, project, env):
        self.proc = subprocess.Popen([exe, "mcp-shim"], cwd=project, env=env, stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.lines = queue.Queue()
        self.next_id = 1
        threading.Thread(target=self._pump, daemon=True).start()
        self.request("initialize", {"protocolVersion": PROTOCOL, "capabilities": {},
                                    "clientInfo": {"name": "gm435", "version": "0"}}, timeout=900)
        self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def _pump(self):
        for raw in iter(self.proc.stdout.readline, b""):
            self.lines.put(raw)
        self.lines.put(None)

    def _send(self, m):
        self.proc.stdin.write((json.dumps(m) + "\n").encode())
        self.proc.stdin.flush()

    def request(self, method, params, timeout=120):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        end = time.monotonic() + timeout
        while True:
            rem = end - time.monotonic()
            if rem <= 0:
                raise RuntimeError("timeout %s" % method)
            raw = self.lines.get(timeout=rem)
            if raw is None:
                raise RuntimeError("shim closed")
            try:
                msg = json.loads(raw)
            except ValueError:
                continue
            if msg.get("id") == rid and "method" not in msg:
                if "error" in msg:
                    raise RuntimeError("rpc error %s" % json.dumps(msg["error"]))
                return msg.get("result")
            if "method" in msg and "id" in msg:
                self._send({"jsonrpc": "2.0", "id": msg["id"], "result": {}} if msg["method"] == "ping"
                           else {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}})

    def call(self, name, args):
        t0 = time.time()
        res = self.request("tools/call", {"name": name, "arguments": args})
        t1 = time.time()
        text = "\n".join(i.get("text", "") for i in (res or {}).get("content", []) if isinstance(i, dict))
        return bool((res or {}).get("isError")), text, t0, t1

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()

def has_decl(text, name, file):
    """True if the tool answer names a declaration `name` in `file`."""
    try:
        obj = json.loads(text)
    except ValueError:
        return False
    def walk(o):
        if isinstance(o, dict):
            if o.get("name") == name and str(o.get("filePath", "")) == file:
                return True
            return any(walk(v) for v in o.values())
        if isinstance(o, list):
            return any(walk(v) for v in o)
        return False
    return walk(obj)

# ---------------------------------------------------------------- environment
class Env:
    def __init__(self, a):
        self.a = a
        self.bin = a.bin
        self.home = a.home
        self.env = dict(os.environ)
        self.env.pop("CLAUDE_PROJECT_DIR", None)
        self.env["G_MESH_HOME"] = self.home
        self.env["G_MESH_EDIT_TIMING"] = "1"
        self.progress = open(a.progress, "a", buffering=1)

    def prog(self, s):
        self.progress.write(s + "\n")

def ps_table():
    out = subprocess.run(["ps", "-Ao", "pid=,ppid=,%cpu=,rss=,comm="], capture_output=True, text=True).stdout
    rows = []
    for l in out.splitlines():
        p = l.split(None, 4)
        if len(p) == 5:
            rows.append((int(p[0]), int(p[1]), float(p[2]), int(p[3]), p[4]))
    return rows

def descendants(pid, rows=None):
    rows = rows or ps_table()
    kids = {}
    for r in rows:
        kids.setdefault(r[1], []).append(r)
    out, stack = [], [pid]
    while stack:
        for r in kids.get(stack.pop(), []):
            out.append(r)
            stack.append(r[0])
    return out

def machine_state(daemon_pid):
    rows = ps_table()
    d = [r for r in rows if r[0] == daemon_pid]
    desc = descendants(daemon_pid, rows)
    return {
        "load": [round(x, 2) for x in os.getloadavg()],
        "rustc": sum(1 for r in rows if os.path.basename(r[4]) == "rustc"),
        "daemon_cpu": d[0][2] if d else None,
        "daemon_rss_mb": round(d[0][3] / 1024) if d else None,
        "desc": [(os.path.basename(r[4]), r[2], round(r[3] / 1024)) for r in desc if r[2] > 0.5 or r[3] > 200000],
        "desc_cpu": round(sum(r[2] for r in desc), 1),
    }

def idle_gate(daemon_pid, cap=60.0):
    end = time.time() + cap
    while time.time() < end:
        a = machine_state(daemon_pid)["desc_cpu"]
        time.sleep(1)
        b = machine_state(daemon_pid)["desc_cpu"]
        if a < 10 and b < 10:
            time.sleep(2)
            return True
    return False

# ---------------------------------------------------------------- log reader
class LogTail(threading.Thread):
    def __init__(self, path):
        super().__init__(daemon=True)
        self.path = path
        self.lines = []  # (arrival, text)
        self.lock = threading.Lock()
        self.stop = False

    def run(self):
        f = open(self.path, "rb")
        buf = b""
        while not self.stop:
            chunk = f.read(65536)
            if not chunk:
                time.sleep(0.005)
                continue
            now = time.time()
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                with self.lock:
                    self.lines.append((now, line.decode("utf-8", "replace")))

    def snapshot(self):
        with self.lock:
            return list(self.lines)

TL = re.compile(r"g-mesh timing: t=([0-9.]+) (\S+)(.*)$")

def events(lines):
    out = []
    for arr, text in lines:
        m = TL.search(text)
        if not m:
            continue
        kv = {}
        for tok in m.group(3).split():
            if "=" in tok:
                k, v = tok.split("=", 1)
                kv[k] = v
        out.append(dict(t=float(m.group(1)), ev=m.group(2), kv=kv, arr=arr, raw=text))
    return out

# ---------------------------------------------------------------- daemon
def start_daemon(E, root, tag):
    log = os.path.join(E.home, "daemon-%s.log" % tag)
    open(log, "ab").close()
    f = open(log, "ab")
    p = subprocess.Popen([E.bin, "daemon", "--project-root", root], env=E.env, stdout=f, stderr=f,
                         stdin=subprocess.DEVNULL, start_new_session=True)
    return p, log

def stop_daemon(p, E, root):
    kids = descendants(p.pid)
    pids = [r[0] for r in kids]
    try:
        p.send_signal(signal.SIGTERM)
        p.wait(timeout=20)
    except Exception:
        p.kill()
    time.sleep(2)
    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    left = [r for r in ps_table() if r[0] in pids + [p.pid]]
    return left

# ---------------------------------------------------------------- observers
class Run:
    pass

def db_path(home):
    c = glob.glob(os.path.join(home, "projects", "*", "index.db"))
    return c

def run_observers(E, cfg, root, shim_pool, t_save_box, res, stop):
    """db observer + tool pollers; fills res (dict of times, absolute)."""
    style = cfg["style"]
    file = cfg["file"]
    new = NEW_NAMES[style]
    gamma = new[2]
    removed = cfg["removed"]
    lock = threading.Lock()
    def put(k, v):
        with lock:
            res.setdefault(k, v)
    def db_thread():
        con = sqlite3.connect("file:%s?mode=ro" % E.dbfile, uri=True, timeout=1)
        ph_new = ",".join("?" * 3)
        ph_rm = ",".join("?" * len(removed))
        q_a1 = "SELECT count(*) FROM nodes WHERE filePath=? AND name IN (%s)" % ph_new
        q_a2 = ("SELECT count(*) FROM nodes WHERE filePath=? AND COALESCE(nativeKind,'')<>'pending_symbol' AND name IN (%s)" % ph_rm)
        q_b = ("SELECT n.name, v.embedding FROM vectors v JOIN nodes n ON n.id=v.nodeId "
               "WHERE n.filePath=? AND n.name IN (%s)" % ph_new)
        q_c = ("SELECT 1 FROM edges e JOIN nodes f ON f.id=e.fromId JOIN nodes t ON t.id=e.toId "
               "WHERE f.filePath=? AND f.name=? AND t.name=? AND e.source='semantic' LIMIT 1")
        pre = dict(con.execute(q_b, [file] + new).fetchall())
        res["pre_vectors"] = len(pre)
        res["pre_c"] = bool(con.execute(q_c, [file, gamma, cfg["callee"]]).fetchone())
        con.rollback()
        while not stop.is_set():
            try:
                now = time.time()
                if "a_db" not in res:
                    if con.execute(q_a1, [file] + new).fetchone()[0] == 3 and con.execute(q_a2, [file] + removed).fetchone()[0] == 0:
                        put("a_db", time.time())
                if "b_db" not in res:
                    rows = dict(con.execute(q_b, [file] + new).fetchall())
                    if len(rows) == 3 and all(rows[k] != pre.get(k) for k in rows):
                        put("b_db", time.time())
                if "c_db" not in res and "a_db" in res:
                    if con.execute(q_c, [file, gamma, cfg["callee"]]).fetchone():
                        put("c_db", time.time())
                con.rollback()
            except sqlite3.Error as e:
                res.setdefault("db_errors", []).append(str(e))
            time.sleep(0.010)
        con.close()
    def tool_struct():
        sh = shim_pool[0]
        rtts = res.setdefault("rtt_struct", [])
        while not stop.is_set():
            if "a_tool_new" not in res:
                err, text, t0, t1 = sh.call("find_definition", {"symbol_name": gamma})
                rtts.append(t1 - t0)
                if not err and has_decl(text, gamma, file):
                    put("a_tool_new", t1)
            if "a_tool_gone" not in res:
                err, text, t0, t1 = sh.call("find_definition", {"symbol_name": removed[0]})
                rtts.append(t1 - t0)
                if err or not has_decl(text, removed[0], file):
                    put("a_tool_gone", t1)
            if "a_tool_new" in res and "a_tool_gone" in res:
                put("a_tool", max(res["a_tool_new"], res["a_tool_gone"]))
                return
            time.sleep(0.025)
    def tool_search():
        sh = shim_pool[1]
        rtts = res.setdefault("rtt_search", [])
        while not stop.is_set() and "b_db" not in res:
            time.sleep(0.005)
        t_first = time.time()
        while not stop.is_set():
            err, text, t0, t1 = sh.call("search_code", {"query": QUERY, "limit": 10})
            rtts.append(t1 - t0)
            if not err and new[0] in text:
                put("b_tool", t1)
                return
            if t1 - t_first > 5.0:
                res["b_not_ranked"] = True
                res["b_top10"] = text[:1500]
                put("b_tool", None)
                return
            time.sleep(0.1)
    def tool_callees():
        sh = shim_pool[2]
        rtts = res.setdefault("rtt_callees", [])
        while not stop.is_set() and "c_db" not in res:
            time.sleep(0.005)
        while not stop.is_set():
            err, text, t0, t1 = sh.call("find_callees", {"symbol_name": gamma})
            rtts.append(t1 - t0)
            if not err and cfg["callee"] in text:
                put("c_tool", t1)
                return
            time.sleep(0.05)
    ths = [threading.Thread(target=f, daemon=True) for f in (db_thread, tool_struct, tool_search, tool_callees)]
    for t in ths:
        t.start()
    return ths

def write_file(path, text):
    t0 = time.time()
    fd = os.open(path, os.O_WRONLY | os.O_TRUNC | os.O_CREAT)
    os.write(fd, text.encode())
    t1 = time.time()
    os.close(fd)
    return t0, t1, time.time()

def sha(path):
    return hashlib.sha256(open(path, "rb").read()).hexdigest()

# ---------------------------------------------------------------- phases
def first(evs, name, after=0.0, pred=None):
    for e in evs:
        if e["ev"] == name and e["t"] >= after and (pred is None or pred(e)):
            return e
    return None

def phases(evs, t_save, rel):
    """Derive per-phase seconds from the timing events of one edit window."""
    P = {}
    fs = first(evs, "fs_event", t_save - 0.01, lambda e: rel in e["raw"] or os.path.basename(rel) in e["raw"])
    if not fs:
        return P
    P["fs_event"] = fs["t"] - t_save
    st = first(evs, "settled", fs["t"])
    sent = first(evs, "rt_sent", fs["t"], lambda e: e["kv"].get("method") == "fileChanged" and rel in e["raw"])
    if st:
        P["debounce"] = st["t"] - fs["t"]
    if not sent:
        return P
    mid = sent["kv"].get("id")
    if st:
        P["route"] = sent["t"] - st["t"]
    def byid(name, i, after):
        return first(evs, name, after, lambda e: e["kv"].get("id") == i)
    ans = byid("rt_answered", mid, sent["t"])
    com = byid("rt_committed", mid, sent["t"])
    emb = byid("rt_embedded", mid, sent["t"])
    vec = byid("rt_vectors", mid, sent["t"])
    if ans:
        P["parse"] = ans["t"] - sent["t"]
        P["affected"] = ans["kv"].get("affected")
    if ans and com:
        P["store"] = com["t"] - ans["t"]
        P["nodes"] = com["kv"].get("nodes")
    if com and emb:
        P["embed"] = emb["t"] - com["t"]
        P["embed_texts"], P["embed_hits"], P["embed_new"] = emb["kv"].get("texts"), emb["kv"].get("hits"), emb["kv"].get("embedded")
    if emb and vec:
        P["vectors"] = vec["t"] - emb["t"]
    if vec:
        P["b_log"] = vec["t"] - t_save
    if com:
        P["a_log"] = com["t"] - t_save
        # re-extract of dependents: reextract_start/done between the main vectors and semantic_start
        rs = first(evs, "reextract_start", (vec or com)["t"])
        rd = first(evs, "reextract_done", rs["t"]) if rs else None
        if rs and rd:
            P["reextract"] = rd["t"] - rs["t"]
            P["reextract_n"] = rs["kv"].get("n")
            P["reextract_n_done"] = rd["kv"].get("n")
    ss = first(evs, "semantic_start", (vec or com or sent)["t"] if (vec or com) else sent["t"])
    if not ss:
        return P
    sid = ss["kv"].get("id")
    P["sem_requested"], P["sem_owed"] = ss["kv"].get("requested"), ss["kv"].get("owed")
    ssent = first(evs, "rt_sent", ss["t"], lambda e: e["kv"].get("id") == sid)
    if not ssent:
        return P
    sans = byid("rt_answered", sid, ssent["t"])
    lsp_end = sans["t"] if sans else 1e18
    in_win = [e for e in evs if ssent["t"] <= e["t"] <= lsp_end]
    sy, rdy, lan, ldn = (first(in_win, n) for n in ("lsp_synced", "lsp_ready", "lsp_answered", "lsp_done"))
    if sy:
        P["sem_sync"] = sy["t"] - ssent["t"]
    if sy and rdy:
        P["sem_wait_ready"] = rdy["t"] - sy["t"]
        P["sem_waited_ms"] = rdy["kv"].get("waited_ms")
    if rdy and lan:
        P["sem_questions"] = lan["t"] - rdy["t"]
        for k in ("questions", "answers", "content_modified", "max_request_ms", "p50_request_ms"):
            P["sem_" + k] = lan["kv"].get(k)
    if lan and ldn:
        P["sem_build"] = ldn["t"] - lan["t"]
    if sans:
        P["sem_roundtrip"] = sans["t"] - ssent["t"]
        P["sem_affected"] = sans["kv"].get("affected")
    scom = byid("rt_committed", sid, ssent["t"])
    semb = byid("rt_embedded", sid, ssent["t"])
    svec = byid("rt_vectors", sid, ssent["t"])
    sdone = first(evs, "semantic_done", ss["t"], lambda e: e["kv"].get("id") == sid) or first(evs, "semantic_done", ss["t"])
    if sans and scom:
        P["sem_commit"] = scom["t"] - sans["t"]
        P["sem_edges"] = scom["kv"].get("edges")
    if scom and semb:
        P["sem_embed"] = semb["t"] - scom["t"]
    if scom:
        P["c_log"] = scom["t"] - t_save
    if sdone:
        P["semantic_done"] = sdone["t"] - t_save
        P["sem_ok"] = sdone["kv"].get("ok")
        if scom:
            P["sem_tail"] = sdone["t"] - (semb or scom)["t"]
    return P

# ---------------------------------------------------------------- one language
def run_language(E, lang, a, raw):
    spec = ARMS[lang]
    root = os.path.join(a.work, spec["root"])
    arms = [k for k in spec if k != "root" and (not a.only_arm or k in a.only_arm.split(","))]
    tag = lang
    proc, logp = start_daemon(E, root, tag)
    E.daemon_pid = proc.pid
    E.prog("GM-435 measure · %s daemon pid %d" % (lang, proc.pid))
    raw({"kind": "daemon", "lang": lang, "pid": proc.pid, "log": logp})
    tail = LogTail(logp)
    tail.start()
    status = "ok"
    shims = []
    try:
        # trigger indexing with a first tool call (a directly started daemon does not index before one)
        t0 = time.time()
        shim = Shim(E.bin, root, E.env)
        shims.append(shim)
        warm_cfg = spec[arms[0]]
        err, text, _, _ = shim.call("get_file_outline", {"file_path": warm_cfg["file"]})
        raw({"kind": "first_call", "lang": lang, "secs": round(time.time() - t0, 1), "err": err, "text": text[:200]})
        E.dbfile = None
        for _ in range(100):
            c = db_path(E.home)
            # newest index.db that belongs to this project: the one modified last
            if c:
                E.dbfile = max(c, key=os.path.getmtime)
                break
            time.sleep(0.5)
        # warm gate
        want = {"rust": "rust semantic pass over", "ts": "typescript semantic pass over", "go": "go semantic pass over"}[lang]
        end = time.time() + (a.warm_cap if lang == 'rust' else 900)
        ok_sem = False
        while time.time() < end:
            lines = tail.snapshot()
            if any(want in t and "complete" in t for _, t in lines):
                ok_sem = True
                break
            if proc.poll() is not None:
                raise RuntimeError("daemon exited during warm-up")
            time.sleep(2)
        lines = tail.snapshot()
        raw({"kind": "warm", "lang": lang, "semantic_complete": ok_sem, "secs": round(time.time() - t0, 1),
             "log_tail": [t for _, t in lines[-12:]]})
        if not ok_sem:
            # not fatal for the structural tiers, but the semantic tier cannot be measured
            raw({"kind": "warn", "lang": lang, "msg": "no 'semantic pass over ... complete' line within %ds" % a.warm_cap})
        # quiet: no new log line for 15 s
        while True:
            n = len(tail.snapshot())
            time.sleep(15)
            if len(tail.snapshot()) == n:
                break
        idle = idle_gate(proc.pid, 120)
        raw({"kind": "warm_idle", "lang": lang, "idle": idle, "machine": machine_state(proc.pid)})
        # plugin lines reach the log?
        raw({"kind": "tools_list", "lang": lang, "tools": [t.get("name") for t in shim.request("tools/list", {}).get("tools", [])]})
        shim_pool = [Shim(E.bin, root, E.env) for _ in range(3)]
        shims += shim_pool
        for s in shim_pool:
            s.call("get_file_outline", {"file_path": warm_cfg["file"]})
        originals = {k: open(os.path.join(root, spec[k]["file"])).read() for k in arms}
        orig_sha = {k: hashlib.sha256(originals[k].encode()).hexdigest() for k in arms}
        # order: warm-up rep (rep 0) then reps 1..N, interleaved over arms
        n_total = a.reps if lang == 'rust' else min(a.reps, 3)
        order = [(0, k) for k in arms] + [(r, k) for r in range(1, n_total + 1) for k in arms]
        for rep, arm in order:
            cfg = spec[arm]
            path = os.path.join(root, cfg["file"])
            if sha(path) != orig_sha[arm]:
                raise RuntimeError("sha mismatch before %s rep %d" % (arm, rep))
            idle = idle_gate(proc.pid, 60)
            ms = machine_state(proc.pid)
            stamp = time.strftime("%H%M%S")
            salt = "run %s rep %d" % (stamp, rep)
            new_text = make_edit(originals[arm], cfg, salt)
            res = {}
            stop = threading.Event()
            ths = run_observers(E, cfg, root, shim_pool, None, res, stop)
            time.sleep(0.3)  # let the db observer take its pre-edit snapshot
            t_save, t_closed, t_after = write_file(path, new_text)
            deadline = t_save + TIER_TIMEOUT
            need = ("a_db", "a_tool", "b_db", "b_tool", "c_db", "c_tool")
            while time.time() < deadline and not all(k in res for k in need):
                time.sleep(0.05)
            time.sleep(0.5)
            stop.set()
            for t in ths:
                t.join(timeout=5)
            missing = [k for k in need if k not in res]
            # wait for the semantic_done of this edit (tail of the pass) before restoring
            sd_end = time.time() + 30
            while time.time() < sd_end:
                evs = [e for e in events(tail.snapshot()) if e["t"] >= t_save - 0.01]
                if first(evs, "semantic_done", t_save):
                    break
                time.sleep(0.1)
            time.sleep(0.5)
            ms_after = machine_state(proc.pid)
            # restore, observed like an edit
            rres = {}
            r_save, _, _ = write_file(path, originals[arm])
            rend = time.time() + 45
            r_ok = False
            while time.time() < rend:
                evs = [e for e in events(tail.snapshot()) if e["t"] >= r_save - 0.01]
                if first(evs, "semantic_done", r_save):
                    r_ok = True
                    break
                time.sleep(0.05)
            time.sleep(1.0)
            all_ev = events(tail.snapshot())
            evs = [e for e in all_ev if t_save - 0.01 <= e["t"] < r_save]
            revs = [e for e in all_ev if e["t"] >= r_save - 0.01]
            P = phases(evs, t_save, cfg["file"])
            RP = phases(revs, r_save, cfg["file"])
            reindex = any("reindexing all of" in t for arr, t in tail.snapshot() if arr >= t_save)
            tiers = {}
            for k in need:
                if k in res and res[k] is not None:
                    tiers[k] = round(res[k] - t_save, 4)
            row = {"kind": "rep", "lang": lang, "arm": arm, "rep": rep, "warmup": rep == 0, "salt": salt,
                   "t_save": t_save, "close_ms": round((t_closed - t_save) * 1000, 3), "tiers": tiers, "missing": missing,
                   "pre_vectors": res.get("pre_vectors"), "b_not_ranked": res.get("b_not_ranked", False), "b_top10": res.get("b_top10"), "pre_c": res.get("pre_c"), "db_errors": res.get("db_errors", [])[:3],
                   "rtt_ms": {k[4:]: [round(x * 1000, 1) for x in v] for k, v in res.items() if k.startswith("rtt_")},
                   "phases": {k: (round(v, 4) if isinstance(v, float) else v) for k, v in P.items()},
                   "restore": {"ok": r_ok, "phases": {k: (round(v, 4) if isinstance(v, float) else v) for k, v in RP.items()}},
                   "machine_before": ms, "machine_after": ms_after, "idle_before": idle,
                   "n_events": len(evs), "reindex_line": reindex, "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()}
            raw(row)
            E.prog("GM-435 measure · %s %s %d/%d · a=%s b=%s c=%s" % (lang, arm, rep, n_total, tiers.get("a_tool"), tiers.get("b_tool"), tiers.get("c_tool")))
            if missing:
                status = "tier not reached: %s (%s rep %d)" % (missing, arm, rep)
                break
            if reindex:
                status = "whole-language reindex line seen"
                break
            if sha(path) != orig_sha[arm]:
                status = "sha mismatch after restore"
                break
    except Exception as e:
        status = "exception: %r" % (e,)
        import traceback
        raw({"kind": "trace", "lang": lang, "tb": traceback.format_exc()})
    finally:
        for s in shims:
            try:
                s.close()
            except Exception:
                pass
        tail.stop = True
        left = stop_daemon(proc, E, root)
        raw({"kind": "stopped", "lang": lang, "status": status, "left_processes": left})
        # restore files in the worktree no matter what
        subprocess.run(["git", "-C", root, "checkout", "--", "."], capture_output=True)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin"); ap.add_argument("--home"); ap.add_argument("--work")
    ap.add_argument("--out"); ap.add_argument("--progress")
    ap.add_argument("--langs", default="rust,ts,go")
    ap.add_argument("--reps", type=int, default=5)
    ap.add_argument("--only-arm", default="", dest="only_arm")
    ap.add_argument("--warm-cap", type=int, default=2400, dest="warm_cap")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    rawf = open(os.path.join(a.out, "raw.jsonl"), "a", buffering=1)
    def raw(o):
        rawf.write(json.dumps(o) + "\n")
    E = Env(a)
    raw({"kind": "start", "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip(),
         "ncpu": os.cpu_count(), "args": vars(a)})
    for lang in a.langs.split(","):
        run_language(E, lang, a, raw)
    raw({"kind": "end", "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()})

if __name__ == "__main__":
    main()
