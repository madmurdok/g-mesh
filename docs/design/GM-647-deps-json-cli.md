# GM-647: `g-mesh deps --json`, a CLI export of the project's import edges

Design note, slice GM-647/S1 (g-mesh 4.17.0). Requested by BlueprintAI (BLU-10,
`BlueprintAI/docs/design/architecture.md` §3.2, owner choice "Новый CLI в g-mesh").

## 0. Summary

A new subcommand `g-mesh deps --json`, run in a project directory, prints every
file-level import edge g-mesh has indexed for that project as one JSON document.
It reaches the project's daemon exactly the way `mcp-shim` does (connect, or
bootstrap one), asks it to build the index if it has not yet, waits until the
structural graph is ready, then reads the index database itself, read-only, and
prints. Default timeout 600 s, distinct exit codes for "not a project", "index
unavailable" and "timeout".

**The consumer already exists.** BlueprintAI's `scan.py` (commit 61b3233, BLU-4)
already calls `g-mesh deps --help` / `g-mesh deps --json` and parses a format of
its own description. That format differs from the one in the GM-647 task text in
one place that matters (`languages` is a *list* there, an *object* in the task).
Decision D1 below resolves it.

Decisions needing the owner (details in each section):

| # | Decision | Recommended |
|---|---|---|
| D1 | Shape of `languages` | List of language ids (what `scan.py` already parses), per-language counts in a new `coverage` object |
| D2 | How `deps` gets its data | Daemon for bootstrap + readiness, then read `index.db` directly |
| D3 | `line` on edges | Omitted in v1: the index does not store an import's line |
| D4 | No plugin indexed anything | Exit 0 with `languages: []` (not "index unavailable") |
| D5 | `--json` | Required in v1 (no text format yet) |

---

## 1. Today

### 1.1 CLI structure

- `core/src/main.rs:6-11`: `main` calls `cli::run()`; on error prints `g-mesh: <err>`
  and exits with `cli::exit_code(&err)`.
- `core/src/cli/mod.rs`:
  - `ExitStatus { code, message }` (`:63`) is how a command picks an exit code other
    than 1; `exit_code` (`:78`) maps it, every other error is 1. One code is
    defined: `PARTIAL_FAILURE_EXIT_CODE = 2` (`:58`, init/reindex). clap's own usage
    errors also exit 2.
  - `enum Command` (`:137-223`, clap derive): `init`, `config`, `status [--full]
    [--json]`, `reindex`, `plugins`, `model`, `clean`, `stop`, `mcp-shim`, hidden
    `daemon`, `debug-candidates`, `debug-embed-eval`.
  - `dispatch` (`:325-345`) maps each variant to its module's `run`.
  - Tests in the same file pin the surface, e.g.
    `help_lists_every_human_facing_command_and_hides_the_daemon` (`:577-592`).
- Precedent for machine output: `status --json` (`cli/status.rs:349`) prints one
  pretty JSON object to stdout, warnings to stderr, and reads the index directly
  (`open_index`, `:506`: read-write without CREATE, so a missing DB is never
  created).
- Project root: `status`, `reindex` use the cwd as the root (no upward search).
  `mcp-shim` prefers `$CLAUDE_PROJECT_DIR` over cwd (`shim.rs:191`).

### 1.2 How `mcp-shim` reaches or bootstraps the daemon

`shim::run` (`core/src/shim.rs:105`) -> `connect_or_bootstrap(root, origin)`
(`:244`, private):

1. `daemon::endpoint(root)` (per-project socket), length check.
2. A rebuild in progress (`g-mesh reindex`/`init` holding the marker) returns a
   `router::Rebuilding` error (`shim/router.rs:166`).
3. `incumbent()` (`:332`): connect; if the running daemon is from an older build it
   is retired (`retire_outdated_daemon`, `:366`) and replaced.
4. Otherwise, under a cross-process bootstrap lock: `spawn_detached_daemon` (`:533`,
   `g-mesh daemon --project-root <root>`, own process group via `process::detach`,
   so a caller's `killpg` does not reach it), then `wait_until_listening` (`:517`)
   for up to `BOOTSTRAP_TIMEOUT` = 10 s (`G_MESH_BOOTSTRAP_TIMEOUT_MS` in tests).
   The daemon binds *before* any walk, so 10 s is a bind budget only.

What travels over the socket is plain MCP (newline-delimited JSON-RPC); the shim
proxies bytes. There is no non-MCP control request.

**Lazy activation.** A freshly started daemon walks nothing
(`daemon/activation.rs` module doc; `docs/architecture/lazy-indexing.md` D2): the
walk starts on the first index-needing tool call (`mcp/mod.rs:276`,
`prepare -> indexing.request_activation()`), which then waits
(`wait_for_index`, `mcp/mod.rs:476`) until the phase allows structural answers,
capped server-side at `DEFAULT_INDEX_WAIT_CAP` = 25 min
(`G_MESH_INDEX_WAIT_CAP_MS`); on the cap the call returns a retryable tool error
and indexing continues. `prepare` also replays changes queued while a plugin slept
before answering. The phase is published to `<state_dir>/index.phase`
(`daemon/indexing_status.rs:53`: `unindexed`, `walking`, `structural`, `embedding`,
`ready`, `failed`; `front` for a folder-of-projects daemon), read by
`daemon::read_phase_in` (`daemon/mod.rs:152`). The integration-test helper
`core/tests/it/common/mod.rs:158-210` (`trigger_activation`) is a ~40-line raw
NDJSON MCP client doing exactly "initialize, initialized, `tools/call
get_file_outline {file_path: "\0"}`" to start a walk; it uses the lib's public
`protocol::ndjson_frame::{write_ndjson_frame, read_ndjson_frame}`.

### 1.3 How `get_dependencies` computes its answer

`mcp/mod.rs:887-910` -> `get_dependencies::handle_covered`
(`core/src/mcp/get_dependencies.rs:802`) -> `from_file` (`:407`) -> a bounded walk
over `IMPORTS` edges (`graph::traversal`), default depth 2, fan-out 50. It is
per-anchor only: one file or module in, its reachable nodes out (`id`, `kind`,
`qualifiedName`, `filePath`, `depth`). No whole-project form exists.

What an `IMPORTS` edge points at depends on the language (`graph/imports.rs`
module doc, `get_dependencies.rs:494` doc):

| Target node | When | Example |
|---|---|---|
| `File` node | TS/JS specifier resolved to a file | `src/a.ts` -> `src/b.ts` |
| Container node (`Module`, `nativeKind = 'container'`, `filePath = ''`, row in `containers`) | Rust module, Python module, Go package | `src/main.rs` -> `app::config` (`use crate::config` in crate `app`); `a.py` -> `requests.adapters`; `x.go` -> `github.com/gin-gonic/gin/render` |
| `Module` placeholder, `nativeKind = 'external_module'` | a package/builtin outside the project | `react`, `std::fs`, `os` |
| `Module` placeholder, `nativeKind = 'resolved_module'` left unlinked, or no nativeKind | the plugin thought it was in-project but the index has no such file/container | `./missing` |

So "file-level edges" are not stored as such for Rust/Python/Go: a container has
to be mapped back to the file(s) that define it. The inverse already exists one
way: `graph::containers::defining_containers(conn, file)` (`containers.rs:675`)
returns the container a file *is* (measured unambiguous on 234 files of gin,
ripgrep, requests). A Go package container is defined by every `.go` file in its
directory, so its file-level target is naturally a directory.

The `edges` table (`storage/schema.rs:306`) has no line/position column, and the
plugin wire shape `WireEdge` (`wire/src/lib.rs:499`) carries none either; a
resolved import's placeholder (which had a position) is dropped when linked.
**An import's line is not in the index.**

### 1.4 Whole-project dump: index or new daemon request?

Everything needed is in `index.db` and readable with four queries (edges of kind
`IMPORTS` joined to their endpoints; `containers`; `File` counts per language;
`language_outcome`). No daemon request exists for it; one would have to be added
(see D2 option A).

### 1.5 The consumer: BlueprintAI `scan.py`

`BlueprintAI/plugins/blueprint-ai/skills/blueprint/scripts/scan.py:1367-1426`
(`gmesh_deps`), docstring `:19-38`:

- Detection: `g-mesh deps --help` exit 0 (30 s probe). Then `g-mesh deps --json`
  with `cwd = repo`, stdin/stderr to /dev/null, its own timeout (default 600 s,
  `--gmesh-timeout`/`$BLUEPRINT_GMESH_TIMEOUT`), kills the **process group** on
  timeout. Non-zero exit, timeout or unexpected JSON -> rules fallback for all
  languages.
- Parses: `languages` **must be a JSON list** (else "bad-json"); `edges` a list of
  objects with string `from`/`to`; `line` optional positive int; top-level
  `unresolved: {lang: n}` optional; unknown keys ignored. Paths may be absolute
  under the repo or relative POSIX. `to` may be a directory.
- Language ids are mapped through its own alias table, case-insensitive. Its
  rules tier classifies imports as internal (`file`|`dir`) / external / unresolved,
  and only *unresolved* feeds `imports_unresolved`.
- scan.py is run inside Claude Code sessions, where `$CLAUDE_PROJECT_DIR` names the
  *session's* project, not the repo being scanned.

---

## 2. How `deps` gets its data (D2)

**A. A new daemon-side request (e.g. a hidden MCP tool `export_dependencies`).**
The CLI speaks MCP to the daemon and prints the tool's JSON.
- Benefit: one reader of the index (the daemon), snapshot taken under the
  daemon's own store lock, freshness replay comes for free.
- Risk: a tool the agent must not see; rmcp's tool router lists every `#[tool]`, so
  hiding it means filtering `tools/list` and keeping the instructions-size and
  tools-list tests green; a multi-MB result through one JSON-RPC message; more
  daemon code and a second public contract (tool + CLI).

**B. Open `index.db` directly, no daemon.**
- Benefit: simplest; no socket.
- Risk: cannot satisfy "cold project: bootstraps the index". Building in-process
  means what `reindex` does (stop the daemon, wipe, rebuild), which would kill a
  live MCP session's daemon and rebuild from scratch on every call. A stale index
  (files edited while no daemon ran) would be printed as if current.

**C. Hybrid (recommended): daemon for bootstrap and readiness, direct read for data.**
1. `connect_or_bootstrap(root)` exactly as the shim (made `pub(crate)`).
2. Over that connection, a minimal raw-NDJSON MCP exchange (the test helper's,
   moved into the lib): `initialize`, `notifications/initialized`, `tools/call
   get_file_outline {file_path: "\0"}`; then *read* frames until the response to
   that call arrives (skipping notifications). Its arrival means the daemon's
   `prepare` is over: activation requested, structural wait finished or capped,
   queued plugin changes replayed. The answer itself is ignored.
3. Read `index.phase`: `structural`/`embedding`/`ready` -> ready; `failed` -> index
   unavailable (exit 4); `walking` (the 25-min server cap fired) -> send the call
   again while the client deadline allows.
4. Open `index.db` read-only (`status::open_index`'s flags plus `PRAGMA
   query_only = ON`), one read transaction (WAL gives a consistent snapshot while
   the daemon keeps writing), run the export queries (§4), print.
- Benefit: no new daemon surface, no `tools/list` change, no new protocol; same
  bootstrap, version-retire and rebuild handling as every MCP session; the
  daemon keeps indexing after the CLI exits or is killed, so a timed-out run
  makes the next one fast; the walk is never duplicated. Schema matches because
  `connect_or_bootstrap` retires a daemon older than this binary.
- Risk: the CLI depends on the index schema directly (same binary, so it moves
  with it; `status` already does this). The readiness signal is two things (the
  call returning, then the phase word) rather than one typed answer. Retiring an
  outdated daemon disconnects any live MCP session of that project once (its shim
  reconnects; same as any new shim).

---

## 3. Cold project, timeout, exit codes

**Root.** The cwd, or `--project <DIR>`. `$CLAUDE_PROJECT_DIR` is ignored (it names
the calling session's project, see 1.5). Canonicalised like the shim does. If
`daemon::candidates::detect(root)` says `Multi` (a folder of projects, where the
daemon would run as a front), exit 3 before starting anything.

**Timeout.** `--timeout <SECONDS>`, default 600 (= BlueprintAI's own default and its
design's M14), `0` = no limit. One deadline covers connect/bootstrap, the
readiness wait and any rebuild wait; the export query and printing are not
bounded (seconds on a 5k-file repo). Socket reads use the remaining time as their
read timeout. On expiry the daemon is left running and keeps indexing.

**A rebuild in progress** (`reindex`/`init` running): retried every 500 ms until the
deadline, then exit 5 with the rebuild's command and pid in the message.

**Exit codes.**

| Code | Meaning | stderr |
|---|---|---|
| 0 | JSON printed (some languages may be failed/absent: see `coverage`) | one line per language not indexed (`cli::language_outcome_lines`, as `init` prints) |
| 1 | Unexpected error (I/O, SQL, malformed state) | `g-mesh: <error chain>` |
| 2 | Usage error (clap: unknown flag, missing `--json`) | clap's message |
| 3 | Not a project: the root is a folder of projects, or does not exist / is not a directory | `g-mesh: <root> is a folder of N projects; run deps inside one of them` |
| 4 | Index unavailable: daemon did not start or accept within its bind budget, the walk failed (`index.phase` = `failed`), the endpoint path is too long, or the daemon disappeared mid-wait and could not be restarted | `g-mesh: the index for <root> is unavailable: <reason>` (+ daemon log path) |
| 5 | Timeout: not ready within `--timeout` | `g-mesh: the index for <root> is still being built (<progress>); waited <t>. The daemon keeps indexing; run again.` |

Code 2 is not reused for partial indexing here (unlike `init`/`reindex`): a
consumer reads a non-zero exit as "no output", and a partly indexed project still
has a usable answer for the languages it lists.

**Progress on stderr.** While waiting, one line every 10 s:
`g-mesh deps: waiting for the index of <root> (<phase>, <progress detail>)`, from
`daemon::read_progress_in`. A cold start also prints the shim's existing
"nothing is serving ... starting a daemon" line. stdout carries the JSON only.

**D4 (owner): nothing indexed.** If no installed plugin covers any of the project's
languages (the situation of the installed g-mesh on this machine while GM-645 is
open), the index is valid and empty. Recommended: exit 0, `languages: []`,
`coverage` naming the `plugin_absent` languages with file counts and stderr lines
saying which plugin to install. scan.py then reports `no-languages` and uses its
rules. Alternative: exit 4. Benefit of 0: "no edges because no plugin" is an
honest answer, and "index unavailable" stays reserved for a broken daemon. Risk: a
caller that only checks the exit code thinks g-mesh covered something.

---

## 4. JSON format, version 1

### 4.1 D1 (owner): `languages` list vs object

- Task text: `languages: {<lang>: {files, edges, unresolved}}` (an object).
- scan.py today: `languages` must be a list; anything else is "bad-json" and every
  language falls back to rules.

Recommended: keep `languages` a **list** (scan.py works unchanged, no BlueprintAI
release), put the per-language numbers under a new key `coverage`, and also emit
the top-level `unresolved: {lang: n}` scan.py already reads. Alternative: the task's
object shape plus a BlueprintAI change (a BLU task; until it ships g-mesh's output
would be rejected). Risk of the recommendation: the field names differ from the task
text, which the format doc must state.

### 4.2 Schema

```json
{
  "version": 1,
  "project_root": "/Users/me/src/app",
  "languages": ["python", "rust", "typescript"],
  "coverage": {
    "go":         {"status": "plugin_absent", "files": 63},
    "python":     {"status": "indexed", "files": 56, "edges": 140, "unresolved": 2, "external": 31},
    "rust":       {"status": "indexed", "files": 40, "edges": 95, "unresolved": 0, "external": 12},
    "typescript": {"status": "indexed", "files": 67, "edges": 210, "unresolved": 3, "external": 44,
                   "extensions": [".cjs", ".cts", ".js", ".jsx", ".mjs", ".mts", ".ts", ".tsx"]}
  },
  "unresolved": {"python": 2, "typescript": 3},
  "edges": [
    {"from": "pkg/cli.py",   "to": "pkg/config.py", "kind": "file", "language": "python",     "module": "pkg.config"},
    {"from": "server/main.go","to": "server/render", "kind": "dir",  "language": "go",         "module": "example.com/app/server/render"},
    {"from": "src/a.ts",     "to": "src/b.ts",      "kind": "file", "language": "typescript", "module": null},
    {"from": "src/main.rs",  "to": "src/config.rs", "kind": "file", "language": "rust",       "module": "app::config"}
  ]
}
```

Fields:

- `version`: integer, the format version (see §5).
- `project_root`: absolute canonical path of the root the paths are relative to.
- `languages`: sorted ids of languages whose outcome is `indexed` (g-mesh plugin
  ids: `typescript`, `rust`, `python`, `go`). A consumer trusts g-mesh's edges for
  these and nothing else. Note `typescript` covers JS files too; `coverage.<lang>.
  extensions` (from the plugin manifest) says which extensions, so a consumer can
  map its own language ids.
- `coverage.<lang>`: every language with a `language_outcome` row. `status`:
  `indexed` | `plugin_absent` | `failed` (+ `error`). For `indexed`: `files` (File
  nodes), `edges` (edges in `edges` from this language's files), `unresolved`,
  `external`. For `plugin_absent`: `files` when counted.
- `unresolved`: `{lang: n}` for indexed languages with n > 0 (the same numbers as
  `coverage`), kept for scan.py.
- `edges[]`, one per distinct `(from, to)` pair:
  - `from`: the importing file.
  - `to`: the imported file, or a directory when `kind` = `dir`.
  - `kind`: what `to` names: `file` | `dir` (matches scan.py's internal
    `"file"|"dir"`). The edge itself is always an import in v1.
  - `language`: the importing file's language.
  - `module`: the container key the import resolved to (Rust/Python/Go), `null`
    for a file-to-file import (TS/JS). Lets a reader compare with
    `get_dependencies`, whose rows carry that key as `qualifiedName`.
  - `line`: **not emitted in v1** (D3). The index stores no import position (1.3);
    adding one is a plugin-protocol change in four plugins and belongs in its own
    task. The name stays reserved for that meaning (1-based line of the import
    statement in `from`), so adding it later is additive. scan.py treats it as
    optional.

### 4.3 How targets are classified

For every `IMPORTS` edge whose source is a `File` node (`f`) and target `t`:

1. `t` is a `File` node -> edge `kind: file`, `to = t.filePath`.
2. `t` is a container (row in `containers`) -> its defining files: every File `F`
   with `t` in `defining_containers(F)` (built once for all files and inverted).
   If none, fall back to the files holding the container's `DEFINES` members.
   Then: one file -> `kind: file`; several files all in one directory ->
   `kind: dir`, `to` = that directory; several directories -> one `kind: file` edge
   per file; none -> counted as `unresolved`.
3. `t` is a `Module` with `nativeKind = 'external_module'` -> counted as `external`,
   no edge.
4. Any other `Module` placeholder -> counted as `unresolved`, no edge.

Edges with `from == to` are dropped. Duplicates (two `use` lines naming the same
module) collapse to one edge; counts in `coverage.edges` are after the collapse;
`unresolved`/`external` count distinct `(from, specifier)` pairs.

Meaning of `unresolved`: imports that look in-project but name nothing the index
holds. External packages are not unresolved (they are `external`), matching
scan.py's internal / external / unresolved split.

### 4.4 Ordering, paths

- `edges` sorted by `(from, to, kind)` byte order; `languages` sorted; object keys
  of `coverage` and `unresolved` sorted (serialise from `BTreeMap`). Two runs over
  an unchanged index print byte-identical output.
- Paths are project-relative, `/`-separated, no leading `./`, no trailing `/` on
  directories, `.` for the root directory. Index `filePath`s are already
  project-relative; on Windows backslashes are converted (must confirm what the
  index stores there).
- stdout is one compact JSON document followed by a newline (not pretty: output
  can be several MB).

### 4.5 D5 (owner): `--json`

Recommended: `--json` is required; `g-mesh deps` alone is a usage error (exit 2)
naming the flag. Benefit: a future human-readable default does not change what
existing scripts get. Risk: one more flag to type. Alternative: JSON by default and
`--json` accepted as a no-op.

---

## 5. Format doc and versioning

- New `docs/cli/deps-json.md`: the command, flags, exit-code table, the schema of
  §4 with the example, target classification, timeout behaviour, and a version
  history table (`1` - g-mesh 4.17.0 - initial).
- `README.md`: a short "Exporting the dependency graph: `g-mesh deps`" section next
  to the `g-mesh status` paragraphs (~`:819`), linking the format doc.
- ADR `docs/adr/0041-deps-cli-reads-the-index-after-a-daemon-readiness-wait.md`
  for D2 (CLAUDE.md: decisions go to ADRs), linked once from the module doc of
  `cli/deps.rs`; index entry in `docs/adr/README.md`.
- Versioning rule (in the doc): adding a key or a new `kind`/`status` value does
  not bump `version` (consumers must ignore unknown keys and treat an unknown
  `kind` as unknown, as scan.py does); removing or renaming a key, or changing a
  field's meaning or type, bumps it. The constant `DEPS_FORMAT_VERSION: u32 = 1`
  lives in `cli/deps.rs`, and a unit test pins the top-level key set to it so a
  shape change forces a deliberate bump.

---

## 6. Edit map

Change:

| File | Where | What |
|---|---|---|
| `core/src/cli/mod.rs` | `enum Command` `:137-223` | `Deps(DepsArgs)`: `--json` (required), `--timeout <SECONDS>` (u64, default 600, 0 = none), `--project <DIR>`; doc comment = `--help` text |
| `core/src/cli/mod.rs` | `dispatch` `:325-345` | `Command::Deps(args) => deps::run(&args)`; `mod deps;` with the other `mod` lines |
| `core/src/cli/mod.rs` | constants near `:58` | `NOT_A_PROJECT_EXIT_CODE = 3`, `INDEX_UNAVAILABLE_EXIT_CODE = 4`, `TIMEOUT_EXIT_CODE = 5` |
| `core/src/cli/mod.rs` | test `:577-592` | add `deps` to the listed commands; parse tests for `deps --json`, missing `--json`, `--timeout` |
| `core/src/shim.rs` | `connect_or_bootstrap` `:244` | `pub(crate)`; re-export `router::Rebuilding` as `pub(crate)` so `deps` can tell it apart (downcast) |
| `core/src/cli/status.rs` | `open_index` `:506` | `pub(crate)` (or move to `storage::connection` as `open_existing_for_read`) |
| `core/src/graph/containers.rs` | after `defining_containers` `:675` | `pub fn defining_files(conn) -> Result<HashMap<String /*container nodeId*/, Vec<String>>>` (all File nodes, inverted) and the members-fallback lookup |

Add:

| File | Content |
|---|---|
| `core/src/cli/deps.rs` | `run(args)`: root, Multi check, deadline, `wait_ready`, `export`, print. `wait_ready(root, deadline)`: connect/bootstrap loop (rebuild retry), activation exchange, phase check, progress lines. `export(conn) -> DepsReport` (serde structs, `BTreeMap`s). `DEPS_FORMAT_VERSION` |
| `core/src/daemon/client.rs` (or `cli/deps/activate.rs`) | the raw-NDJSON MCP exchange: `initialize`, `initialized`, `tools/call get_file_outline "\0"`, read until response id 1 with a read timeout. The test helper `common::try_trigger_activation` can later call it (not required here) |
| `docs/cli/deps-json.md`, README section, ADR 0041 + index line | §5 |

Read for context (no change): `mcp/get_dependencies.rs` `DependencyNode` `:80-120`
and `from_file` `:407` (what the tests compare against); `graph/imports.rs` header
`:1-120` (placeholder kinds, `EXTERNAL_MODULE_NATIVE_KIND`);
`daemon/indexing_status.rs:28-63` (phase words); `daemon/mod.rs:132-153` (phase
and progress files); `cli/status.rs:368-450` (`collect`, how language outcomes and
plugin manifests are read; reuse `manifest::discover` for `extensions`);
`core/tests/it/common/mod.rs:150-260` (activation and wait helpers);
`storage/schema.rs:209-357, 488-494` (tables).

Cross-file facts above came from grep and reading, not g-mesh: `select_project
g-mesh` answered "Nothing is indexed here ... no plugin installed" (the GM-645
state), so no g-mesh structural call could answer.

---

## 7. Behaviours the tests slice must pin

New `core/tests/it/cli_deps.rs` (+ `mod cli_deps;` in `core/tests/it/main.rs`),
temp projects written in code like `import_resolution.rs`, real TS/Rust/Python
plugins from the workspace (`common` helpers). Likely heavy: add to the
`.config/nextest.toml` `local` heavy list if > 10 s.

1. `g-mesh deps --help` exits 0 and mentions `--json` and `--timeout`.
2. `g-mesh deps` without `--json` exits 2.
3. TS fixture (`a.ts` imports `./b`, `react`, `./missing`): edge `a.ts -> b.ts`
   `kind: file`, `module: null`; `coverage.typescript.external == 1`,
   `unresolved == 1`; top-level `unresolved.typescript == 1`.
4. Rust fixture (`main.rs` `mod config; use crate::config::X;`, `use std::fs`):
   edge `src/main.rs -> src/config.rs`, `module: "<crate>::config"`; `std` counted
   external.
5. Python fixture (`pkg/cli.py` `from pkg import config`, `import os`): edge to
   `pkg/config.py`; a package import (`import pkg`) maps to `pkg/__init__.py`.
6. For every file of the three fixtures: the set of (`module` ?? `to`) of its
   `deps` edges equals the set of (`qualifiedName` for container rows, `filePath`
   for File rows) of `get_dependencies {file_path, direction: outgoing,
   max_depth: 1}` minus external/unresolved placeholder rows.
7. Output is byte-identical across two runs; `edges` sorted; paths relative with
   `/`.
8. Cold project (no daemon, no index): `deps --json` bootstraps, prints, exit 0;
   a daemon is left running.
9. Timeout: with a walk made slow (existing slow-plugin test hooks) and
   `--timeout 1`, exit 5, stdout empty, the daemon still running and later
   reaching `structural`.
10. Index unavailable: walk fails (missing plugin binary fixture,
    `common::missing_workspace_binary_plugin_root`) -> exit 4, stdout empty.
11. Folder of projects as cwd -> exit 3 and no daemon started.
12. `$CLAUDE_PROJECT_DIR` set to another dir is ignored; `--project` is honoured.
13. Plugin absent for a language with files: listed in `coverage` as
    `plugin_absent`, not in `languages`, exit 0 (if D4 = 0).
14. Unit (no daemon): `export` on a hand-built index: container defined by two
    files in one dir -> one `dir` edge; self-edge dropped; duplicate imports
    collapse; top-level key set pinned to `DEPS_FORMAT_VERSION`.
15. scan.py compatibility: the document passes scan.py's own checks (`languages`
    list, string `from`/`to`), asserted structurally in Rust, not by running scan.py.

Controls (revert -> must fail): container-to-file mapping (6 fails); readiness
wait skipped (8 prints empty edges); exit-code mapping (9/10); `CLAUDE_PROJECT_DIR`
ignore (12).

**Must confirm** (S2 states which, with evidence):
- Every `IMPORTS` edge leaves a `File` node (count the others; expect 0).
- Python `import pkg` resolves to container `pkg` and `defining_containers(
  "pkg/__init__.py")` returns `pkg`.
- The TS plugin marks bare packages `external_module` (so they are not counted
  unresolved), and what an unmarked placeholder means today.
- A read-only connection (`query_only`) on the WAL index while the daemon writes:
  no `SQLITE_BUSY` in practice; else use a busy timeout.
- `get_file_outline "\0"` goes through `prepare` and returns promptly once ready
  (the test helper relies on the first half only).
- Index `filePath` separators on Windows.
- BlueprintAI: `typescript` covering `.js` files maps to its JS record (its alias
  table), or it reads `coverage.typescript.extensions`. A BlueprintAI-side
  question, not a GM-647 blocker.

---

## 8. Re-slicing

The draft holds, with two adjustments:

- **S2 code + format doc (opus)**: also the ADR and README section; resolves the
  "must confirm" list with evidence; no tests beyond the `cli/mod.rs` parse tests
  that already live beside the enum.
- **S3 review**: unchanged.
- **S4 tests (opus)**: needs all three plugin binaries built; scope §7. If the
  integration file runs > 10 s per test, register it as heavy.
- **S5 verify**: `scripts/test-local.sh --base release-4.17.0`; controls from §7.
- If D1 goes the task-text way (object `languages`), add a BlueprintAI task to
  update `scan.py` before 4.17.0 ships, or the CLI's output will be rejected.
