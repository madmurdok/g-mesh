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
        .query_row("SELECT name, kind FROM nodes WHERE id = 'n1'", [], |row| Ok((row.get(0)?, row.get(1)?)))
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
        .prepare("SELECT ordinal, signature, hasBody FROM declarations WHERE nodeId = 'n1' ORDER BY ordinal")
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

    let bound: Option<i64> =
        conn.query_row("SELECT toDeclaration FROM edges WHERE id = 'bound'", [], |row| row.get(0)).unwrap();
    let unbound: Option<i64> =
        conn.query_row("SELECT toDeclaration FROM edges WHERE id = 'unbound'", [], |row| row.get(0)).unwrap();
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

/// The exact pre-GM-264 DDL (`git show 88394e5~1:core/src/storage/schema.rs`),
/// current as `CURRENT_SCHEMA_VERSION` "7" - i.e. what 2.12.0 (and every
/// earlier 2.x build) actually wrote to disk. `nodes` here has no
/// `container` column; `containers`, `placeholder_targets` and
/// `language_state` do not exist at all.
const SCHEMA_7_DDL: &str = r#"
        CREATE TABLE nodes (
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
        CREATE INDEX idx_nodes_filePath ON nodes(filePath);
        CREATE INDEX idx_nodes_qualifiedName ON nodes(qualifiedName);

        CREATE TABLE declarations (
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

        CREATE TABLE edges (
            id            TEXT PRIMARY KEY,
            fromId        TEXT NOT NULL REFERENCES nodes(id),
            toId          TEXT NOT NULL REFERENCES nodes(id),
            kind          TEXT NOT NULL,
            source        TEXT NOT NULL CHECK (source IN ('tree-sitter', 'ts-compiler')),
            resolved      INTEGER NOT NULL DEFAULT 0,
            toDeclaration INTEGER
        );
        CREATE INDEX idx_edges_fromId ON edges(fromId);
        CREATE INDEX idx_edges_toId ON edges(toId);

        CREATE TABLE meta (
            id              INTEGER PRIMARY KEY CHECK (id = 1),
            schema_version  TEXT NOT NULL,
            indexer_version TEXT NOT NULL,
            embedding_model TEXT,
            lastUsed        TEXT NOT NULL,
            bulkIndexedAt   TEXT,
            semanticPassAt  TEXT
        );

        CREATE TABLE indexed_files (
            filePath    TEXT PRIMARY KEY,
            mtimeMillis INTEGER NOT NULL,
            contentHash TEXT NOT NULL
        );

        CREATE TABLE vectors (
            nodeId          TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
            embedding       BLOB NOT NULL,
            embeddingVersion TEXT NOT NULL
        );
    "#;

/// GM-357: a real pre-schema-8 index (last written by 2.12.0, schema "7")
/// must be recovered - wiped and reindexed through `reset`, exactly like
/// any other version mismatch - not crash before it ever reads
/// `schema_version`.
///
/// This is the fixture the existing `wipes_and_reindexes_on_version_mismatch`
/// test above does *not* provide: that test's `setup()` calls today's
/// `apply` first, so its `nodes` table already has `container` - it seeds
/// only `meta.schema_version`, never an actually-old table shape, which is
/// exactly why it did not catch GM-357. This one builds the tables schema
/// "7" really had, with no `container` column and none of GM-264's new
/// tables, so it fails on unfixed code and passes only once `apply`'s DDL
/// runs after the version check, not before it.
#[test]
fn recovers_a_genuinely_pre_schema_8_index() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_7_DDL).unwrap();
    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES ('n1', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO meta (id, schema_version, indexer_version, lastUsed) VALUES (1, '7', '1', CURRENT_TIMESTAMP)",
        [],
    )
    .unwrap();

    let reindex_needed = ensure_current(&conn, GENERATION).expect(
        "a pre-schema-8 index must be recovered via reset, not fail before schema_version is even read",
    );
    assert!(reindex_needed);

    let version: String =
        conn.query_row("SELECT schema_version FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(version, CURRENT_SCHEMA_VERSION);

    let node_count: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0)).unwrap();
    assert_eq!(node_count, 0, "old data must not survive the wipe");

    conn.execute(
        "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language, visibility, container)
             VALUES ('n2', 'Function', 'foo', 'mod::foo', 'src/lib.rs', 1, 0, 3, 1, 'rust', 'container', 'pkg')",
        [],
    )
    .unwrap_or_else(|err| panic!("nodes.container must exist on a freshly reset index: {err}"));
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
    assert!(!bulk_index_completed(&conn).unwrap(), "data wiped by a version mismatch has to be walked again");
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
        .query_row("SELECT pluginFingerprint FROM language_state WHERE language = 'go'", [], |row| row.get(0))
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
