"""Tests of the GM-456 `claude -p` generation/judging route (claude_cli.py and
the `--run` paths of generate.py and judge.py; export.unjudged as the gate).

No real `claude` is ever called: `claude_cli.CLAUDE` points at a fake
executable written into a temp dir. The fake answers per scenario from a
spec (canned CLI JSON, exit code, stderr, delay), counts its calls per rule
with O_EXCL files (so concurrent calls get distinct indexes) and logs every
call (argv, cwd, stdin). The data dir is a temp $GM456_DATA.

Assertions are only those the code guarantees: no order between concurrent
calls; the stop-on-limit race is pinned with events, not sleeps.

Run: uv run --with pytest --with tokenizers pytest eval/training/test_gm456_claude_cli.py -q
"""

import glob
import json
import os
import shutil
import stat
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import claude_cli as CC  # noqa: E402
import common as C  # noqa: E402
import export as X  # noqa: E402
import generate as G  # noqa: E402
import judge as J  # noqa: E402

FAKE_SRC = '''#!{python}
import json, os, sys, time
d = os.environ["FAKE_DIR"]
argv = sys.argv[1:]
stdin = sys.stdin.read()


def claim(sub, prefix):
    os.makedirs(os.path.join(d, sub), exist_ok=True)
    k = 0
    while True:
        try:
            os.close(os.open(os.path.join(d, sub, "%s.%d" % (prefix, k)), os.O_CREAT | os.O_EXCL | os.O_WRONLY))
            return k
        except FileExistsError:
            k += 1


n = claim("seq", "call")
os.makedirs(os.path.join(d, "log"), exist_ok=True)
kind = "version" if "--version" in argv else "probe" if "stream-json" in argv else "pack"
json.dump({{"kind": kind, "argv": argv, "cwd": os.getcwd(), "stdin": stdin}},
          open(os.path.join(d, "log", "call.%d.json" % n), "w"))
spec = json.load(open(os.path.join(d, "spec.json")))
if kind == "version":
    print("9.9.9 (fake)")
    sys.exit(0)
if kind == "probe":
    p = spec["probe"]
    for ev in p["events"]:
        print(json.dumps(ev))
    sys.exit(p.get("rc", 0))
step = {{}}
for i, rule in enumerate(spec["rules"]):
    if rule["match"] in stdin:
        k = claim("cnt", "r%d" % i)
        step = rule["steps"][min(k, len(rule["steps"]) - 1)]
        break
time.sleep(step.get("sleep", 0))
out = step.get("stdout")
if out is not None:
    sys.stdout.write(out if isinstance(out, str) else json.dumps(out))
sys.stderr.write(step.get("stderr", ""))
sys.exit(step.get("rc", 0))
'''


def cli_ok(structured, turns=1, cost=0.01, **extra):
    d = {"type": "result", "subtype": "success", "is_error": False, "structured_output": structured,
         "num_turns": turns, "terminal_reason": "completed", "stop_reason": "end_turn", "total_cost_usd": cost,
         "duration_ms": 5, "usage": {"input_tokens": 3, "cache_creation_input_tokens": 10,
                                     "cache_read_input_tokens": 0, "output_tokens": 7},
         "modelUsage": {"claude-fake": {"outputTokens": 7}}}
    d.update(extra)
    return d


def cli_err(status=None, result="boom", **extra):
    d = {"type": "result", "subtype": "error_during_execution", "is_error": True, "result": result,
         "api_error_status": status, "num_turns": 1, "total_cost_usd": 0.0}
    d.update(extra)
    return d


LIMIT = {"stdout": cli_err(429, "rate limit"), "rc": 1}
TRANSIENT = {"stdout": None, "rc": 1, "stderr": "connection reset"}


def probe_events(tools=("StructuredOutput",), context=1000, mcp=()):
    return [{"type": "system", "subtype": "init", "tools": list(tools), "mcp_servers": list(mcp), "skills": [],
             "slash_commands": [], "model": "claude-fake", "cwd": "/x"},
            {"type": "result", "subtype": "success", "is_error": False, "structured_output": {"ok": True},
             "num_turns": 1, "total_cost_usd": 0.001,
             "usage": {"input_tokens": context, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}]


def mk_request(cid, user, system="sys", model="m", effort="low"):
    return {"custom_id": cid, "targets": [],
            "params": {"model": model, "system": [{"text": system}],
                       "messages": [{"role": "user", "content": user}],
                       "output_config": {"effort": effort, "format": {"schema": {"type": "object"}}}}}


class FakeBase(unittest.TestCase):
    """A temp dir with the fake `claude`, a temp $GM456_DATA and a sandbox root."""

    def setUp(self):
        if shutil.which("taskpolicy") is None:
            self.skipTest("taskpolicy (macOS) not available")
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        self.fake_dir = self.root / "fake"
        self.fake_dir.mkdir()
        self.script = self.fake_dir / "claude"
        self.script.write_text(FAKE_SRC.format(python=sys.executable))
        self.script.chmod(self.script.stat().st_mode | stat.S_IXUSR)
        self.data = self.root / "data"
        self.data.mkdir()
        self.cwd_root = self.root / "sandbox"
        self.step_dir = self.root / "resp" / "step"
        self.usage = self.root / "usage.jsonl"
        for p in (mock.patch.object(CC, "CLAUDE", str(self.script)),
                  mock.patch.dict(os.environ, {"FAKE_DIR": str(self.fake_dir), "GM456_DATA": str(self.data)})):
            p.start()
            self.addCleanup(p.stop)
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({"v": 1})}]}])

    def tearDown(self):
        self.tmp.cleanup()

    def set_fake(self, rules, probe=None):
        probe = probe or {"events": probe_events()}
        (self.fake_dir / "spec.json").write_text(json.dumps({"rules": rules, "probe": probe}))
        shutil.rmtree(self.fake_dir / "cnt", ignore_errors=True)

    def calls(self, kind=None):
        out = [json.load(open(p)) for p in glob.glob(str(self.fake_dir / "log" / "call.*.json"))]
        return [c for c in out if kind is None or c["kind"] == kind]

    def n_calls(self):
        return len(self.calls())

    def run_packs(self, requests, jobs=1, retry=(), runner=None):
        return CC.run_packs(requests, self.step_dir, jobs, {"type": "object"}, "sys", "m", "low", self.cwd_root,
                            self.usage, retry=retry, runner=runner)

    def args(self, jobs=1, retry=None):
        return SimpleNamespace(cwd_root=str(self.cwd_root), jobs=jobs, retry=retry)

    def run_step(self, requests, prompt_sha="p1", jobs=1, retry=None):
        return CC.run_step("step", requests, self.root / "resp", self.args(jobs, retry), {"type": "object"},
                           "sys", prompt_sha, "m", "low", 5, "v0.1-t")


# --- classify (M3, M4) --------------------------------------------------------

class Classify(unittest.TestCase):
    def test_bare_429_in_stderr_without_json_is_a_limit(self):
        self.assertEqual(CC.classify(None, 1, "API Error: 429 Too Many Requests")[0], "limit")

    def test_a_number_that_merely_contains_429_is_not_a_limit(self):
        self.assertEqual(CC.classify(None, 1, "request id 1429 failed")[0], "transient")

    def test_string_and_int_api_error_status_429_are_limits(self):
        self.assertEqual(CC.classify(cli_err("429", "x"), 1)[0], "limit")
        self.assertEqual(CC.classify(cli_err(" 429 ", "x"), 1)[0], "limit")
        self.assertEqual(CC.classify(cli_err(429, "x"), 1)[0], "limit")

    def test_limit_text_in_result_is_a_limit(self):
        self.assertEqual(CC.classify(cli_err(None, "You hit your limit"), 1)[0], "limit")

    def test_5xx_overloaded_and_nonzero_rc_are_transient(self):
        self.assertEqual(CC.classify(cli_err(503, "x"), 1)[0], "transient")
        self.assertEqual(CC.classify(cli_err(None, "Overloaded"), 0)[0], "transient")
        self.assertEqual(CC.classify(cli_err(None, "x"), 1)[0], "transient")
        self.assertEqual(CC.classify(cli_err(400, "bad"), 0)[0], "malformed")

    def test_extra_turns_on_a_success_are_not_unblind(self):
        for turns in (3, 4):
            self.assertEqual(CC.classify(cli_ok({"v": 1}, turns=turns), 0)[0], "success", turns)

    def test_permission_denials_are_unblind_even_on_one_turn(self):
        cli = cli_ok({"v": 1}, turns=1, permission_denials=[{"tool_name": "Bash"}])
        self.assertEqual(CC.classify(cli, 0)[0], "unblind")

    def test_refusal_malformed_and_success(self):
        self.assertEqual(CC.classify(cli_ok({"v": 1}, stop_reason="refusal"), 0)[0], "refusal")
        self.assertEqual(CC.classify(cli_ok(None), 0)[0], "malformed")
        self.assertEqual(CC.classify(cli_ok({"v": 1}), 0)[0], "success")
        self.assertEqual(CC.classify(cli_ok({"v": 1}), 1)[0], "transient")


# --- run_packs against the fake CLI --------------------------------------------

class RunPacks(FakeBase):
    def test_argv_is_blind_and_cwd_is_a_fresh_sandbox(self):
        req = mk_request("a-1", "user one")
        self.run_packs([req])
        (call,) = self.calls("pack")
        a = call["argv"]
        self.assertEqual(a[a.index("--tools") + 1], "")
        for flag in ("--strict-mcp-config", "--disable-slash-commands", "--no-session-persistence"):
            self.assertIn(flag, a)
        self.assertEqual(a[a.index("--setting-sources") + 1], "")
        self.assertEqual(a[a.index("--permission-prompts") + 1], "none")
        self.assertEqual(a[a.index("--system-prompt") + 1], "sys")
        self.assertEqual(call["stdin"], "user one")
        self.assertTrue(Path(call["cwd"]).resolve().is_relative_to(self.cwd_root.resolve()))

    def test_success_stores_response_and_request_sha_sidecar(self):
        req = mk_request("a-1", "u")
        s = self.run_packs([req])
        self.assertEqual((s["success"], s["skipped"], s["stopped"]), (1, 0, None))
        out, sha, errf = CC.pack_files(self.step_dir, "a-1")
        self.assertEqual(json.load(open(out))["structured_output"], {"v": 1})
        self.assertEqual(sha.read_text().strip(), CC.request_sha(req))
        self.assertFalse(errf.exists())

    def test_same_request_is_not_rerun(self):
        req = mk_request("a-1", "u")
        self.run_packs([req])
        before = self.n_calls()
        s = self.run_packs([req])
        self.assertEqual((self.n_calls(), s["skipped"], s["success"]), (before, 1, 0))

    def test_changed_request_is_pending_and_its_old_response_is_never_returned(self):
        old, new = mk_request("a-1", "user v1"), mk_request("a-1", "user v2")
        self.run_packs([old])
        self.assertIsNone(CC.state(new, self.step_dir))
        self.assertEqual(CC.state(old, self.step_dir), "done")
        cli, why = CC.stored(new, self.step_dir)
        self.assertIsNone(cli)
        self.assertIn("stale", why)
        self.assertEqual([r["custom_id"] for r in CC.pending([new], self.step_dir)], ["a-1"])
        self.run_packs([new])
        self.assertEqual(self.n_calls(), 2)
        self.assertEqual(CC.state(new, self.step_dir), "done")

    def test_response_without_a_sidecar_is_pending(self):
        req = mk_request("a-1", "u")
        self.run_packs([req])
        CC.pack_files(self.step_dir, "a-1")[1].unlink()
        self.assertIsNone(CC.state(req, self.step_dir))
        self.assertEqual(len(CC.pending([req], self.step_dir)), 1)
        self.assertIsNone(CC.stored(req, self.step_dir)[0])

    def test_retry_names_rerun_a_done_pack_and_unknown_names_stop(self):
        req = mk_request("a-1", "u")
        self.run_packs([req])
        self.run_packs([req], retry=("a-1",))
        self.assertEqual(self.n_calls(), 2)
        with self.assertRaises(SystemExit) as cm:
            CC.pending([req], self.step_dir, retry=("nope",))
        self.assertIn("unknown packs", str(cm.exception.code))

    def test_transient_is_retried_once_then_succeeds(self):
        self.set_fake([{"match": "", "steps": [TRANSIENT, {"stdout": cli_ok({"v": 1})}]}])
        s = self.run_packs([mk_request("a-1", "u")])
        self.assertEqual((s["success"], s["transient"]), (1, 0))
        rows = [json.loads(l) for l in open(self.usage)]
        self.assertEqual([(r["outcome"], r["attempt"]) for r in rows], [("transient", 1), ("success", 2)])

    def test_second_transient_goes_to_err_json_and_the_run_continues(self):
        self.set_fake([{"match": "bad", "steps": [TRANSIENT]}, {"match": "good", "steps": [{"stdout": cli_ok({})}]}])
        bad, good = mk_request("a-1", "bad"), mk_request("a-2", "good")
        s = self.run_packs([bad, good])
        self.assertEqual((s["transient"], s["success"], s["stopped"]), (1, 1, None))
        err = json.load(open(CC.pack_files(self.step_dir, "a-1")[2]))
        self.assertEqual((err["outcome"], err["requestSha256"]), ("transient", CC.request_sha(bad)))
        self.assertEqual(CC.state(bad, self.step_dir), "failed")
        self.assertEqual(CC.pending([bad, good], self.step_dir), [])
        self.assertIn("transient", CC.stored(bad, self.step_dir)[1])

    def test_extra_turns_success_is_stored_and_does_not_stop(self):
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({"v": 1}, turns=4)}]}])
        s = self.run_packs([mk_request("a-1", "u")])
        self.assertEqual((s["success"], s["unblind"], s["stopped"]), (1, 0, None))

    def test_permission_denials_stop_the_run_and_record_the_pack(self):
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({"v": 1}, permission_denials=[{"t": 1}])}]}])
        s = self.run_packs([mk_request("a-1", "u")])
        self.assertEqual(s["unblind"], 1)
        self.assertIn("unblind", s["stopped"])
        self.assertTrue(CC.pack_files(self.step_dir, "a-1")[2].exists())

    def test_limit_writes_nothing_for_the_pack_and_it_stays_pending(self):
        self.set_fake([{"match": "", "steps": [LIMIT]}])
        req = mk_request("a-1", "u")
        s = self.run_packs([req])
        self.assertEqual(s["limit"], 1)
        self.assertIn("limit at a-1", s["stopped"])
        self.assertEqual([p.name for p in self.step_dir.iterdir()], [])
        self.assertEqual(len(CC.pending([req], self.step_dir)), 1)

    def test_concurrent_run_stops_at_the_first_limit(self):
        # Guaranteed by the code: a worker takes a new pack only after finishing one, and a
        # limit sets the stop first. So at most `jobs` packs run; the rest are not run.
        self.set_fake([{"match": "", "steps": [dict(LIMIT, sleep=0.3)]}])
        reqs = [mk_request(f"a-{i}", f"u{i}") for i in range(6)]
        s = self.run_packs(reqs, jobs=3)
        self.assertGreaterEqual(s["limit"], 1)
        self.assertLessEqual(s["limit"], 3)
        self.assertGreaterEqual(s["notRun"], 3)
        self.assertEqual(s["limit"] + s["notRun"], 6)
        self.assertEqual(len(self.calls("pack")), s["limit"])
        self.assertIsNotNone(s["stopped"])

    def test_transient_after_stop_is_not_recorded_and_runs_again_on_resume(self):
        # Pack B's first call is transient and arrives after pack A's limit set the stop.
        # Events pin that order: A returns only once B is inside its runner; B returns only
        # once the stop event has been set.
        stop_seen = threading.Event()
        b_in = threading.Event()
        real_event = threading.Event

        made = []

        class Spy(real_event):
            # run_packs creates its stop event first; thread start-up creates more Events later.
            def __init__(self):
                super().__init__()
                made.append(self)

            def set(self):
                super().set()
                if self is made[0]:
                    stop_seen.set()

        calls = []

        def runner(req):
            calls.append(req["custom_id"])
            if req["custom_id"] == "a-A":
                self.assertTrue(b_in.wait(10))
                return CC.json.loads(json.dumps(LIMIT["stdout"])), 1, ""
            b_in.set()
            self.assertTrue(stop_seen.wait(10))
            return None, 1, "connection reset"

        a, b = mk_request("a-A", "u"), mk_request("a-B", "u")
        with mock.patch.object(CC.threading, "Event", Spy):
            s = self.run_packs([a, b], jobs=2, runner=runner)
        self.assertEqual(sorted(calls), ["a-A", "a-B"])  # B was not retried after the stop
        self.assertEqual((s["limit"], s["notRun"], s["transient"]), (1, 1, 0))
        self.assertFalse(CC.pack_files(self.step_dir, "a-B")[2].exists())
        self.assertEqual({r["custom_id"] for r in CC.pending([a, b], self.step_dir)}, {"a-A", "a-B"})


# --- probe_blindness and run_step ------------------------------------------------

class ProbeAndRunStep(FakeBase):
    def test_probe_ok_with_only_structured_output_and_small_context(self):
        rec = CC.probe_blindness(self.cwd_root, "m")
        self.assertTrue(rec["ok"], rec["reasons"])
        self.assertEqual(rec["init"]["tools"], ["StructuredOutput"])
        self.assertEqual(rec["context"], 1000)

    def test_probe_context_above_threshold_is_not_ok(self):
        self.set_fake([], probe={"events": probe_events(context=CC.PROBE_MAX_CONTEXT + 1)})
        rec = CC.probe_blindness(self.cwd_root, "m")
        self.assertFalse(rec["ok"])
        self.assertTrue(any("context" in r for r in rec["reasons"]), rec["reasons"])

    def test_probe_context_at_threshold_is_ok(self):
        self.set_fake([], probe={"events": probe_events(context=CC.PROBE_MAX_CONTEXT)})
        self.assertTrue(CC.probe_blindness(self.cwd_root, "m")["ok"])

    def test_probe_with_extra_tool_or_mcp_server_is_not_ok(self):
        self.set_fake([], probe={"events": probe_events(tools=("StructuredOutput", "Bash"))})
        self.assertTrue(any("tools" in r for r in CC.probe_blindness(self.cwd_root, "m")["reasons"]))
        self.set_fake([], probe={"events": probe_events(mcp=("srv",))})
        self.assertTrue(any("mcp_servers" in r for r in CC.probe_blindness(self.cwd_root, "m")["reasons"]))

    def test_failed_probe_aborts_with_exit_3_before_any_pack(self):
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({})}]}],
                      probe={"events": probe_events(context=CC.PROBE_MAX_CONTEXT + 500)})
        with self.assertRaises(SystemExit) as cm:
            self.run_step([mk_request("a-1", "u")])
        self.assertEqual(cm.exception.code, CC.STOP_EXIT)
        self.assertEqual(self.calls("pack"), [])
        self.assertFalse((self.root / "resp" / "step-run.json").exists())
        self.assertFalse(self.step_dir.exists() and any(self.step_dir.iterdir()))

    def test_resume_with_everything_stored_makes_zero_calls(self):
        reqs = [mk_request("a-1", "u1"), mk_request("a-2", "u2")]
        self.run_step(reqs)
        first = self.n_calls()
        self.assertGreaterEqual(first, 3)  # version, probe, two packs
        self.run_step(reqs)
        self.assertEqual(self.n_calls(), first)  # no probe, no --version, no pack

    def test_limit_stops_the_run_with_exit_3_and_resume_runs_only_the_rest(self):
        self.set_fake([{"match": "u2", "steps": [LIMIT, {"stdout": cli_ok({"v": 2})}]},
                       {"match": "u1", "steps": [{"stdout": cli_ok({"v": 1})}]}])
        reqs = [mk_request("a-1", "u1"), mk_request("a-2", "u2")]
        with self.assertRaises(SystemExit) as cm:
            self.run_step(reqs)
        self.assertEqual(cm.exception.code, CC.STOP_EXIT)
        self.assertEqual([r["custom_id"] for r in CC.pending(reqs, self.step_dir)], ["a-2"])
        packs = len(self.calls("pack"))
        self.run_step(reqs)  # resumes: the limit is over
        self.assertEqual(len(self.calls("pack")), packs + 1)
        self.assertEqual(CC.pending(reqs, self.step_dir), [])

    def test_run_step_exits_3_on_unblind_call(self):
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({}, permission_denials=[{"t": 1}])}]}])
        with self.assertRaises(SystemExit) as cm:
            self.run_step([mk_request("a-1", "u")])
        self.assertEqual(cm.exception.code, 3)

    def test_changed_request_moves_old_files_to_stale_and_reruns_only_it(self):
        v1 = [mk_request("a-1", "u1"), mk_request("a-2", "u2")]
        self.run_step(v1)
        v2 = [mk_request("a-1", "u1"), mk_request("a-2", "u2 changed")]
        before = len(self.calls("pack"))
        self.run_step(v2)
        self.assertEqual(len(self.calls("pack")), before + 1)
        stale = list((self.step_dir / "stale").glob("*/*"))
        self.assertEqual(sorted(p.name for p in stale), ["a-2.json", "a-2.req-sha256"])
        for r in v2:
            self.assertEqual(CC.state(r, self.step_dir), "done")
        rec = CC.run_record(self.root / "resp", "step")
        self.assertEqual(sorted(rec["responses"]), ["a-1.json", "a-2.json"])

    def test_custom_id_gone_from_the_request_set_is_set_aside(self):
        self.run_step([mk_request("a-1", "u1"), mk_request("a-2", "u2")])
        self.run_step([mk_request("a-1", "u1")])
        self.assertEqual(sorted(CC.run_record(self.root / "resp", "step")["responses"]), ["a-1.json"])

    def test_responses_of_another_prompt_stop_the_run(self):
        reqs = [mk_request("a-1", "u")]
        self.run_step(reqs, prompt_sha="p1")
        with self.assertRaises(SystemExit) as cm:
            self.run_step(reqs, prompt_sha="p2")
        self.assertIn("another prompt", str(cm.exception.code))

    def test_each_session_that_runs_packs_is_appended_to_the_run_record(self):
        self.run_step([mk_request("a-1", "u1")])
        self.run_step([mk_request("a-1", "u1"), mk_request("a-2", "u2")])
        rec = json.load(open(self.root / "resp" / "step-run.json"))
        self.assertEqual([s["pending"] for s in rec["sessions"]], [1, 1])
        self.assertEqual(rec["driver"], "claude-cli")
        self.assertEqual(rec["claudeVersion"], "9.9.9 (fake)")


# --- generate.py and judge.py --run end to end ------------------------------------

def node(key, repo="r"):
    return {"key": key, "repo": repo, "filePath": "src/a.rs", "parent": None, "docComment": "Does a thing.",
            "srcLines": ["fn f() {}"], "qualifiedName": key.split(":")[-1], "commit": "c", "language": "rust"}


def gen_entry(n):
    return {"n": n, "phrases": ["p one", "p two"], "sentences": ["Sentence one.", "Sentence two."],
            "paraphrases": ["Para one.", "Para two."], "answersAlso": []}


class E2EBase(FakeBase):
    version = "v0.1-t"

    def setUp(self):
        super().setUp()
        self.vdir = self.data / self.version

    def write_nodes(self, keys, repo="r"):
        C.write_jsonl(self.vdir / "nodes" / f"{repo}.jsonl", [node(k, repo) for k in keys])

    def main(self, module, *extra):
        argv = [module.__name__ + ".py", "--version", self.version, "--run", "--cwd-root", str(self.cwd_root),
                "--jobs", "1", *extra]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print"):
            module.main()


class GenerateRun(E2EBase):
    def targets(self, n):
        ts = [{"repo": "r", "key": f"r:t{i}", "n": i} for i in range(1, n + 1)]
        C.write_jsonl(self.vdir / "targets.jsonl", ts)
        self.write_nodes([t["key"] for t in ts])

    def queries(self):
        return C.read_jsonl(self.vdir / "queries.gen.jsonl")

    def skipped(self):
        return C.read_jsonl(self.vdir / "responses" / "gen-skipped.jsonl")

    def test_run_writes_six_queries_per_target_and_resume_makes_zero_calls(self):
        self.targets(2)
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({"targets": [gen_entry(1), gen_entry(2)]})}]}])
        self.main(G)
        self.assertEqual(len(self.queries()), 12)
        self.assertEqual(self.skipped(), [])
        n = self.n_calls()
        self.main(G)
        self.assertEqual(self.n_calls(), n)
        self.assertEqual(len(self.queries()), 12)

    def test_more_targets_under_the_same_custom_id_rerun_the_pack(self):
        self.targets(1)
        self.set_fake([{"match": "Symbols (1)", "steps": [{"stdout": cli_ok({"targets": [gen_entry(1)]})}]},
                       {"match": "Symbols (2)", "steps": [{"stdout": cli_ok({"targets": [gen_entry(1),
                                                                                         gen_entry(2)]})}]}])
        self.main(G)
        self.assertEqual(len(self.queries()), 6)
        self.targets(2)  # same custom_id gen-r-0001, a different request
        self.main(G)
        self.assertEqual(len(self.calls("pack")), 2)
        self.assertEqual(len(self.queries()), 12)
        stale = list((self.vdir / "responses" / "gen" / "stale").glob("*/gen-r-0001.json"))
        self.assertEqual(len(stale), 1)

    def test_target_missing_from_the_response_is_reported(self):
        self.targets(2)
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok({"targets": [gen_entry(1)]})}]}])
        self.main(G)
        self.assertEqual(len(self.queries()), 6)
        self.assertEqual([s["reason"] for s in self.skipped()], ["no entry for targets n [2]"])

    def test_limit_exits_3_and_writes_no_queries(self):
        self.targets(1)
        self.set_fake([{"match": "", "steps": [LIMIT]}])
        with self.assertRaises(SystemExit) as cm:
            self.main(G)
        self.assertEqual(cm.exception.code, 3)
        self.assertFalse((self.vdir / "queries.gen.jsonl").exists())


class JudgeRun(E2EBase):
    def setUp(self):
        super().setUp()
        self.write_nodes(["r:t1", "r:c1", "r:c2"])
        negs = [{"key": "r:c1", "sources": ["embed"], "sameFile": False},
                {"key": "r:c2", "sources": ["embed"], "sameFile": False}]
        rows = [{"id": f"r-0001-{s}", "repo": "r", "target": "r:t1", "text": f"q {s}", "positives": ["r:t1"],
                 "negatives": negs} for s in ("p1", "p2")]
        C.write_jsonl(self.vdir / "queries.mined.jsonl", rows)

    def judged(self):
        return C.read_jsonl(self.vdir / "queries.judged.jsonl")

    def skipped(self):
        return C.read_jsonl(self.vdir / "judge" / "judge-skipped.jsonl")

    def verdicts(self, *entries):
        return {"verdicts": [{"t": 1, "c": c, "verdicts": v} for c, v in entries]}

    def test_complete_verdicts_judge_every_pair_and_export_gate_is_open(self):
        out = self.verdicts((1, ["non-answer", "non-answer"]), (2, ["non-answer", "non-answer"]))
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok(out)}]}])
        self.main(J)
        self.assertEqual(X.unjudged(self.judged()), [])
        self.assertEqual(self.skipped(), [])

    def test_wrong_length_verdict_list_drops_the_entry_and_export_sees_it_unjudged(self):
        out = self.verdicts((1, ["non-answer", "non-answer"]), (2, ["non-answer"]))
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok(out)}]}])
        self.main(J)
        self.assertEqual(X.unjudged(self.judged()), [("r-0001-p1", "r:c2"), ("r-0001-p2", "r:c2")])
        self.assertEqual([s["reason"] for s in self.skipped()],
                         ["t1 c2: 1 verdicts for 2 queries; entry dropped"])
        # the dropped entry must not leak a partial judgement onto the first query either
        rows = {r["id"]: r for r in self.judged()}
        self.assertIsNone(rows["r-0001-p1"]["negatives"][1]["judge"])
        self.assertEqual(rows["r-0001-p1"]["negatives"][0]["judge"]["verdict"], "non-answer")

    def test_export_refuses_the_judge_output_with_an_unjudged_pair(self):
        out = self.verdicts((1, ["non-answer", "non-answer"]), (2, ["non-answer"]))
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok(out)}]}])
        self.main(J)
        with mock.patch.object(sys, "argv", ["export.py", "--version", self.version]):
            with self.assertRaises(SystemExit) as cm:
                X.main()
        self.assertIn("2 unjudged negatives", str(cm.exception.code))

    def test_answers_verdict_moves_the_candidate_to_positives(self):
        out = self.verdicts((1, ["answers", "partial"]), (2, ["non-answer", "non-answer"]))
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok(out)}]}])
        self.main(J)
        rows = {r["id"]: r for r in self.judged()}
        self.assertEqual(rows["r-0001-p1"]["positives"], ["r:t1", "r:c1"])
        self.assertEqual([c["key"] for c in rows["r-0001-p2"]["negatives"]], ["r:c2"])

    def test_resume_makes_zero_calls_and_a_limit_exits_3(self):
        out = self.verdicts((1, ["non-answer"] * 2), (2, ["non-answer"] * 2))
        self.set_fake([{"match": "", "steps": [{"stdout": cli_ok(out)}]}])
        self.main(J)
        n = self.n_calls()
        self.main(J)
        self.assertEqual(self.n_calls(), n)
        shutil.rmtree(self.vdir / "responses")
        shutil.rmtree(self.vdir / "judge")
        (self.vdir / "queries.judged.jsonl").unlink()
        self.set_fake([{"match": "", "steps": [LIMIT]}])
        with self.assertRaises(SystemExit) as cm:
            self.main(J)
        self.assertEqual(cm.exception.code, 3)
        self.assertFalse((self.vdir / "queries.judged.jsonl").exists())


if __name__ == "__main__":
    unittest.main()
