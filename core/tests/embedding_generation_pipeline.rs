//! The embedding pipeline, end to end: a fixture file walked by the real
//! plugin through the real cold-start bulk index (`daemon::bulk_index::run`),
//! with a real `EmbeddingModel` wired in - and the `vectors` rows that come
//! out the far side.
//!
//! Mirrors `tests/overload_declaration_storage.rs`'s `Project`/`walk` shape:
//! everything below the plugin process is genuine, and the question here is
//! what a walk embeds, not who asked for it.
//!
//! # Real model weights
//!
//! The tests that actually call into `EmbeddingModel::embed` need
//! `model.onnx`/`tokenizer.json` on disk (`g-mesh model fetch`)
//! and are `#[ignore]`d for the same reason `embedding::model`'s own weight-
//! dependent tests are: CI and a fresh checkout do not have ~600MiB of ONNX
//! weights lying around, so a suite that hard-required them would fail
//! everywhere but a machine that happened to have fetched the model first.
//! Run them explicitly once the weights are present:
//!
//!     g-mesh model fetch      (or core/scripts/fetch-embedding-model.sh)
//!     cd core && cargo test --test embedding_generation_pipeline -- --ignored

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use g_mesh::daemon::bulk_index;
use g_mesh::daemon::manifest::DiscoveredPlugins;
use g_mesh::daemon::plugin::{bundled_manifest, BUNDLED_LANGUAGE};
use g_mesh::embedding::{default_model_dir, EmbeddingModel, EmbeddingPipeline};
use g_mesh::storage::connection::{open, project_dir};
use g_mesh::storage::schema;
use rusqlite::Connection;

/// The single-language discovery every test in this file walks with - just
/// the bundled JS/TS plugin, built by hand from [`bundled_manifest`] rather
/// than through `daemon::manifest::discover`, so this suite's isolation from
/// whatever else happens to live under `~/.g-mesh/plugins/` on the machine
/// running it is unchanged from before task 156 generalized
/// `daemon::bulk_index::run` to take a discovery result at all.
fn only_the_bundled_plugin() -> DiscoveredPlugins {
    DiscoveredPlugins {
        manifests: HashMap::from([(BUNDLED_LANGUAGE.to_string(), bundled_manifest())]),
        routing: HashMap::new(),
    }
}

/// Two functions - one with a doc comment, one without - plus whatever
/// structural nodes the plugin always emits alongside them (a `File` node
/// for `src/lib.ts`, notably). The JS/TS plugin fills a `Function` node's
/// `signature` unconditionally, doc comment or not (see
/// `plugins/typescript/src/extract.ts`), so *every* function here has embeddable
/// text - `bare`'s off its signature alone. The real "nothing to embed" case
/// in this fixture is the `File` node itself, which carries neither field;
/// see [`the_file_node_has_no_doc_comment_or_signature_and_is_not_embedded`].
const FIXTURE: &str = r#"/**
 * Reads a file from disk and returns its contents as a string.
 */
export function readFileAsString(path: string): string {
  return "";
}

export function bare(a: number, b: number) {
  return a + b;
}
"#;

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        let path = project.root().join("src/lib.ts");
        std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create the fixture directory");
        std::fs::write(&path, FIXTURE).expect("failed to write the fixture");
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Runs the cold-start walk against this project with `embedding` wired
    /// in exactly as `daemon::run`/`cli::init`/`cli::reindex` wire it, and
    /// hands back the index it filled.
    fn walk(&self, embedding: &EmbeddingPipeline) -> Connection {
        let conn = open(self.root()).expect("failed to open the project index");
        schema::ensure_current(&conn, "embedding-generation-pipeline-test")
            .expect("failed to prepare the index");
        let conn = Mutex::new(conn);
        let discovered = only_the_bundled_plugin();
        let summary =
            bulk_index::run(self.root(), &conn, embedding, &discovered).expect("the bulk walk failed");
        assert!(summary.nodes > 0, "the walk produced no nodes at all");
        assert_eq!(summary.skipped_lines, 0, "the plugin emitted a line core could not read");
        conn.into_inner().unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn node_id(conn: &Connection, name: &str) -> String {
    conn.query_row("SELECT id FROM nodes WHERE name = ?1 AND kind = 'Function'", [name], |row| row.get(0))
        .unwrap_or_else(|e| panic!("no Function node named {name}: {e}"))
}

fn file_node_id(conn: &Connection) -> String {
    conn.query_row("SELECT id FROM nodes WHERE kind = 'File'", [], |row| row.get(0))
        .expect("the walk must produce a File node for src/lib.ts")
}

fn vector_row_exists(conn: &Connection, node_id: &str) -> bool {
    conn.query_row("SELECT COUNT(*) FROM vectors WHERE nodeId = ?1", [node_id], |row| row.get::<_, i64>(0))
        .unwrap()
        > 0
}

fn model_dir() -> PathBuf {
    default_model_dir(&g_mesh::config::EmbeddingConfig::default().model).unwrap()
}

/// Whether the real weights this suite needs are present on disk - the same
/// check `EmbeddingModel::load` would make, done up front so a test can skip
/// past the plugin walk entirely rather than fail deep inside it when the
/// weights are missing.
fn weights_available() -> bool {
    let dir = model_dir();
    dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists()
}

fn load_real_pipeline() -> EmbeddingPipeline {
    assert!(
        weights_available(),
        "real model weights are required for this test; run `g-mesh model fetch` first"
    );
    EmbeddingPipeline::load(&g_mesh::config::EmbeddingConfig::default())
}

/// A disabled pipeline (no model loaded, e.g. weights never fetched) must
/// still let indexing succeed - and must leave the `vectors` table untouched.
/// This is the one test in this file that needs no weights at all.
#[test]
fn a_disabled_pipeline_indexes_the_fixture_without_writing_any_vectors() {
    let project = Project::new();
    let conn = project.walk(&EmbeddingPipeline::disabled());

    let documented = node_id(&conn, "readFileAsString");
    let bare = node_id(&conn, "bare");
    let file = file_node_id(&conn);
    assert!(!vector_row_exists(&conn, &documented));
    assert!(!vector_row_exists(&conn, &bare));
    assert!(!vector_row_exists(&conn, &file));

    let total: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
    assert_eq!(total, 0, "a disabled pipeline must never write a vector row");
}

/// The acceptance criterion, at the fixture-file level: indexing a real
/// project through the real plugin and the real model produces a `vectors`
/// row for every symbol that has embeddable text - a doc comment, a
/// signature, or both.
#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn indexing_a_fixture_file_embeds_its_documented_symbols() {
    let project = Project::new();
    let pipeline = load_real_pipeline();
    let conn = project.walk(&pipeline);

    let documented = node_id(&conn, "readFileAsString");
    let bare = node_id(&conn, "bare");

    assert!(
        vector_row_exists(&conn, &documented),
        "a node with a doc comment and a signature must be embedded"
    );
    assert!(
        vector_row_exists(&conn, &bare),
        "a node with a signature but no doc comment must still be embedded off its signature"
    );

    let (embedding_len, version): (i64, String) = conn
        .query_row(
            "SELECT LENGTH(embedding), embeddingVersion FROM vectors WHERE nodeId = ?1",
            [&documented],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    // sqlite-vec's packed format: 4 bytes per f32 dimension.
    assert_eq!(embedding_len, (g_mesh::embedding::EMBEDDING_DIM as i64) * 4);
    assert_eq!(version, g_mesh::config::EmbeddingConfig::default().model);
}

/// Task #51's acceptance criterion: after a walk embeds anything at all,
/// `meta.embedding_model` names the model that actually produced those rows -
/// not left `NULL` forever the way it was before `EmbeddingPipeline::apply`
/// started stamping it (see `storage::schema::set_embedding_model`).
#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn a_walk_that_embeds_anything_records_the_active_model_in_meta() {
    let project = Project::new();
    let pipeline = load_real_pipeline();
    let conn = project.walk(&pipeline);

    let recorded: Option<String> =
        conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
    assert_eq!(recorded.as_deref(), Some(g_mesh::config::EmbeddingConfig::default().model.as_str()));
}

/// The acceptance criterion's other half: a node with neither a doc comment
/// nor a signature is skipped rather than embedding an empty string. The
/// `File` node the walk produces for `src/lib.ts` is exactly that node - see
/// the fixture's own doc comment for why neither function qualifies.
#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn the_file_node_has_no_doc_comment_or_signature_and_is_not_embedded() {
    let project = Project::new();
    let pipeline = load_real_pipeline();
    let conn = project.walk(&pipeline);

    let file = file_node_id(&conn);
    let (doc_comment, signature): (Option<String>, Option<String>) = conn
        .query_row("SELECT docComment, signature FROM nodes WHERE id = ?1", [&file], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (doc_comment, signature),
        (None, None),
        "sanity check: this fixture's File node has neither field"
    );

    assert!(!vector_row_exists(&conn, &file), "a node with neither field must not get a vector row");

    // And it is the *only* omission - every other node the walk produced
    // (both functions) is embedded, so this is a targeted skip, not a
    // pipeline that quietly embedded nothing at all.
    let embedded_names: Vec<String> = conn
        .prepare("SELECT n.name FROM vectors v JOIN nodes n ON n.id = v.nodeId ORDER BY n.name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(embedded_names, vec!["bare".to_string(), "readFileAsString".to_string()]);
}

/// `search_code`'s query embedding must land in the same vector space as node
/// embeddings - i.e. must go through the identical model and pooling, not a
/// separately loaded instance that happens to use the same weights file.
#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn embed_query_matches_embedding_the_same_text_directly() {
    let pipeline = load_real_pipeline();
    let model = EmbeddingModel::load(&model_dir()).expect("failed to load the real model");

    let expected = model.embed("find a function that reads a file from disk").unwrap();
    let actual = pipeline.embed_query("find a function that reads a file from disk").unwrap();
    assert_eq!(actual, expected);
}

/// Loading a real model directly (not through the pipeline) and comparing
/// its output to what the pipeline actually stored - proof the pipeline
/// embeds the doc comment plus signature, not some other text, and that what
/// lands in `vectors` is exactly what `EmbeddingModel::embed` would produce
/// for it.
#[test]
#[ignore = "needs the real model weights; see this file's module doc comment"]
fn the_stored_vector_matches_embedding_the_doc_comment_and_signature_directly() {
    let project = Project::new();
    let pipeline = load_real_pipeline();
    let conn = project.walk(&pipeline);
    let documented = node_id(&conn, "readFileAsString");

    let (doc_comment, signature): (Option<String>, Option<String>) = conn
        .query_row("SELECT docComment, signature FROM nodes WHERE id = ?1", [&documented], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    let doc_comment = doc_comment.expect("the fixture function has a doc comment");
    let signature = signature.expect("the fixture function has a signature");

    let model = EmbeddingModel::load(&model_dir()).expect("failed to load the real model");
    let expected = model.embed(&format!("{}\n\n{}", doc_comment.trim(), signature.trim())).unwrap();

    let stored: Vec<u8> = conn
        .query_row("SELECT embedding FROM vectors WHERE nodeId = ?1", [&documented], |row| row.get(0))
        .unwrap();
    let stored: Vec<f32> =
        stored.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();

    assert_eq!(stored, expected, "the stored vector must be exactly the model's output for doc+signature");
}
