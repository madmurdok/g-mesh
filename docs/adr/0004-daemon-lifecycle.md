# 0004. Daemon lifecycle: startup order, the singleton lock, two idle timers and the orphan exit

## Status
Accepted

## Context
A project's daemon is two components with very different costs
(`core/src/daemon/lifecycle.rs`; the architecture doc's "Lifecycle &
Operational Model" section in `docs/architecture/g-mesh-v1.md`):

- The **language plugin** (for JS/TS: tree-sitter plus the TS compiler API in
  one Node.js process; for Rust: a `rust-analyzer` tree that ramps to a
  563-580MB plateau and never gives it back) is expensive to hold and
  expensive to warm up.
- The **core** (socket listener, SQLite handle, fs watcher) is cheap, and
  registering fs watchers is a one-time cost per core lifetime.

Further problems ride on the same process:

1. A plugin can grow past what the user wants to pay for it.
2. A daemon can outlive the thing it serves: its project root deleted, or
   its own executable swept away (`cargo clean`, a deleted worktree, a
   `/tmp` build). Four such daemons were once found running at once on one
   machine, each holding a plugin tree plus a core that measured 1.4GB RSS,
   and all four had to be cleared by hand.
3. `g-mesh stop` cannot reach such an orphan: `cli::stop` resolves the
   project root from the current directory, which for an orphan no longer
   exists. `SIGTERM` itself was measured to work (0.19-0.30s on macOS in
   every configuration tried, the whole plugin tree going with the core), so
   the gap is an unreachable stop, not a swallowed signal.
4. Startup races the shim's bootstrap timeout, and two daemons (or a shim
   and a daemon) can race for the same project.

## Decision
1. **Two independent idle timers.** The plugin sleeps after
   `plugin.idleTimeoutMinutes` (default 1h: long enough to survive the pauses
   in one working session, short enough to give memory back over a long idle
   stretch). While it sleeps, file changes go into an ordered, de-duplicated
   dirty queue that the next request needing the graph replays, rather than a
   rescan of the project. The core exits after `daemon.coreIdleTimeoutHours`
   (default 24h) with no MCP traffic and no attached client; that timeout
   bounds OS resource accumulation (inotify watchers, sockets, SQLite
   handles) across many projects over a long uptime, not memory in normal
   use. A live connection holds the core open, because a core that exits
   under a connected client takes its whole tool surface with it.
2. **One periodic tick** (a quarter of the shorter timeout, clamped to
   50ms..30s) runs every check. No third timer is added for the memory limit.
3. **`[plugin] memoryLimitMb` is a circuit breaker on a sustained overage,
   not a ceiling.** A sampler cannot enforce a ceiling; a spike that ends
   before the next tick is not caught, by design. Because a process tree's
   membership is unstable (`rustc`, build scripts and `tsserver` forks come
   and go; one tree was measured to drop 170MB between two snapshots
   milliseconds apart), a single over-limit sample is not evidence: the
   breaker takes a second, confirming sample before acting. Suspension is
   irreversible for the daemon's life, while declining costs at most one tick
   against a plateau that is never given back. Suspension is kept in memory
   and also written as a `plugin-<language>.suspended` marker beside the pid
   file, so a separate `g-mesh status` process can report it. The full
   argument is in `docs/architecture/multi-language-plugins.md`, "Plugin
   memory limit" and its implementation notes.
4. **The daemon exits itself when orphaned** (`lifecycle::orphan_check`):
   first in the tick, on an absence of the project root or of its own
   executable that the filesystem positively reports (`NotFound`, never any
   other stat failure), whether or not a client is attached. This bounds an
   orphan's life at one tick (30s with the defaults). Rejected:
   - making the core idle timeout unconditional or shorter: nothing resets it
     improperly, so this would punish healthy long-lived daemons to reach a
     case the orphan check reaches in one tick;
   - a machine-wide reaper process: a second long-lived process to install,
     update and stop, to learn from outside what each daemon can answer about
     itself with two `stat`s;
   - exiting only once no client is attached: that rule exists so a client's
     tool surface does not vanish under it, and once the project root is gone
     every tool call is about files that do not exist.
5. **No signal handler.** `SIGTERM` kills the daemon outright
   (`core/tests/daemon_sigterm.rs` keeps that true); the orphan exit covers
   the case where no one can send the polite stop.
6. **Bind first, index later** (`daemon::run`). The endpoint is bound before
   the index is built and before any plugin spawns, because the shim's
   bootstrap timeout races the socket appearing and a walk can outlast it.
   The guarantee that no one reads a half-built graph moves to the response
   layer: the handshake is answered at once, and a tool call that needs the
   graph waits for it (`daemon::indexing_status`; D2-D7 of
   `docs/architecture/lazy-indexing.md`). Plugin discovery still runs before
   the bind, so a malformed manifest fails startup before a socket or pid
   file is published.
7. **A singleton lock, separate from the bootstrap lock.** A daemon holds
   `daemon.lock` for its whole life; the shim holds `bootstrap.lock` while
   spawning the daemon, so one shared lock would deadlock. A contended lock
   is retried for 300ms, because the kernel releases a `kill -9`'d holder's
   `flock` slightly after the process is gone; the budget stays well under
   the shim's shortest bootstrap budget so a real incumbent still wins.
8. **A serving record beside the lock** (`daemon.serving`), written after
   the bind and cleared by each new holder, so "held" splits into Free /
   Serving / Starting / Wedged and a holder that stopped serving can be
   evicted instead of wedging the project for its lifetime. It is beside the
   lock, not in it, because `File::try_lock` is `LockFileEx` on Windows and
   a second handle cannot read a locked file there. The record is
   newline-terminated and a record without the newline reads as nothing,
   because its reader may signal the pid.
9. **The embedding model is not loaded at startup**, not even on a
   background thread: that was measured to break the restart budgets under
   load. The first caller that embeds pays for the load on its own thread.

## Consequences
- A woken plugin replays exactly what changed while it slept; a request with
  nothing queued never respawns a plugin.
- Every exit path (idle, orphan) is a `return` from `lifecycle::supervise`
  after putting every plugin to sleep, so a commit in flight is durable
  before the process ends.
- A spike shorter than one tick passes the memory breaker unnoticed; how
  soon it fires on a cold pass is bounded by when the check can take the
  supervisor's lock, which a whole-project `semantic_pass` holds.
- A suspended language stays suspended until the daemon restarts or the
  config changes; config is read once at startup.
