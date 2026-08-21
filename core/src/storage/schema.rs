use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

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
pub const CURRENT_SCHEMA_VERSION: &str = "7";

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
CREATE TABLE IF NOT EXISTS nodes (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    name            TEXT NOT NULL,
    qualifiedName   TEXT NOT NULL,
    filePath        TEXT NOT NULL,
    startLine       INTEGER NOT NULL,
    startCol        INTEGER NOT NULL,
    endLine         INTEGER NOT NULL,
    endCol          INTEGER NOT NULL,
    signature       TEXT,
    exported        INTEGER NOT NULL DEFAULT 0,
    docComment      TEXT,
    language        TEXT NOT NULL,
    nativeKind      TEXT,
    hasSyntaxErrors INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_nodes_filePath ON nodes(filePath);
CREATE INDEX IF NOT EXISTS idx_nodes_qualifiedName ON nodes(qualifiedName);

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
CREATE TABLE IF NOT EXISTS edges (
    id            TEXT PRIMARY KEY,
    fromId        TEXT NOT NULL REFERENCES nodes(id),
    toId          TEXT NOT NULL REFERENCES nodes(id),
    kind          TEXT NOT NULL,
    source        TEXT NOT NULL CHECK (source IN ('tree-sitter', 'ts-compiler')),
    resolved      INTEGER NOT NULL DEFAULT 0,
    toDeclaration INTEGER
);

CREATE INDEX IF NOT EXISTS idx_edges_fromId ON edges(fromId);
CREATE INDEX IF NOT EXISTS idx_edges_toId ON edges(toId);

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

/// Marks the project as fully walked. Written only after the last batch of a
/// bulk index has been committed, so a crash mid-walk leaves it unset and the
/// next daemon start redoes the walk (idempotent - every batch is an upsert).
pub fn record_bulk_index(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE meta SET bulkIndexedAt = CURRENT_TIMESTAMP WHERE id = 1", [])
        .context("failed to record bulkIndexedAt")?;
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

/// Marks the whole-project semantic pass complete. Written only after
/// `daemon::semantic::run_with_registry` / `run_once` returns `Ok(_)` - both
/// `Ok(true)` (the pass actually ran) and `Ok(false)` (no bundled plugin was
/// discovered, so nothing was ever owed) count as complete, the same way
/// `run_once`'s own doc comment treats a missing plugin as a legitimate
/// install rather than a failure. An `Err` (the pass was asked for and did
/// not finish) must never reach this - the caller's `match` is what enforces
/// that, not this function, which just writes what it is told.
pub fn record_semantic_pass(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE meta SET semanticPassAt = CURRENT_TIMESTAMP WHERE id = 1", [])
        .context("failed to record semanticPassAt")?;
    Ok(())
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
        // while `declarations`/`edges`/`vectors` still reference it is an
        // error rather than a cascade.
        "DROP TABLE IF EXISTS declarations; DROP TABLE IF EXISTS edges; DROP TABLE IF EXISTS vectors; DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS meta; DROP TABLE IF EXISTS indexed_files;",
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
        assert_eq!(tables, vec!["declarations", "edges", "indexed_files", "meta", "nodes", "vectors"]);

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for expected in
            ["idx_nodes_filePath", "idx_nodes_qualifiedName", "idx_edges_fromId", "idx_edges_toId"]
        {
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
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved)
             VALUES ('e1', 'n1', 'n2', 'CALLS', 'tree-sitter', 0)",
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
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved, toDeclaration)
             VALUES ('bound', 'n1', 'n2', 'CALLS', 'ts-compiler', 1, 2),
                    ('unbound', 'n2', 'n1', 'CALLS', 'tree-sitter', 0, NULL)",
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

        record_semantic_pass(&conn).unwrap();
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
        record_semantic_pass(&conn).unwrap();

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
}
