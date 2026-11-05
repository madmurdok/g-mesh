# GM-698 rerank latency at K=30 (S16)

Method: design note D6. Driver `eval/embedding/gm464_latency.py` (product path: `g-mesh mcp-shim`, real daemon, g-mesh's own index, 30 queries, warm-up pass untimed, 3 timed passes, fresh daemon per arm). `base` = release-4.26.0 binary (`bcf47d54`), `enr` = branch binary (`239fa361`), `off` = branch binary with `G_MESH_RERANK=off`. Rerank on = 4 threads. Cross-encoder dir (`G_MESH_RERANK_MODEL_DIR`): `g-mesh/eval/embedding/work/models/ms-marco-MiniLM-L6-v2`. Driver change: `BINS` gained `base` and `enr` (`BIN_BASE`, `BIN_ENR`). Order run: `base enr enr base base enr off`.

## Verdict

**Median added latency per query at K=30 = +292 ms** (enr 783 ms vs base 492 ms, 270 timed calls each); **p95 added = +476 ms** (1725 vs 1249). Off reference: 104 ms median, 118 ms p95. First (model-loading) call: base 1337/1530/1388 ms, enr 1746/1641/1520 ms, off 683 ms (not in the medians). No threshold was set; number reported for the owner's gate. **Caveat: load gate was met at the start of every arm, but see machine state: CPU speed limit fell to 46-78% by the end of every rerank-on arm, and pass-to-pass spread is large, so treat +292 ms as noisy.**

## Pooled

| arm | n | p50 ms | p95 ms | mean ms |
|---|---|---|---|---|
| base | 270 | 491.7 | 1248.5 | 568.8 |
| enr | 270 | 783.4 | 1724.7 | 895.4 |
| off | 90 | 103.7 | 117.6 | 103.1 |
| enr - base | | +291.8 | +476.2 | +326.6 |

## Per arm and pass (median ms of 30 calls per pass)

| run | pass 1 | pass 2 | pass 3 | arm p50 | arm p95 | first call ms | daemon CPU s / wall s |
|---|---|---|---|---|---|---|---|
| base#1 | 359 | 440 | 781 | 495 | 1326 | 1337 | 214 / 55 |
| enr#2 | 608 | 580 | 817 | 671 | 1557 | 1746 | 271 / 67 |
| enr#3 | 943 | 883 | 949 | 931 | 1659 | 1641 | 328 / 88 |
| base#4 | 476 | 580 | 549 | 527 | 1554 | 1530 | 223 / 59 |
| base#5 | 405 | 456 | 404 | 414 | 638 | 1388 | 161 / 39 |
| enr#6 | 756 | 783 | 916 | 812 | 2087 | 1520 | 324 / 86 |
| off#7 | 103 | 101 | 106 | 104 | 118 | 683 | 29 / 9 |

Arm-median spread: base 414-527, enr 671-931; every enr arm is above every base arm.

## Controls (the arms differ)

- Rerank-on share (pages not in descending cosine order): base 90/90 per arm, enr 90/90 per arm, off 0/90. Rerank is on in both arms, off in the reference.
- Mean pair token count (offline, same 30 queries, the 30 stored top-30 candidates per query of int8-structured, g-mesh corpus, ms-marco tokenizer, query + text + 3 specials; base text = structured text, enr text = Python builder `EPB`, body budget 48; 900 pairs, 777 nodes, 0 missing sources): **base 66.1, enr 101.9** (+35.8, +54%). Design expected ~49 vs ~93; the absolute numbers differ by method (here the query and specials are counted), the arms are separated. Not logged from the daemon (no Rust change).
- Daemon CPU/wall about 3.5-3.9 cores for rerank-on arms vs 3.2 for off; enr uses 20-30% more daemon CPU than base for the same calls.

## Machine state

Load gate (1-min load < 4 held 30 s) was on and open at every arm (waited within the 1800 s cap). Load and thermals per arm (`pmset` CPU_Speed_Limit/CPU_Scheduler_Limit, start -> end):

| run | load start -> end | CPU speed limit start -> end | `time -p` real / user / sys (driver process) |
|---|---|---|---|
| base#1 | 3.84 -> 42.24 | 100 -> 51 | 232.63 / 0.20 / 0.21 |
| enr#2 | 3.50 -> 7.39 | 100 -> 46 | 441.06 / 0.24 / 0.30 |
| enr#3 | 3.20 -> 10.01 | 100 -> 53 | 299.26 / 0.23 / 0.24 |
| base#4 | 3.80 -> 6.23 | 100 -> 78 | 195.48 / 0.19 / 0.21 |
| base#5 | 3.65 -> 5.51 | 100 -> 56 | 161.68 / 0.17 / 0.20 |
| enr#6 | 3.52 -> 10.78 | 100 -> 51 | 161.35 / 0.20 / 0.21 |
| off#7 | 3.33 -> 4.09 | 100 -> 100 | 327.48 / 0.20 / 0.24 |

`real >> user+sys` in every arm: `time -p` measures the Python driver, which waits on the daemon (indexing, model load, the calls), so it says nothing about the daemon; the daemon's own CPU (table above) shows it was computing, not waiting. Load rose during arms (base#1 end 42 is a load spike from other processes while the arm ran; the other arms ended at 5-11). Full `uptime` lines per arm are in the driver's summary (scratch, not committed).
