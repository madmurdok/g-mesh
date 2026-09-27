# GM-429 A/B: answering concurrency and RA priming threads

Slice GM-429/S3. It measures the "2 + 4" proposal in
`gm-429-speedup-proposal.md`, which asks two things: does raising the bridge's
in-flight cap (`Budgets::concurrency`, 8) shorten answering, and does raising
rust-analyzer's `cachePriming.numThreads` shorten priming?

**Verdict: no change for either knob.** No arm beat the baseline on its own
phase, and all four arms wrote the identical edge set.

## Method

- Code: `6a26322`, which is release-3.15.0 with GM-428 and GM-433. The runs
  used two throwaway detached worktrees at that commit:
  - a build worktree B: `cargo build --workspace --release`,
    `npm ci && npm run build` in `plugins/typescript`, and `npm ci` in
    `plugins/python`;
  - a project worktree P, the indexed project, with `npm ci` in its
    `plugins/typescript`.

  Both worktrees and the scratch `G_MESH_HOME` were removed afterwards.
- Instrumentation: B carried S1's bridge and client `GM429` stderr lines
  (`plugins/sdk/src/lsp/{bridge,client,mod}.rs` only), plus three additions:
  - `Budgets::default().concurrency` reads `GM429_CONCURRENCY`, defaulting to 8.
    One binary therefore served every arm, and there were no builds between
    arms.
  - `run_pass` logs the cap and the maximum in-flight count it reached.
  - `LspClient::initialize` logs the `initializationOptions` it sends.
- Priming arm: before each run the script copied one of two `plugin.toml`
  variants into B's `plugins/rust/`. The daemon reads that manifest at startup
  and passes it to the plugin. The variant for this arm adds

  ```toml
  [plugin.semantic.initialization_options.cachePriming]
  numThreads = 8
  ```

  RA's default is `physical`, which is 4 on this machine. Every p8 run's
  `init-options` line shows `"cachePriming":{"numThreads":8}`, and every
  other run's line shows only `checkOnSave`.
- Scenario: a cold start, as in S1's b1-b4. Each run deleted
  `G_MESH_HOME/projects`, started `g-mesh daemon --project-root P` under
  `/usr/bin/time -p`, and activated it with one MCP call through `mcp-shim`. It
  then waited for `[rust] semantic pass:` and core's
  `rust semantic pass over ... complete`, ran `g-mesh stop`, and sent SIGKILL
  to anything left of the daemon's tree (nothing was left in any run).
  `G_MESH_HOME` was isolated: `models` was linked and the owner's embedding
  cache was copied with `.backup`.
- Order: one discarded warm-up (w0) filled P's empty `target/`. It ran with the
  load average at 143 after the build. After that came three rounds of
  c8, c16, c32, p8, interleaved so that drift spreads across arms. Before each
  run the script waited until load1 < 8.
- Metrics per run:
  - "ready": bridge pass start to `wait_ready` done.
  - "priming": the sum of the live-stamped `rustAnalyzer/cachePriming`
    progress spans. "(2)" marks a run whose second span was 0.0s long.
  - "answering": `run_pass` wall time.
  - "total": the bridge's pass start to its finish.
  - "requests": answered `definition` + `implementation` requests.
  - "RA CPUs": a 1s `ps` sampler of rust-analyzer processes under the daemon.
    CPU-s over the answering window divided by that window's wall time.
  - "peak": RA's peak RSS.
- Edge control: after each run's stop, the rows
  `source='semantic' AND engine='rust-analyzer'` were dumped from `index.db`,
  sorted by id (id, fromId, toId, kind, resolved, toDeclaration). Each dump was
  byte-compared with r1-c8's.

## Machine state

- MacBook, Intel i7-1068NG7, 4 physical / 8 logical CPUs, 32 GB, macOS.
  rust-analyzer 1.97.1 (8bab26f4 2026-07-14).
- load1 at each run's start was 4.4-7.6 (in the table). Nothing else was
  building or measuring. The owner's other daemons were left alone.
- `/usr/bin/time -p` wraps the daemon. Its user/sys (6-10s against 64-92s real)
  excludes rust-analyzer, whose CPU is in the sampler column, as S1 explains.

## Results

All times are in seconds.

| run | arm | load1 | numThreads | cap | max in flight | ready | priming | answering | total | requests | deferred | complete | def p50/p99 ms | RA CPUs (answering) | RA peak GB | rust edges | edges vs r1-c8 | real | user | sys |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| w0 (warm-up) | p8 | 143.48 | 8 | 8 | 8 | 128.3 | 13.5 | 38.5 | 166.8 | 29,168 | 0 | yes | 6.31/65.77 | 1.92 | 2.23 | 2,679 | same | 197.95 | 9.44 | 2.86 |
| r1 | c8 | 7.56 | default | 8 | 8 | 23.4 | 10.0 | 33.6 | 57.1 | 29,168 | 0 | yes | 5.70/53.36 | 2.07 | 2.23 | 2,679 | baseline | 80.31 | 6.37 | 1.90 |
| r1 | c16 | 5.65 | default | 16 | 16 | 16.7 | 10.2 (2) | 32.8 | 49.6 | 29,168 | 0 | yes | 13.35/87.42 | 2.43 | 2.57 | 2,679 | same | 66.19 | 7.97 | 1.75 |
| r1 | c32 | 5.43 | default | 32 | 32 | 18.4 | 11.1 (2) | 32.4 | 50.8 | 29,168 | 0 | yes | 29.05/119.22 | 2.61 | 3.05 | 2,679 | same | 73.25 | 9.17 | 1.87 |
| r1 | p8 | 6.41 | 8 | 8 | 8 | 17.2 | 10.1 (2) | 34.4 | 51.6 | 29,168 | 0 | yes | 5.80/56.50 | 1.99 | 2.25 | 2,679 | same | 72.54 | 10.02 | 1.93 |
| r2 | c8 | 4.91 | default | 8 | 8 | 18.9 | 10.7 (2) | 34.6 | 53.5 | 29,168 | 0 | yes | 5.80/53.44 | 1.93 | 2.24 | 2,679 | same | 75.52 | 7.96 | 1.83 |
| r2 | c16 | 5.34 | default | 16 | 16 | 20.0 | 10.8 (2) | 34.6 | 54.6 | 29,168 | 0 | yes | 13.91/91.95 | 2.31 | 2.54 | 2,679 | same | 77.65 | 8.35 | 1.90 |
| r2 | c32 | 6.10 | default | 32 | 32 | 19.2 | 10.1 (2) | 38.4 | 57.6 | 29,168 | 0 | yes | 33.23/162.94 | 2.46 | 3.24 | 2,679 | same | 78.47 | 7.49 | 1.88 |
| r2 | p8 | 6.01 | 8 | 8 | 8 | 17.2 | 9.6 (2) | 35.7 | 52.8 | 29,168 | 0 | yes | 5.81/66.58 | 1.98 | 2.26 | 2,679 | same | 75.22 | 9.55 | 1.90 |
| r3 | c8 | 5.35 | default | 8 | 8 | 17.1 | 10.0 | 34.3 | 51.3 | 29,168 | 0 | yes | 5.80/53.66 | 2.02 | 2.21 | 2,679 | same | 71.15 | 8.32 | 1.71 |
| r3 | c16 | 4.72 | default | 16 | 16 | 16.0 | 10.2 (2) | 32.2 | 48.2 | 29,168 | 0 | yes | 13.19/86.79 | 2.41 | 2.51 | 2,679 | same | 64.15 | 7.13 | 1.72 |
| r3 | c32 | 4.40 | default | 32 | 32 | 16.3 | 9.8 | 36.7 | 53.0 | 29,168 | 0 | yes | 32.26/164.50 | 2.53 | 3.11 | 2,679 | same | 70.65 | 10.14 | 1.82 |
| r3 | p8 | 6.49 | 8 | 8 | 8 | 21.4 | 12.4 (2) | 46.8 | 68.2 | 29,168 | 0 | yes | 7.28/86.73 | 1.79 | 2.19 | 2,679 | same | 91.86 | 8.94 | 1.99 |

| arm | median answering | range answering | median priming | range priming | median total |
|---|---|---|---|---|---|
| c8 (baseline) | 34.3 | 33.6-34.6 | 10.0 | 10.0-10.7 | 53.5 |
| c16 | 32.8 | 32.2-34.6 | 10.2 | 10.2-10.8 | 49.6 |
| c32 | 36.7 | 32.4-38.4 | 10.1 | 9.8-11.1 | 53.0 |
| p8 (numThreads 8) | 35.7 | 34.4-46.8 | 10.1 | 9.6-12.4 | 52.8 |

RA's CPU share during the priming span, from the same sampler:

| arm | runs | RA CPUs |
|---|---|---|
| default (c8, c16, c32) | 9 | 2.87-3.09 |
| p8 | 3 | 3.34, 3.51, 2.95 |

"Total" also carries the ready phase, which the knobs do not touch.
The ready phase varies by 16-23s from run to run, mostly RA's workspace
loading, so total is not the metric for either verdict.

## Verdicts

- **Concurrency: no change (keep 8).** The arms can be told apart: max in
  flight reached 8, 16 and 32, and RA's CPU share while answering rose
  from 1.93-2.07 to 2.31-2.43 at 16 and to 2.46-2.61 at 32. Answering did not
  get faster. At c16 the range 32.2-34.6 overlaps c8's 33.6-34.6, and r2-c16
  equals the worst c8 run. At c32 the range 32.4-38.4 is wider and its median
  is slower. Per-request latency grew roughly in proportion to the cap
  (definition p50 5.8 → 13.2-13.9 → 29-33ms, p99 53 → 87-92 → 119-165ms), so
  throughput stayed flat. The extra CPU went to contention inside RA and not
  to more answers. That makes this a real "RA-bound" result, not a null
  measurement. Costs of raising the cap anyway: +0.3 GB RA peak RSS at 16 and
  +0.8-1.0 GB at 32, and p99 latency moving toward the 10s `Budgets::request`
  timeout under load.
- **Priming `numThreads`: no change (keep RA's default, `physical`).** The
  option reached RA in every p8 run and moved RA's priming CPU share in 2 of
  3 runs (3.34 and 3.51 against 2.87-3.09). Priming wall time did not move:
  9.6-12.4 against 10.0-10.7. Priming already runs at about 3 CPUs on 4
  physical cores, and hyperthreads add little. r3-p8 was the slowest run on
  every phase (answering 46.8), and nothing in the summary explains it.
  Because answering does not depend on the priming setting, this looks like
  machine noise and not an effect of the arm. It does not change the verdict,
  since no p8 run beat the baseline on priming.
- A correction to S1's reading: S1 put priming at "~1.65 CPUs". That figure
  covers the whole pass-start-to-ready window (b4). Priming alone runs at
  about 3 CPUs, so the proposal's "#4" room (priming using more cores) is
  mostly not there.

## Edge-set control

All 13 runs, including the warm-up, wrote the same 2,679 rust-analyzer
semantic edges. Every run's sorted dump is byte-identical to r1-c8's. Every
run answered 29,168 requests, with 0 deferred, and was complete. No arm is
disqualified. The request count is higher than S1's 28,535 because the code
has grown since `805686f`.
