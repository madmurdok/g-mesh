# GM-399 multi-project roots: measurements M3(b) and M4

Measurement slice S5 of GM-399. Plan: `docs/architecture/lazy-indexing.md`,
section 5, M3 part (b) and M4. M4 covers only the target folder
`~/Projects/ClaudeProjects`, as this slice's brief specified.

## Machine and build

- 2026-09-24, macOS 26.6.2 (25G83), Intel Core i7-1068NG7 @ 2.30GHz, 32 GiB.
- g-mesh commit `05e5951` (branch `feat/GM-399-multi-project-front`),
  `cargo build --workspace --release`, binary
  `wt-gm399/target/release/g-mesh` (reports `3.12.0`).
- `uptime` before M3(b): `up 1 day, 6:46, 7 users, load averages: 50.32 178.78 153.88`.
  The machine was heavily loaded by unrelated work (the 5-min average was 178).
  The M3(b) numbers below are each process's own accumulated CPU `time`,
  so the load does not inflate them. It can only slow wall-clock latencies.
- `uptime` around M4: `up 1 day, 6:48, 7 users, load averages: 12.23 128.90 137.20`.
- `claude --version` not recorded: interactive `claude` was not available to
  the measuring agent, so M3(b) drove `g-mesh mcp-shim` directly over stdio
  with JSON-RPC (a Python driver). That is the same process Claude Code launches.

## Isolation

The branch's `schema::ensure_current` wipes an index whose generation
differs, so the branch binary was never pointed at the owner's real state
(`~/.g-mesh`).

- Every run set `G_MESH_HOME=/tmp/claude-502/gm399s5/home`
  (`core/src/paths.rs::g_mesh_home`). The directory lives under `/tmp`
  rather than the session scratchpad because the scratchpad path would push
  `<home>/projects/<hash>/daemon.sock` past macOS's 104-byte AF_UNIX limit
  (the same reason `.cargo/config.toml` gives).
- Still read from the real home, read-only: `~/.g-mesh/plugins`
  (plugin discovery, `daemon/manifest.rs::default_roots`) and `~/.g-mesh/models`
  (`embedding/model.rs`, deliberately not moved by `G_MESH_HOME`).
- `G_MESH_DAEMON_LOG` sent each daemon's stderr to a scratch file, and
  `G_MESH_TRACE_CALLS=1` was set.
- No g-mesh process was running before the runs (`pgrep -fl g-mesh` printed nothing).
  Every daemon started here was stopped with `g-mesh stop` under the
  isolated home. Afterwards `pgrep -fl wt-gm399` printed nothing.

### Q2 fact: an accidental whole-folder index exists in the real state

Read-only listing of `~/.g-mesh/projects/*/project.root`:

```
/Users/Valentin_Taiurskii/.g-mesh/projects/db453c25299daf62 -> /Users/Valentin_Taiurskii/Projects/ClaudeProjects
-rw-r--r--  daemon.build   150  Sep 23 20:16
-rw-r--r--  daemon.pid       6  Sep 23 20:16
srwxr-xr-x  daemon.sock      0  Sep 23 20:16
-rw-r--r--  index.db  29663232  Sep 24 19:07
/Users/Valentin_Taiurskii/.g-mesh/projects/959ade85d9a343b1 -> /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh
-rw-r--r--  index.db  21299200  Sep 24 00:14   (meta: schema 8, indexer 2+45156ecccc9a64e3)
```

The whole-folder index (`db453c25299daf62`, 29.7 MB, last written today
19:07) is still there. **Not cleaned.** Running `g-mesh clean` on the real
state is the owner's call. Until it is removed, a real session in
`~/Projects/ClaudeProjects` resolves by rule 1/2 (an existing index) and
would *not* take the front path this slice measures. That is why M3(b) ran
against the isolated home, where the folder has no index.

## M3(b): idle connect on the folder, then select_project and one call

### Setup: an existing index of g-mesh in the isolated home

To test reuse, the branch binary first built its own g-mesh index in the
isolated home. The shim ran in `ClaudeProjects/g-mesh` and made one
`get_file_outline core/src/paths.rs` call:

```
[ 29.95s] <- tools/call (28.13s): ... HOME_ENV, g_mesh_home, home_from ...
g-mesh daemon: initial index built - 12177 nodes, 23673 edges (960 imports linked to their target file)
```

The daemon was stopped (`g-mesh stop`) straight after the structural build.
Its semantic backfill was left incomplete, and that matters below.

### Run

cwd `/Users/Valentin_Taiurskii/Projects/ClaudeProjects`, `mcp-shim` over
stdio. Steps: `initialize`, `notifications/initialized`, `ps`,
`tools/list`, 60 s idle, `ps`, `tools/call select_project {"project":"g-mesh"}`,
`tools/call get_file_outline {"file_path":"core/src/paths.rs"}`, 3 s, `ps`.

Every `ps` was `ps -axo pid,%cpu,time,command | grep -E 'g-mesh|bulk-index'`,
with only the grep and driver lines removed.

After `initialize` (0.07 s round trip):

```
65938   0.0   0:00.02 .../wt-gm399/target/release/g-mesh mcp-shim
65939   0.0   0:00.02 .../wt-gm399/target/release/g-mesh daemon --project-root /Users/Valentin_Taiurskii/Projects/ClaudeProjects
```

`initialize` `instructions` (whole text):

```
Structural code-graph queries over this project's index. Prefer these over grepping when you need definitions, references, call edges or imports.

/Users/Valentin_Taiurskii/Projects/ClaudeProjects is a folder of 9 projects; g-mesh serves one at a time and has indexed none of them. Before any other g-mesh tool, call select_project with the one you are working on (ask the user if unclear). Its result names the project this session then serves and carries that project's guidance; call it again to switch, or to re-read that guidance. Projects: GoogleAgenticHackaton, brushwork, datagrok-chem-1.17.5, expense-bot, g-mesh, g-mesh-bench, task-tracker-mcp, torpeek, wt-gm399.
```

After 60 s idle:

```
65938   0.0   0:00.02 .../g-mesh mcp-shim
65939   0.0   0:00.02 .../g-mesh daemon --project-root /Users/Valentin_Taiurskii/Projects/ClaudeProjects
```

`select_project g-mesh` (0.38 s), result start:

```
g-mesh: this session now serves /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh; file paths are relative to it. Guidance for this project, as a session started in /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh would receive it:

Structural code-graph queries over this project's index. Prefer these over grepping ... A result anchored by `symbol_id`, or by an unambiguous `symbol_name` ... `resolved: false` marks ... The one legitimate reason to grep afterward: a method call through a variable receiver ...
```

This is the same guidance text a direct g-mesh session receives in phase A's
`initialize`.

`get_file_outline core/src/paths.rs` answered in 0.02 s with the same
symbols as phase A (`paths::HOME_ENV`, `paths::g_mesh_home`,
`paths::home_from`, ...).

After the call:

```
65938   0.0   0:00.02 .../g-mesh mcp-shim
65939   0.0   0:00.02 .../g-mesh daemon --project-root /Users/Valentin_Taiurskii/Projects/ClaudeProjects
66181   0.0   0:00.09 .../g-mesh daemon --project-root /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh
66188  26.0   0:01.64 .../wt-gm399/core/../plugins/go/./g-mesh-plugin-go /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh
```

Daemon log (the whole file, front and g-mesh daemon together):

```
g-mesh daemon: /Users/Valentin_Taiurskii/Projects/ClaudeProjects is a folder of 9 projects - serving the front (no index)
g-mesh daemon: prepare: entered tool=get_file_outline request=4 progressToken=absent
g-mesh daemon: prepare: wait over tool=get_file_outline request=4 outcome=satisfied waited_ms=0 progress_sent=0
g-mesh daemon: prepare: past the indexing wait tool=get_file_outline request=4
g-mesh daemon: the project was walked but its semantic pass never completed - retrying it
g-mesh daemon: prepare: done tool=get_file_outline request=4
g-mesh daemon: ensure_fresh: tool=get_file_outline request=4 file=core/src/paths.rs outcome=AlreadyFresh elapsed_ms=9 progress_sent=0
```

Shim stderr:

```
g-mesh mcp-shim: nothing is serving /Users/Valentin_Taiurskii/Projects/ClaudeProjects (the current directory) - starting a daemon for it
g-mesh mcp-shim: nothing is serving /Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh (the current directory) - starting a daemon for it
```

Isolated state after the run: `projects/` holds only `959ade85d9a343b1` (g-mesh)
and `db453c25299daf62` (the folder). The folder's directory contains
`bootstrap.lock daemon.lock daemon.serving index.phase project.root` and
**no `index.db`**.

### Verdict per expectation

| Expectation | Result |
|---|---|
| After connect: the front daemon only, no plugins, no `--bulk-index` | **Met.** One daemon plus the shim. CPU time 0.02 s, unchanged after 60 s idle. No plugin or `--bulk-index` process. The folder got no `index.db`. |
| Instructions list the repos | **Met.** All 9 are listed by name. The g-mesh worktree `wt-gm399` is one of them (`isWorktree: true` in M4). |
| `select_project` result carries g-mesh's own instructions | **Met.** The result names the project, says paths are relative to it, and carries the direct-session guidance text. |
| The g-mesh daemon walks only g-mesh | **Met.** Its daemon and its only plugin run with root `.../ClaudeProjects/g-mesh`. The folder daemon never started a plugin. |
| An existing g-mesh index is reused, with no "initial index built" line | **Met.** No such line. `ensure_fresh ... outcome=AlreadyFresh elapsed_ms=9`, and the answer came back in 0.02 s. The "semantic pass never completed - retrying it" line and the go plugin's 1.64 s of CPU are this setup's doing: phase A was stopped mid-backfill on purpose. They are not a rebuild. |

### Side findings (not failures of the expectations)

1. **The front's sentence says "has indexed none of them"** even though g-mesh
   had an index in this home. The text is static
   (`core/src/mcp/instructions.rs:656`) and matches the spec wording
   (`lazy-indexing.md:736`), so it is as designed. But it is literally false
   whenever a sub-project already has its own index, which is the common
   case on the owner's machine. Owner to decide whether to reword it, e.g.
   "has not indexed this folder as a whole".
2. **The shim's switch log line reads "nothing is serving .../g-mesh (the
   current directory)"**, but the current directory is the folder, not
   g-mesh. It is cosmetic stderr wording on the switch path.

## M4: candidate-detection cost on `~/Projects/ClaudeProjects`

Command (isolated home, so the folder has no index and rule 1/2 does not settle it):

```
G_MESH_HOME=/tmp/claude-502/gm399s5/home /usr/bin/time -p \
  target/release/g-mesh debug-candidates /Users/Valentin_Taiurskii/Projects/ClaudeProjects --json
```

First invocation, run before the timed series (full JSON trimmed to its counters):

```
"candidateCount": 9, "decisionElapsedMs": 4.091807, "elapsedMs": 4.091807,
"entriesRead": 19, "mode": "multi", "reason": "two or more candidates",
"truncated": false, "walkNeeded": true
```

Timed series:

```
run 1 elapsedMs=0.657 entriesRead= 19 candidates= 9 truncated= False mode= multi walkNeeded= True
real 0.03 user 0.00 sys 0.01
run 2 elapsedMs=0.707 entriesRead= 19 candidates= 9 truncated= False mode= multi walkNeeded= True
real 0.02 user 0.00 sys 0.00
run 3 elapsedMs=0.450 entriesRead= 19 candidates= 9 truncated= False mode= multi walkNeeded= True
real 0.02 user 0.00 sys 0.00
run 4 elapsedMs=0.467 entriesRead= 19 candidates= 9 truncated= False mode= multi walkNeeded= True
real 0.02 user 0.00 sys 0.00
run 5 elapsedMs=0.782 entriesRead= 19 candidates= 9 truncated= False mode= multi walkNeeded= True
real 0.02 user 0.00 sys 0.00
```

| | value |
|---|---|
| `elapsed` over 5 runs | min 0.450 ms, median 0.657 ms, max 0.782 ms (first invocation before the series: 4.09 ms) |
| `entries_read` | 19 (all runs) |
| candidates | 9 (all runs), `truncated: false` |
| process `real` / `user` / `sys` | 0.02-0.03 s / 0.00 s / 0.00-0.01 s. Process startup dominates; the walk itself is under 1 ms. |

Cold cache was **not** measured: `/usr/sbin/purge` needs sudo, and no reboot
was done. The 4.09 ms first invocation is the closest data point, but it is
not guaranteed cold.

Ground truth: `find . -maxdepth 3 \( -name node_modules -o -name target -o -name .venv \) -prune -o -name .git -print`
finds 8 repos: GoogleAgenticHackaton, brushwork, expense-bot, g-mesh-bench,
g-mesh, task-tracker-mcp, torpeek, wt-gm399. All 8 are among the candidates.
The 9th candidate, `datagrok-chem-1.17.5`, is found by its `package.json`
marker; it has no `.git`. The folder's 15 top-level entries account for the
rest of `entries_read`: `Gobba` (`docs`, a pdf), `threadnote`
(`REQUIREMENTS.md`, `docs`) and the empty `torpeek-worktrees` hold no markers,
and `.claude`, `.DS_Store` and `transcript.txt` are not projects.

### Verdict

- `truncated` on this folder: **no**. No repo that the ground truth finds is
  missing. The depth and skip list need no change for this folder.
- Cost: **well inside budget.** The walk takes under 1 ms warm and 4 ms on
  first touch, against the 10 s bootstrap and the 100 ms `$HOME` threshold.
- `~/Projects`, `$HOME` and the synthetic wide folder were outside this
  slice's brief and were not measured. The `max_entries` bound is therefore
  still unproven here.
