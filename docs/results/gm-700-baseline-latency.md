# GM-700 quiet-machine rerank latency baseline at K=30 (S1)

Method: as `gm-698-latency.md`. Driver `eval/embedding/gm464_latency.py` (product path: `g-mesh mcp-shim`, real daemon, g-mesh's own index, 30 queries, untimed warm-up pass, 3 timed passes, fresh daemon per arm, load gate 1-min load < 4 held 30 s, cap 7200 s). `base` = release-4.26.0 build of `bcf47d54` (`g-mesh 4.26.0`, built in a throwaway worktree, removed afterwards), `enr` = release-4.27.0 build of `9bb789cd` (`g-mesh 4.27.0`), `off` = the enr binary with `G_MESH_RERANK=off`. Release builds of `g-mesh` plus the python/rust/typescript plugin binaries, copied next to each `g-mesh`. Cross-encoder dir: `g-mesh/eval/embedding/work/models/ms-marco-MiniLM-L6-v2`. Order run: `base enr enr base base enr off`, whole run under `caffeinate -i -s`. Run window 2026-11-08 19:44:35 - 20:25:29 (+0400).

## Verdict

At K=30 the rerank adds a median **+199 ms** (enr 726 vs off 101 gives +625 ms for the whole rerank on enr text; base 528 vs off 101 gives +427 ms). **enr - base: p50 +198.5 ms, p95 +633 ms, mean +238 ms.** Caveat: the gate opened at every arm start, but the machine was not quiet during arms (other work on the Mac; see machine state), so the rerank-on numbers are noisy and the CPU speed limit fell in 4 of 6 rerank-on arms.

## Pooled

| arm | n | p50 ms | p95 ms | mean ms |
|---|---|---|---|---|
| base | 270 | 527.5 | 926.1 | 554.9 |
| enr | 270 | 726.0 | 1559.0 | 793.3 |
| off | 90 | 100.8 | 125.9 | 104.1 |
| enr - base | | +198.5 | +632.9 | +238.4 |
| enr - off | | +625.2 | +1433.1 | +689.2 |

## Per arm and pass (median ms of 30 calls per pass)

| run | pass 1 | pass 2 | pass 3 | arm p50 | arm p95 | first call ms | daemon CPU s / wall s |
|---|---|---|---|---|---|---|---|
| base#1 | 551 | 629 | 499 | 533 | 795 | 1689 | 188.0 / 49.7 |
| enr#2 | 751 | 945 | 844 | 830 | 1602 | 1648 | 296.3 / 80.7 |
| enr#3 | 639 | 595 | 738 | 657 | 1361 | 1499 | 240.5 / 63.8 |
| base#4 | 479 | 434 | 456 | 450 | 681 | 1526 | 167.8 / 41.4 |
| base#5 | 479 | 754 | 551 | 574 | 1565 | 1425 | 221.0 / 58.8 |
| enr#6 | 604 | 711 | 806 | 703 | 1716 | 1672 | 269.3 / 69.7 |
| off#7 | 99 | 103 | 102 | 101 | 126 | 728 | 30.2 / 9.4 |

Arm-median spread: base 450-574, enr 657-830; every enr arm is above every base arm.

## Controls

- Rerank-on share (pages not in descending cosine order): base 90/90 per arm, enr 90/90 per arm, off 0/90. The arms differ as intended.
- Index: the first attempt (plugin binaries missing from the bin dirs, only the Go index built) gave no structural index and every arm timed out; discarded, nothing from it is reported. The reported run used `g-mesh-plugin-{python,rust,typescript}` beside each `g-mesh` and passed a 2-query smoke before the timed run.

## Machine state

| run | uptime at start | gate waited s | load start / end | CPU_Speed_Limit start / end | time -p real / user / sys |
|---|---|---|---|---|---|
| base#1 | 20:00 up 18d 8:03, load 3.24 7.01 8.22 | 85 | 3.24 / 5.92 | 100 / 97 | 1040.76 / 0.33 / 0.56 |
| enr#2 | 20:03 up 18d 8:05, load 3.29 6.02 7.68 | 60 | 3.29 / 12.12 | 100 / 53 | 164.58 / 0.17 / 0.19 |
| enr#3 | 20:08 up 18d 8:10, load 3.50 5.99 7.38 | 200 | 3.50 / 23.13 | 100 / 100 | 286.07 / 0.19 / 0.22 |
| base#4 | 20:14 up 18d 8:16, load 3.36 5.87 7.14 | 295 | 3.36 / 5.48 | 100 / 73 | 357.28 / 0.17 / 0.22 |
| base#5 | 20:16 up 18d 8:18, load 3.56 5.38 6.79 | 55 | 3.56 / 10.93 | 100 / 65 | 133.70 / 0.19 / 0.21 |
| enr#6 | 20:19 up 18d 8:22, load 3.98 5.56 6.65 | 120 | 3.98 / 6.18 | 100 / 60 | 213.28 / 0.20 / 0.21 |
| off#7 | 20:25 up 18d 8:27, load 3.31 5.00 6.10 | 240 | 3.31 / 3.93 | 100 / 100 | 258.41 / 0.21 / 0.26 |

`time -p` wraps the Python driver process (user/sys is the driver, not the daemon; real includes gate wait and index/ready wait). The gate opened (load just under 4) every time, never "quiet" in the sense of idle: the 1-min load at arm start was 3.2-4.0 and the 5/15-min loads 5-8, so something else was running; the end-of-arm loads of 5-23 include the 4-thread rerank itself plus that. CPU_Speed_Limit fell to 53-73% at the end of base#4, base#5, enr#2, enr#6 (thermal throttling under the rerank load), so the later passes of those arms are pessimistic. `uptime` ranges are in the run log.

## Sleep check

`pmset -g log | grep -E ' Sleep  '` over 19:44:35 - 20:25:29: empty. (The last Sleep entries, 11:34 and 19:03, are outside the window.)

## Comparison with gm-698-latency.md (loaded machine)

| | GM-698 (loaded) | this run | change |
|---|---|---|---|
| base p50 / p95 | 491.7 / 1248.5 | 527.5 / 926.1 | +36 / -322 |
| enr p50 / p95 | 783.4 / 1724.7 | 726.0 / 1559.0 | -57 / -166 |
| off p50 / p95 | 103.7 / 117.6 | 100.8 / 125.9 | -3 / +8 |
| enr - base p50 / p95 / mean | +291.8 / +476.2 / +326.6 | +198.5 / +632.9 / +238.4 | -93 / +157 / -88 |
| first call base / enr / off | 1337-1530 / 1520-1746 / 683 | 1425-1689 / 1499-1672 / 728 | same range |

The "quiet" re-run did not tighten the picture: the enr-base median gap shrank (+292 to +199) but the p95 gap grew, off is unchanged (rerank off is insensitive to load), and arm medians still spread 450-574 (base) and 657-830 (enr) with throttling in most rerank-on arms. Treat the median enr-base gap as roughly +200 to +300 ms and the rerank as 4-6x the no-rerank latency; a before/after comparison for GM-700 should use paired arms in the same run, not these absolute numbers.
