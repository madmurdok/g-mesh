# GM-700 product rerank latency before and after, K=30 (S26)

Design section 4.5. Driver `eval/embedding/gm464_latency.py` (product path: `g-mesh mcp-shim`, real daemon, g-mesh's own index of this worktree, 30 queries, untimed warm-up pass, 3 timed passes, fresh daemon per arm, load gate 1-min load < 4 held 30 s). Order run: `base new new base base new off`, whole script under `caffeinate -i -s` with `set -e`/`pipefail`. Run window 2026-11-09 16:50:16 - 18:43:51 (+0400).

- `base`: `g-mesh 4.27.0` build of `9bb789cd` (the S1 binaries in `/private/tmp/claude-502/gm700/enr`, with all three plugin binaries beside it), fp32 model (sha256 `5d3e70fd...`), CHUNK 16.
- `new`: release build of branch tip `2bd2f8c5` (`g-mesh -p python -p rust -p typescript`, binaries copied side by side), quint8 file, sha256 recomputed `c80a8b34256ea453093d612e3ac48d3d965a0c0a48c7906709af8b8e28461bf9` (matches the required value).
- `off`: the new binary with `G_MESH_RERANK=off`.
- One index home per binary (`home-base`, `home-new`), created fresh for this run.

## Pooled (3 passes x 30 queries per arm run)

| arm | n | p50 ms | p95 ms | mean ms |
|---|---|---|---|---|
| base | 270 | 754.9 | 1996.2 | 876.2 |
| new | 270 | 386.8 | 565.7 | 396.5 |
| off | 90 | 102.0 | 116.5 | 102.7 |

| comparison | d p50 | d p95 | d mean | ratio p50 | ratio p95 | ratio mean |
|---|---|---|---|---|---|---|
| new vs base | -368.2 | -1430.5 | -479.7 | 0.51 | 0.28 | 0.45 |
| new vs off | +284.8 | +449.2 | +293.8 | 3.79 | 4.85 | 3.86 |
| base vs off (reference) | +653.0 | +1879.6 | +773.6 | 7.40 | 17.13 | 8.54 |

Per arm run (p50/p95 ms): base#1 725/1759, new#2 389/565, new#3 347/470, base#4 824/2587, base#5 734/1634, new#6 433/611, off#7 102/117. The two orderings agree: every new run is below every base run in p50 and p95.

**Caveat (read before quoting the ratios).** The machine was not quiet and the base arms were hit harder than the new arms. CPU_Speed_Limit at the end of base#1/#4/#5 was 53/46/70 (100 at start); all new and off arms stayed at 100 at both ends. The 1-min load at the end of base#4 and base#5 was 15.0 and 20.3 (other work started on the Mac during those arms), against 4.6-6.2 for the new arms. The base p95 (1996 ms) and the p95 ratio (0.28) are therefore inflated by throttling and load; p50 is less affected but still biased against base. Treat "new is about half of base at p50" as an upper-bound speedup, not a precise figure. New vs off is the cleaner comparison (both unthrottled): the rerank at K=30 now costs about +285 ms at p50 over the name-query floor of 102 ms, against about +653 ms before (S1 measured +427 ms for the same base build on a different, also noisy, window).

## Controls

- **Rerank-on share** (pages not in descending cosine order, per 90 timed pages): base 90/90 in each of the 3 runs, new 90/90 in each of the 3 runs, off 0/90. Expected for new: 90/90 minus the exact-name skips. The skip rule (`confident` in `core/src/embedding/rerank.rs`, Python form `skip_flags` rule (a) in `gm463_bakeoff.py`, using `gm462_ce_text.split_words` and the `::|[./#\\]` separators) applied to the 30 queries and the cosine top-1 of each (taken from the `off` arm, whose page order is the cosine order) skips **0** queries, so the expected count is 90/90: matches. The arms differ as intended.
- **Arm identity:** `rerank.outcome` of every new run reads `loaded` with dir `.../rerank-quint8`; base runs `loaded` with the fp32 dir; off runs `off`. The new binary handed the fp32 dir (untimed, 2 queries): outcome `failed` with the fp32 dir (the sha refusal path; `check_rerank_files` refuses the non-pinned sha256). The log text itself was not captured; the recorded evidence is the `failed` outcome record.
- **Indexing:** the failure mode of S1 (plugin binaries missing) did not recur; a 2-query smoke (1 pass, base/new/off) indexed and passed before the timed run. A first launch of this script was discarded after the driver lacked a `new` arm (KeyError, found by the smoke summary); nothing from it is reported. The driver gained a `new` arm, a per-arm model dir (`RDIR_<ARM>`), a home per binary, and records the cosine top-1 name per query and `rerank.outcome` (eval only).

## Machine state

- Power: AC, battery 100% (`pmset -g batt`) at the end of the run.
- Gate: open at every arm start (load < 4 held 30 s). Start-of-arm 1-min load 3.1-3.7; end-of-arm load, CPU_Speed_Limit (start/end), `uptime` and `/usr/bin/time -p` user/sys/real per arm are in the table below. `user`/`sys` are the driver Python's own CPU, not the daemon's; `real` includes indexing (about 1200 s for base#1 and new#2, the two cold indexes), so it is not a latency.
- Sleep check (`pmset -g log`, window 16:50-18:43): no Sleep/Wake entry inside the window (the last one is a Wake at 13:05).

| arm | start load | end load | speed limit start/end | daemon cpu/wall | real/user/sys s |
|---|---|---|---|---|---|
| base#1 | 3.22 | 6.26 | 100/53 | 3.71 | 1601.76/0.53/0.99 |
| new#2 | 3.10 | 4.56 | 100/100 | 3.84 | 1357.36/0.52/0.93 |
| new#3 | 3.35 | 4.90 | 100/100 | 4.17 | 92.17/0.15/0.16 |
| base#4 | 3.42 | 14.98 | 100/46 | 3.47 | 156.71/0.18/0.19 |
| base#5 | 3.53 | 20.25 | 100/70 | 3.69 | 654.97/0.26/0.35 |
| new#6 | 3.66 | 6.15 | 100/100 | 3.89 | 828.67/0.23/0.39 |
| off#7 | 3.41 | 3.66 | 100/100 | 3.25 | 373.16/0.19/0.29 |

(The long `real` of base#5, new#6 and off#7 comes from waiting at the load gate and the index reconcile, not from the timed calls.)

## GM-700 A/Bs and their controls (AC3)

| slice | A/B | doc | control (section) | result |
|---|---|---|---|---|
| S1 | rerank latency baseline 4.26.0 / 4.27.0 / off | `gm-700-baseline-latency.md` | Controls: rerank-on share base 90/90, enr 90/90, off 0/90; plugin binaries beside g-mesh, 2-query smoke | arms differ |
| S8 | offline quality fp32 vs qint8 vs quint8; K 20/30; chunk 1 vs 16; skip simulation | `gm-700-offline.md` | Controls (quality): chunk-16 control max abs diff 0.511 / 0.812 > 1e-3. Controls (latency): per-arm model sha distinct, ms/pair differs (27.3 vs 20.9 fp32; 5.5-6.9 int8); mean pair tokens 101.9 in both fp32 arms. Controls (skip): m=+inf without name rule equals no-skip; all-skipped equals int8-structured. K control: 376 of 845 held-out NL queries differ between K=20 and 30 | differed / pass |
| S9 | gte int8 vs gte fp32 probe | `gm-700-gte-int8-probe.md` | Control (3.3) and checks: input format, rank-only check, gte int8 vs fp32 logits max abs diff 1.7166 on 5 pairs; the quality harness's `--controls-only` was not run (verdict skips the quality step) | differed (one control not run, stated) |
| S15/S21 | Rust vs Python int8 logit parity | `gm-700-int8-parity.md` | no section named Controls; the Evidence table is the control set: tokenization 0/540 differ, fp32 Rust vs Python mean 0 max 0, decisive probe (one `vpxord zmm0` before the session) flips Rust to equal Python 540/540, the other variants stay 0.274 apart | arms distinguished by the probe |
| S22 | Rust vs Python quint8 quality | `gm-700-rust-quality.md` | Controls: Rust differs from Python quint8 on 39,645 pairs (> 0), top-10 differs fp32 vs Rust on 2052 queries, fp32 reference row reproduces S8/GM-698 (C0), 10-pair smoke with C1, C5, C8, C7, skip-sim controls | pass |
| S26 | product latency base / new / off, K=30 | this doc | Controls: rerank-on share base 90/90, new 90/90 (0 skips expected), off 0/90; arm identity loaded/failed/off records | pass |

Gaps for AC3: S15/S21's doc has no Controls heading (listed above by its evidence table), and S9's `--controls-only` was not run. The S26 base-vs-new ratios carry the throttling caveat above.
