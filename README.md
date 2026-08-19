# g-mesh

Local structural code-graph indexer for AI agents, exposed as an MCP server.
MVP scope: JavaScript/TypeScript only. Resolution is structural (tree-sitter)
first, with a semantic pass over it that drives the project's own `tsserver`
for the questions a name-matching layer cannot answer — today, the members of
an `import * as ns` namespace import. See `REQUIREMENTS.md` and
`docs/architecture/g-mesh-v1.md` for the full design.

## Layout

- `core/` — Rust workspace: the `g-mesh` binary (`mcp-shim` + the per-project
  `daemon`), SQLite storage, graph queries, file watcher, and the MCP tool
  surface.
- `plugins/typescript/` — Node/TypeScript language plugin: tree-sitter parsing,
  bulk indexing, incremental reparse. Spawned by the daemon as a child
  process, one instance per project.

The daemon and shim are one binary (`core/target/{debug,release}/g-mesh`);
the plugin is a separate Node entry point the daemon launches with `node`.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/madmurdok/g-mesh/main/scripts/install.sh | sh
```

`scripts/install.sh` detects the platform, downloads the matching release
archive, **verifies its SHA-256 before unpacking anything** (a mismatch aborts
with both hashes printed and installs nothing), runs the downloaded binary once
to prove it executes and discovers its plugin, and only then installs. It needs
neither Rust nor Node.js: the archive carries the core binary and a plugin with
its own embedded runtime.

What lands on disk is a *directory*, not one file — by default `~/.g-mesh/bin`:

```
~/.g-mesh/bin/
  g-mesh                 the core binary
  plugins/typescript/    the plugin core discovers beside it
  LICENSE*, README.md
```

Those two halves have to stay together: core looks for `plugins/` next to the
running executable, so a lone `g-mesh` copied onto your `PATH` is a binary that
cannot index anything. That is also why the script creates no
`/usr/local/bin/g-mesh` symlink — `std::env::current_exe()` does not resolve
symlinks on macOS, so a symlinked g-mesh would hunt for its plugin beside the
symlink and find nothing. Put the install directory itself on `PATH`; the
script prints the line and edits no rc file of yours:

```bash
export PATH="$HOME/.g-mesh/bin:$PATH"
```

Flags go through the pipe with `sh -s --`, or as environment variables:

```bash
curl -fsSL .../install.sh | sh -s -- --version 2.7.0          # pin a release
curl -fsSL .../install.sh | sh -s -- --install-dir ~/opt/g-mesh
G_MESH_VERSION=2.7.0 G_MESH_INSTALL_DIR=~/opt/g-mesh sh install.sh
```

Uninstalling is `rm -rf ~/.g-mesh/bin` — settings, project indexes and the
embedding model live elsewhere under `~/.g-mesh` and survive it.

**Windows is not supported by this script.** That target ships a `.zip`, which
a POSIX shell has no portable way to unpack, so the script refuses instead of
pretending. Install it by hand: download
`g-mesh-v<version>-x86_64-pc-windows-msvc.zip` from the releases page, unpack
it somewhere permanent — keeping `g-mesh.exe` and `plugins\` beside each other,
for the reason above — and add that directory to `PATH`.

**Until a release is published, this installs nothing.** Releases are built as
drafts and are invisible — and their download URLs 404 — until a human presses
Publish (see "Cutting a release" below). The script says exactly that, with
what to do about it, instead of showing a bare 404 — until then the
build-from-source path below is the only one that works.

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
cd ../plugins/typescript
npm install
npm run build
# -> plugins/typescript/dist/src/index.js
```

Build order doesn't matter, but both are required — the daemon refuses to
start (hard failure) if it can't spawn the plugin.

### 3. Embedding model (optional — only `search_code` needs it)

The seven structural tools work with nothing else installed. `search_code`,
the semantic one, needs a model directory that **you fetch explicitly** —
nothing in g-mesh ever downloads anything on its own, by design (that is
enforced by a test: the HTTP client is reachable from this one command and
from nowhere else in the codebase). Using the binary built in step 1
(`core/target/release/g-mesh`, until it is on your `PATH`):

```bash
g-mesh model fetch
# -> ~/.g-mesh/models/jina-embeddings-v2-base-code/{model.onnx,tokenizer.json}

g-mesh model status   # where the weights are expected, and whether they're there
```

That is `jina-embeddings-v2-base-code` (Apache-2.0), pinned to revision
`516f4baf13dec4ddddda8631e019b5737c8bc250`, and `model.onnx` alone is ~612 MiB
— which is why it is neither vendored nor downloaded behind your back. Each
file is checked against a pinned SHA-256 and only then moved into place, so an
interrupted download leaves a `.partial` you can delete, never a truncated
`model.onnx` that loads as garbage. Pass `--dir`, or set `G_MESH_MODEL_DIR`, to
put it elsewhere; the command and the loader share one resolution function, so
they cannot disagree.

The download is HTTPS from the binary itself — no `curl`, no Python, no
Hugging Face CLI. `core/scripts/fetch-embedding-model.sh` does the same
download as a shell one-liner and is kept for a checkout with nothing built
yet; it skips the checksum.

Skipping this step entirely is a perfectly good choice. Everything else works;
`search_code` just reports that semantic search is unavailable.

A binary built this way finds the plugin through this checkout: the daemon
falls back to a path relative to `core`'s own source tree, baked in at *compile
time* (`core/src/daemon/plugin.rs`):

```
<repo>/plugins/typescript/dist/src/index.js
```

That fallback is the last of three steps. In precedence order, the daemon uses:

1. `G_MESH_JS_TS_PLUGIN_PATH`, if set — a plugin build of your own. A `.js`
   path is run with `node`; anything else is executed directly.
2. `plugins/typescript/` **next to the `g-mesh` binary**, which is what a
   release archive unpacks to and needs no Node.js at all (below).
3. The compile-time checkout path above.

```bash
export G_MESH_JS_TS_PLUGIN_PATH=/path/to/plugins/typescript/dist/src/index.js
```

### Release artifacts

`scripts/build-targets.sh` builds and packages one archive per Rust target
triple — `g-mesh-v<version>-<triple>.tar.gz` (`.zip` on Windows) plus a
`.sha256`, written to `dist/`:

```bash
scripts/build-targets.sh                      # host target
scripts/build-targets.sh x86_64-apple-darwin  # a specific one
scripts/build-targets.sh --list               # the four supported triples
```

`.github/workflows/release.yml` runs that same script across four native
runners (Intel macOS, Apple Silicon, Linux, Windows) from one trigger, so CI
artifact names are reproducible locally. Native runners rather than true
cross-compilation because `rusqlite`(bundled)/`tokenizers`(onig)/`sqlite-vec`
compile C for the target and `ort` links ONNX Runtime statically against a
per-platform C++ stdlib.

Each archive holds a complete install, not just the binary:

```
g-mesh-v<version>-<triple>/
  g-mesh                                  the core binary
  plugins/typescript/
    g-mesh-plugin-typescript              the JS/TS plugin, runtime included
    node_modules/                         its native tree-sitter grammars
    plugin.toml                           how core discovers and spawns it
    LICENSE-nodejs                        the embedded runtime's notice
  LICENSE, LICENSE-MIT, LICENSE-APACHE, README.md
```

**No Node.js required.** The plugin is compiled with [Node's single-executable
application](https://nodejs.org/api/single-executable-applications.html)
support (`scripts/bundle-plugin.sh`), so it embeds its own JS runtime. Indexing
— the whole structural graph — works on a machine with no Node installed.

The one thing the archive does *not* carry is a TypeScript compiler. The
semantic pass that upgrades unresolved edges drives `tsserver`, and it
deliberately prefers **the project's own** `node_modules/typescript` so a
project is analyzed by the compiler it builds with; the plugin's embedded
runtime is what executes it, so this too needs no system Node. A project with
no TypeScript installed at all simply gets no semantic upgrade — every
structural edge is still indexed. To cover that case, drop a `typescript`
package into `node_modules/` beside the plugin executable.

Building an archive requires Node 20+ on the build machine, and each archive
must be built on the platform it targets: a single-executable plugin embeds the
build machine's own Node runtime and cannot be cross-built.

### Cutting a release

1. Merge the release branch (with `core/Cargo.toml` already bumped to the new
   version) into `main`.
2. Run `scripts/cut-release.sh <version>` on `main`. It verifies the crate
   version, working tree and branch state, runs `cargo test`, and creates an
   annotated `v<version>` tag locally — it does not push by default, since
   pushing the tag is what starts the public four-platform build and drafts a
   Release. Pass `--push` to push it in the same step, or run the printed
   `git push origin v<version>` yourself when ready.
3. Once the build finishes, approve the draft on GitHub.

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

`CLAUDE_PROJECT_DIR` is an ordinary environment variable, so it is **inherited
by everything a Claude Code session starts**, however deep — and the shim
cannot tell "my own MCP client set this" from "an MCP server four processes up
did". Anything that runs `g-mesh mcp-shim` from inside such a process (a
wrapper script, a task runner, a test suite) is therefore served the
*session's* project, whatever `cwd` it set. Clear the variable for the child
if you meant `cwd` to decide. On a cold start the shim now says which root it
picked and where the root came from, on stderr, so a redirected shim shows up
in the client's server log rather than as an empty answer.

The shim is a stateless proxy: on first connect for a project it bootstraps
a detached daemon (`g-mesh daemon --project-root <root>`), which opens the
project's SQLite index, spawns the JS/TS plugin, builds the initial index if
the project has never been indexed (see below), starts the file watcher, and
serves the MCP tool surface over a per-project endpoint (an `AF_UNIX` socket on
Linux and macOS, a named pipe on Windows). The daemon outlives the shim and is
reused by later connections for the same project.

## Is this worth registering?

Honest answer, from actual measurement (full numbers in
`../g-mesh-bench/docs/results/v0.2.0-session-economy-findings.md`): on simple
lookup-shaped questions (find a definition, list a file's exports, an
unambiguous `find_implementations`) g-mesh costs *more* tokens than an agent
just using Read/Grep/Glob — about +46% in isolated measurement, and worse
(+67%) inside a long continuing session at the time of that measurement,
because the fixed tool-schema cost is paid on every turn rather than once.
That specific +46%/+67% figure is from v0.2.0 and predates several
schema/response-size trims since (0.8.0's redundant-field cut, 0.13.0's
server-instructions truncation fix) that shrank the same fixed cost this
number is measuring — the gap is very likely narrower now, but hasn't been
remeasured on this exact "simple lookup" task shape; don't treat +46%/+67%
as current without rerunning that experiment. What *is* current (see the
`gmesh-configured` default and its own measurements in `g-mesh-bench`,
including a same-day comparison on a real code-edit task where the
`gmesh-configured` arm came in at roughly half baseline's turns and cost) is
that this fixed-cost problem does not show up the same way on harder task
shapes — see below.

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
  - `find_definition(symbol_name)` or `find_definition(file_path, position)` — resolve a symbol to its definition and get its `symbol_id`. Not required before the tools below — they accept `symbol_name` directly, skip this call when the name is likely unambiguous, and their response's `anchor` field ({id, qualifiedName, kind, filePath, startLine}) already gives the declaration site. Call `find_definition` first only when you expect ambiguity.
  - `find_references(symbol_name or symbol_id)` — every usage of a symbol across the project; use before renaming or removing something.
  - `find_callers(symbol_name or symbol_id)` / `find_callees(...)` — walk the call graph up or down from a function.
  - `find_implementations(symbol_name or symbol_id)` — concrete types implementing an interface/abstract class.
  - `get_dependencies(file_path, direction: Outgoing|Incoming)` — walk the import graph (what a file imports / what imports it); use for impact analysis before changing a shared module.
  - `search_code(query)` — free-text semantic search over doc comments and signatures, ranked by similarity. Default to this as your *first* move on a "find the function/bug that does X" prompt when no symbol name is given — not something to reach for only after Grep has already failed a few times. Measured: on a bug-hunt task with no named symbol, reps that called `search_code` first converged in 8-11 turns; the one rep that skipped it and grep-guessed regex patterns from turn 1 took 15 turns for the same final answer (g-mesh-bench, `ex-implement-mutateelement-elbow-zero-position`). Skip it only for a symbol whose name you already know — `find_definition`/`find_references` are cheaper and exact there. Needs the project's embedding model available; if it errors saying semantic search is unavailable, fall back to grep or the structural tools instead.
  - If a `symbol_name` turns out ambiguous, the result carries `ambiguous: true` with a ranked candidate list — re-query using a candidate's `id` as `symbol_id`, not its `qualifiedName` (the same qualifiedName can name more than one declaration).
- Typical flow: call `find_references`/`find_callers`/`find_callees`/`find_implementations` directly with `symbol_name` when it's likely unique — their `anchor` field already carries the declaration site, so only call `find_definition` first if you expect ambiguity. Use `get_file_outline` first if you don't already know the right symbol name.
- A `find_references`/`find_callers`/`find_callees`/`find_implementations` result is complete for the question it answers when: it was anchored by `symbol_id` or an unambiguous `symbol_name` (same guarantee either way), every row shows `resolved: true`, and the response has no `allUnresolved: true` flag — don't re-verify that with grep/Read. As of g-mesh 0.8.x, `resolved: false` is a narrow, accurate signal (only edges whose target is in another file g-mesh couldn't confirm — same-file edges are always `resolved: true`, matched against declarations actually in scope), not a blanket disclaimer, so still check: a row that shows `resolved: false` (check that row, not the whole list), a response with `allUnresolved: true` (the whole page is unconfirmed), or anything the result doesn't claim to cover at all — e.g. whether other, similarly-named symbols exist elsewhere, or a method call reached through a variable receiver (`x.foo()`, which produces no edge by design). Measured on real g-mesh-bench runs after the 0.8.x same-file-resolution fix: mean cost dropped ~38% and mean turns ~35% on the task this was tested on, with the remaining tool calls answering things g-mesh genuinely doesn't cover rather than re-checking it — but grep/Read still earn their keep on the cases above, so don't suppress those.
- Resolving an ambiguous name (the bullet above on `ambiguous: true` candidates) to a specific `symbol_id` doesn't reopen the completeness question: a `find_references`/`find_callers`/`find_callees`/`find_implementations` page anchored by that `symbol_id` carries the exact same `resolved: true`/no-`allUnresolved` guarantee as an unambiguous `symbol_name` query. Once you've picked the right candidate, treat its result as final — don't grep/Read each returned call site file-by-file to reconfirm it's "really" that symbol and not the same-named other one, and don't run a second, broad text search across the repo to check for anything the query might have missed. Both duplicate work the tool has already resolved, the same way re-verifying a plain unambiguous result would.
- `find_callers`/`find_callees` only ever walk `CALLS` edges, and a `CALLS` edge only exists when the call site sits lexically inside a *named, tracked* function or method. A call written at a file's top level, or inside an anonymous/inline callback that isn't itself extracted as its own symbol (exactly the shape of `it("...", () => { requireTask(...) })` in a test file), gets a `REFERENCES` edge instead — which `find_callers` never sees, even on an otherwise complete, `resolved: true`, `hasMore: false` page. That's not a hole in its own guarantee (it's complete for `CALLS` edges specifically), but it's narrower than "every place this is called" when the prompt implies that — use `find_references` *instead of* `find_callers` whenever the task needs an exhaustive caller list (before a rename/removal, or anything that should include test files). Instead of, not as well as: for the same anchor `find_references` returns a strict superset of `find_callers`' rows (every `CALLS` edge, plus the `REFERENCES`/`SUPERTYPE_OF` ones), and each row's own `referenceKind` separates them inside that one page — asking both tools is two round-trips for one answer. A usage that sits outside any tracked symbol comes back as a whole-file row — `kind: File`, with no `qualifiedName`/`startLine`/`startCol` — because the graph has no smaller unit to point at there, not because the position went missing from an otherwise complete row. When the task asks which *files* are affected (a rename, an impact list), that row is already the answer at the granularity it claims: take it and move on, rather than grepping the file for the exact lines it deliberately doesn't carry.
- When the question is about which *files* are affected — a rename, a signature change, "list every file that calls X" — read the response's `files` array and answer from it. `find_references`/`find_callers` attach `files` exactly when the rows don't already answer at that granularity (the page is incomplete, or several rows share one file), and unlike `results` it is computed over the *whole* edge set rather than the page: on excalidraw's `pointFrom`, a `limit: 200` call returns 51 rows spanning 46 files and still says `hasMore: true`, while the same response's `files` lists all 81 referencing files with a per-file count in a quarter of the bytes. So it is both the cheaper answer and the *more complete* one — deduplicating the rows by hand produces a shorter file list than the tally already holds, and paging the cursor to repair that spends round-trips on something already in hand. Use `results` when you need the calling symbol or its line; use `files` for "what do I have to touch". When `files` is absent the page is complete and its rows already sit one per file, so there is nothing to deduplicate — the `filePath` column is the list.
- A `get_dependencies` result's completeness is signaled by `truncated`/`truncatedBy`, not a per-row `resolved` flag — there isn't one; a multi-hop path can't be summarized by one boolean the way a single edge can. `truncated: false` means the walk reached everything within its depth/fanout bounds — trust it fully, don't re-verify with grep. `truncated: true` needs a follow-up keyed off `truncatedBy`, not a blanket re-query: on `maxDepth`, re-call anchored on the returned `frontierNodes` to go further; on `maxFanout`, that one node had more imports/importers than the fanout cap, so re-query just that node with the single-hop tools' own pagination; on `explorationBudget`/`responseSize`, call again with the returned `resumeToken`. The default `max_depth` is only 2 (shallower than a single-hop tool's own completeness bar), so check `truncated` before treating one result as the whole *transitive* tree — but a depth bound limits only how far the walk goes, never how completely it walked the levels it did reach: `truncated: false` with an empty `frontierNodes` is the entire answer for the depth you asked for, and at `max_depth: 1` that is exactly the complete set of direct importers (`Incoming`) or direct imports (`Outgoing`).
- Which imports produce those rows is the other half of trusting one. A row is a *file*, not an import statement, and its edge comes from a parsed module specifier: `import ... from`, type-only `import type ...`, `export ... from`, and `import()`/`require()` whose specifier is a static string or folds to one. Type-only imports sit in the graph exactly like value imports, so an `Incoming` walk already answers "every file that imports this, both kinds" — measured on g-mesh-bench's `tt-deps-incoming-db-connection`, one `Incoming`, `max_depth: 1` call on `src/db/connection.ts` returned all 21 importing `src/` files (18 of them `import type`-only), exactly the task's ground-truth set, and the follow-up greps three separate runs ran to check it found nothing it had missed. So don't re-derive that list with a `from ["'].*<module path>` grep: it is the most expensive habit on this tool, a whole extra round-trip that reproduces an answer already in hand. What a row genuinely doesn't carry is which names the importing file binds, whether that particular import was type-only, and on what line — `IMPORTS` edges have no position in the schema. When the task needs that for some file, Read that one file; don't grep the tree for all of them. The only importer that can be missing is one whose specifier no static fold can compute (built from a runtime value, `process.env`, or another file's constant).
- `search_code` is similarity-ranked, not a resolved graph query — its top hit isn't automatically "the answer" the way a `find_definition` hit is. But once a hit's `qualifiedName`/`kind`/`filePath` plausibly match what the prompt describes, one targeted confirming read (the exact lines, or `get_file_outline`) is enough — check the doc comment/signature there, then stop. Don't keep re-issuing `search_code` with reworded queries hunting for a "better" match, and don't follow a confirmed hit with a broad grep sweep across the repo "just in case" — that's the same wasted re-verification the bullet above warns against for the structural tools, just dressed up as more searching instead of more reading.
- `find_implementations` only returns direct implementors/extenders by default — a class extending a class that implements the anchor interface won't show up in a `hasMore: false` page. For the whole hierarchy, re-call with `transitive: true` (walks the same edges transitively, up to a bounded depth, resumable via `resume_token`).
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

- The **JS/TS plugin** (the expensive one — tree-sitter parsing in a Node
  process; `typescript` is a runtime dependency, since `tsserver` ships inside
  that package, but it is never loaded in this process — the checker runs as a
  child, spawned only by the first semantic question actually asked and killed
  with the plugin) exits after an hour with no reparse work.
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

Both are configurable via `[plugin] idleTimeoutMinutes` (per-project
`config.toml`) and `[daemon] coreIdleTimeoutHours` (global `~/.g-mesh/config.toml`)
— see `g-mesh config` / `g-mesh config --global`. The values above are the
defaults when a project or machine has never run either.

## Tools exposed

`find_definition`, `find_references`, `find_callers`, `find_callees`,
`find_implementations`, `get_file_outline`, `get_dependencies` — all
structural, all available with no extra setup.

Plus `search_code`: free-text semantic search over doc comments and
signatures, ranked by similarity. It is the one tool with a prerequisite —
the embedding model above — and the one whose top hit is a ranked guess
rather than a resolved graph answer.

## Known limits

**Computed `import()`/`require()` specifiers.** `import(\`./plugins/${name}/index.js\`)`
resolves when every interpolated part is a same-file constant (a `const`
bound to a string literal, a named string enum member, a literal ternary, or
`path.join`/`path.resolve(__dirname, ...)`) — that is arithmetic over one
file's own syntax, no different in kind from resolving a plain relative
`import "./x"`. `import(getSpecifier(id))` and anything else whose value only
exists at runtime (an argument, `process.env`, an arbitrary function call)
does not resolve and never will — no edge is recorded for it, not a wrong or
partial one. This is a permanent limit, not a gap scheduled to close: a
static analysis pass cannot evaluate an expression that only has a value once
the program is actually running. Full scope and reasoning in
`docs/architecture/g-mesh-v1.md` ("Computed import specifiers").

## State & cleanup

Per-project state lives under `~/.g-mesh/projects/<hash-of-project-root>/`:
SQLite DB, daemon socket, pid files, lock files. Delete a project's directory
there to force a clean reindex.

`G_MESH_HOME` moves that root: set it and `projects/` and the global
`config.toml` are read and written under it instead of `~/.g-mesh`. It is what
this repository's own test suite uses to stop writing into the developer's
state (see "Run tests"), and it makes a sandboxed or throwaway g-mesh possible
without touching your real one. It does *not* move the embedding model —
that is a per-machine cache with its own `G_MESH_MODEL_DIR`, and moving it
would mean re-downloading 612 MiB — nor `~/.g-mesh/bin`, which belongs to the
installer, not to the binary.

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
cd ../plugins/typescript && npm run build && npm test
```

The integration tests spawn real shims, real daemons and a real plugin against
temp-directory fixtures, so they care about the environment they run in:

- **`CLAUDE_PROJECT_DIR` must not leak into the suite.** Every shim spawn in
  `core/tests` clears it (a test enforces that for the whole directory), so
  the suite is safe to launch from a CI runner, a release script, or an MCP
  server that shells out — all of which inherit it from a Claude Code session
  and would otherwise point every fixture's shim at the session's own
  project. Before that fix the symptom was a hang, not an error: the fixture's
  daemon was never started, so its walk never completed and the run failed in
  whichever file happened to reach a shim first with "the cold-start bulk walk
  for /var/folders/…/.tmpXXXX did not finish within 90s".
- Nothing else about the launching process matters: piped stdio, a
  long-running parent, and a one-shot script all behave identically (measured,
  task 192).
- **State is redirected away from your `~/.g-mesh`.** `.cargo/config.toml` at
  the repo root sets `G_MESH_HOME` to `/tmp/g-mesh-test-home` for everything
  cargo launches, so the fixtures' project directories land there and a run is
  safe next to a real daemon serving a real project. (Not `target/`: a daemon
  socket inside a checkout overruns the 104-byte AF_UNIX path limit. `rm -rf
  /tmp/g-mesh-test-home` to clear it; `cargo clean` cannot.)
  Without it the suite left ~350 directories a day in the real home and could
  fail against state something else on the machine was mutating;
  `core/tests/state_isolation.rs` fails loudly if the override stops reaching
  either the test binary or the `g-mesh` processes it spawns. Note that cargo
  applies `[env]` to `cargo run` too — `G_MESH_HOME="$HOME/.g-mesh" cargo run`
  if you want a run against your own state.
- `G_MESH_TEST_INDEXED_TIMEOUT_SECS` raises the per-project indexing budget
  (default 90s) on a machine slow enough to need it. If it does not help, the
  walk is not slow — something is wrong.
- `G_MESH_DAEMON_LOG=<file>` makes a shim-bootstrapped daemon append its
  stderr (and its plugins') to that file instead of discarding it. Detached
  daemons have no console, so this is the only way to see what one is doing
  during a test run; unset, nothing changes.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option — the Rust ecosystem's usual dual license. Take MIT if you want
the shorter terms, Apache-2.0 if you want its explicit patent grant. Unless you
state otherwise, any contribution you intentionally submit for inclusion in
this work shall be dual licensed as above, with no additional terms.

Everything g-mesh depends on is permissively licensed (MIT, Apache-2.0,
CC0/Artistic-2.0, and public-domain SQLite via `rusqlite`'s bundled build);
nothing here is copyleft.

**The embedding model is not part of this repository.** `search_code` needs a
model directory that you fetch yourself (see "Embedding model" above —
`model.onnx` alone is ~610 MiB, which is why it is not vendored). The default
model, `jina-embeddings-v2-base-code`, is Apache-2.0, so redistributing it
inside a machine image or container is permitted under its own terms; if you
point g-mesh at a different model, check that model's license yourself.
