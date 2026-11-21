# Spot GPU training launcher

Runs a training job on one AWS spot GPU instance (g5.xlarge / g5.2xlarge, A10G
24 GB), relaunches it after a spot interruption, pulls the results back and
checks that nothing is left running. Design:
[docs/design/GM-457-spot-gpu-training.md](../../docs/design/GM-457-spot-gpu-training.md).

Laptop side: `gpu_train.py` (boto3, pinned in its PEP 723 header, run with
`uv run`). Instance side: `runner.py` (stdlib only), started by the user data
script `user_data.sh.tmpl`.

## One-command launch

```
caffeinate -i -s uv run eval/gpu/gpu_train.py run eval/gpu/jobs/gm436-parity.toml
```

It refuses uncommitted changes to tracked files (the instance runs `git
archive HEAD`), uploads the code, the env lock and the job's data subsets
(content-addressed: unchanged data is not uploaded again), launches attempt 1,
then watches every 30 s. An interrupted attempt, or one whose instance vanished
without a status before its lifetime, is relaunched with the same run id (up to
`max_attempts`); the trainer resumes from the last uploaded checkpoint. At the
end the run is pulled to `~/Projects/ClaudeProjects/g-mesh-data/gpu-runs/<run-id>/`,
`cost.json` is written, and the instances are checked terminated.

Options: `--set k=v` (a job key such as `max_minutes`, or a `[vars]` entry such
as `mode`), `--region us-west-2` (when us-east-1 has no capacity),
`--instance-types g5.xlarge` (the configured types, or G-family types of one
family such as `g6.xlarge`; anything else is refused), `--tag k1` (two runs of one job within a minute),
`--detach` (launch and exit; continue with `run --resume <run-id>`),
`--dry-run` (print the manifest, launch template, fleet request, user data and
terminate schedule; no AWS call).

If the laptop slept or the watcher was stopped, nothing is lost:
`uv run eval/gpu/gpu_train.py run --resume <run-id>`. It refuses while an
instance tagged for the run is alive but missing from `launches.json`.

Other commands: `status <run-id>`, `pull <run-id>`, `reap`, `verify-clean`,
`interrupt <run-id> --fis|--terminate` (test hook for the resume test),
`kill-switch-test [<run-id>]` (below).

## Setup (once per account)

```
uv run eval/gpu/gpu_train.py setup --check      # read-only: what would change
uv run eval/gpu/gpu_train.py setup [--with-fis] [--ssm]
```

Creates, idempotently (a second run prints `no changes`): the bucket
`gmesh-train-<account>-us-east-1` (public access blocked, SSE-S3, lifecycle:
abort incomplete uploads after 1 day, expire `ckpt/` after 30 days; `runs/`
does not expire), the role
and instance profile `gmesh-train-instance` (S3 only, see `iam/`), the role
`gmesh-train-scheduler` (terminate tagged instances only), with `--with-fis`
the role `gmesh-train-fis`, and the security group `gmesh-train` (no ingress)
in the default VPC of each region. It then dry-runs a spot launch and simulates
the IAM user's policy for PassRole, CreateFleet and CreateSchedule.

A launch refused with an explicit deny means the budget kill switch fired
(deny-all-EC2 policy attached to the IAM user).

## Safety net

1. The runner and the user data script power off on every exit path
   (`InstanceInitiatedShutdownBehavior=terminate`).
2. `shutdown -h +<max_minutes>` is the first line of the user data.
3. An EventBridge Scheduler one-time schedule terminates the instance 10 min
   after its `gmesh-expires` tag.
4. `reap` (run at the start and end of `run`) terminates tagged instances past
   `gmesh-expires`, cancels the spot requests of ended tagged instances, deletes
   tagged fleets whose instances all ended (or, with no instance listed, older
   than 10 min) and deletes leftover schedules and launch templates. It exits 1
   when a spot request or fleet could not be released; a fleet is kept while
   its request's cancel failed, so verify-clean keeps reporting it.
5. `run` cancels an attempt's spot request and deletes its instant fleet when
   the attempt ends (before a relaunch, and after the last attempt): an active
   request keeps holding the spot vCPU quota after its instance terminated.
6. The `gpu-training-50` budget kill switch ($50/month, hours of lag).

After any AWS experiment:

```
uv run eval/gpu/gpu_train.py verify-clean
```

fails unless, in us-east-1 and us-west-2, no `gmesh-train=1` instance and no
G/P-family instance is pending/running/stopping/stopped/shutting-down (it waits
up to 5 min for shutting-down), no spot request is open, or active for a live
instance or for a `gmesh-train` / gmesh fleet instance (whatever its state), no
tagged fleet is submitted/active/modifying, no `gmesh-run` volume is available,
and no `gmesh-*` schedule or tagged launch template is left.

Kill-switch scenario (fires the real `gpu-training-50-kill-gpu` Lambda, which
terminates every GPU-family instance in every enabled region):

```
uv run eval/gpu/gpu_train.py kill-switch-test            # no GPU instance anywhere, else refuses
uv run eval/gpu/gpu_train.py run eval/gpu/jobs/smoke.toml --set mode=hang --detach
uv run eval/gpu/gpu_train.py kill-switch-test <run-id>   # re-checks, invokes, waits, reaps, verify-clean
```

The second call invokes the Lambda (synthetic SNS budget event) only when the
run's running instance is the only GPU-family instance in any enabled region.

## Job files

`jobs/*.toml`: `name`, `requirements` (the hash-locked Linux lock),
`max_minutes` (or `max_hours`; default 4 h), `checkpoint_minutes`,
`grace_seconds` (SIGTERM to SIGKILL on a spot notice), `max_attempts`,
`smoke` (import transformers and run a tiny BERT forward on CUDA before the
job), `disable_os_lifetime` (test hook), `[vars]` (substituted into step args
as `{name}`), `[[steps]]` (`name`, `entry`, `args`, `device`; run in order, a
step done in an earlier attempt is skipped), `[[data]]` (`name`, `env`,
`local_root`, `include` globs; the instance gets the subset under
`$<env>`).

A trainer runs under the runner when it writes its whole state atomically to
`--out/checkpoint.pt`, resumes from it, and exits 0 when done, 75 when stopped
resumably, anything else on failure (design D5, C1-C7).

## Cost

`cost.json` (in `runs/<run-id>/` and pulled locally): per attempt the instance
hours (launch to the termination time EC2 reports; once EC2 no longer lists
the instance, the time `run` saw it gone, else the runner's end in
`status.json`), the spot price history of
its type and AZ over that interval, and root EBS GB-hours at the gp3 list
price. Cross-check a day's total 24-48 h later with Cost Explorer:

```
aws ce get-cost-and-usage --time-period Start=<day>,End=<day+1> --granularity DAILY \
  --metrics UnblendedCost --group-by Type=DIMENSION,Key=USAGE_TYPE
```

(rows whose usage type contains `SpotUsage:g5`).

## Offline hooks

`runner.py` reads `GM457_IMDS` (IMDS base URL), `GM457_SHUTDOWN_CMD`,
`GM457_S3_ROOT` (a directory standing in for the bucket), `GM457_PYTHON` (an
interpreter instead of the uv venv), `GM457_AWS` and `GM457_STATUS_MARKER`.
