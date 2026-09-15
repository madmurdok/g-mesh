use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// Bumped whenever the DDL below changes in a way that isn't backward
/// compatible. No migration framework in v1 - a mismatch means a full
/// wipe-and-reindex, since the index is a reproducible cache, not source
/// of truth.
///
/// Bumped to "2" when `indexed_files` was added (query-time staleness
/// checking - see `watcher::staleness`): it records the on-disk mtime/hash
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
pub const CURRENT_SCHEMA_VERSION: &str = "8";

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
pub const CURRENT_INDEXER_VERSION: &str = "1";

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
    container            TEXT
);

CREATE INDEX IF NOT EXISTS idx_nodes_filePath ON nodes(filePath);
CREATE INDEX IF NOT EXISTS idx_nodes_qualifiedName ON nodes(qualifiedName);
-- `(language, container)`, matching `containers`' own `UNIQUE (language,
-- key)` below: a container key is only unique *within* a language (Data
-- Model > Logical containers table - a Go import path and a Rust module path
-- can collide as bare strings), so every lookup of "this container's
-- members" has to filter on both columns together, never `container` alone.
CREATE INDEX IF NOT EXISTS idx_nodes_container ON nodes(language, container);

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
-- `edgeIdFor` in plugins/typescript/src/extract.ts), so one caller calling two
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
    toDeclaration INTEGER
);

CREATE INDEX IF NOT EXISTS idx_edges_fromId ON edges(fromId);
CREATE INDEX IF NOT EXISTS idx_edges_toId ON edges(toId);

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
-- explicitly, since the daemon runs with foreign keys off and the `ON DELETE
-- CASCADE` below never fires there - when the count reaches zero. That
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
-- file - `importedSymbol` in plugins/typescript/src/extract.ts).
CREATE TABLE IF NOT EXISTS placeholder_targets (
    nodeId        TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    scopeKind     TEXT NOT NULL CHECK (scopeKind IN ('file', 'container')),
    scope         TEXT NOT NULL,
    keyKind       TEXT NOT NULL CHECK (keyKind IN ('name', 'qualifiedName')),
    key           TEXT NOT NULL,
    fromContainer TEXT,
    fromFile      TEXT NOT NULL
);

-- The shape every linker lookup needs: "placeholders waiting on (this scope
-- kind, this scope, this key)" - `graph::symbol_links::link_diff`'s triggers
-- for a new declaration, re-export or container member, and its re-export
-- walk-back, all probe this index (for a file and a container scope alike).
-- `graph::imports` (GM-267) is a narrower reader of the same shape: it never
-- reads `key`, only `(scopeKind, scope)` - its own "new file"/"new container
-- member" triggers - so it uses this index's leading columns.
CREATE INDEX IF NOT EXISTS idx_targets_scope ON placeholder_targets(scopeKind, scope, key);

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
CREATE TABLE IF NOT EXISTS language_state (
    language          TEXT PRIMARY KEY,
    bulkIndexedAt     TEXT,
    semanticPassAt    TEXT,
    pluginFingerprint TEXT
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
-- built from - see watcher::staleness. mtime is a cheap fast-path check
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
-- re-embed overwrites the row instead of accumulating one. ON DELETE CASCADE
-- so a node going away (a file edit, a deletion) drops its stale embedding
-- with it instead of leaving an orphan a later similarity search could still
-- surface - the same reasoning `declarations` and `edges` already follow for
-- their own FKs into `nodes`.
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
pub fn ensure_current(conn: &Connection, indexer_version: &str) -> Result<bool> {
    apply(conn)?;

    let schema: Option<String> = conn
        .query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read schema_version")?;

    let Some(schema) = schema else {
        record_version(conn, indexer_version)?;
        return Ok(true);
    };
    if schema != CURRENT_SCHEMA_VERSION {
        return reset(conn, indexer_version).map(|()| true);
    }

    let indexer: String = conn
        .query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get(0))
        .context("failed to read indexer_version")?;
    if indexer != indexer_version {
        return reset(conn, indexer_version).map(|()| true);
    }

    Ok(false)
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

/// Whether a full project walk has ever finished for this index. `false`
/// means the daemon owes the project a cold-start bulk index - on a fresh or
/// wiped DB, but equally after a walk that was killed part way through, which
/// is why this is its own recorded fact rather than "was the schema just
/// created?".
pub fn bulk_index_completed(conn: &Connection) -> Result<bool> {
    let recorded: Option<Option<String>> = conn
        .query_row("SELECT bulkIndexedAt FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read bulkIndexedAt")?;
    Ok(matches!(recorded, Some(Some(_))))
}

/// Every language currently "present" in the index, for the roll-up
/// [`record_bulk_index`]/[`record_semantic_pass`] compute below - defined as
/// "has at least one `File` node", not "was discovered" or "has a
/// `language_state` row". A plugin that was *discovered* but never matched a
/// single file in this project (GM-264's own multi-language test fixtures,
/// say) would otherwise block the roll-up forever waiting on a fact that
/// language has no reason to ever record; a `File`-node count is the same
/// signal `daemon::semantic::indexed_file_count` already uses to size a
/// pass's timeout, for the same underlying reason - it is the cheapest thing
/// already in the table that means "this language actually has content
/// here".
fn present_languages(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT language FROM nodes WHERE kind = 'File'")
        .context("failed to prepare the present-language query")?;
    let rows = stmt.query_map([], |row| row.get(0)).context("failed to query the present languages")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read the present language set")
}

/// [`present_languages`], each paired with whether that language's
/// `language_state.semanticPassAt` is set - what `mcp::instructions` reads
/// (GM-262) to decide, per present language, whether a `receiver_calls`
/// capability that depends on a completed semantic pass
/// (`daemon::manifest::Capabilities::receiver_calls`, gated on
/// `receiver_calls_structural` being unresolved) still names an open gap.
///
/// `pub` rather than folded into a private helper: this is read from
/// `mcp::mod::GMeshMcpServer::get_info`, a synchronous trait method with a
/// plain `&Connection` (via the session's `Mutex` guard) and no roll-up to
/// compute - it wants the raw per-language pairs, not `present_languages`'
/// column-agnostic boolean roll-up shape.
pub fn present_languages_with_semantic_state(conn: &Connection) -> Result<Vec<(String, bool)>> {
    let mut result = Vec::new();
    for language in present_languages(conn)? {
        let passed: Option<Option<String>> = conn
            .query_row(
                "SELECT semanticPassAt FROM language_state WHERE language = ?1",
                params![language],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read language_state.semanticPassAt for {language}"))?;
        result.push((language, matches!(passed, Some(Some(_)))));
    }
    Ok(result)
}

/// Whether every language [`present_languages`] names has a non-NULL
/// `language_state.<column>` - the shared roll-up condition behind both
/// [`record_bulk_index`] and [`record_semantic_pass`]. `column` is always one
/// of this module's own two literal strings (`"bulkIndexedAt"` /
/// `"semanticPassAt"`), never external input, so interpolating it into the
/// query text is as safe as it would be to hand-write two near-identical
/// functions - which this exists to avoid.
///
/// Vacuously `true` when no language is present at all (an index with no
/// `File` nodes yet, or a project none of whose discovered plugins matched
/// anything): there is nothing to wait on, so the roll-up fires immediately,
/// exactly like the old unconditional `UPDATE meta SET ... = CURRENT_TIMESTAMP`
/// this replaces did for the same case.
fn every_present_language_has(conn: &Connection, column: &str) -> Result<bool> {
    for language in present_languages(conn)? {
        let recorded: Option<Option<String>> = conn
            .query_row(
                &format!("SELECT {column} FROM language_state WHERE language = ?1"),
                params![language],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read language_state.{column} for {language}"))?;
        if !matches!(recorded, Some(Some(_))) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Upserts `language`'s `language_state.bulkIndexedAt` to now - the per-
/// language half of [`record_bulk_index`]'s roll-up (design doc: Data Model >
/// Per-language index state). Called once per language from
/// `daemon::bulk_index::walk_one_language`, right after that language's own
/// walk finishes, which is the only place that reliably knows *which*
/// language just completed - `record_bulk_index` itself, called once after
/// every language's walk is done, no longer takes a language at all and only
/// re-checks the roll-up condition.
///
/// `plugin_fingerprint` is `daemon::plugin::fingerprint`'s digest of the
/// plugin build that produced this walk, when the caller has it "readily
/// available" (this task's own scope note) - `walk_one_language` does,
/// because it already holds the `PluginManifest`. `None` leaves whatever
/// `pluginFingerprint` this language already had untouched (via `COALESCE`),
/// rather than overwriting a real value with a gap - the column has no other
/// reader yet (see `language_state`'s own DDL comment), so a caller with
/// nothing to report should not erase what an earlier caller did.
pub fn record_language_bulk_indexed(
    conn: &Connection,
    language: &str,
    plugin_fingerprint: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO language_state (language, bulkIndexedAt, pluginFingerprint)
         VALUES (?1, CURRENT_TIMESTAMP, ?2)
         ON CONFLICT(language) DO UPDATE SET
            bulkIndexedAt = excluded.bulkIndexedAt,
            pluginFingerprint = COALESCE(excluded.pluginFingerprint, language_state.pluginFingerprint)",
        params![language, plugin_fingerprint],
    )
    .with_context(|| format!("failed to record that {language} was bulk-indexed"))?;
    Ok(())
}

/// Re-checks the project-wide roll-up and, if every present language's
/// `language_state.bulkIndexedAt` is now set, marks the project as fully
/// walked. Written only after the last batch of a bulk index has been
/// committed and every language's own [`record_language_bulk_indexed`] call
/// has already landed (`daemon::bulk_index::run` walks every discovered
/// language, in order, before returning - see its own doc comment), so a
/// crash mid-walk leaves both the per-language row and this roll-up unset,
/// and the next daemon start redoes the walk (idempotent - every batch is an
/// upsert).
///
/// Takes no language itself, unlike [`record_language_bulk_indexed`]: every
/// caller here (`daemon::mod`'s cold start, `cli::init`, `cli::reindex`)
/// calls this exactly once, after *every* language's own walk has already
/// recorded its row - the fact this function writes is a property of the
/// whole project, not of any one language, which is what "roll-up" means.
pub fn record_bulk_index(conn: &Connection) -> Result<()> {
    if every_present_language_has(conn, "bulkIndexedAt")? {
        conn.execute("UPDATE meta SET bulkIndexedAt = CURRENT_TIMESTAMP WHERE id = 1", [])
            .context("failed to record bulkIndexedAt")?;
    }
    Ok(())
}

/// Whether the whole-project semantic pass has ever finished for this index -
/// independent of [`bulk_index_completed`], and deliberately so. `false` with
/// `bulk_index_completed` also `false` just means the project has not been
/// walked yet (the ordinary "brand new index" case, and the pass has nowhere
/// to run before the walk it upgrades). `false` with `bulk_index_completed`
/// `true` is the gap this column exists to name: a walk that finished, and a
/// pass that started (its own record is what set `bulkIndexedAt`, per
/// `daemon::semantic`'s module doc's ordering) but was interrupted - a killed
/// plugin, a crash, a process timeout - before it could finish and record
/// itself. See `daemon::semantic::run_with_registry` / `run_once` for the two
/// call shapes that write this, and `daemon::mod`'s cold start / `cli::init`
/// for the readers that treat the second case as still owed.
pub fn semantic_pass_completed(conn: &Connection) -> Result<bool> {
    let recorded: Option<Option<String>> = conn
        .query_row("SELECT semanticPassAt FROM meta WHERE id = 1", [], |row| row.get(0))
        .optional()
        .context("failed to read semanticPassAt")?;
    Ok(matches!(recorded, Some(Some(_))))
}

/// Upserts `language`'s `language_state.semanticPassAt` to now, with no
/// roll-up check - the per-language half [`record_semantic_pass`] builds on.
/// Exists as its own function (rather than folded straight into
/// `record_semantic_pass`) so a test can write one language's row without
/// tripping the roll-up, the same way [`record_language_bulk_indexed`] is
/// kept apart from [`record_bulk_index`] - see
/// `a_two_language_semantic_pass_roll_up_waits_for_the_slower_language`
/// below for why that separation is what makes the roll-up rule testable at
/// all.
pub fn record_language_semantic_pass(conn: &Connection, language: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO language_state (language, semanticPassAt) VALUES (?1, CURRENT_TIMESTAMP)
         ON CONFLICT(language) DO UPDATE SET semanticPassAt = excluded.semanticPassAt",
        params![language],
    )
    .with_context(|| format!("failed to record that {language}'s semantic pass completed"))?;
    Ok(())
}

/// Whether every *present and semantic-pass-capable* language has recorded
/// `language_state.semanticPassAt` - [`record_semantic_pass`]'s roll-up
/// condition, generalized (GM-270) from [`every_present_language_has`]'s
/// single-column check to skip a present language whose manifest declares
/// `capabilities.semantic_pass = false`.
///
/// That skip is the whole point of this being a separate function rather
/// than `every_present_language_has(conn, "semanticPassAt")` reused as-is: a
/// language nobody will ever ask for a pass (the design doc's `plugin.toml`
/// `semantic_pass = false`, the conservative default for a manifest that
/// says nothing) also never gets a `language_state` row written for that
/// column - nothing calls [`record_language_semantic_pass`] for it - so the
/// old, uncapped check would wait on a fact that can never happen and hold
/// `meta.semanticPassAt` hostage forever. `bulkIndexedAt`'s own roll-up
/// ([`record_bulk_index`], still built on [`every_present_language_has`]) has
/// no equivalent gap: every discovered plugin gets walked regardless of its
/// semantic capability, so every present language does eventually write that
/// column.
///
/// `semantic_pass_languages` is the caller's set of languages whose manifest
/// declares `capabilities.semantic_pass = true` - this module reads no
/// capability itself and has no `daemon::manifest` dependency, matching its
/// existing "pure storage, callers bring the facts" boundary; the caller
/// (`daemon::semantic`, which already holds the discovered manifests or a
/// `PluginRegistry` to ask) is what can answer "capable of what".
///
/// Vacuously `true` when no present language is in `semantic_pass_languages`
/// at all - including the empty set, i.e. no discovered plugin anywhere
/// declares the capability - for the same reason
/// [`every_present_language_has`] is vacuously `true` on an empty project:
/// there is nothing to wait on, so a project with no semantic-capable plugin
/// must not be retried forever by `daemon::mod`'s "pass still owed" check.
fn every_present_semantic_language_has_passed(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<bool> {
    Ok(owed_semantic_pass_languages(conn, semantic_pass_languages)?.is_empty())
}

/// Every language among `semantic_pass_languages` that is currently *owed* a
/// whole-project semantic pass - present in the index (at least one `File`
/// node, the same [`present_languages`] definition GM-264's roll-up uses) and
/// whose `language_state.semanticPassAt` is still `NULL`.
///
/// This is the "Owed" definition `daemon::semantic::run_with_registry`/
/// `run_once` (GM-270) schedule against: asking again for a language whose
/// pass already completed would repeat real, possibly expensive work (a cold
/// `rust-analyzer`/`tsserver` load) for no new information, and is exactly
/// what makes "an interrupted pass for one language is retried without
/// re-running the other" true rather than aspirational - a retry call only
/// ever asks the languages this returns, never every capable language again.
///
/// A capable language with **no** files in this project yet is not owed
/// either, the same way it is not part of [`every_present_semantic_language_has_passed`]'s
/// roll-up: there is nothing for its semantic tier to improve on, so asking
/// would only cost a plugin spawn (and, for a heavy engine, real memory) for
/// an empty-diff answer.
pub fn owed_semantic_pass_languages(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<Vec<String>> {
    let mut owed = Vec::new();
    for language in present_languages(conn)? {
        if !semantic_pass_languages.contains(&language) {
            continue;
        }
        let recorded: Option<Option<String>> = conn
            .query_row(
                "SELECT semanticPassAt FROM language_state WHERE language = ?1",
                params![language],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("failed to read language_state.semanticPassAt for {language}"))?;
        if !matches!(recorded, Some(Some(_))) {
            owed.push(language);
        }
    }
    // Sorted: `present_languages` reads `SELECT DISTINCT language FROM
    // nodes`, which carries no ordering guarantee, but
    // `daemon::semantic::run_with_registry`/`run_once` schedule the languages
    // this returns *sequentially* (see that module's doc comment on
    // "Sequential, not concurrent") - a fixed, sorted order is what makes
    // which language runs first (and therefore which one a shared machine's
    // memory pressure hits first) reproducible from run to run, matching
    // `daemon::manifest::semantic_pass_capable_languages`'s own sort for the
    // same reason.
    owed.sort();
    Ok(owed)
}

/// Re-checks the project-wide semantic-pass roll-up and marks
/// `meta.semanticPassAt` if [`every_present_semantic_language_has_passed`]
/// now says yes - the half of [`record_semantic_pass`] that does not write
/// any one language's own row, split out so it can be called on its own.
///
/// GM-270 needs that split because the roll-up can become true without any
/// call in the same batch writing a per-language row: a whole-project pass
/// run (`daemon::semantic::run_with_registry`/`run_once`) asks every
/// semantic-pass-capable language, and when that set is empty (nothing
/// discovered declares the capability) or every capable language's row was
/// already set by an earlier run, the loop over languages never calls
/// [`record_language_semantic_pass`] at all - yet the roll-up still has to
/// fire once per run, or a project with no semantic-capable plugin would be
/// reported as "pass still owed" by `daemon::mod`'s cold-start retry on
/// every single daemon start, forever.
pub fn reconcile_semantic_pass_rollup(
    conn: &Connection,
    semantic_pass_languages: &HashSet<String>,
) -> Result<()> {
    if every_present_semantic_language_has_passed(conn, semantic_pass_languages)? {
        conn.execute("UPDATE meta SET semanticPassAt = CURRENT_TIMESTAMP WHERE id = 1", [])
            .context("failed to record semanticPassAt")?;
    }
    Ok(())
}

/// Marks `language`'s whole-project semantic pass complete, then re-checks
/// the roll-up via [`reconcile_semantic_pass_rollup`] - the one-call
/// convenience for a caller that has exactly one language's completion to
/// record and wants both effects at once (a test, or a single-language
/// caller). `daemon::semantic::run_with_registry`/`run_once` iterate many
/// languages per call, so they use [`record_language_semantic_pass`] and
/// [`reconcile_semantic_pass_rollup`] separately instead - one row write per
/// completed language, one roll-up check at the end of the whole run, not one
/// per language (see that module for why, and for the case with zero
/// completions this function alone would not reach).
///
/// Written only after a caller's own pass attempt for `language` has
/// actually succeeded - never for an `Err` (the pass was asked for and did
/// not finish), which must leave `language`'s row unset so the next attempt
/// retries it. `semantic_pass_languages` is forwarded to
/// [`reconcile_semantic_pass_rollup`] unchanged - see that function and
/// [`every_present_semantic_language_has_passed`] for what it is for.
pub fn record_semantic_pass(
    conn: &Connection,
    language: &str,
    semantic_pass_languages: &HashSet<String>,
) -> Result<()> {
    record_language_semantic_pass(conn, language)?;
    reconcile_semantic_pass_rollup(conn, semantic_pass_languages)
}

/// Records the project's current active embedding model, so `meta` reflects
/// what `embedding::EmbeddingPipeline` actually tagged its rows with rather
/// than staying `NULL` forever (see the DDL comment on `vectors` for why the
/// two columns are a distinct fact from each other, and `EmbeddingPipeline`'s
/// module doc for why this is written on every `apply`, not once). A plain
/// `UPDATE` rather than a schema column type built for a fixed set of models:
/// a future model switch is a data change to this one row, never a migration.
pub fn set_embedding_model(conn: &Connection, model: &str) -> Result<()> {
    conn.execute("UPDATE meta SET embedding_model = ?1 WHERE id = 1", rusqlite::params![model])
        .context("failed to record the active embedding model")?;
    Ok(())
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
         DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS meta; DROP TABLE IF EXISTS indexed_files; \
         DROP TABLE IF EXISTS language_state;",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A generation string shaped like the one `daemon::plugin::indexer_version`
    /// composes - this constant plus the plugin build's digest - so these tests
    /// exercise the value that is really stored rather than only half of it.
    const GENERATION: &str = "1+0123456789abcdef";
    /// The same core pipeline with the plugin rebuilt: the shape of task 115's
    /// change, and the one nothing used to notice.
    const GENERATION_AFTER_A_PLUGIN_REBUILD: &str = "1+fedcba9876543210";

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply(&conn).unwrap();
        conn
    }

    #[test]
    fn creates_all_tables_and_indexes() {
        let conn = setup();
        let mut tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.sort();
        assert_eq!(
            tables,
            vec![
                "containers",
                "declarations",
                "edges",
                "indexed_files",
                "language_state",
                "meta",
                "nodes",
                "placeholder_targets",
                "vectors",
            ]
        );

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in [
            "idx_nodes_filePath",
            "idx_nodes_qualifiedName",
            "idx_nodes_container",
            "idx_edges_fromId",
            "idx_edges_toId",
            "idx_targets_scope",
        ] {
            assert!(indexes.contains(&expected.to_string()), "missing index {expected}");
        }
    }

    #[test]
    fn nodes_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        let (name, kind): (String, String) = conn
            .query_row("SELECT name, kind FROM nodes WHERE id = 'n1'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "foo");
        assert_eq!(kind, "Function");
    }

    #[test]
    fn edges_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n2', 'Function', 'bar', 'mod::bar', 'src/lib.rs', 5, 0, 7, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, engine, resolved)
             VALUES ('e1', 'n1', 'n2', 'CALLS', 'syntactic', 'tree-sitter', 0)",
            [],
        )
        .unwrap();

        let (kind, resolved): (String, bool) = conn
            .query_row("SELECT kind, resolved FROM edges WHERE id = 'e1'", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(kind, "CALLS");
        assert!(!resolved);
    }

    #[test]
    fn declarations_round_trip_in_ordinal_order() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'parse', 'parse', 'src/parse.ts', 3, 7, 5, 1, 'typescript')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO declarations (nodeId, ordinal, startLine, startCol, endLine, endCol, signature, hasBody)
             VALUES ('n1', 1, 2, 7, 2, 61, 'parse(input: number): number', 0),
                    ('n1', 0, 1, 7, 1, 47, 'parse(input: string): string[]', 0)",
            [],
        )
        .unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT ordinal, signature, hasBody FROM declarations WHERE nodeId = 'n1' ORDER BY ordinal",
            )
            .unwrap();
        let rows: Vec<(i64, String, bool)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (0, "parse(input: string): string[]".to_string(), false),
                (1, "parse(input: number): number".to_string(), false),
            ],
            "ordinal is what orders a declaration list, not insertion order"
        );
    }

    /// Two declarations of one node cannot share an ordinal: it is the address
    /// an edge's `toDeclaration` uses, so a duplicate would make "which
    /// overload did this call bind" unanswerable.
    #[test]
    fn a_node_cannot_have_two_declarations_at_the_same_ordinal() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'parse', 'parse', 'src/parse.ts', 3, 7, 5, 1, 'typescript')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO declarations (nodeId, ordinal, startLine, startCol, endLine, endCol, hasBody)
             VALUES ('n1', 0, 1, 7, 1, 47, 0)",
            [],
        )
        .unwrap();

        let clash = conn.execute(
            "INSERT INTO declarations (nodeId, ordinal, startLine, startCol, endLine, endCol, hasBody)
             VALUES ('n1', 0, 9, 0, 9, 9, 1)",
            [],
        );
        assert!(clash.is_err());
    }

    #[test]
    fn an_edge_records_which_declaration_it_bound() {
        let conn = setup();
        for id in ["n1", "n2"] {
            conn.execute(
                "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
                 VALUES (?1, 'Function', 'f', 'f', 'src/lib.ts', 1, 0, 3, 1, 'typescript')",
                [id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, engine, resolved, toDeclaration)
             VALUES ('bound', 'n1', 'n2', 'CALLS', 'semantic', 'ts-compiler', 1, 2),
                    ('unbound', 'n2', 'n1', 'CALLS', 'syntactic', 'tree-sitter', 0, NULL)",
            [],
        )
        .unwrap();

        let bound: Option<i64> = conn
            .query_row("SELECT toDeclaration FROM edges WHERE id = 'bound'", [], |row| row.get(0))
            .unwrap();
        let unbound: Option<i64> = conn
            .query_row("SELECT toDeclaration FROM edges WHERE id = 'unbound'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(bound, Some(2));
        assert_eq!(unbound, None, "binding no particular declaration is the default, and stays NULL");
    }

    #[test]
    fn wipes_and_reindexes_on_version_mismatch() {
        let conn = setup();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, '0', ?1, CURRENT_TIMESTAMP)",
            rusqlite::params![GENERATION],
        )
        .unwrap();

        let reindex_needed = ensure_current(&conn, GENERATION).unwrap();
        assert!(reindex_needed);

        let version: String =
            conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 0, "old data must not survive a version mismatch wipe");

        // GM-264's acceptance criterion at the schema level: an index whose
        // schema predates this bump (the "0"-tagged meta row seeded above,
        // standing in for schema "7" and earlier - none of which ever had
        // these tables or columns) is wiped and rebuilt *with* them, not left
        // on the old DDL just because the tables it lacked cannot fail an
        // `INSERT`.
        for table in ["containers", "placeholder_targets", "language_state"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
                .unwrap_or_else(|err| panic!("{table} must exist on a freshly reset index: {err}"));
            assert_eq!(count, 0, "{table} must be empty right after the reset");
        }
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language, visibility, container)
             VALUES ('n2', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust', 'container', 'pkg')",
            [],
        )
        .unwrap_or_else(|err| panic!("nodes.visibility/container must exist on a freshly reset index: {err}"));
    }

    #[test]
    fn leaves_current_version_untouched() {
        let conn = setup();
        // First call on a fresh DB: no meta row yet, so a reindex is (correctly) signaled.
        assert!(ensure_current(&conn, GENERATION).unwrap());

        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        // Second call at the same (current) version must not wipe existing data.
        let reindex_needed = ensure_current(&conn, GENERATION).unwrap();
        assert!(!reindex_needed);

        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 1, "data at the current schema version must survive");
    }

    #[test]
    fn a_fresh_index_owes_a_bulk_index_until_one_is_recorded() {
        let conn = setup();
        assert!(ensure_current(&conn, GENERATION).unwrap());
        assert!(!bulk_index_completed(&conn).unwrap(), "a fresh index has never been walked");

        record_bulk_index(&conn).unwrap();
        assert!(bulk_index_completed(&conn).unwrap());

        // The whole point of the flag: reopening an unchanged, already-walked
        // index must not ask for the walk again.
        assert!(!ensure_current(&conn, GENERATION).unwrap());
        assert!(bulk_index_completed(&conn).unwrap());
    }

    /// The gap task 62cc2d0f closes: `bulkIndexedAt` and `semanticPassAt` are
    /// independent facts, so a walk finishing says nothing about whether the
    /// pass that follows it did too.
    #[test]
    fn a_walked_index_still_owes_its_semantic_pass_until_one_is_recorded() {
        let conn = setup();
        assert!(ensure_current(&conn, GENERATION).unwrap());
        assert!(!semantic_pass_completed(&conn).unwrap(), "a fresh index has had no semantic pass either");

        record_bulk_index(&conn).unwrap();
        assert!(bulk_index_completed(&conn).unwrap());
        assert!(
            !semantic_pass_completed(&conn).unwrap(),
            "recording the walk must not also mark the pass complete - \
             a pass interrupted right after this point is exactly the gap this column exists for"
        );

        record_semantic_pass(&conn, "typescript", &HashSet::from(["typescript".to_string()])).unwrap();
        assert!(semantic_pass_completed(&conn).unwrap());
        // Recording the pass must not retroactively touch the walk's own flag.
        assert!(bulk_index_completed(&conn).unwrap());
    }

    /// A version-mismatch wipe throws the whole graph away, so both facts
    /// about it - not just the walk - are owed again afterwards.
    #[test]
    fn a_version_mismatch_wipe_makes_the_semantic_pass_owed_again_too() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        record_bulk_index(&conn).unwrap();
        record_semantic_pass(&conn, "typescript", &HashSet::from(["typescript".to_string()])).unwrap();

        conn.execute("UPDATE meta SET schema_version = '0' WHERE id = 1", []).unwrap();
        assert!(ensure_current(&conn, GENERATION).unwrap());
        assert!(
            !semantic_pass_completed(&conn).unwrap(),
            "data wiped by a version mismatch has to have its semantic pass redone too"
        );
    }

    #[test]
    fn the_active_embedding_model_starts_unset_and_can_be_recorded() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();

        let before: Option<String> =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(before, None, "a fresh index has no active model recorded yet");

        set_embedding_model(&conn, "jina-embeddings-v2-base-code").unwrap();
        let after: String =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(after, "jina-embeddings-v2-base-code");
    }

    /// The acceptance criterion behind the column's whole design: a future
    /// model switch is a data change to this one row, not a schema migration.
    #[test]
    fn recording_a_different_model_overwrites_the_previous_one_with_no_schema_change() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();

        set_embedding_model(&conn, "jina-embeddings-v2-base-code").unwrap();
        set_embedding_model(&conn, "some-future-model-v2").unwrap();

        let model: String =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(model, "some-future-model-v2");
    }

    #[test]
    fn a_version_mismatch_wipe_makes_a_bulk_index_owed_again() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        record_bulk_index(&conn).unwrap();

        conn.execute("UPDATE meta SET schema_version = '0' WHERE id = 1", []).unwrap();
        assert!(ensure_current(&conn, GENERATION).unwrap());
        assert!(
            !bulk_index_completed(&conn).unwrap(),
            "data wiped by a version mismatch has to be walked again"
        );
    }

    /// Task 96: the shape that let a 2026-07-28 index keep answering queries
    /// three releases later. Nothing about the DDL changed, so the schema
    /// check passed and the walk marker survived - and the stale graph was
    /// served as if it were current.
    #[test]
    fn an_index_from_an_older_indexer_is_wiped_even_though_its_schema_still_matches() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        record_bulk_index(&conn).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        conn.execute("UPDATE meta SET indexer_version = '0' WHERE id = 1", []).unwrap();

        assert!(ensure_current(&conn, GENERATION).unwrap(), "a previous generation's graph must not be kept");
        let schema: String =
            conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(schema, CURRENT_SCHEMA_VERSION, "the schema was current all along and stays so");
        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 0, "what the old indexer produced must not survive");
        assert!(
            !bulk_index_completed(&conn).unwrap(),
            "the project is owed a full walk, or the wipe just made the index emptier and no fresher"
        );
    }

    #[test]
    fn a_fresh_index_records_the_indexer_generation_that_will_fill_it() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();

        let indexer: String =
            conn.query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(indexer, GENERATION);
        assert!(
            !ensure_current(&conn, GENERATION).unwrap(),
            "the stamp it just wrote must satisfy its own check"
        );
    }

    /// Task 116: the generation that filled an index names the plugin build as
    /// well as core's own pipeline, so rebuilding only the plugin invalidates
    /// it - which is what did *not* happen when task 115 changed the extractor
    /// and left every existing index serving the resolution it replaced.
    #[test]
    fn an_index_a_previous_plugin_build_filled_is_wiped_though_cores_own_generation_is_unchanged() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        record_bulk_index(&conn).unwrap();
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
            [],
        )
        .unwrap();

        assert!(
            ensure_current(&conn, GENERATION_AFTER_A_PLUGIN_REBUILD).unwrap(),
            "a graph the previous plugin build produced must not be kept"
        );

        let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
        assert_eq!(node_count, 0, "what the old extractor produced must not survive");
        assert!(
            !bulk_index_completed(&conn).unwrap(),
            "the project is owed a full walk by the plugin that replaced it"
        );
        let indexer: String =
            conn.query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(indexer, GENERATION_AFTER_A_PLUGIN_REBUILD, "the new generation has to be recorded");
    }

    #[test]
    fn meta_round_trip() {
        let conn = setup();
        conn.execute(
            "INSERT INTO meta (id, schema_version, indexer_version, embedding_model, lastUsed)
             VALUES (1, '1', '1', 'jina-embeddings-v2-base-code', '2026-07-27T00:00:00Z')",
            [],
        )
        .unwrap();

        let version: String =
            conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(version, "1");
    }

    /// A `File` node for `language`, present so [`present_languages`]/the
    /// roll-up tests below have something to consider that language
    /// "present" for.
    fn seed_file(conn: &Connection, id: &str, language: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'File', ?1, ?1, ?1, 0, 0, 0, 0, ?2)",
            params![id, language],
        )
        .unwrap();
    }

    /// The acceptance criterion: with two languages present, the project-wide
    /// roll-up must not fire off the first language's own record - only once
    /// *every* present language has recorded its walk. Discriminates: comment
    /// out the `every_present_language_has` check inside `record_bulk_index`
    /// (i.e. have it unconditionally `UPDATE meta ...`) and the first
    /// `assert!(!...)` below fails, because the roll-up would fire after `go`
    /// alone.
    #[test]
    fn the_bulk_index_roll_up_waits_for_every_present_language() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        seed_file(&conn, "f1", "go");
        seed_file(&conn, "f2", "rust");

        record_language_bulk_indexed(&conn, "go", Some("fingerprint-go")).unwrap();
        record_bulk_index(&conn).unwrap();
        assert!(
            !bulk_index_completed(&conn).unwrap(),
            "rust has not recorded its own walk yet - the roll-up must not fire early"
        );

        record_language_bulk_indexed(&conn, "rust", None).unwrap();
        record_bulk_index(&conn).unwrap();
        assert!(
            bulk_index_completed(&conn).unwrap(),
            "both present languages have now recorded their walk - the roll-up must fire"
        );

        let fingerprint: Option<String> = conn
            .query_row("SELECT pluginFingerprint FROM language_state WHERE language = 'go'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(fingerprint.as_deref(), Some("fingerprint-go"));
        let rust_fingerprint: Option<String> = conn
            .query_row("SELECT pluginFingerprint FROM language_state WHERE language = 'rust'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rust_fingerprint, None, "a caller with no fingerprint to report must not invent one");
    }

    /// The same rule, for the semantic-pass roll-up - kept as its own test
    /// rather than folded into the one above because [`record_semantic_pass`]
    /// writes its language's own row *and* re-checks the roll-up in the same
    /// call (see its own doc comment for why that differs from
    /// `record_bulk_index`), which is exactly the shape worth exercising
    /// directly rather than assuming it behaves like the other roll-up.
    #[test]
    fn the_semantic_pass_roll_up_waits_for_every_present_language() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        seed_file(&conn, "f1", "go");
        seed_file(&conn, "f2", "rust");
        let both_capable = HashSet::from(["go".to_string(), "rust".to_string()]);

        record_semantic_pass(&conn, "go", &both_capable).unwrap();
        assert!(
            !semantic_pass_completed(&conn).unwrap(),
            "rust has not had its own pass recorded yet - the roll-up must not fire early"
        );

        record_semantic_pass(&conn, "rust", &both_capable).unwrap();
        assert!(
            semantic_pass_completed(&conn).unwrap(),
            "both present languages have now had their pass recorded - the roll-up must fire"
        );
    }

    /// GM-270's own acceptance criterion: a present language whose manifest
    /// declares `capabilities.semantic_pass = false` is never asked for a
    /// pass and must not hold `meta.semanticPassAt` hostage waiting for a
    /// `language_state` row that will never be written - see
    /// `every_present_semantic_language_has_passed`'s doc comment.
    ///
    /// Discriminates: pass `HashSet::from(["typescript".to_string(),
    /// "go".to_string()])` (i.e. treat `go` as capable too, the old
    /// `every_present_language_has`-style behaviour) instead of
    /// `capable_languages` below, and the final assertion fails, because the
    /// roll-up would then wait on a `go` row nothing ever writes.
    #[test]
    fn a_present_language_without_semantic_pass_capability_does_not_block_the_roll_up() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        // Both languages have files in the index, but only "typescript" is
        // semantic-pass-capable - "go" here stands in for a discovered plugin
        // whose manifest never set `capabilities.semantic_pass = true` (the
        // conservative default), so nothing ever calls
        // `record_language_semantic_pass` for it.
        seed_file(&conn, "f1", "typescript");
        seed_file(&conn, "f2", "go");
        let capable_languages = HashSet::from(["typescript".to_string()]);

        record_semantic_pass(&conn, "typescript", &capable_languages).unwrap();

        assert!(
            semantic_pass_completed(&conn).unwrap(),
            "go is present but not semantic-pass-capable, so it must not gate the roll-up \
             typescript alone already satisfies"
        );
    }

    /// The other half of GM-270's roll-up split: a run that asks *zero*
    /// semantic-pass-capable languages (nothing discovered declares the
    /// capability at all) never calls `record_language_semantic_pass` for
    /// anything, so nothing would ever reconcile the roll-up if
    /// `reconcile_semantic_pass_rollup` could only be reached through
    /// `record_semantic_pass`'s per-language call. Called directly, with an
    /// empty capable set, it still has to mark the project's semantic pass
    /// complete - vacuously, nothing was ever owed - so `daemon::mod`'s
    /// "pass still owed" retry does not loop on such a project forever.
    #[test]
    fn reconciling_with_no_semantic_pass_capable_language_still_completes_the_roll_up() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        seed_file(&conn, "f1", "go");

        reconcile_semantic_pass_rollup(&conn, &HashSet::new()).unwrap();

        assert!(
            semantic_pass_completed(&conn).unwrap(),
            "no discovered language is semantic-pass-capable, so nothing is owed"
        );
    }

    /// A language `record_language_bulk_indexed`/`record_semantic_pass` never
    /// heard of - GM-264's actual state of the world, where only `typescript`
    /// exists - must not block or vacuously satisfy the roll-up for the
    /// language that *is* present: only `present_languages` (derived from
    /// `File` nodes actually in the index) is ever consulted.
    #[test]
    fn a_language_with_no_present_files_is_not_part_of_the_roll_up() {
        let conn = setup();
        ensure_current(&conn, GENERATION).unwrap();
        seed_file(&conn, "f1", "typescript");
        // "go" was discovered and even recorded a row (a manifest matching
        // zero files still gets spawned - see `daemon::bulk_index::run`'s own
        // doc comment) but has no `File` node, so it must not gate the
        // roll-up for the language that *is* present.
        record_language_bulk_indexed(&conn, "go", None).unwrap();

        record_language_bulk_indexed(&conn, "typescript", None).unwrap();
        record_bulk_index(&conn).unwrap();
        assert!(bulk_index_completed(&conn).unwrap());
    }
}
