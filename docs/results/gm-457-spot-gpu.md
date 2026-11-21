# GM-457: spot GPU training, first real AWS runs (measure slice S22)

Date: 2026-11-19. Account profile `default`, us-east-1 only (us-west-2 not needed). Launcher
`eval/gpu/gpu_train.py` at branch `feat/GM-457-spot-gpu-training`, no production code changed.
Spec: `docs/design/GM-457-spot-gpu-training.md` (D6, D7, D8, section 4 items 22-28).
Instance: g5.xlarge (A10G 24 GB), AMI Deep Learning Base OSS Nvidia Driver GPU (Ubuntu 22.04) 20261113.

## Verdict

| item | result |
|---|---|
| 22 one-command launch end to end, artifacts pulled | pass |
| 23 CUDA forward on A10G, deterministic mode with `CUBLAS_WORKSPACE_CONFIG` | pass |
| 24 parity L vs G vs A within D7 tolerances | pass (after both local controls) |
| 25 resume K1 (FIS spot notice) and K2 (hard terminate) equal U bit for bit, really interrupted | pass, bit-exact |
| 26 success, failure, in-OS lifetime, scheduler-only lifetime leave nothing alive | pass |
| 27 kill-switch Lambda terminates the test instance; budget and subscription read back | pass |
| 28 measured cost of the parity run | $0.0295 (0.0408 instance-hours); Cost Explorer cross-check pending |

Total measured: 1.43 instance-hours, $0.99 by `cost.json` (10 runs), plus about $0.30 FIS
(estimated, $0.10 per action-minute, not metered by `cost.json`) and about $0.02 for the
kill-switch instance (upper-bound estimate, no `cost.json`): about $1.3 against the $10 cap.
`verify-clean` was ok after every scenario and ok at the end.

## setup

`setup --with-fis` created: bucket `gmesh-train-992382475637-us-east-1` (public access block,
SSE-S3, lifecycle), roles `gmesh-train-instance` (+ inline policy, instance profile),
`gmesh-train-scheduler`, `gmesh-train-fis`, security group `gmesh-train` in us-east-1 and
us-west-2. The first `setup` ended with PROBLEM lines (dry-run RunInstances: invalid instance
profile name), IAM propagation lag; a second `setup` after ~10 min printed `no changes` and
the dry run passed (`setup1.txt`, `setup2-idempotent.txt`).

## 22, 23: smoke job

Run `smoke-ff3e996b3313-20261119T0140-s1`: state `done`, exit 0, artifacts pulled (6 files).
Runner smoke log:

```
smoke: torch 2.2.2+cu121 cuda 12.1 transformers 4.46.3 sentence-transformers 3.3.1
smoke: driver 595.112.07
smoke: device NVIDIA A10G
smoke: forward ok on cuda, shape (2, 16, 32), abs sum 828.852417
```

The smoke step runs with `torch.use_deterministic_algorithms(True)` and tf32 off; the runner
exports `CUBLAS_WORKSPACE_CONFIG=:4096:8` to every job (`job_env`), and every CUDA arm below
(parity G, resume U/K1/K2) ran deterministic with it. Cost 0.0475 h, $0.0289.

## 24: parity (D7)

Local controls first (arm L = laptop CPU `--threads 2`, fraction 0.25, 5 epochs):

| control | max dev-MRR diff | relative W deviation | first-hit rank changes | result |
|---|---|---|---|---|
| (i) L `--threads 4` vs `--threads 2` | 0 | 2.18e-06 | 0.0% | within tolerances: pass |
| (ii) L `--seed 12345` vs default seed | 8.4e-03 | 0.887 | 68.2% | exceeds W tolerance 1e-2: pass |

So the tolerances neither are too tight nor too loose to detect a real difference.

AWS run `gm436-parity-...-par` (one launch, both arms G=CUDA and A=instance CPU, `--threads 2`),
dev queries 3,198, epochs 0-5. Tolerances: epoch 0 MRR to 1e-6, MRR diff <= 0.005 at every epoch,
W relative deviation <= 1e-2, rank changes <= 2%.

| pair | epoch-0 MRR diff | max MRR diff, epochs 0-5 | `\|\|W-W_L\|\|/\|\|W_L-I\|\|` | rank changes | result |
|---|---|---|---|---|---|
| L vs G | 0 | 0 | 2.87e-06 | 0.0% | pass |
| L vs A | 0 | 0 | 2.27e-06 | 0.0% | pass |
| A vs G | 0 | 0 | 3.21e-06 | 0.0% | pass |

Dev MRR per epoch (all three arms identical): 0.3080, 0.3136, 0.3229, 0.3200, 0.3246, 0.3291.
W hashes differ (L `bca878a9a084`, G `10a2251c5434`, A `961fba1a0d83`), as float reordering
predicts; the deviation is about 3e-6, four orders below the tolerance and five below the
different-seed control. Cost of the parity run: 0.0408 instance-hours, **$0.0295** (spot
$0.0291 + EBS $0.0004), `cost.json` of `gm436-parity-e4314f42c0d7-20261119T0154-par`.

## 25: resume U / K1 / K2 (D7)

Observation that changed the arm job: the trainer runs ~0.6 s per epoch on the A10G, so the
committed 30-epoch job finishes in ~19 s after boot. A first K1 try on it (run `...T0223-k1`)
was not tested: the FIS interrupt started after the job had finished ("instances are not in the
correct state", $0.14 wasted). The arms below use a copy of `gm436-resume.toml` with
`--epochs 400 --patience 400` (kept in the scratchpad, not committed), identical for U, K1, K2;
the 30-epoch U (`wSha 45b5e36bf625`) is recorded only as a first data point. Interrupt trigger:
the CLI harness polls S3 `ckpt/<id>/gpu/checkpoint.pt` every 10 s and interrupts once its
`epoch >= 3`.

| arm | runs | attempts | interrupt | evidence | `wSha256` | `lastWSha256` | history |
|---|---|---|---|---|---|---|---|
| U | `...-ux` | 1 | none | 400 epochs, `done` | `84e8b11dfac9...` | reference | reference |
| K1 | `...-k1x` | 2 | FIS `aws:ec2:send-spot-instance-interruptions` (PT2M) when uploaded checkpoint was epoch 42 | attempt 1 reason `spot-notice` (notice logged 03:02:20Z, job stopped exit -15, uploaded); attempt-2 log `resumed after epoch 98` | equal | equal | identical (401 entries) |
| K2 | `...-k2x` | 2 | `TerminateInstances`, no notice, at uploaded epoch 41 | CLI log `attempt 1: relaunch (no-status)`; attempt-2 log `resumed after epoch 41` | equal | equal | identical (401 entries) |

Both interrupted arms are **bit-exact** against U on the same GPU model (A10G, g5.xlarge); the
1e-6 fallback was not needed. Notes: K2's attempt-1 `status.json` record has no `reason`
(a killed instance writes no status), so "no-status" is evidenced by the CLI log, not by
status.json. K2's first relaunch failed with "no spot capacity" after 10 retries and was
completed with `run --resume <id>` about 18 minutes later (attempt 2 then ran normally).

## 26: terminate paths (smoke job, `verify-clean` after each)

| scenario | run | what ended the instance | evidence | billed |
|---|---|---|---|---|
| success | smoke `...-s1` | runner exit path | state `done`, exit 0 | 0.0475 h |
| failure exit (`mode=fail`) | `...-tf` | runner exit path | `StateReason: Client.InstanceInitiatedShutdown`, run exit 1, 0.039 h | $0.0258 |
| in-OS lifetime (`mode=hang`, 10 min) | `...-tos` | in-OS `shutdown -h +10` | `Client.InstanceInitiatedShutdown`, watcher reported `expired`, scheduler deleted by reap | $0.1718 (upper bound, see below) |
| scheduler-only (in-OS disabled, `mode=hang`, 10 min) | `...-ts` | EventBridge Scheduler `TerminateInstances` at +20 min | `Client.UserInitiatedShutdown`, `User initiated (2026-11-19 05:14:59 GMT)`, launch 04:54:50Z (20.1 min) | $0.2446 |

The in-OS row's end time is "now" (EC2 no longer listed the instance when the CLI looked), so
0.263 h overstates the real ~0.19 h; it is an upper bound. `verify-clean` was ok after each.

## 27: kill switch (D6)

- `kill-switch-test` with no run id: "no GPU instance in any enabled region" (every region checked).
- Test instance: smoke `mode=hang` (`...-ks`), then `kill-switch-test <run-id>` re-checked it
  was the only GPU instance and invoked `gpu-training-50-kill-gpu` with a synthetic SNS budget
  event: `status 200 {"terminated": ["us-east-1:i-06b18ba12667d4703"]}`. EC2 shows
  `User initiated (2026-11-19 05:24:49 GMT)`, instance `terminated`, schedule reaped.
- The command itself returned 1: its internal `verify-clean` saw the spot request still
  `active` for the already-terminated instance (see findings); the harness's own
  `verify-clean` (which waits for the request to close) was ok.
- Read back, not fired: budget `gpu-training-50` MONTHLY COST $50; notifications ACTUAL > 100%,
  ACTUAL > 80%, FORECASTED > 100%; SNS subscription Protocol `lambda`, endpoint
  `...:function:gpu-training-50-kill-gpu`, `PendingConfirmation: false`; budget action
  `APPLY_IAM_POLICY` (`gpu-training-50-deny-gpu-launch` on user `valentinitc`) at 100% ACTUAL,
  status `STANDBY`, AUTOMATIC: defined and not fired.

## 28: cost (D8)

| run | attempts | instance-hours | $ (`cost.json`) |
|---|---|---|---|
| smoke s1 | 1 | 0.0475 | 0.0289 |
| parity (G + A arms) | 1 | 0.0408 | 0.0295 |
| resume U, 30 epochs (first data point) | 1 | 0.0450 | 0.0407 |
| resume K1 first try (not tested) | 1 | 0.1983 | 0.1435 |
| resume U, 400 epochs | 1 | 0.1017 | 0.0736 |
| resume K1 | 2 | 0.2205 | 0.1443 |
| resume K2 | 2 | 0.1381 | 0.0888 |
| failure exit | 1 | 0.0394 | 0.0258 |
| in-OS lifetime | 1 | 0.2627 | 0.1718 |
| scheduler-only lifetime | 1 | 0.3358 | 0.2446 |
| **sum of `cost.json`** | 12 attempts | **1.43** | **0.99** |
| FIS experiment (estimate, $0.10/action-minute) | | | ~0.30 |
| kill-switch instance (estimate, upper bound) | | ~0.04 | ~0.02 |

About $1.3 total versus D8's ~$1.8 base estimate (boot plus venv install is ~2-3 min per
attempt, shorter than the 8 min assumed) and the $10 cap. Effective spot cost about $0.7 per instance-hour (parity: $0.0291 over 0.0408 h).

## Findings

1. `verify-clean --wait` covers `shutting-down` instances only. A one-time spot request stays
   `active/fulfilled` for up to ~7 min after its instance is terminated, so `verify-clean` (and
   `kill-switch-test`, which ends with it) can report NOT CLEAN for a resource that clears by
   itself. The harness retried `verify-clean` up to 15 min while only spot-request lines were
   outstanding; nothing leaked. Suggested fix (separate slice): wait on spot requests whose
   instance is terminated.
2. `setup` right after creating the instance profile can fail its dry-run (IAM propagation);
   rerunning after a few minutes is enough.
3. `gm436-resume.toml` (30 epochs) finishes before any interrupt can land; the committed job
   needs ~400 epochs for the resume test (follow-up, not edited here).
4. Spot capacity: attempts 1 of K1 first try and K2's attempt 2 got "no spot capacity" in some
   AZs; the launcher retried (see the run logs). K2's relaunch exhausted its retries once.
5. Data: `g-mesh-data/gm456/v0.4-full/mine/{variants,corpora}.toml` were dangling symlinks into
   a removed worktree; repointed to this worktree's `eval/training/` (data outside the repo).
6. Parity measured something: control (ii) moves W by 0.887 and 68% of ranks; L/G/A differ by
   about 3e-6.

## Follow-up

Cross-check the day's total with Cost Explorer 24-48 h later (`aws ce get-cost-and-usage
--time-period Start=2026-11-19,End=2026-11-20 --granularity DAILY --metrics UnblendedCost
--group-by Type=DIMENSION,Key=USAGE_TYPE`, usage types containing `SpotUsage:g5`), expecting
about $0.9-1.0 of spot plus EBS and about $0.30 of FIS.
