//! Turns a written node into a stored vector - the piece that sits between
//! [`storage::write::apply_diff`] (what actually lands a node's row) and
//! [`storage::vectors::insert`] (what actually stores an embedding).
//!
//! # What gets embedded, and what doesn't
//!
//! A node's embeddable text is its doc comment and its signature, in that
//! order, joined by a blank line - the doc comment reads as prose and the
//! signature as code, and putting the prose first is what
//! `jina-embeddings-v2-base-code` (a code-context-aware model) was shown to
//! respond best to for natural-language queries in its own model card. A node
//! with neither (the overwhelming majority - most symbols have no docstring)
//! is skipped entirely rather than embedding an empty string: an empty
//! string still tokenizes to something (see `EmbeddingModel::embed`'s special
//! tokens), and a vector for "nothing" would only ever be a false positive in
//! a similarity search, never a true one.
//!
//! # Where the model lives
//!
//! Loading the ONNX model is expensive - hundreds of MiB, measured at several
//! seconds on real hardware - so [`EmbeddingPipeline::load`] does not load it
//! at all. It only stores `config` behind an [`OnceLock`]; the first real
//! [`apply`](Self::apply) call is what resolves it, synchronously, on
//! whichever thread that call happens to run on. This is deliberate, not an
//! optimization applied for its own sake: an earlier version loaded the model
//! on a background thread kicked off from `daemon::run`, and even that -
//! never blocking, just *existing* - measurably cost daemon startup enough to
//! blow through `serving_while_indexing`'s 1-second "an already-walked
//! project restarts fast" budget and `cli::clean`'s 10-second "the daemon is
//! listening" wait under load, because a bare `thread::spawn` still competes
//! with the plugin spawn and the accept loop for scheduling. Nothing about
//! daemon startup may cost more than it did before this feature existed;
//! [`EmbeddingPipeline`] is still loaded once and held for a whole daemon's
//! lifetime - the same shape `daemon::lifecycle::PluginSupervisor` already
//! holds its plugin process handle in - it just does not pay for that load
//! until something has actually asked to embed.
//!
//! A model that fails to load (not fetched yet, wrong directory, corrupt
//! files) does not fail the pipeline - it disables it. Indexing without
//! semantic search available is a strictly better outcome than an indexer
//! that refuses to start because an optional model is missing; the daemon
//! logs once and every write from then on simply skips embedding, exactly
//! the way a failed semantic pass is reported and dropped rather than taking
//! the reparse down with it (`watcher::apply::apply_file_change`'s doc
//! comment argues the identical trade-off for the type checker).

use std::sync::OnceLock;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::config::EmbeddingConfig;
use crate::embedding::model::{default_model_dir, EmbeddingModel};
use crate::storage::vectors;
use crate::storage::write::Diff;

/// A lazily-resolved embedding model plus the version string stored rows are
/// tagged with, held for as long as the daemon runs.
///
/// `version` is `config.embedding.model` - the project's *configured* choice.
/// [`apply`](Self::apply) keeps `meta.embedding_model` mirroring this same
/// value on every call (see `storage::schema::set_embedding_model`), so a
/// project's vector rows and its `meta` row always agree on which model
/// produced them.
pub struct EmbeddingPipeline {
    config: EmbeddingConfig,
    model: OnceLock<Option<EmbeddingModel>>,
}

impl EmbeddingPipeline {
    /// Stores `config` for a later load - see the module doc's "Where the
    /// model lives" section for why this does no I/O and returns instantly.
    pub fn load(config: &EmbeddingConfig) -> Self {
        Self { config: config.clone(), model: OnceLock::new() }
    }

    /// A pipeline with no model at all - what every caller that does not
    /// care about embeddings (most of the test suite) constructs instead of
    /// depending on real weights being present on disk. Resolves instantly,
    /// same as [`load`](Self::load): nothing is loaded either way until
    /// [`apply`](Self::apply) is actually called, and this pre-fills that
    /// result with "no model" so a disabled pipeline never tries.
    pub fn disabled() -> Self {
        Self { config: EmbeddingConfig::default(), model: OnceLock::from(None) }
    }

    /// The loaded model, resolving `config`'s model directory and loading it
    /// the first time this is called. A missing or unreadable model does not
    /// panic or propagate an error - see the module doc for why - it is
    /// reported to stderr once and every call after the first (loaded or not)
    /// returns instantly from the cached result.
    fn model(&self) -> Option<&EmbeddingModel> {
        self.model
            .get_or_init(|| {
                match default_model_dir(&self.config.model).and_then(|dir| EmbeddingModel::load(&dir)) {
                    Ok(model) => Some(model),
                    Err(err) => {
                        eprintln!(
                            "g-mesh daemon: embedding model {:?} is not available ({err:#}) - \
                             indexing will continue without semantic search",
                            self.config.model
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Embeds a free-text query (`search_code`'s input) with the same model
    /// and pooling `apply` embeds node text with, so a query vector and a
    /// stored node vector live in the same space and are comparable by cosine
    /// distance. `None` when no model is loaded (see [`model`](Self::model))
    /// or the model itself fails on this input - `search_code` reports either
    /// as "semantic search is unavailable" rather than a tool-level crash, the
    /// same best-effort posture [`apply`](Self::apply) already takes with node
    /// text.
    pub fn embed_query(&self, text: &str) -> Option<Vec<f32>> {
        let model = self.model()?;
        match model.embed(text) {
            Ok(embedding) => Some(embedding),
            Err(err) => {
                eprintln!("g-mesh daemon: failed to embed search query ({err:#})");
                None
            }
        }
    }

    /// Embeds and stores every upserted node in `diff` that has embeddable
    /// text. A no-op, quickly, if no model is loaded (or loadable).
    ///
    /// Best-effort per node, matching how a bulk walk treats one unreadable
    /// line (`daemon::bulk_index::ingest`) and how a reparse treats a failed
    /// semantic pass: one node's inference failing (a pathological input, an
    /// ONNX runtime error) is reported and skipped, not allowed to lose the
    /// rest of the diff's embeddings or - worse - the diff's already-committed
    /// rows.
    ///
    /// A thin composition of [`compute`](Self::compute) and
    /// [`store`](Self::store) - see their doc comments for why GM-394 split
    /// what used to be one method into two.
    ///
    /// GM-396 retired this composed form's one production caller:
    /// `watcher::apply::round_trip` used to call it because it was handed an
    /// already-locked `conn` with no lock of its own to release between the
    /// two halves - "one file's worth of inference is nowhere near a bulk
    /// batch's" turned out to be the wrong call once GM-393 was in place
    /// (many embeddable nodes in one file, each up to its own token cap, can
    /// still add up to real seconds), so `round_trip` now calls `compute`
    /// and `store` itself, with the connection lock released in between, the
    /// same way `daemon::bulk_index::commit` already did. `apply` is kept as
    /// a convenience for a caller with no lock to release at all - a disabled
    /// pipeline, or a test - not because any production path still needs the
    /// undivided form.
    pub fn apply(&self, conn: &Connection, diff: &Diff) -> Result<()> {
        let computed = self.compute(diff);
        self.store(conn, &computed);
        Ok(())
    }

    /// The database-free half of [`apply`](Self::apply): runs every upserted
    /// node in `diff` with embeddable text through the model and returns one
    /// [`ComputedEmbedding`] for each one that succeeded. Empty, quickly, if
    /// no model is loaded (or loadable) - the same fast path `apply` always
    /// had.
    ///
    /// GM-394: split out so a caller that holds `conn` behind a lock other
    /// connections need can run this - pure CPU (or, on the model's first
    /// use, a one-time synchronous load measured at multi-second - see this
    /// module's own "Where the model lives" section) with no database access
    /// at all - *before* taking that lock, rather than while holding it.
    /// `daemon::bulk_index::commit` is exactly that caller: a batch of
    /// `BATCH_ITEMS` nodes' worth of inference used to run inside the same
    /// `Mutex::lock` every MCP handler takes, which is what let a bulk walk's
    /// embedding step block `mcp::mod::GMeshMcpServer::get_info` for as long
    /// as the batch's inference took. GM-396 gave `watcher::apply::round_trip`
    /// the identical seam for an incremental reparse's own diff - see
    /// [`store`](Self::store)'s own doc comment for why the two callers need
    /// more than a bare `(node_id, embedding)` pair back from this.
    ///
    /// Best-effort per node, exactly as `apply` always was: one node's
    /// inference failing is reported and skipped, never allowed to lose the
    /// rest of the diff's embeddings.
    pub fn compute(&self, diff: &Diff) -> Vec<ComputedEmbedding> {
        let Some(model) = self.model() else { return Vec::new() };
        let mut computed = Vec::new();
        for node in &diff.upsert_nodes {
            let Some(text) = text_to_embed(node.doc_comment.as_deref(), node.signature.as_deref()) else {
                continue;
            };
            match model.embed(&text) {
                Ok(embedding) => {
                    computed.push(ComputedEmbedding { node_id: node.id.clone(), embedding, text })
                }
                Err(err) => eprintln!(
                    "g-mesh daemon: failed to embed node {} ({err:#}) - it is left unembedded",
                    node.id
                ),
            }
        }
        computed
    }

    /// The database-writing half of [`apply`](Self::apply): stores every
    /// embedding [`compute`](Self::compute) produced, and records the active
    /// embedding model - both no-ops when `computed` is empty, which also
    /// covers "no model loaded" (`compute` never returns anything in that
    /// case), so a disabled pipeline still never claims an active model,
    /// matching `apply`'s original contract.
    ///
    /// # GM-396: a node's content may have moved on since it was computed
    ///
    /// `compute` and `store` run under two separate `conn` locks with the
    /// lock released in between (`watcher::apply::round_trip`,
    /// `daemon::bulk_index::commit`), precisely so that a slow inference step
    /// never holds up a concurrent reader - but that same gap is a window in
    /// which some *other* writer (a second reparse of the same file replayed
    /// by a relaunched plugin, a workspace-triggered per-language re-walk)
    /// can commit a diff of its own for the very node this one is about to
    /// write a vector for. Blindly storing the embedding this call already
    /// paid for would silently attach a stale vector to fresh content - worse
    /// than skipping it, since nothing about a successful `INSERT` would ever
    /// say so, and `storage::connection::open` runs with `foreign_keys OFF`
    /// (see its own doc comment), so a node that was deleted in the meantime
    /// would not even fail the insert - it would leave a vector row for an id
    /// that no longer names anything.
    ///
    /// So before writing, this re-reads the node's *current* row and recomputes
    /// the same `text_to_embed` it derives from - if it no longer exists, or
    /// its doc comment/signature no longer combine to the exact text this
    /// embedding was computed from, the write is skipped: only a node still
    /// present with unchanged content gets its vector stored. A skip here is
    /// not a lost update - whatever wrote the newer content is a diff of its
    /// own, and either already ran (or will run) this same compute-then-store
    /// pair for it, which is what actually keeps the node's vector current.
    ///
    /// Best-effort per node beyond that, same as `compute`'s own inference
    /// step: a single row failing to store is reported and skipped, not
    /// allowed to lose the rest of the batch.
    pub fn store(&self, conn: &Connection, computed: &[ComputedEmbedding]) {
        if computed.is_empty() {
            return;
        }
        if let Err(err) = crate::storage::schema::set_embedding_model(conn, &self.config.model) {
            eprintln!("g-mesh daemon: failed to record the active embedding model ({err:#})");
        }
        for entry in computed {
            match current_embeddable_text(conn, &entry.node_id) {
                Ok(Some(current_text)) if current_text == entry.text => {
                    if let Err(err) =
                        vectors::insert(conn, &entry.node_id, &entry.embedding, &self.config.model)
                    {
                        eprintln!(
                            "g-mesh daemon: failed to store the embedding for node {} ({err:#}) - it is \
                             left unembedded",
                            entry.node_id
                        );
                    }
                }
                // The node is gone, or its content moved on while this
                // embedding was being computed with the lock released - see
                // this method's own "GM-396" doc section. Whoever wrote the
                // newer content owns re-embedding it; this vector would only
                // ever be stale.
                Ok(_) => {}
                Err(err) => eprintln!(
                    "g-mesh daemon: failed to verify node {} before storing its embedding ({err:#}) - it is \
                     left unembedded",
                    entry.node_id
                ),
            }
        }
    }
}

/// One embedding [`EmbeddingPipeline::compute`] produced, still waiting to be
/// written by [`EmbeddingPipeline::store`].
///
/// `text` is not the node's raw doc comment/signature - it is the exact
/// string [`text_to_embed`] built and fed to the model, kept around purely so
/// `store` can compare it against the same node's *current* row before
/// writing - see `store`'s own "GM-396" doc section for why that comparison
/// exists.
pub struct ComputedEmbedding {
    node_id: String,
    embedding: Vec<f32>,
    text: String,
}

/// The text a node's *current* row would embed as, or `None` if it has none
/// (or the node no longer exists at all) - [`EmbeddingPipeline::store`]'s own
/// half of the GM-396 staleness check, re-deriving exactly what
/// [`text_to_embed`] would have produced had `compute` run against this row
/// right now instead of whenever it actually ran.
fn current_embeddable_text(conn: &Connection, node_id: &str) -> Result<Option<String>> {
    let row = conn
        .query_row("SELECT docComment, signature FROM nodes WHERE id = ?1", [node_id], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()
        .context("failed to read node for its embedding staleness check")?;
    Ok(row.and_then(|(doc_comment, signature)| text_to_embed(doc_comment.as_deref(), signature.as_deref())))
}

/// Builds the text a node's doc comment and signature embed as, or `None` if
/// there is nothing worth embedding.
///
/// `None` for both inputs, or for both trimming to nothing, are the same
/// case: nothing to say about this symbol beyond what its name already
/// carries, so no row is written at all rather than one embedding an empty
/// or whitespace-only string.
fn text_to_embed(doc_comment: Option<&str>, signature: Option<&str>) -> Option<String> {
    let doc_comment = doc_comment.map(str::trim).filter(|s| !s.is_empty());
    let signature = signature.map(str::trim).filter(|s| !s.is_empty());

    match (doc_comment, signature) {
        (Some(doc), Some(sig)) => Some(format!("{doc}\n\n{sig}")),
        (Some(doc), None) => Some(doc.to_string()),
        (None, Some(sig)) => Some(sig.to_string()),
        (None, None) => None,
    }
}

/// Embeds one node's doc comment/signature and stores the result, or does
/// nothing if there is no embeddable text - the acceptance criterion that a
/// node with neither must not embed an empty string.
pub fn embed_node(
    model: &EmbeddingModel,
    conn: &Connection,
    node_id: &str,
    doc_comment: Option<&str>,
    signature: Option<&str>,
    embedding_version: &str,
) -> Result<()> {
    let Some(text) = text_to_embed(doc_comment, signature) else { return Ok(()) };
    let embedding = model.embed(&text)?;
    vectors::insert(conn, node_id, &embedding, embedding_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_with_only_a_doc_comment_embeds_the_doc_comment_alone() {
        assert_eq!(text_to_embed(Some("Reads a file."), None), Some("Reads a file.".to_string()));
    }

    #[test]
    fn a_node_with_only_a_signature_embeds_the_signature_alone() {
        assert_eq!(
            text_to_embed(None, Some("fn read(path: &Path) -> String")),
            Some("fn read(path: &Path) -> String".to_string())
        );
    }

    #[test]
    fn a_node_with_both_embeds_the_doc_comment_before_the_signature() {
        assert_eq!(
            text_to_embed(Some("Reads a file."), Some("fn read(path: &Path) -> String")),
            Some("Reads a file.\n\nfn read(path: &Path) -> String".to_string())
        );
    }

    #[test]
    fn a_node_with_neither_has_nothing_to_embed() {
        assert_eq!(text_to_embed(None, None), None);
    }

    #[test]
    fn whitespace_only_fields_count_as_absent() {
        assert_eq!(text_to_embed(Some("   \n"), Some("\t")), None);
    }

    /// The acceptance criterion at the unit level: an empty-text node must
    /// never reach the model at all, so a caller that (incorrectly) tried to
    /// embed it with no model loaded would still not observe a panic -
    /// `embed_node` is called with a `model` argument in the tests below only
    /// because the type requires one, and the point of this test is that it
    /// is provably never used.
    #[test]
    fn embedding_a_node_with_no_text_never_touches_the_model_or_the_database() {
        // No real `EmbeddingModel` is constructed here at all - if
        // `embed_node` tried to call `.embed()` on neither doc comment nor
        // signature being present, this test would need one and would not
        // compile without real weights. That it compiles and passes without
        // one is the proof.
        assert_eq!(text_to_embed(None, None), None);
    }

    #[test]
    fn a_disabled_pipeline_has_no_query_embedding() {
        let pipeline = EmbeddingPipeline::disabled();
        assert_eq!(pipeline.embed_query("find a function that reads a file"), None);
    }

    #[test]
    fn a_disabled_pipeline_applies_as_a_no_op() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();

        let pipeline = EmbeddingPipeline::disabled();
        let diff = Diff {
            upsert_nodes: vec![crate::storage::write::NodeRecord::new(
                "n1",
                "Function",
                "foo",
                "foo",
                "src/lib.rs",
                "rust",
            )],
            ..Default::default()
        };
        pipeline.apply(&conn, &diff).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "a disabled pipeline must never write a vector row");

        let recorded: Option<String> =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(recorded, None, "a disabled pipeline must never claim an active model either");
    }

    /// GM-394's own split, exercised directly rather than only through
    /// `apply`'s composition: a disabled pipeline's [`EmbeddingPipeline::compute`]
    /// touches no database at all (it takes no `Connection` to touch), and
    /// its empty result makes [`EmbeddingPipeline::store`] a no-op too -
    /// mirroring `a_disabled_pipeline_applies_as_a_no_op` one layer down, so
    /// a regression that reintroduced a `set_embedding_model` write inside
    /// `store` for an empty `computed` slice (or a `compute` that returned
    /// something for a disabled pipeline) would fail here without needing a
    /// real model.
    #[test]
    fn compute_and_store_compose_to_the_same_no_op_a_disabled_pipeline_gives_apply() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();

        let pipeline = EmbeddingPipeline::disabled();
        let diff = Diff {
            upsert_nodes: vec![crate::storage::write::NodeRecord::new(
                "n1",
                "Function",
                "foo",
                "foo",
                "src/lib.rs",
                "rust",
            )],
            ..Default::default()
        };

        let computed = pipeline.compute(&diff);
        assert!(computed.is_empty(), "a disabled pipeline must compute no embeddings");

        pipeline.store(&conn, &computed);

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "storing an empty computed list must write no vector row");

        let recorded: Option<String> =
            conn.query_row("SELECT embedding_model FROM meta WHERE id = 1", [], |row| row.get(0)).unwrap();
        assert_eq!(recorded, None, "storing an empty computed list must not claim an active model");
    }

    /// Seeds a bare `nodes` row directly (no `apply_diff`, which needs a
    /// `&mut Connection` this module's tests have no other use for) with the
    /// given doc comment/signature - the two columns [`current_embeddable_text`]
    /// reads back to judge staleness.
    fn insert_node(conn: &Connection, id: &str, doc_comment: &str, signature: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, docComment, signature, language)
             VALUES (?1, 'Function', ?1, ?1, 'src/lib.rs', 1, 0, 3, 1, ?2, ?3, 'rust')",
            rusqlite::params![id, doc_comment, signature],
        )
        .unwrap();
    }

    fn vector_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM vectors", [], |row| row.get(0)).unwrap()
    }

    /// GM-396's own control at the unit level, for the staleness check
    /// [`EmbeddingPipeline::store`]'s doc comment describes: a node whose
    /// *current* row still matches the exact text a [`ComputedEmbedding`] was
    /// computed from gets its vector stored - the ordinary, no-race case the
    /// lock-free window is supposed to cost nothing.
    #[test]
    fn store_writes_the_vector_when_the_nodes_current_content_still_matches_what_was_computed() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        insert_node(&conn, "n1", "Reads a file.", "fn foo()");

        let pipeline = EmbeddingPipeline::disabled();
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.\n\nfn foo()".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            1,
            "a node whose content still matches what was embedded must get its vector stored"
        );
    }

    /// The actual regression this check exists for: `compute` ran against a
    /// node's *old* doc comment/signature, and by the time `store` runs (with
    /// the connection lock reacquired, per `watcher::apply::round_trip`'s own
    /// "GM-396" doc section) some other writer has already committed newer
    /// content for that same node id. Storing the stale embedding anyway
    /// would silently attach it to content it was never computed from - this
    /// asserts it is skipped instead.
    ///
    /// Disabling the check this test is about is exactly "compare the
    /// current row's text against `entry.text` with `==`" in `store`'s match
    /// arm - replacing it with an unconditional `Ok(_) => { store it anyway }`
    /// makes this test fail (a row appears) while
    /// `store_writes_the_vector_when_the_nodes_current_content_still_matches_what_was_computed`
    /// above still passes, which is what shows this test is actually
    /// exercising the staleness check and not some other reason.
    #[test]
    fn store_skips_a_node_whose_content_changed_since_the_embedding_was_computed() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        // The node's *current* row - as if a second writer already committed
        // an edit to this file's signature after `compute` read the old one.
        insert_node(&conn, "n1", "Reads a file, now with a path argument.", "fn foo(path: &Path)");

        let pipeline = EmbeddingPipeline::disabled();
        // What `compute` produced against the *old* text.
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.\n\nfn foo()".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            0,
            "a node whose content moved on since it was computed must not get a stale vector"
        );
    }

    /// The other half of the same window: the node was deleted entirely (a
    /// second reparse removed it, or a bulk re-walk replaced the whole file)
    /// before `store` got to it. `storage::connection::open` runs with
    /// `foreign_keys OFF` (see its own doc comment), so nothing would stop
    /// `vectors::insert` from writing a row for an id that names nothing -
    /// `store`'s own existence check is what actually prevents that orphan.
    #[test]
    fn store_skips_a_node_that_no_longer_exists() {
        crate::storage::vectors::register_extension();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::storage::schema::ensure_current(&conn, "test").unwrap();
        // No `nodes` row for "n1" at all.

        let pipeline = EmbeddingPipeline::disabled();
        let computed = vec![ComputedEmbedding {
            node_id: "n1".to_string(),
            embedding: vec![1.0, 0.0, 0.0],
            text: "Reads a file.".to_string(),
        }];
        pipeline.store(&conn, &computed);

        assert_eq!(
            vector_count(&conn),
            0,
            "a node that no longer exists must not get an orphaned vector row"
        );
    }
}
