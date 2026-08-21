//! sqlite-vec extension loading and the `vectors` table (see `storage::schema`
//! for the DDL and its doc comment for why the table is 1:0-or-1 with
//! `nodes`).
//!
//! # Loading the extension
//!
//! sqlite-vec ships as a regular SQLite loadable extension, but this crate
//! links it in statically via the `sqlite-vec` crate (built with
//! `SQLITE_CORE` defined, the same way `rusqlite`'s `bundled` feature builds
//! SQLite itself - the two are compiled to share symbols rather than talk
//! through the loadable-extension ABI). [`register_extension`] hands its
//! init function to `sqlite3_auto_extension`, SQLite's *global* "run this on
//! every connection this process opens from now on" hook - so unlike a
//! normal per-connection `load_extension` call, this is registered once for
//! the whole process and every `rusqlite::Connection` subsequently opened
//! anywhere (via `storage::connection::open` or directly) has
//! `vec_distance_cosine`/`vec_version`/etc. available with no further setup.
//! [`storage::connection::open`] calls it before opening its connection so
//! the daemon's real database always has it; tests here call it themselves
//! since they open their own in-memory connections.
//!
//! # Vector format
//!
//! `vectors.embedding` stores sqlite-vec's compact format: each dimension as
//! a 4-byte little-endian float32, back to back, no header - what
//! [`pack`]/[`unpack`] convert to and from a plain `&[f32]`. sqlite-vec's
//! distance functions also accept a JSON-text vector (`'[0.1, 0.2, ...]'`),
//! but packing to the binary form ourselves avoids formatting/parsing floats
//! as text on every insert and every query.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Registers the sqlite-vec extension with SQLite's process-wide
/// auto-extension hook. Idempotent - guarded by [`std::sync::Once`] so
/// calling it from every place that opens a connection (the daemon's real
/// database, this module's own tests) is safe even though the underlying
/// `sqlite3_auto_extension` call is not itself safe to repeat with the same
/// function pointer in every SQLite build.
pub fn register_extension() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| unsafe {
        // SAFETY: `sqlite3_vec_init` matches the `sqlite3_auto_extension`
        // entry point signature SQLite expects (see sqlite-vec's own
        // `test_rusqlite_auto_extension`, which registers it the same way);
        // the transmute only recovers that signature from the crate's
        // `extern "C"` declaration, which itself has none.
        // Spelled out rather than inferred: this is the signature being
        // recovered, and an unannotated transmute to a function pointer is a
        // reader's dead end.
        type InitFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *const std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        let init = std::mem::transmute::<*const (), InitFn>(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
}

/// Packs a vector into sqlite-vec's compact float32 blob format. `pub(crate)`
/// rather than private: `mcp::search_code` packs its query vector with this
/// same routine so a query and a stored `vectors.embedding` row are encoded
/// identically for `vec_distance_cosine` to compare.
pub(crate) fn pack(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Inserts or replaces the embedding for `node_id`. One row per node (see
/// `storage::schema`'s DDL comment on `vectors`), so re-embedding an
/// already-embedded node overwrites its row rather than adding another one.
pub fn insert(conn: &Connection, node_id: &str, embedding: &[f32], embedding_version: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO vectors (nodeId, embedding, embeddingVersion) VALUES (?1, ?2, ?3)
         ON CONFLICT(nodeId) DO UPDATE SET embedding = excluded.embedding, embeddingVersion = excluded.embeddingVersion",
        rusqlite::params![node_id, pack(embedding), embedding_version],
    )
    .context("failed to insert vector")?;
    Ok(())
}

/// One result row from [`search`]: a node id and its distance from the query
/// vector (smaller is more similar).
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub node_id: String,
    pub distance: f64,
}

/// Cosine-distance similarity search: the `limit` nodes whose embedding is
/// closest to `query`, nearest first. A brute-force scan via sqlite-vec's
/// `vec_distance_cosine` scalar function rather than a `vec0` virtual-table
/// ANN index - the right trade-off for a per-project index sized in the
/// thousands of symbols, not the `vec0` KNN index's target of millions of
/// rows.
pub fn search(conn: &Connection, query: &[f32], limit: usize) -> Result<Vec<Match>> {
    let mut stmt = conn
        .prepare(
            "SELECT nodeId, vec_distance_cosine(embedding, ?1) AS distance
             FROM vectors
             ORDER BY distance ASC
             LIMIT ?2",
        )
        .context("failed to prepare vector similarity search")?;

    let rows = stmt
        .query_map(rusqlite::params![pack(query), limit as i64], |row| {
            Ok(Match { node_id: row.get(0)?, distance: row.get(1)? })
        })
        .context("failed to run vector similarity search")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read vector similarity search results")?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn setup() -> Connection {
        register_extension();
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn insert_node(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, 'src/lib.ts', 1, 0, 3, 1, 'typescript')",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn the_sqlite_vec_extension_loads_and_reports_a_version() {
        let conn = setup();
        let version: String = conn.query_row("SELECT vec_version()", [], |row| row.get(0)).unwrap();
        assert!(version.starts_with('v'), "unexpected sqlite-vec version string: {version}");
    }

    #[test]
    fn a_vector_round_trips_through_insert_and_search() {
        let conn = setup();
        for id in ["alpha", "beta", "gamma"] {
            insert_node(&conn, id);
        }

        insert(&conn, "alpha", &[1.0, 0.0, 0.0], "jina-embeddings-v2-base-code").unwrap();
        insert(&conn, "beta", &[0.0, 1.0, 0.0], "jina-embeddings-v2-base-code").unwrap();
        insert(&conn, "gamma", &[0.0, 0.0, 1.0], "jina-embeddings-v2-base-code").unwrap();

        // Closer to "alpha" in direction than to "beta"/"gamma" (which are
        // symmetric to each other around this query, hence tied with one
        // another but both farther than "alpha"), so cosine distance should
        // rank "alpha" nearest.
        let results = search(&conn, &[0.9, 0.05, 0.05], 3).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].node_id, "alpha", "nearest neighbor should be the closest vector by direction");
        assert!(
            results[0].distance < results[1].distance && results[0].distance < results[2].distance,
            "the nearest result should have the smallest distance: {results:?}"
        );
    }

    #[test]
    fn search_respects_the_requested_limit() {
        let conn = setup();
        for id in ["alpha", "beta", "gamma"] {
            insert_node(&conn, id);
        }
        insert(&conn, "alpha", &[1.0, 0.0, 0.0], "v1").unwrap();
        insert(&conn, "beta", &[0.0, 1.0, 0.0], "v1").unwrap();
        insert(&conn, "gamma", &[0.0, 0.0, 1.0], "v1").unwrap();

        let results = search(&conn, &[1.0, 0.0, 0.0], 1).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node_id, "alpha");
    }

    #[test]
    fn re_embedding_a_node_overwrites_its_row_instead_of_adding_one() {
        let conn = setup();
        insert_node(&conn, "alpha");

        insert(&conn, "alpha", &[1.0, 0.0, 0.0], "v1").unwrap();
        insert(&conn, "alpha", &[0.0, 1.0, 0.0], "v2").unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1, "re-embedding must overwrite, not accumulate");

        let version: String =
            conn.query_row("SELECT embeddingVersion FROM vectors WHERE nodeId = 'alpha'", [], |row| row.get(0)).unwrap();
        assert_eq!(version, "v2");
    }

    /// A node going away (a file edit, a deletion) must take its embedding
    /// with it - see the DDL comment on `vectors`'s `ON DELETE CASCADE`.
    #[test]
    fn deleting_a_node_deletes_its_vector_with_foreign_keys_on() {
        let conn = setup();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        insert_node(&conn, "alpha");
        insert(&conn, "alpha", &[1.0, 0.0, 0.0], "v1").unwrap();

        conn.execute("DELETE FROM nodes WHERE id = 'alpha'", []).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "the cascade should remove the orphaned vector row");
    }
}
