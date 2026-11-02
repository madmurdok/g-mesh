#!/usr/bin/env python3
"""GM-692: the GM-690 fresh-function repro, with the cross-encoder installed. Reuses measure.py (GM-435 harness, unchanged).

Changes from the GM-690 driver: restart sessions gate on the index being ready (status --json) when the
rust semantic pass log line never comes; per-rep rerank evidence; N sessions with first-query reps; progress."""
import argparse, base64, json, math, os, sqlite3, struct, subprocess, sys, threading, time, traceback
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import measure as m

QUERY = m.QUERY
ALPHA = "edit_probe_alpha"


def parse_page(text):
    """(rank of alpha 1-based or None, alpha score, list of scores, names)."""
    try:
        obj = json.loads(text)
    except ValueError:
        # partial pages lead with a note block; take the last JSON line
        obj = None
        for part in reversed(text.split("\n")):
            try:
                obj = json.loads(part)
                break
            except ValueError:
                continue
        if obj is None:
            return None, None, [], [], {}
    res = obj.get("results", []) if isinstance(obj, dict) else []
    rank, score = None, None
    for i, r in enumerate(res):
        if str(r.get("qualifiedName", "")).endswith(ALPHA):
            rank, score = i + 1, r.get("score")
            break
    extra = {k: obj.get(k) for k in ("noMatch", "no_match", "lowSimilarity", "partial", "hint") if isinstance(obj, dict) and obj.get(k) is not None}
    if isinstance(obj, dict) and obj.get("nextCursor"):
        extra["cursor_prefix"] = str(obj["nextCursor"])[:7]
    return rank, score, [round(r.get("score", 0), 4) for r in res], [r.get("qualifiedName") for r in res], extra


def monotone_desc(scores):
    return all(a >= b for a, b in zip(scores, scores[1:]))


def unpack(blob):
    return struct.unpack("<%df" % (len(blob) // 4), blob)


def cos(a, b):
    if not a or not b or len(a) != len(b):
        return None
    d = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a)); nb = math.sqrt(sum(y * y for y in b))
    return d / (na * nb) if na and nb else None


def db_nodes(dbfile, file):
    con = sqlite3.connect("file:%s?mode=ro" % dbfile, uri=True, timeout=2)
    rows = con.execute(
        "SELECT n.id, n.name, n.docComment, n.signature, n.startLine, v.embedding, v.embeddingVersion "
        "FROM nodes n LEFT JOIN vectors v ON v.nodeId=n.id WHERE n.filePath=? AND n.name LIKE 'edit_probe_%'", [file]).fetchall()
    nvec = con.execute("SELECT count(*) FROM vectors").fetchone()[0]
    con.close()
    return rows, nvec


def run_observers(E, cfg, shim_pool, res, stop):
    """measure.run_observers with a recording search poller."""
    style, file = cfg["style"], cfg["file"]
    new = m.NEW_NAMES[style]
    gamma, removed = new[2], cfg["removed"]
    lock = threading.Lock()

    def put(k, v):
        with lock:
            res.setdefault(k, v)

    def db_thread():
        con = sqlite3.connect("file:%s?mode=ro" % E.dbfile, uri=True, timeout=1)
        ph_new = ",".join("?" * 3); ph_rm = ",".join("?" * len(removed))
        q_a1 = "SELECT count(*) FROM nodes WHERE filePath=? AND name IN (%s)" % ph_new
        q_a2 = "SELECT count(*) FROM nodes WHERE filePath=? AND COALESCE(nativeKind,'')<>'pending_symbol' AND name IN (%s)" % ph_rm
        q_b = "SELECT n.name, v.embedding FROM vectors v JOIN nodes n ON n.id=v.nodeId WHERE n.filePath=? AND n.name IN (%s)" % ph_new
        q_c = ("SELECT 1 FROM edges e JOIN nodes f ON f.id=e.fromId JOIN nodes t ON t.id=e.toId "
               "WHERE f.filePath=? AND f.name=? AND t.name=? AND e.source='semantic' LIMIT 1")
        pre = dict(con.execute(q_b, [file] + new).fetchall())
        res["pre_vectors"] = len(pre)
        con.rollback()
        while not stop.is_set():
            try:
                if "a_db" not in res:
                    if con.execute(q_a1, [file] + new).fetchone()[0] == 3 and con.execute(q_a2, [file] + removed).fetchone()[0] == 0:
                        put("a_db", time.time())
                if "b_db" not in res:
                    rows = dict(con.execute(q_b, [file] + new).fetchall())
                    if len(rows) == 3 and all(rows[k] != pre.get(k) for k in rows):
                        put("b_db", time.time())
                        res["alpha_vec_at_b"] = base64.b64encode(rows[ALPHA]).decode()
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
        while not stop.is_set():
            if "a_tool_new" not in res:
                err, text, t0, t1 = sh.call("find_definition", {"symbol_name": gamma})
                if not err and m.has_decl(text, gamma, file):
                    put("a_tool_new", t1)
            if "a_tool_gone" not in res:
                err, text, t0, t1 = sh.call("find_definition", {"symbol_name": removed[0]})
                if err or not m.has_decl(text, removed[0], file):
                    put("a_tool_gone", t1)
            if "a_tool_new" in res and "a_tool_gone" in res:
                put("a_tool", max(res["a_tool_new"], res["a_tool_gone"]))
                return
            time.sleep(0.025)

    def tool_search():
        sh = shim_pool[1]
        calls = res.setdefault("search_calls", [])
        while not stop.is_set() and "b_db" not in res:
            time.sleep(0.005)
        t_first = time.time()
        while not stop.is_set():
            err, text, t0, t1 = sh.call("search_code", {"query": QUERY, "limit": 10})
            rank, score, scores, names, extra = parse_page(text)
            calls.append({"t0": t0, "t1": t1, "rank": rank, "score": score, "err": err})
            if len(calls) == 1:
                res["first_page"] = {"rank": rank, "score": score, "scores": scores, "names": names, "extra": extra,
                                     "monotone": monotone_desc(scores), "secs": round(t1 - t0, 3)}
            if not err and m.NEW_NAMES[style][0] in text:
                res["found_page"] = {"rank": rank, "score": score, "scores": scores, "names": names, "extra": extra,
                                     "monotone": monotone_desc(scores), "secs": round(t1 - t0, 3)}
                put("b_tool", t1)
                return
            if t1 - t_first > 5.0:
                res["b_not_ranked"] = True
                res["miss_page"] = {"scores": scores, "names": names, "extra": extra, "text": text[:4000]}
                put("b_tool", None)
                return
            time.sleep(0.1)

    def tool_callees():
        sh = shim_pool[2]
        while not stop.is_set() and "c_db" not in res:
            time.sleep(0.005)
        while not stop.is_set():
            err, text, t0, t1 = sh.call("find_callees", {"symbol_name": gamma})
            if not err and cfg["callee"] in text:
                put("c_tool", t1)
                return
            time.sleep(0.05)

    ths = [threading.Thread(target=f, daemon=True) for f in (db_thread, tool_struct, tool_search, tool_callees)]
    for t in ths:
        t.start()
    return ths


def diagnostics(E, cfg, shim, ref, miss):
    """After the tiers: db rows, vector cosines, a wide late search."""
    d = {}
    rows, nvec = db_nodes(E.dbfile, cfg["file"])
    d["n_vectors_total"] = nvec
    vecs = {}
    for (nid, name, doc, sig, line, emb, ver) in rows:
        d.setdefault("nodes", []).append({"name": name, "id": nid, "line": line, "doc": (doc or "")[:120], "sig": (sig or "")[:80],
                                          "has_vec": emb is not None, "ver": ver})
        if emb is not None:
            vecs[name] = unpack(emb)
    a = vecs.get(ALPHA)
    if a is not None:
        d["alpha_norm"] = round(math.sqrt(sum(x * x for x in a)), 4)
        d["alpha_dim"] = len(a)
        d["cos_alpha_beta"] = round(cos(a, vecs.get("edit_probe_beta")) or 0, 4)
        if ref.get(cfg["file"]) is not None:
            d["cos_alpha_ref"] = round(cos(a, ref[cfg["file"]]) or 0, 4)
        else:
            ref[cfg["file"]] = a
            d["cos_alpha_ref"] = 1.0
    err, text, t0, t1 = shim.call("search_code", {"query": QUERY, "limit": 60})
    rank, score, scores, names, extra = parse_page(text)
    # cutoff = the 10th highest cosine in the 30-row window (what the top 10 would be without the rerank)
    w = sorted(scores[:30], reverse=True)
    d["late60"] = {"rank": rank, "score": score, "top3": scores[:3], "s10": scores[9] if len(scores) >= 10 else None,
                   "s30": scores[29] if len(scores) >= 30 else None, "cutoff10": w[9] if len(w) >= 10 else None,
                   "monotone": monotone_desc(scores[:30]), "cursor_prefix": extra.get("cursor_prefix"), "secs": round(t1 - t0, 3)}
    if miss:
        doc_q = "Tallies the lantern shipments waiting in the quartz harbour ledger."
        err, text, t0, t1 = shim.call("search_code", {"query": doc_q, "limit": 10})
        r2 = parse_page(text)
        d["docq10"] = {"rank": r2[0], "score": r2[1], "scores": r2[2]}
        time.sleep(10)
        err, text, t0, t1 = shim.call("search_code", {"query": QUERY, "limit": 10})
        r3 = parse_page(text)
        d["after10s"] = {"rank": r3[0], "score": r3[1], "scores": r3[2], "names": r3[3]}
    return d


class Progress:
    """Flushed progress line every ~10% of the reps or 5 minutes, to the separate progress log."""
    def __init__(self, E, total):
        self.E, self.total, self.done = E, total, 0
        self.t0 = time.time(); self.last_t = 0.0; self.last_pct = -1

    def tick(self, stage, force=False):
        now = time.time(); pct = int(100 * self.done / self.total)
        if not force and now - self.last_t < 300 and pct // 10 == self.last_pct // 10:
            return
        el = now - self.t0
        eta = (el / self.done * (self.total - self.done)) if self.done else -1
        try:
            load = os.getloadavg()[0]
        except OSError:
            load = -1
        self.E.prog("GM-692 %s · %d/%d (%d%%) · elapsed %ds · ETA %s · load1 %.1f" % (
            stage, self.done, self.total, pct, el, ("%ds" % eta) if eta >= 0 else "?", load))
        self.last_t, self.last_pct = now, pct


def tree_cpu(pid):
    """Cumulative cpu seconds (ps cputime, user+sys) of the daemon and its descendants."""
    out = subprocess.run(["ps", "-Ao", "pid=,ppid=,cputime="], capture_output=True, text=True).stdout
    rows = []
    for l in out.splitlines():
        p = l.split()
        if len(p) == 3:
            t = 0.0
            for part in p[2].split(":"):
                t = t * 60 + float(part)
            rows.append((int(p[0]), int(p[1]), t))
    keep, grew = {pid}, True
    while grew:
        grew = False
        for r in rows:
            if r[1] in keep and r[0] not in keep:
                keep.add(r[0]); grew = True
    return round(sum(r[2] for r in rows if r[0] in keep), 2)


def index_ready(E, root):
    """Restart gate: the index is complete (bulk indexed, phase ready, every embedding done)."""
    try:
        st = subprocess.run([E.bin, "status", "--json"], cwd=root, env=E.env, capture_output=True, text=True, timeout=120)
        ix = json.loads(st.stdout)["index"]
        emb = ix["progress"]["embeddings"]
        return bool(ix.get("bulkIndexed")) and ix.get("phase") == "ready" and emb["total"] > 0 and emb["done"] == emb["total"]
    except (ValueError, KeyError, TypeError, subprocess.SubprocessError):
        return False


def session(E, a, raw, sess, plan, ref, PG):
    root = os.path.join(a.work, "P")
    spec = m.ARMS["rust"]
    proc, logp = m.start_daemon(E, root, "rust-s%d" % sess)
    E.daemon_pid = proc.pid
    raw({"kind": "daemon", "session": sess, "pid": proc.pid, "log": logp})
    with open(os.path.join(a.out, "pids.txt"), "a") as f:
        f.write("%d\n" % proc.pid)
    tail = m.LogTail(logp); tail.start()
    shims, status = [], "ok"
    searched = False
    try:
        t0 = time.time()
        shim = m.Shim(E.bin, root, E.env); shims.append(shim)
        err, text, _, _ = shim.call("get_file_outline", {"file_path": spec["B"]["file"]})
        raw({"kind": "first_call", "session": sess, "secs": round(time.time() - t0, 1), "err": err})
        E.dbfile = None
        for _ in range(200):
            c = m.db_path(E.home)
            if c:
                E.dbfile = max(c, key=os.path.getmtime); break
            time.sleep(0.5)
        end = time.time() + a.warm_cap
        ok_sem = False
        warm_via = None
        ready_hits = 0; last_check = 0.0
        while time.time() < end:
            if any("rust semantic pass over" in t and "complete" in t for _, t in tail.snapshot()):
                ok_sem = True; warm_via = "log"; break
            if proc.poll() is not None:
                raise RuntimeError("daemon exited during warm-up")
            # GM-692 harness fix: after a restart on an already indexed project the daemon runs no rust semantic
            # pass, so that log line never comes (GM-690 session 2 timed out after 2400 s). Gate on the index
            # being ready instead (two consecutive status checks), for every session (a cold start still needs a fully embedded ready index).
            if time.time() - last_check > 10:
                last_check = time.time()
                ready_hits = ready_hits + 1 if index_ready(E, root) else 0
                if ready_hits >= 2:
                    ok_sem = True; warm_via = "status-ready"; break
            PG.tick("s%d warm-up" % sess)
            time.sleep(2)
        # embeddings done: status --json via the cli
        st = subprocess.run([E.bin, "status", "--json"], cwd=root, env=E.env, capture_output=True, text=True, timeout=120)
        raw({"kind": "warm", "session": sess, "warm_via": warm_via, "semantic_complete": ok_sem, "secs": round(time.time() - t0, 1), "status": st.stdout[:1500]})
        if not ok_sem:
            raise RuntimeError("no rust semantic pass complete within warm cap")
        while True:
            n = len(tail.snapshot()); time.sleep(15)
            if len(tail.snapshot()) == n:
                break
        # GM-692 harness fix 2: a restarted daemon (index already built) registers its file watcher some
        # 1-2 minutes after the index reports ready; an edit before that is never seen (GM-692 first full run,
        # rep 0 "tier not reached"). Canary: rewrite one project file with its own bytes until an fs_event
        # for it shows in the daemon's timing log, then wait for the log to go quiet again.
        canary = os.path.join(root, spec["B"]["file"])
        canary_bytes = open(canary, "rb").read()
        seen = False
        for attempt in range(40):
            n0 = len([1 for _, t in tail.snapshot() if "fs_event" in t])
            with open(canary, "wb") as f:
                f.write(canary_bytes)
            for _ in range(15):
                time.sleep(1)
                if len([1 for _, t in tail.snapshot() if "fs_event" in t]) > n0:
                    seen = True; break
            if seen:
                break
            PG.tick("s%d watcher canary" % sess)
        raw({"kind": "watcher_canary", "session": sess, "seen": seen, "attempts": attempt + 1, "secs": round(time.time() - t0, 1)})
        if not seen:
            raise RuntimeError("file watcher never reported the canary rewrite")
        while True:
            n = len(tail.snapshot()); time.sleep(15)
            if len(tail.snapshot()) == n:
                break
        idle = m.idle_gate(proc.pid, 120)
        raw({"kind": "warm_idle", "session": sess, "idle": idle, "machine": m.machine_state(proc.pid)})
        shim_pool = [m.Shim(E.bin, root, E.env) for _ in range(3)]
        shims += shim_pool
        for s in shim_pool:
            s.call("get_file_outline", {"file_path": spec["B"]["file"]})
        originals = {k: open(os.path.join(root, spec[k]["file"])).read() for k in ("A", "B")}
        orig_sha = {k: m.hashlib.sha256(originals[k].encode()).hexdigest() for k in ("A", "B")}
        for rep, arm in plan:
            cfg = spec[arm]
            path = os.path.join(root, cfg["file"])
            if m.sha(path) != orig_sha[arm]:
                raise RuntimeError("sha mismatch before %s rep %d" % (arm, rep))
            idle = m.idle_gate(proc.pid, 60)
            ms = m.machine_state(proc.pid)
            rep_t0 = time.time(); cpu0 = tree_cpu(proc.pid)
            salt = "run %s rep %d" % (time.strftime("%H%M%S"), rep)
            new_text = m.make_edit(originals[arm], cfg, salt)
            res = {}
            stop = threading.Event()
            first_query = not searched
            ths = run_observers(E, cfg, shim_pool, res, stop)
            time.sleep(0.3)
            t_save, t_closed, _ = m.write_file(path, new_text)
            need = ("a_db", "a_tool", "b_db", "b_tool", "c_db", "c_tool")
            deadline = t_save + m.TIER_TIMEOUT
            while time.time() < deadline and not all(k in res for k in need):
                time.sleep(0.05)
            time.sleep(0.5)
            stop.set()
            for t in ths:
                t.join(timeout=5)
            searched = searched or bool(res.get("search_calls"))
            missing = [k for k in need if k not in res]
            sd_end = time.time() + 30
            while time.time() < sd_end:
                evs = [e for e in m.events(tail.snapshot()) if e["t"] >= t_save - 0.01]
                if m.first(evs, "semantic_done", t_save):
                    break
                time.sleep(0.1)
            time.sleep(0.5)
            miss = bool(res.get("b_not_ranked"))
            diag = diagnostics(E, cfg, shim_pool[1], ref, miss)
            # restore
            r_save, _, _ = m.write_file(path, originals[arm])
            rend = time.time() + 45; r_ok = False
            while time.time() < rend:
                evs = [e for e in m.events(tail.snapshot()) if e["t"] >= r_save - 0.01]
                if m.first(evs, "semantic_done", r_save):
                    r_ok = True; break
                time.sleep(0.05)
            time.sleep(1.0)
            all_ev = m.events(tail.snapshot())
            evs = [e for e in all_ev if t_save - 0.01 <= e["t"] < r_save]
            P = m.phases(evs, t_save, cfg["file"])
            log_lines = [t for arr, t in tail.snapshot() if t_save - 0.5 <= arr < r_save and "timing:" not in t]
            calls = res.get("search_calls", [])
            row = {"kind": "rep", "session": sess, "arm": arm, "rep": rep, "first_query": first_query, "salt": salt,
                   "t_save": t_save, "tiers": {k: round(res[k] - t_save, 4) for k in need if res.get(k) is not None},
                   "missing": missing, "pre_vectors": res.get("pre_vectors"), "miss": miss,
                   "n_search": len(calls), "first_search_t": round(calls[0]["t0"] - t_save, 4) if calls else None,
                   "first_page": res.get("first_page"), "found_page": res.get("found_page"), "miss_page": res.get("miss_page"),
                   "search_calls": [{"t0": round(c["t0"] - t_save, 3), "t1": round(c["t1"] - t_save, 3), "rank": c["rank"], "score": c["score"]} for c in calls],
                   "diag": diag, "phases": {k: (round(v, 4) if isinstance(v, float) else v) for k, v in P.items()},
                   "restore_ok": r_ok, "machine_before": ms, "idle_before": idle, "log_other": log_lines[:40],
                   "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip(),
                   "real_s": round(time.time() - rep_t0, 1), "daemon_cpu_s": round(tree_cpu(proc.pid) - cpu0, 2)}
            raw(row)
            PG.done += 1
            PG.tick("s%d %s rep %d miss=%s" % (sess, arm, rep, miss))
            if [k for k in missing if k != "b_tool"]:
                status = "tier not reached: %s (%s rep %d)" % (missing, arm, rep); break
            if m.sha(path) != orig_sha[arm]:
                status = "sha mismatch after restore"; break
    except Exception as e:
        status = "exception: %r" % (e,)
        raw({"kind": "trace", "session": sess, "tb": traceback.format_exc()})
    finally:
        for s in shims:
            try:
                s.close()
            except Exception:
                pass
        tail.stop = True
        left = m.stop_daemon(proc, E, root)
        raw({"kind": "stopped", "session": sess, "status": status, "left_processes": left})
        subprocess.run(["git", "-C", root, "checkout", "--", "."], capture_output=True)
    return status


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin"); ap.add_argument("--home"); ap.add_argument("--work")
    ap.add_argument("--out"); ap.add_argument("--progress")
    ap.add_argument("--warm-cap", type=int, default=2400, dest="warm_cap")
    ap.add_argument("--rerank-dir", dest="rerank_dir", help="G_MESH_RERANK_MODEL_DIR (the cross-encoder)")
    ap.add_argument("--only-reps", type=int, default=0, dest="only_reps", help="dry run: stop after this many reps of session 1")
    ap.add_argument("--restarts", type=int, default=4, help="restart sessions after session 1 (each: first-query rep + 1)")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    rawf = open(os.path.join(a.out, "raw.jsonl"), "a", buffering=1)
    raw = lambda o: rawf.write(json.dumps(o) + "\n")
    E = m.Env(a)
    if a.rerank_dir:
        E.env["G_MESH_RERANK_MODEL_DIR"] = a.rerank_dir
    E.env.pop("G_MESH_RERANK", None)
    raw({"kind": "start", "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip(), "ncpu": os.cpu_count()})
    ref = {}
    # session 1: first-query B rep, 6 B reps, then 6 interleaved A/B (GM-435's order) = 19 edits;
    # then a restart per extra first-query rep (each: first-query B rep + 1 B rep).
    plan1 = [(0, "B")] + [(r, "B") for r in range(1, 7)] + [(r, k) for r in range(7, 13) for k in ("A", "B")]
    plans = [plan1] + [[(13 + 2 * i, "B"), (14 + 2 * i, "B")] for i in range(a.restarts)]
    if a.only_reps:
        plans = [plan1[:a.only_reps]]
    PG = Progress(E, sum(len(p) for p in plans))
    st = "ok"
    for n, plan in enumerate(plans, 1):
        if st != "ok":
            break
        st = session(E, a, raw, n, plan, ref, PG)
    raw({"kind": "end", "status": st, "uptime": subprocess.run(["uptime"], capture_output=True, text=True).stdout.strip()})


if __name__ == "__main__":
    main()
