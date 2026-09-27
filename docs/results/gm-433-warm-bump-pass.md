# GM-433 warm-RA bump pass: before and after the fix

Slice S6 (measure) of GM-433. It re-runs GM-429/S1's "warm-RA bump"
scenario (`docs/results/gm-429-semantic-pass-time.md` on
`perf/GM-429-semantic-pass-time`) against the fix (`5455903`: a
`workspaceChanged` unlatches the client's settle, and `ContentModified` is
re-asked once). A same-day before arm on the release tip's SDK (`805686f`)
ran alongside it. No product code was changed. The counters come from
temporary instrumentation in a throwaway worktree.

## Result

| arm | runs | requests per pass | deferred (empty) | content modified | complete | `semanticPassAt` set |
|---|---|---|---|---|---|---|
| before, GM-429/S1 b0-b4 (805686f, 28,535 questions) | 5 | 43,107-57,059 | 14,582-28,533 | yes in b0, b3, b4 | 2 of 5 | 2 of 5 |
| before, same day (805686f SDK, fresh version) | 3 | 57,263-57,265 | 28,624-28,626 | 0, 1, 2 | 1 of 3 | 1 of 3 |
| **after (5455903), fresh version** | 3 | **28,639** | **0** | **0** | **3 of 3** | **3 of 3** |

- The fix meets the acceptance criterion: 3 of 3 passes complete, each asks
  every question once (28,639 = the cold pass's own count on this commit;
  S1's 28,535 is for `805686f`, which has fewer call sites), and has no
  deferrals and no `ContentModified`.
- The same-day before arm reproduces S1's finding 1 on the same project,
  binary build and machine. Every question is asked twice. 2 of 3 passes
  end incomplete on `core/build.rs (content modified)`, and core logs "the
  rust semantic pass after a workspace reindex failed" and leaves
  `semanticPassAt` NULL.
- This is the control that tells the arms apart. The two arms differ only
  in `plugins/sdk` (the fix, or `805686f`'s version of it), and they differ
  in requests per pass and in the incomplete flag.
- End to end, the fix is not faster on this machine. It asks half as many
  questions but pays the full 2s settle, and the before arm's 28.6k extra
  answers are cheap empty ones from a busy server. The two arms overlap:
  bump to answered is 30.3-59.0s after the fix and 39.4-51.5s before, at
  load 5.6-11.6. The fix is a correctness change, so this was expected.

## Machine and method

- MacBook, Intel i7-1068NG7 (4 cores / 8 logical CPUs), macOS 26.6.2,
  rust-analyzer 1.97.1 (8bab26f4 2026-07-14). The same machine as S1.
- Load. At 15:51, before the build, the load average was 13 / 73 / 111
  (1, 5 and 15 minutes). Outside these runs it was at 108 at 15:57. The
  measured runs started at a 1-minute load of 5.6-11.6 (see the table).
  The owner's g-mesh daemons and a rust-analyzer were running and were
  left alone. They sat at 0% CPU at the end.
- Code. There were two throwaway detached worktrees at `5455903`:
  - A build worktree. It ran `npm ci && npm run build` in
    `plugins/typescript`, `npm ci` in `plugins/python`, and
    `cargo build --workspace --release` (264.7s, not counted). It held
    the instrumentation (41 lines in `plugins/sdk/src/lsp/{bridge,client}.rs`).
    For the before arm, `plugins/sdk` was checked out from `805686f` and
    given the same instrumentation, then rebuilt. The second rebuild
    restored the fix.
  - A project worktree P, which was indexed. It had `npm ci` in
    `plugins/typescript` so that P's `core/build.rs` does the real
    `npm run build`. P's `target/` started empty and was kept.
- Instrumentation. It adds `GM433 <epoch-ms>` lines to the file that
  `GM433_LOG` names. The lines come from three places:
  - `LspBridge::answer`: pass start, documents synced (question count),
    `wait_ready` done, end with the complete flag;
  - `run_pass`: requests sent, answers, empty-answer deferrals,
    `ContentModified` answers (after the fix; before it, refusals whose
    message is "content modified"), `ContentModified` deferrals,
    refusals, timeouts, and answering time;
  - `LspClient::track_progress`: every `$/progress` begin and end, with
    its title and the time it was read. Progress that RA sends between
    passes is read, and timestamped, when the next pass drains it, as in
    S1.
  - `workspace_changed` is logged after the fix.
- One run is scripted as one script:
  1. Fresh isolated `G_MESH_HOME` (`~/.gm433/<arm><n>`, because the
     scratchpad path makes the daemon socket longer than 103 bytes).
     `models` is linked to `~/.g-mesh/models`, and the embedding cache
     is a `.backup` copy of the owner's.
  2. Reset P's `core/Cargo.toml` and `Cargo.lock`.
  3. Start `g-mesh daemon --project-root P` under `/usr/bin/time -p`,
     then activate it with one `get_file_outline` call through
     `g-mesh mcp-shim`.
  4. Wait until `language_state.semanticPassAt` for rust is set (the
     first pass; RA is now warm), then wait 20s more.
  5. Record `uptime` and set t0.
  6. Bump `core/Cargo.toml`'s `version`.
  7. Wait for the rust whole-project pass's end line, then 8s.
  8. Read `semanticPassAt` and `semanticPassError` back from `index.db`.
  9. `g-mesh stop`, then SIGKILL every process in the daemon's tree. The
     tree was collected before the stop. `ps` found none alive afterwards.
- **The bump must go to a version P's `target/` has never built.** The
  first round bumped to 3.15.1-3.15.3 in both arms, but a dry run and the
  first fix run had already built them. With those build scripts cached,
  RA finished its whole reload (compile-time deps, crate graph, roots,
  proc-macros, indexing) within ~5s. That is before core even asked for
  the pass, which came 4.9-5.4s after the bump. So neither arm could show
  the bug. Four before-arm runs in that shape were all complete, with
  28,639 requests and 0 deferrals. They measured nothing and are not in
  the table. The runs above bump to 3.16.1-3.16.3 (before) and
  3.17.1-3.17.3 (after). RA's "Building compile-time-deps" then lasts
  until 7.0-12.1s after the bump, as in S1 (build scripts 7.8-11.6s).

## Per run

Times are in seconds from the bump (t0). "Ready" is when `wait_ready`
returned. "Deps end" is when RA's "Building compile-time-deps" ended.
"Last RA end" is the end of its final "Indexing".

| arm | run | version | load1 | t0 to pass | deps end | ready | last RA end | RA progress after ready | answering | t0 to answer end | requests | deferred | content modified | complete | `semanticPassAt` | daemon `time -p` real / user / sys |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| before | 1 | 3.16.1 | 5.61 | 4.3 | 10.39 | +10.4 | 13.53 | 4 begins, 2.3s | 29.0 | 39.4 | 57,265 | 28,626 | 0 | yes | set | 143.82 / 32.65 / 3.76 |
| before | 2 | 3.16.2 | 11.06 | 6.3 | 10.18 | +10.2 | 14.31 | 4 begins, 2.9s | 41.3 | 51.5 | 57,263 | 28,624 | 1 (`core/build.rs`) | **no** | NULL | 159.24 / 37.54 / 3.97 |
| before | 3 | 3.16.3 | 6.59 | 5.0 | 9.78 | +9.8 | 12.56 | 4 begins, 2.1s | 33.9 | 43.7 | 57,264 | 28,625 | 2 (`core/build.rs`) | **no** | NULL | 151.34 / 36.92 / 4.10 |
| after | 1 | 3.17.1 | 11.60 | 6.3 | 12.10 | +17.8 | 15.75 | 0 | 36.6 | 54.4 | 28,639 | 0 | 0 | yes | set | 158.81 / 39.80 / 4.42 |
| after | 2 | 3.17.2 | 6.45 | 4.8 | 7.04 | +12.4 | 10.37 | 0 | 17.9 | 30.3 | 28,639 | 0 | 0 | yes | set | 130.47 / 32.00 / 3.64 |
| after | 3 | 3.17.3 | 8.08 | 6.8 | 11.48 | +17.8 | 15.75 | 0 | 41.2 | 59.0 | 28,639 | 0 | 0 | yes | set | 167.85 / 36.72 / 4.22 |

- Before: `wait_ready` returns the moment "Building compile-time-deps"
  ends (+0.00-0.01s; the client is still latched from the first pass).
  RA then runs "Building CrateGraph" (ended 50-70ms later), and after it
  "Roots Scanned", "Loading proc-macros" and "Indexing", for 2.1-2.9s.
  This is S1's 13ms gap and its 3.4-4.5s of busy time. The
  `ContentModified` answers arrive within 0.15s of the deps phase
  ending, all for `core/build.rs`, as in S1 (x3).
- After: `wait_ready` waits through all of RA's reload phases and returns
  exactly one settle (2.0s) after the last "Indexing" ends. No progress
  begins after ready.
- "Answering" is `run_pass` wall time. Before the fix it includes waiting
  for the deferred half. After-arm run 2's answering time (17.9s) is
  about half the other runs'. Its load at the start was the lowest, but
  nothing else here explains it, and it did not change any counter.
- The `time -p` user/sys is the daemon only. It does not include the
  plugins or rust-analyzer; see S1's method.
- The cold first pass on each daemon asked 28,639 questions (28,627
  planned plus 12 implementation follow-ups) with 0 deferrals, in every
  run of both arms.
