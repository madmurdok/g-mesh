"""Headless Claude Code driver for GM-456 generation and judging
(docs/design/GM-456-training-data.md, section 2b).

One `claude -p` call per pack, with no tools, no MCP servers, no settings
sources (so no user CLAUDE.md, hooks or skills), the step's prompt as the
whole system prompt, and a fresh empty cwd outside any git repo
(`blind_flags`, `sandbox`). The CLI's JSON result is the artefact of record:
it is stored as `<step_dir>/<custom_id>.json` and turned into the message
dict the steps' `parse_message` read (`to_message`).

Outcomes (`classify`): success, refusal, malformed, limit, transient, unblind.
Every stored response (`<custom_id>.json` plus `<custom_id>.req-sha256`) and
recorded failure (`<custom_id>.err.json`) carries the sha256 of the request it
answers (`request_sha`); one whose request changed or left the request set is
stale: `run_step` moves it to `<step_dir>/stale/<time>/` and the pack reruns,
and `stored` never returns it to a parser.
`run_packs` never reruns a pack that has a current stored response or recorded
failure unless `--retry` names it; retries a transient failure once; stops the
whole run, starting no new pack, on a rate or usage limit or on a call that was
not blind. A transient failure whose retry the stop prevented is not recorded,
so a resume runs it again.
"""

import json
import os
import re
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import common as C

CLAUDE = os.environ.get("GM456_CLAUDE", "claude")
TIMEOUT = 1200
# A schema-constrained call has exactly this tool; nothing else may be listed.
ALLOWED_TOOLS = ("StructuredOutput",)
# Context tokens of the probe call. Measured: 1.1k with these flags, 6.0k when
# the user CLAUDE.md loads; above this the run stops.
PROBE_MAX_CONTEXT = 3000
PROBE_SCHEMA = {"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"],
                "additionalProperties": False}
LIMIT_TEXT = ("rate limit", "rate_limit", "usage limit", "limit reached", "hit your limit", "limit will reset",
              "out of extra usage", "quota")
LIMIT_STATUS = re.compile(r"\b429\b")
# Exit code of a resumable stop (usage limit, unblind or failed probe):
# relaunching the same command later resumes it. Config STOPs (sandbox root
# under .git/CLAUDE.md, no free sandbox dir, unknown --retry ids, stored
# responses of another prompt/schema/effort/model, invalid custom_id) exit 1:
# the same command fails the same way until someone changes an input.
STOP_EXIT = 3
OUTCOMES = ("success", "refusal", "malformed", "limit", "transient", "unblind")


def blind_flags():
    return ["--tools", "", "--strict-mcp-config", "--setting-sources", "", "--disable-slash-commands",
            "--no-session-persistence", "--permission-prompts", "none"]


def schema_text(schema):
    return json.dumps(schema, sort_keys=True, separators=(",", ":"))


def argv(model, effort, schema, prompt, output_format="json"):
    out = [CLAUDE, "-p", "--model", model, "--effort", effort, "--output-format", output_format,
           "--json-schema", schema_text(schema), "--system-prompt", prompt] + blind_flags()
    if output_format == "stream-json":
        out.append("--verbose")
    return out


def argv_template(model, effort):
    out = argv(model, effort, {}, "<prompt>")
    out[out.index("--json-schema") + 1] = "<schema>"
    return out


def claude_version():
    try:
        return subprocess.run([CLAUDE, "--version"], capture_output=True, text=True, timeout=60).stdout.strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unknown ({e})"


def default_root(version):
    return Path(os.environ.get("TMPDIR", "/tmp")) / f"gm456-{version}"


def sandbox(cwd_root, name):
    """A fresh empty dir `<cwd_root>/<name>-<n>`; refuses a root inside a git
    repo or under a CLAUDE.md, which the CLI would discover."""
    root = Path(cwd_root).resolve()
    for p in [root, *root.parents]:
        if (p / ".git").exists() or (p / "CLAUDE.md").exists():
            sys.exit(f"STOP: sandbox root {root} is under {p}, which has .git or CLAUDE.md")
    root.mkdir(parents=True, exist_ok=True)
    for i in range(1000):
        d = root / f"{name}-{i}"
        try:
            d.mkdir()
            return d
        except FileExistsError:
            continue
    sys.exit(f"STOP: no free sandbox dir under {root}")


def call(cmd, stdin, cwd, timeout=TIMEOUT):
    """(parsed stdout or None, rc, stderr) of one CLI call under `taskpolicy -b`."""
    try:
        p = subprocess.run(["taskpolicy", "-b", *cmd], input=stdin, cwd=cwd, capture_output=True, text=True,
                           timeout=timeout)
    except subprocess.TimeoutExpired as e:
        return None, -1, f"timeout after {timeout}s: {(e.stderr or '')[-2000:]}"
    try:
        out = json.loads(p.stdout)
    except ValueError:
        out = None
    return out, p.returncode, (p.stderr or "")[-4000:] + ("" if out is not None else f"\nstdout: {p.stdout[-2000:]}")


def run_pack(request, schema, prompt, cwd_root, model, effort):
    """One CLI call for a pack: (cli_json or None, rc, stderr). The user
    message is the request's `params.messages[0].content`, on stdin."""
    cwd = sandbox(cwd_root, request["custom_id"])
    return call(argv(model, effort, schema, prompt), request["params"]["messages"][0]["content"], cwd)


def is_refusal(cli):
    return cli.get("stop_reason") == "refusal" or "refusal" in str(cli.get("terminal_reason") or "")


def to_message(cli):
    """The message dict `parse_message` reads, from the CLI's JSON result."""
    usage_by_model = cli.get("modelUsage") or {}
    model = max(usage_by_model, key=lambda m: usage_by_model[m].get("outputTokens", 0)) if usage_by_model else None
    so = cli.get("structured_output")
    return {
        "model": model,
        "stop_reason": "refusal" if is_refusal(cli) else "end_turn",
        "stop_details": cli.get("terminal_reason"),
        "content": [] if so is None else [{"type": "text", "text": json.dumps(so)}],
        "usage": cli.get("usage"),
    }


def is_limit_text(text):
    text = text.lower()
    return any(t in text for t in LIMIT_TEXT) or bool(LIMIT_STATUS.search(text))


def classify(cli, rc, stderr=""):
    """One of OUTCOMES, with a reason. More than two turns on a success is not
    a blindness breach: the CLI re-prompts when the structured output fails
    the schema, and with `--tools ""` StructuredOutput is the only tool."""
    if cli is None:
        if is_limit_text(stderr):
            return "limit", f"rc {rc}, no JSON: {stderr.strip()[-300:]}"
        return "transient", f"rc {rc}, no JSON: {stderr.strip()[-300:]}"
    if is_refusal(cli):
        return "refusal", f"terminal_reason {cli.get('terminal_reason')}, stop_reason {cli.get('stop_reason')}"
    if cli.get("is_error") or cli.get("subtype") != "success":
        status = cli.get("api_error_status")
        text = f"{cli.get('result')} {stderr}".lower()
        why = f"subtype {cli.get('subtype')}, api_error_status {status}: {str(cli.get('result'))[:300]}"
        if str(status).strip() == "429" or is_limit_text(text):
            return "limit", why
        if (isinstance(status, int) and status >= 500) or "overloaded" in text or rc != 0:
            return "transient", why
        return "malformed", why
    if cli.get("permission_denials"):
        return "unblind", f"permission_denials {cli.get('permission_denials')}, num_turns {cli.get('num_turns')}"
    if rc != 0:
        return "transient", f"rc {rc} with a success result"
    if cli.get("structured_output") is None:
        return "malformed", "no structured_output"
    return "success", "ok"


def usage_row(custom_id, outcome, attempt, cli):
    cli = cli or {}
    u = cli.get("usage") or {}
    return {"customId": custom_id, "outcome": outcome, "attempt": attempt,
            "model": sorted(cli.get("modelUsage") or {}),
            "usage": {"input": u.get("input_tokens"), "cacheCreation": u.get("cache_creation_input_tokens"),
                      "cacheRead": u.get("cache_read_input_tokens"), "output": u.get("output_tokens"),
                      "thinking": (u.get("output_tokens_details") or {}).get("thinking_tokens")},
            "numTurns": cli.get("num_turns"), "durationMs": cli.get("duration_ms"),
            "totalCostUsd": cli.get("total_cost_usd")}


def request_sha(request):
    """sha256 of what the CLI call for `request` sends: model, effort, system
    prompt, schema and user message."""
    p = request["params"]
    sent = {"model": p["model"], "effort": p["output_config"]["effort"], "system": p["system"][0]["text"],
            "schema": p["output_config"]["format"]["schema"], "user": p["messages"][0]["content"]}
    return C.sha256_bytes(json.dumps(sent, sort_keys=True, separators=(",", ":")).encode())


def pack_files(step_dir, cid):
    """(response, its request-sha256 sidecar, recorded failure) paths."""
    step_dir = Path(step_dir)
    return step_dir / f"{cid}.json", step_dir / f"{cid}.req-sha256", step_dir / f"{cid}.err.json"


def state(request, step_dir):
    """"done" (response for this exact request), "failed" (failure recorded
    for this exact request) or None. Files for another request do not count."""
    out, sha, errf = pack_files(step_dir, request["custom_id"])
    h = request_sha(request)
    if out.exists() and sha.exists() and sha.read_text().strip() == h:
        return "done"
    if errf.exists():
        try:
            if json.load(open(errf)).get("requestSha256") == h:
                return "failed"
        except ValueError:
            pass
    return None


def stored(request, step_dir):
    """(CLI JSON, None) of the current response to `request`, or (None,
    reason): the recorded failure, "not run", or a stale response."""
    out, _, errf = pack_files(step_dir, request["custom_id"])
    st = state(request, step_dir)
    if st == "done":
        return json.load(open(out)), None
    if st == "failed":
        e = json.load(open(errf))
        return None, f"{e['outcome']}: {e['reason']}"
    if out.exists() or errf.exists():
        return None, "stale: stored for another request; rerun the step"
    return None, "not run: None"


def set_aside_stale(requests, step_dir):
    """Moves every response, sidecar and failure that does not belong to a
    current request (changed request, or custom_id no longer in the set) to
    `<step_dir>/stale/<time>/`. Returns the moved file names."""
    step_dir = Path(step_dir)
    if not step_dir.is_dir():
        return []
    keep = set()
    for r in requests:
        out, sha, errf = pack_files(step_dir, r["custom_id"])
        st = state(r, step_dir)
        if st == "done":
            keep |= {out.name, sha.name}
        if st is not None and errf.exists() and json.load(open(errf)).get("requestSha256") == request_sha(r):
            keep.add(errf.name)
    moved = []
    dest = step_dir / "stale" / time.strftime("%Y%m%dT%H%M%S")
    for p in sorted(step_dir.iterdir()):
        if p.is_file() and p.name.endswith((".json", ".req-sha256")) and p.name not in keep:
            dest.mkdir(parents=True, exist_ok=True)
            p.rename(dest / p.name)
            moved.append(p.name)
    return moved


def pending(requests, step_dir, retry=()):
    """Requests to run: no response or failure stored for this exact request,
    or named in `retry`. Unknown `retry` ids stop the run."""
    ids = {r["custom_id"] for r in requests}
    unknown = sorted(set(retry) - ids)
    if unknown:
        sys.exit(f"STOP: --retry names unknown packs {unknown}")
    return [r for r in requests if r["custom_id"] in retry or state(r, step_dir) is None]


def run_packs(requests, step_dir, jobs, schema, prompt, model, effort, cwd_root, usage_path, retry=(),
              runner=None):
    """Runs the pending packs, `jobs` at a time. Returns {outcome: count,
    "skipped": n, "notRun": n, "stopped": reason or None, "costUsd": sum}.
    `runner(request)` replaces `run_pack` (tests)."""
    step_dir = Path(step_dir)
    step_dir.mkdir(parents=True, exist_ok=True)
    todo = pending(requests, step_dir, retry)
    runner = runner or (lambda r: run_pack(r, schema, prompt, cwd_root, model, effort))
    stop = threading.Event()
    lock = threading.Lock()
    summary = {o: 0 for o in OUTCOMES}
    summary.update(skipped=len(requests) - len(todo), notRun=0, stopped=None, costUsd=0.0)

    def log(row):
        with lock:
            with open(usage_path, "a") as f:
                f.write(json.dumps(row) + "\n")
            summary["costUsd"] += row["totalCostUsd"] or 0.0

    def one(req):
        cid = req["custom_id"]
        if stop.is_set():
            with lock:
                summary["notRun"] += 1
            return
        for attempt in (1, 2):
            cli, rc, err = runner(req)
            outcome, why = classify(cli, rc, err)
            log(usage_row(cid, outcome, attempt, cli))
            if outcome == "transient" and attempt == 1 and not stop.is_set():
                print(f"{cid}: transient ({why}); retrying once", flush=True)
                continue
            break
        if outcome == "transient" and attempt == 1:
            # The stop prevented the retry: nothing is recorded, so a resume runs the pack again.
            with lock:
                summary["notRun"] += 1
            print(f"{cid}: transient ({why}); not retried after the stop, left for the resume", flush=True)
            return
        out, sha, errf = pack_files(step_dir, cid)
        if outcome == "success":
            sha.unlink(missing_ok=True)
            C.write_json(out, cli)
            sha.write_text(request_sha(req) + "\n")
            errf.unlink(missing_ok=True)
        elif outcome == "limit":
            stop.set()
            with lock:
                summary["stopped"] = summary["stopped"] or f"limit at {cid}: {why}"
        else:
            if outcome == "unblind":
                stop.set()
                with lock:
                    summary["stopped"] = summary["stopped"] or f"unblind call at {cid}: {why}"
            out.unlink(missing_ok=True)
            sha.unlink(missing_ok=True)
            C.write_json(errf, {"customId": cid, "requestSha256": request_sha(req), "outcome": outcome,
                                "reason": why, "cli": cli,
                                "at": time.strftime("%Y-%m-%dT%H:%M:%S%z")})
        with lock:
            summary[outcome] += 1
        print(f"{cid}: {outcome}" + ("" if outcome == "success" else f" ({why})"), flush=True)

    with ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
        list(pool.map(one, todo))
    summary["costUsd"] = round(summary["costUsd"], 6)
    return summary


def probe_blindness(cwd_root, model, effort="low"):
    """One stream-json call with the run's flags. Returns the record kept in
    the manifest; `ok` is false when the init event lists a tool other than
    ALLOWED_TOOLS, any MCP server, skill or slash command, or the call's
    context exceeds PROBE_MAX_CONTEXT (the user CLAUDE.md loaded)."""
    cwd = sandbox(cwd_root, "probe")
    cmd = argv(model, effort, PROBE_SCHEMA, "Reply with ok.", output_format="stream-json")
    try:
        p = subprocess.run(["taskpolicy", "-b", *cmd], input="Reply with ok.", cwd=cwd, capture_output=True,
                           text=True, timeout=300)
        lines, rc, err = p.stdout.splitlines(), p.returncode, p.stderr[-2000:]
    except subprocess.TimeoutExpired:
        lines, rc, err = [], -1, "timeout"
    init, result = None, None
    for line in lines:
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            init = ev
        elif ev.get("type") == "result":
            result = ev
    rec = {"rc": rc, "init": None, "context": None, "ok": False, "reasons": []}
    if init is None or result is None:
        outcome, why = classify(result, rc, err)
        rec["reasons"].append(f"no init/result event: {outcome} {why}")
        rec["outcome"] = outcome
        return rec
    rec["init"] = {k: init.get(k) for k in ("tools", "mcp_servers", "skills", "slash_commands", "model",
                                            "claude_code_version", "apiKeySource", "cwd")}
    u = result.get("usage") or {}
    rec["context"] = sum(u.get(k) or 0 for k in ("input_tokens", "cache_creation_input_tokens",
                                                 "cache_read_input_tokens"))
    rec["outcome"] = classify(result, rc, err)[0]
    rec["totalCostUsd"] = result.get("total_cost_usd")
    extra = [t for t in init.get("tools") or [] if t not in ALLOWED_TOOLS]
    if extra:
        rec["reasons"].append(f"tools {extra}")
    for k in ("mcp_servers", "skills", "slash_commands"):
        if init.get(k):
            rec["reasons"].append(f"{k} {init.get(k)}")
    if rec["context"] > PROBE_MAX_CONTEXT:
        rec["reasons"].append(f"context {rec['context']} > {PROBE_MAX_CONTEXT}: CLAUDE.md or memory loaded")
    if rec["outcome"] != "success":
        rec["reasons"].append(f"probe outcome {rec['outcome']}")
    rec["ok"] = not rec["reasons"]
    return rec


def stop_run(msg):
    print(msg, file=sys.stderr, flush=True)
    sys.exit(STOP_EXIT)


def run_step(step, requests, resp_dir, args, schema, prompt, prompt_sha, model, effort, pack_size, version):
    """The `--run` of a step: stale responses set aside, blindness probe (when
    anything is pending), packs, run record `<step>-run.json`. Exits 3
    (STOP_EXIT) on a stop (limit, unblind, failed probe), so a pipeline stops
    and is resumed later."""
    resp_dir = Path(resp_dir)
    step_dir = resp_dir / step
    root = Path(args.cwd_root) if args.cwd_root else default_root(version)
    run_path = resp_dir / f"{step}-run.json"
    record = json.load(open(run_path)) if run_path.exists() else {}
    schema_sha = C.sha256_bytes(schema_text(schema).encode())
    on_disk = sum(1 for r in requests if pack_files(step_dir, r["custom_id"])[0].exists())
    same = (record.get("promptSha256"), record.get("schemaSha256"), record.get("effort"),
            record.get("requestedModel")) == (prompt_sha, schema_sha, effort, model)
    if record and on_disk and not same:
        sys.exit(f"STOP {step}: {on_disk} stored responses come from another prompt, schema, effort or model "
                 f"({run_path}); move {step_dir} aside to start over")
    moved = set_aside_stale(requests, step_dir)
    if moved:
        print(f"run {step}: {len(moved)} stale files (request changed or gone) moved to {step_dir / 'stale'}",
              flush=True)
    todo = pending(requests, step_dir, args.retry or ())
    if todo:
        probe = probe_blindness(root, model, effort)
        C.write_json(resp_dir / f"{step}-probe.json", probe)
        print(f"run {step}: probe ok={probe['ok']} tools={(probe['init'] or {}).get('tools')} "
              f"mcp={(probe['init'] or {}).get('mcp_servers')} context={probe['context']}", flush=True)
        if not probe["ok"]:
            if probe.get("outcome") == "limit":
                stop_run(f"STOP {step}: usage limit at the probe; resume later")
            stop_run(f"STOP {step}: blindness probe failed: {probe['reasons']}")
        session = {"at": time.strftime("%Y-%m-%dT%H:%M:%S%z"), "claudeVersion": claude_version(),
                   "pending": len(todo),
                   "probe": {"tools": probe["init"]["tools"], "mcpServers": probe["init"]["mcp_servers"],
                             "context": probe["context"]}}
        record = {
            "driver": "claude-cli",
            "claudeVersion": session["claudeVersion"],
            "effort": effort,
            "requestedModel": model,
            "promptSha256": prompt_sha,
            "schemaSha256": schema_sha,
            "argvTemplate": argv_template(model, effort),
            "packSize": pack_size,
            "probe": session["probe"],
            "sessions": (record.get("sessions", []) if same else []) + [session],
        }
        C.write_json(run_path, record)
    summary = run_packs(requests, step_dir, args.jobs, schema, prompt, model, effort, root,
                        resp_dir / f"{step}-usage.jsonl", args.retry or ())
    print(f"run {step}: " + json.dumps(summary), flush=True)
    if summary["stopped"]:
        stop_run(f"STOP {step}: {summary['stopped']}; {summary['notRun']} packs not run; resume later")
    return record


def run_record(resp_dir, step):
    """The manifest block of a `--run` step, or None when the step never ran
    on the CLI: the run record plus the models seen and the sha256 of every
    stored response (stale ones are under `stale/` and not listed)."""
    resp_dir = Path(resp_dir)
    run_path = resp_dir / f"{step}-run.json"
    if not run_path.exists():
        return None
    rec = json.load(open(run_path))
    files, models = {}, set()
    for p in sorted((resp_dir / step).glob("*.json")):
        if p.name.endswith(".err.json"):
            continue
        files[p.name] = C.sha256_file(p)
        models.update((json.load(open(p)).get("modelUsage") or {}).keys())
    return dict(rec, models=sorted(models), responses=files)


def add_run_args(ap):
    ap.add_argument("--jobs", type=int, default=3)
    ap.add_argument("--retry", nargs="+", metavar="CUSTOM_ID")
    ap.add_argument("--cwd-root", help="root of the per-call sandbox dirs (default $TMPDIR/gm456-<version>)")
