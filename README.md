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

`g-mesh mcp-shim` takes no arguments — it uses its **current working
directory** as the project identity (hashed to derive the daemon's socket,
lock and SQLite paths under `~/.g-mesh/projects/<hash>/`). Whatever spawns
the shim must set its `cwd` to the target project's root.

The shim is a stateless proxy: on first connect for a project it bootstraps
a detached daemon (`g-mesh daemon --project-root <root>`), which opens the
project's SQLite index, spawns the JS/TS plugin, builds the initial index if
the project has never been indexed (see below), starts the file watcher, and
serves the MCP tool surface over an `AF_UNIX` socket. The daemon outlives the
shim and is reused by later connections for the same project.

## Register with an MCP client

Run from inside the target project's directory (so the client's own `cwd` —
and therefore the shim's — is the project root), e.g. with Claude Code:

```bash
cd /path/to/target-project
claude mcp add g-mesh -- /path/to/g-mesh/core/target/release/g-mesh mcp-shim
```

Any MCP client that speaks stdio + newline-delimited JSON works the same
way — the shim doesn't do anything Claude Code-specific.

## First run: the initial index

The first time a daemon starts for a project it walks the whole tree once
(gitignore-aware, skipping `.git`, `node_modules` and `dist`), parses every
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

## Tools exposed

`find_definition`, `find_references`, `find_callers`, `find_callees`,
`find_implementations`, `get_file_outline`, `get_dependencies`.

## State & cleanup

Per-project state lives under `~/.g-mesh/projects/<hash-of-project-root>/`:
SQLite DB, daemon socket, pid file, lock files. Delete a project's directory
there to force a clean reindex (schema mismatches also auto-wipe and
reindex). To stop a running daemon, kill its pid from `daemon.pid` in that
directory — there's no `g-mesh stop` command yet.

## Run tests

```bash
cd core && cargo test
cd ../plugins/js-ts && npm run build && npm test
```
