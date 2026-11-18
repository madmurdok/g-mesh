# g-mesh 4.30 module map (factual)

Source: worktree `g-mesh-wt-gm704`, code identical to main 97fa0f87 except the version bump. Counts are mechanical (see Method). No evaluation is made here.

Conventions: "lines" = `wc -l`-style newline count. "test files" = files under a `tests/` or `testdata/` directory, or named `tests.rs`, `*_tests.rs`, `*_test.rs`. "inline test" = lines inside `#[cfg(test)] mod x { ... }` blocks in non-test files (brace-balanced, approximate). "pub" = lines starting a `pub` item (`fn struct enum trait type const static mod union use macro_rules!`) with plain `pub`; "pub(r)" = `pub(crate)`/`pub(super)`/`pub(in ..)` items. pub counts cover non-test files only and include associated `pub fn`s inside `impl` blocks and `pub use` re-exports.

## 1. Workspace crates

Workspace root `Cargo.toml` (virtual manifest) members: `core`, `plugins/python`, `plugins/rust`, `plugins/sdk`, `plugins/typescript`, `walk`, `wire`. The Go plugin (`plugins/go`) is a Go module, not a cargo member.

| crate | path | version | .rs files | Rust lines total | of which test files | inline test (approx, in non-test files) | path deps (kind) |
|---|---|---|---|---|---|---|---|
| g-mesh | core | 4.30.0 | 273 | 166513 | 71826 | 31634 | g-mesh-walk (normal), g-mesh-wire (normal) |
| g-mesh-wire | wire | 4.30.0 | 1 | 1962 | 0 | 1011 | - |
| g-mesh-walk | walk | 4.30.0 | 1 | 579 | 0 | 0 | - |
| g-mesh-plugin-sdk | plugins/sdk | 4.30.0 | 37 | 24230 | 6442 | 5623 | g-mesh-walk (normal), g-mesh-wire (normal) |
| g-mesh-plugin-python | plugins/python | 4.30.0 | 21 | 12420 | 3990 | 2009 | g-mesh-plugin-sdk (normal), g-mesh-wire (normal) |
| g-mesh-plugin-rust | plugins/rust | 4.30.0 | 51 | 23828 | 8675 | 4432 | g-mesh-plugin-sdk (normal), g-mesh-wire (normal) |
| g-mesh-plugin-typescript | plugins/typescript | 4.30.0 | 40 | 16354 | 9147 | 126 | g-mesh-plugin-sdk (normal), g-mesh-wire (normal) |

Core depends on `g-mesh-wire` with feature `json-schema` and on `g-mesh-walk`. Core targets: `[lib] g_mesh` (`core/src/lib.rs`) and `[[bin]] g-mesh` (`core/src/main.rs`); `core/build.rs` and `core/go_plugin_build_flags.rs` also exist. Core Rust lines split: `core/src` = 130254, rest of `core/` (`tests/`, build.rs, etc.) = 36259.

## 2. Core modules by area (`core/src`)

Top-level table per area, then a child table per module that is a directory. Columns: files, lines (total / of which test files / inline test approx), pub, pub(r). A module that is a file plus a same-named directory (e.g. `shim.rs` + `shim/`) counts both. "dir" = the module has a subdirectory or is `mod.rs`-style.

### Area A. Runtime

| module | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose (from doc comment / main items) |
|---|---|---|---|---|---|---|---|---|
| daemon | dir | 29 | 25240 | 7856 | 4879 | 264 | 134 | The per-project daemon: its state files, its singleton lock and [`run`]. Decisions: `docs/adr/0004-daemon-lifecycle.md`. |
| ipc | dir | 3 | 864 | 0 | 175 | 31 | 0 | The shim<->daemon channel, named once and implemented per platform. |
| protocol | dir | 7 | 1295 | 0 | 652 | 27 | 6 | no module doc; declares submodules conformance, handshake, jsonrpc, ndjson, ndjson_frame, types |
| shim | dir | 2 | 3100 | 0 | 1214 | 4 | 10 | no module doc (plain `//` comment: shim is its own process, stderr to MCP client); `pub fn run`; submodule `router` |
| watcher | dir | 11 | 7812 | 3348 | 1741 | 67 | 10 | no module doc; declares submodules apply, batch, burst, debounce, ignore_layers, rescan, staleness |
| gc | dir | 3 | 511 | 0 | 218 | 8 | 0 | Garbage-collection support for `~/.g-mesh/projects/`: the bookkeeping that lets a later scan tell which project indexes are still in use from which have been idle for mo... |
| process | file | 1 | 560 | 0 | 0 | 37 | 0 | The things this project does to processes that POSIX spells with signals, named once and implemented per platform: is it alive, ask it to stop, make it stop, start one d... |

`daemon/` children (`core/src/daemon/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| activation | dir | 2 | 828 | 430 | 0 | 0 | 2 | Lazy activation (D2 and D8 in `docs/architecture/lazy-indexing.md`): everything `daemon::run` used to do eagerly after binding its socket - register the watcher, walk, r... |
| build_stamp | file | 1 | 411 | 0 | 173 | 7 | 0 | Which build a running daemon came from, published next to its socket so anything else on the machine can tell whether that daemon is still the current one. |
| bulk_index | file | 1 | 1154 | 0 | 609 | 7 | 6 | Cold-start bulk index: the one full walk of a project that gives a never-indexed codebase a populated graph before its daemon answers anything. The file watcher cannot d... |
| candidates | file | 1 | 979 | 0 | 503 | 13 | 3 | Multi-project detection (D10 in `docs/architecture/lazy-indexing.md`, GM-399). |
| client | file | 1 | 102 | 0 | 0 | 2 | 0 | A minimal synchronous MCP client over one daemon connection, for CLI commands that need the daemon to do what a session's first tool call does: start the walk and wait u... |
| config_reindex | dir | 2 | 1813 | 1193 | 16 | 0 | 11 | What a settled edit to a watch file does for a plugin whose manifest declares `capabilities.resolution_delta`: the plugin says what the edit changed for resolution, and... |
| front | file | 1 | 139 | 0 | 0 | 3 | 1 | Front mode (D11 step 1 in `docs/architecture/lazy-indexing.md`, GM-399). |
| identity | file | 1 | 201 | 0 | 101 | 6 | 0 | no module doc; `project_hash`, `record_project_root`, `read_root_device`, `read_project_root` |
| indexing_status | file | 1 | 1020 | 0 | 344 | 41 | 3 | Whether the daemon's index is ready to answer, and how much of "ready" a given tool call needs. Design: `docs/architecture/lazy-indexing.md` (D2 activation, D3 phases, D... |
| lifecycle | dir | 2 | 2152 | 1086 | 0 | 40 | 4 | The daemon's two idle timers, and the state each one owns. Decisions: `docs/adr/0004-daemon-lifecycle.md`. |
| manifest | dir | 2 | 2152 | 1231 | 0 | 27 | 8 | Reads and validates one plugin's `plugin.toml` manifest, and discovers the plugins under the plugin roots. Schema: `docs/architecture/plugin-modularity.md` and `docs/arc... |
| memory | file | 1 | 578 | 0 | 341 | 8 | 0 | Resident-memory sampling for one plugin's whole process tree - the `[plugin] memoryLimitMb` enforcement task GM-274 adds (`docs/architecture/multi-language-plugins.md`'s... |
| mod | file | 1 | 1038 | 0 | 0 | 44 | 12 | The per-project daemon: its state files, its singleton lock and [`run`]. Decisions: `docs/adr/0004-daemon-lifecycle.md`. |
| path_case | dir | 2 | 176 | 66 | 0 | 0 | 3 | Keys for project-relative paths that must match a saved file's path as the watcher reports it, whatever case a plugin spelled them in. |
| plugin | dir | 2 | 2211 | 581 | 0 | 27 | 5 | Spawns a language plugin as a child process from its manifest, performs its handshake, and gives the rest of the daemon a way to route `FileChanged` requests to it and a... |
| registry | dir | 2 | 3724 | 1688 | 0 | 29 | 24 | One [`PluginSupervisor`] per language, spawned the first time a file of that language is actually touched. |
| seed | dir | 2 | 703 | 340 | 0 | 4 | 6 | Seeding a linked git worktree's index from its main working tree's index (GM-564, `docs/design/GM-564-seed-worktree-index.md`). |
| semantic | file | 1 | 1406 | 0 | 790 | 6 | 4 | The whole-project semantic pass, and the one rule about when it runs: a freshly built index is not finished until every semantic-pass-capable language in it has had one. |
| test_plugin | file | 1 | 723 | 0 | 0 | 0 | 35 | A stand-in language plugin, for unit tests that need a *second* language to exist. |
| tests | file | 1 | 1241 | 1241 | 0 | 0 | 0 | test file (daemon unit tests) |
| workspace_reindex | file | 1 | 2489 | 0 | 2002 | 0 | 7 | The per-language reindex a settled edit to one of a manifest's `[plugin.workspace] watch_files` triggers (`go.mod`, `Cargo.toml`, `*.csproj`...). `daemon::registry::Plug... |

`ipc/` children (`core/src/ipc/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| mod | file | 1 | 183 | 0 | 129 | 2 | 0 | The shim<->daemon channel, named once and implemented per platform. |
| unix | file | 1 | 222 | 0 | 46 | 15 | 0 | The AF_UNIX half of [`crate::ipc`] - the transport this project has always used, renamed rather than reimplemented. |
| windows | file | 1 | 459 | 0 | 0 | 14 | 0 | The named-pipe half of [`crate::ipc`]. |

`protocol/` children (`core/src/protocol/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| conformance | file | 1 | 349 | 0 | 144 | 5 | 6 | no module doc; `Violation` (wire-conformance checks over NDJSON/JSON-RPC frames) |
| handshake | file | 1 | 91 | 0 | 58 | 2 | 0 | no module doc; `verify`, `perform` (protocol-version handshake) |
| jsonrpc | file | 1 | 401 | 0 | 207 | 7 | 0 | no module doc; `write_frame`, `write_message`, `read_message` (Content-Length framing) |
| mod | file | 1 | 6 | 0 | 0 | 6 | 0 | declares submodules |
| ndjson | file | 1 | 236 | 0 | 138 | 4 | 0 | no module doc; `BulkItem`, `NdjsonReader` |
| ndjson_frame | file | 1 | 194 | 0 | 105 | 2 | 0 | Newline-delimited framing for the MCP wire, kept deliberately separate from `jsonrpc`'s Content-Length framing. |
| types | file | 1 | 18 | 0 | 0 | 1 | 0 | The plugin wire protocol, re-exported. |

`shim/` children (`core/src/shim/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| router | file | 1 | 2358 | 0 | 1135 | 0 | 7 | The shim's switchable router (D11 step 3 in `docs/architecture/lazy-indexing.md`). |

`watcher/` children (`core/src/watcher/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| apply | dir | 3 | 4661 | 3242 | 0 | 8 | 6 | Bridges a (debounced) file-change event to a committed diff: sends a `FileChanged` control-plane request to a language plugin, reads back its diff, and applies it throug... |
| batch | dir | 2 | 195 | 106 | 0 | 5 | 0 | Routing order inside one drained debounce batch. |
| burst | file | 1 | 244 | 0 | 146 | 5 | 0 | no module doc; `BurstBatcher` |
| debounce | file | 1 | 142 | 0 | 77 | 4 | 0 | no module doc; `Debouncer` |
| ignore_layers | file | 1 | 134 | 0 | 0 | 6 | 0 | The watcher's `.gitignore` matcher, layered per directory the way the walks that populate the index are (`project_walk::project_files`, the SDK's `walker`): every direct... |
| mod | file | 1 | 1093 | 0 | 753 | 19 | 1 | declares submodules |
| rescan | file | 1 | 301 | 0 | 161 | 13 | 0 | Coalesces the watcher's dropped-event signals (GM-616) into few reconciles: see [`RescanScheduler`]. |
| staleness | file | 1 | 1042 | 0 | 604 | 7 | 3 | Query-time staleness check with synchronous fallback reindex. |

`gc/` children (`core/src/gc/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| last_used | file | 1 | 252 | 0 | 116 | 4 | 0 | Keeps `meta.lastUsed` current, and reads it back without a daemon. |
| mod | file | 1 | 10 | 0 | 0 | 2 | 0 | Garbage-collection support for `~/.g-mesh/projects/`: the bookkeeping that lets a later scan tell which project indexes are still in use from which have been idle for mo... |
| warning | file | 1 | 249 | 0 | 102 | 2 | 0 | The GC warning: a printed, never-acted-on heads-up that some projects under `~/.g-mesh/projects/` have gone idle past `cleanup.idleThresholdDays`. |

### Area B. Data and indexing

| module | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose (from doc comment / main items) |
|---|---|---|---|---|---|---|---|---|
| graph | dir | 10 | 14594 | 5882 | 2915 | 100 | 7 | no module doc; declares submodules containers, imports, pagination, queries, resume_token, symbol_links, traversal |
| storage | dir | 10 | 9364 | 1664 | 2906 | 133 | 13 | no module doc; declares submodules connection, file_rows, index_store, language_swap, qualified_path, schema, vectors, write |
| embedding | dir | 8 | 8507 | 0 | 4206 | 90 | 102 | Local, CPU-only text embedding. |
| languages | dir | 2 | 683 | 420 | 0 | 11 | 0 | The language catalogue: what core knows about a language *without* its plugin. |
| project_walk | file | 1 | 650 | 0 | 0 | 13 | 2 | Core's own walk of a project tree: the one place that says which tree "the project" is when core, not a plugin, has to look at it. |
| paths | file | 1 | 96 | 0 | 35 | 2 | 0 | Where g-mesh keeps what it owns on the machine it runs on. |

`graph/` children (`core/src/graph/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| containers | dir | 2 | 1971 | 1164 | 0 | 10 | 3 | Logical containers: the core-owned nodes a Go package, a Rust module, a C# namespace... becomes in the graph (docs/architecture/multi-language- plugins.md, "Data Model >... |
| imports | file | 1 | 1156 | 0 | 731 | 5 | 0 | Import linking: the pass that turns a plugin's *resolved* module placeholder into a real edge onto whatever it names - a `File` node, or, since GM-267, a core-owned logi... |
| mod | file | 1 | 7 | 0 | 0 | 7 | 0 | declares submodules |
| pagination | file | 1 | 2125 | 0 | 1026 | 36 | 1 | no module doc; `resolve_page_size`, `EdgeRow`, `longest_prefix_fitting`, `paginate_edges` |
| queries | file | 1 | 1298 | 0 | 550 | 17 | 3 | no module doc; `declaration_only`, `map_node_row` and query helpers over the nodes/edges tables |
| resume_token | file | 1 | 134 | 0 | 39 | 4 | 0 | no module doc; `ResumeState`, `VisitedNode` (gzip+base64 token) |
| symbol_links | dir | 2 | 6891 | 4718 | 0 | 11 | 0 | Cross-file symbol linking: the pass that turns a plugin's *pending symbol* placeholder into a real `CALLS`/`REFERENCES`/`SUPERTYPE_OF` edge onto the symbol another file... |
| traversal | file | 1 | 1012 | 0 | 569 | 10 | 0 | no module doc; `TruncatedBy` and transitive edge traversal |

`storage/` children (`core/src/storage/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| connection | file | 1 | 209 | 0 | 57 | 5 | 3 | no module doc; `projects_root` and SQLite connection opening |
| file_rows | file | 1 | 169 | 0 | 0 | 1 | 5 | The rows one file owns, and how a `fileChanged` diff is widened to retire the ones the plugin no longer stands behind but could not name. |
| index_store | file | 1 | 1629 | 0 | 1022 | 31 | 1 | The one owner of the index's SQLite connection and of how long each write holds it. Design: [ADR 0001](../../../docs/adr/0001-index-store.md). |
| language_swap | file | 1 | 1530 | 0 | 795 | 6 | 1 | The plan and the swap of a per-language reindex walked into a staging index: [`plan`] compares the staging file with the live one and records only the difference in stag... |
| mod | file | 1 | 8 | 0 | 0 | 8 | 0 | declares submodules |
| qualified_path | file | 1 | 103 | 0 | 43 | 3 | 0 | A plugin's [`QualifiedPath`] in storage: the column encoding of `nodes.qualifiedPath`/`placeholder_targets.keyPath`, and the `qualified_suffixes` rows derived from a dec... |
| schema | dir | 2 | 3913 | 1664 | 0 | 64 | 1 | no module doc; `apply`, `ensure_current`, `stored_generation`, `reset` (DDL + schema generation) |
| vectors | file | 1 | 221 | 0 | 100 | 5 | 1 | sqlite-vec extension loading and the `vectors` table (see `storage::schema` for the DDL and its doc comment for why the table is 1:0-or-1 with `nodes`). |
| write | file | 1 | 1582 | 0 | 889 | 10 | 1 | no module doc; `DeclarationRecord`, `PlaceholderTargetRecord`, `NodeRecord`, `EdgeRecord`, `Diff`, `apply_diff` |

`embedding/` children (`core/src/embedding/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| backfill | file | 1 | 1112 | 0 | 636 | 5 | 2 | The embedding backfill pass: fills in `vectors` rows for every node the graph is missing one for, as its own bounded scan rather than as part of the walk that wrote thos... |
| cache | file | 1 | 950 | 0 | 444 | 5 | 18 | The machine-wide embedding cache: one SQLite file under `$G_MESH_HOME/embedding-cache/`, shared by every daemon and CLI process on the machine, mapping `sha256(text)` un... |
| ce_text | file | 1 | 394 | 0 | 132 | 0 | 12 | The text the cross-encoder reads for one node: |
| mod | file | 1 | 29 | 0 | 0 | 8 | 2 | Local, CPU-only text embedding. |
| model | file | 1 | 1907 | 0 | 808 | 39 | 10 | The ONNX embedding model itself: load it once, then embed text with it. |
| pipeline | file | 1 | 2143 | 0 | 1225 | 16 | 22 | Turns a written node into a stored vector - the piece that sits between [`storage::write::apply_diff`] (what actually lands a node's row) and [`storage::vectors::insert`... |
| rerank | file | 1 | 1150 | 0 | 552 | 17 | 20 | The cross-encoder that reorders `search_code`'s top rows. |
| text | file | 1 | 822 | 0 | 409 | 0 | 16 | The text a symbol is embedded as: its doc comment trimmed to its prose outline (the `structured` form), then its signature. Why this form and not the whole doc comment i... |

`languages/` children (`core/src/languages/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| tests | file | 1 | 420 | 420 | 0 | 0 | 0 | test file |

### Area C. Surfaces

| module | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose (from doc comment / main items) |
|---|---|---|---|---|---|---|---|---|
| mcp | dir | 39 | 28186 | 13239 | 5495 | 39 | 148 | The MCP server on the daemon's socket: the structural tool surface, and one `rmcp` session per accepted connection. Async stops here: handlers take the same synchronous... |
| cli | dir | 37 | 28139 | 3371 | 6950 | 281 | 85 | The `g-mesh` command-line surface: every subcommand the binary exposes, declared once here with clap's derive API, plus the dispatch that hands each one to the module th... |
| config | dir | 1 | 463 | 0 | 173 | 14 | 0 | Per-project and global TOML configuration. |
| log | file | 1 | 162 | 0 | 75 | 7 | 0 | Line-atomic stderr logging for daemon-process code. |
| main | file | 1 | 11 | 0 | 0 | 0 | 0 | Thin entry point: the whole command surface lives in `cli`, where it can be unit-tested without taking over a real process's argv. |
| lib | file | 1 | 17 | 0 | 0 | 17 | 0 | declares 17 `pub mod` (the module list below) |

`mcp/` children (`core/src/mcp/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| anchor | file | 1 | 302 | 0 | 138 | 0 | 6 | Which node does a symbol query hang off? Answered once, here, for the four tools that all hang off exactly one node and differ only in which edges they walk afterwards:... |
| answer | file | 1 | 135 | 0 | 0 | 0 | 4 | The non-row answers of `find_references`, `find_callers` and `find_callees` (`answer: "files"` and `answer: "count"`): the same whole-set predicate the row walk pages th... |
| answer_tests | file | 1 | 400 | 400 | 0 | 0 | 0 | `answer: rows/files/count` and `total` on `find_references`, `find_callers` and `find_callees`, through their real handlers. |
| find_callers_callees | file | 1 | 2416 | 0 | 1565 | 0 | 6 | Real logic behind the `find_callers`/`find_callees` MCP tools. Same shape as `find_references` - anchor lookup, then `paginate_edges` over one edge kind - except the edg... |
| find_definition | dir | 2 | 3555 | 2286 | 0 | 0 | 11 | Real logic behind the `find_definition` MCP tool. Kept out of `mcp/mod.rs` so that file stays pure tool-router wiring - this is where the actual "name or position -> nod... |
| find_implementations | dir | 2 | 1739 | 1090 | 0 | 0 | 4 | Real logic behind the `find_implementations` MCP tool. Same shape as `find_references`: anchor lookup, then a single-direction `paginate_edges` walk over one edge kind.... |
| find_references | file | 1 | 2083 | 0 | 1634 | 0 | 3 | Real logic behind the `find_references` MCP tool. The hard part - cursor pagination with the structural ordering rule (`EdgeRank`) - already lives in `graph::pagination:... |
| front | file | 1 | 392 | 0 | 71 | 8 | 1 | What a front daemon serves (D11 step 2 in `docs/architecture/lazy-indexing.md`, GM-399): the usual eight tools' schemas plus `select_project`, instructions that list the... |
| get_dependencies | dir | 2 | 2316 | 1474 | 0 | 0 | 2 | Real logic behind the `get_dependencies` MCP tool - the one MVP tool whose answer is genuinely transitive rather than a single hop. The walk itself, and the whole trunca... |
| get_file_outline | file | 1 | 723 | 0 | 519 | 0 | 2 | Real logic behind the `get_file_outline` MCP tool. Same shape as `find_references`/`find_implementations` - anchor lookup, then a paginated edge walk - except the anchor... |
| instructions | dir | 2 | 2938 | 2294 | 0 | 11 | 2 | Assembles `get_info`'s `with_instructions` string from this project's language coverage (design and measurements: `docs/adr/0003-mcp-instructions-rendering.md`, amended... |
| member_name_collision_tests | file | 1 | 608 | 608 | 0 | 0 | 0 | A type member and a free declaration of the same name in one module, walked by each real plugin and read back through `find_references`. |
| mod | file | 1 | 1121 | 0 | 0 | 20 | 10 | The MCP server on the daemon's socket: the structural tool surface, and one `rmcp` session per accepted connection. Async stops here: handlers take the same synchronous... |
| not_indexed | file | 1 | 496 | 0 | 279 | 0 | 19 | Why a path-anchored answer is empty when the path's language is not indexed at all. Design: `docs/architecture/gm-503-absent-language-field.md`. |
| overrides | file | 1 | 246 | 0 | 0 | 0 | 4 | The base members a method overrides or implements, disclosed on its caller page. Design: `docs/architecture/gm-502-override-callers-field.md`. |
| overrides_tests | file | 1 | 357 | 357 | 0 | 0 | 0 | `overrides`: the probe against a hand-built index, per mode. The handler cases (capability gating, both wire paths, the once-per-session hint) sit in `find_callers_calle... |
| provenance | file | 1 | 720 | 0 | 318 | 0 | 8 | Per-response tier provenance (GM-382): which language plugin answered a query, and - when that plugin's semantic tier did not contribute - the fact that it did not. |
| query_shapes | dir | 2 | 280 | 148 | 0 | 0 | 10 | Which query shapes each discovered language declares are never its symbols (`[plugin.non_symbol_queries]`, `daemon::manifest::NonSymbolShapes`), and which prefixes, stri... |
| response_bound_tests | file | 1 | 527 | 527 | 0 | 0 | 0 | The whole-response byte ceiling (`MAX_RESPONSE_BYTES`) on the edge tools that page rows (`find_callers`, `find_callees`, `find_references`, single-hop `find_implementati... |
| search_code | file | 1 | 1145 | 0 | 441 | 0 | 10 | Real logic behind the `search_code` MCP tool: embed the caller's free-text query with the project's [`EmbeddingPipeline`], then rank stored node vectors against it by co... |
| search_code_rerank_tests | file | 1 | 1243 | 1243 | 0 | 0 | 0 | `search_code`'s cross-encoder rerank through the real handler, with a stub scorer standing in for the model (`embedding::rerank`). |
| search_code_wait_tests | file | 1 | 415 | 415 | 0 | 0 | 0 | `search_code`'s bounded wait for the embedding pass, through the real `GMeshMcpServer` served in-process to a real rmcp client: the structural wait, the embedding wait a... |
| search_code_worker_tests | file | 1 | 175 | 175 | 0 | 0 | 0 | `search_code`'s query inference runs off the async workers: while one call's inference is blocked, another session's call on the same daemon is still answered. |
| semantic_pending_tests | file | 1 | 757 | 757 | 0 | 0 | 0 | The `pending` provenance block (ADR 0009) on the four edge-walking tools, through their real handlers. |
| semantic_rung_worker_tests | file | 1 | 323 | 323 | 0 | 0 | 0 | The resolution ladder's semantic rung (`find_definition::by_semantic_neighbours`, reached by every name-resolving tool) embeds its query lazily, with no store lock held... |
| session_hints | file | 1 | 275 | 0 | 105 | 0 | 23 | One-sentence `hint`s that say how to read a response field, attached only when the field's triggering value is in the response. Rare triggers carry their sentence every... |
| similarity | file | 1 | 741 | 0 | 313 | 0 | 7 | The similarity floor, and the one shape `search_code` has ever had that means *no* (GM-381). |
| source | file | 1 | 256 | 0 | 112 | 0 | 5 | Reads the source a node's coordinates point at, so a caller that asked "where is this defined" does not have to spend a whole round trip finding out what is there. |
| tool_result | file | 1 | 20 | 0 | 0 | 0 | 3 | The three response shapes every MCP tool handler needs - success payload, tool-level failure, and protocol-level failure - factored out once `find_references` needed the... |
| tools_list_tests | file | 1 | 57 | 57 | 0 | 0 | 0 | The `tools/list` result's size, serialized exactly as the server sends it. |
| unlinked | file | 1 | 224 | 0 | 0 | 0 | 6 | Usages the linker could not attach to any declaration, disclosed on the page of a declaration they may belong to. |
| unlinked_tests | file | 1 | 609 | 609 | 0 | 0 | 0 |  |
| untyped | file | 1 | 116 | 0 | 0 | 0 | 2 | Method calls made through a receiver whose type the structural tier did not infer, disclosed on the page of a method they may reach. Design: `docs/architecture/gm-486-un... |
| untyped_tests | file | 1 | 476 | 476 | 0 | 0 | 0 | `untypedReceiverCalls`: the probe against a hand-built index, then both handlers. The end-to-end cases through the rust plugin sit beside `unlinkedUsages`' in `unlinked_... |

`cli/` children (`core/src/cli/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| agent_instructions | file | 1 | 489 | 0 | 251 | 8 | 0 | Auto-installs the cross-tool project-instruction files that `g-mesh init --agent <tool>...` writes, instead of asking a person to hand-copy the snippet README.md documen... |
| clean | file | 1 | 1925 | 0 | 1085 | 8 | 9 | `g-mesh clean`: delete cached project indexes under `~/.g-mesh/projects/`. |
| config_wizard | file | 1 | 431 | 0 | 157 | 3 | 0 | `g-mesh config` / `g-mesh config --global`: an interactive wizard that prompts for a handful of user-facing settings and writes exactly the answered values into `config.... |
| debug_candidates | file | 1 | 90 | 0 | 0 | 1 | 0 | `g-mesh debug-candidates [DIR] [--json]`: the multi-project detection a daemon would run for `DIR` (D10 in `docs/architecture/lazy-indexing.md`), printed. Hidden; for su... |
| deps | dir | 2 | 978 | 292 | 0 | 7 | 0 | `g-mesh deps --json`: every file-level import edge of one project, as one JSON document on stdout (format and exit codes: `docs/cli/deps-json.md`). |
| embed_eval | dir | 14 | 8270 | 0 | 2854 | 160 | 5 | `g-mesh debug-embed-eval`: the embedding search-quality eval (`docs/architecture/embedding-eval.md`). Hidden; an instrument for choosing the embedding model, not a user-... |
| init | file | 1 | 523 | 0 | 168 | 4 | 0 | `g-mesh init`: bootstrap the current project's g-mesh state up front, before any agent has asked it a question. |
| mod | file | 1 | 804 | 0 | 417 | 29 | 1 | The `g-mesh` command-line surface: every subcommand the binary exposes, declared once here with clap's derive API, plus the dispatch that hands each one to the module th... |
| model | file | 1 | 1413 | 0 | 598 | 1 | 4 | `g-mesh model fetch` / `g-mesh model status`: acquiring the embedding weights `search_code` needs, from the binary itself. |
| plugin_check | dir | 7 | 7486 | 1177 | 620 | 16 | 57 | `g-mesh plugins check <plugin-dir> --fixture <project-dir>`: the conformance kit - GM-276, piece 3 of `docs/architecture/multi-language-plugins.md`. |
| plugin_install | file | 1 | 542 | 0 | 0 | 3 | 0 | `g-mesh plugins install` / `g-mesh plugins remove`: adding and deleting a language plugin beside the running g-mesh binary. |
| plugins | file | 1 | 786 | 0 | 468 | 6 | 0 | `g-mesh plugins list`: report the language plugins this install has, globally rather than per-project - a plugin is a property of the g-mesh install, not of any one inde... |
| reindex | file | 1 | 367 | 0 | 134 | 4 | 2 | `g-mesh reindex`: throw away the current project's index and rebuild it from scratch, on demand. |
| status | dir | 3 | 3487 | 1902 | 0 | 25 | 5 | `g-mesh status`: what the current project's daemon, plugin and index are actually doing. |
| stop | file | 1 | 548 | 0 | 198 | 6 | 2 | `g-mesh stop`: shut down the current project's daemon core and every language plugin it has spawned. |

`config/` children (`core/src/config/`):

| child | form | files | lines | test-file lines | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|---|---|
| mod | file | 1 | 463 | 0 | 173 | 14 | 0 | Per-project and global TOML configuration. |

Core `src/` totals: 168 files, 130254 lines, 35780 in test files, 31634 inline test (approx), 1145 pub, 517 pub(r).

## 3. Module dependency edges within core

Edge A -> B: code in top-level module A refers to `crate::B` (A != B). `use` = `use crate::B...` statements (a `use crate::{a, b}` group counts once per distinct first-level target); `inline` = `crate::B::...` paths outside `use` statements. Test files and inline `#[cfg(test)]` blocks are excluded here; edges that exist only in test code are listed after the table. `main` uses `g_mesh::cli` (external-crate path, not counted). `lib` only declares modules.

Fan-out / fan-in (non-test): distinct modules and total use sites.

| module | area | fan-out modules | fan-out sites | fan-in modules | fan-in sites |
|---|---|---|---|---|---|
| cli | C | 14 | 133 | 2 | 6 |
| config | C | 2 | 3 | 4 | 13 |
| daemon | A | 15 | 108 | 7 | 67 |
| embedding | B | 4 | 13 | 5 | 47 |
| gc | A | 2 | 2 | 3 | 5 |
| graph | B | 2 | 9 | 5 | 39 |
| ipc | A | 0 | 0 | 3 | 6 |
| languages | B | 2 | 2 | 4 | 22 |
| log | C | 0 | 0 | 1 | 1 |
| mcp | C | 8 | 91 | 3 | 11 |
| paths | B | 0 | 0 | 6 | 7 |
| process | A | 0 | 0 | 3 | 12 |
| project_walk | B | 1 | 1 | 4 | 12 |
| protocol | A | 1 | 3 | 7 | 29 |
| shim | A | 8 | 13 | 1 | 3 |
| storage | B | 6 | 15 | 9 | 112 |
| watcher | A | 5 | 20 | 3 | 21 |

Edge list (use / inline), grouped by source module:

| from | to (use/inline) |
|---|---|
| cli | config 4/2, daemon 27/8, embedding 18/2, gc 3/0, graph 4/0, languages 4/7, mcp 4/2, paths 1/0, process 2/2, project_walk 1/0, protocol 8/0, shim 2/1, storage 14/15, watcher 2/0 |
| config | paths 0/1, storage 1/1 |
| daemon | cli 0/4, config 1/1, embedding 9/3, gc 1/0, graph 1/0, ipc 3/0, languages 3/5, log 0/1, mcp 2/2, paths 0/1, process 1/6, project_walk 0/7, protocol 9/0, storage 29/1, watcher 17/1 |
| embedding | config 1/3, daemon 1/0, paths 0/1, storage 6/1 |
| gc | config 1/0, storage 1/0 |
| graph | protocol 1/0, storage 7/1 |
| languages | daemon 1/0, project_walk 1/0 |
| mcp | daemon 23/1, embedding 8/0, gc 1/0, graph 26/0, ipc 2/0, languages 2/0, protocol 2/0, storage 23/3 |
| project_walk | watcher 1/0 |
| protocol | graph 3/0 |
| shim | cli 2/0, daemon 4/0, ipc 1/0, mcp 1/0, paths 0/2, process 1/0, protocol 1/0, storage 1/0 |
| storage | daemon 1/0, embedding 5/0, graph 5/0, languages 1/0, paths 1/0, protocol 2/0 |
| watcher | daemon 1/0, embedding 2/0, project_walk 3/0, protocol 3/3, storage 8/0 |

Edges present only in test code: gc->daemon (use 0, inline 3); graph->daemon (use 1, inline 0); graph->watcher (use 0, inline 1); mcp->config (use 0, inline 3); storage->cli (use 0, inline 2); watcher->graph (use 0, inline 6).

Edges also present in test code (non-test use/inline | test use/inline), for pairs where tests add sites: 

| edge | non-test | test |
|---|---|---|
| cli->config | 4/2 | 1/2 |
| cli->daemon | 27/8 | 6/9 |
| cli->embedding | 18/2 | 1/7 |
| cli->graph | 4/0 | 2/0 |
| cli->languages | 4/7 | 0/2 |
| cli->mcp | 4/2 | 0/2 |
| cli->protocol | 8/0 | 0/2 |
| cli->storage | 14/15 | 2/2 |
| config->paths | 0/1 | 0/1 |
| daemon->cli | 0/4 | 0/2 |
| daemon->config | 1/1 | 0/3 |
| daemon->embedding | 9/3 | 2/1 |
| daemon->languages | 3/5 | 1/4 |
| daemon->process | 1/6 | 0/2 |
| daemon->protocol | 9/0 | 4/2 |
| daemon->storage | 29/1 | 9/24 |
| daemon->watcher | 17/1 | 0/4 |
| embedding->config | 1/3 | 0/2 |
| embedding->storage | 6/1 | 2/36 |
| gc->storage | 1/0 | 1/1 |
| graph->protocol | 1/0 | 2/0 |
| graph->storage | 7/1 | 12/1 |
| languages->daemon | 1/0 | 2/0 |
| mcp->daemon | 23/1 | 43/32 |
| mcp->embedding | 8/0 | 20/7 |
| mcp->graph | 26/0 | 17/7 |
| mcp->languages | 2/0 | 2/3 |
| mcp->protocol | 2/0 | 10/3 |
| mcp->storage | 23/3 | 59/3 |
| shim->daemon | 4/0 | 1/0 |
| shim->storage | 1/0 | 1/0 |
| storage->daemon | 1/0 | 0/3 |
| storage->embedding | 5/0 | 0/1 |
| storage->graph | 5/0 | 0/3 |
| storage->languages | 1/0 | 6/2 |
| storage->protocol | 2/0 | 0/2 |
| watcher->daemon | 1/0 | 1/0 |
| watcher->embedding | 2/0 | 2/0 |
| watcher->protocol | 3/3 | 7/0 |
| watcher->storage | 8/0 | 7/5 |

## 4. Cross-area edges (core)

Areas: A = Runtime (daemon, ipc, protocol, shim, watcher, gc, process); B = Data and indexing (graph, storage, embedding, languages, project_walk, paths); C = Surfaces (mcp, cli, config, log, main, lib). Counts are non-test use/inline sites as in section 3.

| from-area | to-area | non-test use | non-test inline | test use | test inline | module pairs (use/inline) |
|---|---|---|---|---|---|---|
| A | A | 42 | 10 | 13 | 11 | daemon->watcher 17/1; daemon->protocol 9/0; daemon->process 1/6; daemon->ipc 3/0; daemon->gc 1/0; shim->daemon 4/0; shim->ipc 1/0; shim->process 1/0; shim->protocol 1/0; watcher->protocol 3/3; watcher->daemon 1/0 |
| A | B | 60 | 19 | 23 | 41 | daemon->embedding 9/3; daemon->languages 3/5; daemon->storage 29/1; daemon->graph 1/0; daemon->project_walk 0/7; daemon->paths 0/1; gc->storage 1/0; protocol->graph 3/0; shim->storage 1/0; shim->paths 0/2; watcher->embedding 2/0; watcher->storage 8/0; watcher->project_walk 3/0 |
| A | C | 7 | 8 | 0 | 5 | daemon->mcp 2/2; daemon->config 1/1; daemon->cli 0/4; daemon->log 0/1; gc->config 1/0; shim->cli 2/0; shim->mcp 1/0 |
| B | A | 7 | 0 | 5 | 6 | embedding->daemon 1/0; graph->protocol 1/0; languages->daemon 1/0; project_walk->watcher 1/0; storage->daemon 1/0; storage->protocol 2/0 |
| B | B | 26 | 3 | 20 | 43 | embedding->storage 6/1; embedding->paths 0/1; graph->storage 7/1; languages->project_walk 1/0; storage->paths 1/0; storage->embedding 5/0; storage->graph 5/0; storage->languages 1/0 |
| B | C | 1 | 3 | 0 | 4 | embedding->config 1/3 |
| C | A | 72 | 12 | 59 | 46 | cli->daemon 27/8; cli->gc 3/0; cli->process 2/2; cli->shim 2/1; cli->protocol 8/0; cli->watcher 2/0; mcp->protocol 2/0; mcp->daemon 23/1; mcp->ipc 2/0; mcp->gc 1/0 |
| C | B | 102 | 29 | 103 | 32 | cli->storage 14/15; cli->graph 4/0; cli->languages 4/7; cli->embedding 18/2; cli->paths 1/0; cli->project_walk 1/0; config->storage 1/1; config->paths 0/1; mcp->graph 26/0; mcp->storage 23/3; mcp->embedding 8/0; mcp->languages 2/0 |
| C | C | 8 | 4 | 1 | 7 | cli->config 4/2; cli->mcp 4/2 |

## 5. Cycles between top-level core modules

Two-module cycles (A uses B and B uses A, non-test code, use or inline path):

| pair | A->B sites (use/inline) | B->A sites (use/inline) | cycle exists with `use` statements alone |
|---|---|---|---|
| cli <-> daemon | 27/8 | 0/4 | no |
| cli <-> shim | 2/1 | 2/0 | yes |
| daemon <-> embedding | 9/3 | 1/0 | yes |
| daemon <-> languages | 3/5 | 1/0 | yes |
| daemon <-> mcp | 2/2 | 23/1 | yes |
| daemon <-> storage | 29/1 | 1/0 | yes |
| daemon <-> watcher | 17/1 | 1/0 | yes |
| embedding <-> storage | 6/1 | 5/0 | yes |
| graph <-> protocol | 1/0 | 3/0 | yes |
| graph <-> storage | 7/1 | 5/0 | yes |
| project_walk <-> watcher | 1/0 | 3/0 | yes |

Strongly connected components with more than one module (Tarjan over the non-test edge set): 1. {cli, config, daemon, embedding, gc, graph, languages, mcp, project_walk, protocol, shim, storage, watcher} (13 modules).
Modules in no multi-module component: ipc, lib, log, main, paths, process.

Test-code cycles: pairs that form a two-module cycle only when test files/inline test blocks are counted: cli<->storage, daemon<->gc, daemon<->graph, graph<->watcher.
## 6. Area D: plugins, wire, walk, scripts, workflows

### 6.1 Crate dependencies (from `cargo metadata --offline --no-deps`, path deps only)

| crate | depends on (workspace path deps) | depended on by |
|---|---|---|
| g-mesh (core) | g-mesh-wire (feature json-schema), g-mesh-walk | - |
| g-mesh-wire | - | g-mesh, g-mesh-plugin-sdk, g-mesh-plugin-python, g-mesh-plugin-rust, g-mesh-plugin-typescript |
| g-mesh-walk | - | g-mesh, g-mesh-plugin-sdk |
| g-mesh-plugin-sdk | g-mesh-wire, g-mesh-walk | g-mesh-plugin-python, g-mesh-plugin-rust, g-mesh-plugin-typescript |
| g-mesh-plugin-python | g-mesh-plugin-sdk, g-mesh-wire | - |
| g-mesh-plugin-rust | g-mesh-plugin-sdk, g-mesh-wire | - |
| g-mesh-plugin-typescript | g-mesh-plugin-sdk, g-mesh-wire | - |

No workspace crate depends on `g-mesh` (core); the SDK and plugins do not link core (stated in the workspace manifest comment). `plugins/go` is a separate Go module (`github.com/madmurdok/g-mesh/plugins/go`, requires `golang.org/x/tools`) with no Rust or workspace dependency; it carries its own `wire.go` and `walk.go`. Each plugin dir has a `plugin.toml` manifest (language: go, python, rust, typescript) and a `conformance/` fixture (`expect.toml` + `project/`).

### 6.2 Crate modules (non-test files; lines / pub / pub(r) / purpose from `//!` first sentence)

`g-mesh-wire` (`wire`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/lib.rs | 1962 |  | 1011 | 49 | 0 | The g-mesh plugin wire protocol: every type that crosses the boundary between core and a language plugin, and nothing else. |

`g-mesh-walk` (`walk`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/lib.rs | 579 |  |  | 9 | 0 | The project walk g-mesh's plugins and core share: which entries under a root a walk reaches, and what became of every symlink on the way. |

`g-mesh-plugin-sdk` (`plugins/sdk`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/columns.rs | 84 |  |  | 6 | 0 | Byte columns in, character columns out. |
| src/diff.rs | 306 |  | 188 | 2 | 0 | The incremental diff: what a `fileChanged` answer is. |
| src/framing.rs | 132 |  | 46 | 2 | 1 | `Content-Length` framing: one header, a blank line, a JSON body. |
| src/generated.rs | 236 |  | 45 | 1 | 0 | Generated-file detection from a file's leading comment block. |
| src/graph.rs | 1110 |  | 383 | 38 | 0 | What an [`Extractor`](crate::Extractor) returns for one file, and the builders that make returning a *conformant* one the path of least res... |
| src/hold.rs | 68 |  |  | 0 | 2 | A test-only knob that parks the plugin at a named point, neither reading stdin nor writing stdout, for as long as a test wants it parked (G... |
| src/ids.rs | 233 |  | 70 | 4 | 0 | Node and edge ids: the one thing in this crate that is a contract with plugins written in other languages, not just with core. |
| src/index.rs | 301 |  | 89 | 20 | 1 | What the plugin remembers: the last extraction of every file it has seen, the text it was extracted from, and the open sites the structural... |
| src/lib.rs | 283 |  |  | 18 | 0 | The g-mesh plugin SDK: everything a language plugin needs that is not its language. |
| src/log.rs | 166 |  | 75 | 7 | 0 | Line-atomic stderr logging for plugins. |
| src/lsp/bridge.rs | 5195 |  | 1801 | 9 | 0 | The engine itself: open sites in, a semantic diff out, with a language server in the middle and a budget around the whole thing. |
| src/lsp/client.rs | 944 |  | 84 | 1 | 19 | The client half of the Language Server Protocol: as much of it as answering an open site needs, and not one method more. |
| src/lsp/config.rs | 625 |  | 134 | 6 | 0 | What the bridge needs to know about a server it has never heard of, and where that comes from. |
| src/lsp/cwd_exe_lookup_windows_tests.rs | 147 | yes |  | 0 | 0 | npm's `.cmd` shims (`vtsls.cmd`, `pyright-langserver.cmd`) run `node` through `cmd.exe`, and `cmd.exe` looks in its current directory - the... |
| src/lsp/mod.rs | 101 |  |  | 4 | 0 | `LspBridge`: a [`SemanticEngine`](crate::SemanticEngine) that answers a structural pass's open sites by driving a language server, with not... |
| src/lsp/position.rs | 383 |  | 137 | 0 | 9 | Columns and file names: the two things the bridge and a language server both have and spell differently. |
| src/lsp/resolve.rs | 530 |  | 311 | 13 | 0 | Finding a language server's binary, and proving it is one before the bridge is handed it. |
| src/manifest.rs | 288 |  | 82 | 6 | 2 | What the plugin knows about itself: its language, the extensions it claims, and the directories its walk must skip. |
| src/path.rs | 109 |  | 29 | 6 | 0 | The one spelling of a file path that crosses the wire. |
| src/resolution.rs | 70 |  |  | 2 | 0 | Resolution facts for a project model keyed by container: which file sits in which container, and which container keys exist. Shared by the... |
| src/run.rs | 2239 |  | 1156 | 1 | 0 | The plugin process: its two modes, its framing, and everything it does between core's questions and the extractor's answers. |
| src/run/resolution_tests.rs | 561 | yes |  | 0 | 0 | `resolutionChanged` and `fileChanged { reextract }` on the control plane, driven through [`Session::handle`] as core sends them. |
| src/semantic.rs | 638 |  | 234 | 10 | 8 | The semantic tier: the engine trait, and the machinery that guarantees it is not started until core asks a semantic question. |
| src/testing.rs | 622 |  | 109 | 22 | 0 | Running `g-mesh plugins check` from a plugin crate's own `#[test]`. |
| src/walk.rs | 752 |  | 536 | 10 | 0 | The project walk: which files a plugin's `--bulk-index` mode turns into `File` nodes. |

`g-mesh-plugin-rust` (`plugins/rust`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/census.rs | 377 |  | 116 | 0 | 18 | GM-314: a test-only census of what the structural tier *declines* to turn into an open site. |
| src/extractor/bodies.rs | 1921 |  |  | 0 | 3 | The body pass: every edge, every placeholder that is not a `use`, and every open site. |
| src/extractor/decls.rs | 1065 |  | 98 | 0 | 15 | The declaration pass: every node this file contributes to the graph, and everything a `use` item says. |
| src/extractor/emit.rs | 436 |  | 85 | 0 | 21 | The one place this plugin writes to the graph: a thin layer over the SDK's [`FileGraphBuilder`] that adds the two things a Rust extractor c... |
| src/extractor/keys.rs | 573 |  | 161 | 0 | 14 | Container keys, `qualifiedName`s and visibility: the three things every node this plugin emits has to get right, and the one place the `::`... |
| src/extractor/mod.rs | 231 |  |  | 1 | 1 | The Rust structural extractor (GM-286): one file's bytes in, one [`FileGraph`] out. |
| src/extractor/model.rs | 291 |  | 54 | 0 | 19 | What the declaration pass learned about one file, and the body pass resolves against: every declaration by the two names it can be looked u... |
| src/extractor/scope.rs | 375 |  | 167 | 0 | 11 | Decision 1: the lexical scope stack, and why a structural extractor cannot do without one. |
| src/extractor/syntax.rs | 396 |  | 119 | 0 | 12 | The thin layer between tree-sitter-rust's concrete syntax tree and the questions the rest of this extractor asks of it: what is this item's... |
| src/extractor/tests.rs | 3084 | yes |  | 0 | 0 | Unit tests for the extractor, one per shape the task names. |
| src/extractor/tests/fn_local_impls.rs | 550 | yes |  | 0 | 0 | An `impl` block declared as its own node (Decision 8) inside a fn body is qualified by the fns it is inside, as rustc's def paths are, so t... |
| src/extractor/tests/glob_paths.rs | 227 | yes |  | 0 | 0 | In a module with a glob `use`, a path whose first segment names no workspace crate, declared `mod` or named import of the module (`use supe... |
| src/extractor/tests/module_paths.rs | 271 | yes |  | 0 | 0 | A path qualifier spelled like a type (`Keys::f()`) is a module when this file declares a module of that name or the project's module regist... |
| src/extractor/tests/variants.rs | 190 | yes |  | 0 | 0 | GM-630 / ADR 0038: an enum variant is a node of its own, named `E::V`, of the kind its shape takes in Rust's value namespace. |
| src/extractor/typing.rs | 306 |  | 64 | 0 | 13 | Typed receivers: the type of a local, when this file spells it out. |
| src/lib.rs | 23 |  |  | 4 | 1 | `g-mesh-plugin-rust`'s library half: the [`Extractor`](g_mesh_plugin_sdk::Extractor) and the project model it runs on. Split from `main.rs`... |
| src/main.rs | 30 |  |  | 0 | 0 | `g-mesh-plugin-rust`: the entry point the SDK's [`run`] loop drives. Everything else lives in the library half of this crate (`src/lib.rs`)... |
| src/project/cargo_manifest.rs | 207 |  | 22 | 0 | 7 | Reading `Cargo.toml` as data - never by invoking `cargo` - and resolving `[workspace] members`/`exclude` globs against the filesystem. |
| src/project/facts.rs | 515 |  | 310 | 6 | 0 | The resolution facts of a [`ProjectContext`] ([`RustFacts`]) and what changed between two of them ([`resolution_delta`]): which files a `Ca... |
| src/project/mod.rs | 1488 |  | 828 | 12 | 0 | `ProjectContext`: the Rust plugin's [`Extractor::Project`](g_mesh_plugin_sdk::Extractor::Project) - every crate `Cargo.toml`/workspace memb... |
| src/project/module_tree.rs | 956 |  | 228 | 0 | 6 | Building one crate's module tree - which file (or, for an inline module, which nested block of which file) backs each `<crate>::<module pat... |
| src/sandbox.rs | 1566 |  | 169 | 5 | 19 | The OS sandbox rust-analyzer and every process it starts run under. |
| src/sandbox/landlock_ns.rs | 1229 |  | 542 | 2 | 23 | The Linux backend: the plugin binary is its own trampoline. |
| src/semantic.rs | 2480 |  | 1469 | 4 | 2 | The Rust plugin's semantic tier: finding rust-analyzer, and handing it to the SDK's generic [`LspBridge`]. |

`g-mesh-plugin-python` (`plugins/python`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/census.rs | 274 |  | 94 | 0 | 13 | GM-314: a test-only census of what the structural tier *declines* to turn into an open site. |
| src/extractor/bodies.rs | 1267 |  |  | 0 | 3 | The body pass: every edge that is not an import, every placeholder that is not a `from … import`, and every open site. |
| src/extractor/decls.rs | 1030 |  | 44 | 0 | 11 | The declaration pass: every node this file contributes to the graph, and everything an `import` statement says. |
| src/extractor/emit.rs | 550 |  | 86 | 0 | 23 | The one place this plugin writes to the graph: a thin layer over the SDK's [`FileGraphBuilder`] that adds the two things a Python extractor... |
| src/extractor/keys.rs | 395 |  | 89 | 0 | 9 | Container keys, `qualifiedName`s, visibility and import resolution: the four things every node and every edge this plugin emits has to get... |
| src/extractor/mod.rs | 272 |  |  | 1 | 0 | The Python structural extractor (GM-296): one file's bytes in, one [`FileGraph`] out. |
| src/extractor/model.rs | 554 |  | 112 | 0 | 27 | What the declaration pass learned about one file, and the body pass resolves against: every declaration by the two keys it can be looked up... |
| src/extractor/scope.rs | 626 |  | 149 | 0 | 22 | Decision 1: the lexical scope stack, and why a structural extractor cannot do without one. |
| src/extractor/syntax.rs | 554 |  | 131 | 0 | 17 | The thin layer between tree-sitter-python's concrete syntax tree and the questions the rest of this extractor asks of it: what does this do... |
| src/extractor/tests.rs | 2399 | yes |  | 0 | 0 | Unit tests for the extractor, one per shape the task names. |
| src/lib.rs | 25 |  |  | 3 | 1 | `g-mesh-plugin-python`'s library half: the [`Extractor`](g_mesh_plugin_sdk::Extractor) and the project model it runs on. Split from `main.r... |
| src/main.rs | 25 |  |  | 0 | 0 | `g-mesh-plugin-python`: the entry point the SDK's [`run`] loop drives. Everything else lives in the library half of this crate (`src/lib.rs... |
| src/project/facts.rs | 334 |  | 173 | 6 | 0 | The resolution facts of a [`ProjectContext`] ([`PyFacts`]) and what changed between two of them ([`resolution_delta`]): which files a `pypr... |
| src/project/mod.rs | 1349 |  | 534 | 11 | 0 | `ProjectContext`: the Python plugin's [`Extractor::Project`](g_mesh_plugin_sdk::Extractor::Project) - which container key owns each `.py`/`... |
| src/project/pyproject.rs | 224 |  | 97 | 0 | 1 | Reading `pyproject.toml`'s own root-redirection hints - never by running `setup.py`, invoking `pip`, or asking a build backend anything. |
| src/semantic.rs | 951 |  | 500 | 6 | 0 | The Python plugin's semantic tier: finding pyright, telling it where the project's venv packages are, and handing it to the SDK's generic [... |

`g-mesh-plugin-typescript` (`plugins/typescript`):

| file | lines | test-file | inline test | pub | pub(r) | purpose |
|---|---|---|---|---|---|---|
| src/extractor/bodies.rs | 495 |  |  | 1 | 13 | Function bodies and the names they use: the lexical scope chain, calls and references. |
| src/extractor/decls.rs | 674 |  |  | 4 | 5 | The declaration pass: every class, interface, type alias, enum, namespace, function, method, function-valued field and module-level variabl... |
| src/extractor/emit.rs | 64 |  |  | 1 | 0 | The settled draft, flushed into the SDK's [`FileGraphBuilder`]. |
| src/extractor/grammar.rs | 110 |  |  | 7 | 0 | Which tree-sitter grammar reads which extension: `typescript` for `.ts/.mts/.cts`, `tsx` for `.tsx`, `javascript` for `.js/.jsx/.mjs/.cjs`... |
| src/extractor/imports.rs | 601 |  |  | 11 | 7 | Imports, re-exports and computed specifiers: the part of the walk that names other files. |
| src/extractor/imports/tests.rs | 152 | yes |  | 0 | 0 | The names an import binds. They are read back only by uses of those names, so these tests drive the import statements directly and ask the... |
| src/extractor/keys.rs | 135 |  |  | 17 | 0 | How a node is addressed: `qualifiedName`, `qualifiedPath`, and the placeholder addresses. |
| src/extractor/mod.rs | 137 |  |  | 11 | 0 | One TypeScript or JavaScript file's structural graph, from tree-sitter. |
| src/extractor/model.rs | 511 |  |  | 31 | 0 | The per-file draft graph: nodes and edges by id, in insertion order, plus every declaration written for each node. |
| src/extractor/scope.rs | 277 |  |  | 14 | 0 | The lexical context threaded through the walk. |
| src/extractor/sites.rs | 161 |  |  | 0 | 6 | The questions the structural pass leaves for a semantic tier, as SDK open sites (section 1.4 of `docs/architecture/gm-324-typescript-rust-p... |
| src/extractor/syntax.rs | 177 |  |  | 13 | 0 | Reading the syntax tree: node text, literals, heritage clauses, signatures and doc comments. |
| src/lib.rs | 14 |  |  | 3 | 0 | `g-mesh-plugin-typescript`: the TypeScript/JavaScript language plugin on the Rust plugin SDK. |
| src/main.rs | 21 |  |  | 0 | 0 | `g-mesh-plugin-typescript`: the entry point the SDK's [`run`] loop drives. Everything else lives in the library half of this crate (`src/li... |
| src/project/exports.rs | 295 |  |  | 16 | 0 | package.json `exports` and `imports` maps: which paths a subpath or a `#name` could name, most specific first. |
| src/project/facts.rs | 280 |  |  | 6 | 0 | The resolution facts of a [`TsProject`] ([`TsFacts`]) and what changed between two of them ([`resolution_delta`]): which importers a config... |
| src/project/jsonc.rs | 210 |  |  | 8 | 0 | JSON as the project model reads it: objects keep their key order, and tsconfig files may carry comments and trailing commas (JSONC). |
| src/project/mod.rs | 269 |  |  | 17 | 0 | The project model [`TsProject`] the extractor resolves import specifiers against, and the resolver itself ([`TsProject::resolve`]). |
| src/project/paths.rs | 109 |  |  | 11 | 0 | Project-relative POSIX path arithmetic, the spelling every lookup in the project model uses. The root directory is `""`. |
| src/project/resolve.rs | 810 |  |  | 22 | 0 | Module-specifier resolution: which indexed file an import names. |
| src/project/tsconfig.rs | 836 |  |  | 16 | 0 | tsconfig/jsconfig `paths` aliases (`@/utils` -> `src/utils.ts`), with `baseUrl` and the `extends` chain applied. |
| src/project/version_range.rs | 272 |  |  | 1 | 0 | tsc 5.9's semver ranges (`VersionRange` in `semver.ts`), only as far as testing them against tsc's own version: the range of a versioned `t... |
| src/project/workspace.rs | 473 |  |  | 14 | 0 | The project's own workspace packages: which directory a bare package name like `@scope/math` is, and which paths its entry could be. |
| src/semantic.rs | 276 |  | 126 | 1 | 0 | The TypeScript plugin's semantic tier: finding vtsls and handing it to the SDK's generic [`LspBridge`]. |

Non-`src` Rust in plugin crates (lines of `.rs`):

| dir | files | .rs lines |
|---|---|---|
| plugins/sdk/tests | 8 | 5726 |
| plugins/sdk/fake | 2 | 1034 |
| plugins/sdk/fake-lsp | 1 | 928 |
| plugins/sdk/toy | 1 | 407 |
| plugins/rust/tests | 6 | 4336 |
| plugins/python/tests | 5 | 1586 |
| plugins/typescript/tests | 16 | 8979 |
| core/tests | 103 | 35906 |
| core/tests/it | 92 | 30204 |

`plugins/sdk` `[[bin]]` targets: `g-mesh-plugin-toy` (`toy/main.rs`), `g-mesh-fake-lsp` (`fake-lsp/main.rs`); `plugins/sdk/fake/` holds a fake plugin used by SDK tests. `plugins/python` and `plugins/typescript` also have `package.json`/`package-lock.json` (npm dependencies for the language servers used by tests/bundling).

### 6.3 Go plugin (`plugins/go`)

18 non-test `.go` files, 8084 lines; 30 `_test.go` files, 8601 lines.

| file | lines |
|---|---|
| bulkindex.go | 97 |
| control.go | 448 |
| extract.go | 1024 |
| facts.go | 343 |
| generated.go | 257 |
| hold.go | 55 |
| ids.go | 52 |
| ignore.go | 230 |
| jsonrpc.go | 107 |
| main.go | 232 |
| open_sites.go | 182 |
| scope.go | 141 |
| semantic.go | 2961 |
| symlinks.go | 127 |
| uses.go | 808 |
| walk.go | 160 |
| wire.go | 379 |
| workspace.go | 481 |

### 6.4 Scripts (`scripts/`)

| script | lines | purpose (first comment/docstring sentence) |
|---|---|---|
| build-targets.sh | 467 | Builds `g-mesh` for one or more Rust target triples and packages each build into a release archive with a stable, predictable name. |
| bundle-go-plugin.sh | 214 | Cross-compiles the bundled Go plugin (plugins/go) for one release target and stages it, with a manifest naming its own binary, as a `go/` plugin directory core can disco... |
| bundle-plugin.sh | 188 | Builds the bundled TypeScript/JavaScript plugin (plugins/typescript) for one release target and stages it, with a manifest naming its own binary, as a `typescript/` plug... |
| bundle-python-plugin.sh | 204 | Builds the bundled Python plugin (plugins/python) for one release target and stages it, with a manifest naming its own binary, as a `python/` plugin directory core can d... |
| bundle-rust-plugin.sh | 216 | Builds the bundled Rust plugin (plugins/rust) for one release target and stages it, with a manifest naming its own binary, as a `rust/` plugin directory core can discove... |
| check.sh | 49 | The formatting and lint gates, defined once so a local run and CI cannot disagree about what they check. |
| ci-diff-runs.sh | 299 | What changed between two CI runs (GM-345). |
| ci-green-for-commit.sh | 140 | GM-613: has ci.yml already passed a *full* run on this exact code? |
| ci-junit-summary.py | 89 | Summarise a nextest JUnit file as a per-binary failure table (GM-344). |
| cut-release.sh | 969 | One command to validate, test and tag a release, instead of the four manual steps described in .github/workflows/release.yml's header (check the crate version, run the w... |
| install.ps1 | 670 | Installs a released g-mesh on Windows: downloads the x86_64-pc-windows-msvc release archive from GitHub Releases, verifies its SHA-256 before unpacking, and puts core and its bundled plugins on disk together. |
| install.sh | 862 | Installs a released g-mesh: detects the platform, downloads the matching release archive from GitHub Releases, verifies its SHA-256 before unpacking, and puts core *and*... |
| mcp-smoke.py | 347 | One MCP round trip through an installed g-mesh, standard library only. |
| new-task-worktree.sh | 136 | Creates a task worktree next to the main checkout, ready to build: installs the TypeScript and Python plugins' npm dependencies |
| prepare-release-assets.sh | 268 | Checks that a directory holds exactly the assets a complete four-target release consists of - per target the main archive and one plugin asset per bundled language, each... |
| prune-stale-objects.sh | 124 | Deletes the `*.rcgu.o` files in <target>/debug/deps that no current executable or dylib references. |
| refresh-mcp-install.sh | 415 | Installs or refreshes g-mesh in a fixed directory an MCP client runs it from (e.g |
| release-smoke.sh | 268 | GM-333: runs a freshly staged release artifact for real, on the runner that just built it, instead of trusting that a green `cargo build` means the thing works. |
| test-ci-green-for-commit.sh | 334 | Offline tests for scripts/ci-green-for-commit.sh (GM-613's release test gate) and for the smoke/architecture guards in scripts/lib/runnable.sh. |
| test-deps.sh | 84 | The test dependencies `cargo test --workspace` needs that cargo cannot fetch itself, installed the way CI installs them - so a fresh clone or worktree gets a green suite... |
| test-heavy-report.py | 118 | Compares a full run's test times against the `local` profile's heavy list. |
| test-install.sh | 753 | Tests install.sh's PATH editing without a release and without touching the real shell rc files of whoever runs it. |
| test-linux-sandbox-docker.sh | 99 | Runs the Rust plugin's Linux sandbox tests (`sandbox::landlock_ns` unit tests, `semantic::tests::a_partial_sandbox_is_admitted`) and every integration binary that starts... |
| test-local.sh | 357 | The local test run: the sections a change can affect, faster than CI runs them |
| test-plugin-spawns.sh | 267 | Observes which core tests start a real plugin process, and checks or writes scripts/test-plugin-spawns.txt, the map behind `scripts/test-sections.sh filter core-spawns-<... |
| test-plugin-spawns.txt | 112 | data file: map of core tests that start a real plugin process (written/checked by test-plugin-spawns.sh) |
| test-sections-check.py | 89 | Check that test sections partition the suite. |
| test-sections.sh | 321 | The test suite as named sections, each a nextest filterset over the whole workspace |
| test-select.sh | 241 | Maps a set of changed paths to the test sections of scripts/test-sections.sh that the change can affect |
| lib/plugin-bins.sh | 29 | GM-628: the plugins' test dependencies (vtsls, pyright) are found on PATH only (ADR 0032 D7); CI puts each plugin's node_modules/.bin there in its "Install vtsls" step,... |
| lib/runnable.sh | 65 | GM-613: "can this machine execute what it just built, and is it really the architecture it claims to be". Sourced by scripts/build-targets.sh and the four scripts/bundle... |

`scripts/release-smoke-fixture/` is a directory (fixture for `release-smoke.sh`).

### 6.5 Workflows (`.github/workflows/`)

| workflow | lines | name | triggers | jobs (id: display name; needs; condition) |
|---|---|---|---|---|
| ci.yml | 1307 | CI | push (main, `release-*`, `experiment/*`), pull_request, workflow_call (input `full_suite`), workflow_dispatch (input `target`, ...) | `matrix`: Select the platforms to test; `select`: Select the test sections; `test`: tests (${{ matrix.target }}) [needs matrix, select; if select mode != none]; `full-suite`: Full test suite ran [needs matrix, select, test; if mode == full and partial == false]; `shellcheck`: shellcheck; `installer`: install.sh PATH tests (${{ matrix.os }}); `go-plugin-without-toolchain`: Go plugin conformance (no toolchain on PATH); `lint`: cargo clippy |
| release.yml | 1069 | Release artifacts | workflow_dispatch, push tags `v*`, pull_request (paths: release.yml and release build/bundle scripts) | `ci-gate`: CI already green on this code?; `tests`: calls `./.github/workflows/ci.yml` [needs ci-gate; if not green]; `build`: ${{ matrix.target }} [needs ci-gate, tests]; `publish`: Publish Release [needs build; if tag `v*`]; `install-latest`: Install the latest published release on Windows [needs publish]; `verify`: Verify assets (dry run) [needs build; if not a tag] |

Job counts: ci.yml 8, release.yml 6. `test` and `build` are matrix jobs.

## 7. Largest files and functions

### 7.1 Largest 15 Rust files (all)

| # | file | lines | test file |
|---|---|---|---|
| 1 | plugins/sdk/src/lsp/bridge.rs | 5195 |  |
| 2 | plugins/sdk/tests/lsp_bridge.rs | 4846 | yes |
| 3 | core/src/graph/symbol_links/tests.rs | 4719 | yes |
| 4 | plugins/typescript/tests/project_model.rs | 3290 | yes |
| 5 | plugins/rust/src/extractor/tests.rs | 3084 | yes |
| 6 | core/src/watcher/apply/tests.rs | 3058 | yes |
| 7 | core/src/cli/embed_eval.rs | 2849 |  |
| 8 | plugins/rust/tests/semantic_runs_no_project_code.rs | 2791 | yes |
| 9 | core/src/daemon/workspace_reindex.rs | 2490 |  |
| 10 | plugins/rust/src/semantic.rs | 2480 |  |
| 11 | core/src/mcp/find_callers_callees.rs | 2417 |  |
| 12 | plugins/python/src/extractor/tests.rs | 2399 | yes |
| 13 | core/src/shim/router.rs | 2359 |  |
| 14 | core/src/mcp/instructions/tests.rs | 2295 | yes |
| 15 | core/src/mcp/find_definition/tests.rs | 2287 | yes |

### 7.2 Largest 15 Rust files (excluding test files)

| # | file | lines | inline test (approx) |
|---|---|---|---|
| 1 | plugins/sdk/src/lsp/bridge.rs | 5195 | 1801 |
| 2 | core/src/cli/embed_eval.rs | 2849 | 1040 |
| 3 | core/src/daemon/workspace_reindex.rs | 2490 | 2002 |
| 4 | plugins/rust/src/semantic.rs | 2480 | 1469 |
| 5 | core/src/mcp/find_callers_callees.rs | 2417 | 1565 |
| 6 | core/src/shim/router.rs | 2359 | 1135 |
| 7 | core/src/storage/schema.rs | 2250 | 0 |
| 8 | plugins/sdk/src/run.rs | 2239 | 1156 |
| 9 | core/src/graph/symbol_links.rs | 2174 | 0 |
| 10 | core/src/embedding/pipeline.rs | 2144 | 1225 |
| 11 | core/src/graph/pagination.rs | 2126 | 1026 |
| 12 | core/src/mcp/find_references.rs | 2084 | 1634 |
| 13 | core/src/daemon/registry.rs | 2037 | 0 |
| 14 | core/src/cli/plugin_check/expectations.rs | 1965 | 0 |
| 15 | wire/src/lib.rs | 1962 | 1011 |

Largest non-Rust source file: see 6.3 (Go plugin) and 6.4 (scripts; `cut-release.sh` 969 lines).

### 7.3 Largest 25 functions

Function length = lines from the `fn` line to its matching closing brace (brace counting over comment/string-stripped source; approximate; closures and nested fns are counted inside their parent; fns inside inline `#[cfg(test)]` blocks are not flagged). Column "test file" = path is a test file by the Conventions rule, or under `tests/`, or a test-support fixture dir (`fake`, `fake-lsp`, `toy`).

| # | lines | fn | location | test file / fixture |
|---|---|---|---|---|
| 1 | 294 | run | core/src/daemon/mod.rs:308 |  |
| 2 | 268 | check | core/src/cli/plugin_check/mod.rs:113 |  |
| 3 | 262 | main | plugins/sdk/fake-lsp/main.rs:360 | yes |
| 4 | 259 | write_diff | core/src/storage/write.rs:397 |  |
| 5 | 255 | run_corpus | core/src/cli/embed_eval.rs:854 |  |
| 6 | 220 | report | core/src/cli/embed_eval.rs:1477 |  |
| 7 | 206 | answer | plugins/sdk/src/lsp/bridge.rs:3167 |  |
| 8 | 197 | plan_attached | core/src/storage/language_swap.rs:177 |  |
| 9 | 185 | swap_attached | core/src/storage/language_swap.rs:509 |  |
| 10 | 183 | apply_semantic_pass_in | core/src/watcher/apply.rs:637 |  |
| 11 | 163 | equivalence_fixture | core/src/graph/symbol_links/tests.rs:1820 | yes |
| 12 | 159 | export | core/src/cli/deps.rs:435 |  |
| 13 | 157 | paginate_edges | core/src/graph/pagination.rs:724 |  |
| 14 | 151 | handle | plugins/sdk/src/run.rs:418 |  |
| 15 | 150 | render | core/src/cli/status.rs:909 |  |
| 16 | 148 | apply_file_change_in | core/src/watcher/apply.rs:234 |  |
| 17 | 142 | run_with_progress | core/src/daemon/bulk_index.rs:137 |  |
| 18 | 142 | a_timed_out_file_change_relaunches_the_plugin_and_replays_the_dirty_file_without_blocking_another_language | core/src/daemon/lifecycle/tests.rs:74 | yes |
| 19 | 138 | deps_on_a_cold_ts_rust_python_project_matches_get_dependencies_and_reruns_byte_identically | core/tests/it/cli_deps.rs:272 | yes |
| 20 | 128 | reconcile_since_index | core/src/daemon/registry.rs:1458 |  |
| 21 | 126 | init | core/src/cli/init.rs:160 |  |
| 22 | 126 | get_dependencies_walks_real_files_in_both_directions_after_a_cold_start | core/tests/it/import_resolution.rs:134 | yes |
| 23 | 126 | a_namespace_import_caller_needs_the_semantic_pass_to_resolve | core/tests/it/plugin_check.rs:1290 | yes |
| 24 | 122 | calibrate | core/src/cli/embed_eval/cost.rs:433 |  |
| 25 | 121 | the_plugin_sleeps_alone_and_a_request_replays_only_what_it_missed | core/tests/it/idle_lifecycle.rs:326 | yes |

Largest 10 functions outside test/fixture files:

| # | lines | fn | location |
|---|---|---|---|
| 1 | 294 | run | core/src/daemon/mod.rs:308 |
| 2 | 268 | check | core/src/cli/plugin_check/mod.rs:113 |
| 3 | 259 | write_diff | core/src/storage/write.rs:397 |
| 4 | 255 | run_corpus | core/src/cli/embed_eval.rs:854 |
| 5 | 220 | report | core/src/cli/embed_eval.rs:1477 |
| 6 | 206 | answer | plugins/sdk/src/lsp/bridge.rs:3167 |
| 7 | 197 | plan_attached | core/src/storage/language_swap.rs:177 |
| 8 | 185 | swap_attached | core/src/storage/language_swap.rs:509 |
| 9 | 183 | apply_semantic_pass_in | core/src/watcher/apply.rs:637 |
| 10 | 159 | export | core/src/cli/deps.rs:435 |

## 8. Method

- Line counts: Python `str.count('\n')` over every `*.rs` (script `an.py`), equivalent to `wc -l`; `wc -l` for scripts and workflows. Test-file split by path rule (Conventions above). Inline-test lines: brace-balanced `#[cfg(test)] mod x { }` blocks after stripping comments/strings (approximate).
- pub counts: regex `^\s*pub(\(...\))?\s+(async|unsafe|const|extern)* (fn|struct|enum|trait|type|const|static|mod|union|use|macro_rules!)` over comment/string-stripped source (sections 2, 6.2).
- Section 1: `cargo metadata --offline --format-version 1 --no-deps` (path deps and kinds) plus line counts above.
- Section 2 purposes: first `//!` paragraph of the module file (`mod.rs` or `<name>.rs`, truncated to 170 chars); where none exists the cell says "no module doc" and lists pub items found by `grep -nE "^pub (fn|struct|enum)"`/`mod` declarations.
- Sections 3-5: a top-level module is the first path component under `core/src` (file or directory). Edges come from a regex parse of `use crate::X`/`use crate::{X.., Y..}` statements and inline `crate::X::` paths (excluding `use` statements), per file; inline `#[cfg(test)]` blocks are counted as test. Cross-area and cycle tables are computed from this edge set (two-cycles by pair lookup; components by Tarjan). References through `super::`, `self::`, `g_mesh::` (external-crate paths from `main.rs`, `core/tests`) and `use crate::*` globs are not counted. Textual, not name-resolved.
- g-mesh calls: `select_project`/`get_dependencies` were loaded; one call, `get_dependencies` (Outgoing, depth 1) on `core/src/graph/queries.rs` in project `g-mesh`, returned storage, storage::schema, storage::write, graph::containers, graph::imports, graph::symbol_links, graph::queries (plus anyhow, rusqlite), consistent with the regex edges for that file (graph->storage). No other g-mesh calls; all other tables are mechanical.
- Section 6: `cargo metadata` (crate deps), `grep`/`awk` over `.github/workflows/*.yml` for job ids, `name:`, `needs:`, `if:`, `uses:`; script purposes are the first comment/docstring sentence of each file.
- Section 7: file sizes as above; function lengths by brace counting from each `fn` line (`an.py`, `fn_spans`), top 40 kept.
- Scratch scripts: `an.py`, `gen.py`, `gen2.py`, `gen3.py` in the session scratchpad (not part of the repo).
