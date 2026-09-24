# GM-395 lazy indexing — measurement results

Measurement phase for `docs/architecture/lazy-indexing.md`'s "## 5. Measurement
plan": M1 (steps 1-5; step 6 skipped), M2, and M3(a). No code was changed to
produce these numbers.

## Environment

- `claude --version`: `2.1.281 (Claude Code)`
- g-mesh commit: `163eb5e78f6817d770e1f4a2a7ebbc3accb2f69b` (branch
  `feat/GM-395-lazy-indexing`)
- Build: `cargo build --release --workspace` at that commit (fresh rebuild,
  50.62s, no errors); `plugins/typescript/dist` already built, confirmed
  present.
- `uptime` at the start of the M1/M2/M3 runs: `4:51 up 15:06, 7 users, load
  averages: 5.17 14.97 12.86` (a loaded dev machine throughout — see the M2
  timing note below).
- `uptime` after all runs: `5:36 up 15:51, 7 users, load averages: 3.51 5.24
  5.71`
- Scratch layout: `/tmp/gm395m/{m1,m2,m3}/...` — the task's designated
  scratchpad directory produces a `G_MESH_HOME` whose daemon socket path
  (`$G_MESH_HOME/projects/<16-hex-hash>/daemon.sock`) exceeds a safe margin
  under the ~103-byte AF_UNIX path limit, so `/tmp/gm395m` was used instead,
  per the task's own fallback instruction.

## Deviation: the hold knob used for M1

The doc's M1 setup names `G_MESH_BULK_INDEX_HOLD_FILE` and a hold-file +
background-`rm` release at 180s. Reading the code
(`core/src/daemon/bulk_index.rs`, `hold_the_walk_open_for_tests`) shows this
knob (env var actually named `G_MESH_BULK_INDEX_HOLD_FILE`,
`WALK_HOLD_FILE_ENV`) polls for the file's removal but is **hard-capped at a
30s deadline regardless of whether the file is removed**:

```rust
let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
while path.exists() && std::time::Instant::now() < deadline {
    std::thread::sleep(std::time::Duration::from_millis(2));
}
```

The same 30s cap exists on the embedding pass's sibling knob
(`G_MESH_EMBED_PASS_HOLD_FILE` / `HOLD_FILE_ENV` in
`core/src/embedding/backfill.rs`). A 180s scheduled release against either
knob would silently do nothing past 30s — the walk resumes at 30s regardless,
which would let both arm A and arm B finish indexing before the 60s idle
window even opens, defeating the control.

**Used instead:** `G_MESH_BULK_INDEX_DELAY_MS=180000` (`WALK_DELAY_ENV`), a
plain, uncapped `thread::sleep`, applied right after `bulk_index::run`
finishes every language's walk and the project-wide import/symbol linking —
i.e. still before the caller can mark the project ready, so `get_file_outline`
still waits on it. This is a real GM-395 test knob, not a workaround bolted
on for this measurement; it just isn't the specific one the doc named from
memory, and it doesn't have the 30s cap. Confirmed by the exact log line each
run produced: `g-mesh daemon: holding the finished bulk walk open for
180000ms (G_MESH_BULK_INDEX_DELAY_MS)`. No background hold-file/`rm` process
was needed as a result — the 180s wait is intrinsic to the sleep.

Env var names actually used (all confirmed against `core/src` before use):
`G_MESH_HOME`, `G_MESH_BULK_INDEX_DELAY_MS`, `G_MESH_PROGRESS_INTERVAL_MS`,
`G_MESH_TRACE_CALLS`, `G_MESH_DAEMON_LOG`, `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT`
(Claude Code's own env var, unchanged from the doc).

## M1. Claude Code's tool timeout, with progress on and off

**Setup:** release binary, TS fixture (`src/a.ts`: one function, one class)
under `/tmp/gm395m/m1/proj`, a fresh `G_MESH_HOME` per rep (a shared home
would let rep 2+ see an already-built index and never wait at all, since
`G_MESH_BULK_INDEX_DELAY_MS` is an unconditional sleep, not a
release-on-demand latch). `claude -p --model haiku --output-format
stream-json --verbose --allowedTools=mcp__g-mesh__get_file_outline
--mcp-config <arm's config> --strict-mcp-config "Call the g-mesh
get_file_outline tool on src/a.ts and print its raw result."`, with
`CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=60000`. `--allowedTools` had to use
`--allowedTools=value` (not a space-separated value before the positional
prompt) — the space form let the variadic option swallow the prompt string
and Claude Code refused to start ("Input must be provided either through
stdin or as a prompt argument").

`g-mesh` is a deferred tool in this Claude Code version, so every rep's
transcript shows a `ToolSearch` call before the first `get_file_outline` —
expected, harmless, not part of the timing.

Arm A = default progress (5s interval). Arm B = `G_MESH_PROGRESS_INTERVAL_MS=0`.
3 reps each, per the doc, run with **B first** as the control.

### Arm B (progress off) — the control

| Rep | Real (whole `claude -p` run) | `get_file_outline` attempts | Per-attempt outcome | `progressToken` | notifications sent |
|---|---|---|---|---|---|
| B1 | 142.38s | 2 (model retried once after the first errored) | both: **client-side abort at 60s**, error `MCP server "g-mesh" tool "get_file_outline" sent no response or progress for 60s; aborting...`; daemon-side: `cancelled`, `waited_ms=69484` then `waited_ms=140224` (second number counts from *that* request's own entry, ~66s after the first) | present (both) | 0 (both) |
| B2 | 133.90s | 2 (same retry pattern) | both aborted at 60s client-side; daemon: `cancelled waited_ms=67930` / `waited_ms=131938` | present (both) | 0 (both) |
| B3 | 74.58s | 1 (model did not retry this time, answered from a local `Read` instead) | aborted at 60s client-side; daemon: `cancelled waited_ms=69501` | present | 0 |

Every attempt in every rep aborted client-side at **exactly 60.0s** (verified
from the `stream-json` transcript: `tool_use` to `tool_result` gap was
60.009s, 60.014s, 60.010s, 60.017s, 60.009s across the 5 individual attempts),
with the identical error text quoted above. The daemon's own trace shows
`outcome=cancelled` for all of them, not the `satisfied`/`failed`/`timed_out`
trio the code's own doc comment on `trace_call` enumerates — worth noting as
a fourth, real outcome value the comment doesn't list (the client tears down
the MCP connection rather than the daemon receiving a structured per-call
cancellation).

**The differing whole-run "real" times (142s/134s/75s) are not the abort
window** — they're however long the *haiku* agent spent retrying (once, or
zero times) and then answering from `Read`/its own summary after g-mesh gave
up. The discriminating number is the per-attempt abort time, which was 60s on
every single attempt, 5 for 5.

**Control passed: arm B aborted at ~60s in every rep.** Proceeding to arm A.

### Arm A (progress on, default 5s interval)

| Rep | Real (whole `claude -p` run) | `get_file_outline` attempts | Outcome | `progressToken` | notifications sent |
|---|---|---|---|---|---|
| A1 | 197.44s | 1 | `satisfied`, `waited_ms=180501` | present | 36 |
| A2 | 194.67s | 1 | `satisfied`, `waited_ms=180983` | present | 36 |
| A3 | 203.40s | 1 | `satisfied`, `waited_ms=180205` | present | 36 |

All 3 reps survived the 60s idle window (which arm B could not) and returned
the real `get_file_outline` result once the 180s hold released. 36 progress
notifications at a 5s interval over ~180.5-181.0s of wait is exactly
`waited_ms / 5000` — consistent with the documented heartbeat.

### Extra run (step 5): arm A config + per-server `"timeout": 90000`

| Rep | Real | Outcome | `progressToken` | notifications sent |
|---|---|---|---|---|
| A90-1 | 101.78s | `cancelled`, `waited_ms=90017` | present | 18 |

Client-observed error: `MCP server "g-mesh" tool "get_file_outline" timed out
after 90s`, at exactly 90.008s wall (tool_use → tool_result). Progress was
sent the whole time (18 heartbeats × 5s = 90s) and did **not** extend the
per-server `timeout` — matches the doc's expectation that a per-server
timeout is a hard cap progress cannot lift.

Step 6 (interactive `claude`, observing 2-minute auto-backgrounding) was
skipped per the task's scope — left for the owner to run by hand.

### M1 conclusion, per the doc's "What the result changes" rules

- A `progressToken` was present on every call in both arms.
- Arm A survived to the release (180s) on every rep; arm B aborted at ~60s
  on every attempt in every rep.
- → **"A token is present, A survives to the release, and B aborts at 60s:
  design confirmed."** `INDEX_WAIT_CAP` should stay at its current 25-minute
  default as the guard against a per-server wall-clock limit; nothing here
  argues for lowering it.
- Additional, not in the doc's four bullets: the per-server `"timeout"`
  extra run confirms progress cannot lift a fixed per-server cap (matches
  documented MCP client behavior) — no action implied, just corroboration.
- Minor code-vs-doc finding: the daemon's `trace_call` outcome values include
  `cancelled` (client disconnected) in addition to the three
  (`satisfied`/`failed`/`timed_out`) the doc comment above `trace_call`
  enumerates. Worth a one-line doc-comment fix, not a design change.

## M2. Real timings

**Setup:** fresh `G_MESH_HOME` (`/tmp/gm395m/m2/home`), project = the g-mesh
repo itself, release binary. Driven with **a raw MCP client** (a small Python
script speaking newline-delimited JSON-RPC directly to `g-mesh mcp-shim`'s
stdio — `/tmp/gm395m/m2/mcp_client.py`), not `claude -p`: this avoids
agent-loop retries/variability muddying the single timing number the doc
wants, and lets one process hold the connection open across both calls. The
raw client's `tools/call` requests carry no `progressToken` (unlike Claude
Code's), so the daemon log shows `progressToken=absent` for both calls made
this way — a property of this client, not of the server; M1 already showed
that a real progress-aware client gets `present` and heartbeats.

Sequence: `initialize` → `tools/list` → `get_file_outline` on
`core/src/mcp/mod.rs` → `search_code` for `"parse json rpc message"`, all on
the same connection, then the client exits (the daemon keeps running
independently to finish the backfill). CPU was sampled every 10s via a
background script (`/tmp/gm395m/m2/sample_cpu.sh`) that polls `ps -o
pid,%cpu,time,rss,command -p <daemon pid>` and stops once the daemon log
shows the backfill's completion line.

Embedding model check: `~/.g-mesh/models/jina-embeddings-v2-base-code/`
present (`model.onnx` 641,517,466 bytes, `tokenizer.json`), so `search_code`
was not skipped.

**Numbers:**

- Structural bulk index, all 4 languages, whole g-mesh repo (38 Go files, 14
  Python, 205 Rust, 41 TS/JS — 11,882 nodes / 23,189 edges, 936 imports
  linked): daemon-reported `waited_ms=22721` (**22.7s**) for the
  `get_file_outline` request's own indexing-wait window.
- **Client-observed wall time to the first `get_file_outline` answer:
  93.193s** — 70s more than the daemon's own 22.7s figure. The CPU-sample
  log shows why: at t=20s the daemon was at 6.6% CPU / 0.16s accumulated, at
  t=30-40s it burst to ~380% CPU (0.16s → 40.11s accumulated — the actual
  walk), and then sat at **0.0% CPU from t=40s through t=122s** (~80s) before
  the response reached the client. Per this project's own timing-diagnostic
  rule, seconds of real time at 0% CPU mean the process was *waiting*, not
  computing — most likely on process-level startup for the four freshly
  built plugin binaries (this was the first execution of each since `cargo
  build --release --workspace` moments earlier) rather than on anything
  `waited_ms` tracks. `uptime` showed load averages of 5-15 on this machine
  throughout the session (7 users, other processes active), which is a
  plausible confound; this was not diagnosed further, as it's outside this
  task's scope. Report both numbers rather than picking one: **22.7s of
  daemon-tracked indexing wait, 93.2s of wall clock to the client's first
  answer.**
- **First `search_code` answer: 736.501s** (~12.3 min) wall clock from
  request to response — this is the full embedding-backfill wait, per D4
  ("`search_code` while embeddings are running: it waits"), plus the ~93s
  the connection had already been open for the prior call is not included
  (this is measured from sending the `search_code` request itself).
- **Total backfill time** (from the CPU-sample log, daemon at 0% CPU marks
  start and end): began ramping up around t=30s and dropped back to 0.0%
  CPU between t=829s and t=839s. Daemon log's completion line: `g-mesh
  daemon: embedding backfill - 6016 of 6016 candidate nodes embedded`, all
  6016 candidates embedded, none skipped/failed. **~800-830s total**,
  matching the doc's ~800s estimate for "about as long as the old inline
  embedding."
- CPU during the backfill: consistently **~385-400%** (roughly 4 cores),
  sampled every 10s for the full ~830s run (`/tmp/gm395m/m2/cpu_samples.log`,
  84 samples, not truncated here — full file kept on disk).

**Expectation check:** doc predicted "about 31s structural, backfill about
800s." Structural landed at 22.7s (daemon-tracked) / 93.2s (wall-clock,
inflated by an ~80s CPU-idle gap, likely one-time process-startup cost, not
representative of a warm daemon). Neither number exceeds the 2-minute
auto-background threshold in isolation, but 93.2s is close enough to it that
a slower machine or first run should be watched — this argues mildly for
Q4's prefetch discussion, without being a clear trigger. Backfill matched
the ~800s estimate closely.

## M3(a). Idle connect costs nothing

**Setup:** unindexed small fixture (`/tmp/gm395m/m3/proj`, one `.ts` file), a
fresh `G_MESH_HOME`. Connected with the same raw MCP client pattern
(`/tmp/gm395m/m3/mcp_idle_client.py`): `initialize`, `tools/list`, then held
the connection open for 60s calling no tool at all, then disconnected.

**Result**, `ps -axo pid,%cpu,time,command | grep -E 'g-mesh|bulk-index'`
(full, untruncated output, captured at the end of the 60s idle window, before
teardown):

```
93881   0.0   0:00.02  g-mesh mcp-shim   [M2's leftover shim from the concurrent M2 run, unrelated to M3]
93883 400.0  11:42.26  g-mesh daemon --project-root .../g-mesh   [M2's daemon, mid-backfill, unrelated to M3]
94135-95048  (various)                                            [M2's plugin processes, unrelated to M3]
95907   0.0   0:00.09  g-mesh daemon --project-root /private/tmp/gm395m/m3/proj   [the M3 daemon]
```

The M3 daemon (pid 95907) is the only process this measurement is about: it
had accumulated **0.09s of CPU time** after 60+ idle seconds — well under 1s,
as the doc expects. No `--bulk-index` process and no plugin process was
running for the M3 project (the plugin processes visible above all belong to
M2's concurrent, unrelated repo). `/tmp/gm395m/m3/daemon.log` contained only
`g-mesh daemon: index (re)initialized - a full reindex is needed` — no
"initial index built" line, confirming the walk was never triggered, exactly
matching D2 (activation only on an index-needing tool call, and none was
made).

**Result: matches the doc's expectation exactly** — one daemon, CPU time
under 1s, no `--bulk-index`, no plugin process.

M3(b) needs slice 5 (the front-daemon/session-switch work) and is out of
scope for this task, per the task's own instruction.

## Deviations from the doc, summarized

1. M1's hold knob: used `G_MESH_BULK_INDEX_DELAY_MS` (`WALK_DELAY_ENV`)
   instead of `G_MESH_BULK_INDEX_HOLD_FILE` (`WALK_HOLD_FILE_ENV`), because
   the latter (and its embedding-pass sibling) is hard-capped at 30s
   regardless of file removal, which would have made a 180s release
   meaningless. No background hold-file-removal process was needed as a
   result.
2. `claude -p`'s `--allowedTools` needed `=value` syntax, not a
   space-separated value before the positional prompt, or the prompt was
   swallowed by the variadic option and the CLI refused to run.
3. M1 used `--output-format stream-json --verbose` (not in the doc) to get
   exact per-attempt error text and timestamps; this is additive, not a
   change to what was measured.
4. M2 and M3(a) were driven with a raw MCP client (a small Python script)
   rather than `claude -p`, as the task instructions allowed. Raw-client
   `tools/call` requests carry no `progressToken`, so `progressToken=absent`
   in M2/M3(a)'s logs reflects the client, not the server.
5. Step 6 of M1 (interactive backgrounding) was skipped per the task's
   scope.
6. M2's "time to first `get_file_outline` answer" is reported as two
   numbers (daemon-tracked wait vs. client-observed wall clock) rather than
   one, because they differed by ~70s and the gap traces to an ~80s
   0%-CPU window in the daemon process — see the M2 section above.

## Process cleanup

`g-mesh stop` was run against each `G_MESH_HOME` used (M1's 7 reps, M2, M3),
confirmed by its own "stopped the daemon" + child-process report each time.
Final check, full output:

```
$ ps -axo pid,command | grep -E 'g-mesh (daemon|mcp-shim)|bulk-index' | grep -v grep
$ ps -axo pid,command | grep -i 'gm395m' | grep -v grep
```

Both empty — no daemon, shim, bulk-index, or gm395m-rooted process left
running. (Two long-lived `rust-analyzer`/`rust-analyzer-proc-macro-srv`
processes remain on the machine with an uptime of ~14h48m, parented by an
unrelated pre-existing interactive session (pid 1680) — not started by this
measurement and left alone.)
