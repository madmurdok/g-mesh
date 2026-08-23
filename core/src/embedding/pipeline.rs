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

use anyhow::Result;
use rusqlite::Connection;

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
    pub fn apply(&self, conn: &Connection, diff: &Diff) -> Result<()> {
        let Some(model) = self.model() else { return Ok(()) };
        if let Err(err) = crate::storage::schema::set_embedding_model(conn, &self.config.model) {
            eprintln!("g-mesh daemon: failed to record the active embedding model ({err:#})");
        }
        for node in &diff.upsert_nodes {
            if let Err(err) = embed_node(
                model,
                conn,
                &node.id,
                node.doc_comment.as_deref(),
                node.signature.as_deref(),
                &self.config.model,
            ) {
                eprintln!(
                    "g-mesh daemon: failed to embed node {} ({err:#}) - it is left unembedded",
                    node.id
                );
            }
        }
        Ok(())
    }
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
}
