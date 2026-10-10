# GM-542: `g-mesh reindex` without dropping MCP sessions

Status: design note (slice S1). Branch `fix/GM-542-reindex-keeps-mcp-sessions`, off `release-4.4.0`.

## 1. Findings

### 1.1 Which daemons `reindex` stops

Only the daemon of the project `reindex` runs in. Code path:

- `cli::reindex::run` (core/src/cli/reindex.rs:72-77) takes the cwd as the root and calls
  `reindex` (84-146), whose first line is `stop::stop(project_root)`.
- `cli::stop::stop` (core/src/cli/stop.rs:128-176) finds pids through `daemon::pid_path(root)`,
  `daemon::inspect_daemon_lock(root)` and `registry::discovered_pid_files(project_dir(root))`.
  Every path is under `~/.g-mesh/projects/<hash(root)>/` (`storage::connection::project_dir`,
  connection.rs:26-28). A folder front has a different root, so a different hash and different
  files: `stop` cannot see it.

So `reindex` in `.../g-mesh` never signals the `--project-root .../ClaudeProjects` front. The
front died on its own: a front exits after `FRONT_CORE_IDLE` = 60 s with no live connection
(core/src/daemon/front.rs:46, `lifecycle::supervise` 846-902 via `CoreActivity::idle_beyond`
738-747). Its only connection was the shim's, and the shim had exited (1.2). The reindex took
285 s, so the front had been gone for about 225 s when it finished.

### 1.2 Why the shim exits

`shim::router` ends the session when the current upstream's connection ends:

- `spawn_reader` (router.rs:299-325) calls `Shared::upstream_ended(id)` on EOF.
- `upstream_ended` (599-650) answers the requests that upstream still owed. Then, when
  `current == Some(id)`, it sends `Event::Done`.
- `serve` (254-297) breaks on `Event::Done` and returns. `shim::run` (shim.rs:104-128) returns,
  and the process exits. Claude Code then sees the server go away and has no tools until `/mcp`.

In the observed session `current()` (155-157, `sub.or(front)`) was the selected sub-project
`g-mesh`, so losing that daemon ended the session, and the shim's front connection closed with
the process. This behaviour is documented, not accidental: the module doc says "Returns when the
current upstream's connection ends", and `connect_or_bootstrap`'s doc (shim.rs:163-189) accepts
that another client of a retired outdated daemon "loses its connection and its shim exits".

### 1.3 Related defect: a bootstrap can race the rebuild

`reindex` holds no lock while it rebuilds. During the 285 s, any shim (this session's after a
reconnect, or a new session) that finds no daemon bootstraps one (`connect_or_bootstrap`,
shim.rs:190-265). That daemon opens the half-built index, sees `bulk_index_completed == false`
(daemon/mod.rs:239-330) and walks the same database the CLI is writing. This is the race the
module doc says stopping first prevents. Making the shim reconnect (1.2) without fixing this
would trigger the race on every reindex.

### 1.4 Who stops, who spawns, who connects (g-mesh calls used)

| Question | Answer | g-mesh call |
|---|---|---|
| Callers of `cli::stop::stop` | `stop::run`, `init::init`, `reindex::reindex`, `shim::retire_outdated_daemon` | `find_callers cli::stop::stop` (complete, `hasMore: false`) |
| Callers of `upstream_ended` | `spawn_reader` only | `find_callers upstream_ended` |
| State-dir derivation | `project_dir` → `project_dir_under(g_mesh_home, root)` | `find_definition project_dir` |

Note: the first query, `find_callers crate::cli::stop::stop`, resolved by
`semanticNeighbours` to test helpers. Querying again by the qualified name without `crate::`
resolved exactly. Front idle, `connect_or_bootstrap` and the router were read directly (single
known files).

## 2. Options for the shim on daemon loss

All three keep one rule: the shim exits only when its client closes stdin.

**A. Reconnect and wait with a bound.** On loss, the next request re-runs
`connect_or_bootstrap`, which blocks until the daemon is reachable or a deadline passes.
- Benefit: the client sees only latency. No new answer type.
- Risk: a reindex lasts minutes, so a call either blocks past the client's tool timeout or hits
  the deadline with an unhelpful timeout message. If nothing marks "reindex in progress", the
  bootstrap races the rebuild (1.3).

**B. Reconnect lazily, and answer "reindexing" explicitly.** `reindex` publishes a marker
(1.3 fix). The shim's reconnect checks it first. While it is held, each call gets an immediate
tool error naming the reindex and how long it has run. Otherwise the shim reconnects
(bootstrap ≤ `BOOTSTRAP_TIMEOUT` = 10 s) and replays `initialize`.
- Benefit: no call hangs. The agent learns why tools are unavailable and when to retry. The
  marker also closes the race in 1.3 for every session, not only this one.
- Risk: during the reindex, calls fail rather than wait. That is a change from "invisible except
  latency", and an agent might give up. Mitigated by wording ("call again after it finishes").

**C. The running daemon reindexes in place.** `g-mesh reindex` asks the live daemon to rebuild
each language into a staging file and swap it in. `daemon::workspace_reindex` already does this
per language (staging/plan/swap, ADR 0008). Queries keep seeing the old graph until each swap.
- Benefit: truly invisible. Answers continue throughout.
- Risk: it needs a control channel into the daemon (today a connection must start with MCP
  `initialize`). A full reindex is the escape hatch for a suspect index, and C would run it
  through the incremental swap machinery it exists to bypass: no `schema::reset`, and
  `meta`/embedding tables are not wiped. It is the largest change, and it does not fix
  `g-mesh stop`, `init` or outdated-build retirement, which drop sessions the same way.

**Recommendation: B.** It meets every acceptance criterion, fixes 1.3, and has the smallest
surface: router plus a marker. It also covers every other daemon restart (`stop`, `init`,
retirement, crash). C can follow later as an optimisation and does not conflict with B.

## 3. Behaviour under B

### 3.1 Reindex side

`cli::reindex::reindex` gains a guard held for the whole rebuild:
1. Take the project's bootstrap lock (`shim::acquire_bootstrap_lock`, blocking, held only briefly).
2. Write `reindex.pid` (own pid plus start time) in the state dir.
3. `stop::stop(root)` (unchanged).
4. Take the daemon singleton lock (`daemon::acquire_singleton_lock`, made `pub(crate)`; retry
   while the stopped daemon's flock is released). A hand-started `g-mesh daemon` now stands down.
5. Release the bootstrap lock. Rebuild. On drop, remove `reindex.pid` and release the daemon lock.

A shim that queued on the bootstrap lock re-checks the marker under it
(`connect_or_bootstrap`), so no shim can spawn a daemon between steps 2 and 4. A crashed
reindex leaves `reindex.pid` naming a dead pid. That reads as "not reindexing", the kernel has
already dropped the flock, and the next daemon cold-walks the incomplete index as it does today.

### 3.2 Shim side

- A lost upstream is marked *lost*: its root is kept and its connection dropped. No
  `Event::Done`. `Done` comes only from `client_loop` on stdin EOF.
- The next request routed to a lost upstream reconnects first, on the client thread (client
  frames are processed in order anyway): `connector(root)`, then the same
  initialize/initialized replay that `switch` does (router.rs:544-597, factored out), then a new
  reader.
- If the connector reports the reindex marker, the request gets `error_result`:
  `g-mesh: <root> is being reindexed (g-mesh reindex, pid N, running M s); its tools come back
  when it finishes - call again then.` It is not recorded in `pending`. The next request tries
  again.
- If the connector fails for another reason, the request gets an error result carrying the
  bootstrap error. The session stays up.
- Notifications for a lost upstream (`notifications/cancelled`) are dropped. Nothing is owed.

### 3.3 In-flight calls

A call already sent when the daemon is stopped is answered by the existing owed-request path
(`upstream_ended`): an `isError` result, "not answered ... call the tool again". When the marker
is present, the message names the reindex as the cause. The answer is never lost and never
silently replayed: a replay could duplicate a non-idempotent call, and the client's retry is
explicit.

### 3.4 The front case

- Sub-project lost: `sub` keeps its root and is marked lost. `current()` must not fall back to
  the front (today `sub.or(front)` would send `get_file_outline` to a front that serves only
  `select_project`). The reconnect goes to the selected root, and the answer keeps the
  `answered from project <name>` stamp. The selection survives.
- Front lost while a sub is selected (for example `g-mesh stop` in the folder): routing to the
  sub continues. `select_project` and `tools/list` reconnect the front lazily, with the same
  replay. That replaces today's permanent "cannot switch projects any more" error
  (router.rs:395-405).
- The front itself is never touched by a sub-project reindex (1.1). With the shim alive, its
  connection holds the front open, so it no longer idles out.

### 3.5 Instructions and GM-543

MCP delivers `instructions` once, in the client's `initialize`. The replayed `initialize`
response is consumed by the shim. The client keeps the instructions of its first daemon, and
there is no MCP message to update them. After a reindex the new daemon is warm and complete, so
the GM-543 cold-cause wording (Fresh/Discarded/Incomplete) would normally be the warm text anyway.
`select_project` state lives in the shim (`Router::sub`) and in the front's per-connection
state. Lazy front reconnect (3.4) starts a new front connection, which knows of no selection.
That is harmless because the front only lists projects and names a switch target; routing state
is the shim's. See M3 for whether to tell the agent anything.

## 4. Edit map

Files to change (lines as of `release-4.4.0`):

- core/src/shim/router.rs
  - module doc 1-61: "Returns when the current upstream's connection ends" becomes "only when
    the client ends".
  - `Upstream` 91-117 / `Router` 120-143: add a lost state (for example `Option<Upstream>` plus
    `lost_root: Option<PathBuf>` per slot, or an enum `Slot { Live(Upstream), Lost(PathBuf) }`).
  - `Router::current` 155-157: never fall back from a selected sub to the front.
  - `Event` 179-183: `Done` only from `client_loop`.
  - `serve` 254-297; `client_loop` 333-357: send `Done` after closing the upstreams.
  - `on_client_frame` 375-443: reconnect-before-route; reindexing answer.
  - `switch` 544-597: extract `handshake(link, init, initialized) -> Result<(Link, String)>`.
  - `upstream_ended` 599-650: mark lost instead of `Done`; name reindex in the owed message.
  - `Connector` 89: return a typed error (or an enum) so "reindexing" is distinguishable.
- core/src/shim.rs: `run` 104-128 (connector closure); `connect_or_bootstrap` 190-265 (check
  the marker before `incumbent` and again under the bootstrap lock; doc 163-189 loses the
  "its shim exits" trade); `acquire_bootstrap_lock` 412-445 (`pub(crate)` for reindex).
- core/src/daemon/mod.rs: `acquire_singleton_lock` 614-646 → `pub(crate)`. New
  `reindex_marker_path`, `reindex_in_progress(root) -> Option<(pid, started)>` next to
  `pid_path` 107-111.
- core/src/cli/reindex.rs: `reindex` 84-146 (guard of 3.1); module doc 16-28; `render` 148-.
  The line "the next tool call starts a fresh one" stays true.
- core/src/cli/stop.rs: no change. `tidy_state_files` 236-273 removes only `daemon.pid` and
  `plugin-*.pid` (`registry::discovered_pid_files` matches the `plugin-` prefix), so
  `reindex.pid` survives the `stop` inside `reindex`.

Functions to read before editing: `stop::stop` 128-176, `evict_wedged_daemon` shim.rs:373-398,
`daemon::stand_down` mod.rs:442-457, `inspect_daemon_lock_in` 674-686 (a daemon lock held by
reindex with no serving owner reads `Starting` and is never evicted, which is the intended
outcome), and the router test harness `Fake`/`Session` router.rs:770-975.

## 5. Test plan

Unit (router.rs, fake connector; no processes):
- U1 `a_lost_daemon_is_reconnected_on_the_next_call`: fake hangs up, the next `tools/call`
  makes the connector run for the same root, `initialize` + `initialized` are replayed, the call
  is answered by the new fake, and `serve` has not returned.
- U2 `a_lost_sub_project_reconnects_to_the_selection`: after `select`, the sub hangs up; the next
  call goes to the sub's root, not the front, and is stamped with the sub's name.
- U3 `a_call_during_reindex_gets_an_explicit_answer`: the connector returns Reindexing, so the
  call gets `isError` with "reindexed"; the connector then succeeds and the next call is answered.
- U4 `stdin_eof_still_ends_the_session`: the existing behaviour, kept.
- Update `a_daemon_that_closes_with_a_call_pending_gets_it_answered` (1219-1235): it asserts that
  the session ends with its daemon. It keeps the owed-answer assertions and drops the join.

Integration (processes; 5 runs each in the tests slice):
- I1 core/tests/cli_reindex.rs `a_shim_session_survives_reindex`: rmcp client over `mcp-shim`,
  one tool call, `g-mesh reindex`, a second call on the same client succeeds, and the shim child
  has not exited (`try_wait` is `None`).
- I2 `a_call_while_reindexing_is_answered_not_dropped`: the test plays reindex by holding the
  daemon lock and writing `reindex.pid` with its own pid. The call gets the reindexing text with
  `G_MESH_BOOTSTRAP_TIMEOUT_MS` short. After the test releases them, the next call is answered.
- I3 core/tests/multi_project_front.rs `reindexing_the_selected_project_keeps_the_front_and_the_selection`:
  folder session, select `a`, reindex `a`. The front pid is unchanged and alive, and the next call
  says "answered from project a".

Controls (revert code, never tests; group by binary):
- C1 `upstream_ended` sends `Done` again on current loss → U1, I1 fail.
- C2 `current()` falls back to the front after a sub loss → U2, I3 fail.
- C3 the connector's Reindexing error is treated like any bootstrap failure (generic text) →
  U3 fails.
- C4 drop the marker check in `connect_or_bootstrap` → I2 fails (the spawned daemon stands down
  on the held lock; timeout text without "reindex").
- C5 `reindex` skips taking the daemon lock → a racing `g-mesh daemon` during I2-style hold is
  able to serve (assert in I2: no `daemon.pid` appears while the marker is held) → I2 fails.

## 6. Must-confirm

**M1. During a reindex, answer immediately or wait?** Today a call during a reindex has no
server at all. Under B it gets an immediate tool error such as `g-mesh: …/g-mesh is being
reindexed (pid 4242, running 40 s); call again when it finishes`. The alternative waits up to N
seconds for the reindex to end before answering. Example: a 285 s reindex with a 30 s wait means
each call blocks 30 s and then errors anyway. With immediate answers, the agent decides at once
whether to use grep. Consequence: immediate answers are honest and cheap, but the acceptance
criterion "invisible except latency" holds only after the reindex, not during it.

**M2. Reconnect lazily (on the next call) or eagerly (as soon as the daemon is lost)?** Today,
`g-mesh stop` ends every connected session. Lazy: `stop` stops the daemon, and it comes back
only when some session calls a tool. Eager: the shim re-bootstraps immediately, so `g-mesh stop`
under a live session restarts the daemon within milliseconds, and `g-mesh clean` races it.
Recommended: lazy. Consequence: the first call after a restart pays the bootstrap (≤ 10 s,
normally ms).

**M3. Tell the agent the daemon restarted?** Today a restart is visible as a disconnect. Under B
the client's MCP `instructions` stay those of the first daemon, and MCP cannot resend them.
Option: prefix the first answer after a reconnect with one line, such as `g-mesh: the daemon for
<project> restarted (reindex finished); the index was rebuilt.` Example: after a reindex fixed a
resolver bug, the agent knows earlier answers may differ. Consequence: one more stamp format that
GM-543's instruction wording and ADR 0014's stamp rule must agree with. Without it, the restart
is silent.

**M4. Scope: all restarts, not only reindex.** B changes the shim, so it also changes
`g-mesh stop`, `g-mesh init`, outdated-build retirement (`retire_outdated_daemon`) and daemon
crashes. Today each of these ends other connected sessions. After B, those sessions reconnect on
their next call. Example: installing a new build and starting one new session no longer kills
the other open sessions; they move to the new daemon on their next call. Consequence: the
`connect_or_bootstrap` doc's "deliberate trade" disappears. `init` (which also calls `stop`)
gets no reindex marker in this task, so a session calling during `init`'s rebuild can still race
it (1.3). Fix it here too, or file a follow-up?
