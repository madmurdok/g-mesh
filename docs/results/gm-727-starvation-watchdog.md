# GM-727/S13: starvation watchdog, measured

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
