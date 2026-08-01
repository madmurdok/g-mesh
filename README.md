# g-mesh

Local structural code-graph indexer for AI agents, exposed as an MCP server.
MVP scope: JavaScript/TypeScript only, structural (tree-sitter) resolution —
no TS compiler API semantics yet. See `REQUIREMENTS.md` and
`docs/architecture/g-mesh-v1.md` for the full design.

## Layout

- `core/` — Rust workspace: the `g-mesh` binary (`mcp-shim` + the per-project
  `daemon`), SQLite storage, graph queries, file watcher, and the MCP tool
  surface.
- `plugins/js-ts/` — Node/TypeScript language plugin: tree-sitter parsing,
  bulk indexing, incremental reparse. Spawned by the daemon as a child
  process, one instance per project.

The daemon and shim are one binary (`core/target/{debug,release}/g-mesh`);
the plugin is a separate Node entry point the daemon launches with `node`.

## Prerequisites

- Rust toolchain (`cargo`, stable) — build the core.
- Node.js >= 20 and `node` on `PATH` — build the plugin, and required at
  *runtime* because the daemon spawns the plugin via `node <entry.js>`.

## Build

```bash
# 1. Core (Rust binary: g-mesh, with the mcp-shim/daemon subcommands)
cd core
cargo build --release
# -> core/target/release/g-mesh

# 2. JS/TS plugin
cd ../plugins/js-ts
npm install
npm run build
# -> plugins/js-ts/dist/src/index.js
```

Build order doesn't matter, but both are required — the daemon refuses to
start (hard failure) if it can't spawn the plugin.

**Important**: there is no distribution/packaging step yet. The daemon
resolves the plugin's path relative to `core`'s own source tree, baked in at
*compile time* (`core/src/daemon/plugin.rs`):

```
<repo>/plugins/js-ts/dist/src/index.js
```

So the built `g-mesh` binary only works run from (or copied while keeping
the relative layout of) this checked-out repo. To point it at a plugin build
elsewhere, override with an env var:

```bash
export G_MESH_JS_TS_PLUGIN_PATH=/path/to/plugins/js-ts/dist/src/index.js
```

## How it finds a project

`g-mesh mcp-shim` takes no arguments. It resolves the project root — the
only project identity it needs, hashed to derive the daemon's socket, lock
and SQLite paths under `~/.g-mesh/projects/<hash>/` — like this:

1. **`CLAUDE_PROJECT_DIR`**, if set. Claude Code sets this in every stdio
   MCP server it spawns, in every registration scope (local/project/user) —
   unlike the process's own `cwd`, which Claude Code's docs call unreliable
   for this. This is what lets `g-mesh` be registered *once, globally* and
   still get the right project per session.
2. Otherwise, the shim's own **current working directory** — the only
   option for an MCP client that isn't Claude Code, and how earlier versions
   of this doc had you register it (once per project, cwd pinned at
   registration time).

The shim is a stateless proxy: on first connect for a project it bootstraps
a detached daemon (`g-mesh daemon --project-root <root>`), which opens the
project's SQLite index, spawns the JS/TS plugin, builds the initial index if
the project has never been indexed (see below), starts the file watcher, and
serves the MCP tool surface over an `AF_UNIX` socket. The daemon outlives the
shim and is reused by later connections for the same project.

## Is this worth registering?

Honest answer, from actual measurement (full numbers in
`../g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`): g-mesh is
**not** cheaper on average in token terms. On simple lookup-shaped questions
(find a definition, list a file's exports, an unambiguous
`find_implementations`) it costs *more* tokens than an agent just using
Read/Grep/Glob — about +46% in isolated measurement, and worse (+67%) inside
a long continuing session, because the dominant cost (re-reading the tool
schemas on every turn) grows with conversation length rather than shrinking
the way a one-time setup cost would.

Where it wins, consistently and by a wide margin, is multi-hop questions
(chained relationships — "which files call both X and Y", "what implements
this interface, and which of those also call Z") and ambiguous-name
resolution (two same-named symbols declared in different scopes or files).
On these, a grep-based agent has real, unbounded tail risk — 20-40+ tool-call
round-trips, 300-580k tokens, occasionally exhausting a real budget outright
— while g-mesh keeps the same class of question in a bounded 11-15 turns and
gets an answer grep cannot reach even in principle (re-export chains,
same-qualified-name-but-different-declaration symbols).

**Registration guidance**: worth registering for codebases where cross-file
impact analysis, deep call graphs, or ambiguous naming are a real, recurring
cost — monorepos, large multi-package projects, codebases with several
same-named exports across modules. Not worth it for a small, single-package
project where most real questions are simple lookups grep already handles
fine — there the fixed tool-schema cost has nothing to pay itself back
against. This is a per-project decision, orthogonal to the global-vs-per-project
*scope* question the next section covers — that's about where a registration
applies once you've decided to register at all.

### Reducing self-verification cost (optional)

A repeated pattern in g-mesh-bench's own measurements
(`../g-mesh-bench/docs/results/v0.4.0-disambiguation-tail-findings.md`):
Claude Code often re-checks an already-correct, already-complete g-mesh
answer with an extra manual grep anyway — on the benchmark's hardest
disambiguation task this tail alone was 54-72% of that task's total spend.
Restating the guarantees in the MCP server's own instructions did not stop
it, in repeated live spot-checks.

Most of that turned out not to be a wording problem. Until this was fixed,
*every* edge left the extractor `resolved: false`, including the ones it had
matched against a declaration sitting in the same file — so on a typical
`find_callers` result the majority of rows carried the marker that the tool
instructions describe as the row worth double-checking. The agent's extra
grep was a rational response to that. Two genuine wrong-edge bugs behind it
were fixed at the same time (a call to a local shadowing a same-named
file-level function, and a bare name matching a class member no bare name can
reach), and a same-file edge now says `resolved: true`. Corpus-wide that
moves `resolved: false` from 100% of edges to ~8% — the cross-file ones core
really could not confirm.

Measured on the same prompt against the same project, before and after
(4 samples each, `claude -p`, sonnet): mean cost $0.081 → $0.050, mean turns
6.5 → 4.25, and the long verification tails (one run spent 6 greps and 3 reads
re-checking rows) disappeared. What is left in the "after" runs is a single
grep serving a question g-mesh does not answer at all — "which *other*
symbols have similar names?" — not a re-check of what it did answer.

If you still want to trim that last step, put a short instruction in your
project's `CLAUDE.md` (a task/project prompt reaches the model more reliably
than a server capability description does):

```markdown
# Code search (TypeScript/JavaScript projects)

- In TS/JS projects, prefer g-mesh (`mcp__g-mesh__*`) for cross-file impact analysis, ambiguous naming (same symbol name declared in different scopes/files), and call-graph/multi-hop questions (callers, implementations, transitive dependencies) — grep can't resolve these reliably and has real unbounded cost (many round-trips, occasionally very expensive) when it tries. For simple, unambiguous single-symbol lookups, grep/`Explore`/manual reading is often just as fast and cheaper — g-mesh's tool schema adds fixed overhead per turn that doesn't pay for itself on easy questions (measured: g-mesh costs *more* tokens than grep on simple lookups, both isolated and in a long session — see `g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`). Fall back to grep when g-mesh returns no result, errors, or the target isn't something it tracks (non-code files, config, CSS, etc.).
- No manual indexing command exists or is needed. The g-mesh daemon bootstraps and indexes a project automatically on its first tool call in that project's directory. On first use in a new project, just issue any g-mesh call (e.g. `get_file_outline` on a source file) to trigger indexing, then proceed.
- How to use the tools:
  - `get_file_outline(file_path)` — list a file's top-level symbols before reading it in full, or to find the right symbol name to query next.
  - `find_definition(symbol_name)` or `find_definition(file_path, position)` — resolve a symbol to its definition and get its `symbol_id`. Not required before the tools below — they accept `symbol_name` directly and skip this call when the name is likely unambiguous, saving a round-trip. Call `find_definition` first only when you already expect ambiguity or need the declaration site itself.
  - `find_references(symbol_name or symbol_id)` — every usage of a symbol across the project; use before renaming or removing something.
  - `find_callers(symbol_name or symbol_id)` / `find_callees(...)` — walk the call graph up or down from a function.
  - `find_implementations(symbol_name or symbol_id)` — concrete types implementing an interface/abstract class.
  - `get_dependencies(file_path, direction: Outgoing|Incoming)` — walk the import graph (what a file imports / what imports it); use for impact analysis before changing a shared module.
  - If a `symbol_name` turns out ambiguous, the result carries `ambiguous: true` with a ranked candidate list — re-query using a candidate's `id` as `symbol_id`, not its `qualifiedName` (the same qualifiedName can name more than one declaration).
- Typical flow: call `find_references`/`find_callers`/`find_callees`/`find_implementations` directly with `symbol_name` when it's likely unique; only call `find_definition` first if you expect ambiguity or need the declaration site itself. Use `get_file_outline` first if you don't already know the right symbol name.
- A `find_references`/`find_callers`/`find_callees`/`find_implementations` result is complete for the question it answers when: it was anchored by `symbol_id` or an unambiguous `symbol_name` (same guarantee either way), every row shows `resolved: true`, and the response has no `allUnresolved: true` flag — don't re-verify that with grep/Read. As of g-mesh 0.8.x, `resolved: false` is a narrow, accurate signal (only edges whose target is in another file g-mesh couldn't confirm — same-file edges are always `resolved: true`, matched against declarations actually in scope), not a blanket disclaimer, so still check: a row that shows `resolved: false` (check that row, not the whole list), a response with `allUnresolved: true` (the whole page is unconfirmed), or anything the result doesn't claim to cover at all — e.g. whether other, similarly-named symbols exist elsewhere, or a method call reached through a variable receiver (`x.foo()`, which produces no edge by design). Measured on real g-mesh-bench runs after the 0.8.x same-file-resolution fix: mean cost dropped ~38% and mean turns ~35% on the task this was tested on, with the remaining tool calls answering things g-mesh genuinely doesn't cover rather than re-checking it — but grep/Read still earn their keep on the cases above, so don't suppress those.
```

Prefer that shape over a blanket "never verify anything". `resolved: false`
is now a narrow, accurate flag rather than a blanket disclaimer, so an
instruction that suppresses it throws away the one honest quality signal in
the response — and the measurements above show the expensive part of the tail
is already gone without it.

**A second, unrelated truncation bug (fixed in 0.13.0):** Claude Code
truncates an MCP server's `instructions` field at 2KB — a single flat cap on
that one field, independent of and not shared with each tool's own 2KB
`description` budget. `GMeshMcpServer::get_info()`'s instructions text had
grown to 3016 bytes, so the model never saw the back half of it, including
the single most directly actionable line ("only fall back to grep when you
specifically suspect a variable-receiver method call was missed, not as a
routine double-check"). Trimmed to 1804 bytes with the core anti-grep rule
moved to the front, per Claude Code's own documented advice to put critical
details near the start of a field that might get cut.

That fix helped, but did not close the gap alone — evidence for the same
point this section already makes above. A live before/after comparison
(`g-mesh-bench`, `G_MESH_BENCH_REPS=low`, 8 `task-tracker-mcp` tasks, `gmesh`
arm): mean turns 3.8 → 3.1 (matching the `gmesh-trusted` arm's 3.1) and
sessions with a habitual re-grep after a fully-resolved (`resolved: true`,
`allUnresolved: false`) answer dropped from 4/8 to 1/8 — real progress, but
not zero. The one remaining case was an ordinary symbol lookup re-checked
with grep for no signaled reason at all, confirming again that a
well-written, correctly-sized server instructions field narrows the habit
but a project-level `CLAUDE.md` instruction (the snippet above) is still
what actually closes it.

## Register with an MCP client

**Recommended (Claude Code): register once, globally.** Since the shim reads
`CLAUDE_PROJECT_DIR`, a single user-scoped registration works for every
project you open Claude Code in — no per-project setup:

```bash
claude mcp add g-mesh -s user -- /path/to/g-mesh/core/target/release/g-mesh mcp-shim
```

**Fallback (any other stdio MCP client): register per project.** Run from
inside the target project's directory, so the client's own `cwd` — and
therefore the shim's, absent `CLAUDE_PROJECT_DIR` — is the project root:

```bash
cd /path/to/target-project
claude mcp add g-mesh -- /path/to/g-mesh/core/target/release/g-mesh mcp-shim
```

## First run: the initial index

The first time a daemon starts for a project it walks the whole tree once
(gitignore-aware, skipping `.git`, `node_modules`, `dist`, and `.claude`), parses every
`.ts`/`.tsx`/`.js`/`.jsx` file, and commits the result **before** it accepts
any MCP connection — so a client's first tool call already sees a complete
graph, with nothing to touch or warm up first. Expect that first start to
take proportionally longer on a large project; every later start is
immediate.

The walk is a cold start, not a startup routine: it runs only until it has
succeeded once (recorded as `meta.bulkIndexedAt` in the project's index), so
restarting a daemon against an already-indexed project skips it entirely. A
walk that was interrupted part way counts as unfinished and is redone on the
next start.

From then on the file watcher keeps the index live, reindexing each file as
it changes. Edits made while **no** daemon is running are the one gap: there
is no rescan on start, so such a file is only picked up the next time it
changes with a daemon up — or by deleting the project's state directory (see
below) to force a fresh full walk.

## Idle behaviour

The daemon is two processes with very different costs, and each has its own
idle timeout:

- The **JS/TS plugin** (the expensive one — tree-sitter plus the TypeScript
  compiler API in a Node process) exits after an hour with no reparse work.
  While it is asleep the core keeps watching the project and remembers which
  files changed; the next query wakes it and replays exactly that list, not
  the whole project. `g-mesh status` reports it as `asleep`, which needs no
  action.
- The **daemon core** (socket, index handle, watcher) is cheap and stays up
  across that, because re-registering fs watchers is the expensive part of a
  start. It exits on its own only after 24 hours with no MCP requests and
  nobody connected, releasing its socket and pid files; the next query
  bootstraps a fresh daemon exactly as a first-ever query does. The index on
  disk is untouched either way, so nothing is reindexed because of it.

Both are configurable per project once the config file lands; today they are
the defaults above.

## Tools exposed

`find_definition`, `find_references`, `find_callers`, `find_callees`,
`find_implementations`, `get_file_outline`, `get_dependencies`.

## State & cleanup

Per-project state lives under `~/.g-mesh/projects/<hash-of-project-root>/`:
SQLite DB, daemon socket, pid files, lock files. Delete a project's directory
there to force a clean reindex.

Upgrading g-mesh does not need that, though: an index records which build of
the indexing pipeline filled it — core's own generation *and* a digest of the
JS/TS plugin's compiled output — and an index that no longer matches is wiped
and re-walked on the next daemon start. A daemon already running when the
upgrade lands is retired first, whether it is the core binary or only the
plugin that was rebuilt, so the next MCP call is answered by what is on disk
now. `g-mesh status` says which build the running daemon came from.

Run `g-mesh status` in a project to see whether its daemon and plugin are up,
how much of the project the index covers, and which files failed to parse.
`g-mesh stop` shuts the daemon core and its plugin down; running it when
nothing is up is a no-op, not an error.

`g-mesh clean` deletes a cached index: the current project's by default, a
named one with `g-mesh clean <project-id>`, everything unused for 90+ days
with `g-mesh clean expired`, or the lot with `g-mesh clean all --force`
(without `--force` it only reports how many it would delete). Stop a
project's daemon before cleaning it.

## Run tests

```bash
cd core && cargo test
cd ../plugins/js-ts && npm run build && npm test
```
