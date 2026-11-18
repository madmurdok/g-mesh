# GM-457: resumable training on AWS spot GPU instances (design, release 4.35.0)

Design slice GM-457/S1. No code, no AWS create/modify calls, no spend. The
only AWS calls made were read-only `describe-spot-price-history` and
`describe-instance-type-offerings` (profile `default`, 2026-11-18 ~20:20 UTC).

The consumer is GM-730 (gte-small student full fine-tune, jina LoRA/full
fine-tune with ONNX parity and int8 re-quantisation, reranker distillation
from gte-reranker-modernbert-base). This task builds the launcher, the
instance-side runner, the resume contract and the safety net, and proves them
on the one trainer that exists today: the GM-436 adapter trainer
(`eval/training/gm436/train_adapter.py`). GM-730 adds job recipes, not
launcher code.

## 0. Facts this design builds on

- **Quota:** spot "All G and VT" = 8 vCPU in us-east-1 and us-west-2. On-demand
  G/VT and P = 0. So spot only, and at most 8 vCPU of G-family running at once.
- **Kill switch (exists):** budget `gpu-training-50` ($50/month). At 100%
  actual: SNS -> Lambda `gpu-training-50-kill-gpu` (us-east-1) terminates
  GPU-family instances in all regions, and a budget action attaches a
  deny-all-EC2-launch policy to the IAM user. Budget "actual" data lags by
  hours (AWS refreshes it a few times a day), so it is a backstop, not a
  guard. It also counts the whole month: GM-730 spends from the same $50.
- **Trainer checkpoint (read by symbol):** `save_ckpt` (train_adapter.py:193)
  writes `torch.save` to `<path>.tmp`, fsyncs, `os.replace`s. `rng_state`
  (:201) / `set_rng_state` (:205) cover python, numpy and torch **CPU** RNG
  only. `train` (:211) resumes when `--out/checkpoint.pt` exists, refuses a
  checkpoint whose `configSha256` differs, checkpoints after every epoch
  (params, AdamW state, history, best W, RNG), and draws the epoch's batch
  order from `np.random.default_rng([seed, epoch])`, so kill-and-resume is
  bit-identical to an uninterrupted run at the same `--threads` (GM-436
  dry run: "W sha256 equal after kill+resume"). `--stop-after-epochs N` exits
  `KILLED_EXIT = 75` after epoch N's checkpoint. The trainer is CPU-only:
  no device handling, `torch.load` without `map_location`.
- **Callers (g-mesh, project `g-mesh-wt-gm457`):**
  - `find_callers save_ckpt` -> only `train` (train_adapter.py).
  - `find_references rng_state` -> only `train` (CALLS).
  - `find_references set_rng_state` -> only `train` (CALLS).
  - `find_references` on `train` (id `157afba8…`, after a `nameAmbiguous`
    answer that also listed `R2Temperatures.train` in the test) -> only the
    file-level `__main__` call.
  - All answers carried `provenance: semanticTier absent` (Python semantic
    pass not run yet); these are plain function calls, not method calls
    through a receiver, so the gap does not apply. Non-Python/attribute uses
    by grep (one known symbol set, non-code included):
    `test_gm436_train.py:69,90` use `T.KILLED_EXIT`;
    `run_measure.sh:136` tests for `checkpoint.pt`;
    **`export_adapter.py:99` `torch.load(.../checkpoint.pt)` without
    `map_location`**, which fails on the CPU-only laptop for a checkpoint
    holding CUDA tensors (section 5).
- **Environment (GM-436 section 2):** CPython 3.11.15 (uv-managed), torch
  2.2.2, numpy 1.26.4; transformers 4.46.3 and sentence-transformers 3.3.1
  import cleanly with torch 2.2.2. `requirements.txt` is hash-locked for
  `x86_64-apple-darwin` only, and says the student-route pins "land with
  GM-457".
- **Data the trainer reads:** `$GM456_DATA/v0.4-full/queries.jsonl`
  (119 MB, 23,798 rows: train 20,600 / dev 3,198), `v0.4-full/mine/`
  (270 MB, of which `rankings/` 259 MB), node spans under `v0.4-full`
  (`nodes/` 33 MB, `texts/` 10 MB), and `$GM436_DATA/cache/` (306 MB:
  gte-small 102 MB, jina int8 192 MB, teacher 12 MB). Under 1 GB in all.
- **Spot prices, Linux, $/h** (now = 2026-11-18 20:20 UTC; 7 days = min /
  median / max over the price-change records):

  | type | GPU | vCPU / RAM | us-east-1 now (cheapest AZ) | us-east-1 7 d | us-west-2 now | us-west-2 7 d |
  |---|---|---|---|---|---|---|
  | g5.xlarge | A10G 24 GB | 4 / 16 GB | 0.596 (1f) … 0.894 (1a) | 0.487 / 0.615 / 0.894 | 0.614 (2c) | 0.554 / 0.614 / 0.695 |
  | g5.2xlarge | A10G 24 GB | 8 / 32 GB | 0.696 (1a) … 0.938 (1b) | 0.696 / 0.935 / 1.102 | 0.742 (2b) | 0.615 / 0.756 / 0.843 |
  | g6.xlarge | L4 24 GB | 4 / 16 GB | 0.414 (1b) | 0.372 / 0.611 / 0.748 | 0.543 (2b) | 0.462 / 0.709 / 0.762 |
  | g4dn.xlarge | T4 16 GB | 4 / 16 GB | 0.204 (1d) | n/a | 0.230 (2c) | n/a |

  The ~$0.44/h g5.xlarge figure in the GM-436 history notes is stale: plan
  with ~$0.65/h. All three g5/g6 types are offered in every AZ listed.

## 1. Decisions for the owner

### D1. Instance type and region

| option | benefit | risk |
|---|---|---|
| **a. g5.xlarge default, g5.2xlarge as an allowed override in the same request; us-east-1 primary, us-west-2 fallback (Recommended)** | One A10G (24 GB) is enough for every GM-730 route: gte-small (33M) and jina (137M) full fine-tune fit in 24 GB with AMP, the teacher reranker (149M) scores on it. Uses half the quota, so a stray second instance can still launch. Both sizes have the same GPU, so results are comparable and the resume test stays bit-exact. The fleet request takes the cheaper of the two per AZ (today g5.2xlarge in 1a is cheaper than g5.xlarge in 1a). | 4 vCPU / 16 GB RAM can bottleneck tokenisation or a 1.6 GB jina checkpoint + data in RAM; the override covers it. |
| b. g5.2xlarge only | 8 vCPU, 32 GB RAM, faster data loading | Takes the whole quota (no room for a second instance or a debug box); median 7-day price +50% over g5.xlarge in us-east-1 for the same GPU. |
| c. 2x g5.xlarge in parallel | Two GM-730 arms at once | Whole quota; doubles the burn rate against the $50 cap; nothing in GM-457 needs parallelism. Launcher keeps it possible (`--count` is not built, two `run`s work) but not the default. |
| d. Add g6.xlarge (L4) to the pool | Often cheapest (0.37 min) | Different GPU: results and resume are not bit-comparable across g5/g6. Allowed only by an explicit `--instance-types` override, never mixed within one run id. |

Region: us-east-1 primary (5 AZs for capacity, the kill-switch Lambda lives
there, bucket there). us-west-2 only via `--region us-west-2` when us-east-1
has no capacity; it reads data cross-region from the us-east-1 bucket
(~1.2 GB x $0.02/GB = $0.02 per launch, accepted).

Allocation: one-time `CreateFleet` `type=instant`, `SpotOptions.AllocationStrategy=price-capacity-optimized`,
overrides = allowed types x all AZs of the region. Gets capacity across AZs in
one call instead of looping on `InsufficientInstanceCapacity`.

### D2. Image and Python environment

| option | benefit | risk |
|---|---|---|
| **a. AWS DLAMI "Base OSS Nvidia Driver GPU (Ubuntu 22.04)" for the driver only + uv-managed CPython 3.11.15 + a hash-locked Linux requirements file (Recommended)** | Driver and CUDA runtime preinstalled (boot in ~3 min, no driver build). Python stack is ours and pinned like the laptop: torch 2.2.2, numpy 1.26.4, same transformers / sentence-transformers. The AMI id is resolved once from the public SSM parameter (`/aws/service/deeplearning/ami/x86_64/base-oss-nvidia-driver-gpu-ubuntu-22.04/latest/ami-id`), written into `eval/gpu/config.toml`, and refreshed only on purpose, so runs do not drift. | uv installs ~2.5 GB of torch + nvidia-* wheels per launch (~1-3 min). Mitigation, later if needed: cache the built venv as a tarball in S3 keyed by the requirements sha. The exact SSM parameter path must be confirmed by a read-only `ssm get-parameter` in the code slice. |
| b. DLAMI with PyTorch preinstalled | Zero install time | Ships its own torch (2.x newer than 2.2.2) and numpy 2; not the laptop pin, so parity is between different library versions. Pinning inside its conda env is the same work as (a) plus a large unused env. |
| c. Plain Ubuntu AMI + driver install | Smallest base | Driver install/compile 5-10 min per launch, a new failure surface on every spot relaunch. |
| d. Docker image in ECR | Fully reproducible | Build and push of an ~8 GB image from the Intel Mac on every dependency change; ECR permissions and lifecycle; no gain over (a) for one user. |

Requirements: add `eval/training/gm436/requirements.in` (torch==2.2.2,
numpy==1.26.4, transformers==4.46.3, sentence-transformers==3.3.1) and
compile it twice with `uv pip compile --generate-hashes`:
`requirements.txt` (`--python-platform x86_64-apple-darwin`, regenerated with
the current file as constraint so existing pins stay) and
`requirements-linux-cu121.txt` (`--python-platform x86_64-manylinux_2_28`).
The PyPI Linux torch 2.2.2 wheel is the CUDA 12.1 build (it pulls the
`nvidia-*-cu12` wheels); it needs a driver >= 525, which the DLAMI base
provides. onnx / onnxruntime stay with GM-730.

**CPU wheel vs CUDA wheel parity.** Same torch version, different kernels:
MKL/oneDNN on the Mac's Intel CPU, cuBLAS on the A10G. Reduction order differs,
so results agree to float tolerance, never bit for bit. Rules the runner
enforces for comparability: `torch.backends.cuda.matmul.allow_tf32 = False`
(the 2.2 default, set explicitly so a later default flip cannot change
results), `torch.use_deterministic_algorithms(True)` (the trainer already
sets it) with `CUBLAS_WORKSPACE_CONFIG=:4096:8` exported before torch starts
(without it, deterministic mode raises on the first cuBLAS matmul). GM-730
may turn on bf16/TF32 for speed; then its own parity tolerance applies.
Bit-exact claims are only made between runs on the same device type (resume
test, D7).

### D3. Launch tool

| option | benefit | risk |
|---|---|---|
| **a. Our own Python CLI on boto3 (`eval/gpu/gpu_train.py`, run with `uv run`, boto3 pinned in a PEP 723 header) (Recommended)** | Exactly the resources we need, least-privilege IAM we can read, every request testable offline with moto / botocore Stubber, no extra running infrastructure. One command: `uv run eval/gpu/gpu_train.py run eval/gpu/jobs/<job>.toml [--set k=v]` uploads code and data, launches, watches, relaunches after a spot interruption, pulls artifacts and verifies termination. | We own the relaunch loop. It runs on the laptop under `caffeinate -i -s`; if the laptop sleeps, an interrupted job waits (nothing is lost, the checkpoint is in S3, and `run --resume <run-id>` continues). |
| b. SkyPilot managed spot jobs | Auto-recovery without the laptop, multi-region/cloud search | Needs a controller VM (an always-on CPU instance while jobs run), creates its own broad IAM roles, frequent releases (pin drift), a large dependency, and its recovery is hard to stub offline. The 2-minute-notice hook and our checkpoint contract still have to be written. |
| c. Bare `aws` CLI shell script | No Python | Spot fleet request JSON, waiting, relaunch and cost math in bash; poor testability (GM rules want each behaviour pinned by a test). |

### D4. S3 layout and IAM

Bucket `gmesh-train-<account-id>-us-east-1` (account id read at setup, not
hard-coded). Block Public Access on, SSE-S3, versioning off, lifecycle:
abort incomplete multipart uploads after 1 day; expire `runs/*/ckpt/` after
30 days. Layout:

```
code/<git-sha>.tar.gz                     git archive of a clean commit (dirty tree refused)
env/<requirements-sha>/requirements.txt   the lock file the runner installs
data/<dataset>/<data-sha>/...             synced subsets + MANIFEST.json (path, size, sha256)
models/<hf-repo>@<revision>/...           GM-730: pinned HF snapshots (optional cache)
runs/<run-id>/manifest.json               job spec, git sha, data sha, AMI id, instance types, max lifetime
runs/<run-id>/status.json                 attempts: instance id, type, AZ, price, start/end, exit, reason
runs/<run-id>/ckpt/checkpoint.pt          last checkpoint (+ checkpoint.prev.pt)
runs/<run-id>/out/...                     everything else the job wrote to --out (result.json, ranks)
runs/<run-id>/logs/attempt-<n>.log        runner + trainer stdout/stderr, uploaded every 60 s and at exit
```

`run-id` = `<job>-<config-sha12>-<utc yyyymmddThhmm>`. Data upload is
content-addressed: the CLI hashes the local subset, uploads only when
`data/<dataset>/<data-sha>/MANIFEST.json` is absent, and the runner verifies
every file's sha256 after download (a mismatch is a failure, not a retry).
The data subset per job is listed in the job file; for the adapter:
`v0.4-full/{queries.jsonl, manifest.json, targets.jsonl, mine/, nodes/, texts/}`
and `gm436/cache/vec/<variant>/`. The offline test proves the list is
sufficient by running the trainer against a directory that holds only it.

IAM, created by `gpu_train.py setup` (idempotent: get-or-create, and for a
policy compare the document and create a new default version only when it
differs; a second run prints "no changes"):

- Role `gmesh-train-instance` + instance profile of the same name. Trust:
  `ec2.amazonaws.com`. Inline policy: `s3:GetObject` on `code/*`, `env/*`,
  `data/*`, `models/*`; `s3:GetObject`, `s3:PutObject` on `runs/*`;
  `s3:ListBucket` with `s3:prefix` limited to `runs/` and `data/`. Nothing
  else: no EC2 API (termination is by OS shutdown), no IAM.
- Role `gmesh-train-scheduler` for the cloud-side lifetime cap (D6). Trust:
  `scheduler.amazonaws.com`. Policy: `ec2:TerminateInstances` with condition
  `aws:ResourceTag/gmesh-train = 1`.
- Security group `gmesh-train` in the default VPC: no ingress, all egress
  (S3, PyPI, HF). No SSH key pair. Debug access, if ever needed, is an opt-in
  `--ssm` flag that attaches `AmazonSSMManagedInstanceCore`; off by default.
- The IAM user running the CLI needs `iam:PassRole` on `gmesh-train-instance`
  (and on the scheduler role for `scheduler:CreateSchedule`). `setup` checks
  this with a dry-run `CreateFleet`/`RunInstances` (`DryRun=True`) and
  reports a missing permission instead of failing mid-launch. A launch error
  `UnauthorizedOperation` with an explicit deny is reported as "budget kill
  switch is active (deny-all-EC2 policy attached)".

Instances: IMDSv2 required (`HttpTokens=required`, hop limit 1), root gp3
volume sized to the AMI's minimum + 20 GB, `DeleteOnTermination=true`,
tags `gmesh-train=1`, `gmesh-run=<run-id>`, `gmesh-attempt=<n>`,
`gmesh-expires=<utc iso>` on instance and volume.

### D5. Spot interruption and the resume contract

Instance side (`eval/gpu/runner.py`, stdlib only, started by user data):

1. Poll IMDSv2 `/latest/meta-data/spot/instance-action` every 5 s (404 = no
   notice). A notice -> send SIGTERM to the job's process group, wait up to
   60 s (`--grace`), then SIGKILL. Upload the checkpoint and logs, write the
   attempt as `interrupted` in `status.json`, `shutdown -h now`.
   Rebalance recommendations are logged only.
2. Every 30 s, if `--out/checkpoint.pt` changed (mtime + size), copy it to
   `runs/<id>/ckpt/` (previous one rotated to `checkpoint.prev.pt`). The
   trainer replaces the file atomically, so an upload never reads a partial
   file (an open handle keeps the old inode).
3. Before starting the job, download `runs/<id>/ckpt/checkpoint.pt` into
   `--out` if present; the trainer's existing "resume if `checkpoint.pt`
   exists" path does the rest.

**Trainer contract (what any GM-730 trainer must do to run under the runner):**

| # | rule | adapter today |
|---|---|---|
| C1 | Writes its whole state to `--out/checkpoint.pt` atomically (tmp + fsync + replace) | yes (`save_ckpt`) |
| C2 | If `--out/checkpoint.pt` exists, resumes from it; refuses one written for another config | yes (`configSha256`) |
| C3 | Checkpoint tensors are CPU tensors (portable to the laptop); loads use `map_location` | **no**: params/AdamW state would be CUDA tensors |
| C4 | RNG state includes CUDA generators when CUDA is in use | **no**: CPU only |
| C5 | Exit codes: 0 done, 75 stopped resumably, anything else = failure (no relaunch) | yes (`KILLED_EXIT`) |
| C6 | Checkpoints at least every `checkpoint_minutes` (job file, default 10) **or** on SIGTERM saves at the next step boundary within 60 s and exits 75 | per epoch (seconds on GPU); SIGTERM -> killed, resumes from the last epoch, which is still bit-exact because the epoch order depends only on (seed, epoch) |
| C7 | Records the device in the state and refuses to resume on a different device unless `--allow-device-change` | **no** |

So the adapter needs no signal handler. GM-730's step-based trainers (epochs
of minutes) implement C6 with the shared helper (`StopOnSignal`, below) and a
data cursor (epoch permutation + offset) in the checkpoint.

Shared helpers move to `eval/training/ckpt.py` (g-mesh shows `train` is the
only caller, so the move is safe; `train_adapter.py` keeps `KILLED_EXIT`
importable for its tests): `save_ckpt(path, obj)` (moves tensors to CPU
recursively, then the existing atomic write), `load_ckpt(path)`
(`map_location="cpu"`), `rng_state()` / `set_rng_state(s)` (add
`torch.cuda.get_rng_state_all()` when available; restoring a state with a
`cuda` key on a host without CUDA is an error unless the device-change flag
is set), `StopOnSignal` (SIGTERM sets a flag the loop checks), `KILLED_EXIT`.

Laptop side (`gpu_train.py run/watch`): poll `status.json` and the instance
state every 30 s (one background process, not an agent loop). On an
`interrupted` attempt, or an instance that terminated with no status
(hard kill, hung OS), launch attempt n+1 with the same run id, up to
`max_attempts` (job file, default 5). On `failed` or `done`: no relaunch,
pull `runs/<id>/out` and `logs` to
`~/Projects/ClaudeProjects/g-mesh-data/gpu-runs/<run-id>/`, then verify
termination (D6).

### D6. Auto-terminate and the safety net

Five independent layers, cheapest first:

1. **Runner exit path.** User data is a bash script with `trap finish EXIT`;
   `finish` uploads logs and status and runs `shutdown -h now` on success,
   failure, exception or signal. `InstanceInitiatedShutdownBehavior=terminate`
   is set explicitly (a one-time spot instance terminates on shutdown anyway;
   the explicit flag guards a future on-demand path).
2. **In-OS max lifetime.** The first line of user data, before any download:
   `shutdown -h +<max_minutes>` (job file `max_hours`, default 4; tests use 10
   min). It fires even if Python hangs.
3. **Cloud-side max lifetime.** After launch the CLI creates an EventBridge
   Scheduler one-time schedule `at(<expires>)` + 10 min that calls
   `ec2:TerminateInstances` on the instance (universal target,
   `ActionAfterCompletion=DELETE`). It fires when the OS is hung or the
   laptop is asleep. Cost: free tier (14M invocations/month).
4. **Laptop reaper.** `gpu_train.py reap` terminates every instance tagged
   `gmesh-train=1` whose `gmesh-expires` is in the past, in both regions,
   and deletes leftover schedules. `run` calls it at start and end.
5. **Budget kill switch** (exists, $50 actual, hours of lag). Backstop only.

"No instance survives" is verified, after every AWS scenario in S6, by
`gpu_train.py verify-clean`: in us-east-1 and us-west-2, zero instances
tagged `gmesh-train=1` in `pending|running|stopping|stopped`, zero
G/P-family instances in those states at all, zero open/active spot
requests, zero `available` EBS volumes tagged `gmesh-run`, zero leftover
`gmesh-*` schedules. It waits up to 5 min for `shutting-down` to finish and
fails otherwise. The scenarios (D7, D8): success, injected failure
(`exit 1`), hung job hitting the in-OS lifetime (`sleep`), in-OS watchdog
disabled so only the scheduler can stop it, and the kill-switch Lambda.

Kill switch verification without spending $50: invoke the existing Lambda
with a synthetic SNS budget event (`aws lambda invoke`, payload shaped like
the budget notification) while exactly one tagged test instance runs, then
`verify-clean`. The Lambda terminates every GPU-family instance in all
regions, so it runs only when no other GPU job is up (the CLI checks first).
The budget itself is verified by read-back: `budgets describe-budget` and
`describe-notifications-for-budget` show the 100% actual threshold and the
SNS subscriber, `sns get-subscription-attributes` shows the Lambda
subscription confirmed. The deny-launch budget action is not fired (it would
block the IAM user until removed by hand); its definition is read back with
`budgets describe-budget-action`.

### D7. Parity run and kill-mid-run resume test

**Parity job** (`eval/gpu/jobs/gm436-parity.toml`): the adapter trainer, R1,
full form on gte-small-structured (d = 384, the smallest cache),
`--fraction 0.25 --epochs 5 --patience 5 --batch 512 --lr 1e-3 --lam 0.001
--tau 0.05 --no-train-metric`, default seed. Minutes on the laptop CPU,
seconds per epoch on the GPU, exercises the whole data path and the dev
metric.

| option | benefit | risk |
|---|---|---|
| **a. Adapter R1 parity, local CPU vs AWS CUDA, plus an AWS CPU arm in the same launch (Recommended)** | The trainer exists and is deterministic; the third arm separates "different host CPU" from "CPU vs GPU". | Proves the launcher and the CUDA path of a small model, not a transformer: HF loading, AMP and tokenisation are first exercised by GM-730. |
| b. A new tiny gte-small fine-tune (200 steps) as the parity job | Exercises transformers on CUDA, closer to GM-730 | New trainer code in an infra task, with its own determinism questions; GM-730's design owns that recipe. |

Mitigation for (a)'s gap: the runner's smoke step on every launch imports
transformers and sentence-transformers and runs one forward pass of a
bundled tiny BERT config (random weights, no download) on CUDA, logging
`torch.cuda.get_device_name()`, driver and CUDA versions.

Metrics and tolerances (pre-registered; arms: L = laptop CPU `--threads 2`,
G = AWS CUDA, A = AWS CPU):

- Per-epoch dev MRR (3,198 dev queries): `|MRR_G - MRR_L| <= 0.005` at every
  epoch 0-5. Epoch 0 (identity W) must match to 1e-6: same vectors, no
  training, so a difference there is a data or code bug, not floats.
- Final W: relative deviation `||W_G - W_L||_F / ||W_L - I||_F <= 1e-2`
  (the update, not W, is the scale, since W starts at I).
- First-hit rank changes: at most 2% of dev queries differ in rank.
- Why these: float reordering perturbs W at ~1e-6 relative per step and
  AdamW amplifies it over ~200 steps; MRR moves only when a perturbation
  swaps two near-tied candidates.
- **Controls that tell the arms apart** (run locally, before AWS): (i) noise
  floor, L with `--threads 4` vs `--threads 2` must be **within** the
  tolerances (else they are too tight); (ii) a different `--seed` (different
  batch order) must **exceed** at least the W tolerance (else they are too
  loose to detect a real difference). If (ii) fails, the measure slice
  reports "parity measured nothing" and stops before AWS.

**Kill-mid-run resume test** (on AWS; arms on the same instance type,
g5.xlarge pinned with `--instance-types g5.xlarge`):

- U: uninterrupted AWS run, `--fraction 1.0 --epochs 30 --patience 30`
  (no early stop, long enough to interrupt: ~1-3 min of epochs after setup).
- K1 (spot notice path): same job; once `runs/<id>/ckpt/` holds epoch >= 3,
  the CLI starts an AWS FIS experiment with action
  `aws:ec2:send-spot-instance-interruptions` (`durationBeforeInterruption`
  = PT2M) on the instance. Runner sees the notice, stops the trainer,
  uploads, shuts down; the CLI relaunches attempt 2, which resumes.
- K2 (hard kill, no notice): `TerminateInstances` from the CLI at the same
  point; attempt 2 resumes from the last uploaded checkpoint.
- Pass: K1 and K2 final `wSha256` and `lastWSha256` equal U's, and the
  `history` arrays are identical. Expected bit-exact (same GPU model,
  deterministic algorithms, epoch order independent of the kill).
  Fallback if CUDA determinism turns out weaker than CPU: relative W
  deviation <= 1e-6 and identical history ranks; this fallback needs the
  owner's approval in the measure report, it is not silent.
- "It was actually interrupted": `status.json` has >= 2 attempts, attempt 1
  reason `spot-notice` (K1) / `no-status` (K2), and the attempt-2 log
  contains `resumed after epoch <n>` with n >= 3. Without these the run
  counts as not tested.
- Offline twin (laptop, tests slice): the same three arms on CPU with the
  fixture data, the "kill" being SIGTERM from a fake runner mid-epoch.

FIS needs a role (`gmesh-train-fis`, trust `fis.amazonaws.com`,
`ec2:SendSpotInstanceInterruptions` + `ec2:DescribeInstances` on tagged
instances), created by `setup --with-fis`. Cost: $0.10 per action-minute.

### D8. GPU-hour and cost estimate for S6

Per launch: boot ~3 min, venv install ~2-3 min, data pull < 1 min, smoke
< 1 min; so ~8 min of overhead billed per attempt.

| scenario | launches | billed hours | $ at 0.65/h |
|---|---|---|---|
| smoke (tiny job, end-to-end) | 1 | 0.20 | 0.13 |
| parity G + A arms (one launch) | 1 | 0.30 | 0.20 |
| resume U | 1 | 0.25 | 0.16 |
| resume K1 (2 attempts) + FIS | 2 | 0.45 | 0.30 + 0.20 FIS |
| resume K2 (2 attempts) | 2 | 0.40 | 0.26 |
| failure exit path | 1 | 0.15 | 0.10 |
| in-OS lifetime (10 min) | 1 | 0.25 | 0.16 |
| scheduler lifetime (in-OS disabled) | 1 | 0.30 | 0.20 |
| kill-switch Lambda | 1 | 0.15 | 0.10 |
| **total** | **11** | **~2.5** | **~$1.8** |

With 2x for retries and debugging: ~5 GPU-hours, **~$4**. Plus S3 (~2 GB,
< $0.10/month) and cross-region reads (cents). The terminate checks need no
GPU and could run on g4dn.xlarge ($0.20/h), saving ~$0.30: not worth a
second AMI path. Brief cap for the measure slice: stop and report at $10
measured; the $50 kill switch stays far away. GM-730's budget starts from
whatever S6 spends this month.

**How cost is measured** (acceptance "measured cost"): the CLI records each
attempt's launch and termination time (`describe-instances` state
transition) and the spot price that applied
(`describe-spot-price-history` for that type/AZ over the interval), plus
EBS GB-hours, and writes `runs/<id>/cost.json`. After 24-48 h the measure
report cross-checks the day's total with Cost Explorer
(`ce get-cost-and-usage`, filter `UsageType` contains `SpotUsage:g5`;
$0.01 per request). Per-run Cost Explorer attribution would need the
`gmesh-run` cost allocation tag activated (a billing write, owner decision,
not needed for the acceptance).

## 2. Open questions for the owner

1. D1-D7 recommendations as stated?
2. Python stack on the instance: also pin transformers 4.46.3 and
   sentence-transformers 3.3.1 now (so GM-730 starts from a proven GPU env),
   or leave all student pins to GM-730?
3. Firing the kill-switch Lambda with a synthetic event during S6
   (terminates all GPU instances in all regions): OK, or read-back only?
4. Resume pass criterion: bit-exact `wSha256`, with the relative-1e-6
   fallback only by your approval in the report?

## 3. Edit map for the code slice

New files (all under `eval/gpu/` unless noted):

| file | contents |
|---|---|
| `gpu_train.py` | PEP 723 header (python 3.11, `boto3==` pinned, `tomli` not needed on 3.11). Subcommands: `setup [--with-fis] [--ssm]` -> `ensure_bucket`, `ensure_lifecycle`, `ensure_role(name, trust, policy)`, `ensure_instance_profile`, `ensure_security_group`, `check_launch_permissions` (DryRun). `run <job.toml> [--set k=v] [--region] [--instance-types] [--resume <run-id>] [--detach]` -> `load_job`, `run_id_of`, `upload_code` (refuses a dirty tree), `sync_data` (content-addressed, `MANIFEST.json`), `write_manifest`, `launch_attempt` (`CreateFleet` instant, tags, IMDSv2, user data from `user_data()`), `schedule_terminate`, `watch` (relaunch policy), `pull`, `record_cost`. `status <run-id>`, `pull <run-id>`, `reap`, `verify-clean`, `interrupt <run-id> --fis|--terminate` (test hook). Pure functions (`run_id_of`, `relaunch_decision(status)`, `user_data(...)`, `fleet_request(...)`, `cost_of(intervals, prices)`) kept separate from boto3 calls for offline tests. |
| `runner.py` | Instance side, stdlib only (uses `aws` CLI from the AMI for S3): `imds_token`, `spot_notice()`, `fetch_inputs` (code, env lock, data + sha check, checkpoint), `setup_env` (uv venv), `smoke()`, `run_job` (process group, env `CUBLAS_WORKSPACE_CONFIG`), `upload_loop`, `on_notice`, `finish(exit_code, reason)` -> `status.json`. Shutdown command and IMDS endpoint injectable by env (`GM457_SHUTDOWN_CMD`, `GM457_IMDS`) for offline tests. |
| `user_data.sh.tmpl` | `shutdown -h +N` first, `trap finish EXIT`, install uv, run `runner.py`. |
| `config.toml` | bucket name pattern, pinned AMI id per region, default instance types, regions, `max_attempts`. |
| `jobs/gm436-parity.toml`, `jobs/gm436-resume.toml`, `jobs/smoke.toml` | job specs: entry script + args, requirements lock, data subset, `max_hours`, `checkpoint_minutes`, device. `smoke.toml` has `mode = ok|fail|hang` for the terminate scenarios. |
| `iam/*.json` | trust and inline policy documents (instance, scheduler, FIS). |
| `README.md` | the one-command launch, setup, verify-clean, cost. |
| `test_gm457_launcher.py`, `test_gm457_runner.py` | tests slice (below). |
| `eval/training/ckpt.py` | `save_ckpt`, `load_ckpt`, `rng_state`, `set_rng_state`, `StopOnSignal`, `KILLED_EXIT` (D5). |
| `eval/training/gm436/requirements.in`, `requirements-linux-cu121.txt` | D2. |

Changes:

| file | where | change |
|---|---|---|
| `eval/training/gm436/train_adapter.py` | imports (:30-49) | import the helpers from `eval/training/ckpt.py`; keep `KILLED_EXIT` name |
| same | `save_ckpt` :193-198, `rng_state` :201-202, `set_rng_state` :205-208 | delete (moved) |
| same | `r1_loss` :137-142, `kd_loss` :170-175 | build the `-inf` / zero constants on the input's device (or use Python scalars) so CUDA tensors work |
| same | `train` :211-313 | `device = torch.device(a.device)`; on cuda: `os.environ.setdefault("CUBLAS_WORKSPACE_CONFIG", ":4096:8")` before the first CUDA call, `allow_tf32 = False`; `model.to(device)`, `Q.to(device)`, `batch_tensors`/`kd_tensors` results `.to(device)` at the call sites; `matrix().detach().cpu().numpy()` (two sites); `load_ckpt`; store `device` in `state`, refuse mismatch (C7); `set_num_threads` unchanged (CPU side still used by `dev_eval`) |
| same | `parse` :338-372 | `--device {cpu,cuda}` default `cpu`; `--allow-device-change`. Not added to `config_of` (keeps existing config hashes and GM-436 checkpoints valid) |
| `eval/training/gm436/export_adapter.py` | :99 | `torch.load(..., map_location="cpu")` (or `ckpt.load_ckpt`) |
| `eval/training/gm436/requirements.txt` | whole file | regenerated from `requirements.in`, existing pins as constraints |
| `.gitignore` | | nothing new (runs pull into `g-mesh-data`, outside the repo) |

Read for context in the code slice: `train` (train_adapter.py:211-313),
`Base.__init__` (:77-98), `parse` (:338-372), `export_adapter.py:90-105`,
`cache.gm436_root` (cache.py:65) and `common.version_dir` (common.py:43) for
the data env vars, `test_gm436_train.py:40-95` for the subprocess test
pattern.

## 4. Behaviours the tests slice must pin

Offline (moto / Stubber / fake IMDS on localhost / subprocess, no AWS):

1. `setup` creates bucket, roles, profile, SG on an empty moto account, and a second `setup` changes nothing.
2. Instance policy grants only the listed S3 actions on the listed prefixes, no `ec2:*`, no `iam:*`.
3. `fleet_request` sets spot one-time, `InstanceInitiatedShutdownBehavior=terminate`, IMDSv2 required, hop limit 1, `DeleteOnTermination`, all four tags, instance profile, allowed types only.
4. `user_data` puts `shutdown -h +<max_minutes>` before any download and installs `trap ... EXIT`.
5. `upload_code` refuses a dirty tree and uploads `code/<sha>.tar.gz` once.
6. `sync_data` uploads only when the manifest is absent; the runner fails on a sha mismatch.
7. The adapter job's data subset is sufficient: the trainer runs on a directory holding only it (fixture data).
8. `relaunch_decision`: interrupted -> relaunch; no-status termination -> relaunch; failed / done -> stop; attempt limit -> stop.
9. Runner: a fake IMDS instance-action -> job gets SIGTERM, checkpoint and logs uploaded, status `interrupted`, shutdown command called.
10. Runner: job exit 0 -> `done`; exit 1 -> `failed`; exception in the runner -> shutdown still called.
11. Runner: a checkpoint present under `runs/<id>/ckpt/` is placed in `--out` before the job starts.
12. Runner uploads a changed `checkpoint.pt` and rotates the previous one; never uploads `.tmp`.
13. Kill (SIGTERM mid-epoch) + resume of the adapter on CPU ends with the same `wSha256` / `lastWSha256` / history as an uninterrupted run.
14. `save_ckpt` writes only CPU tensors; `load_ckpt` loads on a CPU-only host.
15. `rng_state` round-trips; a `cuda` key on a CUDA-less host is refused without the device-change flag.
16. A checkpoint written with one `--device` is refused by a run with another (C7).
17. `--device` does not change `configSha256` (existing GM-436 checkpoints still resume).
18. `export_adapter` loads a checkpoint containing tensors saved from another device (simulated with `map_location`).
19. `reap` terminates only tagged, expired instances; `verify-clean` fails on a running tagged instance and on an `available` tagged volume.
20. A launch rejected by an explicit deny is reported as the budget kill switch.
21. `cost_of` integrates price changes over an interval correctly (piecewise prices).

Only on AWS (measure slice S6, one background script, `verify-clean` after each):

22. One-command launch end to end (smoke job) and artifacts pulled back.
23. CUDA path: smoke forward pass on the A10G; deterministic mode works with `CUBLAS_WORKSPACE_CONFIG`.
24. Parity L vs G vs A within D7 tolerances (after the two local controls pass).
25. Resume K1 (FIS spot notice) and K2 (hard terminate) equal U bit for bit, and were really interrupted.
26. Success, failure, in-OS lifetime and scheduler-only lifetime each end with no surviving resource.
27. Kill-switch Lambda (synthetic event) terminates the test instance; budget and subscriptions read back as configured.
28. Measured cost of the parity run recorded (`cost.json`), cross-checked against Cost Explorer 24-48 h later.

## 5. Risks and must-confirm items

- **CUDA tensors in checkpoints** would break `export_adapter.py` and any
  laptop-side read; C3 (CPU tensors in, `map_location` out) fixes it. Pinned
  by tests 14 and 18.
- **CUDA determinism.** Deterministic algorithms + workspace config are
  expected to make the adapter bit-exact on one GPU model; not yet observed.
  The fallback criterion needs owner approval (open question 4).
- **Kill switch lag.** Budget actual data lags hours; layers 1-4 of D6 are
  the real guard. Pinned by scenario 26.
- **Spot capacity.** g5 spot can be unavailable in an AZ for hours; the fleet
  request spans all AZs and both g5 sizes; `--region us-west-2` is the manual
  fallback. An attempt that cannot get capacity is not billed.
- **Laptop-driven relaunch.** Interrupted jobs wait while the laptop sleeps;
  run under `caffeinate -i -s` (memory note), state is safe in S3.
- **Must confirm in the code slice (read-only calls):** the DLAMI SSM
  parameter path and its driver version (>= 525); that `price-capacity-optimized`
  instant fleets honour the 8-vCPU quota as expected; that the
  EventBridge Scheduler universal target for `ec2:terminateInstances` is
  available in both regions.
