use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

/// Bumped whenever the DDL below changes in a way that isn't backward
/// compatible. No migration framework in v1 - a mismatch means a full
/// wipe-and-reindex, since the index is a reproducible cache, not source
/// of truth.
///
/// Bumped to "2" when `indexed_files` was added (query-time staleness
/// checking - see `ingest::staleness`): it records the on-disk mtime/hash
/// a file's index was last built from, which didn't exist in "1".
///
/// Bumped to "3" when `meta.bulkIndexedAt` was added (cold-start bulk index -
/// see `daemon::bulk_index`): the daemon needs to know whether a full walk of
/// the project has ever *finished*, which nothing in "2" recorded.
///
/// Bumped to "4" when `meta.indexer_version` was added - see
/// [`CURRENT_INDEXER_VERSION`] for the failure that column exists to end.
///
/// Bumped to "5" when `declarations` and `edges.toDeclaration` were added -
/// see "Overloads and merged declarations" in docs/architecture/g-mesh-v1.md.
/// A symbol written as several declarations (overload signatures beside their
/// implementation, an interface or namespace merged across statements) used to
/// keep only one of them; the child table is where the rest now live, and the
/// edge column is where a call site's binding to one of them goes.
///
/// Bumped to "6" when `vectors` was added - see `storage::vectors`. It was
/// deliberately left out of every schema before this one (see the comment on
/// `DDL` below); the Embeddings epic is what wires sqlite-vec in, and a
/// version bump is how every existing index picks up the new table the same
/// way it has picked up every table before it - a wipe and a full reindex,
/// not a hand-written migration.
///
/// Bumped to "7" when `meta.semanticPassAt` was added - see
/// `daemon::semantic`'s module doc for the gap it closes: `bulkIndexedAt`
/// alone cannot tell a project whose whole-project semantic pass genuinely
/// finished apart from one whose pass was interrupted after the walk had
/// already been recorded complete. A version bump (rather than an `ALTER
/// TABLE`, which this codebase's "no migration framework" rule already rules
/// out - see this constant's own doc above) is the same reindex-on-mismatch
/// path every other column here has taken; the column is nullable and reset
/// by a wipe exactly like `bulkIndexedAt`, so a fresh index after the bump
/// starts owing both facts, not just the new one.
///
/// Bumped to "8" for GM-264's core graph generalization
/// (docs/architecture/multi-language-plugins.md, "Chosen Approach" > Core
/// graph generalization): `containers`, `placeholder_targets` and
/// `language_state` are new tables, `nodes` gains `container` and
/// `visibility`/`visibilityContainer` (with `exported` becoming a `GENERATED
/// ALWAYS` column derived from `visibility` rather than a value `apply_diff`
/// writes - see that function's own comment on why), and `edges.source`'s
/// CHECK narrows from the two-value v1 pair (`'tree-sitter' | 'ts-compiler'`)
/// to the tier alone (`'syntactic' | 'semantic'`), with the engine that used
/// to be baked into it moving to its own new `engine` column. None of this
/// is expressible as an `ALTER TABLE` this codebase's "no migration
/// framework" rule would accept even if it wanted to - the `exported` column
/// in particular has to be *dropped and recreated* as generated, which
/// SQLite has no `ALTER TABLE` form for at all - so it takes the version
/// bump every schema change here has taken, and every existing index pays
/// the one-time wipe-and-reindex the design doc's Constraints section
/// accepts for this release.
///
/// Bumped to "9" when `language_state.semanticPassError` was added: the
/// reason a language's last whole-project semantic pass failed, which
/// `g-mesh status` shows. Nullable and reset by a wipe like its neighbours.
///
/// "10" adds `nodes.qualifiedPath`, `placeholder_targets.keyPath` and the
/// `qualified_suffixes` table (ADR 0015).
///
/// "11" adds the `untyped_calls` table (GM-486). Without the bump an existing
/// index would keep an empty table until each file was reparsed, and caller
/// pages would silently lack the marker it feeds.
///
/// "12" adds `edges.linkedFrom` and its partial index (GM-491): the
/// placeholder a usage edge was linked from, so a woken placeholder can
/// re-decide - move or unlink - an edge it already linked. An existing index
/// has no record of where its linked edges came from, so it is rebuilt.
///
/// "13" adds the `language_outcome` table (ADR 0021). An empty table reads as
/// "every language covered", so an existing index is rebuilt rather than left
/// claiming that until its next walk.
///
/// "14" adds `edges.specifier` and the `resolution_facts` table. An existing
/// index holds no specifiers on its linked imports and no facts, so it is
/// rebuilt.
///
/// "15" adds `nodes.generated` (GM-646). An existing index has every node of
/// a generated file stored as hand-written, with a vector it must not have,
/// so it is rebuilt.
///
/// "16" adds the `resolution_watch_paths` table. An existing index holds
/// facts without the files whose save changes them, so it is rebuilt.
///
/// "17" adds the `placeholder_also_scopes` table. An existing index holds no
/// extra waiting scopes, so an import waiting at a static fallback would stay
/// unlinked when that file is created; it is rebuilt.
pub const CURRENT_SCHEMA_VERSION: &str = "17";

/// The generation of the extractor+linker whose output an index holds.
///
/// The schema version above answers "can this database still be read?". This
/// one answers the question that turned out to matter far more in practice:
/// "is what is *in* it still what today's code would produce?" The two are
/// independent - fixing a resolver, teaching the extractor a new edge, or
/// changing how core links placeholders all leave the DDL untouched while
/// silently invalidating every row already stored.
///
/// Without this, an index simply never caught up. A project indexed once kept
/// serving that first extraction for as long as its files did not change:
/// the schema still matched, so nothing was wiped, and `bulkIndexedAt` was
/// set, so no walk was owed. The measured case (task 96) was an index built
/// on 2026-07-28, before module specifiers resolved at all, still answering
/// `get_dependencies(..., Incoming)` with `[]` on a build three releases
/// later - while the same query against a fresh index of the same tree
/// returned all 39 importers. Every such answer is a confident,
/// well-formed lie, which is worse than an error.
///
/// So: **bump this whenever a change makes the indexing pipeline produce
/// different nodes or edges for the same source tree** - anything in
/// `graph::imports` / `graph::symbol_links` that alters what gets linked, and
/// anything outside the plugin's own compiled output (a tree-sitter grammar
/// upgrade, say) that alters what is extracted. A mismatch takes the same path
/// a schema mismatch does: wipe, then a full re-walk on the next daemon start.
/// That costs one cold start per project per bump, which is the whole price of
/// never serving a previous generation's graph.
///
/// # This constant is only half of what is compared
///
/// It used to be all of it, and that was the hole task 116 closed. The rule
/// above said to bump it for "anything in `plugins/typescript/src` that alters what
/// is extracted", and task 115 - which rewrote how same-file edges resolve -
/// did not, because nothing made it. Every index built before it went on being
/// served afterwards: current schema, current constant, current binary, wrong
/// answers.
///
/// What is written to `meta.indexer_version` is therefore this constant *plus*
/// a digest of the plugin's compiled output - see
/// `daemon::plugin::indexer_version`, which composes the two, and
/// `daemon::plugin::fingerprint` for why the plugin's half is derived from the
/// artifact rather than declared. This half remains for everything that digest
/// cannot see, which is why it is still worth keeping honest by hand.
///
/// It is deliberately *not* the crate version: releases and extraction
/// changes are different events, and tying the two would both miss changes
/// (a release that fixes nothing about extraction) and force pointless
/// reindexes (a release that only touches the CLI).
///
/// # History
///
/// Bumped to "2" by GM-293 on 2.12.1, and not because the pipeline extracts
/// anything differently from a clean start. Until then the daemon's
/// connection enforced foreign keys without anyone knowing (see
/// `storage::connection::open`), so from the second edit of a file in a
/// plugin process's lifetime on, the diff was refused and the failure
/// swallowed - and the query-time staleness check then recorded the new
/// content hash over the graph that never took the edit. An index that lived
/// through that is wrong in ways nothing will ever notice on its own: its
/// `indexed_files` baselines claim the stale files are fresh, so no future
/// query re-reads them, and a restart trusts it as current. The only repair
/// is the one this constant already buys - a mismatch in [`ensure_current`]
/// drops every table and the next daemon start re-walks the project.
///
/// GM-294 carried the same fix, and the same "2", into 3.0.0. Here the bump
/// is nearly free rather than necessary: [`CURRENT_SCHEMA_VERSION`] "8"
/// already wipes every index a 2.x binary built, whatever its indexer
/// generation. What it still catches is an index built by a *pre-release*
/// 3.0.0 binary - schema "8", indexer "1", the same bug - which the schema
/// check alone would trust as current. Leaving it at "1" would also give two
/// different pipeline generations (2.12.0's buggy writer and 3.0.0's fixed
/// one) the same number in this constant's history, which is exactly the
/// ambiguity it exists to rule out.
///
/// Bumped to "3" by GM-502: the Rust plugin emits a `SUPERTYPE_OF` edge from
/// each trait-impl method to the trait method it implements.
///
/// Bumped to "4" by GM-537: the Rust plugin now also emits both of those
/// edges (type and method level) for a trait that reaches the impl's module
/// only through a glob `use`. An index built before has neither, and nothing
/// else would ever re-extract its files.
pub const CURRENT_INDEXER_VERSION: &str = "4";

/// DDL per the architecture doc's Data Model erDiagram
/// (docs/architecture/g-mesh-v1.md).
const DDL: &str = r#"
-- `visibility`/`visibilityContainer` are the storage mirror of
-- `protocol::types::Visibility` (design doc: Data Model > Visibility).
-- **Two columns, not one packed string** (`'container:<key>'`), because
-- GM-266's linker needs "every node visible from container X" to be a plain
-- indexed equality - `visibilityContainer = ?` - rather than a `LIKE` or a
-- substring split on every candidate row; a single TEXT column would force
-- one or the other. `visibility` alone is the enum (`'public' | 'file' |
-- 'container'`); `visibilityContainer` is NULL except when `visibility =
-- 'container'`, where it holds the key from `Visibility::Container(key)` -
-- the same nullable-unless-relevant shape `edges.toDeclaration` already uses
-- for "this column means nothing until a specific state applies". The
-- container-scoped check itself (is the requester's own container `key` or a
-- descendant of it, via `containers.parentKey`) still needs a parent-chain
-- walk no single `WHERE` clause can express, and that walk is GM-266's to
-- write - this column only has to make the equality half of it cheap.
--
-- DEFAULT 'file' - not 'public' - so a row written by any of this schema's
-- own tests or by a future writer that has no opinion on visibility lands
-- exactly where the old `exported INTEGER NOT NULL DEFAULT 0` default did:
-- unexported. See `exported`'s own comment just below for how the two stay
-- identical in meaning.
--
-- `container` (bottom of the table) is a different fact from
-- `visibilityContainer`: it is the language-defined unit this *declaration*
-- is a member of (Data Model > Logical containers - a Go import path, a Rust
-- module path, ...), while `visibilityContainer` is who may *see* it, and the
-- two disagree exactly for `pub(crate)`/`pub(super)` (visible from an
-- ancestor container, declared in a descendant one). The column is the
-- source of membership: `graph::containers` reads it inside `apply_diff` to
-- materialize container nodes, `containers` rows and `DEFINES` edges, and to
-- GC a container whose last member goes.
CREATE TABLE IF NOT EXISTS nodes (
    id                   TEXT PRIMARY KEY,
    kind                 TEXT NOT NULL,
    name                 TEXT NOT NULL,
    qualifiedName        TEXT NOT NULL,
    filePath             TEXT NOT NULL,
    startLine            INTEGER NOT NULL,
    startCol             INTEGER NOT NULL,
    endLine              INTEGER NOT NULL,
    endCol               INTEGER NOT NULL,
    signature            TEXT,
    visibility           TEXT NOT NULL DEFAULT 'file' CHECK (visibility IN ('public', 'file', 'container')),
    visibilityContainer  TEXT,
    -- A `GENERATED ALWAYS ... STORED` column, not a value `apply_diff`
    -- writes: v1's `exported INTEGER NOT NULL DEFAULT 0` used to be set by
    -- hand on every upsert, which is exactly the kind of "two places have to
    -- agree" a future writer forgets - see `storage::write::apply_diff`'s own
    -- comment on why it no longer accepts this column at all. Deriving it in
    -- SQLite instead means every reader (`get_file_outline` chief among them
    -- - see the design doc's Data Model > Visibility: "exported stays as a
    -- *derived storage column*, so get_file_outline's output does not
    -- change") gets the same byte-identical `SELECT * FROM nodes` it always
    -- has, and there is no code path left that *can* disagree with
    -- `visibility` about whether a node is public.
    exported             INTEGER NOT NULL GENERATED ALWAYS AS (CASE visibility WHEN 'public' THEN 1 ELSE 0 END) STORED,
    docComment           TEXT,
    language             TEXT NOT NULL,
    nativeKind           TEXT,
    hasSyntaxErrors      INTEGER NOT NULL DEFAULT 0,
    -- 1 when the node's file carries a generated-code header (GM-646): the
    -- node keeps every structural answer and gets no vector.
    generated            INTEGER NOT NULL DEFAULT 0,
    container            TEXT,
    -- The plugin's segments of `qualifiedName`, encoded as name0, then each
    -- later segment's separator and name, all joined by U+001F
    -- (`storage::qualified_path`). NULL when the plugin sent no valid path.
    -- Joined without the U+001F it equals `qualifiedName`.
    qualifiedPath        TEXT
);

CREATE INDEX IF NOT EXISTS idx_nodes_filePath ON nodes(filePath);
CREATE INDEX IF NOT EXISTS idx_nodes_qualifiedName ON nodes(qualifiedName);
-- `(language, container)`, matching `containers`' own `UNIQUE (language,
-- key)` below: a container key is only unique *within* a language (Data
-- Model > Logical containers table - a Go import path and a Rust module path
-- can collide as bare strings), so every lookup of "this container's
-- members" has to filter on both columns together, never `container` alone.
CREATE INDEX IF NOT EXISTS idx_nodes_container ON nodes(language, container);
-- Placeholders by bare name, for `find_callers`/`find_references` to find the
-- unlinked usages that may belong to a declaration (`mcp::unlinked`). Partial,
-- so it costs only placeholder rows; a query uses it only when it spells
-- `nativeKind = 'pending_symbol'` as this literal, not as a bound parameter.
CREATE INDEX IF NOT EXISTS idx_nodes_pending_name ON nodes(name) WHERE nativeKind = 'pending_symbol';

-- One row per declaration of a symbol that has more than one - overload
-- signatures beside their implementation, an interface or a namespace written
-- across several statements. A symbol with a single declaration (nearly every
-- symbol in a real file) has *no* rows here: the node's own flat columns
-- already say everything about it, and costing the ordinary case nothing is
-- the whole point of hanging this off to the side instead of widening `nodes`.
--
-- `ordinal` is source order from 0, and it is an identity rather than a
-- presentation detail: it is what `edges.toDeclaration` names when the
-- semantic pass says which overload a call site bound.
--
-- Written only through `storage::write::apply_diff`, which replaces a node's
-- whole set on every upsert of it - an overload list is a fact about the
-- symbol as it is written now, never an accumulation across edits.
CREATE TABLE IF NOT EXISTS declarations (
    nodeId    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    ordinal   INTEGER NOT NULL,
    startLine INTEGER NOT NULL,
    startCol  INTEGER NOT NULL,
    endLine   INTEGER NOT NULL,
    endCol    INTEGER NOT NULL,
    signature TEXT,
    hasBody   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (nodeId, ordinal)
);

-- toDeclaration: which of the target's declarations this edge binds, as an
-- ordinal into `declarations`. NULL for every edge that binds no particular
-- one, which is every edge the structural pass produces and every edge whose
-- target has a single declaration - i.e. almost all of them. Set only on
-- CALLS, only by the semantic pass, and part of the edge's own identity (see
-- `edge_id` in plugins/sdk/src/ids.rs), so one caller calling two
-- overloads of the same function stores both bindings instead of one
-- overwriting the other.
-- `source` was the two-value pair `'tree-sitter' | 'ts-compiler'` through
-- schema "7" - v1's *tier* and its one bundled engine conflated into a single
-- CHECK (design doc: Data Model > Edge source). Splitting them is what lets a
-- Go or Rust plugin report its own engine (`go-types`, `rust-analyzer`, ...)
-- with no further schema change: `source` narrows to the closed tier alone
-- (queries and code branch on this, and only this), and `engine` is the free
-- label everything else moves into - diagnostic only, never matched against
-- in a `WHERE` clause the way `source` is. Migration is moot in practice
-- (`tree-sitter` -> `('syntactic', 'tree-sitter')`, `ts-compiler` ->
-- `('semantic', 'ts-compiler')`), because the schema version bump this table
-- change is part of wipes and reindexes every existing project regardless -
-- see `CURRENT_SCHEMA_VERSION`'s own comment.
CREATE TABLE IF NOT EXISTS edges (
    id            TEXT PRIMARY KEY,
    fromId        TEXT NOT NULL REFERENCES nodes(id),
    toId          TEXT NOT NULL REFERENCES nodes(id),
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL CHECK (source IN ('syntactic', 'semantic')),
    engine        TEXT NOT NULL,
    resolved      INTEGER NOT NULL DEFAULT 0,
    toDeclaration INTEGER,
    linkedFrom    TEXT,
    specifier     TEXT
);

CREATE INDEX IF NOT EXISTS idx_edges_fromId ON edges(fromId);
CREATE INDEX IF NOT EXISTS idx_edges_toId ON edges(toId);
-- `linkedFrom` (GM-491): the pending-symbol placeholder `graph::symbol_links`
-- moved this usage edge off, NULL for an edge it never linked (and reset to
-- NULL whenever a plugin re-sends the edge, which then says what the plugin
-- says now). Without it a repoint overwrites the only link back, and a
-- placeholder woken by a new provider could not reopen the edge it already
-- linked. No foreign key: a placeholder and its usage edges belong to one
-- importer file and leave together, and a stale value is never looked up -
-- no pending set can name a deleted placeholder. Partial, because most edges
-- were never linked.
CREATE INDEX IF NOT EXISTS idx_edges_linkedFrom ON edges(linkedFrom) WHERE linkedFrom IS NOT NULL;

-- One row per logical container actually present in the index - a Go
-- package, a Rust module, ... (design doc: Data Model > Logical containers).
-- `nodeId` is the container's own node (an ordinary `Module` row with
-- `nativeKind = 'container'`, `filePath = ''` - see
-- `graph::containers::container_id` for the id scheme), not a member; a
-- member points *at* its container through `nodes.container` above, a plain
-- key string, not a foreign key onto this table - a member is written before
-- its container exists, by a plugin diff that never names the container.
--
-- Written only by `graph::containers`, inside `apply_diff`'s transaction
-- (GM-265): a row appears with a container's first member, `memberCount` is
-- recounted from the container's `DEFINES` edges whenever a diff touches it,
-- `parentKey` comes from the members' `containerParent`, and the row goes -
-- explicitly, since the daemon runs with foreign keys off
-- (`storage::connection::open` switches them off; the bundled SQLite would
-- enforce them otherwise, GM-292) and the `ON DELETE CASCADE` below never
-- fires there - when the count reaches zero. That
-- module's doc has the reasoning for each of those choices.
CREATE TABLE IF NOT EXISTS containers (
    nodeId      TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    language    TEXT NOT NULL,
    key         TEXT NOT NULL,
    parentKey   TEXT,
    memberCount INTEGER NOT NULL,
    UNIQUE (language, key)
);

-- What a placeholder node (`nativeKind` `pending_symbol` | `reexport` |
-- `resolved_module`) is waiting on - the storage mirror of
-- `protocol::types::PlaceholderTarget`, and the row form of what used to be
-- packed into a placeholder's own `qualifiedName` as a `<file>#<name>`
-- string (design doc: Data Model > Structured placeholder targets). This row
-- is the only address either linker reads: `graph::symbol_links` since GM-266
-- (its module doc states the linker contract over these columns) and
-- `graph::imports` since GM-267 (which reads `(scopeKind, scope)` only). GM-264
-- introduced the table while both still parsed `qualifiedName`. `nodeId` is
-- 1:0-or-1 with `nodes` - a placeholder has exactly one target, an ordinary
-- declaration has none - the same shape `storage::vectors` already uses for
-- "this node has at most one of these".
--
-- `scopeKind`/`scope` and `keyKind`/`key` are two independent two-valued
-- facts, not one four-valued column, because the linker contract (design
-- doc: Interfaces > Linker contract) branches on them independently: a
-- `name` key can be scoped to either a `file` or a `container`, and so can a
-- `qualifiedName` key - collapsing the pair into one enum would just move the
-- branching into a string convention this table exists to get away from.
--
-- `fromContainer` is nullable (only a requester with a container has one -
-- every placeholder from a language with no containers, TS included, leaves
-- it NULL) and `fromFile` is not (every placeholder has a requesting file,
-- since `file`-scoped visibility has to be checkable regardless of whether
-- the *target* is container-scoped). `fromFile` is not part of the wire's
-- `PlaceholderTarget` struct at all - `apply_diff` fills it from the
-- placeholder node's own `filePath`, which is already the requester's file
-- by the existing convention (a placeholder's `filePath` is the *importing*
-- file - `imported_symbol` in plugins/typescript/src/extractor/imports.rs).
CREATE TABLE IF NOT EXISTS placeholder_targets (
    nodeId        TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    scopeKind     TEXT NOT NULL CHECK (scopeKind IN ('file', 'container')),
    scope         TEXT NOT NULL,
    keyKind       TEXT NOT NULL CHECK (keyKind IN ('name', 'qualifiedName')),
    key           TEXT NOT NULL,
    fromContainer TEXT,
    fromFile      TEXT NOT NULL,
    -- The segments of a `qualifiedName` key, encoded like `nodes.qualifiedPath`.
    -- NULL for a `name` key and for a plugin that sends no paths.
    keyPath       TEXT
);

-- The shape every linker lookup needs: "placeholders waiting on (this scope
-- kind, this scope, this key)" - `graph::symbol_links::link_diff`'s triggers
-- for a new declaration, re-export or container member, and its re-export
-- walk-back, all probe this index (for a file and a container scope alike).
-- `graph::imports` (GM-267) is a narrower reader of the same shape: it never
-- reads `key`, only `(scopeKind, scope)` - its own "new file"/"new container
-- member" triggers - so it uses this index's leading columns.
CREATE INDEX IF NOT EXISTS idx_targets_scope ON placeholder_targets(scopeKind, scope, key);

-- The further file paths a `file`-scoped placeholder waits at besides its
-- `placeholder_targets.scope` (the wire's `PlaceholderTarget::also_scopes`):
-- the "who waits at this path" reads (`file_rows::waiting_importers`,
-- `config_reindex::indexed_importers`) select its importer for each, while
-- linking stays keyed by the primary scope alone. A placeholder with none has
-- no rows.
--
-- Written only by `storage::write::apply_diff`, which replaces a node's set
-- together with its `placeholder_targets` row. Foreign keys are off on the
-- daemon's connection, so every site that deletes or replaces a
-- `placeholder_targets` row does the same here, or a later node with the same
-- id inherits the orphan rows.
CREATE TABLE IF NOT EXISTS placeholder_also_scopes (
    nodeId TEXT NOT NULL,
    scope  TEXT NOT NULL,
    PRIMARY KEY (nodeId, scope)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_also_scopes_scope ON placeholder_also_scopes(scope);

-- Every spelling by which a partial path finds a declaration: each suffix of
-- its `qualifiedPath` that starts at segment 1 or later and keeps at least
-- two segments, and each suffix of each of its alias paths that starts at
-- segment 0 or later and keeps at least two segments, as display text
-- (segment names joined by the path's own separators). The whole primary
-- path is left out: it is `nodes.qualifiedName`. Matched by `suffix = ?`
-- only, never by LIKE. A node with no path has no rows.
--
-- Written only by `storage::write::apply_diff`, which replaces a node's whole
-- set on every upsert of it, and copied by `storage::language_swap`. Foreign
-- keys are off on the daemon's connection, so every site that deletes a node
-- deletes its rows here too; `idx_qualified_suffixes_nodeId` serves those
-- deletes. See ADR 0015.
CREATE TABLE IF NOT EXISTS qualified_suffixes (
    suffix TEXT NOT NULL,
    nodeId TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY (suffix, nodeId)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_qualified_suffixes_nodeId ON qualified_suffixes(nodeId);

-- Bare names of methods a node calls through a receiver whose type the
-- structural tier did not know (wire `untypedCalls`, GM-486). Lets a caller or
-- reference page say it may be missing such calls to a method of that name.
-- Only from a plugin that reports them; no rows means "not reported".
--
-- Written only by `storage::write::apply_diff`, which replaces a node's whole
-- set on every upsert of it, and copied by `storage::language_swap`. Foreign
-- keys are off on the daemon's connection, so every site that deletes a node
-- deletes its rows here too; `idx_untyped_calls_nodeId` serves those deletes,
-- the primary key serves the `name = ?` lookup.
CREATE TABLE IF NOT EXISTS untyped_calls (
    name   TEXT NOT NULL,
    nodeId TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    PRIMARY KEY (name, nodeId)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_untyped_calls_nodeId ON untyped_calls(nodeId);

-- Per-language index state - the roll-up `meta.bulkIndexedAt`/
-- `semanticPassAt` are built from (design doc: Data Model > Per-language
-- index state). One row per language `storage::schema::record_language_
-- bulk_indexed`/`record_language_semantic_pass` has ever recorded a fact
-- for - not one row per language *discovered*, so a plugin that was
-- discovered but never actually walked (nothing routes to it yet) leaves no
-- row rather than a row of NULLs indistinguishable from "walked, pass owed".
--
-- `bulkIndexedAt`/`semanticPassAt` are this table's own per-language mirror
-- of `meta`'s project-wide columns of the same name, and follow the same
-- rule: NULL until that language's whole-project fact has completed once,
-- independent of every other language's. `pluginFingerprint` is
-- `daemon::plugin::fingerprint`'s digest of the plugin build that produced
-- the *most recent* `bulkIndexedAt` for this language, when that digest was
-- readily available at the write site (`daemon::bulk_index::walk_one_
-- language`, which already holds the manifest); every other writer of this
-- table (the semantic-pass call sites, which only ever touch the row a bulk
-- index already created) leaves it untouched. It is not yet read by
-- anything - `daemon::plugin::fingerprint` is already folded into `meta.
-- indexer_version` via `daemon::registry::indexer_version`, which is what
-- actually triggers a reindex today - but it is the natural place a future
-- per-language staleness check would look, so it is captured now rather than
-- thrown away at the one site that has it for free.
--
-- `semanticPassError` is why this language's most recent whole-project
-- semantic pass failed, NULL when it has not failed since its last success:
-- a completed pass clears it, a failed one overwrites it.
CREATE TABLE IF NOT EXISTS language_state (
    language          TEXT PRIMARY KEY,
    bulkIndexedAt     TEXT,
    semanticPassAt    TEXT,
    pluginFingerprint TEXT,
    semanticPassError TEXT
);

-- One row per language the last bulk walk had an outcome for (ADR 0021):
-- every discovered language, plus every catalogue language without a plugin
-- that has files in the project. Rewritten whole by each walk
-- (`record_language_outcomes`); incremental updates leave it alone. An
-- `indexed` row stores no count: `language_outcomes` reads it live from the
-- `File` nodes.
CREATE TABLE IF NOT EXISTS language_outcome (
    language   TEXT PRIMARY KEY,
    outcome    TEXT NOT NULL CHECK (outcome IN ('indexed', 'plugin_absent', 'failed')),
    files      INTEGER, -- plugin_absent only; NULL = not counted
    error      TEXT,    -- failed only
    recordedAt TEXT NOT NULL
);

-- Automatic retries of a language recorded `failed` in language_outcome,
-- counted since the last full walk. No row = no retry yet. Cleared by every
-- full walk (`record_language_outcomes`), deleted by a successful retry's
-- swap, dropped by `wipe`. Added by `CREATE TABLE IF NOT EXISTS`, so an
-- existing index gains it without a schema bump.
CREATE TABLE IF NOT EXISTS language_retry (
    language    TEXT PRIMARY KEY,
    retries     INTEGER NOT NULL,
    lastRetryAt TEXT NOT NULL
);

-- bulkIndexedAt is NULL until a full project walk has completed at least
-- once (see daemon::bulk_index). Deliberately not derived from "are there
-- any nodes?": a walk interrupted half way also leaves nodes behind, and
-- resuming from a partial index as if it were complete is the exact failure
-- this column exists to rule out.
--
-- semanticPassAt is the same idea for the whole-project semantic pass that
-- follows a walk (see daemon::semantic): NULL until that pass has completed
-- at least once, independently of bulkIndexedAt. The two are deliberately
-- separate facts rather than one - every caller that builds an index records
-- bulkIndexedAt *before* asking for the pass (see daemon::semantic's module
-- doc for why), so a pass interrupted afterwards (a killed plugin, a crash, a
-- timeout) would otherwise leave a project that calls itself fully indexed
-- with no semantic layer and nothing left to notice. semanticPassAt is what
-- notices: unset here after a project's walk is done means the pass is still
-- owed, and daemon::mod's cold start (and cli::init's idempotent rerun) read
-- exactly that to retry it without repeating the walk.
CREATE TABLE IF NOT EXISTS meta (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    schema_version  TEXT NOT NULL,
    indexer_version TEXT NOT NULL,
    embedding_model TEXT,
    lastUsed        TEXT NOT NULL,
    bulkIndexedAt   TEXT,
    semanticPassAt  TEXT
);

-- Baseline on-disk state (mtime + content hash) a file's index was last
-- built from - see ingest::staleness. mtime is a cheap fast-path check
-- (no file read); contentHash is the authoritative fallback used only when
-- mtime disagrees, so a query touching many files doesn't have to hash all
-- of them every time.
CREATE TABLE IF NOT EXISTS indexed_files (
    filePath    TEXT PRIMARY KEY,
    mtimeMillis INTEGER NOT NULL,
    contentHash TEXT NOT NULL
);

-- One row per node that has been embedded. The relation is 1:0-or-1 (see the
-- architecture doc's Data Model erDiagram: `NODES ||--o| VECTORS`), so
-- nodeId is the primary key rather than an ordinary indexed FK column - a
-- re-embed overwrites the row instead of accumulating one. A node going away
-- (a file edit, a deletion) must drop its stale embedding with it instead of
-- leaving an orphan a later similarity search could still surface. ON DELETE
-- CASCADE states that intent, but it only runs on a connection that enforces
-- foreign keys, and the daemon's does not (`storage::connection::open`
-- switches them off, GM-293/GM-294 for why) - so every production delete from
-- `nodes` removes the vector itself (`storage::write::apply_diff`,
-- `graph::imports`' placeholder drop, `graph::containers`,
-- `daemon::workspace_reindex`), exactly as it does for `declarations` and
-- `placeholder_targets`.
--
-- embedding is sqlite-vec's compact vector format: each dimension as a
-- 4-byte little-endian float32, back to back, no header - see
-- `storage::vectors::pack`. embeddingVersion names the model (+ version)
-- that produced *this row*, which is not the same fact as `meta`'s
-- `embedding_model` (the project's current choice): the two only agree once
-- every row has been re-embedded after a model switch. Carrying it per row
-- from the start means a future model switch costs a background re-embed,
-- not a schema migration - see REQUIREMENTS.md's "Инвалидация эмбеддингов
-- при смене embedding-модели".
CREATE TABLE IF NOT EXISTS vectors (
    nodeId          TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    embedding       BLOB NOT NULL,
    embeddingVersion TEXT NOT NULL
);

-- One row per language whose workspace reindex has started and not yet
-- swapped in (daemon::workspace_reindex). Removed in the swap's own
-- transaction, so a row outliving its daemon means the reindex was
-- interrupted: the next start runs it again. Added by `CREATE TABLE IF NOT
-- EXISTS`, so an existing index gains it without a schema-version bump.
CREATE TABLE IF NOT EXISTS pending_reindex (
    language  TEXT PRIMARY KEY,
    trigger   TEXT NOT NULL,
    startedAt TEXT NOT NULL
);

-- The files of `language` a source edit's `affected` answer selected for a
-- re-extract (GM-507, `ingest::apply`), written before the first of them is
-- re-extracted and removed once all of them and their semantic pass are done.
-- A row outliving its daemon means the loop was interrupted: the next start
-- re-extracts them again (`daemon::config_reindex::resume_owed_reextracts`).
-- `trigger` is the edited file, or a deleted one whose importers are owed
-- (`storage::index_store::Writer::apply_file_diff_linked`, in the delete's
-- commit). A workspace reindex swap removes the
-- language's rows. Added by `CREATE TABLE IF NOT EXISTS`, so an existing
-- index gains it without a schema-version bump.
CREATE TABLE IF NOT EXISTS reextract_owed_files (
    language TEXT NOT NULL,
    filePath TEXT NOT NULL,
    trigger  TEXT NOT NULL,
    PRIMARY KEY (language, filePath)
);

-- One row per language whose whole-project semantic pass is owed after a
-- workspace reindex swapped it in, and has neither completed nor recorded a
-- failure since (ADR 0009). Written in the swap's own transaction; removed by
-- `record_language_semantic_pass` and `record_language_semantic_pass_failure`
-- in theirs. `since` is the swap's commit time, RFC 3339 UTC. Added by
-- `CREATE TABLE IF NOT EXISTS`, so an existing index gains it without a
-- schema-version bump.
CREATE TABLE IF NOT EXISTS semantic_pending (
    language TEXT PRIMARY KEY,
    since    TEXT NOT NULL
);

-- The files of `language` whose edges that pass has not refreshed yet. A row
-- here without its language's `semantic_pending` row is never read.
CREATE TABLE IF NOT EXISTS semantic_pending_files (
    language TEXT NOT NULL,
    filePath TEXT NOT NULL,
    PRIMARY KEY (language, filePath)
);

-- The files of `language` a semantic pass named in its `unfinishedFiles`,
-- with how many passes have tried them. Core puts them into the scope of the
-- language's next per-file pass, so a file a cold server left unanswered is
-- asked again without waiting for an edit; a residual language's next start
-- asks exactly these (`semantic_residual`). Written and settled by
-- `settle_owed_files` (also via `record_language_semantic_residual`); removed by
-- `record_language_semantic_pass` (a complete whole-project pass answered
-- everything) and by a workspace reindex swap. Added by
-- `CREATE TABLE IF NOT EXISTS`, so an existing index gains it without a
-- schema-version bump.
CREATE TABLE IF NOT EXISTS semantic_owed_files (
    language TEXT NOT NULL,
    filePath TEXT NOT NULL,
    attempts INTEGER NOT NULL,
    PRIMARY KEY (language, filePath)
);

-- The opaque resolution facts a `resolution_delta` plugin reported with the
-- rows this index holds for `language` (daemon::config_reindex): written by
-- its bulk walk's trailer, swapped in with the language's rows, and replaced
-- once a `resolutionChanged` answer has been acted on. No row: the next
-- watch-file edit reindexes the whole language.
CREATE TABLE IF NOT EXISTS resolution_facts (
    language TEXT PRIMARY KEY,
    facts    TEXT NOT NULL
);

-- The files beyond the manifest's static `watch_files` whose save changes
-- the model `resolution_facts` describes (a plugin's `watchPaths`), one row
-- per (path, language). The rows live and die with the language's facts row:
-- every write of the facts replaces them, a deletion of the facts deletes
-- them, and the language swap copies them with the facts. `path` is
-- project-relative, lowercased when the project's volume is case-insensitive
-- (`daemon::path_case`).
CREATE TABLE IF NOT EXISTS resolution_watch_paths (
    path     TEXT NOT NULL,
    language TEXT NOT NULL,
    PRIMARY KEY (path, language)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_resolution_watch_paths_language
    ON resolution_watch_paths(language);

-- One row per language whose last whole-project semantic pass was incomplete
-- and named the files it did not finish (GM-521): those files are its
-- `semantic_owed_files` rows, and the next start asks only them instead of
-- the whole project. Written by `record_language_semantic_residual`; removed
-- when the language's pass is recorded (`record_language_semantic_pass`,
-- `record_language_semantic_pass_settled`) and by a workspace reindex swap.
-- `since` is RFC 3339 UTC. Added by `CREATE TABLE IF NOT EXISTS`, so an
-- existing index gains it without a schema-version bump.
CREATE TABLE IF NOT EXISTS semantic_residual (
    language TEXT PRIMARY KEY,
    since    TEXT NOT NULL,
    reason   TEXT NOT NULL
);

-- The files of `language` that used up `MAX_OWED_ATTEMPTS` passes without
-- being answered: no longer asked until they change, and counted by
-- `g-mesh status` as never answered. A file is in at most one of
-- `semantic_owed_files` and this table. Written by `settle_owed_files` when
-- it drops a file, removed when a later pass settles or re-owes the file, by
-- a complete whole-project pass and by a workspace reindex swap.
CREATE TABLE IF NOT EXISTS semantic_gap_files (
    language TEXT NOT NULL,
    filePath TEXT NOT NULL,
    PRIMARY KEY (language, filePath)
);

-- Where a seeded index was copied from (daemon::seed): one row, written into
-- the copy before its rename into place. `reconciledAt` NULL means the seeded
-- first-start reconcile still owes this worktree its pass. Dropped by `wipe`,
-- so a reset index is no longer seeded. Added by `CREATE TABLE IF NOT
-- EXISTS`, so an existing index gains it without a schema-version bump.
CREATE TABLE IF NOT EXISTS seed_origin (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    sourceRoot          TEXT NOT NULL,
    sourceBulkIndexedAt TEXT NOT NULL,
    seededAt            TEXT NOT NULL,
    reconciledAt        TEXT
);

-- The generation of the embedding noise rule (`embedding::text::
-- NOISE_RULE_VERSION`) whose vectors this index last purged
-- (`embedding::backfill`'s one-time noise purge). No row, or another
-- generation, means vectors stored before that rule may still be held by
-- nodes it filters. Dropped by `wipe`, so a rebuilt index purges again (a
-- no-op scan of what it embedded under the rule). Added by `CREATE TABLE IF
-- NOT EXISTS`, so an existing index gains it without a schema-version bump.
CREATE TABLE IF NOT EXISTS embedding_noise_purge (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    ruleVersion TEXT NOT NULL,
    purgedAt    TEXT NOT NULL
);
"#;

/// Applies the graph schema DDL to a fresh (or already up-to-date) connection.
pub fn apply(conn: &Connection) -> Result<()> {
    conn.execute_batch(DDL).context("failed to apply schema DDL")
}

/// Ensures the DB matches both [`CURRENT_SCHEMA_VERSION`] (it can still be
/// read) and `indexer_version` (what it holds is still what today's pipeline
/// would produce), wiping and recreating it on either mismatch - no migration
/// framework, full reindex is the only upgrade path. Returns `true` if a full
/// reindex is now required (fresh DB or either version stale), `false` if the
/// existing schema and data were already current and were left untouched.
///
/// `indexer_version` is passed in rather than read from a constant here
/// because half of it is not a constant: it names the plugin build that will
/// fill the index as well as core's own pipeline generation, and only the
/// caller is in a position to look at the plugin (see
/// `daemon::plugin::indexer_version`, which is what every non-test caller
/// passes). Keeping it a parameter also keeps this module free of filesystem
/// access it would otherwise have to do behind its callers' backs.
///
/// The two are read in sequence rather than in one query because a database
/// old enough to fail the first check has no `indexer_version` column to
/// select at all - `schema_version` is the one column every generation of
/// this schema has had, so it is the only one safe to ask about first.
///
/// For the same reason, `apply` itself is not safe to run first either
/// (GM-357). `apply`'s DDL is written for `CURRENT_SCHEMA_VERSION`'s shape:
/// `CREATE TABLE IF NOT EXISTS` is harmless against an older table (it is
/// simply skipped), but `CREATE INDEX ... nodes(language, container)` is
/// not, because a pre-schema-8 `nodes` has no `container` column, so SQLite
/// errors and execution never reaches the `schema_version` read below, let
/// alone the `reset` that mismatch should have triggered. So `apply` now
/// runs only after `schema_version` itself has confirmed the tables already
/// match `CURRENT_SCHEMA_VERSION`, at which point its DDL is a genuine
/// no-op, or, in the `Err` branch below, as a probe for whether the
/// database is empty enough for that DDL to succeed as a first-time create.
pub fn ensure_current(conn: &Connection, indexer_version: &str) -> Result<bool> {
    let schema: Option<String> =
        match conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0)).optional()
        {
            Ok(schema) => schema,
            Err(_) => {
                // Reading `schema_version` itself failed - either an entirely
                // fresh database (no `meta` table yet: the ordinary first-run
                // case, where `apply` below creates it with no row) or something
                // this tool cannot read or repair as one of its own indexes at
                // all. `apply` is what tells the two apart: it is a harmless
                // no-op against nothing, and it fails the same way this function
                // used to fail unconditionally against the latter - so a failure
                // here is reported as one sentence naming the remedy, not
                // forwarded as SQLite's DDL dump.
                apply(conn).map_err(|_| {
                    anyhow::anyhow!(
                        "this index predates what this build can read or repair in place - \
                     run `g-mesh clean` to remove it, then reindex"
                    )
                })?;
                None
            }
        };

    let Some(schema) = schema else {
        record_version(conn, indexer_version)?;
        return Ok(true);
    };
    if schema != CURRENT_SCHEMA_VERSION {
        return reset(conn, indexer_version).map(|()| true);
    }

    // Only reached once `schema_version` has confirmed the tables on disk
    // already match `CURRENT_SCHEMA_VERSION` - so this DDL is a no-op, never
    // the DDL that broke GM-357.
    apply(conn)?;

    let indexer: String = conn
        .query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get(0))
        .context("failed to read indexer_version")?;
    if indexer != indexer_version {
        return reset(conn, indexer_version).map(|()| true);
    }

    Ok(false)
}

/// The generation `meta` records, `(schema_version, indexer_version)`, read
/// without changing anything: `None` when there is no `meta` table or no row
/// (an index never built here), as for any failure to read it. Read before [`ensure_current`], whose reset
/// would otherwise hide that a generation existed.
///
/// `schema_version` is read first and alone, for the reason [`ensure_current`]
/// gives; an `indexer_version` an old schema cannot supply reads as `""`.
pub fn stored_generation(conn: &Connection) -> Option<(String, String)> {
    let Ok(Some(schema)) = conn
        .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get::<_, String>(0))
        .optional()
    else {
        return None;
    };
    let indexer = conn
        .query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get::<_, Option<String>>(0))
        .ok()
        .flatten()
        .unwrap_or_default();
    Some((schema, indexer))
}

/// `meta.schema_version`, `None` when it cannot be read (no `meta` table, no
/// row).
pub fn stored_schema_version(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
}

/// Throws away everything a stale generation left behind and starts the index
/// over empty. `bulkIndexedAt` going with it is the point, not a side effect:
/// it is what makes the next daemon start walk the project again instead of
/// trusting a graph nothing will ever refresh.
///
/// `pub(crate)` rather than private: `cli::reindex` calls this directly to
/// give `g-mesh reindex` the same wipe a version mismatch triggers
/// automatically, on demand instead of waiting for a version bump to notice
/// one is owed. Both callers hand it a connection and both then owe it a
/// fresh walk - `ensure_current` via the daemon's own cold-start path,
/// `cli::reindex` by calling `daemon::bulk_index::run` itself.
pub(crate) fn reset(conn: &Connection, indexer_version: &str) -> Result<()> {
    wipe(conn)?;
    apply(conn)?;
    record_version(conn, indexer_version)
}

fn wipe(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        // Children before parents: with `foreign_keys` on, dropping `nodes`
        // while `declarations`/`edges`/`vectors`/`containers`/
        // `placeholder_targets` still reference it is an error rather than a
        // cascade. `language_state` has no FK onto `nodes` at all (it is
        // keyed by language, not by node), so its position in the list does
        // not matter - it is dropped here anyway because a version-mismatch
        // wipe throws away every fact this generation ever recorded, per-
        // language ones included (see `record_language_bulk_indexed`'s own
        // comment on why a wipe has to be owed again by every language, not
        // just the project-wide roll-up).
        "DROP TABLE IF EXISTS declarations; DROP TABLE IF EXISTS edges; DROP TABLE IF EXISTS vectors; \
         DROP TABLE IF EXISTS containers; DROP TABLE IF EXISTS placeholder_targets; \
         DROP TABLE IF EXISTS placeholder_also_scopes; \
         DROP TABLE IF EXISTS qualified_suffixes; DROP TABLE IF EXISTS untyped_calls; \
         DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS meta; DROP TABLE IF EXISTS indexed_files; \
         DROP TABLE IF EXISTS language_state; DROP TABLE IF EXISTS pending_reindex; \
         DROP TABLE IF EXISTS semantic_pending; DROP TABLE IF EXISTS semantic_pending_files; \
         DROP TABLE IF EXISTS semantic_owed_files; DROP TABLE IF EXISTS semantic_residual; \
         DROP TABLE IF EXISTS semantic_gap_files; DROP TABLE IF EXISTS language_outcome; \
         DROP TABLE IF EXISTS language_retry; \
         DROP TABLE IF EXISTS resolution_facts; DROP TABLE IF EXISTS resolution_watch_paths; \
         DROP TABLE IF EXISTS seed_origin; \
         DROP TABLE IF EXISTS embedding_noise_purge;",
    )
    .context("failed to wipe schema")
}

fn record_version(conn: &Connection, indexer_version: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, ?1, ?2, CURRENT_TIMESTAMP)",
        rusqlite::params![CURRENT_SCHEMA_VERSION, indexer_version],
    )
    .context("failed to record the schema and indexer versions")?;
    Ok(())
}

pub use super::state::{
    embedding_meta::*, languages::*, reindex::*, resolution::*, retry::*, seed::*, semantic::*,
};

#[cfg(test)]
mod tests;
