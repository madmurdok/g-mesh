# /// script
# requires-python = ">=3.11"
# dependencies = ["boto3==1.44.6"]
# ///
"""Launch a training job on an AWS spot GPU instance and see it through.

    uv run eval/gpu/gpu_train.py setup [--with-fis] [--ssm] [--check]
    uv run eval/gpu/gpu_train.py run eval/gpu/jobs/<job>.toml [--set k=v] [--region R]
        [--instance-types t1,t2] [--tag T] [--detach] [--dry-run]
    uv run eval/gpu/gpu_train.py run --resume <run-id>
    uv run eval/gpu/gpu_train.py status|pull <run-id>
    uv run eval/gpu/gpu_train.py reap
    uv run eval/gpu/gpu_train.py verify-clean
    uv run eval/gpu/gpu_train.py interrupt <run-id> --fis|--terminate
    uv run eval/gpu/gpu_train.py kill-switch-test [<run-id>]

`run` uploads the code (git archive of a clean HEAD), the env lock and the
job's data subsets (content-addressed), launches attempt 1 with a one-time
instant spot fleet, arms the cloud-side terminate schedule, then watches:
an interrupted attempt, or one whose instance vanished without a status, is
relaunched with the same run id until `max_attempts`; a done or failed run is
pulled to `local_runs/<run-id>/`, its cost is written to cost.json, and the
instances are checked terminated. Design: docs/design/GM-457-spot-gpu-training.md.

The pure functions (run_id_of, user_data, fleet_request, schedule_request,
relaunch_decision, cost_of, clean_problems, live_spot_requests, classify_launch_error,
data_manifest) take no AWS client; everything that calls AWS takes one.
"""

import argparse
import base64
import hashlib
import json
import os
import re
import string
import subprocess
import sys
import time
import tomllib
from datetime import datetime, timedelta, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
TAG = "gmesh-train"
LIVE_STATES = ["pending", "running", "stopping", "stopped"]
TERMINATE_TARGET = "arn:aws:scheduler:::aws-sdk:ec2:terminateInstances"
SSM_POLICY = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
WATCH_EVERY = 30


# --- pure: config, jobs, ids ---------------------------------------------------------

def utcnow():
    return datetime.now(timezone.utc)


def iso(dt):
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def parse_iso(s):
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


def sha256_bytes(b):
    return hashlib.sha256(b).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_config(path=HERE / "config.toml"):
    with open(path, "rb") as f:
        return tomllib.load(f)


def parse_set(items):
    """--set k=v pairs; v is read as a TOML value when it parses, else as a string."""
    out = {}
    for item in items or ():
        if "=" not in item:
            raise SystemExit(f"--set {item!r}: expected key=value")
        k, v = item.split("=", 1)
        try:
            out[k.strip()] = tomllib.loads(f"v = {v}")["v"]
        except tomllib.TOMLDecodeError:
            out[k.strip()] = v
    return out


def load_job(path, sets=None):
    """A job spec with defaults filled, --set applied (a top-level key, else a [vars]
    key) and {var} placeholders in step args expanded."""
    with open(path, "rb") as f:
        job = tomllib.load(f)
    job.setdefault("vars", {})
    for k, v in (sets or {}).items():
        if k in job and k not in ("steps", "data", "vars"):
            job[k] = v
        elif k in job["vars"]:
            job["vars"][k] = str(v)
        else:
            raise SystemExit(f"--set {k}: no such job key or [vars] entry in {path}")
    if "max_minutes" not in job:
        job["max_minutes"] = int(round(float(job.get("max_hours", 4)) * 60))
    job.pop("max_hours", None)
    job.setdefault("checkpoint_minutes", 10)
    job.setdefault("grace_seconds", 60)
    job.setdefault("smoke", True)
    job.setdefault("disable_os_lifetime", False)
    job.setdefault("data", [])
    if not job.get("steps"):
        raise SystemExit(f"{path}: no [[steps]]")
    names = [s["name"] for s in job["steps"]]
    if len(set(names)) != len(names):
        raise SystemExit(f"{path}: step names must be unique")
    envs = [d["env"] for d in job["data"]]
    if len(set(envs)) != len(envs):
        raise SystemExit(f"{path}: two [[data]] entries set the same env variable")
    for s in job["steps"]:
        s["args"] = [a.format(**job["vars"]) for a in s.get("args", [])]
    return job


def config_sha(job):
    return sha256_bytes(canonical({k: v for k, v in job.items() if k != "description"}).encode())


def run_id_of(job, now, tag=None):
    """<job>-<config sha12>-<utc yyyymmddThhmm>[-<tag>]: a schedule name and tag value."""
    rid = f"{job['name']}-{config_sha(job)[:12]}-{now.astimezone(timezone.utc):%Y%m%dT%H%M}"
    if tag:
        rid += f"-{tag}"
    if len(rid) > 54 or not all(c.isalnum() or c in "-_." for c in rid):
        raise SystemExit(f"run id {rid!r}: over 54 characters or not [A-Za-z0-9._-]"
                         " (schedule names gmesh-<run-id>-a<n> allow 64)")
    return rid


def ckpt_prefix(run_id):
    """Checkpoints live outside runs/ so the bucket lifecycle can expire only them."""
    return f"ckpt/{run_id}/"


def bucket_name(cfg, account):
    return cfg["bucket"].format(account=account)


def env_key(lock_bytes):
    return f"env/{sha256_bytes(lock_bytes)}/requirements.txt"


def data_manifest(root, include):
    """{"files": [{"path", "size", "sha256"}]} for the files under root matching the
    include globs (`**` recurses), sorted by path. Fails on a glob that matches nothing."""
    root = Path(os.path.expanduser(root))
    files = {}
    for pattern in include:
        # A trailing `**` matches directories only before Python 3.13; `**/*` matches
        # every file below on all versions.
        glob = pattern.rstrip("/")
        if glob.split("/")[-1] == "**":
            glob += "/*"
        hits = [p for p in root.glob(glob) if p.is_file()]
        if not hits:
            raise SystemExit(f"data: {root}/{pattern} matches no file")
        for p in hits:
            files[p.relative_to(root).as_posix()] = p
    return {"files": [{"path": rel, "size": p.stat().st_size, "sha256": sha256_file(p)}
                      for rel, p in sorted(files.items())]}


def data_sha(manifest):
    return sha256_bytes(canonical(manifest["files"]).encode())


def render_iam(name, **subs):
    return json.loads(string.Template((HERE / "iam" / f"{name}.json").read_text()).substitute(subs))


# --- pure: launch requests ---------------------------------------------------------------

def user_data(*, bucket, bucket_region, run_id, attempt, code_key, max_minutes, uv_version,
              grace, disable_os_lifetime=False, shutdown="shutdown -h now", template=None):
    """The attempt's user data script. Its first command is the in-OS lifetime
    (`shutdown -h +max_minutes`), before any download; `trap finish EXIT` powers off."""
    text = template if template is not None else (HERE / "user_data.sh.tmpl").read_text()
    lifetime = ("# in-OS lifetime disabled for this attempt (scheduler-only scenario)"
                if disable_os_lifetime else f"shutdown -h +{int(max_minutes)}")
    subs = {"lifetime": lifetime, "bucket": bucket, "bucket_region": bucket_region, "run_id": run_id,
            "attempt": str(attempt), "code_key": code_key, "uv_version": uv_version,
            "grace": str(int(grace)), "shutdown": shutdown}
    for k, v in subs.items():
        text = text.replace(f"@@{k}@@", v)
    if "@@" in text:
        raise ValueError("user data: unfilled placeholder " + text[text.index("@@"):][:40])
    if len(text.encode()) > 16 * 1024:
        raise ValueError("user data exceeds 16 KB")
    return text


def instance_tags(run_id, attempt, expires):
    return [{"Key": TAG, "Value": "1"}, {"Key": "gmesh-run", "Value": run_id},
            {"Key": "gmesh-attempt", "Value": str(attempt)}, {"Key": "gmesh-expires", "Value": iso(expires)}]


def template_name(run_id, attempt):
    return f"gmesh-{run_id}-a{attempt}"


def fleet_request(*, run_id, attempt, expires, ami_id, root_device, root_gb, profile, sg_id,
                  user_data_text, subnets_by_type):
    """{"LaunchTemplate": create_launch_template kwargs, "CreateFleet": create_fleet kwargs}.
    One-time instant spot fleet of one instance over every allowed type x subnet."""
    tags = instance_tags(run_id, attempt, expires)
    name = template_name(run_id, attempt)
    lt = {
        "LaunchTemplateName": name,
        "LaunchTemplateData": {
            "ImageId": ami_id,
            "IamInstanceProfile": {"Name": profile},
            "SecurityGroupIds": [sg_id],
            "InstanceInitiatedShutdownBehavior": "terminate",
            "MetadataOptions": {"HttpTokens": "required", "HttpPutResponseHopLimit": 1, "HttpEndpoint": "enabled"},
            "BlockDeviceMappings": [{"DeviceName": root_device, "Ebs": {
                "VolumeSize": int(root_gb), "VolumeType": "gp3", "DeleteOnTermination": True}}],
            "UserData": base64.b64encode(user_data_text.encode()).decode(),
            "TagSpecifications": [{"ResourceType": "instance", "Tags": tags},
                                  {"ResourceType": "volume", "Tags": tags}],
        },
        "TagSpecifications": [{"ResourceType": "launch-template", "Tags": tags}],
    }
    overrides = [{"InstanceType": t, "SubnetId": s} for t, subnets in subnets_by_type.items() for s in subnets]
    if not overrides:
        raise ValueError("fleet: no instance type is offered in any default subnet")
    fleet = {
        "Type": "instant",
        "TargetCapacitySpecification": {"TotalTargetCapacity": 1, "DefaultTargetCapacityType": "spot"},
        "SpotOptions": {"AllocationStrategy": "price-capacity-optimized", "InstanceInterruptionBehavior": "terminate"},
        "LaunchTemplateConfigs": [{"LaunchTemplateSpecification": {"LaunchTemplateName": name, "Version": "$Latest"},
                                   "Overrides": overrides}],
        "TagSpecifications": [{"ResourceType": "fleet", "Tags": tags}],
    }
    return {"LaunchTemplate": lt, "CreateFleet": fleet}


def schedule_name(run_id, attempt):
    return f"gmesh-{run_id}-a{attempt}"


def schedule_request(*, run_id, attempt, instance_id, at, role_arn):
    """EventBridge Scheduler one-time schedule that terminates the instance at `at`."""
    return {
        "Name": schedule_name(run_id, attempt),
        "ScheduleExpression": f"at({at.astimezone(timezone.utc):%Y-%m-%dT%H:%M:%S})",
        "ScheduleExpressionTimezone": "UTC",
        "FlexibleTimeWindow": {"Mode": "OFF"},
        "ActionAfterCompletion": "DELETE",
        "Description": f"lifetime cap of {run_id} attempt {attempt}",
        "Target": {"Arn": TERMINATE_TARGET, "RoleArn": role_arn,
                   "Input": json.dumps({"InstanceIds": [instance_id]}),
                   "RetryPolicy": {"MaximumRetryAttempts": 10, "MaximumEventAgeInSeconds": 3600}},
    }


# --- pure: decisions, cost, checks --------------------------------------------------------

def relaunch_decision(record, attempt, max_attempts, instance_state, expired):
    """("wait" | "relaunch" | "stop", reason) for the current attempt.

    record: this attempt's entry from status.json (or a boot-failure record), None if
    the runner wrote nothing. instance_state: the EC2 state, None when the instance is
    no longer listed. expired: the attempt's lifetime has passed. A relaunch waits for
    the instance to be gone, so two attempts never hold the vCPU quota at once."""
    state = (record or {}).get("state")
    if state == "done":
        return "stop", "done"
    if state == "failed":
        return "stop", f"failed: {(record or {}).get('reason', '?')}"
    gone = instance_state in (None, "terminated")
    if state == "interrupted":
        reason = record.get("reason", "interrupted")
    elif gone or instance_state == "shutting-down":
        if expired:
            return "stop", "expired"
        reason = "no-status"
    else:
        return "wait", instance_state or "unknown"
    if not gone:
        return "wait", f"{reason}, instance {instance_state}"
    if attempt >= max_attempts:
        return "stop", f"attempt limit {max_attempts} reached ({reason})"
    return "relaunch", reason


def cost_of(intervals, prices):
    """Dollars for running over `intervals` [(start, end)] at piecewise-constant
    `prices` [(effective_from, $/h)]: each price holds until the next one starts;
    time before the first price is billed at the first price."""
    if not prices:
        raise ValueError("cost_of: no prices")
    pts = sorted(prices)
    total = 0.0
    for start, end in intervals:
        if end <= start:
            continue
        for i, (t0, p) in enumerate(pts):
            seg_start = start if i == 0 else max(start, t0)
            seg_end = min(end, pts[i + 1][0]) if i + 1 < len(pts) else end
            if seg_end > seg_start:
                total += p * (seg_end - seg_start).total_seconds() / 3600
    return total


GPU_FAMILY = re.compile(r"^(g|gr|p|dl|trn|inf|vt)\d")


def is_gpu(instance_type):
    """The kill-switch Lambda's definition of a GPU-family instance."""
    return bool(GPU_FAMILY.match(instance_type))


def check_instance_types(types, configured):
    """The fleet's types: the configured ones, or an explicit override of G-family types
    of one family (one GPU model per run id, e.g. g6.xlarge alone)."""
    types = [t.strip() for t in types if t.strip()]
    if not types:
        raise SystemExit("instance types: none given")
    if set(types) <= set(configured):
        return types
    bad = [t for t in types if not re.fullmatch(r"g\d+[a-z]*\.[0-9a-z]+", t)]
    if bad:
        raise SystemExit(f"instance types {bad}: only G-family types (spot quota is G/VT only)")
    families = sorted({t.split(".")[0] for t in types})
    if len(families) > 1:
        raise SystemExit(f"instance types {types}: one GPU family per run, got {families}")
    return types


def classify_launch_error(code, message):
    msg = message or ""
    if code in ("UnauthorizedOperation", "AccessDenied", "AccessDeniedException") and "explicit deny" in msg.lower():
        return f"budget kill switch is active (deny-all-EC2 policy attached): {msg}"
    if code in ("UnauthorizedOperation", "AccessDenied", "AccessDeniedException"):
        return f"missing permission: {msg}"
    if code in ("MaxSpotInstanceCountExceeded", "VcpuLimitExceeded") or "vcpu" in msg.lower():
        return f"spot vCPU quota exceeded (8 vCPU of G/VT per region): {msg}"
    if code in ("InsufficientInstanceCapacity", "UnfulfillableCapacity") or "capacity" in msg.lower():
        return f"no spot capacity for the allowed types: {msg}"
    return f"{code}: {msg}"


def clean_problems(snapshot):
    """What verify-clean reports, from {region: {"tagged", "gpu", "spot", "volumes",
    "schedules", "templates", "fleets"}} lists of described resources."""
    out = []
    for region, s in sorted(snapshot.items()):
        for i in s.get("tagged", []):
            out.append(f"{region}: tagged instance {i['InstanceId']} is {i['State']['Name']}")
        for i in s.get("gpu", []):
            out.append(f"{region}: GPU instance {i['InstanceId']} ({i['InstanceType']}) is {i['State']['Name']}")
        for r in s.get("spot", []):
            out.append(f"{region}: spot request {r['SpotInstanceRequestId']} is {r['State']}")
        for v in s.get("volumes", []):
            out.append(f"{region}: volume {v['VolumeId']} is {v['State']}")
        for sc in s.get("schedules", []):
            out.append(f"{region}: schedule {sc['Name']} still exists")
        for t in s.get("templates", []):
            out.append(f"{region}: launch template {t['LaunchTemplateName']} still exists")
        for f in s.get("fleets", []):
            out.append(f"{region}: fleet {f['FleetId']} is {f['FleetState']}")
    return out


def live_spot_requests(requests, live_ids, ours=frozenset()):
    """The spot requests verify-clean reports: an open request without an instance, one
    whose instance is in `live_ids`, or one whose instance is in `ours` (a gmesh-train
    instance or a gmesh fleet's instance, whatever its state). A request of some other
    ended instance stays active for minutes after the instance ends and is not ours; a
    gmesh request still active after its instance ended is a leak, since the launcher
    cancels it: an active request holds the spot vCPU quota the next job needs."""
    return [r for r in requests
            if not r.get("InstanceId") or r["InstanceId"] in live_ids or r["InstanceId"] in ours]


def fleet_instance_ids(fleet):
    """Instance ids an instant fleet launched (DescribeFleets lists them for type instant)."""
    return {iid for i in fleet.get("Instances", []) for iid in i.get("InstanceIds", [])}


def reapable_fleets(fleets, live_ids, ended_ids, now, min_age=timedelta(minutes=10)):
    """The gmesh fleets reap may delete: none of their instances is in `live_ids`, and
    either every instance is in `ended_ids` (at any age) or the fleet is older than
    `min_age`. A young fleet with no instance, or with one listed neither live nor
    ended, may belong to another launcher process mid-launch and is never touched."""
    out = []
    for f in fleets:
        iids = fleet_instance_ids(f)
        if iids & live_ids:
            continue
        if (iids and iids <= ended_ids) or now - f["CreateTime"] > min_age:
            out.append(f)
    return out


def expired_instances(instances, now):
    """Ids of tagged instances whose gmesh-expires is in the past (no tag counts as expired)."""
    out = []
    for i in instances:
        tags = {t["Key"]: t["Value"] for t in i.get("Tags", [])}
        if tags.get(TAG) != "1":
            continue
        exp = tags.get("gmesh-expires")
        if exp is None or parse_iso(exp) <= now:
            out.append(i["InstanceId"])
    return out


def termination_time(instance):
    """The time in StateTransitionReason ("User initiated (2026-11-19 12:00:00 GMT)"), or None."""
    reason = instance.get("StateTransitionReason", "")
    if "(" in reason and reason.endswith("GMT)"):
        try:
            t = datetime.strptime(reason[reason.rindex("(") + 1:-1], "%Y-%m-%d %H:%M:%S GMT")
            return t.replace(tzinfo=timezone.utc)
        except ValueError:
            return None
    return None


# --- AWS plumbing -------------------------------------------------------------------------

class Aws:
    def __init__(self, profile=None):
        import boto3
        self.session = boto3.Session(profile_name=profile or os.environ.get("AWS_PROFILE", "default"))
        self._clients = {}

    def client(self, name, region):
        key = (name, region)
        if key not in self._clients:
            self._clients[key] = self.session.client(name, region_name=region)
        return self._clients[key]

    def account(self):
        return self.client("sts", "us-east-1").get_caller_identity()["Account"]


def client_error():
    from botocore.exceptions import ClientError
    return ClientError


def err_code(e):
    return e.response.get("Error", {}).get("Code", "")


def s3_exists(s3, bucket, key):
    try:
        s3.head_object(Bucket=bucket, Key=key)
        return True
    except client_error() as e:
        if err_code(e) in ("404", "NoSuchKey", "NotFound"):
            return False
        raise


def s3_get_json(s3, bucket, key):
    try:
        return json.loads(s3.get_object(Bucket=bucket, Key=key)["Body"].read())
    except client_error() as e:
        if err_code(e) in ("404", "NoSuchKey", "NotFound"):
            return None
        raise


def s3_put_json(s3, bucket, key, obj):
    s3.put_object(Bucket=bucket, Key=key, Body=(json.dumps(obj, indent=1) + "\n").encode(),
                  ContentType="application/json")


# --- setup ----------------------------------------------------------------------------------

def lifecycle_rules():
    return [
        {"ID": "abort-incomplete-mpu", "Status": "Enabled", "Filter": {"Prefix": ""},
         "AbortIncompleteMultipartUpload": {"DaysAfterInitiation": 1}},
        {"ID": "expire-ckpt", "Status": "Enabled", "Filter": {"Prefix": "ckpt/"},
         "Expiration": {"Days": 30}},
    ]


def ensure_bucket(s3, bucket, region, apply):
    changes = []
    try:
        s3.head_bucket(Bucket=bucket)
    except client_error() as e:
        if err_code(e) not in ("404", "NoSuchBucket", "NotFound"):
            raise
        changes.append(f"create bucket {bucket}")
        if apply:
            kw = {} if region == "us-east-1" else {"CreateBucketConfiguration": {"LocationConstraint": region}}
            s3.create_bucket(Bucket=bucket, **kw)
        else:
            return changes + [f"set public access block, SSE-S3 and lifecycle on {bucket}"]
    pab = {"BlockPublicAcls": True, "IgnorePublicAcls": True, "BlockPublicPolicy": True, "RestrictPublicBuckets": True}
    try:
        cur = s3.get_public_access_block(Bucket=bucket)["PublicAccessBlockConfiguration"]
    except client_error():
        cur = None
    if cur != pab:
        changes.append(f"block public access on {bucket}")
        if apply:
            s3.put_public_access_block(Bucket=bucket, PublicAccessBlockConfiguration=pab)
    sse = {"Rules": [{"ApplyServerSideEncryptionByDefault": {"SSEAlgorithm": "AES256"}}]}
    try:
        cur = s3.get_bucket_encryption(Bucket=bucket)["ServerSideEncryptionConfiguration"]
        cur_alg = [r["ApplyServerSideEncryptionByDefault"]["SSEAlgorithm"] for r in cur["Rules"]]
    except client_error():
        cur_alg = []
    if cur_alg != ["AES256"]:
        changes.append(f"SSE-S3 default encryption on {bucket}")
        if apply:
            s3.put_bucket_encryption(Bucket=bucket, ServerSideEncryptionConfiguration=sse)
    changes += ensure_lifecycle(s3, bucket, apply)
    return changes


def ensure_lifecycle(s3, bucket, apply):
    want = lifecycle_rules()
    try:
        cur = s3.get_bucket_lifecycle_configuration(Bucket=bucket)["Rules"]
    except client_error():
        cur = []
    if canonical(sorted(cur, key=lambda r: r["ID"])) == canonical(sorted(want, key=lambda r: r["ID"])):
        return []
    if apply:
        s3.put_bucket_lifecycle_configuration(Bucket=bucket, LifecycleConfiguration={"Rules": want})
    return [f"lifecycle rules on {bucket}"]


def ensure_role(iam, name, trust, policy, apply, managed=()):
    """Get-or-create the role; the trust and the inline policy are rewritten only when
    they differ from the wanted documents."""
    changes = []
    try:
        role = iam.get_role(RoleName=name)["Role"]
    except iam.exceptions.NoSuchEntityException:
        role = None
    if role is None:
        changes.append(f"create role {name}")
        if apply:
            iam.create_role(RoleName=name, AssumeRolePolicyDocument=json.dumps(trust),
                            Tags=[{"Key": TAG, "Value": "1"}], Description="g-mesh GPU training")
    elif canonical(role["AssumeRolePolicyDocument"]) != canonical(trust):
        changes.append(f"update trust of {name}")
        if apply:
            iam.update_assume_role_policy(RoleName=name, PolicyDocument=json.dumps(trust))
    pname = f"{name}-policy"
    cur = None
    if role is not None:
        try:
            cur = iam.get_role_policy(RoleName=name, PolicyName=pname)["PolicyDocument"]
        except iam.exceptions.NoSuchEntityException:
            cur = None
    if cur is None or canonical(cur) != canonical(policy):
        changes.append(f"{'put' if cur is None else 'update'} inline policy {pname}")
        if apply:
            iam.put_role_policy(RoleName=name, PolicyName=pname, PolicyDocument=json.dumps(policy))
    attached = set()
    if role is not None:
        attached = {p["PolicyArn"] for p in iam.list_attached_role_policies(RoleName=name)["AttachedPolicies"]}
    for arn in managed:
        if arn not in attached:
            changes.append(f"attach {arn.rsplit('/', 1)[-1]} to {name}")
            if apply:
                iam.attach_role_policy(RoleName=name, PolicyArn=arn)
    return changes


def ensure_instance_profile(iam, name, apply):
    changes = []
    try:
        prof = iam.get_instance_profile(InstanceProfileName=name)["InstanceProfile"]
    except iam.exceptions.NoSuchEntityException:
        prof = None
    if prof is None:
        changes.append(f"create instance profile {name}")
        if apply:
            iam.create_instance_profile(InstanceProfileName=name, Tags=[{"Key": TAG, "Value": "1"}])
        roles = []
    else:
        roles = [r["RoleName"] for r in prof["Roles"]]
    if name not in roles:
        changes.append(f"add role {name} to instance profile {name}")
        if apply:
            iam.add_role_to_instance_profile(InstanceProfileName=name, RoleName=name)
    return changes


def default_vpc(ec2):
    vpcs = ec2.describe_vpcs(Filters=[{"Name": "isDefault", "Values": ["true"]}])["Vpcs"]
    if not vpcs:
        raise SystemExit(f"{ec2.meta.region_name}: no default VPC")
    return vpcs[0]["VpcId"]


def find_security_group(ec2, name):
    groups = ec2.describe_security_groups(Filters=[{"Name": "group-name", "Values": [name]},
                                                   {"Name": "vpc-id", "Values": [default_vpc(ec2)]}])["SecurityGroups"]
    return groups[0] if groups else None


def ensure_security_group(ec2, name, apply):
    """No ingress, the default all-egress rule; in the region's default VPC."""
    region = ec2.meta.region_name
    sg = find_security_group(ec2, name)
    if sg is None:
        if apply:
            ec2.create_security_group(GroupName=name, Description="g-mesh GPU training: egress only",
                                      VpcId=default_vpc(ec2),
                                      TagSpecifications=[{"ResourceType": "security-group",
                                                          "Tags": [{"Key": TAG, "Value": "1"}]}])
        return [f"{region}: create security group {name}"]
    changes = []
    if sg.get("IpPermissions"):
        changes.append(f"{region}: remove ingress rules of {name}")
        if apply:
            ec2.revoke_security_group_ingress(GroupId=sg["GroupId"], IpPermissions=sg["IpPermissions"])
    return changes


def check_launch_permissions(aws, cfg, region, account):
    """Dry-run launches and a policy simulation; returns problems (no change is made)."""
    problems = []
    ec2 = aws.client("ec2", region)
    sg = find_security_group(ec2, cfg["security_group"])
    kw = {"ImageId": cfg["ami"][region]["id"], "InstanceType": cfg["instance_types"][0], "MinCount": 1,
          "MaxCount": 1, "DryRun": True,
          "InstanceMarketOptions": {"MarketType": "spot", "SpotOptions": {"SpotInstanceType": "one-time"}},
          "IamInstanceProfile": {"Name": cfg["instance_role"]},
          "TagSpecifications": [{"ResourceType": "instance", "Tags": [{"Key": TAG, "Value": "1"}]}]}
    if sg:
        kw["SecurityGroupIds"] = [sg["GroupId"]]
    else:
        problems.append(f"{region}: security group {cfg['security_group']} missing (run setup)")
    try:
        ec2.run_instances(**kw)
    except client_error() as e:
        if err_code(e) != "DryRunOperation":
            problems.append(f"{region}: dry-run RunInstances: {classify_launch_error(err_code(e), str(e))}")
    try:
        user_arn = aws.client("sts", region).get_caller_identity()["Arn"]
        iam = aws.client("iam", "us-east-1")
        checks = [(["iam:PassRole"], f"arn:aws:iam::{account}:role/{cfg['instance_role']}"),
                  (["iam:PassRole"], f"arn:aws:iam::{account}:role/{cfg['scheduler_role']}"),
                  (["ec2:CreateFleet", "ec2:CreateLaunchTemplate", "ec2:DeleteLaunchTemplate",
                    "ec2:TerminateInstances", "ec2:DescribeSpotPriceHistory"], "*"),
                  (["scheduler:CreateSchedule", "scheduler:DeleteSchedule"], "*")]
        for actions, res in checks:
            r = iam.simulate_principal_policy(PolicySourceArn=user_arn, ActionNames=actions, ResourceArns=[res])
            for ev in r["EvaluationResults"]:
                if ev["EvalDecision"] != "allowed":
                    problems.append(f"{ev['EvalActionName']} on {res}: {ev['EvalDecision']}")
    except client_error() as e:
        problems.append(f"policy simulation unavailable ({err_code(e)}); permissions beyond RunInstances unchecked")
    return problems


def cmd_setup(a, cfg):
    aws = Aws(a.profile)
    account = aws.account()
    bucket = bucket_name(cfg, account)
    apply = not a.check
    iam = aws.client("iam", "us-east-1")
    changes = ensure_bucket(aws.client("s3", cfg["bucket_region"]), bucket, cfg["bucket_region"], apply)
    changes += ensure_role(iam, cfg["instance_role"], render_iam("instance-trust"),
                           render_iam("instance-policy", bucket=bucket), apply,
                           managed=[SSM_POLICY] if a.ssm else ())
    changes += ensure_instance_profile(iam, cfg["instance_role"], apply)
    changes += ensure_role(iam, cfg["scheduler_role"], render_iam("scheduler-trust", account=account),
                           render_iam("scheduler-policy", account=account), apply)
    if a.with_fis:
        changes += ensure_role(iam, cfg["fis_role"], render_iam("fis-trust", account=account),
                               render_iam("fis-policy", account=account), apply)
    for region in cfg["regions"]:
        changes += ensure_security_group(aws.client("ec2", region), cfg["security_group"], apply)
    verb = "changed" if apply else "would change"
    print("\n".join(f"{verb}: {c}" for c in changes) if changes else "no changes")
    problems = []
    for region in cfg["regions"]:
        problems += check_launch_permissions(aws, cfg, region, account)
    for p in problems:
        print(f"PROBLEM: {p}")
    return 1 if problems else 0


# --- run: inputs ---------------------------------------------------------------------------

def git_state(repo=REPO):
    sha = subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], capture_output=True, text=True,
                         check=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(repo), "status", "--porcelain", "--untracked-files=no"],
                           capture_output=True, text=True, check=True).stdout.strip()
    return sha, bool(dirty)


def upload_code(s3, bucket, repo=REPO):
    """code/<sha>.tar.gz of a clean HEAD, uploaded once; a dirty tree is refused."""
    sha, dirty = git_state(repo)
    if dirty:
        raise SystemExit(f"{repo}: uncommitted changes to tracked files; commit them first (the instance runs HEAD)")
    key = f"code/{sha}.tar.gz"
    if not s3_exists(s3, bucket, key):
        tar = subprocess.run(["git", "-C", str(repo), "archive", "--format=tar.gz", sha], capture_output=True,
                             check=True).stdout
        s3.put_object(Bucket=bucket, Key=key, Body=tar)
        print(f"code: uploaded {key} ({len(tar)} bytes)")
    return sha, key


def upload_env(s3, bucket, lock_path):
    body = Path(lock_path).read_bytes()
    key = env_key(body)
    if not s3_exists(s3, bucket, key):
        s3.put_object(Bucket=bucket, Key=key, Body=body)
        print(f"env: uploaded {key}")
    return key


def sync_data(s3, bucket, ds, manifest=None):
    """Upload a data subset to data/<name>/<sha>/ unless its MANIFEST.json is there.
    MANIFEST.json goes last, so its presence means the subset is complete."""
    manifest = manifest or data_manifest(ds["local_root"], ds["include"])
    sha = data_sha(manifest)
    prefix = f"data/{ds['name']}/{sha}/"
    if s3_exists(s3, bucket, prefix + "MANIFEST.json"):
        return {"name": ds["name"], "env": ds["env"], "sha": sha, "prefix": prefix, "uploaded": False}
    root = Path(os.path.expanduser(ds["local_root"]))
    total = 0
    for f in manifest["files"]:
        s3.upload_file(str(root / f["path"]), bucket, prefix + f["path"])
        total += f["size"]
    s3_put_json(s3, bucket, prefix + "MANIFEST.json", {"dataset": ds["name"], "sha": sha, **manifest})
    print(f"data {ds['name']}: uploaded {len(manifest['files'])} files, {total / 1e6:.0f} MB")
    return {"name": ds["name"], "env": ds["env"], "sha": sha, "prefix": prefix, "uploaded": True}


def run_manifest(*, run_id, job, git_sha, code_key, env, data, cfg, region, instance_types):
    return {"runId": run_id, "created": iso(utcnow()), "job": job, "configSha256": config_sha(job),
            "gitSha": git_sha, "codeKey": code_key, "envKey": env, "python": cfg["python"],
            "data": [{k: d[k] for k in ("name", "env", "sha", "prefix")} for d in data],
            "steps": job["steps"], "smoke": job["smoke"], "region": region,
            "amiId": cfg["ami"][region]["id"], "instanceTypes": instance_types,
            "maxMinutes": job["max_minutes"], "checkpointMinutes": job["checkpoint_minutes"],
            "maxAttempts": job.get("max_attempts", cfg["max_attempts"])}


# --- run: launch and watch -----------------------------------------------------------------

class Ctx:
    """One run as the launcher sees it."""

    def __init__(self, aws, cfg, manifest, bucket):
        self.aws, self.cfg, self.m, self.bucket = aws, cfg, manifest, bucket
        self.run_id = manifest["runId"]
        self.region = manifest["region"]
        self.s3 = aws.client("s3", cfg["bucket_region"])
        self.ec2 = aws.client("ec2", self.region)
        self.launches = (s3_get_json(self.s3, bucket, self.key("launches.json"))
                         or {"runId": self.run_id, "attempts": []})

    def key(self, rel):
        return f"runs/{self.run_id}/{rel}"

    def save_launches(self):
        s3_put_json(self.s3, self.bucket, self.key("launches.json"), self.launches)

    def last(self):
        return self.launches["attempts"][-1] if self.launches["attempts"] else None


def subnets_by_type(ec2, types):
    subnets = ec2.describe_subnets(Filters=[{"Name": "default-for-az", "Values": ["true"]}])["Subnets"]
    by_az = {s["AvailabilityZone"]: s["SubnetId"] for s in subnets}
    offers = ec2.describe_instance_type_offerings(LocationType="availability-zone",
                                                  Filters=[{"Name": "instance-type", "Values": list(types)}])
    out = {t: [] for t in types}
    for o in offers["InstanceTypeOfferings"]:
        if o["Location"] in by_az:
            out[o["InstanceType"]].append(by_az[o["Location"]])
    return {t: sorted(s) for t, s in out.items() if s}


class LaunchError(Exception):
    pass


def launch_attempt(ctx, attempt):
    cfg, m = ctx.cfg, ctx.m
    ec2 = ctx.ec2
    job = m["job"]
    expires = utcnow() + timedelta(minutes=m["maxMinutes"])
    sg = find_security_group(ec2, cfg["security_group"])
    if sg is None:
        raise SystemExit(f"{ctx.region}: security group {cfg['security_group']} missing; run setup")
    ud = user_data(bucket=ctx.bucket, bucket_region=cfg["bucket_region"], run_id=ctx.run_id, attempt=attempt,
                   code_key=m["codeKey"], max_minutes=m["maxMinutes"], uv_version=cfg["uv_version"],
                   grace=job["grace_seconds"], disable_os_lifetime=job.get("disable_os_lifetime", False))
    req = fleet_request(run_id=ctx.run_id, attempt=attempt, expires=expires, ami_id=m["amiId"],
                        root_device=cfg["ami"][ctx.region]["root_device"], root_gb=cfg["root_volume_gb"],
                        profile=cfg["instance_role"], sg_id=sg["GroupId"], user_data_text=ud,
                        subnets_by_type=subnets_by_type(ec2, m["instanceTypes"]))
    ClientError = client_error()
    tpl_name = req["LaunchTemplate"]["LaunchTemplateName"]
    try:
        ec2.create_launch_template(**req["LaunchTemplate"])
    except ClientError as e:
        raise LaunchError(classify_launch_error(err_code(e), str(e))) from e
    try:
        resp = ec2.create_fleet(**req["CreateFleet"])
    except ClientError as e:
        raise LaunchError(classify_launch_error(err_code(e), str(e))) from e
    finally:
        try:
            ec2.delete_launch_template(LaunchTemplateName=tpl_name)
        except Exception as e:  # reap deletes leftover templates
            print(f"attempt {attempt}: WARNING: launch template {tpl_name} not deleted: {e}")
    fleet = [inst for inst in resp.get("Instances", []) if inst.get("InstanceIds")]
    if not fleet:
        release_spot(ec2, [], [resp.get("FleetId")], f"attempt {attempt}")
        errs = resp.get("Errors", [])
        first = errs[0] if errs else {"ErrorCode": "?", "ErrorMessage": "no instance and no error"}
        raise LaunchError(classify_launch_error(first.get("ErrorCode"), first.get("ErrorMessage")))
    return adopt_instance(ctx, attempt, fleet[0], resp.get("FleetId"), expires)


def retrying(what, fn, tries=5, wait=5):
    for i in range(tries):
        try:
            return fn()
        except Exception as e:
            if i + 1 == tries:
                raise
            print(f"{what}: {type(e).__name__}: {e}; retry {i + 2}/{tries} in {wait}s")
            time.sleep(wait)


def adopt_instance(ctx, attempt, fleet_inst, fleet_id, expires):
    """Records a launched instance in launches.json, then describes it and arms its
    terminate schedule. Invariant: once CreateFleet returned an instance, every path
    either leaves it recorded with a schedule or terminates it; a failure after the
    terminate exits the CLI instead of returning, so no caller launches again."""
    iid = fleet_inst["InstanceIds"][0]
    ov = fleet_inst.get("LaunchTemplateAndOverrides", {}).get("Overrides", {})
    rec = {"n": attempt, "instanceId": iid, "fleetId": fleet_id, "instanceType": fleet_inst.get("InstanceType"),
           "az": ov.get("AvailabilityZone"), "launchTime": iso(utcnow()), "expires": iso(expires),
           "region": ctx.region}
    try:
        ctx.launches["attempts"].append(rec)
        retrying(f"attempt {attempt}: save launches.json", ctx.save_launches)
        ctx.ec2.get_waiter("instance_exists").wait(InstanceIds=[iid], WaiterConfig={"Delay": 5, "MaxAttempts": 24})
        inst = retrying(f"attempt {attempt}: describe {iid}",
                        lambda: ctx.ec2.describe_instances(InstanceIds=[iid])["Reservations"][0]["Instances"][0])
        rec.update(instanceType=inst["InstanceType"], az=inst["Placement"]["AvailabilityZone"],
                   launchTime=iso(inst["LaunchTime"]))
        retrying(f"attempt {attempt}: save launches.json", ctx.save_launches)
        print(f"attempt {attempt}: {iid} {rec['instanceType']} in {rec['az']}, expires {rec['expires']}")
        retrying(f"attempt {attempt}: terminate schedule", lambda: schedule_terminate(ctx, rec))
    except BaseException as e:
        try:
            ctx.ec2.terminate_instances(InstanceIds=[iid])
            outcome = "terminated by the launcher"
        except Exception as te:
            outcome = f"TERMINATE FAILED ({te}); run `reap` or terminate it by hand"
        if release_spot(ctx.ec2, [iid], [fleet_id], f"attempt {attempt}"):
            outcome += "; spot request/fleet not released, run `reap`"
        rec["outcome"] = f"launcher error: {type(e).__name__}: {e}; {outcome}"
        try:
            ctx.save_launches()
        except Exception as se:
            print(f"attempt {attempt}: launches.json not saved: {se}")
        raise SystemExit(f"attempt {attempt}: instance {iid} launched but not armed: {rec['outcome']}") from e
    return rec


def schedule_terminate(ctx, rec):
    cfg = ctx.cfg
    at = parse_iso(rec["expires"]) + timedelta(minutes=cfg["schedule_margin_minutes"])
    account = ctx.aws.account()
    req = schedule_request(run_id=ctx.run_id, attempt=rec["n"], instance_id=rec["instanceId"], at=at,
                           role_arn=f"arn:aws:iam::{account}:role/{cfg['scheduler_role']}")
    sch = ctx.aws.client("scheduler", ctx.region)
    try:
        sch.create_schedule(**req)
    except client_error() as e:
        if err_code(e) != "ConflictException":  # created by an earlier try whose answer was lost
            raise
    rec["schedule"] = req["Name"]
    ctx.save_launches()
    print(f"attempt {rec['n']}: terminate schedule {req['Name']} at {iso(at)}")


def instance_state(ec2, iid):
    """(state name, instance), or (None, None) once EC2 no longer lists it."""
    try:
        r = ec2.describe_instances(InstanceIds=[iid])["Reservations"]
    except client_error() as e:
        if err_code(e) == "InvalidInstanceID.NotFound":
            return None, None
        raise
    if not r:
        return None, None
    inst = r[0]["Instances"][0]
    return inst["State"]["Name"], inst


def attempt_record(ctx, n):
    status = s3_get_json(ctx.s3, ctx.bucket, ctx.key("status.json")) or {}
    for r in status.get("attempts", []):
        if r.get("n") == n:
            return r
    return s3_get_json(ctx.s3, ctx.bucket, ctx.key(f"status/attempt-{n}.boot.json"))


def launch_with_retry(ctx, attempt, tries=10, wait=120):
    for i in range(tries):
        try:
            return launch_attempt(ctx, attempt)
        except LaunchError as e:
            msg = str(e)
            print(f"attempt {attempt}: launch failed: {msg}")
            if not msg.startswith("no spot capacity") or i + 1 == tries:
                raise
            time.sleep(wait)


def unrecorded_instances(ctx):
    """Ids of instances tagged for this run, not yet terminated, that launches.json does not list."""
    recorded = {r["instanceId"] for r in ctx.launches["attempts"]}
    out = []
    for i in tagged_instances(ctx.ec2, LIVE_STATES + ["shutting-down"]):
        tags = {t["Key"]: t["Value"] for t in i.get("Tags", [])}
        if tags.get("gmesh-run") == ctx.run_id and i["InstanceId"] not in recorded:
            out.append(i["InstanceId"])
    return out


def watch(ctx):
    """Polls status.json and the instance every WATCH_EVERY seconds; relaunches per
    relaunch_decision; returns the final reason."""
    max_attempts = ctx.m["maxAttempts"]
    shown = None
    while True:
        rec = ctx.last()
        n = rec["n"]
        st, inst = instance_state(ctx.ec2, rec["instanceId"])
        if st in (None, "terminated") and "endObserved" not in rec:
            rec["endObserved"] = iso((termination_time(inst) if inst else None) or utcnow())
            ctx.save_launches()
        expired = utcnow() >= parse_iso(rec["expires"])
        action, why = relaunch_decision(attempt_record(ctx, n), n, max_attempts, st, expired)
        if (action, why) != shown:
            print(f"{iso(utcnow())} attempt {n}: {action} ({why})", flush=True)
            shown = (action, why)
        if action == "wait":
            time.sleep(WATCH_EVERY)
            continue
        rec["outcome"] = why
        ctx.save_launches()
        if action == "stop":
            return why
        release_run(ctx, f"attempt {n}")  # the ended attempt's spot request holds the vCPU quota
        try:
            launch_with_retry(ctx, n + 1)
        except LaunchError as e:
            return f"relaunch failed: {e}"


def wait_terminated(ctx, timeout=300):
    ids = [r["instanceId"] for r in ctx.launches["attempts"]]
    deadline = time.monotonic() + timeout
    while True:
        alive = [(iid, st) for iid in ids for st in [instance_state(ctx.ec2, iid)[0]]
                 if st not in (None, "terminated")]
        if not alive:
            print(f"terminated: {', '.join(ids)}")
            return True
        if time.monotonic() > deadline:
            print(f"NOT TERMINATED after {timeout}s: {alive}")
            return False
        time.sleep(15)


def attempt_end(rec, inst, status_rec):
    """(end time, source) of an attempt's billing interval: EC2's termination time while
    EC2 still lists the instance, else the time `watch` saw it gone, else the runner's
    end in status.json, else now (the instance may still run)."""
    t = termination_time(inst) if inst else None
    if t is not None:
        return t, "StateTransitionReason"
    if rec.get("endObserved"):
        return parse_iso(rec["endObserved"]), "watch"
    if (status_rec or {}).get("end"):
        return parse_iso(status_rec["end"]), "status"
    return utcnow(), "now"


def record_cost(ctx):
    """runs/<id>/cost.json: per attempt, instance hours x the spot price history of its
    type/AZ, plus root EBS GB-hours at the gp3 list price."""
    cfg = ctx.cfg
    rows = []
    for rec in ctx.launches["attempts"]:
        st, inst = instance_state(ctx.ec2, rec["instanceId"])
        start = parse_iso(rec["launchTime"])
        end, end_source = attempt_end(rec, inst, attempt_record(ctx, rec["n"]))
        hist = ctx.ec2.describe_spot_price_history(InstanceTypes=[rec["instanceType"]],
                                                   AvailabilityZone=rec["az"], ProductDescriptions=["Linux/UNIX"],
                                                   StartTime=start, EndTime=end)["SpotPriceHistory"]
        prices = [(h["Timestamp"], float(h["SpotPrice"])) for h in hist]
        hours = (end - start).total_seconds() / 3600
        spot = cost_of([(start, end)], prices) if prices else None
        ebs = cfg["root_volume_gb"] * hours * cfg["ebs_gp3_usd_gb_month"] / 730
        rows.append({"n": rec["n"], "instanceId": rec["instanceId"], "instanceType": rec["instanceType"],
                     "az": rec["az"], "start": iso(start), "end": iso(end), "endSource": end_source,
                     "state": st, "hours": round(hours, 4), "prices": [[iso(t), p] for t, p in sorted(prices)],
                     "spotUsd": None if spot is None else round(spot, 4), "ebsUsd": round(ebs, 4)})
    total = sum((r["spotUsd"] or 0) + r["ebsUsd"] for r in rows)
    out = {"runId": ctx.run_id, "attempts": rows, "totalUsd": round(total, 4),
           "hours": round(sum(r["hours"] for r in rows), 4), "computed": iso(utcnow())}
    s3_put_json(ctx.s3, ctx.bucket, ctx.key("cost.json"), out)
    print(f"cost: {out['hours']:.3f} instance-hours, ${out['totalUsd']:.3f} (cost.json)")
    return out


def pull(aws, cfg, bucket, run_id):
    """runs/<run-id>/ (manifest, status, launches, cost, out, logs) to local_runs; not the checkpoints."""
    s3 = aws.client("s3", cfg["bucket_region"])
    dest = Path(os.path.expanduser(cfg["local_runs"])) / run_id
    n = 0
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=bucket, Prefix=f"runs/{run_id}/"):
        for obj in page.get("Contents", []):
            rel = obj["Key"][len(f"runs/{run_id}/"):]
            p = dest / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            s3.download_file(bucket, obj["Key"], str(p))
            n += 1
    print(f"pulled {n} files to {dest}")
    return dest


def cmd_run(a, cfg):
    if a.dry_run:
        return dry_run(a, cfg)
    aws = Aws(a.profile)
    account = aws.account()
    bucket = bucket_name(cfg, account)
    s3 = aws.client("s3", cfg["bucket_region"])
    reap(aws, cfg)
    if a.resume:
        m = s3_get_json(s3, bucket, f"runs/{a.resume}/manifest.json")
        if m is None:
            raise SystemExit(f"run {a.resume}: no manifest in s3://{bucket}")
        ctx = Ctx(aws, cfg, m, bucket)
        stray = unrecorded_instances(ctx)
        if stray:
            raise SystemExit(f"run {a.resume}: live instance(s) {', '.join(stray)} tagged for this run are not in "
                             "launches.json; terminate them (or wait for them to end) before --resume")
        rec = ctx.last()
        if rec is None:
            launch_with_retry(ctx, 1)
    else:
        job = load_job(a.job, parse_set(a.set))
        region = a.region or cfg["default_region"]
        types = check_instance_types(a.instance_types.split(",") if a.instance_types
                                 else job.get("instance_types", cfg["instance_types"]), cfg["instance_types"])
        run_id = run_id_of(job, utcnow(), a.tag)
        git_sha, code_key = upload_code(s3, bucket)
        env = upload_env(s3, bucket, REPO / job["requirements"])
        data = [sync_data(s3, bucket, ds) for ds in job["data"]]
        m = run_manifest(run_id=run_id, job=job, git_sha=git_sha, code_key=code_key, env=env, data=data,
                         cfg=cfg, region=region, instance_types=types)
        if s3_exists(s3, bucket, f"runs/{run_id}/manifest.json"):
            raise SystemExit(f"run {run_id} exists; use --tag or run --resume {run_id}")
        s3_put_json(s3, bucket, f"runs/{run_id}/manifest.json", m)
        print(f"run {run_id}")
        ctx = Ctx(aws, cfg, m, bucket)
        launch_with_retry(ctx, 1)
    if a.detach:
        print(f"detached; continue with: uv run eval/gpu/gpu_train.py run --resume {ctx.run_id}")
        return 0
    final = watch(ctx)
    print(f"run {ctx.run_id}: {final}")
    pull(aws, cfg, bucket, ctx.run_id)
    ok = wait_terminated(ctx)
    released = not release_run(ctx, f"run {ctx.run_id}")
    record_cost(ctx)
    reaped = not reap(aws, cfg)
    return 0 if final == "done" and ok and released and reaped else 1


def dry_run(a, cfg):
    """Prints what `run` would send, with no AWS call."""
    if a.resume:
        raise SystemExit("--dry-run needs a job file, not --resume")
    job = load_job(a.job, parse_set(a.set))
    region = a.region or cfg["default_region"]
    types = check_instance_types(a.instance_types.split(",") if a.instance_types
                                 else job.get("instance_types", cfg["instance_types"]), cfg["instance_types"])
    now = utcnow()
    run_id = run_id_of(job, now, a.tag)
    bucket = bucket_name(cfg, "<account>")
    git_sha, dirty = git_state()
    code_key = f"code/{git_sha}.tar.gz"
    env = env_key((REPO / job["requirements"]).read_bytes())
    data = []
    for ds in job["data"]:
        man = data_manifest(ds["local_root"], ds["include"])
        sha = data_sha(man)
        data.append({"name": ds["name"], "env": ds["env"], "sha": sha, "prefix": f"data/{ds['name']}/{sha}/"})
        mb = sum(f["size"] for f in man["files"]) / 1e6
        print(f"data {ds['name']}: {len(man['files'])} files, {mb:.0f} MB, sha {sha[:12]}")
    print(f"code: {code_key}" + ("  (tree is DIRTY: run would refuse)" if dirty else ""))
    m = run_manifest(run_id=run_id, job=job, git_sha=git_sha, code_key=code_key, env=env, data=data, cfg=cfg,
                     region=region, instance_types=types)
    print(f"\n== runs/{run_id}/manifest.json\n{json.dumps(m, indent=1)}")
    expires = now + timedelta(minutes=job["max_minutes"])
    ud = user_data(bucket=bucket, bucket_region=cfg["bucket_region"], run_id=run_id, attempt=1, code_key=code_key,
                   max_minutes=job["max_minutes"], uv_version=cfg["uv_version"], grace=job["grace_seconds"],
                   disable_os_lifetime=job.get("disable_os_lifetime", False))
    req = fleet_request(run_id=run_id, attempt=1, expires=expires, ami_id=cfg["ami"][region]["id"],
                        root_device=cfg["ami"][region]["root_device"], root_gb=cfg["root_volume_gb"],
                        profile=cfg["instance_role"], sg_id="<sg gmesh-train>", user_data_text=ud,
                        subnets_by_type={t: [f"<default subnets of {region} offering {t}>"] for t in types})
    req["LaunchTemplate"]["LaunchTemplateData"]["UserData"] = f"<base64 of the user data below, {len(ud)} bytes>"
    print(f"\n== create_launch_template + create_fleet ({region})\n{json.dumps(req, indent=1)}")
    print(f"\n== user data (attempt 1)\n{ud}")
    at = expires + timedelta(minutes=cfg["schedule_margin_minutes"])
    sreq = schedule_request(run_id=run_id, attempt=1, instance_id="<instance id>", at=at,
                            role_arn=f"arn:aws:iam::<account>:role/{cfg['scheduler_role']}")
    print(f"\n== scheduler.create_schedule ({region})\n{json.dumps(sreq, indent=1)}")
    print(f"\nlifetime: in-OS shutdown at launch + {job['max_minutes']} min"
          f"{' (DISABLED)' if job.get('disable_os_lifetime') else ''}; cloud terminate at {iso(at)}")
    return 0


# --- status, reap, verify-clean, interrupt ---------------------------------------------------

def cmd_status(a, cfg):
    aws = Aws(a.profile)
    bucket = bucket_name(cfg, aws.account())
    s3 = aws.client("s3", cfg["bucket_region"])
    for name in ("launches.json", "status.json", "cost.json"):
        doc = s3_get_json(s3, bucket, f"runs/{a.run_id}/{name}")
        print(f"== {name}\n{json.dumps(doc, indent=1) if doc is not None else '(absent)'}")
    print(f"== {ckpt_prefix(a.run_id)}")
    for page in s3.get_paginator("list_objects_v2").paginate(Bucket=bucket, Prefix=ckpt_prefix(a.run_id)):
        for obj in page.get("Contents", []):
            print(f"{obj['Key']}  {obj['Size']} bytes  {iso(obj['LastModified'])}")
    return 0


def cmd_pull(a, cfg):
    aws = Aws(a.profile)
    pull(aws, cfg, bucket_name(cfg, aws.account()), a.run_id)
    return 0


def tagged_instances(ec2, states):
    out = []
    for page in ec2.get_paginator("describe_instances").paginate(
            Filters=[{"Name": f"tag:{TAG}", "Values": ["1"]}, {"Name": "instance-state-name", "Values": states}]):
        for r in page["Reservations"]:
            out += r["Instances"]
    return out


def gmesh_schedules(sch):
    out = []
    for page in sch.get_paginator("list_schedules").paginate(NamePrefix="gmesh-"):
        out += page["Schedules"]
    return out


def gmesh_templates(ec2):
    return ec2.describe_launch_templates(Filters=[{"Name": f"tag:{TAG}", "Values": ["1"]}])["LaunchTemplates"]


FLEET_OPEN_STATES = ["submitted", "active", "modifying"]


def gmesh_fleets(ec2, run_id=None):
    """Fleets tagged gmesh-train=1 (and gmesh-run=`run_id` when given) not yet deleted;
    an instant fleet stays active until DeleteFleets."""
    out = []
    for page in ec2.get_paginator("describe_fleets").paginate(
            Filters=[{"Name": "fleet-state", "Values": FLEET_OPEN_STATES}]):
        for f in page["Fleets"]:
            tags = {t["Key"]: t["Value"] for t in f.get("Tags", [])}
            if tags.get(TAG) == "1" and (run_id is None or tags.get("gmesh-run") == run_id):
                out.append(f)
    return out


def chunks(items, n):
    return [items[i:i + n] for i in range(0, len(items), n)]


def release_spot(ec2, instance_ids, fleet_ids, label):
    """Cancels the open/active spot requests of `instance_ids`, then deletes the instant
    fleets `fleet_ids`. Call it only for instances that ended or are being terminated:
    DeleteFleets of an instant fleet must terminate its instances (TerminateInstances
    false is not supported for type instant). Cancelling a one-time request does not
    terminate its instance. Idempotent: a closed/cancelled request is not listed and a
    fleet that no longer exists counts as deleted. When a cancel fails no fleet is
    deleted: the open fleet keeps the request visible to verify-clean (through the
    fleet's instance ids) after EC2 stops listing the terminated instance. Never
    raises; returns the problems."""
    problems = []
    instance_ids = sorted({i for i in instance_ids if i})
    fleet_ids = sorted({f for f in fleet_ids if f})
    for part in chunks(instance_ids, 100):
        try:
            reqs = ec2.describe_spot_instance_requests(Filters=[
                {"Name": "instance-id", "Values": part},
                {"Name": "state", "Values": ["open", "active"]}])["SpotInstanceRequests"]
            ids = sorted(r["SpotInstanceRequestId"] for r in reqs)
            if ids:
                ec2.cancel_spot_instance_requests(SpotInstanceRequestIds=ids)
                print(f"{label}: cancelled spot request(s) {', '.join(ids)}")
        except Exception as e:
            problems.append(f"spot requests of {', '.join(part)} not cancelled: {type(e).__name__}: {e}")
    if problems and fleet_ids:
        problems.append(f"fleet(s) {', '.join(fleet_ids)} kept while a spot request may be active")
        fleet_ids = []
    for part in chunks(fleet_ids, 25):  # DeleteFleets takes at most 25 instant fleets
        try:
            resp = ec2.delete_fleets(FleetIds=part, TerminateInstances=True)
        except Exception as e:
            problems.append(f"fleet(s) {', '.join(part)} not deleted: {type(e).__name__}: {e}")
            continue
        done = [f["FleetId"] for f in resp.get("SuccessfulFleetDeletions", [])]
        if done:
            print(f"{label}: deleted fleet(s) {', '.join(done)}")
        for f in resp.get("UnsuccessfulFleetDeletions", []):
            err = f.get("Error", {})
            if err.get("Code") != "fleetIdDoesNotExist":
                problems.append(f"fleet {f.get('FleetId')} not deleted: {err.get('Code')}: {err.get('Message')}")
    for p in problems:
        print(f"{label}: WARNING: {p}")
    return problems


def release_run(ctx, label):
    """release_spot for every attempt of the run: the instances and fleets launches.json
    records plus any fleet tagged for the run (one created but never recorded). Only
    once no attempt of the run can still be running."""
    ids = {r["instanceId"] for r in ctx.launches["attempts"]}
    fleets = {r.get("fleetId") for r in ctx.launches["attempts"]}
    problems = []
    try:
        for f in gmesh_fleets(ctx.ec2, ctx.run_id):
            fleets.add(f["FleetId"])
            ids |= fleet_instance_ids(f)
    except Exception as e:
        problems.append(f"fleets of {ctx.run_id} not listed: {type(e).__name__}: {e}")
        print(f"{label}: WARNING: {problems[-1]}")
    return problems + release_spot(ctx.ec2, ids, fleets, label)


def release_ended(ec2, terminated_ids, live_ids, now, label):
    """reap's spot cleanup in one region: cancels the open/active spot requests of tagged
    instances that ended (or `terminated_ids`, just terminated) and of the instances of
    reapable_fleets, and deletes those fleets. Never touches a fleet with an instance
    in `live_ids`."""
    try:
        ended = {i["InstanceId"] for i in tagged_instances(ec2, ["shutting-down", "terminated"])} | set(terminated_ids)
        stale = reapable_fleets(gmesh_fleets(ec2), live_ids, ended, now)
    except Exception as e:
        print(f"{label}: WARNING: spot requests and fleets not checked: {type(e).__name__}: {e}")
        return [f"not checked: {e}"]
    fleet_iids = [iid for f in stale for iid in fleet_instance_ids(f)]
    return release_spot(ec2, sorted(ended) + fleet_iids, [f["FleetId"] for f in stale], label)


def reap(aws, cfg):
    """Terminates tagged instances past gmesh-expires, cancels the spot requests of
    ended tagged instances and deletes their ended fleets (release_ended), deletes
    schedules whose instance is gone and launch templates older than 10 minutes, in
    every configured region. Returns release_ended's problems (empty when clean)."""
    now = utcnow()
    problems = []
    for region in cfg["regions"]:
        ec2 = aws.client("ec2", region)
        live = tagged_instances(ec2, LIVE_STATES)
        ids = expired_instances(live, now)
        if ids:
            ec2.terminate_instances(InstanceIds=ids)
            print(f"reap {region}: terminated expired {', '.join(ids)}")
        live_ids = {i["InstanceId"] for i in live} - set(ids)
        problems += [f"{region}: {p}" for p in release_ended(ec2, ids, live_ids, now, f"reap {region}")]
        sch = aws.client("scheduler", region)
        for s in gmesh_schedules(sch):
            target = json.loads(sch.get_schedule(Name=s["Name"])["Target"].get("Input") or "{}")
            if not set(target.get("InstanceIds", [])) & live_ids:
                sch.delete_schedule(Name=s["Name"])
                print(f"reap {region}: deleted schedule {s['Name']}")
        for t in gmesh_templates(ec2):
            if now - t["CreateTime"] > timedelta(minutes=10):
                ec2.delete_launch_template(LaunchTemplateId=t["LaunchTemplateId"])
                print(f"reap {region}: deleted launch template {t['LaunchTemplateName']}")
    return problems


def snapshot_region(aws, region):
    ec2 = aws.client("ec2", region)
    states = LIVE_STATES + ["shutting-down"]
    gpu = []
    for page in ec2.get_paginator("describe_instances").paginate(
            Filters=[{"Name": "instance-type", "Values": ["g*", "p*"]},
                     {"Name": "instance-state-name", "Values": states}]):
        for r in page["Reservations"]:
            gpu += r["Instances"]
    tagged = tagged_instances(ec2, states)
    tagged_ids = {i["InstanceId"] for i in tagged}
    spot = ec2.describe_spot_instance_requests(
        Filters=[{"Name": "state", "Values": ["open", "active"]}])["SpotInstanceRequests"]
    spot_iids = sorted({r["InstanceId"] for r in spot if r.get("InstanceId")})
    live_spot_ids = set()
    if spot_iids:
        for page in ec2.get_paginator("describe_instances").paginate(
                Filters=[{"Name": "instance-id", "Values": spot_iids},
                         {"Name": "instance-state-name", "Values": LIVE_STATES}]):
            for r in page["Reservations"]:
                live_spot_ids |= {i["InstanceId"] for i in r["Instances"]}
    fleets = gmesh_fleets(ec2)
    ours = tagged_ids | {i["InstanceId"] for i in tagged_instances(ec2, ["terminated"])}
    for f in fleets:
        ours |= fleet_instance_ids(f)
    spot = live_spot_requests(spot, live_spot_ids, ours)
    vols = ec2.describe_volumes(Filters=[{"Name": "tag-key", "Values": ["gmesh-run"]},
                                         {"Name": "status", "Values": ["available"]}])["Volumes"]
    return {"tagged": tagged, "gpu": [i for i in gpu if i["InstanceId"] not in tagged_ids], "spot": spot,
            "volumes": vols, "schedules": gmesh_schedules(aws.client("scheduler", region)),
            "templates": gmesh_templates(ec2), "fleets": fleets}


def cmd_verify_clean(a, cfg):
    aws = Aws(a.profile)
    deadline = time.monotonic() + a.wait
    while True:
        snap = {r: snapshot_region(aws, r) for r in cfg["regions"]}
        settling = [i for s in snap.values() for i in s["tagged"] + s["gpu"] if i["State"]["Name"] == "shutting-down"]
        if not settling or time.monotonic() > deadline:
            break
        time.sleep(15)
    problems = clean_problems(snap)
    for p in problems:
        print(f"NOT CLEAN: {p}")
    if not problems:
        print(f"clean: {', '.join(cfg['regions'])}")
    return 1 if problems else 0


def cmd_interrupt(a, cfg):
    """Test hook: a spot interruption notice through AWS FIS (--fis) or a hard
    TerminateInstances (--terminate) on the run's current instance."""
    aws = Aws(a.profile)
    account = aws.account()
    bucket = bucket_name(cfg, account)
    m = s3_get_json(aws.client("s3", cfg["bucket_region"]), bucket, f"runs/{a.run_id}/manifest.json")
    if m is None:
        raise SystemExit(f"run {a.run_id}: no manifest")
    ctx = Ctx(aws, cfg, m, bucket)
    rec = ctx.last()
    if rec is None:
        raise SystemExit(f"run {a.run_id}: no launched attempt")
    st, _ = instance_state(ctx.ec2, rec["instanceId"])
    if st != "running":
        raise SystemExit(f"attempt {rec['n']}: instance {rec['instanceId']} is {st}, not running")
    if a.terminate:
        ctx.ec2.terminate_instances(InstanceIds=[rec["instanceId"]])
        print(f"terminated {rec['instanceId']} (no notice)")
        return 0
    fis = aws.client("fis", ctx.region)
    arn = f"arn:aws:ec2:{ctx.region}:{account}:instance/{rec['instanceId']}"
    tpl = fis.create_experiment_template(
        clientToken=f"{a.run_id}-a{rec['n']}-{int(time.time())}",
        description=f"spot notice for {a.run_id} attempt {rec['n']}",
        targets={"Instances": {"resourceType": "aws:ec2:spot-instance", "resourceArns": [arn], "selectionMode": "ALL"}},
        actions={"interrupt": {"actionId": "aws:ec2:send-spot-instance-interruptions",
                               "parameters": {"durationBeforeInterruption": "PT2M"},
                               "targets": {"SpotInstances": "Instances"}}},
        stopConditions=[{"source": "none"}], roleArn=f"arn:aws:iam::{account}:role/{cfg['fis_role']}",
        tags={TAG: "1", "gmesh-run": a.run_id})["experimentTemplate"]
    try:
        exp = fis.start_experiment(experimentTemplateId=tpl["id"], tags={TAG: "1"})["experiment"]
        print(f"FIS experiment {exp['id']} started on {rec['instanceId']}")
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline:
            state = fis.get_experiment(id=exp["id"])["experiment"]["state"]
            if state["status"] in ("completed", "stopped", "failed"):
                print(f"FIS experiment {exp['id']}: {state['status']} {state.get('reason', '')}")
                return 0 if state["status"] == "completed" else 1
            time.sleep(15)
        print(f"FIS experiment {exp['id']}: still running after 600 s")
        return 1
    finally:
        fis.delete_experiment_template(id=tpl["id"])


def gpu_instances_everywhere(aws):
    """{region: [instance]} of the instances the kill-switch Lambda would terminate: GPU
    families (is_gpu) in pending|running|stopping|stopped, in every enabled region."""
    regions = [r["RegionName"] for r in aws.client("ec2", "us-east-1").describe_regions()["Regions"]]
    out = {}
    for region in sorted(regions):
        found = []
        for page in aws.client("ec2", region).get_paginator("describe_instances").paginate(
                Filters=[{"Name": "instance-state-name", "Values": LIVE_STATES}]):
            for r in page["Reservations"]:
                found += [i for i in r["Instances"] if is_gpu(i["InstanceType"])]
        if found:
            out[region] = found
    return out


def kill_switch_problems(found, run_id):
    """Why the kill-switch Lambda must not fire now, from {region: [instance]}. With no
    run id (before launching the test instance) any GPU instance is a problem; with one,
    exactly one running instance tagged for that run may exist."""
    ours, problems = [], []
    for region, insts in sorted(found.items()):
        for i in insts:
            tags = {t["Key"]: t["Value"] for t in i.get("Tags", [])}
            desc = f"{region}: {i['InstanceId']} ({i['InstanceType']}, {i['State']['Name']}, run {tags.get('gmesh-run')})"
            if (run_id is not None and tags.get(TAG) == "1" and tags.get("gmesh-run") == run_id
                    and i["State"]["Name"] == "running"):
                ours.append(desc)
            else:
                problems.append(f"other GPU instance {desc}")
    if run_id is not None and len(ours) != 1:
        problems.append(f"expected exactly one running instance of run {run_id}, found {len(ours)}")
    return problems


def budget_event(budget):
    """A synthetic SNS budget notification, as the budget's topic delivers it to the Lambda."""
    return {"Records": [{"EventSource": "aws:sns", "EventVersion": "1.0", "Sns": {
        "Type": "Notification", "Timestamp": iso(utcnow()),
        "Subject": f"AWS Budgets: {budget} has exceeded your alert threshold",
        "Message": f"Synthetic budget notification for {budget} (gpu_train.py kill-switch-test)."}}]}


def cmd_kill_switch_test(a, cfg):
    """Without a run id: checks that no GPU instance runs anywhere (before launching the
    test instance). With one: checks again that the run's instance is the only GPU
    instance, invokes the kill-switch Lambda with a synthetic budget event, waits for
    the termination, reaps the schedule and runs verify-clean."""
    aws = Aws(a.profile)
    problems = kill_switch_problems(gpu_instances_everywhere(aws), a.run_id)
    for p in problems:
        print(f"REFUSED: {p}")
    if problems:
        return 1
    if a.run_id is None:
        print("no GPU instance in any enabled region: launch the test instance, then kill-switch-test <run-id>")
        return 0
    ks = cfg["kill_switch"]
    resp = aws.client("lambda", ks["region"]).invoke(FunctionName=ks["function"], InvocationType="RequestResponse",
                                                     Payload=json.dumps(budget_event(ks["budget"])).encode())
    body = resp["Payload"].read().decode()
    print(f"lambda {ks['function']}: status {resp['StatusCode']} {resp.get('FunctionError', '')} {body}")
    if resp.get("FunctionError"):
        return 1
    bucket = bucket_name(cfg, aws.account())
    m = s3_get_json(aws.client("s3", cfg["bucket_region"]), bucket, f"runs/{a.run_id}/manifest.json")
    if m is None:
        raise SystemExit(f"run {a.run_id}: no manifest")
    ok = wait_terminated(Ctx(aws, cfg, m, bucket))
    reaped = not reap(aws, cfg)
    return 1 if cmd_verify_clean(a, cfg) or not ok or not reaped else 0


# --- CLI ----------------------------------------------------------------------------------------

def parse(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--profile", help="AWS profile (default: $AWS_PROFILE or default)")
    ap.add_argument("--config", default=str(HERE / "config.toml"))
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("setup", help="bucket, roles, instance profile, security groups (idempotent)")
    s.add_argument("--with-fis", action="store_true", help="also the FIS role for the interrupt test hook")
    s.add_argument("--ssm", action="store_true", help="attach AmazonSSMManagedInstanceCore to the instance role")
    s.add_argument("--check", action="store_true", help="read-only: report what setup would change")
    r = sub.add_parser("run", help="launch a job and watch it to the end")
    r.add_argument("job", nargs="?")
    r.add_argument("--set", action="append", default=[], metavar="K=V")
    r.add_argument("--region")
    r.add_argument("--instance-types", help="comma-separated; overrides config and job")
    r.add_argument("--tag", help="suffix of the run id (two runs of one job within a minute)")
    r.add_argument("--resume", metavar="RUN_ID")
    r.add_argument("--detach", action="store_true", help="launch attempt 1 and exit")
    r.add_argument("--dry-run", action="store_true", help="print the requests; no AWS call")
    for name in ("status", "pull"):
        p = sub.add_parser(name)
        p.add_argument("run_id")
    sub.add_parser("reap", help="terminate expired tagged instances, delete leftover schedules")
    v = sub.add_parser("verify-clean", help="fail unless no training resource survives")
    v.add_argument("--wait", type=float, default=300, help="seconds to wait for shutting-down instances")
    k = sub.add_parser("kill-switch-test", help="fire the budget kill-switch Lambda at the run's only GPU instance")
    k.add_argument("run_id", nargs="?", help="omit to only check that no GPU instance runs anywhere")
    k.add_argument("--wait", type=float, default=300, help="verify-clean: seconds for shutting-down instances")
    i = sub.add_parser("interrupt", help="test hook: interrupt the run's current instance")
    i.add_argument("run_id")
    g = i.add_mutually_exclusive_group(required=True)
    g.add_argument("--fis", action="store_true")
    g.add_argument("--terminate", action="store_true")
    a = ap.parse_args(argv)
    if a.cmd == "run" and not (a.job or a.resume):
        ap.error("run needs a job file or --resume <run-id>")
    return a


def main(argv=None):
    a = parse(argv)
    cfg = load_config(a.config)
    if a.cmd == "reap":
        return 1 if reap(Aws(a.profile), cfg) else 0
    return {"setup": cmd_setup, "run": cmd_run, "status": cmd_status, "pull": cmd_pull,
            "verify-clean": cmd_verify_clean, "interrupt": cmd_interrupt,
            "kill-switch-test": cmd_kill_switch_test}[a.cmd](a, cfg)


if __name__ == "__main__":
    sys.exit(main())
