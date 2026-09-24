# Plugin lifetime: plugins die with their daemon (GM-397)

Status: **design, awaiting owner review.** No code for it has been written yet.
Branch: `fix/GM-397-plugins-die-with-daemon`, cut from `release-3.12.0`.

Each later slice is done by an agent that knows only this document, so the
document says everything that slice needs. Line numbers refer to the tree at
`release-3.12.0` (`2049f8f`). Treat them as a starting point for search, since
a slice that runs earlier will move them.

## 0. Problem and acceptance criteria

GM-393 stopped a `g-mesh daemon` with SIGTERM and then SIGKILL. Its
`g-mesh-plugin-go --bulk-index` child kept running until someone killed it by
pid. Nothing records that pid anywhere (see §1.1), so `g-mesh stop` could not
have found it either. This is the same family as the leaked daemons seen in
bench runs.

Requirement: a daemon that dies for any reason, SIGKILL included, leaves no
plugin processes behind.

Acceptance:

- SIGKILL of a daemon in the middle of a bulk index leaves no plugin process
  within a few seconds, for every bundled plugin (go, rust, python,
  typescript).
- There is a test that fails on the current code.

This document also covers the long-lived plugin, because it has the same gap
(§1.3).

## 1. Current behaviour

### 1.1 How plugins are spawned

**Bulk index (`--bulk-index`).** The spawn is at `core/src/daemon/bulk_index.rs:344-352`,
in `walk_one_language`:

- `stdin(Stdio::null())`: the plugin's stdin is `/dev/null`. No plugin reads
  it, and it could not tell the daemon's death from it anyway.
- `stdout(Stdio::piped())`: an NDJSON stream that the daemon reads until EOF
  (`ingest`).
- `stderr(Stdio::inherit())`: the daemon's own stderr, which is the log file or
  `/dev/null`.
- There is no `process_group` call, so the plugin runs in the daemon's process
  group. The daemon is the leader of that group, because the shim detaches it
  with `process_group(0)` (`core/src/process.rs:144-147`,
  `core/src/shim.rs:432-443`).
- There is no pid file. The `Child` is local to `walk_one_language`. The only
  kill is on an ingest error (`bulk_index.rs:365-371`), and after that the only
  step is `child.wait()` (`:373`).
- Languages are walked one at a time (`bulk_index.rs:237-252`), so at most one
  bulk child exists at a time. `plugin_check` spawns a bulk child the same way,
  with stdin also `/dev/null` (`core/src/cli/plugin_check/session.rs:349-363`).

**Long-lived plugin.** The spawn is `PluginState::spawn` at
`core/src/daemon/plugin.rs:791-807`:

- stdin and stdout are pipes carrying framed JSON-RPC. stderr is inherited.
- It stays in the same process group as the daemon.
- There is one pid file per language, `plugin-<lang>.pid`. The registry writes
  it (`registry.rs:146` `discovered_pid_files` lists them).
- The graceful end is `PluginState::end` (`plugin.rs:850-869`). It drops both
  pipes ("closing stdin is the please-exit signal"), waits for `grace`, then
  kills.

**What the daemon does at exit.** The graceful exits go through `supervise`
(`core/src/daemon/lifecycle.rs:1148-1210`): the orphan check and the core idle
timeout. Both call `registry.sleep_all_now`, which ends every long-lived plugin
through `end()`, then `release_state_files` (`lifecycle.rs:1225-1248`). That
function only deletes files. Neither path touches a bulk child that is in
flight: the walk thread is simply abandoned when the process returns.

The daemon installs no signal handler (`lifecycle.rs:121-137`,
`process.rs:8-24`). SIGTERM kills it as abruptly as SIGKILL does, and no daemon
code runs on either one. On those paths the daemon cleans up nothing at all.
What ends a plugin today is the kernel closing the daemon's end of each pipe.

### 1.2 What each plugin does when its pipes go away

| | stdin read? | EOF noticed while busy? | Write to a closed stdout | Output buffering | Grandchildren |
|---|---|---|---|---|---|
| **go**, bulk | no (`/dev/null`) | n/a | SIGPIPE kills the process, because Go does this for fd 1 unless `signal.Notify` is used and the plugin never calls it | `bufio.Writer` 4 KiB (`plugins/go/bulkindex.go:49`) | none (walk + `go/parser` only) |
| **go**, long-lived | yes, on the **main** goroutine (`plugins/go/main.go:91-111`) | **no**. It is read only between requests. | SIGPIPE, as above | unbuffered frames | `go list` via `packages.Load` (`semantic.go:316`) during a semantic pass |
| **rust / python** (SDK), bulk | no | n/a | EPIPE, because the Rust runtime ignores SIGPIPE. `bulk_index` returns 1 (`plugins/sdk/src/run.rs:139-145`) | `BufWriter` 8 KiB (`run.rs:131`) | none |
| **rust / python** (SDK), long-lived | yes, on the **main** thread (`run.rs:202-224`) | **no**. It is read only between requests. | EPIPE, then `return 1`, then `Session` is dropped, then `LspClient::drop` shuts the server down (`sdk/src/lsp/client.rs:500-541`) | unbuffered frames | rust-analyzer / pyright (`lsp/client.rs:156-165`), plus their own children (proc-macro-srv, `cargo`) |
| **typescript**, bulk | no | n/a | EPIPE is emitted as an `'error'` event. `waitForDrain` swallows it (`plugins/typescript/src/bulkIndex.ts:59-72`), and later writes to the destroyed stream fail silently. **The walk runs to the end.** | per line, async | none |
| **typescript**, long-lived | yes, event-driven (`index.ts:263`) | **yes**. `'end'` fires even while a semantic pass is awaiting tsserver (`index.ts:290-292`). | n/a (exits on `'end'`) | unbuffered frames | tsserver. It is killed by the exit hook (`semantic.ts:241-279`) and also exits on its own stdin EOF. |

"How long can it compute without writing" decides how long a plugin outlives
its daemon:

- **go, bulk**: `loadWorkspace` + `walkProjectFiles`, which lists the whole
  tree before the first write (`bulkindex.go:48,63`). After that, it is any
  stretch that produces less than 4 KiB. A project with **no or few `.go`
  files** never fills the buffer at all, and the final `w.Flush()` of an empty
  buffer makes no syscall. In that case the plugin lives exactly as long as
  the walk and then exits 0. This is the most likely GM-393 shape, where the
  Go plugin was walking a large non-Go tree. That is an inference, not a
  reproduction.
- **SDK, bulk**: `load_project`, then any stretch under 8 KiB. The zero-files
  case behaves the same as Go.
- **typescript, bulk**: the whole walk, whatever it writes, because EPIPE does
  not stop it.
- **go / SDK, long-lived**: the request in hand. A whole-project
  `semanticPass` may run up to core's ceiling, which is `max(20 min, 10 s ×
  files)` (`core/src/daemon/plugin.rs:196`). The SDK bridge budgets itself to
  stay inside that (`sdk/src/lsp/bridge.rs:27-36`), and the language server
  runs alongside it the whole time. For Go this also covers a long
  `packages.Load` on a large module.
- **typescript, long-lived**: only synchronous stretches (one file's
  tree-sitter pass). These are short.

### 1.3 Measured on 2026-09-24 (macOS, `target/debug`)

A throwaway script started `g-mesh daemon --project-root <p>` with a temporary
`G_MESH_HOME` (`/private/tmp/gm397/...`, which is short because `sun_path` is
104 bytes) and triggered activation over the socket. For bulk runs it held the
first batch commit with `G_MESH_BULK_INDEX_HOLD_LOCK_FILE`. The fixture was
300 files × 15 functions of one language, which is enough to fill the pipe and
block the plugin on a write. The script then SIGKILLed the daemon and ran `ps`
on the recorded plugin pids 5 s later. The machine was heavily loaded
(`uptime` load average 595), so no timing claims are made here beyond "gone
within 5 s".

| Plugin × mode | State at SIGKILL | Alive 5 s later? | Evidence |
|---|---|---|---|
| go, bulk | blocked writing to a full pipe | no | killed by SIGPIPE, silently |
| go, bulk | SIGSTOPped, SIGCONT 3 s after the kill | no | resumed its blocked write and got SIGPIPE |
| rust, bulk | blocked writing | no | `[rust] failed to write the bulk stream: Broken pipe (os error 32)` |
| python, bulk | blocked writing | no | `[python] failed to write the bulk stream: Broken pipe` |
| typescript, bulk | awaiting `drain` | no, **but only because the walk finished** | logged `bulk index complete: 300 files, 4800 nodes, 13500 edges` *after* its reader was dead |
| go / rust / python / typescript, long-lived | idle, after their post-walk semantic passes | no (all four) | stdin EOF, then a clean exit |

After every run, `ps -A | grep -E 'gm397\|--bulk-index\|g-mesh-plugin'`
returned nothing, and `/private/tmp/gm397` was removed.

**Conclusion.** Today a plugin dies with its daemon only when it next *writes*
(bulk) or next *reads* (long-lived). Until then nothing tells it the daemon is
gone:

- **Blocked writer**: dies promptly.
- **Silent walk (bulk)**: lives until the walk ends. A zero-file language or a
  long pre-walk phase makes this minutes, and there is no pid file to find the
  process by.
- **Busy long-lived go / SDK plugin**: lives until the request ends, up to
  about 20 min, with its language server alongside.
- **typescript, bulk**: lives until the end of the walk, always.

A second, rarer way to survive: on macOS, Rust std creates pipes with `pipe()`
followed by a separate `fcntl(FD_CLOEXEC)`, not atomically with `pipe2`
(`library/std/src/sys/pipe/unix.rs:33-40`). Its `posix_spawn` path does not
set `POSIX_SPAWN_CLOEXEC_DEFAULT` either. So a spawn on another daemon thread
that lands in that window inherits the pipe end. A sibling holding the read
end of a bulk child's stdout means that child never gets EPIPE at all. The
window is microseconds. It is not reproduced here.

## 2. Options and the decision

| Mechanism | SIGKILL of daemon | Graceful exits | Grandchildren | macOS / Linux / Windows | Cost |
|---|---|---|---|---|---|
| **A. stdin is the lifeline**: every plugin exits on stdin EOF, noticed by a dedicated reader | yes | yes, since process exit closes the pipe | via each plugin's own teardown plus servers' stdin EOF (see below) | same on all three (pipe EOF) | core: 1 line per bulk spawn site. SDK once (rust, python and toy get it free), Go once, TS once. |
| B. Process group owned by the daemon, `killpg` on exit | **no**: no daemon code runs on SIGKILL or SIGTERM (§1.1) | yes | yes, unless a server calls `setsid` | Unix only | small, but it guards a path that `end()` already covers |
| C1. Linux `PR_SET_PDEATHSIG`, set by the daemon in `pre_exec` | yes | yes | **no**: cleared on `fork`, so a grandchild does not inherit it | Linux only | one place in core. However, it fires when the *spawning thread* dies, not the process, and plugins are spawned from request, watcher and relaunch threads. Every spawn would have to move to one long-lived thread, or plugins get killed at random. It also forces `fork` instead of `posix_spawn`. |
| C2. macOS kqueue `EVFILT_PROC`/`NOTE_EXIT` on the parent, or polling `getppid()` for a change | yes | yes | no | macOS (kqueue), Unix (ppid) | per plugin, per OS. Node has no kqueue API, so it would be ppid polling there. |
| C3. Windows Job Object, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` | yes | yes | **yes**: children of a job member join the job unless breakaway is allowed | Windows only | one place in core. Putting the *daemon itself* in the job at startup avoids the assign-after-spawn race. |
| D. Next daemon start kills leftovers from pid files | eventually, if a daemon ever starts again | n/a | no | all | bulk children have no pid file. pid reuse needs a start-time check. The delay is unbounded, so it misses "within a few seconds". |

**Decision: A, in both modes, on every platform. Plus two cheap hardenings.
No OS-specific mechanism.**

1. **Core, bulk mode.** `walk_one_language` (`bulk_index.rs:348`) and
   `plugin_check`'s bulk run (`session.rs:359`) spawn with
   `stdin(Stdio::piped())`, keep the `ChildStdin` inside the `Child`, and
   never write to it.
   - `Child::wait` closes stdin before it waits, which is harmless: by then
     stdout has hit EOF, so the plugin is already exiting.
   - Core also sets `G_MESH_BULK_STDIN_LIFELINE=1` on that spawn. A plugin arms
     the bulk-mode watcher **only** when that variable is set.

   Without the gate, a plugin run by an older core, by
   `plugin --bulk-index . < /dev/null`, or by a harness that hands it a closed
   pipe would see EOF at once and exit before walking. The gate makes the
   contract opt-in by the spawner, it is portable (no `isFIFO` probing, which
   is uncertain for Node on Windows), and it gives the tests a clean control
   (§3).
2. **Plugins, bulk mode.** A watcher reads and discards stdin. On EOF it exits
   the process immediately with code 1 and a stderr line
   (`core closed the bulk stream's lifeline - exiting`).
   - SDK: a `std::thread` in `run()` (`plugins/sdk/src/run.rs:92-106`).
   - Go: a goroutine started in `main.go:42`.
   - TS: `process.stdin.on("end", ...)` plus `resume()` in the bulk branch
     (`index.ts:240-250`). TS then has to **destroy/unref stdin once the walk
     resolves**. Otherwise the open stdin handle keeps the event loop alive,
     the plugin never exits, stdout never reaches EOF, and core waits forever.
     This is the one deadlock this design can introduce, and the positive test
     in §3 guards it.
3. **Plugins, long-lived mode.** Frame reading moves off the work thread. A
   reader thread (SDK) or goroutine (Go) parses frames into a channel, and the
   main loop consumes them as it does today.
   - On EOF the reader posts `Closed`. An idle main loop takes the existing
     graceful path (`LspClient` shutdown).
   - If the main loop has not exited within a short grace (proposed 1 s), the
     reader ends the process itself. The SDK first kills the engine's `Child`,
     which is held in an `Arc<Mutex<Option<Child>>>` that the reader can reach,
     because `process::exit` skips `LspClient::drop`.
   - TS already works this way (`index.ts:290-292`) and does not change.
4. **Hardening, TS bulk.** Exit on `process.stdout` `'error'` (EPIPE), rather
   than letting `waitForDrain` swallow it (`bulkIndex.ts:59-72`). Then a
   TypeScript walk also stops on the write path, as Go and SDK walks already
   do.
5. **Hardening, core.** Serialize every `Command::spawn` in the daemon behind
   one process-wide mutex, in a helper in `core/src/process.rs` used by
   `plugin.rs:791` and `bulk_index.rs:344` (plus `plugin_check`'s two spawns; `manifest.rs:886` is test code). This matters on
   targets without `pipe2`, which includes macOS. It closes the §1.3 race in
   which a sibling inherits a lifeline pipe end. That leak would now defeat A,
   and in the worst case it is a cycle: two plugins each holding the other's
   stdin write end, so neither ever sees EOF. Spawns are rare, so the lock
   costs nothing measurable.

**Grandchildren under A.**

- **tsserver**: already covered by the exit hook and its own stdin EOF
  (`semantic.ts:241-252`).
- **SDK language servers**: killed explicitly in step 3. Even if the plugin
  itself is SIGKILLed, their stdin is a pipe whose only writer was the plugin,
  so they get EOF. GM-320 observed a live rust-analyzer tree go down with its
  plugin this way (`lifecycle.rs:127-131`).
- **Go's `go list`**: not killed. It finishes, or it dies of SIGPIPE on its
  next write to the dead plugin. That is bounded, and it is the same as today
  for a crashed plugin.

A covers what C3 would add on Windows except a server that ignores stdin EOF.
That is why C3 is left as an open question, not part of this task.

**Platforms covered:** macOS, Linux, Windows, identically. A pipe's EOF is the
only signal used, and it is delivered by the kernel even on SIGKILL or
`TerminateProcess`.

**Risks.**

- The TS event-loop deadlock described in step 2.
- A third-party plugin that ignores stdin in bulk mode keeps today's behaviour.
  This is documented as a contract clause, not enforced.
- Step 3 changes the threading of both control loops, which is the largest
  diff here. It is still mechanical: the frame parser and handlers do not
  change, only where `read_frame` runs.

## 3. Test plan

### 3.1 The hold knob: a real plugin kept alive and silent

The existing knobs hold the *daemon* (`G_MESH_BULK_INDEX_HOLD_FILE`,
`..._HOLD_LOCK_FILE`, `bulk_index.rs:85-106,578`). None of them can hold a
*plugin* in a state where it neither reads nor writes, which is exactly the
state that leaks.

New test-only knob: **`G_MESH_PLUGIN_HOLD_DIR`**. Plugins inherit the daemon's
environment. When the knob is set, a plugin at hold point `P` checks for
`<dir>/<P>-<language>.hold`. If that file exists, the plugin:

1. writes its own pid to `<dir>/<P>-<language>.pid` (write to a tmp file, then
   rename);
2. waits while the hold file exists, polling every 10 ms and bounded at 60 s.
   The wait neither reads stdin nor writes stdout.

There are two hold points:

- `bulk`: in bulk mode, after the project/workspace load and before the first
  write. That is SDK `run.rs:130`, Go `bulkindex.go:49`, and TS
  `bulkIndexProject` before its loop (`bulkIndex.ts:260`).
- `semantic`: at the top of the `semanticPass` handler, before any engine
  starts. That is SDK `Session::handle`, Go `control.go:240`, and TS
  `index.ts:156`. The daemon sends that request itself after the walk (the log
  shows `semantic pass requested for the whole project` for every language).

The Go and SDK holds block their thread, as a real computation does. The TS
hold is an async `setTimeout` loop, because TS's real waits (fs, tsserver) are
async too. There is precedent for test-only knobs in shipped binaries in
core's `HOLD_*` knobs.

The pid file is also the only way a test can learn a bulk child's pid, since
bulk children have no pid file of their own.

### 3.2 Tests: new file `core/tests/plugins_die_with_daemon.rs`

All tests share one pattern, modelled on `core/tests/daemon_sigterm.rs`:

- a temp project with one file per language (one `.go`, `src/lib.rs` +
  `Cargo.toml`, `.py`, `.ts`);
- a shim bootstrap with `G_MESH_PLUGIN_HOLD_DIR` set on the shim, which the
  daemon inherits;
- `common::trigger_activation`, then a wait for the pid file with
  `common::wait_for` / `startup_timeout()`;
- `process::force_stop(daemon)`, which is SIGKILL on Unix and
  `TerminateProcess` on Windows;
- an assertion that `daemon::is_process_alive(plugin_pid)` turns false within
  **5 s**.

The plugin is reparented to init, so a zombie cannot fake "alive". `Drop`
removes the hold files and kills any pid left over, so a failing test cannot
leak a process.

| Test | Hold file | Fails on current code (knob only)? | Control: the revert that must make it fail |
|---|---|---|---|
| `bulk_<lang>_plugin_dies_with_a_killed_daemon`, ×4 (go, rust, python, typescript) | `bulk-<lang>.hold` | **yes, all four**: stdin is `/dev/null` and the hold never writes | (a) Delete that plugin's bulk stdin watcher. (b) Core half: revert `bulk_index.rs` to `Stdio::null()` / drop the `G_MESH_BULK_STDIN_LIFELINE` env. Because the watcher is gated, the plugin no longer arms and all four fail. |
| `long_lived_<lang>_plugin_dies_with_a_killed_daemon`, ×4 | `semantic-<lang>.hold` (the pid is cross-checked with `plugin-<lang>.pid`) | **go, rust, python: yes** (main thread blocked, stdin unread). **typescript: no.** TS is already correct in this mode, and the test guards it. | go / SDK: move `read_frame` back onto the main loop, i.e. remove the reader thread. typescript: delete `process.stdin.on("end")` (`index.ts:290`). |
| `bulk_walk_still_completes_with_its_lifeline_open` (positive) | none | no (passes today) | (a) Keep the TS watcher but remove the post-walk stdin destroy/unref: the walk never finishes, and the test fails on `wait_until_indexed_within`. (b) Arm the watcher without the env gate while core still passes `/dev/null`: every plugin exits at once and the daemon reports the bulk index failed. |

The positive test also asserts one indexed file node per language. Every
control is demonstrated in a throwaway `git worktree`, never in the main
checkout. The run log (the red run on the knob-only tree, the green run, and
each control's red run) goes into the S3 completion summary.

A grandchild assertion is **not** included. The only realistic grandchildren
are real language servers, and CI may not have rust-analyzer or pyright. A test
that skips itself when they are missing is exactly the never-executed test
GM-335 shipped. See Q3.

## 4. Implementation slices

**S2: hold knob and red tests.**

- Files:
  - `plugins/sdk/src/run.rs`, plus a small `hold.rs` module in the SDK;
  - `plugins/go/bulkindex.go`, `plugins/go/control.go`, plus a new
    `plugins/go/hold.go`;
  - `plugins/typescript/src/bulkIndex.ts`, `plugins/typescript/src/index.ts`,
    plus a new `plugins/typescript/src/testHold.ts`;
  - `core/tests/plugins_die_with_daemon.rs`.
- Rebuild `plugins/go/g-mesh-plugin-go` and `npm run build`.
- Exit criterion: `cargo test -p g-mesh --test plugins_die_with_daemon`
  reports these tests failing with "still alive after 5 s": `bulk_*` ×4 and
  `long_lived_{go,rust,python}`. `long_lived_typescript` and the positive test
  pass. The pid files prove each hold was actually reached, not just set up.

**S3: the lifeline, green and with controls.**

- Files:
  - `core/src/daemon/bulk_index.rs:344-352` and
    `core/src/cli/plugin_check/session.rs:349-363`: piped stdin plus the env
    variable;
  - `core/src/process.rs`: the spawn lock helper, used by `plugin.rs:791`,
    `bulk_index.rs:344` and `plugin_check/session.rs:349,934`;
  - `plugins/sdk/src/run.rs` (bulk watcher; control-plane reader thread and
    engine kill) and `plugins/sdk/src/lsp/client.rs`: a shareable `Child`
    handle;
  - `plugins/go/main.go` and `bulkindex.go`;
  - `plugins/typescript/src/index.ts` (bulk watcher and post-walk stdin
    destroy) and `bulkIndex.ts` (exit on stdout error);
  - the plugin contract in `docs/architecture/multi-language-plugins.md`: in
    bulk mode, when `G_MESH_BULK_STDIN_LIFELINE=1`, stdin is a pipe core never
    writes, and its EOF means core is gone, so exit. In long-lived mode, stdin
    EOF means exit even mid-request.
- Exit criterion: the whole `plugins_die_with_daemon` file is green. Each
  control in §3.2 is shown red. The full `cargo test --workspace`, `go test
  ./...` and `npm test` pass. `§1.3`'s experiment re-run on the fixed build
  shows every plugin gone and a clean `ps`.

## 5. Open questions for the owner

**Resolved 2026-09-24:** the owner accepted the design and every
recommendation below ("да, давай так"). The spawn lock ships, marked in its
doc comment as an unverified hardening. The Windows Job Object stays out of
GM-397. A grandchild (language-server) test is not written now; if wanted, it
becomes its own task with a fake EOF-ignoring LSP server.

1. **The spawn lock (decision item 5) has no deterministic test.** The race is
   microseconds wide. It is in the design because the std source shows it
   (`pipe/unix.rs:33-40`), not because it was observed.
   *Recommendation:* ship it in S3, labelled in its doc comment as untested
   hardening. The alternative is to leave it out and accept a rare cycle that
   defeats the fix.
2. **Windows Job Object (C3).** It adds only one thing over A on Windows:
   killing a language server that ignores stdin EOF, and only on Windows.
   *Recommendation:* not in GM-397. Revisit only if a surviving server is ever
   observed.
3. **No grandchild test.** *Recommendation:* accept for GM-397. If one is
   wanted, the honest version is a fake LSP server configured through a test
   manifest's `[plugin.semantic] command` that **ignores** stdin EOF, so it
   proves the SDK's explicit kill rather than a real server's good manners. It
   would be a separate small task.
