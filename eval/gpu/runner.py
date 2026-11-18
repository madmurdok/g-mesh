#!/usr/bin/env python3
"""Instance-side runner of a GPU training run (stdlib only).

Started by the user data script (user_data.sh.tmpl) after it has extracted the
code archive. Reads runs/<run-id>/manifest.json, fetches the env lock and the
data subsets (sha256-checked against each subset's MANIFEST.json), builds the
venv with uv, runs the smoke check, then runs the job's steps one after the
other. While a step runs it polls IMDSv2 for a spot interruption notice and
uploads a changed --out/checkpoint.pt; at the end it writes this attempt into
runs/<run-id>/status.json, uploads the log and shuts the instance down, on every
exit path.

S3 layout (one run): manifest.json, status.json, ckpt/<step>/checkpoint.pt
(+ checkpoint.prev.pt), out/<step>/..., logs/attempt-<n>.log.

Test hooks (environment):
  GM457_IMDS          IMDS base URL (default http://169.254.169.254)
  GM457_SHUTDOWN_CMD  shutdown command (default "shutdown -h now")
  GM457_S3_ROOT       a local directory standing in for the bucket
  GM457_PYTHON        an existing interpreter to run steps with (skips uv)
  GM457_AWS           the aws CLI command (default "aws")
  GM457_STATUS_MARKER file touched once status.json is written (default /var/run/gm457-status-written)
"""

import argparse
import hashlib
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

KILLED_EXIT = 75
STATUS_MARKER = "/var/run/gm457-status-written"

SMOKE = r'''
import subprocess, sys
import torch, transformers, sentence_transformers
print(f"smoke: torch {torch.__version__} cuda {torch.version.cuda} transformers {transformers.__version__} "
      f"sentence-transformers {sentence_transformers.__version__}", flush=True)
if "--cuda" in sys.argv:
    drv = subprocess.run(["nvidia-smi", "--query-gpu=driver_version", "--format=csv,noheader"],
                         capture_output=True, text=True, check=True).stdout.strip().splitlines()[0]
    print(f"smoke: driver {drv}", flush=True)
    if int(drv.split(".")[0]) < 525:
        sys.exit(f"smoke: driver {drv} < 525, the cu121 torch wheel needs >= 525")
    if not torch.cuda.is_available():
        sys.exit("smoke: torch.cuda.is_available() is False")
    print(f"smoke: device {torch.cuda.get_device_name(0)}", flush=True)
    device = "cuda"
else:
    device = "cpu"
torch.backends.cuda.matmul.allow_tf32 = False
torch.use_deterministic_algorithms(True)
cfg = transformers.BertConfig(vocab_size=128, hidden_size=32, num_hidden_layers=2, num_attention_heads=2,
                              intermediate_size=64, max_position_embeddings=64)
torch.manual_seed(0)
model = transformers.BertModel(cfg).to(device).eval()
ids = torch.randint(0, 128, (2, 16), device=device)
with torch.no_grad():
    h = model(input_ids=ids).last_hidden_state
print(f"smoke: forward ok on {device}, shape {tuple(h.shape)}, abs sum {float(h.abs().sum()):.6f}", flush=True)
'''


class RunnerSignal(Exception):
    """The runner itself got SIGTERM/SIGINT (OS shutdown, in-OS lifetime)."""


class StepFailure(Exception):
    """A failure with a status reason (no relaunch)."""

    def __init__(self, reason, msg):
        super().__init__(msg)
        self.reason = reason


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


# --- IMDSv2 --------------------------------------------------------------------

class Imds:
    def __init__(self, base=None, timeout=2.0):
        self.base = (base or os.environ.get("GM457_IMDS", "http://169.254.169.254")).rstrip("/")
        self.timeout = timeout
        self._token = None
        self._token_at = 0.0

    def token(self):
        if self._token is None or time.monotonic() - self._token_at > 3600:
            req = urllib.request.Request(f"{self.base}/latest/api/token", method="PUT",
                                         headers={"X-aws-ec2-metadata-token-ttl-seconds": "21600"})
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                self._token = r.read().decode()
            self._token_at = time.monotonic()
        return self._token

    def get(self, path):
        """Text of a metadata path, or None when it is absent (404)."""
        req = urllib.request.Request(f"{self.base}/{path.lstrip('/')}",
                                     headers={"X-aws-ec2-metadata-token": self.token()})
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                return r.read().decode()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if e.code == 401:
                self._token = None
            raise


def spot_notice(imds):
    """The instance-action document ({"action", "time"}) or None when no notice is pending."""
    text = imds.get("latest/meta-data/spot/instance-action")
    return json.loads(text) if text else None


def rebalance_notice(imds):
    text = imds.get("latest/meta-data/events/recommendations/rebalance")
    return json.loads(text) if text else None


# --- object store ----------------------------------------------------------------

class Store:
    """The run bucket through the aws CLI, or a local directory (GM457_S3_ROOT)."""

    def __init__(self, bucket, region):
        self.bucket = bucket
        self.region = region
        root = os.environ.get("GM457_S3_ROOT")
        self.root = Path(root) if root else None
        self.aws = shlex.split(os.environ.get("GM457_AWS", "aws"))

    def _aws(self, *args):
        return subprocess.run([*self.aws, "--region", self.region, *args], capture_output=True, text=True)

    def url(self, key):
        return f"s3://{self.bucket}/{key}"

    def get(self, key, dest):
        """Download key to dest; False when the key does not exist."""
        dest = Path(dest)
        dest.parent.mkdir(parents=True, exist_ok=True)
        if self.root is not None:
            src = self.root / key
            if not src.is_file():
                return False
            shutil.copyfile(src, dest)
            return True
        r = self._aws("s3", "cp", "--only-show-errors", self.url(key), str(dest))
        if r.returncode == 0:
            return True
        if "404" in r.stderr or "Not Found" in r.stderr or "NoSuchKey" in r.stderr:
            return False
        raise RuntimeError(f"aws s3 cp {key}: {r.stderr.strip()}")

    def put(self, src, key):
        if self.root is not None:
            dst = self.root / key
            dst.parent.mkdir(parents=True, exist_ok=True)
            tmp = dst.with_name(dst.name + ".part")
            shutil.copyfile(src, tmp)
            os.replace(tmp, dst)
            return
        r = self._aws("s3", "cp", "--only-show-errors", str(src), self.url(key))
        if r.returncode != 0:
            raise RuntimeError(f"aws s3 cp -> {key}: {r.stderr.strip()}")

    def copy(self, src_key, dst_key):
        """Server-side copy; False when src_key does not exist."""
        if self.root is not None:
            src = self.root / src_key
            if not src.is_file():
                return False
            self.put(src, dst_key)
            return True
        r = self._aws("s3", "cp", "--only-show-errors", self.url(src_key), self.url(dst_key))
        if r.returncode == 0:
            return True
        if "404" in r.stderr or "Not Found" in r.stderr or "NoSuchKey" in r.stderr:
            return False
        raise RuntimeError(f"aws s3 cp {src_key} -> {dst_key}: {r.stderr.strip()}")

    def sync_down(self, prefix, dest):
        dest = Path(dest)
        dest.mkdir(parents=True, exist_ok=True)
        if self.root is not None:
            src = self.root / prefix
            if not src.is_dir():
                raise RuntimeError(f"{prefix}: not in the store")
            shutil.copytree(src, dest, dirs_exist_ok=True)
            return
        r = self._aws("s3", "sync", "--only-show-errors", self.url(prefix.rstrip("/") + "/"), str(dest))
        if r.returncode != 0:
            raise RuntimeError(f"aws s3 sync {prefix}: {r.stderr.strip()}")


def verify_data(dest, manifest):
    """Every file listed in a data MANIFEST.json exists under dest with its size and sha256."""
    bad = []
    for f in manifest["files"]:
        p = Path(dest) / f["path"]
        if not p.is_file():
            bad.append(f"{f['path']}: missing")
        elif p.stat().st_size != f["size"]:
            bad.append(f"{f['path']}: size {p.stat().st_size} != {f['size']}")
        elif sha256_file(p) != f["sha256"]:
            bad.append(f"{f['path']}: sha256 differs")
    return bad


def attempt_state(reason):
    """done | interrupted (resumable) | terminated (the OS stopped the runner: a hard
    terminate or the in-OS lifetime; the launcher tells them apart by the expiry) |
    failed (no relaunch)."""
    if reason == "done":
        return "done"
    if reason in ("spot-notice", "stopped-resumable"):
        return "interrupted"
    if reason == "runner-signal":
        return "terminated"
    return "failed"


# --- the runner ------------------------------------------------------------------

class Runner:
    def __init__(self, a):
        self.a = a
        self.work = Path(a.work)
        self.src = Path(a.src)
        self.run_id = a.run_id
        self.attempt = a.attempt
        self.prefix = f"runs/{a.run_id}"
        self.store = Store(a.bucket, a.region)
        self.imds = Imds()
        self.log_path = self.work / "logs" / f"attempt-{a.attempt}.log"
        self.log_path.parent.mkdir(parents=True, exist_ok=True)
        self.log_f = open(self.log_path, "a", buffering=1)
        self.manifest = None
        self.status = {"runId": a.run_id, "attempts": [], "stepsDone": []}
        self.record = {"n": a.attempt, "start": now_iso(), "state": "running"}
        self.ckpt_sig = {}
        self.data_env = {}
        self.python = None
        self.proc = None
        self.rebalance_seen = False
        self.finished = False

    def log(self, msg):
        line = f"[runner {now_iso()}] {msg}"
        print(line, flush=True)
        self.log_f.write(line + "\n")

    # status.json: this attempt is the only writer while it runs
    def load_status(self):
        p = self.work / "status.prev.json"
        if self.store.get(f"{self.prefix}/status.json", p):
            self.status = json.loads(p.read_text())
            self.status.setdefault("attempts", [])
            self.status.setdefault("stepsDone", [])

    def write_status(self):
        attempts = [r for r in self.status["attempts"] if r.get("n") != self.attempt]
        self.status["attempts"] = sorted(attempts + [self.record], key=lambda r: r["n"])
        self.status["state"] = self.record["state"]
        self.status["updated"] = now_iso()
        p = self.work / "status.json"
        p.write_text(json.dumps(self.status, indent=1) + "\n")
        self.store.put(p, f"{self.prefix}/status.json")

    def identify(self):
        for key, path in (("instanceId", "latest/meta-data/instance-id"),
                          ("instanceType", "latest/meta-data/instance-type"),
                          ("az", "latest/meta-data/placement/availability-zone"),
                          ("lifecycle", "latest/meta-data/instance-life-cycle")):
            try:
                self.record[key] = self.imds.get(path)
            except Exception as e:  # identity is informational
                self.log(f"imds {path}: {e}")

    def fetch_inputs(self):
        mp = self.work / "manifest.json"
        if not self.store.get(f"{self.prefix}/manifest.json", mp):
            raise StepFailure("no-manifest", f"{self.prefix}/manifest.json is missing")
        self.manifest = json.loads(mp.read_text())
        lock = self.work / "requirements.txt"
        if not self.store.get(self.manifest["envKey"], lock):
            raise StepFailure("no-env-lock", f"{self.manifest['envKey']} is missing")
        for ds in self.manifest.get("data", []):
            dest = self.work / "data" / ds["name"]
            t0 = time.monotonic()
            self.store.sync_down(ds["prefix"], dest)
            man = json.loads((dest / "MANIFEST.json").read_text())
            bad = verify_data(dest, man)
            if bad:
                raise StepFailure("data-sha-mismatch", f"data {ds['name']}: " + "; ".join(bad[:5]))
            self.data_env[ds["env"]] = str(dest)
            secs = time.monotonic() - t0
            self.log(f"data {ds['name']}@{ds['sha'][:12]}: {len(man['files'])} files verified in {secs:.0f}s")

    def setup_env(self):
        override = os.environ.get("GM457_PYTHON")
        if override:
            self.python = override
            self.log(f"env: using {override} (GM457_PYTHON)")
            return
        venv = self.work / "venv"
        py = self.manifest.get("python", "3.11.15")
        t0 = time.monotonic()
        self.sh(["uv", "venv", "--python", py, str(venv)], "env-setup-failed")
        self.sh(["uv", "pip", "sync", "--python", str(venv / "bin" / "python"), "--require-hashes",
                 str(self.work / "requirements.txt")], "env-setup-failed")
        self.python = str(venv / "bin" / "python")
        self.log(f"env: venv ready in {time.monotonic() - t0:.0f}s")

    def sh(self, cmd, reason, env=None):
        self.log("$ " + " ".join(shlex.quote(c) for c in cmd))
        r = subprocess.run(cmd, stdout=self.log_f, stderr=subprocess.STDOUT, env=env)
        if r.returncode != 0:
            raise StepFailure(reason, f"{cmd[0]} exited {r.returncode}")

    def job_env(self):
        env = dict(os.environ)
        env.update(self.data_env)
        env["CUBLAS_WORKSPACE_CONFIG"] = ":4096:8"
        env["PYTHONUNBUFFERED"] = "1"
        env["GM457_RUN_ID"] = self.run_id
        env["GM457_ATTEMPT"] = str(self.attempt)
        return env

    def smoke(self):
        if not self.manifest.get("smoke", True):
            return
        cuda = any(s.get("device") == "cuda" for s in self.manifest["steps"])
        self.sh([self.python, "-c", SMOKE] + (["--cuda"] if cuda else []), "smoke-failed", env=self.job_env())

    # checkpoints
    def ckpt_key(self, step, name="checkpoint.pt"):
        return f"{self.prefix}/ckpt/{step}/{name}"

    def out_dir(self, step):
        return self.work / "out" / step

    def restore_checkpoint(self, step):
        out = self.out_dir(step)
        out.mkdir(parents=True, exist_ok=True)
        p = out / "checkpoint.pt"
        if self.store.get(self.ckpt_key(step), p):
            st = p.stat()
            self.ckpt_sig[step] = (st.st_mtime_ns, st.st_size)
            self.log(f"step {step}: restored {self.ckpt_key(step)} into {p}")
            return True
        return False

    def upload_checkpoint(self, step):
        """Upload --out/checkpoint.pt when it changed since the last upload; rotates the
        previous upload to checkpoint.prev.pt. Only the final name is read: the trainer
        writes <name>.tmp and os.replace()s it, and the open handle pins one inode."""
        p = self.out_dir(step) / "checkpoint.pt"
        stage = self.work / "stage" / step / "checkpoint.pt"
        try:
            f = open(p, "rb")
        except FileNotFoundError:
            return False
        with f:
            st = os.fstat(f.fileno())
            sig = (st.st_mtime_ns, st.st_size)
            if self.ckpt_sig.get(step) == sig:
                return False
            stage.parent.mkdir(parents=True, exist_ok=True)
            with open(stage, "wb") as g:
                shutil.copyfileobj(f, g, 1 << 20)
        self.store.copy(self.ckpt_key(step), self.ckpt_key(step, "checkpoint.prev.pt"))
        self.store.put(stage, self.ckpt_key(step))
        self.ckpt_sig[step] = sig
        self.log(f"step {step}: uploaded checkpoint ({sig[1]} bytes)")
        return True

    def upload_out(self, step):
        out = self.out_dir(step)
        n = 0
        for p in sorted(out.rglob("*")):
            if not p.is_file() or p.name.endswith(".tmp") or p.name in ("checkpoint.pt", "checkpoint.prev.pt"):
                continue
            self.store.put(p, f"{self.prefix}/out/{step}/{p.relative_to(out).as_posix()}")
            n += 1
        self.log(f"step {step}: uploaded {n} output files")

    def upload_logs(self):
        self.log_f.flush()
        self.store.put(self.log_path, f"{self.prefix}/logs/attempt-{self.attempt}.log")

    # the job
    def step_cmd(self, step):
        cmd = [self.python, str(self.src / step["entry"]), *step.get("args", []),
               "--out", str(self.out_dir(step["name"]))]
        if step.get("device"):
            cmd += ["--device", step["device"]]
        return cmd

    def stop_job(self, proc):
        """SIGTERM to the job's process group, SIGKILL after --grace seconds."""
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            return proc.wait()
        try:
            return proc.wait(timeout=self.a.grace)
        except subprocess.TimeoutExpired:
            self.log(f"job ignored SIGTERM for {self.a.grace}s, SIGKILL")
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            return proc.wait()

    def run_step(self, step):
        """(exit code, "spot-notice" | None)."""
        name = step["name"]
        cmd = self.step_cmd(step)
        self.log(f"step {name}: $ " + " ".join(shlex.quote(c) for c in cmd))
        proc = subprocess.Popen(cmd, cwd=self.src, env=self.job_env(), stdout=self.log_f,
                                stderr=subprocess.STDOUT, start_new_session=True)
        self.proc = proc
        last_ckpt = last_log = time.monotonic()
        try:
            while proc.poll() is None:
                try:
                    notice = spot_notice(self.imds)
                    if not self.rebalance_seen and rebalance_notice(self.imds):
                        self.rebalance_seen = True
                        self.log("rebalance recommendation received (logged only)")
                except Exception as e:  # IMDS hiccup: keep the job running
                    self.log(f"imds poll: {e}")
                    notice = None
                if notice:
                    self.log(f"spot interruption notice: {notice}")
                    self.record["notice"] = notice
                    rc = self.stop_job(proc)
                    self.log(f"step {name}: stopped, exit {rc}")
                    self.upload_checkpoint(name)
                    return rc, "spot-notice"
                t = time.monotonic()
                if t - last_ckpt >= self.a.upload_every:
                    self.upload_checkpoint(name)
                    last_ckpt = t
                if t - last_log >= self.a.log_every:
                    self.upload_logs()
                    last_log = t
                time.sleep(self.a.poll)
        finally:
            if proc.poll() is None:
                self.stop_job(proc)
            self.proc = None
        self.upload_checkpoint(name)
        return proc.returncode, None

    def run(self):
        self.identify()
        self.load_status()
        self.write_status()
        self.fetch_inputs()
        self.setup_env()
        self.smoke()
        for step in self.manifest["steps"]:
            name = step["name"]
            if name in self.status["stepsDone"]:
                self.log(f"step {name}: done in an earlier attempt, skipped")
                continue
            self.restore_checkpoint(name)
            rc, why = self.run_step(step)
            self.record.setdefault("steps", []).append({"name": name, "exit": rc})
            if why == "spot-notice":
                return rc, "spot-notice"
            if rc == 0:
                self.upload_out(name)
                self.status["stepsDone"].append(name)
                self.write_status()
            elif rc == KILLED_EXIT:
                return rc, "stopped-resumable"
            else:
                self.upload_out(name)
                return rc, "failed"
        return 0, "done"

    def finish(self, exit_code, reason, error=None):
        """Writes this attempt into status.json, uploads the log, shuts down. Every
        step is guarded so the shutdown command always runs."""
        if self.finished:
            return
        self.finished = True
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        self.record.update({"end": now_iso(), "exit": exit_code, "reason": reason, "state": attempt_state(reason)})
        if error:
            self.record["error"] = error
        self.log(f"finish: exit {exit_code}, reason {reason}, state {self.record['state']}")
        for what, fn in (("status", self.write_status), ("logs", self.upload_logs)):
            try:
                fn()
            except Exception as e:
                print(f"[runner] finish: {what} upload failed: {e}", file=sys.stderr, flush=True)
        try:
            Path(os.environ.get("GM457_STATUS_MARKER", STATUS_MARKER)).touch()
        except OSError:
            pass
        cmd = shlex.split(os.environ.get("GM457_SHUTDOWN_CMD", "shutdown -h now"))
        print(f"[runner] finish: $ {' '.join(cmd)}", flush=True)
        subprocess.run(cmd)


def parse(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bucket", required=True)
    ap.add_argument("--region", required=True, help="the bucket's region")
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--attempt", type=int, required=True)
    ap.add_argument("--work", default="/opt/gm457")
    ap.add_argument("--src", default="/opt/gm457/src", help="the extracted code archive")
    ap.add_argument("--grace", type=float, default=60, help="seconds between SIGTERM and SIGKILL")
    ap.add_argument("--poll", type=float, default=5, help="IMDS poll interval, seconds")
    ap.add_argument("--upload-every", type=float, default=30, help="checkpoint upload check interval, seconds")
    ap.add_argument("--log-every", type=float, default=60, help="log upload interval, seconds")
    return ap.parse_args(argv)


def main(argv=None):
    a = parse(argv)
    r = Runner(a)

    def on_signal(signum, _frame):
        raise RunnerSignal(signal.Signals(signum).name)

    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)
    code, reason, error = 1, "runner-error", None
    try:
        code, reason = r.run()
    except RunnerSignal as e:
        code, reason, error = 143, "runner-signal", str(e)
        if r.proc is not None and r.proc.poll() is None:
            r.stop_job(r.proc)
    except StepFailure as e:
        code, reason, error = 1, e.reason, str(e)
        r.log(f"failure: {e.reason}: {e}")
    except BaseException as e:
        code, reason, error = 1, "runner-error", f"{type(e).__name__}: {e}"
        r.log(traceback.format_exc())
    finally:
        r.finish(code, reason, error)
    return 128 - code if code < 0 else code


if __name__ == "__main__":
    sys.exit(main())
