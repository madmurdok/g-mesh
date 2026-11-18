# Area B (Data and indexing) - architecture review

Model: claude-opus-5-5 (Opus 5.5). Blind review, GM-704 S4. Code: worktree `g-mesh-wt-gm704` (= main 97fa0f87 plus version bump); g-mesh project `g-mesh`.

Scope: `core/src/{graph,storage,embedding,languages,languages.rs,project_walk.rs,paths.rs}`. Goal: seams that let weakly coupled parts change independently.

Summary of the shape (measured, module-map sections 3-5 plus the greps below): area B is not layered. `storage` is meant to be the bottom of the data stack, but it imports `graph` (5 sites) and `embedding` (5 sites), and both import `storage` back, so storage<->graph and storage<->embedding are two-module cycles. Area B also points up into Runtime: storage->daemon, languages->daemon, project_walk->watcher, embedding->daemon, graph->protocol. That puts all 13 core modules in one strongly connected component (module-map section 5). Most of these upward edges come down to one constant, one hash function or one counter living in the wrong module. Those are the cheap wins (findings 1-3). The structural one is finding 4.

Ranked by benefit/effort, highest first.

---

## B-opus-1. Placeholder native kinds are a wire contract defined in core `graph`, again in the SDK, and again in Go

- **Location:** `core/src/graph/symbol_links.rs:PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND` (305, 311); `core/src/graph/imports.rs:RESOLVED_MODULE_NATIVE_KIND, EXTERNAL_MODULE_NATIVE_KIND` (97, 116); `core/src/graph/containers.rs:CONTAINER_NATIVE_KIND` (197); `plugins/sdk/src/graph.rs:PlaceholderKind` (238-241); `plugins/go/extract.go:107-117`; consumers `core/src/protocol/conformance.rs:7-9`, `core/src/storage/language_swap.rs:57-58`.
- **Problem / principle:** duplication of a cross-process contract, and wrong dependency direction. Plugins emit the strings `pending_symbol`, `reexport`, `resolved_module` and `external_module`, and core matches on them. The wire crate (`g_mesh_wire`) is shared by core and the SDK, and its doc comments name these kinds (`wire/src/lib.rs:165, 421`), yet it does not define them. Because core defines them in `graph`, `protocol::conformance` imports `graph`. That is the whole `protocol -> graph` half of the graph<->protocol cycle (module-map: protocol->graph 3/0). `storage::language_swap` imports `graph` only for two of these constants.
- **Proposed change:** add `pub enum PlaceholderKind` with `as_str()` (or `pub const` strings) to `g_mesh_wire`, next to the node/edge types. Core's `graph::*`, `protocol::conformance` and `storage::language_swap` import from `crate::protocol::types` (which re-exports `g_mesh_wire::*`, `protocol/types.rs:18`). The SDK's `PlaceholderKind::as_str` uses the wire enum. The Go plugin keeps its literals, guarded by the existing conformance test.
- **Benefit:** adding or renaming a placeholder kind becomes a one-crate change that core and the SDK both see at compile time. The `protocol -> graph` edge disappears, and `storage -> graph` loses its constant-only sites.
- **Effort/Risk:** S. Risk: very low (string values unchanged). A typo in the move is caught by `protocol_conformance` tests.
- **Evidence:** grep for `NATIVE_KIND: &str` (measured: five consts, all in `core/src/graph`). Grep for the literals in `wire/src plugins/sdk/src plugins/go` (measured: `plugins/sdk/src/graph.rs:238-241`, `plugins/go/extract.go:107-117`). `protocol/conformance.rs:7-9` imports the three graph modules (measured).

## B-opus-2. `BASELINE_EXCLUDED_DIRS` is defined twice, and core's walk takes it from `watcher`

- **Location:** `core/src/watcher/mod.rs:34` and `plugins/sdk/src/walk.rs:50` (both `[".git", ".claude"]`); `core/src/project_walk.rs:31` (`use crate::watcher::BASELINE_EXCLUDED_DIRS`).
- **Problem / principle:** duplication, and a data-layer module depending on runtime. Both definitions feed the same function, `g_mesh_walk::walk` (`project_walk.rs:56`). The walk crate is the one place core and the SDK share for "which tree is the project", but the constant lives outside it. The comment at `watcher/mod.rs:30` says it must mirror the SDK's copy, so it is kept in sync by hand. This constant is the only `project_walk -> watcher` edge (module-map: 1/0), and it closes the project_walk<->watcher cycle.
- **Proposed change:** move the constant into `walk/src/lib.rs` as `g_mesh_walk::BASELINE_EXCLUDED_DIRS`, and have `walk()` add it itself (both callers already prepend it: `project_walk.rs:170`, `sdk walk.rs:208`). `watcher::ignore_layers`, `project_walk` and the SDK (re-export for plugins) import from there.
- **Benefit:** changing the baseline exclusions is one edit seen by every walker: core, the watcher and all plugins. `project_walk` no longer depends on `watcher`.
- **Effort/Risk:** S. Risk: a plugin built against an older SDK keeps its own copy. That is already the case today.
- **Evidence:** `grep -rn BASELINE_EXCLUDED_DIRS core/src plugins walk` (measured: two definitions, the hand-mirror comment at `watcher/mod.rs:30`).

## B-opus-3. The on-disk state layout is split across `paths`, `storage::connection` and `daemon::identity`

- **Location:** `core/src/daemon/identity.rs:project_hash` (10), `ROOT_FILE`, `record_project_root`, `read_project_root`; `core/src/storage/connection.rs:7` (`use crate::daemon::identity::{self, project_hash}`), `projects_root`, `project_dir`, `ensure_project_dir`; `core/src/paths.rs:g_mesh_home`.
- **Problem / principle:** low cohesion, and a dependency pointing upward. "Where a project's state lives" is one concern: home, then `projects/`, then `<hash>/`, then `project.root`. Its parts live in three modules, and the hash is in `daemon`. So `storage`, the lowest data module, depends on `daemon` (module-map: storage->daemon 1/0). `shim`, `ipc/windows` and `cli::clean` also reach into `daemon::identity` for the hash (grep: `project_hash` used in `cli/clean.rs` 6, `shim.rs` 2, `ipc/windows.rs` 1). The comment on `ensure_project_dir` (`connection.rs:38-60`) shows the cost: three call sites once created this directory three different ways.
- **Proposed change:** make `paths` (already "Where g-mesh keeps what it owns") own the whole layout: `project_hash`, `projects_root[_under]`, `project_dir[_under]`, `ensure_project_dir`, `ROOT_FILE` and the root-file read/write. `storage::connection` keeps only opening SQLite connections, `daemon::identity` keeps socket/pid naming, and callers switch imports.
- **Benefit:** changing the layout (hash length, a new per-project file, a relocatable home) touches one leaf module with no dependencies. `storage` no longer depends on `daemon`, and `shim` and `ipc` get the hash without importing `daemon`.
- **Effort/Risk:** S (move plus import changes, about 40 sites, mostly `storage::connection::project_dir` at 25 sites by grep). Risk: low. The hash value is unchanged; a changed hash would orphan every existing state dir, so a pinned-hash test belongs with the move.
- **Evidence:** `storage/connection.rs:7, 206` (measured). Grep counts for `project_hash` and `storage::connection::project_dir` (measured).

## B-opus-4. `storage` orchestrates the write pipeline: `IndexStore`, `language_swap` and `write` call graph linking and embedding

- **Location:** `core/src/storage/index_store.rs:39-42` (imports `embedding::{EmbeddingPipeline, ComputedBatch}` and `graph::{imports, symbol_links, LinkRules}`), `link_applied` (163), `Writer::apply_file_diff_linked` (491), `IndexStore::swap_language` (362), `Writer::store_vectors` (548); `core/src/storage/language_swap.rs:54-58` (`embedding::text::node_text`, `EmbeddingPipeline`) and `swap_attached` (calls `EmbeddingPipeline::store`); `core/src/storage/write.rs:write_diff` (398, 652: `graph::containers::detach/attach`).
- **Problem / principle:** wrong dependency direction, which causes the storage<->graph and storage<->embedding cycles. `graph::{imports,symbol_links,containers,queries,traversal,pagination}` and `embedding::{pipeline,backfill,cache}` all import `storage::write`/`vectors`/`schema`/`index_store`, and storage imports them back. In practice `storage` is two layers in one module:
  - (a) rows and transactions: connection, schema DDL, `write::apply_diff`, `vectors`, `file_rows`, the lock;
  - (b) the indexing unit of work: write, then link imports, then link symbols, then store vectors, then do the bookkeeping, all in one hold.

  (b) depends on everything above (a). The embedding module's own doc says it "knows nothing about where those vectors are stored - persistence is `storage`'s job" (`embedding/mod.rs:6-8`). But `EmbeddingPipeline::store` writes `vectors` and `schema::set_embedding_model` on a raw connection (`pipeline.rs:711-728`), and storage calls it back (find_callers `EmbeddingPipeline::store`: `Writer::store_vectors`, `language_swap::swap_attached`, measured).
- **Proposed change:** keep ADR 0001 (one owner of the connection and of hold extents). Move the orchestration up a layer:
  1. New module `core/src/index/` (above storage, graph and embedding, below daemon). It gets the linked operations: `apply_and_link`/`link_applied`, `Writer::apply_file_diff_linked`, `apply_semantic_diff_linked`, `link_all`, `swap_language`, `store_vectors`, and `language_swap` as a whole. They are written as an extension trait or free functions over `storage::index_store::Writer::step`/`IndexStore::unit`, which are already `pub` (outline: `step` exported). `IndexStore` keeps the mutex, `hold`/`in_one_commit`, the `unclaimed` set (expose `claim`/`take_unclaimed` as `pub(crate)`), and `LinkRules` moves out as a parameter.
  2. Container row maintenance (`containers::detach/attach`, called only by `storage::write::write_diff`, find_callers `containers::detach`, measured) is part of keeping one node write consistent. Move it into `storage::containers_rows`. `graph::containers` keeps the read queries (`parent_chain`, `member_files`, `defining_containers`, ...).
  3. Split `EmbeddingPipeline` into `compute` (pure: text to vectors, in `embedding`) and a `storage::vectors::store_batch(conn, &StoredBatch)` that `index` calls. `embedding` then no longer imports `storage::write`, only plain node fields.
- **Benefit:** `storage` becomes a leaf: schema and row changes no longer recompile or retest linking and embedding logic, and the reverse holds too. The linking passes (`symbol_links` is 2174 lines, the area's hottest logic) can change without touching the module that owns the connection's lock discipline. An alternative embedding backend or a no-embedding build becomes a change in `index` only. This also removes 10 of the 12 intra-area back edges (storage->graph 5, storage->embedding 5).
- **Effort/Risk:** L (index_store 1629 lines and language_swap 1530 lines move, plus about 41 `IndexStore` import sites in daemon/mcp/cli per grep). Risk: the lock-order and single-hold invariants in `index_store.rs:10-29` must survive the move. Keep every multi-step write inside one `Writer::step`/`unit`, and keep the `HELD` re-entrancy check in `storage`. Do it in two steps: (2) and the constants (B-opus-1) first, which are mechanical; then (1) and (3).
- **Evidence:** grep `crate::(embedding|graph)` in `storage` (measured, listed above). find_callers `graph::containers::detach` gives only `storage::write::write_diff` (g-mesh, measured). find_callers `EmbeddingPipeline::store` (g-mesh, measured). Module-map section 5 cycles (measured).

## B-opus-5. `storage::schema` is a god module: DDL, generation, and about 60 bookkeeping functions of the language/semantic-pass state machine

- **Location:** `core/src/storage/schema.rs` (2250 non-test lines; DDL 188-750; then `record_language_*`, `begin_language_retry` / `languages_owed_a_retry`, `mark_pending_reindex`, `owe_reextracts` / `settle_owed_reextracts`, `record_semantic_pending_files`, `settle_owed_files` (`MAX_OWED_ATTEMPTS`), `semantic_leftovers`, `reconcile_semantic_pass_rollup`, `set_embedding_model`, `record_noise_purge`, `record_seed_origin`, ...).
- **Problem / principle:** low cohesion, and a missing seam. Each row-level rule of the indexing lifecycle (retries, owed reextracts, semantic residuals, seed state, noise purge) is a free `fn(&Connection, ...)` here. Its policy lives in its callers: about 50 distinct `schema::fn(` names are called from outside storage, including `daemon/workspace_reindex.rs` 34 sites, `daemon/semantic.rs` 22, `embedding/pipeline.rs` 12, `cli/status.rs` 12, `daemon/bulk_index.rs` 11, `watcher/apply.rs` 11 (grep, measured; counts include some inline-test sites). Changing, for example, how semantic-pass state is recorded means editing the 2250-line file that also holds the DDL and `CURRENT_SCHEMA_VERSION`, then hunting the six caller modules. Every caller can also call any step in any order on a raw connection, so the state machine has no single owner.
- **Proposed change:** split by concern into `storage/state/` submodules, each owning its tables:
  - `generation` (apply, ensure_current, reset, versions)
  - `languages` (outcomes, retries, pending reindex, resolution facts, watch paths)
  - `semantic` (pass, pending, residual, owed files, leftovers, rollup)
  - `reextracts`
  - `seed`
  - `embedding_meta` (model, noise purge)

  Keep the DDL in `schema` and `pub use` the moved functions there for one release. As a follow-up, give `semantic` a small typed API (`SemanticState::for_language(conn, lang)` with transition methods) so `daemon::semantic` drives transitions instead of writing columns.
- **Benefit:** a lifecycle-rule change lands in one ~300-line file with its own tests, and a DDL migration does not collide with it. Reviewers can see which daemon module owns which table.
- **Effort/Risk:** M for the split (mechanical, behind re-exports); M+ for the typed state API. Risk: low for the split. The typed API can change transaction boundaries, so keep the `in_savepoint` usage (schema.rs:1614) intact.
- **Evidence:** `grep -nE "pub fn" storage/schema.rs` (measured: 64 pub items, matching module-map). Grep of `schema::[a-z_]+\(` per caller file (measured).

## B-opus-6. `graph::pagination` holds MCP response-size policy, not graph logic

- **Location:** `core/src/graph/pagination.rs:MAX_RESPONSE_BYTES` (45), `OUTLINE_MAX_RESPONSE_BYTES` (55), `wire_len`, `bound_page`, `bound_page_in_response`, `fit_budget[_within]`, `truncate_to_bytes`, `FILE_TALLY_MAX_BYTES`, `EXCLUDED_TALLY_MAX_BYTES`, `tally_is_worth_sending`, `bound_defines_page`.
- **Problem / principle:** leaky abstraction and wrong layer. About half of this 1100-line (non-test) file is serialized-byte budgeting for one client's response cap: the doc at 40-55 cites measured MCP responses, GM-523 and outline sizes. The rest is keyset SQL over `edges`/`nodes` (`paginate_edges`, `paginate_defines`, `count_edges_by_resolution`). The importers are all `mcp/*` plus `cli/plugin_check/expectations.rs` (grep `graph::pagination`, 13 files, measured). Retuning a response budget therefore edits the graph layer, and a schema change to `edges` lands in the same file as response-size tuning.
- **Proposed change:** move the byte-budget items and `FileTally` sizing into `mcp/bound.rs` (a surface concern). Keep in `graph::pagination` (or rename it `graph::keyset`) only cursor encoding, `EdgeRank`/`ScoreCursor` and the SQL page functions returning `Page<T>`. `resume_token` (used only by `mcp/get_dependencies` and `mcp/find_implementations`) can go with the MCP side too.
- **Benefit:** response-size tuning becomes MCP-only, and graph query changes stop touching surface policy. Also prepares for a second surface (CLI JSON, HTTP) with different budgets.
- **Effort/Risk:** S-M (pure move; about 13 importers). Risk: low. The functions are generic over `Serialize`.
- **Evidence:** `grep -n "^pub fn\|^pub const" graph/pagination.rs` (measured). Importers by grep (measured; this is one known module path, so grep rather than g-mesh).

## B-opus-7. Embedding reads global config and env by itself in places, and reports progress through a daemon type

- **Location:** `core/src/embedding/pipeline.rs:CacheSettings::from_global_config` (115-135, called lazily at 605 via `settings.or_else(...)`), `EmbeddingPipeline::load(config: &EmbeddingConfig)` (216); `core/src/embedding/rerank.rs:RerankSettings::from_environment` (319-325); `core/src/embedding/model.rs:127` (`EmbeddingConfig::default().model`); `core/src/embedding/backfill.rs:102, 179` (`run(store, embedding, progress: &IndexingStatus)`, which uses only `set_embed_total` (199) and `add_embed_done` (249)).
- **Problem / principle:** config sprawl and hidden inputs. The model config is injected (`load(&EmbeddingConfig)`), but the cache and rerank settings come from `crate::config::read_global_config()` deep inside the data layer, at first use. Tests have to set `G_MESH_HOME` or env vars, or use the `SettingsSource` box indirection (`rerank.rs:333`), to control them. These reads are the only `embedding -> config` edges (module-map: 1/3, the only B->C edge). The `backfill -> daemon::indexing_status` import is the only `embedding -> daemon` edge, and it is used for two counter calls.
- **Proposed change:** add one `EmbeddingSettings { model, cache: Option<CacheSettings>, rerank: RerankSettings }`, built in `config` (or at the daemon/CLI composition root) and passed to `EmbeddingPipeline::load` and `Reranker::new`. Delete `from_global_config`/`from_environment` from `embedding`. Have `backfill::run` take `&dyn EmbedProgress` (two methods), implemented by `IndexingStatus` in `daemon`.
- **Benefit:** embedding becomes testable and reusable (for example by `cli/embed_eval`) without touching global config, and config-file schema changes stop reaching into `embedding`. Removes the area's only B->C edge and the embedding<->daemon cycle.
- **Effort/Risk:** S-M. Risk: the lazy read at first use exists so a daemon that never embeds never reads the config. Moving the read to startup changes when a broken config file gets logged (behaviour stays defaults-on-error).
- **Evidence:** `pipeline.rs:119`, `rerank.rs:321`, `backfill.rs:102,199,249` (measured, by grep and excerpt).

## B-opus-8. `storage::write::NodeRecord`/`EdgeRecord` is the read model of every surface

- **Location:** `core/src/storage/write.rs:NodeRecord` (56), `EdgeRecord`, `Diff` (281); readers `graph::queries::map_node_row`, `graph::pagination`, `graph::traversal`, `mcp/*`.
- **Problem / principle:** coupling of the read side to the write DTO. `NodeRecord` has 167 references in 34 files, among them 13 `mcp` production files (`overrides`, `find_definition`, `anchor`, `find_callers_callees`, `get_dependencies`, `unlinked`, `untyped`, ...) (g-mesh `find_references NodeRecord answer=files`, measured). Any field added for writing (a new column, an embedding bookkeeping flag) changes the type every query tool consumes, and the reverse holds too. Every query module imports `storage::write`, which is why `graph -> storage` is mostly `write` imports.
- **Proposed change:** move `NodeRecord`, `EdgeRecord`, `DeclarationRecord` and `PlaceholderTargetRecord` to a dependency-free `core/src/model.rs` (or `graph::model`). Leave `Diff` and the write functions in `storage::write`. As a second step, consider a narrower read struct for query tools if write-only fields accumulate.
- **Benefit:** the data model becomes a leaf both layers depend on, so `mcp` stops depending on `storage::write` for types. Changes to write mechanics (`write_diff` is 259 lines, the area's biggest function) no longer reach every MCP handler through imports.
- **Effort/Risk:** S for the move (re-export from `storage::write` keeps callers compiling); M if a separate read type is introduced. Risk: low.
- **Evidence:** g-mesh `find_references symbol_name=NodeRecord answer=files` (measured, 167 refs / 34 files, complete answer).

## B-opus-9. Surfaces and runtime query the schema tables directly

- **Location:** non-test SQL outside `storage`/`graph`: `mcp/overrides.rs` (4 statements, `SELECT * FROM nodes ... JOIN edges`), `mcp/unlinked.rs` (2, joins `placeholder_targets`), `mcp/untyped.rs` (1), `cli/status.rs` (6, including `sqlite_master` probes for `language_outcome`/`language_retry` tables and `meta.schema_version`), `cli/deps.rs` (2), `watcher/apply.rs` (5, including `DELETE FROM edges`), `daemon/bulk_index.rs` (2), `daemon/semantic.rs` (1).
- **Problem / principle:** leaky abstraction. The table layout is a de facto public API. A schema change (renaming a column, splitting `nodes`, moving `vectors`) has to find SQL strings in `mcp`, `cli`, `watcher` and `daemon`, and the compiler cannot help. `cli/status.rs` re-implements schema-generation probing that `schema::stored_generation` owns.
- **Proposed change:** move each statement next to its table owner as a named function: `graph::queries` for node/edge reads (`overrides`, `unlinked`, `untyped`, `deps`), `storage::state::*` (see B-opus-5) for `status` probes, and `storage::write`/`file_rows` for the watcher's edge delete. The callers keep their logic and only call the function. Optionally add a `#[test]` or CI grep forbidding `FROM nodes|FROM edges` outside `storage/` and `graph/`.
- **Benefit:** schema migrations become local to area B, and `mcp`/`cli` can be changed without knowing column names.
- **Effort/Risk:** M (about 23 statements). Risk: low. These are moves of query text with the same parameters; the watcher's delete must stay inside the same `Writer::step`.
- **Evidence:** per-file count of `.prepare(`/`query_row(`/`.execute(` before the first `#[cfg(test)]` (measured; script in the review session). Excerpts `cli/status.rs:487-520`, `watcher/apply.rs:866-877`, `mcp/overrides.rs:113-234` (measured).

## B-opus-10. `languages` (the catalogue) holds index-outcome types and depends on `daemon::manifest`

- **Location:** `core/src/languages.rs:42` (`use crate::daemon::manifest::{extension_of, under_excluded_dir, DiscoveredPlugins}`), `LanguageOutcome` (78), `failed_error` (97), `error_on_one_line` (113); consumer `core/src/storage/schema.rs:6`.
- **Problem / principle:** low cohesion, plus a dependency pointing upward. The module doc defines the catalogue as "what core knows about a language without its plugin". `LanguageOutcome` is what a bulk walk recorded (ADR 0021) and is stored by `storage::schema`. That is what creates the `storage -> languages` edge, and it is not catalogue data. `failed_error` and `error_on_one_line` are the stored-error encoding for that outcome. The catalogue needs only "which extensions and excluded dirs does a discovered plugin claim", yet it imports `daemon::manifest`, which gives the languages<->daemon cycle (module-map: languages->daemon 1/0).
- **Proposed change:** move `LanguageOutcome` and the two error-encoding functions to `storage::state::languages` (see B-opus-5), or to the model module (B-opus-8). Move `extension_of` and `under_excluded_dir` (pure path helpers) into `languages`, or into `project_walk`, and have `missing`/`absent_for_path` take a `&dyn ClaimedLanguages` (or `impl Fn(&str) -> bool`) instead of `&DiscoveredPlugins`. `daemon::manifest` then implements it.
- **Benefit:** the catalogue becomes a leaf, so adding a language entry or changing outcome storage no longer recompiles or couples to daemon discovery. Removes one cycle.
- **Effort/Risk:** S. Risk: low (about 20 importer files of `LanguageOutcome` by grep, re-export possible).
- **Evidence:** `languages.rs:42, 78-115` (measured). Grep of `LanguageOutcome|failed_error|error_on_one_line` per file (measured).

---

## Considered and rejected

- **`IndexStore` as one `Mutex<Connection>` for all reads and writes:** a deliberate, documented decision (ADR 0001, lock-order doc at `index_store.rs:10-29`). It is not a coupling problem; finding 4 keeps it.
- **`graph/symbol_links.rs` at 2174 lines:** large but cohesive (one linking pass). Splitting it would not decouple anything.
- **`embedding::cache` owning its own SQLite file outside `storage`:** it is a machine-wide store with a different lifetime from the project index, so a separate owner is correct. Only its `vectors::pack` import is shared, and that is fine.
- **`storage/mod.rs` and `graph/mod.rs` declaring every submodule `pub` with no facade:** real, but a facade alone would not fix the cycles. Findings 4, 5, 6 and 9 shrink the public surface as a side effect.
- **`embedding` having 102 `pub(crate)` items:** mostly test hooks and internal plumbing inside one module. They do not leak beyond the crate's own layering.
- **`storage::index_store` -> `daemon`/`cli` imports:** inside `#[cfg(test)]` only (`index_store.rs:820-839`), so they are not production coupling.
- **`languages::CATALOGUE` as a static table duplicating `plugin.toml` extensions:** intentional, since it must exist when the plugin is absent. The drift is guarded by `languages/tests.rs`.
- **`paths.rs` (96 lines) being too thin to be a module:** finding 3 gives it its proper content instead of merging it away.
- **`project_walk` vs the SDK walker:** both already delegate to `g_mesh_walk::walk`. Only the excluded-dirs constant is duplicated (finding 2).

## Not decided

- Whether finding 4's new `index` layer should absorb `embedding::backfill` too (it drives `IndexStore::unit` and embedding together), or whether backfill should move to `daemon`. Both are defensible. It depends on whether the owner wants `daemon` thin.
- g-mesh calls used: `select_project`, `get_file_outline(storage/index_store.rs)`, `find_references(NodeRecord)`, `find_references(Diff)` (ambiguous, not needed further), `find_callers(IndexStore::read)`, `find_callers(containers::detach)`, `find_callers(EmbeddingPipeline::store)`. All answers were complete. grep was used for single known constants, per-file SQL counts and `crate::` import edges.
