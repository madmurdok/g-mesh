//! [`QueryCtx`]: everything one tool call reads besides its parameters.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData;

use crate::embedding::EmbeddingPipeline;
use crate::plugin_manifest::Capabilities;
use crate::storage::index_store::IndexStore;

use super::find_definition::SemanticRung;
use super::query_shapes::QueryShapes;
use super::session_hints::SessionHints;
use super::tool_result::internal_error;
use super::PathCoverage;

/// Everything one tool call reads besides its parameters. Built once per call
/// (by `GMeshMcpServer::query_ctx` or `plugin_check`'s `EvalContext`) and
/// moved into the blocking closure; a new per-call input is one more field.
#[derive(Clone)]
pub(crate) struct QueryCtx {
    pub(crate) store: Arc<IndexStore>,
    pub(crate) embedding: Arc<EmbeddingPipeline>,
    /// Every discovered language's `[plugin.non_symbol_queries]`.
    pub(crate) shapes: Arc<QueryShapes>,
    /// A clone shares the session's set: a hint sent once on a session is not
    /// sent again by any tool on it.
    pub(crate) hints: SessionHints,
    /// Where `find_definition` reads declaration source from.
    pub(crate) project_root: PathBuf,
    /// Every discovered plugin's `[plugin.capabilities]`, keyed by language,
    /// for `provenance::resolve`.
    pub(crate) capabilities: HashMap<String, Capabilities>,
    /// The union of every discovered plugin's `[plugin.workspace]
    /// entry_points` (`get_dependencies`' fallback anchors).
    pub(crate) entry_points: Vec<String>,
    /// The `file_paths` filter entries whose language is not indexed, each
    /// with its coverage; the find tools name them in `notIndexed`.
    pub(crate) uncovered: Vec<(String, PathCoverage)>,
    /// Why the call's anchoring file (`file_path`) can be missing from the
    /// index; `None` when its language is indexed.
    pub(crate) coverage: Option<PathCoverage>,
}

impl QueryCtx {
    /// Runs `handler` with the semantic rung deferred, then - only if the
    /// ladder reached that rung - embeds the name with no lock held and runs
    /// it again with the vector (see [`SemanticRung`]). `handler` must take
    /// and release the store itself, so nothing is held between the passes.
    /// Synchronous: for callers already off the async workers (the CLI,
    /// tests).
    pub(crate) fn resolve_lazily<P: Clone, R>(
        &self,
        handler: impl Fn(&QueryCtx, &SemanticRung<'_>, P) -> Result<R, ErrorData>,
        params: P,
    ) -> Result<R, ErrorData> {
        let first = SemanticRung::deferred(&self.embedding, &self.shapes);
        let answer = handler(self, &first, params.clone())?;
        let Some(name) = first.reached() else { return Ok(answer) };
        let query = self.embedding.embed_query(&name);
        handler(
            self,
            &SemanticRung::Embedded { name: &name, query: query.as_deref(), shapes: &self.shapes },
            params,
        )
    }

    /// [`resolve_lazily`](Self::resolve_lazily) for the server: the first
    /// pass, plain store reads, runs on the async worker as every structural
    /// tool does; the inference and the second pass run on the blocking pool,
    /// so the model's load and the vector scan never stall the other calls
    /// the daemon is serving. As with `search_code::handle_off_worker`,
    /// dropping the future does not stop the blocking work.
    pub(crate) async fn resolve_lazily_off_worker<P, R, H>(
        self,
        handler: H,
        params: P,
    ) -> Result<R, ErrorData>
    where
        P: Clone + Send + 'static,
        R: Send + 'static,
        H: Fn(&QueryCtx, &SemanticRung<'_>, P) -> Result<R, ErrorData> + Send + 'static,
    {
        let name = {
            let first = SemanticRung::deferred(&self.embedding, &self.shapes);
            let answer = handler(&self, &first, params.clone())?;
            match first.reached() {
                Some(name) => name,
                None => return Ok(answer),
            }
        };
        tokio::task::spawn_blocking(move || {
            let query = self.embedding.embed_query(&name);
            let semantic =
                SemanticRung::Embedded { name: &name, query: query.as_deref(), shapes: &self.shapes };
            handler(&self, &semantic, params)
        })
        .await
        .map_err(|e| internal_error("symbol resolution task failed", e.into()))?
    }

    /// A context over `store` alone: a fresh session's hints, the disabled
    /// embedding pipeline, the shipped plugins' query shapes, and nothing
    /// else. Tests override fields with struct-update syntax.
    #[cfg(test)]
    pub(crate) fn for_test(store: &Arc<IndexStore>) -> Self {
        static EMBEDDING: std::sync::LazyLock<Arc<EmbeddingPipeline>> =
            std::sync::LazyLock::new(|| Arc::new(EmbeddingPipeline::disabled()));
        static SHAPES: std::sync::LazyLock<Arc<QueryShapes>> =
            std::sync::LazyLock::new(|| Arc::new(QueryShapes::shipped().clone()));
        Self {
            store: Arc::clone(store),
            embedding: Arc::clone(&EMBEDDING),
            shapes: Arc::clone(&SHAPES),
            hints: SessionHints::default(),
            project_root: PathBuf::new(),
            capabilities: HashMap::new(),
            entry_points: Vec::new(),
            uncovered: Vec::new(),
            coverage: None,
        }
    }
}
