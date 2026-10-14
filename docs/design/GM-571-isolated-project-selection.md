# GM-571: isolated project selection and late worktrees

Status: design (GM-571/S1), for owner review. Builds on GM-447
(`docs/architecture/gm-447-shared-project-selection.md`) and ADR 0014 (the
"answered from project X" stamp, commit 2fe3293). The owner chose
"Настоящая изоляция": the stamp alone is not enough.

## What happens today (facts this note relies on)

1. **One selection per connection.** Claude Code runs one g-mesh MCP server
   process (the shim, `core/src/shim/router.rs`) per Claude Code session;
   every subagent of that session talks through it. The shim's `Router`
   holds one `sub: Option<Slot>` (router.rs:216-242). `select_project` is
   answered by the front (`core/src/mcp/front.rs`), whose result carries
   `_meta["g-mesh/switchProject"].root`; the shim's `on_front_frame` /
   `switch` (router.rs:680-767) then points *every* later `tools/call` of the
   connection at that project. GM-447 established (from the Claude Code
   binary) that the shim cannot tell callers apart: JSON-RPC ids are per
   connection, `claudecode/toolUseId` is per call, and `agentId` is never sent
   to g-mesh. So per-MCP-session selection is what exists already and does
   not separate agents; any isolation must come from the call itself.
2. **The front never caches the project list.** `Front::select_project`
   and `Front::not_selected` do a fresh bounded walk on every call
   (`Front::candidates`, front.rs:95-109; same code in v4.7.0). Calling
   `select_project` with no argument in this session listed
   `g-mesh-wt-gm571`, created after the server started.
3. **The instructions are a snapshot.** `Front::new` builds the front's
   `instructions` once, when the front daemon starts (front.rs:77-92,
   `instructions::build_front`, instructions.rs:618-635), and a client keeps
   the instructions it got at `initialize` (MCP cannot resend them). This
   session's instructions list `g-mesh-wt-gm566`, `gm567`, `gm579`, `rel480`
   (all deleted) and omit `gm571`, `gm587`, `gm599`, `rel490` (all present).
4. **What GM-519/S1 actually did.** Its transcript
   (`7e1a2db7.../subagents/agent-a6f89d3a947f2fce5.jsonl`) has zero
   `mcp__g-mesh__*` calls; its system prompt carried the instructions
   "folder of 17 projects ... Projects: brushwork (indexed), g-mesh
   (indexed), ..." without `g-mesh-wt-gm519`. It never tried
   `select_project`; it read the stale list and went to grep. Part 2 is
   therefore a *discoverability* bug (stale instructions telling the agent
   what exists), plus one real gap: the walk is capped
   (`Limits { max_entries: 5_000, max_candidates: 64 }`, candidates.rs:50-64)
   and a project past the cap cannot be selected at all.

## Decisions

Each decision: today, the change, an example, the consequence, benefit and
risk. Recommended option first.

### D1. Isolation mechanism: an optional per-call `project` argument, routed by the shim

- **Today:** a call goes to whatever the connection last selected.
- **Change:** each of the eight index tools takes an optional `project`
  (same spelling as `select_project`'s: a path relative to the folder, or
  absolute). When present, the shim routes that one call to that project's
  daemon, whatever is selected, and the answer's stamp names it. The shim
  keeps a small pool of sub-project connections (D5) instead of one.
- **Example:** subagent A (worktree `g-mesh-wt-gm571`) calls
  `find_references {symbol_name: "walk", project: "g-mesh-wt-gm571"}`; at the
  same time subagent B selects `g-mesh`. A's call is still answered from
  gm571, B's selection is untouched.
- **Consequence:** a subagent knows its worktree's name/path from its brief
  or cwd, so it can always pass it. Briefs say "pass `project` on every g-mesh
  call" instead of "select_project first".
- **Benefit:** the only mechanism that separates agents on one connection
  (GM-447 options 3/5/6 are impossible or outside g-mesh; a session token,
  option 4, is the same thing with an opaque value).
- **Risk:** ~1.1 k chars of schema in every turn's tool list (GM-447 cost
  estimate); `TOOLS_LIST_BYTE_CEILING` (mod.rs:144, 11 800, kept tight by
  `the_tools_list_fits_its_ceiling`) must be raised. The shim parses and
  re-serializes every call frame that carries `project` (it already parses
  every client frame in `classify`).

### D2. Where `project` lives in the schema: injected by the front only (Recommended) vs in every daemon's params

- **Today:** front and daemon tool schemas are byte-identical by
  construction (`listed_tools`, front.rs:56-64), pinned by
  `a_project_daemon_lists_a_subset_of_the_front` (tools_list_tests.rs:37-46).
- **(a) Recommended:** `front::listed_tools` adds the `project` property to
  the eight tools' `inputSchema`; daemons' param structs do not change. In a
  multi-project session `tools/list` always goes to the front
  (`reach_for_tools_list`), so agents see it. The shim removes `project`
  from the arguments before forwarding (daemons never see it). A
  single-project session is unchanged: no schema cost, frames byte-identical.
- **(b)** Add `project: Option<String>` to `FindDefinitionParams`,
  `SymbolQueryParams`, `FindImplementationsParams`, `GetFileOutlineParams`,
  `GetDependenciesParams`, `SearchCodeParams` (mod.rs:711-917) and have each
  daemon check it names its own root.
- **Benefit (a):** the isolation layer stays where the multi-project layer
  is (front + shim); no daemon or tool handler changes; single-project users
  pay nothing.
- **Risk (a):** schemas differ by session type; the subset test changes to
  "subset modulo `project`"; when the front is unreachable and `tools/list`
  falls back to the selected daemon (router.rs:603-624), `project` drops out
  of the list for that answer. In a single-project session an agent that
  passes `project` anyway gets it ignored by serde (no `deny_unknown_fields`
  in `core/src/mcp`): harmless, because that session has only one project to
  answer from; the shim still validates it (D6) so a wrong name is an error,
  not a silent answer.
- **Benefit/risk (b):** one schema everywhere, a daemon-side check; but six
  param structs and eight handlers change, and single-project users pay the
  schema every turn.

### D3. A call without `project`: refuse once the session's selection is contested (Recommended)

- **Today:** goes to the last selection (stamped).
- **(a) Recommended - "contested" rule:** the shim records every distinct
  project a successful `select_project` switched to. While only one project
  was ever selected, a call without `project` goes there (today's
  behaviour). Once a second, *different* project has been selected in this
  session, the session is contested for good: a call without `project` is
  answered by the shim with a tool error, and nothing is sent upstream.
  Re-selecting the same project does not contest.
  Text: `g-mesh: more than one project was selected on this connection (g-mesh, then g-mesh-wt-gm571), which other agents may share, so a call without "project" is not answered. Pass "project" with the project you mean, e.g. "g-mesh-wt-gm571".`
- **(b)** Always default to the last selection (option 1 of GM-447): no
  guarantee - an agent that forgets `project` is silently misrouted again
  (only the stamp tells it).
- **(c)** Require `project` on every call in a multi-project session (1b):
  the strongest rule; every call of the owner's main session needs it.
- **Example (a):** A selects `g-mesh`, calls without `project` -> answered.
  B selects `gm571` -> contested. A's next call without `project` -> refused
  with the text above, A retries with `project: "g-mesh"`.
- **Guarantee (a):** an answer to a call without `project` comes from the
  only project anyone selected; the switch and the routing decision happen
  under the router lock, so no call is routed between a second select's
  switch and the contested flag. A call sent before B's switch completes
  goes to A's project, which is correct for A.
- **Benefit (a):** single-agent sessions keep today's flow; any session where
  a misroute is possible fails loudly instead of answering wrongly.
- **Risk (a):** a single agent that switches projects itself also has to pass
  `project` afterwards (one refused call teaches it). Residual hole: an agent
  that never selected and never passes `project`, in a session where exactly
  one other project was selected, is answered from that project (stamped).
  The instructions tell agents to select or pass `project`, so this is an
  agent bug, and the stamp names the project.

### D4. Guidance for a project reached only through `project`: on its first answer (Recommended)

- **Today:** a project's guidance (its daemon's `initialize` instructions)
  arrives only in the `select_project` result.
- **(a) Recommended:** the first `tools/call` answer from a project this
  session has not handshaken through `select_project` carries the guidance as
  one more trailing text item (the handshake already returns it:
  `handshake`, router.rs:769-806). Once per project per session.
- **(b)** None: agents that want guidance call `select_project` - which
  contests the session (D3) and so forces everyone else to pass `project`.
- **Benefit (a):** subagents never need `select_project`, so they never
  contest the parent's selection.
- **Risk (a):** up to ~1.9 kB (`INSTRUCTIONS_BYTE_CEILING`) once per project;
  a second subagent on the same project in the same session does not get it
  again (guidance is mostly static, and the first agent's need is the main
  one).

### D5. Pool of sub-project connections: LRU, at most 4 live (Recommended)

- **Today:** one `sub` slot; a switch retires the previous connection once it
  owes nothing (`switch`, router.rs:744-767).
- **Change:** `subs: HashMap<PathBuf, Slot>` plus `selected: Option<PathBuf>`
  and an LRU order. A per-call `project` or a `select_project` opens (or
  reuses) that project's connection. Above 4 live connections the least
  recently used one is retired with the existing `Retired` machinery (it
  still delivers what it owes, then half-closes).
- **Why a cap:** "a live connection holds the core open"
  (`daemon/lifecycle.rs`, `live_connections`): without one, every project
  touched in a session keeps its daemon (and plugins) alive until the client
  exits.
- **Benefit:** parallel worktree agents (the standard workflow, 3-6 at once)
  stay on warm connections; idle daemons can still exit.
- **Risk:** with more than 4 projects in rotation, connections churn (a
  connect each time, cheap when the daemon is still up). An evicted project's
  next call must open a fresh connection *without* the restart line
  (`restarted` set): eviction is not a restart, so it must not go through
  `reconnect`. A first call to a cold worktree's daemon bootstraps it on the
  client thread, under the lock, as `reconnect` does today: other frames wait
  that long (seconds).

### D6. Resolving a project name: a direct probe in `candidates.rs`, not a walk (Recommended; serves both parts)

- **Today:** `find_candidate` (front.rs:202-224) looks the name up in a fresh
  but capped walk; the shim resolves nothing.
- **Change:** `pub fn resolve(root: &Path, project: &str) -> Result<Candidate, Unresolved>`
  in `core/src/daemon/candidates.rs`, built from `segments` /
  `relative_to_root` (moved from front.rs:190-253) and the walk's own rules
  (`probe`, `is_skipped`, `markers_of`, candidates.rs:231-255): the path is
  below `root` by components (or canonicalizes there), 1..=`max_depth`
  segments, no dot or skipped segment, a real directory (not a symlink), it
  carries a marker, and no directory between `root` and it does (else that one
  is the candidate). `Unresolved` says why (not below root / no marker / inside
  candidate X / does not exist). Both `Front::select_project` and the shim use
  it; `select_project` with no argument and error texts keep the walk for the
  list.
- **Example:** `project: "g-mesh-wt-gm599"` created a minute ago resolves with
  three `stat`s; a 70th project past `max_candidates` resolves too.
- **Benefit:** fresh by construction (no cache to invalidate), immune to the
  walk's caps, O(depth) syscalls on the hot path (the shim resolves on every
  call that carries `project`; it may cache name -> root for the session and
  re-resolve on a miss). Sane for GM-564: it returns the same `Candidate`
  (with `is_worktree`), and seeding a worktree's index can take a `Candidate`
  without caring how it was found.
- **Risk:** two implementations of "what is a candidate" (walk and resolve)
  could drift; a test pins that `resolve` accepts exactly the walk's
  candidates on the fixture (T8).

### D7. Late worktrees: per-connection instructions plus "snapshot" wording (Recommended); no fs watch, no TTL

- **Today:** see facts 2-4. The select path is already live; the stale
  instructions are what misled GM-519/S1.
- **Change:**
  1. `Front` builds instructions per connection, in `serve_connection`
     (front.rs:255-264), from a fresh walk (`spawn_blocking`), instead of once
     in `Front::new`. A Claude Code session started after a worktree exists
     sees it, and deleted worktrees drop out.
  2. `front_sentence` (instructions.rs:566-582) says the list is as of the
     session start and that any project below the folder, a worktree created
     since included, can be passed as `project` or to `select_project` by
     name or path. This is what helps an *already running* session (GM-519's
     case: subagents inherit the parent's instructions).
  3. The `project` property's description (D2) says the same in a few words.
- **Rejected:** a filesystem watch or a TTL refresh. Nothing in the front
  caches a list to refresh except the instructions, which MCP cannot push to
  a client after `initialize`; a watch adds a platform dependency for no
  observable effect. "Re-walk on a select miss" is moot: every select already
  walks, and D6 replaces that walk with a probe.
- **Benefit:** fixes the observed failure with wording plus one moved call;
  no new machinery.
- **Risk:** per-connection instructions cost one bounded walk and one sqlite
  open per candidate (`has_completed_index`) at each connection (a session
  start, a reconnect): milliseconds for 18 projects, up to the 5 000-entry
  cap. The longer sentence costs instruction bytes under
  `INSTRUCTIONS_BYTE_CEILING` (1 900), so fewer names fit before
  "(+K more ...)".

### D8. Back-compat of `select_project`

Unchanged in shape: no argument lists (fresh walk), `project` selects and
returns guidance, and the selection remains the default for calls without
`project` until contested (D3). Its result text (router.rs:700-710) changes
from "call select_project again" to "pass `project` on every call when other
agents share this connection". ADR 0014's stamp stays (it is what tells an
agent which project answered a call routed by `project`, too); a new ADR
records D1-D5 and amends 0014's "Consequences" (the hard guarantee it
anticipated).

## Edit map

Line numbers are the worktree at `dfc9cac` (= main).

### S2 - isolation (D1-D6, D8)

| File | Symbol (lines) | Change |
|---|---|---|
| `core/src/daemon/candidates.rs` | new `resolve`, `Unresolved` after `walk` (257-307); reuse `probe` (231-243), `markers_of` (245-247), `is_skipped` (249-255) | D6. Move `segments` / `relative_to_root` here from front.rs, `pub(crate)` |
| `core/src/mcp/front.rs` | `PROJECT_ARG` const + `listed_tools` (56-64) | D2a: insert the `project` property into the eight schemas (not `select_project`) |
| | `SELECT_PROJECT_DESCRIPTION` (42-46) | mention per-call `project` |
| | `Front::select_project` (111-133) | resolve with `candidates::resolve`; walk only for the miss text |
| | `find_candidate` (202-224), `segments` (190-192), `relative_to_root` (247-253) | removed / moved; `containing_candidate` (226-245) keeps using the moved helpers; their unit tests (323-475) move with them |
| | `Front::not_selected` (137-158) | add "or pass `project`" |
| `core/src/shim/router.rs` | `Which` (210-214) | `Front`, `Current`, `Project(PathBuf)` |
| | `Router` (216-242) | `sub` -> `subs: HashMap<PathBuf, Slot>`, `selected`, `selections: Vec<PathBuf>` (distinct, for D3 text), `lru`, `guided: HashSet<PathBuf>` (D4), `folder: Option<bool>` (lazy, see below) |
| | `Router::slot`/`slot_mut`/`upstream`/`open_upstreams` (256-288) | iterate the pool |
| | `ClientFrame`, `classify` (313-354) | `Call { project: Option<String> }` for `tools/call` other than select_project |
| | `on_client_frame` (514-577) | route: `project` present -> resolve (D6) -> strip arg -> `Which::Project`; absent and contested -> shim error (`error_result`, 937); else `Current` |
| | `reach` (579-601), `reconnect` (626-644) | per-root slots; a fresh pool connect (eviction re-open) does not mark `restarted` |
| | `reach_for_tools_list` (603-624) | fallback picks the selected slot |
| | `on_front_frame` (680-742) | new result text (700-710); record selection; set contested |
| | `switch` (744-767) | insert/reuse in pool + LRU instead of retiring the previous |
| | `upstream_ended` (808-865) | mark the matching pool slot lost |
| | `settle` (489-512) | D4: append guidance on the first answer of an unguided project |
| | module doc (1-87, "Which project answered" 25-35) | rewrite for per-call routing |
| | new `without_project(frame) -> Vec<u8>` beside `stamped` (921-935) | strip the argument |
| `core/src/shim.rs` | `run` (104-131), `router::Daemons` (router.rs ~120-126) | add `Daemons::is_folder: Box<dyn Fn(&Path) -> bool>` (real: `candidates::detect(..).mode == Multi`, evaluated lazily on the first call with `project`); in a single-project session `project` must name the root itself, else a shim error |
| `core/src/mcp/mod.rs` | `TOOLS_LIST_BYTE_CEILING` (144) | raise to fit (the headroom assertion sets the new value) |
| `core/src/mcp/tools_list_tests.rs` | `a_project_daemon_lists_a_subset_of_the_front` (37-46) | compare with `project` removed from the front's schemas |
| `docs/adr/` | new ADR; amend 0014 Consequences | D1-D5 |

Read for context: `Shared::handshake` (769-806), `spawn_reader` (422-450),
`answered_before_session` (646-678), the router test harness `Session`
(router.rs ~1160-1290: `open`, `select`, `call`, `daemon`, `Fake`),
`daemon/lifecycle.rs` `live_connections` (~709-720).

### S3 - late worktrees (D7)

| File | Symbol (lines) | Change |
|---|---|---|
| `core/src/mcp/front.rs` | `Front` (67-74), `Front::new` (77-92), `serve_connection` (255-264), `FrontServer::get_info` (273-279) | build instructions per connection (walk in `spawn_blocking`, then `build_front`); `FrontServer` holds them |
| `core/src/mcp/instructions.rs` | `front_sentence` (566-582), `build_front` (618-635) | snapshot wording + `project`; keep under `INSTRUCTIONS_BYTE_CEILING` (31) |
| `core/src/mcp/instructions/tests.rs` | `build_front_*` tests (433-520) | update expected text |
| `core/src/daemon/front.rs` | `Front::new` call (90), `serve_connection` call (133) | adjust to the new constructor / per-connection instructions |

S3 depends on S2 only for the `project` wording; it can run after S2.

## Tests (S4) and controls

Router tests use `Fake` daemons and threads: 5x repeats per the standing
rules. Integration tests run the real shim (`core/tests/multi_project_front.rs`,
fixture `a/`, `b/`, `c/`).

| # | Test (file) | Asserts | Control (revert in code, test must fail) |
|---|---|---|---|
| T1 | `a_call_with_project_is_answered_by_that_project_whatever_is_selected` (router.rs) | select `a`; call with `project: "b"` reaches b's fake, without the `project` argument; stamp names `b`; a call in flight on `a` is still answered by `a` | **C1:** `classify` ignores `project` (route to `Current`) |
| T2 | `a_second_distinct_selection_refuses_calls_without_project` (router.rs) | select a, call -> a; select b; call without `project` -> shim error naming a and b; no daemon receives it; call with `project` -> answered | **C2:** never set contested |
| T3 | `reselecting_the_same_project_does_not_contest` (router.rs) | select a, select a, call without `project` -> answered by a | **C3:** contest on every select |
| T4 | `an_unknown_project_is_refused_not_routed` (router.rs) | `project: "zz"` -> shim error with the reason; no daemon frame | **C4:** fall back to `Current` on a resolve miss |
| T5 | `the_least_recently_used_connection_is_retired_above_the_cap` (router.rs) | 5 projects -> the first is half-closed once drained; its next call reconnects without the restart line | **C5:** no eviction (no close observed) |
| T6 | `resolve_accepts_exactly_the_walks_candidates` + `resolve_refuses_nested_skipped_symlinked_and_deep_dirs` + `resolve_finds_a_project_past_the_walk_cap` (candidates.rs) | every walk candidate resolves to the same `Candidate`; nested package / `node_modules` / symlink / depth 3 are refused with their reason; with `max_candidates: 1` the walk truncates and resolve still finds the second | **C6:** drop the "no marked ancestor" check |
| T7 | `a_project_created_after_the_session_started_is_selectable_and_routable` (multi_project_front.rs) | start a session; create `d/` with `go.mod`; `select_project "d"` succeeds; `get_file_outline` with `project: "d"` is answered from d | **C7:** `Front::select_project` resolves from a walk taken in `Front::new` |
| T8 | `a_new_connection_lists_a_project_created_after_the_front_started` (multi_project_front.rs) | session 1 starts the front; create `e/`; session 2's instructions list `e` and say the list is a snapshot | **C8:** build instructions once in `Front::new` |

Also (no separate control): D4's guidance item appears once on the first
`project`-routed answer (T1 extension); `tools_list_tests` updated;
`single_project_frames_are_byte_identical` stays unchanged and green (frames
without `project` must still cross byte-for-byte).

Controls C1-C5 share one test binary (core lib, `--lib shim::router`), C6
the lib (`--lib daemon::candidates`), C7-C8 `--test multi_project_front`:
three build groups.

## Callers and references relied on (g-mesh, project `g-mesh` = main checkout, identical code)

Every answer carried `g-mesh: answered from project g-mesh.`

- `find_references symbol_id=daemon::candidates::walk` -> `candidates::decide`,
  `cli::debug_candidates::run`, `cli::status::collect`, `Front::candidates`
  (the only MCP-path user).
- `find_references mcp::front::listed_tools` (by id) -> `Front::new`,
  `tools_list_tests::the_tools_list_fits_its_ceiling`,
  `tools_list_tests::a_project_daemon_lists_a_subset_of_the_front`.
- `find_callers shim::router::serve` -> `shim::run` (shim.rs), and the router
  tests `single_project_frames_are_byte_identical`, `Session::open`.
- `find_references mcp::front::SELECT_PROJECT` -> `listed_tools`,
  `Front::new`, `FrontServer::call_tool`, and router.rs (file-level `use`).
- `find_references mcp::instructions::build_front` -> `Front::new` and five
  `instructions::tests::build_front_*` tests.
- `find_references daemon::candidates::Candidate answer=files` ->
  `candidates.rs`, `front.rs` only (so moving `find_candidate` touches no
  other file).
- grep (single files / non-code): `deny_unknown_fields` in `core/src/mcp`
  (none), `#[tool(` / `Parameters<` in mod.rs (the eight tools' param types),
  `live_connections` in lifecycle.rs, the two byte ceilings.
- Note: `find_callers core::shim::router::serve` (crate-prefixed name) returned
  `semanticNeighbours`; qualified names omit the crate (`shim::router::serve`).
