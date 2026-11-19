"""Offline tests of the instance-side runner (eval/gpu/runner.py; design note
docs/design/GM-457-spot-gpu-training.md, section 4 behaviours 6, 9-12, D4) and
of the user data script's exit path (user_data.sh.tmpl, behaviour 4, review fix F4).

No AWS: the runner runs as a subprocess against
  - a local directory standing in for the bucket (GM457_S3_ROOT),
  - a fake IMDSv2 server on 127.0.0.1 (GM457_IMDS) that the test drives,
  - this interpreter for the job steps (GM457_PYTHON, no uv venv),
  - `touch <file>` as the shutdown command (GM457_SHUTDOWN_CMD),
  - GM457_AWS=false, so a code path that reached for the aws CLI fails.
Jobs are a small fake trainer (fake_job.py, written per test) that the test
steers through its arguments; it waits on files, never on wall-clock order.
The user data test runs the rendered script under /bin/bash with fake
`shutdown`, `curl` and `aws` first on PATH.

Run (stdlib only; the launcher tests need boto3 + moto, same command):
  uv run --no-project --python 3.11 --with boto3==1.44.6 \
      --with 'moto[ec2,s3,iam,scheduler,sts]==5.2.5' \
      python -m unittest discover -s eval/gpu -p 'test_gm457_*.py'
"""

import hashlib
import importlib.util
import json
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gpu_train as G  # noqa: E402
import runner as R  # noqa: E402

RUN_ID = "job-0123456789ab-20261119T0000"

FAKE_JOB = r'''
import argparse, json, os, signal, subprocess, sys, time
from pathlib import Path

ap = argparse.ArgumentParser()
ap.add_argument("--mode", required=True)
ap.add_argument("--out", required=True)
ap.add_argument("--device")
a = ap.parse_args()
out = Path(a.out)
out.mkdir(parents=True, exist_ok=True)
step = out.name
store = Path(os.environ["GM457_S3_ROOT"])
run_id = os.environ["GM457_RUN_ID"]
ctl = Path(os.environ["TEST_CTL"])
log = Path(os.environ["TEST_WORK"]) / "logs" / f"attempt-{os.environ['GM457_ATTEMPT']}.log"
remote = store / "ckpt" / run_id / step / "checkpoint.pt"


def ckpt(data):
    tmp = out / "checkpoint.pt.tmp"
    tmp.write_bytes(data)
    os.replace(tmp, out / "checkpoint.pt")


def wait_for(pred, what, timeout=30):
    end = time.monotonic() + timeout
    while not pred():
        if time.monotonic() > end:
            print(f"fake_job: timed out waiting for {what}", flush=True)
            sys.exit(3)
        time.sleep(0.02)


def started():
    (ctl / f"started-{step}").write_text(str(os.getpid()))


mode = a.mode
(ctl / f"ran-{step}").write_text(json.dumps({"device": a.device}))
if mode.startswith("exit:"):
    (out / "result.txt").write_text("result")
    sys.exit(int(mode.split(":")[1]))
if mode == "restore":
    p = out / "checkpoint.pt"
    (out / "seen.txt").write_bytes(p.read_bytes() if p.exists() else b"<none>")
    sys.exit(0)
if mode == "env":
    d = os.environ.get("GM_TEST_DATA")
    (out / "env.json").write_text(json.dumps({"dir": d, "file": (Path(d) / "a.txt").read_text() if d else None}))
    sys.exit(0)
if mode == "rotate":
    ckpt(b"v1")
    wait_for(lambda: remote.is_file() and remote.read_bytes() == b"v1", "upload of v1")
    (out / "stray.tmp").write_bytes(b"partial")
    (out / "metrics.json").write_text("{}")
    ckpt(b"v2")
    sys.exit(0)
if mode == "retry":
    ckpt(b"v1")
    wait_for(lambda: log.is_file() and "checkpoint upload failed" in log.read_text(), "a failed upload")
    os.remove(store / "ckpt" / run_id)
    wait_for(lambda: remote.is_file() and remote.read_bytes() == b"v1", "the retried upload")
    sys.exit(0)
if mode == "fail-upload-done":
    ckpt(b"v1")
    wait_for(lambda: log.is_file() and "checkpoint upload failed" in log.read_text(), "a failed upload")
    sys.exit(0)
if mode == "fifo-ckpt":
    os.mkfifo(out / "checkpoint.pt")
    started()
    time.sleep(300)
    sys.exit(4)
if mode in ("sleep", "ignore-term"):
    if mode == "ignore-term":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    ckpt(b"before-notice")
    child = subprocess.Popen(["sleep", "300"])
    (ctl / "grandchild").write_text(str(child.pid))
    started()
    time.sleep(300)
    sys.exit(4)
sys.exit(f"fake_job: unknown mode {mode}")
'''


class FakeImds:
    """IMDSv2 on 127.0.0.1: PUT token, GETs need it (401 otherwise). `notice` is the
    instance-action document (404 while None); `errors` > 0 answers that many
    instance-action GETs with 500."""

    def __init__(self):
        self.notice = None
        self.errors = 0
        self.requests = []
        self.hold_action = False
        self.in_flight = threading.Event()
        self.release = threading.Event()
        self.rebalance_served = threading.Event()
        imds = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def reply(self, code, body=b""):
                self.send_response(code)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_PUT(self):
                imds.requests.append(("PUT", self.path, self.headers.get("X-aws-ec2-metadata-token")))
                if self.path == "/latest/api/token" and self.headers.get("X-aws-ec2-metadata-token-ttl-seconds"):
                    return self.reply(200, b"tok-1")
                return self.reply(400)

            def do_GET(self):
                imds.requests.append(("GET", self.path, self.headers.get("X-aws-ec2-metadata-token")))
                if self.headers.get("X-aws-ec2-metadata-token") != "tok-1":
                    return self.reply(401)
                p = self.path
                fixed = {"/latest/meta-data/instance-id": b"i-0fake", "/latest/meta-data/instance-type": b"g5.xlarge",
                         "/latest/meta-data/placement/availability-zone": b"us-east-1a",
                         "/latest/meta-data/instance-life-cycle": b"spot"}
                if p in fixed:
                    return self.reply(200, fixed[p])
                if p == "/latest/meta-data/events/recommendations/rebalance":
                    self.reply(404)
                    imds.rebalance_served.set()
                    return
                if p == "/latest/meta-data/spot/instance-action":
                    if imds.hold_action:
                        imds.in_flight.set()
                        imds.release.wait(60)
                    if imds.errors > 0:
                        imds.errors -= 1
                        return self.reply(500)
                    if imds.notice is None:
                        return self.reply(404)
                    return self.reply(200, json.dumps(imds.notice).encode())
                return self.reply(404)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


def pid_gone(pid, timeout=10):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        time.sleep(0.05)
    return False


class ImdsClient(unittest.TestCase):
    """A16: token PUT, then GET with the token; 404 is None."""

    def setUp(self):
        self.imds = FakeImds()
        self.addCleanup(self.imds.close)

    def test_token_then_get(self):
        c = R.Imds(self.imds.url)
        self.assertEqual(c.get("latest/meta-data/instance-id"), "i-0fake")
        self.assertEqual(self.imds.requests[0][0], "PUT")
        self.assertEqual(self.imds.requests[1][2], "tok-1")

    def test_404_is_none_and_notice_parses(self):
        c = R.Imds(self.imds.url)
        self.assertIsNone(R.spot_notice(c))
        self.imds.notice = {"action": "terminate", "time": "2026-11-19T00:02:00Z"}
        self.assertEqual(R.spot_notice(c), self.imds.notice)

    def test_server_error_raises(self):
        self.imds.errors = 1
        with self.assertRaises(Exception):
            R.spot_notice(R.Imds(self.imds.url))


class PureRunner(unittest.TestCase):
    def test_attempt_state(self):
        self.assertEqual(R.attempt_state("done"), "done")
        self.assertEqual(R.attempt_state("spot-notice"), "interrupted")
        self.assertEqual(R.attempt_state("stopped-resumable"), "interrupted")
        self.assertEqual(R.attempt_state("runner-signal"), "terminated")
        for r in ("failed", "runner-error", "data-sha-mismatch", "smoke-failed", "no-manifest"):
            self.assertEqual(R.attempt_state(r), "failed")

    def test_data_sha_matches_launcher(self):
        man = {"files": [{"path": "b", "size": 1, "sha256": "x"}, {"path": "a", "size": 2, "sha256": "y"}]}
        self.assertEqual(R.data_sha(man), G.data_sha(man))

    def test_verify_data(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "a").write_bytes(b"abc")
            good = {"path": "a", "size": 3, "sha256": hashlib.sha256(b"abc").hexdigest()}
            self.assertEqual(R.verify_data(d, {"files": [good]}), [])
            self.assertTrue(R.verify_data(d, {"files": [{**good, "size": 4}]}))
            self.assertTrue(R.verify_data(d, {"files": [{**good, "sha256": "0" * 64}]}))
            self.assertTrue(R.verify_data(d, {"files": [{**good, "path": "missing"}]}))


class RunnerCase(unittest.TestCase):
    """The runner as a subprocess against a local store and a fake IMDS."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        self.store, self.work, self.src, self.ctl = root / "store", root / "work", root / "src", root / "ctl"
        for d in (self.store, self.work, self.src, self.ctl):
            d.mkdir()
        (self.src / "fake_job.py").write_text(FAKE_JOB)
        self.shutdown_flag = root / "shutdown-called"
        self.marker = root / "status-marker"
        self.imds = FakeImds()
        self.addCleanup(self.imds.close)
        self.put("env/abc/requirements.txt", b"torch==0\n")
        self.proc = None

    def put(self, key, data):
        p = self.store / key
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data if isinstance(data, bytes) else data.encode())

    def key(self, key):
        return self.store / key

    def manifest(self, steps, data=(), smoke=False, raw=None):
        m = {"runId": RUN_ID, "envKey": "env/abc/requirements.txt", "data": list(data), "smoke": smoke,
             "steps": steps}
        self.put(f"runs/{RUN_ID}/manifest.json", raw if raw is not None else json.dumps(m))

    @staticmethod
    def step(name, mode, device=None):
        s = {"name": name, "entry": "fake_job.py", "args": ["--mode", mode]}
        if device:
            s["device"] = device
        return s

    def env(self):
        e = {k: v for k, v in os.environ.items() if not k.startswith("AWS_")}
        e.update(GM457_IMDS=self.imds.url, GM457_S3_ROOT=str(self.store), GM457_PYTHON=sys.executable,
                 GM457_SHUTDOWN_CMD=f"touch {shlex.quote(str(self.shutdown_flag))}",
                 GM457_STATUS_MARKER=str(self.marker), GM457_AWS="false", TEST_CTL=str(self.ctl),
                 TEST_WORK=str(self.work), AWS_ACCESS_KEY_ID="testing", AWS_SECRET_ACCESS_KEY="testing",
                 AWS_EC2_METADATA_DISABLED="true")
        return e

    def start(self, grace=1.0, poll=0.05, upload_every=0.05, log_every=0.1):
        cmd = [sys.executable, str(HERE / "runner.py"), "--bucket", "test-bucket", "--region", "us-east-1",
               "--run-id", RUN_ID, "--attempt", "1", "--work", str(self.work), "--src", str(self.src),
               "--grace", str(grace), "--poll", str(poll), "--upload-every", str(upload_every),
               "--log-every", str(log_every)]
        self.proc = subprocess.Popen(cmd, env=self.env(), stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.addCleanup(self.kill)
        return self.proc

    def kill(self):
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.communicate()

    def finish(self, timeout=120):
        out, _ = self.proc.communicate(timeout=timeout)
        self.output = out
        return self.proc.returncode

    def run_runner(self, **kw):
        self.start(**kw)
        return self.finish()

    def status(self):
        return json.loads(self.key(f"runs/{RUN_ID}/status.json").read_text())

    def attempt(self):
        st = self.status()
        self.assertEqual([r["n"] for r in st["attempts"]], [1])
        return st["attempts"][0]

    def assert_finished(self, state, reason):
        rec = self.attempt()
        self.assertEqual((rec["state"], rec["reason"]), (state, reason), self.output)
        self.assertEqual(self.status()["state"], state)
        self.assertTrue(self.shutdown_flag.exists(), "shutdown command not run")
        self.assertTrue(self.marker.exists(), "status marker not touched")
        self.assertTrue(self.key(f"runs/{RUN_ID}/logs/attempt-1.log").is_file(), "log not uploaded")
        return rec

    def wait_file(self, p, timeout=60):
        end = time.monotonic() + timeout
        while not p.exists():
            if self.proc.poll() is not None:
                self.fail(f"runner exited before {p.name}: {self.proc.communicate()[0]}")
            if time.monotonic() > end:
                self.fail(f"timed out waiting for {p}")
            time.sleep(0.02)


class Outcomes(RunnerCase):
    """Behaviour 10, A4: job exit code -> attempt state; runner errors still shut down."""

    def test_exit_0_is_done(self):
        self.manifest([self.step("gpu", "exit:0", device="cuda")])
        self.assertEqual(self.run_runner(), 0, self.output)
        rec = self.assert_finished("done", "done")
        self.assertEqual(rec["instanceId"], "i-0fake")
        self.assertEqual(self.status()["stepsDone"], ["gpu"])
        self.assertEqual(self.key(f"runs/{RUN_ID}/out/gpu/result.txt").read_text(), "result")
        self.assertEqual(json.loads((self.ctl / "ran-gpu").read_text())["device"], "cuda")

    def test_exit_1_is_failed(self):
        self.manifest([self.step("gpu", "exit:1")])
        self.assertNotEqual(self.run_runner(), 0)
        self.assert_finished("failed", "failed")
        self.assertEqual(self.status()["stepsDone"], [])

    def test_exit_75_without_notice_is_interrupted(self):
        self.manifest([self.step("gpu", "exit:75")])
        self.run_runner()
        self.assert_finished("interrupted", "stopped-resumable")

    def test_runner_exception_is_runner_error_and_still_shuts_down(self):
        self.manifest([], raw="{not json")
        self.assertNotEqual(self.run_runner(), 0)
        rec = self.assert_finished("failed", "runner-error")
        self.assertIn("JSONDecodeError", rec["error"])

    def test_missing_manifest_fails_and_shuts_down(self):
        self.run_runner()
        self.assert_finished("failed", "no-manifest")

    @unittest.skipIf(importlib.util.find_spec("torch") is not None, "needs an interpreter without torch")
    def test_smoke_failure_stops_before_the_steps(self):
        self.manifest([self.step("gpu", "exit:0")], smoke=True)
        self.run_runner()
        self.assert_finished("failed", "smoke-failed")
        self.assertFalse((self.ctl / "ran-gpu").exists())

    def test_runner_sigterm_is_terminated(self):
        """A3: SIGTERM to the runner (OS shutdown) stops the job and still shuts down.
        The signal is sent while the runner sleeps between polls: a 2 s poll, sent
        0.2 s after it answered an IMDS poll, with no upload due."""
        self.manifest([self.step("gpu", "sleep")])
        self.start(poll=2, upload_every=1000, log_every=1000)
        self.wait_file(self.ctl / "started-gpu")
        job_pid = int((self.ctl / "started-gpu").read_text())
        self.imds.rebalance_served.clear()
        self.assertTrue(self.imds.rebalance_served.wait(30))
        time.sleep(0.2)
        self.proc.send_signal(signal.SIGTERM)
        self.finish()
        self.assert_finished("terminated", "runner-signal")
        self.assertTrue(pid_gone(job_pid), "job still running after the runner stopped")

    def test_runner_sigterm_during_imds_poll_is_terminated(self):
        """A3 with the signal landing inside an IMDS request: the `except Exception`
        around the IMDS poll must not swallow it."""
        self.manifest([self.step("gpu", "sleep")])
        self.start(poll=0.05, upload_every=1000, log_every=1000)
        self.wait_file(self.ctl / "started-gpu")
        self.imds.in_flight.clear()
        self.imds.hold_action = True
        self.assertTrue(self.imds.in_flight.wait(30))
        self.proc.send_signal(signal.SIGTERM)
        time.sleep(0.2)
        self.imds.hold_action = False
        self.imds.release.set()
        try:
            self.finish(timeout=15)
        except subprocess.TimeoutExpired:
            self.fail("runner kept running after SIGTERM")
        self.assert_finished("terminated", "runner-signal")

    def test_runner_sigterm_during_upload_is_terminated(self):
        """A3 with the signal landing inside try_upload: the job's checkpoint.pt is a
        FIFO, so the checkpoint upload blocks reading it while the test holds the
        write end open. The test's non-blocking open of the write end succeeds only
        once the runner has the FIFO open for reading, i.e. is inside the upload.
        The `except Exception` around the upload must not swallow the signal."""
        self.manifest([self.step("gpu", "fifo-ckpt")])
        self.start(poll=0.05, upload_every=0.05, log_every=1000)
        self.wait_file(self.ctl / "started-gpu")
        job_pid = int((self.ctl / "started-gpu").read_text())
        fifo = self.work / "out" / "gpu" / "checkpoint.pt"
        end = time.monotonic() + 30
        while True:
            try:
                wfd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)
                break
            except OSError:  # ENXIO: no reader yet
                if self.proc.poll() is not None or time.monotonic() > end:
                    self.fail(f"runner never opened the checkpoint for upload: {self.proc.communicate()[0]}")
                time.sleep(0.02)
        self.addCleanup(os.close, wfd)
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.finish(timeout=15)
        except subprocess.TimeoutExpired:
            self.fail("runner kept running after SIGTERM during an upload")
        self.assert_finished("terminated", "runner-signal")
        self.assertTrue(pid_gone(job_pid), "job still running after the runner stopped")


class SpotNotice(RunnerCase):
    """Behaviour 9: an instance-action notice stops the job's process group, uploads
    the checkpoint and the log, records `interrupted`/`spot-notice`, shuts down."""

    def check(self, mode):
        self.manifest([self.step("gpu", mode)])
        self.start(grace=1.0)
        self.wait_file(self.ctl / "started-gpu")
        job_pid = int((self.ctl / "started-gpu").read_text())
        grandchild = int((self.ctl / "grandchild").read_text())
        self.imds.notice = {"action": "terminate", "time": "2026-11-19T00:02:00Z"}
        self.finish()
        rec = self.assert_finished("interrupted", "spot-notice")
        self.assertEqual(rec["notice"], self.imds.notice)
        self.assertNotIn("uploadError", rec)
        self.assertEqual(self.key(f"ckpt/{RUN_ID}/gpu/checkpoint.pt").read_bytes(), b"before-notice")
        self.assertTrue(pid_gone(job_pid), "job survived the notice")
        self.assertTrue(pid_gone(grandchild), "the job's process group was not signalled")
        return rec

    def test_notice_stops_job(self):
        self.check("sleep")

    def test_job_ignoring_sigterm_is_killed_after_grace(self):
        self.check("ignore-term")
        self.assertIn("SIGKILL", self.key(f"runs/{RUN_ID}/logs/attempt-1.log").read_text())

    def test_imds_errors_keep_the_job_running(self):
        """A16: an IMDS error during a poll is logged; the job keeps running."""
        self.imds.errors = 3
        self.manifest([self.step("gpu", "exit:0")])
        self.run_runner()
        self.assert_finished("done", "done")


class Checkpoints(RunnerCase):
    """Behaviours 11, 12."""

    def test_checkpoint_restored_into_out_before_the_job(self):
        self.put(f"ckpt/{RUN_ID}/gpu/checkpoint.pt", b"from-attempt-0")
        self.manifest([self.step("gpu", "restore")])
        self.run_runner()
        self.assert_finished("done", "done")
        self.assertEqual(self.key(f"runs/{RUN_ID}/out/gpu/seen.txt").read_bytes(), b"from-attempt-0")
        self.assertFalse(self.key(f"runs/{RUN_ID}/ckpt").exists())

    def test_changed_checkpoint_uploaded_previous_rotated_tmp_never(self):
        self.manifest([self.step("gpu", "rotate")])
        self.run_runner()
        self.assert_finished("done", "done")
        ck = self.key(f"ckpt/{RUN_ID}/gpu")
        self.assertEqual((ck / "checkpoint.pt").read_bytes(), b"v2")
        self.assertEqual((ck / "checkpoint.prev.pt").read_bytes(), b"v1")
        stored = [p.relative_to(self.store).as_posix() for p in self.store.rglob("*") if p.is_file()]
        self.assertEqual([p for p in stored if p.endswith(".tmp")], [], stored)
        out = sorted(p.name for p in self.key(f"runs/{RUN_ID}/out/gpu").iterdir())
        self.assertEqual(out, ["metrics.json"])


class UploadFailures(RunnerCase):
    """F1: a failed checkpoint upload during a step is logged and retried next tick;
    it does not decide the attempt. A file at ckpt/<run-id> makes every put under it fail."""

    def block(self):
        self.put(f"ckpt/{RUN_ID}", b"blocker")

    def test_failed_upload_is_retried_next_tick(self):
        self.block()
        self.manifest([self.step("gpu", "retry")])
        self.run_runner()
        rec = self.assert_finished("done", "done")
        self.assertNotIn("uploadError", rec)
        self.assertEqual(self.key(f"ckpt/{RUN_ID}/gpu/checkpoint.pt").read_bytes(), b"v1")

    def test_exit_0_with_failing_uploads_is_done(self):
        self.block()
        self.manifest([self.step("gpu", "fail-upload-done")])
        self.run_runner()
        rec = self.assert_finished("done", "done")
        self.assertIn("uploadError", rec)

    def test_notice_with_failing_upload_is_interrupted(self):
        self.block()
        self.manifest([self.step("gpu", "sleep")])
        self.start()
        self.wait_file(self.ctl / "started-gpu")
        self.imds.notice = {"action": "terminate", "time": "2026-11-19T00:02:00Z"}
        self.finish()
        rec = self.assert_finished("interrupted", "spot-notice")
        self.assertIn("uploadError", rec)


class Steps(RunnerCase):
    """A5: a step in status.json stepsDone is skipped; each step under its own name."""

    def test_done_step_is_skipped(self):
        self.put(f"runs/{RUN_ID}/status.json", json.dumps(
            {"runId": RUN_ID, "attempts": [{"n": 0, "state": "interrupted"}], "stepsDone": ["a"]}))
        self.manifest([self.step("a", "exit:0"), self.step("b", "exit:0")])
        self.run_runner()
        st = self.status()
        self.assertEqual(st["stepsDone"], ["a", "b"])
        self.assertFalse((self.ctl / "ran-a").exists())
        self.assertTrue((self.ctl / "ran-b").exists())
        self.assertTrue(self.key(f"runs/{RUN_ID}/out/b/result.txt").is_file())
        self.assertFalse(self.key(f"runs/{RUN_ID}/out/a").exists())
        self.assertEqual([r["n"] for r in st["attempts"]], [0, 1])


class Data(RunnerCase):
    """Behaviour 6, F8: data subsets verified against MANIFEST.json and its content address."""

    def subset(self, content=b"hello", manifest_size=None):
        files = [{"path": "a.txt", "size": manifest_size or len(b"hello"),
                  "sha256": hashlib.sha256(b"hello").hexdigest()}]
        man = {"files": files}
        sha = G.data_sha(man)
        prefix = f"data/ds/{sha}/"
        self.put(prefix + "a.txt", content)
        self.put(prefix + "MANIFEST.json", json.dumps({"dataset": "ds", "sha": sha, **man}))
        return {"name": "ds", "env": "GM_TEST_DATA", "sha": sha, "prefix": prefix}

    def test_verified_subset_reaches_the_job(self):
        self.manifest([self.step("gpu", "env")], data=[self.subset()])
        self.run_runner()
        self.assert_finished("done", "done")
        env = json.loads(self.key(f"runs/{RUN_ID}/out/gpu/env.json").read_text())
        self.assertEqual(env["file"], "hello")

    def test_file_mismatch_fails(self):
        self.manifest([self.step("gpu", "env")], data=[self.subset(content=b"hellO!")])
        self.run_runner()
        self.assert_finished("failed", "data-sha-mismatch")
        self.assertFalse((self.ctl / "ran-gpu").exists())

    def test_manifest_not_matching_content_address_fails(self):
        ds = self.subset()
        ds["sha"] = "0" * 64
        self.manifest([self.step("gpu", "env")], data=[ds])
        self.run_runner()
        self.assert_finished("failed", "data-sha-mismatch")


FAKE_AWS = r'''#!{python}
import shutil, sys, os
from pathlib import Path
with open(os.environ["CALLS"], "a") as f:
    f.write("aws " + " ".join(sys.argv[1:]) + "\n")
src, dst = sys.argv[-2], sys.argv[-1]
store = Path(os.environ["STORE"])
def local(u):
    return store / u.split("/", 3)[3] if u.startswith("s3://") else Path(u)
if not local(src).exists():
    sys.exit(1)
local(dst).parent.mkdir(parents=True, exist_ok=True)
shutil.copyfile(local(src), local(dst))
'''


class UserDataScript(unittest.TestCase):
    """Behaviour 4 and F4, by running the rendered user data under bash with fake
    shutdown/curl/aws: the lifetime shutdown is the first command run, before any
    download; the EXIT trap records the boot outcome when the runner wrote nothing."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.bin, self.store = self.root / "bin", self.root / "store"
        self.bin.mkdir()
        self.store.mkdir()
        self.calls = self.root / "calls.log"
        self.marker = self.root / "marker"
        (self.bin / "aws").write_text(FAKE_AWS.replace("{python}", sys.executable))
        (self.bin / "shutdown").write_text('#!/bin/sh\necho "shutdown $*" >> "$CALLS"\n')

    def fake_curl(self, body):
        (self.bin / "curl").write_text('#!/bin/sh\necho "curl $*" >> "$CALLS"\n' + body)
        for f in self.bin.iterdir():
            f.chmod(0o755)

    def run_script(self, **kw):
        text = G.user_data(bucket="b", bucket_region="us-east-1", run_id=RUN_ID, attempt=2, code_key="code/x.tar.gz",
                           max_minutes=60, uv_version="0.12.1", grace=60, **kw)
        for path, repl in (("/var/run/gm457-status-written", self.marker), ("/var/log/gm457-boot.log", self.root / "boot.log"),
                           ("/opt/gm457", self.root / "opt"), ("/tmp/gm457-boot.json", self.root / "boot.json")):
            self.assertIn(path, text)
            text = text.replace(path, str(repl))
        script = self.root / "ud.sh"
        script.write_text(text)
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "CALLS": str(self.calls), "STORE": str(self.store),
               "HOME": str(self.root)}
        return subprocess.run(["/bin/bash", str(script)], env=env, capture_output=True, text=True, timeout=60)

    def calls_list(self):
        return self.calls.read_text().splitlines()

    def boot_json(self):
        p = self.store / f"runs/{RUN_ID}/status/attempt-2.boot.json"
        return json.loads(p.read_text()) if p.exists() else None

    def test_lifetime_first_then_failure_recorded(self):
        self.fake_curl("exit 7\n")
        p = self.run_script()
        calls = self.calls_list()
        self.assertEqual(calls[0], "shutdown -h +60", calls)
        self.assertTrue(any(c.startswith("curl ") for c in calls))
        self.assertEqual(calls[-1], "shutdown -h now", calls)
        rec = self.boot_json()
        self.assertEqual((rec["n"], rec["state"], rec["reason"], rec["exit"]), (2, "failed", "boot-failed", 7), p.stdout)
        self.assertTrue((self.store / f"runs/{RUN_ID}/logs/attempt-2.boot.log").is_file())

    def test_signal_is_terminated(self):
        self.fake_curl('kill -TERM $PPID\nsleep 1\nexit 0\n')
        self.run_script()
        rec = self.boot_json()
        self.assertEqual((rec["state"], rec["reason"], rec["exit"]), ("terminated", "boot-signal", 143))
        self.assertEqual(self.calls_list()[-1], "shutdown -h now")

    def test_runner_status_marker_suppresses_boot_record(self):
        self.marker.touch()
        self.fake_curl("exit 7\n")
        self.run_script()
        self.assertIsNone(self.boot_json())
        self.assertEqual(self.calls_list()[-1], "shutdown -h now")

    def test_disabled_lifetime_has_no_in_os_shutdown(self):
        self.fake_curl("exit 7\n")
        self.run_script(disable_os_lifetime=True)
        self.assertEqual([c for c in self.calls_list() if c.startswith("shutdown")], ["shutdown -h now"])


if __name__ == "__main__":
    unittest.main()
