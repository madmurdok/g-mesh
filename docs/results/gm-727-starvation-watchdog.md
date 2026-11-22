# GM-727/S13: starvation watchdog, measured

S13 numbers below were taken at load 24-301 (another session). The quiet-machine
re-measure (S14) is the last section; where they differ, trust S14.

Binary: `target/release/g-mesh` built from this worktree at c3a55e38 (never the
installed MCP copy). Harness: GM-725's `churn.py` (16 processes creating and
deleting 500 files in a loop), `plugroot` and a driver adapted from GM-725's
`driver.py` (MCP shim, `find_definition` by name polled every 1 s, daemon log
followed with timestamps). Real clock, real FSEvents, no injectable clock.
Scripts live in the S13 scratchpad and are not committed.

## Machine state

The machine was shared with another session throughout. `uptime` load
(1 min) at arm start / end, and the whole-driver `/usr/bin/time -p`:

| Arm | load start | load end | time -p real / user / sys (driver + 16 churners) |
|---|---|---|---|
| ctl_small (no churn) | 65.7 | 24.1 | 142.4 / 0.50 / 0.52 |
| churn_small | 24.1 | 166.1 | 87.0 / 18.1 / 207.0 |
| churn_big (50k files) | 133.0 | 301.9 | 339.1 / 25.7 / 240.2 |

The churn arms are sys-bound by the churners themselves, so absolute times in
them carry that load. Starvation was real: during churn 0 `fs_event` lines were
logged for the edited files until the churn stopped (churn_big: the stream
delivered `e2.rs` only at 322.97 s, after which "delivers again" was logged).

## Criterion 1: edit made while starved is seen by name

Three edits per arm, one file each, 3 s apart; time from the edit to the first
`find_definition` by name that returns it.

| Arm | edit-visible latency (s) |
|---|---|
| no-churn control (small) | 1.03, 1.03, 1.03 (watcher event, `fs_event` logged each time) |
| churn, small project (3 files) | 11.23, 12.26, 12.25 |
| churn, 50k-file project | 7.86, **50.49**, 4.11 (last one: event delivered, see below) |

Small project: met. Each is under N + sweep = 15 s; sweeps ran at 27.6, 43.0,
58.1, 72.9 s of the run (about 15 s apart, 19-284 ms each).

50k project: **not met for the second edit.** Sweeps took 9.0 s and 9.2 s (see
criterion 2), and the second sweep came about 44 s after the first ended
because of the 3x-sweep-duration gap (27 s) plus a 15 s period. Edit 1 landed
before the second sweep started and was seen in 7.9 s; edit 2 landed just after
a sweep and waited 50 s. At this sweep cost the bound is N + 3 x sweep, not N.
The third edit (4.1 s) was seen through the recovered stream, not a sweep.

Episode line in the churn arms: `watcher starved: no event for 20 s [40 s on the
50k arm, counted from the last signal before churn] and the canary is overdue -
sweeping every 15 s`. M4: the canary went unheard under churn (episode line
present) and was heard in the control (no episode line).

## Criterion 2: sweep cost at 50k files

Synthetic tree: 50,000 one-line `.rs` files (GM-727/S1 `mktree.py`) plus 20,000
git-ignored files in `target/`. Initial index took 154 s at load 70-130.

Daemon log, forced `Starved` sweeps (each reindexed the edited file, so the
line is logged): `reconciled 50004 files ... in 9049 ms` and `... in 9203 ms`
(2 sweeps, not 3: a sweep with no change logs nothing, and only two ran with a
change). Load at the time: 71.9 at window start, 301 at window end, 8 cores.

Against N = 15 s: 9 s is 60% of N and above the design's 1 s threshold ("if
above about 1 s, revisit N or the gap factor"). The S1 estimate was 0.5-0.7 s
at load 10-35. This run cannot say how much of the 9 s is load: no per-sweep
`user`/`sys` was recorded (the daemon is a separate process), the daemon's CPU
over the 88.6 s window was 61 s (it includes sweeps, post-index work and the
reindexes, so it cannot be split), and the load was 70-300. The number is an
upper bound under heavy contention, not the sweep's cost; I did not retry in
this load. A quiet-machine repeat, with the daemon's `user`/`sys` read around
one forced sweep, is still owed for M1. M1 is therefore **not confirmed**.

## Criterion 3: no sweep while events are delivered

- No-churn control, 3 edits plus 120 s idle (window 129 s): daemon log has 3
  `fs_event` lines, no `watcher starved` line, no `reconciled` line, no
  "delivers again" line, so 0 sweeps. Daemon CPU over the window: 1.83 s.
  (M3's probe count is not logged, so "about 12 probes" is not checked.)
- Normal edits in that control: each edit arrived as an `fs_event` within
  about 1 s and no episode started.
- The log shows episode start, sweeps and episode end only in the churn arms
  (churn_big: "delivers again after 111 s: 2 sweeps, 4 files reindexed").

## Not measured

- Quiet-machine sweep cost (M1), see above.
- M5 (plugin `fileChanged` requests per edited file).
- Probe count in an idle daemon (M3).

## S14: quiet-machine re-measure (50k files)

Same binary (c3a55e38), same harness plus a load gate (1-min load < 4 for 60 s
before each window; waits were 60, 991, 806, 841 s) and a 0.5 s `ps utime/stime`
sampler on the daemon and its plugin. Machine: 8 cores, window start load
3.2-3.6 in every arm. The churners themselves push load to 40-90 inside a churn
arm, so churn-arm sweeps are not quiet; the quiet sweep is the no-churn restart.
Scripts live in the S14 scratchpad, not committed.

### Sweep cost (M1)

Quiet, no churn: restart on the indexed 50k project; the start reconcile is the
same walk/reconcile function as a `Starved` sweep (scope differs only in the log
guard). Load 3.1-3.8, `uptime` recorded per run.

| Run | real (log) | user | sys | note |
|---|---|---|---|---|
| restart 0 | 20855 ms | 1.04 s | 1.07 s | first walk after the arm's edits, 3 reindexed; cold cache, outlier |
| restart 1 | 685 ms | 0.47 s | 0.52 s | 0 reindexed |
| restart 2 | 590 ms | 0.45 s | 0.48 s | 0 reindexed |

Warm quiet sweep: about 0.6-0.7 s real, about 1 s CPU (user+sys), which matches
the S1 estimate (0.5-0.7 s) and is just under the design's 1 s threshold. The
20.9 s cold first run costs the same CPU (2.1 s) and is waiting on I/O.

Forced `Starved` sweeps under churn (16 churners, load rising 3-6 at the first
sweep to 30-90 later; per-sweep user/sys from the sampler):

| Arm | sweep real (ms) | user / sys (s) |
|---|---|---|
| churn 1 | 5802, 6110, 7460 | 1.18/0.95, 1.71/1.11, 2.39/1.52 |
| churn 2 | 4746, 7404, 4386 | 0.51/0.68, 1.88/1.40, 2.15/1.16 |
| churn 3 | 3713, 6677, 6266, 4272 | 0.56/0.75, 2.19/1.28, 2.41/1.39, 1.72/1.12 |

10 sweeps: 3.7-7.5 s real for 1.2-3.8 s CPU, so the sweep is CPU-light but
slowed by the churners competing for the disk and cores. S13's 9.0/9.2 s were
the same effect at higher load. A sweep during starvation costs about 4-7 s at
50k, not 0.6 s.

### Criterion 1 at 50k (M2)

Edit-visible latency (s), three edits per arm, 3 s apart:

| Arm | load at window start | latencies |
|---|---|---|
| no-churn control | 3.24 | 0.67, 0.35, 1.90 (watcher events, no episode line, no sweep) |
| churn 1 | 3.55 | 2.29, 30.11, 34.83 |
| churn 2 | 3.08 | 26.28, 34.19, 6.12 |
| churn 3 | 3.25 | 26.40, 34.23, 37.27 |
| S13 churn (load 72-301) | 72 | 7.86, 50.49, 4.11 |

Against N = 15 s: **not met** if the criterion is "seen within about N + one
sweep" (about 20-22 s): 6 of 9 edits took 26-37 s. Against the 3x gap rule,
bound = N + 3 x sweep: with sweeps of 4.4-7.5 s that is 28-37 s, and the worst
edit (37.27 s) sits at the bound for the slowest sweep (7.46 s: 37.4 s). So
the observed worst case matches N + 3 x sweep and is at the edge, not
under it. Cause: the gap after a sweep is 3 x sweep duration, and a sweep that
takes 4-7 s under churn makes the gap 12-22 s on top of the 15 s period.
Quiet S13-style claim "N = 15 s bounds it" holds only for small projects
(11-12 s there). Owner decision: N, or the gap factor (3x), or accept
N + 3 x sweep (about 35 s at 50k).

### Criterion 3 and M3

No-churn control (3 edits, 120 s idle, window 129 s): 3 `fs_event` lines, 0
`watcher starved`, 0 `reconciled` lines, so 0 sweeps. Daemon CPU over the
window: 2.16 s user + 0.65 s sys. The probe count is not logged, so "about 12
probes heard" is not measured.

### M5

`fileChanged` requests per edited file, from `rt_sent ... method=fileChanged`
in the daemon log: control 1, 1, 1; churn arms 1-2 per file (8 of 9 files
2, expected 2 = sweep plus late event; 1 where no late event arrived). Never
above 2.

## Owner decision

2026-11-22, on criterion 1 at 50k files: "Принять ~35 с при 50k (Recommended)".
N stays 15 s and the 3x gap stays; the bound under heavy churn is about
N + 3·sweep (37.5 s at a 7.5 s sweep), and the gap keeps the sweeps at about
a quarter of one core during a build.
