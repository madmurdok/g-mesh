"""Offline tests of the spot GPU launcher (eval/gpu/gpu_train.py, iam/*.json,
user_data.sh.tmpl; design note docs/design/GM-457-spot-gpu-training.md,
section 4 behaviours 1-8, 19-21, D4-D6, and the review fixes F2, F3, F5-F7, F9).

AWS is never reached:
  - the pure functions get plain data;
  - the AWS paths run under moto's mock_aws (in-process, no network), with
    `create_fleet` answered by a stub that starts a moto instance (moto's
    instant spot fleet is not a faithful model) and failures injected through
    a client proxy;
  - `run --dry-run` runs with boto3 made unimportable, so any AWS call fails;
  - the module sets dummy credentials and points AWS_CONFIG_FILE /
    AWS_SHARED_CREDENTIALS_FILE at temp files with a dummy `default` profile,
    so a call that escaped moto would be refused by AWS, never billed.

Run (needs boto3 and moto; the runner tests run under the same command):
  uv run --no-project --python 3.11 --with boto3==1.44.6 \
      --with 'moto[ec2,s3,iam,scheduler,sts]==5.2.5' \
      python -m unittest discover -s eval/gpu -p 'test_gm457_*.py'
"""

import argparse
import base64
import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import gpu_train as G  # noqa: E402

_AWS_TMP = tempfile.TemporaryDirectory()
_SAFE_ENV = None


def setUpModule():
    global _SAFE_ENV
    d = Path(_AWS_TMP.name)
    (d / "config").write_text("[default]\nregion = us-east-1\n")
    (d / "credentials").write_text("[default]\naws_access_key_id = testing\naws_secret_access_key = testing\n")
    env = {k: v for k, v in os.environ.items() if not k.startswith("AWS_")}
    env.update(AWS_ACCESS_KEY_ID="testing", AWS_SECRET_ACCESS_KEY="testing", AWS_SESSION_TOKEN="testing",
               AWS_DEFAULT_REGION="us-east-1", AWS_CONFIG_FILE=str(d / "config"),
               AWS_SHARED_CREDENTIALS_FILE=str(d / "credentials"), AWS_EC2_METADATA_DISABLED="true")
    _SAFE_ENV = mock.patch.dict(os.environ, env, clear=True)
    _SAFE_ENV.start()


def tearDownModule():
    _SAFE_ENV.stop()
    _AWS_TMP.cleanup()


try:
    import boto3
    from botocore.exceptions import ClientError
    from moto import mock_aws
except ImportError:  # pragma: no cover
    boto3 = None

T0 = datetime(2026, 11, 19, 12, 0, tzinfo=timezone.utc)
CFG = G.load_config()
ACCOUNT = "123456789012"
BUCKET = G.bucket_name(CFG, ACCOUNT)
RUN_ID = "job-0123456789ab-20261119T1200"


def h(n):
    return T0 + timedelta(hours=n)


def all_actions(policy):
    out = []
    for st in policy["Statement"]:
        acts = st["Action"]
        out += [acts] if isinstance(acts, str) else acts
    return out


# --- pure functions -----------------------------------------------------------------------

class InstancePolicy(unittest.TestCase):
    """Behaviour 2, F2."""

    def setUp(self):
        self.p = G.render_iam("instance-policy", bucket="bkt")

    def test_no_ec2_or_iam_actions(self):
        acts = all_actions(self.p)
        self.assertTrue(acts)
        for a in acts:
            self.assertTrue(a.startswith("s3:"), a)
            self.assertNotIn("*", a, a)
        self.assertTrue(all(st["Effect"] == "Allow" for st in self.p["Statement"]))

    def grants(self, action):
        res = []
        for st in self.p["Statement"]:
            acts = [st["Action"]] if isinstance(st["Action"], str) else st["Action"]
            if action in acts:
                res += [st["Resource"]] if isinstance(st["Resource"], str) else st["Resource"]
        return sorted(res)

    def test_object_access_by_prefix(self):
        self.assertEqual(self.grants("s3:GetObject"), sorted(
            f"arn:aws:s3:::bkt/{p}/*" for p in ("code", "env", "data", "models", "runs", "ckpt")))
        self.assertEqual(self.grants("s3:PutObject"), ["arn:aws:s3:::bkt/ckpt/*", "arn:aws:s3:::bkt/runs/*"])

    def test_object_tagging_read_only_on_run_state(self):
        # `aws s3 cp` between S3 keys (checkpoint -> checkpoint.prev) reads the source's tags.
        self.assertEqual(self.grants("s3:GetObjectTagging"),
                         ["arn:aws:s3:::bkt/ckpt/*", "arn:aws:s3:::bkt/runs/*"])
        self.assertEqual(self.grants("s3:PutObjectTagging"), [])

    def test_list_bucket_without_prefix_condition(self):
        self.assertEqual(self.grants("s3:ListBucket"), ["arn:aws:s3:::bkt"])
        for st in self.p["Statement"]:
            if "s3:ListBucket" in ([st["Action"]] if isinstance(st["Action"], str) else st["Action"]):
                self.assertNotIn("Condition", st)


class FleetRequest(unittest.TestCase):
    """Behaviour 3."""

    def req(self, subnets=None):
        return G.fleet_request(run_id=RUN_ID, attempt=2, expires=h(1), ami_id="ami-1", root_device="/dev/sda1",
                               root_gb=95, profile="prof", sg_id="sg-1", user_data_text="#!/bin/bash\n",
                               subnets_by_type=subnets or {"g5.xlarge": ["s-a", "s-b"], "g5.2xlarge": ["s-a"]})

    def test_launch_template(self):
        lt = self.req()["LaunchTemplate"]
        d = lt["LaunchTemplateData"]
        self.assertEqual(d["InstanceInitiatedShutdownBehavior"], "terminate")
        self.assertEqual(d["MetadataOptions"]["HttpTokens"], "required")
        self.assertEqual(d["MetadataOptions"]["HttpPutResponseHopLimit"], 1)
        self.assertEqual(d["IamInstanceProfile"], {"Name": "prof"})
        ebs = d["BlockDeviceMappings"][0]["Ebs"]
        self.assertIs(ebs["DeleteOnTermination"], True)
        self.assertEqual(ebs["VolumeSize"], 95)
        self.assertEqual(base64.b64decode(d["UserData"]).decode(), "#!/bin/bash\n")
        specs = {s["ResourceType"]: s["Tags"] for s in d["TagSpecifications"]}
        self.assertEqual(set(specs), {"instance", "volume"})
        for tags in specs.values():
            tags = {t["Key"]: t["Value"] for t in tags}
            self.assertEqual(tags, {"gmesh-train": "1", "gmesh-run": RUN_ID, "gmesh-attempt": "2",
                                    "gmesh-expires": "2026-11-19T13:00:00Z"})

    def test_fleet(self):
        f = self.req()["CreateFleet"]
        self.assertEqual(f["Type"], "instant")
        self.assertEqual(f["TargetCapacitySpecification"],
                         {"TotalTargetCapacity": 1, "DefaultTargetCapacityType": "spot"})
        self.assertNotIn("OnDemandOptions", f)
        ov = f["LaunchTemplateConfigs"][0]["Overrides"]
        self.assertEqual(sorted((o["InstanceType"], o["SubnetId"]) for o in ov),
                         [("g5.2xlarge", "s-a"), ("g5.xlarge", "s-a"), ("g5.xlarge", "s-b")])
        self.assertEqual(f["LaunchTemplateConfigs"][0]["LaunchTemplateSpecification"]["LaunchTemplateName"],
                         self.req()["LaunchTemplate"]["LaunchTemplateName"])

    def test_no_offered_type_is_an_error(self):
        with self.assertRaises(ValueError):
            G.fleet_request(run_id=RUN_ID, attempt=1, expires=h(1), ami_id="a", root_device="/dev/sda1", root_gb=95,
                            profile="p", sg_id="s", user_data_text="", subnets_by_type={})


class UserData(unittest.TestCase):
    """Behaviour 4, A10 (text level; test_gm457_runner.py runs the script)."""

    def ud(self, **kw):
        args = dict(bucket="b", bucket_region="us-east-1", run_id=RUN_ID, attempt=1, code_key="code/x.tar.gz",
                    max_minutes=90, uv_version="0.12.1", grace=60)
        args.update(kw)
        return G.user_data(**args)

    def commands(self, text):
        return [ln.strip() for ln in text.splitlines() if ln.strip() and not ln.strip().startswith("#")]

    def test_shutdown_is_the_first_command(self):
        text = self.ud()
        cmds = self.commands(text)
        self.assertEqual(cmds[0], "shutdown -h +90")
        first_fetch = min(i for i, c in enumerate(cmds) if "curl" in c or " s3 " in c)
        self.assertGreater(first_fetch, 0)
        self.assertIn("trap finish EXIT", cmds)

    def test_disabled_lifetime(self):
        self.assertNotIn("shutdown -h +", self.ud(disable_os_lifetime=True))

    def test_unfilled_placeholder_and_size(self):
        with self.assertRaises(ValueError):
            self.ud(template="echo @@nope@@\n")
        with self.assertRaises(ValueError):
            self.ud(template="@@lifetime@@\n" + "#" * (16 * 1024))


class RelaunchDecision(unittest.TestCase):
    """Behaviour 8, A1, A2, F4 (boot-signal record)."""

    def d(self, record, attempt=1, state=None, expired=False, max_attempts=5):
        return G.relaunch_decision(record, attempt, max_attempts, state, expired)

    def test_table(self):
        intr = {"state": "interrupted", "reason": "spot-notice"}
        cases = [
            (dict(record=intr), "relaunch"),
            (dict(record=intr, state="terminated"), "relaunch"),
            (dict(record=intr, state="running"), "wait"),
            (dict(record=intr, state="shutting-down"), "wait"),
            (dict(record=None), "relaunch"),
            (dict(record=None, state="shutting-down"), "wait"),
            (dict(record=None, state="running"), "wait"),
            (dict(record=None, expired=True), "stop"),
            (dict(record={"state": "terminated", "reason": "runner-signal"}), "relaunch"),
            (dict(record={"state": "terminated", "reason": "runner-signal"}, expired=True), "stop"),
            (dict(record={"state": "terminated", "reason": "boot-signal"}), "relaunch"),
            (dict(record={"state": "failed", "reason": "boot-failed"}), "stop"),
            (dict(record={"state": "done"}, state="running"), "stop"),
            (dict(record={"state": "failed", "reason": "x"}), "stop"),
            (dict(record={"state": "running"}, state="running"), "wait"),
            (dict(record=intr, attempt=5), "stop"),
            (dict(record=None, attempt=5), "stop"),
        ]
        for kw, want in cases:
            with self.subTest(**{k: str(v) for k, v in kw.items()}):
                self.assertEqual(self.d(**kw)[0], want)

    def test_reasons(self):
        self.assertEqual(self.d(None), ("relaunch", "no-status"))
        self.assertEqual(self.d(None, expired=True), ("stop", "expired"))
        self.assertEqual(self.d({"state": "interrupted", "reason": "spot-notice"}), ("relaunch", "spot-notice"))
        self.assertEqual(self.d({"state": "done"}), ("stop", "done"))
        self.assertIn("boot-failed", self.d({"state": "failed", "reason": "boot-failed"})[1])


class LaunchErrors(unittest.TestCase):
    """Behaviour 20."""

    def test_explicit_deny_is_kill_switch(self):
        msg = ("You are not authorized to perform this operation. User: arn:aws:iam::1:user/u is not authorized to "
               "perform: ec2:CreateFleet with an explicit deny in an identity-based policy")
        for code in ("UnauthorizedOperation", "AccessDenied", "AccessDeniedException"):
            self.assertTrue(G.classify_launch_error(code, msg).startswith("budget kill switch is active"))

    def test_other_classes(self):
        self.assertTrue(G.classify_launch_error("UnauthorizedOperation", "not authorized").startswith("missing permission"))
        self.assertTrue(G.classify_launch_error("MaxSpotInstanceCountExceeded", "x").startswith("spot vCPU quota"))
        self.assertTrue(G.classify_launch_error("InsufficientInstanceCapacity", "x").startswith("no spot capacity"))
        self.assertEqual(G.classify_launch_error("Weird", "m"), "Weird: m")


class Cost(unittest.TestCase):
    """Behaviour 21, F5."""

    def test_piecewise(self):
        prices = [(h(1), 2.0), (h(0), 1.0)]
        self.assertAlmostEqual(G.cost_of([(h(0), h(2))], prices), 3.0)
        self.assertAlmostEqual(G.cost_of([(h(0.5), h(1.5))], prices), 1.5)

    def test_before_first_price_billed_at_first(self):
        self.assertAlmostEqual(G.cost_of([(h(-1), h(1))], [(h(0), 1.0), (h(0.5), 3.0)]), 1.0 + 0.5 + 1.5)

    def test_empty_interval_is_zero(self):
        self.assertEqual(G.cost_of([(h(1), h(1)), (h(2), h(1))], [(h(0), 5.0)]), 0.0)

    def test_attempt_end_sources(self):
        inst = {"StateTransitionReason": "User initiated (2026-11-19 13:00:00 GMT)"}
        rec = {"endObserved": "2026-11-19T14:00:00Z"}
        st = {"end": "2026-11-19T15:00:00Z"}
        self.assertEqual(G.attempt_end(rec, inst, st), (h(1), "StateTransitionReason"))
        self.assertEqual(G.attempt_end(rec, {}, st), (h(2), "watch"))
        self.assertEqual(G.attempt_end({}, None, st), (h(3), "status"))
        self.assertEqual(G.attempt_end({}, None, None)[1], "now")

    def test_termination_time(self):
        self.assertEqual(G.termination_time({"StateTransitionReason": "User initiated (2026-11-19 12:00:00 GMT)"}), T0)
        self.assertIsNone(G.termination_time({"StateTransitionReason": ""}))
        self.assertIsNone(G.termination_time({}))


class JobsAndIds(unittest.TestCase):
    """Behaviour 7, A7, A8, A9, F6."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def job_file(self, body):
        p = self.root / "job.toml"
        p.write_text(body)
        return p

    JOB = ('name = "j"\nrequirements = "r.txt"\nmax_hours = 1.5\nsmoke = true\n[vars]\nlr = "0.1"\n'
           '[[steps]]\nname = "s"\nentry = "e.py"\nargs = ["--lr", "{lr}"]\n')

    def test_load_job_and_set(self):
        job = G.load_job(self.job_file(self.JOB), G.parse_set(["lr=0.5", "smoke=false"]))
        self.assertEqual(job["max_minutes"], 90)
        self.assertNotIn("max_hours", job)
        self.assertEqual(job["steps"][0]["args"], ["--lr", "0.5"])
        self.assertIs(job["smoke"], False)
        with self.assertRaises(SystemExit):
            G.load_job(self.job_file(self.JOB), G.parse_set(["nope=1"]))
        with self.assertRaises(SystemExit):
            G.load_job(self.job_file(self.JOB + '[[steps]]\nname = "s"\nentry = "e.py"\n'))
        dup = self.JOB + ('[[data]]\nname = "a"\nenv = "X"\nlocal_root = "/"\ninclude = []\n'
                          '[[data]]\nname = "b"\nenv = "X"\nlocal_root = "/"\ninclude = []\n')
        with self.assertRaises(SystemExit):
            G.load_job(self.job_file(dup))

    def test_run_id(self):
        job = G.load_job(self.job_file(self.JOB))
        rid = G.run_id_of(job, T0)
        self.assertRegex(rid, r"^j-[0-9a-f]{12}-20261119T1200$")
        self.assertEqual(rid, G.run_id_of(G.load_job(self.job_file(self.JOB)), T0))
        self.assertEqual(G.run_id_of(job, T0, "u"), rid + "-u")
        other = G.load_job(self.job_file(self.JOB), G.parse_set(["lr=0.2"]))
        self.assertNotEqual(G.run_id_of(other, T0), rid)
        with self.assertRaises(SystemExit):
            G.run_id_of(job, T0, "x" * 40)
        self.assertLessEqual(len(G.schedule_name("x" * 54, 99)), 64)

    def test_data_manifest(self):
        (self.root / "d" / "sub").mkdir(parents=True)
        (self.root / "d" / "a.txt").write_text("a")
        (self.root / "d" / "sub" / "b.txt").write_text("bb")
        m = G.data_manifest(self.root / "d", ["sub/*", "a.txt"])
        self.assertEqual([(f["path"], f["size"]) for f in m["files"]], [("a.txt", 1), ("sub/b.txt", 2)])
        m2 = G.data_manifest(self.root / "d", ["a.txt", "sub/*"])
        self.assertEqual(G.data_sha(m), G.data_sha(m2))
        with self.assertRaises(SystemExit):
            G.data_manifest(self.root / "d", ["nothing/*"])

    def test_trailing_double_star_recurses(self):
        """The docstring's "`**` recurses", as jobs/gm436-*.toml use it ("mine/**"),
        on every supported Python (before 3.13 a trailing `**` matches directories only)."""
        (self.root / "d" / "sub" / "deep").mkdir(parents=True)
        (self.root / "d" / "sub" / "deep" / "c.txt").write_text("c")
        m = G.data_manifest(self.root / "d", ["sub/**"])
        self.assertEqual([f["path"] for f in m["files"]], ["sub/deep/c.txt"])

    def test_adapter_job_data_includes_run_dirs(self):
        job = G.load_job(HERE / "jobs" / "gm436-resume.toml")
        inc = [i for d in job["data"] for i in d["include"]]
        self.assertTrue(any(i.endswith("/manifest.json") and "work/runs/" in i for i in inc))
        self.assertTrue(any(i.endswith("/node_ids.txt") and "work/runs/" in i for i in inc))

    def test_instance_types(self):
        conf = ["g5.xlarge", "g5.2xlarge"]
        self.assertEqual(G.check_instance_types(["g5.xlarge"], conf), ["g5.xlarge"])
        self.assertEqual(G.check_instance_types(["g6.xlarge"], conf), ["g6.xlarge"])
        for bad in (["p3.2xlarge"], ["g5.xlarge", "g6.xlarge"], ["m5.large"], []):
            with self.assertRaises(SystemExit, msg=bad):
                G.check_instance_types(bad, conf)


class Schedules(unittest.TestCase):
    """A11."""

    def test_schedule_request(self):
        r = G.schedule_request(run_id=RUN_ID, attempt=3, instance_id="i-1", at=h(1), role_arn="arn:role")
        self.assertEqual(r["Name"], f"gmesh-{RUN_ID}-a3")
        self.assertEqual(r["ScheduleExpression"], "at(2026-11-19T13:00:00)")
        self.assertEqual(r["ScheduleExpressionTimezone"], "UTC")
        self.assertEqual(r["ActionAfterCompletion"], "DELETE")
        self.assertEqual(r["Target"]["Arn"], "arn:aws:scheduler:::aws-sdk:ec2:terminateInstances")
        self.assertEqual(json.loads(r["Target"]["Input"]), {"InstanceIds": ["i-1"]})


class CleanAndReapPure(unittest.TestCase):
    """Behaviour 19 (pure parts), A18, A19, F7."""

    def inst(self, iid, tags, state="running", itype="g5.xlarge"):
        return {"InstanceId": iid, "InstanceType": itype, "State": {"Name": state},
                "Tags": [{"Key": k, "Value": v} for k, v in tags.items()]}

    def test_expired_instances(self):
        insts = [self.inst("a", {"gmesh-train": "1", "gmesh-expires": "2026-11-19T11:00:00Z"}),
                 self.inst("b", {"gmesh-train": "1", "gmesh-expires": "2026-11-19T13:00:00Z"}),
                 self.inst("c", {"gmesh-expires": "2026-11-19T11:00:00Z"}),
                 self.inst("d", {"gmesh-train": "1"})]
        self.assertEqual(G.expired_instances(insts, T0), ["a", "d"])

    def test_clean_problems(self):
        self.assertEqual(G.clean_problems({"us-east-1": {}}), [])
        p = G.clean_problems({"us-east-1": {"tagged": [self.inst("i-1", {})],
                                            "volumes": [{"VolumeId": "vol-1", "State": "available"}]}})
        self.assertEqual(len(p), 2)
        self.assertTrue(any("i-1" in x for x in p) and any("vol-1" in x for x in p))

    def test_lifecycle_rules(self):
        rules = G.lifecycle_rules()
        mpu = [r for r in rules if "AbortIncompleteMultipartUpload" in r]
        self.assertEqual([r["AbortIncompleteMultipartUpload"]["DaysAfterInitiation"] for r in mpu], [1])
        exp = [r for r in rules if "Expiration" in r]
        self.assertEqual([(r["Filter"]["Prefix"], r["Expiration"]["Days"]) for r in exp], [("ckpt/", 30)])
        self.assertFalse(any(r["Filter"].get("Prefix", "").startswith("runs") for r in rules))

    def test_ckpt_prefix_outside_runs(self):
        self.assertEqual(G.ckpt_prefix(RUN_ID), f"ckpt/{RUN_ID}/")

    def test_is_gpu(self):
        for t in ("g5.xlarge", "g6e.2xlarge", "p3.2xlarge", "gr6.4xlarge", "inf2.xlarge", "trn1.2xlarge", "dl1.24xlarge",
                  "vt1.3xlarge"):
            self.assertTrue(G.is_gpu(t), t)
        for t in ("m5.large", "c7g.large", "t3.micro"):
            self.assertFalse(G.is_gpu(t), t)

    def test_kill_switch_problems(self):
        ours = self.inst("i-ours", {"gmesh-train": "1", "gmesh-run": RUN_ID})
        other = self.inst("i-other", {}, itype="p3.2xlarge", state="stopped")
        self.assertEqual(G.kill_switch_problems({}, None), [])
        self.assertTrue(G.kill_switch_problems({"us-west-2": [other]}, None))
        self.assertTrue(G.kill_switch_problems({"us-east-1": [ours]}, None))
        self.assertEqual(G.kill_switch_problems({"us-east-1": [ours]}, RUN_ID), [])
        self.assertTrue(G.kill_switch_problems({"us-east-1": [ours], "us-west-2": [other]}, RUN_ID))
        self.assertTrue(G.kill_switch_problems({}, RUN_ID))
        pending = self.inst("i-2", {"gmesh-train": "1", "gmesh-run": RUN_ID}, state="pending")
        self.assertTrue(G.kill_switch_problems({"us-east-1": [pending]}, RUN_ID))
        self.assertTrue(G.kill_switch_problems({"us-east-1": [ours, self.inst("i-3", {
            "gmesh-train": "1", "gmesh-run": RUN_ID})]}, RUN_ID))


class GitRepo(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.repo = Path(self.tmp.name) / "repo"
        self.repo.mkdir()
        env = {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@t", "HOME": self.tmp.name, "PATH": os.environ.get("PATH", "")}
        self.git_env = env
        self.git("init", "-q")
        (self.repo / "f.txt").write_text("one\n")
        self.git("add", "f.txt")
        self.git("commit", "-q", "-m", "c")

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.repo), *args], check=True, env=self.git_env, capture_output=True)


class DirtyTree(GitRepo):
    """Behaviour 5: a tree with tracked changes is refused before any upload."""

    def test_dirty_tree_refused(self):
        (self.repo / "f.txt").write_text("two\n")

        class NoS3:
            def __getattr__(self, name):
                raise AssertionError(f"s3.{name} called for a dirty tree")

        with self.assertRaises(SystemExit) as cm:
            G.upload_code(NoS3(), "b", self.repo)
        self.assertIn("uncommitted", str(cm.exception))

    def test_untracked_file_is_not_dirty(self):
        (self.repo / "new.txt").write_text("x")
        self.assertFalse(G.git_state(self.repo)[1])


class DryRun(unittest.TestCase):
    """A15: `run --dry-run` prints the requests and makes no AWS call (boto3 unimportable)."""

    def test_dry_run_without_boto3(self):
        with tempfile.TemporaryDirectory() as d:
            data = Path(d) / "data"
            data.mkdir()
            (data / "x.bin").write_bytes(b"123")
            job = Path(d) / "job.toml"
            job.write_text('name = "dry"\nrequirements = "eval/training/gm436/requirements-linux-cu121.txt"\n'
                           'max_minutes = 30\n[[steps]]\nname = "s"\nentry = "e.py"\ndevice = "cuda"\n'
                           f'[[data]]\nname = "ds"\nenv = "D"\nlocal_root = "{data}"\ninclude = ["*.bin"]\n')
            blocked = {m: None for m in ("boto3", "botocore", "botocore.exceptions")}
            buf = io.StringIO()
            with mock.patch.dict(sys.modules, blocked), mock.patch.object(G, "Aws", side_effect=AssertionError("Aws")), \
                    contextlib.redirect_stdout(buf):
                rc = G.main(["run", str(job), "--dry-run"])
        out = buf.getvalue()
        self.assertEqual(rc, 0)
        self.assertIn("/manifest.json", out)
        self.assertIn("create_launch_template + create_fleet", out)
        self.assertIn("shutdown -h +30", out)
        self.assertIn("scheduler.create_schedule", out)
        self.assertIn("data ds: 1 files", out)


# --- AWS paths under moto ---------------------------------------------------------------------

class Proxy:
    """A boto3 client whose calls are logged into `log` and can be overridden."""

    def __init__(self, name, client, log, overrides):
        self._name, self._client, self._log, self._overrides = name, client, log, overrides

    def __getattr__(self, attr):
        fn = self._overrides.get((self._name, attr)) or getattr(self._client, attr)
        if not callable(fn) or attr in ("meta", "exceptions"):
            return fn

        def call(*a, **k):
            self._log.append((self._name, attr, k))
            return fn(*a, **k)

        return call

    @property
    def meta(self):
        return self._client.meta

    @property
    def exceptions(self):
        return self._client.exceptions


class FakeAws:
    def __init__(self):
        self.log, self.overrides, self._clients = [], {}, {}

    def client(self, name, region):
        key = (name, region)
        if key not in self._clients:
            self._clients[key] = Proxy(name, boto3.client(name, region_name=region), self.log, self.overrides)
        return self._clients[key]

    def account(self):
        return self.client("sts", "us-east-1").get_caller_identity()["Account"]


def client_error(code, msg="m", op="Op"):
    return ClientError({"Error": {"Code": code, "Message": msg}}, op)


@unittest.skipIf(boto3 is None, "boto3/moto not installed (see the module docstring)")
class MotoCase(unittest.TestCase):
    def setUp(self):
        self.mock = mock_aws()
        self.mock.start()
        self.addCleanup(self.mock.stop)
        sleep = mock.patch.object(G.time, "sleep", lambda s: None)
        sleep.start()
        self.addCleanup(sleep.stop)
        self.aws = FakeAws()
        self.s3 = boto3.client("s3", region_name="us-east-1")
        self.ec2 = boto3.client("ec2", region_name="us-east-1")
        self.stdout = io.StringIO()
        out = contextlib.redirect_stdout(self.stdout)
        out.__enter__()
        self.addCleanup(out.__exit__, None, None, None)

    def ami(self):
        return self.ec2.describe_images(Owners=["amazon"])["Images"][0]["ImageId"]

    def run_instance(self, tags, itype="g5.xlarge", ec2=None):
        ec2 = ec2 or self.ec2
        img = ec2.describe_images(Owners=["amazon"])["Images"][0]["ImageId"]
        r = ec2.run_instances(ImageId=img, MinCount=1, MaxCount=1, InstanceType=itype, TagSpecifications=[
            {"ResourceType": "instance", "Tags": [{"Key": k, "Value": v} for k, v in tags.items()]}])
        return r["Instances"][0]["InstanceId"]

    def state(self, iid):
        return self.ec2.describe_instances(InstanceIds=[iid])["Reservations"][0]["Instances"][0]["State"]["Name"]

    def setup_args(self, check=False):
        return argparse.Namespace(profile=None, check=check, with_fis=False, ssm=False)

    def get_json(self, key):
        return json.loads(self.s3.get_object(Bucket=BUCKET, Key=key)["Body"].read())


class Setup(MotoCase):
    """Behaviour 1, A18: setup on an empty account, then a second setup changes nothing."""

    def test_setup_is_idempotent(self):
        with mock.patch.object(G, "check_launch_permissions", return_value=[]):
            self.assertEqual(G.cmd_setup(self.setup_args(), CFG), 0)
            first = self.stdout.getvalue()
            self.stdout.truncate(0)
            self.stdout.seek(0)
            self.assertEqual(G.cmd_setup(self.setup_args(), CFG), 0)
        self.assertIn("create bucket", first)
        self.assertEqual(self.stdout.getvalue().strip(), "no changes")
        self.s3.head_bucket(Bucket=BUCKET)
        iam = boto3.client("iam")
        prof = iam.get_instance_profile(InstanceProfileName=CFG["instance_role"])["InstanceProfile"]
        self.assertEqual([r["RoleName"] for r in prof["Roles"]], [CFG["instance_role"]])
        pol = iam.get_role_policy(RoleName=CFG["instance_role"], PolicyName=CFG["instance_role"] + "-policy")
        self.assertEqual(pol["PolicyDocument"], G.render_iam("instance-policy", bucket=BUCKET))
        iam.get_role(RoleName=CFG["scheduler_role"])
        for region in CFG["regions"]:
            self.assertIsNotNone(G.find_security_group(boto3.client("ec2", region_name=region), CFG["security_group"]))
        rules = self.s3.get_bucket_lifecycle_configuration(Bucket=BUCKET)["Rules"]
        self.assertEqual(sorted(r["ID"] for r in rules), sorted(r["ID"] for r in G.lifecycle_rules()))

    def test_check_mode_changes_nothing(self):
        with mock.patch.object(G, "check_launch_permissions", return_value=[]):
            G.cmd_setup(self.setup_args(check=True), CFG)
        self.assertIn("would change: create bucket", self.stdout.getvalue())
        self.assertEqual(self.s3.list_buckets()["Buckets"], [])


class Uploads(MotoCase, GitRepo):
    """Behaviours 5, 6: code once per sha; data once per content address, MANIFEST.json last."""

    def setUp(self):
        GitRepo.setUp(self)
        MotoCase.setUp(self)
        self.s3.create_bucket(Bucket=BUCKET)

    def puts(self):
        return [k.get("Key") or (k.get("Key") if "Key" in k else None) for (n, a, k) in self.aws.log
                if n == "s3" and a == "put_object"]

    def test_code_uploaded_once(self):
        s3 = self.aws.client("s3", "us-east-1")
        sha, key = G.upload_code(s3, BUCKET, self.repo)
        self.assertEqual(key, f"code/{sha}.tar.gz")
        G.upload_code(s3, BUCKET, self.repo)
        self.assertEqual([k for k in self.puts()], [key])
        self.s3.head_object(Bucket=BUCKET, Key=key)

    def test_data_synced_once_manifest_last(self):
        root = Path(self.tmp.name) / "data"
        (root / "sub").mkdir(parents=True)
        (root / "a.txt").write_text("a")
        (root / "sub" / "b.txt").write_text("b")
        ds = {"name": "ds", "env": "D", "local_root": str(root), "include": ["a.txt", "sub/*"]}
        s3 = self.aws.client("s3", "us-east-1")
        r = G.sync_data(s3, BUCKET, ds)
        self.assertTrue(r["uploaded"])
        writes = [(a, k.get("Key") or (len(k) == 0 and None)) for (n, a, k) in self.aws.log
                  if n == "s3" and a in ("put_object", "upload_file")]
        self.assertEqual(writes[-1], ("put_object", r["prefix"] + "MANIFEST.json"))
        self.assertEqual(len(writes), 3)
        man = self.get_json(r["prefix"] + "MANIFEST.json")
        self.assertEqual(G.data_sha(man), r["sha"])
        self.aws.log.clear()
        r2 = G.sync_data(s3, BUCKET, ds)
        self.assertFalse(r2["uploaded"])
        self.assertEqual([a for (n, a, k) in self.aws.log if a in ("put_object", "upload_file")], [])


class SpotFakes:
    """Fakes of the spot-request and fleet calls (moto's instant fleets are not a faithful model):
    `self.requests` / `self.fleets` hold what AWS would list; cancels and deletes are recorded in
    `self.cancelled`, `self.deleted` and, in order, `self.events`. Mix into a MotoCase."""

    def fake_spot(self):
        self.requests, self.fleets, self.cancelled, self.deleted, self.events = [], {}, [], [], []
        case = self

        def describe(**kw):
            f = {x["Name"]: x["Values"] for x in kw.get("Filters", [])}
            return {"SpotInstanceRequests": [
                r for r in case.requests if r["State"] in f.get("state", ["open", "active"])
                and ("instance-id" not in f or r.get("InstanceId") in f["instance-id"])]}

        def cancel(**kw):
            case.cancelled.append(list(kw["SpotInstanceRequestIds"]))
            case.events.append(("cancel", list(kw["SpotInstanceRequestIds"])))
            for r in case.requests:
                if r["SpotInstanceRequestId"] in kw["SpotInstanceRequestIds"]:
                    r["State"] = "cancelled"
            return {}

        def delete(**kw):
            case.deleted.append(dict(kw))
            case.events.append(("delete", list(kw["FleetIds"])))
            for fid in kw["FleetIds"]:
                case.fleets.pop(fid, None)
            return {"SuccessfulFleetDeletions": [{"FleetId": f} for f in kw["FleetIds"]],
                    "UnsuccessfulFleetDeletions": []}

        class Pages:
            def paginate(self, **kw):
                return [{"Fleets": list(case.fleets.values())}]

        def paginator(name):
            return Pages() if name == "describe_fleets" else case.ec2.get_paginator(name)

        for k, fn in (("describe_spot_instance_requests", describe), ("cancel_spot_instance_requests", cancel),
                      ("delete_fleets", delete), ("get_paginator", paginator)):
            self.aws.overrides[("ec2", k)] = fn

    def add_request(self, rid, iid, state="active"):
        self.requests.append({"SpotInstanceRequestId": rid, "InstanceId": iid, "State": state})

    def add_fleet(self, fid, iids=(), age=timedelta(hours=1), run=RUN_ID):
        self.fleets[fid] = {"FleetId": fid, "FleetState": "active", "CreateTime": datetime.now(timezone.utc) - age,
                            "Tags": [{"Key": "gmesh-train", "Value": "1"}, {"Key": "gmesh-run", "Value": run}],
                            "Instances": [{"InstanceIds": list(iids)}] if iids else []}

    def patch_aws(self):
        patch = mock.patch.object(G, "Aws", lambda profile=None: self.aws)
        patch.start()
        self.addCleanup(patch.stop)


class ReapableFleets(unittest.TestCase):
    """Pure: which fleets reap may delete (GM-732)."""
    NOW = T0
    OLD, YOUNG = T0 - timedelta(hours=1), T0 - timedelta(minutes=1)

    def fleet(self, fid, created, iids=()):
        return {"FleetId": fid, "CreateTime": created, "Instances": [{"InstanceIds": list(iids)}] if iids else []}

    def reapable(self, fleets, live=(), ended=()):
        return [f["FleetId"] for f in G.reapable_fleets(fleets, set(live), set(ended), self.NOW)]

    def test_fleet_with_a_live_instance_is_never_reaped(self):
        self.assertEqual(self.reapable([self.fleet("f", self.OLD, ["i-1"])], live=["i-1"]), [])
        self.assertEqual(self.reapable([self.fleet("f", self.OLD, ["i-1", "i-2"])], live=["i-1"], ended=["i-2"]), [])

    def test_old_fleet_without_a_live_instance_is_reaped_whatever_its_instances(self):
        fleets = [self.fleet("none", self.OLD), self.fleet("unlisted", self.OLD, ["i-9"])]
        self.assertEqual(self.reapable(fleets), ["none", "unlisted"])

    def test_young_fleet_is_reaped_only_when_all_its_instances_ended(self):
        fleets = [self.fleet("ended", self.YOUNG, ["i-1", "i-2"]), self.fleet("partly", self.YOUNG, ["i-1", "i-3"]),
                  self.fleet("none", self.YOUNG), self.fleet("unlisted", self.YOUNG, ["i-4"])]
        self.assertEqual(self.reapable(fleets, ended=["i-1", "i-2"]), ["ended"])


class ReleaseSpot(unittest.TestCase):
    """release_spot on a hand-made client: what it asks AWS for, and what counts as a problem."""

    class Ec2:
        def __init__(self, requests=(), delete_resp=None, cancel_error=None):
            self.requests, self.cancel_error = list(requests), cancel_error
            self.delete_resp = delete_resp or {"SuccessfulFleetDeletions": [], "UnsuccessfulFleetDeletions": []}
            self.describes, self.cancels, self.deletes = [], [], []

        def describe_spot_instance_requests(self, **kw):
            self.describes.append(kw)
            return {"SpotInstanceRequests": [{"SpotInstanceRequestId": r} for r in self.requests]}

        def cancel_spot_instance_requests(self, **kw):
            if self.cancel_error:
                raise self.cancel_error
            self.cancels.append(kw)

        def delete_fleets(self, **kw):
            self.deletes.append(kw)
            return self.delete_resp

    def release(self, ec2, iids=("i-1",), fleets=("fleet-1",)):
        with contextlib.redirect_stdout(io.StringIO()):
            return G.release_spot(ec2, list(iids), list(fleets), "t")

    def test_requests_are_matched_by_instance_id_and_cancelled(self):
        ec2 = self.Ec2(requests=["sir-b", "sir-a"])
        self.assertEqual(self.release(ec2, iids=["i-2", "i-1"], fleets=[]), [])
        flt = {f["Name"]: f["Values"] for f in ec2.describes[0]["Filters"]}
        self.assertEqual(flt["instance-id"], ["i-1", "i-2"])
        self.assertEqual(flt["state"], ["open", "active"])
        self.assertEqual(ec2.cancels, [{"SpotInstanceRequestIds": ["sir-a", "sir-b"]}])

    def test_no_open_request_means_no_cancel_call(self):
        ec2 = self.Ec2()
        self.assertEqual(self.release(ec2), [])
        self.assertEqual(ec2.cancels, [])

    def test_fleets_are_deleted_with_terminate_instances(self):
        """An instant fleet cannot be deleted with TerminateInstances=False."""
        ec2 = self.Ec2()
        self.release(ec2)
        self.assertEqual(ec2.deletes, [{"FleetIds": ["fleet-1"], "TerminateInstances": True}])

    def test_fleet_already_gone_is_not_a_problem_other_errors_are(self):
        gone = {"SuccessfulFleetDeletions": [], "UnsuccessfulFleetDeletions": [
            {"FleetId": "fleet-1", "Error": {"Code": "fleetIdDoesNotExist", "Message": "m"}}]}
        self.assertEqual(self.release(self.Ec2(delete_resp=gone)), [])
        bad = {"SuccessfulFleetDeletions": [], "UnsuccessfulFleetDeletions": [
            {"FleetId": "fleet-1", "Error": {"Code": "unexpectedError", "Message": "m"}}]}
        problems = self.release(self.Ec2(delete_resp=bad))
        self.assertEqual(len(problems), 1)
        self.assertIn("fleet-1", problems[0])

    def test_failed_cancel_keeps_every_fleet_and_is_reported_not_raised(self):
        ec2 = self.Ec2(requests=["sir-a"], cancel_error=client_error("UnauthorizedOperation"))
        problems = self.release(ec2, fleets=["fleet-1", "fleet-2"])
        self.assertEqual(ec2.deletes, [])
        self.assertTrue(any("not cancelled" in p for p in problems))


class Reap(MotoCase, SpotFakes):
    """Behaviour 19, A13."""

    def test_reap_terminates_only_expired_tagged(self):
        past, future = "2026-01-01T00:00:00Z", "2099-01-01T00:00:00Z"
        a = self.run_instance({"gmesh-train": "1", "gmesh-run": "r", "gmesh-expires": past})
        b = self.run_instance({"gmesh-train": "1", "gmesh-run": "r", "gmesh-expires": future})
        c = self.run_instance({"gmesh-run": "r", "gmesh-expires": past})
        G.reap(self.aws, CFG)
        self.assertIn(self.state(a), ("shutting-down", "terminated"))
        self.assertEqual(self.state(b), "running")
        self.assertEqual(self.state(c), "running")

    def test_reap_deletes_dead_schedules_and_old_templates(self):
        b = self.run_instance({"gmesh-train": "1", "gmesh-expires": "2099-01-01T00:00:00Z"})
        sch = boto3.client("scheduler", region_name="us-east-1")
        for name, iid in (("gmesh-live-a1", b), ("gmesh-dead-a1", "i-0gone")):
            sch.create_schedule(**G.schedule_request(run_id=name[6:-3], attempt=1, instance_id=iid, at=h(1),
                                                     role_arn=f"arn:aws:iam::{ACCOUNT}:role/x"))
        self.ec2.create_launch_template(LaunchTemplateName="gmesh-t", LaunchTemplateData={"ImageId": self.ami()},
                                        TagSpecifications=[{"ResourceType": "launch-template",
                                                            "Tags": [{"Key": "gmesh-train", "Value": "1"}]}])
        self.ec2.create_launch_template(LaunchTemplateName="other", LaunchTemplateData={"ImageId": self.ami()})
        G.reap(self.aws, CFG)
        names = [s["Name"] for s in sch.list_schedules()["Schedules"]]
        self.assertEqual(names, ["gmesh-live-a1"])
        self.assertEqual(len(self.ec2.describe_launch_templates()["LaunchTemplates"]), 2)
        with mock.patch.object(G, "utcnow", return_value=datetime.now(timezone.utc) + timedelta(minutes=11)):
            G.reap(self.aws, CFG)
        self.assertEqual([t["LaunchTemplateName"] for t in self.ec2.describe_launch_templates()["LaunchTemplates"]],
                         ["other"])

    def test_reap_cancels_ended_requests_and_deletes_stale_fleets(self):
        """GM-732: reap cancels the active request of a tagged ended instance and deletes a fleet
        with no live instance; a fleet with a live instance (and its request) is left alone."""
        self.fake_spot()
        far = "2099-01-01T00:00:00Z"
        dead = self.run_instance({"gmesh-train": "1", "gmesh-run": "r", "gmesh-expires": far})
        live = self.run_instance({"gmesh-train": "1", "gmesh-run": "r2", "gmesh-expires": far})
        self.ec2.terminate_instances(InstanceIds=[dead])
        self.add_request("sir-dead", dead)
        self.add_request("sir-live", live)
        self.add_fleet("fleet-old")
        self.add_fleet("fleet-live", [live])
        self.add_fleet("fleet-young", age=timedelta(minutes=1))
        self.assertEqual(G.reap(self.aws, dict(CFG, regions=["us-east-1"])), [])
        self.assertEqual(self.cancelled, [["sir-dead"]])
        self.assertEqual(sorted(self.fleets), ["fleet-live", "fleet-young"])
        self.assertEqual(self.deleted, [{"FleetIds": ["fleet-old"], "TerminateInstances": True}])

    def test_reap_deletes_a_young_fleet_whose_instance_was_just_expired(self):
        self.fake_spot()
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": "r", "gmesh-expires": "2026-01-01T00:00:00Z"})
        self.add_request("sir-exp", iid)
        self.add_fleet("fleet-1", [iid], age=timedelta(minutes=1))
        self.assertEqual(G.reap(self.aws, dict(CFG, regions=["us-east-1"])), [])
        self.assertEqual(self.cancelled, [["sir-exp"]])
        self.assertEqual(self.fleets, {})

    def test_reap_reports_a_failed_release(self):
        self.fake_spot()
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": "r", "gmesh-expires": "2026-01-01T00:00:00Z"})
        self.add_request("sir-exp", iid)
        self.add_fleet("fleet-1", [iid])

        def broken(**kw):
            raise client_error("UnauthorizedOperation")

        self.aws.overrides[("ec2", "cancel_spot_instance_requests")] = broken
        self.assertTrue(G.reap(self.aws, dict(CFG, regions=["us-east-1"])))
        self.assertEqual(self.deleted, [])


class VerifyClean(MotoCase):
    """Behaviour 19: verify-clean fails on a live tagged instance and on an available run volume."""

    def args(self):
        return argparse.Namespace(profile=None, wait=0)

    def test_clean_account(self):
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 0)

    def test_running_tagged_instance(self):
        iid = self.run_instance({"gmesh-train": "1"})
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 1)
        self.assertIn(iid, self.stdout.getvalue())

    def test_available_run_volume(self):
        v = self.ec2.create_volume(AvailabilityZone="us-east-1a", Size=1, TagSpecifications=[
            {"ResourceType": "volume", "Tags": [{"Key": "gmesh-run", "Value": "r"}]}])["VolumeId"]
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 1)
        self.assertIn(v, self.stdout.getvalue())


class LiveSpotRequests(unittest.TestCase):
    """Pure: a spot request is a leak only while it can hold or launch an instance."""

    def test_terminated_instance_request_dropped(self):
        reqs = [{"SpotInstanceRequestId": "sir-1", "InstanceId": "i-dead"}]
        self.assertEqual(G.live_spot_requests(reqs, set()), [])
        self.assertEqual(G.live_spot_requests(reqs, {"i-other"}), [])

    def test_open_request_without_instance_kept(self):
        reqs = [{"SpotInstanceRequestId": "sir-2"}, {"SpotInstanceRequestId": "sir-3", "InstanceId": ""}]
        self.assertEqual(G.live_spot_requests(reqs, set()), reqs)

    def test_request_with_live_instance_kept(self):
        reqs = [{"SpotInstanceRequestId": "sir-4", "InstanceId": "i-live"},
                {"SpotInstanceRequestId": "sir-5", "InstanceId": "i-dead"}]
        self.assertEqual(G.live_spot_requests(reqs, {"i-live"}), [reqs[0]])


class VerifyCleanSpot(MotoCase):
    """verify-clean through snapshot_region: spot requests of ended instances are not leaks."""

    def args(self):
        return argparse.Namespace(profile=None, wait=0)

    def instance(self):
        """An untagged non-GPU instance, so only a spot line can report it."""
        return self.run_instance({"other": "x"}, itype="t3.micro")

    def active_request(self, request):
        """Make describe_spot_instance_requests return `request` (moto closes requests itself)."""
        self.aws.overrides[("ec2", "describe_spot_instance_requests")] = \
            lambda **kw: {"SpotInstanceRequests": [request]}
        patch = mock.patch.object(G, "Aws", lambda profile=None: self.aws)
        patch.start()
        self.addCleanup(patch.stop)

    def test_terminated_instance_request_not_reported(self):
        iid = self.instance()
        self.ec2.terminate_instances(InstanceIds=[iid])
        self.assertEqual(self.state(iid), "terminated")
        self.active_request({"SpotInstanceRequestId": "sir-term0001", "InstanceId": iid, "State": "active"})
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 0)
        self.assertNotIn("sir-term0001", self.stdout.getvalue())

    def test_running_instance_request_reported(self):
        iid = self.instance()
        self.assertEqual(self.state(iid), "running")
        self.active_request({"SpotInstanceRequestId": "sir-run00001", "InstanceId": iid, "State": "active"})
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 1)
        self.assertIn("sir-run00001", self.stdout.getvalue())

    def test_open_request_without_instance_reported(self):
        self.active_request({"SpotInstanceRequestId": "sir-open0001", "State": "open"})
        self.assertEqual(G.cmd_verify_clean(self.args(), CFG), 1)
        self.assertIn("sir-open0001", self.stdout.getvalue())


class VerifyCleanRelease(MotoCase, SpotFakes):
    """GM-732: verify-clean fails on what the launcher should have released."""

    def run_verify(self):
        self.patch_aws()
        return G.cmd_verify_clean(argparse.Namespace(profile=None, wait=0), dict(CFG, regions=["us-east-1"]))

    def test_active_request_of_our_terminated_instance_is_reported(self):
        self.fake_spot()
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": RUN_ID})
        self.ec2.terminate_instances(InstanceIds=[iid])
        self.assertEqual(self.state(iid), "terminated")
        self.add_request("sir-ours001", iid)
        self.assertEqual(self.run_verify(), 1)
        self.assertIn("sir-ours001", self.stdout.getvalue())

    def test_active_gmesh_fleet_is_reported(self):
        self.fake_spot()
        self.add_fleet("fleet-leak001")
        self.assertEqual(self.run_verify(), 1)
        self.assertIn("fleet-leak001", self.stdout.getvalue())

    def test_clean_when_released(self):
        self.fake_spot()
        self.assertEqual(self.run_verify(), 0)


class Launch(MotoCase, SpotFakes):
    """A11, A12, F3: launch_attempt / adopt_instance / schedule_terminate."""

    def setUp(self):
        super().setUp()
        self.s3.create_bucket(Bucket=BUCKET)
        self.ec2.create_security_group(GroupName=CFG["security_group"], Description="d",
                                       VpcId=G.default_vpc(self.ec2))
        self.m = {"runId": RUN_ID, "region": "us-east-1", "job": {"grace_seconds": 60}, "codeKey": "code/x.tar.gz",
                  "maxMinutes": 60, "amiId": self.ami(), "instanceTypes": ["g5.xlarge"], "maxAttempts": 5}
        self.aws.overrides[("ec2", "create_fleet")] = self.fleet_ok

    def fleet_ok(self, **kw):
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": RUN_ID})
        self.launched = iid
        return {"FleetId": "fleet-1", "Errors": [], "Instances": [{
            "InstanceIds": [iid], "InstanceType": "g5.xlarge",
            "LaunchTemplateAndOverrides": {"Overrides": {"AvailabilityZone": "us-east-1a"}}}]}

    def ctx(self):
        return G.Ctx(self.aws, CFG, self.m, BUCKET)

    def launches(self):
        return self.get_json(f"runs/{RUN_ID}/launches.json")

    def templates(self):
        return self.ec2.describe_launch_templates()["LaunchTemplates"]

    def test_launch_records_arms_and_cleans_template(self):
        rec = G.launch_attempt(self.ctx(), 1)
        self.assertEqual(rec["instanceId"], self.launched)
        saved = self.launches()["attempts"]
        self.assertEqual([r["instanceId"] for r in saved], [self.launched])
        self.assertEqual(saved[0]["schedule"], f"gmesh-{RUN_ID}-a1")
        self.assertNotIn("outcome", saved[0])
        self.assertEqual(self.templates(), [])
        s = boto3.client("scheduler", region_name="us-east-1").get_schedule(Name=f"gmesh-{RUN_ID}-a1")
        self.assertEqual(json.loads(s["Target"]["Input"]), {"InstanceIds": [self.launched]})
        self.assertEqual(self.state(self.launched), "running")
        fleet = [k for (n, a, k) in self.aws.log if a == "create_fleet"][0]
        self.assertEqual([o["InstanceType"] for o in fleet["LaunchTemplateConfigs"][0]["Overrides"]],
                         ["g5.xlarge"] * len(fleet["LaunchTemplateConfigs"][0]["Overrides"]))

    def test_record_saved_before_describe(self):
        G.launch_attempt(self.ctx(), 1)
        log = self.aws.log
        first_save = next(i for i, (n, a, k) in enumerate(log)
                          if a == "put_object" and k["Key"].endswith("launches.json")
                          and self.launched.encode() in k["Body"])
        first_look = next(i for i, (n, a, k) in enumerate(log)
                          if (n, a) in (("ec2", "describe_instances"), ("ec2", "get_waiter"))
                          and i > next(j for j, x in enumerate(log) if x[1] == "create_fleet"))
        self.assertLess(first_save, first_look)

    def prior_attempt(self, used_minutes):
        """launches.json with one earlier attempt that ran `used_minutes`, ending at T0 - 5 min."""
        end = T0 - timedelta(minutes=5)
        rec = {"n": 1, "instanceId": "i-old", "launchTime": G.iso(end - timedelta(minutes=used_minutes)),
               "endObserved": G.iso(end), "expires": G.iso(T0 + timedelta(hours=9)), "outcome": "interrupted"}
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/launches.json",
                           Body=json.dumps({"runId": RUN_ID, "attempts": [rec]}).encode())

    def launched_lifetime(self):
        """(minutes in the user data's shutdown line, the attempt's recorded expiry)."""
        kw = [k for (n, a, k) in self.aws.log if a == "create_launch_template"][-1]
        text = base64.b64decode(kw["LaunchTemplateData"]["UserData"]).decode()
        line = next(x.strip() for x in text.splitlines() if x.strip().startswith("shutdown -h +"))
        return int(line.rsplit("+", 1)[1]), self.launches()["attempts"][-1]["expires"]

    def test_job_scope_gives_the_next_attempt_what_the_earlier_ones_left(self):
        self.m.update(maxMinutes=240)
        self.m["job"]["max_minutes_scope"] = "job"
        self.prior_attempt(90)
        with mock.patch.object(G, "utcnow", lambda: T0):
            G.launch_attempt(self.ctx(), 2)
        minutes, expires = self.launched_lifetime()
        self.assertEqual(minutes, 150)
        self.assertEqual(expires, G.iso(T0 + timedelta(minutes=150)))

    def test_attempt_scope_keeps_max_minutes_per_attempt(self):
        self.m.update(maxMinutes=240)
        self.prior_attempt(90)
        with mock.patch.object(G, "utcnow", lambda: T0):
            G.launch_attempt(self.ctx(), 2)
        minutes, expires = self.launched_lifetime()
        self.assertEqual(minutes, 240)
        self.assertEqual(expires, G.iso(T0 + timedelta(minutes=240)))

    def test_job_scope_refuses_a_launch_once_the_lifetime_is_used(self):
        self.m.update(maxMinutes=240)
        self.m["job"]["max_minutes_scope"] = "job"
        self.prior_attempt(235)
        with mock.patch.object(G, "utcnow", lambda: T0), self.assertRaises(G.LaunchError) as cm:
            G.launch_attempt(self.ctx(), 2)
        self.assertIn("job lifetime used", str(cm.exception))
        self.assertEqual([a for (n, a, k) in self.aws.log if a in ("create_fleet", "create_launch_template")], [])

    def check_orphan_terminated(self):
        with self.assertRaises(SystemExit):
            G.launch_attempt(self.ctx(), 1)
        self.assertIn(self.state(self.launched), ("shutting-down", "terminated"))
        rec = self.launches()["attempts"][-1]
        self.assertEqual(rec["instanceId"], self.launched)
        self.assertIn("terminated by the launcher", rec["outcome"])

    def test_describe_failure_terminates_instance(self):
        calls = []

        def broken(**kw):
            calls.append(kw)
            raise client_error("RequestLimitExceeded")

        self.aws.overrides[("ec2", "describe_instances")] = broken
        self.check_orphan_terminated()
        self.assertGreater(len(calls), 1, "describe was not retried")

    def test_schedule_failure_terminates_instance(self):
        def broken(**kw):
            raise client_error("ValidationException")

        self.aws.overrides[("scheduler", "create_schedule")] = broken
        self.check_orphan_terminated()

    def test_existing_schedule_counts_as_created(self):
        real = boto3.client("scheduler", region_name="us-east-1")

        def twice(**kw):
            real.create_schedule(**kw)
            return real.create_schedule(**kw)

        self.aws.overrides[("scheduler", "create_schedule")] = twice
        rec = G.launch_attempt(self.ctx(), 1)
        self.assertEqual(rec["schedule"], f"gmesh-{RUN_ID}-a1")
        self.assertEqual(self.state(self.launched), "running")

    def test_template_delete_error_only_warns(self):
        def broken(**kw):
            raise client_error("InternalError")

        self.aws.overrides[("ec2", "delete_launch_template")] = broken
        rec = G.launch_attempt(self.ctx(), 1)
        self.assertEqual(self.state(rec["instanceId"]), "running")
        self.assertIn("WARNING", self.stdout.getvalue())

    def test_fleet_error_still_deletes_template(self):
        def broken(**kw):
            raise client_error("InsufficientInstanceCapacity", "no capacity")

        self.aws.overrides[("ec2", "create_fleet")] = broken
        with self.assertRaises(G.LaunchError) as cm:
            G.launch_attempt(self.ctx(), 1)
        self.assertTrue(str(cm.exception).startswith("no spot capacity"))
        self.assertEqual(self.templates(), [])

    def test_empty_fleet_raises_first_error(self):
        self.aws.overrides[("ec2", "create_fleet")] = lambda **kw: {
            "FleetId": "f", "Instances": [], "Errors": [
                {"ErrorCode": "UnauthorizedOperation", "ErrorMessage": "blocked with an explicit deny"},
                {"ErrorCode": "InsufficientInstanceCapacity", "ErrorMessage": "x"}]}
        with self.assertRaises(G.LaunchError) as cm:
            G.launch_attempt(self.ctx(), 1)
        self.assertTrue(str(cm.exception).startswith("budget kill switch is active"))
        self.assertEqual(self.templates(), [])

    def launch_with_request(self):
        self.fake_spot()

        def fleet(**kw):
            resp = self.fleet_ok(**kw)
            self.add_request("sir-launch01", self.launched)
            return resp

        self.aws.overrides[("ec2", "create_fleet")] = fleet

    def test_unarmed_instance_has_its_request_cancelled_then_fleet_deleted(self):
        self.launch_with_request()

        def broken(**kw):
            raise client_error("ValidationException")

        self.aws.overrides[("scheduler", "create_schedule")] = broken
        self.check_orphan_terminated()
        self.assertEqual(self.events, [("cancel", ["sir-launch01"]), ("delete", ["fleet-1"])])

    def test_empty_fleet_is_deleted(self):
        self.fake_spot()
        self.aws.overrides[("ec2", "create_fleet")] = lambda **kw: {
            "FleetId": "fleet-empty", "Instances": [], "Errors": [
                {"ErrorCode": "InsufficientInstanceCapacity", "ErrorMessage": "x"}]}
        with self.assertRaises(G.LaunchError):
            G.launch_attempt(self.ctx(), 1)
        self.assertEqual(self.events, [("delete", ["fleet-empty"])])

    def test_only_capacity_errors_are_retried(self):
        tries = []

        def fake(ctx, attempt, msgs):
            tries.append(attempt)
            raise G.LaunchError(msgs[min(len(tries), len(msgs)) - 1])

        with mock.patch.object(G, "launch_attempt", lambda c, a: fake(c, a, ["no spot capacity x", "missing permission"])):
            with self.assertRaises(G.LaunchError):
                G.launch_with_retry(None, 1, tries=5, wait=0)
        self.assertEqual(len(tries), 2)
        tries.clear()
        with mock.patch.object(G, "launch_attempt", lambda c, a: fake(c, a, ["no spot capacity x"])):
            with self.assertRaises(G.LaunchError):
                G.launch_with_retry(None, 1, tries=3, wait=0)
        self.assertEqual(len(tries), 3)


class RunCommands(MotoCase, SpotFakes):
    """F3 (--resume with an unrecorded instance), F9, A6, A19 through the CLI entry points."""

    def setUp(self):
        super().setUp()
        self.s3.create_bucket(Bucket=BUCKET)
        self.m = {"runId": RUN_ID, "region": "us-east-1", "maxAttempts": 5}
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/manifest.json", Body=json.dumps(self.m).encode())

    def main(self, *argv):
        return G.main(list(argv))

    def test_main_reap_exits_1_on_a_release_problem(self):
        self.patch_aws()
        with mock.patch.object(G, "reap", return_value=["us-east-1: fleet(s) f kept"]):
            self.assertEqual(self.main("reap"), 1)
        with mock.patch.object(G, "reap", return_value=[]):
            self.assertEqual(self.main("reap"), 0)

    def patch_aws(self):
        patch = mock.patch.object(G, "Aws", lambda profile=None: self.aws)
        patch.start()
        self.addCleanup(patch.stop)

    def end_of_run(self, reap_problems=(), cancel_fails=False):
        """cmd_run --resume of a run whose watch ends `done`; returns (exit code, events)."""
        self.fake_spot()
        self.patch_aws()
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": RUN_ID})
        self.ec2.terminate_instances(InstanceIds=[iid])
        self.add_request("sir-end00001", iid)
        self.add_fleet("fleet-end")
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/launches.json", Body=json.dumps({
            "runId": RUN_ID, "attempts": [{"n": 1, "instanceId": iid, "fleetId": "fleet-end",
                                           "expires": "2099-01-01T00:00:00Z"}]}).encode())
        if cancel_fails:
            def broken(**kw):
                raise client_error("UnauthorizedOperation")

            self.aws.overrides[("ec2", "cancel_spot_instance_requests")] = broken
        a = argparse.Namespace(profile=None, dry_run=False, resume=RUN_ID, detach=False)
        dest = tempfile.TemporaryDirectory()
        self.addCleanup(dest.cleanup)
        with mock.patch.object(G, "watch", return_value="done"), \
                mock.patch.object(G, "pull", return_value=Path(dest.name)), \
                mock.patch.object(G, "wait_terminated", return_value=True), \
                mock.patch.object(G, "record_cost", return_value={"totalUsd": 0.0}), \
                mock.patch.object(G, "reap", return_value=list(reap_problems)):
            return G.cmd_run(a, CFG)

    def test_run_end_releases_the_run_and_exits_0(self):
        self.assertEqual(self.end_of_run(), 0)
        self.assertEqual(self.events, [("cancel", ["sir-end00001"]), ("delete", ["fleet-end"])])

    def test_run_end_exits_1_when_the_release_failed(self):
        self.assertEqual(self.end_of_run(cancel_fails=True), 1)
        self.assertEqual(self.deleted, [])

    def test_run_end_exits_1_when_reap_reports_a_problem(self):
        self.assertEqual(self.end_of_run(reap_problems=["us-east-1: not checked"]), 1)

    def test_relaunch_cancels_the_ended_attempts_request_before_launching(self):
        self.fake_spot()
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": RUN_ID})
        self.ec2.terminate_instances(InstanceIds=[iid])
        self.add_request("sir-relaunch", iid)
        self.add_fleet("fleet-1")
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/launches.json", Body=json.dumps({
            "runId": RUN_ID, "attempts": [{"n": 1, "instanceId": iid, "fleetId": "fleet-1",
                                           "expires": "2099-01-01T00:00:00Z"}]}).encode())
        ctx = G.Ctx(self.aws, CFG, self.m, BUCKET)
        seen = []

        def launch(c, n):
            seen.append((n, list(self.events)))
            raise G.LaunchError("stop here")

        with mock.patch.object(G, "relaunch_decision", return_value=("relaunch", "interrupted")), \
                mock.patch.object(G, "launch_with_retry", launch):
            self.assertEqual(G.watch(ctx), "relaunch failed: stop here")
        self.assertEqual(seen, [(2, [("cancel", ["sir-relaunch"]), ("delete", ["fleet-1"])])])

    def test_resume_refuses_unrecorded_live_instance(self):
        iid = self.run_instance({"gmesh-train": "1", "gmesh-run": RUN_ID, "gmesh-expires": "2099-01-01T00:00:00Z"})
        self.run_instance({"gmesh-train": "1", "gmesh-run": "other", "gmesh-expires": "2099-01-01T00:00:00Z"})
        with mock.patch.object(G, "launch_with_retry", side_effect=AssertionError("launched")):
            with self.assertRaises(SystemExit) as cm:
                self.main("run", "--resume", RUN_ID)
        self.assertIn(iid, str(cm.exception))
        ctx = G.Ctx(self.aws, CFG, self.m, BUCKET)
        self.assertEqual(G.unrecorded_instances(ctx), [iid])
        ctx.launches["attempts"].append({"n": 1, "instanceId": iid})
        self.assertEqual(G.unrecorded_instances(ctx), [])

    def test_interrupt_without_launch_exits_with_message(self):
        with self.assertRaises(SystemExit) as cm:
            self.main("interrupt", RUN_ID, "--terminate")
        self.assertIn("no launched attempt", str(cm.exception))

    def test_run_writes_cost_json_into_the_pulled_run_dir(self):
        rec = {"n": 1, "instanceId": "i-1", "launchTime": G.iso(T0), "expires": G.iso(T0 + timedelta(hours=1))}
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/launches.json",
                           Body=json.dumps({"runId": RUN_ID, "attempts": [rec]}).encode())
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/out/s/r.json", Body=b"{}")
        cost = {"totalUsd": 1.25, "hours": 0.5}
        args = argparse.Namespace(dry_run=False, profile=None, resume=RUN_ID, detach=False)
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.object(G, "watch", return_value="done"), \
                mock.patch.object(G, "wait_terminated", return_value=True), \
                mock.patch.object(G, "record_cost", return_value=cost), \
                mock.patch.object(G, "reap", return_value=[]):
            self.assertEqual(G.cmd_run(args, {**CFG, "local_runs": d}), 0)
            dest = Path(d) / RUN_ID
            self.assertEqual(json.loads((dest / "cost.json").read_text()), cost)
            self.assertTrue((dest / "out" / "s" / "r.json").exists())

    def test_attempt_record_falls_back_to_boot_record(self):
        ctx = G.Ctx(self.aws, CFG, self.m, BUCKET)
        self.assertIsNone(G.attempt_record(ctx, 1))
        boot = {"n": 1, "state": "failed", "reason": "boot-failed", "exit": 7}
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/status/attempt-1.boot.json", Body=json.dumps(boot).encode())
        self.assertEqual(G.attempt_record(ctx, 1), boot)
        st = {"attempts": [{"n": 1, "state": "done"}]}
        self.s3.put_object(Bucket=BUCKET, Key=f"runs/{RUN_ID}/status.json", Body=json.dumps(st).encode())
        self.assertEqual(G.attempt_record(ctx, 1), {"n": 1, "state": "done"})

    def test_pull_copies_runs_only_and_status_lists_ckpt(self):
        for key in (f"runs/{RUN_ID}/status.json", f"runs/{RUN_ID}/out/s/r.json",
                    f"ckpt/{RUN_ID}/s/checkpoint.pt", f"runs/{RUN_ID}x/other.json"):
            self.s3.put_object(Bucket=BUCKET, Key=key, Body=b"{}")
        with tempfile.TemporaryDirectory() as d:
            cfg = {**CFG, "local_runs": d}
            dest = G.pull(self.aws, cfg, BUCKET, RUN_ID)
            got = sorted(p.relative_to(dest).as_posix() for p in dest.rglob("*") if p.is_file())
        self.assertEqual(got, ["manifest.json", "out/s/r.json", "status.json"])
        G.cmd_status(argparse.Namespace(profile=None, run_id=RUN_ID), CFG)
        self.assertIn(f"ckpt/{RUN_ID}/s/checkpoint.pt", self.stdout.getvalue())


class JobLifetime(unittest.TestCase):
    """max_minutes_scope = "job": remaining_minutes, attempt_minutes, load_job."""

    NOW = h(5)

    def att(self, start, end=None, expires=None):
        r = {"launchTime": G.iso(start), "expires": G.iso(expires or start + timedelta(minutes=500))}
        if end is not None:
            r["endObserved"] = G.iso(end)
        return r

    def test_remaining_is_the_max_minus_each_attempts_launch_to_end_span(self):
        done = self.att(h(0), end=h(0) + timedelta(minutes=45))
        self.assertEqual(G.remaining_minutes(240, [done], self.NOW), 195)
        second = self.att(h(2), end=h(2) + timedelta(minutes=30))
        self.assertEqual(G.remaining_minutes(240, [done, second], self.NOW), 165)
        self.assertEqual(G.remaining_minutes(240, [], self.NOW), 240)

    def test_an_attempt_without_an_end_counts_to_its_expiry_or_to_now_whichever_is_first(self):
        gone = self.att(h(0), expires=h(0) + timedelta(minutes=60))
        self.assertEqual(G.remaining_minutes(240, [gone], self.NOW), 180)
        live = self.att(h(4), expires=h(9))
        self.assertEqual(G.remaining_minutes(240, [live], self.NOW), 240 - 60)

    def test_the_scope_decides_the_next_attempts_lifetime(self):
        used = [self.att(h(0), end=h(0) + timedelta(minutes=90))]
        for job in ({}, {"max_minutes_scope": "attempt"}):
            self.assertEqual(G.attempt_minutes(job, 240, used, self.NOW), 240)
        self.assertEqual(G.attempt_minutes({"max_minutes_scope": "job"}, 240, used, self.NOW), 150)
        self.assertEqual(G.attempt_minutes({"max_minutes_scope": "job"}, 240, [], self.NOW), 240)

    def test_the_remainder_is_truncated_to_whole_minutes(self):
        used = [self.att(h(0), end=h(0) + timedelta(minutes=89, seconds=30))]
        self.assertEqual(G.attempt_minutes({"max_minutes_scope": "job"}, 240, used, self.NOW), 150)

    def test_under_ten_minutes_left_is_a_launch_error_and_ten_is_allowed(self):
        job = {"max_minutes_scope": "job"}
        ten = [self.att(h(0), end=h(0) + timedelta(minutes=230))]
        self.assertEqual(G.attempt_minutes(job, 240, ten, self.NOW), 10)
        nine = [self.att(h(0), end=h(0) + timedelta(minutes=231))]
        with self.assertRaises(G.LaunchError) as cm:
            G.attempt_minutes(job, 240, nine, self.NOW)
        self.assertIn("job lifetime used", str(cm.exception))
        self.assertEqual(G.MIN_ATTEMPT_MINUTES, 10)

    def test_load_job_accepts_attempt_and_job_and_refuses_another_scope(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "j.toml"
            body = ('name = "j"\nrequirements = "r.txt"\nmax_minutes = 60\nmax_minutes_scope = "%s"\n'
                    '[[steps]]\nname = "s"\nentry = "e.py"\n')
            for scope in ("attempt", "job"):
                p.write_text(body % scope)
                self.assertEqual(G.load_job(p)["max_minutes_scope"], scope)
            p.write_text(body % "weekly")
            with self.assertRaises(SystemExit):
                G.load_job(p)
            p.write_text(body.replace('max_minutes_scope = "%s"\n', ""))
            self.assertNotIn("max_minutes_scope", G.load_job(p))

    def test_the_committed_gm730_jobs_are_job_scoped(self):
        jobs = sorted((HERE / "jobs").glob("gm730-*.toml"))
        self.assertTrue(jobs)
        for p in jobs:
            self.assertEqual(G.load_job(p)["max_minutes_scope"], "job", p.name)


class RunnerSmoke(unittest.TestCase):
    """The smoke imports sentence-transformers only when the job's lock pins it."""

    def setUp(self):
        import runner as R
        self.R = R
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.work = Path(self.tmp.name)

    def runner(self, lock=None, steps=({"device": "cpu"},), smoke=True):
        r = self.R.Runner.__new__(self.R.Runner)
        r.work, r.python = self.work, "py"
        r.manifest = {"smoke": smoke, "steps": list(steps)}
        r.job_env = lambda: {}
        r.calls = []
        r.sh = lambda cmd, reason, env=None: r.calls.append((cmd, reason))
        if lock is not None:
            (self.work / "requirements.txt").write_text(lock)
        return r

    def test_lock_has_matches_a_pin_at_line_start_case_insensitively(self):
        self.assertTrue(self.runner("torch==2.2\nsentence-transformers==3.0.1\n").lock_has("sentence-transformers"))
        self.assertTrue(self.runner("Sentence-Transformers==3.0.1 \\\n").lock_has("sentence-transformers"))
        self.assertFalse(self.runner("torch==2.2\ntransformers==4.48\n").lock_has("sentence-transformers"))
        self.assertFalse(self.runner("# sentence-transformers==3\n x sentence-transformers==3\n").lock_has("sentence-transformers"))
        self.assertFalse(self.runner("sentence-transformers-extra==1\n").lock_has("sentence-transformers"))

    def test_without_a_lock_file_it_is_strict(self):
        self.assertTrue(self.runner().lock_has("sentence-transformers"))

    def test_smoke_passes_the_flag_iff_the_lock_pins_it(self):
        r = self.runner("torch==2\n")
        r.smoke()
        cmd, reason = r.calls[0]
        self.assertEqual((cmd, reason), (["py", "-c", self.R.SMOKE], "smoke-failed"))
        r = self.runner("sentence-transformers==3\n")
        r.smoke()
        self.assertEqual(r.calls[0][0][3:], ["--sentence-transformers"])
        r = self.runner("sentence-transformers==3\n", steps=({"device": "cuda"},))
        r.smoke()
        self.assertEqual(r.calls[0][0][3:], ["--cuda", "--sentence-transformers"])
        r = self.runner("torch==2\n", steps=({"device": "cuda"},))
        r.smoke()
        self.assertEqual(r.calls[0][0][3:], ["--cuda"])

    def test_smoke_is_skipped_when_the_manifest_turns_it_off(self):
        r = self.runner("torch==2\n", smoke=False)
        r.smoke()
        self.assertEqual(r.calls, [])

    @unittest.skipUnless(importlib.util.find_spec("torch") and importlib.util.find_spec("transformers"),
                         "torch and transformers are not installed")
    def test_the_smoke_script_imports_sentence_transformers_only_with_the_flag(self):
        (self.work / "sentence_transformers.py").write_text('raise ImportError("sentence-transformers is not installed")\n')
        env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
        env["PYTHONPATH"] = str(self.work)

        def run(*flags):
            return subprocess.run([sys.executable, "-c", self.R.SMOKE, *flags], env=env, capture_output=True, text=True,
                                  cwd=self.work, timeout=300)

        ok = run()
        self.assertEqual(ok.returncode, 0, ok.stderr[-2000:])
        self.assertIn("sentence-transformers not in the lock", ok.stdout)
        self.assertIn("smoke: forward ok on cpu", ok.stdout)
        bad = run("--sentence-transformers")
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("sentence-transformers is not installed", bad.stderr)


if __name__ == "__main__":
    unittest.main()
