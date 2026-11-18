# GM-725: daemon misses edits while FSEvents is starved (diagnosis, S1)

Follow-up to GM-720 section 5. Question: during a filesystem-event flood that
starves FSEvents, does an MCP query about a file edited during the starvation
answer from fresh data?

## 1. Verdict

**Partly fresh.** The three file-anchored tools (`get_file_outline`,
`find_definition` with `file_path`, `get_dependencies` with `file_path`) answer
fresh for the file they name: the query-time staleness check reindexes it
inline. Every symbol-anchored query (`find_definition` by name,
`find_references`, `find_callers`, `find_callees`, `find_implementations`,
`search_code`) stays stale for an edited file that no file-anchored query has
named, for as long as the starvation lasts. In this run that was the whole flood
plus 10-39 s after it ended. A by-name lookup of the new symbol fails. Worse, it
can answer with "closest by meaning" neighbours instead, marked not an error
(section 4).

## 2. What the code does

- `core/src/mcp/mod.rs:387` `ensure_file_fresh` is the only query-time
  freshness hook. It is called from `find_definition` (`:675`, only when
  `file_path` is given), `get_file_outline` (`:763`) and `get_dependencies`
  (`:782`, only with `file_path`). Its doc (`:375`) says symbol-anchored tools
  skip it on purpose: "their files are known only after the query runs".
  `find_references` (`:687`), `find_callers` (`:702`), `find_callees` (`:717`),
  `find_implementations` (`:735`) and `search_code` (`:792`) never consult
  staleness. Checked with g-mesh `find_callers` on
  `GMeshMcpServer::ensure_file_fresh`, which returned exactly these 3 callers.
- It checks **only the named file**:
  `daemon::registry` `ensure_fresh_with` (`core/src/daemon/registry.rs:1851`) ->
  `staleness::is_stale` (`core/src/ingest/staleness.rs:237`) -> `decide`
  (`:263`). That is `stat` mtime against the `indexed_files` baseline, and on a
  mismatch a SHA-256 of the content against the recorded hash.
- On a stale file it **reindexes inline before answering**:
  `PluginSupervisor::ensure_fresh` (`core/src/daemon/lifecycle.rs:475`) ->
  `staleness::ensure_fresh` (`core/src/ingest/staleness.rs:157`) ->
  `apply_file_change_in`, then writes the new baseline. A failure is logged and
  the query answers from the old index (`mcp/mod.rs:420-425`).
- The only other disk reconcile is at daemon start (`registry.rs:1379`).
  Nothing re-checks the disk periodically. A starved FSEvents stream raises no
  `Rescan` either (GM-720 section 2), so `watcher/rescan.rs` never fires.

**Prediction before running.** The file-anchored tools return fresh results for
the edited file. Symbol-anchored queries, and file-anchored queries on any
*other* file, return stale results until the watcher delivers.

## 3. Reproduction

- Build: `cargo build --release -p g-mesh -p g-mesh-plugin-rust` in this
  worktree (2194be21). Cold: real 516.33, user 94.63, sys 40.18. The installed
  binary was not used.
- Driver (`scratchpad/gm725s1/driver.py`, stdlib Python): runs
  `target/release/g-mesh mcp-shim` over stdio against a fresh scratch Cargo
  project (`src/{lib,a,b,c}.rs`). Each arm gets its own `G_MESH_HOME`. The
  plugin root holds only the Rust manifest. Env: `G_MESH_SEED=0`,
  `G_MESH_EDIT_TIMING=1` (the watcher logs `fs_event path=...` per delivered
  path, `core/src/daemon/watch_loop.rs:174`) and `G_MESH_TRACE_CALLS=1` (logs
  the outcome of each `ensure_fresh`).
- Load: GM-720's `churn.py` x16 (create+unlink of 500 files per loop), in a
  directory beside the project, not inside it.
- Steps per arm:
  1. `get_file_outline(a.rs)` to wait for the index.
  2. Under churn: start the flood, then wait 20 s.
  3. Append `alpha_new` to `b.rs` and `beta_new` to `c.rs`.
  4. Poll `find_definition(symbol_name=alpha_new)` for up to 30 s. This path is
     watcher-only. Count the `fs_event` lines.
  5. `get_file_outline(a.rs)`, a different file, then
     `find_definition(beta_new)`.
  6. `get_file_outline(b.rs)`, then `find_definition(b.rs, position)`.
  7. `find_definition(alpha_new)` and `find_definition(beta_new)` by name.
  8. Under churn: stop the flood and poll `beta_new` by name for up to 60 s.
- Starvation is shown independently of the query results: the daemon's own
  watcher log shows 0 `fs_event` lines for `b.rs`/`c.rs` during the flood.
- Every daemon/plugin process started was killed by process group. None were
  left running (`left_running: []`). No process from the release build
  remained afterwards.

## 4. Results

The 1-minute load average comes from `uptime` at arm start. The machine already
carried 10-100 load from other work.

| Arm | Load (1 min) | Watcher: `alpha_new` by name | `fs_event` b/c during flood | Other file's outline, then `beta_new` by name | `get_file_outline(b.rs)` has `alpha_new` | `find_definition(b.rs, pos)` | `alpha_new` by name after outline(b.rs) | `beta_new` by name | `beta_new` after flood stops | real / user / sys |
|---|---|---|---|---|---|---|---|---|---|---|
| ctl1 | none (load 10) | found at 1.05 s | 2 / 2 | found | yes | fresh | found | found | n/a | 8.13 / 0.46 / 0.23 |
| B: churn after index (stream existed before) | churn x16 (load 10 -> 105) | **not found in 30 s** | **0 / 0** | **not found** | **yes** | **fresh** | found | **not found** | 10.36 s | 68.34 / 15.23 / 157.98 |
| C1: daemon started under churn | churn x16 (load 105) | **not found in 30 s** | **0 / 0** | **not found** | **yes** | **fresh** | found | **not found** | 38.98 s | 94.90 / 15.46 / 156.51 |
| C2: daemon started under churn | churn x16 (load 94) | **not found in 30 s** | **0 / 0** | **not found** | **yes** | **fresh** | found | **not found** | 20.72 s | 76.78 / 15.35 / 160.63 |
| ctl2 | none (load 102, after the flood) | found at 1.13 s | 1 / 1 | found | yes | fresh | found | found | n/a | 10.25 / 0.39 / 0.21 |

`user`/`sys` of the churn arms is the 16 churn loops (children of the driver).
The no-churn arms cost 0.4 s of CPU in about 10 s.

Observations:

- **The control and the starved arms differ in one cheap observation.** The
  watcher-only by-name lookup of `alpha_new` is found at about 1 s with no
  churn, and not at all within 30 s under churn. The daemon's watcher log
  agrees: 0 events for the edited files.
- **The staleness check works as designed.** `ensure_fresh` traced
  `file=src/b.rs` with a reindex outcome. After that, a by-name lookup of
  `alpha_new` finds it, because the inline reindex updates the shared index.
  The trace outcome was `ReindexedNoPriorRecord`: the scratch files were
  younger than `WALK_BASELINE_MTIME_MARGIN` at the walk, so they had no
  baseline. A long-lived file would take the hash-mismatch branch, which ends
  in the same `apply_file_change` call.
- **Querying a different file does not help.** `get_file_outline(a.rs)` traced
  `AlreadyFresh` and left `c.rs` stale.
- **A stale by-name miss can look like an answer.** The first `beta_new`
  lookup returned `no symbol named 'beta_new' found` (`isError: true`). The
  next one returned `resolvedBy: semanticNeighbours` with nearby declarations
  and no error flag. An agent could take a wrong neighbour for the edited
  symbol.
- **The events are delayed, not lost, in these runs.** Each arm got one
  `fs_event` per file only after the flood stopped (10-39 s later). After that,
  `beta_new` was found without any file-anchored query.
- **New compared with GM-720:** a stream created *before* the flood starves
  too (arm B), not only one created during it (C1/C2).

Limits: the flood is synthetic (`churn.py`), not a real cargo build. It lasted
about 50-60 s per arm, so starvation longer than that, or real loss with no
delivery after the flood, was not observed. Each arm ran once (C twice).

## 5. Fix options

**F1. Starvation watchdog with a stat sweep.**
- How it works:
  - The daemon keeps "time of the last delivered event".
  - A canary helps detect starvation: a file the daemon touches every N s in a
    watched directory of its own state dir, with its own stream. FSEvents is
    served by one system `fseventsd`, so a canary that goes silent is a usable
    proxy. Without a canary, the sweep runs unconditionally every N s.
  - While starved, every N s the daemon stats all `indexed_files` rows plus a
    walk for new files, and sends mismatches through the same
    `apply_file_change` path the watcher uses.
- Benefit: bounds staleness for *every* tool at about N s, including files
  no query names and new files. It also covers real event loss, which the
  current design cannot detect at all.
- Risk:
  - It adds stat and walk I/O at exactly the moment the machine is busiest:
    50k files is about 0.1-0.5 s per sweep, and a walk costs more.
  - The canary is a proxy for this stream, not proof: GM-720 saw per-stream
    bimodal behaviour.
  - It needs a starvation threshold N, and tests for it would need the same
    flooded setup GM-720 found flaky.

**F2. Wider query-time check for symbol-anchored tools.**
- How it works:
  - Before answering, stat-check the files changed since the last delivered
    event. The daemon cannot know which files those are, so in practice this
    is a throttled full mtime sweep, at most once every M s per daemon, before
    any symbol-anchored query.
  - Optionally, also run `is_stale` on each file in the result rows.
- Benefit: a query never answers from a known-stale index. It is pull-based,
  so it costs nothing when nobody queries.
- Risk:
  - Query latency grows with project size, and with inline reindexes of every
    file the build or the agent touched.
  - It duplicates F1's sweep inside the request path.
  - The result-row variant alone does *not* fix the `beta_new` case: a new
    symbol is in no result row.

**F3. Accept as a limitation, and document it.**
- How it works: state in the tool descriptions/instructions that a
  file-anchored query (`get_file_outline` on the edited file) forces
  freshness, and that by-name answers can lag during heavy builds.
  Optionally, make a by-name miss say "the watcher has delivered no event for
  X s".
- Benefit: no new I/O and no new moving parts. Staleness is bounded by the
  end of the flood (10-39 s here).
- Risk:
  - Agents tend to query by name right after editing. During a long build they
    get misses or `semanticNeighbours` guesses.
  - Real event loss, if it ever happens, stays undetected until the daemon
    restarts.

## 6. Owner decisions

1. Fix or accept: F1 (watchdog sweep), F2 (query-time sweep), F3 (document),
   or F3 now and F1 later.
2. If F1 or F2: the threshold N (or M) and the largest project size the sweep
   must stay cheap on.
3. Whether a by-name miss that falls back to `semanticNeighbours` should say
   that the watcher has been quiet, independent of 1.
4. Whether to repeat the reproduction with a real parallel cargo build and a
   longer flood (more than 5 min) before choosing, to rule out real event loss.

## 6.1 Owner review (2026-11-18)

- Decision 1: "F3 сейчас, F1 в бэклог (Recommended)". F3 lands in this task:
  tool descriptions say a file-anchored query forces freshness and by-name
  answers can lag during heavy builds, and a by-name miss or a
  `semanticNeighbours` fallback says how long the watcher has delivered no
  event (decision 3 is folded into F3). F1 (starvation watchdog with a stat
  sweep) is backlog task GM-727; decision 2 (N, project size) moves there.
- Decision 4: "Нет, хватает (Recommended)": no repeat with a real cargo build.
- Review of the F3 code (S3, finding F-1, an idle project shows the hint on
  nearly every miss): "Мягкая формулировка (Recommended)". The hint stays on
  every miss past the threshold, worded as a neutral fact plus advice that
  holds both on an idle project and under starvation: "No file change
  reported for N s. If you just edited the file that should define it,
  get_file_outline on that file reads it fresh." The clock also counts
  events under ignored directories (finding F-2).

## 7. Artifacts

Scratchpad
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/7ee728f3-349d-4163-9680-c24d1b1a2660/scratchpad/gm725s1/`:
`driver.py`, `run.sh`, `churn.py`, `plugroot/rust/plugin.toml`,
`summary.txt`, and `runs/<arm>/{result.json,calls.log,daemon.log}`.
