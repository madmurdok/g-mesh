# GM-708: break `storage<->graph` and `storage<->embedding`, split `storage::schema`, SQL out of surfaces (R-12, R-16, R-18, R-22, R-23, R-32)

Source: `docs/review/4.30/README.md` R-12, R-16, R-18, R-22, R-23, R-32; B-fable-5/6/7/8/9/12.
Read on `main` at `142af2e9` (`release-4.32.0` differs only by the version bump). Line numbers
are that checkout's. Parallel tasks: GM-707 (`docs/design/GM-707-watcher-ingest-startup-phases.md`:
`watcher/apply.rs`+`staleness.rs` -> `ingest/`, `config_reindex` selection -> `ingest/select.rs`,
`mtime_millis` -> leaf `fsutil`), GM-709 (`docs/design/GM-709-mcp-host-trait.md`: `McpHost`,
`ToolContext`, `embed-eval` feature).

## 0. Where we start

- `scripts/module-cycles.py` today: `SCC (3): embedding, graph, storage` and `SCC (3): daemon, mcp,
  watcher`. Edges in the first: `storage -> embedding 5/0`, `storage -> graph 3/0`,
  `graph -> storage 7/1`, `embedding -> storage 6/1` (use/inline counts).
- `storage -> graph`: `storage/write.rs:6` (`graph::containers`, for `detach` at 398 and `attach` at
  652 inside `write_diff`'s transaction), `storage/index_store.rs:41-42` (`LinkRules`,
  `imports::link_diff/link_all`, `symbol_links::link_diff/link_all` at 169, 172, 347, 348).
- `storage -> embedding`: `index_store.rs:39-40` (`ComputedBatch`, `EmbeddingPipeline` in
  `swap_language` 363-368 and `Writer::store_vectors` 549-551), `language_swap.rs:54-56`
  (`plan_embeddings` 382-466 builds `EmbedInput` at 418 and calls `node_text`; `swap`/`swap_attached`
  496/511 take `Option<(&EmbeddingPipeline, &ComputedBatch)>`, `embedding.store(&tx, computed)` at 624-626).
- `graph -> storage` (non-test): record types from `storage::write` in `containers.rs:193`,
  `imports.rs:92`, `symbol_links.rs:300`, `traversal.rs:9`, `queries.rs:4`, `pagination.rs:6`;
  `storage::qualified_path` in `symbol_links.rs:299` and `queries.rs:166`; `write::apply_diff` in
  `queries.rs` `upsert_node` 185, `delete_node` 196, `upsert_edge` 734, `delete_edge` 744 (all their
  callers are test code, section 9).
- `NodeRecord`: 167 references in 34 files; 55 `use ...storage::write::...` lines in 53 files
  (one integration test: `core/tests/it/protocol_conformance.rs:10`).
- `EmbedInput { .. }` literals in non-test code: `embedding/pipeline.rs:862` (`From<&NodeRecord>`),
  `:890` (`current_embeddable_text`), `embedding/backfill.rs:333` (`purge_noise_page`), `:386`
  (`candidate_tier`), `storage/language_swap.rs:418`, `cli/embed_eval.rs:296`,
  `cli/embed_eval/schedule.rs:292`. `mcp/search_code.rs:1062,1078` are inside its test module (705).
- `storage/schema.rs`: 2249 lines; DDL + generation 1-872, state accessors 873-2238 (with `wipe`
  2149-2177 and `record_version` 2239-2247 in between/after). Callers use `schema::x` from 98 files.
- SQL literals outside `storage/`+`graph/` in non-test code: **72** in 19 files (section 5).
- `cli::status` is reached from `daemon/semantic.rs:786,793` and `storage/index_store.rs:835`, all in
  test modules; `mcp/mod.rs:677-679` reads `present_languages_with_semantic_state`,
  `language_outcomes`, `language_retries` itself.

## 1. Split into two tasks? Recommendation: yes

**Recommended:** GM-708 keeps R-12, R-16, R-18 (the cycle break: `storage` stops naming `graph` and
`embedding`). A new task (call it GM-708b) takes R-22, R-23, R-32 (state split, SQL out of surfaces,
health read model), with this note as its design.

- **Benefit:** the halves have different preconditions and different review shapes. Half A needs
  nothing from GM-707/GM-709 and is checked by one tool (`module-cycles.py`); half B's R-23 and
  R-32 edit `ingest/select.rs`, `ingest/apply.rs`, `ingest/staleness.rs` and `fsutil`, which exist
  only after GM-707 merges (section 6 shows R-32 without `fsutil` joins the daemon SCC). One task
  would hold the cycle break hostage to GM-707. Each half's diff is reviewable on its own: half A is
  ~55 import lines + three moves + two seams; half B is a ~1350-line verbatim move + 46 SQL sites.
- **Risk:** one more task/branch; GM-709 S4 then waits for both halves' mcp parts (R-18 imports in
  half A, R-23/R-32 in `mcp/{overrides,unlinked,untyped,search_code,find_definition,mod}.rs` in
  half B), so half B's S6 must not drift behind GM-709 (section 8). R-22 in half B can start at once
  (pure move behind re-exports), so the split costs no wall time.
- **If not split:** the draft order (S2 R-12+R-18, S3 R-16, S4 review, S5 R-22, S6 R-23+R-32, S7
  review, S8 tests, S9 verify) works unchanged; S6 still waits for GM-707.

## 2. Decisions

### D1. R-12: the column list and owned row live in `storage::embed_row`; the text rule stays in `embedding::text`

- **Today:** which `nodes` columns feed the embedded text is restated at four SELECTs
  (`pipeline.rs:883`, `backfill.rs:321,424`, `language_swap.rs:384-392`) and seven `EmbedInput { .. }`
  literals. A column added to the text form and missed at one SELECT makes the swap's "text
  unchanged" check disagree with what `compute` embeds, which keeps a stale vector (inferred).
- **Change:** new `core/src/storage/embed_row.rs`:
  `pub const EMBED_COLUMNS: [&str; 7] = ["kind", "name", "qualifiedName", "filePath", "docComment",
  "signature", "generated"]`, `pub fn embed_columns(alias: &str) -> String` (`"n."` ->
  `"n.kind, n.name, ..."`), owned `pub struct EmbedRow { kind, name, qualified_name, file_path, doc,
  signature, generated }` with `EmbedRow::from_row(&Row, at) -> rusqlite::Result<Self>`,
  `EmbedRow::from_nullable_row(&Row, at) -> rusqlite::Result<Option<Self>>` (`None` when `kind` is
  NULL, for a LEFT JOIN side), `EmbedRow::into_node_record(self, id) -> NodeRecord` (only what
  `compute` reads, language `""`), and `pub fn read(conn, node_id) -> Result<Option<EmbedRow>>`.
  `embedding::text` gains `EmbedInput::from_row(&'a EmbedRow) -> EmbedInput<'a>` and takes the
  `From<&NodeRecord>` impl from `pipeline.rs:860-872`, and re-exports
  `pub(crate) use crate::storage::embed_row::{embed_columns, EmbedRow, EMBED_COLUMNS}` so the
  task's path `embedding::text::{EMBED_COLUMNS, ..}` exists. Non-test `EmbedInput { .. }` literals
  then exist only in `embedding/text.rs` (the two constructors).
- **Example:** `current_embeddable_text` becomes
  `Ok(embed_row::read(conn, node_id).context(..)?.and_then(|row| node_text(&EmbedInput::from_row(&row))))`.
- **Why storage, not `embedding::text`:** after R-16 the swap (in storage) still selects these
  columns on both sides of its join and cannot import `embedding`. Columns are SQL knowledge (storage);
  the rule that turns them into text is embedding's.
- **Why an owned `EmbedRow` plus a borrowed `EmbedInput`:** `EmbedInput<'a>` borrows; it cannot be
  built straight from a `rusqlite::Row`. Keeping `EmbedInput` borrowed keeps `compute`'s
  per-node path allocation-free (`From<&NodeRecord>`).
- **`cli/embed_eval`:** it reads eval *snapshots* whose `nodes` table may predate `generated`
  (schema 15) and are built by its own fixture DDL (`embed_eval.rs` tests). Its SELECT
  (`embed_eval.rs:343`) stays; `Node` gets `fn embed_row(&self) -> EmbedRow` (clones, `generated:
  false`, as today) and both sites call `EmbedInput::from_row(&n.embed_row())`.
- **Consequence:** `purge_noise_page` now reads `generated` instead of forcing `false`; `is_noise`
  and `embed_tier` do not read `generated` (`text.rs:143-199`), so results are identical. The
  candidate query already filters `n.generated = 0`.
- **Alternative:** `EMBED_COLUMNS: &str` (B-fable's wording) - rejected: the swap needs `s.`/`l.`
  prefixes, so a list plus `embed_columns(alias)` is required anyway.
- **Risk:** column order: the swap reads staged columns at 1..=7, `in_live` at 8, live at 9..=15,
  version at 16; `EMBED_COLUMNS` keeps today's order, so the indices do not move.

### D2. R-16(a): record types, `qualified_path`, shared kinds and `container_id` go to a leaf `model`

- **Today:** `NodeRecord`, `EdgeRecord`, `Diff`, `DeclarationRecord`, `PlaceholderTargetRecord`
  (`storage/write.rs:10-296`) live in the module that executes writes; every reader (graph, mcp,
  embedding) imports `storage::write` to name a row.
- **Change:** new top-level `core/src/model/` (`pub mod model;` in `lib.rs`), no SQL statements, no
  `crate::` imports except `protocol::types`:
  `model/records.rs` (write.rs 10-296 verbatim, `normalize_legacy_source` included), re-exported as
  `crate::model::{NodeRecord, EdgeRecord, Diff, DeclarationRecord, PlaceholderTargetRecord}`;
  `model/qualified_path.rs` (`git mv storage/qualified_path.rs`); `model/kinds.rs`
  (`NON_DECLARATION_NATIVE_KINDS` + `declaration_only` from `graph/queries.rs:89-118`, and
  `MODULE_KIND`, `DEFINES_KIND`, `FILE_KIND` from `graph/containers.rs:196-200`); `container_id`
  (`graph/containers.rs:214-237`) in `model/mod.rs`. `graph::queries` keeps
  `pub(crate) use crate::model::kinds::{declaration_only, NON_DECLARATION_NATIVE_KINDS};` (graph ->
  model is fine) so `mcp/overrides.rs`/`mcp/find_definition.rs` paths keep working;
  `graph::containers` keeps `pub use crate::model::container_id;` for its test callers.
- **Every** `use crate::storage::write::{..}` is rewritten: type names to `crate::model::`, function
  names (`apply_diff`, `apply_semantic_diff`, `upsert_indexed_file`) stay. **No** re-export of the
  types from `storage::write`: one name per type.
- **Example:** `use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};` becomes
  `use crate::model::{Diff, EdgeRecord, NodeRecord};` + `use crate::storage::write::apply_diff;`.
- **Consequence:** `mcp`, `graph`, `embedding` stop depending on `storage::write` to name rows; 53
  files get import-only edits (the churn GM-709 S4 rebases over, section 8).
- **Alternative:** keep a `pub use` in `storage::write` and switch only `graph`/`link` - less churn,
  but two paths per type forever and `mcp` still names the write module. Rejected.

### D3. R-16(b): the linking passes become a top-level `link` module; `graph` is the read side

- **Today:** `graph` holds read code (`queries`, `pagination`, `traversal`, `resume_token`,
  read half of `containers`) and write passes (`imports::link_*`, `symbol_links::link_*`) that the
  store calls inside its hold (`index_store.rs:164-175, 344-352`).
- **Change:** `git mv core/src/graph/imports.rs core/src/link/imports.rs`, `graph/symbol_links.rs`
  and `graph/symbol_links/` to `link/`; `core/src/link/mod.rs` declares both; `lib.rs` gains
  `pub mod link;`. `index_store.rs:41-42` become `use crate::link::symbol_links::LinkRules;
  use crate::link::{imports, symbol_links};`; `plugin_manifest.rs:20` the same for `LinkRules`.
  Resulting directions: `storage -> link -> {graph, model}`, `graph -> model`, nothing back.
- **Example:** `IndexStore::link_all` is unchanged except its import line.
- **Why top-level and not `graph::link`:** the acceptance says no `crate::graph` import in
  `storage/`; with `graph::link` the store still imports `crate::graph` (no cycle once graph stops
  importing storage, but the acceptance grep fails). A linker callback (`trait Linker` held by
  `IndexStore`) would satisfy it too, at the cost of every `IndexStore::new` site supplying one
  (`IndexStore::new` defaults `LinkRules`); rejected as more indirection for the same graph.
- **Consequence:** `graph::read` is not created as a submodule: after the move, `graph` *is* the read
  side. R-23's "named functions in `graph::read`" go to `graph::queries` and new `graph::files`,
  `graph::importers`, `graph::search` (D8).
- **Alternative kept for later:** B-opus's `index` layer above storage (both `index_store` and
  `language_swap` move up, storage becomes a pure leaf). Not needed to break the cycle.

### D4. `containers` write half moves into `storage::containers`; read half stays in `graph`

- **Today:** `graph::containers::detach`/`attach` (only caller `storage::write`, 398/652) run inside
  `write_diff`'s transaction; `parent_chain`, `defining_containers`, `defining_files`,
  `member_files`, `own_module_files`, `has_file` are readers (callers: `link::symbol_links`,
  `mcp/get_dependencies.rs`, `cli/deps.rs`).
- **Change:** `graph/containers.rs` 186-555 except what D2 moves (consts, `container_id`) ->
  `core/src/storage/containers.rs`: `CORE_ENGINE`, `Key`, `defines_edge_id`, `membership`,
  `record_membership`, `Pending`, `detach`, `attach`, `ensure_container`, `apply_parents`,
  `delete_container`; `pub(crate) fn detach/attach`. `storage/write.rs:6` becomes
  `use crate::storage::containers;`. The 184-line module doc stays in `graph/containers.rs`;
  `storage/containers.rs` gets a two-line doc pointing to it. Call order inside `write_diff`
  unchanged (detach before deletes, attach after all writes).
- **Consequence:** `write.rs -> graph` goes without a callback (B-opus's choice, R-16).
- **Risk:** `graph/containers/tests.rs` may call write-half privates through `super::`; those tests
  move to `storage/containers.rs`'s test module or import the new path (must confirm, section 7).

### D5. `graph::queries`' four write helpers move to `storage::write`

- **Today:** `upsert_node`, `delete_node`, `upsert_edge`, `delete_edge` (`queries.rs:185-215,
  734-747`) wrap `write::apply_diff`; every caller is test code (section 9: 171 + 1 + 75 + 1 refs, all
  in test modules or `*_tests.rs`/`tests.rs`).
- **Change:** move them verbatim to `storage/write.rs` as `pub fn`; update the 8 test import lines
  (`mcp/get_file_outline.rs:208`, `mcp/unlinked_tests.rs:12`, `mcp/find_callers_callees.rs:855`,
  `mcp/get_dependencies/tests.rs:2`, `mcp/answer_tests.rs:14`, `mcp/find_implementations/tests.rs:2`,
  `mcp/find_references.rs:453`, `mcp/find_definition/tests.rs:4`) and `graph/queries.rs`' own tests.
- **Consequence:** the last non-test `graph -> storage` edge goes.
- **Alternative:** `#[cfg(test)] pub(crate) use` re-export in `graph::queries` (zero test edits) -
  acceptable, but a read module that re-exports writers is what this task removes. Rejected.

### D6. R-16(c): two seams in storage, `VectorWrite` and `NodeText`; embedding implements both

- **Today:** `swap_language`/`swap` take `Option<(&EmbeddingPipeline, &ComputedBatch)>` and only call
  `embedding.store(conn, computed)`; `plan` takes `embedding_version: &str` and calls `node_text`.
- **Change:**
  - `storage::vectors`: `pub trait VectorWrite { fn write(&self, conn: &Connection); }` with
    `impl<F: Fn(&Connection)> VectorWrite for F`. Best-effort like today (`store` logs its own
    errors; no `Result`).
  - `storage::embed_row`: `pub trait NodeText { fn embedding_version(&self) -> &str;
    fn text(&self, row: &EmbedRow) -> Option<String>; }`.
  - `language_swap::plan(staging, live_path, language, text: &dyn NodeText, keep)` (was
    `embedding_version: &str`); `plan_embeddings(tx, text)` calls `text.text(&row)` and
    `text.embedding_version()`. `swap(live, staging_path, vectors: Option<&dyn VectorWrite>, ..)`,
    `swap_attached` the same; line 624-626 becomes `if let Some(vectors) = vectors { vectors.write(&tx); }`.
  - `IndexStore::swap_language(staging, vectors: Option<&dyn VectorWrite>, bookkeeping)`;
    `Writer::store_vectors(&mut self, vectors: &dyn VectorWrite)` =
    `self.step(|conn| vectors.write(conn))`.
  - `embedding`: `EmbeddingPipeline::vector_write<'a>(&'a self, computed: &'a ComputedBatch) ->
    impl VectorWrite + 'a` (`move |conn: &Connection| self.store(conn, computed)`);
    `embedding::text::TextRule<'a> { version: &'a str }` implementing `NodeText` with
    `node_text(&EmbedInput::from_row(row))`; `EmbeddingPipeline::text_rule(&self) -> TextRule<'_>`.
  - Callers: `daemon/workspace_reindex.rs:438-468` (`plan(.., &embedding.text_rule(), ..)`;
    `let write = embedding.vector_write(&computed); let vectors = match kind { Reindex =>
    Some(&write as &dyn VectorWrite), Retry => None };`), `embedding/backfill.rs:248,1032`,
    `watcher/apply.rs:1090` (`ingest/apply.rs` after GM-707) `store.store_vectors(&embedding.vector_write(&computed))`;
    tests: `mcp/semantic_pending_tests.rs:99` and the `language_swap.rs` tests pass
    `&TextRule { version: "model" }` (test code may name `embedding`).
- **Consequence:** `storage` never names `EmbeddingPipeline`, `ComputedBatch`, `node_text` or
  `EmbedInput`; the vector write keeps its place in the swap's transaction (section 4).
- **Alternative:** `Option<&dyn Fn(&Connection)>` without a trait - same effect, but the invariant
  has no named home to document. A plain closure still works at call sites through the blanket impl.

### D7. R-18: the byte budget moves to `mcp::response_bound`; `Direction` to `graph/mod.rs`

- **Today:** `graph/pagination.rs` (2125 lines) mixes keyset SQL with how many JSON bytes an MCP
  answer may carry.
- **Change:** new `core/src/mcp/response_bound.rs` (`pub(crate) mod response_bound;` in
  `mcp/mod.rs`) takes `MAX_RESPONSE_BYTES`, `OUTLINE_MAX_RESPONSE_BYTES`, `longest_prefix_fitting`,
  `bound_page`, `wire_len`, `fit_budget`, `fit_budget_within`, `bound_page_in_response`,
  `widest_total`, `truncate_to_bytes`, `bound_page_within`, `FILE_TALLY_MAX_BYTES`,
  `MAX_EXCLUDED_FILE_TALLY`, `EXCLUDED_TALLY_MAX_BYTES`, `tally_edge_files_bounded`,
  `tally_is_worth_sending`, `bound_defines_page`. Keyset side stays: `DEFAULT_PAGE_SIZE`,
  `MAX_PAGE_SIZE`, `resolve_page_size`, `FILE_KIND`, `EdgeRow`, `Page`, cursors, `EdgeRank`,
  `FileTally`, `MAX_FILE_TALLY`, `tally_edge_files(_limited)`, `count_*`, `paginate_*`.
  Visibility widened to `pub(crate)`: `encode_cursor`, `source_order_cursor`, `MAX_FILE_TALLY`.
  `Direction` (390-396) moves to `graph/mod.rs`; importers switch to `crate::graph::Direction`.
- **Consequence:** `cli/plugin_check/expectations.rs:629` keeps `graph::pagination` for
  `MAX_PAGE_SIZE` (its `tally_is_worth_sending`/`MAX_EXCLUDED_FILE_TALLY` mentions at 517, 587,
  1392, 1433 are comments and string text) and takes `Direction` from `crate::graph`.
- **Alternative:** leave `Direction` in `pagination` with a re-export - rejected, `traversal` and
  `resume_token` need it without paging.

### D8. R-23: SQL moves verbatim into named functions; one shared open; a ratchet check

- **Change:** each site in section 5's table moves, statement text unchanged, into the named
  function there: file-level reads to new `graph::files`, import/placeholder joins to new
  `graph::importers`, vector+node scans to new `graph::search`, node/edge lookups to `graph::queries`,
  row maintenance to `storage::write`/`storage::file_rows`/`storage::vectors`, state to
  `storage::state::*`. `storage::connection::open_existing(db_path) -> Result<Connection>`
  (`READ_WRITE | URI`, no CREATE; today's `cli/status.rs:540-543`) replaces the copies in
  `cli/status.rs:541`, `gc/last_used.rs:118`, `project_detect.rs:220`.
- **Allow-listed, not moved:** `embedding/cache.rs` (19: its own database file and schema, not the
  index) and `cli/embed_eval.rs` (7: reads snapshot copies whose schema may predate the current one,
  and is behind GM-709's `embed-eval` feature).
- **Check:** `scripts/sql-sites.py` (section 5) + `core/tests/scripts/sql_sites_script.rs`.
- **Risk:** a few queries are tuned to their caller (`config_reindex.rs:388` five-table join): moved
  as text, params and row mapping unchanged.

### D9. R-22: `storage::state::{languages, retry, reindex, resolution, semantic, embedding_meta, seed}` behind re-exports in `schema`

- **Today:** `schema.rs` is DDL + generation (1-872, `wipe`, `record_version`) plus ~60 accessors.
- **Change:** move the accessors by line range (section 3.4) into `core/src/storage/state/*.rs`;
  shared private helpers (`present_languages`, `every_present_language_has`, `in_savepoint`) into
  `state/mod.rs` as `pub(super)`. `schema.rs` ends with one block
  `pub use super::state::{embedding_meta::*, languages::*, reindex::*, resolution::*, retry::*,
  seed::*, semantic::*};` so the 98 caller files and `schema/tests.rs` (`use super::*`) compile
  unchanged. DDL stays whole in `schema.rs` (no per-module DDL fragments: every
  `CREATE TABLE IF NOT EXISTS` keeps running on each `apply`, unchanged).
- **Naming vs. the task text:** the task lists `reextracts`; `reindex` holds both `pending_reindex`
  and `owed_reextracts`, and `resolution` (facts + watch paths, B-fable's family) is added.
- **Consequence:** callers migrate from `schema::x` to `state::y::x` opportunistically (R-23 sites
  that move in this task use the new path). B-opus's typed `SemanticState` API is a follow-up.

### D10. R-32: a top-level `index_health` read model; `cli::status` keeps rendering

- **Change:** new `core/src/index_health.rs`: `IndexStatus`, `Mode`, `Coverage` (+`ratio`),
  `LanguageSection`, `language_section`, `language_retries`, `index_status`, `coverage`,
  `semantic_pending`, `semantic_stale`, `semantic_pass_state`, `semantic_pass_lines`, `SourceFile`,
  `discover_source_files` (lines in 3.6) - with their SQL already moved by R-23, so the module holds
  no SQL. `cli::status` re-exports `IndexStatus, Mode, Coverage, LanguageSection` for `status/json.rs`
  and its tests. `daemon::semantic::NOT_RUN_REASON` (580) moves to `storage::state::semantic`
  (it is a value written to `semantic_pass_failure`); `daemon::semantic` keeps a `pub(crate) use`.
  `mtime_millis` comes from GM-707's `fsutil`. New `index_health::language_overview(&Connection) ->
  LanguageOverview { present, outcomes, retries }` (three `Result`s, the three reads of
  `mcp/mod.rs:677-679` in one call); `instructions()` keeps its cold-start branch (no I/O) and its
  per-part fallbacks as they are.
- **`semantic_pass_lines` moves with the data half** (recommended): the daemon tests assert on its
  exact lines (`daemon/semantic.rs:991-1353`), so moving it keeps them byte-identical and only their
  path changes. Alternative: a `SemanticPassLine` enum in `index_health`, wording in `cli` - cleaner
  split, but rewrites ~8 daemon test assertions; offered as an owner choice.
- **Consequence:** `daemon/semantic.rs:786,793` and `storage/index_store.rs:835` import
  `crate::index_health`, not `crate::cli::status`.
- **Risk:** `index_health -> daemon` (via `NOT_RUN_REASON`) or `-> watcher` (via `mtime_millis`)
  would pull it into the daemon SCC (section 6); both are cut by the moves above.

## 3. Edit map

Ranges include each item's doc comment. "Verbatim" = body and comments unchanged, only `use` lines
and paths.

### 3.1 R-12 (S2, sonnet)

| What | From | To |
|---|---|---|
| `EMBED_COLUMNS`, `embed_columns`, `EmbedRow` (+`from_row`, `from_nullable_row`, `into_node_record`), `read` | new | `core/src/storage/embed_row.rs`; `storage/mod.rs`: `pub mod embed_row;` |
| `From<&NodeRecord> for EmbedInput` | `embedding/pipeline.rs:860-872` | `embedding/text.rs` after `EmbedInput` (115) |
| `EmbedInput::from_row(&'a EmbedRow)` + re-export of `embed_columns, EmbedRow, EMBED_COLUMNS` | new | `embedding/text.rs` |
| `current_embeddable_text` body | `pipeline.rs:878-898` | `embed_row::read` + `node_text(&EmbedInput::from_row(&row))`; keep the `.context("failed to read node for its embedding staleness check")` |
| `purge_noise_page` | `backfill.rs:312-348` | SELECT `format!("SELECT n.id, {} FROM vectors v JOIN nodes n ON n.id = v.nodeId WHERE v.nodeId > ?1 ORDER BY v.nodeId LIMIT ?2", embed_columns("n."))`; row `(String, EmbedRow)` via `EmbedRow::from_row(row, 1)`; drop the `generated: false` comment (333-340) |
| `Candidate` / `to_node_record` / `candidate_tier` / candidate query | `backfill.rs:353-397, 418-445` | `Candidate { id, visibility, row: EmbedRow }`; SELECT `n.id, n.visibility, {embed_columns("n.")}`; `to_node_record` = `candidate.row.into_node_record(candidate.id)`; `candidate_tier` = `embed_tier(&EmbedInput::from_row(&candidate.row), &candidate.visibility)` |
| `plan_embeddings` | `language_swap.rs:382-466` | SELECT via `embed_columns("s.")`/`embed_columns("l.")` (same positions); delete `Side`; rows `(String, EmbedRow, bool, Option<EmbedRow>, Option<String>)` (`from_row(row,1)`, `get(8)`, `from_nullable_row(row,9)`, `get(16)`); `text = \|r: &EmbedRow\| node_text(&EmbedInput::from_row(r))`; `to_embed.upsert_nodes.push(staged.into_node_record(id))` |
| `Node::filtered`, schedule tiers | `cli/embed_eval.rs:292-306`, `cli/embed_eval/schedule.rs:288-300` | `Node::embed_row(&self) -> EmbedRow` (clone, `generated: false`); both sites `EmbedInput::from_row(&n.embed_row())` |

Read for context: `embedding/text.rs:98-199`, `embedding/pipeline.rs:700-725` (`store`),
`language_swap.rs:440-466`. Leave `mcp/search_code.rs:1062,1078` (test fixtures).

### 3.2 R-18 (S2, sonnet)

| What | From `graph/pagination.rs` | To |
|---|---|---|
| `MAX_RESPONSE_BYTES`, `OUTLINE_MAX_RESPONSE_BYTES` | 26-55 | `core/src/mcp/response_bound.rs` |
| `longest_prefix_fitting` .. `bound_page_within` | 84-254 | same |
| `FILE_TALLY_MAX_BYTES`, `MAX_EXCLUDED_FILE_TALLY`, `EXCLUDED_TALLY_MAX_BYTES` | 418-440 | same |
| `tally_edge_files_bounded` | 467-489 | same (calls `pagination::tally_edge_files_limited`, `pagination::MAX_FILE_TALLY`) |
| `tally_is_worth_sending` | 630-646 | same |
| `bound_defines_page` | 951-987 | same (calls `pagination::source_order_cursor`) |
| `Direction` | 390-396 | `graph/mod.rs` |
| tests naming moved items | `pagination.rs` tests 1581-1680 (8 `bound_page` tests), 1904 (`a_page_cut_for_bytes_resumes_exactly_after_its_last_kept_row`) + the helpers they alone use | `mcp/response_bound_tests.rs` (exists, `mod` at `mcp/mod.rs:58`) |

Importers to switch (grep, all under `core/src/mcp/` except where noted): `MAX_RESPONSE_BYTES` 9
files, `wire_len` 6, `truncate_to_bytes` 5, `bound_page_in_response` 4, `widest_total` 3,
`tally_edge_files_bounded` 3, `longest_prefix_fitting` 2, `fit_budget` 2, `bound_page`,
`bound_defines_page`, `OUTLINE_MAX_RESPONSE_BYTES` 1 each; `Direction`: `graph/traversal.rs:6`,
`graph/resume_token.rs:10`, `mcp/{find_implementations,get_dependencies,find_references,answer,mod,find_callers_callees}.rs`,
`mcp/find_implementations/tests.rs`, `mcp/get_dependencies/tests.rs`,
`cli/plugin_check/expectations.rs:629`. Fix intra-doc links in `pagination.rs` that name moved
items (`[`bound_page`]` at 652 etc.) to `crate::mcp::response_bound::..` or plain backticks.

### 3.3 R-16 (S3, opus)

Order inside the slice: (1) `model` (D2), compile; (2) `link` (D3) and `storage::containers` (D4),
compile; (3) D5; (4) seams (D6). Import rewrite of step 1 can be scripted: for each `use
crate::storage::write::{..}` line split type names (`NodeRecord|EdgeRecord|Diff|DeclarationRecord|PlaceholderTargetRecord`)
into `use crate::model::{..}`; also `core/tests/it/protocol_conformance.rs:10` (`g_mesh::model`).

| Item | From | To |
|---|---|---|
| record types + `normalize_legacy_source` | `storage/write.rs:10-296` | `model/records.rs` |
| `qualified_path` | `storage/qualified_path.rs` (git mv) | `model/qualified_path.rs`; callers `write.rs:8`, `link/symbol_links.rs:299`, `graph/queries.rs:166`, `mcp/unlinked.rs:28` |
| kinds | `graph/queries.rs:89-118`, `graph/containers.rs:196-200` | `model/kinds.rs` |
| `container_id` | `graph/containers.rs:214-237` | `model/mod.rs` |
| linking passes | `graph/imports.rs`, `graph/symbol_links.rs`, `graph/symbol_links/` | `link/` |
| container row maintenance | `graph/containers.rs:186-555` (less the D2 items) | `storage/containers.rs` |
| `upsert_node`, `delete_node`, `upsert_edge`, `delete_edge` | `graph/queries.rs:185-215, 734-747` | `storage/write.rs` |
| `VectorWrite` | new | `storage/vectors.rs` |
| `NodeText` | new | `storage/embed_row.rs` |
| `vector_write`, `text_rule`, `TextRule` | new | `embedding/pipeline.rs`, `embedding/text.rs` |
| seams applied | `language_swap.rs:54-58,160-176,382,493-512,624-626`; `index_store.rs:39-42,363-372,549-551` | D6 |
| callers | `daemon/workspace_reindex.rs:438-468`, `embedding/backfill.rs:248,1032`, `watcher/apply.rs:1090`, `mcp/semantic_pending_tests.rs:99`, `language_swap.rs` tests | D6 |

Read for context: `storage/write.rs:300-460, 640-660` (`apply_diff`, `write_diff` around
detach/attach), `language_swap.rs:485-630`, `index_store.rs:160-175, 340-372, 545-552`,
`embedding/pipeline.rs:700-725`.

### 3.4 R-22 (half B S2, sonnet)

`schema.rs` keeps 1-872 (`CURRENT_*`, `DDL`, `apply`, `ensure_current`, `stored_generation`,
`reset`), 2149-2177 (`wipe`), 2239-2249 (`record_version`, `mod tests`), plus the re-export block (D9).

| Module | Lines of `schema.rs` |
|---|---|
| `state/mod.rs` | `present_languages` 900-918, `every_present_language_has` 971-1000, `in_savepoint` 1612-1629 (all `pub(super)`); `pub mod` lines for the seven below |
| `state/languages.rs` | 873-899, 919-970, 1001-1132 |
| `state/retry.rs` | 1133-1217 |
| `state/reindex.rs` | 1218-1228, 1315-1409 |
| `state/resolution.rs` | 1229-1314 |
| `state/semantic.rs` | 1410-1611, 1630-2116 |
| `state/embedding_meta.rs` | 2117-2148 |
| `state/seed.rs` | 2178-2238 |

`storage/mod.rs`: `pub mod state;`. `LanguageOutcome` import (`schema.rs:6`) goes to
`state/languages.rs`. Cross-module calls become paths (`super::languages::language_semantic_pass_done`
from `semantic`, `super::languages::bulk_index_completed` from `reindex`). Nothing in 1-872 calls
a moved function (checked: only DDL comments name them); moved code calls none of the kept ones.

### 3.5 R-23 (half B S3, opus; after GM-707)

| Site (main, pre-GM-707 path) | SQL | Destination (verbatim) |
|---|---|---|
| `cli/status.rs:490,498` | outcome-table probe, `schema_version` | `state::languages::has_outcome_table`, `schema::stored_schema_version` |
| `cli/status.rs:522` | retry-table probe | `state::retry::language_retries_if_present` |
| `cli/status.rs:878` | `File` paths | `graph::files::indexed_file_paths` |
| `cli/status.rs:890` | `indexed_files` mtimes | `storage::file_rows::recorded_mtimes` |
| `cli/status.rs:902` | syntax-error files | `graph::files::files_with_syntax_errors` |
| `config_reindex.rs:302,311` (`ingest/select.rs`) | files / count of a language | `graph::files::indexed_file_paths_of`, `graph::files::indexed_file_count` (also serves `daemon/semantic.rs:196`, same text) |
| `config_reindex.rs:337,356,388` (`ingest/select.rs`) | importer joins | `graph::importers::{importers_by_scope, importers_by_key, scan_import_rows}` |
| `config_reindex.rs:523` | `DELETE FROM pending_reindex` | `state::reindex::clear_pending_reindex` |
| `watcher/apply.rs:833` (`ingest/apply.rs`) | `linked_edges` (whole fn + `LinkedEdge`) | `graph::queries` |
| `watcher/apply.rs:867,877` | `sweep_semantic_edges` (whole fn) | `storage::write` |
| `watcher/apply.rs:1107` | `nodes_with_vectors` | `storage::vectors::has_vector` |
| `watcher/staleness.rs:305` (`ingest/staleness.rs`) | `lookup_indexed_file` | `storage::file_rows` |
| `mcp/overrides.rs:115,179,211,234` | type/override lookups | `graph::queries` (4 fns) |
| `mcp/search_code.rs:225,249,543,653` | vector counts and scans | `storage::vectors::count_embedded_nodes`, `graph::search::{..}` |
| `mcp/unlinked.rs:187,215`, `mcp/untyped.rs:85`, `mcp/find_definition.rs:418` | node/placeholder reads | `graph::queries` |
| `cli/plugin_check/session.rs:573,589,800` | node ids/spans by file, edge rows | `graph::queries` |
| `cli/deps.rs:455` / `:610` | import rows / container keys | `graph::importers` / `graph::containers::container_keys` |
| `daemon/registry.rs:239,286` | file languages / known paths | `graph::files` |
| `daemon/bulk_index.rs:289,313` | paths of a language / count by language | `graph::files` |
| `gc/last_used.rs:69,83,129` | `meta.lastUsed`, meta probe | `storage::state::last_used` (new) |
| `embedding/backfill.rs:321,405,424`, `pipeline.rs:883` | owed / purge / candidates / one row | `storage::embed_row` (S2 already made `read`; the two page queries and the count move here) |

Shared open: `storage::connection::open_existing` (D8). Callers of moved `pub(crate)` fns
(`linked_edges`, `sweep_semantic_edges`) update their imports; list them with `find_callers` on the
post-GM-707 tree.

### 3.6 R-32 (half B S3, opus, after 3.5)

| Item | `cli/status.rs` | To |
|---|---|---|
| `IndexStatus`, `Mode`, `Coverage`+impl, `LanguageSection` | 141-241 (types only) | `index_health.rs` |
| `language_section`, `language_retries` | 483-538 | same |
| `open_index` | 540-543 | deleted; `storage::connection::open_existing` |
| `index_status` .. `semantic_pass_lines` | 626-827 | same |
| `SourceFile`, `discover_source_files`, `indexed_file_paths`, `recorded_baselines`, `syntax_error_files` | 829-911 | same (the three SQL fns are already `graph::files`/`file_rows` calls after 3.5) |
| `NOT_RUN_REASON` | `daemon/semantic.rs:580` | `storage::state::semantic`; `pub(crate) use` in `daemon::semantic` |
| `language_overview` | new (reads of `mcp/mod.rs:677-679`) | `index_health.rs`; `mcp/mod.rs` calls it under the same `self.store.read()` |
| test imports | `daemon/semantic.rs:786,793`, `storage/index_store.rs:835,839` | `crate::index_health::..` |

`lib.rs`: `pub mod index_health;`. `cli::status` keeps `collect`, `render`, `json`, the rerank/core/
plugin/build parts, and re-exports the four types.

## 4. The vector transaction invariant

Stated at `index_store.rs:360-362` ("Applies a per-language reindex's plan ... with its vectors and
bookkeeping, in one hold and one transaction") and `language_swap.rs:485-492` (a failure rolls all
of it back). Today the order inside `swap_attached`'s single `tx` is: stale-vector delete (520-525,
deleted nodes and `plan_text_changed`), row deletes, upserts, containers, **`embedding.store(&tx,
computed)` (624-626)**, `language_state` and the rest of the bookkeeping, commit.

`VectorWrite` keeps it because the call site does not move: `swap_attached` still owns `tx` and
calls `vectors.write(&tx)` at the same point; the implementation (`EmbeddingPipeline::store`) gets
the transaction's connection, so its staleness re-read (`current_embeddable_text`, now
`embed_row::read`) sees the swapped rows, and its writes commit or roll back with the swap. What
must not happen (review checks): (1) the seam receiving the live `Connection` instead of `&tx`;
(2) the write moved after `tx.commit()`; (3) `vector_write` doing its own `BEGIN`/savepoint commit.
`NodeText` runs in `plan_attached`'s staging transaction as `node_text` does today.

Test for S5 (control: move the `vectors.write(&tx)` call after `tx.commit()` on `live`, the test
fails): a `VectorWrite` that inserts a vector, plus bookkeeping that fails (e.g. a trigger on
`language_state` raising, as `backfill.rs:805` does for `vectors`) -> after `swap` returns `Err`,
the vector row is absent. Second control (R-12): make `EmbedRow::from_row` read `generated` as
`false`; a swap test where a staged node turns generated must still delete its live vector.

## 5. SQL-in-surfaces baseline and allow-list

**Metric:** string literals starting with an SQL keyword (`"\s*(SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|PRAGMA|WITH|REPLACE|ATTACH|DETACH|BEGIN|SAVEPOINT|RELEASE)\b`)
on non-test, non-comment lines of `core/src/**/*.rs`, outside `storage/`, `graph/`, `link/`,
`model/`. Test code = `module-cycles.py`'s rule (`is_test_file`, `strip_cfg_test`). The statement
verbs `.prepare(`/`.execute(` were rejected as the metric: `mcp/mod.rs` has 8 `self.prepare(&ctx,..)`
false positives.

**Baseline (main 142af2e9): 72 in 19 files.** `embedding/cache.rs` 19, `cli/embed_eval.rs` 7,
`cli/status.rs` 6, `daemon/config_reindex.rs` 6, `watcher/apply.rs` 4, `mcp/overrides.rs` 4,
`mcp/search_code.rs` 4, `embedding/backfill.rs` 3, `cli/plugin_check/session.rs` 3,
`gc/last_used.rs` 3, `mcp/unlinked.rs` 2, `cli/deps.rs` 2, `daemon/registry.rs` 2,
`daemon/bulk_index.rs` 2, `watcher/staleness.rs` 1, `embedding/pipeline.rs` 1,
`mcp/find_definition.rs` 1, `mcp/untyped.rs` 1, `daemon/semantic.rs` 1.
**Projected after R-23: 26** (`embedding/cache.rs` 19, `cli/embed_eval.rs` 7). After GM-707 the
watcher/config_reindex counts sit under `ingest/` with the same totals until R-23 moves them.

**Mechanism:** `scripts/sql-sites.py` (prints `count path` per file and the total; reuses
`module-cycles.py`'s `is_test_file`/`strip_cfg_test` via `importlib`, and masks comments and
string bodies itself, keeping every newline, because `strip_comments_and_strings` does not keep line
alignment) with `--check scripts/sql-sites.allow`. The allow file lists `path count  # reason`. The
check fails when a file is not listed, when a file's count exceeds its line, **and** when it is
below it ("lower the allowance"), so the file only changes by review and the total only goes down.
Test: `core/tests/scripts/sql_sites_script.rs` (runs the check on the real tree, and on a temp tree
to show a new literal and a lowered count both fail), `mod sql_sites_script;` in
`core/tests/scripts/main.rs`. Prototype of the counter used for the baseline:
`/private/tmp/claude-502/-Users-Valentin-Taiurskii-Projects-ClaudeProjects/57f9eeff-ce48-4fef-a935-e2d113711786/scratchpad/sqlsites.py`
(scratch; S3 rewrites it under `scripts/`).

## 6. Projected cycle-checker result

`python3 scripts/module-cycles.py --help`: `--drop A->B` / `--add A->B` edit the computed graph
before Tarjan; `--absent`, `--max-scc` fail on an edge / a component size.

Half A (R-12/16/18) plus half B's new edges:

```
python3 scripts/module-cycles.py \
  --drop 'storage->embedding' --drop 'storage->graph' --drop 'graph->storage' \
  --drop 'plugin_manifest->graph' \
  --add 'storage->link' --add 'storage->model' --add 'graph->model' --add 'link->model' \
  --add 'link->graph' --add 'link->protocol' --add 'model->protocol' \
  --add 'plugin_manifest->link' --add 'embedding->model' --add 'mcp->model' \
  --add 'daemon->model' --add 'watcher->model' --add 'cli->model' --add 'gc->storage' \
  --add 'index_health->storage' --add 'index_health->graph' \
  --add 'index_health->plugin_manifest' --add 'index_health->project_walk' \
  --add 'index_health->fsutil' --add 'cli->index_health' --add 'mcp->index_health' --max-scc 3
SCC (3): daemon, mcp, watcher
exit 0
```

The `embedding, graph, storage` component is gone; the only one left is the pre-existing daemon
SCC (GM-707/GM-709's subject). The same run with `--add 'index_health->watcher'` instead of `fsutil`
prints `SCC (4): daemon, index_health, mcp, watcher`: R-32 must use `fsutil::mtime_millis`
(GM-707 D3) and must not import `daemon::semantic::NOT_RUN_REASON` (D10).

S5 (tests) adds `storage->graph`, `storage->embedding`, `graph->storage` to `REMOVED_EDGES` in
`core/tests/scripts/module_cycles_script.rs:9` (control: put back `use crate::graph::queries;` in
`storage/write.rs`, the test fails). Acceptance greps: `grep -rn "crate::graph\|crate::embedding"
core/src/storage --include=*.rs` shows only test-module lines; `grep -rn EmbeddingPipeline
core/src/storage` shows none outside tests.

## 7. Must confirm (code slices)

- **S2 (R-12+R-18):** `is_noise`/`embed_tier` still do not read `generated` (`text.rs:143-199`);
  `embed_eval` field names used by `Node::embed_row`; which `pagination` test helpers move with the
  9 tests; `EdgeRow.rank`/`EdgeRank.resolved` visibility from `mcp`; rustdoc intra-doc links compile
  (`cargo doc` warnings).
- **S3 (R-16):** `graph/containers/tests.rs` calls into write-half privates (move those tests to
  `storage/containers.rs`); `link/symbol_links` uses only `graph::containers::parent_chain` and
  `graph::queries` items from `graph` (no `link -> storage` non-test edge left); `IndexStore` tests
  that call `swap_language` with a pipeline; `apply_semantic_diff` callers unaffected;
  `module-cycles.py` result matches section 6 after the slice (run it, not the projection).
- **Half B S2 (R-22):** compile proves the cross-module calls; `schema/tests.rs` still sees every
  name through `use super::*`; no behaviour change, so the existing schema tests are the check.
- **Half B S3 (R-23+R-32):** each moved query byte-identical (diff the SQL text old vs new);
  `config_reindex`/`apply`/`staleness` paths as GM-707 left them; whether `gc` may depend on
  `storage` (`gc -> storage` is new; projected acyclic); `project_detect.rs:220` keeps its `.ok()`
  semantics with `open_existing`; `sql-sites.py` total equals the allow file's sum.

## 8. Slicing and merge order

Split (recommended):

| Task | Slice | Content | Model |
|---|---|---|---|
| GM-708 | S2 | R-12 + R-18 (3.1, 3.2) | sonnet |
| GM-708 | S3 | R-16 (3.3) | opus |
| GM-708 | S4 | review against D1-D7 and section 4 | opus |
| GM-708 | S5 | tests: section 4 controls, `REMOVED_EDGES` | opus |
| GM-708 | S6 | verify | opus |
| GM-708b | S2 | R-22 (3.4); may start at once, parallel to GM-708 | sonnet |
| GM-708b | S3 | R-23 + R-32 (3.5, 3.6); after GM-707 merges into the release branch | opus |
| GM-708b | S4 | review | opus |
| GM-708b | S5 | tests: `sql-sites` script test (controls: new literal fails; lowered allowance fails) | opus |
| GM-708b | S6 | verify | opus |

Not split: the draft (S2 R-12+R-18 sonnet, S3 R-16 opus, S4 review, S5 R-22 sonnet, S6 R-23+R-32
opus, S7 review, S8 tests, S9 verify) holds; S5 can run in parallel with S3 in its own worktree
(R-22 changes no caller), S6 starts after GM-707 merges.

Merge order with the parallel tasks:

- **GM-707 before GM-708b S3.** Shared: `watcher/apply.rs` (R-16 changes one line, 1090
  `store_vectors`; R-23 moves `linked_edges`, `sweep_semantic_edges`, `nodes_with_vectors`),
  `watcher/staleness.rs` (R-23 `lookup_indexed_file`; R-32 needs `fsutil`),
  `daemon/config_reindex.rs` -> `ingest/select.rs` (R-23 six sites). GM-708 S3's one-line edit in
  `watcher/apply.rs` rebases trivially whichever lands first.
- **GM-708 (and GM-708b's mcp part) before GM-709 S4**, as GM-709's note already says: shared
  `mcp/mod.rs` (`instructions()` 677-679 for R-32), the handler files (R-18 imports; R-23 SQL in
  `overrides`, `unlinked`, `untyped`, `search_code`, `find_definition`), `cli/embed_eval.rs` /
  `schedule.rs` (R-12; GM-709 S5 puts them behind `embed-eval`, a `cfg` on the module line only).
  GM-709 S2/S3/S5 may run in parallel.

## 9. g-mesh calls behind this note (project `g-mesh`, main checkout)

- `find_references EmbedInput` (complete, 7 files): construction sites; `mcp/search_code.rs` row is
  `tests::a_query_related_to_a_docstring_ranks...` (test).
- `find_references storage::write::NodeRecord` `answer: files`: 167 refs, 34 files.
- `get_dependencies` Incoming on `core/src/graph/symbol_links.rs` (resolved to module
  `g_mesh::graph::symbol_links`): `plugin_manifest.rs`, `storage/index_store.rs`, its tests.
  On `graph/imports.rs`: `storage/index_store.rs` only.
- `find_callers storage::language_swap::plan_embeddings`: `plan_attached` only.
- `find_callers IndexStore::swap_language`: `daemon::workspace_reindex::rebuild` only.
- `find_callers Writer::store_vectors`: `embedding::backfill::run`, a backfill test,
  `watcher::apply::round_trip`.
- `find_callers graph::queries::upsert_node` (171), `delete_node` (1), `upsert_edge` (75),
  `delete_edge` (1), `answer: files`: all in test modules/files (checked first-use line against each
  file's `#[cfg(test)]` line for the four non-`tests.rs` files).
- `find_references watcher::staleness::mtime_millis` `answer: files`: `cli/status.rs`,
  `daemon/build_stamp.rs`, `watcher/staleness.rs` + tests.
- `find_definition watcher::staleness::mtime_millis`: signature and body.
- `get_file_outline` on `embedding/text.rs`, `graph/containers.rs`, `cli/status.rs`.
- grep (not g-mesh) for: `crate::graph`/`crate::embedding` lines in `storage/`, the `use
  ...storage::write::` census, budget-item importers (one known symbol each), `schema::` caller
  files, SQL literal counts, `NOT_RUN_REASON`, `Connection::open*` sites.
- `find_references` for `containers::detach`'s single caller was taken from the review (R-16:
  `find_callers graph::containers::detach`, 1 file) and matched by grep (`storage/write.rs:398,652`).

## 10. Owner decisions (2026-11-16)

- Split: "Разделить на две (Recommended)" - GM-708 keeps R-12, R-16, R-18; R-22, R-23, R-32 are
  GM-723 (called GM-708b above), which depends on GM-707.
- Linking passes: "Новый модуль link (Recommended)" (D3).
- Allow-list: "Разрешить оба (Recommended)" - `embedding/cache.rs` and `cli/embed_eval.rs` stay
  allow-listed (section 5).
- Rest of the design, with `semantic_pass_lines` moving with the data half (D10): "Утвердить (Recommended)".
